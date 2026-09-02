#!/usr/bin/env bash
# Idempotent repository bootstrap for the WattMail Cloud Agent environment.
# Prepares everything a future agent needs to build, lint, test, and run the app
# on Linux. Safe to run repeatedly.
set -euo pipefail
cd "$(dirname "$0")/.."

export DEBIAN_FRONTEND=noninteractive

# System libraries Tauri needs to compile and run on Linux (mirrors the CI
# cross-check job), plus a headless X server and a keyring so the desktop app can
# launch and the full `cargo test --workspace` suite (which touches the OS
# Secret Service) can pass.
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libwebkit2gtk-4.1-dev \
  libgtk-3-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  libxdo-dev \
  libsoup-3.0-dev \
  patchelf \
  xvfb \
  x11-utils \
  imagemagick \
  gnome-keyring \
  dbus-x11 \
  libsecret-tools

# Frontend dependencies and build. Tauri's generate_context! embeds dist/, so the
# frontend must be built before the Rust presentation crate compiles.
npm ci
npm run build

# The Linux keyring backend (keyring crate, sync-secret-service feature) talks to
# a session D-Bus. Source the shared helper from every login/interactive shell so
# the Secret Service is brought up (and self-heals) even if the per-boot `start`
# phase didn't run — `cargo test --workspace` and the app then reach the keyring
# without any per-command wrapper.
marker="# wattmail-dbus"
line="[ -f /workspace/.cursor/dbus-env.sh ] && . /workspace/.cursor/dbus-env.sh  # wattmail-dbus"
for f in "$HOME/.bashrc" "$HOME/.profile"; do
  touch "$f"
  if grep -qF "$marker" "$f"; then
    # Replace any earlier version of the line so re-runs converge on the helper.
    sed -i "\|$marker|c\\$line" "$f"
  else
    printf '%s\n' "$line" >> "$f"
  fi
done

# Warm the Cargo build cache so future agents get fast incremental builds from the
# snapshot. --all-targets also compiles the test harnesses.
cargo build --workspace --all-targets

# Don't let a keyring created during build/setup get baked into the image: build
# layers can zero its mtime, which makes gnome-keyring SIGABRT on the next boot.
# dbus-env.sh also recovers from this at runtime, but shipping a clean slate is
# better hygiene. The dev keyring is throwaway.
rm -rf "$HOME/.local/share/keyrings"

echo "WattMail install complete."

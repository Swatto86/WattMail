#!/usr/bin/env bash
# Project verify gate — run before declaring work done. The global stop-gate
# hook prefers this over its generic cargo fallback.
#
# PATH note: this machine carries a standalone GNU-target Rust install in
# Program Files ahead of rustup's shims, and its mingw linker cannot link the
# app's cdylib ("export ordinal too large" — >64k exports overflow PE's 16-bit
# export ordinals). Prepend rustup's shims so the pinned MSVC toolchain
# (rust-toolchain.toml) is used, matching CI.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

npm run build
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace

# The three files that carry the version must agree.
#
# `src-tauri/Cargo.toml` decides what the binary reports, `tauri.conf.json`
# decides what the installer and the updater manifest say, and `package.json` is
# what a person reads first. Nothing made them equal: a release that bumped two
# of the three would ship an installer whose name disagreed with the app's own
# About box, and the updater would offer a version that never existed.
cargo_version=$(sed -n '0,/^version = /s/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml)
tauri_version=$(sed -n '0,/"version":/s/.*"version": "\(.*\)".*/\1/p' src-tauri/tauri.conf.json)
npm_version=$(sed -n '0,/"version":/s/.*"version": "\(.*\)".*/\1/p' package.json)
# Read before compared: an empty match from a moved key would make all three
# "equal" and the check silently vacuous.
for pair in "src-tauri/Cargo.toml:$cargo_version" "tauri.conf.json:$tauri_version" "package.json:$npm_version"; do
  if [ -z "${pair#*:}" ]; then
    echo "version check is broken: read nothing from ${pair%%:*}"
    exit 1
  fi
done
if [ "$cargo_version" != "$tauri_version" ] || [ "$cargo_version" != "$npm_version" ]; then
  echo "version mismatch: Cargo.toml $cargo_version, tauri.conf.json $tauri_version, package.json $npm_version"
  exit 1
fi
echo "version agreement OK ($cargo_version)"

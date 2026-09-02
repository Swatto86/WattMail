# Shared, idempotent session-D-Bus + keyring bring-up for WattMail on Linux.
#
# Sourced by every login/interactive shell (via ~/.bashrc and ~/.profile, wired
# by install.sh) AND by .cursor/start.sh. It exports the fixed session bus
# address and, if the Secret Service isn't already registered, launches a
# detached session D-Bus bus with an unlocked gnome-keyring. Being sourced from
# shells makes the keyring self-healing: the Linux keyring backend (keyring
# crate, sync-secret-service feature) that `cargo test --workspace` and the app
# rely on is available even if the per-boot `start` phase didn't run.
#
# Do not enable `set -e`/pipefail here — this file is sourced into interactive
# shells.

export DBUS_SESSION_BUS_ADDRESS="unix:path=/tmp/wattmail/dbus.sock"

# True only when the Secret Service *name* is registered on the bus — i.e. a
# working keyring, not merely a running bus daemon. Checking the bus alone is the
# trap that hid an aborted gnome-keyring: the bus was up, secrets were not.
__wm_secrets_alive() {
  dbus-send --session --dest=org.freedesktop.DBus --type=method_call \
    --print-reply /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner \
    string:org.freedesktop.secrets >/dev/null 2>&1
}

__wm_bus_alive() {
  dbus-send --session --dest=org.freedesktop.DBus --type=method_call \
    --print-reply /org/freedesktop/DBus org.freedesktop.DBus.ListNames \
    >/dev/null 2>&1
}

if ! __wm_secrets_alive; then
  mkdir -p /tmp/wattmail
  # Serialize bring-up so concurrent shells don't race to start duplicate daemons.
  (
    flock 9
    if ! __wm_secrets_alive; then
      if ! __wm_bus_alive; then
        # A stale socket from a crashed daemon would block a new bind.
        [ -S /tmp/wattmail/dbus.sock ] || rm -f /tmp/wattmail/dbus.sock
        # setsid fully detaches the daemon so it survives the sourcing shell.
        setsid dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" \
          --nofork --nopidfile >/dev/null 2>&1 &
        for _ in $(seq 1 50); do
          [ -S /tmp/wattmail/dbus.sock ] && break
          sleep 0.1
        done
      fi
      # gnome-keyring SIGABRTs on keyring files with a zero/invalid mtime — an
      # artifact of prebuilt-build image layers (egg-file-tracker assertion).
      # The dev keyring is throwaway (a fresh agent has no real secrets), so
      # start from a clean directory to guarantee the Secret Service comes up.
      rm -rf "$HOME/.local/share/keyrings"
      # Empty password creates an auto-unlocked default collection (headless
      # recipe); a non-empty password leaves it locked and blocks on a GUI prompt.
      printf '\n' | setsid gnome-keyring-daemon --unlock --components=secrets \
        >/dev/null 2>&1 || true
      for _ in $(seq 1 50); do
        __wm_secrets_alive && break
        sleep 0.1
      done
    fi
  ) 9>/tmp/wattmail/.bringup.lock
fi

unset -f __wm_secrets_alive __wm_bus_alive 2>/dev/null || true

#!/usr/bin/env bash
# Per-boot runtime setup for WattMail. Brings up a session D-Bus bus and an
# unlocked gnome-keyring so the Linux keyring backend (Secret Service) is
# available to the test suite and the running app. The actual work lives in the
# shared, idempotent helper that login shells also source, so the keyring is
# available whether it comes up here on boot or lazily from the first shell.
set -euo pipefail
cd "$(dirname "$0")"

# shellcheck source=/dev/null
. ./dbus-env.sh

if dbus-send --session --dest=org.freedesktop.DBus --type=method_call \
     --print-reply /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner \
     string:org.freedesktop.secrets >/dev/null 2>&1; then
  echo "WattMail: session D-Bus + keyring ready at $DBUS_SESSION_BUS_ADDRESS"
else
  echo "WattMail: WARNING - Secret Service did not come up at $DBUS_SESSION_BUS_ADDRESS" >&2
fi

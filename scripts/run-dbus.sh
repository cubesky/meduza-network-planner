#!/bin/bash
set -euo pipefail

# D-Bus daemon startup wrapper
# Checks for stale PID file before starting dbus-daemon

DBUS_PID_FILE="/run/dbus/pid"

# Remove stale PID file if it exists
if [[ -f "${DBUS_PID_FILE}" ]]; then
  echo "[dbus] Removing stale PID file: ${DBUS_PID_FILE}"
  rm -f "${DBUS_PID_FILE}"
fi

# Ensure /run/dbus directory exists
mkdir -p /run/dbus

echo "[dbus] Starting dbus-daemon"
exec /usr/bin/dbus-daemon --system --nofork

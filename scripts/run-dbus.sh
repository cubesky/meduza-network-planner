#!/bin/sh

set -eu

DBUS_DIR="/run/dbus"
DBUS_DAEMON="/usr/bin/dbus-daemon"

if [ ! -d "${DBUS_DIR}" ]; then
    if ! mkdir -p "${DBUS_DIR}"; then
        echo "error: failed to create ${DBUS_DIR}" >&2
        exit 1
    fi
fi

if [ ! -x "${DBUS_DAEMON}" ]; then
    echo "error: dbus-daemon not found at ${DBUS_DAEMON}" >&2
    exit 1
fi

if [ -f "${DBUS_DIR}/pid" ]; then
    rm -f "${DBUS_DIR}/pid"
fi

exec "${DBUS_DAEMON}" --system --nofork

#!/bin/bash
set -euo pipefail

NETNAME_FILE="/etc/tinc/.netname"

if [[ ! -f "$NETNAME_FILE" ]]; then
  echo "missing $NETNAME_FILE" >&2
  exit 1
fi

netname="$(cat "$NETNAME_FILE" 2>/dev/null || true)"
netname="${netname//[$'\r\n\t ']}"
if [[ -z "$netname" ]]; then
  echo "empty netname in $NETNAME_FILE" >&2
  exit 1
fi

exec tincd -c "/etc/tinc/${netname}" -D --pidfile=/run/tincd.pid

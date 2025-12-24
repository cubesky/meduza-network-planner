#!/bin/bash
set -euo pipefail

DEV="${1:-}"
if [[ -z "${DEV}" ]]; then
  echo "[wireguard] missing dev argument" >&2
  exit 1
fi

CONF="/etc/wireguard/${DEV}.conf"
if [[ ! -f "${CONF}" ]]; then
  echo "[wireguard] config not found: ${CONF}" >&2
  exit 1
fi

WG_UP=0
cleanup() {
  if [[ "${WG_UP}" -eq 1 ]]; then
    wg-quick down "${CONF}" >/dev/null 2>&1 || true
  fi
}

trap cleanup TERM INT

wg-quick down "${CONF}" >/dev/null 2>&1 || true
wg-quick up "${CONF}"
WG_UP=1

while true; do
  sleep 3600
done

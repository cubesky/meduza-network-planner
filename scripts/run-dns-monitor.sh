#!/bin/bash
set -euo pipefail

INTERVAL="${DNS_MONITOR_INTERVAL:-5}"
NAMESERVERS="${DNS_NAMESERVERS:-${DNS_NAMESERVER:-}}"
if [[ -z "${NAMESERVERS}" ]]; then
  NAMESERVERS="223.5.5.5 223.6.6.6"
fi

NAMESERVERS="$(echo "${NAMESERVERS}" | tr ',\t' '  ')"

render_resolv() {
  for ns in ${NAMESERVERS}; do
    echo "nameserver ${ns}"
  done
}

write_resolv() {
  printf "%s\n" "$(render_resolv)" > /etc/resolv.conf
}

if [[ "${1:-}" == "--once" ]]; then
  write_resolv
  exit 0
fi

while true; do
  if [[ ! -f /etc/resolv.conf ]] || ! cmp -s <(printf "%s\n" "$(render_resolv)") /etc/resolv.conf; then
    echo "[dns-monitor] reset /etc/resolv.conf"
    write_resolv
  fi
  sleep "${INTERVAL}"
done

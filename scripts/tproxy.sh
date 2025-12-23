#!/bin/bash
set -euo pipefail

ACTION="${1:-apply}"   # apply | remove
TPROXY_PORT="${TPROXY_PORT:-7893}"
MARK="${MARK:-0x1}"
TABLE="${TABLE:-100}"

# If EXCLUDE_CIDRS is provided (space-separated CIDRs), use it.
# Otherwise use a conservative default (local/reserved/private).
if [[ -n "${EXCLUDE_CIDRS:-}" ]]; then
  read -r -a EXCLUDE_ARR <<< "${EXCLUDE_CIDRS}"
else
  EXCLUDE_ARR=(
    "127.0.0.0/8"
    "0.0.0.0/8"
    "10.0.0.0/8"
    "172.16.0.0/12"
    "192.168.0.0/16"
    "169.254.0.0/16"
    "224.0.0.0/4"
    "240.0.0.0/4"
  )
fi

# Optional source CIDRs to bypass.
if [[ -n "${EXCLUDE_SRC_CIDRS:-}" ]]; then
  read -r -a EXCLUDE_SRC_ARR <<< "${EXCLUDE_SRC_CIDRS}"
else
  EXCLUDE_SRC_ARR=()
fi

# Optional ingress interfaces to bypass.
if [[ -n "${EXCLUDE_IFACES:-}" ]]; then
  read -r -a EXCLUDE_IFACES_ARR <<< "${EXCLUDE_IFACES}"
else
  EXCLUDE_IFACES_ARR=()
fi

# Optional destination ports to bypass.
if [[ -n "${EXCLUDE_PORTS:-}" ]]; then
  read -r -a EXCLUDE_PORTS_ARR <<< "${EXCLUDE_PORTS}"
else
  EXCLUDE_PORTS_ARR=()
fi

ensure_sysctl() {
  sysctl -w net.ipv4.ip_forward=1 >/dev/null
  sysctl -w net.ipv4.conf.all.route_localnet=1 >/dev/null
  sysctl -w net.ipv4.conf.all.rp_filter=0 >/dev/null
  sysctl -w net.ipv4.conf.default.rp_filter=0 >/dev/null
}

remove_rules() {
  iptables -t mangle -D PREROUTING -j CLASH_TPROXY 2>/dev/null || true

  iptables -t mangle -F CLASH_TPROXY 2>/dev/null || true
  iptables -t mangle -X CLASH_TPROXY 2>/dev/null || true

  ip rule del pref 100 fwmark ${MARK} table ${TABLE} 2>/dev/null || true
  ip rule del fwmark ${MARK} table ${TABLE} 2>/dev/null || true
  ip route flush table ${TABLE} 2>/dev/null || true
}

apply_rules() {
  ensure_sysctl
  remove_rules

  iptables -t mangle -N CLASH_TPROXY

  # PREROUTING only: proxy forwarded/inbound traffic.
  # Intentionally DO NOT hook OUTPUT, so local-originated traffic is not proxied.
  for iface in "${EXCLUDE_IFACES_ARR[@]}"; do
    iptables -t mangle -A CLASH_TPROXY -i "${iface}" -j RETURN
  done
  for cidr in "${EXCLUDE_SRC_ARR[@]}"; do
    iptables -t mangle -A CLASH_TPROXY -s "${cidr}" -j RETURN
  done
  for cidr in "${EXCLUDE_ARR[@]}"; do
    iptables -t mangle -A CLASH_TPROXY -d "${cidr}" -j RETURN
  done
  for port in "${EXCLUDE_PORTS_ARR[@]}"; do
    iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${port}" -j RETURN
    iptables -t mangle -A CLASH_TPROXY -p udp --dport "${port}" -j RETURN
    iptables -t mangle -A CLASH_TPROXY -p tcp --sport "${port}" -j RETURN
    iptables -t mangle -A CLASH_TPROXY -p udp --sport "${port}" -j RETURN
  done
  iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${TPROXY_PORT}" -j RETURN
  iptables -t mangle -A CLASH_TPROXY -p udp --dport "${TPROXY_PORT}" -j RETURN

  iptables -t mangle -A CLASH_TPROXY -p tcp -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
  iptables -t mangle -A CLASH_TPROXY -p udp -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"

  iptables -t mangle -A PREROUTING -j CLASH_TPROXY

  # policy routing for marked packets (mark is set by TPROXY in PREROUTING)
  ip rule add pref 100 fwmark ${MARK} table ${TABLE}
  ip route add local 0.0.0.0/0 dev lo table ${TABLE}
}

case "${ACTION}" in
  apply)  apply_rules ;;
  remove) remove_rules ;;
  *) echo "usage: $0 apply|remove" && exit 1 ;;
esac

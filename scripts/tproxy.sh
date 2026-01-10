#!/bin/bash
set -euo pipefail

ACTION="${1:-apply}"   # apply | remove
TPROXY_PORT="${TPROXY_PORT:-7893}"
MARK="${MARK:-0x1}"
TABLE="${TABLE:-100}"

# PROTOCOL: which protocols to proxy (tcp, udp, or tcp+udp)
# Default: tcp+udp (proxy both TCP and UDP)
PROTOCOL="${PROTOCOL:-tcp+udp}"

# USE_CONNTRACK: use conntrack to match connection original source (ctorigsrc)
# When true, only proxy connections initiated FROM PROXY_CIDRS (not external connections)
# When false, use simple -s matching (backward compatible)
# Default: false
USE_CONNTRACK="${USE_CONNTRACK:-false}"

# PROXY_CIDRS: space-separated CIDRs of source addresses to proxy
# Only traffic FROM these sources will be proxied, everything else bypasses
if [[ -n "${PROXY_CIDRS:-}" ]]; then
  read -r -a PROXY_ARR <<< "${PROXY_CIDRS}"
else
  PROXY_ARR=()
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

  # Apply exclusions that apply in both modes
  for iface in "${EXCLUDE_IFACES_ARR[@]}"; do
    iptables -t mangle -A CLASH_TPROXY -i "${iface}" -j RETURN
  done

  # Exclude connections from specified source CIDRs (e.g., main gateway)
  # Method depends on USE_CONNTRACK setting
  for cidr in "${EXCLUDE_SRC_ARR[@]}"; do
    if [[ "${USE_CONNTRACK}" == "true" ]]; then
      # Use conntrack to match the original source of the connection
      iptables -t mangle -A CLASH_TPROXY -m conntrack --ctstate NEW,ESTABLISHED,RELATED --ctorigsrc "${cidr}" -j RETURN
    else
      # Use simple source address matching (backward compatible)
      iptables -t mangle -A CLASH_TPROXY -s "${cidr}" -j RETURN
    fi
  done

  for port in "${EXCLUDE_PORTS_ARR[@]}"; do
    iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${port}" -j RETURN
    iptables -t mangle -A CLASH_TPROXY -p udp --dport "${port}" -j RETURN
    iptables -t mangle -A CLASH_TPROXY -p tcp --sport "${port}" -j RETURN
    iptables -t mangle -A CLASH_TPROXY -p udp --sport "${port}" -j RETURN
  done
  iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${TPROXY_PORT}" -j RETURN
  iptables -t mangle -A CLASH_TPROXY -p udp --dport "${TPROXY_PORT}" -j RETURN

  # Only proxy traffic FROM specified source CIDRs
  # Method depends on USE_CONNTRACK setting:
  # - true:  Use conntrack to only proxy connections initiated FROM these CIDRs
  # - false: Use simple -s matching (backward compatible, but may proxy external connections)
  for cidr in "${PROXY_ARR[@]}"; do
    if [[ "${USE_CONNTRACK}" == "true" ]]; then
      # Conntrack mode: only proxy connections initiated from this CIDR
      # This prevents proxying externally-initiated, locally-responded connections
      ct_match="-m conntrack --ctstate NEW,ESTABLISHED,RELATED --ctorigsrc \"${cidr}\""
    else
      # Legacy mode: simple source address matching
      ct_match="-s \"${cidr}\""
    fi

    if [[ "${PROTOCOL}" == "tcp" ]]; then
      # Proxy only TCP
      iptables -t mangle -A CLASH_TPROXY -p tcp ${ct_match} -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
    elif [[ "${PROTOCOL}" == "udp" ]]; then
      # Proxy only UDP
      iptables -t mangle -A CLASH_TPROXY -p udp ${ct_match} -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
    else
      # Proxy both TCP and UDP (default)
      iptables -t mangle -A CLASH_TPROXY -p tcp ${ct_match} -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
      iptables -t mangle -A CLASH_TPROXY -p udp ${ct_match} -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
    fi
  done

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

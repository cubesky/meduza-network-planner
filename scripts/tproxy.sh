#!/bin/bash
set -euo pipefail

ACTION="${1:-apply}"   # apply | remove
TPROXY_PORT="${TPROXY_PORT:-7893}"
MARK="${MARK:-0x1}"
TABLE="${TABLE:-100}"

# PROTOCOL: which protocols to proxy (tcp, udp, or tcp+udp)
# Default: tcp+udp (proxy both TCP and UDP)
PROTOCOL="${PROTOCOL:-tcp+udp}"

# USE_CONNTRACK: use conntrack for exclusion rules (EXCLUDE_SRC_CIDRS)
# When true, exclude rules use --ctorigsrc to match connection original source
#   - This prevents proxying connections initiated from excluded sources (e.g., main gateway)
#   - Useful for bypass gateway scenarios to exclude port forwarding responses
# When false, exclude rules use simple -s matching
# Note: Proxy rules always use simple -s matching for reliability
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

  # Parse and apply port exclusions with optional direction and protocol
  # Format: [(in|out):][(tcp|udp):]<port>
  # Examples:
  #   "22"           - exclude TCP/UDP port 22 (both directions)
  #   "tcp:22"       - exclude TCP port 22 (both directions)
  #   "udp:53"       - exclude UDP port 53 (both directions)
  #   "in:80"        - exclude inbound (dport) port 80 (TCP/UDP)
  #   "out:8080"     - exclude outbound (sport) port 8080 (TCP/UDP)
  #   "in:tcp:443"   - exclude inbound TCP port 443
  #   "out:udp:123"  - exclude outbound UDP port 123
  #
  # Note: Port exclusions are automatically filtered based on PROTOCOL setting:
  #   - If PROTOCOL=tcp, UDP-specific exclusions (udp:53) are ignored
  #   - If PROTOCOL=udp, TCP-specific exclusions (tcp:22) are ignored
  #   - Unspecified protocol exclusions (22, in:80) apply to enabled protocols only
  for port_spec in "${EXCLUDE_PORTS_ARR[@]}"; do
    # Parse direction and protocol
    local direction=""
    local protocol=""
    local port="${port_spec}"

    # Check if contains colon (has direction or protocol spec)
    if [[ "${port_spec}" == *:* ]]; then
      local prefix="${port_spec%%:*}"
      local rest="${port_spec#*:}"

      if [[ "${prefix}" == "in" || "${prefix}" == "out" ]]; then
        direction="${prefix}"
        # Check if protocol is also specified
        if [[ "${rest}" == *:* ]]; then
          local proto_prefix="${rest%%:*}"
          local port_num="${rest#*:}"
          if [[ "${proto_prefix}" == "tcp" || "${proto_prefix}" == "udp" ]]; then
            protocol="${proto_prefix}"
            port="${port_num}"
          else
            port="${rest}"
          fi
        else
          port="${rest}"
        fi
      elif [[ "${prefix}" == "tcp" || "${prefix}" == "udp" ]]; then
        protocol="${prefix}"
        port="${rest}"
      fi
    fi

    # Skip if protocol is specified but doesn't match PROTOCOL setting
    if [[ -n "${protocol}" ]]; then
      if [[ "${PROTOCOL}" == "tcp" && "${protocol}" == "udp" ]]; then
        # Skip UDP-specific exclusion when only TCP is enabled
        continue
      fi
      if [[ "${PROTOCOL}" == "udp" && "${protocol}" == "tcp" ]]; then
        # Skip TCP-specific exclusion when only UDP is enabled
        continue
      fi
    fi

    # Apply exclusion rules based on parsed spec
    if [[ -n "${protocol}" ]]; then
      # Protocol specified (already filtered above, so it matches PROTOCOL)
      if [[ "${direction}" == "in" ]]; then
        # Inbound only
        iptables -t mangle -A CLASH_TPROXY -p "${protocol}" --dport "${port}" -j RETURN
      elif [[ "${direction}" == "out" ]]; then
        # Outbound only
        iptables -t mangle -A CLASH_TPROXY -p "${protocol}" --sport "${port}" -j RETURN
      else
        # Both directions
        iptables -t mangle -A CLASH_TPROXY -p "${protocol}" --dport "${port}" -j RETURN
        iptables -t mangle -A CLASH_TPROXY -p "${protocol}" --sport "${port}" -j RETURN
      fi
    else
      # Protocol not specified (apply to enabled protocols based on PROTOCOL)
      if [[ "${direction}" == "in" ]]; then
        # Inbound only
        if [[ "${PROTOCOL}" == "tcp" ]]; then
          iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${port}" -j RETURN
        elif [[ "${PROTOCOL}" == "udp" ]]; then
          iptables -t mangle -A CLASH_TPROXY -p udp --dport "${port}" -j RETURN
        else
          # tcp+udp (default)
          iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p udp --dport "${port}" -j RETURN
        fi
      elif [[ "${direction}" == "out" ]]; then
        # Outbound only
        if [[ "${PROTOCOL}" == "tcp" ]]; then
          iptables -t mangle -A CLASH_TPROXY -p tcp --sport "${port}" -j RETURN
        elif [[ "${PROTOCOL}" == "udp" ]]; then
          iptables -t mangle -A CLASH_TPROXY -p udp --sport "${port}" -j RETURN
        else
          # tcp+udp (default)
          iptables -t mangle -A CLASH_TPROXY -p tcp --sport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p udp --sport "${port}" -j RETURN
        fi
      else
        # Both directions
        if [[ "${PROTOCOL}" == "tcp" ]]; then
          iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p tcp --sport "${port}" -j RETURN
        elif [[ "${PROTOCOL}" == "udp" ]]; then
          iptables -t mangle -A CLASH_TPROXY -p udp --dport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p udp --sport "${port}" -j RETURN
        else
          # tcp+udp (default, legacy behavior)
          iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p udp --dport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p tcp --sport "${port}" -j RETURN
          iptables -t mangle -A CLASH_TPROXY -p udp --sport "${port}" -j RETURN
        fi
      fi
    fi
  done
  iptables -t mangle -A CLASH_TPROXY -p tcp --dport "${TPROXY_PORT}" -j RETURN
  iptables -t mangle -A CLASH_TPROXY -p udp --dport "${TPROXY_PORT}" -j RETURN

  # Only proxy traffic FROM specified source CIDRs
  # Always use simple -s matching for reliability (works in all network topologies)
  for cidr in "${PROXY_ARR[@]}"; do
    if [[ "${PROTOCOL}" == "tcp" ]]; then
      # Proxy only TCP
      iptables -t mangle -A CLASH_TPROXY -p tcp -s "${cidr}" -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
    elif [[ "${PROTOCOL}" == "udp" ]]; then
      # Proxy only UDP
      iptables -t mangle -A CLASH_TPROXY -p udp -s "${cidr}" -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
    else
      # Proxy both TCP and UDP (default)
      iptables -t mangle -A CLASH_TPROXY -p tcp -s "${cidr}" -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
      iptables -t mangle -A CLASH_TPROXY -p udp -s "${cidr}" -j TPROXY --on-port "${TPROXY_PORT}" --tproxy-mark "${MARK}/${MARK}"
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

#!/bin/bash
set -euo pipefail

: "${NODE_ID:?NODE_ID required}"

mkdir -p /run/openvpn /run/easytier /run/clash
mkdir -p /etc/openvpn/generated /etc/clash

chown -R frr:frr /etc/frr
chmod 640 /etc/frr/* || true

# Optional: override container default gateway (upstream).
# Use-case: LAN clients use container IP as DHCP gateway, but container egress should go to an upstream GW.
if [[ -n "${DEFAULT_GW:-}" ]]; then
  if [[ -n "${DEFAULT_GW_DEV:-}" ]]; then
    ip route replace default via "${DEFAULT_GW}" dev "${DEFAULT_GW_DEV}"
  else
    ip route replace default via "${DEFAULT_GW}"
  fi
fi

# FRR must be up before any transparent proxy rules are applied.
/usr/lib/frr/frrinit.sh start

# Start Clash (config will be written & reloaded by watcher)
mihomo -d /etc/clash >/var/log/clash.log 2>&1 &
echo $! >/run/clash/mihomo.pid

exec python3 /watcher.py

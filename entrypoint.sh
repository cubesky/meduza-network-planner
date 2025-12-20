#!/bin/bash
set -euo pipefail

: "${NODE_ID:?NODE_ID required}"

mkdir -p /run/openvpn /run/easytier /run/clash /run/tinc
mkdir -p /etc/openvpn/generated /etc/clash /etc/tinc

chown -R frr:frr /etc/frr
chmod 640 /etc/frr/* || true

# Ensure Clash controller API is reachable from outside the container.
python3 - <<'PY'
import yaml

path = "/clash/base.yaml"
with open(path, encoding="utf-8") as f:
    cfg = yaml.safe_load(f) or {}

ec = cfg.get("external-controller")
if ec == "127.0.0.1:9090":
    cfg["external-controller"] = "0.0.0.0:9090"
cfg.setdefault("allow-lan", True)

with open(path, "w", encoding="utf-8") as f:
    yaml.safe_dump(cfg, f, sort_keys=False, allow_unicode=True)
PY

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

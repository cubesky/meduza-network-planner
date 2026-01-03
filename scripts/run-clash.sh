#!/bin/bash

CFG="/etc/clash/config.yaml"
DATA_DIR="/data/clash"
PROVIDERS_DIR="/etc/clash/providers"
PID_FILE="/run/clash/mihomo.pid"

mkdir -p "${DATA_DIR}"
mkdir -p "${PROVIDERS_DIR}"
mkdir -p "$(dirname "${PID_FILE}")"

if [ ! -s "${CFG}" ]; then
    echo "error: clash config missing or empty: ${CFG}" >&2
    exit 1
fi

# 1. 下载 GeoX 文件如果配置了
if ! python3 - <<'PY'
import os
import sys
import yaml
from urllib.parse import urlparse

cfg_path = "/etc/clash/config.yaml"
data_dir = "/data/clash"

try:
    with open(cfg_path, encoding="utf-8") as f:
        cfg = yaml.safe_load(f) or {}
except FileNotFoundError:
    sys.exit(0)

geox = cfg.get("geox-url")
if not isinstance(geox, dict):
    sys.exit(0)

urls = []
for v in geox.values():
    if isinstance(v, str) and v.strip():
        urls.append(v.strip())

for url in urls:
    u = urlparse(url)
    name = os.path.basename(u.path)
    if not name:
        continue
    out_path = os.path.join(data_dir, name)
    os.system(f"curl -fL --retry 2 --connect-timeout 10 -o '{out_path}' '{url}'")
PY
then
    echo "error: geox download failed" >&2
    exit 1
fi

# 2. 预处理 proxy-provider: 下载并提取 IP
echo "[*] 预处理 proxy-providers..." >&2
if ! python3 /usr/local/bin/preprocess-clash.py "${CFG}" "${PROVIDERS_DIR}"; then
    echo "error: preprocess-clash failed" >&2
    exit 1
fi

# 3. 如果存在代理服务器 IP 列表,创建 ipset
PROXY_IPS_FILE="${PROVIDERS_DIR}/proxy_servers.txt"
if [ -f "${PROXY_IPS_FILE}" ]; then
    echo "[*] 创建 ipset for proxy servers..." >&2

    # 删除旧的 ipset (如果存在)
    ipset destroy proxy-servers 2>/dev/null || true

    # 创建新的 ipset
    ipset create proxy-servers hash:ip

    # 添加所有 IP 到 ipset
    while read -r ip; do
        [ -n "$ip" ] && ipset add proxy-servers "$ip"
    done < "${PROXY_IPS_FILE}"

    echo "[✓] ipset 创建完成" >&2

    # 4. 添加 iptables 规则跳过代理服务器 IP
    # 这需要在 TPROXY 规则之前添加
    IPTABLES_CHECK="iptables -t mangle -C CLASH_TPROXY -m set --match-set proxy-servers src -j RETURN 2>/dev/null"
    if ! $IPTABLES_CHECK; then
        # 在 CLASH_TPROXY 链的开头插入规则,跳过来自代理服务器的流量
        iptables -t mangle -I CLASH_TPROXY -m set --match-set proxy-servers src -j RETURN
        iptables -t mangle -I CLASH_TPROXY -m set --match-set proxy-servers dst -j RETURN
        echo "[✓] 已添加 iptables 规则跳过代理服务器" >&2
    fi
else
    echo "[!] 未找到代理服务器 IP 列表" >&2
fi

# Clean up old PID file
rm -f "${PID_FILE}"

# Start mihomo in background, capture PID, and wait
mihomo -d /etc/clash &
MIHOMO_PID=$!
echo ${MIHOMO_PID} > "${PID_FILE}"

# Trap signals to ensure PID file cleanup
trap 'rm -f "${PID_FILE}"; exit' INT TERM EXIT

# Wait for mihomo process
wait ${MIHOMO_PID}
EXIT_CODE=$?

if [ ${EXIT_CODE} -ne 0 ]; then
    echo "error: mihomo exited with code ${EXIT_CODE}" >&2
fi

# Clean up PID file on exit
rm -f "${PID_FILE}"
exit ${EXIT_CODE}

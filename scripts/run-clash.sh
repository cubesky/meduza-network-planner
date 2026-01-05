#!/bin/bash

CFG="/etc/clash/config.yaml"
DATA_DIR="/data/clash"
PID_FILE="/run/clash/mihomo.pid"

mkdir -p "${DATA_DIR}"
mkdir -p "$(dirname "${PID_FILE}")"

# Download GeoX files if configured
python3 - <<'PY'
import os
import sys
import subprocess
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
    subprocess.run(
        ["curl", "-fL", "--retry", "2", "--connect-timeout", "10", "-o", out_path, url],
        check=False,
        capture_output=True
    )
PY

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

# Clean up PID file on exit
rm -f "${PID_FILE}"
exit ${EXIT_CODE}


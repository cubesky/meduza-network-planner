#!/bin/bash
set -euo pipefail

CFG="/etc/clash/config.yaml"
DATA_DIR="/data/clash"

mkdir -p "${DATA_DIR}"

python3 - <<'PY'
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

exec mihomo -d /etc/clash

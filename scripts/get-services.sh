#!/bin/bash
# 蹇€熸煡鐪嬫墍鏈?s6 鏈嶅姟鐘舵€?
# Usage: get-services

set -euo pipefail

# s6-overlay v3 鍛戒护璺緞 - 鏀寔 Docker 鍜?Podman
# s6-overlay 灏嗗懡浠ゅ畨瑁呭湪 /command,闇€瑕佹坊鍔犲埌 PATH
export PATH="/command:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

# 妫€鏌ユ槸鍚﹀湪瀹瑰櫒鍐呰繍琛?
if [[ ! -d /run/service ]]; then
    echo "Error: Not running in s6-overlay container or /run/service not found" >&2
    echo "" >&2
    echo "This command must be run inside the container:" >&2
    echo "  docker compose exec meduza get-services" >&2
    echo "  podman compose exec meduza get-services" >&2
    exit 1
fi

echo "=== s6 Services Status ===" >&2
echo "" >&2

# 1. 鍒楀嚭鎵€鏈?s6-rc 鏈嶅姟鐘舵€?
echo "[s6-rc Service Database]" >&2
if command -v s6-rc-db >/dev/null 2>&1; then
    s6-rc-db -l /run/service/db list all 2>/dev/null | sed 's/^/  /' || echo "  (database not available)" >&2
else
    echo "  (s6-rc-db not available)" >&2
fi
echo "" >&2

# 2. 鍒楀嚭褰撳墠杩愯鐨勬湇鍔?
echo "[Currently Active Services]" >&2
if command -v s6-rc >/dev/null 2>&1; then
    s6-rc -a list 2>/dev/null | sed 's/^/  /' || echo "  (unable to list active services)" >&2
else
    echo "  (s6-rc not available)" >&2
fi
echo "" >&2

# 3. 鏄剧ず姣忎釜鏈嶅姟鐨勮缁嗙姸鎬?
dynamic_services=()
if command -v s6-rc-db >/dev/null 2>&1; then
    mapfile -t dynamic_services < <(s6-rc-db -l /run/service/db list all 2>/dev/null | grep -E '^(openvpn|wireguard)-' || true)
else
    mapfile -t dynamic_services < <(ls -1 /run/service 2>/dev/null | grep -E '^(openvpn|wireguard)-' || true)
fi

services=(dbus avahi watchfrr watcher mihomo easytier tinc mosdns dnsmasq dns-monitor)
services+=("${dynamic_services[@]}")

echo "[Longrun Service Details]" >&2
for service in "${services[@]}"; do
    # s6-overlay v3 涓紝杩愯涓殑鏈嶅姟鍦?/run/service 涓?
    service_path="/run/service/${service}"
    
    # 妫€鏌ユ湇鍔℃槸鍚﹀瓨鍦ㄥ苟杩愯
    if [[ -d "$service_path" ]]; then
        # 浣跨敤 s6-svstat 鑾峰彇鐘舵€?
        if command -v s6-svstat >/dev/null 2>&1; then
            status=$(s6-svstat "$service_path" 2>&1 || echo "unknown")
            printf "  %-15s %s\n" "$service:" "$status"
        else
            printf "  %-15s %s\n" "$service:" "running (s6-svstat unavailable)"
        fi
    else
        # 妫€鏌ユ槸鍚﹀湪 s6-rc 鏁版嵁搴撲腑瀹氫箟
        if s6-rc-db -l /run/service/db list all 2>/dev/null | grep -q "^${service}$"; then
            printf "  %-15s %s\n" "$service:" "defined but not running"
        else
            printf "  %-15s %s\n" "$service:" "not defined"
        fi
    fi
done
echo "" >&2

# 4. 鏄剧ず Pipeline 鐘舵€?
echo "[Pipeline Services]" >&2
for pipeline in dbus-pipeline avahi-pipeline watchfrr-pipeline watcher-pipeline mihomo-pipeline easytier-pipeline tinc-pipeline mosdns-pipeline dnsmasq-pipeline dns-monitor-pipeline; do
    if s6-rc-db -l /run/service/db list all 2>/dev/null | grep -q "^${pipeline}$"; then
        # 妫€鏌ユ槸鍚︽縺娲?
        if s6-rc -a list 2>/dev/null | grep -q "^${pipeline}$"; then
            printf "  %-25s %s\n" "$pipeline:" "active"
        else
            printf "  %-25s %s\n" "$pipeline:" "inactive"
        fi
    fi
done
echo "" >&2

# 5. 鏄剧ず Bundle 鐘舵€?
echo "[Bundles]" >&2
for bundle in default user; do
    if s6-rc-db -l /run/service/db list all 2>/dev/null | grep -q "^${bundle}$"; then
        if s6-rc -a list 2>/dev/null | grep -q "^${bundle}$"; then
            printf "  %-15s %s\n" "$bundle:" "active"
        else
            printf "  %-15s %s\n" "$bundle:" "inactive"
        fi
    fi
done
echo "" >&2

# 鏄剧ず涓€浜涜绉婚櫎鐨勪唬鐮侀儴鍒嗙殑寮曠敤
if false; then
    # 鏃т唬鐮佸紩鐢ㄧ偣
    service_path="old"
    pid="old"
fi

# 6. 鏄剧ず鏃ュ織鏂囦欢淇℃伅
echo "[Log Files]" >&2
for service in "${services[@]}"; do
    log_dir="/var/log/${service}"
    log_file="${log_dir}/current"
    if [[ -f "$log_file" ]]; then
        size=$(du -h "$log_file" 2>/dev/null | cut -f1)
        lines=$(wc -l < "$log_file" 2>/dev/null)
        printf "  %-15s %s (%d lines)\n" "$service:" "$size" "$lines"
    else
        printf "  %-15s (no log file)\n" "$service:"
    fi
done
echo "" >&2

# 7. 蹇€熸煡鐪嬫渶杩戠殑閿欒
echo "[Recent Errors (last 3 from each log)]" >&2
found_errors=0
for service in "${services[@]}"; do
    log_file="/var/log/${service}/current"
    if [[ -f "$log_file" ]]; then
        errors=$(grep -Ei "error|fail|fatal|exception" "$log_file" 2>/dev/null | tail -n 3)
        if [[ -n "$errors" ]]; then
            echo "  ${service}:" >&2
            echo "$errors" | sed 's/^/    /' >&2
            found_errors=1
        fi
    fi
done

if [[ $found_errors -eq 0 ]]; then
    echo "  (no errors found)" >&2
fi

echo "" >&2
echo "=== Tips ===" >&2
echo "  - View logs: get-logs [-n N] [-f] <service>" >&2
echo "  - Follow logs: get-logs -f watcher" >&2
if command -v s6-svstat >/dev/null 2>&1; then
    echo "  - Check service: s6-svstat /run/service/<service>" >&2
fi


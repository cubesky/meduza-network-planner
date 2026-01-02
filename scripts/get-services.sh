#!/bin/bash
# 快速查看所有 s6 服务状态
# Usage: get-services

set -euo pipefail

echo "=== s6 Services Status ===" >&2
echo "" >&2

# 1. 列出所有运行中的服务
echo "[Running Services]" >&2
s6-rc -a 2>/dev/null || echo "  (no services running)" >&2
echo "" >&2

# 2. 列出所有已定义的服务
echo "[All Defined Services]" >&2
s6-rc listall 2>/dev/null || echo "  (no services defined)" >&2
echo "" >&2

# 3. 显示每个服务的详细状态
echo "[Service Details]" >&2
for service in watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    service_path="/etc/s6-overlay/sv/${service}"
    if [[ -d "$service_path" ]]; then
        status=$(s6-svstat "$service_path" 2>/dev/null || echo "unknown")
        printf "  %-15s %s\n" "$service:" "$status"

        # 显示 PID (如果正在运行)
        if [[ -f "$service_path/supervise/pid" ]]; then
            pid=$(cat "$service_path/supervise/pid" 2>/dev/null)
            if [[ -n "$pid" ]]; then
                printf "    %-15s PID %s\n" "" "$pid"
            fi
        fi
    fi
done
echo "" >&2

# 4. 显示日志文件信息
echo "[Log Files]" >&2
for service in watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    log_file="/var/log/${service}.out.log"
    if [[ -f "$log_file" ]]; then
        size=$(du -h "$log_file" 2>/dev/null | cut -f1)
        lines=$(wc -l < "$log_file" 2>/dev/null)
        printf "  %-15s %s (%d lines)\n" "$service:" "$size" "$lines"
    else
        printf "  %-15s (no log file)\n" "$service:"
    fi
done
echo "" >&2

# 5. 快速查看最近的错误
echo "[Recent Errors (last 10 lines from each log)]" >&2
for service in watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    log_file="/var/log/${service}.out.log"
    if [[ -f "$log_file" ]]; then
        errors=$(grep -i "error\|fail\|fatal\|exception" "$log_file" 2>/dev/null | tail -n 3)
        if [[ -n "$errors" ]]; then
            echo "  ${service}:" >&2
            echo "$errors" | sed 's/^/    /' >&2
        fi
    fi
done

if [[ -z "$(grep -i "error\|fail\|fatal\|exception" /var/log/*.out.log 2>/dev/null)" ]]; then
    echo "  (no errors found)" >&2
fi

echo "" >&2
echo "=== Tips ===" >&2
echo "  - View logs: get-logs [-f] <service>" >&2
echo "  - Follow logs: get-logs -f watcher" >&2
echo "  - Check service: s6-svstat /etc/s6-overlay/sv/<service>" >&2

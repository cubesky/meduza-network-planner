#!/bin/bash
# 快速查看 s6 服务日志
# Usage: get-logs [-f] [-n lines] <service>
# Supports: Docker and Podman

set -euo pipefail

# s6-overlay v3 命令路径 - 支持 Docker 和 Podman
export PATH="/command:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

FOLLOW=false
LINES=100
SERVICE=""

# 解析参数
while [[ $# -gt 0 ]]; do
    case $1 in
        -f|--follow)
            FOLLOW=true
            shift
            ;;
        -n|--lines)
            LINES="$2"
            shift 2
            ;;
        *)
            SERVICE="$1"
            shift
            ;;
    esac
done

if [[ -z "$SERVICE" ]]; then
    echo "Usage: get-logs [-f] [-n lines] <service>" >&2
    echo "" >&2
    echo "Examples:" >&2
    echo "  get-logs watcher           # 查看 watcher 最近 100 行日志" >&2
    echo "  get-logs -n 50 mihomo      # 查看 mihomo 最近 50 行日志" >&2
    echo "  get-logs -f easytier       # 实时跟踪 easytier 日志" >&2
    echo "  get-logs -n 20 -f dnsmasq  # 显示 20 行后实时跟踪 dnsmasq" >&2
    echo "" >&2
    echo "Options:" >&2
    echo "  -f, --follow      实时跟踪日志 (类似 tail -f)" >&2
    echo "  -n, --lines N     显示最近 N 行日志 (默认: 100)" >&2
    echo "" >&2
    echo "Available services:" >&2
    echo "  watcher, mihomo, easytier, tinc, mosdns, dnsmasq, dns-monitor" >&2
    echo "" >&2
    echo "This command must be run inside the container:" >&2
    echo "  docker compose exec meduza get-logs <service>" >&2
    echo "  podman compose exec meduza get-logs <service>" >&2
    exit 1
fi

# s6-log 使用目录而不是单个文件
LOG_DIR="/var/log/${SERVICE}"
LOG_FILE="${LOG_DIR}/current"

if [[ ! -f "$LOG_FILE" ]]; then
    echo "Error: Log file not found: $LOG_FILE" >&2
    echo "Available log directories:" >&2
    ls -1d /var/log/*/ 2>/dev/null | sed 's|/var/log/||; s|/$||' | grep -v '^s6-' || echo "  (none)" >&2
    exit 1
fi

echo "=== ${SERVICE} logs (${LOG_FILE}) ===" >&2
echo "" >&2

if [[ "$FOLLOW" == "true" ]]; then
    # 先显示指定行数,然后进入跟踪模式
    tail -n "$LINES" "$LOG_FILE"
    echo "" >&2
    echo "=== Following log (Ctrl+C to exit) ===" >&2
    tail -f "$LOG_FILE"
else
    # 只显示指定行数
    tail -n "$LINES" "$LOG_FILE"
fi

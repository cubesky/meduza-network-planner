#!/bin/bash
# 快速查看所有 s6 服务状态
# Usage: get-services

set -euo pipefail

# s6-overlay v3 命令路径 - 支持 Docker 和 Podman
# s6-overlay 将命令安装在 /command,需要添加到 PATH
export PATH="/command:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

# 检查是否在容器内运行
if [[ ! -d /etc/s6-overlay/sv ]]; then
    echo "Error: Not running in s6-overlay container or /etc/s6-overlay/sv not found" >&2
    echo "" >&2
    echo "This command must be run inside the container:" >&2
    echo "  docker compose exec meduza get-services" >&2
    echo "  podman compose exec meduza get-services" >&2
    exit 1
fi

echo "=== s6 Services Status ===" >&2
echo "" >&2

# 1. 列出所有运行中的服务
echo "[Running Services]" >&2
if command -v s6-rc >/dev/null 2>&1; then
    s6-rc -a 2>/dev/null || echo "  (no services running)" >&2
else
    echo "  (s6-rc not available - checking services manually)" >&2
fi
echo "" >&2

# 2. 列出所有已定义的服务
echo "[All Defined Services]" >&2
if command -v s6-rc >/dev/null 2>&1; then
    s6-rc listall 2>/dev/null || echo "  (no services defined)" >&2
else
    # 手动列出服务目录
    for svc_dir in /etc/s6-overlay/sv/*/; do
        if [[ -d "$svc_dir" ]]; then
            svc_name=$(basename "$svc_dir")
            echo "  $svc_name"
        fi
    done
fi
echo "" >&2

# 3. 显示每个服务的详细状态
echo "[Service Details]" >&2
for service in watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    service_path="/etc/s6-overlay/sv/${service}"
    if [[ -d "$service_path" ]]; then
        # 检查 supervise 目录是否存在
        if [[ ! -d "$service_path/supervise" ]]; then
            status="no supervise dir"
            printf "  %-15s %s\n" "$service:" "$status"
            continue
        fi

        # 尝试使用 s6-svstat,如果失败则直接检查 supervise 目录
        if command -v s6-svstat >/dev/null 2>&1; then
            # 捕获 s6-svstat 错误,避免 "s6-supervise not running" 中断
            status=$(s6-svstat "$service_path" 2>&1)
            # 如果包含 "not running" 或其他错误,使用手动检查
            if [[ "$status" == *"not running"* ]] || [[ "$status" == *"unable"* ]]; then
                # 手动检查服务状态
                if [[ -f "$service_path/supervise/pid" ]]; then
                    pid=$(cat "$service_path/supervise/pid" 2>/dev/null)
                    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
                        status="up (pid $pid)"
                    else
                        status="down"
                    fi
                elif [[ -f "$service_path/down" ]]; then
                    status="disabled"
                else
                    status="not started"
                fi
            fi
        else
            # 手动检查服务状态
            if [[ -f "$service_path/supervise/pid" ]]; then
                pid=$(cat "$service_path/supervise/pid" 2>/dev/null)
                if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
                    # 计算运行时间 (如果可用)
                    status="up (pid $pid)"
                else
                    status="down"
                fi
            elif [[ -f "$service_path/down" ]]; then
                status="disabled"
            else
                status="not started"
            fi
        fi
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
found_errors=0
for service in watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    log_file="/var/log/${service}.out.log"
    if [[ -f "$log_file" ]]; then
        errors=$(grep -i "error\|fail\|fatal\|exception" "$log_file" 2>/dev/null | tail -n 3)
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
    echo "  - Check service: s6-svstat /etc/s6-overlay/sv/<service>" >&2
fi

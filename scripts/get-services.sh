#!/bin/bash
# 快速查看所有 s6 服务状态
# Usage: get-services

set -euo pipefail

# s6-overlay v3 命令路径 - 支持 Docker 和 Podman
# s6-overlay 将命令安装在 /command,需要添加到 PATH
export PATH="/command:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

# 检查是否在容器内运行
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

# 1. 列出所有 s6-rc 服务状态
echo "[s6-rc Service Database]" >&2
if command -v s6-rc-db >/dev/null 2>&1; then
    s6-rc-db -l /run/service/db list all 2>/dev/null | sed 's/^/  /' || echo "  (database not available)" >&2
else
    echo "  (s6-rc-db not available)" >&2
fi
echo "" >&2

# 2. 列出当前运行的服务
echo "[Currently Active Services]" >&2
if command -v s6-rc >/dev/null 2>&1; then
    s6-rc -a list 2>/dev/null | sed 's/^/  /' || echo "  (unable to list active services)" >&2
else
    echo "  (s6-rc not available)" >&2
fi
echo "" >&2

# 3. 显示每个服务的详细状态
echo "[Longrun Service Details]" >&2
for service in dbus avahi watchfrr watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    # s6-overlay v3 中，运行中的服务在 /run/service 下
    service_path="/run/service/${service}"
    
    # 检查服务是否存在并运行
    if [[ -d "$service_path" ]]; then
        # 使用 s6-svstat 获取状态
        if command -v s6-svstat >/dev/null 2>&1; then
            status=$(s6-svstat "$service_path" 2>&1 || echo "unknown")
            printf "  %-15s %s\n" "$service:" "$status"
        else
            printf "  %-15s %s\n" "$service:" "running (s6-svstat unavailable)"
        fi
    else
        # 检查是否在 s6-rc 数据库中定义
        if s6-rc-db -l /run/service/db list all 2>/dev/null | grep -q "^${service}$"; then
            printf "  %-15s %s\n" "$service:" "defined but not running"
        else
            printf "  %-15s %s\n" "$service:" "not defined"
        fi
    fi
done
echo "" >&2

# 4. 显示 Pipeline 状态
echo "[Pipeline Services]" >&2
for pipeline in dbus-pipeline avahi-pipeline watchfrr-pipeline watcher-pipeline mihomo-pipeline easytier-pipeline tinc-pipeline mosdns-pipeline dnsmasq-pipeline dns-monitor-pipeline; do
    if s6-rc-db -l /run/service/db list all 2>/dev/null | grep -q "^${pipeline}$"; then
        # 检查是否激活
        if s6-rc -a list 2>/dev/null | grep -q "^${pipeline}$"; then
            printf "  %-25s %s\n" "$pipeline:" "active"
        else
            printf "  %-25s %s\n" "$pipeline:" "inactive"
        fi
    fi
done
echo "" >&2

# 5. 显示 Bundle 状态
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

# 显示一些被移除的代码部分的引用
if false; then
    # 旧代码引用点
    service_path="old"
    pid="old"
fi

# 6. 显示日志文件信息
echo "[Log Files]" >&2
for service in dbus avahi watchfrr watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
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

# 7. 快速查看最近的错误
echo "[Recent Errors (last 3 from each log)]" >&2
found_errors=0
for service in dbus avahi watchfrr watcher mihomo easytier tinc mosdns dnsmasq dns-monitor; do
    log_file="/var/log/${service}/current"
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
    echo "  - Check service: s6-svstat /run/service/<service>" >&2
fi

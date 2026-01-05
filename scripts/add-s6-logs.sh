#!/bin/bash
# 为所有 s6 服务添加日志配置

set -euo pipefail

SERVICES=("watcher" "mihomo" "easytier" "tinc" "mosdns" "dnsmasq" "dns-monitor")

for service in "${SERVICES[@]}"; do
    service_dir="s6-services/${service}"
    if [ ! -d "$service_dir" ]; then
        echo "跳过 ${service} (目录不存在)"
        continue
    fi

    log_dir="${service_dir}/log"
    mkdir -p "$log_dir"

    # 创建日志运行脚本
    cat > "${log_dir}/run" <<'EOF'
#!/command/execlineb -P
s6-setenv logfile /var/log/SERVICE.out.log
s6-setenv maxbytes 10485760
s6-setenv maxfiles 10
exec s6-svlogd "${logfile}" "${maxbytes}" "${maxfiles}"
EOF

    # 替换 SERVICE 名称
    sed -i "s/SERVICE.out.log/${service}.out.log/" "${log_dir}/run"

    chmod +x "${log_dir}/run"
    echo "✓ 添加日志配置: ${service}"
done

echo "所有服务日志配置完成"

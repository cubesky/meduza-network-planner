#!/bin/bash
# Clash 性能诊断脚本

echo "=========================================="
echo "Clash 性能诊断工具"
echo "=========================================="
echo ""

# 1. DNS 延迟测试
echo "1. DNS 延迟测试"
echo "----------------------------"
for ns in "53:dnsmasq" "1153:MosDNS" "1053:Clash"; do
    port="${ns%%:*}"
    name="${ns##*:}"
    if time nslookup google.com 127.0.0.1:$port 2>&1 | grep -q "Address"; then
        latency=$(time nslookup google.com 127.0.0.1:$port 2>&1 | grep real)
        echo "  $name ($port): ✓"
    else
        echo "  $name ($port): ✗ 无响应"
    fi
done
echo ""

# 2. TPROXY 规则统计
echo "2. TPROXY 规则统计"
echo "----------------------------"
rules_count=$(iptables -t mangle -L CLASH_TPROXY -n 2>/dev/null | wc -l)
echo "  总规则数: $rules_count"
echo "  前10条规则:"
iptables -t mangle -L CLASH_TPROXY -n --line-numbers | head -12
echo ""

# 3. Clash 连接统计
echo "3. Clash 连接统计"
echo "----------------------------"
connections=$(netstat -an 2>/dev/null | grep :7893 | wc -l)
echo "  活动连接数: $connections"
echo ""

# 4. DNS 进程检查
echo "4. DNS 进程状态"
echo "----------------------------"
for proc in "dnsmasq" "mosdns" "mihomo"; do
    if pgrep -x "$proc" > /dev/null; then
        pid=$(pgrep -x "$proc")
        mem=$(ps -p "$pid" -o rss= 2>/dev/null || echo "N/A")
        echo "  $proc: ✓ 运行中 (PID: $pid, 内存: ${mem}KB)"
    else
        echo "  $proc: ✗ 未运行"
    fi
done
echo ""

# 5. 端口监听检查
echo "5. DNS 端口监听"
echo "----------------------------"
for port in 53 1153 1053; do
    if netstat -tuln 2>/dev/null | grep -q ":$port "; then
        proc=$(netstat -tuln 2>/dev/null | grep ":$port " | awk '{print $7}' | cut -d'/' -f1 | head -1)
        echo "  端口 $port: ✓ ($proc)"
    else
        echo "  端口 $port: ✗ 未监听"
    fi
done
echo ""

# 6. dnsmasq 配置检查
echo "6. dnsmasq 配置"
echo "----------------------------"
if [ -f /etc/dnsmasq.conf ]; then
    echo "  DNS 转发服务器:"
    grep "^server=" /etc/dnsmasq.conf | head -5
    forward_count=$(grep "^server=" /etc/dnsmasq.conf | wc -l)
    echo "  总转发服务器数: $forward_count"
    if [ $forward_count -gt 3 ]; then
        echo "  ⚠️  警告: 转发服务器过多,可能影响性能"
    fi
else
    echo "  ✗ /etc/dnsmasq.conf 不存在"
fi
echo ""

# 7. iptables 流量统计
echo "7. TPROXY 流量统计"
echo "----------------------------"
if iptables -t mangle -L CLASH_TPROXY -v -n >/dev/null 2>&1; then
    echo "  规则流量统计 (包/字节):"
    iptables -t mangle -L CLASH_TPROXY -v -n --line-numbers | head -15
else
    echo "  ✗ 无法读取 iptables 统计"
fi
echo ""

# 8. 最近错误日志
echo "8. 最近错误日志"
echo "----------------------------"
echo "  mihomo 错误:"
tail -20 /var/log/mihomo.err.log 2>/dev/null | grep -i "error\|warn\|fail" | tail -5 || echo "    无错误日志"
echo ""
echo "  mosdns 错误:"
tail -20 /var/log/mosdns.err.log 2>/dev/null | grep -i "error\|warn\|fail" | tail -5 || echo "    无错误日志"
echo ""

# 9. 系统资源
echo "9. 系统资源"
echo "----------------------------"
echo "  内存使用:"
free -m | awk 'NR==2{printf "    已用: %sMB / 总计: %sMB (%.1f%%)\n", $3, $2, ($3*100/$2)}'
echo "  CPU 负载:"
uptime | awk -F'load average:' '{print $2}'
echo ""

# 10. 代理测试
echo "10. 代理速度测试"
echo "----------------------------"
if command -v curl >/dev/null 2>&1; then
    echo "  测试 Google (通过代理):"
    if output=$(curl -w "@-" -o /dev/null -s "https://www.google.com" 2>&1 <<'EOF'
    time_total:       %{time_total}\n
EOF
); then
        total_sec=$(echo "$output" | grep "time_total" | awk '{print $2}')
        echo "    总时间: ${total_sec}s"
        if (( $(echo "$total_sec < 1.0" | bc -l 2>/dev/null || echo "0") )); then
            echo "    ✓ 速度良好"
        elif (( $(echo "$total_sec < 3.0" | bc -l 2>/dev/null || echo "0") )); then
            echo "    ⚠️  速度一般"
        else
            echo "    ✗ 速度缓慢"
        fi
    else
        echo "    ✗ 测试失败"
    fi
else
    echo "  curl 不可用,跳过测试"
fi
echo ""

echo "=========================================="
echo "诊断完成"
echo "=========================================="
echo ""
echo "建议优化方向:"
echo "1. 如果 DNS 延迟 > 10ms → 简化 DNS 链路"
echo "2. 如果 TPROXY 规则数 > 20 → 增加排除规则"
echo "3. 如果 Clash 连接数 > 1000 → 检查是否有连接泄漏"
echo "4. 查看详细优化建议: docs/performance-tuning.md"

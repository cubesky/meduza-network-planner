# 快速调试指南 - s6-overlay 服务

## 🚀 快速工具

### 查看所有服务状态
```bash
docker compose exec meduza get-services
```

**输出内容**:
- ✅ 运行中的服务列表
- ✅ 所有已定义的服务
- ✅ 每个服务的详细状态 (PID, 运行时间)
- ✅ 日志文件信息 (大小, 行数)
- ✅ 最近的错误信息 (从日志中提取)

### 查看服务日志
```bash
# 查看最近 100 行 (默认)
docker compose exec meduza get-logs watcher

# 查看最近 50 行
docker compose exec meduza get-logs -n 50 mihomo

# 实时跟踪日志 (Ctrl+C 退出)
docker compose exec meduza get-logs -f easytier

# 显示最近 20 行后实时跟踪
docker compose exec meduza get-logs -n 20 -f mosdns

# 查看其他服务
docker compose exec meduza get-logs dnsmasq
```

## 📋 常用命令速查

### 服务状态检查
```bash
# 快速方法 (推荐)
docker compose exec meduza get-services

# 传统方法
docker compose exec meduza s6-rc -a
docker compose exec meduza s6-svstat /etc/s6-overlay/sv/watcher
```

### 日志查看
```bash
# 快速方法 (推荐)
docker compose exec meduza get-logs watcher
docker compose exec meduza get-logs -n 50 watcher    # 指定行数
docker compose exec meduza get-logs -f watcher       # 跟踪模式
docker compose exec meduza get-logs -n 20 -f watcher # 显示20行后跟踪

# 传统方法
docker compose exec meduza tail -n 100 /var/log/watcher.out.log
docker compose exec meduza tail -f /var/log/watcher.out.log
```

### 服务控制
```bash
# 启动服务
docker compose exec meduza s6-rc -u watcher

# 停止服务
docker compose exec meduza s6-rc -d watcher

# 重启服务
docker compose exec meduza s6-rc -r watcher
```

## 🔍 故障排查流程

### 1. 容器状态检查
```bash
docker compose ps
```
**预期**: 状态为 `Up`

### 2. 服务状态概览
```bash
docker compose exec meduza get-services
```
**预期**: 看到所有服务的状态、PID、日志文件信息

### 3. 查看问题服务的日志
```bash
docker compose exec meduza get-logs watcher
docker compose exec meduza get-logs -f watcher  # 跟踪模式
```
**预期**: 看到服务启动和运行日志

### 4. 查看容器日志
```bash
docker compose logs meduza | tail -50
```
**预期**: 看到 s6 初始化日志

### 5. 进入容器手动调试
```bash
docker compose exec meduza bash
```

然后在容器内:
```bash
# 查看服务状态
s6-rc -a

# 查看服务详情
s6-svstat /etc/s6-overlay/sv/watcher

# 查看日志
tail -f /var/log/watcher.out.log

# 手动启动服务
s6-rc -u watcher
```

## 📝 可用服务列表

- `watcher` - 主协调服务
- `mihomo` - Clash Meta 代理
- `easytier` - EasyTier 网络覆盖
- `tinc` - Tinc VPN
- `mosdns` - DNS 转发器
- `dnsmasq` - 前端 DNS
- `dns-monitor` - DNS 监控

## 💡 使用示例

### 场景 1: 容器启动后检查状态
```bash
# 1. 启动容器
docker compose up -d

# 2. 等待 10 秒
sleep 10

# 3. 检查所有服务
docker compose exec meduza get-services

# 4. 如果有服务未运行,查看日志
docker compose exec meduza get-logs watcher
```

### 场景 2: Clash 代理问题调试
```bash
# 1. 查看 Clash 状态
docker compose exec meduza get-logs mihomo

# 2. 实时跟踪 Clash 日志
docker compose exec meduza get-logs -f mihomo

# 3. 查看 Clash 配置
docker compose exec meduza cat /etc/clash/config.yaml
```

### 场景 3: 网络问题调试
```bash
# 1. 查看所有服务状态
docker compose exec meduza get-services

# 2. 查看 EasyTier 日志
docker compose exec meduza get-logs easytier

# 3. 查看 Watcher 日志 (主协调服务)
docker compose exec meduza get-logs watcher

# 4. 查看 TPROXY 规则
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -n
```

### 场景 4: DNS 问题调试
```bash
# 1. 查看 MosDNS 日志
docker compose exec meduza get-logs mosdns

# 2. 查看 dnsmasq 日志
docker compose exec meduza get-logs dnsmasq

# 3. 查看配置
docker compose exec meduza cat /etc/dnsmasq.conf

# 4. 测试 DNS
docker compose exec meduza nslookup google.com
```

## ⚠️ 常见问题

### 问题: get-services 显示服务未运行
```bash
# 检查服务文件是否存在
docker compose exec meduza ls -la /etc/s6-overlay/sv/

# 手动启动服务
docker compose exec meduza s6-rc -u watcher

# 查看日志了解失败原因
docker compose exec meduza get-logs watcher
```

### 问题: 日志文件不存在
```bash
# 检查日志目录
docker compose exec meduza ls -la /var/log/

# 检查服务日志配置
docker compose exec meduza cat /etc/s6-overlay/sv/watcher/log/run
```

### 问题: 服务反复重启
```bash
# 查看服务日志
docker compose exec meduza get-logs -f watcher

# 查看服务状态
docker compose exec meduza s6-svstat /etc/s6-overlay/sv/watcher

# 手动运行服务脚本查找错误
docker compose exec meduza python3 /watcher.py
```

## 📚 更多信息

- **完整调试指南**: [S6-DEBUG-GUIDE.md](S6-DEBUG-GUIDE.md)
- **故障排查**: [S6-TROUBLESHOOTING.md](S6-TROUBLESHOOTING.md)
- **部署指南**: [DEPLOY-GUIDE.md](DEPLOY-GUIDE.md)

---

**提示**: 使用快速工具可以大幅提高调试效率! ⚡

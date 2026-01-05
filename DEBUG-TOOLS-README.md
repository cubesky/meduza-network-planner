# 🚀 调试工具快速上手

## 两个简单命令替代复杂的 s6 管理

### 安装
```bash
# 重新构建容器 (已包含工具)
docker compose build
docker compose up -d
```

### 使用

#### 1. 查看所有服务状态
```bash
docker compose exec meduza get-services
```

**输出**:
- ✅ 运行中的服务
- ✅ 服务 PID 和运行时间
- ✅ 日志文件大小和行数
- ✅ 最近的错误

#### 2. 查看服务日志
```bash
# 查看最近 100 行
docker compose exec meduza get-logs watcher

# 查看最近 50 行
docker compose exec meduza get-logs -n 50 mihomo

# 实时跟踪 (Ctrl+C 退出)
docker compose exec meduza get-logs -f easytier

# 显示最近 20 行后实时跟踪
docker compose exec meduza get-logs -n 20 -f dnsmasq
```

## 可用服务

```bash
get-logs watcher       # 主协调服务
get-logs mihomo        # Clash 代理
get-logs easytier      # EasyTier 网络
get-logs tinc          # Tinc VPN
get-logs mosdns        # DNS 转发
get-logs dnsmasq       # 前端 DNS
get-logs dns-monitor   # DNS 监控
```

## 对比

### 之前 (复杂)
```bash
docker compose exec meduza bash
s6-rc -a
s6-svstat /etc/s6-overlay/sv/watcher
tail -n 50 /var/log/watcher.out.log
tail -f /var/log/watcher.out.log
exit
```

### 现在 (简单)
```bash
docker compose exec meduza get-services
docker compose exec meduza get-logs -f watcher
```

## 常见用法

```bash
# 快速检查
docker compose exec meduza get-services

# 查看错误
docker compose exec meduza get-services | grep -A 10 "Recent Errors"

# 查看最近 50 行
docker compose exec meduza get-logs -n 50 watcher

# 显示 20 行后实时跟踪
docker compose exec meduza get-logs -n 20 -f mihomo

# 检查特定服务
docker compose exec meduza get-logs easytier
```

## 更多信息

- 📖 **完整文档**: [QUICK-DEBUG.md](QUICK-DEBUG.md)
- 📋 **命令速查**: [DEBUG-COMMANDS.md](DEBUG-COMMANDS.md)
- 🔧 **技术细节**: [DEBUG-TOOLS-SUMMARY.md](DEBUG-TOOLS-SUMMARY.md)

---

**就这么简单!** ⚡

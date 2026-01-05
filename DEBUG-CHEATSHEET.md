# 调试工具速查卡片

## ⚡ 两个核心命令

```bash
# 查看所有服务状态
docker compose exec meduza get-services

# 查看服务日志
docker compose exec meduza get-logs <service>              # 最近 100 行
docker compose exec meduza get-logs -n 50 <service>        # 最近 50 行
docker compose exec meduza get-logs -f <service>           # 跟踪模式
docker compose exec meduza get-logs -n 20 -f <service>     # 显示 20 行后跟踪
```

## 📋 服务列表

| 服务 | 说明 | 命令 |
|-----|------|-----|
| watcher | 主协调服务 | `get-logs watcher` |
| mihomo | Clash 代理 | `get-logs mihomo` |
| easytier | EasyTier 网络 | `get-logs easytier` |
| tinc | Tinc VPN | `get-logs tinc` |
| mosdns | DNS 转发 | `get-logs mosdns` |
| dnsmasq | 前端 DNS | `get-logs dnsmasq` |
| dns-monitor | DNS 监控 | `get-logs dns-monitor` |

## 🎯 常用场景

### 检查所有服务
```bash
docker compose exec meduza get-services
```

### 查看主服务日志
```bash
docker compose exec meduza get-logs watcher          # 最近 100 行
docker compose exec meduza get-logs -n 50 watcher     # 最近 50 行
```

### 实时监控 Clash
```bash
docker compose exec meduza get-logs -f mihomo                    # 直接跟踪
docker compose exec meduza get-logs -n 20 -f mihomo              # 显示 20 行后跟踪
```

### 查找错误
```bash
docker compose exec meduza get-services | grep -A 10 "Recent Errors"
```

### 检查 DNS
```bash
docker compose exec meduza get-logs dnsmasq
docker compose exec meduza get-logs mosdns
```

### 网络问题
```bash
docker compose exec meduza get-logs easytier
docker compose exec meduza get-logs mihomo
```

## 🔍 快速诊断

### 服务不运行
```bash
1. docker compose exec meduza get-services
2. docker compose exec meduza get-logs <service>
```

### 网络不通
```bash
1. docker compose exec meduza get-logs mihomo
2. docker compose exec meduza get-logs easytier
```

### DNS 失败
```bash
1. docker compose exec meduza get-logs dnsmasq
2. docker compose exec meduza get-logs mosdns
```

### 容器问题
```bash
1. docker compose ps
2. docker compose logs meduza
```

## 💡 提示

- 使用 `-f` 标志实时跟踪日志
- `get-services` 自动显示错误信息
- 所有命令可以从宿主机直接执行
- 日志文件位置: `/var/log/<service>.out.log`

## 📚 文档

- [QUICK-DEBUG.md](QUICK-DEBUG.md) - 快速调试指南
- [DEBUG-COMMANDS.md](DEBUG-COMMANDS.md) - 命令速查表
- [S6-DEBUG-GUIDE.md](S6-DEBUG-GUIDE.md) - 完整调试指南

---

**记住这两个命令就够了!** ⚡

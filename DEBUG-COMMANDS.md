# 调试命令速查表

## 一分钟快速检查

```bash
# 检查容器状态
docker compose ps

# 查看所有服务状态和日志文件信息
docker compose exec meduza get-services

# 查看主要服务日志
docker compose exec meduza get-logs watcher | tail -20
```

## 快速工具 vs 传统命令

| 任务 | 快速工具 ⚡ | 传统命令 |
|------|-----------|---------|
| 查看所有服务状态 | `get-services` | `s6-rc -a` + `s6-svstat ...` |
| 查看服务日志 | `get-logs watcher` | `tail /var/log/watcher.out.log` |
| 查看指定行数 | `get-logs -n 50 watcher` | `tail -n 50 /var/log/watcher.out.log` |
| 跟踪服务日志 | `get-logs -f watcher` | `tail -f /var/log/watcher.out.log` |
| 跟踪并显示初始行 | `get-logs -n 20 -f watcher` | `tail -n 20 && tail -f` |
| 查看服务详情 | `get-services` (包含) | `s6-svstat /etc/s6-overlay/sv/watcher` |
| 查看日志文件信息 | `get-services` (包含) | `ls -la /var/log/` |
| 查找错误日志 | `get-services` (包含) | `grep error /var/log/*.out.log` |

## 常用场景

### 场景 1: 容器启动后快速检查
```bash
docker compose up -d && sleep 10 && docker compose exec meduza get-services
```

### 场景 2: 监控所有服务状态
```bash
watch -n 2 'docker compose exec meduza get-services'
```

### 场景 3: 快速查看错误
```bash
docker compose exec meduza get-services | grep -A 10 "Recent Errors"
```

### 场景 4: 跟踪特定服务
```bash
docker compose exec meduza get-logs -f watcher
```

### 场景 5: 检查日志文件大小
```bash
docker compose exec meduza get-services | grep -A 10 "Log Files"
```

## 服务列表

可以使用 `get-logs` 查看的服务:

```bash
get-logs watcher       # 主协调服务
get-logs mihomo        # Clash 代理
get-logs easytier      # EasyTier 网络
get-logs tinc          # Tinc VPN
get-logs mosdns        # DNS 转发
get-logs dnsmasq       # 前端 DNS
get-logs dns-monitor   # DNS 监控
```

## 快速排查流程

### 1. 容器不运行
```bash
docker compose ps
docker compose logs meduza
```

### 2. 服务未启动
```bash
docker compose exec meduza get-services
docker compose exec meduza get-logs watcher
```

### 3. 服务反复重启
```bash
docker compose exec meduza get-logs -f watcher
docker compose exec meduza s6-svstat /etc/s6-overlay/sv/watcher
```

### 4. 网络问题
```bash
docker compose exec meduza get-logs mihomo
docker compose exec meduza get-logs easytier
docker compose exec meduza get-logs watcher
```

### 5. DNS 问题
```bash
docker compose exec meduza get-logs mosdns
docker compose exec meduza get-logs dnsmasq
docker compose exec meduza cat /etc/dnsmasq.conf
```

## 管道和组合

### 查找所有错误
```bash
docker compose exec meduza get-services | grep -i error
```

### 查看特定服务详情
```bash
docker compose exec meduza get-services | grep -A 2 "watcher:"
```

### 统计日志行数
```bash
docker compose exec meduza get-services | grep "lines" | awk '{sum+=$4} END {print sum}'
```

### 查找运行中的服务
```bash
docker compose exec meduza get-services | sed -n '/Running Services/,/^$/p'
```

## 时间对比

| 任务 | 传统命令耗时 | 快速工具耗时 |
|------|------------|-------------|
| 查看所有服务状态 | ~5 命令, 30 秒 | 1 命令, 2 秒 |
| 查看服务日志 | ~3 步, 15 秒 | 1 步, 2 秒 |
| 找错误日志 | ~5 步, 30 秒 | 自动显示, 2 秒 |
| 查看日志文件信息 | ~3 步, 10 秒 | 自动显示, 2 秒 |

**效率提升**: 约 5-15 倍

## 高级用法

### 监控多个服务
```bash
# 在多个终端中分别运行
docker compose exec meduza get-logs -f watcher
docker compose exec meduza get-logs -f mihomo
docker compose exec meduza get-logs -f dnsmasq
```

### 导出日志
```bash
# 导出单个服务日志
docker compose exec meduza get-logs watcher > watcher.log

# 导出所有服务日志
for svc in watcher mihomo easytier tinc mosdns dnsmasq; do
  docker compose exec meduza get-logs $svc > ${svc}.log
done
```

### 自动化监控
```bash
# 每 5 秒检查服务状态
while true; do
  clear
  docker compose exec meduza get-services
  sleep 5
done
```

### 定期日志轮转检查
```bash
# 检查日志文件大小
docker compose exec meduza get-services | grep "Log Files" -A 10 | grep -E "M|G"
```

## 别名建议

在 `~/.bashrc` 或 `~/.zshrc` 中添加:

```bash
# Meduza 快速别名
alias meduza-status='docker compose exec meduza get-services'
alias meduza-logs='docker compose exec meduza get-logs watcher'
alias meduza-follow='docker compose exec meduza get-logs -f watcher'
alias meduza-clash='docker compose exec meduza get-logs mihomo'
alias meduza-dns='docker compose exec meduza get-logs dnsmasq'
```

使用:
```bash
meduza-status    # 查看服务状态
meduza-logs      # 查看 watcher 日志
meduza-follow    # 跟踪 watcher 日志
meduza-clash     # 查看 Clash 日志
meduza-dns       # 查看 DNS 日志
```

## 提示和技巧

1. **使用 `-f` 标志进行实时监控**
   ```bash
   get-logs -f watcher  # 类似 tail -f
   ```

2. **先运行 `get-services` 获取全局视图**
   ```bash
   get-services  # 查看所有服务状态后再深入查看特定服务
   ```

3. **利用自动错误检测**
   ```bash
   get-services  # 自动显示最近 10 行错误
   ```

4. **检查日志文件大小防止磁盘满**
   ```bash
   get-services | grep "Log Files" -A 10
   ```

5. **快速判断服务是否正常**
   ```bash
   # 正常输出包含:
   # - Service Details: up (pid XXX)
   # - Recent Errors: (no errors found)
   ```

---

**推荐工作流程**:
1. `docker compose ps` - 检查容器
2. `get-services` - 查看所有服务状态
3. `get-logs <service>` - 查看问题服务日志
4. `get-logs -f <service>` - 实时跟踪调试

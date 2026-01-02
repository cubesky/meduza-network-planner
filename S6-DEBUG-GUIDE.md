# s6-overlay 故障排查指南

## 问题：服务没有启动，无法查看日志

### 已修复

✅ 为所有 s6 服务添加了日志配置
✅ 修复了 entrypoint.sh（添加 /var/log 目录）

### 验证修复

```bash
# 1. 重新构建容器
docker compose build

# 2. 启动容器
docker compose up -d

# 3. 查看日志
docker compose logs -f meduza
```

## 调试步骤

### 快速工具 ⚡

本项目提供了两个快速调试脚本:

#### `get-services` - 查看所有服务状态

```bash
# 从宿主机
docker compose exec meduza get-services

# 或进入容器后
docker compose exec meduza bash
get-services
```

**输出示例**:
```
=== s6 Services Status ===

[Running Services]
watcher
mihomo
dnsmasq
mosdns

[Service Details]
  watcher:       up (pid 123) 2345 seconds
  mihomo:        up (pid 456) 2340 seconds
  dnsmasq:       up (pid 789) 2338 seconds

[Log Files]
  watcher:       45K (234 lines)
  mihomo:        12K (89 lines)
  dnsmasq:       8K (45 lines)

[Recent Errors (last 10 lines from each log)]
  (no errors found)
```

#### `get-logs` - 查看服务日志

```bash
# 查看最近 100 行日志 (默认)
docker compose exec meduza get-logs watcher

# 查看最近 50 行
docker compose exec meduza get-logs -n 50 mihomo

# 实时跟踪日志 (类似 tail -f)
docker compose exec meduza get-logs -f easytier

# 显示最近 20 行后实时跟踪
docker compose exec meduza get-logs -n 20 -f mosdns

# 查看特定服务
docker compose exec meduza get-logs dnsmasq
```

### 1. 检查容器是否运行

```bash
docker compose ps
```

**预期**: 容器状态为 `Up`

### 2. 查看容器日志

```bash
docker compose logs meduza
```

**预期输出**:
```
[entrypoint] Starting s6-overlay with services...
[s6-init] copying service files...
[s6-init] compiling service database...
```

### 3. 使用快速工具检查 s6 状态

```bash
# 快速查看所有服务状态
docker compose exec meduza get-services

# 查看特定服务日志
docker compose exec meduza get-logs watcher
```

### 4. 手动 s6 命令（高级用法）

```bash
docker compose exec meduza bash
```

然后在容器内：

```bash
# 查看所有服务状态
s6-rc -a

# 查看所有已定义服务
s6-rc listall

# 查看特定服务状态
s6-svstat /etc/s6-overlay/sv/watcher
```

**预期**: `watcher`, `mihomo`, `dnsmasq` 等服务在 `s6-rc -a` 输出中

### 5. 查看服务日志（快速方法 vs 传统方法）

**使用快速工具** (推荐):
```bash
# 从宿主机直接查看
docker compose exec meduza get-logs watcher
docker compose exec meduza get-logs -n 50 watcher      # 指定行数
docker compose exec meduza get-logs -f mihomo          # 跟踪模式
docker compose exec meduza get-logs -n 20 -f dnsmasq   # 显示20行后跟踪
```

**传统方法**:
```bash
# 在容器内
tail -f /var/log/watcher.out.log

# 或从宿主机
docker compose exec meduza tail -f /var/log/watcher.out.log
```

### 6. 手动启动服务（调试用）

```bash
# 在容器内
s6-rc -u watcher

# 查看启动日志
s6-svstat /etc/s6-overlay/sv/watcher
```

## 常见问题

### 问题 1: 容器立即退出

**症状**: `docker compose ps` 显示容器状态为 `Exited (1)`

**原因**: entrypoint.sh 执行失败

**解决**:
```bash
# 查看容器日志
docker compose logs meduza

# 检查环境变量
docker compose exec meduza env | grep NODE_ID
```

### 问题 2: s6 服务未启动

**症状**: `s6-rc -a` 输出为空

**原因**: 服务数据库未正确编译或服务配置错误

**解决**:
```bash
# 在容器内检查服务文件
ls -la /etc/s6-overlay/sv/

# 手动编译服务数据库
s6-rc-compile /etc/s6-overlay/compiled /etc/s6-overlay/sv/

# 重启容器
docker compose restart meduza
```

### 问题 3: 服务启动后立即退出

**症状**: `s6-svstat /etc/s6-overlay/sv/watcher` 显示状态在 `up` 和 `down` 之间切换

**原因**: run 脚本执行失败

**解决**:
```bash
# 查看服务状态
s6-svstat /etc/s6-overlay/sv/watcher

# 手动运行服务脚本（查看错误）
/usr/local/bin/run-dns-monitor.sh --once

# 查看详细错误日志
tail -f /run/s6/uncaught-logs/current
```

### 问题 4: 无法查看日志

**症状**: `/var/log/watcher.out.log` 不存在或为空

**原因**: 日志配置未正确设置

**解决**:
```bash
# 检查日志目录
ls -la /var/log/

# 检查日志服务
s6-svstat /etc/s6-overlay/sv/watcher/log

# 查看日志输出
docker compose logs -f meduza watcher
```

## s6 服务管理命令

### 快速工具 (推荐) ⚡

```bash
# 查看所有服务状态（包括日志文件信息）
get-services

# 查看特定服务日志
get-logs watcher
get-logs -f mihomo      # 跟踪模式
get-logs dnsmasq
```

### 手动命令（高级用法）

#### 查看服务

```bash
# 查看运行中的服务
s6-rc -a

# 查看所有服务
s6-rc listall

# 查看服务状态
s6-svstat /etc/s6-overlay/sv/watcher
```

#### 控制服务

```bash
# 启动服务
s6-rc -u watcher

# 停止服务
s6-rc -d watcher

# 重启服务
s6-rc -r watcher
```

#### 服务状态

```bash
# 查看服务详细状态
s6-svstat /etc/s6-overlay/sv/watcher

# 持续监控服务状态
watch s6-rc -a
```

## 日志位置

### s6 日志

- **服务日志**: `/var/log/<service>.out.log`
- **s6 日志**: `/run/s6/uncaught-logs/current`
- **容器日志**: `docker compose logs meduza`

### 快速查看日志 ⚡

```bash
# 使用快速工具（推荐）
docker compose exec meduza get-logs watcher
docker compose exec meduza get-logs -f mihomo     # 跟踪模式

# 传统方法
docker compose exec meduza tail -f /var/log/watcher.out.log
```

### 示例

```bash
# Watcher 日志
/var/log/watcher.out.log

# Clash 日志
/var/log/mihomo.out.log

# MosDNS 日志
/var/log/mosdns.out.log
```

## 验证修复

### 快速验证（使用新工具）⚡

```bash
# 1. 重新构建（包含日志配置和快速工具）
docker compose build

# 2. 启动容器
docker compose up -d

# 3. 等待 10 秒让服务启动
sleep 10

# 4. 检查容器状态
docker compose ps

# 5. 使用快速工具查看所有服务状态
docker compose exec meduza get-services

# 6. 查看特定服务日志
docker compose exec meduza get-logs watcher
docker compose exec meduza get-logs mihomo

# 7. 实时跟踪日志
docker compose exec meduza get-logs -f watcher
```

### 完整的验证流程（传统方法）

```bash
# 1. 重新构建（包含日志配置）
docker compose build

# 2. 启动容器
docker compose up -d

# 3. 等待 10 秒让服务启动
sleep 10

# 4. 检查容器状态
docker compose ps

# 5. 查看日志
docker compose logs meduza | tail -50

# 6. 进入容器检查 s6
docker compose exec meduza bash

# 7. 在容器内检查服务
s6-rc -a
s6-svstat /etc/s6-overlay/sv/watcher

# 8. 查看日志文件
tail -f /var/log/watcher.out.log

# 9. 退出容器
exit

# 10. 查看所有服务日志
docker compose logs -f meduza
```

## 如果仍然无法启动

### 临时解决方案：绕过 s6

如果 s6 仍然有问题，可以临时修改 entrypoint.sh 直接启动 watcher：

```bash
# 修改 entrypoint.sh 最后一行为：
exec python3 /watcher.py
```

然后：

```bash
docker compose build
docker compose up -d
docker compose logs -f meduza
```

这样 watcher 会直接运行，所有日志输出到 stdout/stderr。

## 总结

✅ **已修复**: 为所有服务添加了日志配置
✅ **已修复**: entrypoint.sh 添加了 /var/log 目录
✅ **已修复**: 添加了启动日志输出

**下一步**: 重新构建并测试容器

---

**文档日期**: 2026-01-02
**状态**: ✅ 已修复

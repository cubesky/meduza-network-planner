# s6-supervise 问题完整修复指南

## 问题症状

```
s6-supervise not running
```

## 📋 问题原因

### 1. 容器刚启动
s6-overlay 需要时间初始化:
- 编译服务数据库
- 启动 supervision tree
- 启动各个服务

**典型时间**: 10-30 秒

### 2. 服务未启动
某些服务可能没有被 s6-rc 启动

### 3. supervise 目录缺失
服务配置不完整,supervise 目录未创建

### 4. 权限问题
supervise 目录或文件权限不正确

## ✅ 自动修复

### 使用更新后的脚本

新的 `get-services` 脚本已经内置了处理逻辑:

1. **检测 supervise 目录**: 如果不存在,显示 "no supervise dir"
2. **捕获错误**: 捕获 "s6-supervise not running" 错误
3. **自动降级**: 自动切换到手动 PID 检查
4. **友好显示**: 显示服务状态而不是报错退出

### 立即使用

```bash
# 重新构建容器
./fix-debug-tools.sh

# 或手动执行
docker compose down && \
docker compose build --no-cache && \
docker compose up -d && \
sleep 15 && \
docker compose exec meduza get-services
```

## 🔍 手动诊断

### 步骤 1: 检查 s6 进程

```bash
docker compose exec meduza ps aux | grep s6
```

**预期输出**:
```
/usr/bin/s6-svscan /etc/s6-overlay/sv
s6-supervise s6-svscan
s6-supervise watcher
s6-supervise mihomo
...
```

**如果看不到 s6 进程**:
- s6-overlay 未正确初始化
- 需要检查 entrypoint.sh
- 可能需要重启容器

### 步骤 2: 检查 supervise 目录

```bash
# 进入容器
docker compose exec meduza bash

# 检查特定服务
ls -la /etc/s6-overlay/sv/watcher/supervise/

# 预期输出:
# control
# lock
# ok
# pid
# status
```

**如果目录为空或不存在**:
- 服务配置有问题
- s6 未正确启动服务

### 步骤 3: 检查 PID 文件

```bash
# 检查 PID 文件
cat /etc/s6-overlay/sv/watcher/supervise/pid

# 检查进程是否运行
ps -p $(cat /etc/s6-overlay/sv/watcher/supervise/pid)
```

### 步骤 4: 手动启动服务

```bash
# 启动特定服务
docker compose exec meduza s6-rc -u watcher

# 检查服务状态
docker compose exec meduza s6-svstat /etc/s6-overlay/sv/watcher
```

## 🛠️ 修复方法

### 方法 1: 等待初始化 (推荐)

```bash
# 启动容器
docker compose up -d

# 等待足够时间
sleep 20

# 检查服务
docker compose exec meduza get-services
```

### 方法 2: 重启容器

```bash
# 停止容器
docker compose down

# 启动容器
docker compose up -d

# 等待初始化
sleep 20

# 检查
docker compose exec meduza get-services
```

### 方法 3: 重新构建

```bash
# 完全重新构建
docker compose down
docker compose build --no-cache
docker compose up -d
sleep 20
docker compose exec meduza get-services
```

### 方法 4: 修复权限

```bash
# 进入容器
docker compose exec meduza bash

# 修复服务目录权限
chmod -R 0755 /etc/s6-overlay/sv/*/supervise
chmod 0644 /etc/s6-overlay/sv/*/supervise/control/*

# 退出并重启
exit
docker compose restart
```

### 方法 5: 重新编译服务数据库

```bash
# 进入容器
docker compose exec meduza bash

# 停止 s6 (如果运行)
s6-rc -aD

# 重新编译
s6-rc-compile /etc/s6-overlay/compiled /etc/s6-overlay/sv/

# 启动所有服务
s6-rc -a

# 退出
exit
```

## 📊 脚本处理逻辑

更新后的 `get-services` 脚本处理流程:

```
开始检查服务
    ↓
检查 supervise 目录是否存在?
    ├─ 否 → 显示 "no supervise dir"
    └─ 是 ↓
尝试 s6-svstat?
    ├─ 捕获 "not running" 错误
    ├─ 捕获 "unable" 错误
    └─ 降级到手动检查
        ↓
检查 supervise/pid 文件?
    ├─ 是 ↓
    │   读取 PID
    │   检查进程是否存活
    │   ├─ 存活 → 显示 "up (pid XXX)"
    │   └─ 死亡 → 显示 "down"
    └─ 否 ↓
检查 down 文件?
    ├─ 是 → 显示 "disabled"
    └─ 否 → 显示 "not started"
```

## 🎯 预期输出

### 正常情况

```
=== s6 Services Status ===

[Running Services]
watcher
mihomo

[Service Details]
  watcher:       up (pid 123, 4567 seconds)
                 PID 123
  mihomo:        up (pid 456, 4321 seconds)
                 PID 456
```

### supervise 未初始化

```
=== s6 Services Status ===

[Running Services]
  (s6-rc not available - checking services manually)

[Service Details]
  watcher:       no supervise dir
  mihomo:        no supervise dir
```

### 服务未启动

```
=== s6 Services Status ===

[Running Services]
  (no services running)

[Service Details]
  watcher:       not started
  mihomo:        not started
```

## ⚠️ 常见错误和解决

### 错误 1: "s6-supervise not running"

**原因**: s6-supervise 进程未运行

**解决**:
```bash
# 等待初始化
sleep 20

# 或重启容器
docker compose restart
```

### 错误 2: "unable to take supervise lock"

**原因**: supervise 锁文件存在,服务正在停止

**解决**:
```bash
# 等待几秒
sleep 5

# 或删除锁文件
docker compose exec meduza rm -f /etc/s6-overlay/sv/*/supervise/lock
```

### 错误 3: "no supervise dir"

**原因**: supervise 目录未创建

**解决**:
```bash
# 进入容器
docker compose exec meduza bash

# 手动创建 supervise 目录
for svc in watcher mihomo; do
  mkdir -p /etc/s6-overlay/sv/$svc/supervise
  chmod 0755 /etc/s6-overlay/sv/$svc/supervise
done

# 重启容器
exit
docker compose restart
```

## 📝 检查清单

启动容器后按顺序检查:

- [ ] 容器状态: `docker compose ps`
- [ ] 等待 15-20 秒让 s6 初始化
- [ ] s6 进程: `docker compose exec meduza ps aux | grep s6`
- [ ] supervise 目录: `docker compose exec meduza ls -la /etc/s6-overlay/sv/watcher/supervise/`
- [ ] 服务状态: `docker compose exec meduza get-services`
- [ ] 服务日志: `docker compose exec meduza get-logs watcher`

## 🎯 最佳实践

1. **启动后等待**: 容器启动后至少等待 15 秒
2. **使用新脚本**: 使用更新后的 `get-services`,它会自动处理错误
3. **查看日志**: 如果服务未启动,查看日志找出原因
4. **逐步检查**: 按照"手动诊断"步骤逐步检查
5. **最后手段**: 重新构建容器

## 📚 相关文档

- **[S6-DEBUG-GUIDE.md](S6-DEBUG-GUIDE.md)** - s6 调试指南
- **[PODMAN-SUPPORT.md](PODMAN-SUPPORT.md)** - Podman 支持
- **[fix-debug-tools.sh](fix-debug-tools.sh)** - 自动修复脚本

---

**更新日期**: 2026-01-02
**状态**: ✅ 脚本已更新,自动处理 s6-supervise 错误
**建议**: 重新构建容器后使用新脚本

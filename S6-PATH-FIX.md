# s6-overlay PATH 修复说明

## ✅ 已修复

脚本已更新,现在包含正确的 s6-overlay v3 PATH 配置。

### 修改内容

在 `get-services.sh` 和 `get-logs.sh` 开头添加:

```bash
# s6-overlay v3 命令路径
export PATH="/command:/usr/local/bin:/usr/bin:/bin"
```

### 原因

s6-overlay v3 将命令安装在 `/command` 目录,而不是标准的 `/usr/bin`。需要将 `/command` 添加到 PATH 中。

## 🔍 验证修复

### 1. 重新构建镜像

```bash
# 停止容器
docker compose down

# 重新构建
docker compose build --no-cache

# 启动容器
docker compose up -d

# 等待启动
sleep 10
```

### 2. 验证命令可用

```bash
# 测试 get-services
docker compose exec meduza get-services

# 测试 get-logs
docker compose exec meduza get-logs watcher
```

### 3. 检查 PATH

```bash
# 查看 get-services 脚本中的 PATH
docker compose exec meduza grep "export PATH" /usr/local/bin/get-services

# 应该输出:
# export PATH="/command:/usr/local/bin:/usr/bin:/bin"
```

### 4. 验证 s6 命令

```bash
# 检查 s6-rc 是否可用
docker compose exec meduza sh -c 'export PATH="/command:/usr/local/bin:/usr/bin:/bin" && which s6-rc'

# 应该输出: /command/s6-rc
```

## 📋 完整修复流程

```bash
# 使用自动修复脚本
./fix-debug-tools.sh
```

或手动执行:

```bash
docker compose down \
  && docker compose build --no-cache \
  && docker compose up -d \
  && sleep 10 \
  && docker compose exec meduza get-services
```

## 🎯 预期结果

修复后,`get-services` 应该正常工作:

```
=== s6 Services Status ===

[Running Services]
watcher
mihomo
dnsmasq
...

[Service Details]
  watcher:       up (pid 123)
                 PID 123
  mihomo:        up (pid 456)
                 PID 456

[Log Files]
  watcher:       45K (234 lines)
  mihomo:        12K (89 lines)

[Recent Errors]
  (no errors found)
```

## 🔧 如果仍然有问题

### 问题 1: s6-rc 仍然找不到

**症状**: `s6-rc: command not found`

**解决**:
```bash
# 手动检查 s6-overlay 安装
docker compose exec meduza ls -la /command/

# 应该看到 s6-rc, s6-svstat 等命令
```

### 问题 2: 服务未启动

**症状**: `(no services running or s6-rc not available)`

**原因**: s6-overlay 可能未正确初始化

**检查**:
```bash
# 检查 s6 进程
docker compose exec meduza ps aux | grep s6

# 检查服务目录
docker compose exec meduza ls -la /etc/s6-overlay/sv/
```

### 问题 3: 日志文件不存在

**症状**: `(no log file)`

**原因**: 服务可能未启动或日志配置有问题

**检查**:
```bash
# 检查日志目录
docker compose exec meduza ls -la /var/log/

# 检查服务日志配置
docker compose exec meduza ls -la /etc/s6-overlay/sv/watcher/log/
```

## 📚 相关文档

- **[TROUBLESHOOT-COMMANDS.md](TROUBLESHOOT-COMMANDS.md)** - 完整故障排查
- **[S6-DEBUG-GUIDE.md](S6-DEBUG-GUIDE.md)** - s6 调试指南
- **[fix-debug-tools.sh](fix-debug-tools.sh)** - 自动修复脚本

## ✅ 修复清单

- ✅ 添加 `/command` 到 PATH
- ✅ 添加 s6-rc 失败时的降级处理
- ✅ 手动检查服务状态 (当 s6-svstat 不可用时)
- ✅ 语法验证通过
- ⏳ 需要重新构建容器

---

**修复日期**: 2026-01-02
**状态**: ✅ 已修复,待重新构建验证

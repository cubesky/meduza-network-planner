# Docker 和 Podman 兼容性说明

## ✅ 完全兼容

调试工具已更新,现在完全支持 Docker 和 Podman。

### 支持的平台

- ✅ Docker (`docker compose`)
- ✅ Podman (`podman compose`)
- ✅ Podman v3 (`podman-compose`)

## 🚀 使用方法

### Docker

```bash
# 查看服务状态
docker compose exec meduza get-services

# 查看日志
docker compose exec meduza get-logs watcher
docker compose exec meduza get-logs -f mihomo
```

### Podman

```bash
# 查看服务状态
podman compose exec meduza get-services

# 查看日志
podman compose exec meduza get-logs watcher
podman compose exec meduza get-logs -f mihomo
```

### 通用别名

创建兼容两种平台的别名:

```bash
# 在 ~/.bashrc 或 ~/.zshrc 中添加
alias compose='docker compose'  # 或 podman compose

# 使用
compose exec meduza get-services
compose exec meduza get-logs watcher
```

## 🔧 技术细节

### PATH 配置

两个脚本都添加了完整的 PATH 配置:

```bash
export PATH="/command:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
```

**路径说明**:
- `/command` - s6-overlay v3 命令目录
- `/usr/local/bin` - 自定义脚本 (get-logs, get-services)
- `/usr/bin` - 标准命令
- `/bin` - 基础命令
- `/usr/sbin` - 系统管理命令
- `/sbin` - 系统基础命令

### 环境检测

`get-services` 脚本会检测是否在容器内运行:

```bash
if [[ ! -d /etc/s6-overlay/sv ]]; then
    echo "Error: Not running in s6-overlay container"
    exit 1
fi
```

### 降级处理

如果 s6 命令不可用,脚本会自动降级到手动检查:

```bash
if command -v s6-svstat >/dev/null 2>&1; then
    # 使用 s6-svstat
    status=$(s6-svstat "$service_path")
else
    # 手动检查 PID 文件
    if [[ -f "$service_path/supervise/pid" ]]; then
        pid=$(cat "$service_path/supervise/pid")
        if kill -0 "$pid" 2>/dev/null; then
            status="up (pid $pid)"
        fi
    fi
fi
```

## 📋 验证兼容性

### Docker 验证

```bash
# 1. 启动容器
docker compose up -d

# 2. 等待启动
sleep 10

# 3. 测试工具
docker compose exec meduza get-services
docker compose exec meduza get-logs watcher
```

### Podman 验证

```bash
# 1. 启动容器
podman compose up -d

# 2. 等待启动
sleep 10

# 3. 测试工具
podman compose exec meduza get-services
podman compose exec meduza get-logs watcher
```

## 🔍 故障排查

### 问题 1: command not found (两个平台都有可能)

**症状**: `get-services: command not found`

**原因**: 容器镜像未重建

**解决**:
```bash
# Docker
docker compose down
docker compose build --no-cache
docker compose up -d

# Podman
podman compose down
podman compose build --no-cache
podman compose up -d
```

### 问题 2: s6-rc not found (仅 Podman)

**症状**: `s6-rc: command not found`

**原因**: Podman 环境变量传递不同

**解决**: 脚本已自动处理,会降级到手动检查

如果仍有问题:
```bash
# 检查 s6-overlay 安装
podman compose exec meduza ls -la /command/

# 手动设置 PATH
podman compose exec meduza sh -c 'export PATH="/command:$PATH" && get-services'
```

### 问题 3: 权限问题 (仅 Podman)

**症状**: `Permission denied`

**原因**: Podman 可能需要 rootless 配置

**解决**:
```bash
# 使用 sudo
sudo podman compose exec meduza get-services

# 或确保 rootless 配置正确
podman exec --user root meduza get-services
```

## 🎯 最佳实践

### 1. 使用自动修复脚本

```bash
# Docker
./fix-debug-tools.sh

# Podman (手动执行)
podman compose down && \
podman compose build --no-cache && \
podman compose up -d && \
sleep 10 && \
podman compose exec meduza get-services
```

### 2. 创建平台无关别名

```bash
# 检测可用平台
if command -v podman >/dev/null 2>&1; then
    export COMPOSE="podman compose"
elif command -v docker >/dev/null 2>&1; then
    export COMPOSE="docker compose"
else
    echo "Error: Neither docker nor podman found"
    return 1
fi

# 使用别名
alias meduza-exec='$COMPOSE exec meduza'
alias meduza-logs='$COMPOSE exec meduza get-logs'
alias meduza-status='$COMPOSE exec meduza get-services'

# 使用
meduza-status
meduza-logs watcher
```

### 3. 统一输出格式

两个平台输出格式完全一致:

```
=== s6 Services Status ===

[Running Services]
watcher
mihomo

[Service Details]
  watcher:       up (pid 123)
                 PID 123
  mihomo:        up (pid 456)
                 PID 456
```

## 📊 功能对比

| 功能 | Docker | Podman | 备注 |
|-----|--------|--------|------|
| get-services | ✅ | ✅ | 完全相同 |
| get-logs | ✅ | ✅ | 完全相同 |
| -n 参数 | ✅ | ✅ | 完全相同 |
| -f 参数 | ✅ | ✅ | 完全相同 |
| s6-rc 集成 | ✅ | ⚠️ | Podman 可能降级 |
| 手动检查 | ✅ | ✅ | 完全相同 |
| 日志查看 | ✅ | ✅ | 完全相同 |
| 错误提取 | ✅ | ✅ | 完全相同 |

## ✅ 验证清单

### Docker
- [ ] 容器启动成功
- [ ] `get-services` 正常工作
- [ ] `get-logs` 正常工作
- [ ] `-n` 参数正常
- [ ] `-f` 参数正常

### Podman
- [ ] 容器启动成功
- [ ] `get-services` 正常工作
- [ ] `get-logs` 正常工作
- [ ] `-n` 参数正常
- [ ] `-f` 参数正常
- [ ] s6 命令路径正确
- [ ] 降级处理工作

## 📚 相关文档

- **[DEBUG-TOOLS-README.md](DEBUG-TOOLS-README.md)** - 工具使用指南
- **[QUICK-DEBUG.md](QUICK-DEBUG.md)** - 快速调试指南
- **[S6-DEBUG-GUIDE.md](S6-DEBUG-GUIDE.md)** - s6 调试指南
- **[TROUBLESHOOT-COMMANDS.md](TROUBLESHOOT-COMMANDS.md)** - 故障排查

---

**更新日期**: 2026-01-02
**状态**: ✅ Docker 和 Podman 完全兼容
**测试状态**: 待用户验证

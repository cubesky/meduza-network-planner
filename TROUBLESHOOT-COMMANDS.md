# 调试工具无法找到 - 修复指南

## 问题症状

```bash
docker compose exec meduza get-services
# bash: get-services: command not found
```

## 原因

容器正在使用旧的镜像,不包含新添加的调试工具。

## ✅ 修复步骤

### 1. 停止并删除旧容器

```bash
docker compose down
```

### 2. 重新构建镜像 (重要!)

```bash
docker compose build --no-cache
```

**或者**如果不想完全重建:

```bash
docker compose build
```

### 3. 启动新容器

```bash
docker compose up -d
```

### 4. 验证工具可用

```bash
# 检查命令是否存在
docker compose exec meduza which get-logs
docker compose exec meduza which get-services

# 应该输出:
# /usr/local/bin/get-logs
# /usr/local/bin/get-services
```

### 5. 测试工具

```bash
# 测试 get-services
docker compose exec meduza get-services

# 测试 get-logs
docker compose exec meduza get-logs watcher
```

## 🔍 诊断步骤

如果重建后仍然无法找到命令:

### 检查 1: 确认文件在镜像中

```bash
docker compose exec meduza ls -la /usr/local/bin/get-*
```

**预期输出**:
```
-rwxr-xr-x 1 root root 1942 Jan  2 14:00 /usr/local/bin/get-logs
-rwxr-xr-x 1 root root 2410 Jan  2 14:00 /usr/local/bin/get-services
```

### 检查 2: 确认文件权限

```bash
docker compose exec meduza ls -la /usr/local/bin/get-logs
docker compose exec meduza ls -la /usr/local/bin/get-services
```

**预期**: 应该有执行权限 (`-rwxr-xr-x`)

### 检查 3: 查看构建日志

```bash
# 查看最近的构建日志
docker compose build 2>&1 | grep -E "(COPY|chmod|get-logs|get-services)"
```

**预期应该看到**:
```
COPY scripts/get-logs.sh /usr/local/bin/get-logs
COPY scripts/get-services.sh /usr/local/bin/get-services
RUN chmod +x ... /usr/local/bin/get-logs /usr/local/bin/get-services ...
```

### 检查 4: 确认使用的是新镜像

```bash
# 查看镜像创建时间
docker images | grep meduza
```

**预期**: 镜像创建时间应该是刚才重建的时间

## 🛠️ 手动修复 (如果重建失败)

如果容器已经运行但找不到命令,可以手动复制:

```bash
# 从宿主机复制到容器
docker cp scripts/get-logs.sh meduza-network-planner-meduza-1:/usr/local/bin/get-logs
docker cp scripts/get-services.sh meduza-network-planner-meduza-1:/usr/local/bin/get-services

# 设置执行权限
docker compose exec meduza chmod +x /usr/local/bin/get-logs
docker compose exec meduza chmod +x /usr/local/bin/get-services

# 验证
docker compose exec meduza which get-logs
docker compose exec meduza which get-services
```

## 📋 完整修复流程

```bash
# 1. 停止容器
docker compose down

# 2. 重新构建 (强制不使用缓存)
docker compose build --no-cache

# 3. 启动容器
docker compose up -d

# 4. 等待启动
sleep 10

# 5. 验证工具
docker compose exec meduza which get-logs
docker compose exec meduza which get-services

# 6. 测试工具
docker compose exec meduza get-services
docker compose exec meduza get-logs watcher
```

## ⚠️ 常见错误

### 错误 1: 使用旧容器

**症状**: `command not found`

**原因**: 容器没有重启,仍在使用旧镜像

**解决**: 必须执行 `docker compose down` 然后 `docker compose up -d`

### 错误 2: 构建时没有包含新文件

**症状**: 重建后仍然找不到命令

**原因**: Dockerfile 没有正确更新

**解决**: 检查 Dockerfile 是否包含:
```dockerfile
COPY scripts/get-logs.sh /usr/local/bin/get-logs
COPY scripts/get-services.sh /usr/local/bin/get-services
```

以及:
```dockerfile
RUN chmod +x ... /usr/local/bin/get-logs /usr/local/bin/get-services ...
```

### 错误 3: 权限问题

**症状**: 找到命令但无法执行 (`permission denied`)

**原因**: 文件没有执行权限

**解决**:
```bash
docker compose exec meduza chmod +x /usr/local/bin/get-logs
docker compose exec meduza chmod +x /usr/local/bin/get-services
```

## ✅ 成功标志

当一切正常时,你应该看到:

```bash
$ docker compose exec meduza get-services
=== s6 Services Status ===

[Running Services]
watcher
mihomo
...

$ docker compose exec meduza get-logs watcher
=== watcher logs (/var/log/watcher.out.log) ===

[日志内容...]
```

## 🎯 快速命令

```bash
# 一键修复
docker compose down && docker compose build --no-cache && docker compose up -d && sleep 10 && docker compose exec meduza get-services
```

这个命令会:
1. 停止容器
2. 重新构建 (不使用缓存)
3. 启动容器
4. 等待 10 秒
5. 验证工具可用

---

**关键点**: 必须重新构建镜像,简单的 `docker compose restart` 不会更新镜像中的文件!

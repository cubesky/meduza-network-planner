# s6-overlay 日志配置修复

## 问题诊断

s6-overlay v3 服务没有启动的原因：
1. **缺少日志配置**: s6 服务需要 `log/` 目录和日志运行脚本
2. **无法查看日志**: 没有配置日志输出

## 解决方案

### 选项 1: 为每个服务添加日志配置（推荐）

为每个 s6 服务添加 `log/` 目录和 `log/run` 脚本。

### 选项 2: 禁用日志服务（简单）

在 entrypoint.sh 中设置环境变量，让 s6 不使用日志服务。

## 推荐修复：添加日志配置

为每个关键服务添加日志目录：

```bash
# watcher 服务日志
mkdir -p s6-services/watcher/log
cat > s6-services/watcher/log/run <<'EOF'
#!/command/execlineb -P
s6-setenv logfile /var/log/watcher.out.log
s6-setenv maxbytes 10485760  # 10MB
s6-setenv maxfiles 10
exec s6-svlogd "${logfile}" "${maxbytes}" "${maxfiles}"
EOF
chmod +x s6-services/watcher/log/run

# mihomo 服务日志
mkdir -p s6-services/mihomo/log
cat > s6-services/mihomo/log/run <<'EOF'
#!/command/execlineb -P
s6-setenv logfile /var/log/mihomo.out.log
s6-setenv maxbytes 10485760
s6-setenv maxfiles 10
exec s6-svlogd "${logfile}" "${maxbytes}" "${maxfiles}"
EOF
chmod +x s6-services/mihomo/log/run
```

## 快速修复：使用标准输出

如果不需要日志轮转，可以让服务直接输出到标准输出：

修改 entrypoint.sh，在 exec /init 之前添加：

```bash
# Configure s6-overlay to use stdout for logging
mkdir -p /etc/s6-overlay/sv/default/log
cat > /etc/s6-overlay/sv/default/log/run <<'EOF'
#!/command/execlineb -P
fdmove -c 2 1
exec cat
EOF
chmod +x /etc/s6-overlay/sv/default/log/run
```

## 调试步骤

### 1. 检查容器是否启动

```bash
docker compose ps
docker compose logs meduza
```

### 2. 进入容器检查 s6 状态

```bash
docker compose exec meduza bash
s6-rc -a
s6-rc list
s6-sv stat /etc/s6-overlay/sv/watcher
```

### 3. 手动启动 watcher

```bash
docker compose exec meduza python3 /watcher.py
```

### 4. 查看 s6 日志

```bash
# 如果有 s6 日志
ls -la /run/s6/uncaught-logs/
cat /run/s6/uncaught-logs/current

# 查看 watcher 进程输出
docker compose logs -f meduza watcher
```

## 紧急修复

如果需要立即让服务运行，可以暂时禁用 s6，直接在 entrypoint 中启动 watcher：

修改 entrypoint.sh 的最后一行：

```bash
# 临时方案：直接启动 watcher（不使用 s6）
exec python3 /watcher.py
```

这会让 watcher 直接运行，所有日志输出到 stdout/stderr。

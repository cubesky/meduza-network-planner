# Supervisord → s6-overlay 迁移总结

## 📊 变更概览

### 修改的文件 (4个)
- `Dockerfile` - 安装 s6-overlay,移除 supervisord
- `entrypoint.sh` - 使用 s6-overlay 初始化
- `watcher.py` - 替换所有 supervisor API 为 s6 API
- `CLAUDE.md` - 更新文档

### 删除的文件 (1个)
- `supervisord.conf` - 旧的配置文件

### 新增的文件/目录
- `s6-services/` - s6 服务定义目录
  - `dbus/` - D-Bus 服务
  - `avahi/` - mDNS 服务 (依赖 dbus)
  - `watchfrr/` - FRR 监控
  - `watcher/` - 主编排服务 (依赖 dbus, avahi, watchfrr)
  - `mihomo/` - Clash 代理
  - `dns-monitor/` - DNS 监控
  - `easytier/` - EasyTier 网状网络 (按需启动)
  - `tinc/` - Tinc VPN (按需启动)
  - `mosdns/` - DNS 转发器 (按需启动)
  - `dnsmasq/` - DNS 缓存 (按需启动)
  - `default/` - 默认启动的服务包

- 文档:
  - `S6-MIGRATION.md` - 技术文档
  - `MIGRATION-GUIDE.md` - 用户指南
  - `MIGRATION-CHECKLIST.md` - 测试清单
  - `MIGRATION-SUMMARY.md` - 本文件

## ✅ 关键修复

### 1. 服务依赖关系
**问题**: watcher 的依赖文件格式错误
**修复**: 将 `dependencies.d/base` (包含多个依赖) 拆分为独立文件:
- `dependencies.d/dbus`
- `dependencies.d/avahi`
- `dependencies.d/watchfrr`

### 2. execlineb 语法
**问题**: avahi 服务使用了 bash 的 `export` 语法
**修复**: 改用 execlineb 的 `s6-setenv` 命令

### 3. 环境变量更新
**修复**: `SUPERVISOR_RETRY_INTERVAL` → `S6_RETRY_INTERVAL`

### 4. 服务状态检查
**修复**:
- 移除对 "FATAL" 状态的检查(s6 没有此状态)
- 简化 `s6_retry_loop()` 逻辑
- 添加超时和异常处理

### 5. 动态服务管理
**新增**:
- `_s6_create_dynamic_service()` - 创建动态服务
- `_s6_remove_dynamic_service()` - 删除动态服务
- `_s6_reload_services()` - 重新编译服务数据库

## 🔍 架构对比

| 特性 | supervisord | s6-overlay |
|------|------------|-----------|
| 配置文件 | INI 格式 | Shell 脚本 |
| 状态 | RUNNING, STOPPED, FATAL, etc. | up, down |
| 重启策略 | autorestart=true | 自动处理 |
| 动态服务 | reread/update | 创建目录 + 重新编译 |
| 依赖管理 | 无原生支持 | dependencies.d/ |
| 进程信号 | 支持 | 更好的支持 |
| 资源占用 | 较高 | 较低 |

## 📝 API 变更对照表

| supervisord | s6-overlay | 说明 |
|------------|-----------|------|
| `_supervisor_status(name)` | `_s6_status(name)` | 返回 "up"/"down" |
| `_supervisor_start(name)` | `_s6_start(name)` | 启动服务 |
| `_supervisor_stop(name)` | `_s6_stop(name)` | 停止服务 |
| `_supervisor_restart(name)` | `_s6_restart(name)` | 重启服务 |
| `_supervisor_is_running(name)` | `_s6_is_running(name)` | 检查运行状态 |
| `_supervisor_status_all()` | `_s6_status_all()` | 获取所有服务状态 |
| `_supervisorctl(["reread"])` | `_s6_reload_services()` | 重新加载配置 |
| 生成 .conf 文件 | `_s6_create_dynamic_service()` | 创建动态服务 |

## 🎯 启动顺序

### s6-overlay 启动流程:
1. `/init` (s6-overlay 主进程)
2. 根据 `default` bundle 启动服务:
   - `dbus` (无依赖)
   - `avahi` (依赖 dbus)
   - `watchfrr` (无依赖)
   - `watcher` (依赖 dbus, avahi, watchfrr)
   - `mihomo` (无依赖)
   - `dns-monitor` (无依赖)

### 按需启动的服务:
- `easytier` - 当 etcd 中 `/nodes/<NODE_ID>/easytier/enable = "true"`
- `tinc` - 当 etcd 中 `/nodes/<NODE_ID>/tinc/enable = "true"`
- `mosdns` - 当 etcd 中 `/nodes/<NODE_ID>/mosdns/enable = "true"`
- `dnsmasq` - 与 mosdns 一起启动
- `openvpn-*` - 动态创建
- `wireguard-*` - 动态创建

## 🧪 测试要点

### 基础功能
- [ ] 容器正常启动
- [ ] 所有默认服务正常运行
- [ ] watcher 日志无错误

### 动态服务
- [ ] EasyTier 启动/停止
- [ ] Tinc 启动/停止
- [ ] OpenVPN 动态实例创建
- [ ] WireGuard 动态实例创建

### 服务管理
- [ ] 服务重启正常
- [ ] etcd 触发正常工作
- [ ] 配置重新加载

### DNS 功能
- [ ] MosDNS 启动
- [ ] dnsmasq 启动
- [ ] DNS 解析正常

## ⚠️ 注意事项

1. **首次启动时间**: s6-overlay 首次编译服务数据库可能需要几秒钟
2. **动态服务**: 添加/删除服务后必须调用 `_s6_reload_services()`
3. **日志位置**: 保持不变,仍在 `/var/log/<service>.*.log`
4. **服务状态**: s6 只有 "up" 和 "down" 两种状态
5. **依赖解析**: s6-overlay 自动处理服务启动顺序

## 🚀 预期改进

1. **稳定性**: s6-overlay 的进程监控更可靠
2. **性能**: 更低的内存和 CPU 开销
3. **启动速度**: 更快的服务启动时间
4. **信号处理**: 更好的信号传递和处理
5. **资源清理**: 更可靠的进程树清理

## 📚 参考文档

- [s6-overlay 官方文档](https://github.com/just-containers/s6-overlay)
- [s6-rc 文档](https://www.skarnet.org/software/s6-rc/)
- [execlineb 语法](https://www.skarnet.org/software/execline/)

## 🔧 故障排查

### 服务无法启动
```bash
# 检查服务定义
docker compose exec meduza cat /etc/s6-overlay/sv/<service>/run

# 检查日志
docker compose exec meduza tail -f /var/log/<service>.*.log

# 手动测试
docker compose exec meduza /etc/s6-overlay/sv/<service>/run
```

### 编译失败
```bash
# 检查服务目录权限
docker compose exec meduza ls -la /etc/s6-overlay/sv/

# 检查 run 脚本权限
docker compose exec meduza find /etc/s6-overlay/sv/ -name "run" -exec ls -l {} \;
```

### 依赖问题
```bash
# 检查依赖文件
docker compose exec meduza find /etc/s6-overlay/sv/ -path "*/dependencies.d/*" -exec cat {} \;
```

## ✨ 迁移完成

所有代码修改已完成,可以进行构建和测试。

**下一步**: 运行 `docker compose build && docker compose up -d`

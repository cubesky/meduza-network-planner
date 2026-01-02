# s6-overlay v3 修复总结

## 修复的问题

### 1. 路径错误 ✅
**问题**: Dockerfile 将服务复制到错误的路径
- ❌ 旧路径: `/etc/s6-overlay/sv/`
- ✅ 新路径: `/etc/s6-overlay/s6-rc.d/`

**文件**: `Dockerfile`

### 2. Entrypoint 编译逻辑错误 ✅
**问题**: 手动编译 s6-rc 服务数据库
- ❌ s6-overlay v3 会自动编译，不需要手动操作
- ✅ 移除了所有手动编译逻辑

**文件**: `entrypoint.sh`

### 3. 服务类型配置错误 ✅
**问题**: Bundle 类型文件内容错误
- ❌ `default/type`: `bundled` 
- ✅ `default/type`: `bundle`
- ❌ `rc/type`: `bundled`（整个 rc 目录应删除）
- ✅ 已删除 `rc/` 目录

**文件**: `s6-services/default/type`, `s6-services/rc/` (已删除)

### 4. Bundle 结构错误 ✅
**问题**: 缺少正确的 bundle 结构

**修复**:
- ✅ 创建了 `user/` bundle 目录
- ✅ 创建了 `user/type` 文件 (内容: `bundle`)
- ✅ 创建了 `user/contents.d/` 目录
- ✅ 创建了 `default/contents.d/` 目录
- ✅ 在 `default/contents.d/` 中添加了 `user` 文件
- ❌ 删除了旧的 `default/contents` 文件
- ❌ 删除了 `default/types/` 目录
- ❌ 删除了 `default/run` 文件

### 5. 日志脚本语法错误 ✅
**问题**: 所有日志脚本使用了错误的语法

**旧语法** (错误):
```bash
#!/command/execlineb -P
s6-setenv logfile /var/log/mihomo.out.log
s6-setenv maxbytes 10485760
s6-setenv maxfiles 10
exec s6-svlogd "${logfile}" "${maxbytes}" "${maxfiles}"
```

**新语法** (正确):
```bash
#!/bin/sh
exec logutil-service /var/log/mihomo
```

**修复的服务日志**:
- ✅ mihomo
- ✅ watcher
- ✅ tinc
- ✅ mosdns
- ✅ easytier
- ✅ dnsmasq
- ✅ dns-monitor

### 6. 依赖文件内容错误 ✅
**问题**: 依赖文件包含文本内容
- ❌ 旧方式: 文件包含依赖名称（如 `dbus`）
- ✅ 新方式: 文件应为空，文件名即依赖名

**修复的依赖文件**:
- ✅ `avahi/dependencies.d/dbus` - 已清空
- ✅ `watcher/dependencies.d/avahi` - 已清空
- ✅ `watcher/dependencies.d/dbus` - 已清空
- ✅ `watcher/dependencies.d/watchfrr` - 已清空

### 7. 缺少 base 依赖 ✅
**问题**: 服务没有依赖 `base` bundle

**修复**: 为所有服务添加了 `base` 依赖:
- ✅ dbus
- ✅ avahi
- ✅ watchfrr
- ✅ watcher
- ✅ mihomo
- ✅ easytier
- ✅ tinc
- ✅ mosdns
- ✅ dnsmasq
- ✅ dns-monitor

### 8. Pipeline 配置问题 ✅ (已修正)
**初始错误**: 错误地使用了 s6-rc pipeline 配置

**问题**: 
- ❌ 创建了 `producer-for`, `consumer-for`, `pipeline-name` 文件
- ❌ 在 `user/contents.d/` 中添加了 `<service>-pipeline` 文件
- ❌ 这导致服务无法启动

**正确方式**:
在 s6-overlay v3 中，如果服务目录下有 `log/` 子目录：
- ✅ `log/` 目录应该包含 `type` (longrun) 和 `run` 脚本
- ✅ s6-overlay 会**自动**识别 `log/` 目录并创建管道
- ✅ **不需要** `producer-for`, `consumer-for`, `pipeline-name` 文件
- ✅ 直接在 `user/contents.d/` 中添加服务名（如 `mihomo`），不是 `mihomo-pipeline`
- ✅ s6 会自动处理服务输出到 log 的重定向

**修复**: 
- 删除了所有 `producer-for`, `consumer-for`, `pipeline-name` 文件
- 删除了 `user/contents.d/*-pipeline` 文件
- 在 `user/contents.d/` 中直接添加服务名
**修复**: 
- 删除了所有 `producer-for`, `consumer-for`, `pipeline-name` 文件
- 删除了 `user/contents.d/*-pipeline` 文件
- 在 `user/contents.d/` 中直接添加服务名

**带日志的服务**:
- ✅ mihomo
- ✅ watcher
- ✅ tinc
- ✅ mosdns
- ✅ easytier
- ✅ dnsmasq
- ✅ dns-monitor

### 9. 更新工具脚本 ✅
**问题**: get-logs 和 get-services 脚本使用旧的 s6 v2 路径和命令

**修复**:

#### get-logs.sh:
- ❌ 旧路径: `/var/log/<service>.out.log` (单个文件)
- ✅ 新路径: `/var/log/<service>/current` (s6-log 目录结构)

#### get-services.sh:
- ❌ 旧路径: `/etc/s6-overlay/sv/<service>`
- ✅ 新路径: `/run/service/<service>` (运行时服务目录)
- ❌ 旧命令: `s6-rc listall`, 手动检查 `/etc/s6-overlay/sv/`
- ✅ 新命令: `s6-rc-db list all`, `s6-rc -a list`
- ✅ 新增: Pipeline 和 Bundle 状态显示

## 最终结构

### Bundle 层级:
```
default (bundle)
  └── user (bundle)
      ├── dbus (longrun)
      ├── avahi (longrun) - depends on: dbus
      ├── watchfrr (longrun)
      ├── watcher (longrun) - depends on: dbus, avahi, watchfrr
      ├── mihomo (longrun with log/)
      ├── easytier (longrun with log/)
      ├── tinc (longrun with log/)
      ├── mosdns (longrun with log/)
      ├── dnsmasq (longrun with log/)
      └── dns-monitor (longrun with log/)
```

**注意**: 每个带 `log/` 的服务，s6-overlay 会自动：
1. 识别 `log/` 子目录
2. 创建从服务到 log 的管道
3. 启动 log 服务接收主服务的输出

### 依赖关系:
```
base (s6-overlay 内置)
  ├── dbus
  │   └── avahi
  │       └── watcher (also depends on watchfrr)
  └── watchfrr
```

### 目录结构:
```
s6-services/
├── default/
│   ├── type (bundle)
│   └── contents.d/
│       └── user (空文件)
├── user/
│   ├── type (bundle)
│   └── contents.d/
│       ├── dbus (空文件)
│       ├── avahi (空文件)
│       ├── watchfrr (空文件)
│       ├── watcher (空文件)
│       ├── mihomo (空文件)
│       ├── easytier (空文件)
│       ├── tinc (空文件)
│       ├── mosdns (空文件)
│       ├── dnsmasq (空文件)
│       └── dns-monitor (空文件)
├── dbus/
│   ├── type (longrun)
│   ├── run (可执行)
│   └── dependencies.d/
│       └── base (空文件)
├── avahi/
│   ├── type (longrun)
│   ├── run (可执行)
│   └── dependencies.d/
│       ├── base (空文件)
│       └── dbus (空文件)
├── watchfrr/
│   ├── type (longrun)
│   ├── run (可执行)
│   └── dependencies.d/
│       └── base (空文件)
├── watcher/
│   ├── type (longrun)
│   ├── run (可执行)
│   ├── producer-for (内容: watcher-log)
│   ├── dependencies.d/
│   │   ├── base (空文件)
│   │   ├── dbus (空文件)
│   │   ├── avahi (空文件)
│   │   └── watchfrr (空文件)
│   └── log/
│       ├── type (longrun)
│       ├── run (可执行)
│       ├── consumer-for (内容: watcher)
│       └── pipeline-name (内容: watcher-pipeline)
└── mihomo/ (及其他带日志的服务，结构类似)
    ├── type (longrun)
    ├── run (可执行)
    ├── finish (可选)
    ├── dependencies.d/
    │   └── base (空文件)
    └── log/
        ├── type (longrun)
        └── run (可执行)
```

**重要**: log/ 目录会被 s6-overlay **自动识别和处理**，不需要额外的 pipeline 配置文件。

## 验证

运行验证脚本:
```bash
bash scripts/verify-s6-config.sh
```

该脚本会检查:
1. ✅ Bundle 结构和类型
2. ✅ Bundle 内容配置
3. ✅ 服务类型定义
4. ✅ 依赖关系和依赖文件格式
5. ✅ Pipeline 配置完整性
6. ✅ 日志脚本语法正确性

## 重新构建

所有修复完成后，重新构建容器:
```bash
docker compose down
docker compose build --no-cache
docker compose up -d
```

## 参考文档

- [s6-overlay v3 官方文档](https://github.com/just-containers/s6-overlay)
- [s6-rc 服务定义格式](https://skarnet.org/software/s6-rc/s6-rc-compile.html)
- [从 v2 迁移到 v3](https://github.com/just-containers/s6-overlay/blob/master/MOVING-TO-V3.md)

## 关键变更总结

1. **路径**: `/etc/s6-overlay/sv/` → `/etc/s6-overlay/s6-rc.d/`
2. **编译**: 不需要手动编译，s6-overlay 自动处理
3. **Bundle 类型**: `bundled` → `bundle`
4. **Bundle 结构**: 使用 `contents.d/` 目录，而不是 `contents` 文件
5. **依赖文件**: 必须是空文件，文件名即依赖名
6. **日志语法**: 使用 `logutil-service`，而不是 `s6-svlogd`
7. **日志目录**: `/var/log/<service>/current`，而不是 `/var/log/<service>.out.log`
8. **Log 处理**: log/ 子目录会被 s6 自动识别，不需要 pipeline 配置文件
9. **base 依赖**: 所有服务都应该依赖 `base`
10. **服务列表**: 在 `user/contents.d/` 中直接列出服务名，不是 pipeline 名

# s6-overlay v3 Service Definitions

这个目录包含 s6-overlay v3 的服务定义。

## 重要提示 ⚠️

### 关于空文件
这个目录中包含大量的**空文件**，这些文件对 s6-overlay 的正常运行**至关重要**：

1. **Bundle 内容文件** (`*/contents.d/*`)
   - 位置：如 `user/contents.d/mihomo`
   - 作用：文件名表示该 bundle 包含哪些服务
   - **必须是空文件**，文件名即为服务名

2. **依赖关系文件** (`*/dependencies.d/*`)
   - 位置：如 `avahi/dependencies.d/dbus`
   - 作用：文件名表示该服务依赖哪个服务
   - **必须是空文件**，文件名即为依赖名

### Git 注意事项

- ✅ 所有空文件都已被 git 追踪
- ⚠️ **请勿删除这些空文件**
- ⚠️ 提交前请确认所有空文件都被包含
- ⚠️ git 操作时请小心，避免意外删除空文件

### 验证配置

运行以下命令验证配置是否正确：
```bash
# PowerShell
.\scripts\verify-s6-config.ps1

# Bash (Linux/容器内)
bash scripts/verify-s6-config.sh
```

## 目录结构

```
s6-services/
├── default/              # 默认 bundle
│   ├── type             # 内容: bundle
│   └── contents.d/
│       └── user         # 空文件，引用 user bundle
├── user/                # 用户 bundle
│   ├── type             # 内容: bundle
│   └── contents.d/
│       ├── dbus         # 空文件，包含 dbus 服务
│       ├── avahi        # 空文件，包含 avahi 服务
│       └── ...          # 其他服务
├── dbus/                # dbus 服务
│   ├── type             # 内容: longrun
│   ├── run              # 启动脚本
│   └── dependencies.d/
│       └── base         # 空文件，依赖 base
├── avahi/               # avahi 服务
│   ├── type             # 内容: longrun
│   ├── run              # 启动脚本
│   └── dependencies.d/
│       ├── base         # 空文件，依赖 base
│       └── dbus         # 空文件，依赖 dbus
└── mihomo/              # mihomo 服务（带日志）
    ├── type             # 内容: longrun
    ├── run              # 启动脚本
    ├── finish           # 可选，退出处理脚本
    ├── dependencies.d/
    │   └── base         # 空文件，依赖 base
    └── log/             # 日志服务（s6 自动识别）
        ├── type         # 内容: longrun
        └── run          # 日志处理脚本
```

## 服务列表

### 系统服务
- `dbus` - D-Bus 消息总线
- `avahi` - mDNS/DNS-SD 服务发现
- `watchfrr` - FRRouting 看门狗

### 应用服务（带自动日志）
- `watcher` - 配置监控和热重载
- `mihomo` - Clash Meta 代理
- `easytier` - EasyTier VPN
- `tinc` - Tinc VPN
- `mosdns` - MosDNS 递归解析器
- `dnsmasq` - Dnsmasq DHCP/DNS
- `dns-monitor` - DNS 监控

## 依赖关系

```
base (s6-overlay 内置)
├── dbus
│   └── avahi
│       └── watcher (also depends on watchfrr)
├── watchfrr
└── (其他所有服务)
```

## 参考文档

- [s6-overlay v3 官方文档](https://github.com/just-containers/s6-overlay)
- [s6-rc 服务定义](https://skarnet.org/software/s6-rc/s6-rc-compile.html)
- [项目修复文档](../docs/s6-v3-fixes.md)

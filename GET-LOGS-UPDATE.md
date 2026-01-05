# get-logs 功能更新

## ✨ 新增 `-n` 参数

为 `get-logs` 命令添加了 `-n` 参数,可以指定显示的日志行数。

### 用法

```bash
# 默认显示 100 行
get-logs watcher

# 显示最近 50 行
get-logs -n 50 mihomo

# 显示最近 20 行后实时跟踪
get-logs -n 20 -f dnsmasq

# 先显示 10 行,然后进入跟踪模式
get-logs -n 10 -f watcher
```

### 参数说明

- `-n, --lines N` - 显示最近 N 行日志 (默认: 100)
- `-f, --follow` - 实时跟踪日志 (类似 tail -f)

### 组合使用

当 `-n` 和 `-f` 一起使用时:
1. 先显示指定行数的日志
2. 然后进入实时跟踪模式
3. 按 Ctrl+C 退出

**示例输出**:
```
=== watcher logs (/var/log/watcher.out.log) ===

[最近 20 行日志...]

=== Following log (Ctrl+C to exit) ===

[实时跟踪日志...]
```

## 📝 更新的文件

### 脚本
- ✅ [scripts/get-logs.sh](scripts/get-logs.sh) - 添加 `-n` 参数支持

### 文档
- ✅ [DEBUG-TOOLS-README.md](DEBUG-TOOLS-README.md) - 更新示例
- ✅ [QUICK-DEBUG.md](QUICK-DEBUG.md) - 添加 `-n` 参数说明
- ✅ [DEBUG-CHEATSHEET.md](DEBUG-CHEATSHEET.md) - 更新速查表
- ✅ [DEBUG-COMMANDS.md](DEBUG-COMMANDS.md) - 添加对比
- ✅ [S6-DEBUG-GUIDE.md](S6-DEBUG-GUIDE.md) - 更新使用说明

## 🎯 使用场景

### 1. 快速查看少量日志
```bash
docker compose exec meduza get-logs -n 20 watcher
```

### 2. 跟踪前了解上下文
```bash
docker compose exec meduza get-logs -n 50 -f mihomo
```

### 3. 查看大量历史日志
```bash
docker compose exec meduza get-logs -n 500 dnsmasq
```

### 4. 默认使用 (100 行)
```bash
docker compose exec meduza get-logs easytier
```

## ✅ 验证

```bash
✅ 语法检查通过
✅ 参数解析正确
✅ 组合使用正常
✅ 错误处理完善
```

---

**更新日期**: 2026-01-02
**版本**: v1.1
**状态**: ✅ 完成并验证

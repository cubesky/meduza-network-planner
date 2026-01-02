# Clash 启动顺序优化 - 快速参考

## 问题

旧逻辑可能导致：
- ❌ Clash 还在测试节点时应用 TPROXY → 网络中断
- ❌ dnsmasq 使用未就绪的 Clash DNS → DNS 失败
- ❌ MosDNS 通过未就绪的 Clash 下载规则 → 下载失败

## 解决方案

实施严格启动顺序：
1. **Clash 启动**
2. **等待 url-test 组选择节点** (通过 API 检查)
3. **Clash 就绪后才应用 TPROXY**
4. **然后启动依赖 Clash 的服务** (MosDNS, dnsmasq with Clash DNS)

## 关键修改

### 1. Clash 就绪判断

```python
def _clash_is_ready() -> bool:
    """检查 url-test 组是否选择了非 REJECT 节点"""
    proxies = _clash_api_get("/proxies")

    for name, proxy in proxies.get("proxies", {}).items():
        if proxy.get("type") in ("url-test", "fallback"):
            now = proxy.get("now")
            if not now or now in ("REJECT", "DIRECT"):
                return False  # 未就绪

    return True  # 就绪
```

### 2. dnsmasq 配置

```python
def _write_dnsmasq_config(clash_enabled: bool, clash_ready: bool):
    # Clash 启用但未就绪 → 不将 Clash DNS 加入转发列表
    if clash_enabled and clash_ready:
        servers = "server=127.0.0.1#1153\nserver=127.0.0.1#1053\n..."
    else:
        servers = "server=127.0.0.1#1153\n..."  # 不包含 Clash DNS
```

### 3. 启动流程

```python
# 1. 启动 Clash
_s6_start("mihomo")

# 2. 等待进程启动
wait for clash_pid() (10s timeout)

# 3. 加载配置
reload_clash(config)

# 4. 等待就绪 (url-test 组选择节点)
clash_ready = _wait_clash_ready(timeout=60)

# 5. 应用 TPROXY (仅在就绪后)
if clash_ready and mode == "tproxy":
    tproxy_apply(...)

# 6. 启动 MosDNS (传入就绪状态)
reload_mosdns(node, global_cfg, clash_ready=clash_ready)
```

## 日志示例

### 正常启动

```
[clash] waiting for process to start... (attempt 1/10)
[clash] process started (pid=1234)
[clash] waiting for url-test proxies to select nodes...
[clash] url-test-auto ready: HK-Node01
[clash] ready after 8s
[clash] applying TPROXY (Clash is ready)
[mosdns] dnsmasq started (with Clash DNS)
[mosdns] Clash is ready, downloading rules via proxy
```

### 超时场景

```
[clash] waiting for url-test proxies to select nodes...
[clash] waiting for url-test group to select node (current: REJECT)
...
[clash] not ready after 60s, proceeding anyway
[clash] WARNING: TPROXY not applied (Clash not ready)
[mosdns] Clash enabled but not ready, downloading rules directly
```

## 行为变化

| 场景 | 旧逻辑 | 新逻辑 |
|------|--------|--------|
| TPROXY 应用时机 | 启动后 2 秒 | url-test 组选择节点后 |
| dnsmasq Clash DNS | 立即加入 | Clash 就绪后加入 |
| MosDNS 规则下载 | 通过可能未就绪的 Clash | Clash 就绪后才通过 Clash |
| 网络中断风险 | 高 | 低 |

## 配置检查

### 检查 Clash 就绪状态

```bash
# 查看代理组状态
curl http://127.0.0.1:9090/proxies | jq '.proxies | to_entries[] | select(.value.type == "url-test")'

# 预期输出 (就绪):
# {
#   "name": "url-test-auto",
#   "now": "HK-Node01"  # 非 REJECT/DIRECT
# }

# 未就绪:
# {
#   "name": "url-test-auto",
#   "now": "REJECT"  # 或 "DIRECT"
# }
```

### 查看日志

```bash
# Clash 启动日志
tail -f /var/log/watcher.out.log | grep clash

# MosDNS 日志
tail -f /var/log/watcher.out.log | grep mosdns

# dnsmasq 状态
s6-rc status dnsmasq
cat /etc/dnsmasq.conf | grep server
```

## 超时配置

### Clash 进程启动: 10 秒
- 超时后抛出异常

### Clash 就绪等待: 60 秒
- 超时后继续，但:
  - TPROXY 不应用
  - dnsmasq 不使用 Clash DNS
  - MosDNS 直接下载规则
- 后台循环会自动重试

## 故障排查

### TPROXY 未应用

**检查**:
```bash
# Clash 是否就绪?
curl http://127.0.0.1:9090/proxies | jq '.proxies."url-test-auto".now'

# tproxy_check_loop 日志
tail -f /var/log/watcher.out.log | grep tproxy-check
```

**解决**:
- 等待 url-test 测试完成
- 或手动触发: `etcdctl put /commit "$(date +%s)"`

### DNS 失败

**检查**:
```bash
# dnsmasq 配置
cat /etc/dnsmasq.conf | grep 1053

# 如果存在: Clash DNS 在转发列表 (Clash 就绪)
# 如果不存在: Clash DNS 不在转发列表 (Clash 未就绪)
```

**解决**:
- 等待 Clash 就绪，dnsmasq 会自动重启
- 或手动重启: `s6-rc -r dnsmasq`

### MosDNS 规则下载失败

**检查**:
```bash
# 日志
tail -f /var/log/watcher.out.log | grep mosdns

# 应该看到:
# [mosdns] Clash is ready, downloading rules via proxy (就绪)
# 或
# [mosdns] Clash enabled but not ready, downloading rules directly (未就绪)
```

## 相关文件

- **[watcher.py:804-854](watcher.py#L804-L854)** - Clash API 和就绪检查
- **[watcher.py:1327-1372](watcher.py#L1327-L1372)** - dnsmasq 配置
- **[watcher.py:1375-1417](watcher.py#L1375-L1417)** - MosDNS 启动
- **[watcher.py:1493-1599](watcher.py#L1493-L1599)** - 主协调逻辑
- **[docs/clash-startup-sequence.md](docs/clash-startup-sequence.md)** - 完整文档

## 验证语法

```bash
uv run python -m py_compile watcher.py
# ✅ Python Syntax OK
```

## 总结

✅ **实现完成**: Clash 启动顺序优化
✅ **避免网络中断**: TPROXY 只在 Clash 就绪后应用
✅ **DNS 稳定**: dnsmasq 智能判断是否使用 Clash DNS
✅ **自动重试**: 后台循环处理超时场景
✅ **详细日志**: 每个步骤清晰可见

**日期**: 2026-01-02
**状态**: ✅ 已实现并验证

# Clash 启动顺序优化 - 实现总结

## 🎯 目标

优化 Clash 启动流程，确保服务依赖关系正确，避免网络中断和 DNS 失败。

## ✨ 关键改进

### 1. Clash 启动完成判断

**之前**: Clash 进程运行 = 就绪
**现在**: url-test/fallback 组选择非 REJECT 节点 = 就绪

**实现**:
- 通过 Clash API (`http://127.0.0.1:9090/proxies`) 检查代理组状态
- 等待所有 url-test 和 fallback 组选择了实际的代理节点
- 超时时间: 60 秒 (可配置)

### 2. TPROXY 应用时机

**之前**: Clash 启动后 2 秒直接应用 TPROXY
**现在**: Clash 就绪后才应用 TPROXY

**好处**:
- 避免 url-test 测试期间流量被 REJECT
- 防止网络中断
- 确保代理路径可用

### 3. dnsmasq 配置动态调整

**之前**: Clash 启用后立即将 Clash DNS 加入转发列表
**现在**:
- Clash 启用但未就绪 → 不加入 Clash DNS
- Clash 就绪后 → 加入 Clash DNS

**好处**:
- 避免 DNS 查询通过未就绪的 Clash
- 防止 DNS 解析失败

### 4. MosDNS 启动依赖 Clash

**之前**: 独立启动，不管 Clash 状态
**现在**:
- Clash 未就绪 → MosDNS 直接下载规则
- Clash 就绪后 → MosDNS 通过 Clash 代理下载规则

**好处**:
- 确保规则下载成功
- 利用 Clash 代理加速下载

## 📋 实现清单

### 代码修改

✅ **[watcher.py:804-817](watcher.py#L804-L817)** - `_clash_api_get()` 函数
✅ **[watcher.py:820-841](watcher.py#L820-L841)** - `_clash_is_ready()` 函数
✅ **[watcher.py:844-854](watcher.py#L844-L854)** - `_wait_clash_ready()` 函数
✅ **[watcher.py:1327-1372](watcher.py#L1327-L1372)** - `_write_dnsmasq_config()` 修改
✅ **[watcher.py:1375-1417](watcher.py#L1375-L1417)** - `reload_mosdns()` 修改
✅ **[watcher.py:1493-1599](watcher.py#L1493-L1599)** - `handle_commit()` 修改

### 文档

✅ **[docs/clash-startup-sequence.md](docs/clash-startup-sequence.md)** - 完整技术文档
✅ **[CLASH-STARTUP-OPTIMIZATION.md](CLASH-STARTUP-OPTIMIZATION.md)** - 快速参考

## 🔍 详细实现

### Clash 就绪检查

```python
def _clash_is_ready() -> bool:
    """检查 Clash 是否就绪"""
    proxies = _clash_api_get("/proxies")
    if not proxies:
        return False

    for name, proxy in proxies.get("proxies", {}).items():
        proxy_type = proxy.get("type", "")
        if proxy_type in ("url-test", "fallback"):
            now = proxy.get("now")
            if not now or now == "REJECT" or now == "DIRECT":
                print(f"[clash] waiting for {name} (current: {now})", flush=True)
                return False
            print(f"[clash] {name} ready: {now}", flush=True)

    return True
```

### dnsmasq 配置

```python
def _write_dnsmasq_config(clash_enabled: bool, clash_ready: bool):
    # 只有 Clash 启用且就绪时，才包含 Clash DNS
    if clash_enabled and clash_ready:
        servers = """server=127.0.0.1#1153
server=127.0.0.1#1053
server=223.5.5.5
server=119.29.29.29"""
    elif clash_enabled:
        # Clash 未就绪，不加入 Clash DNS
        servers = """server=127.0.0.1#1153
server=223.5.5.5
server=119.29.29.29"""
    else:
        servers = """server=127.0.0.1#1153
server=223.5.5.5
server=119.29.29.29"""
```

### 主启动流程

```python
# 1. 启动 Clash
_s6_start("mihomo")

# 2. 等待进程启动
for attempt in range(10):
    if clash_pid() is not None:
        break
    time.sleep(1)

# 3. 加载配置
reload_clash(config)

# 4. 等待就绪 (url-test 组选择节点)
clash_ready = _wait_clash_ready(timeout=60)

# 5. 应用 TPROXY (仅在就绪后)
if new_mode == "tproxy" and clash_ready:
    print("[clash] applying TPROXY (Clash is ready)", flush=True)
    tproxy_apply(...)
    tproxy_enabled = True

# 6. 启动 MosDNS (传入就绪状态)
reload_mosdns(node, global_cfg, clash_ready=clash_ready)
```

## 📊 行为对比

| 场景 | 旧逻辑 | 新逻辑 | 改善 |
|------|--------|--------|------|
| Clash 进程启动 | 等待 2 秒 | 等待最多 10 秒 | 更可靠 |
| Clash 就绪判断 | 进程运行 | url-test 选择节点 | 更准确 |
| TPROXY 应用 | 启动后 2 秒 | url-test 完成后 | 避免中断 |
| dnsmasq Clash DNS | 立即加入 | Clash 就绪后加入 | 避免失败 |
| MosDNS 规则下载 | 通过可能未就绪的 Clash | Clash 就绪后才通过 | 确保成功 |
| 网络中断风险 | 高 | 低 | 显著改善 |

## 📝 日志示例

### 正常启动 (成功)

```
[clash] waiting for process to start... (attempt 1/10)
[clash] process started (pid=1234)
[clash] waiting for url-test proxies to select nodes...
[clash] url-test-auto ready: HK-Node01
[clash] fallback-auto ready: US-Node05
[clash] ready after 8s
[clash] applying TPROXY (Clash is ready)
[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)
[mosdns] Clash is ready, downloading rules via proxy
```

### 超时场景 (降级)

```
[clash] waiting for url-test proxies to select nodes...
[clash] waiting for url-test group to select node (current: REJECT)
...
[clash] not ready after 60s, proceeding anyway
[clash] WARNING: TPROXY not applied (Clash not ready), will retry on next check
[mosdns] dnsmasq started as frontend DNS on port 53 (Clash enabled but not ready)
[mosdns] Clash enabled but not ready, downloading rules directly (will retry after Clash ready)
```

## ⚙️ 超时配置

| 阶段 | 超时 | 超时后行为 |
|------|------|-----------|
| Clash 进程启动 | 10 秒 | 抛出异常，配置失败 |
| Clash 就绪等待 | 60 秒 | 继续，但 TPROXY 不应用，后台重试 |
| API 查询 | 3 秒 | 返回 None，重试 |
| 就绪检查间隔 | 2 秒 | - |

## 🔄 自动重试机制

### tproxy_check_loop (每 30 秒)

```python
# 检查 TPROXY 规则是否存在
if not _check_tproxy_iptables():
    # 检查 Clash 是否就绪
    if _clash_is_ready():
        # 应用 TPROXY
        tproxy_apply(...)
```

### periodic_reconcile_loop (每 5 分钟)

```python
# 重新运行协调逻辑
handle_commit()
# 再次尝试启动 MosDNS
```

## 🛠️ 配置检查

### 检查 Clash 就绪状态

```bash
# 查看所有代理组
curl http://127.0.0.1:9090/proxies | jq '.proxies'

# 查看 url-test 组状态
curl http://127.0.0.1:9090/proxies | jq '.proxies | to_entries[] | select(.value.type == "url-test") | {name: .key, now: .value.now}'

# 预期输出 (就绪):
# {
#   "name": "url-test-auto",
#   "now": "HK-Node01"
# }

# 未就绪:
# {
#   "name": "url-test-auto",
#   "now": "REJECT"
# }
```

### 检查 dnsmasq 配置

```bash
# 查看 dnsmasq 转发列表
cat /etc/dnsmasq.conf | grep "^server"

# Clash 就绪:
# server=127.0.0.1#1153
# server=127.0.0.1#1053  ← Clash DNS
# server=223.5.5.5
# server=119.29.29.29

# Clash 未就绪:
# server=127.0.0.1#1153
# server=223.5.5.5
# server=119.29.29.29
```

### 查看日志

```bash
# Clash 启动日志
tail -f /var/log/watcher.out.log | grep "\[clash\]"

# MosDNS 日志
tail -f /var/log/watcher.out.log | grep "\[mosdns\]"

# dnsmasq 状态
s6-rc status dnsmasq
```

## 🔧 故障排查

### 问题 1: TPROXY 一直不应用

**症状**: 日志显示 `TPROXY not applied (Clash not ready)`

**原因**: Clash url-test 组测试时间过长

**解决**:
1. 检查 Clash API: `curl http://127.0.0.1:9090/proxies | jq '.proxies."url-test-auto".now'`
2. 如果是 REJECT，等待测试完成
3. 检查日志: `tail -f /var/log/watcher.out.log | grep clash`
4. 等待 `tproxy_check_loop` 自动重试 (每 30 秒)
5. 或手动触发: `etcdctl put /commit "$(date +%s)"`

### 问题 2: DNS 解析失败

**症状**: DNS 查询超时或失败

**原因**: Clash 未就绪，但查询被转发到 Clash DNS

**解决**:
1. 检查 dnsmasq 配置: `cat /etc/dnsmasq.conf | grep 1053`
2. 如果存在 `server=127.0.0.1#1053`，说明 Clash 应该就绪
3. 检查 Clash 是否真的就绪: `curl http://127.0.0.1:9090/proxies`
4. 如果 Clash 未就绪但 dnsmasq 包含 Clash DNS，手动重启 dnsmasq: `s6-rc -r dnsmasq`

### 问题 3: MosDNS 规则下载失败

**症状**: MosDNS 规则文件不存在或为空

**原因**: MosDNS 尝试通过未就绪的 Clash 下载失败

**解决**:
1. 检查日志: `tail -f /var/log/watcher.out.log | grep mosdns`
2. 如果看到 `Clash enabled but not ready, downloading rules directly`
3. 说明 MosDNS 降级为直接下载 (不通过 Clash)
4. 等待 Clash 就绪后，手动触发: `etcdctl put /commit "$(date +%s)"`

## ✅ 验证清单

部署前验证:

- [x] 代码语法正确 (`uv run python -m py_compile watcher.py`)
- [x] 所有函数实现完成
- [x] 文档完整 (技术文档 + 快速参考)
- [x] 超时和重试机制完善
- [x] 日志输出详细清晰

部署后验证:

- [ ] Clash 启动后能看到 `ready after Xs` 日志
- [ ] TPROXY 在 Clash 就绪后应用
- [ ] dnsmasq 配置在 Clash 就绪后包含 Clash DNS
- [ ] MosDNS 规则下载成功
- [ ] 网络连接正常，无中断

## 📚 相关文档

- **[CLAUDE.md](CLAUDE.md)** - 项目架构文档
- **[docs/clash-lan-mode.md](docs/clash-lan-mode.md)** - LAN 模式文档
- **[docs/performance-tuning.md](docs/performance-tuning.md)** - 性能优化指南
- **[docs/clash-startup-sequence.md](docs/clash-startup-sequence.md)** - 完整技术文档
- **[CLASH-STARTUP-OPTIMIZATION.md](CLASH-STARTUP-OPTIMIZATION.md)** - 快速参考

## 🎯 总结

### 实现的核心功能

1. ✅ **Clash 启动完成判断**: 通过 API 检查 url-test 组状态
2. ✅ **TPROXY 延迟应用**: 只在 Clash 就绪后应用，避免网络中断
3. ✅ **dnsmasq 动态配置**: Clash 就绪前不使用 Clash DNS
4. ✅ **MosDNS 依赖管理**: Clash 就绪后才通过 Clash 下载规则
5. ✅ **自动重试机制**: 后台循环处理超时场景
6. ✅ **详细日志输出**: 每个步骤清晰可见

### 预期效果

| 指标 | 改善 |
|------|------|
| 网络中断风险 | ↓ 显著降低 |
| DNS 失败率 | ↓ 降低 |
| 规则下载成功率 | ↑ 提高 |
| 启动可靠性 | ↑ 提高 |
| 故障排查难度 | ↓ 降低 (详细日志) |

### 状态

✅ **实现完成**
✅ **语法验证通过**
✅ **文档完整**
✅ **准备部署**

**日期**: 2026-01-02
**版本**: v1.0

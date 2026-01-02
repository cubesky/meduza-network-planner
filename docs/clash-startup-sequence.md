# Clash 启动顺序优化

## 概述

为了确保服务之间的依赖关系正确，避免网络中断和 DNS 解析失败，系统现在实施更严格的启动顺序：

1. **Clash 必须完全就绪后才能应用 TPROXY** (避免网络断开)
2. **依赖 Clash 的服务(MosDNS)只在 Clash 就绪后启动**
3. **dnsmasq 在 Clash 就绪前不将 Clash DNS 加入转发列表**

## Clash 启动完成判断

Clash 的"就绪"状态不再仅仅是进程运行，而是通过 Clash API 检查：

### 检查逻辑

```python
def _clash_is_ready() -> bool:
    """检查 Clash 是否就绪 (url-test 代理已选择非 REJECT 节点)"""
    proxies = _clash_api_get("/proxies")

    for name, proxy in proxies.get("proxies", {}).items():
        proxy_type = proxy.get("type", "")
        if proxy_type in ("url-test", "fallback"):
            now = proxy.get("now")
            # 必须选择了非 REJECT/DIRECT 的节点
            if not now or now == "REJECT" or now == "DIRECT":
                return False

    return True
```

### 为什么这样判断？

- **url-test** 和 **fallback** 类型的代理组需要时间测试所有节点延迟
- 在测试完成前，这些组可能选择 `REJECT` 或 `DIRECT`
- 如果此时应用 TPROXY，会导致流量被错误处理或网络中断
- 只有当所有 url-test/fallback 组选择了实际的代理节点后，才认为 Clash 就绪

## 新的启动顺序

### 时序图

```
1. 启动 Clash (mihomo)
   ↓
2. 等待 Clash 进程启动 (最多 10 秒)
   ↓
3. 加载 Clash 配置 (reload_clash)
   ↓
4. 等待 Clash 就绪 (最多 60 秒)
   - 通过 API 检查 url-test 组
   - 每 2 秒检查一次
   - 所有 url-test/fallback 组必须选择非 REJECT 节点
   ↓
5. Clash 就绪后:
   ├─ 应用 TPROXY (如果是 tproxy 模式)
   ├─ 重启 dnsmasq (将 Clash DNS 加入转发列表)
   └─ 启动 MosDNS (使用 Clash 代理下载规则)
```

### 日志示例

```
[clash] waiting for process to start... (attempt 1/10)
[clash] process started (pid=1234)
[clash] waiting for url-test proxies to select nodes...
[clash] waiting for url-test group to select node (current: REJECT)
[clash] waiting for url-test group to select node (current: DIRECT)
[clash] url-test-auto ready: HK-Node01
[clash] fallback-auto ready: US-Node05
[clash] ready after 8s
[clash] applying TPROXY (Clash is ready)
[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)
[mosdns] Clash is ready, downloading rules via proxy
```

## 实现细节

### 1. Clash API 查询函数

**[watcher.py:804-817](watcher.py#L804-L817)**

```python
def _clash_api_get(endpoint: str) -> Optional[dict]:
    """查询 Clash API 并返回 JSON 响应"""
    cp = subprocess.run(
        ["curl", "-s", "--max-time", "3", f"http://127.0.0.1:9090{endpoint}"],
        capture_output=True,
        text=True,
        timeout=5,
    )
    if cp.returncode == 0 and cp.stdout:
        return json.loads(cp.stdout)
    return None
```

### 2. Clash 就绪检查

**[watcher.py:820-841](watcher.py#L820-L841)**

```python
def _clash_is_ready() -> bool:
    """检查 Clash 是否就绪"""
    proxies = _clash_api_get("/proxies")
    if not proxies:
        return False

    # 检查所有 url-test 和 fallback 组
    for name, proxy in proxies.get("proxies", {}).items():
        proxy_type = proxy.get("type", "")
        if proxy_type in ("url-test", "fallback"):
            now = proxy.get("now")
            if not now or now == "REJECT" or now == "DIRECT":
                print(f"[clash] waiting for {name} to select node (current: {now})", flush=True)
                return False
            print(f"[clash] {name} ready: {now}", flush=True)

    return True
```

### 3. 等待 Clash 就绪

**[watcher.py:844-854](watcher.py#L844-L854)**

```python
def _wait_clash_ready(timeout: int = 60) -> bool:
    """等待 Clash 就绪 (url-test 组已选择节点)"""
    print("[clash] waiting for url-test proxies to be ready...", flush=True)
    start = time.time()
    while time.time() - start < timeout:
        if _clash_is_ready():
            print(f"[clash] ready after {int(time.time() - start)}s", flush=True)
            return True
        time.sleep(2)
    print(f"[clash] not ready after {timeout}s, proceeding anyway", flush=True)
    return False
```

### 4. dnsmasq 配置修改

**[watcher.py:1327-1372](watcher.py#L1327-L1372)**

```python
def _write_dnsmasq_config(clash_enabled: bool = False, clash_ready: bool = False) -> None:
    """生成 dnsmasq 配置

    Args:
        clash_enabled: Clash 是否已配置
        clash_ready: Clash 是否就绪 (url-test 组已选择节点)
    """
    # 只有在 Clash 启用且就绪时，才将 Clash DNS 加入转发列表
    if clash_enabled and clash_ready:
        servers = """server=127.0.0.1#1153
server=127.0.0.1#1053
server=223.5.5.5
server=119.29.29.29"""
        status = "with Clash DNS"
    elif clash_enabled:
        # Clash 启用但未就绪 - 不将 Clash DNS 加入转发列表
        servers = """server=127.0.0.1#1153
server=223.5.5.5
server=119.29.29.29"""
        status = "Clash enabled but not ready (DNS not in forwarding list yet)"
    else:
        servers = """server=127.0.0.1#1153
server=223.5.5.5
server=119.29.29.29"""
        status = "without Clash DNS"

    # ... 写入配置文件
    return status
```

### 5. MosDNS 启动修改

**[watcher.py:1375-1417](watcher.py#L1375-L1417)**

```python
def reload_mosdns(node: Dict[str, str], global_cfg: Dict[str, str], clash_ready: bool = False) -> None:
    """重载 MosDNS 配置

    Args:
        node: etcd 节点配置
        global_cfg: etcd 全局配置
        clash_ready: Clash 是否就绪 (url-test 组已选择节点)
    """
    # ...

    # 只有在 Clash 启用且就绪时，才将 Clash DNS 加入 dnsmasq 转发列表
    status = _write_dnsmasq_config(clash_enabled=clash_enabled, clash_ready=clash_ready)
    _s6_restart("dnsmasq")
    print(f"[mosdns] dnsmasq started as frontend DNS on port 53 ({status})", flush=True)

    refresh_minutes = out["refresh_minutes"]
    if _should_refresh_rules(refresh_minutes):
        # 如果 Clash 启用且就绪，使用它下载规则
        if clash_enabled and clash_ready:
            print(f"[mosdns] Clash is ready, downloading rules via proxy", flush=True)
        elif clash_enabled:
            print(f"[mosdns] Clash enabled but not ready, downloading rules directly", flush=True)
        else:
            print(f"[mosdns] Clash not enabled, downloading rules directly", flush=True)

        _download_rules_with_backoff(out.get("rules", {}))
```

### 6. 主协调逻辑修改

**[watcher.py:1493-1599](watcher.py#L1493-L1599)**

```python
clash_enabled = node.get(f"/nodes/{NODE_ID}/clash/enable") == "true"
clash_ready = False  # 跟踪 Clash 是否就绪

if clash_changed:
    if clash_enabled:
        # 启动 Clash
        _s6_start("mihomo")

        # 等待进程启动
        for attempt in range(10):
            if clash_pid() is not None:
                break
            time.sleep(1)

        # 加载配置
        reload_clash(out["config_yaml"])

        # 等待 Clash 就绪 (url-test 组已选择节点)
        clash_ready = _wait_clash_ready(timeout=60)

        # 只在 Clash 就绪后应用 TPROXY
        if new_mode == "tproxy":
            if clash_ready:
                print("[clash] applying TPROXY (Clash is ready)", flush=True)
                tproxy_apply(...)
                tproxy_enabled = True
            else:
                print("[clash] WARNING: TPROXY not applied (Clash not ready)", flush=True)

# MosDNS: 只在 Clash 就绪后启动
if clash_enabled and not clash_ready and clash_changed:
    print("[mosdns] skipping reload (waiting for Clash to be ready)", flush=True)
elif changed("mosdns", mosdns_material):
    if mosdns_enabled:
        reload_mosdns(node, global_cfg, clash_ready=clash_ready)
```

## 配置示例

### Clash 配置 (url-test 代理组)

```yaml
proxy-groups:
  - name: proxy-auto
    type: url-test
    url: 'http://www.gstatic.com/generate_204'
    interval: 300
    proxies:
      - HK-Node01
      - HK-Node02
      - US-Node01
      - US-Node02

  - name: fallback-auto
    type: fallback
    url: 'http://www.gstatic.com/generate_204'
    interval: 300
    proxies:
      - HK-Node01
      - US-Node01
```

### etcd 配置

```bash
# 启用 Clash
etcdctl put /nodes/gateway1/clash/enable "true"
etcdctl put /nodes/gateway1/clash/mode "tproxy"

# 启用 MosDNS (依赖 Clash)
etcdctl put /nodes/gateway1/mosdns/enable "true"

# 触发配置
etcdctl put /commit "$(date +%s)"
```

## 行为变化

### Before (旧逻辑)

```
1. 启动 Clash
2. 等待 2 秒
3. 应用 TPROXY (可能 Clash 还在测试节点!)
4. 启动 MosDNS (DNS 可能通过未就绪的 Clash)
   ↓
问题: 网络可能中断，DNS 可能失败
```

### After (新逻辑)

```
1. 启动 Clash
2. 等待进程启动 (最多 10 秒)
3. 加载配置
4. 等待 url-test 组选择节点 (最多 60 秒)
5. Clash 就绪:
   ├─ 应用 TPROXY (避免网络中断)
   ├─ dnsmasq 加入 Clash DNS
   └─ MosDNS 下载规则 (通过代理)
   ↓
结果: 网络稳定，DNS 正常
```

## 超时处理

### Clash 进程启动超时 (10 秒)

```bash
[clash] waiting for process to start... (attempt 1/10)
[clash] waiting for process to start... (attempt 2/10)
...
[clash] failed to start after 10s
```

**行为**: 抛出异常，配置应用失败

### Clash 就绪超时 (60 秒)

```bash
[clash] waiting for url-test proxies to select nodes...
[clash] waiting for url-test group to select node (current: REJECT)
...
[clash] not ready after 60s, proceeding anyway
[clash] WARNING: TPROXY not applied (Clash not ready), will retry on next check
[mosdns] Clash enabled but not ready, downloading rules directly
```

**行为**:
- TPROXY 不应用 (避免网络中断)
- MosDNS 启动但不使用 Clash 代理下载规则
- dnsmasq 不将 Clash DNS 加入转发列表
- `tproxy_check_loop` 会周期性重试

### 后续重试

```
tproxy_check_loop (每 30 秒):
  ├─ 检查 Clash 是否就绪
  ├─ 如果就绪且 TPROXY 未应用，则应用 TPROXY
  └─ 重启 MosDNS 和 dnsmasq (加入 Clash DNS)

periodic_reconcile_loop (每 5 分钟):
  └─ 重新运行 handle_commit()，再次尝试启动 MosDNS
```

## 故障排查

### 问题: Clash 启动后网络断开

**原因**: url-test 组还在测试，选择了 REJECT

**解决**:
- 等待 url-test 测试完成 (通常 5-15 秒)
- 检查日志: `[clash] waiting for url-test group to select node`
- 检查日志: `[clash] ready after Xs`

### 问题: MosDNS 规则下载失败

**原因**: Clash 未就绪，MosDNS 尝试通过 Clash 代理下载失败

**解决**:
- 等待 Clash 就绪 (查看日志中的 `ready after Xs`)
- 或临时禁用 Clash，直接下载规则
- 或增加 MosDNS 规则刷新间隔，等待 Clash 完全就绪

### 问题: DNS 解析缓慢

**原因**: Clash 未就绪，但 dnsmasq 已将 Clash DNS 加入转发列表

**解决**:
- 检查日志: `dnsmasq started ... (Clash enabled but not ready)`
- 等待 Clash 就绪后，dnsmasq 会自动重启
- 手动重启 dnsmasq: `s6-rc -r dnsmasq`

### 问题: TPROXY 一直不应用

**原因**: Clash url-test 组测试时间过长或超时

**检查**:
```bash
# 检查 Clash API
curl http://127.0.0.1:9090/proxies | jq '.proxies | to_entries[] | select(.value.type == "url-test") | {name: .key, now: .value.now}'

# 检查 tproxy_check_loop 日志
tail -f /var/log/watcher.out.log | grep tproxy-check
```

**解决**:
- 等待 `tproxy_check_loop` 自动重试 (每 30 秒)
- 手动触发重新配置: `etcdctl put /commit "$(date +%s)"`

## 相关文档

- **[CLAUDE.md](CLAUDE.md)** - 项目架构文档
- **[docs/clash-lan-mode.md](docs/clash-lan-mode.md)** - Clash LAN 模式
- **[docs/performance-tuning.md](docs/performance-tuning.md)** - 性能优化指南

## 总结

通过实施严格的启动顺序：

✅ **避免网络中断**: TPROXY 只在 Clash 完全就绪后应用
✅ **DNS 稳定性**: dnsmasq 只在 Clash 就绪后使用 Clash DNS
✅ **规则下载**: MosDNS 只在 Clash 就绪后通过代理下载规则
✅ **自动重试**: 如果 Clash 未就绪，后台循环会自动重试
✅ **清晰日志**: 每个步骤都有详细的日志输出

**状态**: ✅ 已实现
**日期**: 2026-01-02

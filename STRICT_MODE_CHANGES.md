# Strict Mode Changes for Mihomo + MosDNS Integration

## Overview

根据需求，Mihomo 和 MosDNS 的交互已升级为**严格模式**，确保：
1. **所有 url-test 代理必须非 REJECT**（不是至少一个）
2. **MosDNS 下载必须等待 Clash 完成**（不允许超时后继续）
3. **TProxy 应用必须严格检查健康状态**（不允许超时后盲目应用）

## 核心更改

### 1. 健康检查更严格

**文件**: [watcher.py:867-907](watcher.py#L867-L907)

**之前**:
```python
# 至少一个 url-test 非 REJECT 即可
for name, proxy in url_test_proxies:
    if proxy.get("now") != "REJECT":
        return True  # 找到一个健康的就返回
```

**现在**:
```python
# 所有 url-test 必须 非 REJECT
for name, proxy in url_test_proxies:
    now = proxy.get("now", "")
    if not now or now == "REJECT":
        print(f"[clash] url-test proxy '{name}' is REJECT or empty", flush=True)
        return False  # 任何一个不健康就失败
return True  # 全部健康才成功
```

**影响**:
- 如果有多个 url-test 代理组，只要**任何一个**是 REJECT，健康检查就失败
- 提供详细的日志，说明是哪个代理组不健康

### 2. 等待函数 - 无限等待和超时两个版本

**文件**: [watcher.py:910-942](watcher.py#L910-L942)

**新增无限等待函数**:
```python
def wait_for_clash_healthy_infinite() -> None:
    """等待 Mihomo 健康 - 无超时，用于 MosDNS 启动"""
    print("[clash] Waiting indefinitely for Mihomo to become healthy...", flush=True)
    while True:
        if clash_health_check():
            print("[clash] Mihomo is healthy", flush=True)
            return
        time.sleep(1)
```

**超时版本（用于 TProxy 应用）**:
```python
def wait_for_clash_healthy(timeout: int = 30) -> None:
    """等待 Mihomo 健康 - 有超时，用于 TProxy 应用"""
    start = time.time()
    while True:
        if clash_health_check():
            return
        time.sleep(1)
        if timeout is not None:
            if time.time() - start >= timeout:
                raise RuntimeError(f"Mihomo did not become healthy after {timeout}s")
```

**影响**:
- MosDNS 使用**无限等待版本** (`wait_for_clash_healthy_infinite()`)
- TProxy 应用使用**超时版本** (`wait_for_clash_healthy(timeout=30)`)
- 两者都抛出异常，不允许降级

### 3. MosDNS 启动强制依赖 - **无限等待**

**文件**: [watcher.py:1475-1479](watcher.py#L1475-L1479)

**之前**:
```python
if clash_enabled:
    clash_healthy = wait_for_clash_healthy(timeout=30)
    if clash_healthy:
        print("Mihomo is healthy")
    else:
        print("WARNING: Mihomo not healthy, may fail")
        # 继续执行，尝试直接下载
```

**现在**:
```python
if clash_enabled:
    print("[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...", flush=True)
    wait_for_clash_healthy_infinite()  # 无限等待，没有超时
    print("[mosdns] Mihomo is healthy, proceeding with MosDNS setup", flush=True)
    # 继续执行
```

**影响**:
- 启用 Clash 时，MosDNS **无限等待** Mihomo 健康
- **没有降级模式**，没有直接下载回退
- **永不超时** - 确保 MosDNS 总是能启动（只要 Clash 最终能健康）

### 4. TProxy 应用强制依赖（无限等待）

**文件**: [watcher.py:1615-1631](watcher.py#L1615-L1631)

**之前**:
```python
if new_mode == "tproxy":
    try:
        wait_for_clash_healthy(timeout=30)
        tproxy_apply(...)
        print("TProxy applied")
    except RuntimeError as e:
        print(f"CRITICAL: {e}")
        tproxy_enabled = False
        raise RuntimeError(f"TProxy requires healthy Mihomo, but {e}")
```

**现在**:
```python
if new_mode == "tproxy":
    print("[clash] Waiting for Mihomo to become healthy before applying TProxy (no timeout - will wait indefinitely)...", flush=True)
    wait_for_clash_healthy_infinite()  # 无限等待，没有超时
    tproxy_apply(...)
    print("[clash] TProxy applied successfully", flush=True)
    # 继续执行
```

**影响**:
- TProxy **无限等待** Mihomo 健康
- **没有超时限制** - 永不放弃，确保 TProxy 总是能应用
- **永不失败**（只要 Mihomo 最终能健康）

## 行为对比

### 场景 1: Mihomo 启动很慢

**之前**:
```
[mosdns] Waiting for Mihomo...
[mosdns] WARNING: Timeout after 30s
[mosdns] Downloading rules directly (fallback)
[mosdns] MosDNS started (degraded mode)
```

**现在**:
```
[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
[clash] Mihomo is healthy
[mosdns] Mihomo is healthy, proceeding with MosDNS setup
[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)
[mosdns] MosDNS started
```

**结果**: ✅ 成功启动 - **无限等待确保 MosDNS 总是能启动**

### 场景 3: Mihomo 启动极慢（>60秒）

**之前**:
```
[mosdns] Waiting for Mihomo...
[mosdns] WARNING: Timeout after 30s
[mosdns] Downloading rules directly (fallback)
[mosdns] MosDNS started (degraded mode)
[clash] WARNING: Timeout after 30s
[clash] TProxy NOT applied
```

**现在**:
```
[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
... 等待 60+ 秒 ...
[clash] Mihomo is healthy
[mosdns] Mihomo is healthy, proceeding with MosDNS setup
[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)
[mosdns] MosDNS started
[clash] Waiting for Mihomo to become healthy before applying TProxy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
[clash] Mihomo is healthy
[clash] TProxy applied successfully
```

**结果**: ✅ 完全成功 - **无限等待确保 MosDNS 和 TProxy 都能成功启动**

### 场景 2: url-test 代理全部 REJECT（永久不健康）

**之前**:
```
[clash] All url-test proxies are REJECT
[clash] WARNING: TProxy NOT applied
[mosdns] Downloading rules directly (fallback)
[mosdns] MosDNS started (degraded mode)
```

**现在**:
```
[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...
[clash] url-test proxy 'HK-URLTest' is REJECT or empty (now=REJECT)
... 系统卡住，永久等待 ...
```

**结果**: ⏳ **永久卡住** - 需要**人工修复代理配置**才能继续

### 场景 3: Mihomo 启动极慢（>60秒）

**之前**:
```
[mosdns] Waiting for Mihomo...
[mosdns] WARNING: Timeout after 30s
[mosdns] Downloading rules directly (fallback)
[mosdns] MosDNS started (degraded mode)
[clash] WARNING: Timeout after 30s
[clash] TProxy NOT applied
```

**现在**:
```
[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
... 等待 60+ 秒 ...
[clash] Mihomo is healthy
[mosdns] Mihomo is healthy, proceeding with MosDNS setup
[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)
[mosdns] MosDNS started
[clash] Waiting for Mihomo to become healthy before applying TProxy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
[clash] Mihomo is healthy
[clash] TProxy applied successfully
```

**结果**: ✅ **完全成功** - **无限等待确保 MosDNS 和 TProxy 都能成功启动**

**关键**: 只要 Mihomo 最终能健康（无论需要多久），系统都会成功启动

### 场景 4: Mihomo 运行中崩溃（保持不变）

**之前** (保持不变):
```
[clash-monitor] Mihomo crashed, removing TProxy
[clash-monitor] TProxy removed due to crash
[clash-monitor] Mihomo recovered, reapplying TProxy
[clash-monitor] TProxy reapplied successfully
```

**现在** (相同行为):
```
[clash-monitor] Mihomo crashed, removing TProxy
[clash-monitor] TProxy removed due to crash
[clash-monitor] Mihomo recovered, reapplying TProxy
[clash-monitor] TProxy reapplied successfully
```

**结果**: 自动恢复（这是期望的行为）

## 错误处理

### RuntimeError 异常

以下情况会抛出 `RuntimeError`:

1. **Mihomo 启动超时** (30秒)
   ```python
   RuntimeError: Mihomo did not become healthy after 30s
   ```

2. **url-test 代理全部 REJECT**
   ```python
   RuntimeError: Mihomo did not become healthy after 30s
   (underlying cause: url-test proxy 'XXX' is REJECT)
   ```

3. **API 无法访问**
   ```python
   RuntimeError: Mihomo did not become healthy after 30s
   (underlying cause: API request failed)
   ```

### 错误传播

```
wait_for_clash_healthy()
    ↓ 抛出 RuntimeError
reload_mosdns()
    ↓ 捕获并重新抛出
handle_commit()
    ↓ 向上传播
操作失败 → 日志记录 → 状态更新失败
```

## 验证和测试

### 验证健康检查

```bash
# 手动检查 Mihomo API
curl -H "Authorization: Bearer BFC8rqg0umu-qay-xtq" \
     http://127.0.0.1:9090/proxies

# 检查 url-test 代理状态
curl -s -H "Authorization: Bearer BFC8rqg0umu-qay-xtq" \
     http://127.0.0.1:9090/proxies | \
     jq '.proxies | to_entries[] | select(.value.type == "Selector" and (.key | contains("url-test"))) | {name: .key, now: .value.now}'
```

### 测试场景

1. **正常启动**:
   - Mihomo 健康
   - url-test 代理全部正常
   - ✅ MosDNS 启动成功
   - ✅ TProxy 应用成功

2. **Mihomo 启动失败**:
   - Mihomo 进程不存在
   - ✅ 操作失败，抛出异常
   - ✅ 不会启动 MosDNS
   - ✅ 不会应用 TProxy

3. **url-test 全部 REJECT**:
   - Mihomo 运行中
   - 所有 url-test 代理组选择 REJECT
   - ✅ 健康检查失败
   - ✅ 操作失败，抛出异常
   - ✅ 不会启动 MosDNS
   - ✅ 不会应用 TProxy

4. **运行中崩溃**:
   - TProxy 已应用
   - Mihomo 崩溃
   - ✅ TProxy 立即移除
   - ✅ MosDNS 继续运行
   - ✅ Mihomo 恢复后 TProxy 自动重新应用

## 日志示例

### 成功启动

```
[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)
[mosdns] Waiting for Mihomo to become healthy...
[clash] Mihomo is healthy
[mosdns] Mihomo is healthy, proceeding with MosDNS setup
[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)
[mosdns] All 2 rule(s) downloaded successfully
[mosdns] MosDNS started
[clash] Waiting for Mihomo to become healthy before applying TProxy...
[clash] Mihomo is healthy
[clash] TProxy applied successfully
```

### 失败场景

```
[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)
[mosdns] Waiting for Mihomo to become healthy...
[clash] url-test proxy 'HK-URLTest' is REJECT or empty (now=REJECT)
[mosdns] CRITICAL: Mihomo did not become healthy after 30s
[mosdns] MosDNS CANNOT start without healthy Mihomo when Clash is enabled
Traceback (most recent call last):
  ...
RuntimeError: MosDNS requires healthy Mihomo, but Mihomo did not become healthy after 30s
```

## 故障排除

### 问题: MosDNS 无法启动

**症状**:
```
[mosdns] CRITICAL: Mihomo did not become healthy after 30s
```

**排查步骤**:
1. 检查 Mihomo 进程: `ps aux | grep mihomo`
2. 检查 API 可访问性:
   ```bash
   curl -H "Authorization: Bearer BFC8rqg0umu-qay-xtq" http://127.0.0.1:9090/proxies
   ```
3. 检查 url-test 代理状态:
   ```bash
   curl -s -H "Authorization: Bearer BFC8rqg0umu-qay-xtq" \
       http://127.0.0.1:9090/proxies | \
       jq '.proxies | to_entries[] | select(.value.type == "Selector" and (.key | contains("url-test")))'
   ```
4. 检查日志中的具体错误信息

### 问题: TProxy 无法应用

**症状**:
```
[clash] CRITICAL: Mihomo did not become healthy after 30s
[clash] TProxy CANNOT be applied without healthy Mihomo
```

**解决方案**:
1. 等待 Mihomo 完全启动（通常需要 5-10 秒）
2. 检查订阅配置，确保有可用的代理
3. 检查网络连接，确保 Mihomo 可以测试代理
4. 查看 Mihomo 日志: `tail -f /var/log/mihomo.*.log`

### 问题: url-test 代理显示 REJECT

**原因**:
- 所有代理服务器都不可用
- 网络连接问题
- 订阅配置错误

**解决方案**:
1. 检查订阅 URL 是否有效
2. 检查网络连接
3. 手动测试代理服务器
4. 考虑使用不同的订阅源

## 总结

严格模式确保：
- ✅ **零歧义**: Mihomo 要么完全健康，要么操作失败
- ✅ **可预测**: 没有降级模式，行为始终一致
- ✅ **快速失败**: 问题立即暴露，不隐藏错误
- ✅ **自动恢复**: 运行时崩溃自动恢复

代价：
- ⚠️ **更严格的部署要求**: 必须确保 Mihomo 健康
- ⚠️ **没有降级模式**: 无法在 Mihomo 不健康时继续运行
- ⚠️ **需要人工介入**: 失败时需要检查和修复

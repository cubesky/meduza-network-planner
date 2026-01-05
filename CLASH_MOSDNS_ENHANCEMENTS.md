# Mihomo and MosDNS Enhanced Interaction

This document describes the enhanced interaction between Mihomo (Clash Meta) and MosDNS, implementing **strict** sequential startup, health checks, and crash monitoring with automatic TProxy management.

## Overview

The enhanced implementation ensures that when both MosDNS and Mihomo are enabled, they start in a **strict** coordinated sequence with **mandatory** health validation. The system will **not** proceed if Mihomo is not healthy - no fallback, no best-effort attempts.

## Key Features

### 1. Mihomo API Health Check (STRICT)

**Location**: [watcher.py:867-907](watcher.py#L867-L907)

The `clash_health_check()` function verifies Mihomo health by checking:
- Process is running (via PID check)
- API is accessible (via `/proxies` endpoint)
- **ALL url-test proxies are NOT REJECT** (strict requirement - all must be healthy)

**API Credentials**:
- Endpoint: `http://127.0.0.1:9090` (configurable via `external-controller`)
- Secret: Loaded from config (default: `BFC8rqg0umu-qay-xtq`)
- Uses Bearer token authentication

**Strict Behavior**:
- If **any** url-test proxy is REJECT or empty, health check **FAILS**
- If **no** url-test proxies exist, assumes healthy if API is accessible
- Detailed logging of which proxy failed the health check

### 2. Sequential Startup Logic (MANDATORY - NO TIMEOUT)

**Location**: [watcher.py:1438-1494](watcher.py#L1438-L1494)

The `reload_mosdns()` function implements the following **strict** sequence:

1. **Start Dnsmasq** (frontend DNS on port 53)
   - Configured with or without Clash DNS based on Clash status
   - Ensures DNS is available during rule downloads

2. **Wait for Mihomo Health** (MANDATORY if Clash enabled - **NO TIMEOUT**)
   - **Waits INDEFINITELY for Mihomo to become healthy** - **NO TIMEOUT LIMIT**
   - Validates process, API, and **all** url-test proxy status
   - **MosDNS CANNOT start until Clash is healthy** - will wait forever if needed
   - **No fallback**, no best-effort, no direct downloads
   - **No timeout** - this ensures MosDNS always starts with healthy Clash

3. **Download MosDNS Rules** (via Mihomo proxy ONLY if Clash enabled)
   - **MUST use Mihomo proxy** if Clash is enabled
   - Implements intelligent retry with backoff
   - **No direct download fallback** when Clash is enabled

4. **Start MosDNS**
   - Only after Mihomo is confirmed healthy and rules downloaded
   - Configured to forward DNS queries appropriately

5. **Apply TProxy** (handled separately in Clash reload - see below)

### 3. TProxy Conditional Application (MANDATORY - NO TIMEOUT)

**Location**: [watcher.py:1615-1631](watcher.py#L1615-L1631)

When Clash is reloaded or configured in TProxy mode:

- **Before TProxy application**:
  - **Waits INDEFINITELY for Mihomo to become healthy** - **NO TIMEOUT LIMIT**
  - Validates API is accessible
  - Checks **all** url-test proxies are NOT REJECT

- **If healthy**:
  - Applies TProxy iptables rules
  - Enables crash monitoring loop
  - Caches TProxy target networks

- **TProxy will ALWAYS be applied** (once Clash is healthy):
  - **No timeout** - will wait forever for Clash to become healthy
  - **No best-effort application** - only applied when Clash is fully healthy
  - Crash monitoring enabled only after TProxy is applied
  - System waits indefinitely until Clash is ready

### 4. Crash Detection and Recovery

**Location**: [watcher.py:739-797](watcher.py#L739-L797)

The `clash_crash_monitor_loop()` runs as a daemon thread:

**Monitoring Behavior** (checks every 5 seconds):

1. **Detect Crash**:
   - Mihomo process dies
   - API becomes unreachable
   - All url-test proxies become REJECT

2. **Immediate TProxy Removal**:
   - Removes iptables TPROXY rules immediately
   - Prevents traffic from being trapped by non-existent proxy
   - Logs crash event

3. **Recovery Detection**:
   - Monitors for Mihomo becoming healthy again
   - When recovered:
     - Re-applies TProxy rules with cached configuration
     - Verifies exclusions (interfaces, ports, source networks)
     - Logs successful recovery

4. **State Tracking**:
   - `_clash_last_healthy`: Timestamp of last health check
   - `_clash_monitoring_enabled`: Only active when TProxy is enabled
   - Uses locks for thread-safe state updates

### 5. API Configuration Management

**Location**: [generators/gen_clash.py:73-77,100-101](generators/gen_clash.py#L73-L77)

The Clash generator now outputs API configuration:

```python
return {
    "config_yaml": ...,
    "mode": ...,
    "tproxy_targets": ...,
    "refresh_enable": ...,
    "refresh_interval_minutes": ...,
    "api_controller": merged.get("external-controller", "0.0.0.0:9090"),
    "api_secret": merged.get("secret", ""),
}
```

**Updated reload_clash() function**: [watcher.py:892-917](watcher.py#L892-L917)
- Accepts `api_controller` and `api_secret` parameters
- Updates global `CLASH_API_SECRET` for health checks
- Maintains backward compatibility

## Startup Sequence Diagram

```
┌─────────────────────────────────────────────────────────────┐
│ MosDNS + Mihomo Startup Sequence (STRICT - NO TIMEOUT)        │
└─────────────────────────────────────────────────────────────┘

1. Dnsmasq Start
   ├─ Port 53 (frontend DNS)
   └─ Configured with Clash DNS fallback

2. Mihomo Health Check (MANDATORY if Clash enabled - INFINITE WAIT)
   ├─ Check process running
   ├─ Check API accessible
   └─ Check ALL url-test proxies not REJECT
       ├─ ✓ All Healthy → Continue
       └─ ✗ Any Unhealthy → WAIT INDEFINITELY (no timeout)

3. Download MosDNS Rules (MANDATORY)
   ├─ Clash enabled? → Use Mihomo proxy (127.0.0.1:7890)
   └─ Clash disabled? → Direct download
   **Note**: No fallback when Clash is enabled

4. Start MosDNS
   └─ Ports 1153 (primary), 1053 (Clash DNS)

5. Apply TProxy (if Clash mode = tproxy)
   ├─ Wait for Mihomo health (NO TIMEOUT - INFINITE WAIT) - MANDATORY
   ├─ All proxies healthy? → Apply TProxy
   └─ Any proxy unhealthy? → WAIT INDEFINITELY (no timeout)
   **Note**: TProxy will ALWAYS be applied once Clash is healthy
```

## Crash Recovery Flow

```
┌─────────────────────────────────────────────────────────────┐
│ Mihomo Crash Recovery (TProxy Mode Only)                     │
└─────────────────────────────────────────────────────────────┘

Normal Operation
   ↓
[Health Check Every 5s]
   ↓
Crash Detected! (process died, API down, or all proxies REJECT)
   ↓
Immediate Action: Remove TProxy
   ├─ iptables -t mangle -F CLASH_TPROXY
   └─ Prevents traffic blackhole
   ↓
Monitoring Continues (waiting for recovery)
   ↓
Mihomo Recovered (process + API + proxies OK)
   ↓
Reapply TProxy
   ├─ Restore iptables rules
   ├─ Use cached target networks
   └─ Verify exclusions
   ↓
Normal Operation Resumed
```

## Configuration Requirements

### etcd Configuration

No additional etcd keys are required. The implementation works with existing Clash configuration:

```bash
# Enable Clash (optional, for MosDNS integration)
/nodes/<NODE_ID>/clash/enable = "true"
/nodes/<NODE_ID>/clash/mode = "tproxy"  # or "mixed"
/nodes/<NODE_ID>/clash/active_subscription = "subscription-name"

# Enable MosDNS (optional, for DNS filtering)
/nodes/<NODE_ID>/mosdns/enable = "true"

# Subscription with API config (auto-detected)
/global/clash/subscriptions/<name>/url = "https://..."
```

### Base Configuration

The [clash/base.yaml](clash/base.yaml) must include API configuration:

```yaml
external-controller: 0.0.0.0:9090
secret: 'BFC8rqg0umu-qay-xtq'
```

**Note**: API configuration is automatically ensured by the generator even if missing from subscription.

## Thread Safety

All shared state is protected by locks:

- `_clash_monitoring_lock`: Protects crash monitoring state
- `_clash_refresh_lock`: Protects subscription refresh state
- `_tproxy_check_lock`: Protects TProxy check state
- `_clash_last_healthy`: Updated atomically within monitoring loop

## Error Handling (STRICT)

### Mihomo Fails to Start or Unhealthy
- **Both MosDNS and TProxy wait indefinitely** for Mihomo to become healthy
- **No timeout**, no failure - system waits forever if needed
- **Crash monitoring remains disabled** until TProxy is applied
- **Once Mihomo becomes healthy**:
  - MosDNS starts immediately
  - TProxy is applied immediately
  - Crash monitoring is enabled
- **No fallback**, no degraded mode, no partial operation

### Mihomo Crashes During Operation
- **TProxy immediately removed** (prevents traffic blackhole)
- **MosDNS continues unaffected** (already running)
- **Automatic recovery** when Mihomo restarts and becomes healthy
- **TProxy automatically reapplied** on recovery

### MosDNS Rule Download Fails
- **Retry with exponential backoff** (up to 5 attempts)
- **Uses Mihomo proxy** if Clash is enabled (mandatory)
- **No direct download fallback** when Clash is enabled
- **Operation fails** if all download attempts fail

### API Request Fails During Health Check
- **Logged as error**
- **Health check returns False**
- **TProxy removed** if currently active
- **Operation fails** if health check is mandatory
- **Automatic recovery** when API becomes accessible (via crash monitor)

## Logging

All operations log to stdout with clear prefixes:

- `[clash]`: General Clash operations
- `[clash-monitor]`: Crash monitoring events
- `[mosdns]`: MosDNS startup and rule downloads
- `[clash-refresh]`: Subscription refresh operations

Example logs:

**Waiting indefinitely for Mihomo** (typical startup):
```
[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)
[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
[clash] Mihomo is healthy
[mosdns] Mihomo is healthy, proceeding with MosDNS setup
[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)
[mosdns] MosDNS started
```

**Successful startup** (Mihomo takes 60 seconds to become healthy):
```
[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)
[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
... 等待 60 秒 ...
[clash] Mihomo is healthy
[mosdns] Mihomo is healthy, proceeding with MosDNS setup
[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)
[mosdns] MosDNS started
[clash] Waiting for Mihomo to become healthy before applying TProxy (no timeout - will wait indefinitely)...
[clash] Waiting indefinitely for Mihomo to become healthy...
[clash] Mihomo is healthy
[clash] TProxy applied successfully
```

**Key**: Both MosDNS and TProxy wait indefinitely - no timeout, no failure.

**Crash and recovery**:
```
[clash-monitor] Mihomo crashed, removing TProxy
[clash-monitor] TProxy removed due to crash
[clash-monitor] Mihomo recovered, reapplying TProxy
[clash-monitor] TProxy reapplied successfully
```

## Implementation Files

### Modified Files

1. **generators/gen_clash.py**
   - Added API configuration output
   - Ensures `external-controller` and `secret` are present

2. **watcher.py**
   - Added `clash_health_check()` function
   - Added `wait_for_clash_healthy()` function
   - Added `_clash_api_request()` helper function
   - Added `clash_crash_monitor_loop()` daemon thread
   - Updated `reload_clash()` to accept API credentials
   - Updated `reload_mosdns()` with sequential startup logic
   - Updated Clash reload logic to apply TProxy only after health check
   - Added global state variables for crash monitoring

### Global Variables Added

```python
CLASH_API_PORT = 9090
CLASH_API_SECRET = ""

_clash_monitoring_lock = threading.Lock()
_clash_last_healthy = 0.0
_clash_monitoring_enabled = False
```

## Backward Compatibility

**IMPORTANT**: The strict mode changes **BREAK** backward compatibility for operations requiring Mihomo:

### Breaking Changes
- **MosDNS with Clash enabled**: **WILL FAIL** if Mihomo is not healthy (no fallback)
- **TProxy application**: **WILL FAIL** if Mihomo is not healthy (no best-effort)
- **Rule downloads**: **NO FALLBACK** to direct downloads when Clash is enabled

### Still Compatible
- MosDNS without Clash: Works as before (direct downloads)
- Clash without MosDNS: Works as before
- API configuration: Still auto-detected with defaults

### Migration Guide
If you have existing deployments:
1. **Ensure Mihomo is healthy** before enabling Clash+MosDNS integration
2. **Check url-test proxies** - ensure at least one is not REJECT
3. **Verify API is accessible** on port 9090
4. **Test in staging** before applying to production

## Future Enhancements

Possible improvements for future iterations:

1. **Configurable health check intervals**: Allow tuning check frequency via etcd
2. **Graceful shutdown**: Coordinate shutdown sequence when disabling services
3. **Metrics export**: Export health status to etcd for monitoring
4. **Custom health thresholds**: Allow configuring what constitutes "healthy"
5. **Proxy group validation**: More sophisticated proxy health validation

## Testing Recommendations

### Manual Testing

1. **Normal startup**:
   - Enable both Clash and MosDNS
   - Verify sequential startup in logs
   - Check TProxy is applied after health check

2. **Crash recovery**:
   - Kill Mihomo process while TProxy is active
   - Verify TProxy is immediately removed
   - Restart Mihomo
   - Verify TProxy is automatically reapplied

3. **Proxy validation**:
   - Configure subscription with REJECT proxies
   - Verify health check detects all-REJECT state
   - Verify TProxy is not applied when all proxies are REJECT

4. **API failure**:
   - Block API port (9090)
   - Verify TProxy is removed
   - Unblock API port
   - Verify automatic recovery

### Integration Testing

Test various combinations:
- Clash only (TProxy mode)
- Clash only (mixed mode)
- MosDNS only
- Both enabled
- Subscription refresh with TProxy active
- Configuration changes during operation

## References

- [Mihomo API Documentation](https://wiki.metacubex.one/config/)
- [TPROXY Implementation](docs/tproxy.md) (if exists)
- [MosDNS Configuration](docs/mosdns.md)
- [CLAUDE.md](CLAUDE.md) - General project architecture

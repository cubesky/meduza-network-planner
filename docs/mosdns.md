# MosDNS

MosDNS is optional and controlled per node.

## Enable

Set `/nodes/<NODE_ID>/mosdns/enable` to `true` to start MosDNS on that node.
If the key is missing or not `true`, MosDNS is disabled.

When enabled, the watcher:
1. Writes `/etc/dnsmasq.conf` and starts dnsmasq on port 53 (frontend DNS)
2. Writes `/etc/mosdns/config.yaml`
3. Downloads rule files
4. Starts MosDNS
5. Sets `/etc/resolv.conf` to `127.0.0.1`

## Rule files

Rules are stored in etcd under:

- `/global/mosdns/rule_files`

Value must be a JSON object that maps relative file paths to URLs:

```json
{
  "ddns.txt": "https://profile.kookxiang.com/rules/mosdns/ddns.txt",
  "block.txt": "https://profile.kookxiang.com/rules/mosdns/block.txt",
  "geosite/private.txt": "https://profile.kookxiang.com/geosite/domains/private"
}
```

Downloaded files are written to `/etc/mosdns/<path>`.

## Plugins

Plugins are stored in etcd under:

- `/global/mosdns/plugins`

Value must be a YAML list. Example:

```yaml
- tag: cache
  type: cache
  args:
    size: 4194304
    lazy_cache_ttl: 86400
    dump_file: cache.dat
    dump_interval: 600

- tag: hosts
  type: hosts
  args:
    files:
      - /etc/mosdns/hosts.txt
```

This list becomes the `plugins:` section of `/etc/mosdns/config.yaml`.
If the key is missing or empty, `/mosdns/config.yaml` is used as the default.

## Rule updates

Rules are refreshed based on the `refresh` interval only.

## Refresh

Rule updates are controlled by `/nodes/<NODE_ID>/mosdns/refresh` (minutes).
Default is `1440` (24 hours). If missing or invalid, the default is used.

## Socks port

MosDNS uses a fixed SOCKS port: `7891`.

## HTTP proxy for rule downloads

**Automatic Clash Proxy Integration**: MosDNS automatically detects if Clash is running and uses its HTTP proxy for downloading rule files. This significantly speeds up downloads when network access is restricted.

### Behavior

1. **Clash Running**: Uses Clash HTTP proxy (`http://127.0.0.1:7890`) by default
   - Faster downloads through proxy
   - Bypasses network restrictions
   - Logs: `[mosdns] Using Clash proxy for rule downloads: http://127.0.0.1:7890`

2. **Clash Not Running**: Downloads directly or uses explicit proxy
   - Direct download if no proxy configured
   - Logs: `[mosdns] Clash not running, downloading rules directly (may be slow)`

3. **Manual Override**: Set `MOSDNS_HTTP_PROXY` environment variable to force a specific proxy

### Startup Sequence

When both Clash and MosDNS are enabled:
1. Clash starts first
2. MosDNS waits up to 10 seconds for Clash to be ready
3. Rule files are downloaded via Clash proxy
4. MosDNS starts with updated rules

This ensures MosDNS rules are always downloaded optimally without manual configuration.

### Implementation

**Location**: [watcher.py:1061-1098](watcher.py#L1061-L1098)

```python
def _download_rules(rules: Dict[str, str]) -> None:
    # Check if Clash is running
    clash_pid_val = clash_pid()
    if clash_pid_val is not None:
        proxy = os.environ.get("MOSDNS_HTTP_PROXY", f"http://127.0.0.1:{CLASH_HTTP_PORT}")
        # Use proxy for downloads
    else:
        # Direct download or explicit proxy
```

## Frontend DNS with dnsmasq

When MosDNS is enabled, dnsmasq automatically runs as a lightweight frontend DNS server on port 53.

### Why dnsmasq?

- **DNS availability during startup**: dnsmasq starts BEFORE MosDNS downloads rule files, ensuring DNS service is always available
- **Sequential fallback**: Tries multiple DNS servers in order (MosDNS → public DNS)
- **Hosts file integration**: Uses `/etc/etcd_hosts` for dynamic DNS from etcd
- **Private network support**: Doesn't block private IP results (RFC 1918)

### Configuration

The watcher generates `/etc/dnsmasq.conf` with these settings:

- **Port**: 53 (standard DNS port)
- **Upstream servers** (tried in order):
  1. `127.0.0.1#1153` (MosDNS primary)
  2. `127.0.0.1#1053` (Clash DNS - **only if Clash is enabled**)
  3. `223.5.5.5` (AliDNS public DNS)
  4. `119.29.29.29` (DNSPod public DNS)
- **Hosts file**: `/etc/etcd_hosts` (dynamic DNS from etcd)
- **Private networks**: Not blocked (`bogus-priv`)
- **No caching**: Cache is handled by upstream DNS servers
- **Strict ordering**: Queries servers in sequence

**Dynamic Configuration**:
- Clash DNS (`127.0.0.1#1053`) is only included when `/nodes/<NODE_ID>/clash/enable = true`
- If Clash is disabled, dnsmasq skips the Clash DNS server

**Port Syntax**:
- dnsmasq uses `#` for non-standard ports: `server=127.0.0.1#1153`
- This is the correct dnsmasq syntax for custom DNS ports

### Startup sequence

1. MosDNS enable detected
2. dnsmasq config written to `/etc/dnsmasq.conf`
3. dnsmasq started on port 53 (DNS now available)
4. MosDNS config written to `/etc/mosdns/config.yaml`
5. Rule files downloaded (DNS available via dnsmasq)
6. MosDNS started

This ensures DNS queries work even during MosDNS rule download phase.

### Implementation

**Location**: [watcher.py:1125-1142](watcher.py#L1125-L1142)

```python
def _write_dnsmasq_config(clash_enabled: bool = False) -> None:
    """Generate dnsmasq configuration for frontend DNS forwarding."""
    # Only include Clash DNS (1053) if Clash is enabled
    # dnsmasq uses # syntax for non-standard ports
    if clash_enabled:
        servers = """server=127.0.0.1#1153
server=127.0.0.1#1053
server=223.5.5.5
server=119.29.29.29"""
    else:
        servers = """server=127.0.0.1#1153
server=223.5.5.5
server=119.29.29.29"""

    config = f"""# dnsmasq configuration for MosDNS frontend
port=53
no-resolv
{servers}
addn-hosts=/etc/etcd_hosts
bogus-priv
strict-order
keep-in-foreground
log-queries=extra
"""
    _write_text("/etc/dnsmasq.conf", config, mode=0o644)
```

**Location**: [watcher.py:1145-1175](watcher.py#L1145-L1175)

```python
def reload_mosdns(node: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    # Write MosDNS config...
    # Start dnsmasq FIRST (before downloading rules)
    _write_dnsmasq_config()
    _supervisor_restart("dnsmasq")
    print("[mosdns] dnsmasq started as frontend DNS on port 53", flush=True)

    # Now download rules (DNS is available via dnsmasq)
    if _should_refresh_rules(refresh_minutes):
        _download_rules_with_backoff(out.get("rules", {}))

    # Finally start MosDNS
    _supervisor_restart("mosdns")
```

When MosDNS is disabled, dnsmasq is automatically stopped.

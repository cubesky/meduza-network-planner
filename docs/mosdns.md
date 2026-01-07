# MosDNS

MosDNS is optional and controlled per node.

## Enable

Set `/nodes/<NODE_ID>/mosdns/enable` to `true` to start MosDNS on that node.
If the key is missing or not `true`, MosDNS is disabled.

When enabled, the watcher:
1. Writes `/etc/dnsmasq.conf` and starts dnsmasq on port 53 (frontend DNS)
2. Writes MosDNS text files from etcd:
   - `/etc/mosdns/etcd_local.txt` (from `/global/mosdns/local`)
   - `/etc/mosdns/etcd_block.txt` (from `/global/mosdns/block`)
   - `/etc/mosdns/etcd_ddns.txt` (from `/global/mosdns/ddns`)
   - `/etc/mosdns/etcd_global.txt` (from `/global/mosdns/global`)
3. Writes `/etc/mosdns/config.yaml`
4. Downloads rule files
5. Restarts MosDNS
6. Sets `/etc/resolv.conf` to `127.0.0.1`

All changes are triggered by `/commit` - MosDNS is restarted when any of these files change.

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

## Text Files

MosDNS can load additional text lists from etcd. These are written directly to files:

### Local Domains
- **etcd key**: `/global/mosdns/local`
- **File path**: `/etc/mosdns/etcd_local.txt`
- **Usage**: Define local domain names (one per line)

Example:
```bash
etcdctl put /global/mosdns/local "local1.example.com
local2.example.com
home.local"
```

### Blocked Domains
- **etcd key**: `/global/mosdns/block`
- **File path**: `/etc/mosdns/etcd_block.txt`
- **Usage**: Block specific domains (one per line)

Example:
```bash
etcdctl put /global/mosdns/block "ads.example.com
tracker malicious.local"
```

### DDNS Domains
- **etcd key**: `/global/mosdns/ddns`
- **File path**: `/etc/mosdns/etcd_ddns.txt`
- **Usage**: Dynamic DNS domains (one per line)

Example:
```bash
etcdctl put /global/mosdns/ddns "myhome.ddns.net
office.dynamicdns.com"
```

### Global Domains
- **etcd key**: `/global/mosdns/global`
- **File path**: `/etc/mosdns/etcd_global.txt`
- **Usage**: Global domain list (one per line)

Example:
```bash
etcdctl put /global/mosdns/global "google.com
cloudflare.com
github.com"
```

**Important**:
- Files are created even if the key is missing (empty files)
- Files are rewritten on every `/commit` trigger
- MosDNS is restarted when any file changes
- Use newline (`\n`) to separate multiple entries

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

When MosDNS is enabled, dnsmasq automatically runs as a lightweight frontend DNS server on port 53 with mDNS support.

### Why dnsmasq?

- **DNS availability during startup**: dnsmasq starts BEFORE MosDNS downloads rule files, ensuring DNS service is always available
- **Sequential fallback**: Tries multiple DNS servers in order (MosDNS → public DNS)
- **Hosts file integration**: Uses `/etc/etcd_hosts` for dynamic DNS from etcd
- **mDNS support**: Integrates with Avahi for Multicast DNS (`.local` hostnames)
- **Private network support**: Doesn't block private IP results (RFC 1918)

### Configuration

The watcher generates `/etc/dnsmasq.conf` with these settings:

- **Port**: 53 (standard DNS port)
- **Upstream servers** (tried in order):
  1. `127.0.0.1#1153` (MosDNS primary)
  2. `127.0.0.1#1053` (Clash DNS - **only if Clash is enabled**)
  3. `119.29.29.29` (DNSPod public DNS)
  4. `1.0.0.1` (Cloudflare public DNS)
- **Hosts file**: `/etc/etcd_hosts` (dynamic DNS from etcd)
- **mDNS support**: Enabled via Avahi D-Bus (`enable-dbus=org.freedesktop.Avahi`)
- **Reverse DNS**: Supported for local networks (`local-ttl=1`)
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
3. dnsmasq started on port 53 (DNS now available with mDNS via Avahi)
4. MosDNS config written to `/etc/mosdns/config.yaml`
5. Rule files downloaded (DNS available via dnsmasq)
6. MosDNS started

This ensures DNS queries work even during MosDNS rule download phase, with full mDNS support enabled.

**Note**: D-Bus system service and Avahi daemon run at all times (autostart=true) to provide continuous mDNS support, independent of MosDNS state.

### Implementation

**Location**: [watcher.py:1125-1156](watcher.py#L1125-L1156)

```python
def _write_dnsmasq_config(clash_enabled: bool = False) -> None:
    """Generate dnsmasq configuration for frontend DNS forwarding."""
    # Only include Clash DNS (1053) if Clash is enabled
    # dnsmasq uses # syntax for non-standard ports
    if clash_enabled:
        servers = """server=127.0.0.1#1153
server=127.0.0.1#1053
server=119.29.29.29
server=1.0.0.1"""
    else:
        servers = """server=127.0.0.1#1153
server=119.29.29.29
server=1.0.0.1"""

    config = f"""# dnsmasq configuration for MosDNS frontend
port=53
no-resolv
{servers}
addn-hosts=/etc/etcd_hosts
bogus-priv
strict-order
keep-in-foreground
log-queries=extra
# Enable mDNS (Multicast DNS) via Avahi
enable-dbus=org.freedesktop.Avahi
# Enable reverse DNS (PTR records for local networks)
# Allow RFC 1918 private IP reverse lookups
bogus-priv
# Enable DHCP reverse lookup for local names
local-ttl=1
"""
    _write_text("/etc/dnsmasq.conf", config, mode=0o644)
```

**Location**: [watcher.py:1159-1205](watcher.py#L1159-L1205)

```python
def reload_mosdns(node: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": {}}
    out = _run_generator("gen_mosdns", payload)

    # Write MosDNS config
    with open("/etc/mosdns/config.yaml", "w", encoding="utf-8") as f:
        f.write(out["config_text"])

    # Write MosDNS text files from etcd (always write, even if empty)
    _write_text("/etc/mosdns/etcd_local.txt", out.get("local", ""), mode=0o644)
    _write_text("/etc/mosdns/etcd_block.txt", out.get("block", ""), mode=0o644)
    _write_text("/etc/mosdns/etcd_ddns.txt", out.get("ddns", ""), mode=0o644)
    _write_text("/etc/mosdns/etcd_global.txt", out.get("global", ""), mode=0o644)
    print("[mosdns] wrote etcd text files (local, block, ddns, global)", flush=True)

    # Start dnsmasq FIRST (before downloading rules)
    # Avahi and D-Bus are always running, no need to start them here
    _write_dnsmasq_config(clash_enabled=clash_enabled)
    _supervisor_restart("dnsmasq")

    # Download rules if needed
    if _should_refresh_rules(refresh_minutes):
        _download_rules_with_backoff(out.get("rules", {}))
        _touch_rules_stamp()

    # Finally start MosDNS
    _supervisor_restart("mosdns")
```

**Generator**: [generators/gen_mosdns.py](generators/gen_mosdns.py)

**Trigger**: Changes to MosDNS configuration trigger `/commit` watch → `reconcile_once()` → `reload_mosdns()` → MosDNS restart.

When MosDNS is disabled, dnsmasq is stopped. D-Bus and Avahi continue running to provide mDNS support for other services.

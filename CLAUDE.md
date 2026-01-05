# Meduza Network Planner - LLM Coding Guide

This document provides essential guidance for AI/LLM assistants working on the Meduza Network Planner project.

## Project Overview

Meduza is a single-container edge gateway that uses **etcd as the single source of truth** for network configuration. It coordinates multiple networking tools to create an overlay network with routing, VPN, and transparent proxying capabilities.

### Core Components

- **Overlay Network**: EasyTier or Tinc (mutually exclusive, selected globally)
- **Routing**: FRR (OSPF and/or BGP)
- **External Connectivity**: OpenVPN / WireGuard
- **Transparent Proxy**: Clash Meta (mihomo) with TPROXY mode
- **DNS**: MosDNS (optional, externally callable only)
- **Dynamic DNS**: etcd_hosts (watch etcd for host records)

### Architecture Principles

1. **etcd is the only configuration source** - All configuration is stored in etcd
2. **Single trigger mechanism** - Only `/commit` key is watched; changes trigger full reconciliation
3. **No file-based configuration** - All configs must be generated from etcd keys
4. **Generator pattern** - Each tool has a dedicated Python script in `generators/`
5. **Declarative semantics** - System converges to desired state defined in etcd

## Critical Design Constraints

### 1. Configuration Flow
```
etcd (keys) → generator scripts (Python) → config files → watcher reloads services
```

- **Never** write configuration files directly
- **Always** use generator scripts to transform etcd keys into config files
- Generator scripts read JSON from stdin, write JSON to stdout

### 2. Watcher Responsibilities
The `watcher.py` is the central orchestrator:

- Watches `/commit` key for changes
- On change, pulls relevant etcd keys for the node
- Calls generator scripts via `_run_generator()`
- Applies generated configs and reloads services
- Publishes status updates to `/updated/<NODE_ID>/...`

### 3. Threading and State Management
- Multiple background loops run as daemon threads
- Use locks for shared state: `_reconcile_lock`, `_ovpn_lock`, `_wg_lock`, `_clash_refresh_lock`, `_tproxy_check_lock`
- Never block the main watch loop
- Use backoff for retry logic (`Backoff` class)

### 4. etcd Interaction
- Use `load_prefix()` for bulk key retrieval
- Use `load_key()` for single keys
- Handle authentication failures with `_etcd_call()` wrapper
- Support for UNAUTHENTICATED status with automatic reconnection

## etcd Schema Structure

### Top-Level Keys
```
/global/...           # Global settings (mesh type, BGP filters, clash subscriptions)
/nodes/<NODE_ID>/...  # Per-node configuration
/commit               # Trigger key (any change triggers reconciliation)
```

### Node Configuration Schema

#### LAN Configuration
```python
/nodes/<NODE_ID>/lan = "10.42.10.0/24\n10.42.11.0/24"  # Newline-separated
/nodes/<NODE_ID>/private_lan = "10.99.10.0/24"          # Not exported to external BGP
```

#### Overlay Network Selection
```python
/global/mesh_type = "easytier" | "tinc"
/global/internal_routing_system = "ospf" | "bgp"
```

#### EasyTier Configuration
```python
# Global (required)
/global/easytier/network_name
/global/easytier/network_secret
/global/easytier/private_mode   # "true" | "false"
/global/easytier/dhcp           # "true" | "false"

# Per-node
/nodes/<NODE_ID>/easytier/enable
/nodes/<NODE_ID>/easytier/dev_name
/nodes/<NODE_ID>/easytier/ipv4
/nodes/<NODE_ID>/easytier/listeners          # Newline-separated
/nodes/<NODE_ID>/easytier/mapped_listeners   # Public addresses (newline-separated)
/nodes/<NODE_ID>/easytier/peers              # Newline-separated
```

#### Tinc Configuration
```python
# Global
/global/tinc/netname    # default: "mesh"
/global/tinc/cipher
/global/tinc/digest

# Per-node
/nodes/<NODE_ID>/tinc/enable
/nodes/<NODE_ID>/tinc/name              # default: NODE_ID
/nodes/<NODE_ID>/tinc/dev_name          # default: tnc0
/nodes/<NODE_ID>/tinc/port
/nodes/<NODE_ID>/tinc/address           # Public address
/nodes/<NODE_ID>/tinc/ipv4              # CIDR
/nodes/<NODE_ID>/tinc/subnet            # Newline-separated Subnet entries
/nodes/<NODE_ID>/tinc/public_key
/nodes/<NODE_ID>/tinc/ed25519_public_key
/nodes/<NODE_ID>/tinc/private_key
/nodes/<NODE_ID>/tinc/ed25519_private_key
```

#### OpenVPN Configuration
```python
/nodes/<NODE_ID>/openvpn/<NAME>/enable
/nodes/<NODE_ID>/openvpn/<NAME>/dev
/nodes/<NODE_ID>/openvpn/<NAME>/proto           # tcp-server | tcp-client | udp
/nodes/<NODE_ID>/openvpn/<NAME>/port
/nodes/<NODE_ID>/openvpn/<NAME>/remote          # Newline-separated
/nodes/<NODE_ID>/openvpn/<NAME>/ifconfig        # "local remote"
/nodes/<NODE_ID>/openvpn/<NAME>/keepalive       # "10 60"
# ... plus standard OpenVPN directives

# Secrets (inline content only, not file paths)
/nodes/<NODE_ID>/openvpn/<NAME>/secret
/nodes/<NODE_ID>/openvpn/<NAME>/ca
/nodes/<NODE_ID>/openvpn/<NAME>/cert
/nodes/<NODE_ID>/openvpn/<NAME>/key
/nodes/<NODE_ID>/openvpn/<NAME>/tls_auth
/nodes/<NODE_ID>/openvpn/<NAME>/tls_crypt

# BGP over OpenVPN (optional)
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/enable
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/peer_asn
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/peer_ip
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/update_source
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/weight
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/no_transit         # "true" | "false", default "false"
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/no_forward        # "true" | "false", default "false"
```

<a name="bgp-control-flags"></a>
**BGP Control Flags:**

- **no_transit**: Only learn routes directly originated by this peer (not transit routes).
  - **Effect**: Filters inbound routes based on AS_PATH length. Only accepts routes with AS_PATH ≤ 2.
  - **AS_PATH = 1**: Peer's own routes (directly originated by the peer) → **ACCEPT**
  - **AS_PATH = 2**: Peer's customer routes (one hop beyond peer) → **ACCEPT**
  - **AS_PATH > 2**: Transit routes (peer is providing transit for other ASes) → **DENY**
  - **Outbound**: Routes learned from this peer are still advertised to iBGP and other eBGP neighbors normally.
  - **Example**: If you have A - B - C, and C sets `no_transit=true` for B, then C will only learn B's own routes and B's customer routes. C will NOT learn routes that B learned from A (transit routes). However, C will still advertise B's routes to other neighbors, allowing them to reach B via C.
  - **Use Case**: Learn routes from a peer without using them as a transit AS. Useful for preferring direct paths over indirect ones.
  - **Implementation**: Uses AS_PATH filtering (`^.+ .+ .+` pattern) to deny routes with 3+ ASNs.

- **no_forward**: Only advertise locally-originated and iBGP routes to this peer.
  - **Effect**: Filters outbound routes to prevent this peer from reaching other eBGP networks through you.
  - **Allows to peer**:
    - Locally-originated routes (from `network` statements)
    - Routes from OSPF (internal LANs)
    - Routes learned from iBGP neighbors
  - **Denies to peer**: Routes learned from other eBGP peers
  - **Example**: If you have A - B - C - D, and C sets `no_forward=true` for B, then C will advertise C's own routes and iBGP routes to B, but will NOT advertise routes learned from D (or other eBGP peers) to B. B can reach C's networks, but cannot use C as a transit to reach D.
  - **Use Case**: Allow a peer to access your networks without using you as transit to other eBGP networks. Useful for backup links or bandwidth-limited connections.
  - **Implementation**: Uses BGP community (9999) to tag routes learned from eBGP peers, then filters them in outbound advertisements.

- **Interaction**: If both `no_transit` and `no_forward` are set, both flags take effect independently (no_transit affects inbound, no_forward affects outbound).

**Implementation Details:**
- `no_transit`: AS_PATH filter `^.+ .+ .+` (3+ ASNs) applied in inbound route-map
- `no_forward`: Community 9999 tags eBGP-learned routes, filtered in outbound route-map
- Both options preserve iBGP routes and OSPF-learned routes (internal connectivity)
- **Implementation**: [generators/gen_frr.py:137-161](generators/gen_frr.py#L137-L161)

#### WireGuard Configuration
```python
/nodes/<NODE_ID>/wireguard/<NAME>/enable
/nodes/<NODE_ID>/wireguard/<NAME>/dev              # default: wg-<NAME>
/nodes/<NODE_ID>/wireguard/<NAME>/private_key
/nodes/<NODE_ID>/wireguard/<NAME>/listen_port
/nodes/<NODE_ID>/wireguard/<NAME>/address          # Newline-separated

# Per-peer
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/public_key
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/allowed_ips      # Newline-separated
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/endpoint
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/persistent_keepalive
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/preshared_key

# BGP over WireGuard (optional)
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/enable
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/peer_asn
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/peer_ip
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/no_transit         # "true" | "false", default "false"
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/no_forward        # "true" | "false", default "false"
```

**BGP Control Flags:** (same as OpenVPN BGP above)
- **no_transit**: Prevent traffic from being sent through this peer. Only advertise locally-originated routes.
- **no_forward**: Completely disable transit. Stricter than `no_transit`.
- See [OpenVPN BGP Control Flags](#bgp-control-flags) above for details.

#### BGP Configuration
```python
# Node-specific
/nodes/<NODE_ID>/bgp/enable
/nodes/<NODE_ID>/bgp/asn
/nodes/<NODE_ID>/bgp/router_id

# Global BGP filters (applied to all neighbors)
/global/bgp/filter/in   = "deny 0.0.0.0/0\npermit 10.0.0.0/8 le 32"
/global/bgp/filter/out  = "permit 0.0.0.0/0 le 32"

# Format: Newline-separated rules
# - "permit <prefix> [ge N] [le N]"
# - "deny <prefix> [ge N] [le N]"
```

#### OSPF Configuration
```python
/nodes/<NODE_ID>/ospf/enable
/nodes/<NODE_ID>/ospf/router_id
```

#### Clash Configuration
```python
# Global subscriptions
/global/clash/subscriptions/<name>/url

# Per-node
/nodes/<NODE_ID>/clash/enable
/nodes/<NODE_ID>/clash/mode                      # "mixed" | "tproxy"
/nodes/<NODE_ID>/clash/active_subscription       # Selects subscription name
/nodes/<NODE_ID>/clash/refresh/enable
/nodes/<NODE_ID>/clash/refresh/interval_minutes
/nodes/<NODE_ID>/clash/exclude_tproxy_port       # Ports to exclude from TPROXY
```

#### MosDNS Configuration
```python
/nodes/<NODE_ID>/mosdns/enable
/nodes/<NODE_ID>/mosdns/refresh                  # Minutes (default: 1440)

/global/mosdns/rule_files                       # JSON object: {"path": "url"}
/global/mosdns/plugins                          # YAML list of plugins
```

**Startup Sequence**:
1. dnsmasq started on port 53 (frontend DNS, ensures DNS availability)
2. MosDNS config written to `/etc/mosdns/config.yaml`
3. Rule files downloaded (via Clash proxy if available)
4. MosDNS started on ports 1153 and 1053
5. `/etc/resolv.conf` → `127.0.0.1` (dnsmasq)

**dnsmasq Frontend DNS**:
- Listens on port 53
- Forwards to sequential servers:
  - `127.0.0.1#1153` (MosDNS primary)
  - `127.0.0.1#1053` (Clash DNS - **only when Clash is enabled**)
  - `223.5.5.5` (AliDNS)
  - `119.29.29.29` (DNSPod)
- Uses `/etc/etcd_hosts` for hosts file
- Doesn't block private network results
- Starts BEFORE MosDNS rule downloads
- Dynamically adjusts upstream servers based on Clash status
- Uses `#` syntax for non-standard DNS ports (dnsmasq standard)

**Implementation**: [watcher.py:1125-1175](watcher.py#L1125-L1175)


#### Dynamic DNS (etcd_hosts)
```python
# DNS host records (watched in real-time)
# Single IP:
/dns/hosts/gateway.internal = "10.42.1.1"

# Multiple IPs (one per line):
/dns/hosts/db.example.com = "192.168.1.100\n192.168.1.101\n192.168.1.102"
/dns/hosts/api.service = "172.16.0.10\n172.16.0.11"
```

**Behavior**:
- Watcher monitors `/dns/hosts/` prefix for changes
- Updates `/etc/etcd_hosts` file in hosts file format
- Supports multiple IPs per hostname (one IP per line in value)
- Initial empty file created on startup
- Real-time updates when keys change
- Hash-based change detection minimizes file writes

**File Format** (for multiple IPs):
```
192.168.1.100	db.example.com
192.168.1.101	db.example.com
192.168.1.102	db.example.com
172.16.0.10	api.service
172.16.0.11	api.service
10.42.1.1	gateway.internal
```

**Implementation**: [watcher.py:1382-1418](watcher.py#L1382-L1418)

**Usage**:
Mount `/etc/etcd_hosts` into applications that need custom DNS resolution:
```yaml
# docker-compose.yml
services:
  app:
    volumes:
      - /etc/etcd_hosts:/etc/hosts:ro
```

## Generator Scripts

### Location and Naming
All generators are in `generators/` directory:
- `gen_easytier.py` - EasyTier overlay config
- `gen_tinc.py` - Tinc overlay config
- `gen_openvpn.py` - OpenVPN instances
- `gen_wireguard.py` - WireGuard instances
- `gen_frr.py` - FRR routing (OSPF/BGP)
- `gen_clash.py` - Clash proxy config
- `gen_mosdns.py` - MosDNS config
- `common.py` - Shared utilities

### Generator Interface

**Input (JSON from stdin)**:
```json
{
  "node_id": "gateway1",
  "node": {"/nodes/gateway1/...": "value"},
  "global": {"/global/...": "value"},
  "all_nodes": {"/nodes/...": "values"}
}
```

**Output (JSON to stdout)**:
```json
{
  "config_text": "...",        // Main config file content
  "config_yaml": "...",        // Alternative YAML format
  "args": ["--arg1", "..."],   // Command-line arguments (if applicable)
  "instances": [...],          // For multi-instance services (OpenVPN/WireGuard)
  "files": [                   // Additional files to write
    {"path": "/path/to/file", "content": "...", "mode": 0o600}
  ],
  "tproxy_exclude": [...],     // For Clash: CIDRs to exclude from TPROXY
  "mode": "tproxy",            // For Clash
  "refresh_enable": true,      // For Clash
  "refresh_interval_minutes": 60
}
```

### Creating a New Generator

When adding support for a new tool:

1. **Create generator script** in `generators/gen_<tool>.py`
2. **Use common utilities**:
   ```python
   from common import read_input, write_output, split_ml, node_lans
   data = read_input()
   # ... process ...
   write_output({"config_text": "..."})
   ```

3. **Handle multiline values**:
   ```python
   listeners = split_ml(node.get("/nodes/<NODE_ID>/tool/listeners", ""))
   ```

4. **Add reload function** in `watcher.py`:
   ```python
   def reload_tool(node: Dict[str, str], global_cfg: Dict[str, str]) -> None:
       payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": {}}
       out = _run_generator("gen_tool", payload)
       # Write config files
       # Reload service via supervisor
   ```

5. **Add reconciliation logic** in `handle_commit()`:
   ```python
   tool_domain = {k: v for k, v in node.items() if "/tool/" in k}
   if changed("tool", tool_domain):
       reload_tool(node, global_cfg)
       did_apply = True
   ```

6. **Add supervisor config** if needed:
   - Update `supervisord.conf` with program section
   - Create run script in `scripts/run-<tool>.sh`

## Service Management Patterns

### Supervisor-Managed Services

All long-running services are managed by supervisord:

```python
# Check status
status = _supervisor_status("service-name")  # Returns "RUNNING", "STOPPED", "FATAL"

# Control service
_supervisor_start("service-name")
_supervisor_stop("service-name")
_supervisor_restart("service-name")

# For multi-instance services (OpenVPN, WireGuard)
_supervisorctl(["reread"])
_supervisorctl(["update"])
```

### Service Categories

1. **Single-instance**: EasyTier, Tinc, Clash, MosDNS, dnsmasq, FRR
2. **Multi-instance**: OpenVPN, WireGuard (dynamic instances based on etcd config)
3. **Externally managed**: FRR (via `vtysh -f` or `frr-reload.py`)

### Reload Strategies

1. **Restart process**:
   - EasyTier, Tinc, MosDNS, dnsmasq
   ```python
   _supervisor_restart("service-name")
   ```

2. **Signal-based reload**:
   - Clash (SIGHUP for config reload)
   ```python
   run(f"kill -HUP {clash_pid()}")
   ```

3. **Smooth reload**:
   - FRR (via `frr-reload.py` or fallback to `vtysh -f`)
   ```python
   reload_frr_smooth(conf_text)
   ```

4. **Supervisor reread/update**:
   - OpenVPN, WireGuard (when instances change)
   ```python
   _supervisorctl(["reread"])
   _supervisorctl(["update"])
   ```

## Background Loops

The watcher runs multiple daemon threads:

1. **`keepalive_loop()`** - Refresh online lease TTL
2. **`openvpn_status_loop()`** - Monitor OpenVPN interface status
3. **`wireguard_status_loop()`** - Monitor WireGuard interface status
4. **`monitor_children_loop()`** - Restart failed mesh services
5. **`supervisor_retry_loop()`** - Handle FATAL state services
6. **`clash_refresh_loop()`** - Refresh Clash subscription periodically
7. **`tproxy_check_loop()`** - Verify and fix TPROXY iptables rules
8. **`periodic_reconcile_loop()`** - Full reconciliation every 5 minutes
9. **`etcd_hosts_watch_loop()`** - Watch `/dns/hosts/` for DNS record changes

### Adding a New Background Loop

```python
def new_service_loop():
    while True:
        time.sleep(interval)
        try:
            # Do work
            pass
        except Exception as e:
            print(f"[service] error: {e}", flush=True)

# In main():
threading.Thread(target=new_service_loop, daemon=True).start()
```

## State Reporting

### Online Status
```python
/updated/<NODE_ID>/online  = "1"  # TTL key (disappears when offline)
/updated/<NODE_ID>/last    = "<timestamp>"  # Persistent
```

### Service Status
```python
# OpenVPN / WireGuard instances
/updated/<NODE_ID>/openvpn/<NAME>/status = "up|connecting|down <timestamp>"
/updated/<NODE_ID>/wireguard/<NAME>/status = "up|connecting|down <timestamp>"
```

### Publishing Updates
```python
publish_update("reason")  # Writes online/last keys
```

## TPROXY (Transparent Proxy) Implementation

### Clash TPROXY Mode

Clash uses iptables TPROXY to intercept traffic:

- **Chain**: `CLASH_TPROXY` in mangle table
- **Hook**: PREROUTING only (no OUTPUT - local traffic not proxied)
- **Port**: 7893 (fixed)
- **Mark**: 0x1 with routing table 100

### Essential Clash Meta Configurations

The generator automatically applies these critical settings for optimal performance:

```yaml
find-process-mode: off      # Disable process name detection (reduces overhead)
unified-delay: true         # Use unified delay for all proxy groups
geodata-loader: standard    # Use standard geodata loader (recommended)
```

These settings are **always enforced** regardless of subscription configuration to ensure:

1. **Performance**: `find-process-mode: off` eliminates unnecessary process lookups
2. **Reliability**: `unified-delay: true` provides consistent delay measurements
3. **Compatibility**: `geodata-loader: standard` ensures maximum compatibility with GeoIP/GeoSite databases

**Location**: [generators/gen_clash.py:64-66](generators/gen_clash.py#L64-L66)

### TPROXY Mode (Include-Based Proxying)

**IMPORTANT**: Clash TPROXY uses **include mode based on source IP** - only traffic **FROM** specified LANs is proxied. All other traffic bypasses the proxy and connects directly.

**Proxy Source Networks** (from `/nodes/<NODE_ID>/lan` and `/nodes/<NODE_ID>/private_lan`):
- Traffic **originating from** CIDRs in `/nodes/<NODE_ID>/lan` is proxied
- Traffic **originating from** CIDRs in `/nodes/<NODE_ID>/private_lan` is also proxied
- All other source networks bypass the proxy automatically
- Empty LAN list = no traffic is proxied

**Exclusions** (applied to all traffic):

1. **Source CIDRs** (`exclude_src`):
   - Default gateway (from `DEFAULT_GW` env var)

2. **Interfaces** (`exclude_ifaces`):
   - EasyTier/Tinc devices
   - OpenVPN/WireGuard interfaces

3. **Ports** (`exclude_ports`):
   - Configured via `/nodes/<NODE_ID>/clash/exclude_tproxy_port`
   - Auto-detected mesh listener ports
   - OpenVPN/WireGuard ports

### Proxy Provider Considerations

**Note**: This project uses `proxy-provider` for dynamic proxy lists, not static `proxies` in the configuration. Proxy servers are loaded externally.

Since TPROXY uses **include mode** (only proxying traffic from specified LANs), proxy server connections automatically bypass the proxy by default (unless the proxy server's source IP is within a configured LAN CIDR).

### Applying TPROXY Rules

```python
tproxy_apply(
    proxy_dst,      # List of source CIDRs to proxy (from /lan and /private_lan)
    exclude_src,    # List of source CIDRs to bypass
    exclude_ifaces, # List of interface names to bypass
    exclude_ports,  # List of port numbers to bypass
)
```

The script `/usr/local/bin/tproxy.sh` handles iptables and ip rule setup using `PROXY_CIDRS` to specify which source addresses to proxy.

### TPROXY Monitoring

The `tproxy_check_loop()` periodically verifies iptables rules and re-applies them if missing (after FRR restart, etc.).

## FRR Routing Configuration

### Internal Routing Selection

Controlled by `/global/internal_routing_system`:
- `ospf`: Default, uses OSPF to distribute internal routes
- `bgp`: Uses iBGP between mesh nodes

### BGP Configuration

1. **Node setup**:
   - `/nodes/<NODE_ID>/bgp/asn` - AS number
   - `/nodes/<NODE_ID>/bgp/router_id` - Router ID (e.g., 1.1.1.1)

2. **Neighbors** (auto-configured):
   - iBGP: Other nodes with `/nodes/*/router_id` set (when using BGP internally)
   - eBGP: OpenVPN/WireGuard instances with `bgp/enable = "true"`

3. **Route filtering**:
   - Global filters apply to ALL neighbors
   - `/global/bgp/filter/in` - Inbound routes
   - `/global/bgp/filter/out` - Outbound routes

### OSPF Configuration

- Redistributes connected routes (LANs, overlay interfaces)
- Auto-discovers neighbors on broadcast networks
- Router ID from `/nodes/<NODE_ID>/ospf/router_id`

### Route Advertisement

**Advertised prefixes**:
- Node LANs from `/nodes/<NODE_ID>/lan/*` (via `redistribute connected`)
- Private LANs from `/nodes/<NODE_ID>/private_lan/*` (internal only)

**Not advertised**:
- Default route (denied by inbound BGP filter)

## Security Considerations

### Secrets Management

- **Inline content only**: Secrets stored directly in etcd keys
- **No file paths**: `/secret`, `/ca`, `/cert`, `/key`, `/private_key` contain actual values
- **File permissions**: Sensitive files written with mode 0o600

### Network Security

1. **Overlay isolation**:
   - EasyTier: Uses `network_secret` for authentication
   - Tinc: Uses public/private key pairs

2. **VPN security**:
   - OpenVPN: TLS with certificate verification
   - WireGuard: Cryptographic key exchange

3. **Routing isolation**:
   - Private LANs not exported to external BGP
   - BGP filters control route propagation

### iptables Safety

- TPROXY only hooks PREROUTING (not OUTPUT)
- Exclusions prevent proxying of:
  - Local mesh traffic
  - Local management access
  - VPN connections

## Testing and Debugging

### Local Development

1. **Build container**:
   ```bash
   docker compose build
   ```

2. **Run with local etcd**:
   Set environment variables:
   - `NODE_ID`
   - `ETCD_ENDPOINTS`
   - `ETCD_CA`, `ETCD_CERT`, `ETCD_KEY`
   - `ETCD_USER`, `ETCD_PASS`

3. **Trigger configuration update**:
   ```bash
   etcdctl put /commit "<timestamp>"
   ```

4. **Check status**:
   ```bash
   supervisorctl status
   etcdctl get /updated/<NODE_ID>/online --prefix
   ```

### Debugging Generator Scripts

```bash
# Test generator manually
echo '{"node_id":"test","node":{},"global":{},"all_nodes":{}}' | \
  python3 generators/gen_clash.py | jq
```

### Common Issues

1. **Generator fails**:
   - Check `watcher.py` logs for `RuntimeError: generator <name> failed`
   - Validate JSON structure with `jq`

2. **Service won't start**:
   - Check supervisor status: `supervisorctl status`
   - View logs: `/var/log/<service>.*.log`

3. **TPROXY not working**:
   - Verify iptables rules: `iptables -t mangle -L CLASH_TPROXY`
   - Check ip rules: `ip rule list`
   - Review `tproxy_check_loop` logs

4. **BGP sessions not establishing**:
   - Verify router IDs are unique
   - Check `vtysh -c "show bgp summary"`
   - Review filter configuration in `/global/bgp/filter/*`

## Code Style and Conventions

### Python Code

1. **Type hints**: Used but not strictly enforced
2. **Error handling**:
   - Use specific exceptions
   - Log errors with context
3. **Threading**:
   - Use locks for shared state
   - Keep critical sections short
4. **Naming**:
   - Private functions: `_function_name()`
   - Global state: `_variable_name`
   - Locks: `_component_lock`

### Configuration Generation

1. **Idempotency**: Generators should produce same output for same input
2. **Validation**: Validate etcd keys before using (provide defaults)
3. **Documentation**: Add comments for complex logic
4. **Separation**: Keep business logic in generators, orchestration in watcher

## When Making Changes

### Adding a New etcd Key

1. **Update schema**: Document in `docs/etcd-schema.md`
2. **Update generator**: Read key in relevant generator script
3. **Update watcher**: Add reconciliation logic in `handle_commit()`
4. **Update documentation**: Add explanation in `CLAUDE.md`

### Modifying Service Behavior

1. **Identify impact**: Does this affect other services?
2. **Update generator**: Modify config generation
3. **Update reload logic**: Ensure service picks up changes
4. **Test reconciliation**: Verify `/commit` trigger works

### Adding Background Monitoring

1. **Create loop function**: Pattern after existing loops
2. **Add thread start**: In `main()` function
3. **Handle errors**: Use try/except with logging
4. **Consider backoff**: Don't retry too aggressively

## Important Gotchas

1. **Only watch `/commit`**: Never watch individual keys
2. **Global mesh type**: EasyTier and Tinc are mutually exclusive
3. **BGP filter defaults**: Missing keys use safe defaults (deny default route)
4. **WireGuard routes**: Handled by FRR, not WireGuard itself
5. **Clash TPROXY**: Only PREROUTING, not OUTPUT (local traffic not proxied)
6. **Private LANs**: Not advertised to external BGP neighbors
7. **Status reporting**: Must include timestamp for proper state tracking
8. **Generator errors**: Cause reconciliation to fail entirely

## Further Reading

- [README.md](README.md) - Quick start guide
- [docs/architecture.md](docs/architecture.md) - Detailed architecture
- [docs/etcd-schema.md](docs/etcd-schema.md) - Complete etcd schema
- [docs/mosdns.md](docs/mosdns.md) - MosDNS configuration details
- [examples/etcd-example.sh](examples/etcd-example.sh) - Sample etcd configuration

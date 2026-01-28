# etcd schema (nodes + sites merged 1:1)

Top level:

- /global/...
- /nodes/<NODE_ID>/...
- /commit  (single trigger)

`sites` is removed. LANs are now owned per-node (single key, newline-separated):

```
/nodes/<NODE_ID>/lan = "10.42.10.0/24\n10.42.11.0/24\n..."
```

Private LANs (internal-only, newline-separated):

```
/nodes/<NODE_ID>/private_lan = "10.99.10.0/24\n10.99.11.0/24\n..."
```

- `private_lan` is distributed only via OSPF or iBGP (not exported to external BGP neighbors).

These prefixes are treated as **Local segments**:
- advertised by routing (OSPF via redistribute connected; BGP via network statements)
- excluded from Clash TPROXY interception

All other keys remain under `/nodes/<NODE_ID>/...`.


## Online status & last update

```
/updated/<NODE_ID>/last    = "<YYYY-MM-DDTHH:mm:ss+0000>"   # persistent
/updated/<NODE_ID>/online  = "1"                           # TTL/lease based
```

- `online` disappears automatically when the node is offline
- `last` always preserves the last successful update time

ENV:
- `UPDATE_TTL_SECONDS` (optional, default `60`)


## OpenVPN

Schema (per instance):

```
/nodes/<NODE_ID>/openvpn/<NAME>/enable                # "true" | "false"
/nodes/<NODE_ID>/openvpn/<NAME>/dev                   # optional, default: tun-<NAME> or tun<digit>
/nodes/<NODE_ID>/openvpn/<NAME>/dev_type              # e.g. tun
/nodes/<NODE_ID>/openvpn/<NAME>/proto                 # tcp-server | tcp-client | udp | ...
/nodes/<NODE_ID>/openvpn/<NAME>/port
/nodes/<NODE_ID>/openvpn/<NAME>/remote                # newline-separated list
/nodes/<NODE_ID>/openvpn/<NAME>/ifconfig              # "local remote"
/nodes/<NODE_ID>/openvpn/<NAME>/keepalive             # "10 60"
/nodes/<NODE_ID>/openvpn/<NAME>/verb
/nodes/<NODE_ID>/openvpn/<NAME>/auth
/nodes/<NODE_ID>/openvpn/<NAME>/cipher
/nodes/<NODE_ID>/openvpn/<NAME>/comp_lzo              # yes | no | adaptive
/nodes/<NODE_ID>/openvpn/<NAME>/allow_compression     # asym | yes | no
/nodes/<NODE_ID>/openvpn/<NAME>/persist_tun           # "1" to enable
/nodes/<NODE_ID>/openvpn/<NAME>/tls_client            # "1" to enable
/nodes/<NODE_ID>/openvpn/<NAME>/remote_cert_tls       # e.g. server
/nodes/<NODE_ID>/openvpn/<NAME>/key_direction
/nodes/<NODE_ID>/openvpn/<NAME>/client                # "1" to enable

# Inline-only secrets (stored directly in etcd, not file paths):
/nodes/<NODE_ID>/openvpn/<NAME>/secret
/nodes/<NODE_ID>/openvpn/<NAME>/ca
/nodes/<NODE_ID>/openvpn/<NAME>/cert
/nodes/<NODE_ID>/openvpn/<NAME>/key
/nodes/<NODE_ID>/openvpn/<NAME>/tls_auth
/nodes/<NODE_ID>/openvpn/<NAME>/tls_crypt

# BGP transport over OpenVPN:
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/peer_asn
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/peer_ip
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/update_source     # e.g. tun0 / tun1 ...
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/enable            # "true" | "false"
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/weight
```

Status reporting:

```
/updated/<NODE_ID>/openvpn/<NAME>/status = "<state> <YYYY-MM-DDTHH:mm:ss+0000>"
```

States: `up` | `connecting` | `down`

ENV:
- `OPENVPN_STATUS_INTERVAL` (seconds, default `10`)

Notes:
- 不再支持直接下发 `config`，必须由上述结构化键生成。
- `secret/ca/cert/key/tls_auth/tls_crypt` 只能使用 inline 内容。


## WireGuard

Schema (per instance):

```
/nodes/<NODE_ID>/wireguard/<NAME>/enable                # "true" | "false"
/nodes/<NODE_ID>/wireguard/<NAME>/dev                   # optional, default: wg-<NAME> or wg<digit>
/nodes/<NODE_ID>/wireguard/<NAME>/private_key
/nodes/<NODE_ID>/wireguard/<NAME>/listen_port

# Optional interface settings:
/nodes/<NODE_ID>/wireguard/<NAME>/address               # newline-separated
/nodes/<NODE_ID>/wireguard/<NAME>/mtu
/nodes/<NODE_ID>/wireguard/<NAME>/dns                   # newline-separated

/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/public_key
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/allowed_ips      # newline-separated
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/endpoint
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/persistent_keepalive
/nodes/<NODE_ID>/wireguard/<NAME>/peer/<PEER>/preshared_key

# BGP transport over WireGuard:
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/peer_asn
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/peer_ip
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/update_source     # ignored (auto from dev)
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/enable            # "true" | "false"
/nodes/<NODE_ID>/wireguard/<NAME>/bgp/weight
```

Status reporting:

```
/updated/<NODE_ID>/wireguard/<NAME>/status = "<state> <YYYY-MM-DDTHH:mm:ss+0000>"
```

States: `up` | `connecting` | `down`

ENV:
- `WIREGUARD_STATUS_INTERVAL` (seconds, default `10`)

Notes:
- `private_key/preshared_key` 只能使用 inline 内容。
- WireGuard `Table` is forced to `off`, and `PreUp/PostUp/PreDown/PostDown` are auto-generated to no-op.
- WireGuard does not manage routes; routing is handled by FRR.
- If `allowed_ips` is empty, it defaults to `0.0.0.0/0`.
- `bgp/enable` defaults to `true` when omitted.


## Global BGP filter (shared for all neighbors)

FRR requires per-neighbor policy to establish sessions. This project applies the **same** inbound/outbound policy
to **every** BGP neighbor.

Configure rules under:

```
/global/bgp/filter/in
/global/bgp/filter/out
```

Value format: **newline-separated** rules (supports `\n`, `\r`, `\r\n`), each line:

- `permit <prefix> [ge N] [le N]`
- `deny   <prefix> [ge N] [le N]`

Lines starting with `#` are ignored.

Defaults (when keys are missing):

- IN: deny default route, permit everything else
  - `deny 0.0.0.0/0`
  - `permit 0.0.0.0/0 le 32`
- OUT: permit everything
  - `permit 0.0.0.0/0 le 32`

Example:

```
/global/bgp/filter/in  = "deny 0.0.0.0/0\npermit 10.0.0.0/8 le 32\npermit 192.168.0.0/16 le 32"
 /global/bgp/filter/out = "permit 0.0.0.0/0 le 32"
```

The generator will build:

- `ip prefix-list PL-BGP-IN ...`
- `ip prefix-list PL-BGP-OUT ...`
- and apply `RM-BGP-IN`/`RM-BGP-OUT` to every neighbor.


## EasyTier

Global network identity (shared by all nodes):

```
/global/easytier/network_name
/global/easytier/network_secret
/global/easytier/private_mode   # "true" | "false"
/global/easytier/dhcp           # "true" | "false"
```

Node-specific EasyTier settings:

```
/nodes/<NODE_ID>/easytier/enable
/nodes/<NODE_ID>/easytier/dev_name
/nodes/<NODE_ID>/easytier/ipv4
/nodes/<NODE_ID>/easytier/listeners          # newline-separated
/nodes/<NODE_ID>/easytier/mapped_listeners   # newline-separated public addresses
/nodes/<NODE_ID>/easytier/peers        # newline-separated
```

Notes:
- `network_name` and `network_secret` are required globally.
- `listeners` and `peers` are strict single-key multiline (no legacy `*/0` keys).

## Mesh type

Select overlay implementation:

```
/global/mesh_type = "easytier" | "tinc"
```

## Internal routing system

Select routing exchange protocol across the mesh:

```
/global/internal_routing_system = "ospf" | "bgp"
```

- `ospf`: current default, OSPF distributes internal routes.
- `bgp`: iBGP between nodes, neighbors are built from `/nodes/*/router_id`.

## Tinc (when /global/mesh_type = "tinc")

Global:

```
/global/tinc/netname    # default: "mesh"
/global/tinc/cipher     # optional, writes Cipher= in tinc.conf
/global/tinc/digest     # optional, writes Digest= in tinc.conf
```

Node-specific Tinc settings:

```
/nodes/<NODE_ID>/tinc/enable
/nodes/<NODE_ID>/tinc/name         # default: NODE_ID
/nodes/<NODE_ID>/tinc/dev_name     # default: tnc0
/nodes/<NODE_ID>/tinc/port         # default: 655
/nodes/<NODE_ID>/tinc/address      # public address for peers to connect
/nodes/<NODE_ID>/tinc/address_family  # default: ipv4
/nodes/<NODE_ID>/tinc/ipv4         # optional, CIDR assigned to the tinc interface
/nodes/<NODE_ID>/tinc/subnet       # optional, newline-separated Subnet entries for host file
/nodes/<NODE_ID>/tinc/host_mode    # optional, writes Mode= in hosts file
/nodes/<NODE_ID>/tinc/host_cipher  # optional, writes Cipher= in hosts file
/nodes/<NODE_ID>/tinc/host_digest  # optional, writes Digest= in hosts file
/nodes/<NODE_ID>/tinc/mode         # default: Switch
/nodes/<NODE_ID>/tinc/public_key
/nodes/<NODE_ID>/tinc/ed25519_public_key
/nodes/<NODE_ID>/tinc/private_key
/nodes/<NODE_ID>/tinc/ed25519_private_key
```

Behavior:
- Uses `Mode = switch` (tinc 1.1).
- Automatically pulls all enabled nodes' public keys under `/nodes/*/tinc/public_key`.
- Public key can be `Ed25519PublicKey` or RSA (multi-line); value is stored verbatim in hosts file.
- Connects to peers with `address` set.

## Clash

Global subscriptions (shared, so updates are consistent):

```
/global/clash/subscriptions/<name>/url
```

Node-level behavior:

```
/nodes/<NODE_ID>/clash/enable
/nodes/<NODE_ID>/clash/mode                 # mixed | tproxy
/nodes/<NODE_ID>/clash/active_subscription  # selects a name under /global/clash/subscriptions/
/nodes/<NODE_ID>/clash/refresh/enable
/nodes/<NODE_ID>/clash/refresh/interval_minutes
```


Mapped listeners usage:

- Local listener (example): `tcp://0.0.0.0:11010`
- Public mapped listener:   `tcp://203.0.113.10:443`

When set, the gateway starts EasyTier with one or more `--mapped-listeners <addr>` arguments.


## Network Mapping (NAT 1:1 Bidirectional)

Network mapping provides 1:1 bidirectional address translation between two network segments of equal size.

Schema (per node):

```
/nodes/<NODE_ID>/network_mapping/<NETWORK_A_CIDR> = <NETWORK_B_CIDR>
```

- **Key**: `<NETWORK_A_CIDR>` - Local network segment (e.g., `192.168.1.0/24`)
- **Value**: `<NETWORK_B_CIDR>` - Target network segment (e.g., `10.100.1.0/24`)
- **Requirement**: Both networks MUST have the same prefix length (same size)
- **Multiple mappings**: Can specify multiple network pairs by using different keys

Example:

```
/nodes/gateway1/network_mapping/192.168.1.0/24 = 10.100.1.0/24
/nodes/gateway1/network_mapping/192.168.2.0/24 = 10.100.2.0/24
```

**Behavior**:

1. **Bidirectional 1:1 NAT**:
   - **Outbound (A→B)**: Traffic from network A is translated to network B
     - Source: 192.168.1.100 → 10.100.1.100
   - **Inbound (B→A)**: Traffic destined for network B is translated to network A
     - Destination: 10.100.1.100 → 192.168.1.100

2. **BGP Advertisement**:
   - Network A (local) is advertised via BGP, NOT network B
   - External networks learn routes to network A
   - Traffic to network A arrives and is translated to network B

3. **Implementation**:
   - Uses iptables `NETMAP` target for efficient 1:1 prefix translation
   - Applied in both PREROUTING (DNAT) and POSTROUTING (SNAT) chains
   - Automatically excludes traffic within the same network segment

**Use Cases**:
- Migrate from one IP range to another without reconfiguring clients
- Connect overlapping network segments
- Provide external-facing IP ranges that differ from internal ranges
- Enable gradual network migration with minimal disruption

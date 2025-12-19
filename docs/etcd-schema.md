# etcd schema (nodes + sites merged 1:1)

Top level:

- /global/...
- /nodes/<NODE_ID>/...
- /commit  (single trigger)

`sites` is removed. LANs are now owned per-node:

```
/nodes/<NODE_ID>/lan/10.42.10.0_24 = true
/nodes/<NODE_ID>/lan/10.42.11.0_24 = true
...
```

These prefixes are treated as **Local segments**:
- advertised by routing (OSPF via redistribute connected; BGP via network statements)
- excluded from Clash TPROXY interception

All other keys remain under `/nodes/<NODE_ID>/...`.


## Online monitoring key (/update/<NODE_ID>)

The gateway can write a heartbeat/update marker:

- Key: `/update/<NODE_ID>`
- Value: UTC epoch timestamp (seconds)
- Uses an etcd **lease/TTL** (default 60s)

Behavior:
- Write once at startup
- Write again whenever any module applies changes (config-applied)
- Lease is refreshed periodically so the key disappears automatically if the container stops

ENV:
- `UPDATE_TTL_SECONDS` (optional, default `60`)

## Online status & last update

```
/updated/<NODE_ID>/last    = "<UTC epoch seconds>"   # persistent
/updated/<NODE_ID>/online  = "1"                     # TTL/lease based
```

- `online` disappears automatically when the node is offline
- `last` always preserves the last successful update time


## OpenVPN

Schema (per instance):

```
/nodes/<NODE_ID>/openvpn/<NAME>/enable                # "true" | "false"
/nodes/<NODE_ID>/openvpn/<NAME>/config                # inline .ovpn config text
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/peer_asn
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/peer_ip
/nodes/<NODE_ID>/openvpn/<NAME>/bgp/update_source     # e.g. tun0 / tun1 ...
```

Status reporting:

```
/updated/<NODE_ID>/openvpn/<NAME>/status = "<state> <utc_epoch>"
```

States: `up` | `connecting` | `down`

ENV:
- `OPENVPN_STATUS_INTERVAL` (seconds, default `10`)


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

When set, the gateway starts EasyTier with one or more `--mapped-listeners <addr>` arguments. citeturn0search0turn0search3

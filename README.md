# Meduza Gateway Planner

Single-container **edge gateway** controlled by **etcd** (single source of truth), coordinating:

- EasyTier (overlay only)
- FRR (OSPF + BGP)
- OpenVPN (BGP transport only)
- Clash Meta (mihomo) (mixed / tproxy fallback)

## Quick start

```bash
docker compose build
docker compose up -d
```

Provide required envs:

- NODE_ID
- ETCD_ENDPOINTS
- ETCD_CA / ETCD_CERT / ETCD_KEY
- ETCD_USER / ETCD_PASS

Then update etcd keys and bump `/commit`.

## Schema change (sites merged into nodes)

- `sites` is removed; LANs are now 1:1 with nodes under `/nodes/<NODE_ID>/lan/*`.
- Clash TPROXY exclusion uses **all Local segments** dynamically:
  - RFC1918/reserved + overlay (10.42.1.0/24) + node LANs.

## Networking ENV

- `DEFAULT_GW` (optional): override the container's default gateway (upstream).
  - Use-case: LAN clients use container IP as their DHCP gateway, but the container egress should go to an upstream GW.
- `DEFAULT_GW_DEV` (optional): device name for the default route (e.g. `eth0`).

## Clash TPROXY behavior

- In `tproxy` mode, iptables TPROXY rules hook **PREROUTING only**.
- Local-originated traffic (**OUTPUT**) is **not** proxied; only forwarded/inbound traffic is intercepted.


## Online monitoring (/updated/<NODE_ID>)

If etcd ACL grants write permission to `/updated/<NODE_ID>`, the watcher will:

- `/updated/<NODE_ID>/online` (TTL): presence = online
- `/updated/<NODE_ID>/last` (persistent): last successful apply time
- attach a lease with TTL (default 60s) and refresh it periodically

This enables simple online monitoring: if the TTL key disappears, the node is offline (or cannot reach etcd).

ENV:
- `UPDATE_TTL_SECONDS` (optional, default `60`)

## OpenVPN status reporting

When OpenVPN is enabled, the gateway writes:

- `/updated/<NODE_ID>/openvpn/<name>/status` = `"<state> <utc_epoch>"`

ENV:
- `OPENVPN_STATUS_INTERVAL` (default `10`)

## Build notes (EasyTier / mihomo assets)

This repo uses a `Dockerfile` (not Containerfile).

Build args:
- `EASYTIER_VERSION` (default `2.4.5`) downloads `easytier-linux-x86_64-v<VER>.zip` and installs `easytier-core` as `/usr/local/bin/easytier-core` (daemon) and `easytier-cli` as `/usr/local/bin/easytier-cli` (optional).
- `MIHOMO_VERSION` (default `1.19.17`) downloads `mihomo-linux-amd64-v2-v<VER>.gz` and installs it as `/usr/local/bin/mihomo`.

Example:
```bash
docker build --build-arg EASYTIER_VERSION=2.4.5 --build-arg MIHOMO_VERSION=1.19.17 -t gateway .
```

## EasyTier listeners/peers format (strict)

This deployment uses **only** single keys with newline-separated values:

- `/nodes/<NODE_ID>/easytier/listeners`
- `/nodes/<NODE_ID>/easytier/peers`

(No legacy `/listeners/*` or `/peers/*` support.)

## OpenVPN status reporting

- `/updated/<NODE_ID>/openvpn/<NAME>/status` = `"<state> <utc_epoch>"`
- Node config prefix: `/nodes/<NODE_ID>/openvpn/<NAME>/...`


## Global BGP filter policy

To satisfy FRR's policy requirement, the gateway applies shared route-maps to every BGP neighbor:

- `RM-BGP-IN`  (inbound)
- `RM-BGP-OUT` (outbound)

Configure them once in etcd:

- `/global/bgp/filter/in`
- `/global/bgp/filter/out`

Each value is newline-separated rules like:

- `deny 0.0.0.0/0`
- `permit 0.0.0.0/0 le 32`

If not set, defaults to **deny default route inbound** and **permit all outbound**.

## Globalized config

To keep consistency across the fleet:

- EasyTier network identity is global:
  - `/global/easytier/network_name`
  - `/global/easytier/network_secret`
  - `/global/easytier/private_mode`
  - `/global/easytier/dhcp`

- Clash subscription URLs are global:
  - `/global/clash/subscriptions/<name>/url`

Each node selects subscription by:
- `/nodes/<NODE_ID>/clash/active_subscription`
and keeps its own `mode` / `refresh` settings under node keys.

## EasyTier mapped listeners

If a node is behind NAT but has a known public IP:port mapping, configure:

- `/nodes/<NODE_ID>/easytier/mapped_listeners` (newline-separated), e.g.
  - `tcp://203.0.113.10:443`

This is passed to easytier-core as --mapped-listeners ... and can be repeated.

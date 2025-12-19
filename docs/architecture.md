# Architecture

- etcd is the single source of truth.
- **Only** `/commit` is watched.
- On each `/commit` change, the node pulls:
  - `/nodes/<NODE_ID>/...`
  - (optional) `/global/...`

## Sites + nodes merged 1:1

- `sites` is removed.
- Local LAN prefixes live under `/nodes/<NODE_ID>/lan/*`.

## Reload semantics

- EasyTier: restart process
- OpenVPN: start/stop per instance
- FRR: generate config and apply via `vtysh -f`
- Clash: pull subscription, write config, `SIGHUP`
  - When mode is `tproxy`, iptables/policy-routing are applied **after FRR is ready**.
  - Clash TPROXY exclusion uses **all Local segments**:
    - RFC1918/reserved blocks
    - EasyTier overlay (10.42.1.0/24)
    - node LANs (`/nodes/<NODE_ID>/lan/*`)

- TPROXY iptables hooks PREROUTING only (no OUTPUT), so local traffic is not proxied.

- EasyTier runs `easytier-core` as the dataplane daemon; `easytier-cli` is optional for inspection.

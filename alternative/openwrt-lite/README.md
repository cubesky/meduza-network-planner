# Meduza OpenWrt Lite

`meduza-openwrt-lite` is a small OpenWrt-native implementation of Meduza's
etcd generator/reporter contract. It supports the existing `/global`,
`/nodes/<NODE_ID>` and `/updated/<NODE_ID>` schema for:

- tinc mesh configuration;
- FRR OSPF/BGP configuration, including BGP peers transported by OpenVPN or
  WireGuard;
- multiple OpenVPN instances;
- multiple WireGuard interfaces;
- online, service and tunnel status reporting.

It deliberately uses OpenWrt's own packages and init system. There is no
Python, supervisor, Docker, EasyTier, Clash, MosDNS, access VPN, NAT mapping or
port-forward implementation in this lite variant.

This package only supports the **routed side-gateway** deployment mode. The
OpenWrt device keeps its existing LAN/WAN role and clients use it only when an
upstream router or the clients themselves explicitly route selected prefixes
to it. The package enables IPv4 forwarding, but does not modify:

- WAN interfaces or the system default gateway;
- DHCP or DNS;
- firewall zones and forwarding rules;
- NAT/masquerading;
- policy routing for clients.

## Build and install

Copy this directory to `package/meduza-openwrt-lite` in an OpenWrt source tree:

```sh
make menuconfig                    # Network -> meduza-openwrt-lite
make package/meduza-openwrt-lite/compile V=s
opkg install meduza-openwrt-lite_*.ipk
```

The `build-openwrt-lite` GitHub Actions workflow builds the package with the
official OpenWrt SDK and uploads the package, repository indexes, checksums and
build log as an artifact. Pushes and pull requests affecting this directory use
OpenWrt 25.12.2 x86/64. A manual run accepts `openwrt_release`, `target` and
`subtarget` inputs for other devices. Both `.apk` and legacy `.ipk` outputs are
collected.

The workflow verifies the SDK against the target directory's official
`sha256sums` file before extraction.

The package directly depends on `tinc`, `frr`, `frr-vtysh`,
`openvpn-openssl`, `wireguard-tools`, `kmod-wireguard`, `curl`, `jq` and the
small `pgrep` utility used by the reporter.

## Configure

```sh
uci set meduza.main.enable='1'
uci set meduza.main.NODE_ID='router-01'
uci set meduza.main.ETCD_ENDPOINTS='https://etcd.example.net:2379'
uci set meduza.main.ETCD_CA='/etc/meduza/pki/ca.crt'
uci set meduza.main.ETCD_CERT='/etc/meduza/pki/client.crt'
uci set meduza.main.ETCD_KEY='/etc/meduza/pki/client.key'
uci set meduza.main.ETCD_USER='meduza'
uci set meduza.main.ETCD_PASS='change-me'
uci set meduza.main.VPN_FIREWALL_ZONE='lan'
uci commit meduza
/etc/init.d/meduza enable
/etc/init.d/meduza restart
```

The same settings are available at **LuCI -> Services -> Meduza**. The firewall
zone selector is populated from existing OpenWrt zones. An empty
`VPN_FIREWALL_ZONE` leaves Meduza VPN interfaces outside every firewall zone.
The package does not create or alter the selected zone's policies.

Firewall integration is backend-neutral. Meduza only updates the selected
zone's `network` membership through UCI and calls OpenWrt's standard firewall
init script. OpenWrt therefore selects fw3/iptables or fw4/nftables itself.
Meduza does not invoke `iptables`, `nft`, `fw3` or `fw4` directly. Zone
input/output/forward and masquerading policies remain entirely under the
administrator's existing firewall configuration.

Leave `ETCD_CA`, `ETCD_CERT` and `ETCD_KEY` empty when mutual TLS is not used.
`ETCD_ENDPOINTS` accepts a comma-separated value for compatibility with the
main project; the lite package uses the first endpoint. The generator polls `/commit`; it does
not watch individual configuration keys. A changed commit triggers a complete,
idempotent reconciliation.

For a one-shot diagnostic reconciliation run:

```sh
/usr/libexec/meduza/meduza-generator
logread -e meduza
```

Generated files are under `/etc/tinc`, `/etc/openvpn`, `/etc/frr` and
`/etc/meduza/wireguard`. WireGuard is applied with `wg setconf` and `ip`, so it
does not create routes; FRR remains responsible for routing, matching the main
project's behavior.


For every enabled VPN instance, reconciliation also maintains an unmanaged
OpenWrt network interface:

- OpenVPN instance `office` becomes `ovpn_office`, bound to its configured
  tunnel device;
- WireGuard instance `office` becomes `wg_office`, bound to its WireGuard
  device. UCI identifiers use `_` because OpenWrt forbids `-` in section names;
  the underlying Linux VPN device keeps the exact name configured in etcd.

All such interfaces are automatically moved to `VPN_FIREWALL_ZONE`. Stale
Meduza interfaces and their old zone memberships are removed when instances
are disabled or renamed. Interfaces not marked as Meduza-managed are untouched.
## Compatibility and status semantics

The implementation consumes the same tinc, OpenVPN, WireGuard, OSPF, BGP,
LAN/private-LAN, mesh type and internal-routing keys documented in the root
`docs/etcd-schema.md`. Inline private keys and certificates are written mode
`0600`.

The reporter writes:

```text
/updated/<NODE_ID>/online
/updated/<NODE_ID>/last
/updated/<NODE_ID>/openvpn/<NAME>/status
/updated/<NODE_ID>/wireguard/<NAME>/status
/updated/<NODE_ID>/tinc/default/status
/updated/<NODE_ID>/frr/default/status
```

`online` uses an etcd lease. OpenVPN is `up` when its process and interface are
up. WireGuard is `up` after a handshake in the last 180 seconds, `connecting`
when the interface exists without a recent handshake, and `down` when absent.

## Operational notes

- The etcd endpoint must expose the v3 JSON gRPC gateway (`/v3/...`).
- Instance and interface names are restricted to letters, digits, `_` and `-`.
- Configuration is written atomically before native services are restarted.
- Give the etcd account read access to `/commit`, `/global/`, `/nodes/` and
  write access to `/updated/<NODE_ID>/`.

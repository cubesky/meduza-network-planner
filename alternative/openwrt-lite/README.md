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

It deliberately uses OpenWrt's own packages and init system. A Python 3 agent
handles etcd connectivity, while native shell helpers apply OpenWrt state.
There is no supervisor, Docker, EasyTier, Clash, MosDNS, access VPN, NAT mapping or
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
apk add --allow-untrusted meduza-openwrt-lite-*.apk
```

The `build-openwrt-lite` GitHub Actions workflow builds the package with the
official OpenWrt SDK and uploads the package, repository indexes, checksums and
build log as an artifact. The build matrix produces OpenWrt 24.10 `.ipk` and
OpenWrt 25.12 `.apk` packages for ARM64 and x86-64. A manual run accepts
`ipk_release` and `apk_release` inputs. The Python `etcd3`, `protobuf`, `six`,
`typing-extensions` and native `grpcio` runtime are bundled into each package,
so installation never runs pip and does not depend on unavailable OpenWrt feed
packages. ARM64 and x86-64 use grpcio's official musllinux wheels. Before
packaging, native wheel libraries are normalized from the Python wheel musl SONAME
(`libc.musl-<arch>.so.1`) to OpenWrt's standard `libc.so`; this lets OpenWrt's
dependency scanner and runtime linker resolve the normal libc package.

After both formats succeed, non-PR runs create or update a GitHub Release whose
name is `<UTC YYYYMMDD>-<7-character commit hash>`. The release contains both
packages, `SHA256SUMS` and release metadata. Pull requests build both formats

Published package files use the same date/hash identity and expose their CPU
family, for example
`meduza-openwrt-lite-<YYYYMMDD>-<short-hash>-arm64.ipk` and
`meduza-openwrt-lite-<YYYYMMDD>-<short-hash>-arm64.apk` or
`meduza-openwrt-lite-<YYYYMMDD>-<short-hash>-x86-64.apk`.
The display filename uses `arm64`, while package metadata follows the package
manager ABI: OpenWrt 24.10 IPK uses `aarch64_generic`, OpenWrt 25.12 APK uses
`aarch64`, and x86-64 uses `x86_64` in both formats.
but never publish a release.

The workflow verifies the SDK against the target directory's official
`sha256sums` file before extraction. It rejects any grpcio-containing IPK/APK
that is accidentally marked architecture-independent.

The package directly depends on `tinc`, `frr` (which provides `vtysh`),
`openvpn-openssl`, `wireguard-tools`, `python3`, `jq` and the small `pgrep`
utility used by the Python agent.

`kmod-wireguard` is intentionally not a package dependency. OpenWrt kernel
modules are tied to the exact firmware kernel ABI, so a package built by a
generic SDK cannot safely require a kernel module from another target or
firmware build. The firmware must provide WireGuard kernel support itself (or
install its matching `kmod-wireguard` from the firmware vendor's feed). Meduza
detects missing support when it creates the WireGuard link.

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
main project. The Python agent automatically fails over between all configured
endpoints, refreshes expired authentication tokens, and retries failures with
bounded exponential backoff. It polls `/commit` rather than watching individual
configuration keys. A changed commit triggers a complete, idempotent reconciliation.

For a one-shot diagnostic reconciliation run:

```sh
/usr/bin/python3 /usr/libexec/meduza/meduza-agent.py
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

- The agent connects directly to etcd's native v3 gRPC service; the HTTP/JSON
  gateway does not need to be enabled.
- Instance and interface names are restricted to letters, digits, `_` and `-`.
- Configuration is written atomically before native services are restarted.
- Give the etcd account read access to `/commit`, `/global/`, `/nodes/` and
  write access to `/updated/<NODE_ID>/`.

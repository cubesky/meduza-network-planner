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
- firewall zone policies or forwarding rules (it only manages membership in
  the zone selected by `VPN_FIREWALL_ZONE`);
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

For downloaded release assets (rather than a configured package repository),
pass the matching dependency files to the package manager together so it can
resolve the local dependencies. For example on ARM64:

```sh
apk add --allow-untrusted ./*-arm64.apk
opkg install ./*-arm64.ipk
```

The `build-openwrt-lite` GitHub Actions workflow builds the package with the
official OpenWrt SDK and uploads the packages, checksums and build log as an
artifact. Before downloading an SDK it runs shell/Python syntax
checks plus the ownership, power-recovery and OpenClash-safe UCI lifecycle test
suite. The build matrix produces OpenWrt 24.10 `.ipk` and
OpenWrt 25.12 `.apk` packages for ARM64 and x86-64. A manual run accepts
`ipk_release` and `apk_release` inputs. The Python `etcd3`, `protobuf`, `six`,
`tenacity`, `typing-extensions` and native `grpcio` runtime are built as separate
`python3-*` packages, so installation never runs pip. The Meduza package uses
normal package-manager dependencies to install them. ARM64 and x86-64 use
grpcio's official musllinux wheels. Before
packaging, native wheel libraries are normalized from the Python wheel musl SONAME
(`libc.musl-<arch>.so.1`) to OpenWrt's standard `libc.so`; this lets OpenWrt's
dependency scanner and runtime linker resolve the normal libc package.

After both formats succeed, non-PR runs create or update a GitHub Release whose
name is `<UTC YYYYMMDD>-<7-character commit hash>`. The release contains both
packages, `SHA256SUMS` and release metadata. Pull requests build both formats
without creating a release.

Published package files use the same date/hash identity and expose their CPU
family, for example
`meduza-openwrt-lite-<YYYYMMDD>-<short-hash>-arm64.ipk` and
`meduza-openwrt-lite-<YYYYMMDD>-<short-hash>-arm64.apk` or
`meduza-openwrt-lite-<YYYYMMDD>-<short-hash>-x86-64.apk`.
Each architecture build publishes `meduza-openwrt-lite`, `python3-etcd3`,
`python3-grpcio`, `python3-protobuf`, `python3-six`, `python3-tenacity`, and
`python3-typing-extensions`. The display filename records the build's real CPU
family. Pure Python and main packages are architecture-independent. OpenWrt
24.10 IPK metadata for `python3-grpcio` uses `aarch64_generic` or `x86_64`.
APK metadata uses `noarch` for compatibility with vendor APK implementations,
but the `python3-grpcio` APK still contains native code: install the `arm64`
package set only on ARM64 and the `x86-64` set only on x86-64.

The workflow verifies the SDK against the target directory's official
`sha256sums` file before extraction and verifies each produced package's
expected metadata architecture.

The package uses the native `tinc`, `frr` (which provides `vtysh`),
`openvpn-openssl`, `wireguard-tools` and `python3` packages. They are not
encoded as hard APK/IPK dependencies because vendor OpenWrt feeds frequently
rename, omit, or pin these packages to a firmware-specific ABI. A single
unavailable integration package would otherwise make the Meduza package itself
uninstallable. Install the native packages needed for the integrations enabled
on the router. At service startup Meduza checks its core commands and reports
missing commands through syslog instead of failing package installation.
The reporter reads `/proc` directly and does not require `procps-ng-pgrep`.
JSON processing uses Python's standard library and does not require `jq`.
For legacy OpenVPN static-key configurations, Meduza detects OpenVPN 2.7's
compatibility option and adds `allow-deprecated-insecure-static-crypto`
automatically. TLS-based configurations are not changed.

Stopping the Meduza service stops every tinc, OpenVPN and WireGuard runtime it
owns. Persistent UCI interfaces, generated configuration and the last-known-good
etcd cache are retained so a normal reboot and an unexpected power loss recover
the same state even while etcd is temporarily unavailable. To remove all
owner-marked UCI sections, zone memberships, devices, generated secrets and the
cache, use:

```sh
/etc/init.d/meduza purge
```

Setting `meduza.main.enable=0` and restarting, or uninstalling the package,
performs the same purge. Package upgrades stop only runtime processes and retain
the persistent state for the upgraded service. Before an in-place upgrade, the
package writes a durable transaction-nonce-bound `blocked` handoff record,
unregisters the old procd agent, proves every old generator/helper process has
exited, seals the one-time legacy firewall migration state, and only then writes
`ready`. The custom post-install step publishes a matching build/transaction
completion seal last. The new init script and generator refuse every mutation
until both seals match, including when an APK implementation invokes its default
start action before the custom post-install step or continues unpacking after a
failed pre-upgrade script.

Some APK implementations also continue deleting package payload files after a
pre-deinstall script reports an error. Meduza therefore keeps an untracked,
owner-marked purge bundle at `/etc/meduza/recovery` while installed. A successful
purge/uninstall removes it. The bundle deliberately does not duplicate Python,
rpcd/UCI or native VPN dependencies. If APK removed only the main package while
those dependencies remain, fix the reported ownership/UCI conflict and run:

```sh
/etc/meduza/recovery/meduza-recover --purge
```

If `python3`, `ubus`, `uci` or a required helper dependency was also removed,
first reinstall the same Meduza release and its matching `python3-*` package set
with the service disabled, then run `meduza-recover --purge`. Package-script
return codes alone cannot make every vendor APK implementation retain orphaned
dependencies after a failed uninstall.

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
init script when that membership actually changed. OpenWrt therefore selects
fw3/iptables or fw4/nftables itself.
Meduza does not invoke `iptables`, `nft`, `fw3` or `fw4` directly. Zone
input/output/forward and masquerading policies remain entirely under the
administrator's existing firewall configuration.

The sync uses an isolated rpcd UCI session, so it neither reads nor commits
another LuCI/CLI client's `/tmp/.uci` changes. Network/OpenVPN sections carry
a random generation nonce backed by an external, durable before/after
fingerprint record; an inline `meduza_owner` option is never sufficient by
itself to authorize replacement or deletion. Firewall membership uses genuine
per-token UCI `LIST_ADD`/`LIST_DEL` deltas and owns only the `(zone, logical
interface)` edge it added—it never deletes and reconstructs a complete zone
list. The helper validates its byte and semantic baseline before each official
rpcd commit and retries conflicts from live state. An unchanged reconciliation
performs no network or firewall reload. This is important on systems running
OpenClash: Meduza never
touches `utun`, OpenClash's UCI, policy-routing tables, fwmarks, or nftables /
iptables chains, and preserves `utun` when editing a zone membership list.

Leave `ETCD_CA`, `ETCD_CERT` and `ETCD_KEY` empty when mutual TLS is not used.
`ETCD_ENDPOINTS` accepts a comma-separated value for compatibility with the
main project. The Python agent automatically fails over between all configured
endpoints, refreshes expired authentication tokens, and retries failures with
bounded exponential backoff. It polls `/commit` rather than watching individual
configuration keys. A changed commit triggers a complete, idempotent reconciliation.

To inspect reconciliation diagnostics:

```sh
logread -e meduza
```

An apply failure now includes a fixed, non-sensitive stage, for example
`generator apply failed: stage=uci-apply status=1`. The stage identifies
whether the failure occurred in the package seal, UCI transaction, generated
configuration, VPN activation, FRR activation, or final ownership publish
without printing etcd values, credentials, or VPN keys.

Generated VPN files are isolated under `/etc/meduza/generated`. The persistent
ownership manifest is `/etc/meduza/managed/interfaces`; an interrupted apply is
journaled in `interfaces.pending`, and each generated directory has a separate
external create/owned/delete phase record under `/etc/meduza/managed`. The
delete phase first renames the directory to a nonce-bearing private tombstone,
making a power loss during recursive removal safely replayable.
`uci-ownership.json` separately records every managed UCI section generation
and firewall membership edge, so a copied/stale inline owner option cannot
authorize deletion.
`/etc/meduza/cache.json` stores the last successfully applied etcd snapshot;
`cache.pending.json` closes the first-apply crash window. These files are mode
`0600` and the parent directories are mode `0700`. WireGuard is applied with
`wg setconf` and `ip`, so it does not create routes; FRR remains responsible for
routing, matching the main project's behavior.


For every enabled VPN instance, reconciliation maintains an owner-marked
OpenWrt network interface:

- tinc network `mesh` becomes `tinc_mesh`, bound to its configured TAP device;
- OpenVPN instance `office` becomes `ovpn_office`, bound to its configured
  tunnel device. OpenWrt 25.12 and newer use the native netifd `openvpn` proto;
  older releases use an instance with the same name in `/etc/config/openvpn`,
  so hotplug, firewall and PBR consumers all see `ovpn_office`;
- WireGuard instance `office` becomes `wg_office`, bound to its WireGuard
  device. UCI identifiers use `_` because OpenWrt forbids `-` in section names;
  the underlying Linux VPN device keeps the exact name configured in etcd.

When no OpenVPN or WireGuard device is configured, the lite package creates an
`ovpn-<instance>` or `wg-<instance>` device name. Names longer than Linux's
15-byte limit receive a deterministic suffix instead of being ambiguously
truncated. All three interface types are automatically moved to
`VPN_FIREWALL_ZONE`. Stale Meduza interfaces, generated files and old zone
memberships are removed when instances are disabled or renamed. A UCI section
or Linux device that already exists without Meduza's matching external
ownership record and runtime marker causes reconciliation to fail safely rather
than being adopted or overwritten.
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
- Instance names are restricted to letters, digits, `_` and `-`; Linux device
  names are additionally limited to 15 bytes. `lo` and OpenClash's `utun` are
  reserved and rejected.
- Configuration and ownership phase journals are written and directory-fsynced
  before native services are changed. Reconcile and cleanup share one lock;
  generated-directory creation and deletion are replayable after power loss.
- Removing `/var/run/meduza` does not lose ownership: the persistent manifest,
  external UCI generation records and device markers remain authoritative, and
  startup reconstructs runtime state from the last-known-good cache before
  polling etcd.
- Give the etcd account read access to `/commit`, `/global/`, `/nodes/` and
  write access to `/updated/<NODE_ID>/`.

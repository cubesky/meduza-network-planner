# Meduza OpenWrt Rust

`openwrt-rust` is the Rust replacement for the retired
`meduza-openwrt-lite` Python and shell implementation. Its application logic is
delivered as one statically linked executable **per CPU architecture**:

- `meduza-openwrt-linux-x86_64`
- `meduza-openwrt-linux-aarch64`

These are two independent ELF files. A single ELF cannot execute on both x86-64
and AArch64. "Single-file" means that an individual router needs only the one
application executable matching its CPU; it does not mean one universal binary.

The raw AArch64 ELF uses a generic static instruction baseline. OpenWrt package
metadata is necessarily target-specific: release APK/IPK files for Cudy's
MediaTek Filogic devices are built with the `mediatek/filogic` SDK and therefore
carry the `aarch64_cortex-a53` architecture tag. An `aarch64_generic` APK from
the `armsr/armv8` SDK is not installable on those systems even though the ELF
inside is otherwise executable.

The OpenWrt APK/IPK may additionally contain a small procd init script, the UCI
conffile, and a native LuCI JavaScript settings page. The page reuses
`luci-base` and does not bundle a separate web framework. Those integration
files do not become part of the executable and do not reintroduce Python, gRPC
shared objects, or the old shell controller.

> **Implementation status:** the Rust controller, public CLI, persistent
> ownership/recovery state machines, OpenWrt package integration, and the two
> static release targets are implemented in this directory. Host tests and CI
> checks do not replace a real-router acceptance run; use the current binary's
> `--help`, package manifest, CI results, and the verification matrix below as
> the source of truth for a particular build.

## Scope

The replacement reconciles the existing Meduza etcd model on a
routed OpenWrt side gateway:

- read `/commit`, `/global/` and `/nodes/<NODE_ID>/`;
- create and operate Meduza-owned tinc, OpenVPN and WireGuard instances;
- generate and activate FRR OSPF/BGP configuration;
- directly create, configure and remove only its own Linux VPN interfaces;
- publish node, service and tunnel status under `/updated/<NODE_ID>/`;
- retain a durable last-known-good state and recover interrupted work after a
  reboot or power loss.

Only `/etc/config/meduza` is read through UCI. Reconciliation never creates or
changes `network`, `openvpn` or `firewall` UCI sections and never calls
`ifup`/`ifdown`. It must not take ownership of an administrator's Linux
interface, VPN process, configuration file, or OpenClash's `utun` device.

## Runtime layout

The target layout is:

| Path | Purpose | Persistence |
| --- | --- | --- |
| `/usr/sbin/meduza-openwrt` | Architecture-specific Rust executable | package payload |
| `/etc/init.d/meduza` | Minimal procd integration, when installed from APK/IPK | package payload |
| `/etc/config/meduza` | Administrator-owned UCI configuration | persistent conffile |
| `/www/luci-static/resources/view/meduza/settings.js` | LuCI settings view | package payload |
| `/etc/meduza/pki/` | Administrator-provided etcd CA/client material | persistent, never generated cleanup data |
| `/etc/meduza/generated/` | Meduza-owned generated VPN data | persistent, mode-restricted |
| `/etc/meduza-state/cache.json` | Last-known-good desired-state cache | persistent |
| `/etc/meduza-state/cache.pending.json` | Interrupted cache transaction | persistent journal |
| `/etc/meduza-state/managed/` | Ownership records and replayable transaction journals | persistent |
| `/var/run/meduza/` | PIDs, live status JSON and other reconstructable runtime data | volatile |
| Linux abstract socket `meduza-openwrt-transaction-v1` | Cross-command reconciliation lock | kernel-only; disappears on exit/crash |

`/etc/meduza` is deliberately limited to operator configuration/PKI and
Meduza-generated VPN configuration. Controller journals, ownership evidence and
LKG caches belong only in `/etc/meduza-state`; the executable belongs in
`/usr/sbin`, not either data directory. `purge` removes the Rust-owned state root
after its external resources have been restored, but it never recursively
deletes `/etc/meduza` and therefore does not delete `/etc/meduza/pki`.

Persistent formats may evolve while the Rust implementation is being built.
Every format must be versioned, atomically replaced, and directory-fsynced.
The program must be able to distinguish its own complete state from foreign,
legacy, truncated, or symlinked content before modifying or deleting anything.

The first Rust development builds stored cache and managed state directly under
`/etc/meduza`. On startup this layout is moved once to `/etc/meduza-state` only
when its Rust-specific `managed/ownership.json` validates. Cache/manifest names
alone are also used by the retired Lite controller and are intentionally not
treated as proof. An ambiguous old layout is left untouched and startup fails
with an operator-actionable error; see [MIGRATION.md](MIGRATION.md).

## CLI contract

The entry points are shown below. Exact options must be read from
`meduza-openwrt --help` for the installed build; undocumented flags should not
be assumed.

| Command | Intended responsibility |
| --- | --- |
| `daemon` | Run the long-lived etcd reconcile and status-report loop under procd |
| `apply <SNAPSHOT>` | Validate and atomically reconcile one captured JSON generation |
| `recover` | Replay or roll back an interrupted local transaction and restore LKG |
| `runtime-stop` | Stop only owned VPN processes and links while retaining restart state |
| `purge` | Remove only resources proven to be owned by this implementation |
| `status [--json]` | Report local reconciliation and managed-runtime state |
| `doctor` | Check configuration, native commands, kernel features and writable paths |
| `--version` | Print the executable version |

These commands are lifecycle boundaries, not permission to trust an inline
`meduza_owner` marker by itself. Replacement or deletion also requires the
corresponding durable external ownership generation.

Offline roots are intentionally not exposed by the production CLI: applying
an offline journal while commands still target the live process and network
namespaces would be unsafe. Tests use an internal path prefix without executing host
mutations.

## UCI configuration

The design keeps the existing `/etc/config/meduza` `main` section so an
administrator does not have to re-enter connection settings:

| UCI option | Meaning |
| --- | --- |
| `enable` | Enable the procd-managed daemon |
| `NODE_ID` | Node name used below `/nodes/` and `/updated/` |
| `ETCD_ENDPOINTS` | Comma-separated native etcd v3 endpoints |
| `ETCD_CA` | Optional CA certificate path |
| `ETCD_CERT` | Optional client certificate path |
| `ETCD_KEY` | Optional client private-key path |
| `ETCD_USER` | Optional etcd username |
| `ETCD_PASS` | Optional etcd password |

When the package is installed with LuCI, open **Services → Meduza** to edit
these values. The page masks `ETCD_PASS`, but UCI still stores the value in the
root-readable `/etc/config/meduza` file. The first tab shows etcd,
OpenVPN/WireGuard and filtered Meduza logs; the second shows Tinc and FRR; the
third contains these settings and certificate paths.

Example configuration:

```sh
uci set meduza.main.enable='1'
uci set meduza.main.NODE_ID='router-01'
uci set meduza.main.ETCD_ENDPOINTS='https://etcd.example.net:2379'
uci set meduza.main.ETCD_CA='/etc/meduza/pki/ca.crt'
uci set meduza.main.ETCD_CERT='/etc/meduza/pki/client.crt'
uci set meduza.main.ETCD_KEY='/etc/meduza/pki/client.key'
uci commit meduza
```

Compatibility of configuration keys does **not** imply that the Rust program
may adopt the old implementation's ownership records or generated files. See
[MIGRATION.md](MIGRATION.md) before installing it on a router that has run
`meduza-openwrt-lite`.

## Native OpenWrt dependencies

The Rust executable replaces the Python agent and shell controller, not the VPN
daemons, routing daemon, or the kernel. Install the native packages for
the enabled features from the router firmware's own feed. Package names vary by
vendor, but commonly include:

- core integration: `uci`, `ubus`, `rpcd`, `luci-base`, procd and a usable
  `ip` (`ubus`/`rpcd`/`luci-base` serve the web UI, not VPN reconciliation);
- tinc: `tinc` providing `tincd`;
- OpenVPN: `openvpn-openssl` or a compatible vendor OpenVPN build;
- WireGuard: `wireguard-tools` providing `wg`;
- routing: `frr` providing `vtysh` and the required OSPF/BGP daemons.

Firewall policy is deliberately outside daemon ownership. If the router's
policy blocks traffic on dynamically created interfaces, add rules using the
stable device names from the status page; Meduza does not rewrite a firewall
zone behind the administrator or OpenClash.

WireGuard kernel support must come from the running firmware. If a separate
`kmod-wireguard` package is required, it must match that firmware's exact kernel
ABI; never install a generic SDK's kernel module on the router.

Before enabling a feature, check the actual commands on the target:

```sh
command -v uci
command -v ubus
command -v ip
command -v tincd
command -v openvpn
command -v wg
command -v vtysh
```

Missing optional commands should disable or fail the corresponding feature with
a non-sensitive diagnostic. Their presence alone is not proof that the feature
has passed an end-to-end test.

## Build and single-file checks

The Rust release targets are:

```text
x86_64-unknown-linux-musl
aarch64-unknown-linux-musl
```

A representative local build is:

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
```

The build host also needs `protoc` plus a musl C compiler/linker for the target
architecture (`etcd-client` generates protocol bindings and `ring` builds C
objects). Cross-linker setup is environment-specific; the repository workflow
downloads the matching OpenWrt SDK and should be used for release artifacts.
Do not interpret the commands above as evidence that a particular checkout has
already cross-compiled successfully.

Release CI should verify each output rather than relying on its filename:

```sh
file meduza-openwrt-linux-x86_64
readelf -h meduza-openwrt-linux-x86_64
readelf -l meduza-openwrt-linux-x86_64
readelf -d meduza-openwrt-linux-x86_64

file meduza-openwrt-linux-aarch64
readelf -h meduza-openwrt-linux-aarch64
readelf -l meduza-openwrt-linux-aarch64
readelf -d meduza-openwrt-linux-aarch64
```

The expected result is the correct ELF machine, no requested program
interpreter, and no `DT_NEEDED` shared-library entries. CI must also execute
`--version` or an equivalent smoke command natively for x86-64 and through an
AArch64 runner or QEMU for AArch64.

## Verification matrix

The following is the acceptance matrix, not a statement that every cell is
currently green:

| Layer | x86-64 | AArch64 | Required evidence |
| --- | --- | --- | --- |
| Rust quality | host | host | `cargo fmt --check`, clippy with warnings denied, locked unit tests |
| Cross-build | musl target | musl target | successful release link from `Cargo.lock` |
| Single executable | native inspection | native inspection | correct ELF machine, no interpreter and no shared-library dependency |
| CLI smoke | native | native runner or QEMU | help/`--version` plus failure-safe configuration validation |
| Reconcile integration | mock OpenWrt | shared logic plus target smoke | direct tinc/OpenVPN/WG, ip and FRR command/state assertions |
| Idempotency/concurrency | required | shared logic | unchanged apply does not restart or reconfigure healthy runtimes |
| Ownership safety | required | shared logic | preserve `utun`, foreign devices, UCI sections, files and firewall members |
| Power-loss recovery | fault injection | shared logic | kill before/after each journal, fsync, rename, runtime and FRR transition |
| etcd behavior | integration etcd | target smoke | TLS, endpoint failover, `/commit`, lease and ack/apply separation |
| Real OpenWrt | QEMU/device | QEMU/device | direct VPN/FRR activation, LuCI status/logs, reboot, purge and rollback |
| Release | artifact check | artifact check | two correctly named executables, hashes and build metadata |

At every injected crash point, restart must converge to either the complete old
generation or the complete new generation. A mixture of the two is a failure.
Logs and errors must never contain etcd credentials, private keys, certificates,
or rendered child-process arguments containing secrets.

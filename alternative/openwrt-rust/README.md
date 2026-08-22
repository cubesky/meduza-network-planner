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
- publish a minimal `proto none`, `auto 0` UCI network description for every
  daemon-owned VPN so netifd and LuCI can represent its status;
- publish node, service and tunnel status under `/updated/<NODE_ID>/`;
- retain a durable last-known-good state and recover interrupted work after a
  reboot or power loss.

VPN daemons are not started by netifd/procd; the Rust daemon owns their full
lifecycle. Their generated `network.<logical>` sections use `proto none` and
only bind netifd status to the already-created Linux device. When
`VPN_FIREWALL_ZONE` is set, Meduza puts those logical interfaces in a dedicated
`meduza` zone and creates bidirectional forwarding between `meduza` and the
selected existing zone. If the `meduza` zone or either matching forwarding
already exists, it is reused without being adopted. Meduza never changes the
selected zone's policy or NAT. It must not take ownership of an administrator's UCI
section, Linux interface,
VPN process, configuration file, existing firewall member, or OpenClash's
`utun` device.

## Runtime layout

The target layout is:

| Path | Purpose | Persistence |
| --- | --- | --- |
| `/usr/sbin/meduza-openwrt` | Architecture-specific Rust executable | package payload |
| `/etc/init.d/meduza` | Minimal procd integration, when installed from APK/IPK | package payload |
| `/etc/config/meduza` | Administrator-owned UCI configuration | persistent conffile |
| `/www/luci-static/resources/view/meduza/settings.js` | LuCI settings view | package payload |
| `/etc/meduza/pki/` | Administrator-provided etcd CA/client material | persistent, never generated cleanup data |
| `/etc/meduza-state/cache.json` | Last-known-good desired-state cache | persistent |
| `/etc/meduza-state/cache.pending.json` | Interrupted cache transaction | persistent journal |
| `/etc/meduza-state/managed/` | Ownership records and replayable transaction journals | persistent |
| `/var/run/meduza/generated/` | VPN and FRR configurations regenerated from the current etcd generation | volatile, mode-restricted |
| `/var/run/meduza/` | PIDs, live status JSON and other reconstructable runtime data | volatile |
| Linux abstract socket `meduza-openwrt-transaction-v1` | Cross-command reconciliation lock | kernel-only; disappears on exit/crash |

`/etc/meduza` is deliberately limited to operator configuration and PKI.
Generated VPN and FRR files are reconstructed beneath `/var/run/meduza` only
after the daemon reads the current etcd generation. `/etc/frr/frr.conf` remains
the administrator-owned baseline: Meduza restarts that baseline and loads its
volatile routing overlay through `vtysh`, then restores the baseline on stop.
Controller journals, ownership
evidence and bounded rollback caches belong only in `/etc/meduza-state`; the
executable belongs in `/usr/sbin`, not any data directory. `purge` removes the
Rust-owned state root after its external resources have been restored, but it
never recursively deletes `/etc/meduza` and therefore does not delete
`/etc/meduza/pki`.

The renderer rejects any generated VPN file larger than 6 MiB, rejects a VPN
generation larger than 16 MiB in aggregate, and rejects a generated FRR file
larger than 16 MiB before writing to tmpfs.

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
| `VPN_FIREWALL_ZONE` | Existing firewall zone connected bidirectionally to the dedicated `meduza` zone |

When the package is installed with LuCI, open **Services → Meduza** to edit
these values. The page masks `ETCD_PASS`, but UCI still stores the value in the
root-readable `/etc/config/meduza` file. The first tab shows etcd,
OpenVPN/WireGuard and filtered Meduza logs; the second shows each remote Tinc
peer's live reachability plus individual BGP/OSPF neighbor state from FRR; the
third contains these settings and certificate paths. Disabling the controller
performs owner-aware cleanup without contacting etcd and leaves a volatile
`Disabled / not connected` status for the page. While disabled, the status
endpoint and LuCI page suppress stale VPN, Tinc and FRR observations instead
of presenting a prior `up` result as current state.

Example configuration:

```sh
uci set meduza.main.enable='1'
uci set meduza.main.NODE_ID='router-01'
uci set meduza.main.ETCD_ENDPOINTS='https://etcd.example.net:2379'
uci set meduza.main.ETCD_CA='/etc/meduza/pki/ca.crt'
uci set meduza.main.ETCD_CERT='/etc/meduza/pki/client.crt'
uci set meduza.main.ETCD_KEY='/etc/meduza/pki/client.key'
uci set meduza.main.VPN_FIREWALL_ZONE='lan'
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

- core integration: `netifd`, `uci`, `ubus`, `rpcd`, `luci-base`, procd and a usable
  `ip` (`uci` stores controller settings, minimal VPN interface descriptions,
  and optional narrow firewall membership; `ubus`/`rpcd`/`luci-base` serve the
  web UI);
- tinc 1.1 providing both `tincd` and the `tinc` control command used for
  per-peer reachability;
- OpenVPN: `openvpn-openssl` or a compatible vendor OpenVPN build;
- WireGuard: `wireguard-tools` providing `wg`;
- routing: `frr` providing `vtysh` and the required OSPF/BGP daemons.

The optional firewall-zone setting names an existing interconnect zone, such as
`lan`; it must not be `meduza`. Meduza places every stable VPN logical interface
in a dedicated `meduza` zone and ensures `meduza -> <selected>` plus
`<selected> -> meduza` forwarding. A missing `meduza` zone is created with
`input`, `output` and `forward` set to `ACCEPT`. An existing `meduza` zone,
matching forwarding or membership is borrowed as-is and never deleted. Meduza
removes only forwarding and membership objects carrying its exact external
ownership record and nonce when the setting changes, the daemon stops, or
`purge` runs. An automatically created `meduza` zone is retained after its
ownership markers are released, so administrator additions cannot be lost and
the next start reuses it. OpenClash members, the selected zone's policy, NAT and
every unrelated firewall object are preserved.

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

After the two static executables have been verified, CI assembles the APK/IPK
payload directly with `package/build-direct.sh`. It does not update feeds,
install feed sources, run `defconfig`, or compile LuCI and runtime dependencies.
The matching SDK is retained only for its official `ipkg-build` or `apk mkpkg`
tool, while dependency names are recorded as package metadata for the router's
package manager to resolve at installation time.

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

# Migrating from `meduza-openwrt-lite`

This procedure retires the old Python/shell package before the Rust controller
is allowed to manage the router. It is deliberately conservative: the new
program must not infer ownership from a familiar interface name or from an
inline UCI `meduza_owner` option.

> **Migration status:** in-place adoption is intentionally unsupported. Use the
> purge-and-clean-install procedure below. The Rust controller will not rewrite
> old ownership markers or infer ownership from old inline UCI fields. Do not point an
> unverified build at a production router whose only management path traverses
> one of the VPNs being replaced.

## What is compatible

The intended input compatibility is:

- the `/etc/config/meduza` `main` settings listed in [README.md](README.md);
- the existing `/commit`, `/global/`, `/nodes/<NODE_ID>/` desired-state keys;
- native tinc, OpenVPN, WireGuard, netifd/firewall and FRR integration.

The following old runtime state is **not automatically compatible** merely
because it is stored below the same directory:

- `/etc/meduza/cache.json` and `cache.pending.json`;
- `/etc/meduza/managed/`, including interface, UCI and firewall-edge records;
- `/etc/meduza/generated/`;
- `/etc/meduza/recovery/`;
- UCI sections, firewall members, Linux links and processes created by the old
  controller;
- an FRR file or backup that is still inside an old takeover transaction.

User-provided `/etc/meduza/pki/` material is not generated lifecycle state and
must be preserved.

## Moving state from an earlier Rust development build

Early Rust builds temporarily placed their LKG cache and Rust ownership
journals at `/etc/meduza/cache*.json` and `/etc/meduza/managed/`. Current builds
reserve `/etc/meduza` for operator PKI/configuration plus
`/etc/meduza/generated`, and keep controller state under `/etc/meduza-state`.

Stop the earlier Rust service before installing or starting the current build.
The first current invocation performs a crash-replayable, one-time relocation
only when `managed/ownership.json` is a valid Rust ownership database. It moves
the managed directory first, rewrites the one stored FRR backup path, and then
moves stable/pending caches. A power loss between those operations is resumed
from the relocated ownership database on the next invocation.

The retired Lite implementation used the same cache and manifest names but did
not create Rust `ownership.json`. Therefore cache or manifest files without
that proof are deliberately left untouched and startup fails. Do not work
around that refusal by copying them into `/etc/meduza-state`: use the Lite
ownership-aware purge procedure below, or audit the ambiguous resources with
the previous controller still available. The relocation never moves or removes
`/etc/meduza/pki` and does not adopt an old generated directory without matching
Rust ownership records.

## Before changing the router

1. Confirm an independent management path. A failed VPN replacement must not
   remove the SSH route needed to repair it.
2. Stop topology changes at the source, or hold `/commit` stable for the
   migration window.
3. Save a secure, off-device backup of `/etc/config/meduza`,
   `/etc/meduza/pki/`, the current UCI network/firewall/OpenVPN exports and the
   current FRR configuration. The backup contains credentials and private keys;
   encrypt it and restrict access.
4. Record the installed package and firmware versions and the output of
   `ip link`, `uci show network`, `uci show firewall`, `uci show openvpn` when
   that package exists, and `vtysh -c 'show running-config'` when FRR is enabled.
5. Verify that the native VPN/FRR packages required by the desired topology are
   available from this firmware's own feed.

Do not recursively delete `/etc/meduza`: it may contain the only copy of the
router's client certificate and private key.

## 1. Quiesce the old controller

If its init script is intact:

```sh
/etc/init.d/meduza stop
/etc/init.d/meduza disable
```

Confirm that the old command is no longer running:

```sh
ps w | grep '[m]eduza-agent.py'
```

An empty result is expected. Also check for old `meduza-generator` or
`meduza-openwrt-sync` processes before continuing. If a controller process
cannot be stopped, do not run two reconcilers at once; repair or isolate the old
installation first.

## 2. Ask the old implementation to purge its owned state

When `/etc/init.d/meduza` is available, use its ownership-aware cleanup:

```sh
/etc/init.d/meduza purge
```

Some vendor APK implementations can leave the package installed with a failed
post-install or can remove payload files after a failed uninstall. If the init
script is unavailable but the old owner-marked recovery bundle exists, inspect
it and use the old release's documented recovery entry point:

```sh
/etc/meduza/recovery/meduza-recover --purge
```

That recovery helper belongs to `meduza-openwrt-lite`; it is not part of the
Rust application. If neither ownership-aware entry point is available, do not
replace `purge` with broad `rm -rf`, UCI wildcard deletion, or deletion based
only on an inline owner string. Reinstall/repair the matching old package with
its service disabled, or audit the external ownership records and resources
manually before proceeding.

The old purge must be allowed to finish its FRR restore and firewall/UCI
cleanup. Removing the package first can make recovery harder.

## 3. Verify the old ownership domain is empty

Before installing the Rust controller, verify all of the following:

- no old Python agent or shell helper process is running;
- no Meduza-owned tinc/OpenVPN/WireGuard process or Linux link remains;
- no `tinc_*`, `ovpn_*` or `wg_*` UCI section remains solely because of the old
  package;
- the selected firewall zone still contains administrator/OpenClash members,
  but no membership edge that the old package owned;
- `/etc/meduza/generated/` contains no active old generation;
- old pending interface, UCI, generated-directory or cache transactions have
  been completed or safely rolled back;
- `/etc/frr/frr.conf` is either the administrator's restored file or an
  intentionally accepted configuration, not a half-finished old takeover;
- old procd/rc links do not start `meduza-agent.py` on the next boot.

Names alone do not prove ownership. For example, an administrator may have a
legitimate interface beginning with `wg_`; preserve it unless the old durable
ownership record and the live generation agree.

If any check is ambiguous, stop. Resolve it with the saved pre-migration state
instead of allowing the new controller to adopt the object.

## 4. Remove the old package

Only after its ownership-aware purge succeeds, remove
`meduza-openwrt-lite` with the firmware's package manager. Package-manager
syntax and repair behavior differ between OpenWrt releases and vendor forks;
check the installed package database before and after removal.

The old Python dependency packages may be removed later if no other installed
software uses them. Their removal is not required for the Rust executable to
run and should not be bundled into the network-state migration.

Preserve `/etc/config/meduza` and `/etc/meduza/pki/`. If the package manager
leaves `.apk-new` or equivalent conffile variants, compare them explicitly;
never overwrite working endpoint or certificate paths without review.

## 5. Install the Rust build

Prefer the APK/IPK matching the router firmware because it installs the procd
and UCI integration. A raw executable installation is suitable for controlled
testing, but the operator must then supply boot integration separately.

Before installation:

1. verify the published SHA-256 digest;
2. verify that the x86-64 router receives
   `meduza-openwrt-linux-x86_64`, or that the 64-bit ARM router receives
   `meduza-openwrt-linux-aarch64`;
3. inspect the ELF and confirm it is static as described in the README;
4. ensure `tincd`, `openvpn`, `wg`, `vtysh` and WireGuard kernel support exist
   for the features enabled in etcd.

The installed application path is expected to be:

```text
/usr/sbin/meduza-openwrt
```

Run the installed build's help and non-mutating checks first:

```sh
/usr/sbin/meduza-openwrt --help
/usr/sbin/meduza-openwrt --version
/usr/sbin/meduza-openwrt doctor
```

`doctor` is part of the target CLI contract. If it is absent in an early build,
that build has not met the documented operational interface; use explicit
command/path checks and do not treat this document as proof of readiness.

There is no in-place migration subcommand. If the old ownership-aware purge
cannot finish, repair/reinstall the matching old release with its service
disabled, or perform a documented manual audit. Installing the Rust controller
over unresolved legacy transactions is intentionally fail-closed.

## 6. First reconciliation

Keep automatic startup disabled for the first run. The target sequence is:

1. run `recover` so a new-format interrupted transaction, if any, is resolved;
2. validate `/etc/config/meduza` and etcd reachability;
3. run one foreground `apply`;
4. inspect `status`, the UCI diff, Linux links, firewall membership, VPN
   processes and FRR running configuration;
5. repeat `apply` and prove that no network/firewall reload occurs when desired
   state is unchanged;
6. reboot once and prove recovery works without immediate etcd availability;
7. only then enable and start the procd service.

Use the actual `--help` output for arguments to `recover`, `apply` and `status`.
This document intentionally does not invent flags that may differ in an early
implementation.

## Rollback

Do not attempt rollback by installing the old controller over live Rust-owned
state.

1. stop and disable the Rust service;
2. use the Rust build's `runtime-stop`, followed by `purge`, while its executable
   and ownership database are still present;
3. verify that Rust-owned UCI sections, links, generated files and FRR takeover
   state are gone or restored;
4. reinstall the previous package and restore its configuration only after the
   Rust ownership domain is empty;
5. trigger one controlled old-controller reconciliation and verify the same
   management-path and idempotency checks.

If Rust `purge` refuses a resource because its generation, type, content, or
path changed, preserve the resource and investigate. A refusal is safer than
deleting administrator data.

## Migration acceptance checks

Migration is complete only when the following have been demonstrated on each
supported architecture and representative firmware:

| Check | Required result |
| --- | --- |
| Old controller isolation | no old process, boot link or pending transaction |
| Foreign-resource preservation | user UCI, links, files and OpenClash `utun` unchanged |
| First Rust apply | all requested VPN/FRR resources reach the intended state |
| Second identical apply | zero unnecessary network/firewall reloads |
| Interrupted apply | reboot yields complete old or complete new generation |
| etcd unavailable at boot | local LKG is restored; retry does not corrupt state |
| etcd ack failure | committed local state remains; only acknowledgement retries |
| Runtime stop | only Rust-owned processes and links stop; restart data remains |
| Purge/rollback rehearsal | only Rust-owned persistent state is removed/restored |
| Secret handling | restrictive modes and no credentials/private keys in logs |
| Architecture artifact | correct static x86-64 or AArch64 ELF actually executes |

Passing host unit tests alone is insufficient for migration approval. At least
one real or faithfully emulated OpenWrt boot/reboot cycle is required for each
CPU architecture before production rollout.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::OWNER;
use crate::atomic;
use crate::command::{Runner, command_exists};
use crate::config::Settings;
use crate::firewall::Firewall;
use crate::model::{BuildOptions, FlatSnapshot, build_desired_with_options};
use crate::ownership::{FrrRecord, OwnershipDb, Phase};
use crate::render::{RenderOptions, RenderedFile, render_all_with_options};
use crate::runtime::Runtime;
use crate::state::{
    ManifestEntry, Paths, Snapshot, read_manifest, regular_file_exists, write_manifest,
};

const MAX_FRR_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_GENERATED_FILE_BYTES: usize = 6 * 1024 * 1024;
const MAX_GENERATED_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_SMALL_MARKER_BYTES: usize = 4096;

pub struct Reconciler<R: Runner> {
    paths: Paths,
    runner: R,
}

impl<R: Runner> Reconciler<R> {
    pub fn new(paths: Paths, runner: R) -> Self {
        Self { paths, runner }
    }

    /// Prepare the private layout while holding the same cross-command lock
    /// used by reconciliation. This also serializes the one-time relocation
    /// of state written by the first Rust layout.
    pub fn prepare(&self) -> Result<()> {
        let _lock = ApplyLock::acquire(&self.paths)?;
        self.paths.prepare()
    }

    /// Relocate only an already-existing, positively identified prior Rust
    /// state layout. Unlike `prepare`, this creates no persistent directory on
    /// a clean installation and is therefore safe for read-only/status and
    /// purge entry points.
    pub fn migrate_layout(&self) -> Result<bool> {
        let _lock = ApplyLock::acquire(&self.paths)?;
        // Status is observational: it may relocate the small persistent state
        // journal, but must never retire a generated tree while a previous
        // runtime could still be using it. Daemon/apply preparation performs
        // that second migration after package upgrade has stopped the service.
        self.paths.migrate_legacy_rust_state()
    }

    pub fn apply(&self, settings: &Settings, snapshot: &Snapshot) -> Result<()> {
        let _lock = ApplyLock::acquire(&self.paths)?;
        self.paths.prepare()?;
        if !settings.enabled {
            return self.purge_locked();
        }
        if snapshot.node_id != settings.node_id {
            bail!(
                "snapshot belongs to {}, expected {}",
                snapshot.node_id,
                settings.node_id
            );
        }

        let flat = snapshot_to_flat(snapshot);
        flat.validate()?;
        let desired = build_desired_with_options(
            &flat,
            &BuildOptions {
                generated_root: self.paths.generated.clone(),
            },
        )?;
        let desired_entries: Vec<ManifestEntry> =
            desired.interfaces.iter().map(Into::into).collect();

        let stable = read_manifest(&self.paths.manifest)?;
        let pending = read_manifest(&self.paths.pending_manifest)?;
        let mut inventory = merge_inventory(&stable, &pending);
        let persisted_ownership = OwnershipDb::load(&self.paths)?;
        for record in persisted_ownership.generated.values() {
            inventory.push(record.entry.clone());
            if let Some(target) = &record.pending_entry {
                inventory.push(target.clone());
            }
        }
        inventory = merge_inventory(&inventory, &[]);
        let runtime = Runtime::new(self.paths.clone(), self.runner.clone());
        let firewall = Firewall::new(self.paths.clone(), self.runner.clone());
        // All collision checks precede persistent file/runtime mutation.
        runtime.validate_dependencies(&desired_entries)?;
        runtime.preflight(&desired_entries, &inventory)?;
        firewall.validate_zone(settings.firewall_zone.as_deref())?;

        let rendered = render_all_with_options(
            &flat,
            &desired,
            &RenderOptions {
                owner: OWNER.into(),
                frr_path: self.paths.generated_frr.clone(),
            },
        )?;
        let (vpn_files, frr_file): (Vec<_>, Vec<_>) = rendered
            .into_iter()
            .partition(|file| file.path != self.paths.generated_frr);
        let frr_file = frr_file
            .into_iter()
            .next()
            .context("FRR renderer produced no file")?;
        validate_rendered_vpn_size(&vpn_files)?;
        if self.paths.root.is_none()
            && Path::new("/etc/init.d/frr").is_file()
            && !command_exists("vtysh")
        {
            bail!("installed FRR runtime requires missing command: vtysh");
        }

        // From here onward every persistent or runtime mutation is replayable
        // from the validated snapshot and the five-column pending journal.
        snapshot.persist_pending(&self.paths)?;
        write_manifest(
            &self.paths.pending_manifest,
            &merge_inventory(&inventory, &desired_entries),
        )?;
        ensure_ip_forward(&self.paths)?;

        // Stop every old identity before rekeying its generated-directory
        // marker. Otherwise the new marker would revoke the exact authority
        // needed to terminate a still-running old daemon safely.
        let desired_set: BTreeSet<_> = desired_entries.iter().cloned().collect();
        let stale: Vec<_> = inventory
            .iter()
            .filter(|entry| !desired_set.contains(*entry))
            .cloned()
            .collect();
        runtime.stop_all(&stale)?;
        runtime.preflight(&desired_entries, &[])?;

        let mut ownership = OwnershipDb::load(&self.paths)?;
        for entry in &desired_entries {
            ownership.ensure_generated(&self.paths, entry)?;
        }
        let mut changed = write_rendered_files(&vpn_files, &desired_entries)?;
        for entry in &desired_entries {
            let directory = entry.config.parent().context("config has no parent")?;
            let expected = vpn_files
                .iter()
                .filter(|file| file.path.starts_with(directory))
                .map(|file| file.path.as_path());
            if ownership.prune_generated(entry, expected)? > 0 {
                changed.insert(entry.logical.clone());
            }
        }

        // VPN processes and links are owned directly by the daemon. Firewall
        // UCI is touched only through narrow, tagged `list device` deltas.
        ownership = OwnershipDb::load(&self.paths)?;
        let desired_generated: BTreeSet<_> = desired_entries
            .iter()
            .map(OwnershipDb::generated_key)
            .collect();
        for entry in &stale {
            if !desired_generated.contains(&OwnershipDb::generated_key(entry)) {
                ownership.remove_generated(&self.paths, entry)?;
            }
        }

        firewall.sync(settings.firewall_zone.as_deref(), &desired_entries)?;
        runtime.activate(&desired_entries, &changed)?;
        self.apply_frr(&frr_file)?;

        write_manifest(&self.paths.manifest, &desired_entries)?;
        if self.paths.pending_manifest.exists() {
            atomic::durable_remove(&self.paths.pending_manifest)?;
        } else {
            atomic::sync_dir(&self.paths.managed)?;
        }
        Snapshot::promote(&self.paths)?;
        if let Err(error) = atomic::atomic_write(
            &self.paths.runtime.join("last-success"),
            chrono::Utc::now().timestamp().to_string().as_bytes(),
            0o600,
        ) {
            tracing::warn!("configuration is committed but last-success marker failed: {error:#}");
        }
        tracing::info!(commit = ?snapshot.commit, "configuration reconciled");
        Ok(())
    }

    pub fn recover(&self, settings: &Settings) -> Result<()> {
        self.prepare()?;
        let source = if regular_file_exists(&self.paths.cache)? {
            &self.paths.cache
        } else {
            &self.paths.cache_pending
        };
        if !regular_file_exists(source)? {
            bail!("no persistent last-known-good snapshot exists");
        }
        let snapshot = Snapshot::read_from(source)?;
        self.apply(settings, &snapshot)
    }

    /// Reassert already-committed runtime state without fetching etcd or
    /// rewriting generated configuration. This is the daemon-side supervisor
    /// for directly managed VPN processes and links, replacing netifd/procd
    /// supervision of the individual tunnel instances.
    pub fn ensure_runtime(&self, settings: &Settings) -> Result<()> {
        let _lock = ApplyLock::acquire(&self.paths)?;
        self.paths.migrate_layout()?;
        let pending = read_manifest(&self.paths.pending_manifest)?;
        if !pending.is_empty() {
            bail!("an interrupted apply must be recovered before runtime supervision");
        }
        let entries = read_manifest(&self.paths.manifest)?;
        let runtime = Runtime::new(self.paths.clone(), self.runner.clone());
        runtime.validate_dependencies(&entries)?;
        runtime.preflight(&entries, &[])?;
        Firewall::new(self.paths.clone(), self.runner.clone())
            .sync(settings.firewall_zone.as_deref(), &entries)?;
        runtime.activate(&entries, &BTreeSet::new())?;
        self.ensure_frr_running()
    }

    pub fn runtime_stop(&self) -> Result<()> {
        let _lock = ApplyLock::acquire(&self.paths)?;
        self.paths.migrate_layout()?;
        self.runtime_stop_locked()
    }

    fn runtime_stop_locked(&self) -> Result<()> {
        let entries = merge_inventory(
            &read_manifest(&self.paths.manifest)?,
            &read_manifest(&self.paths.pending_manifest)?,
        );
        let entries = inventory_with_generated(&self.paths, entries)?;
        let mut errors = Vec::new();
        if let Err(error) = Runtime::new(self.paths.clone(), self.runner.clone()).stop_all(&entries)
        {
            errors.push(format!("VPN runtime stop failed: {error:#}"));
        }
        if let Err(error) = self.restore_frr() {
            errors.push(format!("FRR restore failed: {error:#}"));
        }
        if let Err(error) = Firewall::new(self.paths.clone(), self.runner.clone()).sync(None, &[]) {
            errors.push(format!("firewall membership cleanup failed: {error:#}"));
        }
        if let Err(error) = restore_ip_forward(&self.paths) {
            errors.push(format!("IPv4 forwarding restore failed: {error:#}"));
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        tracing::info!("managed VPN runtimes stopped; persistent LKG retained");
        Ok(())
    }

    pub fn purge(&self) -> Result<()> {
        let _lock = ApplyLock::acquire(&self.paths)?;
        self.paths.migrate_layout()?;
        self.purge_locked()
    }

    fn purge_locked(&self) -> Result<()> {
        if !meduza_state_exists(&self.paths)? {
            tracing::info!("no Meduza-owned OpenWrt state to purge");
            return Ok(());
        }
        let entries = merge_inventory(
            &read_manifest(&self.paths.manifest)?,
            &read_manifest(&self.paths.pending_manifest)?,
        );
        let entries = inventory_with_generated(&self.paths, entries)?;
        let mut errors = Vec::new();
        let runtime_stopped =
            match Runtime::new(self.paths.clone(), self.runner.clone()).stop_all(&entries) {
                Ok(()) => true,
                Err(error) => {
                    errors.push(format!("VPN runtime stop failed: {error:#}"));
                    false
                }
            };
        if runtime_stopped {
            let mut ownership = OwnershipDb::load(&self.paths)?;
            for entry in &entries {
                if let Err(error) = ownership.remove_generated(&self.paths, entry) {
                    errors.push(format!(
                        "generated configuration cleanup failed for {}: {error:#}",
                        entry.logical
                    ));
                }
            }
        }
        if let Err(error) = self.restore_frr() {
            errors.push(format!("FRR restore failed: {error:#}"));
        }
        if let Err(error) = Firewall::new(self.paths.clone(), self.runner.clone()).sync(None, &[]) {
            errors.push(format!("firewall membership cleanup failed: {error:#}"));
        }
        if let Err(error) = restore_ip_forward(&self.paths) {
            errors.push(format!("IPv4 forwarding restore failed: {error:#}"));
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        let db = OwnershipDb::load(&self.paths)?;
        if !db.generated.is_empty()
            || !db.wireguard_stages.is_empty()
            || db.frr.is_some()
            || object_exists(&self.paths.firewall_state)?
        {
            bail!("purge left active ownership records; state retained for retry");
        }

        // Ownership is now empty and all externally visible resources have
        // been restored.  Only at this commit point may retry evidence and
        // caches be removed.  Every already-absent path is still parent-fsynced
        // so a previous unlink+fsync failure cannot be mistaken for success.
        let cleanup_files = vec![
            self.paths.cache.clone(),
            self.paths.cache_pending.clone(),
            self.paths.reported.clone(),
            self.paths.manifest.clone(),
            self.paths.pending_manifest.clone(),
            self.paths.runtime.join("last-success"),
            self.paths.daemon_status.clone(),
            self.paths.managed.join("frr.conf.backup"),
            self.paths.managed.join("frr.pending.conf"),
            self.paths.managed.join("frr.reload.pending"),
            self.paths.managed.join("uci-reload.pending"),
            self.paths.firewall_state.clone(),
            self.paths.ip_forward_marker.clone(),
        ];
        for path in &cleanup_files {
            remove_known_file(path)?;
        }
        for entry in &entries {
            let pidfile = match entry.kind {
                crate::state::InterfaceKind::Tinc => Some(format!("tinc.{}.pid", entry.instance)),
                crate::state::InterfaceKind::Openvpn => {
                    Some(format!("openvpn.{}.pid", entry.instance))
                }
                crate::state::InterfaceKind::Wireguard => None,
            };
            if let Some(pidfile) = pidfile {
                remove_known_file(&self.paths.runtime.join(pidfile))?;
            }
        }
        // The ownership database is the final retry authority and is therefore
        // unlinked last.
        remove_known_file(&self.paths.ownership)?;

        for kind in ["tinc", "openvpn", "wireguard"] {
            remove_empty_dir(&self.paths.generated.join(kind), false)?;
        }
        remove_empty_dir(&self.paths.generated, false)?;
        remove_empty_dir(&self.paths.managed, false)?;
        remove_empty_dir(&self.paths.state, false)?;
        remove_empty_dir(&self.paths.runtime, false)?;
        // `/etc/meduza` is the operator/generated configuration domain and may
        // intentionally retain PKI or other configuration. Remove only an
        // entirely empty root; never recurse into or delete unknown contents.
        remove_empty_dir(&self.paths.data, false)?;
        tracing::info!("all Meduza-owned OpenWrt state purged");
        Ok(())
    }

    fn apply_frr(&self, rendered: &RenderedFile) -> Result<()> {
        // Development builds before the volatile layout may have an active
        // takeover of /etc/frr/frr.conf. Restore that transaction exactly
        // once before the new runtime-only model is allowed to start.
        self.restore_legacy_frr()?;

        if !Path::new("/etc/init.d/frr").is_file() && self.paths.root.is_none() {
            self.restore_runtime_frr()?;
            tracing::info!("FRR is not installed; skipping FRR");
            return Ok(());
        }
        if rendered.path != self.paths.generated_frr {
            bail!("FRR renderer returned an unexpected runtime path");
        }
        if rendered.contents.len() > MAX_FRR_FILE_BYTES {
            bail!("rendered FRR configuration exceeds {MAX_FRR_FILE_BYTES} byte limit");
        }

        let rendered_hash = sha256(&rendered.contents);
        if let Some(marker) = read_runtime_frr_marker(&self.paths)?
            && marker.phase == RuntimeFrrPhase::Owned
            && marker.sha256 == rendered_hash
            && runtime_frr_file_matches(&self.paths, &marker.sha256)?
            && self.frr_running()
        {
            return Ok(());
        }

        // Always return to the administrator's persistent FRR baseline before
        // applying a different complete runtime generation. This prevents
        // removed peers/networks from accumulating in the live configuration.
        self.restore_runtime_frr()?;
        let parent = self
            .paths
            .generated_frr
            .parent()
            .context("generated FRR configuration has no parent")?;
        atomic::ensure_private_dir(parent, 0o700)?;
        atomic::atomic_write(&self.paths.generated_frr, &rendered.contents, rendered.mode)?;
        write_runtime_frr_marker(
            &self.paths,
            &RuntimeFrrMarker {
                phase: RuntimeFrrPhase::Applying,
                sha256: rendered_hash.clone(),
            },
        )?;

        let generated_path = self.paths.generated_frr.to_string_lossy().into_owned();
        if let Err(error) = self.restart_frr_baseline().and_then(|()| {
            self.runner
                .status("vtysh", ["-b", "-f", generated_path.as_str()])
        }) {
            let rollback = self.restore_runtime_frr();
            return match rollback {
                Ok(()) => Err(error).context("could not apply volatile FRR configuration"),
                Err(rollback) => bail!(
                    "could not apply volatile FRR configuration: {error:#}; baseline restore also failed: {rollback:#}"
                ),
            };
        }

        write_runtime_frr_marker(
            &self.paths,
            &RuntimeFrrMarker {
                phase: RuntimeFrrPhase::Owned,
                sha256: rendered_hash,
            },
        )
    }

    fn restore_frr(&self) -> Result<()> {
        self.restore_runtime_frr()?;
        self.restore_legacy_frr()
    }

    fn restore_runtime_frr(&self) -> Result<()> {
        let marker = read_runtime_frr_marker(&self.paths)?;
        let file = read_frr_file(&self.paths.generated_frr)?;

        let Some(marker) = marker else {
            if let Some(file) = file {
                if !frr_has_owner(&file.bytes) {
                    bail!("unowned volatile FRR file exists without a runtime marker");
                }
                // The file is written before the Applying marker, and live
                // FRR is mutated only afterwards. This exact crash state can
                // therefore be removed without restarting the service.
                atomic::durable_remove(&self.paths.generated_frr)?;
                remove_empty_dir(
                    self.paths
                        .generated_frr
                        .parent()
                        .context("generated FRR configuration has no parent")?,
                    false,
                )?;
            }
            return Ok(());
        };

        let file_matches = file
            .as_ref()
            .is_some_and(|file| frr_has_owner(&file.bytes) && file.sha256 == marker.sha256);
        // The marker is durable before vtysh can mutate the live service, so
        // it authorizes returning FRR to its administrator-owned baseline.
        self.restart_frr_baseline()?;
        if file.is_some() && !file_matches {
            bail!("volatile FRR configuration changed after it was activated");
        }
        if file_matches {
            atomic::durable_remove(&self.paths.generated_frr)?;
        }
        clear_runtime_frr_marker(&self.paths)?;
        remove_empty_dir(
            self.paths
                .generated_frr
                .parent()
                .context("generated FRR configuration has no parent")?,
            false,
        )
    }

    fn restore_legacy_frr(&self) -> Result<()> {
        let mut ownership = OwnershipDb::load(&self.paths)?;
        let Some(mut record) = ownership.frr.clone() else {
            return Ok(());
        };
        let pending_path = self.paths.managed.join("frr.pending.conf");
        let current = read_frr_file(&self.paths.frr_config)?;

        // Reload completed and the retired record was made durable before the
        // backup/journal cleanup. This state is deliberately independent of
        // the backup file, which may already have been durably removed.
        if record.phase == Phase::Retired {
            let restored = match (record.origin.as_str(), current.as_ref()) {
                ("absent", None) => true,
                ("backup", Some(current)) => {
                    record.original_sha256.as_deref() == Some(current.sha256.as_str())
                }
                ("absent" | "backup", _) => false,
                (value, _) => bail!("unsupported FRR origin: {value}"),
            };
            if !restored {
                bail!("retired FRR restore target changed before cleanup");
            }
            cleanup_frr_files(&self.paths, &record, &pending_path)?;
            ownership.frr = None;
            return ownership.save(&self.paths);
        }

        ensure_frr_backup(&self.paths, &record)?;

        if record.phase == Phase::Creating {
            let origin_unchanged = verify_frr_origin(&record, current.as_ref()).is_ok();
            if origin_unchanged {
                cleanup_frr_files(&self.paths, &record, &pending_path)?;
                ownership.frr = None;
                return ownership.save(&self.paths);
            }
        }

        if record.phase != Phase::Deleting {
            let current = current.context("managed FRR configuration disappeared")?;
            let managed = record
                .pending_sha256
                .as_deref()
                .is_some_and(|hash| hash == current.sha256)
                || record
                    .active_sha256
                    .as_deref()
                    .is_some_and(|hash| hash == current.sha256);
            if !managed || !frr_has_owner(&current.bytes) {
                bail!("refusing to restore FRR over administrator changes");
            }
            record.phase = Phase::Deleting;
            record.active_sha256 = Some(current.sha256);
            record.pending_sha256 = record.original_sha256.clone();
            ownership.frr = Some(record.clone());
            ownership.save(&self.paths)?;
        }

        let target = match record.origin.as_str() {
            "backup" => {
                let path = record
                    .backup
                    .as_ref()
                    .context("FRR backup path is missing")?;
                let bytes = atomic::read_bounded(path, MAX_FRR_FILE_BYTES)
                    .context("FRR backup is missing")?;
                if Some(sha256(&bytes).as_str()) != record.original_sha256.as_deref() {
                    bail!("FRR backup hash changed");
                }
                Some(bytes)
            }
            "absent" => None,
            value => bail!("unsupported FRR origin: {value}"),
        };
        let target_hash = target.as_deref().map(sha256);
        if target_hash != record.pending_sha256 {
            bail!("FRR restore target does not match its journal");
        }
        let current = read_frr_file(&self.paths.frr_config)?;
        let restored = match (&current, &target_hash) {
            (None, None) => true,
            (Some(current), Some(hash)) => current.sha256 == *hash,
            _ => false,
        };
        if !restored {
            let current = current.context("FRR configuration disappeared during restore")?;
            let managed = record
                .active_sha256
                .as_deref()
                .is_some_and(|hash| hash == current.sha256);
            if !managed || !frr_has_owner(&current.bytes) {
                bail!("FRR configuration changed during restore");
            }
            write_frr_reload_marker(
                &self.paths,
                "restore",
                target_hash.as_deref().unwrap_or("absent"),
            )?;
            if let Some(bytes) = &target {
                atomic::atomic_write(
                    &self.paths.frr_config,
                    bytes,
                    record.original_mode.unwrap_or(0o640),
                )?;
            } else if self.paths.frr_config.is_file() {
                atomic::durable_remove(&self.paths.frr_config)?;
            }
        } else {
            write_frr_reload_marker(
                &self.paths,
                "restore",
                target_hash.as_deref().unwrap_or("absent"),
            )?;
        }
        if target.is_some() {
            // As with takeover, metadata repair must also run when the target
            // bytes were already published by an interrupted prior attempt.
            apply_frr_metadata(
                &self.paths.frr_config,
                record.original_mode.unwrap_or(0o640),
                record.original_uid,
                record.original_gid,
            )?;
        }
        self.reload_frr()?;
        record.phase = Phase::Retired;
        ownership.frr = Some(record.clone());
        ownership.save(&self.paths)?;
        cleanup_frr_files(&self.paths, &record, &pending_path)?;
        ownership.frr = None;
        ownership.save(&self.paths)
    }

    fn reload_frr(&self) -> Result<()> {
        if Path::new("/etc/init.d/frr").is_file() || self.paths.root.is_some() {
            self.runner
                .status("/etc/init.d/frr", ["reload"])
                .or_else(|_| self.runner.status("/etc/init.d/frr", ["restart"]))?;
        }
        Ok(())
    }

    fn restart_frr_baseline(&self) -> Result<()> {
        if Path::new("/etc/init.d/frr").is_file() || self.paths.root.is_some() {
            self.runner.status("/etc/init.d/frr", ["restart"])?;
        }
        Ok(())
    }

    fn frr_running(&self) -> bool {
        self.runner
            .output("/etc/init.d/frr", ["running"])
            .is_ok_and(|output| output.status.success())
    }

    fn ensure_frr_running(&self) -> Result<()> {
        self.restore_legacy_frr()?;
        let Some(marker) = read_runtime_frr_marker(&self.paths)? else {
            return Ok(());
        };
        if !runtime_frr_file_matches(&self.paths, &marker.sha256)? {
            bail!("volatile FRR configuration no longer matches its runtime marker");
        }
        if marker.phase == RuntimeFrrPhase::Owned && self.frr_running() {
            Ok(())
        } else {
            tracing::warn!("managed FRR runtime is incomplete; attempting recovery");
            self.restart_frr_baseline()?;
            let path = self.paths.generated_frr.to_string_lossy().into_owned();
            self.runner.status("vtysh", ["-b", "-f", path.as_str()])?;
            write_runtime_frr_marker(
                &self.paths,
                &RuntimeFrrMarker {
                    phase: RuntimeFrrPhase::Owned,
                    sha256: marker.sha256,
                },
            )
        }
    }
}

#[derive(Clone, Debug)]
struct FrrFileState {
    bytes: Vec<u8>,
    sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeFrrPhase {
    Applying,
    Owned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeFrrMarker {
    phase: RuntimeFrrPhase,
    sha256: String,
}

fn runtime_frr_marker(paths: &Paths) -> std::path::PathBuf {
    paths.runtime.join("frr.runtime")
}

fn read_runtime_frr_marker(paths: &Paths) -> Result<Option<RuntimeFrrMarker>> {
    let path = runtime_frr_marker(paths);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("invalid volatile FRR runtime marker")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let value = atomic::read_string_bounded(&path, MAX_SMALL_MARKER_BYTES)?;
    let line = value
        .strip_suffix('\n')
        .context("volatile FRR runtime marker has no final newline")?;
    if line.contains('\n') || line.contains('\r') {
        bail!("volatile FRR runtime marker has extra lines");
    }
    let mut fields = line.split('\t');
    if fields.next() != Some("v1") {
        bail!("unsupported volatile FRR runtime marker");
    }
    let phase = match fields.next() {
        Some("applying") => RuntimeFrrPhase::Applying,
        Some("owned") => RuntimeFrrPhase::Owned,
        _ => bail!("invalid volatile FRR runtime phase"),
    };
    let sha256 = fields
        .next()
        .context("volatile FRR runtime marker has no hash")?;
    if fields.next().is_some()
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid volatile FRR runtime hash");
    }
    Ok(Some(RuntimeFrrMarker {
        phase,
        sha256: sha256.to_ascii_lowercase(),
    }))
}

fn write_runtime_frr_marker(paths: &Paths, marker: &RuntimeFrrMarker) -> Result<()> {
    let phase = match marker.phase {
        RuntimeFrrPhase::Applying => "applying",
        RuntimeFrrPhase::Owned => "owned",
    };
    let value = format!("v1\t{phase}\t{}\n", marker.sha256);
    atomic::atomic_write(&runtime_frr_marker(paths), value.as_bytes(), 0o600)?;
    Ok(())
}

fn clear_runtime_frr_marker(paths: &Paths) -> Result<()> {
    let path = runtime_frr_marker(paths);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("invalid volatile FRR runtime marker")
        }
        Ok(_) => atomic::durable_remove(&path).map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic::sync_dir(&paths.runtime)
        }
        Err(error) => Err(error.into()),
    }
}

fn runtime_frr_file_matches(paths: &Paths, expected_hash: &str) -> Result<bool> {
    Ok(read_frr_file(&paths.generated_frr)?
        .is_some_and(|file| frr_has_owner(&file.bytes) && file.sha256 == expected_hash))
}

fn read_frr_file(path: &Path) -> Result<Option<FrrFileState>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("FRR configuration is not a regular file");
    }
    let bytes = atomic::read_bounded(path, MAX_FRR_FILE_BYTES)?;
    Ok(Some(FrrFileState {
        sha256: sha256(&bytes),
        bytes,
    }))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn frr_has_owner(bytes: &[u8]) -> bool {
    let expected = format!("! meduza-owner: {OWNER}");
    bytes.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line == expected.as_bytes()
    })
}

fn verify_frr_origin(record: &FrrRecord, current: Option<&FrrFileState>) -> Result<()> {
    match record.origin.as_str() {
        "backup" => {
            let current = current.context("original FRR configuration disappeared")?;
            if record.original_sha256.as_deref() != Some(current.sha256.as_str()) {
                bail!("FRR configuration changed during takeover");
            }
        }
        "absent" if current.is_none() => {}
        "absent" => bail!("FRR configuration appeared during takeover"),
        value => bail!("unsupported FRR origin: {value}"),
    }
    Ok(())
}

fn ensure_frr_backup(paths: &Paths, record: &FrrRecord) -> Result<()> {
    match record.origin.as_str() {
        "backup" => {
            let backup = record
                .backup
                .as_ref()
                .context("FRR backup path is missing")?;
            atomic::reject_symlink(backup)?;
            if backup.is_file() {
                let bytes = atomic::read_bounded(backup, MAX_FRR_FILE_BYTES)?;
                if record.original_sha256.as_deref() != Some(sha256(&bytes).as_str()) {
                    bail!("FRR backup hash changed");
                }
                return Ok(());
            }
            let current = read_frr_file(&paths.frr_config)?;
            verify_frr_origin(record, current.as_ref())?;
            let current = current.expect("backup origin checked above");
            atomic::atomic_write(backup, &current.bytes, 0o600)?;
        }
        "absent" => {
            if record.backup.is_some() {
                bail!("absent FRR origin unexpectedly has a backup path");
            }
        }
        value => bail!("unsupported FRR origin: {value}"),
    }
    Ok(())
}

fn frr_reload_marker(paths: &Paths) -> std::path::PathBuf {
    paths.managed.join("frr.reload.pending")
}

fn write_frr_reload_marker(paths: &Paths, action: &str, hash: &str) -> Result<()> {
    let value = format!("v1\t{action}\t{hash}\n");
    atomic::atomic_write(&frr_reload_marker(paths), value.as_bytes(), 0o600)?;
    Ok(())
}

fn clear_frr_reload_marker(paths: &Paths) -> Result<()> {
    let path = frr_reload_marker(paths);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("invalid FRR reload marker")
        }
        Ok(_) => {
            atomic::durable_remove(&path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic::sync_dir(&paths.managed)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn cleanup_frr_files(paths: &Paths, record: &FrrRecord, pending: &Path) -> Result<()> {
    let mut files = record
        .backup
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect::<Vec<_>>();
    files.push(pending);
    for path in files {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("invalid FRR transaction file: {}", path.display())
            }
            Ok(_) => {
                atomic::durable_remove(path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    atomic::sync_dir(parent)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    clear_frr_reload_marker(paths)
}

fn apply_frr_metadata(path: &Path, mode: u32, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        if uid.is_some() || gid.is_some() {
            std::os::unix::fs::chown(path, uid, gid)?;
        }
        File::open(path)?.sync_all()?;
        if let Some(parent) = path.parent() {
            atomic::sync_dir(parent)?;
        }
    }
    #[cfg(not(unix))]
    let _ = (path, mode, uid, gid);
    Ok(())
}

fn snapshot_to_flat(snapshot: &Snapshot) -> FlatSnapshot {
    FlatSnapshot {
        version: snapshot.version,
        node_id: snapshot.node_id.clone(),
        commit: snapshot.commit.clone(),
        applied_at: (!snapshot.applied_at.is_empty()).then(|| snapshot.applied_at.clone()),
        node: snapshot.node.clone(),
        global: snapshot.global.clone(),
        all_nodes: snapshot.all_nodes.clone(),
    }
}

fn merge_inventory(left: &[ManifestEntry], right: &[ManifestEntry]) -> Vec<ManifestEntry> {
    left.iter()
        .chain(right)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn inventory_with_generated(
    paths: &Paths,
    mut entries: Vec<ManifestEntry>,
) -> Result<Vec<ManifestEntry>> {
    let ownership = OwnershipDb::load(paths)?;
    for record in ownership.generated.values() {
        entries.push(record.entry.clone());
        if let Some(target) = &record.pending_entry {
            entries.push(target.clone());
        }
    }
    Ok(merge_inventory(&entries, &[]))
}

fn object_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn meduza_state_exists(paths: &Paths) -> Result<bool> {
    let known = [
        paths.cache.as_path(),
        paths.cache_pending.as_path(),
        paths.manifest.as_path(),
        paths.pending_manifest.as_path(),
        paths.ownership.as_path(),
        paths.firewall_state.as_path(),
        paths.reported.as_path(),
        paths.generated.as_path(),
        paths.managed.as_path(),
        paths.runtime.as_path(),
        paths.ip_forward_marker.as_path(),
    ];
    for path in known {
        if object_exists(path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_known_file(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("cleanup path has no parent: {}", path.display());
    };
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "cleanup parent is not a real directory: {}",
                parent.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    atomic::cleanup_atomic_temps(path)?;
    atomic::durable_remove(path)?;
    Ok(())
}

fn remove_empty_dir(path: &Path, require_empty: bool) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && parent.is_dir()
            {
                atomic::sync_dir(parent)?;
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("cleanup path is not a real directory: {}", path.display());
    }
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        if require_empty {
            bail!("owned cleanup directory is not empty: {}", path.display());
        }
        return Ok(());
    }
    fs::remove_dir(path)?;
    if let Some(parent) = path.parent() {
        atomic::sync_dir(parent)?;
    }
    Ok(())
}

fn write_rendered_files(
    files: &[RenderedFile],
    entries: &[ManifestEntry],
) -> Result<BTreeSet<String>> {
    let mut changed = BTreeSet::new();
    for file in files {
        let parent = file.path.parent().context("rendered file has no parent")?;
        let owned = entries.iter().any(|entry| {
            entry
                .config
                .parent()
                .is_some_and(|directory| file.path.starts_with(directory))
        });
        if !owned {
            bail!(
                "rendered VPN file escaped every owned generated directory: {}",
                file.path.display()
            );
        }
        // Renderers can create nested trees such as tinc/<network>/hosts.
        // `atomic_write` intentionally refuses to invent parents, so create
        // and fsync those private, Meduza-owned directories explicitly.
        atomic::ensure_private_dir(parent, 0o700)?;
        if atomic::atomic_write(&file.path, &file.contents, file.mode)? {
            for entry in entries {
                let directory = entry.config.parent().context("config has no parent")?;
                if file.path.starts_with(directory) {
                    changed.insert(entry.logical.clone());
                    break;
                }
            }
        }
    }
    Ok(changed)
}

fn validate_rendered_vpn_size(files: &[RenderedFile]) -> Result<()> {
    let mut total = 0usize;
    for file in files {
        if file.contents.len() > MAX_GENERATED_FILE_BYTES {
            bail!(
                "generated VPN file exceeds {MAX_GENERATED_FILE_BYTES} byte limit: {}",
                file.path.display()
            );
        }
        total = total
            .checked_add(file.contents.len())
            .context("generated VPN size overflow")?;
        if total > MAX_GENERATED_TOTAL_BYTES {
            bail!("generated VPN files exceed {MAX_GENERATED_TOTAL_BYTES} byte aggregate limit");
        }
    }
    Ok(())
}

fn ensure_ip_forward(paths: &Paths) -> Result<()> {
    let value = atomic::read_string_bounded(&paths.ip_forward, MAX_SMALL_MARKER_BYTES)
        .with_context(|| format!("could not read {}", paths.ip_forward.display()))?;
    match value.trim() {
        "1" => Ok(()),
        "0" => {
            atomic::atomic_write(&paths.ip_forward_marker, b"0\n", 0o600)?;
            fs::write(&paths.ip_forward, b"1\n")
                .with_context(|| format!("could not enable {}", paths.ip_forward.display()))
        }
        value => bail!("unexpected net.ipv4.ip_forward value: {value}"),
    }
}

fn restore_ip_forward(paths: &Paths) -> Result<()> {
    let metadata = match fs::symlink_metadata(&paths.ip_forward_marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("invalid IPv4 forwarding ownership marker");
    }
    if atomic::read_bounded(&paths.ip_forward_marker, MAX_SMALL_MARKER_BYTES)? != b"0\n" {
        bail!("invalid IPv4 forwarding ownership marker contents");
    }
    let current = atomic::read_string_bounded(&paths.ip_forward, MAX_SMALL_MARKER_BYTES)?;
    match current.trim() {
        "1" => fs::write(&paths.ip_forward, b"0\n")?,
        "0" => {}
        value => bail!("unexpected net.ipv4.ip_forward value: {value}"),
    }
    atomic::durable_remove(&paths.ip_forward_marker)?;
    Ok(())
}

pub fn doctor<R: Runner>(paths: &Paths, runner: &R) -> Result<()> {
    let settings = Settings::load(runner)?;
    let _lock = ApplyLock::acquire(paths)?;
    paths.prepare()?;
    let mut missing = Vec::new();
    for command in ["uci", "ip"] {
        if !command_exists(command) {
            missing.push(command);
        }
    }
    if !missing.is_empty() {
        bail!("missing core OpenWrt commands: {}", missing.join(", "));
    }
    if settings
        .endpoints
        .iter()
        .any(|endpoint| endpoint.starts_with("https://"))
        && settings.ca.as_ref().is_some_and(|ca| !ca.is_file())
    {
        bail!("configured etcd CA does not exist");
    }
    let _ = OwnershipDb::load(paths)?;
    Firewall::new(paths.clone(), runner.clone())
        .validate_zone(settings.firewall_zone.as_deref())?;
    let _ = read_manifest(&paths.manifest)?;
    println!("meduza-openwrt doctor: ok (node {})", settings.node_id);
    Ok(())
}

struct ApplyLock {
    #[cfg(unix)]
    _socket: std::os::fd::OwnedFd,
    #[cfg(not(unix))]
    _file: File,
}

impl ApplyLock {
    #[cfg(unix)]
    fn acquire(_paths: &Paths) -> Result<Self> {
        use std::mem::{offset_of, zeroed};
        use std::os::fd::FromRawFd;

        // A Linux abstract Unix socket is an atomic, crash-clean transaction
        // lock. Unlike flocking a pathname, it has no rename/unlink inode race
        // and leaves no file behind after purge or power loss.
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error()).context("could not create apply lock");
        }
        let mut address: libc::sockaddr_un = unsafe { zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let name = b"meduza-openwrt-transaction-v1";
        if name.len() + 1 > address.sun_path.len() {
            unsafe { libc::close(raw) };
            bail!("apply lock name is too long");
        }
        for (index, byte) in name.iter().enumerate() {
            address.sun_path[index + 1] = *byte as libc::c_char;
        }
        let length = offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len();
        let status = unsafe {
            libc::bind(
                raw,
                (&raw const address).cast::<libc::sockaddr>(),
                length as libc::socklen_t,
            )
        };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(raw) };
            if error.raw_os_error() == Some(libc::EADDRINUSE) {
                bail!("another Meduza transaction is already active");
            }
            return Err(error).context("could not bind apply lock");
        }
        // SAFETY: `raw` is a unique live descriptor returned by socket(), and
        // ownership is transferred exactly once to OwnedFd.
        let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        Ok(Self { _socket: socket })
    }

    #[cfg(not(unix))]
    fn acquire(paths: &Paths) -> Result<Self> {
        use std::fs::OpenOptions;

        let parent = paths.lock.parent().context("lock has no parent")?;
        atomic::ensure_dir(parent, 0o700)?;
        atomic::reject_symlink(&paths.lock)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_union_is_sorted_and_deduplicated() {
        let row = ManifestEntry::parse_tsv(
            "tinc\tmesh\ttinc_mesh\ttnc0\t/var/run/meduza/generated/tinc/mesh/tinc.conf",
        )
        .unwrap();
        assert_eq!(
            merge_inventory(std::slice::from_ref(&row), std::slice::from_ref(&row)).len(),
            1
        );
    }

    #[test]
    fn rendered_tinc_hosts_create_private_nested_parents() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("generated/tinc/mesh");
        atomic::ensure_private_dir(&directory, 0o700).unwrap();
        let entry = ManifestEntry::parse_tsv(&format!(
            "tinc\tmesh\ttinc_mesh\ttnc0\t{}",
            directory.join("tinc.conf").display()
        ))
        .unwrap();
        let host = RenderedFile {
            path: directory.join("hosts/router-01"),
            mode: 0o600,
            contents: b"Subnet = 10.0.0.1/32\n".to_vec(),
        };

        let changed = write_rendered_files(std::slice::from_ref(&host), &[entry]).unwrap();
        assert!(changed.contains("tinc_mesh"));
        assert_eq!(fs::read(host.path).unwrap(), host.contents);
    }

    #[test]
    fn oversized_generated_file_is_rejected_before_writing() {
        let file = RenderedFile {
            path: "/var/run/meduza/generated/tinc/mesh/tinc.conf".into(),
            mode: 0o600,
            contents: vec![0; MAX_GENERATED_FILE_BYTES + 1],
        };
        assert!(validate_rendered_vpn_size(&[file]).is_err());
    }

    #[test]
    fn frr_reader_rejects_an_oversized_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("frr.conf");
        let file = fs::File::create(&path).unwrap();
        file.set_len((MAX_FRR_FILE_BYTES + 1) as u64).unwrap();

        assert!(read_frr_file(&path).is_err());
    }

    #[test]
    fn volatile_frr_marker_round_trips_below_var_run() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        atomic::ensure_private_dir(&paths.runtime, 0o700).unwrap();
        let marker = RuntimeFrrMarker {
            phase: RuntimeFrrPhase::Applying,
            sha256: "a".repeat(64),
        };

        write_runtime_frr_marker(&paths, &marker).unwrap();

        assert_eq!(read_runtime_frr_marker(&paths).unwrap(), Some(marker));
        assert!(runtime_frr_marker(&paths).starts_with(&paths.runtime));
        assert!(!runtime_frr_marker(&paths).starts_with(&paths.data));
    }

    #[test]
    fn volatile_frr_marker_rejects_extra_lines() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        atomic::ensure_private_dir(&paths.runtime, 0o700).unwrap();
        fs::write(
            runtime_frr_marker(&paths),
            format!("v1\towned\t{}\nforeign\n", "b".repeat(64)),
        )
        .unwrap();

        assert!(read_runtime_frr_marker(&paths).is_err());
    }

    #[test]
    fn purge_directory_cleanup_removes_empty_state_root_but_preserves_pki() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        fs::create_dir_all(&paths.managed).unwrap();
        fs::create_dir_all(paths.data.join("pki")).unwrap();
        fs::write(paths.data.join("pki/client.key"), b"operator-secret").unwrap();

        remove_empty_dir(&paths.managed, false).unwrap();
        remove_empty_dir(&paths.state, false).unwrap();
        remove_empty_dir(&paths.data, false).unwrap();

        assert!(!paths.state.exists());
        assert_eq!(
            fs::read(paths.data.join("pki/client.key")).unwrap(),
            b"operator-secret"
        );
    }
}

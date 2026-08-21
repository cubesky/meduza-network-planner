use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::OWNER;
use crate::atomic;
use crate::state::{ManifestEntry, Paths};

const MAX_OWNERSHIP_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_OWNER_MARKER_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OwnershipDb {
    #[serde(default = "ownership_version")]
    pub version: u32,
    #[serde(default)]
    pub generated: BTreeMap<String, GeneratedRecord>,
    #[serde(default)]
    pub wireguard_stages: BTreeMap<String, WireguardStageRecord>,
    #[serde(default)]
    pub frr: Option<FrrRecord>,
}

const fn ownership_version() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Creating,
    Owned,
    Updating,
    Deleting,
    Retired,
    Borrowed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GeneratedRecord {
    pub nonce: String,
    pub phase: Phase,
    pub entry: ManifestEntry,
    pub directory: PathBuf,
    #[serde(default)]
    pub tombstone: Option<PathBuf>,
    /// Target identity for a crash-replayable marker rekey.  `entry` always
    /// describes the old marker until the replacement marker is durable.
    #[serde(default)]
    pub pending_entry: Option<ManifestEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireguardStageRecord {
    pub nonce: String,
    pub phase: Phase,
    pub entry: ManifestEntry,
    pub device: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrrRecord {
    pub phase: Phase,
    pub origin: String,
    #[serde(default)]
    pub original_sha256: Option<String>,
    #[serde(default)]
    pub backup: Option<PathBuf>,
    #[serde(default)]
    pub active_sha256: Option<String>,
    #[serde(default)]
    pub pending_sha256: Option<String>,
    #[serde(default)]
    pub original_mode: Option<u32>,
    #[serde(default)]
    pub original_uid: Option<u32>,
    #[serde(default)]
    pub original_gid: Option<u32>,
    #[serde(default)]
    pub managed_mode: Option<u32>,
    #[serde(default)]
    pub managed_uid: Option<u32>,
    #[serde(default)]
    pub managed_gid: Option<u32>,
}

impl OwnershipDb {
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::symlink_metadata(&paths.ownership) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("persistent ownership database is not a regular file")
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: 1,
                    ..Self::default()
                });
            }
            Err(error) => return Err(error.into()),
        }
        let bytes = atomic::read_bounded(&paths.ownership, MAX_OWNERSHIP_FILE_BYTES)
            .context("could not read persistent ownership database")?;
        let value: Self =
            serde_json::from_slice(&bytes).context("invalid persistent ownership database")?;
        if value.version != 1 {
            bail!("unsupported ownership database version {}", value.version);
        }
        value.validate(paths)?;
        Ok(value)
    }

    fn validate(&self, paths: &Paths) -> Result<()> {
        for (key, record) in &self.generated {
            validate_nonce(&record.nonce, "generated nonce")?;
            validate_generated_entry(paths, &record.entry)?;
            if key != &Self::generated_key(&record.entry) {
                bail!("generated ownership key does not match its entry");
            }
            let expected_directory = record
                .entry
                .config
                .parent()
                .context("generated entry has no parent")?;
            if record.directory != expected_directory {
                bail!("generated ownership directory does not match its entry");
            }
            match record.phase {
                Phase::Updating => {
                    let target = record
                        .pending_entry
                        .as_ref()
                        .context("generated update has no pending identity")?;
                    validate_generated_entry(paths, target)?;
                    if Self::generated_key(target) != *key
                        || target.config.parent() != Some(record.directory.as_path())
                    {
                        bail!("generated pending identity changed its resource key");
                    }
                }
                _ if record.pending_entry.is_some() => {
                    bail!("generated pending identity exists outside update phase")
                }
                _ => {}
            }
            match record.phase {
                Phase::Deleting => {
                    let tombstone = record
                        .tombstone
                        .as_ref()
                        .context("generated delete has no tombstone")?;
                    if tombstone != &deletion_tombstone(record, key)? {
                        bail!("generated deletion tombstone identity changed");
                    }
                }
                _ if record.tombstone.is_some() => {
                    bail!("generated tombstone exists outside delete phase")
                }
                Phase::Retired | Phase::Borrowed => {
                    bail!("invalid generated ownership phase")
                }
                _ => {}
            }
        }

        for (key, record) in &self.wireguard_stages {
            validate_nonce(&record.nonce, "WireGuard staging nonce")?;
            validate_generated_entry(paths, &record.entry)?;
            if record.entry.kind != crate::state::InterfaceKind::Wireguard
                || record.phase != Phase::Creating
                || key != &Self::generated_key(&record.entry)
                || record.device != wireguard_stage_name(&record.nonce)
            {
                bail!("invalid WireGuard staging ownership identity");
            }
            crate::model::validate_device(&record.device)?;
            let generated = self
                .generated
                .get(key)
                .context("WireGuard staging record has no generated ownership")?;
            if generated.entry != record.entry || generated.phase != Phase::Owned {
                bail!("WireGuard staging generation does not match generated ownership");
            }
        }

        if let Some(record) = &self.frr {
            if !matches!(record.origin.as_str(), "backup" | "absent")
                || matches!(record.phase, Phase::Borrowed)
            {
                bail!("invalid FRR ownership record");
            }
            validate_optional_hash(record.original_sha256.as_deref(), "FRR original hash")?;
            validate_optional_hash(record.active_sha256.as_deref(), "FRR active hash")?;
            validate_optional_hash(record.pending_sha256.as_deref(), "FRR pending hash")?;
            match record.origin.as_str() {
                "backup"
                    if record.backup.as_ref() != Some(&paths.managed.join("frr.conf.backup")) =>
                {
                    bail!("FRR backup path changed")
                }
                "absent" if record.backup.is_some() || record.original_sha256.is_some() => {
                    bail!("absent FRR origin unexpectedly has a backup")
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.validate(paths)?;
        atomic::atomic_json_bounded(&paths.ownership, self, MAX_OWNERSHIP_FILE_BYTES)?;
        Ok(())
    }

    pub fn generated_key(entry: &ManifestEntry) -> String {
        format!("{}:{}", entry.kind.as_str(), entry.instance)
    }

    /// Return whether the external generation record and its non-symlink
    /// marker authorize this exact generated identity. Transitional before and
    /// after images are accepted only when the marker matches that image.
    pub fn authorizes_generated(&self, entry: &ManifestEntry) -> Result<bool> {
        let key = Self::generated_key(entry);
        let Some(record) = self.generated.get(&key) else {
            return Ok(false);
        };
        match record.phase {
            Phase::Owned | Phase::Creating => {
                if record.entry != *entry || !object_exists(&record.directory)? {
                    return Ok(false);
                }
                Ok(verify_generated_marker(&record.directory, record).is_ok())
            }
            Phase::Updating => {
                let marker = record.directory.join(".meduza-owner");
                let metadata = match fs::symlink_metadata(&marker) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error.into()),
                };
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Ok(false);
                }
                let current = atomic::read_bounded(&marker, MAX_OWNER_MARKER_BYTES)?;
                if record.entry == *entry && current == marker_bytes(record)? {
                    return Ok(true);
                }
                Ok(record.pending_entry.as_ref() == Some(entry)
                    && current == marker_bytes_for_entry(record, entry)?)
            }
            Phase::Deleting => {
                if record.entry != *entry {
                    return Ok(false);
                }
                if object_exists(&record.directory)? {
                    return Ok(verify_generated_marker(&record.directory, record).is_ok());
                }
                let Some(tombstone) = record.tombstone.as_ref() else {
                    return Ok(false);
                };
                match fs::symlink_metadata(tombstone.join(".meduza-owner")) {
                    Ok(_) => Ok(verify_generated_marker(tombstone, record).is_ok()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        if !object_exists(tombstone)? {
                            return Ok(false);
                        }
                        verify_real_directory(tombstone)?;
                        Ok(fs::read_dir(tombstone)?.next().transpose()?.is_none())
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Phase::Retired | Phase::Borrowed => Ok(false),
        }
    }

    /// Authorize the generated backing resource even when a same-instance
    /// identity is being rekeyed. Daemon argv contains the config path, not the
    /// logical/device fields, so either exact marker image may safely authorize
    /// stopping that old process while a rollback or update converges.
    pub fn authorizes_generated_resource(&self, entry: &ManifestEntry) -> Result<bool> {
        let key = Self::generated_key(entry);
        let Some(record) = self.generated.get(&key) else {
            return Ok(false);
        };
        let path_matches = record.entry.config == entry.config
            || record
                .pending_entry
                .as_ref()
                .is_some_and(|target| target.config == entry.config);
        if !path_matches {
            return Ok(false);
        }
        match record.phase {
            Phase::Owned | Phase::Creating => {
                if !object_exists(&record.directory)? {
                    return Ok(false);
                }
                Ok(verify_generated_marker(&record.directory, record).is_ok())
            }
            Phase::Updating => {
                let marker = record.directory.join(".meduza-owner");
                let metadata = match fs::symlink_metadata(&marker) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error.into()),
                };
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Ok(false);
                }
                let current = atomic::read_bounded(&marker, MAX_OWNER_MARKER_BYTES)?;
                Ok(current == marker_bytes(record)?
                    || record.pending_entry.as_ref().is_some_and(|target| {
                        marker_bytes_for_entry(record, target).is_ok_and(|value| value == current)
                    }))
            }
            Phase::Deleting => self.authorizes_generated(&record.entry),
            Phase::Retired | Phase::Borrowed => Ok(false),
        }
    }

    pub fn ensure_generated(&mut self, paths: &Paths, entry: &ManifestEntry) -> Result<PathBuf> {
        let directory = entry
            .config
            .parent()
            .context("generated config has no parent")?
            .to_path_buf();
        if !directory.starts_with(&paths.generated) {
            bail!("generated path escaped {}", paths.generated.display());
        }
        let key = Self::generated_key(entry);
        if let Some(record) = self.generated.get(&key).cloned() {
            if record.directory != directory {
                bail!("generated ownership identity changed for {key}");
            }
            match record.phase {
                Phase::Owned => {
                    verify_generated_marker(&directory, &record)?;
                    if record.entry == *entry {
                        return Ok(directory);
                    }
                    self.generated.get_mut(&key).expect("record exists").phase = Phase::Updating;
                    self.generated
                        .get_mut(&key)
                        .expect("record exists")
                        .pending_entry = Some(entry.clone());
                    self.save(paths)?;
                    return self.finish_generated_rekey(paths, &key);
                }
                Phase::Updating => {
                    // First complete the already durable transition, then
                    // converge to the identity requested by this reconcile.
                    // This is required when boot recovery rolls an interrupted
                    // candidate back to an older LKG entry.
                    self.finish_generated_rekey(paths, &key)?;
                    return self.ensure_generated(paths, entry);
                }
                Phase::Creating => {
                    self.finish_generated_creation(paths, key.clone())?;
                    return self.ensure_generated(paths, entry);
                }
                Phase::Deleting => {
                    self.finish_generated_deletion(paths, &key)?;
                    return self.ensure_generated(paths, entry);
                }
                _ => bail!("invalid generated phase for {key}"),
            }
        }

        if fs::symlink_metadata(&directory).is_ok() {
            bail!(
                "refusing to adopt existing generated directory: {}",
                directory.display()
            );
        }
        let nonce = atomic::random_nonce();
        self.generated.insert(
            key.clone(),
            GeneratedRecord {
                nonce,
                phase: Phase::Creating,
                entry: entry.clone(),
                directory,
                tombstone: None,
                pending_entry: None,
            },
        );
        self.save(paths)?;
        self.finish_generated_creation(paths, key)
    }

    fn finish_generated_creation(&mut self, paths: &Paths, key: String) -> Result<PathBuf> {
        let record = self
            .generated
            .get(&key)
            .cloned()
            .context("missing create record")?;
        let parent = record
            .directory
            .parent()
            .context("generated directory has no parent")?;
        atomic::ensure_private_dir(parent, 0o700)?;
        let stage = parent.join(format!(
            ".meduza-create-{}-{}",
            sanitize_filename(&key),
            record.nonce
        ));

        if object_exists(&record.directory)? {
            if object_exists(&stage)? {
                bail!("both generated stage and final directory exist");
            }
            verify_generated_marker(&record.directory, &record)?;
        } else {
            if !object_exists(&stage)? {
                fs::create_dir(&stage)?;
                set_private_dir(&stage)?;
                atomic::sync_dir(&stage)?;
                atomic::sync_dir(parent)?;
            }
            finish_creation_stage(&stage, &record)?;
            atomic::durable_rename(&stage, &record.directory)?;
        }

        self.generated.get_mut(&key).expect("record exists").phase = Phase::Owned;
        self.save(paths)?;
        Ok(record.directory)
    }

    fn finish_generated_rekey(&mut self, paths: &Paths, key: &str) -> Result<PathBuf> {
        let record = self
            .generated
            .get(key)
            .cloned()
            .context("missing generated rekey record")?;
        if record.phase != Phase::Updating {
            bail!("generated record is not rekeying: {key}");
        }
        let target = record
            .pending_entry
            .clone()
            .context("generated rekey has no target identity")?;
        if target.config.parent() != Some(record.directory.as_path()) {
            bail!("generated rekey changed directory for {key}");
        }

        verify_real_directory(&record.directory)?;
        let marker = record.directory.join(".meduza-owner");
        let metadata =
            fs::symlink_metadata(&marker).context("generated rekey marker is missing")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("invalid generated rekey marker");
        }
        let current = atomic::read_bounded(&marker, MAX_OWNER_MARKER_BYTES)
            .context("generated rekey marker is missing")?;
        let before = marker_bytes(&record)?;
        let after = marker_bytes_for_entry(&record, &target)?;
        if current == before {
            atomic::atomic_write(&marker, &after, 0o600)?;
        } else if current != after {
            bail!("generated rekey marker conflicts with external state");
        }

        let live = self.generated.get_mut(key).expect("record exists");
        live.entry = target;
        live.pending_entry = None;
        live.phase = Phase::Owned;
        self.save(paths)?;
        Ok(record.directory)
    }

    pub fn remove_generated(&mut self, paths: &Paths, entry: &ManifestEntry) -> Result<()> {
        let key = Self::generated_key(entry);
        if self.wireguard_stages.contains_key(&key) {
            bail!("WireGuard staging transaction is still active: {key}");
        }
        let Some(mut record) = self.generated.get(&key).cloned() else {
            if let Some(directory) = entry.config.parent()
                && object_exists(directory)?
            {
                bail!("refusing to remove generated directory without external ownership: {key}");
            }
            return Ok(());
        };
        if record.phase == Phase::Creating {
            self.finish_generated_creation(paths, key.clone())?;
            record = self.generated.get(&key).cloned().expect("record exists");
        }
        if record.phase == Phase::Updating {
            self.finish_generated_rekey(paths, &key)?;
            record = self.generated.get(&key).cloned().expect("record exists");
        }
        if record.phase == Phase::Deleting {
            return self.finish_generated_deletion(paths, &key);
        }
        if record.entry != *entry {
            if entry.config.parent() == Some(record.directory.as_path()) {
                // A reconcile that rekeys a same-instance directory still has
                // the old manifest row in its stale inventory.  That row must
                // stop the old runtime, but it must not delete the newly
                // rekeyed directory.
                verify_generated_marker(&record.directory, &record)?;
                return Ok(());
            }
            bail!("generated cleanup identity changed for {key}");
        }
        if record.phase == Phase::Owned {
            verify_generated_marker(&record.directory, &record)?;
            let tombstone = deletion_tombstone(&record, &key)?;
            if object_exists(&tombstone)? {
                bail!("generated deletion tombstone already exists");
            }
            self.generated.get_mut(&key).expect("record exists").phase = Phase::Deleting;
            self.generated
                .get_mut(&key)
                .expect("record exists")
                .tombstone = Some(tombstone.clone());
            self.save(paths)?;
            atomic::durable_rename(&record.directory, &tombstone)?;
        }
        self.finish_generated_deletion(paths, &key)
    }

    fn finish_generated_deletion(&mut self, paths: &Paths, key: &str) -> Result<()> {
        let record = self
            .generated
            .get(key)
            .cloned()
            .context("missing delete record")?;
        if record.phase != Phase::Deleting {
            bail!("generated record is not deleting: {key}");
        }
        let tombstone = record
            .tombstone
            .as_ref()
            .context("delete record has no tombstone")?;
        if tombstone != &deletion_tombstone(&record, key)? {
            bail!("delete record has an unexpected tombstone path");
        }
        let tombstone_exists = object_exists(tombstone)?;
        let directory_exists = object_exists(&record.directory)?;
        if tombstone_exists && directory_exists {
            bail!("both generated directory and deletion tombstone exist");
        }
        if tombstone_exists {
            match fs::symlink_metadata(tombstone.join(".meduza-owner")) {
                Ok(_) => {
                    verify_generated_marker(tombstone, &record)?;
                    clear_directory_marker_last(tombstone)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // The marker is removed only after every other child has
                    // been durably removed.  Therefore an exact, empty,
                    // externally recorded tombstone is the one safe
                    // marker-less deletion state to replay.
                    remove_empty_tombstone(tombstone)?;
                }
                Err(error) => return Err(error.into()),
            }
        } else if directory_exists {
            verify_generated_marker(&record.directory, &record)?;
            atomic::durable_rename(&record.directory, tombstone)?;
            clear_directory_marker_last(tombstone)?;
        } else if let Some(parent) = tombstone.parent() {
            atomic::sync_dir(parent)?;
        }
        self.generated.remove(key);
        self.save(paths)
    }

    /// Remove regular files that are no longer part of the rendered file set
    /// for a directory owned by this database.  The root marker is retained.
    /// Symlinks and non-file/non-directory objects abort the whole preflight
    /// before any stale file is removed.
    pub fn prune_generated<'a>(
        &self,
        entry: &ManifestEntry,
        expected_files: impl IntoIterator<Item = &'a Path>,
    ) -> Result<usize> {
        let key = Self::generated_key(entry);
        let record = self
            .generated
            .get(&key)
            .with_context(|| format!("missing generated ownership for {key}"))?;
        if record.phase != Phase::Owned || record.entry != *entry {
            bail!("generated directory is not owned by the requested identity: {key}");
        }
        verify_generated_marker(&record.directory, record)?;

        let mut expected = BTreeSet::new();
        for path in expected_files {
            let relative = path.strip_prefix(&record.directory).with_context(|| {
                format!(
                    "expected generated file escaped {}: {}",
                    record.directory.display(),
                    path.display()
                )
            })?;
            if relative.as_os_str().is_empty()
                || relative == Path::new(".meduza-owner")
                || relative
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                bail!("invalid expected generated file: {}", path.display());
            }
            expected.insert(relative.to_path_buf());
        }

        let mut stale_files = Vec::new();
        let mut directories = Vec::new();
        collect_prune_candidates(
            &record.directory,
            &record.directory,
            &expected,
            &mut stale_files,
            &mut directories,
        )?;

        let mut removed = 0usize;
        for path in stale_files {
            atomic::durable_remove(&path)?;
            removed += 1;
        }
        for directory in directories {
            let relative = directory
                .strip_prefix(&record.directory)
                .expect("collected below owned root");
            if expected.iter().any(|path| path.starts_with(relative)) {
                continue;
            }
            if fs::read_dir(&directory)?.next().is_none() {
                fs::remove_dir(&directory)?;
                if let Some(parent) = directory.parent() {
                    atomic::sync_dir(parent)?;
                }
                removed += 1;
            }
        }
        verify_generated_marker(&record.directory, record)?;
        Ok(removed)
    }
}

pub fn wireguard_stage_name(nonce: &str) -> String {
    format!("mw{}", &nonce[..12])
}

fn validate_generated_entry(paths: &Paths, entry: &ManifestEntry) -> Result<()> {
    crate::model::validate_instance(&entry.instance)?;
    crate::model::validate_device(&entry.device)?;
    crate::config::validate_logical_name(&entry.logical)?;
    let kind = match entry.kind {
        crate::state::InterfaceKind::Tinc => crate::model::VpnKind::Tinc,
        crate::state::InterfaceKind::Openvpn => crate::model::VpnKind::OpenVpn,
        crate::state::InterfaceKind::Wireguard => crate::model::VpnKind::WireGuard,
    };
    if entry.logical != crate::model::logical_name(kind, &entry.instance)? {
        bail!("generated entry logical name does not match its identity");
    }
    let filename = match kind {
        crate::model::VpnKind::Tinc => "tinc.conf",
        crate::model::VpnKind::OpenVpn => "openvpn.conf",
        crate::model::VpnKind::WireGuard => "wg.conf",
    };
    let expected = paths
        .generated
        .join(kind.as_str())
        .join(&entry.instance)
        .join(filename);
    if entry.config != expected {
        bail!("generated entry config path changed");
    }
    Ok(())
}

fn validate_nonce(value: &str, description: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid {description}");
    }
    Ok(())
}

fn validate_optional_hash(value: Option<&str>, description: &str) -> Result<()> {
    if let Some(value) = value
        && (value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("invalid {description}");
    }
    Ok(())
}

fn marker_bytes(record: &GeneratedRecord) -> Result<Vec<u8>> {
    marker_bytes_for_entry(record, &record.entry)
}

fn marker_bytes_for_entry(record: &GeneratedRecord, entry: &ManifestEntry) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Marker<'a> {
        version: u32,
        owner: &'a str,
        nonce: &'a str,
        entry: &'a ManifestEntry,
    }
    Ok(serde_json::to_vec(&Marker {
        version: 1,
        owner: OWNER,
        nonce: &record.nonce,
        entry,
    })?)
}

fn object_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn verify_real_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("missing generated directory {}", directory.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "generated path is not a real directory: {}",
            directory.display()
        );
    }
    Ok(())
}

fn verify_generated_marker(directory: &Path, record: &GeneratedRecord) -> Result<()> {
    verify_real_directory(directory)?;
    let marker = directory.join(".meduza-owner");
    let metadata = fs::symlink_metadata(&marker)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("invalid generated ownership marker");
    }
    if atomic::read_bounded(&marker, MAX_OWNER_MARKER_BYTES)? != marker_bytes(record)? {
        bail!("generated ownership marker does not match external record");
    }
    Ok(())
}

fn finish_creation_stage(stage: &Path, record: &GeneratedRecord) -> Result<()> {
    verify_real_directory(stage)?;
    set_private_dir(stage)?;
    let marker = stage.join(".meduza-owner");
    let mut marker_present = false;
    for child in fs::read_dir(stage)? {
        let child = child?;
        let name = child.file_name();
        let name = name.to_str().context("non-UTF-8 creation-stage entry")?;
        if name == ".meduza-owner" {
            marker_present = true;
            continue;
        }
        // atomic_write may have been interrupted after creating its private
        // temporary file but before publishing the marker.  Only its exact,
        // nonce-shaped temporary names are safe to remove here.
        let suffix = name.strip_prefix("..meduza-owner.meduza-");
        let metadata = fs::symlink_metadata(child.path())?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || !suffix.is_some_and(|value| {
                value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            bail!(
                "refusing unexpected object in generated creation stage: {}",
                child.path().display()
            );
        }
        atomic::durable_remove(&child.path())?;
    }

    if marker_present {
        verify_generated_marker(stage, record)?;
    } else {
        atomic::atomic_write(&marker, &marker_bytes(record)?, 0o600)?;
    }
    verify_generated_marker(stage, record)
}

fn deletion_tombstone(record: &GeneratedRecord, key: &str) -> Result<PathBuf> {
    let parent = record
        .directory
        .parent()
        .context("generated directory has no parent")?;
    Ok(parent.join(format!(
        ".meduza-delete-{}-{}",
        sanitize_filename(key),
        record.nonce
    )))
}

fn remove_empty_tombstone(tombstone: &Path) -> Result<()> {
    verify_real_directory(tombstone)?;
    if fs::read_dir(tombstone)?.next().is_some() {
        bail!(
            "marker-less generated tombstone is not empty: {}",
            tombstone.display()
        );
    }
    fs::remove_dir(tombstone)?;
    if let Some(parent) = tombstone.parent() {
        atomic::sync_dir(parent)?;
    }
    Ok(())
}

fn collect_prune_candidates(
    root: &Path,
    directory: &Path,
    expected: &BTreeSet<PathBuf>,
    stale_files: &mut Vec<PathBuf>,
    directories: &mut Vec<PathBuf>,
) -> Result<()> {
    verify_real_directory(directory)?;
    for child in fs::read_dir(directory)? {
        let child = child?;
        let path = child.path();
        let relative = path
            .strip_prefix(root)
            .context("generated traversal escaped owned root")?;
        if directory == root && relative == Path::new(".meduza-owner") {
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("invalid generated ownership marker");
            }
            continue;
        }

        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "refusing symlink in generated directory: {}",
                path.display()
            );
        }
        if metadata.is_file() {
            if !expected.contains(relative) {
                stale_files.push(path);
            }
        } else if metadata.is_dir() {
            if expected.contains(relative) {
                bail!("expected generated file is a directory: {}", path.display());
            }
            collect_prune_candidates(root, &path, expected, stale_files, directories)?;
            // Children are collected before their parent so the removal pass
            // can use this vector directly as a post-order traversal.
            directories.push(path);
        } else {
            bail!("refusing unknown generated object: {}", path.display());
        }
    }
    Ok(())
}

fn clear_directory_marker_last(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".meduza-owner" {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(&path)?;
        } else if metadata.is_dir() {
            clear_directory_marker_last(&path)?;
        } else {
            bail!("refusing unknown generated object: {}", path.display());
        }
        atomic::sync_dir(directory)?;
    }
    let marker = directory.join(".meduza-owner");
    if marker.exists() {
        atomic::durable_remove(&marker)?;
    }
    fs::remove_dir(directory)?;
    if let Some(parent) = directory.parent() {
        atomic::sync_dir(parent)?;
    }
    Ok(())
}

fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn set_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InterfaceKind;

    fn wireguard_entry(paths: &Paths, logical: &str, device: &str) -> ManifestEntry {
        ManifestEntry {
            kind: InterfaceKind::Wireguard,
            instance: "office".into(),
            logical: logical.into(),
            device: device.into(),
            config: paths.generated.join("wireguard/office/wg.conf"),
        }
    }

    #[test]
    fn ownership_loader_rejects_an_oversized_journal() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let file = fs::File::create(&paths.ownership).unwrap();
        file.set_len((MAX_OWNERSHIP_FILE_BYTES + 1) as u64).unwrap();

        assert!(OwnershipDb::load(&paths).is_err());
    }

    #[test]
    fn retired_uci_ownership_fields_are_ignored_without_touching_uci() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        atomic::atomic_write(
            &paths.ownership,
            br#"{"version":1,"generated":{},"sections":{"network.ovpn_old":{"nonce":"0123456789abcdef0123456789abcdef","phase":"owned","package":"network","section":"ovpn_old"}},"edges":{"vpn\u0000ovpn_old":{"nonce":"0123456789abcdef0123456789abcdef","phase":"owned","zone":"vpn","member":"ovpn_old","network_nonce":"0123456789abcdef0123456789abcdef","tag_option":"meduza_edge_old"}},"wireguard_stages":{},"frr":null}"#,
            0o600,
        )
        .unwrap();

        let db = OwnershipDb::load(&paths).unwrap();
        assert!(db.generated.is_empty());
        assert!(db.wireguard_stages.is_empty());
        assert!(db.frr.is_none());
        db.save(&paths).unwrap();
        let saved = String::from_utf8(fs::read(&paths.ownership).unwrap()).unwrap();
        assert!(!saved.contains("sections"));
        assert!(!saved.contains("edges"));
    }

    #[test]
    fn generated_create_and_delete_are_replayable() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let entry = wireguard_entry(&paths, "wg_office", "wg-office");
        let mut db = OwnershipDb::load(&paths).unwrap();
        let directory = db.ensure_generated(&paths, &entry).unwrap();
        atomic::atomic_write(&directory.join("wg.conf"), b"secret", 0o600).unwrap();
        db.remove_generated(&paths, &entry).unwrap();
        assert!(!directory.exists());
        assert!(
            !db.generated
                .contains_key(&OwnershipDb::generated_key(&entry))
        );
    }

    #[test]
    fn creation_recovers_an_unmarked_stage_and_atomic_temporary() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let entry = wireguard_entry(&paths, "wg_office", "wg-office");
        let key = OwnershipDb::generated_key(&entry);
        let directory = entry.config.parent().unwrap().to_path_buf();
        let record = GeneratedRecord {
            nonce: "0123456789abcdef0123456789abcdef".into(),
            phase: Phase::Creating,
            entry: entry.clone(),
            directory: directory.clone(),
            tombstone: None,
            pending_entry: None,
        };
        let mut db = OwnershipDb::load(&paths).unwrap();
        db.generated.insert(key.clone(), record.clone());
        db.save(&paths).unwrap();

        let parent = directory.parent().unwrap();
        atomic::ensure_private_dir(parent, 0o700).unwrap();
        let stage = parent.join(format!(
            ".meduza-create-{}-{}",
            sanitize_filename(&key),
            record.nonce
        ));
        fs::create_dir(&stage).unwrap();
        fs::write(
            stage.join("..meduza-owner.meduza-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            b"partial",
        )
        .unwrap();
        atomic::sync_dir(parent).unwrap();

        let mut recovered = OwnershipDb::load(&paths).unwrap();
        assert_eq!(
            recovered.ensure_generated(&paths, &entry).unwrap(),
            directory
        );
        let live = recovered.generated.get(&key).unwrap();
        assert_eq!(live.phase, Phase::Owned);
        verify_generated_marker(&directory, live).unwrap();
        assert!(!stage.exists());
    }

    #[test]
    fn deletion_recovers_an_empty_markerless_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let entry = wireguard_entry(&paths, "wg_office", "wg-office");
        let key = OwnershipDb::generated_key(&entry);
        let mut db = OwnershipDb::load(&paths).unwrap();
        let directory = db.ensure_generated(&paths, &entry).unwrap();
        let record = db.generated.get(&key).unwrap().clone();
        let tombstone = deletion_tombstone(&record, &key).unwrap();
        let live = db.generated.get_mut(&key).unwrap();
        live.phase = Phase::Deleting;
        live.tombstone = Some(tombstone.clone());
        db.save(&paths).unwrap();
        atomic::durable_rename(&directory, &tombstone).unwrap();
        atomic::durable_remove(&tombstone.join(".meduza-owner")).unwrap();

        let mut recovered = OwnershipDb::load(&paths).unwrap();
        recovered.remove_generated(&paths, &entry).unwrap();
        assert!(!tombstone.exists());
        assert!(!recovered.generated.contains_key(&key));
    }

    #[test]
    fn markerless_tombstone_with_content_is_never_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let entry = wireguard_entry(&paths, "wg_office", "wg-office");
        let key = OwnershipDb::generated_key(&entry);
        let mut db = OwnershipDb::load(&paths).unwrap();
        let directory = db.ensure_generated(&paths, &entry).unwrap();
        let record = db.generated.get(&key).unwrap().clone();
        let tombstone = deletion_tombstone(&record, &key).unwrap();
        let live = db.generated.get_mut(&key).unwrap();
        live.phase = Phase::Deleting;
        live.tombstone = Some(tombstone.clone());
        db.save(&paths).unwrap();
        atomic::durable_rename(&directory, &tombstone).unwrap();
        atomic::durable_remove(&tombstone.join(".meduza-owner")).unwrap();
        atomic::atomic_write(&tombstone.join("unexpected"), b"keep", 0o600).unwrap();

        let mut recovered = OwnershipDb::load(&paths).unwrap();
        assert!(recovered.remove_generated(&paths, &entry).is_err());
        assert_eq!(fs::read(tombstone.join("unexpected")).unwrap(), b"keep");
    }

    #[test]
    fn same_instance_same_directory_rekeys_and_ignores_old_stale_row() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let old = wireguard_entry(&paths, "wg_office", "wg-old");
        let new = wireguard_entry(&paths, "wg_office", "wg-new");
        let key = OwnershipDb::generated_key(&old);
        let mut db = OwnershipDb::load(&paths).unwrap();
        let directory = db.ensure_generated(&paths, &old).unwrap();
        let nonce = db.generated.get(&key).unwrap().nonce.clone();

        db.ensure_generated(&paths, &new).unwrap();

        let live = db.generated.get(&key).unwrap();
        assert_eq!(live.entry, new);
        assert_eq!(live.nonce, nonce);
        assert_eq!(live.phase, Phase::Owned);
        verify_generated_marker(&directory, live).unwrap();
        db.remove_generated(&paths, &old).unwrap();
        assert!(directory.is_dir());
    }

    #[test]
    fn rekey_recovers_before_and_after_marker_publication() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let old = wireguard_entry(&paths, "wg_office", "wg-one");
        let middle = wireguard_entry(&paths, "wg_office", "wg-two");
        let new = wireguard_entry(&paths, "wg_office", "wg-three");
        let key = OwnershipDb::generated_key(&old);
        let mut db = OwnershipDb::load(&paths).unwrap();
        db.ensure_generated(&paths, &old).unwrap();

        // Crash after the Updating record is durable, before marker publish.
        let live = db.generated.get_mut(&key).unwrap();
        live.phase = Phase::Updating;
        live.pending_entry = Some(middle.clone());
        db.save(&paths).unwrap();
        assert!(db.authorizes_generated_resource(&middle).unwrap());
        assert!(!db.authorizes_generated(&middle).unwrap());
        let mut recovered = OwnershipDb::load(&paths).unwrap();
        recovered.ensure_generated(&paths, &old).unwrap();
        assert_eq!(recovered.generated.get(&key).unwrap().entry, old);
        recovered.ensure_generated(&paths, &middle).unwrap();
        assert_eq!(recovered.generated.get(&key).unwrap().entry, middle);

        // Crash after marker publish, before clearing the Updating record.
        let live = recovered.generated.get_mut(&key).unwrap();
        live.phase = Phase::Updating;
        live.pending_entry = Some(new.clone());
        recovered.save(&paths).unwrap();
        let transition = recovered.generated.get(&key).unwrap().clone();
        atomic::atomic_write(
            &transition.directory.join(".meduza-owner"),
            &marker_bytes_for_entry(&transition, &new).unwrap(),
            0o600,
        )
        .unwrap();
        assert!(recovered.authorizes_generated_resource(&middle).unwrap());
        assert!(recovered.authorizes_generated_resource(&new).unwrap());
        let mut recovered_again = OwnershipDb::load(&paths).unwrap();
        recovered_again.ensure_generated(&paths, &new).unwrap();
        let live = recovered_again.generated.get(&key).unwrap();
        assert_eq!(live.entry, new);
        assert_eq!(live.phase, Phase::Owned);
        assert!(live.pending_entry.is_none());
    }

    #[test]
    fn prune_removes_only_stale_regular_files_and_empty_directories() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let entry = wireguard_entry(&paths, "wg_office", "wg-office");
        let mut db = OwnershipDb::load(&paths).unwrap();
        let directory = db.ensure_generated(&paths, &entry).unwrap();
        let hosts = directory.join("hosts");
        let obsolete = directory.join("obsolete");
        atomic::ensure_private_dir(&hosts, 0o700).unwrap();
        atomic::ensure_private_dir(&obsolete, 0o700).unwrap();
        let keep = hosts.join("keep");
        let stale = hosts.join("stale");
        let obsolete_file = obsolete.join("peer");
        atomic::atomic_write(&keep, b"keep", 0o600).unwrap();
        atomic::atomic_write(&stale, b"stale", 0o600).unwrap();
        atomic::atomic_write(&obsolete_file, b"old", 0o600).unwrap();

        let removed = db.prune_generated(&entry, [keep.as_path()]).unwrap();

        assert!(removed >= 3);
        assert_eq!(fs::read(keep).unwrap(), b"keep");
        assert!(!stale.exists());
        assert!(!obsolete.exists());
        verify_generated_marker(
            &directory,
            db.generated
                .get(&OwnershipDb::generated_key(&entry))
                .unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prune_rejects_symlink_before_removing_any_stale_file() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let entry = wireguard_entry(&paths, "wg_office", "wg-office");
        let mut db = OwnershipDb::load(&paths).unwrap();
        let directory = db.ensure_generated(&paths, &entry).unwrap();
        let stale = directory.join("stale");
        atomic::atomic_write(&stale, b"do not partially prune", 0o600).unwrap();
        let external = temp.path().join("external");
        fs::write(&external, b"external").unwrap();
        symlink(&external, directory.join("link")).unwrap();

        assert!(
            db.prune_generated(&entry, std::iter::empty::<&Path>())
                .is_err()
        );
        assert_eq!(fs::read(stale).unwrap(), b"do not partially prune");
        assert_eq!(fs::read(external).unwrap(), b"external");
    }
}

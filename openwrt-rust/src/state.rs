use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::ownership::OwnershipDb;

/// Encoded cache allowance. The decoded flattened payload is limited more
/// tightly in `model`; this extra headroom covers JSON escaping and metadata.
pub const MAX_SNAPSHOT_FILE_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_MANIFEST_FILE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: Option<PathBuf>,
    /// Operator configuration, PKI and generated VPN configuration only.
    pub data: PathBuf,
    /// Controller-owned durable journals and last-known-good state.
    pub state: PathBuf,
    pub managed: PathBuf,
    pub generated: PathBuf,
    pub runtime: PathBuf,
    pub cache: PathBuf,
    pub cache_pending: PathBuf,
    pub manifest: PathBuf,
    pub pending_manifest: PathBuf,
    pub ownership: PathBuf,
    pub reported: PathBuf,
    pub lock: PathBuf,
    pub ip_forward: PathBuf,
    pub ip_forward_marker: PathBuf,
    pub frr_config: PathBuf,
    pub uci_config_dir: PathBuf,
    pub openvpn_proto: PathBuf,
}

impl Paths {
    pub fn from_root(root: Option<&Path>) -> Self {
        let data = atomic::rooted(root, "/etc/meduza");
        let state = atomic::rooted(root, "/etc/meduza-state");
        let managed = state.join("managed");
        Self {
            root: root.map(Path::to_path_buf),
            generated: data.join("generated"),
            cache: state.join("cache.json"),
            cache_pending: state.join("cache.pending.json"),
            manifest: managed.join("interfaces"),
            pending_manifest: managed.join("interfaces.pending"),
            ownership: managed.join("ownership.json"),
            reported: managed.join("reported.json"),
            runtime: atomic::rooted(root, "/var/run/meduza"),
            lock: atomic::rooted(root, "/var/lock/meduza-openwrt.lock"),
            ip_forward: atomic::rooted(root, "/proc/sys/net/ipv4/ip_forward"),
            ip_forward_marker: atomic::rooted(root, "/var/run/meduza/ip-forward.changed"),
            frr_config: atomic::rooted(root, "/etc/frr/frr.conf"),
            uci_config_dir: atomic::rooted(root, "/etc/config"),
            openvpn_proto: atomic::rooted(root, "/lib/netifd/proto/openvpn.sh"),
            data,
            state,
            managed,
        }
    }

    pub fn prepare(&self) -> Result<()> {
        atomic::ensure_private_dir(&self.data, 0o700)?;
        self.migrate_legacy_rust_state()?;
        atomic::ensure_private_dir(&self.state, 0o700)?;
        atomic::ensure_private_dir(&self.managed, 0o700)?;
        atomic::ensure_private_dir(&self.generated, 0o700)?;
        atomic::ensure_private_dir(&self.runtime, 0o700)
    }

    /// Move the first Rust release's durable state out of `/etc/meduza`.
    ///
    /// The retired Python/shell implementation used the same cache and
    /// manifest names, so those names are not ownership proof.  Automatic
    /// migration is authorized only by a valid Rust `ownership.json`, which
    /// the old implementation never created.  An ambiguous layout fails
    /// closed and is left byte-for-byte intact for an operator audit.
    ///
    /// The managed directory is renamed before either cache.  If power is
    /// lost between those steps, the already-relocated ownership database
    /// authorizes moving the remaining cache on the next invocation.  A
    /// legacy FRR backup path inside the database is rewritten atomically
    /// after the directory move; the recovery reader accepts that one exact
    /// intermediate representation.
    pub fn migrate_legacy_rust_state(&self) -> Result<bool> {
        let legacy = self.legacy_state_layout();
        let mut old_managed_exists = real_directory_exists(&legacy.managed)?;
        let old_cache_exists = regular_file_exists(&legacy.cache)?;
        let old_pending_exists = regular_file_exists(&legacy.cache_pending)?;
        let old_cache_temps_exist = atomic_temporary_siblings_exist(&legacy.cache)?;
        let old_pending_temps_exist = atomic_temporary_siblings_exist(&legacy.cache_pending)?;
        let old_temps_exist = old_cache_temps_exist || old_pending_temps_exist;

        if old_managed_exists
            && directory_is_empty(&legacy.managed)?
            && !old_cache_exists
            && !old_pending_exists
        {
            fs::remove_dir(&legacy.managed)?;
            atomic::sync_dir(&legacy.data)?;
            old_managed_exists = false;
            if !old_temps_exist {
                return Ok(true);
            }
        }

        if !old_managed_exists && !old_cache_exists && !old_pending_exists && !old_temps_exist {
            return Ok(false);
        }

        let old_ownership = regular_file_exists(&legacy.ownership)?;
        let new_ownership = regular_file_exists(&self.ownership)?;
        if old_ownership && new_ownership {
            bail!(
                "both legacy and current Rust ownership databases exist; refusing ambiguous state migration"
            );
        }

        let (mut ownership, authority_is_legacy) = if old_ownership {
            (
                OwnershipDb::load(&legacy)
                    .context("legacy Rust ownership database failed validation")?,
                true,
            )
        } else if new_ownership {
            (load_relocated_ownership(self, &legacy)?, false)
        } else {
            bail!(
                "persistent files remain in the old /etc/meduza state layout but no valid Rust ownership database identifies them; purge the retired controller or audit the files before migration"
            );
        };

        if old_managed_exists {
            validate_legacy_managed_dir(&legacy.managed)?;
            if !authority_is_legacy {
                bail!(
                    "legacy managed files coexist with the current Rust ownership database; refusing ambiguous state migration"
                );
            }
            atomic::ensure_private_dir(&self.state, 0o700)?;
            move_for_layout_migration(&legacy.managed, &self.managed, true)?;
        } else if authority_is_legacy {
            bail!("legacy Rust ownership database disappeared during state migration");
        }

        // The only persistent absolute path stored in the ownership database
        // is the optional FRR backup. Generated paths intentionally remain
        // beneath /etc/meduza/generated and therefore do not change.
        if let Some(record) = ownership.frr.as_mut()
            && record.backup.as_ref() == Some(&legacy.managed.join("frr.conf.backup"))
        {
            record.backup = Some(self.managed.join("frr.conf.backup"));
        }
        ownership
            .save(self)
            .context("could not publish relocated Rust ownership database")?;

        atomic::ensure_private_dir(&self.state, 0o700)?;
        move_for_layout_migration(&legacy.cache, &self.cache, false)?;
        move_for_layout_migration(&legacy.cache_pending, &self.cache_pending, false)?;
        // Interrupted Rust atomic writes are never valid snapshots. Their
        // exact nonce-shaped names provide the same narrow cleanup authority
        // used by normal atomic replacement.
        atomic::cleanup_atomic_temps(&legacy.cache)?;
        atomic::cleanup_atomic_temps(&legacy.cache_pending)?;
        tracing::info!(
            from = %legacy.data.display(),
            to = %self.state.display(),
            "migrated prior Rust persistent state layout"
        );
        Ok(true)
    }

    fn legacy_state_layout(&self) -> Self {
        let mut legacy = self.clone();
        legacy.state = self.data.clone();
        legacy.managed = self.data.join("managed");
        legacy.cache = self.data.join("cache.json");
        legacy.cache_pending = self.data.join("cache.pending.json");
        legacy.manifest = legacy.managed.join("interfaces");
        legacy.pending_manifest = legacy.managed.join("interfaces.pending");
        legacy.ownership = legacy.managed.join("ownership.json");
        legacy.reported = legacy.managed.join("reported.json");
        legacy
    }
}

fn load_relocated_ownership(current: &Paths, legacy: &Paths) -> Result<OwnershipDb> {
    match OwnershipDb::load(current) {
        Ok(value) => Ok(value),
        Err(current_error) => {
            // Crash recovery for the one durable intermediate state after the
            // managed directory rename and before the FRR backup-path rewrite.
            let mut intermediate = legacy.clone();
            intermediate.ownership = current.ownership.clone();
            OwnershipDb::load(&intermediate).with_context(|| {
                format!(
                    "current Rust ownership database is invalid ({current_error:#}) and is not a valid interrupted layout migration"
                )
            })
        }
    }
}

fn real_directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "persistent state is not a real directory: {}",
                path.display()
            )
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn directory_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)?.next().transpose()?.is_none())
}

/// Detect only the exact nonce-shaped siblings produced by `atomic_write`.
/// Detection does not authorize deletion: migration first requires a valid
/// Rust ownership database, and `cleanup_atomic_temps` then validates every
/// matching object as a non-symlink regular file before unlinking it.
fn atomic_temporary_siblings_exist(path: &Path) -> Result<bool> {
    let parent = path.parent().context("atomic state target has no parent")?;
    let metadata = match fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "persistent state parent is not a real directory: {}",
            parent.display()
        );
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("atomic state target has a non-UTF-8 filename")?;
    let prefix = format!(".{name}.meduza-");
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let Some(candidate) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(nonce) = candidate.strip_prefix(&prefix) else {
            continue;
        };
        if nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_legacy_managed_dir(path: &Path) -> Result<()> {
    const KNOWN: &[&str] = &[
        "interfaces",
        "interfaces.pending",
        "ownership.json",
        "reported.json",
        "frr.conf.backup",
        "frr.pending.conf",
        "frr.reload.pending",
        "uci-reload.pending",
    ];
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("legacy managed state contains a non-UTF-8 name"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "legacy managed state contains an unexpected object: {}",
                entry.path().display()
            );
        }
        let known = KNOWN.contains(&name.as_str())
            || KNOWN.iter().any(|target| {
                name.strip_prefix(&format!(".{target}.meduza-"))
                    .is_some_and(|nonce| {
                        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
            });
        if !known {
            bail!(
                "legacy managed state contains an unknown file: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn move_for_layout_migration(source: &Path, target: &Path, directory: bool) -> Result<()> {
    let source_metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let target_metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if source_metadata.is_none() {
        if let Some(metadata) = target_metadata {
            let valid = if directory {
                !metadata.file_type().is_symlink() && metadata.is_dir()
            } else {
                !metadata.file_type().is_symlink() && metadata.is_file()
            };
            if !valid {
                bail!("invalid migrated state target: {}", target.display());
            }
        }
        return Ok(());
    }
    let source_metadata = source_metadata.expect("checked above");
    let valid_source = if directory {
        !source_metadata.file_type().is_symlink() && source_metadata.is_dir()
    } else {
        !source_metadata.file_type().is_symlink() && source_metadata.is_file()
    };
    if !valid_source {
        bail!("invalid legacy state source: {}", source.display());
    }
    if target_metadata.is_some() {
        bail!(
            "legacy and current state objects coexist: {} and {}",
            source.display(),
            target.display()
        );
    }
    let source_parent = source.parent().context("legacy state has no parent")?;
    let target_parent = target.parent().context("current state has no parent")?;
    atomic::confirm_dir(target_parent)?;
    match fs::rename(source, target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A concurrent invocation may have completed the exact atomic
            // rename. Accept only the resulting source-absent/target-present
            // shape; every other outcome remains an error.
            if fs::symlink_metadata(source).is_ok() || fs::symlink_metadata(target).is_err() {
                return Err(error.into());
            }
        }
        Err(error) => return Err(error.into()),
    }
    // Persist the destination name first. If power fails before the source
    // directory fsync, recovery may conservatively see both names and stop for
    // an audit, but it must never durably lose the only state copy.
    if target_parent != source_parent {
        atomic::sync_dir(target_parent)?;
    }
    atomic::sync_dir(source_parent)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u32,
    pub node_id: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub applied_at: String,
    pub node: BTreeMap<String, String>,
    pub global: BTreeMap<String, String>,
    pub all_nodes: BTreeMap<String, String>,
}

impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported snapshot version {}", self.version);
        }
        crate::config::validate_node_id(&self.node_id)?;
        validate_commit(&self.commit)?;
        validate_timestamp(&self.applied_at)?;
        crate::model::validate_flattened_maps(&self.node, &self.global, &self.all_nodes)
    }

    pub fn read_from(path: &Path) -> Result<Self> {
        if !regular_file_exists(path)? {
            bail!("snapshot does not exist: {}", path.display());
        }
        let bytes = atomic::read_bounded(path, MAX_SNAPSHOT_FILE_BYTES)
            .with_context(|| format!("could not read {}", path.display()))?;
        let value: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid snapshot {}", path.display()))?;
        value.validate()?;
        Ok(value)
    }

    pub fn persist_pending(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        atomic::atomic_json_bounded(&paths.cache_pending, self, MAX_SNAPSHOT_FILE_BYTES)?;
        Ok(())
    }

    pub fn promote(paths: &Paths) -> Result<()> {
        if regular_file_exists(&paths.cache_pending)? {
            // Revalidate the exact durable representation before publishing
            // it as LKG. This also prevents a corrupted/oversized interrupted
            // pending cache from replacing a usable stable cache.
            Snapshot::read_from(&paths.cache_pending)?;
            // A regular stable cache may be atomically replaced; every other
            // object type is an ownership conflict.
            let _ = regular_file_exists(&paths.cache)?;
            fs::rename(&paths.cache_pending, &paths.cache)?;
        } else if !regular_file_exists(&paths.cache)? {
            bail!("neither pending nor stable cache exists");
        }
        if !regular_file_exists(&paths.cache)? {
            bail!("stable cache promotion did not publish a regular file");
        }
        atomic::sync_dir(&paths.state)
    }
}

pub fn validate_commit(value: &str) -> Result<()> {
    // `/commit` is an opaque generation marker.  The legacy controller did
    // not constrain its syntax, and real deployments commonly use RFC3339 or
    // Chef-style values containing spaces, '+', '/', '@', or Unicode.  It is
    // never used as a path, UCI name, or command argument, so preserve it
    // exactly.  Bound memory use and reject controls that could corrupt logs.
    if value.len() > 4096 || value.chars().any(char::is_control) {
        bail!("invalid commit generation identifier");
    }
    Ok(())
}

fn validate_timestamp(value: &str) -> Result<()> {
    if value.len() > 64
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || !byte.is_ascii())
    {
        bail!("invalid snapshot timestamp");
    }
    Ok(())
}

/// Inspect a persistent state file without following symbolic links. Missing
/// files are reported as `false`; every existing non-regular object is an
/// explicit conflict rather than being silently ignored.
pub fn regular_file_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("persistent state is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum InterfaceKind {
    Tinc,
    Openvpn,
    Wireguard,
}

impl InterfaceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tinc => "tinc",
            Self::Openvpn => "openvpn",
            Self::Wireguard => "wireguard",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManifestEntry {
    pub kind: InterfaceKind,
    pub instance: String,
    pub logical: String,
    pub device: String,
    pub config: PathBuf,
}

impl ManifestEntry {
    pub fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.kind.as_str(),
            self.instance,
            self.logical,
            self.device,
            self.config.display()
        )
    }

    pub fn parse_tsv(line: &str) -> Result<Self> {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 5 {
            bail!("managed manifest row must have five fields");
        }
        let kind = match fields[0] {
            "tinc" => InterfaceKind::Tinc,
            "openvpn" => InterfaceKind::Openvpn,
            "wireguard" => InterfaceKind::Wireguard,
            value => bail!("unknown interface kind: {value}"),
        };
        crate::model::validate_instance(fields[1])?;
        crate::config::validate_uci_name(fields[2])?;
        crate::model::validate_device(fields[3])?;
        Ok(Self {
            kind,
            instance: fields[1].into(),
            logical: fields[2].into(),
            device: fields[3].into(),
            config: fields[4].into(),
        })
    }
}

impl From<&crate::model::DesiredInterface> for ManifestEntry {
    fn from(value: &crate::model::DesiredInterface) -> Self {
        let kind = match value.kind {
            crate::model::VpnKind::Tinc => InterfaceKind::Tinc,
            crate::model::VpnKind::OpenVpn => InterfaceKind::Openvpn,
            crate::model::VpnKind::WireGuard => InterfaceKind::Wireguard,
        };
        Self {
            kind,
            instance: value.instance.clone(),
            logical: value.logical.clone(),
            device: value.device.clone(),
            config: value.config.clone(),
        }
    }
}

pub fn read_manifest(path: &Path) -> Result<Vec<ManifestEntry>> {
    if !regular_file_exists(path)? {
        return Ok(Vec::new());
    }
    atomic::read_string_bounded(path, MAX_MANIFEST_FILE_BYTES)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ManifestEntry::parse_tsv)
        .collect()
}

pub fn write_manifest(path: &Path, entries: &[ManifestEntry]) -> Result<bool> {
    let mut body = entries
        .iter()
        .map(ManifestEntry::to_tsv)
        .collect::<Vec<_>>()
        .join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    if body.len() > MAX_MANIFEST_FILE_BYTES {
        bail!("managed manifest exceeds {MAX_MANIFEST_FILE_BYTES} byte limit");
    }
    atomic::atomic_write(path, body.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::{FrrRecord, Phase};

    #[test]
    fn persistent_state_is_separate_from_configuration_and_runtime() {
        let root = Path::new("/test-root");
        let paths = Paths::from_root(Some(root));
        assert_eq!(paths.data, root.join("etc/meduza"));
        assert_eq!(paths.generated, root.join("etc/meduza/generated"));
        assert_eq!(paths.state, root.join("etc/meduza-state"));
        assert_eq!(paths.cache, root.join("etc/meduza-state/cache.json"));
        assert_eq!(
            paths.ownership,
            root.join("etc/meduza-state/managed/ownership.json")
        );
        assert_eq!(paths.runtime, root.join("var/run/meduza"));
    }

    #[test]
    fn valid_prior_rust_state_is_relocated_without_touching_pki_or_generated() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        let legacy = paths.legacy_state_layout();
        fs::create_dir_all(paths.data.join("pki")).unwrap();
        fs::create_dir_all(&paths.generated).unwrap();
        fs::create_dir_all(&legacy.managed).unwrap();
        fs::write(paths.data.join("pki/client.key"), b"operator-secret").unwrap();
        fs::write(paths.generated.join("operator-note"), b"generated-root").unwrap();
        fs::write(legacy.managed.join("frr.conf.backup"), b"router bgp 1\n").unwrap();
        fs::write(&legacy.cache, b"stable-cache").unwrap();
        fs::write(&legacy.cache_pending, b"pending-cache").unwrap();

        let mut ownership = OwnershipDb::load(&legacy).unwrap();
        ownership.frr = Some(FrrRecord {
            phase: Phase::Owned,
            origin: "backup".into(),
            original_sha256: None,
            backup: Some(legacy.managed.join("frr.conf.backup")),
            active_sha256: None,
            pending_sha256: None,
            original_mode: None,
            original_uid: None,
            original_gid: None,
            managed_mode: None,
            managed_uid: None,
            managed_gid: None,
        });
        ownership.save(&legacy).unwrap();

        paths.prepare().unwrap();

        assert!(!legacy.managed.exists());
        assert!(!legacy.cache.exists());
        assert!(!legacy.cache_pending.exists());
        assert_eq!(fs::read(&paths.cache).unwrap(), b"stable-cache");
        assert_eq!(fs::read(&paths.cache_pending).unwrap(), b"pending-cache");
        assert_eq!(
            OwnershipDb::load(&paths).unwrap().frr.unwrap().backup,
            Some(paths.managed.join("frr.conf.backup"))
        );
        assert_eq!(
            fs::read(paths.data.join("pki/client.key")).unwrap(),
            b"operator-secret"
        );
        assert_eq!(
            fs::read(paths.generated.join("operator-note")).unwrap(),
            b"generated-root"
        );
    }

    #[test]
    fn interrupted_managed_rename_resumes_before_moving_cache() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        let legacy = paths.legacy_state_layout();
        fs::create_dir_all(&legacy.managed).unwrap();
        fs::create_dir_all(&paths.state).unwrap();
        fs::write(legacy.managed.join("frr.conf.backup"), b"original\n").unwrap();
        fs::write(&legacy.cache, b"stable-cache").unwrap();

        let mut ownership = OwnershipDb::load(&legacy).unwrap();
        ownership.frr = Some(FrrRecord {
            phase: Phase::Owned,
            origin: "backup".into(),
            original_sha256: None,
            backup: Some(legacy.managed.join("frr.conf.backup")),
            active_sha256: None,
            pending_sha256: None,
            original_mode: None,
            original_uid: None,
            original_gid: None,
            managed_mode: None,
            managed_uid: None,
            managed_gid: None,
        });
        ownership.save(&legacy).unwrap();

        // Simulate power loss after the cross-directory rename became durable
        // but before ownership.json's FRR backup path and cache were moved.
        fs::rename(&legacy.managed, &paths.managed).unwrap();
        assert!(OwnershipDb::load(&paths).is_err());

        assert!(paths.migrate_legacy_rust_state().unwrap());
        assert_eq!(fs::read(&paths.cache).unwrap(), b"stable-cache");
        assert!(!legacy.cache.exists());
        assert_eq!(
            OwnershipDb::load(&paths).unwrap().frr.unwrap().backup,
            Some(paths.managed.join("frr.conf.backup"))
        );
    }

    #[test]
    fn relocated_ownership_authorizes_cleanup_of_legacy_atomic_cache_temp() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        fs::create_dir_all(&paths.data).unwrap();
        fs::create_dir_all(&paths.managed).unwrap();
        OwnershipDb::load(&paths).unwrap().save(&paths).unwrap();
        let legacy_temp = paths
            .data
            .join(".cache.json.meduza-0123456789abcdef0123456789abcdef");
        fs::write(&legacy_temp, b"interrupted-cache").unwrap();

        assert!(paths.migrate_legacy_rust_state().unwrap());
        assert!(!legacy_temp.exists());
        assert!(paths.ownership.is_file());
    }

    #[test]
    fn legacy_atomic_cache_temp_without_rust_ownership_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        fs::create_dir_all(&paths.data).unwrap();
        let legacy_temp = paths
            .data
            .join(".cache.json.meduza-0123456789abcdef0123456789abcdef");
        fs::write(&legacy_temp, b"untrusted-cache").unwrap();

        assert!(paths.migrate_legacy_rust_state().is_err());
        assert_eq!(fs::read(legacy_temp).unwrap(), b"untrusted-cache");
    }

    #[test]
    fn ambiguous_legacy_cache_is_rejected_without_modification() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        fs::create_dir_all(&paths.data).unwrap();
        let legacy_cache = paths.data.join("cache.json");
        fs::write(&legacy_cache, b"could-be-openwrt-lite").unwrap();

        let error = paths.prepare().unwrap_err().to_string();

        assert!(error.contains("no valid Rust ownership database"));
        assert_eq!(fs::read(legacy_cache).unwrap(), b"could-be-openwrt-lite");
        assert!(!paths.cache.exists());
    }

    #[test]
    fn five_column_manifest_round_trip() {
        let row = ManifestEntry::parse_tsv(
            "wireguard\toffice\twg_office\twg-office\t/etc/meduza/generated/wireguard/office/wg.conf",
        )
        .unwrap();
        assert_eq!(row.kind, InterfaceKind::Wireguard);
        assert_eq!(row.to_tsv().split('\t').count(), 5);
    }

    #[test]
    fn commit_generation_is_opaque_but_log_safe() {
        for value in [
            "",
            "2026-08-22T19:25:53+08:00",
            "2026-08-22 19:25:53 +0800",
            "chef/run #42 @ edge=prod",
            "发布-42",
        ] {
            validate_commit(value).unwrap();
        }
        for value in [
            "line\nbreak",
            "carriage\rreturn",
            "nul\0byte",
            "escape\u{1b}",
        ] {
            assert!(validate_commit(value).is_err());
        }
        assert!(validate_commit(&"x".repeat(4096)).is_ok());
        assert!(validate_commit(&"x".repeat(4097)).is_err());
    }

    #[test]
    fn snapshot_preserves_an_opaque_commit_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cache.json");
        let expected = "2026-08-22 19:25:53 +0800 / Chef #42 发布";
        let snapshot = Snapshot {
            version: 1,
            node_id: "router-01".into(),
            commit: expected.into(),
            applied_at: "2026-08-22T19:25:53+08:00".into(),
            node: BTreeMap::new(),
            global: BTreeMap::new(),
            all_nodes: BTreeMap::new(),
        };
        atomic::atomic_json(&path, &snapshot).unwrap();
        assert_eq!(Snapshot::read_from(&path).unwrap().commit, expected);
    }

    #[test]
    fn snapshot_rejects_flattened_payload_over_the_resource_budget() {
        let mut snapshot = Snapshot {
            version: 1,
            node_id: "router-01".into(),
            commit: "generation-1".into(),
            applied_at: "2026-08-22T19:25:53+08:00".into(),
            node: BTreeMap::new(),
            global: BTreeMap::new(),
            all_nodes: BTreeMap::new(),
        };
        snapshot.node.insert(
            "/nodes/router-01/openvpn/site/extra_config".into(),
            "x".repeat(crate::model::MAX_FLATTENED_VALUE_BYTES + 1),
        );

        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn snapshot_reader_rejects_oversized_file_before_json_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cache.json");
        let file = fs::File::create(&path).unwrap();
        file.set_len((MAX_SNAPSHOT_FILE_BYTES + 1) as u64).unwrap();

        let error = Snapshot::read_from(&path).unwrap_err().to_string();
        assert!(error.contains("could not read"));
        assert!(format!("{:#}", Snapshot::read_from(&path).unwrap_err()).contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn persistent_state_readers_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("external");
        fs::write(&target, b"{}").unwrap();
        let snapshot = temp.path().join("cache.json");
        let manifest = temp.path().join("interfaces");
        symlink(&target, &snapshot).unwrap();
        symlink(&target, &manifest).unwrap();

        assert!(Snapshot::read_from(&snapshot).is_err());
        assert!(read_manifest(&manifest).is_err());
    }
}

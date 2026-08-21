//! Typed model for Meduza's flattened etcd snapshot.
//!
//! The server stores each leaf as an etcd key/value pair.  A snapshot is
//! therefore represented as maps whose keys are still absolute etcd paths,
//! rather than as a nested JSON document.  Keeping that representation here
//! makes cache files compatible with the previous OpenWrt implementation and
//! avoids lossy conversions of inline keys and certificates.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub const SNAPSHOT_VERSION: u32 = 1;
pub const DEFAULT_GENERATED_ROOT: &str = "/var/run/meduza/generated";
pub const MAX_INSTANCE_BYTES: usize = 64;
pub const MAX_NODE_ID_BYTES: usize = 128;
pub const MAX_DEVICE_BYTES: usize = 15;
pub const MAX_FLATTENED_KEY_BYTES: usize = 512;
pub const MAX_FLATTENED_VALUE_BYTES: usize = 1024 * 1024;
pub const MAX_FLATTENED_ENTRIES: usize = 4096;
pub const MAX_FLATTENED_TOTAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct FlattenedBudget {
    entries: usize,
    total_bytes: usize,
}

impl FlattenedBudget {
    pub fn include(&mut self, key: &str, value: &str) -> Result<()> {
        let entry_bytes = validate_flattened_entry(key, value)?;
        self.entries = self
            .entries
            .checked_add(1)
            .context("flattened snapshot entry count overflow")?;
        self.total_bytes = self
            .total_bytes
            .checked_add(entry_bytes)
            .context("flattened snapshot byte count overflow")?;
        if self.entries > MAX_FLATTENED_ENTRIES {
            bail!("flattened snapshot exceeds {MAX_FLATTENED_ENTRIES} entry limit");
        }
        if self.total_bytes > MAX_FLATTENED_TOTAL_BYTES {
            bail!("flattened snapshot exceeds {MAX_FLATTENED_TOTAL_BYTES} byte limit");
        }
        Ok(())
    }
}

fn snapshot_version() -> u32 {
    SNAPSHOT_VERSION
}

/// Persistent, flattened snapshot used both by etcd reconciliation and LKG
/// recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatSnapshot {
    #[serde(default = "snapshot_version")]
    pub version: u32,
    pub node_id: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
    #[serde(default)]
    pub node: BTreeMap<String, String>,
    #[serde(default, rename = "global")]
    pub global: BTreeMap<String, String>,
    #[serde(default)]
    pub all_nodes: BTreeMap<String, String>,
}

impl FlatSnapshot {
    pub fn new(
        node_id: impl Into<String>,
        node: BTreeMap<String, String>,
        global: BTreeMap<String, String>,
        all_nodes: BTreeMap<String, String>,
    ) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            node_id: node_id.into(),
            commit: String::new(),
            applied_at: None,
            node,
            global,
            all_nodes,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != SNAPSHOT_VERSION {
            bail!("unsupported flattened snapshot version: {}", self.version);
        }
        validate_node_id(&self.node_id)?;
        if !self.commit.is_empty() {
            crate::state::validate_commit(&self.commit)?;
        }
        validate_flattened_maps(&self.node, &self.global, &self.all_nodes)?;
        Ok(())
    }

    pub fn node_key(&self, suffix: &str) -> String {
        format!("/nodes/{}/{}", self.node_id, suffix.trim_start_matches('/'))
    }

    pub fn global_key(suffix: &str) -> String {
        format!("/global/{}", suffix.trim_start_matches('/'))
    }

    pub fn all_node_key(node_id: &str, suffix: &str) -> String {
        format!("/nodes/{}/{}", node_id, suffix.trim_start_matches('/'))
    }

    pub fn node_value(&self, suffix: &str) -> Option<&str> {
        self.node.get(&self.node_key(suffix)).map(String::as_str)
    }

    pub fn global_value(&self, suffix: &str) -> Option<&str> {
        self.global
            .get(&Self::global_key(suffix))
            .map(String::as_str)
    }

    pub fn all_node_value(&self, node_id: &str, suffix: &str) -> Option<&str> {
        self.all_nodes
            .get(&Self::all_node_key(node_id, suffix))
            .map(String::as_str)
    }

    pub fn node_enabled(&self, suffix: &str) -> bool {
        self.node_value(suffix).is_some_and(parse_enabled)
    }

    pub fn all_node_enabled(&self, node_id: &str, suffix: &str) -> bool {
        self.all_node_value(node_id, suffix)
            .is_some_and(parse_enabled)
    }

    /// Return sorted immediate children beneath a local-node collection.
    /// For example, `node_children("openvpn")` extracts instance names from
    /// `/nodes/<id>/openvpn/<instance>/...`.
    pub fn node_children(&self, collection: &str) -> BTreeSet<String> {
        let prefix = format!("/nodes/{}/{}/", self.node_id, collection.trim_matches('/'));
        immediate_children(&self.node, &prefix)
    }

    pub fn all_node_ids(&self) -> BTreeSet<String> {
        immediate_children(&self.all_nodes, "/nodes/")
    }

    pub fn all_node_children(&self, node_id: &str, collection: &str) -> BTreeSet<String> {
        let prefix = format!("/nodes/{}/{}/", node_id, collection.trim_matches('/'));
        immediate_children(&self.all_nodes, &prefix)
    }
}

/// Validate the resource envelope shared by live etcd responses and durable
/// snapshots. The byte budget includes both keys and values and counts the
/// local node twice when it is present in both `node` and `all_nodes`, matching
/// the memory and JSON representation that this process actually owns.
pub fn validate_flattened_maps(
    node: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    all_nodes: &BTreeMap<String, String>,
) -> Result<()> {
    let mut budget = FlattenedBudget::default();
    for (key, value) in node.iter().chain(global.iter()).chain(all_nodes.iter()) {
        budget.include(key, value)?;
    }
    Ok(())
}

/// Validate one flattened etcd pair before allocating owned copies of it.
/// Returns its contribution to the aggregate decoded byte budget.
pub fn validate_flattened_entry(key: &str, value: &str) -> Result<usize> {
    if key.len() > MAX_FLATTENED_KEY_BYTES
        || !key.starts_with('/')
        || key
            .bytes()
            .any(|byte| !byte.is_ascii() || byte.is_ascii_control() || byte == b' ')
    {
        bail!("invalid flattened etcd key: {key:?}");
    }
    if value.len() > MAX_FLATTENED_VALUE_BYTES {
        bail!("etcd value exceeds {MAX_FLATTENED_VALUE_BYTES} byte limit: {key}");
    }
    if value.contains('\0') {
        bail!("etcd value contains NUL: {key}");
    }
    key.len()
        .checked_add(value.len())
        .context("flattened etcd entry byte count overflow")
}

fn immediate_children(map: &BTreeMap<String, String>, prefix: &str) -> BTreeSet<String> {
    map.keys()
        .filter_map(|key| key.strip_prefix(prefix))
        .filter_map(|remainder| remainder.split('/').next())
        .filter(|child| !child.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn parse_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnKind {
    Tinc,
    OpenVpn,
    WireGuard,
}

impl VpnKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tinc => "tinc",
            Self::OpenVpn => "openvpn",
            Self::WireGuard => "wireguard",
        }
    }

    pub const fn logical_prefix(self) -> &'static str {
        match self {
            Self::Tinc => "tinc",
            Self::OpenVpn => "ovpn",
            Self::WireGuard => "wg",
        }
    }

    pub const fn config_filename(self) -> &'static str {
        match self {
            Self::Tinc => "tinc.conf",
            Self::OpenVpn => "openvpn.conf",
            Self::WireGuard => "wg.conf",
        }
    }
}

impl fmt::Display for VpnKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredInterface {
    pub kind: VpnKind,
    pub instance: String,
    pub logical: String,
    pub device: String,
    pub config: PathBuf,
}

impl DesiredInterface {
    pub fn validate(&self, generated_root: &Path) -> Result<()> {
        validate_instance_name(&self.instance)?;
        validate_logical_name(&self.logical)?;
        validate_device_name(&self.device)?;
        let expected_logical = logical_name(self.kind, &self.instance)?;
        if self.logical != expected_logical {
            bail!(
                "logical interface {} does not match {} instance {}",
                self.logical,
                self.kind,
                self.instance
            );
        }
        let expected_config = generated_config_path(generated_root, self.kind, &self.instance)?;
        if self.config != expected_config {
            bail!(
                "configuration path {:?} does not match {} instance {}",
                self.config,
                self.kind,
                self.instance
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredState {
    pub node_id: String,
    pub generated_root: PathBuf,
    pub interfaces: Vec<DesiredInterface>,
}

impl DesiredState {
    pub fn interface(&self, kind: VpnKind, instance: &str) -> Option<&DesiredInterface> {
        self.interfaces
            .iter()
            .find(|item| item.kind == kind && item.instance == instance)
    }

    pub fn validate(&self) -> Result<()> {
        validate_node_id(&self.node_id)?;
        validate_generated_root(&self.generated_root)?;
        let mut logicals = BTreeSet::new();
        let mut devices = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for item in &self.interfaces {
            item.validate(&self.generated_root)?;
            if !identities.insert((item.kind, item.instance.clone())) {
                bail!(
                    "duplicate desired VPN identity: {}/{}",
                    item.kind,
                    item.instance
                );
            }
            if !logicals.insert(item.logical.clone()) {
                bail!("duplicate desired interface identity: {}", item.logical);
            }
            if !devices.insert(item.device.clone()) {
                bail!("duplicate desired Linux device: {}", item.device);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptions {
    pub generated_root: PathBuf,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            generated_root: PathBuf::from(DEFAULT_GENERATED_ROOT),
        }
    }
}

pub fn build_desired(snapshot: &FlatSnapshot) -> Result<DesiredState> {
    build_desired_with_options(snapshot, &BuildOptions::default())
}

pub fn build_desired_with_options(
    snapshot: &FlatSnapshot,
    options: &BuildOptions,
) -> Result<DesiredState> {
    snapshot.validate()?;
    validate_generated_root(&options.generated_root)?;
    let mut interfaces = Vec::new();

    if snapshot.global_value("mesh_type") == Some("tinc") && snapshot.node_enabled("tinc/enable") {
        let instance = snapshot.global_value("tinc/netname").unwrap_or("mesh");
        let device = snapshot.node_value("tinc/dev_name").unwrap_or("tnc0");
        interfaces.push(desired_interface(
            &options.generated_root,
            VpnKind::Tinc,
            instance,
            device,
        )?);
    }

    for instance in snapshot.node_children("openvpn") {
        if !snapshot.node_enabled(&format!("openvpn/{instance}/enable")) {
            continue;
        }
        let suffix = format!("openvpn/{instance}/dev");
        let device = match snapshot.node_value(&suffix) {
            Some(value) if !value.is_empty() => value.to_owned(),
            _ => default_device_name(VpnKind::OpenVpn, &instance)?,
        };
        interfaces.push(desired_interface(
            &options.generated_root,
            VpnKind::OpenVpn,
            &instance,
            &device,
        )?);
    }

    for instance in snapshot.node_children("wireguard") {
        if !snapshot.node_enabled(&format!("wireguard/{instance}/enable")) {
            continue;
        }
        let suffix = format!("wireguard/{instance}/dev");
        let device = match snapshot.node_value(&suffix) {
            Some(value) if !value.is_empty() => value.to_owned(),
            _ => default_device_name(VpnKind::WireGuard, &instance)?,
        };
        interfaces.push(desired_interface(
            &options.generated_root,
            VpnKind::WireGuard,
            &instance,
            &device,
        )?);
    }

    interfaces.sort_by(|left, right| {
        (left.kind, left.instance.as_str()).cmp(&(right.kind, right.instance.as_str()))
    });
    let desired = DesiredState {
        node_id: snapshot.node_id.clone(),
        generated_root: options.generated_root.clone(),
        interfaces,
    };
    desired.validate()?;
    Ok(desired)
}

fn desired_interface(
    generated_root: &Path,
    kind: VpnKind,
    instance: &str,
    device: &str,
) -> Result<DesiredInterface> {
    validate_instance_name(instance)?;
    validate_device_name(device)?;
    Ok(DesiredInterface {
        kind,
        instance: instance.to_owned(),
        logical: logical_name(kind, instance)?,
        device: device.to_owned(),
        config: generated_config_path(generated_root, kind, instance)?,
    })
}

pub fn logical_name(kind: VpnKind, instance: &str) -> Result<String> {
    validate_instance_name(instance)?;
    let normalized = instance.replace('-', "_");
    let logical = format!("{}_{}", kind.logical_prefix(), normalized);
    validate_logical_name(&logical)?;
    Ok(logical)
}

pub fn generated_config_path(root: &Path, kind: VpnKind, instance: &str) -> Result<PathBuf> {
    validate_generated_root(root)?;
    validate_instance_name(instance)?;
    Ok(root
        .join(kind.as_str())
        .join(instance)
        .join(kind.config_filename()))
}

pub fn default_device_name(kind: VpnKind, instance: &str) -> Result<String> {
    validate_instance_name(instance)?;
    let (prefix, retained) = match kind {
        VpnKind::OpenVpn => ("ovpn", 5usize),
        VpnKind::WireGuard => ("wg", 7usize),
        VpnKind::Tinc => bail!("tinc has no instance-derived default device name"),
    };
    let candidate = format!("{prefix}-{instance}");
    if candidate.len() <= MAX_DEVICE_BYTES {
        validate_device_name(&candidate)?;
        return Ok(candidate);
    }
    let digest = Sha256::digest(format!("{}:{instance}", kind.as_str()).as_bytes());
    let suffix = format!("{:02x}{:02x}", digest[0], digest[1]);
    let short = &instance.as_bytes()[..retained.min(instance.len())];
    let short = std::str::from_utf8(short).context("instance name is not ASCII")?;
    let value = format!("{prefix}-{short}-{suffix}");
    validate_device_name(&value)?;
    Ok(value)
}

pub fn validate_node_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_NODE_ID_BYTES {
        bail!("node ID must contain 1..={MAX_NODE_ID_BYTES} bytes");
    }
    let mut bytes = value.bytes();
    let first = bytes.next().expect("checked non-empty");
    if !is_ascii_alnum(first) && first != b'_' {
        bail!("node ID must start with an ASCII letter, digit, or underscore");
    }
    if !bytes.all(|byte| is_ascii_alnum(byte) || matches!(byte, b'_' | b'.' | b'-')) {
        bail!("node ID contains unsafe characters: {value:?}");
    }
    Ok(())
}

pub fn validate_instance_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_INSTANCE_BYTES {
        bail!("instance name must contain 1..={MAX_INSTANCE_BYTES} bytes");
    }
    if value.starts_with('-') || matches!(value, "." | "..") {
        bail!("unsafe instance name: {value:?}");
    }
    if !value
        .bytes()
        .all(|byte| is_ascii_alnum(byte) || matches!(byte, b'_' | b'-'))
    {
        bail!("instance name contains unsafe characters: {value:?}");
    }
    Ok(())
}

/// Compatibility alias used by manifest/state modules.
pub fn validate_instance(value: &str) -> Result<()> {
    validate_instance_name(value)
}

pub fn validate_logical_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| is_ascii_alnum(byte) || byte == b'_')
    {
        bail!("unsafe logical interface identifier: {value:?}");
    }
    Ok(())
}

pub fn validate_device_name(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_DEVICE_BYTES {
        bail!("Linux interface name must contain 1..={MAX_DEVICE_BYTES} bytes");
    }
    if value.starts_with('-') || matches!(value, "lo" | "utun") {
        bail!("reserved or unsafe Linux interface name: {value:?}");
    }
    if !value
        .bytes()
        .all(|byte| is_ascii_alnum(byte) || matches!(byte, b'_' | b'.' | b'-'))
    {
        bail!("Linux interface name contains unsafe characters: {value:?}");
    }
    Ok(())
}

/// Compatibility alias used by manifest/state modules.
pub fn validate_device(value: &str) -> Result<()> {
    validate_device_name(value)
}

pub fn validate_generated_root(root: &Path) -> Result<()> {
    let value = root
        .to_str()
        .context("generated configuration root is not UTF-8")?;
    if !value.starts_with('/') || value == "/" || value.ends_with('/') || value.contains("//") {
        bail!("generated configuration root must be a normalized absolute path");
    }
    for component in value.trim_start_matches('/').split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || !component
                .bytes()
                .all(|byte| is_ascii_alnum(byte) || matches!(byte, b'_' | b'.' | b'-'))
        {
            bail!("unsafe generated configuration root: {value:?}");
        }
    }
    Ok(())
}

const fn is_ascii_alnum(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> FlatSnapshot {
        let node_id = "router-01";
        let mut node = BTreeMap::new();
        node.insert(format!("/nodes/{node_id}/tinc/enable"), "true".into());
        node.insert(format!("/nodes/{node_id}/tinc/dev_name"), "tnc0".into());
        node.insert(
            format!("/nodes/{node_id}/openvpn/long-office-name/enable"),
            "1".into(),
        );
        node.insert(
            format!("/nodes/{node_id}/wireguard/site-a/enable"),
            "yes".into(),
        );
        let global = BTreeMap::from([
            ("/global/mesh_type".into(), "tinc".into()),
            ("/global/tinc/netname".into(), "mesh".into()),
        ]);
        let all_nodes = node.clone();
        FlatSnapshot::new(node_id, node, global, all_nodes)
    }

    #[test]
    fn flattened_snapshot_builds_all_enabled_interfaces() {
        let desired = build_desired(&snapshot()).unwrap();
        assert_eq!(desired.interfaces.len(), 3);
        assert_eq!(
            desired.interface(VpnKind::Tinc, "mesh").unwrap().logical,
            "tinc_mesh"
        );
        let openvpn = desired
            .interface(VpnKind::OpenVpn, "long-office-name")
            .unwrap();
        assert_eq!(openvpn.logical, "ovpn_long_office_name");
        assert_eq!(openvpn.device.len(), 15);
        assert!(openvpn.device.starts_with("ovpn-long-"));
        assert_eq!(
            desired
                .interface(VpnKind::WireGuard, "site-a")
                .unwrap()
                .device,
            "wg-site-a"
        );
    }

    #[test]
    fn deterministic_device_suffix_matches_sha256_contract() {
        let first = default_device_name(VpnKind::OpenVpn, "long-office-name").unwrap();
        let second = default_device_name(VpnKind::OpenVpn, "long-office-name").unwrap();
        assert_eq!(first, second);
        let digest = Sha256::digest(b"openvpn:long-office-name");
        assert_eq!(
            first,
            format!("ovpn-long--{:02x}{:02x}", digest[0], digest[1])
        );
    }

    #[test]
    fn normalization_collision_is_rejected() {
        let mut value = snapshot();
        value.node.insert(
            "/nodes/router-01/openvpn/site_a/enable".into(),
            "true".into(),
        );
        value.node.insert(
            "/nodes/router-01/openvpn/site-a/enable".into(),
            "true".into(),
        );
        let error = build_desired(&value).unwrap_err().to_string();
        assert!(error.contains("duplicate desired interface identity"));
    }

    #[test]
    fn unsafe_and_reserved_names_are_rejected_before_planning() {
        assert!(validate_node_id("-router").is_err());
        assert!(validate_instance_name("office/../../wan").is_err());
        assert!(validate_device_name("utun").is_err());
        assert!(validate_device_name("interface-name-is-too-long").is_err());

        let mut value = snapshot();
        value.node.insert(
            "/nodes/router-01/wireguard/site-a/dev".into(),
            "utun".into(),
        );
        assert!(build_desired(&value).is_err());
    }

    #[test]
    fn serde_keeps_absolute_flattened_keys() {
        let value = snapshot();
        let encoded = serde_json::to_string(&value).unwrap();
        let decoded: FlatSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded.global_value("mesh_type"), Some("tinc"));
        assert_eq!(
            decoded.node_children("openvpn"),
            BTreeSet::from(["long-office-name".into()])
        );
    }

    #[test]
    fn flattened_snapshot_limits_each_value_and_aggregate_bytes() {
        let mut value = snapshot();
        value.node.insert(
            "/nodes/router-01/openvpn/site/extra_config".into(),
            "x".repeat(MAX_FLATTENED_VALUE_BYTES + 1),
        );
        assert!(value.validate().is_err());

        let chunk = "x".repeat(1024 * 1024);
        let node = (0..5)
            .map(|index| (format!("/nodes/router-01/test/{index}"), chunk.clone()))
            .collect();
        assert!(validate_flattened_maps(&node, &BTreeMap::new(), &BTreeMap::new()).is_err());
    }

    #[test]
    fn flattened_snapshot_limits_aggregate_entry_count() {
        let node = (0..=MAX_FLATTENED_ENTRIES)
            .map(|index| (format!("/nodes/router-01/test/{index}"), String::new()))
            .collect();
        assert!(validate_flattened_maps(&node, &BTreeMap::new(), &BTreeMap::new()).is_err());
    }
}

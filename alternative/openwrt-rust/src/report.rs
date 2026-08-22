use anyhow::{Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::atomic;
use crate::command::Runner;
use crate::runtime::Runtime;
use crate::state::{InterfaceKind, ManifestEntry, Paths, read_manifest, regular_file_exists};

const MAX_REPORTED_FILE_BYTES: usize = 1024 * 1024;
const MAX_DAEMON_STATUS_BYTES: usize = 64 * 1024;
const MAX_TINC_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_TINC_DUMP_BYTES: usize = 1024 * 1024;
const MAX_FRR_STATUS_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EtcdStatus {
    pub version: u32,
    pub state: String,
    pub node_id: String,
    pub commit: Option<String>,
    pub updated_at: String,
}

impl EtcdStatus {
    pub fn new(state: &str, node_id: &str, commit: Option<String>) -> Self {
        Self {
            version: 1,
            state: state.into(),
            node_id: node_id.into(),
            commit,
            updated_at: timestamp(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterfaceStatus {
    pub kind: String,
    pub instance: String,
    pub logical: String,
    pub device: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TincPeerStatus {
    pub network: String,
    pub peer: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrrPeerStatus {
    pub protocol: String,
    pub peer: String,
    pub state: String,
    pub detail: String,
    pub remote_as: Option<u64>,
    pub interface: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalStatus {
    pub node_id: Option<String>,
    pub observed_at: String,
    pub etcd: EtcdStatus,
    pub interfaces: BTreeMap<String, String>,
    pub interface_details: Vec<InterfaceStatus>,
    pub tinc_peers: Vec<TincPeerStatus>,
    pub frr: String,
    pub frr_peers: Vec<FrrPeerStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct Reported {
    version: u32,
    node_id: String,
    interfaces: BTreeSet<(String, String)>,
}

pub fn collect<R: Runner>(paths: &Paths, runner: &R) -> Result<LocalStatus> {
    let mut etcd = read_etcd_status(paths)?.unwrap_or_else(|| EtcdStatus {
        version: 1,
        state: "unknown".into(),
        node_id: String::new(),
        commit: None,
        updated_at: timestamp(),
    });
    // The UCI flag is the administrative source of truth. It also closes the
    // short restart window where an old connected status file may still be
    // present while procd is stopping the daemon and starting disabled-mode
    // cleanup. Failure to query UCI is non-fatal; the durable daemon status
    // remains the fallback for recovery and test environments.
    if configured_controller_enabled(runner) == Some(false) && etcd.state != "disabled" {
        etcd = EtcdStatus::new("disabled", &etcd.node_id, None);
    }

    // A stopped or administratively disabled controller no longer owns a
    // live observation cycle. Do not resurrect stale VPN/FRR state from an
    // interrupted purge, an old manifest, or processes that are still
    // winding down. The next daemon start replaces this record with
    // waiting/connecting before status collection becomes active again.
    if matches!(etcd.state.as_str(), "disabled" | "stopped") {
        let frr = if etcd.state == "disabled" {
            "disabled"
        } else {
            "down"
        };
        return Ok(LocalStatus {
            node_id: (!etcd.node_id.is_empty()).then(|| etcd.node_id.clone()),
            observed_at: timestamp(),
            etcd,
            interfaces: BTreeMap::new(),
            interface_details: Vec::new(),
            tinc_peers: Vec::new(),
            frr: frr.into(),
            frr_peers: Vec::new(),
        });
    }

    let runtime = Runtime::new(paths.clone(), runner.clone());
    let mut interfaces = BTreeMap::new();
    let mut interface_details = Vec::new();
    let mut tinc_peers = Vec::new();
    for entry in status_inventory(paths)? {
        if entry.kind == InterfaceKind::Tinc {
            match tinc_peer_states(paths, &runtime, runner, &entry) {
                Ok(peers) => {
                    for peer in peers {
                        interfaces.insert(format!("tinc/{}", peer.peer), peer.state.clone());
                        tinc_peers.push(peer);
                    }
                }
                Err(error) => tracing::warn!(
                    instance = %entry.instance,
                    "could not inspect Tinc peer status: {error:#}"
                ),
            }
            continue;
        }
        let state = match interface_state(&runtime, runner, &entry) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    kind = entry.kind.as_str(),
                    instance = %entry.instance,
                    "could not inspect managed interface status: {error:#}"
                );
                "unavailable".into()
            }
        };
        let name = format!("{}/{}", entry.kind.as_str(), entry.instance);
        interfaces.insert(name, state.clone());
        interface_details.push(InterfaceStatus {
            kind: entry.kind.as_str().into(),
            instance: entry.instance,
            logical: entry.logical,
            device: entry.device,
            state,
        });
    }
    tinc_peers
        .sort_by(|left, right| (&left.network, &left.peer).cmp(&(&right.network, &right.peer)));
    let frr_running = process_name_running(&["zebra", "watchfrr"]);
    let frr = if frr_running { "up" } else { "down" };
    let frr_peers = if frr_running {
        collect_frr_peers(runner)
    } else {
        Vec::new()
    };
    for peer in &frr_peers {
        interfaces.insert(
            format!("frr/{}/{}", peer.protocol, peer.peer),
            peer.state.clone(),
        );
    }
    Ok(LocalStatus {
        node_id: (!etcd.node_id.is_empty()).then(|| etcd.node_id.clone()),
        observed_at: timestamp(),
        etcd,
        interfaces,
        interface_details,
        tinc_peers,
        frr: frr.into(),
        frr_peers,
    })
}

fn configured_controller_enabled<R: Runner>(runner: &R) -> Option<bool> {
    let output = runner
        .output("uci", ["-q", "get", "meduza.main.enable"])
        .ok()?;
    if !output.status.success() || output.stdout.len() > 16 {
        return None;
    }
    match String::from_utf8(output.stdout).ok()?.trim() {
        "1" | "true" | "on" | "yes" | "enabled" => Some(true),
        "0" | "false" | "off" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

fn collect_frr_peers<R: Runner>(runner: &R) -> Vec<FrrPeerStatus> {
    let mut peers = BTreeMap::<(String, String), FrrPeerStatus>::new();
    collect_frr_command(
        runner,
        "show bgp summary json",
        parse_bgp_summary,
        &mut peers,
    );
    collect_frr_command(
        runner,
        "show ip ospf neighbor json",
        parse_ospf_neighbors,
        &mut peers,
    );
    peers.into_values().collect()
}

fn collect_frr_command<R: Runner>(
    runner: &R,
    command: &str,
    parser: fn(&[u8]) -> Result<Vec<FrrPeerStatus>>,
    peers: &mut BTreeMap<(String, String), FrrPeerStatus>,
) {
    let output = match runner.output("vtysh", ["-c", command]) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            // A running zebra/watchfrr does not imply bgpd and ospfd are both
            // enabled. vtysh returns status 1 for an unavailable protocol;
            // that is an empty peer set, not a controller fault.
            tracing::debug!(command, status = %output.status, "FRR protocol status is unavailable");
            return;
        }
        Err(error) => {
            tracing::warn!(command, "could not query FRR peer status: {error:#}");
            return;
        }
    };
    match parser(&output.stdout) {
        Ok(values) => {
            for value in values {
                peers.insert((value.protocol.clone(), value.peer.clone()), value);
            }
        }
        Err(error) => tracing::warn!(command, "could not parse FRR peer status: {error:#}"),
    }
}

fn parse_frr_json(bytes: &[u8]) -> Result<serde_json::Value> {
    if bytes.len() > MAX_FRR_STATUS_BYTES {
        bail!("FRR status response is too large");
    }
    Ok(serde_json::from_slice(bytes)?)
}

fn parse_bgp_summary(bytes: &[u8]) -> Result<Vec<FrrPeerStatus>> {
    let value = parse_frr_json(bytes)?;
    let mut peers = BTreeMap::new();
    walk_bgp_summary(&value, &mut peers);
    Ok(peers.into_values().collect())
}

fn walk_bgp_summary(value: &serde_json::Value, peers: &mut BTreeMap<String, FrrPeerStatus>) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::Object(values)) = object.get("peers") {
                for (peer, data) in values {
                    let Some(data) = data.as_object() else {
                        continue;
                    };
                    let detail = json_string(data, &["state", "peerState"])
                        .unwrap_or_else(|| "Unknown".into());
                    let state = normalize_bgp_state(&detail).into();
                    peers.insert(
                        peer.clone(),
                        FrrPeerStatus {
                            protocol: "bgp".into(),
                            peer: peer.clone(),
                            state,
                            detail,
                            remote_as: data.get("remoteAs").and_then(serde_json::Value::as_u64),
                            interface: None,
                        },
                    );
                }
            }
            for child in object.values() {
                walk_bgp_summary(child, peers);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                walk_bgp_summary(child, peers);
            }
        }
        _ => {}
    }
}

fn parse_ospf_neighbors(bytes: &[u8]) -> Result<Vec<FrrPeerStatus>> {
    let value = parse_frr_json(bytes)?;
    let mut peers = BTreeMap::new();
    walk_ospf_neighbors(&value, &mut peers);
    Ok(peers.into_values().collect())
}

fn walk_ospf_neighbors(value: &serde_json::Value, peers: &mut BTreeMap<String, FrrPeerStatus>) {
    match value {
        serde_json::Value::Object(object) => {
            let detail = json_string(object, &["nbrState", "neighborState"]);
            let peer = json_string(
                object,
                &["neighborId", "neighborID", "routerId", "routerID"],
            );
            if let (Some(peer), Some(detail)) = (peer, detail) {
                let interface = json_string(
                    object,
                    &["ifaceName", "interfaceName", "interface", "ifName"],
                );
                peers.insert(
                    peer.clone(),
                    FrrPeerStatus {
                        protocol: "ospf".into(),
                        peer,
                        state: normalize_ospf_state(&detail).into(),
                        detail,
                        remote_as: None,
                        interface,
                    },
                );
            }
            for child in object.values() {
                walk_ospf_neighbors(child, peers);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                walk_ospf_neighbors(child, peers);
            }
        }
        _ => {}
    }
}

fn json_string(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        object.get(*key).and_then(|value| match value {
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
    })
}

fn normalize_bgp_state(value: &str) -> &'static str {
    let normalized = value.to_ascii_lowercase();
    if normalized == "established" || normalized == "ok" {
        "up"
    } else if normalized.contains("idle")
        || normalized.contains("shutdown")
        || normalized.contains("deleted")
    {
        "down"
    } else {
        "connecting"
    }
}

fn normalize_ospf_state(value: &str) -> &'static str {
    let normalized = value.to_ascii_lowercase();
    if normalized.starts_with("full") {
        "up"
    } else if normalized.starts_with("down") || normalized.starts_with("attempt") {
        "down"
    } else {
        "connecting"
    }
}

fn tinc_peer_states<R: Runner>(
    paths: &Paths,
    runtime: &Runtime<R>,
    runner: &R,
    entry: &ManifestEntry,
) -> Result<Vec<TincPeerStatus>> {
    let directory = entry
        .config
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Tinc config has no parent directory"))?;
    let local_name = read_tinc_local_name(&entry.config)?;
    let peers = read_tinc_peer_names(&directory.join("hosts"), &local_name)?;
    if peers.is_empty() {
        return Ok(Vec::new());
    }

    let common_state = if !regular_config_exists(&entry.config)?
        || !runtime.status_interface_owned(entry)?
        || !runtime.link_up(&entry.device)
        || !runtime.process_running(entry)
    {
        Some("down")
    } else {
        None
    };
    let reachable = if common_state.is_none() {
        let args = vec![
            "-c".to_owned(),
            directory.display().to_string(),
            "-n".to_owned(),
            entry.instance.clone(),
            format!(
                "--pidfile={}",
                paths
                    .runtime
                    .join(format!("tinc.{}.pid", entry.instance))
                    .display()
            ),
            "dump".to_owned(),
            "reachable".to_owned(),
            "nodes".to_owned(),
        ];
        match runner.output("tinc", args) {
            Ok(output) if output.status.success() => {
                Some(parse_reachable_tinc_nodes(&output.stdout)?)
            }
            Ok(_) | Err(_) => None,
        }
    } else {
        None
    };

    Ok(peers
        .into_iter()
        .map(|peer| {
            let state = if let Some(state) = common_state {
                state
            } else if let Some(reachable) = &reachable {
                if reachable.contains(&peer) {
                    "up"
                } else {
                    "down"
                }
            } else {
                "unavailable"
            };
            TincPeerStatus {
                network: entry.instance.clone(),
                peer,
                state: state.into(),
            }
        })
        .collect())
}

fn read_tinc_local_name(config: &std::path::Path) -> Result<String> {
    let bytes = atomic::read_bounded(config, MAX_TINC_CONFIG_BYTES)?;
    let text = std::str::from_utf8(&bytes)?;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("Name") {
            let value = value.trim();
            validate_tinc_host_name(value)?;
            return Ok(value.into());
        }
    }
    bail!("Tinc config does not contain a valid local Name");
}

fn read_tinc_peer_names(hosts: &std::path::Path, local_name: &str) -> Result<Vec<String>> {
    let metadata = fs::symlink_metadata(hosts)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Tinc hosts path is not a real directory");
    }
    let mut peers = Vec::new();
    for child in fs::read_dir(hosts)? {
        let child = child?;
        let metadata = fs::symlink_metadata(child.path())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("Tinc hosts directory contains a non-regular entry");
        }
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Tinc peer name is not UTF-8"))?;
        validate_tinc_host_name(&name)?;
        if name != local_name {
            peers.push(name);
        }
    }
    peers.sort();
    peers.dedup();
    Ok(peers)
}

fn parse_reachable_tinc_nodes(bytes: &[u8]) -> Result<BTreeSet<String>> {
    if bytes.len() > MAX_TINC_DUMP_BYTES {
        bail!("Tinc node dump is too large");
    }
    let text = std::str::from_utf8(bytes)?;
    let mut nodes = BTreeSet::new();
    for line in text.lines() {
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        validate_tinc_host_name(name)?;
        nodes.insert(name.into());
    }
    Ok(nodes)
}

fn validate_tinc_host_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("invalid Tinc host name");
    }
    Ok(())
}

/// A pending manifest is the best status view during reconciliation. It is
/// durably published before runtime mutation and remains available when an
/// apply fails, while the stable manifest is published only at the commit
/// point.
fn status_inventory(paths: &Paths) -> Result<Vec<ManifestEntry>> {
    if regular_file_exists(&paths.pending_manifest)? {
        read_manifest(&paths.pending_manifest)
    } else {
        read_manifest(&paths.manifest)
    }
}

pub fn persist_etcd_status(paths: &Paths, value: &EtcdStatus) -> Result<()> {
    atomic::atomic_json_bounded(&paths.daemon_status, value, MAX_DAEMON_STATUS_BYTES).map(|_| ())
}

/// Publish the state of an administratively disabled controller without
/// loading or validating etcd credentials. This path is used after purge, so
/// it recreates only `/var/run/meduza`; no persistent directory or cache is
/// created. A malformed/missing NODE_ID is represented as an empty identity
/// rather than preventing the controller from being disabled safely.
pub fn persist_disabled_status<R: Runner>(paths: &Paths, runner: &R) -> Result<()> {
    let node_id = match runner.output("uci", ["-q", "get", "meduza.main.NODE_ID"]) {
        Ok(output) if output.status.success() && output.stdout.len() <= 129 => {
            String::from_utf8(output.stdout)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| crate::config::validate_node_id(value).is_ok())
                .unwrap_or_default()
        }
        _ => String::new(),
    };
    atomic::ensure_private_dir(&paths.runtime, 0o700)?;
    persist_etcd_status(paths, &EtcdStatus::new("disabled", &node_id, None))
}

fn read_etcd_status(paths: &Paths) -> Result<Option<EtcdStatus>> {
    if !crate::state::regular_file_exists(&paths.daemon_status)? {
        return Ok(None);
    }
    let bytes = atomic::read_bounded(&paths.daemon_status, MAX_DAEMON_STATUS_BYTES)?;
    let value: EtcdStatus = serde_json::from_slice(&bytes)?;
    if value.version != 1
        || !matches!(
            value.state.as_str(),
            "waiting" | "connecting" | "connected" | "error" | "stopped" | "disabled"
        )
    {
        bail!("invalid daemon status record");
    }
    Ok(Some(value))
}

pub fn print_status<R: Runner>(paths: &Paths, runner: &R, json: bool) -> Result<()> {
    let status = collect(paths, runner)?;
    if json {
        println!("{}", serde_json::to_string(&status)?);
    } else {
        println!("etcd: {}", status.etcd.state);
        for (name, value) in status.interfaces {
            println!("{name}: {value}");
        }
        println!("frr/default: {}", status.frr);
        for peer in status.frr_peers {
            println!(
                "frr/{}/{}: {} ({})",
                peer.protocol, peer.peer, peer.state, peer.detail
            );
        }
    }
    Ok(())
}

pub fn persist_reported(paths: &Paths, node_id: &str, entries: &[(String, String)]) -> Result<()> {
    let value = Reported {
        version: 1,
        node_id: node_id.into(),
        interfaces: entries.iter().cloned().collect(),
    };
    atomic::atomic_json_bounded(&paths.reported, &value, MAX_REPORTED_FILE_BYTES)?;
    Ok(())
}

pub fn read_reported(paths: &Paths, node_id: &str) -> Result<BTreeSet<(String, String)>> {
    if !crate::state::regular_file_exists(&paths.reported)? {
        return Ok(BTreeSet::new());
    }
    let bytes = atomic::read_bounded(&paths.reported, MAX_REPORTED_FILE_BYTES)?;
    let value: Reported = serde_json::from_slice(&bytes)?;
    if value.version != 1 || value.node_id != node_id {
        bail!("reported-state identity changed");
    }
    Ok(value.interfaces)
}

fn interface_state<R: Runner>(
    runtime: &Runtime<R>,
    runner: &R,
    entry: &ManifestEntry,
) -> Result<String> {
    match entry.kind {
        InterfaceKind::Wireguard => {
            if !regular_config_exists(&entry.config)?
                || !runtime.interface_owned(entry)?
                || !runtime.link_up(&entry.device)
            {
                return Ok("down".into());
            }
            let output =
                match runner.output("wg", ["show", entry.device.as_str(), "latest-handshakes"]) {
                    Ok(output) => output,
                    Err(_) => return Ok("unavailable".into()),
                };
            if !output.status.success() {
                return Ok("unavailable".into());
            }
            let latest = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .filter_map(|value| value.parse::<i64>().ok())
                .max()
                .unwrap_or(0);
            if latest > 0 && Utc::now().timestamp() - latest <= 180 {
                Ok("up".into())
            } else {
                Ok("connecting".into())
            }
        }
        InterfaceKind::Openvpn => {
            if !regular_config_exists(&entry.config)? || !runtime.status_interface_owned(entry)? {
                return Ok("down".into());
            }
            let process = runtime.process_running(entry);
            if runtime.link_up(&entry.device) && process {
                Ok("up".into())
            } else if process {
                Ok("connecting".into())
            } else {
                Ok("down".into())
            }
        }
        InterfaceKind::Tinc => Ok(
            if regular_config_exists(&entry.config)?
                && runtime.status_interface_owned(entry)?
                && runtime.link_up(&entry.device)
                && runtime.process_running(entry)
            {
                "up".into()
            } else {
                "down".into()
            },
        ),
    }
}

fn regular_config_exists(path: &std::path::Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S+0000").to_string()
}

#[cfg(target_os = "linux")]
fn process_name_running(names: &[&str]) -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    processes.filter_map(Result::ok).any(|entry| {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|value| value.is_ascii_digit())
        {
            return false;
        }
        fs::read_link(entry.path().join("exe"))
            .ok()
            .and_then(|path| crate::runtime::proc_exe_basename(&path))
            .is_some_and(|name| names.contains(&name.as_str()))
    })
}

#[cfg(not(target_os = "linux"))]
fn process_name_running(_names: &[&str]) -> bool {
    false
}

pub fn validate_status_value(value: &str) -> Result<()> {
    if matches!(value, "up" | "down" | "connecting" | "unavailable") {
        Ok(())
    } else {
        bail!("invalid status value")
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::process::{ExitStatus, Output};

    use super::*;
    use crate::state::write_manifest;

    #[derive(Clone, Copy)]
    struct UnavailableUci;

    impl Runner for UnavailableUci {
        fn output<I, S>(&self, _program: &str, _args: I) -> Result<Output>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            bail!("UCI unavailable")
        }
    }

    #[derive(Clone, Copy)]
    struct DisabledUci;

    impl Runner for DisabledUci {
        fn output<I, S>(&self, program: &str, args: I) -> Result<Output>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            let args = args
                .into_iter()
                .map(|value| value.as_ref().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(program, "uci");
            assert_eq!(args, ["-q", "get", "meduza.main.enable"]);
            Ok(Output {
                status: successful_exit_status(),
                stdout: b"0\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[cfg(unix)]
    fn successful_exit_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn successful_exit_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    fn manifest_entry(instance: &str) -> ManifestEntry {
        ManifestEntry {
            kind: InterfaceKind::Openvpn,
            instance: instance.into(),
            logical: format!("ovpn_{instance}"),
            device: format!("ovpn-{instance}"),
            config: format!("/var/run/meduza/generated/openvpn/{instance}/openvpn.conf").into(),
        }
    }

    #[test]
    fn daemon_status_round_trips_without_using_persistent_storage() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let status = EtcdStatus {
            version: 1,
            state: "connected".into(),
            node_id: "router-01".into(),
            commit: Some("release 42+canary".into()),
            updated_at: "2026-08-22T12:00:00+0000".into(),
        };

        persist_etcd_status(&paths, &status).unwrap();
        assert_eq!(read_etcd_status(&paths).unwrap(), Some(status));
        assert!(paths.daemon_status.starts_with(&paths.runtime));
    }

    #[test]
    fn daemon_status_rejects_unknown_states() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        atomic::atomic_write(
            &paths.daemon_status,
            br#"{"version":1,"state":"owned","node_id":"router-01","commit":null,"updated_at":"now"}"#,
            0o600,
        )
        .unwrap();

        assert!(read_etcd_status(&paths).is_err());
    }

    #[test]
    fn disabled_status_needs_only_volatile_storage_and_no_valid_settings() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));

        persist_disabled_status(&paths, &UnavailableUci).unwrap();

        let status = read_etcd_status(&paths).unwrap().unwrap();
        assert_eq!(status.state, "disabled");
        assert!(status.node_id.is_empty());
        assert!(status.commit.is_none());
        assert!(!paths.data.exists());
        assert!(!paths.state.exists());
        assert!(paths.daemon_status.is_file());

        // Even if stale persistent inventory reappears after an interrupted
        // cleanup, an administratively disabled controller must never expose
        // its old VPN/FRR observations as live status.
        atomic::ensure_private_dir(&paths.managed, 0o700).unwrap();
        write_manifest(&paths.manifest, &[manifest_entry("stale")]).unwrap();
        let collected = collect(&paths, &UnavailableUci).unwrap();
        assert_eq!(collected.etcd.state, "disabled");
        assert!(collected.interfaces.is_empty());
        assert!(collected.interface_details.is_empty());
        assert!(collected.tinc_peers.is_empty());
        assert_eq!(collected.frr, "disabled");
        assert!(collected.frr_peers.is_empty());
    }

    #[test]
    fn disabled_uci_flag_overrides_a_stale_connected_record() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        persist_etcd_status(
            &paths,
            &EtcdStatus::new("connected", "router-01", Some("generation-1".into())),
        )
        .unwrap();
        write_manifest(&paths.manifest, &[manifest_entry("stale")]).unwrap();

        let collected = collect(&paths, &DisabledUci).unwrap();

        assert_eq!(collected.etcd.state, "disabled");
        assert!(collected.etcd.commit.is_none());
        assert!(collected.interfaces.is_empty());
        assert!(collected.interface_details.is_empty());
        assert_eq!(collected.frr, "disabled");
    }

    #[test]
    fn pending_manifest_is_visible_before_apply_commit() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let stable = manifest_entry("stable");
        let pending = manifest_entry("pending");
        write_manifest(&paths.manifest, std::slice::from_ref(&stable)).unwrap();
        write_manifest(&paths.pending_manifest, std::slice::from_ref(&pending)).unwrap();

        assert_eq!(status_inventory(&paths).unwrap(), vec![pending]);
        atomic::durable_remove(&paths.pending_manifest).unwrap();
        assert_eq!(status_inventory(&paths).unwrap(), vec![stable]);
    }

    #[test]
    fn reported_state_reader_rejects_an_oversized_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let file = fs::File::create(&paths.reported).unwrap();
        file.set_len((MAX_REPORTED_FILE_BYTES + 1) as u64).unwrap();

        assert!(read_reported(&paths, "router-01").is_err());
    }

    #[test]
    fn tinc_peer_inventory_excludes_local_host() {
        let temp = tempfile::tempdir().unwrap();
        let hosts = temp.path().join("hosts");
        fs::create_dir(&hosts).unwrap();
        fs::write(hosts.join("local_node"), "local").unwrap();
        fs::write(hosts.join("remote_b"), "remote").unwrap();
        fs::write(hosts.join("remote_a"), "remote").unwrap();

        assert_eq!(
            read_tinc_peer_names(&hosts, "local_node").unwrap(),
            vec!["remote_a", "remote_b"]
        );
    }

    #[test]
    fn tinc_reachable_dump_uses_node_name_column() {
        let reachable = parse_reachable_tinc_nodes(
            b"remote_a id abc at 192.0.2.1 port 655 status 001f\n\
              remote_b id def at 192.0.2.2 port 655 status 001f\n",
        )
        .unwrap();

        assert!(reachable.contains("remote_a"));
        assert!(reachable.contains("remote_b"));
        assert_eq!(reachable.len(), 2);
    }

    #[test]
    fn tinc_local_name_is_read_from_generated_config() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("tinc.conf");
        fs::write(&config, "Mode=switch\nName = local_node\n").unwrap();

        assert_eq!(read_tinc_local_name(&config).unwrap(), "local_node");
    }

    #[test]
    fn bgp_summary_reports_each_peer_and_remote_as() {
        let peers = parse_bgp_summary(
            br#"{
                "ipv4Unicast": {
                    "peers": {
                        "10.20.0.2": { "remoteAs": 65002, "state": "Established" },
                        "10.30.0.2": { "remoteAs": 65003, "state": "Active" }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].peer, "10.20.0.2");
        assert_eq!(peers[0].state, "up");
        assert_eq!(peers[0].remote_as, Some(65002));
        assert_eq!(peers[1].state, "connecting");
    }

    #[test]
    fn ospf_neighbors_report_router_state_and_interface() {
        let peers = parse_ospf_neighbors(
            br#"{
                "default": {
                    "neighbors": [
                        {
                            "routerId": "10.255.0.2",
                            "nbrState": "Full/DR",
                            "ifaceName": "wg-backbone:10.30.0.1"
                        },
                        {
                            "routerId": "10.255.0.3",
                            "nbrState": "ExStart/BDR",
                            "ifaceName": "tnc0:10.10.0.1"
                        }
                    ]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].peer, "10.255.0.2");
        assert_eq!(peers[0].state, "up");
        assert_eq!(peers[0].interface.as_deref(), Some("wg-backbone:10.30.0.1"));
        assert_eq!(peers[1].state, "connecting");
    }
}

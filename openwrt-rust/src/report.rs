use anyhow::{Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::atomic;
use crate::command::Runner;
use crate::runtime::Runtime;
use crate::state::{InterfaceKind, ManifestEntry, Paths, read_manifest};

const MAX_REPORTED_FILE_BYTES: usize = 1024 * 1024;
const MAX_DAEMON_STATUS_BYTES: usize = 64 * 1024;

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
pub struct LocalStatus {
    pub node_id: Option<String>,
    pub observed_at: String,
    pub etcd: EtcdStatus,
    pub interfaces: BTreeMap<String, String>,
    pub interface_details: Vec<InterfaceStatus>,
    pub frr: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct Reported {
    version: u32,
    node_id: String,
    interfaces: BTreeSet<(String, String)>,
}

pub fn collect<R: Runner>(paths: &Paths, runner: &R) -> Result<LocalStatus> {
    let runtime = Runtime::new(paths.clone(), runner.clone());
    let mut interfaces = BTreeMap::new();
    let mut interface_details = Vec::new();
    for entry in read_manifest(&paths.manifest)? {
        let state = interface_state(&runtime, runner, &entry)?;
        let name = if entry.kind == InterfaceKind::Tinc {
            "tinc/default".to_owned()
        } else {
            format!("{}/{}", entry.kind.as_str(), entry.instance)
        };
        interfaces.insert(name, state.clone());
        interface_details.push(InterfaceStatus {
            kind: entry.kind.as_str().into(),
            instance: entry.instance,
            logical: entry.logical,
            device: entry.device,
            state,
        });
    }
    interfaces
        .entry("tinc/default".into())
        .or_insert_with(|| "down".into());
    let frr = if process_name_running(&["zebra", "watchfrr"]) {
        "up"
    } else {
        "down"
    };
    let etcd = read_etcd_status(paths)?.unwrap_or_else(|| EtcdStatus {
        version: 1,
        state: "unknown".into(),
        node_id: String::new(),
        commit: None,
        updated_at: timestamp(),
    });
    Ok(LocalStatus {
        node_id: (!etcd.node_id.is_empty()).then(|| etcd.node_id.clone()),
        observed_at: timestamp(),
        etcd,
        interfaces,
        interface_details,
        frr: frr.into(),
    })
}

pub fn persist_etcd_status(paths: &Paths, value: &EtcdStatus) -> Result<()> {
    atomic::atomic_json_bounded(&paths.daemon_status, value, MAX_DAEMON_STATUS_BYTES).map(|_| ())
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
            "waiting" | "connecting" | "connected" | "error" | "stopped"
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
    use super::*;

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
    fn reported_state_reader_rejects_an_oversized_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let file = fs::File::create(&paths.reported).unwrap();
        file.set_len((MAX_REPORTED_FILE_BYTES + 1) as u64).unwrap();

        assert!(read_reported(&paths, "router-01").is_err());
    }
}

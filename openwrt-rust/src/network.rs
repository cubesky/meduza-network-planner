use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::OWNER;
use crate::atomic;
use crate::command::{Runner, command_exists};
use crate::model::{validate_device, validate_instance, validate_logical_name};
use crate::state::{ManifestEntry, Paths};

const MAX_NETWORK_STATE_BYTES: usize = 1024 * 1024;
const MAX_IFSTATUS_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct NetworkState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    records: BTreeMap<String, NetworkRecord>,
    #[serde(default)]
    reload_pending: bool,
}

const fn state_version() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum NetworkPhase {
    Creating,
    Owned,
    Deleting,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NetworkRecord {
    nonce: String,
    phase: NetworkPhase,
    entry: ManifestEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NetworkLive {
    section_type: String,
    proto: Option<String>,
    device: Option<String>,
    auto: Option<String>,
    defaultroute: Option<String>,
    peerdns: Option<String>,
    delegate: Option<String>,
    owner: Option<String>,
    nonce: Option<String>,
    kind: Option<String>,
    instance: Option<String>,
}

pub struct NetworkInterfaces<R: Runner> {
    paths: Paths,
    runner: R,
}

impl<R: Runner> NetworkInterfaces<R> {
    pub fn new(paths: Paths, runner: R) -> Self {
        Self { paths, runner }
    }

    /// Fail closed on a user-owned section before any reconciliation mutation.
    pub fn validate(&self, desired: &[ManifestEntry]) -> Result<()> {
        if self.paths.root.is_none() {
            for command in ["uci", "ubus", "ifup", "ifdown", "ifstatus"] {
                if !command_exists(command) {
                    bail!("managed network interfaces require missing command: {command}");
                }
            }
        }
        self.ensure_default_delta_clean()?;
        let state = NetworkState::load(&self.paths)?;
        let mut names = BTreeSet::new();
        for entry in desired {
            validate_entry(entry)?;
            if !names.insert(entry.logical.clone()) {
                bail!("duplicate managed network interface: {}", entry.logical);
            }
            let live = self.capture(&entry.logical)?;
            match state.records.get(&entry.logical) {
                Some(record) if live.as_ref().is_some_and(|live| live_exact(live, record)) => {}
                Some(_) if live.is_none() => {}
                Some(_) => bail!(
                    "managed network interface changed outside Meduza: {}",
                    entry.logical
                ),
                None if live.is_none() => {}
                None => bail!(
                    "network interface namespace is already occupied: {}",
                    entry.logical
                ),
            }
        }
        Ok(())
    }

    /// Create or recover all desired interface descriptions. Stale sections
    /// remain until firewall membership has been removed by the caller.
    pub fn ensure(&self, desired: &[ManifestEntry]) -> Result<()> {
        self.validate(desired)?;
        let mut state = NetworkState::load(&self.paths)?;
        for entry in desired {
            if state
                .records
                .get(&entry.logical)
                .is_some_and(|record| record.entry != *entry)
            {
                self.remove_record(&mut state, &entry.logical)?;
            }
            if !state.records.contains_key(&entry.logical) {
                let record = NetworkRecord {
                    nonce: atomic::random_nonce(),
                    phase: NetworkPhase::Creating,
                    entry: entry.clone(),
                };
                state.records.insert(entry.logical.clone(), record);
                state.save(&self.paths)?;
            }
            self.ensure_record(&mut state, &entry.logical)?;
        }
        self.reload_if_pending(&mut state)
    }

    /// Remove every interface description not present in `desired`. Call this
    /// only after the firewall controller has retired stale zone membership.
    pub fn prune(&self, desired: &[ManifestEntry]) -> Result<()> {
        let desired = desired
            .iter()
            .map(|entry| entry.logical.clone())
            .collect::<BTreeSet<_>>();
        let mut state = NetworkState::load(&self.paths)?;
        for logical in state.records.keys().cloned().collect::<Vec<_>>() {
            if !desired.contains(&logical) {
                self.remove_record(&mut state, &logical)?;
            }
        }
        self.reload_if_pending(&mut state)
    }

    /// Bring the `proto none` wrappers up only after the daemon-owned Linux
    /// devices exist. netifd represents status but never creates the VPN.
    pub fn activate(&self, entries: &[ManifestEntry]) -> Result<()> {
        let state = NetworkState::load(&self.paths)?;
        for entry in entries {
            let record = state
                .records
                .get(&entry.logical)
                .with_context(|| format!("network ownership is missing: {}", entry.logical))?;
            let live = self
                .capture(&entry.logical)?
                .with_context(|| format!("network interface is missing: {}", entry.logical))?;
            if record.phase != NetworkPhase::Owned
                || record.entry != *entry
                || !live_exact(&live, record)
            {
                bail!("network interface is not exactly owned: {}", entry.logical);
            }
            if !self.interface_up(&entry.logical)? {
                self.runner.status("ifup", [entry.logical.as_str()])?;
            }
        }
        Ok(())
    }

    fn ensure_record(&self, state: &mut NetworkState, logical: &str) -> Result<()> {
        let record = state
            .records
            .get(logical)
            .cloned()
            .context("network ownership record disappeared")?;
        let live = self.capture(logical)?;
        match record.phase {
            NetworkPhase::Creating if live.is_none() => self.commit_create(state, &record),
            NetworkPhase::Creating
                if live.as_ref().is_some_and(|live| live_exact(live, &record)) =>
            {
                state.records.get_mut(logical).expect("record exists").phase = NetworkPhase::Owned;
                state.reload_pending = true;
                state.save(&self.paths)
            }
            NetworkPhase::Owned if live.as_ref().is_some_and(|live| live_exact(live, &record)) => {
                Ok(())
            }
            NetworkPhase::Owned if live.is_none() => {
                let replacement = NetworkRecord {
                    nonce: atomic::random_nonce(),
                    phase: NetworkPhase::Creating,
                    entry: record.entry,
                };
                state
                    .records
                    .insert(logical.to_owned(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_create(state, &replacement)
            }
            NetworkPhase::Deleting if live.is_none() => {
                let replacement = NetworkRecord {
                    nonce: atomic::random_nonce(),
                    phase: NetworkPhase::Creating,
                    entry: record.entry,
                };
                state
                    .records
                    .insert(logical.to_owned(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_create(state, &replacement)
            }
            NetworkPhase::Deleting
                if live.as_ref().is_some_and(|live| live_exact(live, &record)) =>
            {
                state.records.get_mut(logical).expect("record exists").phase = NetworkPhase::Owned;
                state.save(&self.paths)
            }
            _ => bail!("network interface ownership conflict: {logical}"),
        }
    }

    fn remove_record(&self, state: &mut NetworkState, logical: &str) -> Result<()> {
        let record = state
            .records
            .get(logical)
            .cloned()
            .context("network ownership record disappeared")?;
        let live = self.capture(logical)?;
        if live.is_none() {
            if record.phase == NetworkPhase::Deleting {
                state.reload_pending = true;
            }
            state.records.remove(logical);
            return state.save(&self.paths);
        }
        if !live.as_ref().is_some_and(|live| live_exact(live, &record)) {
            bail!("refusing to remove changed network interface: {logical}");
        }
        state.records.get_mut(logical).expect("record exists").phase = NetworkPhase::Deleting;
        state.save(&self.paths)?;
        // Best effort: the Linux device may already be gone, but netifd must
        // be given a chance to detach its status object before section delete.
        let _ = self.runner.output("ifdown", [logical]);
        self.commit_delete(state, &record)
    }

    fn commit_create(&self, state: &mut NetworkState, record: &NetworkRecord) -> Result<()> {
        self.mutate(record, true)?;
        let live = self
            .capture(&record.entry.logical)?
            .context("network section disappeared after commit")?;
        if !live_exact(&live, record) {
            bail!("network interface creation did not commit");
        }
        state
            .records
            .get_mut(&record.entry.logical)
            .expect("record exists")
            .phase = NetworkPhase::Owned;
        state.reload_pending = true;
        state.save(&self.paths)
    }

    fn commit_delete(&self, state: &mut NetworkState, record: &NetworkRecord) -> Result<()> {
        self.mutate(record, false)?;
        if self.capture(&record.entry.logical)?.is_some() {
            bail!("network interface deletion did not commit");
        }
        state.records.remove(&record.entry.logical);
        state.reload_pending = true;
        state.save(&self.paths)
    }

    fn mutate(&self, record: &NetworkRecord, create: bool) -> Result<()> {
        self.ensure_default_delta_clean()?;
        let savedir = self.reset_savedir()?;
        let result = (|| {
            let current = self.capture(&record.entry.logical)?;
            if create {
                if current.is_some() {
                    bail!("network interface appeared before creation");
                }
                let prefix = format!("network.{}", record.entry.logical);
                for (operation, expression) in network_create_delta(record, &prefix) {
                    self.uci_private(&savedir, operation, &expression)?;
                }
            } else {
                let current = current.context("network interface disappeared before deletion")?;
                if !live_exact(&current, record) {
                    bail!("network interface changed before deletion");
                }
                self.uci_private(
                    &savedir,
                    "delete",
                    &format!("network.{}", record.entry.logical),
                )?;
            }
            self.ensure_default_delta_clean()?;
            self.uci_private(&savedir, "commit", "network")
        })();
        let cleanup = remove_session_directory(&savedir);
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error.context("could not clean private UCI session")),
            (Err(error), Err(cleanup)) => Err(error.context(format!(
                "private UCI session cleanup also failed: {cleanup:#}"
            ))),
        }
    }

    fn reload_if_pending(&self, state: &mut NetworkState) -> Result<()> {
        if !state.reload_pending {
            return Ok(());
        }
        if self.paths.root.is_none() {
            self.runner.status("ubus", ["call", "network", "reload"])?;
        }
        state.reload_pending = false;
        state.save(&self.paths)
    }

    fn capture(&self, logical: &str) -> Result<Option<NetworkLive>> {
        let prefix = format!("network.{logical}");
        let Some(section_type) = self.uci_get(&prefix)? else {
            return Ok(None);
        };
        Ok(Some(NetworkLive {
            section_type,
            proto: self.uci_get(&format!("{prefix}.proto"))?,
            device: self.uci_get(&format!("{prefix}.device"))?,
            auto: self.uci_get(&format!("{prefix}.auto"))?,
            defaultroute: self.uci_get(&format!("{prefix}.defaultroute"))?,
            peerdns: self.uci_get(&format!("{prefix}.peerdns"))?,
            delegate: self.uci_get(&format!("{prefix}.delegate"))?,
            owner: self.uci_get(&format!("{prefix}.meduza_owner"))?,
            nonce: self.uci_get(&format!("{prefix}.meduza_nonce"))?,
            kind: self.uci_get(&format!("{prefix}.meduza_kind"))?,
            instance: self.uci_get(&format!("{prefix}.meduza_instance"))?,
        }))
    }

    fn interface_up(&self, logical: &str) -> Result<bool> {
        let output = match self.runner.output("ifstatus", [logical]) {
            Ok(output) if output.status.success() => output,
            Ok(_) | Err(_) => return Ok(false),
        };
        if output.stdout.len() > MAX_IFSTATUS_BYTES {
            bail!("ifstatus response is too large: {logical}");
        }
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("ifstatus output was not valid JSON")?;
        Ok(value.get("up").and_then(serde_json::Value::as_bool) == Some(true))
    }

    fn ensure_default_delta_clean(&self) -> Result<()> {
        let output = self.runner.output("uci", ["changes", "network"])?;
        if !output.status.success() {
            bail!("could not inspect uncommitted network UCI changes");
        }
        if !String::from_utf8(output.stdout)
            .context("network UCI changes were not UTF-8")?
            .trim()
            .is_empty()
        {
            bail!("uncommitted network UCI changes exist");
        }
        Ok(())
    }

    fn uci_get(&self, expression: &str) -> Result<Option<String>> {
        let output = self.runner.output("uci", ["-q", "get", expression])?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8(output.stdout)
                    .context("network UCI value was not UTF-8")?
                    .trim()
                    .to_owned(),
            ));
        }
        if output.status.code() == Some(1) {
            Ok(None)
        } else {
            bail!("could not read network UCI value {expression}")
        }
    }

    fn uci_private(&self, savedir: &Path, operation: &str, expression: &str) -> Result<()> {
        let savedir = savedir.to_string_lossy().into_owned();
        self.runner
            .status("uci", ["-q", "-t", savedir.as_str(), operation, expression])
    }

    fn reset_savedir(&self) -> Result<PathBuf> {
        let path = self.paths.runtime.join("uci-network");
        atomic::ensure_private_dir(&path, 0o700)?;
        reset_directory(&path)?;
        Ok(path)
    }
}

impl NetworkState {
    fn load(paths: &Paths) -> Result<Self> {
        let metadata = match fs::symlink_metadata(&paths.network_state) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: 1,
                    ..Self::default()
                });
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("network ownership state is not a regular file");
        }
        let bytes = atomic::read_bounded(&paths.network_state, MAX_NETWORK_STATE_BYTES)?;
        let state: Self =
            serde_json::from_slice(&bytes).context("invalid network ownership state")?;
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported network ownership state version");
        }
        for (logical, record) in &self.records {
            validate_entry(&record.entry)?;
            validate_nonce(&record.nonce)?;
            if logical != &record.entry.logical {
                bail!("network ownership identity changed");
            }
        }
        Ok(())
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        if self.records.is_empty() && !self.reload_pending {
            atomic::durable_remove(&paths.network_state)?;
            return Ok(());
        }
        atomic::atomic_json_bounded(&paths.network_state, self, MAX_NETWORK_STATE_BYTES)?;
        Ok(())
    }
}

fn network_create_delta(record: &NetworkRecord, prefix: &str) -> Vec<(&'static str, String)> {
    vec![
        ("set", format!("{prefix}=interface")),
        ("set", format!("{prefix}.proto=none")),
        ("set", format!("{prefix}.device={}", record.entry.device)),
        ("set", format!("{prefix}.auto=0")),
        ("set", format!("{prefix}.defaultroute=0")),
        ("set", format!("{prefix}.peerdns=0")),
        ("set", format!("{prefix}.delegate=0")),
        ("set", format!("{prefix}.meduza_owner={OWNER}")),
        ("set", format!("{prefix}.meduza_nonce={}", record.nonce)),
        (
            "set",
            format!("{prefix}.meduza_kind={}", record.entry.kind.as_str()),
        ),
        (
            "set",
            format!("{prefix}.meduza_instance={}", record.entry.instance),
        ),
    ]
}

fn live_exact(live: &NetworkLive, record: &NetworkRecord) -> bool {
    live.section_type == "interface"
        && live.proto.as_deref() == Some("none")
        && live.device.as_deref() == Some(record.entry.device.as_str())
        && live.auto.as_deref() == Some("0")
        && live.defaultroute.as_deref() == Some("0")
        && live.peerdns.as_deref() == Some("0")
        && live.delegate.as_deref() == Some("0")
        && live.owner.as_deref() == Some(OWNER)
        && live.nonce.as_deref() == Some(record.nonce.as_str())
        && live.kind.as_deref() == Some(record.entry.kind.as_str())
        && live.instance.as_deref() == Some(record.entry.instance.as_str())
}

fn validate_entry(entry: &ManifestEntry) -> Result<()> {
    validate_logical_name(&entry.logical)?;
    validate_device(&entry.device)?;
    validate_instance(&entry.instance)
}

fn validate_nonce(value: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid network ownership nonce");
    }
    Ok(())
}

fn reset_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("private UCI session path is not a real directory");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("private UCI session contains an unknown object");
        }
        fs::remove_file(entry.path())?;
    }
    atomic::sync_dir(path)
}

fn remove_session_directory(path: &Path) -> Result<()> {
    reset_directory(path)?;
    fs::remove_dir(path)?;
    if let Some(parent) = path.parent() {
        atomic::sync_dir(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::process::{ExitStatus, Output};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::state::InterfaceKind;

    #[derive(Clone, Debug)]
    enum Pending {
        Set(String, String),
        Delete(String),
    }

    #[derive(Debug, Default)]
    struct MockState {
        sections: BTreeMap<String, BTreeMap<String, String>>,
        pending: Vec<Pending>,
        reloads: usize,
        ifups: usize,
        up: BTreeSet<String>,
    }

    #[derive(Clone, Debug, Default)]
    struct MockRunner(Arc<Mutex<MockState>>);

    impl Runner for MockRunner {
        fn output<I, S>(&self, program: &str, args: I) -> Result<Output>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            let args = args
                .into_iter()
                .map(|value| value.as_ref().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let mut state = self.0.lock().unwrap();
            if program == "ubus" && args == ["call", "network", "reload"] {
                state.reloads += 1;
                return Ok(output(0, ""));
            }
            if program == "ifstatus" {
                let up = state.up.contains(&args[0]);
                return Ok(output(0, &format!(r#"{{"up":{up}}}"#)));
            }
            if program == "ifup" {
                state.ifups += 1;
                state.up.insert(args[0].clone());
                return Ok(output(0, ""));
            }
            if program == "ifdown" {
                state.up.remove(&args[0]);
                return Ok(output(0, ""));
            }
            assert_eq!(program, "uci");
            if args == ["changes", "network"] {
                return Ok(output(0, ""));
            }
            if args.first().map(String::as_str) == Some("-q")
                && args.get(1).map(String::as_str) == Some("get")
                && args.len() == 3
            {
                let path = args[2].strip_prefix("network.").unwrap();
                let (logical, option) = path
                    .split_once('.')
                    .map_or((path, "_type"), |(logical, option)| (logical, option));
                return match state
                    .sections
                    .get(logical)
                    .and_then(|section| section.get(option))
                {
                    Some(value) => Ok(output(0, &format!("{value}\n"))),
                    None => Ok(output(1, "")),
                };
            }
            assert_eq!(args.len(), 5);
            let operation = args[3].as_str();
            let expression = args[4].as_str();
            match operation {
                "set" => {
                    let (path, value) = expression.split_once('=').unwrap();
                    state
                        .pending
                        .push(Pending::Set(path.to_owned(), value.to_owned()));
                }
                "delete" => state.pending.push(Pending::Delete(expression.to_owned())),
                "commit" => {
                    assert_eq!(expression, "network");
                    for pending in std::mem::take(&mut state.pending) {
                        match pending {
                            Pending::Set(path, value) => {
                                let path = path.strip_prefix("network.").unwrap();
                                let (logical, option) = path
                                    .split_once('.')
                                    .map_or((path, "_type"), |(logical, option)| (logical, option));
                                state
                                    .sections
                                    .entry(logical.into())
                                    .or_default()
                                    .insert(option.into(), value);
                            }
                            Pending::Delete(path) => {
                                let logical = path.strip_prefix("network.").unwrap();
                                state.sections.remove(logical);
                                state.up.remove(logical);
                            }
                        }
                    }
                }
                value => panic!("unexpected UCI operation {value}"),
            }
            Ok(output(0, ""))
        }
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    fn output(code: i32, stdout: &str) -> Output {
        Output {
            status: exit_status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn entry(paths: &Paths) -> ManifestEntry {
        ManifestEntry {
            kind: InterfaceKind::Wireguard,
            instance: "office".into(),
            logical: "wg_office".into(),
            device: "wg-office".into(),
            config: paths.generated.join("wireguard/office/wg.conf"),
        }
    }

    #[test]
    fn interface_is_created_activated_idempotently_and_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        let network = NetworkInterfaces::new(paths.clone(), runner.clone());
        let entry = entry(&paths);

        network.ensure(std::slice::from_ref(&entry)).unwrap();
        {
            let state = runner.0.lock().unwrap();
            let section = state.sections.get("wg_office").unwrap();
            assert_eq!(section.get("_type").map(String::as_str), Some("interface"));
            assert_eq!(section.get("proto").map(String::as_str), Some("none"));
            assert_eq!(section.get("device").map(String::as_str), Some("wg-office"));
            assert_eq!(section.get("auto").map(String::as_str), Some("0"));
            assert_eq!(section.get("meduza_owner").map(String::as_str), Some(OWNER));
            assert_eq!(state.reloads, 0); // rooted tests skip the live ubus call
        }

        network.activate(std::slice::from_ref(&entry)).unwrap();
        network.activate(std::slice::from_ref(&entry)).unwrap();
        assert_eq!(runner.0.lock().unwrap().ifups, 1);

        network.prune(&[]).unwrap();
        assert!(!runner.0.lock().unwrap().sections.contains_key("wg_office"));
        assert!(!paths.network_state.exists());
    }

    #[test]
    fn foreign_named_interface_is_rejected_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        runner.0.lock().unwrap().sections.insert(
            "wg_office".into(),
            BTreeMap::from([
                ("_type".into(), "interface".into()),
                ("proto".into(), "static".into()),
            ]),
        );
        let network = NetworkInterfaces::new(paths.clone(), runner.clone());

        assert!(network.ensure(&[entry(&paths)]).is_err());
        assert_eq!(
            runner.0.lock().unwrap().sections["wg_office"]["proto"],
            "static"
        );
        assert!(!paths.network_state.exists());
    }
}

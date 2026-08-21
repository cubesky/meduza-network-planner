use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic;
use crate::command::Runner;
use crate::config::validate_firewall_zone;
use crate::model::validate_device;
use crate::state::{ManifestEntry, Paths};

const MAX_FIREWALL_STATE_BYTES: usize = 1024 * 1024;
const MAX_ZONE_COUNT: usize = 256;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FirewallState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    records: BTreeMap<String, DeviceRecord>,
    #[serde(default)]
    reload_pending: bool,
}

const fn state_version() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DevicePhase {
    Creating,
    Owned,
    Deleting,
    Borrowed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceRecord {
    zone: String,
    device: String,
    nonce: String,
    tag_option: String,
    phase: DevicePhase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ZoneLive {
    section: String,
    member: bool,
    tag: Option<String>,
}

pub struct Firewall<R: Runner> {
    paths: Paths,
    runner: R,
}

impl<R: Runner> Firewall<R> {
    pub fn new(paths: Paths, runner: R) -> Self {
        Self { paths, runner }
    }

    /// Validate a selected zone before reconciliation performs any external
    /// mutation. Empty means that firewall membership is intentionally
    /// unmanaged (and any previously owned membership will be retired).
    pub fn validate_zone(&self, zone: Option<&str>) -> Result<()> {
        let Some(zone) = zone else { return Ok(()) };
        validate_firewall_zone(zone)?;
        if self.paths.root.is_none() && !Path::new("/etc/init.d/firewall").is_file() {
            bail!("VPN firewall zone is configured but firewall service is not installed");
        }
        self.ensure_default_delta_clean()?;
        if self.resolve_zone(zone)?.is_none() {
            bail!("configured firewall zone does not exist: {zone}");
        }
        Ok(())
    }

    /// Converge only `list device` tokens for directly managed VPN links.
    /// Zone policy, forwarding, NAT, networks and every unrelated member are
    /// outside this controller's ownership domain.
    pub fn sync(&self, zone: Option<&str>, entries: &[ManifestEntry]) -> Result<()> {
        self.validate_zone(zone)?;
        let mut state = FirewallState::load(&self.paths)?;
        let desired: BTreeSet<String> = zone
            .into_iter()
            .flat_map(|zone| {
                entries
                    .iter()
                    .map(move |entry| record_key(zone, &entry.device))
            })
            .collect();

        for key in state.records.keys().cloned().collect::<Vec<_>>() {
            if desired.contains(&key) {
                self.ensure_record(&mut state, &key)?;
            } else {
                self.remove_record(&mut state, &key)?;
            }
        }

        if let Some(zone) = zone {
            for entry in entries {
                let key = record_key(zone, &entry.device);
                if !state.records.contains_key(&key) {
                    self.create_record(&mut state, zone, &entry.device)?;
                }
            }
        }

        if state.reload_pending {
            if Path::new("/etc/init.d/firewall").is_file() || self.paths.root.is_some() {
                self.runner.status("/etc/init.d/firewall", ["reload"])?;
            }
            state.reload_pending = false;
        }
        state.save(&self.paths)
    }

    fn ensure_record(&self, state: &mut FirewallState, key: &str) -> Result<()> {
        let record = state
            .records
            .get(key)
            .cloned()
            .context("firewall ownership record disappeared")?;
        let Some(live) = self.capture(&record.zone, &record.device, &record.tag_option)? else {
            bail!("configured firewall zone disappeared: {}", record.zone);
        };
        let exact_tag = live.tag.as_deref() == Some(owned_tag(&record).as_str());
        match record.phase {
            DevicePhase::Borrowed if live.member && live.tag.is_none() => Ok(()),
            DevicePhase::Borrowed if !live.member && live.tag.is_none() => {
                state.records.remove(key);
                state.save(&self.paths)?;
                self.create_record(state, &record.zone, &record.device)
            }
            DevicePhase::Borrowed => bail!("borrowed firewall membership acquired a tag"),
            DevicePhase::Creating if live.member && exact_tag => {
                state.records.get_mut(key).expect("record exists").phase = DevicePhase::Owned;
                state.save(&self.paths)
            }
            DevicePhase::Creating if !live.member && live.tag.is_none() => {
                self.commit_add(state, key, &record, &live)
            }
            DevicePhase::Creating if live.member && live.tag.is_none() => {
                state.records.get_mut(key).expect("record exists").phase = DevicePhase::Borrowed;
                state.save(&self.paths)
            }
            DevicePhase::Creating => bail!("firewall creation state conflicts with live UCI"),
            DevicePhase::Owned if live.member && exact_tag => Ok(()),
            DevicePhase::Owned if !live.member && live.tag.is_none() => {
                let replacement = new_record(&record.zone, &record.device);
                state.records.insert(key.to_owned(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_add(state, key, &replacement, &live)
            }
            DevicePhase::Owned if live.member && live.tag.is_none() => {
                state.records.get_mut(key).expect("record exists").phase = DevicePhase::Borrowed;
                state.save(&self.paths)
            }
            DevicePhase::Owned => bail!("owned firewall membership changed unexpectedly"),
            DevicePhase::Deleting if live.member && exact_tag => {
                state.records.get_mut(key).expect("record exists").phase = DevicePhase::Owned;
                state.save(&self.paths)
            }
            DevicePhase::Deleting if !live.member && live.tag.is_none() => {
                let replacement = new_record(&record.zone, &record.device);
                state.records.insert(key.to_owned(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_add(state, key, &replacement, &live)
            }
            DevicePhase::Deleting if live.member && live.tag.is_none() => {
                state.records.get_mut(key).expect("record exists").phase = DevicePhase::Borrowed;
                state.save(&self.paths)
            }
            DevicePhase::Deleting => bail!("firewall deletion reversal conflicts with live UCI"),
        }
    }

    fn create_record(&self, state: &mut FirewallState, zone: &str, device: &str) -> Result<()> {
        validate_device(device)?;
        let tag_option = tag_option(zone, device);
        let live = self
            .capture(zone, device, &tag_option)?
            .with_context(|| format!("configured firewall zone disappeared: {zone}"))?;
        let key = record_key(zone, device);
        if live.tag.is_some() {
            bail!("firewall membership tag is already occupied for {zone}/{device}");
        }
        if live.member {
            state.records.insert(
                key,
                DeviceRecord {
                    zone: zone.into(),
                    device: device.into(),
                    nonce: atomic::random_nonce(),
                    tag_option,
                    phase: DevicePhase::Borrowed,
                },
            );
            return state.save(&self.paths);
        }
        let record = new_record(zone, device);
        state.records.insert(key.clone(), record.clone());
        state.save(&self.paths)?;
        self.commit_add(state, &key, &record, &live)
    }

    fn remove_record(&self, state: &mut FirewallState, key: &str) -> Result<()> {
        let record = state
            .records
            .get(key)
            .cloned()
            .context("firewall ownership record disappeared")?;
        let Some(live) = self.capture(&record.zone, &record.device, &record.tag_option)? else {
            state.records.remove(key);
            return state.save(&self.paths);
        };
        if record.phase == DevicePhase::Borrowed {
            state.records.remove(key);
            return state.save(&self.paths);
        }
        let exact_tag = live.tag.as_deref() == Some(owned_tag(&record).as_str());
        if live.tag.is_none() {
            // Both fully absent and an administrator's untagged replacement
            // mean that our generation is gone. Never delete the latter.
            state.records.remove(key);
            return state.save(&self.paths);
        }
        if !exact_tag {
            bail!("firewall membership tag ownership changed for {key}");
        }
        state.records.get_mut(key).expect("record exists").phase = DevicePhase::Deleting;
        state.save(&self.paths)?;
        self.commit_delete(state, key, &record, &live)
    }

    fn commit_add(
        &self,
        state: &mut FirewallState,
        key: &str,
        record: &DeviceRecord,
        before: &ZoneLive,
    ) -> Result<()> {
        state.reload_pending = true;
        state.save(&self.paths)?;
        self.mutate(record, before, true)?;
        let after = self
            .capture(&record.zone, &record.device, &record.tag_option)?
            .context("firewall zone disappeared after commit")?;
        if !after.member || after.tag.as_deref() != Some(owned_tag(record).as_str()) {
            bail!("firewall membership add did not commit");
        }
        state.records.get_mut(key).expect("record exists").phase = DevicePhase::Owned;
        state.save(&self.paths)
    }

    fn commit_delete(
        &self,
        state: &mut FirewallState,
        key: &str,
        record: &DeviceRecord,
        before: &ZoneLive,
    ) -> Result<()> {
        state.reload_pending = true;
        state.save(&self.paths)?;
        self.mutate(record, before, false)?;
        let after = self.capture(&record.zone, &record.device, &record.tag_option)?;
        if after.is_some_and(|after| after.member || after.tag.is_some()) {
            bail!("firewall membership deletion did not commit");
        }
        state.records.remove(key);
        state.save(&self.paths)
    }

    fn mutate(&self, record: &DeviceRecord, before: &ZoneLive, add: bool) -> Result<()> {
        self.ensure_default_delta_clean()?;
        let savedir = self.reset_savedir()?;
        let result = (|| {
            let current = self
                .capture(&record.zone, &record.device, &record.tag_option)?
                .context("firewall zone disappeared before mutation")?;
            if &current != before {
                bail!("firewall membership changed before mutation");
            }
            let prefix = format!("firewall.{}", current.section);
            if add {
                if !current.member {
                    self.uci_private(
                        &savedir,
                        "add_list",
                        &format!("{prefix}.device={}", record.device),
                    )?;
                }
                self.uci_private(
                    &savedir,
                    "set",
                    &format!("{prefix}.{}={}", record.tag_option, owned_tag(record)),
                )?;
            } else {
                if current.member {
                    self.uci_private(
                        &savedir,
                        "del_list",
                        &format!("{prefix}.device={}", record.device),
                    )?;
                }
                self.uci_private(
                    &savedir,
                    "delete",
                    &format!("{prefix}.{}", record.tag_option),
                )?;
            }
            self.ensure_default_delta_clean()?;
            let fresh = self
                .capture(&record.zone, &record.device, &record.tag_option)?
                .context("firewall zone disappeared before commit")?;
            if fresh != current {
                bail!("firewall membership changed before commit");
            }
            self.uci_private(&savedir, "commit", "firewall")
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

    fn ensure_default_delta_clean(&self) -> Result<()> {
        let output = self.runner.output("uci", ["changes", "firewall"])?;
        if !output.status.success() {
            bail!("could not inspect uncommitted firewall UCI changes");
        }
        if !String::from_utf8(output.stdout)
            .context("firewall UCI changes were not UTF-8")?
            .trim()
            .is_empty()
        {
            bail!("uncommitted firewall UCI changes exist");
        }
        Ok(())
    }

    fn resolve_zone(&self, expected: &str) -> Result<Option<String>> {
        let mut found = None;
        for index in 0..MAX_ZONE_COUNT {
            let section = format!("@zone[{index}]");
            let Some(kind) = self.uci_get(&format!("firewall.{section}"))? else {
                break;
            };
            if kind != "zone" {
                continue;
            }
            let name = self
                .uci_get(&format!("firewall.{section}.name"))?
                .context("firewall zone has no name")?;
            if name == expected {
                if found.is_some() {
                    bail!("firewall zone name is not unique: {expected}");
                }
                found = Some(section);
            }
        }
        Ok(found)
    }

    fn capture(&self, zone: &str, device: &str, tag_option: &str) -> Result<Option<ZoneLive>> {
        let Some(section) = self.resolve_zone(zone)? else {
            return Ok(None);
        };
        let prefix = format!("firewall.{section}");
        let devices = self
            .uci_get(&format!("{prefix}.device"))?
            .unwrap_or_default();
        Ok(Some(ZoneLive {
            section,
            member: devices.split_whitespace().any(|value| value == device),
            tag: self.uci_get(&format!("{prefix}.{tag_option}"))?,
        }))
    }

    fn uci_get(&self, expression: &str) -> Result<Option<String>> {
        let output = self.runner.output("uci", ["-q", "get", expression])?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8(output.stdout)
                    .context("firewall UCI value was not UTF-8")?
                    .trim()
                    .to_owned(),
            ));
        }
        if output.status.code() == Some(1) {
            Ok(None)
        } else {
            bail!("could not read firewall UCI value {expression}")
        }
    }

    fn uci_private(&self, savedir: &Path, operation: &str, expression: &str) -> Result<()> {
        let savedir = savedir.to_string_lossy().into_owned();
        self.runner
            .status("uci", ["-q", "-P", savedir.as_str(), operation, expression])
    }

    fn reset_savedir(&self) -> Result<PathBuf> {
        let path = self.paths.runtime.join("uci-firewall");
        atomic::ensure_private_dir(&path, 0o700)?;
        reset_directory(&path)?;
        Ok(path)
    }
}

impl FirewallState {
    fn load(paths: &Paths) -> Result<Self> {
        let metadata = match fs::symlink_metadata(&paths.firewall_state) {
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
            bail!("firewall ownership state is not a regular file");
        }
        let bytes = atomic::read_bounded(&paths.firewall_state, MAX_FIREWALL_STATE_BYTES)?;
        let state: Self =
            serde_json::from_slice(&bytes).context("invalid firewall ownership state")?;
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported firewall ownership state version");
        }
        for (key, record) in &self.records {
            validate_firewall_zone(&record.zone)?;
            validate_device(&record.device)?;
            validate_nonce(&record.nonce)?;
            if key != &record_key(&record.zone, &record.device)
                || record.tag_option != tag_option(&record.zone, &record.device)
            {
                bail!("firewall ownership identity changed");
            }
        }
        Ok(())
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        if self.records.is_empty() && !self.reload_pending {
            atomic::durable_remove(&paths.firewall_state)?;
            return Ok(());
        }
        atomic::atomic_json_bounded(&paths.firewall_state, self, MAX_FIREWALL_STATE_BYTES)?;
        Ok(())
    }
}

fn new_record(zone: &str, device: &str) -> DeviceRecord {
    DeviceRecord {
        zone: zone.into(),
        device: device.into(),
        nonce: atomic::random_nonce(),
        tag_option: tag_option(zone, device),
        phase: DevicePhase::Creating,
    }
}

fn record_key(zone: &str, device: &str) -> String {
    format!("{zone}\0{device}")
}

fn tag_option(zone: &str, device: &str) -> String {
    let hash = hex::encode(Sha256::digest(format!("{zone}\0{device}").as_bytes()));
    format!("meduza_vpn_{}", &hash[..16])
}

fn owned_tag(record: &DeviceRecord) -> String {
    format!("owned:{}", record.nonce)
}

fn validate_nonce(value: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid firewall ownership nonce");
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
        Add(String),
        Delete(String),
        Set(String, String),
        DeleteTag(String),
    }

    #[derive(Debug, Default)]
    struct MockUciState {
        zone: String,
        devices: BTreeSet<String>,
        tags: BTreeMap<String, String>,
        pending: Vec<Pending>,
        reloads: usize,
    }

    #[derive(Clone, Debug, Default)]
    struct MockRunner(Arc<Mutex<MockUciState>>);

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
            if program == "/etc/init.d/firewall" && args == ["reload"] {
                state.reloads += 1;
                return Ok(output(0, ""));
            }
            assert_eq!(program, "uci");
            if args == ["changes", "firewall"] {
                return Ok(output(0, ""));
            }
            if args.first().map(String::as_str) == Some("-q")
                && args.get(1).map(String::as_str) == Some("get")
            {
                let expression = &args[2];
                return if expression == "firewall.@zone[0]" {
                    Ok(output(0, "zone\n"))
                } else if expression == "firewall.@zone[0].name" {
                    Ok(output(0, &format!("{}\n", state.zone)))
                } else if expression == "firewall.@zone[0].device" {
                    if state.devices.is_empty() {
                        Ok(output(1, ""))
                    } else {
                        Ok(output(
                            0,
                            &format!(
                                "{}\n",
                                state.devices.iter().cloned().collect::<Vec<_>>().join(" ")
                            ),
                        ))
                    }
                } else if let Some(option) = expression.strip_prefix("firewall.@zone[0].") {
                    match state.tags.get(option) {
                        Some(value) => Ok(output(0, &format!("{value}\n"))),
                        None => Ok(output(1, "")),
                    }
                } else {
                    Ok(output(1, ""))
                };
            }
            assert_eq!(args.len(), 5);
            assert_eq!(args[0], "-q");
            assert_eq!(args[1], "-P");
            let operation = args[3].as_str();
            let expression = args[4].as_str();
            match operation {
                "add_list" => state.pending.push(Pending::Add(
                    expression.split_once('=').unwrap().1.to_owned(),
                )),
                "del_list" => state.pending.push(Pending::Delete(
                    expression.split_once('=').unwrap().1.to_owned(),
                )),
                "set" => {
                    let (path, value) = expression.split_once('=').unwrap();
                    state.pending.push(Pending::Set(
                        path.rsplit_once('.').unwrap().1.to_owned(),
                        value.to_owned(),
                    ));
                }
                "delete" => state.pending.push(Pending::DeleteTag(
                    expression.rsplit_once('.').unwrap().1.to_owned(),
                )),
                "commit" => {
                    assert_eq!(expression, "firewall");
                    for pending in std::mem::take(&mut state.pending) {
                        match pending {
                            Pending::Add(device) => {
                                state.devices.insert(device);
                            }
                            Pending::Delete(device) => {
                                state.devices.remove(&device);
                            }
                            Pending::Set(option, value) => {
                                state.tags.insert(option, value);
                            }
                            Pending::DeleteTag(option) => {
                                state.tags.remove(&option);
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
    fn device_record_identity_is_stable_and_validated() {
        let record = new_record("vpn-zone", "wg-office");
        let mut state = FirewallState {
            version: 1,
            records: BTreeMap::from([(record_key(&record.zone, &record.device), record.clone())]),
            reload_pending: true,
        };
        state.validate().unwrap();
        state.records.values_mut().next().unwrap().device = "other".into();
        assert!(state.validate().is_err());
    }

    #[test]
    fn tag_is_scoped_to_zone_and_device() {
        assert_eq!(tag_option("vpn", "wg0"), tag_option("vpn", "wg0"));
        assert_ne!(tag_option("vpn", "wg0"), tag_option("lan", "wg0"));
        assert_ne!(tag_option("vpn", "wg0"), tag_option("vpn", "wg1"));
    }

    #[test]
    fn owned_membership_is_added_reloaded_and_removed() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        runner.0.lock().unwrap().zone = "vpn".into();
        let firewall = Firewall::new(paths.clone(), runner.clone());

        firewall.sync(Some("vpn"), &[entry(&paths)]).unwrap();
        {
            let state = runner.0.lock().unwrap();
            assert!(state.devices.contains("wg-office"));
            assert_eq!(state.tags.len(), 1);
            assert_eq!(state.reloads, 1);
        }
        assert!(paths.firewall_state.is_file());

        firewall.sync(Some("vpn"), &[entry(&paths)]).unwrap();
        assert_eq!(runner.0.lock().unwrap().reloads, 1);

        firewall.sync(None, &[]).unwrap();
        let state = runner.0.lock().unwrap();
        assert!(!state.devices.contains("wg-office"));
        assert!(state.tags.is_empty());
        assert_eq!(state.reloads, 2);
        assert!(!paths.firewall_state.exists());
    }

    #[test]
    fn preexisting_membership_is_borrowed_and_never_removed() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        {
            let mut state = runner.0.lock().unwrap();
            state.zone = "vpn".into();
            state.devices.insert("wg-office".into());
        }
        let firewall = Firewall::new(paths.clone(), runner.clone());

        firewall.sync(Some("vpn"), &[entry(&paths)]).unwrap();
        firewall.sync(None, &[]).unwrap();

        let state = runner.0.lock().unwrap();
        assert!(state.devices.contains("wg-office"));
        assert!(state.tags.is_empty());
        assert_eq!(state.reloads, 0);
        assert!(!paths.firewall_state.exists());
    }
}

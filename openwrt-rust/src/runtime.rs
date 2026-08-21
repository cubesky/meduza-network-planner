use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::OWNER;
use crate::atomic;
use crate::command::Runner;
use crate::ownership::{OwnershipDb, Phase, WireguardStageRecord, wireguard_stage_name};
use crate::state::{InterfaceKind, ManifestEntry, Paths};

const MAX_GENERATED_CONFIG_BYTES: usize = 6 * 1024 * 1024;
const MAX_LINK_ALIAS_BYTES: usize = 4096;
const MAX_OPENVPN_RUNTIME_BYTES: usize = 256 * 1024;
#[cfg(target_os = "linux")]
const MAX_PROC_CMDLINE_BYTES: usize = 2 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_PROC_STAT_BYTES: usize = 4096;

#[derive(Clone)]
pub struct Runtime<R: Runner> {
    paths: Paths,
    runner: R,
}

impl<R: Runner> Runtime<R> {
    pub fn new(paths: Paths, runner: R) -> Self {
        Self { paths, runner }
    }

    pub fn preflight(&self, desired: &[ManifestEntry], reusable: &[ManifestEntry]) -> Result<()> {
        for entry in desired {
            if self.paths.openvpn_proto.is_file()
                && entry.kind == InterfaceKind::Openvpn
                && (native_openvpn_process_matches(entry)?
                    || self.native_openvpn_namespace_exists(entry)?)
                && !self.native_openvpn_runtime_owned(entry)?
            {
                let owners = reusable
                    .iter()
                    .filter(|old| {
                        old.kind == InterfaceKind::Openvpn && old.logical == entry.logical
                    })
                    .map(|old| self.native_openvpn_runtime_owned(old))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|owned| *owned)
                    .count();
                if owners != 1 {
                    bail!(
                        "OpenVPN runtime namespace is occupied by an unowned object: {}",
                        entry.logical
                    );
                }
            }
            if self.link_exists(&entry.device)
                && !self.device_owned(entry)?
                && !self.recoverable_script_alias(entry)?
            {
                let owners = reusable
                    .iter()
                    .filter(|old| old.device == entry.device)
                    .map(|old| Ok(self.device_owned(old)? || self.recoverable_script_alias(old)?))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|owned| *owned)
                    .count();
                if owners != 1 {
                    bail!(
                        "refusing to adopt existing non-Meduza device: {}",
                        entry.device
                    );
                }
            }
        }
        Ok(())
    }

    pub fn validate_dependencies(&self, desired: &[ManifestEntry]) -> Result<()> {
        let required = [
            (InterfaceKind::Tinc, "tincd"),
            (InterfaceKind::Openvpn, "openvpn"),
            (InterfaceKind::Wireguard, "wg"),
        ];
        for (kind, command) in required {
            if desired.iter().any(|entry| entry.kind == kind)
                && !crate::command::command_exists(command)
            {
                bail!(
                    "desired {} runtime requires missing command: {command}",
                    kind.as_str()
                );
            }
        }
        if desired
            .iter()
            .any(|entry| entry.kind == InterfaceKind::Openvpn)
            && !self.paths.openvpn_proto.is_file()
            && !Path::new("/etc/init.d/openvpn").is_file()
            && self.paths.root.is_none()
        {
            bail!("desired OpenVPN runtime has neither netifd proto nor init service");
        }
        Ok(())
    }

    pub fn activate(&self, entries: &[ManifestEntry], changed: &BTreeSet<String>) -> Result<()> {
        for entry in entries {
            match entry.kind {
                InterfaceKind::Tinc => {
                    self.activate_tinc(entry, changed.contains(&entry.logical))?
                }
                InterfaceKind::Openvpn => {
                    self.activate_openvpn(entry, changed.contains(&entry.logical))?
                }
                InterfaceKind::Wireguard => {
                    self.activate_wireguard(entry, changed.contains(&entry.logical))?
                }
            }
        }
        Ok(())
    }

    pub fn stop_all(&self, entries: &[ManifestEntry]) -> Result<()> {
        let mut errors = Vec::new();
        for entry in entries.iter().rev() {
            if let Err(error) = self.stop(entry) {
                errors.push(format!("{}: {error:#}", entry.logical));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("runtime stop failed: {}", errors.join("; "))
        }
    }

    pub fn stop(&self, entry: &ManifestEntry) -> Result<()> {
        let mut errors = Vec::new();
        let uci_owned = self.network_section_owned(entry)?;
        let legacy_openvpn_owned = entry.kind == InterfaceKind::Openvpn
            && crate::uci::live_section_owned(
                &self.paths,
                self.runner.clone(),
                "openvpn",
                &entry.logical,
            )?;
        let generated_owned =
            OwnershipDb::load(&self.paths)?.authorizes_generated_resource(entry)?;
        let native_openvpn_owned = entry.kind == InterfaceKind::Openvpn
            && self.paths.openvpn_proto.is_file()
            && self.native_openvpn_runtime_owned(entry)?;
        // A tinc-up/OpenVPN link-up script writes the deterministic short
        // alias before the controller can replace it with the generation
        // nonce.  Preserve that strong, process-backed recovery proof across
        // teardown: ifdown/process termination may remove the evidence before
        // we reach the final link cleanup below.
        let recoverable_link_owned = self.recoverable_script_alias(entry)?;
        if uci_owned
            && crate::command::command_exists("ifdown")
            && let Err(error) = self.runner.status("ifdown", [entry.logical.as_str()])
        {
            errors.push(format!("ifdown failed: {error:#}"));
        }
        match entry.kind {
            InterfaceKind::Openvpn => {
                if (legacy_openvpn_owned || (uci_owned && !self.paths.openvpn_proto.is_file()))
                    && Path::new("/etc/init.d/openvpn").is_file()
                    && let Err(error) = self
                        .runner
                        .status("/etc/init.d/openvpn", ["stop", entry.logical.as_str()])
                {
                    errors.push(format!("OpenVPN procd stop failed: {error:#}"));
                }
                if generated_owned {
                    if let Err(error) = terminate_matching_processes(entry, native_openvpn_owned) {
                        errors.push(format!("OpenVPN process stop failed: {error:#}"));
                    }
                } else if process_matches(entry, None, native_openvpn_owned)? {
                    errors.push("OpenVPN process exists without generated ownership".into());
                }
                if !native_openvpn_owned && native_openvpn_process_matches(entry)? {
                    errors.push(
                        "native OpenVPN process exists without runtime-file ownership".into(),
                    );
                }
            }
            InterfaceKind::Tinc => {
                if generated_owned {
                    if let Err(error) = terminate_matching_processes(entry, false) {
                        errors.push(format!("tinc process stop failed: {error:#}"));
                    }
                } else if process_matches(entry, None, false)? {
                    errors.push("tinc process exists without generated ownership".into());
                }
            }
            InterfaceKind::Wireguard => {
                if let Err(error) = self.remove_wireguard_stage(entry) {
                    errors.push(format!("WireGuard staging cleanup failed: {error:#}"));
                }
            }
        }
        if self.link_exists(&entry.device) {
            if !self.device_owned(entry)? && !recoverable_link_owned {
                errors.push(format!(
                    "device ownership changed while stopping: {}",
                    entry.device
                ));
            } else if !self.wait_link(&entry.device, false, Duration::from_secs(5))
                && let Err(error) = self
                    .runner
                    .status("ip", ["link", "del", "dev", entry.device.as_str()])
            {
                errors.push(format!("owned link deletion failed: {error:#}"));
            }
        }
        if self.link_exists(&entry.device) {
            errors.push(format!("owned device did not disappear: {}", entry.device));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("; "))
        }
    }

    fn activate_tinc(&self, entry: &ManifestEntry, changed: bool) -> Result<()> {
        if changed || !self.link_exists(&entry.device) {
            if self.link_exists(&entry.device) {
                self.stop(entry)?;
            }
            let config_dir = entry
                .config
                .parent()
                .context("tinc config has no directory")?;
            let pidfile = self
                .paths
                .runtime
                .join(format!("tinc.{}.pid", entry.instance));
            let args = vec![
                "-c".to_owned(),
                config_dir.display().to_string(),
                "-n".to_owned(),
                entry.instance.clone(),
                format!("--pidfile={}", pidfile.display()),
            ];
            self.runner.status("tincd", args)?;
            if !self.wait_link(&entry.device, true, Duration::from_secs(15)) {
                bail!("tinc did not create device {}", entry.device);
            }
        }
        self.mark_device(entry)?;
        self.runner.status("ifup", [entry.logical.as_str()])
    }

    fn activate_openvpn(&self, entry: &ManifestEntry, changed: bool) -> Result<()> {
        if self.paths.openvpn_proto.is_file() {
            if changed {
                self.runner.status("ifdown", [entry.logical.as_str()])?;
            }
            if changed || !self.interface_up(&entry.logical) {
                self.runner.status("ifup", [entry.logical.as_str()])?;
            }
        } else {
            if changed {
                self.runner
                    .status("/etc/init.d/openvpn", ["restart", entry.logical.as_str()])?;
            } else if self
                .runner
                .status("/etc/init.d/openvpn", ["running", entry.logical.as_str()])
                .is_err()
            {
                self.runner
                    .status("/etc/init.d/openvpn", ["start", entry.logical.as_str()])?;
            }
            self.runner.status("ifup", [entry.logical.as_str()])?;
        }
        if self.wait_link(&entry.device, true, Duration::from_secs(15)) {
            self.mark_device(entry)?;
            Ok(())
        } else if self.process_running(entry) {
            tracing::info!(instance = %entry.instance, "OpenVPN is registered and still connecting");
            Ok(())
        } else {
            bail!("OpenVPN exited before creating device {}", entry.device)
        }
    }

    fn activate_wireguard(&self, entry: &ManifestEntry, changed: bool) -> Result<()> {
        let key = OwnershipDb::generated_key(entry);
        let stage_pending = OwnershipDb::load(&self.paths)?
            .wireguard_stages
            .contains_key(&key);
        let configure = changed || !self.link_exists(&entry.device) || stage_pending;
        if changed && self.link_exists(&entry.device) {
            self.stop(entry)?;
        }
        if !self.link_exists(&entry.device) {
            let stage = self.begin_wireguard_stage(entry)?;
            if !self.link_exists(&stage) {
                self.runner.status(
                    "ip",
                    ["link", "add", "dev", stage.as_str(), "type", "wireguard"],
                )?;
            }
            if !self.link_is_wireguard(&stage)? {
                bail!("WireGuard staging interface has the wrong kind: {stage}");
            }
            let alias = self.device_alias(entry)?;
            let current_alias = self.link_alias(&stage)?;
            if !current_alias.is_empty() && current_alias != alias {
                bail!("WireGuard staging interface ownership changed: {stage}");
            }
            self.runner.status(
                "ip",
                [
                    "link",
                    "set",
                    "dev",
                    stage.as_str(),
                    "alias",
                    alias.as_str(),
                ],
            )?;
            // Renaming the nonce-bound staging interface publishes the desired
            // name only after its ownership alias is part of kernel state.
            self.runner.status(
                "ip",
                [
                    "link",
                    "set",
                    "dev",
                    stage.as_str(),
                    "name",
                    entry.device.as_str(),
                ],
            )?;
        } else if !self.device_owned(entry)? {
            bail!("refusing to configure unowned WireGuard device");
        }
        if configure {
            self.runner.status(
                "wg",
                [
                    "setconf",
                    entry.device.as_str(),
                    &entry.config.display().to_string(),
                ],
            )?;
            self.runner
                .status("ip", ["addr", "flush", "dev", entry.device.as_str()])?;
            let settings = entry
                .config
                .parent()
                .context("WG config has no directory")?
                .join("settings");
            if let Ok(Some(body)) = read_optional_string(&settings, MAX_GENERATED_CONFIG_BYTES) {
                for line in body.lines() {
                    let Some((kind, value)) = line.split_once('\t') else {
                        continue;
                    };
                    if value.is_empty() {
                        continue;
                    }
                    match kind {
                        "address" => self
                            .runner
                            .status("ip", ["addr", "add", value, "dev", entry.device.as_str()])?,
                        "mtu" => self.runner.status(
                            "ip",
                            ["link", "set", "mtu", value, "dev", entry.device.as_str()],
                        )?,
                        _ => bail!("invalid WireGuard setting type: {kind}"),
                    }
                }
            }
        }
        self.runner
            .status("ip", ["link", "set", "up", "dev", entry.device.as_str()])?;
        self.runner.status("ifup", [entry.logical.as_str()])?;
        self.finish_wireguard_stage(entry)
    }

    fn mark_device(&self, entry: &ManifestEntry) -> Result<()> {
        let alias = self.device_alias(entry)?;
        self.runner.status(
            "ip",
            [
                "link",
                "set",
                "dev",
                entry.device.as_str(),
                "alias",
                alias.as_str(),
            ],
        )
    }

    fn device_owned(&self, entry: &ManifestEntry) -> Result<bool> {
        let alias = self.link_alias(&entry.device)?;
        let db = OwnershipDb::load(&self.paths)?;
        let key = OwnershipDb::generated_key(entry);
        let Some(record) = db.generated.get(&key) else {
            return Ok(false);
        };
        Ok(alias
            == format!(
                "{OWNER}:{}:{}:{}",
                entry.kind.as_str(),
                entry.instance,
                record.nonce
            ))
    }

    fn link_alias(&self, device: &str) -> Result<String> {
        let path = self
            .paths
            .root
            .as_deref()
            .map(|root| root.join(format!("sys/class/net/{device}/ifalias")))
            .unwrap_or_else(|| Path::new("/sys/class/net").join(device).join("ifalias"));
        match read_optional_string(&path, MAX_LINK_ALIAS_BYTES)? {
            Some(alias) => Ok(alias.trim_end().to_owned()),
            None => Ok(String::new()),
        }
    }

    pub(crate) fn interface_owned(&self, entry: &ManifestEntry) -> Result<bool> {
        let db = OwnershipDb::load(&self.paths)?;
        Ok(db.authorizes_generated(entry)? && self.device_owned(entry)?)
    }

    pub(crate) fn status_interface_owned(&self, entry: &ManifestEntry) -> Result<bool> {
        Ok(self.interface_owned(entry)? || self.recoverable_script_alias(entry)?)
    }

    fn recoverable_script_alias(&self, entry: &ManifestEntry) -> Result<bool> {
        if !matches!(entry.kind, InterfaceKind::Tinc | InterfaceKind::Openvpn) {
            return Ok(false);
        }
        let path = self
            .paths
            .root
            .as_deref()
            .map(|root| root.join(format!("sys/class/net/{}/ifalias", entry.device)))
            .unwrap_or_else(|| {
                Path::new("/sys/class/net")
                    .join(&entry.device)
                    .join("ifalias")
            });
        let alias = match read_optional_string(&path, MAX_LINK_ALIAS_BYTES)? {
            Some(value) => value,
            None => return Ok(false),
        };
        if alias.trim_end() != format!("{OWNER}:{}:{}", entry.kind.as_str(), entry.instance) {
            return Ok(false);
        }
        let db = OwnershipDb::load(&self.paths)?;
        if !db.authorizes_generated(entry)? {
            return Ok(false);
        }
        // Native netifd OpenVPN invokes openvpn with the generated runtime
        // wrapper in /var/run rather than entry.config directly.  Accept that
        // argv shape only after the wrapper, network section, generated
        // config markers, and external generation all independently prove
        // ownership.  Tinc and legacy OpenVPN keep the stricter direct argv
        // matcher.
        let allow_native_openvpn = entry.kind == InterfaceKind::Openvpn
            && self.paths.openvpn_proto.is_file()
            && self.native_openvpn_runtime_owned(entry)?;
        process_matches(entry, None, allow_native_openvpn)
    }

    fn link_exists(&self, device: &str) -> bool {
        self.runner
            .output("ip", ["link", "show", "dev", device])
            .is_ok_and(|output| output.status.success())
    }

    fn wait_link(&self, device: &str, present: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.link_exists(device) == present {
                return true;
            }
            thread::sleep(Duration::from_millis(250));
        }
        self.link_exists(device) == present
    }

    pub(crate) fn interface_up(&self, logical: &str) -> bool {
        self.runner
            .text("ifstatus", [logical])
            .ok()
            .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
            .and_then(|value| value.get("up").and_then(serde_json::Value::as_bool))
            .unwrap_or(false)
    }

    pub(crate) fn link_up(&self, device: &str) -> bool {
        self.runner
            .text("ip", ["-o", "link", "show", "dev", device])
            .is_ok_and(|value| {
                value
                    .split_once('<')
                    .and_then(|(_, value)| value.split_once('>'))
                    .is_some_and(|(flags, _)| flags.split(',').any(|flag| flag == "UP"))
            })
    }

    pub(crate) fn process_running(&self, entry: &ManifestEntry) -> bool {
        if !OwnershipDb::load(&self.paths)
            .and_then(|db| db.authorizes_generated(entry))
            .unwrap_or(false)
        {
            return false;
        }
        if process_matches(entry, None, false).unwrap_or(false) {
            return true;
        }
        entry.kind == InterfaceKind::Openvpn
            && self.native_openvpn_runtime_owned(entry).unwrap_or(false)
            && process_matches(entry, None, true).unwrap_or(false)
    }

    fn network_section_owned(&self, entry: &ManifestEntry) -> Result<bool> {
        crate::uci::live_section_owned(&self.paths, self.runner.clone(), "network", &entry.logical)
    }

    fn native_openvpn_runtime_path(&self, entry: &ManifestEntry) -> std::path::PathBuf {
        crate::atomic::rooted(
            self.paths.root.as_deref(),
            &format!("/var/run/openvpn.{}.conf", entry.logical),
        )
    }

    fn native_openvpn_namespace_exists(&self, entry: &ManifestEntry) -> Result<bool> {
        match fs::symlink_metadata(self.native_openvpn_runtime_path(entry)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn native_openvpn_runtime_owned(&self, entry: &ManifestEntry) -> Result<bool> {
        if entry.kind != InterfaceKind::Openvpn || !self.network_section_owned(entry)? {
            return Ok(false);
        }
        let db = OwnershipDb::load(&self.paths)?;
        if !db.authorizes_generated_resource(entry)? {
            return Ok(false);
        }
        let config_metadata = match fs::symlink_metadata(&entry.config) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if config_metadata.file_type().is_symlink() || !config_metadata.is_file() {
            return Ok(false);
        }
        let generated = atomic::read_string_bounded(&entry.config, MAX_GENERATED_CONFIG_BYTES)?;
        let owner = format!("setenv MEDUZA_OWNER {OWNER}");
        let instance = format!("setenv MEDUZA_INSTANCE {}", entry.instance);
        if !generated.lines().any(|line| line == owner)
            || !generated.lines().any(|line| line == instance)
        {
            return Ok(false);
        }

        let runtime = self.native_openvpn_runtime_path(entry);
        let metadata = match fs::symlink_metadata(&runtime) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(false);
        }
        let body = atomic::read_string_bounded(&runtime, MAX_OPENVPN_RUNTIME_BYTES)?;
        let expected = entry.config.display().to_string();
        let mut found = false;
        for line in body.lines() {
            let mut fields = line.split_whitespace();
            if fields.next() != Some("config") {
                continue;
            }
            let Some(value) = fields.next() else {
                return Ok(false);
            };
            if fields.next().is_some() {
                return Ok(false);
            }
            let value = value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
                .or_else(|| {
                    value
                        .strip_prefix('"')
                        .and_then(|value| value.strip_suffix('"'))
                })
                .unwrap_or(value);
            if value != expected || found {
                return Ok(false);
            }
            found = true;
        }
        Ok(found)
    }

    fn device_alias(&self, entry: &ManifestEntry) -> Result<String> {
        let db = OwnershipDb::load(&self.paths)?;
        let key = OwnershipDb::generated_key(entry);
        let record = db
            .generated
            .get(&key)
            .with_context(|| format!("device has no generated ownership generation: {key}"))?;
        Ok(format!(
            "{OWNER}:{}:{}:{}",
            entry.kind.as_str(),
            entry.instance,
            record.nonce
        ))
    }

    fn begin_wireguard_stage(&self, entry: &ManifestEntry) -> Result<String> {
        if entry.kind != InterfaceKind::Wireguard {
            bail!("staging name requested for a non-WireGuard interface");
        }
        let mut db = OwnershipDb::load(&self.paths)?;
        let key = OwnershipDb::generated_key(entry);
        let generated = db
            .generated
            .get(&key)
            .with_context(|| format!("WireGuard has no generated ownership generation: {key}"))?;
        if generated.phase != Phase::Owned || generated.entry != *entry {
            bail!("WireGuard generated ownership is not stable: {key}");
        }
        if let Some(record) = db.wireguard_stages.get(&key) {
            if record.entry != *entry || record.phase != Phase::Creating {
                bail!("WireGuard staging ownership changed: {key}");
            }
            return Ok(record.device.clone());
        }
        let nonce = crate::atomic::random_nonce();
        let device = wireguard_stage_name(&nonce);
        if self.link_exists(&device) {
            bail!("unowned WireGuard staging name already exists: {device}");
        }
        db.wireguard_stages.insert(
            key,
            WireguardStageRecord {
                nonce,
                phase: Phase::Creating,
                entry: entry.clone(),
                device: device.clone(),
            },
        );
        db.save(&self.paths)?;
        Ok(device)
    }

    fn finish_wireguard_stage(&self, entry: &ManifestEntry) -> Result<()> {
        let mut db = OwnershipDb::load(&self.paths)?;
        let key = OwnershipDb::generated_key(entry);
        let Some(record) = db.wireguard_stages.get(&key).cloned() else {
            return Ok(());
        };
        if record.entry != *entry || record.phase != Phase::Creating {
            bail!("WireGuard staging ownership changed: {key}");
        }
        if self.link_exists(&record.device) || !self.device_owned(entry)? {
            bail!("WireGuard staging publish is incomplete: {key}");
        }
        db.wireguard_stages.remove(&key);
        db.save(&self.paths)
    }

    fn link_is_wireguard(&self, device: &str) -> Result<bool> {
        let output = self
            .runner
            .output("ip", ["-d", "-j", "link", "show", "dev", device])?;
        if !output.status.success() {
            bail!("could not inspect interface kind for {device}");
        }
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("ip returned invalid link JSON")?;
        Ok(value.as_array().is_some_and(|links| {
            links.iter().any(|link| {
                link.pointer("/linkinfo/info_kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("wireguard")
            })
        }))
    }

    fn remove_wireguard_stage(&self, entry: &ManifestEntry) -> Result<()> {
        let mut db = OwnershipDb::load(&self.paths)?;
        let key = OwnershipDb::generated_key(entry);
        let Some(record) = db.wireguard_stages.get(&key).cloned() else {
            return Ok(());
        };
        if record.entry != *entry || record.phase != Phase::Creating {
            bail!("WireGuard staging ownership changed: {key}");
        }
        let stage = record.device;
        if !self.link_exists(&stage) {
            db.wireguard_stages.remove(&key);
            return db.save(&self.paths);
        }
        if !self.link_is_wireguard(&stage)? {
            bail!("refusing to delete non-WireGuard staging interface: {stage}");
        }
        let alias = self.link_alias(&stage)?;
        let expected = self.device_alias(entry)?;
        if !alias.is_empty() && alias != expected {
            bail!("refusing to delete a replaced WireGuard staging interface: {stage}");
        }
        self.runner
            .status("ip", ["link", "del", "dev", stage.as_str()])?;
        if self.link_exists(&stage) {
            bail!("WireGuard staging interface did not disappear: {stage}");
        }
        db.wireguard_stages.remove(&key);
        db.save(&self.paths)
    }
}

#[cfg(target_os = "linux")]
fn terminate_matching_processes(entry: &ManifestEntry, allow_native_openvpn: bool) -> Result<()> {
    let mut processes = Vec::new();
    for directory in fs::read_dir("/proc")? {
        let directory = directory?;
        let Ok(pid) = directory.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(start_time) = process_identity(entry, pid, allow_native_openvpn)? {
            // SAFETY: kill is invoked only with a PID whose executable and NUL
            // separated argv were matched against the strong manifest identity.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            processes.push((pid, start_time));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && processes.iter().any(|(pid, start)| {
            process_identity(entry, *pid, allow_native_openvpn)
                .is_ok_and(|value| value.as_deref() == Some(start.as_str()))
        })
    {
        thread::sleep(Duration::from_millis(100));
    }
    for (pid, start) in &processes {
        if process_identity(entry, *pid, allow_native_openvpn)?.as_deref() == Some(start.as_str()) {
            // SAFETY: executable, argv and kernel start-time are revalidated,
            // so a recycled numeric PID is never signalled.
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !processes.iter().any(|(pid, start)| {
            process_identity(entry, *pid, allow_native_openvpn)
                .is_ok_and(|value| value.as_deref() == Some(start.as_str()))
        }) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    if processes.iter().any(|(pid, start)| {
        process_identity(entry, *pid, allow_native_openvpn)
            .is_ok_and(|value| value.as_deref() == Some(start.as_str()))
    }) {
        bail!("owned process did not exit")
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn terminate_matching_processes(_entry: &ManifestEntry, _allow_native_openvpn: bool) -> Result<()> {
    Ok(())
}

fn read_optional_bytes(path: &Path, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(_) => atomic::read_bounded(path, max_bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_string(path: &Path, max_bytes: usize) -> Result<Option<String>> {
    read_optional_bytes(path, max_bytes)?
        .map(String::from_utf8)
        .transpose()
        .with_context(|| format!("file is not valid UTF-8: {}", path.display()))
}

#[cfg(target_os = "linux")]
fn process_matches(
    entry: &ManifestEntry,
    only_pid: Option<i32>,
    allow_native_openvpn: bool,
) -> Result<bool> {
    let pids: Vec<i32> = if let Some(pid) = only_pid {
        vec![pid]
    } else {
        fs::read_dir("/proc")?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_string_lossy().parse().ok())
            .collect()
    };
    for pid in pids {
        if process_identity(entry, pid, allow_native_openvpn)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn native_openvpn_process_matches(entry: &ManifestEntry) -> Result<bool> {
    if entry.kind != InterfaceKind::Openvpn {
        return Ok(false);
    }
    let expected = format!("/var/run/openvpn.{}.conf", entry.logical);
    for process in fs::read_dir("/proc")? {
        let process = process?;
        let Ok(pid) = process.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let root = Path::new("/proc").join(pid.to_string());
        let Ok(exe) = fs::read_link(root.join("exe")) else {
            continue;
        };
        if proc_exe_basename(&exe).as_deref() != Some("openvpn") {
            continue;
        }
        let Ok(Some(raw)) = read_optional_bytes(&root.join("cmdline"), MAX_PROC_CMDLINE_BYTES)
        else {
            continue;
        };
        let argv: Vec<_> = raw
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect();
        if argv_option(&argv, "--config", "--config", &expected) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
fn native_openvpn_process_matches(_entry: &ManifestEntry) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "linux")]
fn process_identity(
    entry: &ManifestEntry,
    pid: i32,
    allow_native_openvpn: bool,
) -> Result<Option<String>> {
    let root = Path::new("/proc").join(pid.to_string());
    let Ok(exe) = fs::read_link(root.join("exe")) else {
        return Ok(None);
    };
    let executable = proc_exe_basename(&exe).unwrap_or_default();
    let expected = match entry.kind {
        InterfaceKind::Tinc => "tincd",
        InterfaceKind::Openvpn => "openvpn",
        InterfaceKind::Wireguard => return Ok(None),
    };
    if executable != expected {
        return Ok(None);
    }
    let Ok(Some(raw)) = read_optional_bytes(&root.join("cmdline"), MAX_PROC_CMDLINE_BYTES) else {
        return Ok(None);
    };
    let argv: Vec<_> = raw
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect();
    if !process_argv_matches(entry, &argv, allow_native_openvpn) {
        return Ok(None);
    }
    let stat = match read_optional_string(&root.join("stat"), MAX_PROC_STAT_BYTES) {
        Ok(Some(value)) => value,
        Ok(None) | Err(_) => return Ok(None),
    };
    let tail = stat
        .rsplit_once(')')
        .map(|(_, tail)| tail.trim_start())
        .context("invalid /proc stat record")?;
    let start_time = tail
        .split_whitespace()
        .nth(19)
        .context("/proc stat omitted start time")?;
    Ok(Some(start_time.to_owned()))
}

/// Linux appends ` (deleted)` to `/proc/PID/exe` after a package replaces a
/// running executable.  Strip only that exact kernel suffix; all other names
/// continue through the strict argv/config ownership checks.
#[cfg(target_os = "linux")]
pub(crate) fn proc_exe_basename(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.strip_suffix(" (deleted)").unwrap_or(value).to_owned())
}

#[cfg(not(target_os = "linux"))]
fn process_matches(
    _entry: &ManifestEntry,
    _only_pid: Option<i32>,
    _allow_native_openvpn: bool,
) -> Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn argv_option(argv: &[String], short: &str, long: &str, value: &str) -> bool {
    argv.iter().enumerate().any(|(index, argument)| {
        ((argument == short || argument == long)
            && argv.get(index + 1).is_some_and(|next| next == value))
            || argument == &format!("{short}{value}")
            || argument == &format!("{long}={value}")
    })
}

#[cfg(any(target_os = "linux", test))]
fn process_argv_matches(
    entry: &ManifestEntry,
    argv: &[String],
    allow_native_openvpn: bool,
) -> bool {
    let config = entry.config.display().to_string();
    let Some(parent) = entry.config.parent() else {
        return false;
    };
    let directory = parent.display().to_string();
    match entry.kind {
        InterfaceKind::Tinc => {
            argv_option(argv, "-n", "--net", &entry.instance)
                && argv_option(argv, "-c", "--config", &directory)
        }
        InterfaceKind::Openvpn => {
            argv_option(argv, "--config", "--config", &config)
                || (argv_option(argv, "--cd", "--cd", &directory)
                    && argv_option(argv, "--config", "--config", "openvpn.conf"))
                || (allow_native_openvpn
                    && argv_option(
                        argv,
                        "--config",
                        "--config",
                        &format!("/var/run/openvpn.{}.conf", entry.logical),
                    ))
        }
        InterfaceKind::Wireguard => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn openvpn_entry() -> ManifestEntry {
        ManifestEntry {
            kind: InterfaceKind::Openvpn,
            instance: "office".into(),
            logical: "ovpn_office".into(),
            device: "ovpn-office".into(),
            config: PathBuf::from("/etc/meduza/generated/openvpn/office/openvpn.conf"),
        }
    }

    #[test]
    fn native_openvpn_argv_requires_runtime_ownership_authorization() {
        let entry = openvpn_entry();
        let argv = vec![
            "openvpn".into(),
            "--config".into(),
            "/var/run/openvpn.ovpn_office.conf".into(),
        ];
        assert!(!process_argv_matches(&entry, &argv, false));
        assert!(process_argv_matches(&entry, &argv, true));
    }

    #[test]
    fn generated_openvpn_argv_remains_directly_owned() {
        let entry = openvpn_entry();
        let argv = vec![
            "openvpn".into(),
            "--cd".into(),
            "/etc/meduza/generated/openvpn/office".into(),
            "--config".into(),
            "openvpn.conf".into(),
        ];
        assert!(process_argv_matches(&entry, &argv, false));
    }
}

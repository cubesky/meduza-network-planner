use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::OWNER;
use crate::atomic;
use crate::command::Runner;
use crate::ownership::{EdgeRecord, OwnershipDb, Phase, SectionRecord};
use crate::state::{InterfaceKind, ManifestEntry, Paths, regular_file_exists};

const PACKAGES: [&str; 3] = ["network", "openvpn", "firewall"];
const MAX_RELOAD_INTENT_BYTES: usize = 4096;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ReloadIntent {
    version: u32,
    network: bool,
    firewall: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SyncOutcome {
    pub network_changed: bool,
    pub firewall_changed: bool,
    pub changed_interfaces: BTreeSet<String>,
}

pub struct Uci<R: Runner> {
    runner: R,
    paths: Paths,
}

impl<R: Runner> Uci<R> {
    pub fn new(paths: Paths, runner: R) -> Self {
        Self { runner, paths }
    }

    pub fn validate(&self, desired: &[ManifestEntry], zone: Option<&str>) -> Result<()> {
        self.assert_default_delta_clean()?;
        self.replay_pending_reload()?;
        let native_openvpn = self.paths.openvpn_proto.is_file();
        self.validate_legacy_openvpn_paths(desired, native_openvpn)?;
        let session = Session::create(self.runner.clone(), &PACKAGES)?;
        let packages = session.packages()?;
        let ownership = OwnershipDb::load(&self.paths)?;
        for entry in desired {
            validate_desired_section(&packages, &ownership, entry, native_openvpn)?;
        }
        if let Some(zone) = zone
            && !desired.is_empty()
            && packages.get("firewall").is_some()
            && find_zone(&packages, zone)?.is_none()
        {
            bail!("VPN_FIREWALL_ZONE does not exist: {zone}");
        }
        session.destroy()
    }

    pub fn sync(
        &self,
        desired: &[ManifestEntry],
        previous: &[ManifestEntry],
        zone: Option<&str>,
    ) -> Result<SyncOutcome> {
        self.assert_default_delta_clean()?;
        self.replay_pending_reload()?;
        let native_openvpn = self.paths.openvpn_proto.is_file();
        self.validate_legacy_openvpn_paths(desired, native_openvpn)?;
        let mut ownership = OwnershipDb::load(&self.paths)?;
        let session = Session::create(self.runner.clone(), &PACKAGES)?;
        let baseline = session.packages()?;
        let mut outcome = SyncOutcome::default();

        // Resolve crash-completed transitions before authorizing new mutation.
        recover_section_phases(&mut ownership, &baseline, native_openvpn)?;
        recover_edge_phases(&mut ownership, &baseline)?;
        ownership.save(&self.paths)?;

        let desired_logicals: BTreeSet<_> =
            desired.iter().map(|row| row.logical.as_str()).collect();
        for entry in desired {
            let changed = ensure_interface_section(
                &session,
                &baseline,
                &mut ownership,
                entry,
                native_openvpn,
            )?;
            if changed {
                outcome.network_changed = true;
                outcome.changed_interfaces.insert(entry.logical.clone());
            }
            if entry.kind == InterfaceKind::Openvpn {
                let auxiliary_changed = if native_openvpn {
                    delete_owned_section(
                        &session,
                        &baseline,
                        &mut ownership,
                        "openvpn",
                        &entry.logical,
                    )?
                } else {
                    ensure_openvpn_section(&session, &baseline, &mut ownership, entry)?
                };
                if auxiliary_changed {
                    outcome.changed_interfaces.insert(entry.logical.clone());
                }
            }
        }

        // Stop owning a section only when the external generation and complete
        // live fingerprint still match. An inline owner option alone is never
        // deletion authority.
        // The external ownership database, not the manifest alone, is the
        // deletion authority. This also converges a crash that committed UCI
        // but lost or never published the corresponding manifest row.
        for record in ownership.sections.clone().into_values() {
            let delete = match record.package.as_str() {
                "network" => !desired_logicals.contains(record.section.as_str()),
                "openvpn" => native_openvpn || !desired_logicals.contains(record.section.as_str()),
                package => bail!("unsupported UCI ownership package: {package}"),
            };
            if !delete {
                continue;
            }
            if delete_owned_section(
                &session,
                &baseline,
                &mut ownership,
                &record.package,
                &record.section,
            )? {
                if record.package == "network" {
                    outcome.network_changed = true;
                }
                outcome.changed_interfaces.insert(record.section);
            }
        }

        sync_firewall_edges(
            &session,
            &baseline,
            &mut ownership,
            desired,
            previous,
            zone,
            &mut outcome,
        )?;
        ownership.save(&self.paths)?;

        if outcome.network_changed || outcome.firewall_changed {
            persist_reload_intent(
                &self.paths,
                &ReloadIntent {
                    version: 1,
                    network: outcome.network_changed,
                    firewall: outcome.firewall_changed,
                },
            )?;
        }

        let changed = session.changed_packages()?;
        // rpcd sessions isolate Meduza's delta, but rpcd/libuci does not offer
        // a compare-and-swap commit. Refuse a commit when the live semantic
        // package changed since our validated baseline. This preserves
        // unrelated LuCI/OpenClash updates by making us discard and rebuild
        // the session instead of replaying a stale delta over them.
        let mut expected_packages = BTreeMap::new();
        for package in &changed {
            expected_packages.insert(package.clone(), session.get_package(package)?);
        }
        let deleting_network = ownership
            .sections
            .values()
            .any(|record| record.package == "network" && record.phase == Phase::Deleting);
        let order = if deleting_network {
            ["firewall", "openvpn", "network"]
        } else {
            ["network", "openvpn", "firewall"]
        };
        for package in order {
            if changed.contains(package) {
                // Revalidate this package immediately before its own commit.
                // Earlier package commits and reloads may take long enough for
                // LuCI/OpenClash to publish a new version of a later package.
                let before_commit = session.fresh_packages()?;
                ensure_package_unchanged(&baseline, &before_commit, package)?;
                session.commit(package)?;
                atomic::sync_dir(&self.paths.uci_config_dir)?;
                let after_commit = session.fresh_packages()?;
                let expected = expected_packages
                    .get(package)
                    .context("missing expected UCI package state")?
                    .as_ref();
                if canonical_package(expected)? != canonical_package(after_commit.get(package))? {
                    bail!("UCI package changed while committing {package}");
                }
            }
        }

        let live = session.fresh_packages()?;
        promote_section_phases(&mut ownership, &live)?;
        promote_edge_phases(&mut ownership, &live)?;
        ownership.save(&self.paths)?;
        session.destroy()?;
        atomic::sync_dir(&self.paths.uci_config_dir)?;
        self.replay_pending_reload()?;
        Ok(outcome)
    }

    pub fn disable_legacy_openvpn(&self, rows: &[ManifestEntry]) -> Result<()> {
        let mut ownership = OwnershipDb::load(&self.paths)?;
        let session = Session::create(self.runner.clone(), &PACKAGES)?;
        let baseline = session.packages()?;
        recover_section_phases(
            &mut ownership,
            &baseline,
            self.paths.openvpn_proto.is_file(),
        )?;
        ownership.save(&self.paths)?;
        for row in rows.iter().filter(|row| row.kind == InterfaceKind::Openvpn) {
            let key = section_key("openvpn", &row.logical);
            let Some(record) = ownership.sections.get_mut(&key) else {
                continue;
            };
            let Some(section) = package_section(&baseline, "openvpn", &row.logical) else {
                continue;
            };
            if !record_authorizes(record, section)? {
                bail!("OpenVPN UCI ownership changed: {}", row.logical);
            }
            let mut desired = section.clone();
            desired
                .as_object_mut()
                .context("OpenVPN UCI section is not an object")?
                .insert("enabled".into(), Value::String("0".into()));
            let before = section_fingerprint(section)?;
            let after = section_fingerprint(&desired)?;
            if before == after {
                continue;
            }
            session.set_option("openvpn", &row.logical, "enabled", "0")?;
            record.phase = Phase::Updating;
            record.before = Some(before);
            record.after = Some(after);
        }
        ownership.save(&self.paths)?;
        if session.changed_packages()?.contains("openvpn") {
            let expected = session.get_package("openvpn")?;
            let before_commit = session.fresh_packages()?;
            ensure_package_unchanged(&baseline, &before_commit, "openvpn")?;
            session.commit("openvpn")?;
            atomic::sync_dir(&self.paths.uci_config_dir)?;
            let after_commit = session.fresh_packages()?;
            if canonical_package(expected.as_ref())?
                != canonical_package(after_commit.get("openvpn"))?
            {
                bail!("UCI package changed while disabling OpenVPN");
            }
        }
        atomic::sync_dir(&self.paths.uci_config_dir)?;
        let live = session.fresh_packages()?;
        promote_section_phases(&mut ownership, &live)?;
        ownership.save(&self.paths)?;
        session.destroy()
    }

    pub fn purge(&self, rows: &[ManifestEntry]) -> Result<SyncOutcome> {
        self.sync(&[], rows, None)
    }

    fn assert_default_delta_clean(&self) -> Result<()> {
        for package in PACKAGES {
            let output = self.runner.output("uci", ["changes", package])?;
            if output.status.success() {
                if !output.stdout.is_empty() {
                    bail!("uncommitted UCI changes exist for {package}");
                }
            } else {
                let config = self.paths.uci_config_dir.join(package);
                if output.status.code() == Some(1)
                    && matches!(
                        fs::symlink_metadata(&config),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound
                    )
                {
                    continue;
                }
                bail!(
                    "could not inspect UCI changes for {package}: {}",
                    output.status
                );
            }
        }
        Ok(())
    }

    fn validate_legacy_openvpn_paths(
        &self,
        desired: &[ManifestEntry],
        native_openvpn: bool,
    ) -> Result<()> {
        if native_openvpn {
            return Ok(());
        }
        for entry in desired
            .iter()
            .filter(|entry| entry.kind == InterfaceKind::Openvpn)
        {
            let absolute = format!("/etc/openvpn/{}.conf", entry.logical);
            let path = atomic::rooted(self.paths.root.as_deref(), &absolute);
            match fs::symlink_metadata(&path) {
                Ok(_) => bail!(
                    "OpenVPN path instance conflicts with managed logical name: {}",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn replay_pending_reload(&self) -> Result<()> {
        replay_reload_intent(&self.paths, &self.runner)
    }
}

#[derive(Clone)]
struct Session<R: Runner> {
    runner: R,
    id: String,
    packages: Vec<String>,
}

impl<R: Runner> Session<R> {
    fn create(runner: R, packages: &[&str]) -> Result<Self> {
        let response = ubus(&runner, "session", "create", json!({"timeout": 300}), false)?
            .context("session.create returned not-found")?;
        let id = response
            .get("ubus_rpc_session")
            .and_then(Value::as_str)
            .context("session.create omitted ubus_rpc_session")?
            .to_owned();
        validate_session(&id)?;
        let objects: Vec<_> = packages
            .iter()
            .flat_map(|package| [json!([package, "read"]), json!([package, "write"])])
            .collect();
        ubus(
            &runner,
            "session",
            "grant",
            json!({
                "ubus_rpc_session": id,
                "scope": "uci",
                "objects": objects,
            }),
            false,
        )?;
        Ok(Self {
            runner,
            id,
            packages: packages.iter().map(|value| (*value).to_owned()).collect(),
        })
    }

    fn request(&self, package: &str) -> Map<String, Value> {
        Map::from_iter([
            ("ubus_rpc_session".into(), Value::String(self.id.clone())),
            ("config".into(), Value::String(package.into())),
        ])
    }

    fn packages(&self) -> Result<Value> {
        let mut output = Map::new();
        for package in &self.packages {
            if let Some(value) = self.get_package(package)? {
                output.insert(package.clone(), value);
            }
        }
        Ok(Value::Object(output))
    }

    fn fresh_packages(&self) -> Result<Value> {
        let package_names: Vec<_> = self.packages.iter().map(String::as_str).collect();
        let fresh = Self::create(self.runner.clone(), &package_names)?;
        let value = fresh.packages();
        fresh.destroy()?;
        value
    }

    fn get_package(&self, package: &str) -> Result<Option<Value>> {
        let response = ubus(
            &self.runner,
            "uci",
            "get",
            Value::Object(self.request(package)),
            true,
        )?;
        Ok(response.and_then(|value| value.get("values").cloned()))
    }

    fn add_section(&self, package: &str, section: &str, kind: &str) -> Result<()> {
        let mut request = self.request(package);
        request.insert("name".into(), Value::String(section.into()));
        request.insert("type".into(), Value::String(kind.into()));
        ubus(&self.runner, "uci", "add", Value::Object(request), false)?;
        Ok(())
    }

    fn set_option(&self, package: &str, section: &str, option: &str, value: &str) -> Result<()> {
        let mut request = self.request(package);
        request.insert("section".into(), Value::String(section.into()));
        request.insert("values".into(), json!({option: value}));
        ubus(&self.runner, "uci", "set", Value::Object(request), false)?;
        Ok(())
    }

    fn delete(&self, package: &str, section: &str, option: Option<&str>) -> Result<()> {
        let mut request = self.request(package);
        request.insert("section".into(), Value::String(section.into()));
        if let Some(option) = option {
            request.insert("option".into(), Value::String(option.into()));
        }
        ubus(&self.runner, "uci", "delete", Value::Object(request), true)?;
        Ok(())
    }

    fn list_delta(
        &self,
        package: &str,
        section: &str,
        option: &str,
        value: &str,
        add: bool,
    ) -> Result<()> {
        // Read through the same rpcd session so a second edge operation sees
        // the list normalization and deltas appended by the first one.
        let package_state = self.get_package(package)?;
        let current = package_state
            .as_ref()
            .and_then(|package| package.get(section))
            .and_then(|section| section.get(option));
        let lines = list_delta_lines(package, section, option, current, value, add)?;
        if lines.is_empty() {
            return Ok(());
        }
        let savedir = Path::new("/var/run/rpcd").join(format!("uci-{}", self.id));
        atomic::ensure_dir(&savedir, 0o700)?;
        let path = savedir.join(package);
        atomic::reject_symlink(&path)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        file.sync_all()?;
        atomic::sync_dir(&savedir)
    }

    fn changed_packages(&self) -> Result<BTreeSet<String>> {
        let mut changed = BTreeSet::new();
        for package in &self.packages {
            let response = ubus(
                &self.runner,
                "uci",
                "changes",
                Value::Object(self.request(package)),
                true,
            )?;
            let Some(response) = response else { continue };
            if response
                .get("changes")
                .and_then(Value::as_array)
                .is_some_and(|rows| !rows.is_empty())
            {
                changed.insert(package.clone());
            }
        }
        Ok(changed)
    }

    fn commit(&self, package: &str) -> Result<()> {
        ubus(
            &self.runner,
            "uci",
            "commit",
            Value::Object(self.request(package)),
            false,
        )?;
        Ok(())
    }

    fn destroy(&self) -> Result<()> {
        ubus(
            &self.runner,
            "session",
            "destroy",
            json!({"ubus_rpc_session": self.id}),
            true,
        )?;
        Ok(())
    }
}

fn ubus<R: Runner>(
    runner: &R,
    object: &str,
    method: &str,
    request: Value,
    not_found_ok: bool,
) -> Result<Option<Value>> {
    let request = serde_json::to_string(&request)?;
    let output = runner.output("ubus", ["-S", "call", object, method, request.as_str()])?;
    if !output.status.success() {
        if not_found_ok && output.status.code() == Some(4) {
            return Ok(None);
        }
        bail!("ubus {object} {method} failed with {}", output.status);
    }
    if output.stdout.is_empty() {
        return Ok(Some(json!({})));
    }
    Ok(Some(
        serde_json::from_slice(&output.stdout).context("invalid ubus JSON")?,
    ))
}

fn validate_session(id: &str) -> Result<()> {
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid rpcd session identifier");
    }
    Ok(())
}

fn package_section<'a>(packages: &'a Value, package: &str, section: &str) -> Option<&'a Value> {
    packages.get(package)?.get(section)
}

fn canonical_section(value: &Value) -> Result<Value> {
    let object = value.as_object().context("UCI section is not an object")?;
    let section_type = object
        .get(".type")
        .and_then(Value::as_str)
        .context("UCI section omitted .type")?;
    let mut options = BTreeMap::new();
    for (name, value) in object.iter().filter(|(name, _)| !name.starts_with('.')) {
        let value = match value {
            Value::Array(values) => json!({"kind":"list", "value": values}),
            Value::Null => continue,
            value => {
                json!({"kind":"scalar", "value": value.as_str().unwrap_or(&value.to_string())})
            }
        };
        options.insert(name.clone(), value);
    }
    Ok(json!({
        "type": section_type,
        "anonymous": object.get(".anonymous").and_then(Value::as_bool).unwrap_or(false),
        "options": options,
    }))
}

fn section_fingerprint(value: &Value) -> Result<String> {
    let encoded = serde_json::to_vec(&canonical_section(value)?)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}

fn canonical_package(value: Option<&Value>) -> Result<Value> {
    let Some(value) = value else {
        return Ok(Value::Null);
    };
    let object = value.as_object().context("UCI package is not an object")?;
    let mut sections = Vec::with_capacity(object.len());
    for (name, section) in object {
        let section_object = section
            .as_object()
            .context("UCI section is not an object")?;
        let anonymous = section_object
            .get(".anonymous")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let index = section_object
            .get(".index")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX);
        sections.push((
            index,
            (!anonymous).then(|| name.clone()),
            canonical_section(section)?,
        ));
    }
    sections.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.to_string().cmp(&right.2.to_string()))
    });
    Ok(serde_json::to_value(sections)?)
}

fn ensure_package_unchanged(baseline: &Value, live: &Value, package: &str) -> Result<()> {
    if canonical_package(baseline.get(package))? != canonical_package(live.get(package))? {
        bail!("UCI package changed after validation: {package}");
    }
    Ok(())
}

fn section_nonce(value: &Value) -> Option<&str> {
    value.get("meduza_nonce").and_then(Value::as_str)
}

fn section_key(package: &str, section: &str) -> String {
    format!("{package}.{section}")
}

fn record_authorizes(record: &SectionRecord, section: &Value) -> Result<bool> {
    let fingerprint = section_fingerprint(section)?;
    Ok(section_nonce(section) == Some(record.nonce.as_str())
        && match record.phase {
            Phase::Owned => record.after.as_deref() == Some(fingerprint.as_str()),
            Phase::Updating | Phase::Deleting => {
                record.before.as_deref() == Some(fingerprint.as_str())
                    || record.after.as_deref() == Some(fingerprint.as_str())
            }
            Phase::Creating => record.after.as_deref() == Some(fingerprint.as_str()),
            _ => false,
        })
}

/// Prove a live UCI section against both the durable external record and the
/// complete owner-recorded fingerprint. Inline nonce/owner options by
/// themselves are deliberately insufficient deletion or stop authority.
pub(crate) fn live_section_owned<R: Runner>(
    paths: &Paths,
    runner: R,
    package: &str,
    section: &str,
) -> Result<bool> {
    if !matches!(package, "network" | "openvpn") {
        bail!("unsupported managed UCI package: {package}");
    }
    let ownership = OwnershipDb::load(paths)?;
    let key = section_key(package, section);
    let Some(record) = ownership.sections.get(&key) else {
        return Ok(false);
    };
    if record.package != package || record.section != section {
        bail!("UCI ownership record identity changed: {key}");
    }
    let session = Session::create(runner, &[package])?;
    let result = session
        .get_package(package)?
        .as_ref()
        .and_then(|value| value.get(section))
        .map(|live| record_authorizes(record, live))
        .transpose()
        .map(|value| value.unwrap_or(false));
    session.destroy()?;
    result
}

fn validate_desired_section(
    packages: &Value,
    ownership: &OwnershipDb,
    entry: &ManifestEntry,
    native_openvpn: bool,
) -> Result<()> {
    if let Some(section) = package_section(packages, "network", &entry.logical) {
        let key = section_key("network", &entry.logical);
        let record = ownership
            .sections
            .get(&key)
            .with_context(|| format!("existing UCI section is not externally owned: {key}"))?;
        if !record_authorizes(record, section)? {
            bail!("UCI ownership changed: {key}");
        }
    }
    if entry.kind != InterfaceKind::Openvpn {
        return Ok(());
    }

    let key = section_key("openvpn", &entry.logical);
    let live = package_section(packages, "openvpn", &entry.logical);
    let record = ownership.sections.get(&key);
    if native_openvpn {
        // A foreign legacy helper is not ours to remove. If an external record
        // exists, however, its exact live generation must still agree before
        // the native migration is allowed to retire it.
        if let (Some(record), Some(live)) = (record, live)
            && !record_authorizes(record, live)?
        {
            bail!("OpenVPN UCI ownership changed: {key}");
        }
    } else if let Some(live) = live {
        let record =
            record.with_context(|| format!("existing OpenVPN UCI section is not owned: {key}"))?;
        if !record_authorizes(record, live)? {
            bail!("OpenVPN UCI ownership changed: {key}");
        }
    } else if record.is_some_and(|record| record.phase == Phase::Owned) {
        bail!("owned OpenVPN UCI section disappeared: {key}");
    }
    Ok(())
}

fn desired_network_options(
    entry: &ManifestEntry,
    nonce: &str,
    native_openvpn: bool,
) -> BTreeMap<&'static str, String> {
    let mut values = BTreeMap::from([
        ("auto", "0".into()),
        ("defaultroute", "0".into()),
        ("delegate", "0".into()),
        ("meduza_config", entry.config.display().to_string()),
        ("meduza_device", entry.device.clone()),
        ("meduza_instance", entry.instance.clone()),
        ("meduza_kind", entry.kind.as_str().into()),
        ("meduza_nonce", nonce.into()),
        ("meduza_owner", OWNER.into()),
        ("peerdns", "0".into()),
    ]);
    if native_openvpn && entry.kind == InterfaceKind::Openvpn {
        values.insert("proto", "openvpn".into());
        values.insert("config", entry.config.display().to_string());
        values.insert("script_security", "3".into());
        values.insert(
            "up",
            entry
                .config
                .parent()
                .expect("validated config")
                .join("link-up")
                .display()
                .to_string(),
        );
    } else {
        values.insert("proto", "none".into());
        values.insert("device", entry.device.clone());
    }
    values
}

fn desired_openvpn_options(entry: &ManifestEntry, nonce: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        ("enabled", "1".into()),
        ("config", entry.config.display().to_string()),
        ("dev", entry.device.clone()),
        ("meduza_device", entry.device.clone()),
        ("meduza_instance", entry.instance.clone()),
        ("meduza_nonce", nonce.into()),
        ("meduza_owner", OWNER.into()),
    ])
}

fn stage_section_options<R: Runner>(
    session: &Session<R>,
    live: Option<&Value>,
    package: &str,
    section: &str,
    options: &BTreeMap<&str, String>,
) -> Result<()> {
    if let Some(live) = live {
        let object = live.as_object().context("UCI section is not an object")?;
        for name in object.keys().filter(|name| !name.starts_with('.')) {
            if !options.contains_key(name.as_str()) {
                session.delete(package, section, Some(name))?;
            }
        }
    }
    for (name, value) in options {
        session.set_option(package, section, name, value)?;
    }
    Ok(())
}

fn ensure_interface_section<R: Runner>(
    session: &Session<R>,
    baseline: &Value,
    ownership: &mut OwnershipDb,
    entry: &ManifestEntry,
    native_openvpn: bool,
) -> Result<bool> {
    let key = section_key("network", &entry.logical);
    let live = package_section(baseline, "network", &entry.logical);
    let nonce = if let Some(record) = ownership.sections.get(&key) {
        if let Some(live) = live {
            if !record_authorizes(record, live)? {
                bail!("refusing to replace non-Meduza UCI section: {key}");
            }
        } else if record.phase == Phase::Owned {
            bail!("owned UCI section disappeared: {key}");
        }
        record.nonce.clone()
    } else {
        if live.is_some() {
            bail!("refusing to adopt existing UCI section: {key}");
        }
        atomic::random_nonce()
    };
    let options = desired_network_options(entry, &nonce, native_openvpn);
    let desired = section_value("interface", &options);
    let desired_fingerprint = section_fingerprint(&desired)?;
    let changed = live.map(canonical_section).transpose()? != Some(canonical_section(&desired)?);
    if !changed {
        if let Some(record) = ownership.sections.get_mut(&key) {
            record.phase = Phase::Owned;
            record.before = None;
            record.after = Some(desired_fingerprint);
        }
        return Ok(false);
    }

    let before = live.map(section_fingerprint).transpose()?;
    let after = Some(desired_fingerprint);
    ownership.sections.insert(
        key.clone(),
        SectionRecord {
            nonce,
            phase: if live.is_some() {
                Phase::Updating
            } else {
                Phase::Creating
            },
            package: "network".into(),
            section: entry.logical.clone(),
            before,
            after,
        },
    );
    if live.is_none() {
        session.add_section("network", &entry.logical, "interface")?;
    }
    stage_section_options(session, live, "network", &entry.logical, &options)?;
    Ok(true)
}

fn ensure_openvpn_section<R: Runner>(
    session: &Session<R>,
    baseline: &Value,
    ownership: &mut OwnershipDb,
    entry: &ManifestEntry,
) -> Result<bool> {
    let key = section_key("openvpn", &entry.logical);
    let live = package_section(baseline, "openvpn", &entry.logical);
    let nonce = if let Some(record) = ownership.sections.get(&key) {
        if record.package != "openvpn" || record.section != entry.logical {
            bail!("OpenVPN UCI ownership identity changed: {key}");
        }
        if let Some(live) = live {
            if !record_authorizes(record, live)? {
                bail!("refusing to replace non-Meduza OpenVPN section: {key}");
            }
        } else if record.phase == Phase::Owned {
            bail!("owned OpenVPN UCI section disappeared: {key}");
        }
        record.nonce.clone()
    } else {
        if live.is_some() {
            bail!("refusing to adopt existing OpenVPN UCI section: {key}");
        }
        atomic::random_nonce()
    };
    let options = desired_openvpn_options(entry, &nonce);
    let desired = section_value("openvpn", &options);
    let desired_fingerprint = section_fingerprint(&desired)?;
    let changed = live.map(canonical_section).transpose()? != Some(canonical_section(&desired)?);
    if !changed {
        if let Some(record) = ownership.sections.get_mut(&key) {
            record.phase = Phase::Owned;
            record.before = None;
            record.after = Some(desired_fingerprint);
        }
        return Ok(false);
    }

    ownership.sections.insert(
        key,
        SectionRecord {
            nonce,
            phase: if live.is_some() {
                Phase::Updating
            } else {
                Phase::Creating
            },
            package: "openvpn".into(),
            section: entry.logical.clone(),
            before: live.map(section_fingerprint).transpose()?,
            after: Some(desired_fingerprint),
        },
    );
    if live.is_none() {
        session.add_section("openvpn", &entry.logical, "openvpn")?;
    }
    stage_section_options(session, live, "openvpn", &entry.logical, &options)?;
    Ok(true)
}

fn section_value(kind: &str, options: &BTreeMap<&str, String>) -> Value {
    let mut value = Map::from_iter([
        (".type".into(), Value::String(kind.into())),
        (".anonymous".into(), Value::Bool(false)),
    ]);
    value.extend(
        options
            .iter()
            .map(|(key, value)| ((*key).into(), Value::String(value.clone()))),
    );
    Value::Object(value)
}

fn delete_owned_section<R: Runner>(
    session: &Session<R>,
    baseline: &Value,
    ownership: &mut OwnershipDb,
    package: &str,
    section: &str,
) -> Result<bool> {
    let key = section_key(package, section);
    let Some(record) = ownership.sections.get_mut(&key) else {
        return Ok(false);
    };
    let Some(live) = package_section(baseline, package, section) else {
        record.phase = Phase::Retired;
        return Ok(false);
    };
    if !record_authorizes(record, live)? {
        bail!("refusing to delete changed UCI section: {key}");
    }
    record.phase = Phase::Deleting;
    record.before = Some(section_fingerprint(live)?);
    record.after = None;
    session.delete(package, section, None)?;
    Ok(true)
}

fn recover_section_phases(
    ownership: &mut OwnershipDb,
    packages: &Value,
    retire_missing_openvpn: bool,
) -> Result<()> {
    for record in ownership.sections.values_mut() {
        let live = package_section(packages, &record.package, &record.section);
        match record.phase {
            Phase::Creating | Phase::Updating => {
                if let Some(live) = live {
                    let fingerprint = section_fingerprint(live)?;
                    if section_nonce(live) == Some(record.nonce.as_str())
                        && record.after.as_deref() == Some(fingerprint.as_str())
                    {
                        record.phase = Phase::Owned;
                    } else if record.before.as_deref() != Some(fingerprint.as_str()) {
                        bail!("UCI transition conflicts with live section");
                    }
                } else if record.before.is_some() {
                    bail!("UCI section disappeared during update");
                }
            }
            Phase::Deleting if live.is_none() => record.phase = Phase::Retired,
            Phase::Deleting => {
                if !record_authorizes(record, live.expect("checked"))? {
                    bail!("UCI delete transition conflicts with live section");
                }
            }
            Phase::Owned
                if live.is_none() && retire_missing_openvpn && record.package == "openvpn" =>
            {
                record.phase = Phase::Retired;
            }
            Phase::Owned => {
                let live = live.context("owned UCI section disappeared")?;
                if !record_authorizes(record, live)? {
                    bail!("owned UCI section was changed externally");
                }
            }
            Phase::Retired if live.is_some() => bail!("retired UCI section reappeared"),
            _ => {}
        }
    }
    Ok(())
}

fn promote_section_phases(ownership: &mut OwnershipDb, packages: &Value) -> Result<()> {
    recover_section_phases(ownership, packages, false)
}

fn find_zone<'a>(packages: &'a Value, name: &str) -> Result<Option<(&'a str, &'a Value)>> {
    let Some(firewall) = packages.get("firewall").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut found = firewall.iter().filter(|(_, value)| {
        value.get(".type").and_then(Value::as_str) == Some("zone")
            && value.get("name").and_then(Value::as_str) == Some(name)
    });
    let first = found.next().map(|(key, value)| (key.as_str(), value));
    if found.next().is_some() {
        bail!("multiple firewall zones are named {name}");
    }
    Ok(first)
}

fn list_tokens(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(value)) => value.split_whitespace().map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

fn delta_quote(value: &str) -> Result<String> {
    if value.contains(['\0', '\r', '\n']) {
        bail!("unsafe UCI list value");
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn list_delta_lines(
    package: &str,
    section: &str,
    option: &str,
    current: Option<&Value>,
    value: &str,
    add: bool,
) -> Result<Vec<String>> {
    let prefix = format!("{package}.{section}.{option}=");
    let mut lines = Vec::new();
    let mut scalar = false;
    let items = match current {
        Some(Value::String(current)) => {
            scalar = true;
            let tokens: Vec<_> = current.split_whitespace().map(str::to_owned).collect();
            if tokens.len() > 1 {
                let mut seen = BTreeSet::new();
                for token in &tokens {
                    if seen.insert(token.clone()) {
                        lines.push(format!("|{prefix}{}", delta_quote(token)?));
                    }
                }
                lines.push(format!("~{prefix}{}", delta_quote(current)?));
                scalar = false;
                tokens
            } else {
                vec![current.clone()]
            }
        }
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .context("UCI list contains a non-string value")
            })
            .collect::<Result<Vec<_>>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => bail!("UCI list option has an unsupported value"),
    };

    if add {
        if !items.iter().any(|item| item == value) {
            lines.push(format!("|{prefix}{}", delta_quote(value)?));
        }
    } else if items.iter().any(|item| item == value) {
        // LIST_DEL against a scalar can delete the entire option. Convert the
        // value to a real list first, then remove only the requested token.
        if scalar {
            lines.push(format!("|{prefix}{}", delta_quote(value)?));
        }
        lines.push(format!("~{prefix}{}", delta_quote(value)?));
    }
    Ok(lines)
}

fn edge_key(zone: &str, member: &str) -> String {
    format!("{zone}\0{member}")
}

fn edge_tag(member: &str) -> String {
    format!(
        "meduza_edge_{}",
        &hex::encode(Sha256::digest(member.as_bytes()))[..16]
    )
}

fn sync_firewall_edges<R: Runner>(
    session: &Session<R>,
    baseline: &Value,
    ownership: &mut OwnershipDb,
    desired: &[ManifestEntry],
    _previous: &[ManifestEntry],
    zone: Option<&str>,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let desired_members: BTreeSet<_> = desired.iter().map(|row| row.logical.as_str()).collect();
    // Retire owned edges in any old zone first; this never rebuilds the zone's
    // complete list and therefore preserves OpenClash's utun token.
    let records: Vec<_> = ownership.edges.clone().into_iter().collect();
    for (key, mut record) in records {
        let keep =
            zone == Some(record.zone.as_str()) && desired_members.contains(record.member.as_str());
        if keep || record.phase == Phase::Borrowed || record.phase == Phase::Retired {
            continue;
        }
        let Some((section, live_zone)) = find_zone(baseline, &record.zone)? else {
            record.phase = Phase::Retired;
            ownership.edges.insert(key, record);
            continue;
        };
        let tag = live_zone.get(&record.tag_option).and_then(Value::as_str);
        if record.phase == Phase::Creating
            && !list_tokens(live_zone.get("network")).contains(&record.member)
            && tag.is_none()
        {
            record.phase = Phase::Retired;
            ownership.edges.insert(key, record);
            continue;
        }
        if tag != Some(format!("owned:{}", record.nonce).as_str()) {
            bail!(
                "firewall edge ownership changed: {}/{}",
                record.zone,
                record.member
            );
        }
        if list_tokens(live_zone.get("network")).contains(&record.member) {
            session.list_delta("firewall", section, "network", &record.member, false)?;
        }
        session.set_option(
            "firewall",
            section,
            &record.tag_option,
            &format!("removed:{}", record.nonce),
        )?;
        record.phase = Phase::Deleting;
        ownership.edges.insert(key, record);
        outcome.firewall_changed = true;
    }

    let Some(zone_name) = zone else { return Ok(()) };
    let Some((section, _live_zone)) = find_zone(baseline, zone_name)? else {
        if baseline.get("firewall").is_none() {
            // Firewall is optional on reduced images; no package means there
            // is no policy object for Meduza to mutate.
            return Ok(());
        }
        bail!("VPN_FIREWALL_ZONE does not exist: {zone_name}");
    };
    for entry in desired {
        let key = edge_key(zone_name, &entry.logical);
        let network_key = section_key("network", &entry.logical);
        let network_nonce = ownership
            .sections
            .get(&network_key)
            .context("firewall edge has no network generation")?
            .nonce
            .clone();
        let session_firewall = session
            .get_package("firewall")?
            .context("firewall package disappeared during edge sync")?;
        let session_zone = session_firewall
            .get(section)
            .context("firewall zone disappeared during edge sync")?;
        let current = list_tokens(session_zone.get("network"));
        let tag_option = edge_tag(&entry.logical);
        let live_tag = session_zone.get(&tag_option).and_then(Value::as_str);
        let mut retired_tag_authorized = false;
        if let Some(mut record) = ownership.edges.get(&key).cloned() {
            match record.phase {
                Phase::Owned => {
                    if record.network_nonce != network_nonce {
                        bail!("firewall edge no longer matches its network generation");
                    }
                    continue;
                }
                Phase::Borrowed if current.contains(&entry.logical) => continue,
                Phase::Borrowed => {
                    ownership.edges.remove(&key);
                }
                Phase::Creating => {
                    if record.network_nonce != network_nonce
                        || record.tag_option != tag_option
                        || live_tag.is_some()
                        || current.contains(&entry.logical)
                    {
                        bail!("firewall edge creation cannot be safely replayed");
                    }
                    session.list_delta("firewall", section, "network", &entry.logical, true)?;
                    session.set_option(
                        "firewall",
                        section,
                        &record.tag_option,
                        &format!("owned:{}", record.nonce),
                    )?;
                    outcome.firewall_changed = true;
                    continue;
                }
                Phase::Deleting => {
                    let owned_tag = format!("owned:{}", record.nonce);
                    if record.network_nonce != network_nonce
                        || record.tag_option != tag_option
                        || !current.contains(&entry.logical)
                        || live_tag != Some(owned_tag.as_str())
                    {
                        bail!("firewall edge deletion cannot be safely cancelled");
                    }
                    record.phase = Phase::Owned;
                    ownership.edges.insert(key.clone(), record);
                    continue;
                }
                Phase::Retired => {
                    let removed_tag = format!("removed:{}", record.nonce);
                    if current.contains(&entry.logical)
                        || (live_tag.is_some() && live_tag != Some(removed_tag.as_str()))
                    {
                        bail!("retired firewall edge was replaced before reuse");
                    }
                    retired_tag_authorized = live_tag == Some(removed_tag.as_str());
                }
                Phase::Updating => bail!("invalid updating phase for firewall edge"),
            }
        }
        if current.contains(&entry.logical) {
            if live_tag.is_some() {
                bail!("foreign firewall member collides with Meduza edge tag");
            }
            ownership.edges.insert(
                key,
                EdgeRecord {
                    nonce: String::new(),
                    phase: Phase::Borrowed,
                    zone: zone_name.into(),
                    member: entry.logical.clone(),
                    network_nonce,
                    tag_option: String::new(),
                },
            );
            continue;
        }
        if live_tag.is_some() && !retired_tag_authorized {
            bail!("refusing to overwrite an unowned firewall edge tag");
        }
        let nonce = atomic::random_nonce();
        ownership.edges.insert(
            key,
            EdgeRecord {
                nonce: nonce.clone(),
                phase: Phase::Creating,
                zone: zone_name.into(),
                member: entry.logical.clone(),
                network_nonce,
                tag_option: tag_option.clone(),
            },
        );
        session.list_delta("firewall", section, "network", &entry.logical, true)?;
        session.set_option("firewall", section, &tag_option, &format!("owned:{nonce}"))?;
        outcome.firewall_changed = true;
    }
    Ok(())
}

fn recover_edge_phases(ownership: &mut OwnershipDb, packages: &Value) -> Result<()> {
    promote_edge_phases(ownership, packages)
}

fn edge_phase_after_observation(
    phase: &Phase,
    present: bool,
    tag: Option<&str>,
    nonce: &str,
) -> Result<Phase> {
    let owned = format!("owned:{nonce}");
    let removed = format!("removed:{nonce}");
    match phase {
        Phase::Creating if present && tag == Some(owned.as_str()) => Ok(Phase::Owned),
        // Durable intent exists but the firewall package was not committed.
        Phase::Creating if !present && tag.is_none() => Ok(Phase::Creating),
        Phase::Creating => bail!("firewall edge creation conflicts with live state"),
        Phase::Deleting if !present && tag == Some(removed.as_str()) => Ok(Phase::Retired),
        // The pre-delete generation is still exact, so deletion may be replayed.
        Phase::Deleting if present && tag == Some(owned.as_str()) => Ok(Phase::Deleting),
        Phase::Deleting => bail!("firewall edge deletion conflicts with live state"),
        Phase::Owned if present && tag == Some(owned.as_str()) => Ok(Phase::Owned),
        Phase::Owned => bail!("managed firewall edge changed externally"),
        Phase::Retired if !present && (tag.is_none() || tag == Some(removed.as_str())) => {
            Ok(Phase::Retired)
        }
        Phase::Retired => bail!("retired firewall edge reappeared"),
        Phase::Borrowed => Ok(Phase::Borrowed),
        Phase::Updating => bail!("invalid updating phase for firewall edge"),
    }
}

fn promote_edge_phases(ownership: &mut OwnershipDb, packages: &Value) -> Result<()> {
    for record in ownership.edges.values_mut() {
        let Some((_section, zone)) = find_zone(packages, &record.zone)? else {
            if record.phase == Phase::Deleting || record.phase == Phase::Retired {
                record.phase = Phase::Retired;
                continue;
            }
            if record.phase == Phase::Borrowed {
                continue;
            }
            bail!("managed firewall zone disappeared: {}", record.zone);
        };
        let present = list_tokens(zone.get("network")).contains(&record.member);
        let tag = zone.get(&record.tag_option).and_then(Value::as_str);
        record.phase = edge_phase_after_observation(&record.phase, present, tag, &record.nonce)?;
    }
    Ok(())
}

fn reload_intent_path(paths: &Paths) -> PathBuf {
    paths.managed.join("uci-reload.pending")
}

fn persist_reload_intent(paths: &Paths, intent: &ReloadIntent) -> Result<()> {
    if intent.version != 1 || (!intent.network && !intent.firewall) {
        bail!("invalid UCI reload intent");
    }
    atomic::atomic_json_bounded(&reload_intent_path(paths), intent, MAX_RELOAD_INTENT_BYTES)?;
    Ok(())
}

fn replay_reload_intent<R: Runner>(paths: &Paths, runner: &R) -> Result<()> {
    let path = reload_intent_path(paths);
    if !regular_file_exists(&path)? {
        return Ok(());
    }
    let bytes = atomic::read_bounded(&path, MAX_RELOAD_INTENT_BYTES)?;
    let intent: ReloadIntent =
        serde_json::from_slice(&bytes).context("invalid persistent UCI reload intent")?;
    if intent.version != 1 || (!intent.network && !intent.firewall) {
        bail!("unsupported persistent UCI reload intent");
    }
    if intent.network {
        runner.status("ubus", ["call", "network", "reload", "{}"])?;
    }
    if intent.firewall {
        runner.status("/etc/init.d/firewall", ["reload"])?;
    }
    atomic::durable_remove(&path)?;
    Ok(())
}

fn find_zone_with_tag<'a>(
    packages: &'a Value,
    option: &str,
    expected: &str,
) -> Result<Option<(&'a str, &'a Value)>> {
    let Some(firewall) = packages.get("firewall").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut found = firewall.iter().filter(|(_, value)| {
        value.get(".type").and_then(Value::as_str) == Some("zone")
            && value.get(option).and_then(Value::as_str) == Some(expected)
    });
    let first = found.next().map(|(section, zone)| (section.as_str(), zone));
    if found.next().is_some() {
        bail!("managed firewall edge tag appears in multiple zones");
    }
    Ok(first)
}

pub fn finalize_ownership<R: Runner>(paths: &Paths, runner: R, purge: bool) -> Result<()> {
    let mut ownership = OwnershipDb::load(paths)?;
    let session = Session::create(runner, &PACKAGES)?;
    let packages = session.packages()?;
    let managed: BTreeSet<_> = if purge {
        BTreeSet::new()
    } else {
        crate::state::read_manifest(&paths.manifest)?
            .into_iter()
            .map(|row| row.logical)
            .collect()
    };

    let mut edge_gc = Vec::new();
    let mut tag_cleanup = Vec::new();
    for (key, record) in ownership.edges.clone() {
        if record.phase == Phase::Borrowed {
            edge_gc.push(key);
            continue;
        }
        if record.phase != Phase::Retired {
            continue;
        }
        if record.tag_option != edge_tag(&record.member)
            || record.nonce.len() != 32
            || !record.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("invalid retired firewall edge identity");
        }
        let removed_tag = format!("removed:{}", record.nonce);
        if let Some((section, zone)) =
            find_zone_with_tag(&packages, &record.tag_option, &removed_tag)?
        {
            if list_tokens(zone.get("network")).contains(&record.member) {
                bail!("retired firewall edge member reappeared");
            }
            let zone_name = zone
                .get("name")
                .and_then(Value::as_str)
                .context("tagged firewall zone has no name")?;
            session.delete("firewall", section, Some(&record.tag_option))?;
            tag_cleanup.push((key, record, section.to_owned(), zone_name.to_owned()));
            continue;
        }

        if let Some((_section, zone)) = find_zone(&packages, &record.zone)? {
            if list_tokens(zone.get("network")).contains(&record.member) {
                bail!("retired firewall edge is still live");
            }
            if zone.get(&record.tag_option).is_some() {
                bail!("retired firewall edge tag changed");
            }
        }
        edge_gc.push(key);
    }

    if !tag_cleanup.is_empty() {
        // Recheck exact nonce authority from a fresh session immediately before
        // committing the isolated option deletions.
        let before_commit = session.fresh_packages()?;
        for (_key, record, section, _zone_name) in &tag_cleanup {
            let removed_tag = format!("removed:{}", record.nonce);
            let (fresh_section, zone) =
                find_zone_with_tag(&before_commit, &record.tag_option, &removed_tag)?
                    .context("retired firewall edge tag changed before cleanup")?;
            if fresh_section != section || list_tokens(zone.get("network")).contains(&record.member)
            {
                bail!("retired firewall edge changed before cleanup");
            }
        }
        if session.changed_packages()?.contains("firewall") {
            session.commit("firewall")?;
            atomic::sync_dir(&paths.uci_config_dir)?;
        }
        let after_commit = session.fresh_packages()?;
        for (key, record, _section, zone_name) in &tag_cleanup {
            let removed_tag = format!("removed:{}", record.nonce);
            if find_zone_with_tag(&after_commit, &record.tag_option, &removed_tag)?.is_some() {
                bail!("retired firewall edge tag cleanup did not commit");
            }
            if let Some((_section, zone)) = find_zone(&after_commit, zone_name)?
                && (list_tokens(zone.get("network")).contains(&record.member)
                    || zone.get(&record.tag_option).is_some())
            {
                bail!("retired firewall edge changed during cleanup");
            }
            edge_gc.push(key.clone());
        }
    }

    for key in edge_gc {
        ownership.edges.remove(&key);
    }

    for (key, record) in ownership.sections.clone() {
        if record.phase != Phase::Retired
            || package_section(&packages, &record.package, &record.section).is_some()
            || (managed.contains(&record.section) && record.package == "network")
        {
            continue;
        }
        if record.package == "network"
            && ownership
                .edges
                .values()
                .any(|edge| edge.network_nonce == record.nonce)
        {
            bail!("retired network section still owns firewall edges");
        }
        ownership.sections.remove(&key);
    }
    ownership.save(paths)?;
    session.destroy()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::process::Output;

    use super::*;

    #[derive(Clone, Copy)]
    struct NoCommands;

    impl Runner for NoCommands {
        fn output<I, S>(&self, _program: &str, _args: I) -> Result<Output>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            panic!("oversized reload intent must fail before executing commands")
        }
    }

    #[test]
    fn reload_intent_reader_rejects_an_oversized_journal() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let path = reload_intent_path(&paths);
        let file = fs::File::create(path).unwrap();
        file.set_len((MAX_RELOAD_INTENT_BYTES + 1) as u64).unwrap();

        assert!(replay_reload_intent(&paths, &NoCommands).is_err());
    }

    #[test]
    fn canonical_section_ignores_rpcd_identity() {
        let one = json!({".type":"zone", ".name":"cfg01", "name":"lan", "network":["lan","utun"]});
        let two = json!({".type":"zone", ".name":"cfg99", "name":"lan", "network":["lan","utun"]});
        assert_eq!(
            section_fingerprint(&one).unwrap(),
            section_fingerprint(&two).unwrap()
        );
    }

    #[test]
    fn canonical_package_ignores_anonymous_cfg_ids_but_not_named_sections() {
        let one = json!({
            "cfg01": {".type":"zone", ".anonymous":true, ".index":0, "name":"lan"},
            "wg_office": {".type":"interface", ".anonymous":false, ".index":1, "proto":"none"}
        });
        let two = json!({
            "cfg99": {".type":"zone", ".anonymous":true, ".index":0, "name":"lan"},
            "wg_office": {".type":"interface", ".anonymous":false, ".index":1, "proto":"none"}
        });
        assert_eq!(
            canonical_package(Some(&one)).unwrap(),
            canonical_package(Some(&two)).unwrap()
        );

        let renamed = json!({
            "cfg99": {".type":"zone", ".anonymous":true, ".index":0, "name":"lan"},
            "wg_user": {".type":"interface", ".anonymous":false, ".index":1, "proto":"none"}
        });
        assert_ne!(
            canonical_package(Some(&one)).unwrap(),
            canonical_package(Some(&renamed)).unwrap()
        );
    }

    #[test]
    fn edge_tag_is_stable_and_namespaced() {
        let value = edge_tag("wg_office");
        assert!(value.starts_with("meduza_edge_"));
        assert_eq!(value.len(), "meduza_edge_".len() + 16);
    }

    #[test]
    fn edge_phases_accept_exact_pre_and_post_commit_states() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            edge_phase_after_observation(&Phase::Creating, false, None, nonce).unwrap(),
            Phase::Creating
        );
        assert_eq!(
            edge_phase_after_observation(
                &Phase::Creating,
                true,
                Some(&format!("owned:{nonce}")),
                nonce,
            )
            .unwrap(),
            Phase::Owned
        );
        assert_eq!(
            edge_phase_after_observation(
                &Phase::Deleting,
                true,
                Some(&format!("owned:{nonce}")),
                nonce,
            )
            .unwrap(),
            Phase::Deleting
        );
        assert_eq!(
            edge_phase_after_observation(
                &Phase::Deleting,
                false,
                Some(&format!("removed:{nonce}")),
                nonce,
            )
            .unwrap(),
            Phase::Retired
        );
        assert!(
            edge_phase_after_observation(&Phase::Creating, true, None, nonce).is_err(),
            "a token that appeared without the nonce tag must not be adopted"
        );
    }

    #[test]
    fn scalar_zone_members_are_normalized_with_only_list_deltas() {
        let current = Value::String("lan utun".into());
        let lines = list_delta_lines(
            "firewall",
            "cfg001",
            "network",
            Some(&current),
            "wg_office",
            true,
        )
        .unwrap();
        assert_eq!(
            lines,
            vec![
                "|firewall.cfg001.network='lan'",
                "|firewall.cfg001.network='utun'",
                "~firewall.cfg001.network='lan utun'",
                "|firewall.cfg001.network='wg_office'",
            ]
        );
        assert!(
            lines
                .iter()
                .all(|line| matches!(line.as_bytes().first(), Some(b'|' | b'~')))
        );

        let current = Value::String("lan utun wg_office".into());
        let remove = list_delta_lines(
            "firewall",
            "cfg001",
            "network",
            Some(&current),
            "wg_office",
            false,
        )
        .unwrap();
        assert!(remove.contains(&"|firewall.cfg001.network='utun'".into()));
        assert_eq!(
            remove.last().map(String::as_str),
            Some("~firewall.cfg001.network='wg_office'")
        );
        assert!(
            remove
                .iter()
                .all(|line| matches!(line.as_bytes().first(), Some(b'|' | b'~')))
        );
    }

    #[test]
    fn legacy_openvpn_section_has_strong_identity_and_runtime_fields() {
        let entry = ManifestEntry {
            kind: InterfaceKind::Openvpn,
            instance: "office".into(),
            logical: "ovpn_office".into(),
            device: "ovpn-office".into(),
            config: "/etc/meduza/generated/openvpn/office/openvpn.conf".into(),
        };
        let nonce = "0123456789abcdef0123456789abcdef";
        let options = desired_openvpn_options(&entry, nonce);
        assert_eq!(options.get("enabled").map(String::as_str), Some("1"));
        assert_eq!(options.get("dev").map(String::as_str), Some("ovpn-office"));
        assert_eq!(options.get("meduza_nonce").map(String::as_str), Some(nonce));
        assert_eq!(
            section_value("openvpn", &options)
                .get(".type")
                .and_then(Value::as_str),
            Some("openvpn")
        );
    }

    #[test]
    fn native_openvpn_network_does_not_bind_a_static_device_or_default_route() {
        let entry = ManifestEntry {
            kind: InterfaceKind::Openvpn,
            instance: "office".into(),
            logical: "ovpn_office".into(),
            device: "ovpn-office".into(),
            config: "/etc/meduza/generated/openvpn/office/openvpn.conf".into(),
        };
        let options = desired_network_options(&entry, "0123456789abcdef0123456789abcdef", true);
        assert_eq!(options.get("proto").map(String::as_str), Some("openvpn"));
        assert_eq!(options.get("defaultroute").map(String::as_str), Some("0"));
        assert_eq!(options.get("peerdns").map(String::as_str), Some("0"));
        assert_eq!(options.get("delegate").map(String::as_str), Some("0"));
        assert_eq!(
            options.get("script_security").map(String::as_str),
            Some("3")
        );
        assert!(!options.contains_key("device"));
    }
}

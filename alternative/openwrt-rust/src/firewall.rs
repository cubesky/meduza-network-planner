use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::OWNER;
use crate::atomic;
use crate::command::Runner;
use crate::config::validate_firewall_zone;
use crate::model::{validate_device, validate_logical_name};
use crate::state::{ManifestEntry, Paths};

const MAX_FIREWALL_STATE_BYTES: usize = 1024 * 1024;
const MAX_UCI_SECTION_BYTES: usize = 64 * 1024;
const MAX_ZONE_COUNT: usize = 256;
const MAX_FORWARDING_COUNT: usize = 512;
const MEDUZA_ZONE: &str = "meduza";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FirewallState {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    records: BTreeMap<String, DeviceRecord>,
    #[serde(default)]
    zone: Option<ManagedZoneRecord>,
    #[serde(default)]
    forwardings: BTreeMap<String, ForwardingRecord>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ManagedPhase {
    Creating,
    Owned,
    Deleting,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManagedZoneRecord {
    nonce: String,
    phase: ManagedPhase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ForwardingRecord {
    src: String,
    dest: String,
    section: String,
    nonce: String,
    phase: ManagedPhase,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum MembershipKind {
    /// Compatibility for state written before logical network interfaces were
    /// introduced. These records are retired, never newly created.
    #[default]
    Device,
    Network,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceRecord {
    zone: String,
    /// The legacy JSON field is named `device`. For new records this contains
    /// the logical UCI network interface placed in the zone.
    device: String,
    #[serde(default)]
    membership: MembershipKind,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManagedZoneLive {
    section_type: String,
    name: Option<String>,
    input: Option<String>,
    output: Option<String>,
    forward: Option<String>,
    owner: Option<String>,
    nonce: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ForwardingLive {
    section_type: String,
    src: Option<String>,
    dest: Option<String>,
    owner: Option<String>,
    nonce: Option<String>,
    fields: BTreeMap<String, String>,
}

pub struct Firewall<R: Runner> {
    paths: Paths,
    runner: R,
}

impl<R: Runner> Firewall<R> {
    pub fn new(paths: Paths, runner: R) -> Self {
        Self { paths, runner }
    }

    /// Validate the administrator-selected interconnect zone before any
    /// mutation. VPN interfaces always belong to the dedicated `meduza` zone.
    /// Empty means firewall integration is disabled and previously owned
    /// objects will be retired.
    pub fn validate_zone(&self, zone: Option<&str>) -> Result<()> {
        let Some(zone) = zone else { return Ok(()) };
        validate_firewall_zone(zone)?;
        if zone == MEDUZA_ZONE {
            bail!("the Meduza interconnect zone must not be named {MEDUZA_ZONE}");
        }
        if self.paths.root.is_none() && !Path::new("/etc/init.d/firewall").is_file() {
            bail!("VPN firewall zone is configured but firewall service is not installed");
        }
        self.ensure_default_delta_clean()?;
        if self.resolve_zone(zone)?.is_none() {
            bail!("configured firewall zone does not exist: {zone}");
        }
        // Detect duplicate or malformed pre-existing dedicated zones now,
        // before generated files or runtimes are touched.
        let _ = self.resolve_zone(MEDUZA_ZONE)?;
        let state = FirewallState::load(&self.paths)?;
        if let Some(record) = &state.zone {
            let live = self.capture_managed_zone()?;
            if live.is_some()
                && !live.as_ref().is_some_and(|live| {
                    zone_live_owned(live, record)
                        || (record.phase == ManagedPhase::Deleting && zone_live_released(live))
                })
            {
                bail!("managed Meduza firewall zone changed outside Meduza");
            }
        }
        for record in state.forwardings.values() {
            if self
                .capture_forwarding(&record.section)?
                .as_ref()
                .is_some_and(|live| !forwarding_live_exact(live, record))
            {
                bail!(
                    "managed firewall forwarding changed outside Meduza: {} -> {}",
                    record.src,
                    record.dest
                );
            }
        }
        for record in state.records.values() {
            if let Some(live) = self.capture_record(record)? {
                let expected_tag = owned_tag(record);
                if live.tag.is_some() && live.tag.as_deref() != Some(expected_tag.as_str()) {
                    bail!(
                        "managed firewall membership changed outside Meduza: {}/{}",
                        record.zone,
                        record.device
                    );
                }
            }
        }
        Ok(())
    }

    /// Converge the dedicated `meduza` zone, its two inter-zone forwardings and
    /// only the exact `list network` tokens for Meduza logical interfaces.
    /// Existing zones, matching forwardings, policies and unrelated members
    /// remain administrator-owned. Legacy tagged `list device` records are
    /// retired during the same transaction.
    pub fn sync(&self, interconnect_zone: Option<&str>, entries: &[ManifestEntry]) -> Result<()> {
        self.validate_zone(interconnect_zone)?;
        let mut state = FirewallState::load(&self.paths)?;
        if interconnect_zone.is_some() {
            self.ensure_managed_zone(&mut state)?;
        }

        let desired: BTreeSet<String> = interconnect_zone
            .into_iter()
            .flat_map(|_| {
                entries.iter().map(move |entry| {
                    record_key(MEDUZA_ZONE, &entry.logical, MembershipKind::Network)
                })
            })
            .collect();

        for key in state.records.keys().cloned().collect::<Vec<_>>() {
            if desired.contains(&key) {
                self.ensure_record(&mut state, &key)?;
            } else {
                self.remove_record(&mut state, &key)?;
            }
        }

        if interconnect_zone.is_some() {
            for entry in entries {
                let key = record_key(MEDUZA_ZONE, &entry.logical, MembershipKind::Network);
                if !state.records.contains_key(&key) {
                    self.create_record(&mut state, MEDUZA_ZONE, entry)?;
                }
            }
        }

        self.sync_forwardings(&mut state, interconnect_zone)?;
        if interconnect_zone.is_none() {
            self.remove_managed_zone(&mut state)?;
        }

        if state.reload_pending {
            if Path::new("/etc/init.d/firewall").is_file() || self.paths.root.is_some() {
                self.runner.status("/etc/init.d/firewall", ["reload"])?;
            }
            state.reload_pending = false;
        }
        state.save(&self.paths)
    }

    fn ensure_managed_zone(&self, state: &mut FirewallState) -> Result<()> {
        if state.zone.is_none() {
            if self.resolve_zone(MEDUZA_ZONE)?.is_some() {
                // An administrator-created zone is borrowed as-is. Its policy
                // and lifetime remain outside Meduza ownership.
                return Ok(());
            }
            if self.uci_get("firewall.meduza")?.is_some() {
                bail!("firewall section namespace is already occupied: meduza");
            }
            state.zone = Some(ManagedZoneRecord {
                nonce: atomic::random_nonce(),
                phase: ManagedPhase::Creating,
            });
            state.save(&self.paths)?;
        }

        let record = state
            .zone
            .clone()
            .context("firewall zone record disappeared")?;
        let live = self.capture_managed_zone()?;
        let named_exact = live
            .as_ref()
            .is_some_and(|live| zone_live_owned(live, &record));
        match record.phase {
            ManagedPhase::Creating if named_exact => {
                state.zone.as_mut().expect("zone exists").phase = ManagedPhase::Owned;
                state.reload_pending = true;
                state.save(&self.paths)
            }
            ManagedPhase::Creating if live.is_none() => {
                if self.resolve_zone(MEDUZA_ZONE)?.is_some() {
                    bail!("Meduza firewall zone appeared during creation");
                }
                self.commit_zone_create(state, &record)
            }
            ManagedPhase::Owned if named_exact => Ok(()),
            ManagedPhase::Owned if live.is_none() => {
                if self.resolve_zone(MEDUZA_ZONE)?.is_some() {
                    bail!("owned Meduza firewall zone was replaced");
                }
                let replacement = ManagedZoneRecord {
                    nonce: atomic::random_nonce(),
                    phase: ManagedPhase::Creating,
                };
                state.zone = Some(replacement.clone());
                state.save(&self.paths)?;
                self.commit_zone_create(state, &replacement)
            }
            ManagedPhase::Deleting if named_exact => {
                state.zone.as_mut().expect("zone exists").phase = ManagedPhase::Owned;
                state.save(&self.paths)
            }
            ManagedPhase::Deleting if live.as_ref().is_some_and(zone_live_released) => {
                state.zone = None;
                state.save(&self.paths)
            }
            ManagedPhase::Deleting if live.is_none() => {
                if self.resolve_zone(MEDUZA_ZONE)?.is_some() {
                    // A foreign replacement is borrowed; do not create a
                    // second zone with the same name.
                    state.zone = None;
                    return state.save(&self.paths);
                }
                let replacement = ManagedZoneRecord {
                    nonce: atomic::random_nonce(),
                    phase: ManagedPhase::Creating,
                };
                state.zone = Some(replacement.clone());
                state.save(&self.paths)?;
                self.commit_zone_create(state, &replacement)
            }
            _ => bail!("managed Meduza firewall zone conflicts with live UCI"),
        }
    }

    fn commit_zone_create(
        &self,
        state: &mut FirewallState,
        record: &ManagedZoneRecord,
    ) -> Result<()> {
        state.reload_pending = true;
        state.save(&self.paths)?;
        self.ensure_default_delta_clean()?;
        let savedir = self.reset_savedir()?;
        let result = (|| {
            if self.resolve_zone(MEDUZA_ZONE)?.is_some()
                || self.uci_get("firewall.meduza")?.is_some()
            {
                bail!("Meduza firewall zone appeared before creation");
            }
            for expression in [
                "firewall.meduza=zone".to_owned(),
                format!("firewall.meduza.name={MEDUZA_ZONE}"),
                "firewall.meduza.input=ACCEPT".to_owned(),
                "firewall.meduza.output=ACCEPT".to_owned(),
                "firewall.meduza.forward=ACCEPT".to_owned(),
                format!("firewall.meduza.meduza_owner={OWNER}"),
                format!("firewall.meduza.meduza_nonce={}", record.nonce),
            ] {
                self.uci_private(&savedir, "set", &expression)?;
            }
            self.ensure_default_delta_clean()?;
            if self.resolve_zone(MEDUZA_ZONE)?.is_some()
                || self.uci_get("firewall.meduza")?.is_some()
            {
                bail!("Meduza firewall zone changed before commit");
            }
            self.uci_private(&savedir, "commit", "firewall")
        })();
        finish_private_session(result, &savedir)?;
        let live = self
            .capture_managed_zone()?
            .context("Meduza firewall zone disappeared after commit")?;
        if !zone_live_created(&live, record) {
            bail!("Meduza firewall zone creation did not commit");
        }
        state.zone.as_mut().expect("zone exists").phase = ManagedPhase::Owned;
        state.save(&self.paths)
    }

    fn remove_managed_zone(&self, state: &mut FirewallState) -> Result<()> {
        let Some(record) = state.zone.clone() else {
            return Ok(());
        };
        let live = self.capture_managed_zone()?;
        if live.is_none() {
            state.zone = None;
            if record.phase == ManagedPhase::Deleting {
                state.reload_pending = true;
            }
            return state.save(&self.paths);
        }
        if live.as_ref().is_some_and(zone_live_released) {
            state.zone = None;
            return state.save(&self.paths);
        }
        if !live
            .as_ref()
            .is_some_and(|live| zone_live_owned(live, &record))
        {
            bail!("refusing to release changed Meduza firewall zone");
        }
        state.zone.as_mut().expect("zone exists").phase = ManagedPhase::Deleting;
        state.reload_pending = true;
        state.save(&self.paths)?;
        self.ensure_default_delta_clean()?;
        let savedir = self.reset_savedir()?;
        let result = (|| {
            let current = self
                .capture_managed_zone()?
                .context("Meduza firewall zone disappeared before ownership release")?;
            if !zone_live_owned(&current, &record) {
                bail!("Meduza firewall zone changed before ownership release");
            }
            self.uci_private(&savedir, "delete", "firewall.meduza.meduza_owner")?;
            self.uci_private(&savedir, "delete", "firewall.meduza.meduza_nonce")?;
            self.ensure_default_delta_clean()?;
            let fresh = self
                .capture_managed_zone()?
                .context("Meduza firewall zone disappeared before ownership commit")?;
            if fresh != current {
                bail!("Meduza firewall zone changed before commit");
            }
            self.uci_private(&savedir, "commit", "firewall")
        })();
        finish_private_session(result, &savedir)?;
        let released = self
            .capture_managed_zone()?
            .context("Meduza firewall zone disappeared after ownership release")?;
        if !zone_live_released(&released) {
            bail!("Meduza firewall zone ownership release did not commit");
        }
        state.zone = None;
        state.save(&self.paths)
    }

    fn sync_forwardings(
        &self,
        state: &mut FirewallState,
        interconnect_zone: Option<&str>,
    ) -> Result<()> {
        let desired = interconnect_zone
            .into_iter()
            .flat_map(|zone| {
                [
                    forwarding_key(MEDUZA_ZONE, zone),
                    forwarding_key(zone, MEDUZA_ZONE),
                ]
            })
            .collect::<BTreeSet<_>>();

        for key in desired.iter().cloned().collect::<Vec<_>>() {
            if state.forwardings.contains_key(&key) {
                self.ensure_forwarding(state, &key)?;
            } else {
                let (src, dest) = split_forwarding_key(&key)?;
                self.create_forwarding(state, src, dest)?;
            }
        }
        for key in state.forwardings.keys().cloned().collect::<Vec<_>>() {
            if !desired.contains(&key) {
                self.remove_forwarding(state, &key)?;
            }
        }
        Ok(())
    }

    fn create_forwarding(&self, state: &mut FirewallState, src: &str, dest: &str) -> Result<()> {
        if self.forwarding_exists(src, dest)? {
            // Existing matching policy is borrowed and intentionally has no
            // ownership record, so cleanup can never delete it.
            return Ok(());
        }
        let section = forwarding_section(src, dest);
        if self.uci_get(&format!("firewall.{section}"))?.is_some() {
            bail!("firewall forwarding namespace is already occupied: {section}");
        }
        let record = ForwardingRecord {
            src: src.into(),
            dest: dest.into(),
            section,
            nonce: atomic::random_nonce(),
            phase: ManagedPhase::Creating,
        };
        let key = forwarding_key(src, dest);
        state.forwardings.insert(key.clone(), record.clone());
        state.save(&self.paths)?;
        self.commit_forwarding_create(state, &key, &record)
    }

    fn ensure_forwarding(&self, state: &mut FirewallState, key: &str) -> Result<()> {
        let record = state
            .forwardings
            .get(key)
            .cloned()
            .context("firewall forwarding record disappeared")?;
        let live = self.capture_forwarding(&record.section)?;
        let exact = live
            .as_ref()
            .is_some_and(|live| forwarding_live_exact(live, &record));
        match record.phase {
            ManagedPhase::Creating if exact => {
                state.forwardings.get_mut(key).expect("record exists").phase = ManagedPhase::Owned;
                state.reload_pending = true;
                state.save(&self.paths)
            }
            ManagedPhase::Creating if live.is_none() => {
                if self.forwarding_exists(&record.src, &record.dest)? {
                    state.forwardings.remove(key);
                    state.save(&self.paths)
                } else {
                    self.commit_forwarding_create(state, key, &record)
                }
            }
            ManagedPhase::Owned if exact => Ok(()),
            ManagedPhase::Owned if live.is_none() => {
                if self.forwarding_exists(&record.src, &record.dest)? {
                    state.forwardings.remove(key);
                    return state.save(&self.paths);
                }
                let replacement = ForwardingRecord {
                    nonce: atomic::random_nonce(),
                    phase: ManagedPhase::Creating,
                    ..record
                };
                state.forwardings.insert(key.into(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_forwarding_create(state, key, &replacement)
            }
            ManagedPhase::Deleting if exact => {
                state.forwardings.get_mut(key).expect("record exists").phase = ManagedPhase::Owned;
                state.save(&self.paths)
            }
            ManagedPhase::Deleting if live.is_none() => {
                if self.forwarding_exists(&record.src, &record.dest)? {
                    state.forwardings.remove(key);
                    return state.save(&self.paths);
                }
                let replacement = ForwardingRecord {
                    nonce: atomic::random_nonce(),
                    phase: ManagedPhase::Creating,
                    ..record
                };
                state.forwardings.insert(key.into(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_forwarding_create(state, key, &replacement)
            }
            _ => bail!("managed firewall forwarding conflicts with live UCI: {key}"),
        }
    }

    fn commit_forwarding_create(
        &self,
        state: &mut FirewallState,
        key: &str,
        record: &ForwardingRecord,
    ) -> Result<()> {
        state.reload_pending = true;
        state.save(&self.paths)?;
        self.ensure_default_delta_clean()?;
        let savedir = self.reset_savedir()?;
        let result = (|| {
            if self.capture_forwarding(&record.section)?.is_some()
                || self.forwarding_exists(&record.src, &record.dest)?
            {
                bail!("firewall forwarding appeared before creation: {key}");
            }
            let prefix = format!("firewall.{}", record.section);
            for expression in [
                format!("{prefix}=forwarding"),
                format!("{prefix}.src={}", record.src),
                format!("{prefix}.dest={}", record.dest),
                format!("{prefix}.meduza_owner={OWNER}"),
                format!("{prefix}.meduza_nonce={}", record.nonce),
            ] {
                self.uci_private(&savedir, "set", &expression)?;
            }
            self.ensure_default_delta_clean()?;
            if self.capture_forwarding(&record.section)?.is_some()
                || self.forwarding_exists(&record.src, &record.dest)?
            {
                bail!("firewall forwarding changed before commit: {key}");
            }
            self.uci_private(&savedir, "commit", "firewall")
        })();
        finish_private_session(result, &savedir)?;
        let live = self
            .capture_forwarding(&record.section)?
            .context("firewall forwarding disappeared after commit")?;
        if !forwarding_live_exact(&live, record) {
            bail!("firewall forwarding creation did not commit: {key}");
        }
        state.forwardings.get_mut(key).expect("record exists").phase = ManagedPhase::Owned;
        state.save(&self.paths)
    }

    fn remove_forwarding(&self, state: &mut FirewallState, key: &str) -> Result<()> {
        let record = state
            .forwardings
            .get(key)
            .cloned()
            .context("firewall forwarding record disappeared")?;
        let live = self.capture_forwarding(&record.section)?;
        if live.is_none() {
            state.forwardings.remove(key);
            if record.phase == ManagedPhase::Deleting {
                state.reload_pending = true;
            }
            return state.save(&self.paths);
        }
        if !live
            .as_ref()
            .is_some_and(|live| forwarding_live_exact(live, &record))
        {
            bail!("refusing to remove changed firewall forwarding: {key}");
        }
        state.forwardings.get_mut(key).expect("record exists").phase = ManagedPhase::Deleting;
        state.reload_pending = true;
        state.save(&self.paths)?;
        self.ensure_default_delta_clean()?;
        let savedir = self.reset_savedir()?;
        let result = (|| {
            let current = self
                .capture_forwarding(&record.section)?
                .context("firewall forwarding disappeared before deletion")?;
            if !forwarding_live_exact(&current, &record) {
                bail!("firewall forwarding changed before deletion: {key}");
            }
            self.uci_private(&savedir, "delete", &format!("firewall.{}", record.section))?;
            self.ensure_default_delta_clean()?;
            let fresh = self
                .capture_forwarding(&record.section)?
                .context("firewall forwarding disappeared before commit")?;
            if fresh != current {
                bail!("firewall forwarding changed before commit: {key}");
            }
            self.uci_private(&savedir, "commit", "firewall")
        })();
        finish_private_session(result, &savedir)?;
        if self.capture_forwarding(&record.section)?.is_some() {
            bail!("firewall forwarding deletion did not commit: {key}");
        }
        state.forwardings.remove(key);
        state.save(&self.paths)
    }

    fn ensure_record(&self, state: &mut FirewallState, key: &str) -> Result<()> {
        let record = state
            .records
            .get(key)
            .cloned()
            .context("firewall ownership record disappeared")?;
        let Some(live) = self.capture_record(&record)? else {
            bail!("configured firewall zone disappeared: {}", record.zone);
        };
        let exact_tag = live.tag.as_deref() == Some(owned_tag(&record).as_str());
        match record.phase {
            DevicePhase::Borrowed if live.member && live.tag.is_none() => Ok(()),
            DevicePhase::Borrowed if !live.member && live.tag.is_none() => {
                let replacement = new_record(&record.zone, &record.device, record.membership);
                state.records.insert(key.to_owned(), replacement.clone());
                state.save(&self.paths)?;
                self.commit_add(state, key, &replacement, &live)
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
                let replacement = new_record(&record.zone, &record.device, record.membership);
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
                let replacement = new_record(&record.zone, &record.device, record.membership);
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

    fn create_record(
        &self,
        state: &mut FirewallState,
        zone: &str,
        entry: &ManifestEntry,
    ) -> Result<()> {
        validate_logical_name(&entry.logical)?;
        let member = &entry.logical;
        let membership = MembershipKind::Network;
        let tag_option = tag_option(zone, member, membership);
        let live = self
            .capture(zone, member, &tag_option, membership)?
            .with_context(|| format!("configured firewall zone disappeared: {zone}"))?;
        let key = record_key(zone, member, membership);
        if live.tag.is_some() {
            bail!("firewall membership tag is already occupied for {zone}/{member}");
        }
        if live.member {
            state.records.insert(
                key,
                DeviceRecord {
                    zone: zone.into(),
                    device: member.into(),
                    membership,
                    nonce: atomic::random_nonce(),
                    tag_option,
                    phase: DevicePhase::Borrowed,
                },
            );
            return state.save(&self.paths);
        }
        let record = new_record(zone, member, membership);
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
        let Some(live) = self.capture_record(&record)? else {
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
            .capture_record(record)?
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
        let after = self.capture_record(record)?;
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
                .capture_record(record)?
                .context("firewall zone disappeared before mutation")?;
            if &current != before {
                bail!("firewall membership changed before mutation");
            }
            let prefix = format!("firewall.{}", current.section);
            let option = membership_option(record.membership);
            if add {
                if !current.member {
                    self.uci_private(
                        &savedir,
                        "add_list",
                        &format!("{prefix}.{option}={}", record.device),
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
                        &format!("{prefix}.{option}={}", record.device),
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
                .capture_record(record)?
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
        let mut exhausted = true;
        for index in 0..MAX_ZONE_COUNT {
            let section = format!("@zone[{index}]");
            let Some(kind) = self.uci_get(&format!("firewall.{section}"))? else {
                exhausted = false;
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
        if exhausted {
            bail!("firewall has too many zones to inspect safely");
        }
        Ok(found)
    }

    fn capture_record(&self, record: &DeviceRecord) -> Result<Option<ZoneLive>> {
        self.capture(
            &record.zone,
            &record.device,
            &record.tag_option,
            record.membership,
        )
    }

    fn capture(
        &self,
        zone: &str,
        member: &str,
        tag_option: &str,
        membership: MembershipKind,
    ) -> Result<Option<ZoneLive>> {
        let Some(section) = self.resolve_zone(zone)? else {
            return Ok(None);
        };
        let prefix = format!("firewall.{section}");
        let members = self
            .uci_get(&format!("{prefix}.{}", membership_option(membership)))?
            .unwrap_or_default();
        Ok(Some(ZoneLive {
            section,
            member: members.split_whitespace().any(|value| value == member),
            tag: self.uci_get(&format!("{prefix}.{tag_option}"))?,
        }))
    }

    fn capture_managed_zone(&self) -> Result<Option<ManagedZoneLive>> {
        let prefix = "firewall.meduza";
        let Some(section_type) = self.uci_get(prefix)? else {
            return Ok(None);
        };
        Ok(Some(ManagedZoneLive {
            section_type,
            name: self.uci_get(&format!("{prefix}.name"))?,
            input: self.uci_get(&format!("{prefix}.input"))?,
            output: self.uci_get(&format!("{prefix}.output"))?,
            forward: self.uci_get(&format!("{prefix}.forward"))?,
            owner: self.uci_get(&format!("{prefix}.meduza_owner"))?,
            nonce: self.uci_get(&format!("{prefix}.meduza_nonce"))?,
        }))
    }

    fn capture_forwarding(&self, section: &str) -> Result<Option<ForwardingLive>> {
        let prefix = format!("firewall.{section}");
        let Some(section_type) = self.uci_get(&prefix)? else {
            return Ok(None);
        };
        Ok(Some(ForwardingLive {
            section_type,
            src: self.uci_get(&format!("{prefix}.src"))?,
            dest: self.uci_get(&format!("{prefix}.dest"))?,
            owner: self.uci_get(&format!("{prefix}.meduza_owner"))?,
            nonce: self.uci_get(&format!("{prefix}.meduza_nonce"))?,
            fields: self.uci_section_fields(section)?,
        }))
    }

    fn uci_section_fields(&self, section: &str) -> Result<BTreeMap<String, String>> {
        let expression = format!("firewall.{section}");
        let output = self
            .runner
            .output("uci", ["-q", "show", expression.as_str()])?;
        if !output.status.success() {
            bail!("could not inspect firewall UCI section {section}");
        }
        if output.stdout.len() > MAX_UCI_SECTION_BYTES {
            bail!("firewall UCI section is too large: {section}");
        }
        let text = String::from_utf8(output.stdout)
            .context("firewall UCI section output was not UTF-8")?;
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .context("invalid firewall UCI section output")?;
            let field = if key == expression {
                "_type"
            } else {
                key.strip_prefix(&format!("{expression}."))
                    .context("firewall UCI section output changed identity")?
            };
            if fields
                .insert(field.into(), normalize_uci_show_value(value))
                .is_some()
            {
                bail!("duplicate firewall UCI field: {section}.{field}");
            }
        }
        Ok(fields)
    }

    fn forwarding_exists(&self, expected_src: &str, expected_dest: &str) -> Result<bool> {
        let mut exhausted = true;
        for index in 0..MAX_FORWARDING_COUNT {
            let section = format!("@forwarding[{index}]");
            let Some(kind) = self.uci_get(&format!("firewall.{section}"))? else {
                exhausted = false;
                break;
            };
            if kind != "forwarding" {
                continue;
            }
            let src = self.uci_get(&format!("firewall.{section}.src"))?;
            let dest = self.uci_get(&format!("firewall.{section}.dest"))?;
            let enabled = self.uci_get(&format!("firewall.{section}.enabled"))?;
            let disabled = enabled.as_deref().is_some_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            });
            if !disabled
                && src.as_deref() == Some(expected_src)
                && dest.as_deref() == Some(expected_dest)
            {
                return Ok(true);
            }
        }
        if exhausted {
            bail!("firewall has too many forwardings to inspect safely");
        }
        Ok(false)
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
            // `uci -P` also sets CLI_FLAG_NOCOMMIT, so `uci -P ... commit`
            // returns success without writing /etc/config. `-t` selects an
            // isolated delta save directory while retaining real commit
            // semantics.
            .status("uci", ["-q", "-t", savedir.as_str(), operation, expression])
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
            match record.membership {
                MembershipKind::Device => validate_device(&record.device)?,
                MembershipKind::Network => validate_logical_name(&record.device)?,
            }
            validate_nonce(&record.nonce)?;
            if key != &record_key(&record.zone, &record.device, record.membership)
                || record.tag_option != tag_option(&record.zone, &record.device, record.membership)
            {
                bail!("firewall ownership identity changed");
            }
        }
        if let Some(zone) = &self.zone {
            validate_nonce(&zone.nonce)?;
        }
        for (key, record) in &self.forwardings {
            validate_firewall_zone(&record.src)?;
            validate_firewall_zone(&record.dest)?;
            validate_nonce(&record.nonce)?;
            if record.src == record.dest
                || key != &forwarding_key(&record.src, &record.dest)
                || record.section != forwarding_section(&record.src, &record.dest)
            {
                bail!("firewall forwarding ownership identity changed");
            }
        }
        Ok(())
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        if self.records.is_empty()
            && self.zone.is_none()
            && self.forwardings.is_empty()
            && !self.reload_pending
        {
            atomic::durable_remove(&paths.firewall_state)?;
            return Ok(());
        }
        atomic::atomic_json_bounded(&paths.firewall_state, self, MAX_FIREWALL_STATE_BYTES)?;
        Ok(())
    }
}

fn zone_live_owned(live: &ManagedZoneLive, record: &ManagedZoneRecord) -> bool {
    live.section_type == "zone"
        && live.name.as_deref() == Some(MEDUZA_ZONE)
        && live.owner.as_deref() == Some(OWNER)
        && live.nonce.as_deref() == Some(record.nonce.as_str())
}

fn zone_live_created(live: &ManagedZoneLive, record: &ManagedZoneRecord) -> bool {
    zone_live_owned(live, record)
        && live.input.as_deref() == Some("ACCEPT")
        && live.output.as_deref() == Some("ACCEPT")
        && live.forward.as_deref() == Some("ACCEPT")
}

fn zone_live_released(live: &ManagedZoneLive) -> bool {
    live.section_type == "zone"
        && live.name.as_deref() == Some(MEDUZA_ZONE)
        && live.owner.is_none()
        && live.nonce.is_none()
}

fn forwarding_live_exact(live: &ForwardingLive, record: &ForwardingRecord) -> bool {
    live.section_type == "forwarding"
        && live.src.as_deref() == Some(record.src.as_str())
        && live.dest.as_deref() == Some(record.dest.as_str())
        && live.owner.as_deref() == Some(OWNER)
        && live.nonce.as_deref() == Some(record.nonce.as_str())
        && live.fields == forwarding_expected_fields(record)
}

fn forwarding_expected_fields(record: &ForwardingRecord) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("_type".into(), "forwarding".into()),
        ("src".into(), record.src.clone()),
        ("dest".into(), record.dest.clone()),
        ("meduza_owner".into(), OWNER.into()),
        ("meduza_nonce".into(), record.nonce.clone()),
    ])
}

fn normalize_uci_show_value(value: &str) -> String {
    value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .filter(|value| {
            value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-')
            })
        })
        .unwrap_or(value)
        .to_owned()
}

fn forwarding_key(src: &str, dest: &str) -> String {
    format!("{src}\0{dest}")
}

fn split_forwarding_key(value: &str) -> Result<(&str, &str)> {
    value
        .split_once('\0')
        .context("invalid firewall forwarding key")
}

fn forwarding_section(src: &str, dest: &str) -> String {
    let hash = hex::encode(Sha256::digest(
        format!("forwarding\0{src}\0{dest}").as_bytes(),
    ));
    format!("meduza_fwd_{}", &hash[..16])
}

fn new_record(zone: &str, member: &str, membership: MembershipKind) -> DeviceRecord {
    DeviceRecord {
        zone: zone.into(),
        device: member.into(),
        membership,
        nonce: atomic::random_nonce(),
        tag_option: tag_option(zone, member, membership),
        phase: DevicePhase::Creating,
    }
}

fn record_key(zone: &str, member: &str, membership: MembershipKind) -> String {
    match membership {
        // Preserve the exact v1 key so existing ownership records validate
        // and can be retired without adopting anything new.
        MembershipKind::Device => format!("{zone}\0{member}"),
        MembershipKind::Network => format!("network\0{zone}\0{member}"),
    }
}

fn tag_option(zone: &str, member: &str, membership: MembershipKind) -> String {
    let identity = match membership {
        MembershipKind::Device => format!("{zone}\0{member}"),
        MembershipKind::Network => format!("network\0{zone}\0{member}"),
    };
    let hash = hex::encode(Sha256::digest(identity.as_bytes()));
    format!("meduza_vpn_{}", &hash[..16])
}

fn membership_option(membership: MembershipKind) -> &'static str {
    match membership {
        MembershipKind::Device => "device",
        MembershipKind::Network => "network",
    }
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

fn finish_private_session(result: Result<()>, savedir: &Path) -> Result<()> {
    let cleanup = remove_session_directory(savedir);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("could not clean private UCI session")),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "private UCI session cleanup also failed: {cleanup:#}"
        ))),
    }
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
        AddList(String, String),
        DelList(String, String),
        Set(String, String),
        Delete(String),
    }

    #[derive(Clone, Debug, Default)]
    struct MockSection {
        kind: String,
        options: BTreeMap<String, String>,
        lists: BTreeMap<String, BTreeSet<String>>,
    }

    #[derive(Debug, Default)]
    struct MockUciState {
        sections: BTreeMap<String, MockSection>,
        pending: Vec<Pending>,
        reloads: usize,
    }

    impl MockUciState {
        fn add_zone(&mut self, section: &str, name: &str) {
            self.sections.insert(
                section.into(),
                MockSection {
                    kind: "zone".into(),
                    options: BTreeMap::from([("name".into(), name.into())]),
                    lists: BTreeMap::new(),
                },
            );
        }

        fn add_forwarding(&mut self, section: &str, src: &str, dest: &str) {
            self.sections.insert(
                section.into(),
                MockSection {
                    kind: "forwarding".into(),
                    options: BTreeMap::from([
                        ("src".into(), src.into()),
                        ("dest".into(), dest.into()),
                    ]),
                    lists: BTreeMap::new(),
                },
            );
        }

        fn section_for_typed_reference(&self, reference: &str) -> Option<&MockSection> {
            let key = self.section_key_for_reference(reference)?;
            self.sections.get(&key)
        }

        fn section_key_for_reference(&self, reference: &str) -> Option<String> {
            if let Some(reference) = reference.strip_prefix('@') {
                let (kind, index) = reference.split_once('[')?;
                let index = index.strip_suffix(']')?.parse::<usize>().ok()?;
                return self
                    .sections
                    .iter()
                    .filter(|(_, section)| section.kind == kind)
                    .nth(index)
                    .map(|(key, _)| key.clone());
            }
            self.sections
                .contains_key(reference)
                .then(|| reference.into())
        }

        fn get(&self, expression: &str) -> Option<String> {
            let path = expression.strip_prefix("firewall.")?;
            let (section, option) = split_uci_path(path);
            let section = self.section_for_typed_reference(section)?;
            match option {
                None => Some(section.kind.clone()),
                Some(option) => section.options.get(option).cloned().or_else(|| {
                    section
                        .lists
                        .get(option)
                        .filter(|values| !values.is_empty())
                        .map(|values| values.iter().cloned().collect::<Vec<_>>().join(" "))
                }),
            }
        }

        fn zone_section(&self, name: &str) -> Option<&MockSection> {
            self.sections.values().find(|section| {
                section.kind == "zone"
                    && section.options.get("name").map(String::as_str) == Some(name)
            })
        }

        fn forwarding_count(&self, src: &str, dest: &str) -> usize {
            self.sections
                .values()
                .filter(|section| {
                    section.kind == "forwarding"
                        && section.options.get("src").map(String::as_str) == Some(src)
                        && section.options.get("dest").map(String::as_str) == Some(dest)
                })
                .count()
        }

        fn apply_pending(&mut self) {
            for pending in std::mem::take(&mut self.pending) {
                match pending {
                    Pending::Set(path, value) => {
                        let path = path.strip_prefix("firewall.").unwrap();
                        let (section, option) = split_uci_path(path);
                        if let Some(option) = option {
                            let section = self
                                .section_key_for_reference(section)
                                .unwrap_or_else(|| section.into());
                            self.sections
                                .entry(section)
                                .or_default()
                                .options
                                .insert(option.into(), value);
                        } else {
                            self.sections.entry(section.into()).or_default().kind = value;
                        }
                    }
                    Pending::AddList(path, value) => {
                        let path = path.strip_prefix("firewall.").unwrap();
                        let (section, option) = split_uci_path(path);
                        let section = self.section_key_for_reference(section).unwrap();
                        self.sections
                            .get_mut(&section)
                            .unwrap()
                            .lists
                            .entry(option.unwrap().into())
                            .or_default()
                            .insert(value);
                    }
                    Pending::DelList(path, value) => {
                        let path = path.strip_prefix("firewall.").unwrap();
                        let (section, option) = split_uci_path(path);
                        let section = self.section_key_for_reference(section).unwrap();
                        if let Some(values) = self
                            .sections
                            .get_mut(&section)
                            .and_then(|section| section.lists.get_mut(option.unwrap()))
                        {
                            values.remove(&value);
                        }
                    }
                    Pending::Delete(path) => {
                        let path = path.strip_prefix("firewall.").unwrap();
                        let (section, option) = split_uci_path(path);
                        let resolved = self
                            .section_key_for_reference(section)
                            .unwrap_or_else(|| section.into());
                        if let Some(option) = option {
                            if let Some(section) = self.sections.get_mut(&resolved) {
                                section.options.remove(option);
                                section.lists.remove(option);
                            }
                        } else {
                            self.sections.remove(&resolved);
                        }
                    }
                }
            }
        }
    }

    fn split_uci_path(path: &str) -> (&str, Option<&str>) {
        if path.starts_with('@') {
            let closing = path.find(']').expect("typed UCI reference");
            let section = &path[..=closing];
            let option = path.get(closing + 2..).filter(|value| !value.is_empty());
            (section, option)
        } else {
            path.split_once('.')
                .map_or((path, None), |(section, option)| (section, Some(option)))
        }
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
                && args.get(1).map(String::as_str) == Some("show")
            {
                let expression = &args[2];
                let section = expression.strip_prefix("firewall.").unwrap();
                let Some(live) = state.sections.get(section) else {
                    return Ok(output(1, ""));
                };
                let mut text = format!("{expression}={}\n", live.kind);
                for (option, value) in &live.options {
                    text.push_str(&format!("{expression}.{option}='{value}'\n"));
                }
                for (option, values) in &live.lists {
                    for value in values {
                        text.push_str(&format!("{expression}.{option}='{value}'\n"));
                    }
                }
                return Ok(output(0, &text));
            }
            if args.first().map(String::as_str) == Some("-q")
                && args.get(1).map(String::as_str) == Some("get")
            {
                return Ok(match state.get(&args[2]) {
                    Some(value) => output(0, &format!("{value}\n")),
                    None => output(1, ""),
                });
            }
            assert_eq!(args.len(), 5);
            assert_eq!(args[0], "-q");
            assert_eq!(args[1], "-t");
            let operation = args[3].as_str();
            let expression = args[4].as_str();
            match operation {
                "add_list" => {
                    let (path, value) = expression.split_once('=').unwrap();
                    state
                        .pending
                        .push(Pending::AddList(path.into(), value.into()));
                }
                "del_list" => {
                    let (path, value) = expression.split_once('=').unwrap();
                    state
                        .pending
                        .push(Pending::DelList(path.into(), value.into()));
                }
                "set" => {
                    let (path, value) = expression.split_once('=').unwrap();
                    state.pending.push(Pending::Set(path.into(), value.into()));
                }
                "delete" => state.pending.push(Pending::Delete(expression.into())),
                "commit" => {
                    assert_eq!(expression, "firewall");
                    state.apply_pending();
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
        let record = new_record("vpn-zone", "wg_office", MembershipKind::Network);
        let mut state = FirewallState {
            version: 1,
            records: BTreeMap::from([(
                record_key(&record.zone, &record.device, record.membership),
                record.clone(),
            )]),
            reload_pending: true,
            ..FirewallState::default()
        };
        state.validate().unwrap();
        state.records.values_mut().next().unwrap().device = "other".into();
        assert!(state.validate().is_err());
    }

    #[test]
    fn tag_is_scoped_to_zone_and_device() {
        assert_eq!(
            tag_option("vpn", "wg0", MembershipKind::Network),
            tag_option("vpn", "wg0", MembershipKind::Network)
        );
        assert_ne!(
            tag_option("vpn", "wg0", MembershipKind::Network),
            tag_option("lan", "wg0", MembershipKind::Network)
        );
        assert_ne!(
            tag_option("vpn", "wg0", MembershipKind::Network),
            tag_option("vpn", "wg1", MembershipKind::Network)
        );
    }

    #[test]
    fn owned_membership_is_added_reloaded_and_removed() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        runner.0.lock().unwrap().add_zone("lan", "lan");
        let firewall = Firewall::new(paths.clone(), runner.clone());

        firewall.sync(Some("lan"), &[entry(&paths)]).unwrap();
        {
            let state = runner.0.lock().unwrap();
            let meduza = state.zone_section(MEDUZA_ZONE).unwrap();
            assert!(meduza.lists["network"].contains("wg_office"));
            assert!(
                !meduza
                    .lists
                    .get("device")
                    .is_some_and(|values| values.contains("wg-office"))
            );
            assert_eq!(
                meduza
                    .options
                    .keys()
                    .filter(|key| key.starts_with("meduza_vpn_"))
                    .count(),
                1
            );
            assert_eq!(state.forwarding_count(MEDUZA_ZONE, "lan"), 1);
            assert_eq!(state.forwarding_count("lan", MEDUZA_ZONE), 1);
            assert_eq!(state.reloads, 1);
        }
        assert!(paths.firewall_state.is_file());

        firewall.sync(Some("lan"), &[entry(&paths)]).unwrap();
        assert_eq!(runner.0.lock().unwrap().reloads, 1);

        firewall.sync(None, &[]).unwrap();
        let state = runner.0.lock().unwrap();
        let meduza = state.zone_section(MEDUZA_ZONE).unwrap();
        assert!(
            !meduza
                .lists
                .get("network")
                .is_some_and(|values| values.contains("wg_office"))
        );
        assert!(!meduza.options.contains_key("meduza_owner"));
        assert!(!meduza.options.contains_key("meduza_nonce"));
        assert_eq!(meduza.options["input"], "ACCEPT");
        assert_eq!(state.forwarding_count(MEDUZA_ZONE, "lan"), 0);
        assert_eq!(state.forwarding_count("lan", MEDUZA_ZONE), 0);
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
            state.add_zone("lan", "lan");
            state.add_zone("existing_meduza", MEDUZA_ZONE);
            let meduza = state.sections.get_mut("existing_meduza").unwrap();
            meduza.options.insert("input".into(), "REJECT".into());
            meduza
                .lists
                .entry("network".into())
                .or_default()
                .insert("wg_office".into());
        }
        let firewall = Firewall::new(paths.clone(), runner.clone());

        firewall.sync(Some("lan"), &[entry(&paths)]).unwrap();
        firewall.sync(None, &[]).unwrap();

        let state = runner.0.lock().unwrap();
        let meduza = state.zone_section(MEDUZA_ZONE).unwrap();
        assert!(meduza.lists["network"].contains("wg_office"));
        assert_eq!(meduza.options["input"], "REJECT");
        assert_eq!(state.forwarding_count(MEDUZA_ZONE, "lan"), 0);
        assert_eq!(state.forwarding_count("lan", MEDUZA_ZONE), 0);
        assert_eq!(state.reloads, 2);
        assert!(!paths.firewall_state.exists());
    }

    #[test]
    fn existing_meduza_zone_and_forwardings_are_reused_without_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        {
            let mut state = runner.0.lock().unwrap();
            state.add_zone("lan", "lan");
            state.add_zone("user_meduza", MEDUZA_ZONE);
            state.add_forwarding("user_out", MEDUZA_ZONE, "lan");
            state.add_forwarding("user_in", "lan", MEDUZA_ZONE);
            state
                .sections
                .get_mut("user_meduza")
                .unwrap()
                .lists
                .entry("network".into())
                .or_default()
                .insert("wg_office".into());
        }
        let firewall = Firewall::new(paths.clone(), runner.clone());

        firewall.sync(Some("lan"), &[entry(&paths)]).unwrap();
        firewall.sync(None, &[]).unwrap();

        let state = runner.0.lock().unwrap();
        assert!(state.zone_section(MEDUZA_ZONE).is_some());
        assert_eq!(state.forwarding_count(MEDUZA_ZONE, "lan"), 1);
        assert_eq!(state.forwarding_count("lan", MEDUZA_ZONE), 1);
        assert_eq!(state.reloads, 0);
        assert!(!paths.firewall_state.exists());
    }

    #[test]
    fn externally_modified_owned_forwarding_is_never_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        runner.0.lock().unwrap().add_zone("lan", "lan");
        let firewall = Firewall::new(paths.clone(), runner.clone());
        firewall.sync(Some("lan"), &[entry(&paths)]).unwrap();

        let key = forwarding_key(MEDUZA_ZONE, "lan");
        let record = FirewallState::load(&paths).unwrap().forwardings[&key].clone();
        runner
            .0
            .lock()
            .unwrap()
            .sections
            .get_mut(&record.section)
            .unwrap()
            .options
            .insert("enabled".into(), "0".into());

        let mut state = FirewallState::load(&paths).unwrap();
        assert!(firewall.remove_forwarding(&mut state, &key).is_err());
        assert!(
            runner
                .0
                .lock()
                .unwrap()
                .sections
                .contains_key(&record.section)
        );
        assert!(paths.firewall_state.exists());
    }

    #[test]
    fn legacy_owned_device_membership_migrates_to_logical_network() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        paths.prepare().unwrap();
        let runner = MockRunner::default();
        let legacy = new_record("vpn", "wg-office", MembershipKind::Device);
        {
            let mut live = runner.0.lock().unwrap();
            live.add_zone("lan", "lan");
            live.add_zone("vpn", "vpn");
            let vpn = live.sections.get_mut("vpn").unwrap();
            vpn.lists
                .entry("device".into())
                .or_default()
                .insert("wg-office".into());
            vpn.options
                .insert(legacy.tag_option.clone(), owned_tag(&legacy));
        }
        FirewallState {
            version: 1,
            records: BTreeMap::from([(
                record_key(&legacy.zone, &legacy.device, legacy.membership),
                legacy,
            )]),
            reload_pending: false,
            ..FirewallState::default()
        }
        .save(&paths)
        .unwrap();
        let firewall = Firewall::new(paths.clone(), runner.clone());

        firewall.sync(Some("lan"), &[entry(&paths)]).unwrap();

        let live = runner.0.lock().unwrap();
        assert!(
            !live
                .zone_section("vpn")
                .unwrap()
                .lists
                .get("device")
                .is_some_and(|values| values.contains("wg-office"))
        );
        let meduza = live.zone_section(MEDUZA_ZONE).unwrap();
        assert!(meduza.lists["network"].contains("wg_office"));
        assert_eq!(live.forwarding_count(MEDUZA_ZONE, "lan"), 1);
        assert_eq!(live.forwarding_count("lan", MEDUZA_ZONE), 1);
    }
}

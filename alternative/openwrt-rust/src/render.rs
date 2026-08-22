//! Pure renderers for native OpenWrt VPN and FRR configuration.
//!
//! Rendering is deliberately side-effect free.  The reconciliation layer can
//! validate the complete plan, journal ownership, and only then persist these
//! files atomically.  Secrets and executable helper scripts carry their final
//! modes in [`RenderedFile`].

use crate::model::{
    DesiredInterface, DesiredState, FlatSnapshot, VpnKind, parse_enabled, validate_device_name,
    validate_instance_name, validate_node_id,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

pub const DEFAULT_OWNER: &str = crate::OWNER;
pub const DEFAULT_FRR_PATH: &str = "/var/run/meduza/generated/frr/frr.conf";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedFile {
    pub path: PathBuf,
    pub mode: u32,
    pub contents: Vec<u8>,
}

impl RenderedFile {
    pub fn text(&self) -> Result<&str> {
        std::str::from_utf8(&self.contents)
            .with_context(|| format!("rendered file is not UTF-8: {:?}", self.path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOptions {
    pub owner: String,
    pub frr_path: PathBuf,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            owner: DEFAULT_OWNER.to_owned(),
            frr_path: PathBuf::from(DEFAULT_FRR_PATH),
        }
    }
}

pub fn render_all(snapshot: &FlatSnapshot, desired: &DesiredState) -> Result<Vec<RenderedFile>> {
    render_all_with_options(snapshot, desired, &RenderOptions::default())
}

pub fn render_all_with_options(
    snapshot: &FlatSnapshot,
    desired: &DesiredState,
    options: &RenderOptions,
) -> Result<Vec<RenderedFile>> {
    snapshot.validate()?;
    desired.validate()?;
    if snapshot.node_id != desired.node_id {
        bail!(
            "snapshot node {} does not match desired node {}",
            snapshot.node_id,
            desired.node_id
        );
    }
    validate_owner(&options.owner)?;
    validate_absolute_file_path(&options.frr_path)?;

    let mut output = BTreeMap::<PathBuf, RenderedFile>::new();
    for interface in &desired.interfaces {
        let rendered = match interface.kind {
            VpnKind::Tinc => render_tinc(snapshot, interface, &options.owner)?,
            VpnKind::OpenVpn => render_openvpn(snapshot, interface, options)?,
            VpnKind::WireGuard => render_wireguard(snapshot, interface)?,
        };
        insert_files(&mut output, rendered)?;
    }
    insert_files(&mut output, vec![render_frr(snapshot, desired, options)?])?;
    Ok(output.into_values().collect())
}

fn insert_files(
    output: &mut BTreeMap<PathBuf, RenderedFile>,
    files: Vec<RenderedFile>,
) -> Result<()> {
    for file in files {
        validate_absolute_file_path(&file.path)?;
        if !matches!(file.mode, 0o600 | 0o640 | 0o644 | 0o755) {
            bail!(
                "unsupported rendered file mode {:o}: {:?}",
                file.mode,
                file.path
            );
        }
        if output.insert(file.path.clone(), file).is_some() {
            bail!("multiple renderers produced the same path");
        }
    }
    Ok(())
}

fn rendered_text(path: PathBuf, mode: u32, mut contents: String) -> RenderedFile {
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    RenderedFile {
        path,
        mode,
        contents: contents.into_bytes(),
    }
}

fn rendered_inline(path: PathBuf, mode: u32, value: &str) -> RenderedFile {
    let mut contents = value.as_bytes().to_vec();
    if !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    RenderedFile {
        path,
        mode,
        contents,
    }
}

fn render_tinc(
    snapshot: &FlatSnapshot,
    interface: &DesiredInterface,
    owner: &str,
) -> Result<Vec<RenderedFile>> {
    if interface.kind != VpnKind::Tinc {
        bail!("tinc renderer received a non-tinc interface");
    }
    validate_device_name(&interface.device)?;
    let directory = parent(&interface.config)?;
    let hosts_directory = directory.join("hosts");
    let mut files = Vec::new();
    let mut host_names = BTreeSet::new();
    let mut rendered_nodes = BTreeMap::<String, String>::new();

    let local_name = tinc_host_name(snapshot, &snapshot.node_id)?;
    let local = render_tinc_host(snapshot, &snapshot.node_id, true, &local_name)?
        .context("local tinc host must always be rendered")?;
    host_names.insert(local_name.clone());
    rendered_nodes.insert(snapshot.node_id.clone(), local_name.clone());
    files.push(rendered_text(
        hosts_directory.join(&local_name),
        0o644,
        local,
    ));

    for node_id in snapshot.all_node_ids() {
        if node_id == snapshot.node_id {
            continue;
        }
        validate_node_id(&node_id)
            .with_context(|| format!("invalid remote tinc node {node_id}"))?;
        if !snapshot.all_node_enabled(&node_id, "tinc/enable") {
            continue;
        }
        let name = tinc_host_name(snapshot, &node_id)?;
        let Some(host) = render_tinc_host(snapshot, &node_id, false, &name)? else {
            continue;
        };
        if !host_names.insert(name.clone()) {
            bail!("duplicate tinc host name after normalization: {name}");
        }
        rendered_nodes.insert(node_id, name.clone());
        files.push(rendered_text(hosts_directory.join(name), 0o644, host));
    }

    let port = parse_port(
        local_or_all_value(snapshot, &snapshot.node_id, "tinc/port").unwrap_or("655"),
        "tinc port",
    )?;
    let mut config = String::new();
    writeln!(config, "Name={local_name}")?;
    writeln!(config, "AddressFamily=ipv4")?;
    writeln!(config, "Mode=switch")?;
    writeln!(config, "DeviceType=tap")?;
    writeln!(config, "Interface={}", interface.device)?;
    writeln!(config, "Port={port}")?;
    writeln!(config, "TCPOnly=yes")?;
    for (node_id, name) in &rendered_nodes {
        if node_id == &snapshot.node_id {
            continue;
        }
        if local_or_all_value(snapshot, node_id, "tinc/address").is_some_and(|v| !v.is_empty()) {
            writeln!(config, "ConnectTo = {name}")?;
        }
    }
    files.push(rendered_text(interface.config.clone(), 0o644, config));

    if let Some(value) = snapshot
        .node_value("tinc/private_key")
        .filter(|v| !v.is_empty())
    {
        validate_inline_material(value, "tinc RSA private key")?;
        files.push(rendered_inline(
            directory.join("rsa_key.priv"),
            0o600,
            value,
        ));
    }
    if let Some(value) = snapshot
        .node_value("tinc/ed25519_private_key")
        .filter(|v| !v.is_empty())
    {
        validate_inline_material(value, "tinc Ed25519 private key")?;
        files.push(rendered_inline(
            directory.join("ed25519_key.priv"),
            0o600,
            value,
        ));
    }

    let alias = managed_alias(owner, VpnKind::Tinc, &interface.instance)?;
    let mut up = String::from("#!/bin/sh\n");
    writeln!(
        up,
        "ip link set dev \"$INTERFACE\" alias {} || exit 1",
        shell_quote(&alias)
    )?;
    writeln!(up, "ip link set \"$INTERFACE\" up")?;
    if let Some(address) = snapshot.node_value("tinc/ipv4").filter(|v| !v.is_empty()) {
        let address = validate_cidr(address, "tinc IPv4 address")?;
        if !address.contains('.') {
            bail!("tinc/ipv4 must be an IPv4 CIDR");
        }
        writeln!(up, "ip addr replace {address} dev \"$INTERFACE\"")?;
    }
    files.push(rendered_text(directory.join("tinc-up"), 0o755, up));
    files.push(rendered_text(
        directory.join("tinc-down"),
        0o755,
        "#!/bin/sh\nip link set \"$INTERFACE\" down 2>/dev/null || true\n".into(),
    ));
    Ok(files)
}

fn tinc_host_name(snapshot: &FlatSnapshot, node_id: &str) -> Result<String> {
    validate_node_id(node_id)?;
    let explicit = local_or_all_value(snapshot, node_id, "tinc/name").unwrap_or("");
    // Keep the Host/Name/ConnectTo identity byte-for-byte compatible with the
    // full generator: both an explicit tinc/name and the NODE_ID fallback are
    // normalized by removing every non-alphanumeric character.  Replacing
    // punctuation with '_' (or retaining an explicit '_') creates a different
    // Tinc node identity when Rust and full-controller nodes share a mesh.
    let source = if explicit.is_empty() {
        node_id
    } else {
        explicit
    };
    let value = source
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(char::from)
        .collect::<String>();
    if value.is_empty() || value.len() > 128 {
        bail!("invalid tinc host name for {node_id}");
    }
    Ok(value)
}

fn render_tinc_host(
    snapshot: &FlatSnapshot,
    node_id: &str,
    local: bool,
    _host_name: &str,
) -> Result<Option<String>> {
    let public_key = local_or_all_value(snapshot, node_id, "tinc/public_key").unwrap_or("");
    let ed25519 = local_or_all_value(snapshot, node_id, "tinc/ed25519_public_key").unwrap_or("");
    if !local && public_key.is_empty() && ed25519.is_empty() {
        return Ok(None);
    }
    validate_inline_material(public_key, "tinc public key")?;
    validate_inline_material(ed25519, "tinc Ed25519 public key")?;
    let port = parse_port(
        local_or_all_value(snapshot, node_id, "tinc/port").unwrap_or("655"),
        "tinc peer port",
    )?;
    let mut output = String::new();
    if let Some(address) =
        local_or_all_value(snapshot, node_id, "tinc/address").filter(|value| !value.is_empty())
    {
        writeln!(
            output,
            "Address={}",
            validate_single_line(address, "tinc address")?
        )?;
    }
    writeln!(output, "Port={port}")?;
    let subnet = local_or_all_value(snapshot, node_id, "tinc/subnet")
        .filter(|value| !value.is_empty())
        .or_else(|| local_or_all_value(snapshot, node_id, "tinc/ipv4"));
    if let Some(subnet) = subnet {
        for value in nonblank_lines(subnet) {
            writeln!(output, "Subnet={}", validate_cidr(&value, "tinc subnet")?)?;
        }
    }
    if !public_key.is_empty() {
        output.push('\n');
        output.push_str(public_key);
        if !public_key.ends_with('\n') {
            output.push('\n');
        }
    }
    if !ed25519.is_empty() {
        let value = validate_single_line(ed25519, "tinc Ed25519 public key")?;
        if value.starts_with("Ed25519PublicKey") {
            writeln!(output, "{value}")?;
        } else {
            writeln!(output, "Ed25519PublicKey = {value}")?;
        }
    }
    Ok(Some(output))
}

fn local_or_all_value<'a>(
    snapshot: &'a FlatSnapshot,
    node_id: &str,
    suffix: &str,
) -> Option<&'a str> {
    if node_id == snapshot.node_id {
        snapshot
            .node_value(suffix)
            .or_else(|| snapshot.all_node_value(node_id, suffix))
    } else {
        snapshot.all_node_value(node_id, suffix)
    }
}

fn render_openvpn(
    snapshot: &FlatSnapshot,
    interface: &DesiredInterface,
    options: &RenderOptions,
) -> Result<Vec<RenderedFile>> {
    if interface.kind != VpnKind::OpenVpn {
        bail!("OpenVPN renderer received another VPN kind");
    }
    let base = format!("openvpn/{}/", interface.instance);
    let directory = parent(&interface.config)?;
    let mut files = Vec::new();
    let secret_names = ["secret", "ca", "cert", "key", "tls_auth", "tls_crypt"];
    let mut present_secrets = BTreeSet::new();
    for name in secret_names {
        let key = format!("{base}{name}");
        let Some(value) = snapshot.node_value(&key).filter(|v| !v.is_empty()) else {
            continue;
        };
        validate_inline_material(value, &format!("OpenVPN {name}"))?;
        present_secrets.insert(name);
        files.push(rendered_inline(
            directory.join(format!("{name}.pem")),
            0o600,
            value,
        ));
    }

    let alias = managed_alias(&options.owner, VpnKind::OpenVpn, &interface.instance)?;
    let mut link_up = String::from("#!/bin/sh\n");
    writeln!(
        link_up,
        "[ \"${{dev:-}}\" = {} ] || exit 1",
        shell_quote(&interface.device)
    )?;
    writeln!(
        link_up,
        "ip link set dev {} alias {}",
        shell_quote(&interface.device),
        shell_quote(&alias)
    )?;
    let link_up_path = directory.join("link-up");
    files.push(rendered_text(link_up_path.clone(), 0o755, link_up));

    let mut config = String::new();
    writeln!(config, "dev {}", interface.device)?;
    let dev_type = snapshot
        .node_value(&format!("{base}dev_type"))
        .filter(|value| !value.is_empty())
        .unwrap_or("tun");
    writeln!(
        config,
        "dev-type {}",
        validate_token(dev_type, "OpenVPN dev_type")?
    )?;
    writeln!(config, "setenv MEDUZA_OWNER {}", options.owner)?;
    writeln!(config, "setenv MEDUZA_INSTANCE {}", interface.instance)?;
    writeln!(config, "script-security 2")?;
    writeln!(config, "up {}", path_text(&link_up_path)?)?;
    if present_secrets.contains("secret") {
        // OpenVPN 2.7 refuses static-key mode unless this compatibility
        // switch is present. OpenVPN 2.3.3 through 2.6 understand
        // ignore-unknown-option, so one configuration remains usable across
        // both supported OpenWrt generations.
        writeln!(
            config,
            "ignore-unknown-option allow-deprecated-insecure-static-crypto"
        )?;
        writeln!(config, "allow-deprecated-insecure-static-crypto")?;
    }

    let scalar_options = [
        ("proto", "proto"),
        ("port", "port"),
        ("ifconfig", "ifconfig"),
        ("keepalive", "keepalive"),
        ("verb", "verb"),
        ("auth", "auth"),
        ("cipher", "cipher"),
        ("comp_lzo", "comp-lzo"),
        ("allow_compression", "allow-compression"),
        ("remote_cert_tls", "remote-cert-tls"),
        ("key_direction", "key-direction"),
    ];
    for (key, directive) in scalar_options {
        let path = format!("{base}{key}");
        let Some(value) = snapshot.node_value(&path).filter(|v| !v.is_empty()) else {
            continue;
        };
        let value = validate_single_line(value, &format!("OpenVPN {key}"))?;
        if key == "port" {
            parse_port(value, "OpenVPN port")?;
        }
        writeln!(config, "{directive} {value}")?;
    }
    if let Some(remotes) = snapshot
        .node_value(&format!("{base}remote"))
        .filter(|value| !value.is_empty())
    {
        for remote in nonblank_lines(remotes) {
            writeln!(
                config,
                "remote {}",
                validate_single_line(&remote, "OpenVPN remote")?
            )?;
        }
    }

    let enabled = |name: &str| {
        snapshot
            .node_value(&format!("{base}{name}"))
            .is_some_and(parse_enabled)
    };
    for (key, directive) in [
        ("client", "client"),
        ("tls_client", "tls-client"),
        ("persist_tun", "persist-tun"),
    ] {
        if enabled(key) {
            writeln!(config, "{directive}")?;
        }
    }
    if enabled("pull") {
        writeln!(config, "pull")?;
    }
    if enabled("client") || enabled("tls_client") || enabled("pull") {
        writeln!(config, "route-nopull")?;
    }
    for name in secret_names {
        if !present_secrets.contains(name) {
            continue;
        }
        let directive = name.replace('_', "-");
        writeln!(
            config,
            "{directive} {}",
            path_text(&directory.join(format!("{name}.pem")))?
        )?;
    }
    files.push(rendered_text(interface.config.clone(), 0o600, config));
    Ok(files)
}

fn render_wireguard(
    snapshot: &FlatSnapshot,
    interface: &DesiredInterface,
) -> Result<Vec<RenderedFile>> {
    if interface.kind != VpnKind::WireGuard {
        bail!("WireGuard renderer received another VPN kind");
    }
    let base = format!("wireguard/{}/", interface.instance);
    let private_key = snapshot
        .node_value(&format!("{base}private_key"))
        .filter(|value| !value.is_empty())
        .context("enabled WireGuard interface is missing private_key")?;
    validate_wireguard_key(private_key, "WireGuard private key")?;
    let mut config = String::from("[Interface]\n");
    writeln!(config, "PrivateKey = {private_key}")?;
    if let Some(port) = snapshot
        .node_value(&format!("{base}listen_port"))
        .filter(|value| !value.is_empty())
    {
        let port = parse_port(port, "WireGuard listen port")?;
        writeln!(config, "ListenPort = {port}")?;
    }

    for peer in snapshot.node_children(&format!("wireguard/{}/peer", interface.instance)) {
        validate_peer_segment(&peer).with_context(|| format!("invalid WireGuard peer {peer}"))?;
        let peer_base = format!("{base}peer/{peer}/");
        let public_key = snapshot
            .node_value(&format!("{peer_base}public_key"))
            .filter(|value| !value.is_empty())
            .with_context(|| format!("WireGuard peer {peer} is missing public_key"))?;
        validate_wireguard_key(public_key, "WireGuard peer public key")?;
        writeln!(config)?;
        writeln!(config, "[Peer]")?;
        writeln!(config, "PublicKey = {public_key}")?;
        if let Some(value) = snapshot
            .node_value(&format!("{peer_base}preshared_key"))
            .filter(|value| !value.is_empty())
        {
            validate_wireguard_key(value, "WireGuard preshared key")?;
            writeln!(config, "PresharedKey = {value}")?;
        }
        let allowed = snapshot
            .node_value(&format!("{peer_base}allowed_ips"))
            .map(nonblank_lines)
            .filter(|items| !items.is_empty())
            .unwrap_or_else(|| vec!["0.0.0.0/0".to_owned()]);
        let allowed = allowed
            .into_iter()
            .map(|value| validate_cidr(&value, "WireGuard AllowedIPs").map(str::to_owned))
            .collect::<Result<Vec<_>>>()?;
        writeln!(config, "AllowedIPs = {}", allowed.join(","))?;
        if let Some(value) = snapshot
            .node_value(&format!("{peer_base}endpoint"))
            .filter(|value| !value.is_empty())
        {
            let endpoint = nonblank_lines(value)
                .into_iter()
                .next()
                .context("WireGuard endpoint is empty")?;
            writeln!(
                config,
                "Endpoint = {}",
                validate_single_line(&endpoint, "WireGuard endpoint")?
            )?;
        }
        if let Some(value) = snapshot
            .node_value(&format!("{peer_base}persistent_keepalive"))
            .filter(|value| !value.is_empty())
        {
            let keepalive: u16 = value
                .parse()
                .with_context(|| format!("invalid WireGuard keepalive: {value:?}"))?;
            writeln!(config, "PersistentKeepalive = {keepalive}")?;
        }
    }

    let mut settings = String::new();
    if let Some(value) = snapshot
        .node_value(&format!("{base}mtu"))
        .filter(|value| !value.is_empty())
    {
        let mtu: u32 = value
            .parse()
            .with_context(|| format!("invalid WireGuard MTU: {value:?}"))?;
        if mtu == 0 || mtu > 65_535 {
            bail!("WireGuard MTU is outside 1..=65535: {mtu}");
        }
        writeln!(settings, "mtu\t{mtu}")?;
    } else {
        writeln!(settings, "mtu\t")?;
    }
    if let Some(addresses) = snapshot
        .node_value(&format!("{base}address"))
        .filter(|value| !value.is_empty())
    {
        for address in nonblank_lines(addresses) {
            writeln!(
                settings,
                "address\t{}",
                validate_cidr(&address, "WireGuard address")?
            )?;
        }
    }
    let directory = parent(&interface.config)?;
    Ok(vec![
        rendered_text(interface.config.clone(), 0o600, config),
        rendered_text(directory.join("settings"), 0o600, settings),
    ])
}

fn render_frr(
    snapshot: &FlatSnapshot,
    desired: &DesiredState,
    options: &RenderOptions,
) -> Result<RenderedFile> {
    let plan = FrrPlan::from_snapshot(snapshot, desired)?;
    let mut lines = vec![
        format!("! meduza-owner: {}", options.owner),
        "frr defaults traditional".into(),
        "service integrated-vtysh-config".into(),
        format!("hostname {}", snapshot.node_id),
    ];
    if let Some(router_id) = plan.router_id {
        lines.push(format!("ip router-id {router_id}"));
    }
    lines.extend([
        String::new(),
        "ip prefix-list PL-DEFAULT seq 10 permit 0.0.0.0/0".into(),
        String::new(),
    ]);

    append_ospf_connected_policy(&mut lines, "LAN", &plan.lans);
    append_ospf_connected_policy(&mut lines, "PRIVATE-LAN", &plan.private_lans);
    append_bgp_filter_policy(&mut lines, "IN", &plan.in_rules);
    append_bgp_filter_policy(&mut lines, "OUT", &plan.out_rules);
    append_private_lan_policy(&mut lines, &plan.private_lans);

    lines.extend([
        "route-map RM-OSPF-TO-BGP deny 20".into(),
        format!(" match tag {TAG_NO_REINJECT}"),
        "!".into(),
        "route-map RM-OSPF-TO-BGP permit 30".into(),
        "!".into(),
        String::new(),
        "route-map RM-BGP-TO-OSPF permit 10".into(),
    ]);
    if plan.to_ospf_default_only {
        lines.push(" match ip address prefix-list PL-DEFAULT".into());
    }
    lines.extend([
        format!(" set tag {TAG_NO_REINJECT}"),
        "!".into(),
        String::new(),
    ]);

    append_bgp_control_policy(&mut lines, &plan.transports);
    if plan.ibgp_neighbors.iter().any(|neighbor| neighbor.roaming) {
        lines.extend([
            "route-map RM-BGP-IN-ROAMING permit 10".into(),
            " match ip address prefix-list PL-BGP-IN".into(),
            " set local-preference 50".into(),
            "route-map RM-BGP-IN-ROAMING permit 20".into(),
            "!".into(),
            String::new(),
        ]);
    }

    if plan.ospf_enabled {
        append_ospf_router(&mut lines, &plan);
    }
    if plan.bgp_enabled {
        append_bgp_router(&mut lines, &plan)?;
    }

    let output = lines.join("\n").trim().to_owned() + "\n";
    Ok(rendered_text(options.frr_path.clone(), 0o640, output))
}

const TAG_NO_REINJECT: u32 = 65_000;
const COMMUNITY_EBGP_LEARNED: u32 = 9_999;

#[derive(Debug, Clone)]
struct PrefixRule {
    action: String,
    rest: String,
}

#[derive(Debug, Clone)]
struct FrrTransport {
    kind: VpnKind,
    name: String,
    peer_ip: Ipv4Addr,
    peer_asn: String,
    update_source: String,
    weight: Option<u32>,
    no_transit: bool,
    no_forward: bool,
}

#[derive(Debug, Clone)]
struct InternalBgpNeighbor {
    name: String,
    router_id: Ipv4Addr,
    roaming: bool,
}

#[derive(Debug)]
struct FrrPlan {
    router_id: Option<Ipv4Addr>,
    internal: String,
    ospf_enabled: bool,
    bgp_enabled: bool,
    local_asn: Option<String>,
    max_paths: u32,
    ospf_area: String,
    active_ifaces: Vec<String>,
    ospf_redistribute_bgp: bool,
    to_ospf_default_only: bool,
    lans: Vec<String>,
    private_lans: Vec<String>,
    access_networks: Vec<String>,
    advertised_networks: Vec<String>,
    edge_broadcast: Vec<String>,
    in_rules: Vec<PrefixRule>,
    out_rules: Vec<PrefixRule>,
    transit_all: bool,
    transit_asns: BTreeSet<String>,
    transports: Vec<FrrTransport>,
    ibgp_neighbors: Vec<InternalBgpNeighbor>,
}

impl FrrPlan {
    fn from_snapshot(snapshot: &FlatSnapshot, desired: &DesiredState) -> Result<Self> {
        let router_id = snapshot
            .node_value("router_id")
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<Ipv4Addr>()
                    .with_context(|| format!("invalid FRR router ID: {value:?}"))
            })
            .transpose()?;
        let internal = snapshot
            .global_value("internal_routing_system")
            .filter(|value| !value.is_empty())
            .unwrap_or("ospf")
            .to_owned();
        if !matches!(internal.as_str(), "ospf" | "bgp") {
            bail!("unsupported internal_routing_system: {internal:?}");
        }

        let configured_ospf = exact_true(snapshot.node_value("ospf/enable"));
        let bgp_enabled = exact_true(snapshot.node_value("bgp/enable"));
        let ospf_enabled = internal != "bgp" && configured_ospf;
        let local_asn = snapshot
            .node_value("bgp/local_asn")
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if bgp_enabled {
            let asn = local_asn
                .as_deref()
                .context("BGP is enabled but bgp/local_asn is not configured")?;
            validate_asn(asn, "local BGP ASN")?;
        }
        let max_paths = parse_positive_u32(
            snapshot.node_value("bgp/max_paths").unwrap_or("1"),
            "BGP maximum paths",
        )?;
        let ospf_area = snapshot
            .node_value("ospf/area")
            .filter(|value| !value.is_empty())
            .unwrap_or("0")
            .to_owned();
        validate_ospf_area(&ospf_area)?;

        let active_ifaces = sorted_tokens(
            snapshot.node_value("ospf/active_ifaces"),
            "OSPF active interface",
        )?;
        let inject_site_lan =
            exact_true_or_default(snapshot.node_value("ospf/inject_site_lan"), true);
        let inject_private_lan =
            exact_true_or_default(snapshot.node_value("ospf/inject_private_lan"), true);
        let lans = if inject_site_lan {
            sorted_ipv4_prefixes(snapshot.node_value("lan"), "LAN prefix")?
        } else {
            Vec::new()
        };
        let private_lans = if inject_private_lan {
            sorted_ipv4_prefixes(snapshot.node_value("private_lan"), "private LAN prefix")?
        } else {
            Vec::new()
        };
        let access_networks = parse_access_networks(snapshot)?;
        let advertised_networks = parse_advertised_network_mappings(snapshot)?;
        let edge_broadcast = sorted_ipv4_prefixes(
            snapshot.global_value("bgp/edge_broadcast"),
            "BGP edge broadcast prefix",
        )?;
        let in_rules = parse_prefix_rules(
            snapshot.global_value("bgp/filter/in"),
            &[("deny", "0.0.0.0/0"), ("permit", "0.0.0.0/0 le 32")],
        )?;
        let out_rules = parse_prefix_rules(
            snapshot.global_value("bgp/filter/out"),
            &[("permit", "0.0.0.0/0 le 32")],
        )?;
        let (transit_all, transit_asns) = parse_transit_asns(snapshot)?;
        let transports = parse_frr_transports(snapshot, desired)?;
        let ibgp_neighbors = if internal == "bgp" && bgp_enabled {
            parse_internal_bgp_neighbors(snapshot)?
        } else {
            Vec::new()
        };
        if !ibgp_neighbors.is_empty() && router_id.is_none() {
            bail!("internal BGP requires /nodes/<NODE_ID>/router_id");
        }

        Ok(Self {
            router_id,
            internal,
            ospf_enabled,
            bgp_enabled,
            local_asn,
            max_paths,
            ospf_area,
            active_ifaces,
            ospf_redistribute_bgp: exact_true_or_default(
                snapshot.node_value("ospf/redistribute_bgp"),
                true,
            ),
            to_ospf_default_only: exact_true(snapshot.node_value("bgp/to_ospf/default_only")),
            lans,
            private_lans,
            access_networks,
            advertised_networks,
            edge_broadcast,
            in_rules,
            out_rules,
            transit_all,
            transit_asns,
            transports,
            ibgp_neighbors,
        })
    }
}

fn exact_true(value: Option<&str>) -> bool {
    value == Some("true")
}

fn exact_true_or_default(value: Option<&str>, default: bool) -> bool {
    value.map_or(default, |value| value == "true")
}

fn parse_positive_u32(value: &str, description: &str) -> Result<u32> {
    let parsed: u32 = validate_single_line(value, description)?
        .parse()
        .with_context(|| format!("invalid {description}: {value:?}"))?;
    if parsed == 0 {
        bail!("{description} cannot be zero");
    }
    Ok(parsed)
}

fn sorted_tokens(value: Option<&str>, description: &str) -> Result<Vec<String>> {
    value
        .map(nonblank_lines)
        .unwrap_or_default()
        .into_iter()
        .map(|value| validate_token(&value, description).map(str::to_owned))
        .collect::<Result<BTreeSet<_>>>()
        .map(BTreeSet::into_iter)
        .map(Iterator::collect)
}

fn sorted_ipv4_prefixes(value: Option<&str>, description: &str) -> Result<Vec<String>> {
    prefix_lines(value, description)?
        .into_iter()
        .map(|prefix| {
            ensure_ipv4_prefix(&prefix, description)?;
            Ok(prefix)
        })
        .collect::<Result<BTreeSet<_>>>()
        .map(BTreeSet::into_iter)
        .map(Iterator::collect)
}

fn parse_access_networks(snapshot: &FlatSnapshot) -> Result<Vec<String>> {
    if !exact_true(snapshot.node_value("access/enable")) {
        return Ok(Vec::new());
    }
    let Some(value) = snapshot
        .node_value("access/network")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    Ok(vec![canonical_ipv4_network(value, "access network")?])
}

fn canonical_ipv4_network(value: &str, description: &str) -> Result<String> {
    let (address, prefix) = ipv4_cidr_parts(value, description)?;
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    Ok(format!(
        "{}/{}",
        Ipv4Addr::from(u32::from(address) & mask),
        prefix
    ))
}

fn ipv4_cidr_parts(value: &str, description: &str) -> Result<(Ipv4Addr, u8)> {
    let value = validate_cidr(value, description)?;
    ensure_ipv4_prefix(value, description)?;
    let (address, prefix) = value
        .split_once('/')
        .context("validated IPv4 CIDR has no prefix")?;
    Ok((
        address.parse().context("validated IPv4 address changed")?,
        prefix.parse().context("validated IPv4 prefix changed")?,
    ))
}

fn parse_advertised_network_mappings(snapshot: &FlatSnapshot) -> Result<Vec<String>> {
    let prefix = format!("/nodes/{}/network_mapping/", snapshot.node_id);
    let mut advertised = BTreeSet::new();
    for (key, target) in snapshot
        .node
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
    {
        let source = key
            .strip_prefix(&prefix)
            .context("network mapping prefix changed")?;
        if source.is_empty() || target.trim().is_empty() {
            continue;
        }
        let (_, source_prefix) = ipv4_cidr_parts(source, "network mapping source")?;
        let (_, target_prefix) = ipv4_cidr_parts(target.trim(), "network mapping target")?;
        if source_prefix != target_prefix {
            bail!(
                "network mapping {source:?} -> {:?} has different prefix lengths",
                target.trim()
            );
        }
        advertised.insert(source.to_owned());
    }
    Ok(advertised.into_iter().collect())
}

fn parse_prefix_rules(value: Option<&str>, defaults: &[(&str, &str)]) -> Result<Vec<PrefixRule>> {
    let mut rules = Vec::new();
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        for line in nonblank_lines(value) {
            if line.starts_with('#') {
                continue;
            }
            let (action, rest) = line
                .split_once(char::is_whitespace)
                .with_context(|| format!("invalid prefix-list rule line: {line:?}"))?;
            let action = action.to_ascii_lowercase();
            if !matches!(action.as_str(), "permit" | "deny") {
                bail!("invalid action in prefix-list rule: {line:?}");
            }
            let rest = rest.trim();
            validate_prefix_rule_rest(rest)?;
            rules.push(PrefixRule {
                action,
                rest: rest.to_owned(),
            });
        }
    } else {
        for (action, rest) in defaults {
            rules.push(PrefixRule {
                action: (*action).into(),
                rest: (*rest).into(),
            });
        }
    }
    Ok(rules)
}

fn validate_prefix_rule_rest(value: &str) -> Result<()> {
    let mut tokens = value.split_whitespace();
    let prefix = tokens.next().context("prefix-list rule has no prefix")?;
    ipv4_cidr_parts(prefix, "BGP prefix-list rule")?;
    let remainder: Vec<_> = tokens.collect();
    if remainder.len() % 2 != 0 {
        bail!("invalid BGP prefix-list modifiers: {value:?}");
    }
    for pair in remainder.chunks_exact(2) {
        if !matches!(pair[0], "ge" | "le") {
            bail!("invalid BGP prefix-list modifier: {:?}", pair[0]);
        }
        let length: u8 = pair[1]
            .parse()
            .with_context(|| format!("invalid BGP prefix length: {:?}", pair[1]))?;
        if length > 32 {
            bail!("BGP prefix length exceeds 32: {length}");
        }
    }
    Ok(())
}

fn parse_transit_asns(snapshot: &FlatSnapshot) -> Result<(bool, BTreeSet<String>)> {
    let mut all = false;
    let mut asns = BTreeSet::new();
    for value in snapshot
        .global_value("bgp/transit")
        .map(nonblank_lines)
        .unwrap_or_default()
    {
        if value == "*" {
            all = true;
        } else {
            validate_asn(&value, "BGP transit ASN")?;
            asns.insert(value);
        }
    }
    Ok((all, asns))
}

fn parse_frr_transports(
    snapshot: &FlatSnapshot,
    desired: &DesiredState,
) -> Result<Vec<FrrTransport>> {
    let mut transports = Vec::new();
    let mut peers = BTreeSet::new();
    for interface in desired
        .interfaces
        .iter()
        .filter(|interface| matches!(interface.kind, VpnKind::OpenVpn | VpnKind::WireGuard))
    {
        let interface_base = format!("{}/{}", interface.kind.as_str(), interface.instance);
        if !exact_true(snapshot.node_value(&format!("{interface_base}/enable"))) {
            continue;
        }
        let base = format!("{interface_base}/bgp/");
        if !exact_true_or_default(snapshot.node_value(&format!("{base}enable")), true) {
            continue;
        }
        let peer_ip = snapshot
            .node_value(&format!("{base}peer_ip"))
            .filter(|value| !value.is_empty());
        let peer_asn = snapshot
            .node_value(&format!("{base}peer_asn"))
            .filter(|value| !value.is_empty());
        let (peer_ip, peer_asn) = match (peer_ip, peer_asn) {
            (None, None) => continue,
            (Some(peer_ip), Some(peer_asn)) => (peer_ip, peer_asn),
            _ => bail!(
                "BGP transport {}/{} must set both peer_ip and peer_asn",
                interface.kind,
                interface.instance
            ),
        };
        let peer_ip = peer_ip
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid BGP peer IP: {peer_ip:?}"))?;
        validate_asn(peer_asn, "BGP peer ASN")?;
        if !peers.insert(peer_ip) {
            bail!("duplicate BGP transport neighbor: {peer_ip}");
        }
        let update_source = if interface.kind == VpnKind::WireGuard {
            interface.device.clone()
        } else {
            snapshot
                .node_value(&format!("{base}update_source"))
                .filter(|value| !value.is_empty())
                .unwrap_or(&interface.device)
                .to_owned()
        };
        validate_token(&update_source, "BGP update source")?;
        let weight = snapshot
            .node_value(&format!("{base}weight"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| parse_positive_u32(value, "BGP neighbor weight"))
            .transpose()?;
        transports.push(FrrTransport {
            kind: interface.kind,
            name: interface.instance.clone(),
            peer_ip,
            peer_asn: peer_asn.to_owned(),
            update_source,
            weight,
            no_transit: snapshot
                .node_value(&format!("{base}no_transit"))
                .is_some_and(|value| value.eq_ignore_ascii_case("true")),
            no_forward: snapshot
                .node_value(&format!("{base}no_forward"))
                .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        });
    }
    Ok(transports)
}

fn parse_internal_bgp_neighbors(snapshot: &FlatSnapshot) -> Result<Vec<InternalBgpNeighbor>> {
    let mut result = Vec::new();
    let mut router_ids = BTreeSet::new();
    for node_id in snapshot.all_node_ids() {
        if node_id == snapshot.node_id {
            continue;
        }
        validate_node_id(&node_id)?;
        let Some(router_id) = snapshot
            .all_node_value(&node_id, "router_id")
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let router_id = router_id
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid iBGP router ID for {node_id}: {router_id:?}"))?;
        if !router_ids.insert(router_id) {
            bail!("duplicate iBGP router ID: {router_id}");
        }
        let behavior = snapshot
            .all_node_value(&node_id, "behavior")
            .filter(|value| !value.is_empty())
            .unwrap_or("static");
        if !matches!(behavior, "static" | "roaming") {
            bail!("unsupported node behavior for {node_id}: {behavior:?}");
        }
        result.push(InternalBgpNeighbor {
            name: node_id,
            router_id,
            roaming: behavior == "roaming",
        });
    }
    Ok(result)
}

fn append_ospf_connected_policy(lines: &mut Vec<String>, suffix: &str, prefixes: &[String]) {
    if prefixes.is_empty() {
        return;
    }
    for (index, prefix) in prefixes.iter().enumerate() {
        lines.push(format!(
            "ip prefix-list PL-OSPF-{suffix} seq {} permit {prefix}",
            (index + 1) * 10
        ));
    }
    lines.extend([
        String::new(),
        format!(
            "route-map RM-OSPF-CONN{} permit 10",
            if suffix == "LAN" { "" } else { "-PRIVATE" }
        ),
        format!(" match ip address prefix-list PL-OSPF-{suffix}"),
        "!".into(),
        String::new(),
    ]);
}

fn append_bgp_filter_policy(lines: &mut Vec<String>, direction: &str, rules: &[PrefixRule]) {
    for (index, rule) in rules.iter().enumerate() {
        lines.push(format!(
            "ip prefix-list PL-BGP-{direction} seq {} {} {}",
            (index + 1) * 10,
            rule.action,
            rule.rest
        ));
    }
    if direction == "IN" {
        lines.push(String::new());
    }
    lines.extend([
        format!("route-map RM-BGP-{direction} permit 10"),
        format!(" match ip address prefix-list PL-BGP-{direction}"),
        "!".into(),
        String::new(),
    ]);
}

fn append_private_lan_policy(lines: &mut Vec<String>, private_lans: &[String]) {
    if private_lans.is_empty() {
        return;
    }
    for (index, prefix) in private_lans.iter().enumerate() {
        lines.push(format!(
            "ip prefix-list PL-PRIVATE-LAN seq {} permit {prefix}",
            (index + 1) * 10
        ));
    }
    lines.extend([
        String::new(),
        "route-map RM-BGP-OUT-EXTERNAL deny 5".into(),
        " match ip address prefix-list PL-PRIVATE-LAN".into(),
        "route-map RM-BGP-OUT-EXTERNAL permit 10".into(),
        " match ip address prefix-list PL-BGP-OUT".into(),
        "!".into(),
        String::new(),
        "route-map RM-BGP-OUT-INTERNAL permit 5".into(),
        " match ip address prefix-list PL-PRIVATE-LAN".into(),
        "route-map RM-BGP-OUT-INTERNAL permit 10".into(),
        " match ip address prefix-list PL-BGP-OUT".into(),
        "!".into(),
        String::new(),
        "route-map RM-OSPF-TO-BGP deny 10".into(),
        " match ip address prefix-list PL-PRIVATE-LAN".into(),
        "!".into(),
    ]);
}

fn append_bgp_control_policy(lines: &mut Vec<String>, transports: &[FrrTransport]) {
    let has_no_forward = transports.iter().any(|transport| transport.no_forward);
    let has_no_transit = transports.iter().any(|transport| transport.no_transit);
    let mut sorted: Vec<_> = transports.iter().collect();
    sorted.sort_by_key(|transport| transport.peer_ip.to_string());
    if has_no_forward {
        lines.extend([
            format!("bgp community-list standard EBGP_LEARNED permit {COMMUNITY_EBGP_LEARNED}"),
            "!".into(),
            "route-map RM-BGP-IN-TAG-EBGP permit 10".into(),
            " match ip address prefix-list PL-BGP-IN".into(),
            format!(" set community {COMMUNITY_EBGP_LEARNED} additive"),
            "route-map RM-BGP-IN-TAG-EBGP permit 20".into(),
            "!".into(),
            String::new(),
        ]);
    }
    for transport in sorted.iter().filter(|transport| transport.no_transit) {
        let peer = transport.peer_ip.to_string().replace('.', "-");
        lines.extend([
            format!("route-map RM-BGP-IN-{peer} permit 10"),
            " match ip address prefix-list PL-BGP-IN".into(),
            " match as-path 1".into(),
            format!("route-map RM-BGP-IN-{peer} deny 20"),
            " match ip address prefix-list PL-BGP-IN".into(),
            format!("route-map RM-BGP-IN-{peer} permit 30"),
            " ! Allow all other routes".into(),
            "!".into(),
            String::new(),
        ]);
    }
    if has_no_transit {
        lines.extend([
            "bgp as-path access-list 1 permit ^[0-9]+$".into(),
            "!".into(),
        ]);
    }
    for transport in sorted.iter().filter(|transport| transport.no_forward) {
        let peer = transport.peer_ip.to_string().replace('.', "-");
        lines.extend([
            format!("route-map RM-BGP-OUT-{peer} deny 10"),
            " match community EBGP_LEARNED".into(),
            format!("route-map RM-BGP-OUT-{peer} permit 20"),
            " ! Allow locally-originated and iBGP routes".into(),
            "!".into(),
            String::new(),
        ]);
    }
}

fn append_ospf_router(lines: &mut Vec<String>, plan: &FrrPlan) {
    for interface in &plan.active_ifaces {
        lines.extend([
            format!("interface {interface}"),
            format!(" ip ospf area {}", plan.ospf_area),
            " ip ospf network broadcast".into(),
            "!".into(),
        ]);
    }
    lines.push("router ospf".into());
    if let Some(router_id) = plan.router_id {
        lines.push(format!(" ospf router-id {router_id}"));
    }
    if !plan.active_ifaces.is_empty() {
        lines.push(" passive-interface default".into());
        for interface in &plan.active_ifaces {
            lines.push(format!(" no passive-interface {interface}"));
        }
    }
    if !plan.lans.is_empty() {
        lines.push(" redistribute connected route-map RM-OSPF-CONN".into());
    }
    if !plan.private_lans.is_empty() {
        lines.push(" redistribute connected route-map RM-OSPF-CONN-PRIVATE".into());
    }
    if plan.ospf_redistribute_bgp && plan.bgp_enabled {
        lines.push(" redistribute bgp route-map RM-BGP-TO-OSPF".into());
    }
    lines.extend(["!".into(), String::new()]);
}

fn append_bgp_router(lines: &mut Vec<String>, plan: &FrrPlan) -> Result<()> {
    let local_asn = plan
        .local_asn
        .as_deref()
        .context("validated BGP plan has no local ASN")?;
    lines.push(format!("router bgp {local_asn}"));
    if let Some(router_id) = plan.router_id {
        lines.push(format!(" bgp router-id {router_id}"));
    }
    for transport in &plan.transports {
        lines.extend([
            format!(
                " neighbor {} remote-as {}",
                transport.peer_ip, transport.peer_asn
            ),
            format!(
                " neighbor {} description {}",
                transport.peer_ip,
                if transport.kind == VpnKind::OpenVpn {
                    transport.name.clone()
                } else {
                    format!("wg-{}", transport.name)
                }
            ),
            format!(
                " neighbor {} update-source {}",
                transport.peer_ip, transport.update_source
            ),
        ]);
    }
    let update_source = plan
        .router_id
        .map(|value| value.to_string())
        .unwrap_or_default();
    for neighbor in &plan.ibgp_neighbors {
        lines.extend([
            format!(" neighbor {} remote-as internal", neighbor.router_id),
            format!(
                " neighbor {} description {}",
                neighbor.router_id, neighbor.name
            ),
            format!(
                " neighbor {} update-source {update_source}",
                neighbor.router_id
            ),
        ]);
    }
    lines.extend([
        " address-family ipv4 unicast".into(),
        format!("  maximum-paths {}", plan.max_paths),
    ]);
    for prefix in plan
        .lans
        .iter()
        .chain(plan.access_networks.iter())
        .chain(
            (plan.internal == "bgp")
                .then_some(plan.private_lans.iter())
                .into_iter()
                .flatten(),
        )
        .chain(plan.advertised_networks.iter())
    {
        lines.push(format!("  network {prefix}"));
    }
    if !plan.transports.is_empty() {
        for prefix in &plan.edge_broadcast {
            lines.push(format!("  network {prefix}"));
        }
    }
    if plan.ospf_enabled {
        lines.push("  redistribute ospf route-map RM-OSPF-TO-BGP".into());
    }
    let has_no_forward = plan.transports.iter().any(|transport| transport.no_forward);
    for transport in &plan.transports {
        lines.push(format!("  neighbor {} activate", transport.peer_ip));
        if let Some(weight) = transport.weight {
            lines.push(format!("  neighbor {} weight {weight}", transport.peer_ip));
        }
        let peer = transport.peer_ip.to_string().replace('.', "-");
        if transport.no_transit {
            lines.push(format!(
                "  neighbor {} route-map RM-BGP-IN-{peer} in",
                transport.peer_ip
            ));
        } else if has_no_forward && !transport.no_forward {
            lines.push(format!(
                "  neighbor {} route-map RM-BGP-IN-TAG-EBGP in",
                transport.peer_ip
            ));
        } else {
            lines.push(format!(
                "  neighbor {} route-map RM-BGP-IN in",
                transport.peer_ip
            ));
        }
        let outbound = if transport.no_forward {
            format!("RM-BGP-OUT-{peer}")
        } else if plan.private_lans.is_empty() {
            "RM-BGP-OUT".into()
        } else {
            "RM-BGP-OUT-EXTERNAL".into()
        };
        lines.push(format!(
            "  neighbor {} route-map {outbound} out",
            transport.peer_ip
        ));
        if !transport.no_forward
            && (plan.transit_all || plan.transit_asns.contains(&transport.peer_asn))
        {
            lines.push(format!("  neighbor {} next-hop-self", transport.peer_ip));
        }
    }
    for neighbor in &plan.ibgp_neighbors {
        lines.push(format!("  neighbor {} activate", neighbor.router_id));
        lines.push(format!(
            "  neighbor {} route-map {} in",
            neighbor.router_id,
            if neighbor.roaming {
                "RM-BGP-IN-ROAMING"
            } else {
                "RM-BGP-IN"
            }
        ));
        lines.push(format!(
            "  neighbor {} route-map {} out",
            neighbor.router_id,
            if plan.private_lans.is_empty() {
                "RM-BGP-OUT"
            } else {
                "RM-BGP-OUT-INTERNAL"
            }
        ));
        lines.push(format!("  neighbor {} next-hop-self", neighbor.router_id));
    }
    lines.extend([" exit-address-family".into(), "!".into(), String::new()]);
    Ok(())
}

fn prefix_lines(value: Option<&str>, description: &str) -> Result<Vec<String>> {
    value
        .map(nonblank_lines)
        .unwrap_or_default()
        .into_iter()
        .map(|item| validate_cidr(&item, description).map(str::to_owned))
        .collect()
}

fn validate_cidr<'a>(value: &'a str, description: &str) -> Result<&'a str> {
    let value = validate_single_line(value.trim(), description)?;
    let (address, prefix) = value
        .split_once('/')
        .with_context(|| format!("{description} is not CIDR: {value:?}"))?;
    let address: IpAddr = address
        .parse()
        .with_context(|| format!("{description} has an invalid address: {value:?}"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("{description} has an invalid prefix: {value:?}"))?;
    let maximum = if address.is_ipv4() { 32 } else { 128 };
    if prefix > maximum {
        bail!("{description} prefix exceeds {maximum}: {value:?}");
    }
    Ok(value)
}

fn ensure_ipv4_prefix(value: &str, description: &str) -> Result<()> {
    let (address, _) = value
        .split_once('/')
        .with_context(|| format!("{description} is not CIDR"))?;
    if address.parse::<Ipv4Addr>().is_err() {
        bail!("{description} must be IPv4: {value:?}");
    }
    Ok(())
}

fn validate_asn(value: &str, description: &str) -> Result<()> {
    validate_single_line(value, description)?;
    if let Some((high, low)) = value.split_once('.') {
        if value.matches('.').count() != 1 {
            bail!("invalid {description}: {value:?}");
        }
        let high: u16 = high
            .parse()
            .with_context(|| format!("invalid {description}: {value:?}"))?;
        let low: u16 = low
            .parse()
            .with_context(|| format!("invalid {description}: {value:?}"))?;
        if high == 0 && low == 0 {
            bail!("{description} cannot be zero");
        }
    } else {
        let parsed: u32 = value
            .parse()
            .with_context(|| format!("invalid {description}: {value:?}"))?;
        if parsed == 0 {
            bail!("{description} cannot be zero");
        }
    }
    Ok(())
}

fn validate_ospf_area(value: &str) -> Result<()> {
    validate_single_line(value, "OSPF area")?;
    if value.parse::<u32>().is_ok() || value.parse::<Ipv4Addr>().is_ok() {
        return Ok(());
    }
    bail!("invalid OSPF area: {value:?}")
}

fn parse_port(value: &str, description: &str) -> Result<u16> {
    let port: u16 = validate_single_line(value, description)?
        .parse()
        .with_context(|| format!("invalid {description}: {value:?}"))?;
    if port == 0 {
        bail!("{description} cannot be zero");
    }
    Ok(port)
}

fn validate_wireguard_key(value: &str, description: &str) -> Result<()> {
    validate_single_line(value, description)?;
    if value.len() != 44
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        || !value.ends_with('=')
    {
        bail!("{description} is not a canonical 32-byte base64 key");
    }
    Ok(())
}

fn validate_inline_material(value: &str, description: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("{description} contains NUL");
    }
    Ok(())
}

fn validate_single_line<'a>(value: &'a str, description: &str) -> Result<&'a str> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| matches!(character, '\0' | '\n' | '\r') || character.is_control())
    {
        bail!("{description} must be a non-empty single-line value");
    }
    Ok(value)
}

fn validate_token<'a>(value: &'a str, description: &str) -> Result<&'a str> {
    validate_single_line(value, description)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        bail!("{description} contains unsafe token characters: {value:?}");
    }
    Ok(value)
}

fn validate_peer_segment(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        bail!("unsafe WireGuard peer key segment: {value:?}");
    }
    Ok(())
}

fn nonblank_lines(value: &str) -> Vec<String> {
    value
        .replace('\r', "\n")
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn validate_owner(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
    {
        bail!("unsafe Meduza owner identity: {value:?}");
    }
    Ok(())
}

fn managed_alias(owner: &str, kind: VpnKind, instance: &str) -> Result<String> {
    validate_owner(owner)?;
    validate_instance_name(instance)?;
    Ok(format!("{owner}:{}:{instance}", kind.as_str()))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parent(path: &Path) -> Result<PathBuf> {
    path.parent()
        .filter(|parent| parent != &Path::new(""))
        .map(Path::to_path_buf)
        .with_context(|| format!("rendered path has no parent: {path:?}"))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("rendered path is not UTF-8: {path:?}"))
}

fn validate_absolute_file_path(path: &Path) -> Result<()> {
    let value = path_text(path)?;
    if !value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.split('/').any(|part| matches!(part, "." | ".."))
        || value.chars().any(char::is_control)
    {
        bail!("unsafe rendered file path: {path:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::build_desired;

    const WG_PRIVATE: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const WG_PUBLIC: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";

    fn comprehensive_snapshot() -> FlatSnapshot {
        let id = "router-01";
        let mut node = BTreeMap::new();
        let mut put = |suffix: &str, value: &str| {
            node.insert(format!("/nodes/{id}/{suffix}"), value.to_owned());
        };
        put("tinc/enable", "true");
        put("tinc/name", "router_01");
        put("tinc/dev_name", "tnc0");
        put("tinc/ipv4", "10.10.0.1/24");
        put(
            "tinc/private_key",
            "-----BEGIN RSA PRIVATE KEY-----\nkey\n-----END RSA PRIVATE KEY-----",
        );
        put(
            "tinc/public_key",
            "-----BEGIN RSA PUBLIC KEY-----\nkey\n-----END RSA PUBLIC KEY-----",
        );
        put("openvpn/office/enable", "true");
        put("openvpn/office/proto", "udp");
        put("openvpn/office/port", "1194");
        put("openvpn/office/client", "true");
        put(
            "openvpn/office/remote",
            "vpn-a.example 1194\r\nvpn-b.example 1194",
        );
        put("openvpn/office/secret", "static-secret");
        put("openvpn/office/bgp/peer_ip", "10.20.0.2");
        put("openvpn/office/bgp/peer_asn", "65002");
        put("wireguard/backbone/enable", "true");
        put("wireguard/backbone/private_key", WG_PRIVATE);
        put("wireguard/backbone/listen_port", "51820");
        put("wireguard/backbone/address", "10.30.0.1/30");
        put("wireguard/backbone/peer/core/public_key", WG_PUBLIC);
        put("wireguard/backbone/peer/core/endpoint", "wg.example:51820");
        put("wireguard/backbone/bgp/peer_ip", "10.30.0.2");
        put("wireguard/backbone/bgp/peer_asn", "65003");
        put("ospf/enable", "true");
        put("ospf/area", "0.0.0.0");
        put("router_id", "10.255.0.1");
        put("bgp/enable", "true");
        put("bgp/local_asn", "65001");
        put("lan", "192.168.10.0/24");
        put("private_lan", "172.16.10.0/24");

        let global = BTreeMap::from([
            ("/global/mesh_type".into(), "tinc".into()),
            ("/global/tinc/netname".into(), "mesh".into()),
            ("/global/internal_routing_system".into(), "bgp".into()),
        ]);
        let mut all_nodes = node.clone();
        for (suffix, value) in [
            ("tinc/enable", "true"),
            ("tinc/name", "remote_02"),
            ("tinc/address", "remote.example"),
            ("tinc/ipv4", "10.10.0.2/24"),
            ("tinc/public_key", "remote-public-key"),
            ("router_id", "10.255.0.2"),
        ] {
            all_nodes.insert(format!("/nodes/remote-02/{suffix}"), value.into());
        }
        FlatSnapshot::new(id, node, global, all_nodes)
    }

    fn find<'a>(files: &'a [RenderedFile], suffix: &str) -> &'a RenderedFile {
        files
            .iter()
            .find(|file| {
                file.path
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with(suffix)
            })
            .unwrap_or_else(|| panic!("missing rendered file ending in {suffix}"))
    }

    #[test]
    fn renders_three_vpns_and_frr_without_writing_disk() {
        let snapshot = comprehensive_snapshot();
        let desired = build_desired(&snapshot).unwrap();
        let options = RenderOptions::default();
        let files = render_all_with_options(&snapshot, &desired, &options).unwrap();

        let tinc = find(&files, "/tinc/mesh/tinc.conf").text().unwrap();
        assert!(tinc.contains("Name=router01"));
        assert!(tinc.contains("ConnectTo = remote02"));
        assert!(find(&files, "/tinc/mesh/hosts/router01").text().is_ok());
        assert!(find(&files, "/tinc/mesh/hosts/remote02").text().is_ok());
        assert_eq!(find(&files, "/tinc/mesh/rsa_key.priv").mode, 0o600);

        let openvpn = find(&files, "/openvpn/office/openvpn.conf").text().unwrap();
        assert!(openvpn.contains("ignore-unknown-option allow-deprecated-insecure-static-crypto"));
        assert!(openvpn.contains("allow-deprecated-insecure-static-crypto"));
        assert!(openvpn.contains("remote vpn-a.example 1194"));
        assert!(openvpn.contains("route-nopull"));
        assert_eq!(find(&files, "/openvpn/office/secret.pem").mode, 0o600);

        let wireguard = find(&files, "/wireguard/backbone/wg.conf").text().unwrap();
        assert!(wireguard.contains(WG_PRIVATE));
        assert!(wireguard.contains("AllowedIPs = 0.0.0.0/0"));

        let frr = find(&files, "/var/run/meduza/generated/frr/frr.conf")
            .text()
            .unwrap();
        assert!(!frr.contains("router ospf"));
        assert!(frr.contains("router bgp 65001"));
        assert!(frr.contains("bgp router-id 10.255.0.1"));
        assert!(frr.contains("neighbor 10.20.0.2 remote-as 65002"));
        assert!(frr.contains("neighbor 10.20.0.2 update-source tun-office"));
        assert!(frr.contains("neighbor 10.30.0.2 update-source wg-backbone"));
        assert!(frr.contains("neighbor 10.255.0.2 remote-as internal"));
        assert!(frr.contains("network 172.16.10.0/24"));
    }

    #[test]
    fn frr_matches_full_generator_routing_policy_surface() {
        let mut snapshot = comprehensive_snapshot();
        for (suffix, value) in [
            ("bgp/max_paths", "4"),
            ("bgp/to_ospf/default_only", "true"),
            ("access/enable", "true"),
            ("access/network", "10.40.0.7/24"),
            ("openvpn/office/bgp/update_source", "ovpn-source"),
            ("openvpn/office/bgp/weight", "150"),
            ("openvpn/office/bgp/no_transit", "true"),
            ("wireguard/backbone/bgp/no_forward", "true"),
        ] {
            snapshot
                .node
                .insert(format!("/nodes/router-01/{suffix}"), value.into());
        }
        snapshot.node.insert(
            "/nodes/router-01/network_mapping/198.51.100.0/24".into(),
            "10.50.0.0/24".into(),
        );
        for (suffix, value) in [
            ("bgp/filter/in", "deny 0.0.0.0/0\npermit 10.0.0.0/8 le 32"),
            ("bgp/filter/out", "permit 192.0.2.0/24"),
            ("bgp/transit", "65002"),
            ("bgp/edge_broadcast", "203.0.113.0/24"),
        ] {
            snapshot
                .global
                .insert(format!("/global/{suffix}"), value.into());
        }
        snapshot
            .all_nodes
            .insert("/nodes/remote-02/behavior".into(), "roaming".into());

        let desired = build_desired(&snapshot).unwrap();
        let files = render_all(&snapshot, &desired).unwrap();
        let frr = find(&files, "/var/run/meduza/generated/frr/frr.conf")
            .text()
            .unwrap();

        assert!(frr.starts_with("! meduza-owner: meduza-openwrt-rust-v1\n"));
        assert!(frr.contains("\nhostname router-01\n"));
        assert!(frr.contains("\nip router-id 10.255.0.1\n"));
        assert!(!frr.contains("hostname meduza-router-01"));
        assert!(!frr.contains("line vty"));
        assert!(frr.contains("ip prefix-list PL-DEFAULT seq 10 permit 0.0.0.0/0"));
        assert!(frr.contains("ip prefix-list PL-OSPF-LAN seq 10 permit 192.168.10.0/24"));
        assert!(frr.contains("route-map RM-OSPF-CONN permit 10"));
        assert!(frr.contains("route-map RM-BGP-TO-OSPF permit 10\n match ip address prefix-list PL-DEFAULT\n set tag 65000"));
        assert!(frr.contains("ip prefix-list PL-BGP-IN seq 20 permit 10.0.0.0/8 le 32"));
        assert!(frr.contains(
            "ip prefix-list PL-BGP-OUT seq 10 permit 192.0.2.0/24\nroute-map RM-BGP-OUT permit 10"
        ));
        assert!(frr.contains("route-map RM-BGP-OUT-EXTERNAL deny 5"));
        assert!(frr.contains("bgp community-list standard EBGP_LEARNED permit 9999"));
        assert!(frr.contains("route-map RM-BGP-IN-10-20-0-2 permit 10"));
        assert!(frr.contains("bgp as-path access-list 1 permit ^[0-9]+$"));
        assert!(frr.contains("route-map RM-BGP-OUT-10-30-0-2 deny 10"));
        assert!(frr.contains("route-map RM-BGP-IN-ROAMING permit 10"));
        assert!(frr.contains(" neighbor 10.20.0.2 description office"));
        assert!(frr.contains(" neighbor 10.20.0.2 update-source ovpn-source"));
        assert!(frr.contains(" neighbor 10.30.0.2 description wg-backbone"));
        assert!(frr.contains("  maximum-paths 4"));
        assert!(frr.contains("  network 10.40.0.0/24"));
        assert!(frr.contains("  network 198.51.100.0/24"));
        assert!(frr.contains("  network 203.0.113.0/24"));
        assert!(frr.contains("  neighbor 10.20.0.2 weight 150"));
        assert!(frr.contains("  neighbor 10.20.0.2 route-map RM-BGP-IN-10-20-0-2 in"));
        assert!(frr.contains("  neighbor 10.20.0.2 next-hop-self"));
        assert!(frr.contains("  neighbor 10.30.0.2 route-map RM-BGP-OUT-10-30-0-2 out"));
        assert!(!frr.contains("  neighbor 10.30.0.2 next-hop-self"));
        assert!(frr.contains("  neighbor 10.255.0.2 route-map RM-BGP-IN-ROAMING in"));
        assert!(frr.contains("  neighbor 10.255.0.2 route-map RM-BGP-OUT-INTERNAL out"));
    }

    #[test]
    fn legacy_bgp_asn_is_not_accepted() {
        let mut snapshot = comprehensive_snapshot();
        snapshot.node.remove("/nodes/router-01/bgp/local_asn");
        snapshot
            .node
            .insert("/nodes/router-01/bgp/asn".into(), "65009".into());
        let desired = build_desired(&snapshot).unwrap();

        let error = render_all(&snapshot, &desired).unwrap_err().to_string();
        assert!(error.contains("bgp/local_asn"));
    }

    #[test]
    fn local_asn_without_bgp_enable_does_not_enable_bgp() {
        let mut snapshot = comprehensive_snapshot();
        snapshot
            .node
            .insert("/nodes/router-01/bgp/enable".into(), "false".into());
        let desired = build_desired(&snapshot).unwrap();
        let files = render_all(&snapshot, &desired).unwrap();
        let frr = find(&files, "/var/run/meduza/generated/frr/frr.conf")
            .text()
            .unwrap();

        assert!(!frr.contains("router bgp"));
    }

    #[test]
    fn current_ospf_schema_renders_node_router_id() {
        let mut snapshot = comprehensive_snapshot();
        snapshot
            .global
            .insert("/global/internal_routing_system".into(), "ospf".into());
        snapshot.node.insert(
            "/nodes/router-01/ospf/active_ifaces".into(),
            "et1\net0\net0".into(),
        );
        let desired = build_desired(&snapshot).unwrap();
        let files = render_all(&snapshot, &desired).unwrap();
        let frr = find(&files, "/var/run/meduza/generated/frr/frr.conf")
            .text()
            .unwrap();

        assert!(frr.contains("router ospf"));
        assert!(frr.contains("ospf router-id 10.255.0.1"));
        assert!(frr.contains("interface et0\n ip ospf area 0.0.0.0\n ip ospf network broadcast"));
        assert!(frr.contains(" passive-interface default"));
        assert!(frr.contains(" no passive-interface et0"));
        assert!(frr.contains(" redistribute connected route-map RM-OSPF-CONN"));
        assert!(frr.contains(" redistribute connected route-map RM-OSPF-CONN-PRIVATE"));
        assert!(frr.contains(" redistribute bgp route-map RM-BGP-TO-OSPF"));
        assert!(frr.contains("router bgp 65001"));
        assert!(frr.contains("  redistribute ospf route-map RM-OSPF-TO-BGP"));
    }

    #[test]
    fn tls_openvpn_does_not_enable_static_key_compatibility() {
        let mut snapshot = comprehensive_snapshot();
        snapshot
            .node
            .remove("/nodes/router-01/openvpn/office/secret");
        snapshot.node.insert(
            "/nodes/router-01/openvpn/office/tls_client".into(),
            "true".into(),
        );
        snapshot.node.insert(
            "/nodes/router-01/openvpn/office/ca".into(),
            "certificate-authority".into(),
        );
        let desired = build_desired(&snapshot).unwrap();
        let files = render_all(&snapshot, &desired).unwrap();
        let openvpn = find(&files, "/openvpn/office/openvpn.conf").text().unwrap();

        assert!(openvpn.contains("tls-client"));
        assert!(!openvpn.contains("allow-deprecated-insecure-static-crypto"));
    }

    #[test]
    fn rejects_config_directive_injection_before_rendering() {
        let mut snapshot = comprehensive_snapshot();
        snapshot.node.insert(
            "/nodes/router-01/openvpn/office/proto".into(),
            "udp\nup /tmp/evil".into(),
        );
        let desired = build_desired(&snapshot).unwrap();
        let error = render_all(&snapshot, &desired).unwrap_err().to_string();
        assert!(error.contains("single-line"));
    }

    #[test]
    fn rejects_incomplete_bgp_transport() {
        let mut snapshot = comprehensive_snapshot();
        snapshot
            .node
            .remove("/nodes/router-01/openvpn/office/bgp/peer_asn");
        let desired = build_desired(&snapshot).unwrap();
        let error = render_all(&snapshot, &desired).unwrap_err().to_string();
        assert!(error.contains("both peer_ip and peer_asn"));
    }

    #[test]
    fn duplicate_normalized_tinc_names_fail_closed() {
        let mut snapshot = comprehensive_snapshot();
        for (node, address) in [("site-a", "a.example"), ("site.a", "b.example")] {
            for (suffix, value) in [
                ("tinc/enable", "true"),
                ("tinc/address", address),
                ("tinc/public_key", "public-key"),
            ] {
                snapshot
                    .all_nodes
                    .insert(format!("/nodes/{node}/{suffix}"), value.into());
            }
        }
        let desired = build_desired(&snapshot).unwrap();
        let error = render_all(&snapshot, &desired).unwrap_err().to_string();
        assert!(error.contains("duplicate tinc host name"));
    }

    #[test]
    fn rendered_paths_are_unique_and_sorted() {
        let snapshot = comprehensive_snapshot();
        let desired = build_desired(&snapshot).unwrap();
        let files = render_all(&snapshot, &desired).unwrap();
        assert!(files.windows(2).all(|pair| pair[0].path < pair[1].path));
        let unique: BTreeSet<_> = files.iter().map(|file| &file.path).collect();
        assert_eq!(unique.len(), files.len());
    }

    #[test]
    fn dotted_wireguard_peer_keys_remain_input_compatible() {
        let mut snapshot = comprehensive_snapshot();
        snapshot.node.insert(
            "/nodes/router-01/wireguard/backbone/peer/core.example.com/public_key".into(),
            WG_PUBLIC.into(),
        );
        let desired = build_desired(&snapshot).unwrap();
        let files = render_all(&snapshot, &desired).unwrap();
        let wireguard = find(&files, "/wireguard/backbone/wg.conf").text().unwrap();
        assert_eq!(wireguard.matches("[Peer]").count(), 2);
    }
}

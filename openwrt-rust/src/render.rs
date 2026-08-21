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
pub const DEFAULT_FRR_PATH: &str = "/etc/frr/frr.conf";

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
    /// The option is only emitted for a static-key configuration when the
    /// caller has detected it in `openvpn --help`.
    pub openvpn_static_compat: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            owner: DEFAULT_OWNER.to_owned(),
            frr_path: PathBuf::from(DEFAULT_FRR_PATH),
            openvpn_static_compat: false,
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
    let value = if explicit.is_empty() {
        node_id.replace(['.', '-'], "_")
    } else {
        if !explicit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            bail!("unsafe explicit tinc host name for {node_id}: {explicit:?}");
        }
        explicit.to_owned()
    };
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
    if present_secrets.contains("secret") && options.openvpn_static_compat {
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
    let router_id = snapshot
        .node_value("bgp/router_id")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            snapshot
                .node_value("router_id")
                .filter(|value| !value.is_empty())
        });
    if let Some(value) = router_id {
        value
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid FRR router ID: {value:?}"))?;
    }
    let area = snapshot
        .node_value("ospf/area")
        .filter(|value| !value.is_empty())
        .unwrap_or("0");
    validate_ospf_area(area)?;
    let lans = prefix_lines(snapshot.node_value("lan"), "LAN prefix")?;
    let private_lans = prefix_lines(snapshot.node_value("private_lan"), "private LAN prefix")?;
    let internal = snapshot
        .global_value("internal_routing_system")
        .filter(|value| !value.is_empty())
        .unwrap_or("ospf");
    if !matches!(internal, "ospf" | "bgp") {
        bail!("unsupported internal_routing_system: {internal:?}");
    }

    let mut output = String::new();
    writeln!(output, "! meduza-owner: {}", options.owner)?;
    writeln!(output, "frr defaults traditional")?;
    writeln!(output, "hostname meduza-{}", snapshot.node_id)?;
    writeln!(output, "service integrated-vtysh-config")?;
    writeln!(output, "!")?;

    if snapshot.node_enabled("ospf/enable") {
        writeln!(output, "router ospf")?;
        if let Some(router_id) = router_id {
            writeln!(output, " ospf router-id {router_id}")?;
        }
        for prefix in lans.iter().chain(private_lans.iter()) {
            writeln!(output, " network {prefix} area {area}")?;
        }
        writeln!(output, "!")?;
    }

    if let Some(asn) = snapshot
        .node_value("bgp/asn")
        .filter(|value| !value.is_empty())
    {
        validate_asn(asn, "local BGP ASN")?;
        writeln!(output, "router bgp {asn}")?;
        if let Some(router_id) = router_id {
            writeln!(output, " bgp router-id {router_id}")?;
        }
        let mut neighbors = BTreeMap::<Ipv4Addr, (String, String)>::new();
        for interface in desired
            .interfaces
            .iter()
            .filter(|item| matches!(item.kind, VpnKind::OpenVpn | VpnKind::WireGuard))
        {
            let base = format!("{}/{}/bgp/", interface.kind.as_str(), interface.instance);
            if snapshot
                .node_value(&format!("{base}enable"))
                .is_some_and(|value| !parse_enabled(value))
            {
                continue;
            }
            let peer_ip = snapshot
                .node_value(&format!("{base}peer_ip"))
                .filter(|value| !value.is_empty());
            let peer_asn = snapshot
                .node_value(&format!("{base}peer_asn"))
                .filter(|value| !value.is_empty());
            match (peer_ip, peer_asn) {
                (None, None) => continue,
                (Some(ip), Some(peer_asn)) => {
                    let ip = ip
                        .parse::<Ipv4Addr>()
                        .with_context(|| format!("invalid BGP peer IP: {ip:?}"))?;
                    validate_asn(peer_asn, "BGP peer ASN")?;
                    insert_neighbor(
                        &mut neighbors,
                        ip,
                        peer_asn.to_owned(),
                        interface.device.clone(),
                    )?;
                }
                _ => bail!(
                    "BGP transport {}/{} must set both peer_ip and peer_asn",
                    interface.kind,
                    interface.instance
                ),
            }
        }
        if internal == "bgp" {
            for node_id in snapshot.all_node_ids() {
                if node_id == snapshot.node_id {
                    continue;
                }
                validate_node_id(&node_id)?;
                let Some(peer) = snapshot
                    .all_node_value(&node_id, "router_id")
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        snapshot
                            .all_node_value(&node_id, "bgp/router_id")
                            .filter(|value| !value.is_empty())
                    })
                else {
                    continue;
                };
                let peer = peer
                    .parse::<Ipv4Addr>()
                    .with_context(|| format!("invalid iBGP router ID for {node_id}: {peer:?}"))?;
                if router_id.is_some_and(|local| local == peer.to_string()) {
                    continue;
                }
                insert_neighbor(&mut neighbors, peer, "internal".into(), String::new())?;
            }
        }
        for (ip, (remote_as, source)) in neighbors {
            writeln!(output, " neighbor {ip} remote-as {remote_as}")?;
            if !source.is_empty() {
                writeln!(output, " neighbor {ip} update-source {source}")?;
            }
        }
        writeln!(output, " address-family ipv4 unicast")?;
        for prefix in &lans {
            ensure_ipv4_prefix(prefix, "BGP LAN")?;
            writeln!(output, "  network {prefix}")?;
        }
        if internal == "bgp" {
            for prefix in &private_lans {
                ensure_ipv4_prefix(prefix, "BGP private LAN")?;
                writeln!(output, "  network {prefix}")?;
            }
        }
        writeln!(output, " exit-address-family")?;
        writeln!(output, "!")?;
    }
    writeln!(output, "line vty")?;
    writeln!(output, "!")?;
    Ok(rendered_text(options.frr_path.clone(), 0o640, output))
}

fn insert_neighbor(
    neighbors: &mut BTreeMap<Ipv4Addr, (String, String)>,
    address: Ipv4Addr,
    remote_as: String,
    source: String,
) -> Result<()> {
    if let Some(existing) = neighbors.get(&address) {
        if existing != &(remote_as.clone(), source.clone()) {
            bail!("conflicting BGP definitions for neighbor {address}");
        }
        return Ok(());
    }
    neighbors.insert(address, (remote_as, source));
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
        put("bgp/asn", "65001");
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
        let options = RenderOptions {
            openvpn_static_compat: true,
            ..RenderOptions::default()
        };
        let files = render_all_with_options(&snapshot, &desired, &options).unwrap();

        let tinc = find(&files, "/tinc/mesh/tinc.conf").text().unwrap();
        assert!(tinc.contains("Name=router_01"));
        assert!(tinc.contains("ConnectTo = remote_02"));
        assert_eq!(find(&files, "/tinc/mesh/rsa_key.priv").mode, 0o600);

        let openvpn = find(&files, "/openvpn/office/openvpn.conf").text().unwrap();
        assert!(openvpn.contains("allow-deprecated-insecure-static-crypto"));
        assert!(openvpn.contains("remote vpn-a.example 1194"));
        assert!(openvpn.contains("route-nopull"));
        assert_eq!(find(&files, "/openvpn/office/secret.pem").mode, 0o600);

        let wireguard = find(&files, "/wireguard/backbone/wg.conf").text().unwrap();
        assert!(wireguard.contains(WG_PRIVATE));
        assert!(wireguard.contains("AllowedIPs = 0.0.0.0/0"));

        let frr = find(&files, "/etc/frr/frr.conf").text().unwrap();
        assert!(frr.contains("router ospf"));
        assert!(frr.contains("neighbor 10.20.0.2 remote-as 65002"));
        assert!(frr.contains("neighbor 10.30.0.2 update-source wg-backbone"));
        assert!(frr.contains("neighbor 10.255.0.2 remote-as internal"));
        assert!(frr.contains("network 172.16.10.0/24"));
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

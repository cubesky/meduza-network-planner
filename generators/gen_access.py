import ipaddress
import json
from typing import Any, Dict, List, Optional, Tuple

from common import read_input, write_output, split_ml, node_lans


def _is_inline(text: str) -> bool:
    return "\n" in text or "-----BEGIN" in text


def _file_ref(name: str, kind: str, value: str) -> Tuple[str, Dict[str, Any]]:
    if value and value.startswith("/") and not _is_inline(value):
        raise ValueError(f"{kind} must be inline content, not a file path")
    ext = kind.replace("_", "")
    path = f"/etc/openvpn/generated/{name}.{ext}"
    content = value.rstrip() + "\n"
    return path, {"path": path, "content": content, "mode": 0o600}


def _parse_access(node_id: str, node: Dict[str, str]) -> Dict[str, str]:
    base = f"/nodes/{node_id}/access/"
    out: Dict[str, str] = {}
    for k, v in node.items():
        if k.startswith(base):
            out[k[len(base):]] = v
    return out


def _parse_global_access(section: str, global_cfg: Dict[str, str]) -> Dict[str, str]:
    base = f"/global/access/{section}/"
    out: Dict[str, str] = {}
    for k, v in global_cfg.items():
        if k.startswith(base):
            out[k[len(base):]] = v
    return out


def _collect_push_routes(
    node_id: str,
    node: Dict[str, str],
    global_cfg: Dict[str, str],
    all_nodes: Dict[str, str],
    access_network: ipaddress.IPv4Network,
) -> List[str]:
    routes: List[str] = []
    seen = set()

    def add_network(raw: str) -> None:
        try:
            net = ipaddress.ip_network(raw, strict=False)
        except ValueError:
            return
        if net.version != 4:
            return
        if net == access_network:
            return
        key = str(net)
        if key in seen:
            return
        seen.add(key)
        routes.append(key)

    base = "/nodes/"
    per_node: Dict[str, Dict[str, str]] = {}
    for k, v in all_nodes.items():
        if not k.startswith(base):
            continue
        rest = k[len(base):]
        parts = rest.split("/", 1)
        if len(parts) != 2:
            continue
        nid, tail = parts
        per_node.setdefault(nid, {})
        per_node[nid][f"/nodes/{nid}/{tail}"] = v

    if not per_node:
        per_node[node_id] = node

    for nid, data in per_node.items():
        for pfx in node_lans(data, nid):
            add_network(pfx)
        for pfx in split_ml(data.get(f"/nodes/{nid}/private_lan", "")):
            add_network(pfx)
        mapping_base = f"/nodes/{nid}/network_mapping/"
        for key in data:
            if key.startswith(mapping_base):
                add_network(key[len(mapping_base):])

    for pfx in split_ml(global_cfg.get("/global/bgp/edge_broadcast", "")):
        add_network(pfx)

    return sorted(routes)


def _append_inline_file(
    name: str,
    cfg: Dict[str, str],
    files: List[Dict[str, Any]],
    lines: List[str],
    key: str,
    directive: str,
) -> None:
    val = cfg.get(key, "")
    if not val:
        return
    path, file_entry = _file_ref(name, key, val)
    files.append(file_entry)
    lines.append(f"{directive} {path}")


def _maybe_line(lines: List[str], key: str, value: str) -> None:
    value = value.strip()
    if value:
        lines.append(f"{key} {value}")


def generate_access(node_id: str, node: Dict[str, str], global_cfg: Dict[str, str], all_nodes: Dict[str, str]) -> Dict[str, Any]:
    access_cfg = _parse_access(node_id, node)
    enabled = access_cfg.get("enable") == "true"
    if not enabled:
        return {"enabled": False}

    if node.get(f"/nodes/{node_id}/openvpn/access/enable") == "true":
        raise ValueError("/nodes/<NODE_ID>/openvpn/access conflicts with dedicated /nodes/<NODE_ID>/access service")

    port = access_cfg.get("port", "").strip()
    network_raw = access_cfg.get("network", "").strip()
    if not port or not network_raw:
        raise ValueError("access server requires /nodes/<NODE_ID>/access/port and /nodes/<NODE_ID>/access/network")

    try:
        network = ipaddress.ip_network(network_raw, strict=False)
    except ValueError as e:
        raise ValueError(f"invalid access network {network_raw!r}: {e}")
    if not isinstance(network, ipaddress.IPv4Network):
        raise ValueError("access network must be an IPv4 CIDR")

    ldap_cfg = _parse_global_access("ldap", global_cfg)
    ovpn_cfg = _parse_global_access("openvpn", global_cfg)
    if not ldap_cfg.get("uri", "").strip():
        raise ValueError("missing /global/access/ldap/uri")
    if not ldap_cfg.get("base_dn", "").strip():
        raise ValueError("missing /global/access/ldap/base_dn")
    for key in ("ca", "cert", "key"):
        if not ovpn_cfg.get(key, "").strip():
            raise ValueError(f"missing /global/access/openvpn/{key}")

    name = "access"
    dev = ovpn_cfg.get("dev", "").strip() or "tun-access"
    proto = ovpn_cfg.get("proto", "").strip() or "udp"
    push_routes = _collect_push_routes(node_id, node, global_cfg, all_nodes, network)

    lines: List[str] = [
        f"dev {dev}",
        f"proto {proto}",
        f"port {port}",
        "mode server",
        f"server {network.network_address} {network.netmask}",
        "topology subnet",
        "client-to-client",
        "duplicate-cn",
        "persist-key",
        "persist-tun",
        "script-security 2",
        "verify-client-cert none",
        "username-as-common-name",
        "auth-user-pass-verify /usr/local/bin/openvpn_ldap_auth.py via-env",
        "ifconfig-pool-persist /etc/openvpn/generated/access.ipp",
    ]
    files: List[Dict[str, Any]] = []

    for key, directive in [
        ("dev_type", "dev-type"),
        ("local", "local"),
        ("keepalive", "keepalive"),
        ("verb", "verb"),
        ("auth", "auth"),
        ("cipher", "cipher"),
        ("data_ciphers", "data-ciphers"),
        ("topology", "topology"),
        ("reneg_sec", "reneg-sec"),
        ("max_clients", "max-clients"),
        ("sndbuf", "sndbuf"),
        ("rcvbuf", "rcvbuf"),
        ("status", "status"),
        ("status_version", "status-version"),
        ("explicit_exit_notify", "explicit-exit-notify"),
        ("tun_mtu", "tun-mtu"),
        ("mssfix", "mssfix"),
    ]:
        _maybe_line(lines, directive, ovpn_cfg.get(key, ""))

    if not ovpn_cfg.get("keepalive", "").strip():
        lines.append("keepalive 10 60")
    if not ovpn_cfg.get("verb", "").strip():
        lines.append("verb 3")

    for key, directive in [
        ("ca", "ca"),
        ("cert", "cert"),
        ("key", "key"),
        ("dh", "dh"),
        ("tls_auth", "tls-auth"),
        ("tls_crypt", "tls-crypt"),
        ("crl_verify", "crl-verify"),
    ]:
        _append_inline_file(name, ovpn_cfg, files, lines, key, directive)

    _maybe_line(lines, "key-direction", ovpn_cfg.get("key_direction", ""))

    for dns in split_ml(ovpn_cfg.get("push_dns", "")):
        lines.append(f'push "dhcp-option DNS {dns}"')
    for raw in push_routes:
        net = ipaddress.ip_network(raw, strict=False)
        lines.append(f'push "route {net.network_address} {net.netmask}"')
    for extra in split_ml(ovpn_cfg.get("extra_config", "")):
        lines.append(extra)

    ldap_payload = {
        "uri": ldap_cfg.get("uri", "").strip(),
        "bind_dn": ldap_cfg.get("bind_dn", "").strip(),
        "bind_password": ldap_cfg.get("bind_password", ""),
        "base_dn": ldap_cfg.get("base_dn", "").strip(),
        "user_filter": ldap_cfg.get("user_filter", "").strip() or "(&(objectClass=person)(uid={username}))",
        "group_base_dn": ldap_cfg.get("group_base_dn", "").strip() or ldap_cfg.get("base_dn", "").strip(),
        "group_filter": ldap_cfg.get("group_filter", "").strip() or "(&(objectClass=groupOfNames)(cn=access)(member={user_dn}))",
        "ca_cert_path": "",
        "insecure": ldap_cfg.get("insecure", "false").strip().lower() == "true",
        "start_tls": ldap_cfg.get("start_tls", "false").strip().lower() == "true",
    }
    if ldap_cfg.get("ca_cert", "").strip():
        path, file_entry = _file_ref(name, "ldap_ca", ldap_cfg["ca_cert"])
        files.append(file_entry)
        ldap_payload["ca_cert_path"] = path
    files.append({
        "path": "/etc/openvpn/generated/access-ldap.json",
        "content": json.dumps(ldap_payload, ensure_ascii=True, indent=2) + "\n",
        "mode": 0o600,
    })

    return {
        "enabled": True,
        "instance": {
            "name": name,
            "dev": dev,
            "config": "\n".join(lines).strip() + "\n",
            "files": files,
        },
        "network": str(network),
    }


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    global_cfg = payload.get("global", {})
    all_nodes = payload.get("all_nodes", {})
    write_output(generate_access(node_id, node, global_cfg, all_nodes))


if __name__ == "__main__":
    main()

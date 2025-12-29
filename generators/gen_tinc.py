from typing import Any, Dict, List
import ipaddress

from common import read_input, write_output, split_ml


def _ipv4_to_subnet(ipv4: str) -> List[str]:
    """Convert ipv4 CIDR to list of /32 host routes for tinc Subnet entries."""
    if not ipv4:
        return []
    subnets = []
    for line in split_ml(ipv4):
        try:
            network = ipaddress.ip_network(line, strict=False)
            # For point-to-point links, use /32 for individual IPs
            if network.prefixlen == 32:
                subnets.append(str(network))
            else:
                # For larger subnets, advertise the whole network
                subnets.append(str(network))
        except ValueError:
            continue
    return subnets


def _parse_tinc_nodes(nodes: Dict[str, str]) -> Dict[str, Dict[str, str]]:
    base = "/nodes/"
    out: Dict[str, Dict[str, str]] = {}
    for k, v in nodes.items():
        if not k.startswith(base):
            continue
        rest = k[len(base):]
        if "/tinc/" not in rest:
            continue
        node_id, tail = rest.split("/", 1)
        if not tail.startswith("tinc/"):
            continue
        key = tail[len("tinc/"):]
        out.setdefault(node_id, {})
        out[node_id][key] = v
    return out


def _normalize_tinc_pubkey(pubkey: str, ed25519: str) -> str:
    lines = []
    for line in pubkey.splitlines():
        s = line.strip()
        if not s:
            continue
        lines.append(s)
    ed = ed25519.strip()
    if ed:
        if not ed.lower().startswith("ed25519publickey"):
            lines.append(f"Ed25519PublicKey = {ed}")
        else:
            lines.append(ed)
    return "\n".join(lines)


def _tinc_host_content(
    address: str,
    port: str,
    subnets: List[str],
    mode: str,
    cipher: str,
    digest: str,
    pubkey: str,
    ed25519: str,
) -> str:
    lines = []
    if address:
        lines.append(f"Address={address}")
    if mode:
        lines.append(f"Mode={mode}")
    if port:
        lines.append(f"Port={port}")
    if cipher:
        lines.append(f"Cipher={cipher}")
    if digest:
        lines.append(f"Digest={digest}")
    for s in subnets:
        lines.append(f"Subnet={s}")
    key_text = _normalize_tinc_pubkey(pubkey, ed25519)
    host_text = "\n".join(lines + ["", key_text, ""])
    return host_text


def generate_tinc(node_id: str, node: Dict[str, str], all_nodes: Dict[str, str], global_cfg: Dict[str, str]) -> Dict[str, Any]:
    netname = global_cfg.get("/global/tinc/netname", "mesh")
    if not netname:
        raise RuntimeError("missing /global/tinc/netname")
    name = node.get(f"/nodes/{node_id}/tinc/name", node_id)
    name = "".join(ch for ch in name if ch.isalnum())
    if not name:
        raise RuntimeError("invalid /nodes/<NODE_ID>/tinc/name (must be alphanumeric)")

    dev_name = node.get(f"/nodes/{node_id}/tinc/dev_name", "tnc0")
    port = node.get(f"/nodes/{node_id}/tinc/port", "655")
    address = node.get(f"/nodes/{node_id}/tinc/address", "")
    address_family = node.get(f"/nodes/{node_id}/tinc/address_family", "ipv4")
    ipv4 = node.get(f"/nodes/{node_id}/tinc/ipv4", "")
    subnet = node.get(f"/nodes/{node_id}/tinc/subnet", "")
    # If subnet is not explicitly set, auto-generate from ipv4
    if not subnet and ipv4:
        auto_subnets = _ipv4_to_subnet(ipv4)
        subnet = "\n".join(auto_subnets)
    host_mode = node.get(f"/nodes/{node_id}/tinc/host_mode", "")
    host_cipher = node.get(f"/nodes/{node_id}/tinc/host_cipher", "")
    host_digest = node.get(f"/nodes/{node_id}/tinc/host_digest", "")
    conf_mode = node.get(f"/nodes/{node_id}/tinc/mode", "switch")
    conf_cipher = global_cfg.get("/global/tinc/cipher", "")
    conf_digest = global_cfg.get("/global/tinc/digest", "")
    pubkey = node.get(f"/nodes/{node_id}/tinc/public_key", "")
    ed25519 = node.get(f"/nodes/{node_id}/tinc/ed25519_public_key", "")
    privkey = node.get(f"/nodes/{node_id}/tinc/private_key", "")
    ed25519_priv = node.get(f"/nodes/{node_id}/tinc/ed25519_private_key", "")

    if not (pubkey or ed25519):
        raise RuntimeError("missing /nodes/<NODE_ID>/tinc/public_key or /nodes/<NODE_ID>/tinc/ed25519_public_key")
    if not (privkey or ed25519_priv):
        raise RuntimeError("missing /nodes/<NODE_ID>/tinc/private_key or /nodes/<NODE_ID>/tinc/ed25519_private_key")

    files: List[Dict[str, Any]] = []

    nodes = _parse_tinc_nodes(all_nodes)
    connect_to: List[str] = []
    for peer_id, cfg in nodes.items():
        if cfg.get("enable") != "true":
            continue
        peer_name = cfg.get("name", peer_id)
        peer_name = "".join(ch for ch in peer_name if ch.isalnum())
        if peer_name == name:
            continue
        peer_addr = cfg.get("address", "")
        peer_port = cfg.get("port", "")
        peer_subnet = cfg.get("subnet", "")
        peer_ipv4 = cfg.get("ipv4", "")
        # If subnet is not explicitly set, auto-generate from ipv4
        if not peer_subnet and peer_ipv4:
            auto_subnets = _ipv4_to_subnet(peer_ipv4)
            peer_subnet = "\n".join(auto_subnets)
        peer_host_mode = cfg.get("host_mode", "")
        peer_host_cipher = cfg.get("host_cipher", "")
        peer_host_digest = cfg.get("host_digest", "")
        peer_pub = cfg.get("public_key", "")
        peer_ed25519 = cfg.get("ed25519_public_key", "")
        if not (peer_pub or peer_ed25519):
            continue
        host_text = _tinc_host_content(
            peer_addr,
            peer_port,
            split_ml(peer_subnet),
            peer_host_mode,
            peer_host_cipher,
            peer_host_digest,
            peer_pub,
            peer_ed25519,
        )
        files.append({
            "path": f"/etc/tinc/{netname}/hosts/{peer_name}",
            "content": host_text,
            "mode": 0o644,
        })
        if peer_addr:
            connect_to.append(peer_name)

    self_host = _tinc_host_content(
        address,
        port,
        split_ml(subnet),
        host_mode,
        host_cipher,
        host_digest,
        pubkey,
        ed25519,
    )
    files.append({
        "path": f"/etc/tinc/{netname}/hosts/{name}",
        "content": self_host,
        "mode": 0o644,
    })
    if privkey.strip():
        files.append({
            "path": f"/etc/tinc/{netname}/rsa_key.priv",
            "content": f"{privkey.strip()}\n",
            "mode": 0o600,
        })
    if ed25519_priv.strip():
        files.append({
            "path": f"/etc/tinc/{netname}/ed25519_key.priv",
            "content": f"{ed25519_priv.strip()}\n",
            "mode": 0o600,
        })

    tinc_conf = [
        f"Name={name}",
        f"AddressFamily={address_family}",
        f"Mode={conf_mode}",
        "DeviceType=tap",
        f"Interface={dev_name}",
        f"Port={port}",
        "TCPOnly=yes",
    ]
    if conf_cipher:
        tinc_conf.append(f"Cipher={conf_cipher}")
    if conf_digest:
        tinc_conf.append(f"Digest={conf_digest}")
    for peer in sorted(set(connect_to)):
        tinc_conf.append(f"ConnectTo = {peer}")
    files.append({
        "path": f"/etc/tinc/{netname}/tinc.conf",
        "content": "\n".join(tinc_conf) + "\n",
        "mode": 0o644,
    })

    tinc_up = [
        "#!/bin/sh",
        "set -e",
        "ip link set \"$INTERFACE\" up",
    ]
    if ipv4:
        tinc_up.append(f"ip addr add {ipv4} dev \"$INTERFACE\" || true")
    files.append({
        "path": f"/etc/tinc/{netname}/tinc-up",
        "content": "\n".join(tinc_up) + "\n",
        "mode": 0o755,
    })

    tinc_down = [
        "#!/bin/sh",
        "set -e",
    ]
    if ipv4:
        tinc_down.append(f"ip addr del {ipv4} dev \"$INTERFACE\" || true")
    files.append({
        "path": f"/etc/tinc/{netname}/tinc-down",
        "content": "\n".join(tinc_down) + "\n",
        "mode": 0o755,
    })

    files.append({
        "path": "/etc/tinc/.netname",
        "content": f"{netname}\n",
        "mode": 0o644,
    })

    return {"files": files, "netname": netname}


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    all_nodes = payload["all_nodes"]
    global_cfg = payload["global"]
    out = generate_tinc(node_id, node, all_nodes, global_cfg)
    write_output(out)


if __name__ == "__main__":
    main()

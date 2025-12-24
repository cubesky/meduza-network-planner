from typing import Any, Dict, List

from common import read_input, write_output, split_ml


def _wg_dev_name(name: str) -> str:
    return f"wg{name[-1]}" if name and name[-1].isdigit() else f"wg-{name}"


def parse_wireguard(node_id: str, node: Dict[str, str]) -> Dict[str, Dict[str, str]]:
    base = f"/nodes/{node_id}/wireguard/"
    out: Dict[str, Dict[str, str]] = {}
    for k, v in node.items():
        if not k.startswith(base):
            continue
        rest = k[len(base):]
        parts = rest.split("/", 1)
        if len(parts) != 2:
            continue
        name, tail = parts
        out.setdefault(name, {})
        out[name][tail] = v
    return out


def _parse_peers(cfg: Dict[str, str]) -> Dict[str, Dict[str, str]]:
    peers: Dict[str, Dict[str, str]] = {}
    for key, val in cfg.items():
        if not key.startswith("peer/"):
            continue
        rest = key[len("peer/"):]
        parts = rest.split("/", 1)
        if len(parts) != 2:
            continue
        name, field = parts
        peers.setdefault(name, {})
        peers[name][field] = val
    return peers


def _add_kv(lines: List[str], key: str, value: str) -> None:
    if value:
        lines.append(f"{key} = {value}")


def build_config(name: str, cfg: Dict[str, str]) -> str:
    lines: List[str] = ["[Interface]"]

    _add_kv(lines, "PrivateKey", cfg.get("private_key", ""))

    for addr in split_ml(cfg.get("address", "")):
        lines.append(f"Address = {addr}")

    for dns in split_ml(cfg.get("dns", "")):
        lines.append(f"DNS = {dns}")

    _add_kv(lines, "ListenPort", cfg.get("listen_port", ""))
    _add_kv(lines, "MTU", cfg.get("mtu", ""))
    lines.append("Table = off")
    lines.append("PreUp = /bin/true")
    lines.append("PostUp = /bin/true")
    lines.append("PreDown = /bin/true")
    lines.append("PostDown = /bin/true")

    peers = _parse_peers(cfg)
    for peer_name in sorted(peers):
        peer = peers[peer_name]
        lines.append("")
        lines.append("[Peer]")
        _add_kv(lines, "PublicKey", peer.get("public_key", ""))
        _add_kv(lines, "PresharedKey", peer.get("preshared_key", ""))
        allowed = split_ml(peer.get("allowed_ips", ""))
        if not allowed:
            allowed = ["0.0.0.0/0"]
        lines.append(f"AllowedIPs = {', '.join(allowed)}")
        _add_kv(lines, "Endpoint", peer.get("endpoint", ""))
        _add_kv(lines, "PersistentKeepalive", peer.get("persistent_keepalive", ""))

    return "\n".join(lines).strip() + "\n"


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    wg = parse_wireguard(node_id, node)
    instances: List[Dict[str, Any]] = []
    for name, cfg in wg.items():
        if cfg.get("enable") != "true":
            continue
        dev = cfg.get("dev", "") or _wg_dev_name(name)
        config_text = build_config(name, cfg)
        instances.append({
            "name": name,
            "dev": dev,
            "config": config_text,
        })
    write_output({"instances": instances})


if __name__ == "__main__":
    main()

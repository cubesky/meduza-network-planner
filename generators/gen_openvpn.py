from typing import Any, Dict, List, Tuple

from common import read_input, write_output, split_ml


def _ovpn_dev_name(name: str) -> str:
    return f"tun{name[-1]}" if name and name[-1].isdigit() else f"tun-{name}"


def parse_openvpn(node_id: str, node: Dict[str, str]) -> Dict[str, Dict[str, str]]:
    base = f"/nodes/{node_id}/openvpn/"
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


def _is_inline(text: str) -> bool:
    return "\n" in text or "-----BEGIN" in text


def _file_ref(name: str, kind: str, value: str) -> Tuple[str, Dict[str, Any]]:
    if value and value.startswith("/") and not _is_inline(value):
        raise ValueError(f"{kind} must be inline content, not a file path")
    ext = kind.replace("_", "")
    path = f"/etc/openvpn/generated/{name}.{ext}"
    content = value.rstrip() + "\n"
    return path, {"path": path, "content": content, "mode": 0o600}


def _maybe_line(lines: List[str], key: str, value: str) -> None:
    if not value:
        return
    lines.append(f"{key} {value}")


def build_config(name: str, cfg: Dict[str, str]) -> Tuple[str, List[Dict[str, Any]]]:
    files: List[Dict[str, Any]] = []
    lines: List[str] = []

    dev = cfg.get("dev", "") or _ovpn_dev_name(name)
    _maybe_line(lines, "dev", dev)
    _maybe_line(lines, "dev-type", cfg.get("dev_type", ""))
    _maybe_line(lines, "proto", cfg.get("proto", ""))
    _maybe_line(lines, "port", cfg.get("port", ""))
    _maybe_line(lines, "ifconfig", cfg.get("ifconfig", ""))
    _maybe_line(lines, "keepalive", cfg.get("keepalive", ""))
    _maybe_line(lines, "verb", cfg.get("verb", ""))
    _maybe_line(lines, "auth", cfg.get("auth", ""))
    _maybe_line(lines, "cipher", cfg.get("cipher", ""))

    comp_lzo = cfg.get("comp_lzo", "")
    if comp_lzo:
        lines.append(f"comp-lzo {comp_lzo}")
    allow_comp = cfg.get("allow_compression", "")
    if allow_comp:
        lines.append(f"allow-compression {allow_comp}")
    if cfg.get("persist_tun", "") == "1":
        lines.append("persist-tun")

    if cfg.get("client", "") == "1":
        lines.append("client")
    if cfg.get("tls_client", "") == "1":
        lines.append("tls-client")
    _maybe_line(lines, "remote-cert-tls", cfg.get("remote_cert_tls", ""))
    _maybe_line(lines, "key-direction", cfg.get("key_direction", ""))

    remotes = split_ml(cfg.get("remote", ""))
    port = cfg.get("port", "")
    for r in remotes:
        if ":" in r or " " in r:
            lines.append(f"remote {r}")
        elif port:
            lines.append(f"remote {r} {port}")
        else:
            lines.append(f"remote {r}")

    for key, opt in [
        ("secret", "secret"),
        ("ca", "ca"),
        ("cert", "cert"),
        ("key", "key"),
        ("tls_auth", "tls-auth"),
        ("tls_crypt", "tls-crypt"),
    ]:
        val = cfg.get(key, "")
        if not val:
            continue
        path, file_entry = _file_ref(name, key, val)
        if file_entry:
            files.append(file_entry)
        lines.append(f"{opt} {path}")

    return "\n".join(lines).strip() + "\n", files


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    ovpn = parse_openvpn(node_id, node)
    instances: List[Dict[str, Any]] = []
    for name, cfg in ovpn.items():
        if cfg.get("enable") != "true":
            continue
        config_text, files = build_config(name, cfg)
        instances.append({
            "name": name,
            "dev": cfg.get("dev", "") or _ovpn_dev_name(name),
            "config": config_text,
            "files": files,
        })
    write_output({"instances": instances})


if __name__ == "__main__":
    main()

from typing import Any, Dict, List

from common import read_input, write_output


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


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    ovpn = parse_openvpn(node_id, node)
    instances: List[Dict[str, Any]] = []
    for name, cfg in ovpn.items():
        if cfg.get("enable") != "true":
            continue
        if "config" not in cfg:
            continue
        instances.append({
            "name": name,
            "dev": _ovpn_dev_name(name),
            "config": cfg["config"],
        })
    write_output({"instances": instances})


if __name__ == "__main__":
    main()

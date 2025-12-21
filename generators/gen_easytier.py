from typing import Any, Dict, List

from common import read_input, write_output, split_ml


def _toml_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def _toml_kv(key: str, value: Any) -> str:
    if isinstance(value, bool):
        return f"{key} = {'true' if value else 'false'}"
    if isinstance(value, list):
        items = ", ".join(f"\"{_toml_escape(v)}\"" for v in value)
        return f"{key} = [{items}]"
    return f"{key} = \"{_toml_escape(str(value))}\""


def _dump_toml(config: Dict[str, Any]) -> str:
    root_order = [
        "ipv4",
        "listeners",
        "peers",
        "mapped_listeners",
    ]
    lines: List[str] = []
    for key in root_order:
        if key not in config:
            continue
        val = config[key]
        if isinstance(val, list) and not val:
            continue
        if val == "":
            continue
        lines.append(_toml_kv(key, val))

    def dump_section(name: str, section: Dict[str, Any]) -> None:
        keys = [k for k in section.keys() if section.get(k) not in ("", [], None)]
        if not keys:
            return
        lines.append("")
        lines.append(f"[{name}]")
        for key in keys:
            lines.append(_toml_kv(key, section[key]))

    if "network_identity" in config:
        dump_section("network_identity", config["network_identity"])
    if "flags" in config:
        dump_section("flags", config["flags"])

    return "\n".join(lines).strip() + "\n"


def generate_config(node_id: str, node: Dict[str, str], global_cfg: Dict[str, str]) -> Dict[str, Any]:
    def ng(k, d=None):
        return node.get(f"/nodes/{node_id}/easytier/{k}", d)

    def gg(k, d=None):
        return global_cfg.get(f"/global/easytier/{k}", d)

    network_name = gg("network_name", "")
    network_secret = gg("network_secret", "")
    if not network_name or not network_secret:
        raise RuntimeError("missing /global/easytier/network_name or /global/easytier/network_secret")

    listeners = split_ml(ng("listeners", ""))
    peers = split_ml(ng("peers", ""))
    mapped_listeners = split_ml(ng("mapped_listeners", ""))

    config: Dict[str, Any] = {
        "network_identity": {
            "network_name": network_name,
            "network_secret": network_secret,
        },
        "flags": {},
    }

    config["flags"]["dev_name"] = ng("dev_name", "et0")
    config["flags"]["multi_thread"] = True

    if gg("private_mode", "false") == "true":
        config["flags"]["private_mode"] = True

    ipv4 = ng("ipv4", "")
    if ipv4:
        config["ipv4"] = ipv4

    if gg("dhcp", "false") == "true":
        config["flags"]["dhcp"] = True
    if listeners:
        config["listeners"] = listeners
    if peers:
        config["peers"] = peers
    if mapped_listeners:
        config["mapped_listeners"] = mapped_listeners

    config["flags"]["enable_exit_node"] = True
    config["flags"]["proxy_forward_by_system"] = True

    args = [
        "--config-file", "/etc/easytier/config.yaml",
    ]

    return {
        "config_text": _dump_toml(config),
        "args": args,
    }


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    global_cfg = payload["global"]
    out = generate_config(node_id, node, global_cfg)
    write_output(out)


if __name__ == "__main__":
    main()

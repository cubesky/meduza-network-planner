from typing import Any, Dict

import toml

from common import read_input, write_output, split_ml


def _normalize_listener(val: str) -> str:
    v = val.strip()
    if "://" in v:
        return v
    if v.startswith("tcp:") or v.startswith("udp:") or v.startswith("wg:"):
        scheme, rest = v.split(":", 1)
        rest = rest.strip("/")
        return f"{scheme}://0.0.0.0:{rest}"
    return v


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
        "instance_name": node_id,
        "network_identity": {
            "network_name": network_name,
            "network_secret": network_secret,
        },
        "flags": {},
    }

    config["flags"]["dev_name"] = ng("dev_name", "et0")
    config["flags"]["multi_thread"] = True
    config["flags"]["manual_routes"] = True
    config["flags"]["use-smoltcp"] = True

    if gg("private_mode", "false") == "true":
        config["flags"]["private_mode"] = True

    ipv4 = ng("ipv4", "")
    if ipv4:
        config["ipv4"] = ipv4

    config["dhcp"] = False
    if listeners:
        config["listeners"] = [_normalize_listener(x) for x in listeners]
    if peers:
        config["peer"] = [{"uri": v} for v in peers]
    if mapped_listeners:
        config["mapped_listeners"] = mapped_listeners
    config["rpc_portal"] = "0.0.0.0:0"

    config["proxy_network"] = [{"cidr": "0.0.0.0/0"}]

    args = [
        "--config-file", "/etc/easytier/config.yaml",
    ]

    return {
        "config_text": toml.dumps(config),
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

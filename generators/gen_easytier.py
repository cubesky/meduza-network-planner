from typing import Any, Dict, List

import yaml

from common import read_input, write_output, split_ml


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
        "network_name": network_name,
        "network_secret": network_secret,
        "dev_name": ng("dev_name", "et0"),
    }

    if gg("private_mode", "false") == "true":
        config["private_mode"] = True

    ipv4 = ng("ipv4", "")
    if ipv4:
        config["ipv4"] = ipv4

    if gg("dhcp", "false") == "true":
        config["dhcp"] = True
    if listeners:
        config["listeners"] = listeners
    if peers:
        config["peers"] = peers
    if mapped_listeners:
        config["mapped_listeners"] = mapped_listeners

    args = [
        "easytier-core",
        "--config", "/etc/easytier/config.yaml",
        "--enable-exit-node",
        "--proxy-forward-by-system",
    ]

    return {
        "config_yaml": yaml.safe_dump(config, sort_keys=False, allow_unicode=True),
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

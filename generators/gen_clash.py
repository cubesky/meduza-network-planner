import json
from typing import Any, Dict, List

import requests
import yaml

from common import read_input, write_output, node_lans

TPROXY_PORT = 7893
SOCKS_PORT = 7891
HTTP_PORT = 7890


def _node_lans_for_exclude(node: Dict[str, str], node_id: str) -> List[str]:
    cidrs = [
        "127.0.0.0/8", "0.0.0.0/8", "10.0.0.0/8",
        "172.16.0.0/12", "192.168.0.0/16",
        "169.254.0.0/16", "224.0.0.0/4", "240.0.0.0/4",
        "10.42.1.0/24",
        "10.88.0.0/16",   # podman
        "10.89.0.0/16",   # podman
        "10.0.0.0/24",    # docker default
    ]
    cidrs.extend(node_lans(node, node_id))
    return sorted(set(cidrs))


def _subscriptions(global_cfg: Dict[str, str]) -> Dict[str, str]:
    subs: Dict[str, str] = {}
    for k, v in global_cfg.items():
        if k.startswith("/global/clash/subscriptions/") and k.endswith("/url"):
            name = k.split("/global/clash/subscriptions/")[1].split("/url")[0]
            subs[name] = v
    return subs


def generate_clash(node_id: str, node: Dict[str, str], global_cfg: Dict[str, str]) -> Dict[str, Any]:
    base = yaml.safe_load(open("/clash/base.yaml", encoding="utf-8")) or {}
    mode = node.get(f"/nodes/{node_id}/clash/mode", "mixed")

    subs = _subscriptions(global_cfg)
    active = node.get(f"/nodes/{node_id}/clash/active_subscription")
    if not active:
        raise RuntimeError("missing /nodes/<NODE_ID>/clash/active_subscription")
    if active not in subs:
        raise RuntimeError(f"active_subscription {active!r} not found under /global/clash/subscriptions/")

    resp = requests.get(subs[active], timeout=15)
    resp.raise_for_status()
    sub_conf = yaml.safe_load(resp.text) or {}

    merged = dict(base)
    merged.update(sub_conf)

    dns_cfg = merged.get("dns")
    if not isinstance(dns_cfg, dict):
        dns_cfg = {}
    dns_cfg["enhanced-mode"] = "redir-host"
    merged["dns"] = dns_cfg

    merged["external-ui"] = "/etc/clash/ui"

    # Set essential Clash Meta configurations for optimal performance
    merged["find-process-mode"] = "off"
    merged["unified-delay"] = True
    merged["geodata-loader"] = "standard"

    merged["socks-port"] = SOCKS_PORT
    if mode == "mixed":
        merged["mixed-port"] = HTTP_PORT
    elif mode == "tproxy":
        merged["tproxy-port"] = TPROXY_PORT
    else:
        raise RuntimeError(f"unsupported clash mode: {mode}")

    refresh_enable = node.get(f"/nodes/{node_id}/clash/refresh/enable") == "true"
    raw_interval = node.get(f"/nodes/{node_id}/clash/refresh/interval_minutes", "")
    try:
        interval = int(raw_interval)
    except Exception:
        interval = 0

    return {
        "config_yaml": yaml.safe_dump(merged, sort_keys=False, allow_unicode=True),
        "mode": mode,
        "tproxy_exclude": _node_lans_for_exclude(node, node_id),
        "refresh_enable": refresh_enable,
        "refresh_interval_minutes": interval,
    }


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    global_cfg = payload["global"]
    out = generate_clash(node_id, node, global_cfg)
    write_output(out)


if __name__ == "__main__":
    main()

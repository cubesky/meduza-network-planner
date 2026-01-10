import json
from typing import Any, Dict, List

import requests
import yaml

from common import read_input, write_output, node_lans

TPROXY_PORT = 7893
SOCKS_PORT = 7891
HTTP_PORT = 7890


def _node_lans_for_proxy(node: Dict[str, str], node_id: str) -> List[str]:
    """
    Generate list of source CIDRs to proxy (include mode).
    Only proxy traffic originating from LANs specified in /lan and /private_lan.
    All other traffic will bypass the proxy.
    """
    from common import split_ml

    # Get public LANs
    lans = split_ml(node.get(f"/nodes/{node_id}/lan", ""))

    # Get private LANs
    private_lans = split_ml(node.get(f"/nodes/{node_id}/private_lan", ""))

    # Combine both
    cidrs = lans + private_lans
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

    # Filter out DUMMY-GROUPS from proxy-groups
    if "proxy-groups" in merged:
        proxy_groups = merged["proxy-groups"]
        if isinstance(proxy_groups, list):
            merged["proxy-groups"] = [pg for pg in proxy_groups if pg.get("name") != "DUMMY-GROUPS"]

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

    # Ensure API configuration is present for health checks
    if "external-controller" not in merged:
        merged["external-controller"] = "0.0.0.0:9090"
    if "secret" not in merged:
        merged["secret"] = "BFC8rqg0umu-qay-xtq"

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

    # Get TPROXY protocol setting (default: tcp+udp)
    tproxy_protocol = node.get(f"/nodes/{node_id}/clash/tproxy_protocol", "tcp+udp")
    # Validate protocol value
    if tproxy_protocol not in ("tcp", "udp", "tcp+udp"):
        raise RuntimeError(f"invalid tproxy_protocol: {tproxy_protocol!r}, must be 'tcp', 'udp', or 'tcp+udp'")

    # Get conntrack usage setting (default: false for backward compatibility)
    use_conntrack = node.get(f"/nodes/{node_id}/clash/use_conntrack", "false") == "true"

    return {
        "config_yaml": yaml.safe_dump(merged, sort_keys=False, allow_unicode=True),
        "mode": mode,
        "tproxy_targets": _node_lans_for_proxy(node, node_id),
        "tproxy_protocol": tproxy_protocol,
        "use_conntrack": use_conntrack,
        "refresh_enable": refresh_enable,
        "refresh_interval_minutes": interval,
        "api_controller": merged.get("external-controller", "0.0.0.0:9090"),
        "api_secret": merged.get("secret", ""),
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

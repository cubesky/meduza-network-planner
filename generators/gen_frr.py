import json
from typing import Any, Dict, List, Tuple

from common import read_input, write_output, split_ml, node_lans

TAG_NO_REINJECT = 65000


def _parse_prefix_list_rules(multiline: str) -> List[Tuple[str, str]]:
    rules: List[Tuple[str, str]] = []
    if not multiline:
        return rules
    s = multiline.replace("\r\n", "\n").replace("\r", "\n")
    for line in s.split("\n"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            raise ValueError(f"invalid prefix-list rule line: {line!r}")
        action, rest = parts[0].lower(), parts[1].strip()
        if action not in ("permit", "deny"):
            raise ValueError(f"invalid action in prefix-list rule: {line!r}")
        rules.append((action, rest))
    return rules


def generate_frr(node_id: str, node: Dict[str, str], global_cfg: Dict[str, str]) -> str:
    router_id = node.get(f"/nodes/{node_id}/router_id", "")
    ospf_enable = node.get(f"/nodes/{node_id}/ospf/enable") == "true"
    bgp_enable = node.get(f"/nodes/{node_id}/bgp/enable") == "true"

    local_as = node.get(f"/nodes/{node_id}/bgp/local_asn", "")
    max_paths = node.get(f"/nodes/{node_id}/bgp/max_paths", "1")
    to_ospf_default_only = node.get(f"/nodes/{node_id}/bgp/to_ospf/default_only") == "true"
    ospf_redistribute_bgp = node.get(f"/nodes/{node_id}/ospf/redistribute_bgp") == "true"
    inject_site_lan = node.get(f"/nodes/{node_id}/ospf/inject_site_lan") == "true"

    in_rules_ml = global_cfg.get("/global/bgp/filter/in", "")
    out_rules_ml = global_cfg.get("/global/bgp/filter/out", "")

    in_rules = _parse_prefix_list_rules(in_rules_ml) if in_rules_ml else [
        ("deny", "0.0.0.0/0"),
        ("permit", "0.0.0.0/0 le 32"),
    ]
    out_rules = _parse_prefix_list_rules(out_rules_ml) if out_rules_ml else [
        ("permit", "0.0.0.0/0 le 32"),
    ]

    active_key = f"/nodes/{node_id}/ospf/active_ifaces"
    if active_key in node:
        active_ifaces = sorted(set(split_ml(node.get(active_key, ""))))
    else:
        active_ifaces = sorted({
            k.split("/")[-1]
            for k in node
            if k.startswith(f"/nodes/{node_id}/ospf/active_ifaces/")
        })

    lans = node_lans(node, node_id)

    lines: List[str] = [
        "frr defaults traditional",
        "service integrated-vtysh-config",
        f"hostname {node_id}",
    ]
    if router_id:
        lines.append(f"ip router-id {router_id}")

    lines += ["", "ip prefix-list PL-DEFAULT seq 10 permit 0.0.0.0/0", ""]

    if inject_site_lan and lans:
        seq = 10
        for pfx in lans:
            lines.append(f"ip prefix-list PL-OSPF-LAN seq {seq} permit {pfx}")
            seq += 10
        lines.append("")
        lines.append("route-map RM-OSPF-CONN permit 10")
        lines.append(" match ip address prefix-list PL-OSPF-LAN")
        lines.append("!")
        lines.append("")

    seq = 10
    for action, rest in in_rules:
        lines.append(f"ip prefix-list PL-BGP-IN seq {seq} {action} {rest}")
        seq += 10
    lines.append("")
    lines.append("route-map RM-BGP-IN permit 10")
    lines.append(" match ip address prefix-list PL-BGP-IN")
    lines.append("!")
    lines.append("")

    seq = 10
    for action, rest in out_rules:
        lines.append(f"ip prefix-list PL-BGP-OUT seq {seq} {action} {rest}")
        seq += 10
    lines.append("route-map RM-BGP-OUT permit 10")
    lines.append(" match ip address prefix-list PL-BGP-OUT")
    lines.append("!")
    lines.append("")

    lines += ["route-map RM-BGP-TO-OSPF permit 10"]
    if to_ospf_default_only:
        lines.append(" match ip address prefix-list PL-DEFAULT")
    lines += [f" set tag {TAG_NO_REINJECT}", "!", ""]

    lines += [
        "route-map RM-OSPF-TO-BGP deny 10",
        f" match tag {TAG_NO_REINJECT}",
        "!",
        "route-map RM-OSPF-TO-BGP permit 20",
        "!",
        "",
    ]

    if ospf_enable:
        ospf_area = node.get(f"/nodes/{node_id}/ospf/area", "0")
        for iface in active_ifaces:
            lines.append(f"interface {iface}")
            lines.append(f" ip ospf area {ospf_area}")
            lines.append(" no ip ospf passive")
            lines.append("!")
        lines.append("router ospf")
        if router_id:
            lines.append(f" ospf router-id {router_id}")
        lines.append(" passive-interface default")
        if inject_site_lan and lans:
            lines.append(" redistribute connected route-map RM-OSPF-CONN")
        if ospf_redistribute_bgp and bgp_enable:
            lines.append(" redistribute bgp route-map RM-BGP-TO-OSPF")
        lines += ["!", ""]

    if bgp_enable and local_as:
        lines.append(f"router bgp {local_as}")
        if router_id:
            lines.append(f" bgp router-id {router_id}")
        lines.append(f" maximum-paths {max_paths}")

        for pfx in lans:
            lines.append(f" network {pfx}")
        lines.append(" redistribute ospf route-map RM-OSPF-TO-BGP")

        base = f"/nodes/{node_id}/openvpn/"
        ovpn: Dict[str, Dict[str, str]] = {}
        for k, v in node.items():
            if not k.startswith(base):
                continue
            rest = k[len(base):]
            parts = rest.split("/", 1)
            if len(parts) != 2:
                continue
            name, tail = parts
            ovpn.setdefault(name, {})
            ovpn[name][tail] = v
        for name, cfg in ovpn.items():
            if cfg.get("enable") != "true":
                continue
            peer_ip = cfg.get("bgp/peer_ip", "")
            peer_asn = cfg.get("bgp/peer_asn", "")
            update_source = cfg.get("bgp/update_source", "")
            if peer_ip and peer_asn and update_source:
                lines.append(f" neighbor {peer_ip} remote-as {peer_asn}")
                lines.append(f" neighbor {peer_ip} update-source {update_source}")
                lines.append(f" neighbor {peer_ip} route-map RM-BGP-IN in")
                lines.append(f" neighbor {peer_ip} route-map RM-BGP-OUT out")
        lines += ["!", ""]

    return "\n".join(lines).strip() + "\n"


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    global_cfg = payload["global"]
    conf_text = generate_frr(node_id, node, global_cfg)
    write_output({"frr_conf": conf_text})


if __name__ == "__main__":
    main()

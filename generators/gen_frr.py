import json
from typing import Any, Dict, List, Tuple

from common import read_input, write_output, split_ml, node_lans

TAG_NO_REINJECT = 65000


def _ovpn_dev_name(name: str) -> str:
    return f"tun{name[-1]}" if name and name[-1].isdigit() else f"tun-{name}"


def _wg_dev_name(name: str) -> str:
    return f"wg{name[-1]}" if name and name[-1].isdigit() else f"wg-{name}"


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


def _parse_openvpn(node_id: str, node: Dict[str, str]) -> Dict[str, Dict[str, str]]:
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


def _parse_wireguard(node_id: str, node: Dict[str, str]) -> Dict[str, Dict[str, str]]:
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


def _node_is_exit(ovpn: Dict[str, Dict[str, str]], wg: Dict[str, Dict[str, str]]) -> bool:
    for cfg in ovpn.values():
        if cfg.get("enable") != "true":
            continue
        if cfg.get("bgp/peer_ip") and cfg.get("bgp/peer_asn"):
            return True
    for cfg in wg.values():
        if cfg.get("enable") != "true":
            continue
        if cfg.get("bgp/peer_ip") and cfg.get("bgp/peer_asn"):
            return True
    return False


def _internal_bgp_neighbors(
    node_id: str,
    all_nodes: Dict[str, str],
) -> Dict[str, Dict[str, str]]:
    out: Dict[str, Dict[str, str]] = {}
    base = "/nodes/"
    per_node: Dict[str, Dict[str, str]] = {}
    for k, v in all_nodes.items():
        if not k.startswith(base):
            continue
        rest = k[len(base):]
        parts = rest.split("/", 1)
        if len(parts) != 2:
            continue
        nid, tail = parts
        per_node.setdefault(nid, {})
        per_node[nid][f"/nodes/{nid}/{tail}"] = v
    for nid, data in per_node.items():
        if nid == node_id:
            continue
        router_id = data.get(f"/nodes/{nid}/router_id", "")
        if not router_id:
            continue
        ovpn = _parse_openvpn(nid, data)
        wg = _parse_wireguard(nid, data)
        out[nid] = {
            "router_id": router_id,
            "is_exit": "true" if _node_is_exit(ovpn, wg) else "false",
            "name": nid,
        }
    return out


def _iter_bgp_transports(
    ovpn: Dict[str, Dict[str, str]],
    wg: Dict[str, Dict[str, str]],
) -> List[Tuple[str, str, Dict[str, str], str]]:
    out: List[Tuple[str, str, Dict[str, str], str]] = []
    for name, cfg in ovpn.items():
        dev = cfg.get("dev", "") or _ovpn_dev_name(name)
        out.append(("openvpn", name, cfg, dev))
    for name, cfg in wg.items():
        dev = cfg.get("dev", "") or _wg_dev_name(name)
        out.append(("wireguard", name, cfg, dev))
    return out


def generate_frr(node_id: str, node: Dict[str, str], global_cfg: Dict[str, str], all_nodes: Dict[str, str]) -> str:
    router_id = node.get(f"/nodes/{node_id}/router_id", "")
    internal_routing = global_cfg.get("/global/internal_routing_system", "ospf")
    ospf_enable = node.get(f"/nodes/{node_id}/ospf/enable") == "true"
    bgp_enable = node.get(f"/nodes/{node_id}/bgp/enable") == "true"

    local_as = node.get(f"/nodes/{node_id}/bgp/local_asn", "")
    max_paths = node.get(f"/nodes/{node_id}/bgp/max_paths", "1")
    to_ospf_default_only = node.get(f"/nodes/{node_id}/bgp/to_ospf/default_only") == "true"
    ospf_redistribute_bgp = node.get(f"/nodes/{node_id}/ospf/redistribute_bgp") == "true"
    inject_site_lan = node.get(f"/nodes/{node_id}/ospf/inject_site_lan") == "true"
    if internal_routing == "bgp":
        ospf_enable = False

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
    private_lans = sorted(set(split_ml(node.get(f"/nodes/{node_id}/private_lan", ""))))

    lines: List[str] = [
        "frr defaults traditional",
        "service integrated-vtysh-config",
        f"hostname {node_id}",
    ]
    if router_id:
        lines.append(f"ip router-id {router_id}")

    lines += ["", "ip prefix-list PL-DEFAULT seq 10 permit 0.0.0.0/0", ""]

    if inject_site_lan and (lans or private_lans):
        seq = 10
        for pfx in lans + private_lans:
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

    if private_lans:
        seq = 10
        for pfx in private_lans:
            lines.append(f"ip prefix-list PL-PRIVATE-LAN seq {seq} permit {pfx}")
            seq += 10
        lines.append("")
        lines.append("route-map RM-BGP-OUT-EXTERNAL deny 5")
        lines.append(" match ip address prefix-list PL-PRIVATE-LAN")
        lines.append("route-map RM-BGP-OUT-EXTERNAL permit 10")
        lines.append(" match ip address prefix-list PL-BGP-OUT")
        lines.append("!")
        lines.append("")
        lines.append("route-map RM-BGP-OUT-INTERNAL permit 5")
        lines.append(" match ip address prefix-list PL-PRIVATE-LAN")
        lines.append("route-map RM-BGP-OUT-INTERNAL permit 10")
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

        ovpn = _parse_openvpn(node_id, node)
        wg = _parse_wireguard(node_id, node)
        self_is_exit = _node_is_exit(ovpn, wg)
        for kind, name, cfg, dev in _iter_bgp_transports(ovpn, wg):
            if cfg.get("enable") != "true":
                continue
            peer_ip = cfg.get("bgp/peer_ip", "")
            peer_asn = cfg.get("bgp/peer_asn", "")
            update_source = dev if kind == "wireguard" else (cfg.get("bgp/update_source", "") or dev)
            if peer_ip and peer_asn and update_source:
                desc = name if kind == "openvpn" else f"wg-{name}"
                lines.append(f" neighbor {peer_ip} remote-as {peer_asn}")
                lines.append(f" neighbor {peer_ip} description {desc}")
                lines.append(f" neighbor {peer_ip} update-source {update_source}")
        ibgp_neighbors: List[Dict[str, str]] = []
        if internal_routing == "bgp":
            neighbors = _internal_bgp_neighbors(node_id, all_nodes)
            for _nid, info in neighbors.items():
                peer_ip = info["router_id"]
                lines.append(f" neighbor {peer_ip} remote-as internal")
                lines.append(f" neighbor {peer_ip} description {info['name']}")
                lines.append(f" neighbor {peer_ip} update-source {router_id}")
                ibgp_neighbors.append(info)
        lines.append(" address-family ipv4 unicast")
        lines.append(f"  maximum-paths {max_paths}")
        for pfx in lans:
            lines.append(f"  network {pfx}")
        if internal_routing == "bgp":
            for pfx in private_lans:
                lines.append(f"  network {pfx}")
        if ospf_enable:
            lines.append("  redistribute ospf route-map RM-OSPF-TO-BGP")
        for _kind, name, cfg, dev in _iter_bgp_transports(ovpn, wg):
            if cfg.get("enable") != "true":
                continue
            peer_ip = cfg.get("bgp/peer_ip", "")
            peer_asn = cfg.get("bgp/peer_asn", "")
            update_source = dev if _kind == "wireguard" else (cfg.get("bgp/update_source", "") or dev)
            if peer_ip and peer_asn and update_source:
                lines.append(f"  neighbor {peer_ip} activate")
                lines.append(f"  neighbor {peer_ip} route-map RM-BGP-IN in")
                if private_lans:
                    lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT-EXTERNAL out")
                else:
                    lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT out")

        for info in ibgp_neighbors:
            peer_ip = info["router_id"]
            lines.append(f"  neighbor {peer_ip} activate")
            lines.append(f"  neighbor {peer_ip} route-map RM-BGP-IN in")
            if private_lans:
                lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT-INTERNAL out")
            else:
                lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT out")
            if self_is_exit and info.get("is_exit") != "true":
                lines.append(f"  neighbor {peer_ip} next-hop-self")
        lines.append(" exit-address-family")
        lines += ["!", ""]

    return "\n".join(lines).strip() + "\n"


def main() -> None:
    payload = read_input()
    node_id = payload["node_id"]
    node = payload["node"]
    global_cfg = payload["global"]
    conf_text = generate_frr(node_id, node, global_cfg, payload.get("all_nodes", {}))
    write_output({"frr_conf": conf_text})


if __name__ == "__main__":
    main()

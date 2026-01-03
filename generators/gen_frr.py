import json
from typing import Any, Dict, List, Tuple, Set

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


def _bgp_enabled(cfg: Dict[str, str]) -> bool:
    return cfg.get("bgp/enable", "true") == "true"


def _node_is_exit(ovpn: Dict[str, Dict[str, str]], wg: Dict[str, Dict[str, str]]) -> bool:
    for cfg in ovpn.values():
        if cfg.get("enable") != "true":
            continue
        if not _bgp_enabled(cfg):
            continue
        if cfg.get("bgp/peer_ip") and cfg.get("bgp/peer_asn"):
            return True
    for cfg in wg.values():
        if cfg.get("enable") != "true":
            continue
        if not _bgp_enabled(cfg):
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
    ospf_redistribute_bgp = node.get(f"/nodes/{node_id}/ospf/redistribute_bgp", "true") == "true"
    inject_site_lan = node.get(f"/nodes/{node_id}/ospf/inject_site_lan", "true") == "true"
    inject_private_lan = node.get(f"/nodes/{node_id}/ospf/inject_private_lan", "true") == "true"

    # Parse BGP transit AS list (newline-separated, '*' means allow all)
    bgp_transit_raw = global_cfg.get("/global/bgp/transit", "")
    bgp_transit_as_list: Set[str] = set()
    bgp_transit_all = False
    if bgp_transit_raw:
        for line in split_ml(bgp_transit_raw):
            line = line.strip()
            if line == "*":
                bgp_transit_all = True
            elif line:
                bgp_transit_as_list.add(line)

    # Parse BGP edge broadcast prefixes (newline-separated)
    bgp_edge_broadcast = sorted(set(split_ml(global_cfg.get("/global/bgp/edge_broadcast", ""))))

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

    lans = node_lans(node, node_id) if inject_site_lan else []
    private_lans = sorted(set(split_ml(node.get(f"/nodes/{node_id}/private_lan", "")))) if inject_private_lan else []

    lines: List[str] = [
        "frr defaults traditional",
        "service integrated-vtysh-config",
        f"hostname {node_id}",
    ]
    if router_id:
        lines.append(f"ip router-id {router_id}")

    lines += ["", "ip prefix-list PL-DEFAULT seq 10 permit 0.0.0.0/0", ""]

    # Prefix lists for LAN and private LAN for OSPF redistribution
    # FRR will redistribute connected routes and filter by these prefix lists
    # Only routes that are actually connected (in routing table) will be advertised
    if lans:
        seq = 10
        for pfx in lans:
            lines.append(f"ip prefix-list PL-OSPF-LAN seq {seq} permit {pfx}")
            seq += 10
        lines.append("")
        lines.append("route-map RM-OSPF-CONN permit 10")
        lines.append(" match ip address prefix-list PL-OSPF-LAN")
        lines.append("!")
        lines.append("")

    if private_lans:
        seq = 10
        for pfx in private_lans:
            lines.append(f"ip prefix-list PL-OSPF-PRIVATE-LAN seq {seq} permit {pfx}")
            seq += 10
        lines.append("")
        lines.append("route-map RM-OSPF-CONN-PRIVATE permit 10")
        lines.append(" match ip address prefix-list PL-OSPF-PRIVATE-LAN")
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

    # Prefix list for local-originated networks (LAN + private_lan + edge_broadcast)
    local_originated_prefixes: List[str] = []
    local_originated_prefixes.extend(lans)
    if internal_routing == "bgp":
        local_originated_prefixes.extend(private_lans)
    local_originated_prefixes = sorted(set(local_originated_prefixes))
    
    if local_originated_prefixes or bgp_edge_broadcast:
        seq = 10
        for pfx in local_originated_prefixes:
            lines.append(f"ip prefix-list PL-LOCAL-ORIGINATED seq {seq} permit {pfx}")
            seq += 10
        # Add edge broadcast prefixes if this is an exit node
        ovpn = _parse_openvpn(node_id, node)
        wg = _parse_wireguard(node_id, node)
        if _node_is_exit(ovpn, wg):
            for pfx in bgp_edge_broadcast:
                lines.append(f"ip prefix-list PL-LOCAL-ORIGINATED seq {seq} permit {pfx}")
                seq += 10
        lines.append("")

    if private_lans:
        seq = 10
        for pfx in private_lans:
            lines.append(f"ip prefix-list PL-PRIVATE-LAN seq {seq} permit {pfx}")
            seq += 10
        lines.append("")

    # Route maps to prevent private_lan from being advertised to external BGP
    # Private LAN should only stay within the internal network (OSPF)
    if private_lans:
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

    # Route map for OSPF to BGP redistribution: exclude private LAN
    if private_lans:
        lines.append("route-map RM-OSPF-TO-BGP deny 10")
        lines.append(" match ip address prefix-list PL-PRIVATE-LAN")
        lines.append("!")

    lines += [
        "route-map RM-OSPF-TO-BGP deny 20",
        f" match tag {TAG_NO_REINJECT}",
        "!",
        "route-map RM-OSPF-TO-BGP permit 30",
        "!",
        "",
    ]

    lines += ["route-map RM-BGP-TO-OSPF permit 10"]
    if to_ospf_default_only:
        lines.append(" match ip address prefix-list PL-DEFAULT")
    lines += [f" set tag {TAG_NO_REINJECT}", "!", ""]

    if ospf_enable:
        ospf_area = node.get(f"/nodes/{node_id}/ospf/area", "0")
        for iface in active_ifaces:
            lines.append(f"interface {iface}")
            lines.append(f" ip ospf area {ospf_area}")
            lines.append(" ip ospf network broadcast")
            lines.append("!")
        lines.append("router ospf")
        if router_id:
            lines.append(f" ospf router-id {router_id}")
        # Passive all interfaces except active_ifaces
        if active_ifaces:
            lines.append(" passive-interface default")
            for iface in active_ifaces:
                lines.append(f" no passive-interface {iface}")
        # Redistribute connected routes and filter by prefix lists
        # FRR will only advertise routes that are actually in the routing table
        if lans:
            lines.append(" redistribute connected route-map RM-OSPF-CONN")
        if private_lans:
            lines.append(" redistribute connected route-map RM-OSPF-CONN-PRIVATE")
        # Redistribute BGP routes into OSPF (external routes from BGP peers)
        if ospf_redistribute_bgp and bgp_enable:
            lines.append(" redistribute bgp route-map RM-BGP-TO-OSPF")
        lines += ["!", ""]

    if bgp_enable and local_as:
        ovpn = _parse_openvpn(node_id, node)
        wg = _parse_wireguard(node_id, node)
        self_is_exit = _node_is_exit(ovpn, wg)
        
        # Create per-neighbor route-maps for no_transit and no_forward
        neighbor_route_maps: Dict[str, Dict[str, str]] = {}
        for kind, name, cfg, dev in _iter_bgp_transports(ovpn, wg):
            if cfg.get("enable") != "true":
                continue
            if not _bgp_enabled(cfg):
                continue
            peer_ip = cfg.get("bgp/peer_ip", "")
            peer_asn = cfg.get("bgp/peer_asn", "")
            if not peer_ip or not peer_asn:
                continue
            
            no_transit = cfg.get("bgp/no_transit", "false") == "true"
            no_forward = cfg.get("bgp/no_forward", "false") == "true"
            
            # Create unique route-map names for this neighbor if needed
            rm_in = f"RM-BGP-IN"
            rm_out = f"RM-BGP-OUT-EXTERNAL" if private_lans else "RM-BGP-OUT"
            
            # no_forward takes precedence over no_transit (more restrictive)
            if no_forward:
                # Only advertise local-originated routes (not learned from other BGP neighbors)
                rm_out = f"RM-BGP-OUT-{peer_ip.replace('.', '-')}"
                lines.append(f"route-map {rm_out} permit 10")
                lines.append(" match ip address prefix-list PL-LOCAL-ORIGINATED")
                if private_lans:
                    lines.append(" call RM-BGP-OUT-EXTERNAL")
                else:
                    lines.append(" call RM-BGP-OUT")
                lines.append("!")
                lines.append("")
            elif no_transit:
                # no_transit: only send back routes that came from this peer + local-originated routes
                # Allow transit of learned routes to this peer, but only routes learned from this peer
                rm_out = f"RM-BGP-OUT-{peer_ip.replace('.', '-')}"
                lines.append(f"bgp as-path access-list AS-PATH-FROM-{peer_ip.replace('.', '-')} permit _{peer_asn}_")
                lines.append(f"bgp as-path access-list AS-PATH-FROM-{peer_ip.replace('.', '-')} permit ^{peer_asn} ")
                lines.append(f"bgp as-path access-list AS-PATH-FROM-{peer_ip.replace('.', '-')} permit ^{peer_asn}$")
                lines.append("")
                lines.append(f"route-map {rm_out} permit 5")
                lines.append(" match ip address prefix-list PL-LOCAL-ORIGINATED")
                if private_lans:
                    lines.append(" call RM-BGP-OUT-EXTERNAL")
                else:
                    lines.append(" call RM-BGP-OUT")
                lines.append(f"route-map {rm_out} permit 10")
                lines.append(f" match as-path AS-PATH-FROM-{peer_ip.replace('.', '-')}")
                if private_lans:
                    lines.append(" call RM-BGP-OUT-EXTERNAL")
                else:
                    lines.append(" call RM-BGP-OUT")
                lines.append("!")
                lines.append("")
            
            neighbor_route_maps[peer_ip] = {"in": rm_in, "out": rm_out}
        
        lines.append(f"router bgp {local_as}")
        if router_id:
            lines.append(f" bgp router-id {router_id}")

        for kind, name, cfg, dev in _iter_bgp_transports(ovpn, wg):
            if cfg.get("enable") != "true":
                continue
            if not _bgp_enabled(cfg):
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
        # Broadcast edge prefixes if this node is an exit node
        if self_is_exit and bgp_edge_broadcast:
            for pfx in bgp_edge_broadcast:
                lines.append(f"  network {pfx}")
        if ospf_enable:
            lines.append("  redistribute ospf route-map RM-OSPF-TO-BGP")
        for _kind, name, cfg, dev in _iter_bgp_transports(ovpn, wg):
            if cfg.get("enable") != "true":
                continue
            if not _bgp_enabled(cfg):
                continue
            peer_ip = cfg.get("bgp/peer_ip", "")
            peer_asn = cfg.get("bgp/peer_asn", "")
            weight = cfg.get("bgp/weight", "").strip()
            update_source = dev if _kind == "wireguard" else (cfg.get("bgp/update_source", "") or dev)
            if peer_ip and peer_asn and update_source:
                lines.append(f"  neighbor {peer_ip} activate")
                if weight:
                    lines.append(f"  neighbor {peer_ip} weight {weight}")
                # Use per-neighbor route-map if available
                rm_maps = neighbor_route_maps.get(peer_ip, {})
                rm_in = rm_maps.get("in", "RM-BGP-IN")
                rm_out = rm_maps.get("out", "RM-BGP-OUT-EXTERNAL" if private_lans else "RM-BGP-OUT")
                lines.append(f"  neighbor {peer_ip} route-map {rm_in} in")
                lines.append(f"  neighbor {peer_ip} route-map {rm_out} out")
                # Enable BGP transit if peer ASN is in the allowed list or '*' is set
                if bgp_transit_all or (bgp_transit_as_list and peer_asn in bgp_transit_as_list):
                    lines.append(f"  neighbor {peer_ip} next-hop-self")

        for info in ibgp_neighbors:
            peer_ip = info["router_id"]
            lines.append(f"  neighbor {peer_ip} activate")
            lines.append(f"  neighbor {peer_ip} route-map RM-BGP-IN in")
            if private_lans:
                lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT-INTERNAL out")
            else:
                lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT out")
            # Always use next-hop-self for iBGP to ensure proper route propagation
            # This ensures all routes learned from eBGP are properly redistributed within the mesh
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

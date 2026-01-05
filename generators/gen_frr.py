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


def _get_bgp_control_flags(cfg: Dict[str, str]) -> Tuple[bool, bool]:
    """
    Get no_transit and no_forward flags from BGP config.

    Returns:
        (no_transit, no_forward) tuple of booleans

    Behavior:
    - no_transit (True): Only learn routes directly originated by this peer (AS_PATH length = 1).
      Don't learn routes that this peer has learned from other ASes (AS_PATH length > 1).
      Routes learned from this peer are still advertised to iBGP and other eBGP neighbors.
      Example: A - B - C, if C sets no_transit for B, then C will only learn B's own routes,
      not routes that B learned from A. But C will advertise B's routes to other neighbors.

    - no_forward (True): Only advertise locally-originated routes and iBGP routes to this peer.
      Don't advertise routes learned from other eBGP peers.
      Example: A - B - C - D, if C sets no_forward for B, then C will advertise C's own
      routes and iBGP routes to B, but not routes learned from D or other eBGP peers.

    - If both are set, no_forward takes precedence (affects outbound advertisement).
    """
    no_transit = cfg.get("bgp/no_transit", "false").lower() == "true"
    no_forward = cfg.get("bgp/no_forward", "false").lower() == "true"
    return no_transit, no_forward


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

    # Collect neighbors with BGP control flags
    ovpn = _parse_openvpn(node_id, node)
    wg = _parse_wireguard(node_id, node)
    bgp_control_peers: Dict[str, Tuple[bool, bool]] = {}  # peer_ip -> (no_transit, no_forward)
    for kind, name, cfg, dev in _iter_bgp_transports(ovpn, wg):
        if cfg.get("enable") != "true":
            continue
        if not _bgp_enabled(cfg):
            continue
        peer_ip = cfg.get("bgp/peer_ip", "")
        if peer_ip:
            no_transit, no_forward = _get_bgp_control_flags(cfg)
            if no_transit or no_forward:
                bgp_control_peers[peer_ip] = (no_transit, no_forward)

    # Calculate control flag states for later use
    has_no_forward = any(nf for _, (_, nf) in bgp_control_peers.items() if nf)
    has_no_transit = any(nt for _, (nt, _) in bgp_control_peers.items() if nt)

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

    # Generate per-neighbor route-maps for BGP control flags
    # These must be generated after collecting all peer information
    if bgp_control_peers:
        # Use communities to tag routes from eBGP peers (for no_forward filtering)
        COMMUNITY_EBGP_LEARNED = 9999  # Community for routes learned from eBGP peers

        # Check if we need to tag eBGP routes (using pre-calculated value)
        if has_no_forward:
            # Create community for eBGP-learned routes
            lines.append(f"bgp community-list standard EBGP_LEARNED permit {COMMUNITY_EBGP_LEARNED}")
            lines.append("!")

            # Create inbound wrapper that tags all eBGP-learned routes
            # This will be applied to eBGP peers without no_forward/no_transit
            lines.append("route-map RM-BGP-IN-TAG-EBGP permit 10")
            lines.append(" match ip address prefix-list PL-BGP-IN")
            lines.append(f" set community {COMMUNITY_EBGP_LEARNED} additive")
            lines.append("route-map RM-BGP-IN-TAG-EBGP permit 20")
            lines.append("!")
            lines.append("")

        # Generate inbound route-maps for peers with no_transit
        for peer_ip, (no_transit, no_forward) in sorted(bgp_control_peers.items()):
            peer_name = peer_ip.replace(".", "-")

            if no_transit:
                # no_transit: Only accept routes with AS_PATH length <= 2
                # AS_PATH = 1: Peer's own routes
                # AS_PATH = 2: Peer's customer's routes (allow)
                # AS_PATH > 2: Transit routes (deny)
                lines.append(f"route-map RM-BGP-IN-{peer_name} deny 10")
                lines.append(" match ip address prefix-list PL-BGP-IN")
                # Deny routes with AS_PATH length > 2 (transit routes)
                lines.append(" match as-path 1")  # ^.+ .+ .+
                lines.append(f"route-map RM-BGP-IN-{peer_name} permit 20")
                lines.append(" match ip address prefix-list PL-BGP-IN")
                # Allow routes with AS_PATH length <= 2
                lines.append(f"route-map RM-BGP-IN-{peer_name} permit 30")
                lines.append(" ! Allow all other routes")
                lines.append("!")
                lines.append("")

        # Generate AS_PATH filter list if needed (using pre-calculated value)
        if has_no_transit:
            # AS_PATH filter list 1: Match routes with AS_PATH length > 2
            # This indicates transit routes (peer is providing transit)
            lines.append("bgp as-path access-list 1 permit ^.+ .+ .+")  # 3 or more ASNs
            lines.append("!")

        # Generate outbound route-maps for peers with no_forward
        for peer_ip, (no_transit, no_forward) in sorted(bgp_control_peers.items()):
            peer_name = peer_ip.replace(".", "-")

            if no_forward:
                # no_forward: Only advertise locally-originated and iBGP routes
                # Deny routes learned from eBGP peers (tagged with EBGP_LEARNED community)
                lines.append(f"route-map RM-BGP-OUT-{peer_name} deny 10")
                lines.append(" match community EBGP_LEARNED")
                lines.append(f"route-map RM-BGP-OUT-{peer_name} permit 20")
                lines.append(" ! Allow locally-originated and iBGP routes")
                lines.append("!")
                lines.append("")

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
        lines.append(f"router bgp {local_as}")
        if router_id:
            lines.append(f" bgp router-id {router_id}")

        ovpn = _parse_openvpn(node_id, node)
        wg = _parse_wireguard(node_id, node)
        self_is_exit = _node_is_exit(ovpn, wg)
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

                # Determine which inbound route-map to use
                no_transit, no_forward = _get_bgp_control_flags(cfg)
                if no_transit:
                    # Use peer-specific inbound route-map with AS_PATH filtering
                    peer_name = peer_ip.replace(".", "-")
                    lines.append(f"  neighbor {peer_ip} route-map RM-BGP-IN-{peer_name} in")
                elif has_no_forward and not no_forward:
                    # This is a normal eBGP peer - tag routes for no_forward filtering
                    # Use wrapper route-map that tags eBGP routes
                    lines.append(f"  neighbor {peer_ip} route-map RM-BGP-IN-TAG-EBGP in")
                else:
                    # Standard inbound filter
                    lines.append(f"  neighbor {peer_ip} route-map RM-BGP-IN in")

                # Determine which outbound route-map to use based on BGP control flags
                if no_forward:
                    # Use per-neighbor route-map that filters eBGP-learned routes
                    peer_name = peer_ip.replace(".", "-")
                    lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT-{peer_name} out")
                elif no_transit:
                    # no_transit doesn't affect outbound to this peer
                    # Use standard outbound route-map
                    if private_lans:
                        lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT-EXTERNAL out")
                    else:
                        lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT out")
                else:
                    # Normal peer - use standard outbound route-map
                    if private_lans:
                        lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT-EXTERNAL out")
                    else:
                        lines.append(f"  neighbor {peer_ip} route-map RM-BGP-OUT out")

                # Enable BGP transit if peer ASN is in the allowed list or '*' is set
                # But only if no_forward is not set (no_forward disables all transit)
                if (bgp_transit_all or (bgp_transit_as_list and peer_asn in bgp_transit_as_list)) and not no_forward:
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

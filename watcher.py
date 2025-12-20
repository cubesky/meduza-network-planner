import os
import time
import hashlib
import subprocess
import threading
import random
import signal
from datetime import datetime, timezone
from typing import Dict, Any, List, Optional, Tuple

import yaml
import requests
import etcd3
import grpc
from grpc import StatusCode
from urllib.parse import urlparse

NODE_ID = os.environ["NODE_ID"]
TAG_NO_REINJECT = 65000
TPROXY_PORT = 7893

# /updated/<NODE_ID>/...
UPDATE_BASE = f"/updated/{NODE_ID}"
UPDATE_LAST_KEY = f"{UPDATE_BASE}/last"      # persistent timestamp
UPDATE_ONLINE_KEY = f"{UPDATE_BASE}/online"  # TTL key

UPDATE_TTL_SECONDS = int(os.environ.get("UPDATE_TTL_SECONDS", "60"))
OPENVPN_STATUS_INTERVAL = int(os.environ.get("OPENVPN_STATUS_INTERVAL", "10"))


def sha(obj: Any) -> str:
    return hashlib.sha256(repr(obj).encode("utf-8")).hexdigest()


def run(cmd: str) -> None:
    subprocess.run(cmd, shell=True, check=True)


def pspawn(args: List[str]) -> subprocess.Popen:
    return subprocess.Popen(args)


def now_utc_epoch() -> str:
    return str(int(time.time()))


def now_utc_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S+0000")


def _parse_etcd_endpoint(raw: str) -> Dict[str, Any]:
    raw = raw.strip()
    if not raw:
        raise ValueError("empty ETCD_ENDPOINTS entry")
    if "://" not in raw:
        raw = f"https://{raw}"
    u = urlparse(raw)
    if not u.hostname or not u.port:
        raise ValueError(f"invalid ETCD_ENDPOINTS entry: {raw!r}")
    return {"host": u.hostname, "port": u.port}


_first_endpoint = _parse_etcd_endpoint(os.environ["ETCD_ENDPOINTS"].split(",")[0])
_etcd_lock = threading.Lock()
etcd = None


def _new_etcd_client():
    return etcd3.client(
        host=_first_endpoint["host"],
        port=_first_endpoint["port"],
        ca_cert=os.environ["ETCD_CA"],
        cert_cert=os.environ["ETCD_CERT"],
        cert_key=os.environ["ETCD_KEY"],
        user=os.environ["ETCD_USER"],
        password=os.environ["ETCD_PASS"],
        timeout=5,
    )


def _reset_etcd() -> None:
    global etcd
    with _etcd_lock:
        etcd = _new_etcd_client()


def _ensure_etcd() -> None:
    global etcd
    if etcd is None:
        _reset_etcd()


def _etcd_call(fn):
    _ensure_etcd()
    try:
        return fn()
    except grpc.RpcError as e:
        if e.code() == StatusCode.UNAUTHENTICATED:
            _reset_etcd()
            return fn()
        raise


def load_prefix(prefix: str) -> Dict[str, str]:
    out: Dict[str, str] = {}
    for value, meta in _etcd_call(lambda: list(etcd.get_prefix(prefix))):
        key = getattr(meta, "key", None)
        if key is None:
            continue
        out[key.decode("utf-8")] = value.decode("utf-8")
    return out


def load_all_nodes() -> Dict[str, str]:
    return load_prefix("/nodes/")


class Backoff:
    def __init__(self, base=1.0, cap=60.0):
        self.base = base
        self.cap = cap
        self.attempt = 0

    def next_sleep(self) -> float:
        self.attempt += 1
        return random.uniform(0, min(self.cap, self.base * (2 ** self.attempt)))

    def reset(self) -> None:
        self.attempt = 0


# state
last_hash: Dict[str, str] = {}
easytier_proc: Optional[subprocess.Popen] = None
tinc_proc: Optional[subprocess.Popen] = None
openvpn_procs: Dict[str, subprocess.Popen] = {}
tproxy_enabled = False
reconcile_force = False

# online lease
_lease_lock = threading.Lock()
_online_lease: Optional[Any] = None

# OpenVPN status
_ovpn_lock = threading.Lock()
_ovpn_cfg_names: List[str] = []


def ensure_online_lease():
    global _online_lease
    with _lease_lock:
        if _online_lease is None:
            _online_lease = etcd.lease(UPDATE_TTL_SECONDS)
        return _online_lease


def publish_update(reason: str) -> None:
    """Write last timestamp (persistent) and online TTL key."""
    try:
        ts = now_utc_iso()
        _etcd_call(lambda: etcd.put(UPDATE_LAST_KEY, ts))
        lease = ensure_online_lease()
        try:
            _etcd_call(lambda: etcd.put(UPDATE_ONLINE_KEY, "1", lease=lease))
        except grpc.RpcError as e:
            if e.code() == StatusCode.NOT_FOUND:
                with _lease_lock:
                    _online_lease = None
                lease = ensure_online_lease()
                _etcd_call(lambda: etcd.put(UPDATE_ONLINE_KEY, "1", lease=lease))
            else:
                raise
        print(f"[updated] {reason} last={ts} ttl={UPDATE_TTL_SECONDS}s", flush=True)
    except Exception as e:
        with _lease_lock:
            _online_lease = None
        print(f"[updated] failed: {e}", flush=True)


def keepalive_loop():
    interval = max(5, UPDATE_TTL_SECONDS // 3)
    while True:
        time.sleep(interval)
        try:
                with _lease_lock:
                    lease = _online_lease
                if lease:
                    try:
                        lease.refresh()
                    except grpc.RpcError as e:
                        if e.code() in (StatusCode.UNAUTHENTICATED, StatusCode.NOT_FOUND):
                            _reset_etcd()
                            with _lease_lock:
                                _online_lease = None
                        else:
                            raise
        except Exception:
            with _lease_lock:
                _online_lease = None


def sigusr1_handler(signum, frame):
    global reconcile_force
    reconcile_force = True
    print("[signal] SIGUSR1 force reconcile", flush=True)


signal.signal(signal.SIGUSR1, sigusr1_handler)

# ---------- EasyTier (NO legacy compat) ----------

def _split_ml(val: str) -> List[str]:
    if not val:
        return []
    return [x.strip() for x in val.replace("\r\n", "\n").replace("\r", "\n").split("\n") if x.strip()]


def reload_easytier(domain: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    global easytier_proc
    run("pkill easytier-core || true; pkill easytier || true")

    # Node-level knobs
    def ng(k, d=None):
        return domain.get(f"/nodes/{NODE_ID}/easytier/{k}", d)

    # Global network identity (must be consistent across nodes)
    def gg(k, d=None):
        return global_cfg.get(f"/global/easytier/{k}", d)

    network_name = gg("network_name", "")
    network_secret = gg("network_secret", "")
    if not network_name or not network_secret:
        raise RuntimeError("missing /global/easytier/network_name or /global/easytier/network_secret")

    args = [
        "easytier-core",
        "--network-name", network_name,
        "--network-secret", network_secret,
        "--dev-name", ng("dev_name", "et0"),
    ]

    # Global private_mode (default false)
    if gg("private_mode", "false") == "true":
        args.append("--private-mode=true")

    # Node ipv4 is still node-specific (per-node address within overlay)
    ipv4 = ng("ipv4", "")
    if ipv4:
        args += ["--ipv4", ipv4]

    # Global DHCP enable (default false)
    if gg("dhcp", "false") == "true":
        args.append("--dhcp")

    # Node-level mapped-listeners: manually specify public address of listener(s)
    # See EasyTier docs: --mapped-listeners tcp://PUBLIC_IP:PUBLIC_PORT with -l tcp://0.0.0.0:LOCAL_PORT
    for v in _split_ml(ng("mapped_listeners", "")):
        args += ["--mapped-listeners", v]

    # Strict schema: newline-separated single keys (node-level)
    for v in _split_ml(ng("listeners", "")):
        args += ["-l", v]
    for v in _split_ml(ng("peers", "")):
        args += ["-p", v]

    easytier_proc = pspawn(args)


# ---------- Tinc (switch mode) ----------
def _parse_tinc_nodes(nodes: Dict[str, str]) -> Dict[str, Dict[str, str]]:
    base = "/nodes/"
    out: Dict[str, Dict[str, str]] = {}
    for k, v in nodes.items():
        if not k.startswith(base):
            continue
        rest = k[len(base):]
        if "/tinc/" not in rest:
            continue
        node_id, tail = rest.split("/", 1)
        if not tail.startswith("tinc/"):
            continue
        key = tail[len("tinc/"):]
        out.setdefault(node_id, {})
        out[node_id][key] = v
    return out


def _write_text(path: str, text: str, mode: Optional[int] = None) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    if mode is not None:
        os.chmod(path, mode)


def _normalize_tinc_pubkey(pubkey: str, ed25519: str) -> str:
    lines = []
    for line in pubkey.splitlines():
        s = line.strip()
        if not s:
            continue
        lines.append(s)
    ed = ed25519.strip()
    if ed:
        if not ed.lower().startswith("ed25519publickey"):
            lines.append(f"Ed25519PublicKey = {ed}")
        else:
            lines.append(ed)
    return "\n".join(lines)


def _write_tinc_host(
    netname: str,
    name: str,
    address: str,
    port: str,
    subnets: List[str],
    mode: str,
    cipher: str,
    digest: str,
    pubkey: str,
    ed25519: str,
) -> None:
    lines = []
    if address:
        lines.append(f"Address={address}")
    if mode:
        lines.append(f"Mode={mode}")
    if port:
        lines.append(f"Port={port}")
    if cipher:
        lines.append(f"Cipher={cipher}")
    if digest:
        lines.append(f"Digest={digest}")
    for s in subnets:
        lines.append(f"Subnet={s}")
    key_text = _normalize_tinc_pubkey(pubkey, ed25519)
    host_text = "\n".join(lines + ["", key_text, ""])
    _write_text(f"/etc/tinc/{netname}/hosts/{name}", host_text, mode=0o644)


def reload_tinc(node: Dict[str, str], all_nodes: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    global tinc_proc
    run("pkill tincd || true")

    netname = global_cfg.get("/global/tinc/netname", "mesh")
    if not netname:
        raise RuntimeError("missing /global/tinc/netname")
    name = node.get(f"/nodes/{NODE_ID}/tinc/name", NODE_ID)
    dev_name = node.get(f"/nodes/{NODE_ID}/tinc/dev_name", "tnc0")
    port = node.get(f"/nodes/{NODE_ID}/tinc/port", "655")
    address = node.get(f"/nodes/{NODE_ID}/tinc/address", "")
    address_family = node.get(f"/nodes/{NODE_ID}/tinc/address_family", "ipv4")
    ipv4 = node.get(f"/nodes/{NODE_ID}/tinc/ipv4", "")
    subnet = node.get(f"/nodes/{NODE_ID}/tinc/subnet", "")
    host_mode = node.get(f"/nodes/{NODE_ID}/tinc/host_mode", "")
    host_cipher = node.get(f"/nodes/{NODE_ID}/tinc/host_cipher", "")
    host_digest = node.get(f"/nodes/{NODE_ID}/tinc/host_digest", "")
    conf_mode = node.get(f"/nodes/{NODE_ID}/tinc/mode", "Switch")
    conf_cipher = global_cfg.get("/global/tinc/cipher", "")
    conf_digest = global_cfg.get("/global/tinc/digest", "")
    pubkey = node.get(f"/nodes/{NODE_ID}/tinc/public_key", "")
    ed25519 = node.get(f"/nodes/{NODE_ID}/tinc/ed25519_public_key", "")
    privkey = node.get(f"/nodes/{NODE_ID}/tinc/private_key", "")
    ed25519_priv = node.get(f"/nodes/{NODE_ID}/tinc/ed25519_private_key", "")

    if not (pubkey or ed25519):
        raise RuntimeError("missing /nodes/<NODE_ID>/tinc/public_key or /nodes/<NODE_ID>/tinc/ed25519_public_key")
    if not (privkey or ed25519_priv):
        raise RuntimeError("missing /nodes/<NODE_ID>/tinc/private_key or /nodes/<NODE_ID>/tinc/ed25519_private_key")

    base_dir = f"/etc/tinc/{netname}"
    os.makedirs(base_dir, exist_ok=True)
    hosts_dir = f"{base_dir}/hosts"
    os.makedirs(hosts_dir, exist_ok=True)
    for f in os.listdir(hosts_dir):
        try:
            os.remove(os.path.join(hosts_dir, f))
        except Exception:
            pass

    nodes = _parse_tinc_nodes(all_nodes)
    connect_to: List[str] = []
    for peer_id, cfg in nodes.items():
        if cfg.get("enable") != "true":
            continue
        peer_name = cfg.get("name", peer_id)
        if peer_name == name:
            continue
        peer_addr = cfg.get("address", "")
        peer_port = cfg.get("port", "")
        peer_subnet = cfg.get("subnet", "")
        peer_host_mode = cfg.get("host_mode", "")
        peer_host_cipher = cfg.get("host_cipher", "")
        peer_host_digest = cfg.get("host_digest", "")
        peer_pub = cfg.get("public_key", "")
        peer_ed25519 = cfg.get("ed25519_public_key", "")
        if not (peer_pub or peer_ed25519):
            continue
        _write_tinc_host(
            netname,
            peer_name,
            peer_addr,
            peer_port,
            _split_ml(peer_subnet),
            peer_host_mode,
            peer_host_cipher,
            peer_host_digest,
            peer_pub,
            peer_ed25519,
        )
        if peer_addr:
            connect_to.append(peer_name)

    _write_tinc_host(
        netname,
        name,
        address,
        port,
        _split_ml(subnet),
        host_mode,
        host_cipher,
        host_digest,
        pubkey,
        ed25519,
    )
    if privkey.strip():
        _write_text(f"/etc/tinc/{netname}/rsa_key.priv", f"{privkey.strip()}\n", mode=0o600)
    if ed25519_priv.strip():
        _write_text(f"/etc/tinc/{netname}/ed25519_key.priv", f"{ed25519_priv.strip()}\n", mode=0o600)

    tinc_conf = [
        f"Name={name}",
        f"AddressFamily={address_family}",
        f"Mode={conf_mode}",
        "DeviceType=tap",
        f"Interface={dev_name}",
        f"Port={port}",
    ]
    if conf_cipher:
        tinc_conf.append(f"Cipher={conf_cipher}")
    if conf_digest:
        tinc_conf.append(f"Digest={conf_digest}")
    for peer in sorted(set(connect_to)):
        tinc_conf.append(f"ConnectTo = {peer}")
    _write_text(f"/etc/tinc/{netname}/tinc.conf", "\n".join(tinc_conf) + "\n", mode=0o644)

    tinc_up = [
        "#!/bin/sh",
        "set -e",
        "ip link set \"$INTERFACE\" up",
    ]
    if ipv4:
        tinc_up.append(f"ip addr add {ipv4} dev \"$INTERFACE\" || true")
    _write_text(f"/etc/tinc/{netname}/tinc-up", "\n".join(tinc_up) + "\n", mode=0o755)

    tinc_down = [
        "#!/bin/sh",
        "set -e",
    ]
    if ipv4:
        tinc_down.append(f"ip addr del {ipv4} dev \"$INTERFACE\" || true")
    _write_text(f"/etc/tinc/{netname}/tinc-down", "\n".join(tinc_down) + "\n", mode=0o755)

    tinc_proc = pspawn(["tincd", "-c", "/etc/tinc", "-n", netname, "-D", "--pidfile", "/run/tincd.pid"])

# ---------- OpenVPN (new schema: /nodes/<ID>/openvpn/<name>/...) ----------

def parse_openvpn(node: Dict[str, str]) -> Dict[str, Dict[str, str]]:
    base = f"/nodes/{NODE_ID}/openvpn/"
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


def _ovpn_dev_name(name: str) -> str:
    # Keep existing convention: name ends with digit => tun<digit>, else tun-<name>
    return f"tun{name[-1]}" if name and name[-1].isdigit() else f"tun-{name}"


def _ovpn_status_key(name: str) -> str:
    return f"{UPDATE_BASE}/openvpn/{name}/status"


def _iface_exists(dev: str) -> bool:
    try:
        subprocess.run(f"ip link show dev {dev} >/dev/null 2>&1", shell=True, check=True)
        return True
    except subprocess.CalledProcessError:
        return False


def _proc_alive(p: subprocess.Popen) -> bool:
    return p.poll() is None


def _compute_openvpn_status(name: str, proc: Optional[subprocess.Popen]) -> str:
    dev = _ovpn_dev_name(name)
    if proc is None:
        return "down"
    if not _proc_alive(proc):
        return "down"
    if _iface_exists(dev):
        return "up"
    return "connecting"


def _write_openvpn_status(name: str, status: str) -> None:
    # Best-effort. Value format: "<status> <utc_epoch>"
    try:
        _etcd_call(lambda: etcd.put(_ovpn_status_key(name), f"{status} {now_utc_epoch()}"))
    except Exception as e:
        print(f"[openvpn-status] failed to write {name}: {e}", flush=True)


def openvpn_start(name: str, cfg: str) -> subprocess.Popen:
    p = f"/etc/openvpn/generated/{name}.conf"
    with open(p, "w", encoding="utf-8") as f:
        f.write(cfg)
    dev = _ovpn_dev_name(name)
    # Use --dev, config may also specify dev; we enforce by CLI for predictability
    return pspawn(["openvpn", "--config", p, "--dev", dev])


def reload_openvpn(node: Dict[str, str]) -> Tuple[bool, List[str]]:
    ovpn = parse_openvpn(node)
    enabled: List[str] = []
    changed = False

    with _ovpn_lock:
        # cache names for status loop (only those enabled)
        _ovpn_cfg_names.clear()

    active = set()
    for name, cfg in ovpn.items():
        if cfg.get("enable") != "true":
            continue
        if "config" not in cfg:
            continue
        active.add(name)
        enabled.append(name)

    with _ovpn_lock:
        _ovpn_cfg_names.extend(sorted(enabled))

    # start
    for name in enabled:
        cfg = ovpn[name]["config"]
        if name not in openvpn_procs:
            openvpn_procs[name] = openvpn_start(name, cfg)
            changed = True
            _write_openvpn_status(name, "connecting")

    # stop removed/disabled
    for name in list(openvpn_procs.keys()):
        if name not in active:
            try:
                openvpn_procs[name].terminate()
            except Exception:
                pass
            openvpn_procs.pop(name, None)
            changed = True
            _write_openvpn_status(name, "down")

    return changed, sorted(enabled)


def openvpn_status_loop():
    while True:
        time.sleep(max(3, OPENVPN_STATUS_INTERVAL))
        with _ovpn_lock:
            names = list(_ovpn_cfg_names)
        for name in names:
            proc = openvpn_procs.get(name)
            status = _compute_openvpn_status(name, proc)
            _write_openvpn_status(name, status)


# ---------- LANs ----------

def node_lans(node: Dict[str, str]) -> List[str]:
    base = f"/nodes/{NODE_ID}/lan/"
    base_key = base.rstrip("/")
    raw = node.get(base_key, "")
    return sorted(set(_split_ml(raw)))


# ---------- FRR smooth reload ----------

def _parse_prefix_list_rules(multiline: str) -> List[Tuple[str, str]]:
    """Parse lines like: 'permit 10.0.0.0/8 le 32' or 'deny 0.0.0.0/0'."""
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


def generate_frr(node: Dict[str, str], global_cfg: Dict[str, str]) -> str:
    router_id = node.get(f"/nodes/{NODE_ID}/router_id", "")
    ospf_enable = node.get(f"/nodes/{NODE_ID}/ospf/enable") == "true"
    bgp_enable = node.get(f"/nodes/{NODE_ID}/bgp/enable") == "true"

    local_as = node.get(f"/nodes/{NODE_ID}/bgp/local_asn", "")
    max_paths = node.get(f"/nodes/{NODE_ID}/bgp/max_paths", "1")
    to_ospf_default_only = node.get(f"/nodes/{NODE_ID}/bgp/to_ospf/default_only") == "true"
    ospf_redistribute_bgp = node.get(f"/nodes/{NODE_ID}/ospf/redistribute_bgp") == "true"
    inject_site_lan = node.get(f"/nodes/{NODE_ID}/ospf/inject_site_lan") == "true"

    # Global shared BGP filter rules (applied to ALL neighbors)
    # Keys (multiline, supports \n/\r/\r\n):
    #   /global/bgp/filter/in
    #   /global/bgp/filter/out
    # Line format:
    #   permit <prefix> [ge N] [le N]
    #   deny   <prefix> [ge N] [le N]
    # If missing:
    #   IN  defaults to: deny 0.0.0.0/0 ; permit 0.0.0.0/0 le 32
    #   OUT defaults to: permit 0.0.0.0/0 le 32
    in_rules_ml = global_cfg.get("/global/bgp/filter/in", "")
    out_rules_ml = global_cfg.get("/global/bgp/filter/out", "")

    in_rules = _parse_prefix_list_rules(in_rules_ml) if in_rules_ml else [
        ("deny", "0.0.0.0/0"),
        ("permit", "0.0.0.0/0 le 32"),
    ]
    out_rules = _parse_prefix_list_rules(out_rules_ml) if out_rules_ml else [
        ("permit", "0.0.0.0/0 le 32"),
    ]

    active_key = f"/nodes/{NODE_ID}/ospf/active_ifaces"
    if active_key in node:
        active_ifaces = sorted(set(_split_ml(node.get(active_key, ""))))
    else:
        active_ifaces = sorted({
            k.split("/")[-1]
            for k in node
            if k.startswith(f"/nodes/{NODE_ID}/ospf/active_ifaces/")
        })

    lans = node_lans(node)

    lines: List[str] = [
        "frr defaults traditional",
        "service integrated-vtysh-config",
        f"hostname {NODE_ID}",
    ]
    if router_id:
        lines.append(f"ip router-id {router_id}")

    # Prefix-lists and route-maps
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

    # Shared inbound policy
    seq = 10
    for action, rest in in_rules:
        lines.append(f"ip prefix-list PL-BGP-IN seq {seq} {action} {rest}")
        seq += 10
    lines.append("")
    lines.append("route-map RM-BGP-IN permit 10")
    lines.append(" match ip address prefix-list PL-BGP-IN")
    lines.append("!")
    lines.append("")

    # Shared outbound policy
    seq = 10
    for action, rest in out_rules:
        lines.append(f"ip prefix-list PL-BGP-OUT seq {seq} {action} {rest}")
        seq += 10
    lines.append("route-map RM-BGP-OUT permit 10")
    lines.append(" match ip address prefix-list PL-BGP-OUT")
    lines.append("!")
    lines.append("")

    # OSPF/BGP controlled redistribute (tag-based anti-loop)
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
        ospf_area = node.get(f"/nodes/{NODE_ID}/ospf/area", "0")
        for i in active_ifaces:
            lines.append(f"interface {i}")
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

        # OpenVPN neighbors for BGP transport
        ovpn = parse_openvpn(node)
        for name, cfg in ovpn.items():
            if cfg.get("enable") != "true":
                continue
            peer_ip = cfg.get("bgp/peer_ip", "")
            peer_asn = cfg.get("bgp/peer_asn", "")
            update_source = cfg.get("bgp/update_source", "")
            if peer_ip and peer_asn and update_source:
                lines.append(f" neighbor {peer_ip} remote-as {peer_asn}")
                lines.append(f" neighbor {peer_ip} update-source {update_source}")
                # Per-neighbor policy required by FRR: shared route-maps
                lines.append(f" neighbor {peer_ip} route-map RM-BGP-IN in")
                lines.append(f" neighbor {peer_ip} route-map RM-BGP-OUT out")

        lines += ["!", ""]

    return "\n".join(lines).strip() + "\n"


def _find_frr_reload() -> Optional[str]:
    for c in ["/usr/lib/frr/frr-reload.py", "/usr/lib/frr/frr-reload", "/usr/sbin/frr-reload.py", "/usr/sbin/frr-reload"]:
        if os.path.exists(c):
            return c
    return None


def reload_frr_smooth(conf_text: str) -> None:
    conf_path = "/etc/frr/frr.conf"
    new_path = "/etc/frr/frr.conf.new"
    with open(new_path, "w", encoding="utf-8") as f:
        f.write(conf_text)

    tool = _find_frr_reload()
    if tool:
        try:
            if tool.endswith(".py"):
                subprocess.run(["python3", tool, "--reload", new_path], check=True)
            else:
                subprocess.run([tool, "--reload", new_path], check=True)
            os.replace(new_path, conf_path)
            return
        except Exception as e:
            print(f"[frr] smooth reload failed, fallback to vtysh: {e}", flush=True)

    os.replace(new_path, conf_path)
    run("vtysh -f /etc/frr/frr.conf")


# ---------- Clash ----------

def clash_pid() -> int:
    try:
        return int(open("/run/clash/mihomo.pid", encoding="utf-8").read().strip())
    except Exception:
        return int(subprocess.check_output("pidof mihomo", shell=True).decode().split()[0])


def reload_clash(conf: Dict[str, Any]) -> None:
    with open("/etc/clash/config.yaml", "w", encoding="utf-8") as f:
        yaml.safe_dump(conf, f, sort_keys=False, allow_unicode=True)
    run(f"kill -HUP {clash_pid()}")


def generate_clash(node: Dict[str, str], global_cfg: Dict[str, str]) -> Dict[str, Any]:
    base = yaml.safe_load(open("/clash/base.yaml", encoding="utf-8")) or {}
    mode = node.get(f"/nodes/{NODE_ID}/clash/mode", "mixed")

    # Subscriptions are global to keep consistent updates across nodes
    subs: Dict[str, str] = {}
    for k, v in global_cfg.items():
        if k.startswith("/global/clash/subscriptions/") and k.endswith("/url"):
            name = k.split("/global/clash/subscriptions/")[1].split("/url")[0]
            subs[name] = v

    active = node.get(f"/nodes/{NODE_ID}/clash/active_subscription")
    if not active:
        raise RuntimeError("missing /nodes/<NODE_ID>/clash/active_subscription")
    if active not in subs:
        raise RuntimeError(f"active_subscription {active!r} not found under /global/clash/subscriptions/")

    resp = requests.get(subs[active], timeout=15)
    resp.raise_for_status()
    sub_conf = yaml.safe_load(resp.text) or {}

    merged = dict(base)
    merged.update(sub_conf)

    if mode == "mixed":
        merged["mixed-port"] = 7890
    elif mode == "tproxy":
        merged["tproxy-port"] = TPROXY_PORT
        merged["tun"] = {
            "enable": True,
            "stack": "system",
            "auto-route": True,
            "auto-detect-interface": True,
        }
    else:
        raise RuntimeError(f"unsupported clash mode: {mode}")

    return merged


def node_lans_for_exclude(node: Dict[str, str]) -> List[str]:
    cidrs = [
        "127.0.0.0/8", "0.0.0.0/8", "10.0.0.0/8",
        "172.16.0.0/12", "192.168.0.0/16",
        "169.254.0.0/16", "224.0.0.0/4", "240.0.0.0/4",
        "10.42.1.0/24",
    ]
    cidrs.extend(node_lans(node))
    return sorted(set(cidrs))


def tproxy_apply(exclude: List[str]) -> None:
    run(
        f"EXCLUDE_CIDRS='{ ' '.join(exclude) }' "
        f"TPROXY_PORT={TPROXY_PORT} MARK=0x1 TABLE=100 "
        f"/usr/local/bin/tproxy.sh apply"
    )


def tproxy_remove() -> None:
    run(f"TPROXY_PORT={TPROXY_PORT} MARK=0x1 TABLE=100 /usr/local/bin/tproxy.sh remove")


# ---------- reconcile ----------

def handle_commit() -> None:
    global reconcile_force, tproxy_enabled

    node = load_prefix(f"/nodes/{NODE_ID}/")
    global_cfg = load_prefix("/global/")

    def changed(key: str, val: Any) -> bool:
        if reconcile_force:
            return True
        h = sha(val)
        if last_hash.get(key) != h:
            last_hash[key] = h
            return True
        return False

    did_apply = False

    mesh_type = global_cfg.get("/global/mesh_type", "easytier")
    if mesh_type == "tinc":
        all_nodes = load_all_nodes()
        tinc_domain = {k: v for k, v in all_nodes.items() if "/tinc/" in k}
        global_tinc = {k: v for k, v in global_cfg.items() if k == "/global/mesh_type" or k.startswith("/global/tinc/")}
        if changed("tinc", {"nodes": tinc_domain, "global": global_tinc}):
            run("pkill easytier-core || true; pkill easytier || true")
            if node.get(f"/nodes/{NODE_ID}/tinc/enable") == "true":
                reload_tinc(node, all_nodes, global_cfg)
            else:
                run("pkill tincd || true")
            did_apply = True
    else:
        easytier_domain = {k: v for k, v in node.items() if "/easytier/" in k}
        if changed("easytier", easytier_domain):
            run("pkill tincd || true")
            if node.get(f"/nodes/{NODE_ID}/easytier/enable") == "true":
                reload_easytier(easytier_domain, global_cfg)
            else:
                run("pkill easytier-core || true; pkill easytier || true")
            did_apply = True

    openvpn_domain = {k: v for k, v in node.items() if "/openvpn/" in k}
    if changed("openvpn", openvpn_domain):
        changed_ovpn, enabled = reload_openvpn(node)
        did_apply = did_apply or changed_ovpn
        for name in enabled:
            _write_openvpn_status(name, _compute_openvpn_status(name, openvpn_procs.get(name)))

    # FRR depends on node routing config + global BGP filter policy
    frr_material = {k: v for k, v in node.items() if (
        "/ospf/" in k or "/bgp/" in k or "/lan/" in k or "/openvpn/" in k
    )}
    global_bgp_filter = {k: v for k, v in global_cfg.items() if k.startswith("/global/bgp/filter/")}
    if changed("frr", {"node": frr_material, "global_bgp_filter": global_bgp_filter}):
        reload_frr_smooth(generate_frr(node, global_cfg))
        did_apply = True

    clash_domain = {k: v for k, v in node.items() if "/clash/" in k}
    if changed("clash", clash_domain):
        if node.get(f"/nodes/{NODE_ID}/clash/enable") != "true":
            try:
                tproxy_remove()
            except Exception:
                pass
            tproxy_enabled = False
        else:
            reload_clash(generate_clash(node, global_cfg))
            mode = node.get(f"/nodes/{NODE_ID}/clash/mode", "mixed")
            if mode == "tproxy":
                tproxy_apply(node_lans_for_exclude(node))
                tproxy_enabled = True
            else:
                if tproxy_enabled:
                    tproxy_remove()
                    tproxy_enabled = False
        did_apply = True

    reconcile_force = False

    if did_apply:
        publish_update("config-applied")


# ---------- watch loop ----------

def watch_loop() -> None:
    backoff = Backoff()
    while True:
        cancel = None
        try:
            try:
                handle_commit()
            except Exception as e:
                print(f"[reconcile] error: {e}", flush=True)

            backoff.reset()
            events, cancel = _etcd_call(lambda: etcd.watch("/commit"))
            for _ in events:
                try:
                    handle_commit()
                except Exception as e:
                    print(f"[reconcile] error: {e}", flush=True)

        except Exception as e:
            t = backoff.next_sleep()
            print(f"[watch] error: {e}; retry in {t:.1f}s", flush=True)
            time.sleep(t)
        finally:
            try:
                if cancel:
                    cancel()
            except Exception:
                pass


def main() -> None:
    threading.Thread(target=keepalive_loop, daemon=True).start()
    threading.Thread(target=openvpn_status_loop, daemon=True).start()

    publish_update("startup")
    watch_loop()


if __name__ == "__main__":
    main()

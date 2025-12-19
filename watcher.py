import os
import time
import hashlib
import subprocess
import threading
import random
import signal
from typing import Dict, Any, List, Optional, Tuple

import yaml
import requests
import etcd3

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


etcd = etcd3.client(
    endpoints=os.environ["ETCD_ENDPOINTS"].split(","),
    ca_cert=os.environ["ETCD_CA"],
    cert_cert=os.environ["ETCD_CERT"],
    cert_key=os.environ["ETCD_KEY"],
    user=os.environ["ETCD_USER"],
    password=os.environ["ETCD_PASS"],
    timeout=5,
)


def load_prefix(prefix: str) -> Dict[str, str]:
    out: Dict[str, str] = {}
    for k, v in etcd.get_prefix(prefix):
        out[k.decode("utf-8")] = v.decode("utf-8")
    return out


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
        ts = now_utc_epoch()
        etcd.put(UPDATE_LAST_KEY, ts)
        lease = ensure_online_lease()
        etcd.put(UPDATE_ONLINE_KEY, "1", lease=lease)
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
                lease.refresh()
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
        args.append("--private-mode")

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
        etcd.put(_ovpn_status_key(name), f"{status} {now_utc_epoch()}")
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
    return sorted({k[len(base):].replace("_", "/") for k in node if k.startswith(base)})


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
        lines.append("router ospf")
        if router_id:
            lines.append(f" ospf router-id {router_id}")
        lines.append(" passive-interface default")
        for i in active_ifaces:
            lines.append(f" no passive-interface {i}")
        if inject_site_lan:
            lines.append(" redistribute connected")
        if ospf_redistribute_bgp:
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

    easytier_domain = {k: v for k, v in node.items() if "/easytier/" in k}
    if changed("easytier", easytier_domain):
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
            events, cancel = etcd.watch("/commit")
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

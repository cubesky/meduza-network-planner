import os
import time
import hashlib
import glob
import subprocess
import json
import shutil
import threading
import random
import signal
from datetime import datetime, timezone
from typing import Dict, Any, List, Optional, Tuple, Set

import etcd3
import requests
import grpc
from grpc import StatusCode
from urllib.parse import urlparse

NODE_ID = os.environ["NODE_ID"]
TPROXY_PORT = 7893
MOSDNS_SOCKS_PORT = 7891
CLASH_HTTP_PORT = 7890
CLASH_API_PORT = 9090
CLASH_API_SECRET = ""
GEN_DIR = "/generators"

# IPSet for proxy server exclusions
PROXY_IPSET_NAME = "clash_proxy_ips"

# /updated/<NODE_ID>/...
UPDATE_BASE = f"/updated/{NODE_ID}"
UPDATE_LAST_KEY = f"{UPDATE_BASE}/last"      # persistent timestamp
UPDATE_ONLINE_KEY = f"{UPDATE_BASE}/online"  # TTL key

# etcd_hosts
ETCD_HOSTS_PATH = "/etc/etcd_hosts"
ETCD_HOSTS_PREFIX = "/dns/hosts"

UPDATE_TTL_SECONDS = int(os.environ.get("UPDATE_TTL_SECONDS", "60"))
OPENVPN_STATUS_INTERVAL = int(os.environ.get("OPENVPN_STATUS_INTERVAL", "10"))
WIREGUARD_STATUS_INTERVAL = int(os.environ.get("WIREGUARD_STATUS_INTERVAL", "10"))
SUPERVISOR_RETRY_INTERVAL = int(os.environ.get("SUPERVISOR_RETRY_INTERVAL", "30"))


def sha(obj: Any) -> str:
    return hashlib.sha256(repr(obj).encode("utf-8")).hexdigest()


def run(cmd: str) -> None:
    subprocess.run(cmd, shell=True, check=True)


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


def load_key(key: str) -> str:
    value, _meta = _etcd_call(lambda: etcd.get(key))
    if value is None:
        return ""
    return value.decode("utf-8")


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
tproxy_enabled = False
reconcile_force = False

# Clash refresh state
_clash_refresh_lock = threading.Lock()
_clash_refresh_enable = False
_clash_refresh_interval = 0
_clash_refresh_next = 0.0

# tproxy iptables check state
_tproxy_check_lock = threading.Lock()
_tproxy_check_enabled = False
_tproxy_check_interval = 60  # 1 minute default
_cached_tproxy_targets: List[str] = []

# Mihomo crash monitoring
_clash_monitoring_lock = threading.Lock()
_clash_last_healthy = 0.0
_clash_monitoring_enabled = False

# Proxy IP extraction and ipset management
_proxy_ips_lock = threading.Lock()
_cached_proxy_ips: Set[str] = set()
_proxy_ips_enabled = False

# reconcile lock
_reconcile_lock = threading.Lock()

# online lease
_lease_lock = threading.Lock()
_online_lease: Optional[Any] = None

# OpenVPN status
_ovpn_lock = threading.Lock()
_ovpn_cfg_names: List[str] = []
_ovpn_devs: Dict[str, str] = {}

# WireGuard status
_wg_lock = threading.Lock()
_wg_cfg_names: List[str] = []
_wg_devs: Dict[str, str] = {}


def ensure_online_lease():
    global _online_lease
    with _lease_lock:
        if _online_lease is None:
            _online_lease = _etcd_call(lambda: etcd.lease(UPDATE_TTL_SECONDS))
        return _online_lease


def publish_update(reason: str) -> None:
    """Write last timestamp (persistent) and online TTL key."""
    try:
        ts = now_utc_iso()
        _etcd_call(lambda: etcd.put(UPDATE_LAST_KEY, ts))
        for _ in range(2):
            try:
                lease = ensure_online_lease()
                _etcd_call(lambda: etcd.put(UPDATE_ONLINE_KEY, "1", lease=lease))
                break
            except grpc.RpcError as e:
                if e.code() in (StatusCode.NOT_FOUND, StatusCode.UNAUTHENTICATED):
                    with _lease_lock:
                        _online_lease = None
                    _reset_etcd()
                    continue
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
            lease = _etcd_call(lambda: etcd.lease(UPDATE_TTL_SECONDS))
            _etcd_call(lambda: etcd.put(UPDATE_ONLINE_KEY, "1", lease=lease))
            with _lease_lock:
                _online_lease = lease
        except Exception:
            with _lease_lock:
                _online_lease = None


def sigusr1_handler(signum, frame):
    global reconcile_force
    reconcile_force = True
    print("[signal] SIGUSR1 force reconcile", flush=True)


signal.signal(signal.SIGUSR1, sigusr1_handler)

# ---------- EasyTier (NO legacy compat) ----------
def reload_easytier(node: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": {}}
    out = _run_generator("gen_easytier", payload)
    os.makedirs("/etc/easytier", exist_ok=True)
    _write_if_changed("/etc/easytier/config.yaml", out["config_text"], mode=0o644)
    _write_if_changed("/run/easytier/args", "\n".join(out["args"]) + "\n", mode=0o644)
    if _supervisor_is_running("easytier"):
        if not _easytier_cli_reload():
            _supervisor_restart("easytier")
    else:
        _supervisor_start("easytier")


# ---------- Tinc (switch mode) ----------
def _write_text(path: str, text: str, mode: Optional[int] = None) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    if mode is not None:
        os.chmod(path, mode)


def _read_text(path: str) -> str:
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def _write_if_changed(path: str, text: str, mode: Optional[int] = None) -> bool:
    try:
        if _read_text(path) == text:
            return False
    except FileNotFoundError:
        pass
    _write_text(path, text, mode=mode)
    return True


def _run_generator(name: str, payload: Dict[str, Any]) -> Dict[str, Any]:
    cmd = ["python3", f"{GEN_DIR}/{name}.py"]
    cp = subprocess.run(
        cmd,
        input=json.dumps(payload),
        text=True,
        capture_output=True,
    )
    if cp.returncode != 0:
        raise RuntimeError(f"generator {name} failed: {cp.stderr.strip() or cp.stdout.strip()}")
    try:
        return json.loads(cp.stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"generator {name} invalid JSON: {e}") from e


def reload_tinc(node: Dict[str, str], all_nodes: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    payload = {"node_id": NODE_ID, "node": node, "all_nodes": all_nodes, "global": global_cfg}
    out = _run_generator("gen_tinc", payload)
    netname = out["netname"]
    hosts_dir = f"/etc/tinc/{netname}/hosts"
    expected_hosts: List[str] = []
    changed_non_host = False
    changed_host_existing = False
    new_host_files = False
    removed_hosts = False
    for entry in out["files"]:
        path = entry["path"]
        is_host = path.startswith(f"{hosts_dir}/")
        if is_host:
            expected_hosts.append(os.path.basename(path))
        existed = os.path.exists(path)
        if _write_if_changed(path, entry["content"], mode=entry.get("mode")):
            if is_host:
                if existed:
                    changed_host_existing = True
                else:
                    new_host_files = True
            else:
                changed_non_host = True
    if os.path.isdir(hosts_dir):
        for fname in os.listdir(hosts_dir):
            if fname not in expected_hosts:
                try:
                    os.remove(os.path.join(hosts_dir, fname))
                    removed_hosts = True
                except Exception:
                    pass
    if _supervisor_is_running("tinc"):
        if changed_non_host or changed_host_existing or removed_hosts:
            _supervisor_restart("tinc")
        elif new_host_files:
            if not _tinc_reload(netname):
                _supervisor_restart("tinc")
    else:
        _supervisor_start("tinc")

# ---------- OpenVPN (supervisord-managed) ----------

def _ovpn_status_key(name: str) -> str:
    return f"{UPDATE_BASE}/openvpn/{name}/status"


def _iface_exists(dev: str) -> bool:
    try:
        subprocess.run(f"ip link show dev {dev} >/dev/null 2>&1", shell=True, check=True)
        return True
    except subprocess.CalledProcessError:
        return False


def _supervisorctl(args: List[str]) -> subprocess.CompletedProcess:
    return subprocess.run(["supervisorctl", *args], capture_output=True, text=True)


def _supervisor_status(name: str) -> str:
    cp = _supervisorctl(["status", name])
    if cp.returncode != 0:
        return ""
    parts = cp.stdout.strip().split()
    if len(parts) < 2:
        return ""
    return parts[1]


def _supervisor_status_all() -> Dict[str, str]:
    cp = _supervisorctl(["status"])
    if cp.returncode != 0:
        return {}
    out: Dict[str, str] = {}
    for line in cp.stdout.splitlines():
        parts = line.strip().split()
        if len(parts) < 2:
            continue
        name, state = parts[0], parts[1]
        out[name] = state
    return out


def _supervisor_is_available() -> bool:
    """
    Check if supervisor is in a valid state to accept commands.
    Returns False if supervisor is in SHUTDOWN_STATE or similar.
    """
    try:
        # Try to get supervisor status - if it fails with SHUTDOWN_STATE, supervisor is shutting down
        cp = _supervisorctl(["status"])
        if cp.returncode != 0:
            error = (cp.stderr or "").strip()
            if "SHUTDOWN_STATE" in error:
                return False
        return True
    except Exception:
        return False


def _supervisor_start(name: str) -> None:
    _supervisorctl(["start", name])


def _supervisor_stop(name: str) -> None:
    _supervisorctl(["stop", name])


def _supervisor_restart(name: str) -> None:
    _supervisorctl(["restart", name])


def _supervisor_is_running(name: str) -> bool:
    return _supervisor_status(name) == "RUNNING"


def _easytier_cli_reload() -> bool:
    if not shutil.which("easytier-cli"):
        return False
    cp = subprocess.run(["easytier-cli", "reload"], capture_output=True, text=True)
    if cp.returncode != 0:
        err = (cp.stderr or cp.stdout or "").strip()
        print(f"[easytier] easytier-cli reload failed: {err}", flush=True)
        return False
    return True


def _tinc_reload(netname: str) -> bool:
    if not shutil.which("tinc"):
        return False
    cp = subprocess.run(
        ["tinc", "--pidfile=/run/tincd.pid", "reload"],
        capture_output=True,
        text=True,
    )
    if cp.returncode != 0:
        err = (cp.stderr or cp.stdout or "").strip()
        print(f"[tinc] reload failed: {err}", flush=True)
        return False
    return True


def _compute_openvpn_status(name: str, dev: str) -> str:
    state = _supervisor_status(f"openvpn-{name}")
    if state != "RUNNING":
        return "down"
    if _iface_exists(dev):
        return "up"
    return "connecting"


def _write_openvpn_status(name: str, status: str) -> None:
    try:
        _etcd_call(lambda: etcd.put(_ovpn_status_key(name), f"{status} {now_utc_iso()}"))
    except Exception as e:
        print(f"[openvpn-status] failed to write {name}: {e}", flush=True)


def _openvpn_program_conf(name: str, dev: str) -> str:
    return "\n".join([
        f"[program:openvpn-{name}]",
        f"command=openvpn --config /etc/openvpn/generated/{name}.conf",
        "autostart=true",
        "autorestart=true",
        f"stdout_logfile=/var/log/openvpn.{name}.out.log",
        f"stderr_logfile=/var/log/openvpn.{name}.err.log",
        "stdout_logfile_maxbytes=5MB",
        "stdout_logfile_backups=2",
        "stderr_logfile_maxbytes=5MB",
        "stderr_logfile_backups=2",
        "",
    ])


def reload_openvpn(node: Dict[str, str]) -> Tuple[bool, List[str]]:
    payload = {"node_id": NODE_ID, "node": node, "global": {}, "all_nodes": {}}
    out = _run_generator("gen_openvpn", payload)
    instances = out.get("instances", [])
    enabled: List[str] = []
    changed = False

    with _ovpn_lock:
        _ovpn_cfg_names.clear()
        _ovpn_devs.clear()

    active = set()
    for inst in instances:
        name = inst["name"]
        dev = inst["dev"]
        cfg = inst["config"]
        active.add(name)
        enabled.append(name)
        _ovpn_devs[name] = dev
        for f in inst.get("files", []):
            if _write_if_changed(f["path"], f["content"], mode=f.get("mode")):
                changed = True
        if _write_if_changed(f"/etc/openvpn/generated/{name}.conf", cfg):
            changed = True
        if _write_if_changed(f"/etc/supervisor/conf.d/openvpn-{name}.conf", _openvpn_program_conf(name, dev)):
            changed = True

    with _ovpn_lock:
        _ovpn_cfg_names.extend(sorted(enabled))

    for path in glob.glob("/etc/supervisor/conf.d/openvpn-*.conf"):
        name = os.path.basename(path).split("openvpn-")[-1].split(".conf")[0]
        if name not in active:
            try:
                os.remove(path)
                changed = True
            except Exception:
                pass

    _supervisorctl(["reread"])
    _supervisorctl(["update"])

    for name in enabled:
        _supervisor_restart(f"openvpn-{name}")
        _write_openvpn_status(name, "connecting")

    return changed, sorted(enabled)


def openvpn_status_loop():
    while True:
        time.sleep(max(3, OPENVPN_STATUS_INTERVAL))
        with _ovpn_lock:
            names = list(_ovpn_cfg_names)
        for name in names:
            with _ovpn_lock:
                dev = _ovpn_devs.get(name) or (f"tun{name[-1]}" if name and name[-1].isdigit() else f"tun-{name}")
            status = _compute_openvpn_status(name, dev)
            _write_openvpn_status(name, status)


def _wg_status_key(name: str) -> str:
    return f"{UPDATE_BASE}/wireguard/{name}/status"


def _compute_wireguard_status(name: str, dev: str) -> str:
    state = _supervisor_status(f"wireguard-{name}")
    if state != "RUNNING":
        return "down"
    if _iface_exists(dev):
        return "up"
    return "connecting"


def _write_wireguard_status(name: str, status: str) -> None:
    try:
        _etcd_call(lambda: etcd.put(_wg_status_key(name), f"{status} {now_utc_iso()}"))
    except Exception as e:
        print(f"[wireguard-status] failed to write {name}: {e}", flush=True)


def _wireguard_program_conf(name: str, dev: str) -> str:
    return "\n".join([
        f"[program:wireguard-{name}]",
        f"command=/usr/local/bin/run-wireguard.sh {dev}",
        "autostart=true",
        "autorestart=true",
        f"stdout_logfile=/var/log/wireguard.{name}.out.log",
        f"stderr_logfile=/var/log/wireguard.{name}.err.log",
        "stdout_logfile_maxbytes=5MB",
        "stdout_logfile_backups=2",
        "stderr_logfile_maxbytes=5MB",
        "stderr_logfile_backups=2",
        "",
    ])


def reload_wireguard(node: Dict[str, str]) -> Tuple[bool, List[str]]:
    payload = {"node_id": NODE_ID, "node": node, "global": {}, "all_nodes": {}}
    out = _run_generator("gen_wireguard", payload)
    instances = out.get("instances", [])
    enabled: List[str] = []
    changed = False

    with _wg_lock:
        _wg_cfg_names.clear()
        _wg_devs.clear()

    active = set()
    for inst in instances:
        name = inst["name"]
        dev = inst["dev"]
        cfg = inst["config"]
        active.add(name)
        enabled.append(name)
        _wg_devs[name] = dev
        if _write_if_changed(f"/etc/wireguard/{dev}.conf", cfg, mode=0o600):
            changed = True
        if _write_if_changed(f"/etc/supervisor/conf.d/wireguard-{name}.conf", _wireguard_program_conf(name, dev)):
            changed = True

    with _wg_lock:
        _wg_cfg_names.extend(sorted(enabled))

    for path in glob.glob("/etc/supervisor/conf.d/wireguard-*.conf"):
        name = os.path.basename(path).split("wireguard-")[-1].split(".conf")[0]
        if name not in active:
            try:
                os.remove(path)
                changed = True
            except Exception:
                pass

    _supervisorctl(["reread"])
    _supervisorctl(["update"])

    for name in enabled:
        _supervisor_restart(f"wireguard-{name}")
        _write_wireguard_status(name, "connecting")

    return changed, sorted(enabled)


def wireguard_status_loop():
    while True:
        time.sleep(max(3, WIREGUARD_STATUS_INTERVAL))
        with _wg_lock:
            names = list(_wg_cfg_names)
        for name in names:
            with _wg_lock:
                dev = _wg_devs.get(name) or _wg_dev_name(name)
            status = _compute_wireguard_status(name, dev)
            _write_wireguard_status(name, status)


def monitor_children_loop():
    backoffs: Dict[str, Backoff] = {}
    next_time: Dict[str, float] = {}

    def should_try(key: str) -> bool:
        return time.time() >= next_time.get(key, 0)

    def on_fail(key: str) -> None:
        b = backoffs.setdefault(key, Backoff())
        next_time[key] = time.time() + b.next_sleep()

    def on_ok(key: str) -> None:
        b = backoffs.setdefault(key, Backoff())
        b.reset()
        next_time[key] = 0

    while True:
        time.sleep(3)
        try:
            node = load_prefix(f"/nodes/{NODE_ID}/")
            global_cfg = load_prefix("/global/")
            mesh_type = global_cfg.get("/global/mesh_type", "easytier")

            if mesh_type == "tinc":
                if node.get(f"/nodes/{NODE_ID}/tinc/enable") == "true":
                    if _supervisor_status("tinc") != "RUNNING":
                        key = "tinc"
                        if should_try(key):
                            try:
                                reload_tinc(node, load_all_nodes(), global_cfg)
                                on_ok(key)
                            except Exception:
                                on_fail(key)
            else:
                if node.get(f"/nodes/{NODE_ID}/easytier/enable") == "true":
                    if _supervisor_status("easytier") != "RUNNING":
                        key = "easytier"
                        if should_try(key):
                            try:
                                reload_easytier(node, global_cfg)
                                on_ok(key)
                            except Exception:
                                on_fail(key)

            # OpenVPN instances are managed by supervisord now.
        except Exception:
            continue


def supervisor_retry_loop():
    backoffs: Dict[str, Backoff] = {}
    next_time: Dict[str, float] = {}
    while True:
        time.sleep(max(5, SUPERVISOR_RETRY_INTERVAL))
        try:
            statuses = _supervisor_status_all()
            now = time.time()
            for name, state in statuses.items():
                if name == "watcher":
                    continue
                if state != "FATAL":
                    if name in backoffs:
                        backoffs[name].reset()
                        next_time[name] = 0
                    continue
                if now < next_time.get(name, 0):
                    continue
                if name == "mosdns":
                    _supervisor_stop(name)
                    time.sleep(2)
                    _supervisor_start(name)
                else:
                    _supervisor_restart(name)
                b = backoffs.setdefault(name, Backoff())
                next_time[name] = now + b.next_sleep()
        except Exception:
            continue


def clash_refresh_loop():
    global tproxy_enabled, _clash_refresh_enable, _clash_refresh_interval, _clash_refresh_next
    while True:
        time.sleep(5)
        with _clash_refresh_lock:
            enabled = _clash_refresh_enable
            interval = _clash_refresh_interval
            next_ts = _clash_refresh_next
        if not enabled or interval <= 0:
            continue
        if time.time() < next_ts:
            continue
        try:
            node = load_prefix(f"/nodes/{NODE_ID}/")
            if node.get(f"/nodes/{NODE_ID}/clash/enable") != "true":
                with _clash_refresh_lock:
                    _clash_refresh_enable = False
                continue
            global_cfg = load_prefix("/global/")
            payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": {}}
            out = _run_generator("gen_clash", payload)

            # Hot reload: update config and send HUP signal instead of restarting
            reload_clash(out["config_yaml"], api_controller=out.get("api_controller", ""), api_secret=out.get("api_secret", ""))

            # No need to wait for health check - the continuous monitoring loop will refresh ipset
            print("[clash-refresh] Config reloaded, ipset will be refreshed by continuous monitoring loop", flush=True)

            # Update tproxy rules if mode changed
            if out["mode"] == "tproxy":
                # If previously not in tproxy mode, remove old rules first
                if not tproxy_enabled:
                    try:
                        tproxy_remove()
                    except Exception:
                        pass
                # Apply new tproxy rules
                tproxy_apply(
                    out["tproxy_targets"],
                    _clash_exclude_src(node),
                    _clash_exclude_ifaces(node),
                    _clash_exclude_ports(node, global_cfg),
                )
                _set_cached_tproxy_targets(out["tproxy_targets"])
                tproxy_enabled = True
            else:
                # If switching away from tproxy mode, remove rules
                if tproxy_enabled:
                    try:
                        tproxy_remove()
                    except Exception:
                        pass
                    tproxy_enabled = False
        except Exception as e:
            print(f"[clash-refresh] error: {e}", flush=True)
        finally:
            with _clash_refresh_lock:
                _clash_refresh_next = time.time() + (interval * 60)


def clash_crash_monitor_loop():
    """
    Monitor Mihomo for crashes and manage TProxy accordingly.

    If Mihomo crashes in TProxy mode:
    1. Immediately remove TProxy iptables rules
    2. Cleanup proxy IP ipset
    3. Wait for Mihomo to recover
    4. Re-apply TProxy rules when Mihomo is healthy again
    """
    global _clash_last_healthy, _clash_monitoring_enabled, tproxy_enabled

    while True:
        time.sleep(5)

        with _clash_monitoring_lock:
            enabled = _clash_monitoring_enabled

        if not enabled or not tproxy_enabled:
            continue

        try:
            is_healthy = clash_health_check()

            if is_healthy:
                # Mihomo is healthy
                if _clash_last_healthy == 0:
                    # Was unhealthy, now recovered - reapply TProxy
                    print("[clash-monitor] Mihomo recovered, reapplying TProxy", flush=True)
                    node = load_prefix(f"/nodes/{NODE_ID}/")
                    global_cfg = load_prefix("/global/")

                    proxy_dst = _get_cached_tproxy_targets()
                    if not proxy_dst:
                        print("[clash-monitor] No cached TProxy targets, skipping reapply", flush=True)
                    else:
                        # Re-create empty ipset and apply TProxy first
                        print("[clash-monitor] Re-initializing proxy IP ipset...", flush=True)
                        _ensure_proxy_ipset()

                        tproxy_apply(
                            proxy_dst,
                            _clash_exclude_src(node),
                            _clash_exclude_ifaces(node),
                            [],  # No individual IPs, using ipset
                            _clash_exclude_ports(node, global_cfg),
                        )
                        print("[clash-monitor] TProxy reapplied successfully", flush=True)

                        # Start async IP extraction
                        threading.Thread(target=_update_proxy_ips_async, daemon=True).start()

                _clash_last_healthy = time.time()
            else:
                # Mihomo is not healthy
                if _clash_last_healthy > 0:
                    # Was healthy, now crashed - remove TProxy immediately
                    print("[clash-monitor] Mihomo crashed, removing TProxy", flush=True)
                    try:
                        tproxy_remove()
                        _cleanup_proxy_ips()
                        print("[clash-monitor] TProxy and ipset removed due to crash", flush=True)
                    except Exception as e:
                        print(f"[clash-monitor] Failed to remove TProxy: {e}", flush=True)

                _clash_last_healthy = 0.0

        except Exception as e:
            print(f"[clash-monitor] Error: {e}", flush=True)


def clash_proxy_ips_monitor_loop():
    """
    Monitor proxy provider IPs and update ipset periodically.

    This ensures that proxy server IP changes are reflected in TProxy exclusions.
    Runs every 1 minute when TProxy is enabled - continuously polls for updates.

    Reads directly from /etc/clash/config.yaml and provider files to detect
    IP changes without waiting for Clash health checks or API availability.
    """
    global tproxy_enabled, _proxy_ips_enabled, _cached_proxy_ips

    while True:
        time.sleep(60)  # Check every 1 minute

        if not tproxy_enabled or not _proxy_ips_enabled:
            continue

        try:
            print("[clash-proxy-ips] Checking for proxy IP updates...", flush=True)

            # Get current IPs from config file and provider files
            current_ips = _get_all_proxy_ips()

            with _proxy_ips_lock:
                if current_ips != _cached_proxy_ips:
                    print(f"[clash-proxy-ips] Proxy IPs changed, updating ipset (old: {len(_cached_proxy_ips)}, new: {len(current_ips)})", flush=True)

                    if not current_ips:
                        # No IPs found, cleanup
                        _ipset_destroy(PROXY_IPSET_NAME)
                        _cached_proxy_ips = set()
                        _proxy_ips_enabled = False
                    else:
                        # Update ipset with new IPs
                        _ipset_create(PROXY_IPSET_NAME)
                        _ipset_flush(PROXY_IPSET_NAME)
                        _ipset_add(PROXY_IPSET_NAME, current_ips)
                        _cached_proxy_ips = current_ips

                    print("[clash-proxy-ips] ipset updated successfully", flush=True)
                else:
                    print("[clash-proxy-ips] No changes detected", flush=True)

        except Exception as e:
            print(f"[clash-proxy-ips] Error: {e}", flush=True)


# ---------- FRR smooth reload ----------
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

def _extract_ips_from_proxies(proxies: List[Dict]) -> Set[str]:
    """
    Extract IPv4 addresses from proxy configurations.

    Args:
        proxies: List of proxy dictionaries from Clash API

    Returns:
        Set of unique IPv4 addresses found
    """
    ips = set()

    for proxy in proxies:
        # Extract server IP from various proxy types
        server = proxy.get("server", "")
        if not server:
            continue

        # Check if server is an IPv4 address (not a hostname or IPv6)
        if _is_ipv4_address(server):
            ips.add(server)
            continue

        # Try to resolve hostname to IPv4 addresses only
        try:
            import socket
            # Get only IPv4 addresses (socket.AF_INET)
            addr_info = socket.getaddrinfo(server, None, socket.AF_INET)
            for info in addr_info:
                ip = info[4][0]
                ips.add(ip)
        except Exception as e:
            print(f"[clash] Failed to resolve {server}: {e}", flush=True)

    return ips


def _is_ipv4_address(addr: str) -> bool:
    """Check if address is an IPv4 address."""
    import ipaddress
    try:
        ip_obj = ipaddress.ip_address(addr.strip())
        return isinstance(ip_obj, ipaddress.IPv4Address)
    except ValueError:
        return False


def _extract_ips_from_yaml(yaml_content: str) -> Set[str]:
    """
    Extract IP addresses from YAML proxy configuration.

    Args:
        yaml_content: YAML content (may be base64 encoded)

    Returns:
        Set of unique IP addresses
    """
    import base64
    import yaml as yaml_lib

    content = yaml_content.strip()

    # Try to decode base64 first
    try:
        decoded = base64.b64decode(content)
        # Check if decoded content is valid UTF-8 text
        try:
            content = decoded.decode("utf-8")
        except UnicodeDecodeError:
            # Not base64, use original
            content = yaml_content.strip()
    except Exception:
        content = yaml_content.strip()

    # Parse YAML
    try:
        config = yaml_lib.safe_load(content)
        if not isinstance(config, dict):
            return set()

        proxies = config.get("proxies", [])
        if isinstance(proxies, list):
            return _extract_ips_from_proxies(proxies)
    except Exception as e:
        print(f"[clash] Failed to parse YAML: {e}", flush=True)

    return set()


def _extract_ips_from_subscription(content: str) -> Set[str]:
    """
    Extract IPs from various subscription formats.

    Args:
        content: Subscription content (YAML, base64 YAML, or ss:// / vless:// URLs)

    Returns:
        Set of unique IP addresses
    """
    ips = set()

    # Try to parse as YAML first (may be base64 encoded)
    yaml_ips = _extract_ips_from_yaml(content)
    if yaml_ips:
        ips.update(yaml_ips)

    # Try to parse as newline-separated URLs
    for line in content.strip().split("\n"):
        line = line.strip()
        if not line:
            continue

        # Parse ss:// or vless:// URLs
        if line.startswith("ss://") or line.startswith("vless://"):
            try:
                import base64
                # ss:// format: ss://base64(info)@server:port...
                # vless:// format: vless://uuid@server:port?params
                if "://" in line:
                    _, rest = line.split("://", 1)
                    # Remove protocol prefix
                    rest = rest.split("?")[0]  # Remove query params
                    rest = rest.split("#")[0]  # Remove fragment

                    if "@" in rest:
                        # Format: base64(info)@server:port or uuid@server:port
                        creds, server_part = rest.rsplit("@", 1)
                        server = server_part.split(":")[0]
                        if _is_ipv4_address(server):
                            ips.add(server)
                        else:
                            # Try to resolve hostname to IPv4 only
                            try:
                                import socket
                                addr_info = socket.getaddrinfo(server, None, socket.AF_INET)
                                for info in addr_info:
                                    ip = info[4][0]
                                    ips.add(ip)
                            except Exception:
                                pass
            except Exception:
                pass

    return ips


def _download_provider_content(url: str) -> Optional[str]:
    """
    Download content from a provider URL.

    Args:
        url: Provider URL

    Returns:
        Content as string, or None on failure
    """
    try:
        # Use Clash proxy if available
        proxy = None
        clash_pid_val = clash_pid()
        if clash_pid_val is not None:
            proxy = f"http://127.0.0.1:{CLASH_HTTP_PORT}"
            print(f"[clash] Downloading provider via proxy: {url}", flush=True)

        proxies = {"http": proxy, "https": proxy} if proxy else None
        resp = requests.get(url, timeout=30, proxies=proxies)
        resp.raise_for_status()
        return resp.text
    except Exception as e:
        print(f"[clash] Failed to download provider {url}: {e}", flush=True)
        return None


def _get_proxy_ips_from_providers(providers: List[Dict]) -> Set[str]:
    """
    Extract all proxy IPs from proxy providers.

    Args:
        providers: List of provider dictionaries from Clash API

    Returns:
        Set of unique IP addresses
    """
    import os

    all_ips = set()
    clash_config_dir = "/etc/clash"

    for provider in providers:
        # Get provider file path (may be relative like "./providers/CNIX.yaml")
        provider_path = provider.get("path", "")
        if not provider_path:
            continue

        # Resolve path relative to Clash config directory
        # If path starts with "./", it's relative to config dir
        if provider_path.startswith("./"):
            full_path = os.path.normpath(os.path.join(clash_config_dir, provider_path))
        elif provider_path.startswith("/"):
            # Absolute path
            full_path = provider_path
        else:
            # Relative path without leading "./"
            full_path = os.path.normpath(os.path.join(clash_config_dir, provider_path))

        try:
            with open(full_path, "r", encoding="utf-8") as f:
                content = f.read()
                ips = _extract_ips_from_subscription(content)
                all_ips.update(ips)
                print(f"[clash] Extracted {len(ips)} IPs from provider {provider_path} (full: {full_path})", flush=True)
        except Exception as e:
            print(f"[clash] Failed to read provider file {full_path}: {e}", flush=True)

    return all_ips


def _get_all_proxy_ips() -> Set[str]:
    """
    Get all proxy server IPs from Clash configuration file.

    Reads directly from /etc/clash/config.yaml to extract proxy IPs from:
    1. proxy-providers (external YAML files)
    2. proxies (inline proxy definitions)

    Returns:
        Set of unique IP addresses
    """
    import yaml as yaml_lib

    try:
        # Read Clash config file
        with open("/etc/clash/config.yaml", "r", encoding="utf-8") as f:
            config = yaml_lib.safe_load(f)

        if not isinstance(config, dict):
            return set()

        all_ips = set()

        # Extract IPs from proxy-providers (external files)
        proxy_providers = config.get("proxy-providers", {})
        if proxy_providers:
            print(f"[clash] Found {len(proxy_providers)} proxy-providers in config", flush=True)
            provider_ips = _get_proxy_ips_from_providers(list(proxy_providers.values()))
            all_ips.update(provider_ips)

        # Extract IPs from inline proxies
        proxies = config.get("proxies", [])
        if proxies:
            print(f"[clash] Found {len(proxies)} inline proxies in config", flush=True)
            inline_ips = _extract_ips_from_proxies(proxies)
            all_ips.update(inline_ips)

        return all_ips
    except Exception as e:
        print(f"[clash] Failed to get proxy IPs from config file: {e}", flush=True)
        return set()


def _ipset_exists(name: str) -> bool:
    """Check if an ipset exists."""
    try:
        result = subprocess.run(
            ["ipset", "list", name],
            capture_output=True,
            text=True,
            stderr=subprocess.DEVNULL
        )
        return result.returncode == 0
    except Exception:
        return False


def _ipset_create(name: str) -> None:
    """Create an ipset if it doesn't exist."""
    try:
        # Check if ipset exists
        result = subprocess.run(
            ["ipset", "list", name],
            capture_output=True,
            text=True
        )
        if result.returncode != 0:
            # Create ipset
            print(f"[clash] Creating ipset {name}", flush=True)
            subprocess.run(
                ["ipset", "create", name, "hash:ip"],
                check=True
            )
    except Exception as e:
        print(f"[clash] Failed to create ipset {name}: {e}", flush=True)


def _ipset_flush(name: str) -> None:
    """Flush all entries from an ipset."""
    try:
        subprocess.run(
            ["ipset", "flush", name],
            check=True
        )
    except Exception as e:
        print(f"[clash] Failed to flush ipset {name}: {e}", flush=True)


def _ipset_add(name: str, ips: Set[str]) -> None:
    """Add IPs to an ipset."""
    if not ips:
        return

    try:
        for ip in ips:
            subprocess.run(
                ["ipset", "add", name, ip],
                check=True,  # ipset add is idempotent
                stderr=subprocess.DEVNULL
            )
    except Exception as e:
        print(f"[clash] Failed to add IPs to ipset {name}: {e}", flush=True)


def _ipset_destroy(name: str) -> None:
    """Destroy an ipset."""
    try:
        subprocess.run(
            ["ipset", "destroy", name],
            check=False,  # May not exist
            stderr=subprocess.DEVNULL
        )
    except Exception:
        pass


def _ensure_proxy_ipset() -> None:
    """
    Ensure proxy IP ipset exists (empty).

    This creates an empty ipset that can be immediately used in iptables rules.
    IPs will be populated asynchronously in the background.

    Call this before applying TProxy to avoid blocking startup.
    """
    global _proxy_ips_enabled

    with _proxy_ips_lock:
        if _ipset_exists(PROXY_IPSET_NAME):
            print(f"[clash] ipset {PROXY_IPSET_NAME} already exists", flush=True)
            _proxy_ips_enabled = True
            return

        print(f"[clash] Creating empty ipset {PROXY_IPSET_NAME}", flush=True)
        _ipset_create(PROXY_IPSET_NAME)
        _proxy_ips_enabled = True
        print(f"[clash] Empty ipset {PROXY_IPSET_NAME} created (will be populated asynchronously)", flush=True)


def _update_proxy_ips_async() -> None:
    """
    Asynchronously update proxy IPs from Clash and sync to ipset.

    This function runs in a background thread after TProxy is applied.
    It extracts IPs from Clash API and provider files, then updates the ipset.

    Non-blocking: Allows TProxy to start immediately even with slow providers.
    """
    global _cached_proxy_ips, _proxy_ips_enabled

    with _proxy_ips_lock:
        if not _proxy_ips_enabled:
            return

    print("[clash] Extracting proxy server IPs (async)...", flush=True)

    try:
        # Get all proxy IPs (may take time for large providers)
        ips = _get_all_proxy_ips()

        with _proxy_ips_lock:
            if not _proxy_ips_enabled:
                # TProxy was disabled while we were extracting
                return

            if not ips:
                print("[clash] No proxy IPs found", flush=True)
                _cached_proxy_ips = set()
                return

            print(f"[clash] Found {len(ips)} unique proxy IPs, updating ipset...", flush=True)

            # Check if IPs actually changed
            current_cached = _cached_proxy_ips
            if ips == current_cached:
                print("[clash] Proxy IPs unchanged, skipping update", flush=True)
                return

            # Flush old entries and add new IPs
            _ipset_flush(PROXY_IPSET_NAME)
            _ipset_add(PROXY_IPSET_NAME, ips)

            # Update cache
            old_count = len(current_cached)
            _cached_proxy_ips = ips

            print(f"[clash] Updated ipset {PROXY_IPSET_NAME}: {old_count} → {len(ips)} IPs", flush=True)

    except Exception as e:
        print(f"[clash] Failed to update proxy IPs (will retry in monitoring loop): {e}", flush=True)


def _cleanup_proxy_ips() -> None:
    """
    Cleanup proxy IP ipset.

    This should be called when Clash crashes or TProxy is disabled.
    """
    global _cached_proxy_ips, _proxy_ips_enabled

    with _proxy_ips_lock:
        print("[clash] Cleaning up proxy IP ipset...", flush=True)
        _ipset_destroy(PROXY_IPSET_NAME)
        _cached_proxy_ips = set()
        _proxy_ips_enabled = False


def _clash_api_request(endpoint: str, method: str = "GET", data: Optional[Dict] = None) -> Optional[Dict]:
    """
    Make a request to Mihomo API.

    Args:
        endpoint: API endpoint (e.g., "/proxies", "/proxies/SELECTED")
        method: HTTP method (GET or DELETE)
        data: Request body data for DELETE requests

    Returns:
        JSON response dict or None on failure
    """
    global CLASH_API_SECRET
    try:
        headers = {}
        if CLASH_API_SECRET:
            headers["Authorization"] = f"Bearer {CLASH_API_SECRET}"

        url = f"http://127.0.0.1:{CLASH_API_PORT}{endpoint}"

        if method == "GET":
            resp = requests.get(url, headers=headers, timeout=5)
        elif method == "DELETE":
            resp = requests.delete(url, headers=headers, json=data, timeout=5)
        else:
            return None

        if resp.status_code == 200:
            return resp.json()
        return None
    except Exception as e:
        print(f"[clash] API request failed: {e}", flush=True)
        return None


def clash_health_check() -> bool:
    """
    Check Mihomo health by verifying:
    1. Process is running
    2. API is accessible
    3. ALL url-test proxies are NOT REJECT (strict check)

    Returns:
        True if Mihomo is healthy, False otherwise
    """
    # Check if process is running
    if clash_pid() is None:
        return False

    # Check API availability
    proxies_data = _clash_api_request("/proxies")
    if not proxies_data:
        return False

    # Check ALL url-test proxies for REJECT status (strict requirement)
    proxies = proxies_data.get("proxies", {})
    url_test_proxies = []

    # Collect all url-test selectors
    for name, proxy in proxies.items():
        if proxy.get("type") == "Selector" and "url-test" in name.lower():
            url_test_proxies.append((name, proxy))

    # If no url-test proxies found, assume healthy if API is accessible
    if not url_test_proxies:
        return True

    # Strict check: ALL url-test proxies must NOT be REJECT
    for name, proxy in url_test_proxies:
        now = proxy.get("now", "")
        if not now or now == "REJECT":
            print(f"[clash] url-test proxy '{name}' is REJECT or empty (now={now})", flush=True)
            return False

    # All url-test proxies are healthy
    return True


def wait_for_clash_healthy(timeout: int = 30) -> None:
    """
    Wait for Mihomo to become healthy. Raises exception if timeout.

    Args:
        timeout: Maximum wait time in seconds (use None for infinite wait)

    Raises:
        RuntimeError: If Mihomo does not become healthy within timeout
    """
    start = time.time()
    while True:
        if clash_health_check():
            print("[clash] Mihomo is healthy", flush=True)
            return
        time.sleep(1)
        if timeout is not None:
            if time.time() - start >= timeout:
                raise RuntimeError(f"Mihomo did not become healthy after {timeout}s")


def wait_for_clash_healthy_infinite() -> None:
    """
    Wait indefinitely for Mihomo to become health. No timeout.

    This is used for MosDNS startup which MUST wait for Clash to be healthy.
    """
    print("[clash] Waiting indefinitely for Mihomo to become healthy...", flush=True)
    while True:
        if clash_health_check():
            print("[clash] Mihomo is healthy", flush=True)
            return
        time.sleep(1)


def clash_pid() -> Optional[int]:
    """Get mihomo PID, return None if process not running."""
    try:
        return int(open("/run/clash/mihomo.pid", encoding="utf-8").read().strip())
    except Exception:
        try:
            output = subprocess.check_output("pidof mihomo", shell=True, stderr=subprocess.DEVNULL).decode().strip()
            if output:
                pids = output.split()
                if pids:
                    return int(pids[0])
        except subprocess.CalledProcessError:
            pass
        except Exception:
            pass
    return None


def reload_clash(conf_text: str, api_controller: str = "", api_secret: str = "") -> None:
    """
    Reload clash config and update API credentials.

    Args:
        conf_text: Clash config YAML content
        api_controller: API controller address (e.g., "0.0.0.0:9090")
        api_secret: API secret for authentication
    """
    global CLASH_API_SECRET
    CLASH_API_SECRET = api_secret

    pid = clash_pid()
    if pid is None:
        print("[clash] not running, skipping reload (config still written)", flush=True)
        with open("/etc/clash/config.yaml", "w", encoding="utf-8") as f:
            f.write(conf_text)
        return
    try:
        with open("/etc/clash/config.yaml", "w", encoding="utf-8") as f:
            f.write(conf_text)
        run(f"kill -HUP {pid}")
        print(f"[clash] reloaded (pid={pid})", flush=True)
    except Exception as e:
        print(f"[clash] reload failed: {e}", flush=True)
        raise


def _split_ml(val: str) -> List[str]:
    if not val:
        return []
    return [x.strip() for x in val.replace("\r\n", "\n").replace("\r", "\n").split("\n") if x.strip()]


def _ovpn_dev_name(name: str) -> str:
    return f"tun{name[-1]}" if name and name[-1].isdigit() else f"tun-{name}"


def _wg_dev_name(name: str) -> str:
    return f"wg{name[-1]}" if name and name[-1].isdigit() else f"wg-{name}"


def _clash_exclude_ifaces(node: Dict[str, str]) -> List[str]:
    out: List[str] = []
    et_dev = node.get(f"/nodes/{NODE_ID}/easytier/dev_name", "")
    if et_dev:
        out.append(et_dev)
    tinc_dev = node.get(f"/nodes/{NODE_ID}/tinc/dev_name", "")
    if tinc_dev:
        out.append(tinc_dev)
    base = f"/nodes/{NODE_ID}/openvpn/"
    names = set()
    for k, v in node.items():
        if not k.startswith(base) or not k.endswith("/enable"):
            continue
        if v != "true":
            continue
        name = k[len(base):].split("/", 1)[0]
        dev = node.get(f"{base}{name}/dev", "")
        out.append(dev or _ovpn_dev_name(name))
    base = f"/nodes/{NODE_ID}/wireguard/"
    for k, v in node.items():
        if not k.startswith(base) or not k.endswith("/enable"):
            continue
        if v != "true":
            continue
        name = k[len(base):].split("/", 1)[0]
        dev = node.get(f"{base}{name}/dev", "")
        out.append(dev or _wg_dev_name(name))
    return sorted(set(out))


def _clash_exclude_src(node: Dict[str, str]) -> List[str]:
    cidrs: List[str] = []
    gw_ip = os.environ.get("DEFAULT_GW", "").strip()
    if gw_ip:
        if "/" not in gw_ip:
            gw_ip = f"{gw_ip}/32"
        cidrs.append(gw_ip)
    return sorted(set(cidrs))


def _parse_port(val: str) -> Optional[str]:
    v = val.strip()
    if not v:
        return None
    if "://" in v:
        try:
            u = urlparse(v)
            if u.port:
                return str(u.port)
        except Exception:
            return None
    if ":" in v:
        parts = v.split(":")
        tail = parts[-1]
        if tail.isdigit():
            return tail
    if v.isdigit():
        return v
    return None


def _clash_exclude_ports(node: Dict[str, str], global_cfg: Dict[str, str]) -> List[str]:
    raw = load_key(f"/nodes/{NODE_ID}/clash/exclude_tproxy_port")
    ports = set(_split_ml(raw))

    mesh_type = global_cfg.get("/global/mesh_type", "easytier")
    if mesh_type == "tinc":
        tinc_port = node.get(f"/nodes/{NODE_ID}/tinc/port", "")
        p = _parse_port(tinc_port)
        if p:
            ports.add(p)
    else:
        listeners = _split_ml(node.get(f"/nodes/{NODE_ID}/easytier/listeners", ""))
        mapped = _split_ml(node.get(f"/nodes/{NODE_ID}/easytier/mapped_listeners", ""))
        for item in listeners + mapped:
            p = _parse_port(item)
            if p:
                ports.add(p)

    base = f"/nodes/{NODE_ID}/openvpn/"
    for k, v in node.items():
        if not k.startswith(base) or not k.endswith("/enable"):
            continue
        if v != "true":
            continue
        name = k[len(base):].split("/", 1)[0]
        p = _parse_port(node.get(f"{base}{name}/port", ""))
        if p:
            ports.add(p)
    base = f"/nodes/{NODE_ID}/wireguard/"
    for k, v in node.items():
        if not k.startswith(base) or not k.endswith("/enable"):
            continue
        if v != "true":
            continue
        name = k[len(base):].split("/", 1)[0]
        p = _parse_port(node.get(f"{base}{name}/listen_port", ""))
        if p:
            ports.add(p)

    return sorted(ports)


def tproxy_apply(
    proxy_dst: List[str],
    exclude_src: List[str],
    exclude_ifaces: List[str],
    exclude_ips: List[str],
    exclude_ports: List[str],
) -> None:
    """
    Apply TPROXY rules in include mode (only proxy specified destinations).

    Args:
        proxy_dst: List of CIDRs to proxy (from /lan configuration)
        exclude_src: Source CIDRs to bypass proxy
        exclude_ifaces: Interfaces to bypass proxy
        exclude_ips: Proxy server IPs to bypass (to prevent proxy loops)
        exclude_ports: Ports to bypass proxy
    """
    run(
        f"PROXY_CIDRS='{ ' '.join(proxy_dst) }' "
        f"EXCLUDE_SRC_CIDRS='{ ' '.join(exclude_src) }' "
        f"EXCLUDE_IFACES='{ ' '.join(exclude_ifaces) }' "
        f"EXCLUDE_IPS='{ ' '.join(exclude_ips) }' "
        f"EXCLUDE_PORTS='{ ' '.join(exclude_ports) }' "
        f"PROXY_IPSET_NAME='{PROXY_IPSET_NAME}' "
        f"TPROXY_PORT={TPROXY_PORT} MARK=0x1 TABLE=100 "
        f"/usr/local/bin/tproxy.sh apply"
    )


def tproxy_remove() -> None:
    run(f"TPROXY_PORT={TPROXY_PORT} MARK=0x1 TABLE=100 /usr/local/bin/tproxy.sh remove")


def _get_cached_tproxy_targets() -> List[str]:
    """Get the cached tproxy target list."""
    return list(_cached_tproxy_targets)


def _set_cached_tproxy_targets(targets: List[str]) -> None:
    """Cache the tproxy target list."""
    global _cached_tproxy_targets
    _cached_tproxy_targets = list(targets)


def _check_tproxy_iptables() -> bool:
    """Check if tproxy iptables rules are correctly applied."""
    try:
        # Check if CLASH_TPROXY chain exists in mangle table
        cp = subprocess.run(
            ["iptables", "-t", "mangle", "-L", "CLASH_TPROXY"],
            capture_output=True,
            text=True,
        )
        if cp.returncode != 0:
            return False

        # Check if PREROUTING chain jumps to CLASH_TPROXY
        cp = subprocess.run(
            ["iptables", "-t", "mangle", "-L", "PREROUTING"],
            capture_output=True,
            text=True,
        )
        if cp.returncode != 0:
            return False

        if "CLASH_TPROXY" not in cp.stdout:
            return False

        # Check ip rule
        cp = subprocess.run(
            ["ip", "rule", "list"],
            capture_output=True,
            text=True,
        )
        if "fwmark 0x1" not in cp.stdout:
            return False

        return True
    except Exception as e:
        print(f"[tproxy-check] error checking iptables: {e}", flush=True)
        return False


def _fix_tproxy_iptables(
    proxy_dst: List[str],
    exclude_src: List[str],
    exclude_ifaces: List[str],
    exclude_ips: List[str],
    exclude_ports: List[str],
) -> None:
    """Fix tproxy iptables rules by reapplying them."""
    try:
        print(f"[tproxy-check] reapplying iptables rules", flush=True)
        tproxy_apply(proxy_dst, exclude_src, exclude_ifaces, exclude_ips, exclude_ports)
        print(f"[tproxy-check] iptables rules reapplied successfully", flush=True)
    except Exception as e:
        print(f"[tproxy-check] failed to reapply iptables: {e}", flush=True)


def tproxy_check_loop() -> None:
    """Periodically check tproxy iptables rules and fix if needed."""
    global tproxy_enabled, _tproxy_check_enabled, _tproxy_check_interval

    while True:
        time.sleep(_tproxy_check_interval)
        try:
            with _tproxy_check_lock:
                enabled = _tproxy_check_enabled
            if not enabled or not tproxy_enabled:
                continue

            if _check_tproxy_iptables():
                continue

            # Rules are missing or incorrect, reapply them
            print(f"[tproxy-check] tproxy iptables rules missing or incorrect, fixing...", flush=True)

            # Load current configuration
            node = load_prefix(f"/nodes/{NODE_ID}/")
            global_cfg = load_prefix("/global/")

            # Reapply tproxy rules (using ipset, no individual IPs needed)
            _fix_tproxy_iptables(
                _get_cached_tproxy_targets(),
                _clash_exclude_src(node),
                _clash_exclude_ifaces(node),
                [],  # No individual IPs, using ipset
                _clash_exclude_ports(node, global_cfg),
            )
        except Exception as e:
            print(f"[tproxy-check] error: {e}", flush=True)


# ---------- MosDNS ----------
def _write_mosdns_rules_json(rules: Dict[str, str]) -> Optional[str]:
    if not rules:
        return None
    path = "/etc/mosdns/rule_files.json"
    _write_text(path, json.dumps(rules, ensure_ascii=True, indent=2) + "\n", mode=0o644)
    return path


def _mosdns_rules_stamp_path() -> str:
    return "/etc/mosdns/.rules_updated"


def _should_refresh_rules(refresh_minutes: int) -> bool:
    path = _mosdns_rules_stamp_path()
    try:
        mtime = os.path.getmtime(path)
    except FileNotFoundError:
        return True
    age = time.time() - mtime
    return age >= (refresh_minutes * 60)


def _safe_rule_path(rel: str) -> str:
    rel = rel.lstrip("/").replace("\\", "/")
    if ".." in rel.split("/"):
        raise ValueError(f"invalid rule file path: {rel}")
    return rel


def _download_rules(rules: Dict[str, str], skip: Optional[Set[str]] = None) -> Set[str]:
    """
    Download MosDNS rule files.

    Args:
        rules: Dictionary of {rel_path: url}
        skip: Set of files to skip (already downloaded successfully)

    Returns:
        Set of successfully downloaded file paths

    Raises:
        Exception: If any download fails (after retries)
    """
    if not rules:
        return set()

    skip = skip or set()
    successful = set()
    failed = []

    # Check if Clash is running and use its proxy if available
    proxy = None
    clash_pid_val = clash_pid()
    if clash_pid_val is not None:
        proxy = os.environ.get("MOSDNS_HTTP_PROXY", f"http://127.0.0.1:{CLASH_HTTP_PORT}")
        print(f"[mosdns] Using Clash proxy for rule downloads: {proxy}", flush=True)
    else:
        proxy = os.environ.get("MOSDNS_HTTP_PROXY")
        if proxy:
            print(f"[mosdns] Using configured proxy: {proxy}", flush=True)
        else:
            print(f"[mosdns] Clash not running, downloading rules directly (may be slow)", flush=True)

    proxies = {"http": proxy, "https": proxy} if proxy else None
    base_dir = "/etc/mosdns"

    for rel, url in rules.items():
        if rel in skip:
            print(f"[mosdns] Skipping already downloaded: {rel}", flush=True)
            successful.add(rel)
            continue

        safe_rel = _safe_rule_path(rel)
        out_path = os.path.join(base_dir, safe_rel)
        os.makedirs(os.path.dirname(out_path), exist_ok=True)

        downloaded = False
        # Try proxy first if available
        if proxies:
            try:
                resp = requests.get(url, timeout=30, proxies=proxies)
                resp.raise_for_status()
                _write_text(out_path, resp.text, mode=0o644)
                print(f"[mosdns] Downloaded rule (via proxy): {rel}", flush=True)
                successful.add(rel)
                downloaded = True
            except Exception as e:
                print(f"[mosdns] Proxy download failed for {rel}: {e}", flush=True)
                print(f"[mosdns] Retrying with direct connection...", flush=True)
                # Immediate direct retry without waiting
                try:
                    resp = requests.get(url, timeout=30)
                    resp.raise_for_status()
                    _write_text(out_path, resp.text, mode=0o644)
                    print(f"[mosdns] Downloaded rule (direct): {rel}", flush=True)
                    successful.add(rel)
                    downloaded = True
                except Exception as e2:
                    print(f"[mosdns] Direct download also failed for {rel}: {e2}", flush=True)

        # If no proxy or proxy fallback failed, try direct (or no proxy case)
        if not downloaded:
            try:
                resp = requests.get(url, timeout=30)
                resp.raise_for_status()
                _write_text(out_path, resp.text, mode=0o644)
                print(f"[mosdns] Downloaded rule (direct): {rel}", flush=True)
                successful.add(rel)
            except Exception as e:
                print(f"[mosdns] Failed to download {rel}: {e}", flush=True)
                failed.append(rel)

    if failed:
        raise Exception(f"Failed to download {len(failed)} file(s): {', '.join(failed)}")

    return successful


def _download_rules_with_backoff(rules: Dict[str, str]) -> None:
    """
    Download MosDNS rule files with intelligent retry logic.

    Strategy:
    - First attempt: Download all files
    - Retry attempts: Only retry failed files, skip successful ones
    - Shorter retry intervals for faster recovery
    - Only on next refresh trigger will all files be re-downloaded
    """
    if not rules:
        return

    successful: Set[str] = set()
    attempt = 0
    max_attempts = 5  # Reduced from infinite backoff to fixed attempts

    while attempt < max_attempts:
        attempt += 1
        try:
            # Download only files that haven't been successfully downloaded yet
            newly_successful = _download_rules(rules, skip=successful)
            successful.update(newly_successful)

            if len(successful) == len(rules):
                print(f"[mosdns] All {len(rules)} rule(s) downloaded successfully", flush=True)
                return
            else:
                print(f"[mosdns] Downloaded {len(successful)}/{len(rules)} rules", flush=True)

        except Exception as e:
            failed_count = len(rules) - len(successful)
            print(f"[mosdns] Attempt {attempt}/{max_attempts}: {failed_count} file(s) failed", flush=True)

            if attempt >= max_attempts:
                print(f"[mosdns] Giving up after {max_attempts} attempts. {failed_count} file(s) could not be downloaded.", flush=True)
                raise

            # Shorter retry times: 2s, 5s, 10s, 20s (instead of exponential backoff)
            retry_delays = [2, 5, 10, 20]
            delay = retry_delays[min(attempt - 1, len(retry_delays) - 1)]
            print(f"[mosdns] Retrying in {delay}s...", flush=True)
            time.sleep(delay)


def _touch_rules_stamp() -> None:
    path = _mosdns_rules_stamp_path()
    _write_text(path, now_utc_iso() + "\n", mode=0o644)


def _write_dnsmasq_base_config() -> None:
    """
    Generate base dnsmasq configuration with only fallback DNS servers.

    dnsmasq will start with minimal upstream servers (119.29.29.29, 1.0.0.1).
    Additional upstreams (MosDNS, Clash DNS) will be added dynamically when those services become ready.
    """
    config = """# dnsmasq configuration - base startup
# Additional upstreams will be added when services become ready
port=53
no-resolv
# Initial fallback DNS servers (always available)
server=119.29.29.29
server=1.0.0.1
addn-hosts=/etc/etcd_hosts
bogus-priv
strict-order
keep-in-foreground
log-queries=extra
# Enable mDNS (Multicast DNS) via Avahi
enable-dbus=org.freedesktop.Avahi
# Enable reverse DNS (PTR records for local networks)
# Allow RFC 1918 private IP reverse lookups
bogus-priv
# Enable DHCP reverse lookup for local names
local-ttl=1
"""
    _write_text("/etc/dnsmasq.conf", config, mode=0o644)


def _update_dnsmasq_upstreams(add_mosdns: bool = False, add_clash: bool = False) -> None:
    """
    Update dnsmasq upstream servers by rewriting config and reloading.

    This function is called when MosDNS or Clash becomes ready to add them as upstreams.

    Args:
        add_mosdns: Whether to add MosDNS (127.0.0.1#1153) as upstream
        add_clash: Whether to add Clash DNS (127.0.0.1#1053) as upstream

    Note:
        Fallback DNS servers (119.29.29.29, 1.0.0.1) are only added when BOTH
        MosDNS and Clash DNS are NOT active. When both are available, they provide
        complete DNS coverage and fallback servers are unnecessary.
    """
    servers_lines = []
    if add_mosdns:
        servers_lines.append("server=127.0.0.1#1153")
    if add_clash:
        servers_lines.append("server=127.0.0.1#1053")

    # Only add fallback DNS servers when BOTH MosDNS and Clash DNS are inactive
    # If both are active, they provide complete DNS coverage without needing fallback
    if not (add_mosdns and add_clash):
        servers_lines.append("server=119.29.29.29")
        servers_lines.append("server=1.0.0.1")

    servers = "\n".join(servers_lines)

    # Read existing config to preserve comments and settings
    config = f"""# dnsmasq configuration - upstreams updated
# Active upstreams: MosDNS={add_mosdns}, Clash={add_clash}
port=53
no-resolv
{servers}
addn-hosts=/etc/etcd_hosts
bogus-priv
strict-order
keep-in-foreground
log-queries=extra
# Enable mDNS (Multicast DNS) via Avahi
enable-dbus=org.freedesktop.Avahi
# Enable reverse DNS (PTR records for local networks)
# Allow RFC 1918 private IP reverse lookups
bogus-priv
# Enable DHCP reverse lookup for local names
local-ttl=1
"""
    _write_text("/etc/dnsmasq.conf", config, mode=0o644)

    # Reload dnsmasq to apply new upstreams
    try:
        _supervisor_restart("dnsmasq")
        upstreams = []
        if add_mosdns:
            upstreams.append("MosDNS")
        if add_clash:
            upstreams.append("Clash DNS")
        # Only add fallback DNS to the list when it's actually enabled
        if not (add_mosdns and add_clash):
            upstreams.append("Fallback DNS")
        else:
            upstreams.append("Fallback DNS (auto-disabled - both MosDNS and Clash DNS active)")
        print(f"[dnsmasq] Upstreams updated: {', '.join(upstreams)}", flush=True)
    except Exception as e:
        print(f"[dnsmasq] Failed to reload upstreams: {e}", flush=True)


def _write_dnsmasq_config(clash_enabled: bool = False, mosdns_enabled: bool = False) -> None:
    """
    Legacy function - kept for compatibility, but should use _update_dnsmasq_upstreams() instead.
    """
    _update_dnsmasq_upstreams(add_mosdns=mosdns_enabled, add_clash=clash_enabled)


def start_dnsmasq() -> None:
    """
    Start dnsmasq with base configuration (fallback DNS only).

    This should be called first when dnsmasq is enabled.
    Additional upstreams will be added when MosDNS/Clash become ready.
    """
    print("[dnsmasq] ===== STARTING DNSTASQ =====", flush=True)
    _write_dnsmasq_base_config()

    # Check if dnsmasq is already running
    if _supervisor_is_running("dnsmasq"):
        print("[dnsmasq] Already running, restarting to apply new config", flush=True)
        # Already running, restart to apply new config
        try:
            _supervisor_restart("dnsmasq")
            print("[dnsmasq] Successfully restarted with base upstreams: Fallback DNS (119.29.29.29, 1.0.0.1)", flush=True)
        except Exception as e:
            print(f"[dnsmasq] Failed to restart: {e}", flush=True)
            raise
    else:
        print("[dnsmasq] Not running, starting service via supervisorctl start...", flush=True)
        # Not running, start it
        try:
            _supervisor_start("dnsmasq")
            time.sleep(1)  # Give it a moment to start
            # Verify it started successfully
            if _supervisor_is_running("dnsmasq"):
                print("[dnsmasq] ✓ Successfully started with base upstreams: Fallback DNS (119.29.29.29, 1.0.0.1)", flush=True)
            else:
                status = _supervisor_status("dnsmasq")
                print(f"[dnsmasq] ✗ Failed to start! Status: {status}", flush=True)
                raise RuntimeError(f"dnsmasq failed to start, status: {status}")
        except Exception as e:
            print(f"[dnsmasq] ✗ Failed to start: {e}", flush=True)
            raise


def reload_mosdns(node: Dict[str, str], global_cfg: Dict[str, str]) -> None:
    """
    Reload MosDNS configuration and update dnsmasq upstream when ready.

    Startup sequence:
    1. Wait INDEFINITELY for Mihomo to become healthy (if Clash enabled)
    2. Download MosDNS rules via Mihomo proxy
    3. Start MosDNS
    4. Update dnsmasq upstream to include MosDNS (if dnsmasq is enabled)

    Note: dnsmasq is managed independently and must be enabled separately.
    """
    payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": {}}
    out = _run_generator("gen_mosdns", payload)
    with open("/etc/mosdns/config.yaml", "w", encoding="utf-8") as f:
        f.write(out["config_text"])

    # Write MosDNS text files from etcd (always write, even if empty)
    _write_text("/etc/mosdns/etcd_local.txt", out.get("local", ""), mode=0o644)
    _write_text("/etc/mosdns/etcd_block.txt", out.get("block", ""), mode=0o644)
    _write_text("/etc/mosdns/etcd_ddns.txt", out.get("ddns", ""), mode=0o644)
    _write_text("/etc/mosdns/etcd_global.txt", out.get("global", ""), mode=0o644)
    print("[mosdns] wrote etcd text files (local, block, ddns, global)", flush=True)

    # Check if Clash is enabled
    clash_enabled = node.get(f"/nodes/{NODE_ID}/clash/enable") == "true"

    # Step 1: If Clash is enabled, wait INDEFINITELY for Mihomo to become healthy
    if clash_enabled:
        print("[mosdns] Waiting for Mihomo to become healthy (no timeout - will wait indefinitely)...", flush=True)
        wait_for_clash_healthy_infinite()
        print("[mosdns] Mihomo is healthy, proceeding with MosDNS setup", flush=True)

    # Step 2: Download rules (MUST use Clash proxy if Clash is enabled)
    refresh_minutes = out["refresh_minutes"]
    if _should_refresh_rules(refresh_minutes):
        if clash_enabled:
            print("[mosdns] Downloading rules via Mihomo proxy (Clash is enabled)", flush=True)
        else:
            print("[mosdns] Downloading rules directly", flush=True)

        _download_rules_with_backoff(out.get("rules", {}))
        _touch_rules_stamp()

    # Step 3: Start MosDNS
    _supervisor_restart("mosdns")
    print("[mosdns] MosDNS started", flush=True)

    # Step 4: Update dnsmasq upstream to include MosDNS (if dnsmasq is enabled)
    dnsmasq_enabled = node.get(f"/nodes/{NODE_ID}/dnsmasq/enable", "false") == "true"
    if dnsmasq_enabled:
        print("[mosdns] Updating dnsmasq upstream to include MosDNS", flush=True)
        _update_dnsmasq_upstreams(add_mosdns=True, add_clash=clash_enabled)


# ---------- reconcile ----------

def handle_commit() -> None:
    global reconcile_force, tproxy_enabled
    global _clash_refresh_enable, _clash_refresh_interval, _clash_refresh_next
    global _tproxy_check_enabled

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

    # ========== dnsmasq: START FIRST (priority) ==========
    # dnsmasq must start before all other services to provide DNS immediately
    # Additional upstreams (MosDNS/Clash) will be added when they become ready
    dnsmasq_enabled = node.get(f"/nodes/{NODE_ID}/dnsmasq/enable", "false") == "true"
    dnsmasq_material = {
        "enabled": dnsmasq_enabled,
    }
    if changed("dnsmasq", dnsmasq_material):
        print(f"[dnsmasq] Configuration changed: enabled={dnsmasq_enabled}", flush=True)
        if dnsmasq_enabled:
            # Start dnsmasq with base configuration (fallback DNS only)
            # Additional upstreams will be added when MosDNS/Clash become ready
            print("[dnsmasq] dnsmasq enabled, starting FIRST...", flush=True)
            start_dnsmasq()
        else:
            # Stop dnsmasq when explicitly disabled
            print("[dnsmasq] dnsmasq disabled, stopping...", flush=True)
            _supervisor_stop("dnsmasq")
            print("[dnsmasq] Stopped", flush=True)
        did_apply = True
    # ========== dnsmasq: START FIRST (priority) ==========

    mesh_type = global_cfg.get("/global/mesh_type", "easytier")
    if mesh_type == "tinc":
        _supervisor_stop("easytier")
        all_nodes = load_all_nodes()
        tinc_domain = {k: v for k, v in all_nodes.items() if "/tinc/" in k}
        global_tinc = {k: v for k, v in global_cfg.items() if k == "/global/mesh_type" or k.startswith("/global/tinc/")}
        if changed("tinc", {"nodes": tinc_domain, "global": global_tinc}):
            if node.get(f"/nodes/{NODE_ID}/tinc/enable") == "true":
                reload_tinc(node, all_nodes, global_cfg)
            else:
                _supervisor_stop("tinc")
            did_apply = True
    else:
        _supervisor_stop("tinc")
        easytier_domain = {k: v for k, v in node.items() if "/easytier/" in k}
        global_easy = {k: v for k, v in global_cfg.items() if k.startswith("/global/easytier/")}
        if changed("easytier", {"node": easytier_domain, "global": global_easy}):
            if node.get(f"/nodes/{NODE_ID}/easytier/enable") == "true":
                reload_easytier(node, global_cfg)
            else:
                _supervisor_stop("easytier")
            did_apply = True

    openvpn_domain = {k: v for k, v in node.items() if "/openvpn/" in k}
    if changed("openvpn", openvpn_domain):
        changed_ovpn, enabled = reload_openvpn(node)
        did_apply = did_apply or changed_ovpn
        for name in enabled:
            with _ovpn_lock:
                dev = _ovpn_devs.get(name) or (f"tun{name[-1]}" if name and name[-1].isdigit() else f"tun-{name}")
            _write_openvpn_status(name, _compute_openvpn_status(name, dev))

    wireguard_domain = {k: v for k, v in node.items() if "/wireguard/" in k}
    if changed("wireguard", wireguard_domain):
        changed_wg, enabled = reload_wireguard(node)
        did_apply = did_apply or changed_wg
        for name in enabled:
            with _wg_lock:
                dev = _wg_devs.get(name) or _wg_dev_name(name)
            _write_wireguard_status(name, _compute_wireguard_status(name, dev))

    # FRR depends on node routing config + global BGP filter policy
    frr_material = {k: v for k, v in node.items() if (
        "/ospf/" in k or "/bgp/" in k or "/lan/" in k or "/openvpn/" in k or "/wireguard/" in k
    )}
    global_bgp_filter = {k: v for k, v in global_cfg.items() if k.startswith("/global/bgp/filter/")}
    if changed("frr", {"node": frr_material, "global_bgp_filter": global_bgp_filter}):
        payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": load_all_nodes()}
        out = _run_generator("gen_frr", payload)
        reload_frr_smooth(out["frr_conf"])
        did_apply = True

    clash_domain = {k: v for k, v in node.items() if "/clash/" in k}
    global_clash = {k: v for k, v in global_cfg.items() if k.startswith("/global/clash/")}
    if changed("clash", {"node": clash_domain, "global": global_clash}):
        if node.get(f"/nodes/{NODE_ID}/clash/enable") != "true":
            # Stop clash (mihomo) service
            try:
                tproxy_remove()
            except Exception:
                pass
            tproxy_enabled = False
            try:
                _supervisor_stop("mihomo")
            except Exception:
                pass
            with _clash_refresh_lock:
                _clash_refresh_enable = False
            with _tproxy_check_lock:
                _tproxy_check_enabled = False
        else:
            # Check if clash needs restart (mode change or subscription change)
            payload = {"node_id": NODE_ID, "node": node, "global": global_cfg, "all_nodes": {}}
            out = _run_generator("gen_clash", payload)
            new_mode = out["mode"]
            api_controller = out.get("api_controller", "")
            api_secret = out.get("api_secret", "")

            # If switching to/from tproxy mode, need to remove tproxy first
            if tproxy_enabled and new_mode != "tproxy":
                try:
                    tproxy_remove()
                except Exception:
                    pass
                tproxy_enabled = False
                with _clash_monitoring_lock:
                    _clash_monitoring_enabled = False

            # Start clash if not running
            if not _supervisor_is_running("mihomo"):
                _supervisor_start("mihomo")
                # Wait a bit for clash to start
                time.sleep(2)

            # Reload configuration with API credentials
            reload_clash(out["config_yaml"], api_controller=api_controller, api_secret=api_secret)

            # Apply tproxy if needed (MANDATORY wait for Mihomo to be healthy - NO TIMEOUT)
            if new_mode == "tproxy":
                print("[clash] Waiting for Mihomo to become healthy before applying TProxy (no timeout - will wait indefinitely)...", flush=True)
                wait_for_clash_healthy_infinite()

                # Create empty ipset immediately (non-blocking)
                # IPs will be populated asynchronously after TProxy is applied
                print("[clash] Initializing proxy IP ipset...", flush=True)
                _ensure_proxy_ipset()

                tproxy_apply(
                    out["tproxy_targets"],
                    _clash_exclude_src(node),
                    _clash_exclude_ifaces(node),
                    [],  # No individual IPs, using ipset instead
                    _clash_exclude_ports(node, global_cfg),
                )
                _set_cached_tproxy_targets(out["tproxy_targets"])
                tproxy_enabled = True
                with _tproxy_check_lock:
                    _tproxy_check_enabled = True
                with _clash_monitoring_lock:
                    _clash_monitoring_enabled = True
                print("[clash] TProxy applied successfully", flush=True)

                # Start async IP extraction in background thread
                # This won't block TProxy startup
                threading.Thread(target=_update_proxy_ips_async, daemon=True).start()
            else:
                # Not in TProxy mode, cleanup proxy IP ipset
                _cleanup_proxy_ips()

                with _tproxy_check_lock:
                    _tproxy_check_enabled = False
                with _clash_monitoring_lock:
                    _clash_monitoring_enabled = False

            # Update dnsmasq upstream to include Clash DNS (if dnsmasq is enabled)
            dnsmasq_enabled = node.get(f"/nodes/{NODE_ID}/dnsmasq/enable", "false") == "true"
            if dnsmasq_enabled:
                mosdns_enabled = node.get(f"/nodes/{NODE_ID}/mosdns/enable") == "true"
                print("[clash] Updating dnsmasq upstream to include Clash DNS", flush=True)
                _update_dnsmasq_upstreams(add_mosdns=mosdns_enabled, add_clash=True)

            with _clash_refresh_lock:
                _clash_refresh_enable = out["refresh_enable"]
                _clash_refresh_interval = max(0, int(out["refresh_interval_minutes"]))
                _clash_refresh_next = time.time() + (_clash_refresh_interval * 60)
        did_apply = True

    mosdns_enabled = node.get(f"/nodes/{NODE_ID}/mosdns/enable") == "true"
    mosdns_material = {
        "enabled": mosdns_enabled,
        "refresh": node.get(f"/nodes/{NODE_ID}/mosdns/refresh", ""),
        "global": {k: v for k, v in global_cfg.items() if k.startswith("/global/mosdns/")},
    }
    if changed("mosdns", mosdns_material):
        if mosdns_enabled:
            reload_mosdns(node, global_cfg)
        else:
            _supervisor_stop("mosdns")
            # Note: Don't stop dnsmasq here, it's controlled independently
        did_apply = True

    # etcd_hosts: process on every /commit (not watched separately)
    # This ensures etcd_hosts is always synchronized with etcd state
    try:
        update_etcd_hosts()
    except Exception as e:
        print(f"[reconcile] etcd_hosts update failed: {e}", flush=True)

    reconcile_force = False

    if did_apply:
        publish_update("config-applied")


def reconcile_once() -> None:
    if not _reconcile_lock.acquire(blocking=False):
        return
    try:
        handle_commit()
    finally:
        _reconcile_lock.release()


# ---------- watch loop ----------

def watch_loop() -> None:
    backoff = Backoff()
    while True:
        cancel = None
        try:
            try:
                reconcile_once()
            except Exception as e:
                print(f"[reconcile] error: {e}", flush=True)

            backoff.reset()
            events, cancel = _etcd_call(lambda: etcd.watch("/commit"))
            for _ in events:
                try:
                    reconcile_once()
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


def periodic_reconcile_loop() -> None:
    while True:
        time.sleep(300)
        try:
            reconcile_once()
        except Exception as e:
            print(f"[reconcile] periodic error: {e}", flush=True)


# ---------- etcd_hosts ----------
def _load_dns_hosts() -> Dict[str, List[str]]:
    """Load all DNS host records from etcd. Supports multiple IPs per hostname (one per line)."""
    hosts: Dict[str, List[str]] = {}
    try:
        records = load_prefix(ETCD_HOSTS_PREFIX + "/")
        for key, value in records.items():
            # Key format: /dns/hosts/example.com => "192.168.1.1\n192.168.1.2"
            # Extract hostname from key
            if key.startswith(ETCD_HOSTS_PREFIX + "/"):
                hostname = key[len(ETCD_HOSTS_PREFIX + "/"):]
                # Split by newline to support multiple IPs per hostname
                ips = [ip.strip() for ip in value.strip().splitlines() if ip.strip()]
                if hostname and ips:
                    hosts[hostname] = ips
    except Exception as e:
        print(f"[etcd_hosts] failed to load hosts: {e}", flush=True)
    return hosts


def _write_hosts_file(hosts: Dict[str, List[str]]) -> None:
    """Write hosts to /etc/etcd_hosts file. Supports multiple IPs per hostname."""
    try:
        # Sort by hostname for consistent output
        lines = []
        for hostname in sorted(hosts.keys()):
            ips = hosts[hostname]
            # Write each IP on a separate line with the hostname
            for ip in ips:
                lines.append(f"{ip}\t{hostname}")

        content = "\n".join(lines) + "\n" if lines else ""
        total_ips = sum(len(ips) for ips in hosts.values())

        # Always write the file (even if empty) to ensure it exists
        if _write_if_changed(ETCD_HOSTS_PATH, content, mode=0o644):
            print(f"[etcd_hosts] wrote {len(hosts)} hostname(s) with {total_ips} IP(s) to {ETCD_HOSTS_PATH}", flush=True)
        else:
            print(f"[etcd_hosts] file unchanged ({len(hosts)} hostname(s), {total_ips} IP(s))", flush=True)
    except Exception as e:
        print(f"[etcd_hosts] failed to write hosts file: {e}", flush=True)


_etcd_hosts_hash: str = ""


def update_etcd_hosts() -> None:
    """Update etcd_hosts file from etcd records."""
    global _etcd_hosts_hash
    try:
        hosts = _load_dns_hosts()
        current_hash = sha(hosts)

        if current_hash != _etcd_hosts_hash:
            _write_hosts_file(hosts)
            _etcd_hosts_hash = current_hash
            total_ips = sum(len(ips) for ips in hosts.values())
            print(f"[etcd_hosts] updated: {len(hosts)} hostname(s), {total_ips} IP(s)", flush=True)
        else:
            total_ips = sum(len(ips) for ips in hosts.values())
            print(f"[etcd_hosts] no changes: {len(hosts)} hostname(s), {total_ips} IP(s)", flush=True)
    except Exception as e:
        print(f"[etcd_hosts] update failed: {e}", flush=True)


def main() -> None:
    # Initialize empty etcd_hosts file
    try:
        _write_text(ETCD_HOSTS_PATH, "\n", mode=0o644)  # Write newline instead of empty string
        print(f"[init] created {ETCD_HOSTS_PATH}", flush=True)
    except Exception as e:
        print(f"[init] failed to create {ETCD_HOSTS_PATH}: {e}", flush=True)

    threading.Thread(target=keepalive_loop, daemon=True).start()
    threading.Thread(target=openvpn_status_loop, daemon=True).start()
    threading.Thread(target=wireguard_status_loop, daemon=True).start()
    threading.Thread(target=monitor_children_loop, daemon=True).start()
    threading.Thread(target=supervisor_retry_loop, daemon=True).start()
    threading.Thread(target=clash_refresh_loop, daemon=True).start()
    threading.Thread(target=clash_crash_monitor_loop, daemon=True).start()
    threading.Thread(target=clash_proxy_ips_monitor_loop, daemon=True).start()
    threading.Thread(target=tproxy_check_loop, daemon=True).start()
    threading.Thread(target=periodic_reconcile_loop, daemon=True).start()
    # etcd_hosts now processed in reconcile_once() via /commit, no separate watch needed

    publish_update("startup")
    watch_loop()


if __name__ == "__main__":
    main()

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
from typing import Dict, Any, List, Optional, Tuple

import etcd3
import requests
import grpc
from grpc import StatusCode
from urllib.parse import urlparse

NODE_ID = os.environ["NODE_ID"]
TPROXY_PORT = 7893
MOSDNS_SOCKS_PORT = 7891
CLASH_HTTP_PORT = 7890
GEN_DIR = "/generators"

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
_cached_tproxy_exclude: List[str] = []

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
            reload_clash(out["config_yaml"])

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
                    out["tproxy_exclude"],
                    _clash_exclude_src(node),
                    _clash_exclude_ifaces(node),
                    _clash_exclude_ports(node, global_cfg),
                )
                _set_cached_tproxy_exclude(out["tproxy_exclude"])
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


def reload_clash(conf_text: str) -> None:
    """Reload clash config. Returns None if clash is not running."""
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
    exclude_dst: List[str],
    exclude_src: List[str],
    exclude_ifaces: List[str],
    exclude_ports: List[str],
) -> None:
    run(
        f"EXCLUDE_CIDRS='{ ' '.join(exclude_dst) }' "
        f"EXCLUDE_SRC_CIDRS='{ ' '.join(exclude_src) }' "
        f"EXCLUDE_IFACES='{ ' '.join(exclude_ifaces) }' "
        f"EXCLUDE_PORTS='{ ' '.join(exclude_ports) }' "
        f"TPROXY_PORT={TPROXY_PORT} MARK=0x1 TABLE=100 "
        f"/usr/local/bin/tproxy.sh apply"
    )


def tproxy_remove() -> None:
    run(f"TPROXY_PORT={TPROXY_PORT} MARK=0x1 TABLE=100 /usr/local/bin/tproxy.sh remove")


def _get_cached_tproxy_exclude() -> List[str]:
    """Get the cached tproxy exclude list."""
    return list(_cached_tproxy_exclude)


def _set_cached_tproxy_exclude(exclude: List[str]) -> None:
    """Cache the tproxy exclude list."""
    global _cached_tproxy_exclude
    _cached_tproxy_exclude = list(exclude)


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
    exclude_dst: List[str],
    exclude_src: List[str],
    exclude_ifaces: List[str],
    exclude_ports: List[str],
) -> None:
    """Fix tproxy iptables rules by reapplying them."""
    try:
        print(f"[tproxy-check] reapplying iptables rules", flush=True)
        tproxy_apply(exclude_dst, exclude_src, exclude_ifaces, exclude_ports)
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

            # Reapply tproxy rules
            _fix_tproxy_iptables(
                _get_cached_tproxy_exclude(),
                _clash_exclude_src(node),
                _clash_exclude_ifaces(node),
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


def _download_rules(rules: Dict[str, str]) -> None:
    if not rules:
        return

    # Check if Clash is running and use its proxy if available
    proxy = None
    clash_pid_val = clash_pid()
    if clash_pid_val is not None:
        # Clash is running, use its HTTP proxy
        proxy = os.environ.get("MOSDNS_HTTP_PROXY", f"http://127.0.0.1:{CLASH_HTTP_PORT}")
        print(f"[mosdns] Using Clash proxy for rule downloads: {proxy}", flush=True)
    else:
        # Clash not running, check for explicit proxy setting or download direct
        proxy = os.environ.get("MOSDNS_HTTP_PROXY")
        if proxy:
            print(f"[mosdns] Using configured proxy: {proxy}", flush=True)
        else:
            print(f"[mosdns] Clash not running, downloading rules directly (may be slow)", flush=True)

    proxies = {"http": proxy, "https": proxy} if proxy else None
    base_dir = "/etc/mosdns"

    for rel, url in rules.items():
        safe_rel = _safe_rule_path(rel)
        out_path = os.path.join(base_dir, safe_rel)
        os.makedirs(os.path.dirname(out_path), exist_ok=True)

        try:
            if proxies:
                resp = requests.get(url, timeout=30, proxies=proxies)
            else:
                resp = requests.get(url, timeout=30)
            resp.raise_for_status()
            _write_text(out_path, resp.text, mode=0o644)
            print(f"[mosdns] Downloaded rule: {rel}", flush=True)
        except Exception as e:
            print(f"[mosdns] Failed to download {rel}: {e}", flush=True)
            raise


def _download_rules_with_backoff(rules: Dict[str, str]) -> None:
    if not rules:
        return
    backoff = Backoff()
    while True:
        try:
            _download_rules(rules)
            print(f"[mosdns] All rules downloaded successfully", flush=True)
            return
        except Exception as e:
            t = backoff.next_sleep()
            print(f"[mosdns] Rules download failed: {e}; retry in {t:.1f}s", flush=True)
            time.sleep(t)


def _touch_rules_stamp() -> None:
    path = _mosdns_rules_stamp_path()
    _write_text(path, now_utc_iso() + "\n", mode=0o644)


def _write_dnsmasq_config(clash_enabled: bool = False) -> None:
    """Generate dnsmasq configuration for frontend DNS forwarding."""
    # Only include Clash DNS (1053) if Clash is enabled
    # dnsmasq uses # syntax for non-standard ports
    if clash_enabled:
        servers = """server=127.0.0.1#1153
server=127.0.0.1#1053
server=223.5.5.5
server=119.29.29.29"""
    else:
        servers = """server=127.0.0.1#1153
server=223.5.5.5
server=119.29.29.29"""

    config = f"""# dnsmasq configuration for MosDNS frontend
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


def reload_mosdns(node: Dict[str, str], global_cfg: Dict[str, str]) -> None:
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

    # Check if Clash is enabled to configure dnsmasq accordingly
    clash_enabled = node.get(f"/nodes/{NODE_ID}/clash/enable") == "true"

    # Start Avahi for mDNS support (D-Bus should already be running)
    _supervisor_restart("avahi")
    print("[mosdns] avahi-daemon started for mDNS support", flush=True)

    # Start dnsmasq first before downloading rules (so DNS is available during download)
    _write_dnsmasq_config(clash_enabled=clash_enabled)
    _supervisor_restart("dnsmasq")
    if clash_enabled:
        print("[mosdns] dnsmasq started as frontend DNS on port 53 (with Clash DNS)", flush=True)
    else:
        print("[mosdns] dnsmasq started as frontend DNS on port 53", flush=True)

    refresh_minutes = out["refresh_minutes"]
    if _should_refresh_rules(refresh_minutes):
        # If Clash is enabled, wait a bit for it to be ready
        if clash_enabled:
            # Check if Clash is running, if not, wait for it to start
            for attempt in range(10):  # Wait up to 10 seconds
                if clash_pid() is not None:
                    print(f"[mosdns] Clash is ready, downloading rules via proxy", flush=True)
                    time.sleep(1)  # Extra second for proxy to be fully ready
                    break
                print(f"[mosdns] Waiting for Clash to start... (attempt {attempt + 1}/10)", flush=True)
                time.sleep(1)
            else:
                print(f"[mosdns] Clash not ready after 10s, downloading rules directly", flush=True)

        _download_rules_with_backoff(out.get("rules", {}))
        _touch_rules_stamp()

    _supervisor_restart("mosdns")


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

            # If switching to/from tproxy mode, need to remove tproxy first
            if tproxy_enabled and new_mode != "tproxy":
                try:
                    tproxy_remove()
                except Exception:
                    pass
                tproxy_enabled = False

            # Start clash if not running
            if not _supervisor_is_running("mihomo"):
                _supervisor_start("mihomo")
                # Wait a bit for clash to start
                time.sleep(2)

            # Reload configuration
            reload_clash(out["config_yaml"])

            # Apply tproxy if needed
            if new_mode == "tproxy":
                tproxy_apply(
                    out["tproxy_exclude"],
                    _clash_exclude_src(node),
                    _clash_exclude_ifaces(node),
                    _clash_exclude_ports(node, global_cfg),
                )
                _set_cached_tproxy_exclude(out["tproxy_exclude"])
                tproxy_enabled = True
                with _tproxy_check_lock:
                    _tproxy_check_enabled = True
            else:
                with _tproxy_check_lock:
                    _tproxy_check_enabled = False

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
            _supervisor_stop("dnsmasq")  # Stop dnsmasq when MosDNS is disabled
            _supervisor_stop("avahi")  # Stop Avahi when MosDNS is disabled
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
    threading.Thread(target=tproxy_check_loop, daemon=True).start()
    threading.Thread(target=periodic_reconcile_loop, daemon=True).start()
    # etcd_hosts now processed in reconcile_once() via /commit, no separate watch needed

    publish_update("startup")
    watch_loop()


if __name__ == "__main__":
    main()

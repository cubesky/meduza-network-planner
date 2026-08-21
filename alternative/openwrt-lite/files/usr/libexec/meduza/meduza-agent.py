#!/usr/bin/python3
"""Reliable Meduza controller for routed OpenWrt side gateways."""

import json
import os
import re
import signal
import shutil
import stat
import subprocess
import sys
import time
from datetime import datetime, timezone
from urllib.parse import urlparse

sys.path.insert(0, "/usr/lib/meduza-python")

import etcd3
import grpc


STATE = os.environ.get("MEDUZA_STATE", "/var/run/meduza")
DATA = os.environ.get("MEDUZA_DATA", "/etc/meduza")
CACHE = os.environ.get("MEDUZA_CACHE", "/etc/meduza/cache.json")
CACHE_PENDING = os.environ.get("MEDUZA_CACHE_PENDING",
                               "/etc/meduza/cache.pending.json")
MANIFEST = os.environ.get("MEDUZA_MANAGED", "/etc/meduza/managed/interfaces")
REPORTED = os.environ.get("MEDUZA_REPORTED",
                          "/etc/meduza/managed/reported.json")
BUILD_ID_FILE = os.environ.get("MEDUZA_BUILD_ID_FILE",
                               "/usr/share/meduza/openwrt-lite-build")
INSTALL_COMPLETE = os.environ.get(
    "MEDUZA_INSTALL_COMPLETE", "/etc/meduza/managed/install-complete")
UPGRADE_STATE = os.environ.get(
    "MEDUZA_UPGRADE_STATE", "/etc/meduza/managed/upgrade.state")

_active_child = None
_stop_requested = False


class StopRequested(Exception):
    """Raised after a requested shutdown has quiesced the active child."""


def log(message):
    print("meduza: " + message, file=sys.stderr, flush=True)


def _regular_file_text(path):
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise RuntimeError("unsafe package state file: {}".format(path))
    with open(path, encoding="utf-8") as handle:
        return handle.read().rstrip("\n")


def payload_allows_agent():
    """Fail closed before a stale procd registration can mutate the router."""
    build = _regular_file_text(BUILD_ID_FILE)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", build):
        raise RuntimeError("invalid package build identity")
    fields = _regular_file_text(INSTALL_COMPLETE).split("\t")
    if (len(fields) != 3 or fields[0] != "v1" or fields[1] != build
            or not re.fullmatch(r"[0-9a-f]{32}", fields[2])):
        raise RuntimeError("package installation completion seal does not match")
    try:
        state = _regular_file_text(UPGRADE_STATE)
    except FileNotFoundError:
        return
    state_fields = state.split(":")
    if (len(state_fields) != 3 or state_fields[0] != "ready"
            or state_fields[1] != fields[2] or state_fields[2] != build):
        raise RuntimeError("package upgrade is blocked or incomplete")


def _signal_process_group(process, signum):
    try:
        os.killpg(process.pid, signum)
    except (ProcessLookupError, PermissionError):
        pass


def _process_group_exists(pgid):
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def request_stop(_signum, _frame):
    global _stop_requested
    _stop_requested = True
    child = _active_child
    if child is not None and child.poll() is None:
        _signal_process_group(child, signal.SIGTERM)


def run(*args, check=True, capture=False):
    """Run one command and keep it in the agent's shutdown boundary.

    procd signals only the supervised Python PID.  Starting every direct child
    in its own process group lets us forward TERM to a generator and all of its
    helpers, then wait for that group before procd runs service_stopped().
    """
    global _active_child
    if _stop_requested:
        raise StopRequested()
    process = subprocess.Popen(
        args,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        start_new_session=True,
    )
    _active_child = process
    terminate_deadline = None
    try:
        while True:
            try:
                stdout, stderr = process.communicate(timeout=0.25)
                break
            except subprocess.TimeoutExpired:
                if not _stop_requested:
                    continue
                if terminate_deadline is None:
                    _signal_process_group(process, signal.SIGTERM)
                    terminate_deadline = time.monotonic() + 5
                elif time.monotonic() >= terminate_deadline:
                    _signal_process_group(process, signal.SIGKILL)
        if _stop_requested:
            # communicate() only reaps the group leader.  A shell generator
            # can exit from its TERM trap while a sync/init helper is still
            # alive, so keep the supervised agent present until the complete
            # process group is gone.  This is the stop/cleanup serialization
            # boundary used by the init script.
            group_deadline = time.monotonic() + 5
            while _process_group_exists(process.pid):
                if time.monotonic() < group_deadline:
                    _signal_process_group(process, signal.SIGTERM)
                else:
                    _signal_process_group(process, signal.SIGKILL)
                time.sleep(0.1)
    finally:
        _active_child = None
    result = subprocess.CompletedProcess(args, process.returncode, stdout, stderr)
    if _stop_requested:
        raise StopRequested()
    if check and result.returncode:
        raise subprocess.CalledProcessError(result.returncode, args,
                                            output=stdout, stderr=stderr)
    return result


def interruptible_sleep(seconds):
    deadline = time.monotonic() + seconds
    while not _stop_requested:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return
        time.sleep(min(remaining, 0.25))
    raise StopRequested()


def uci_get(option, default=""):
    result = run("uci", "-q", "get", "meduza.main." + option,
                 check=False, capture=True)
    return result.stdout.strip() if result.returncode == 0 else default


class EtcdClient:
    def __init__(self):
        raw = uci_get("ETCD_ENDPOINTS", "https://127.0.0.1:2379")
        self.endpoints = [self._parse(item) for item in raw.split(",") if item.strip()]
        if not self.endpoints:
            raise RuntimeError("UCI option ETCD_ENDPOINTS is required")
        self.user = uci_get("ETCD_USER")
        self.password = uci_get("ETCD_PASS")
        self.ca = uci_get("ETCD_CA") or "/etc/ssl/certs/ca-certificates.crt"
        self.cert = uci_get("ETCD_CERT") or None
        self.key = uci_get("ETCD_KEY") or None
        self.endpoint_index = 0
        self.client = None

    @staticmethod
    def _parse(raw):
        value = raw.strip()
        if "://" not in value:
            value = "https://" + value
        parsed = urlparse(value)
        if not parsed.hostname or not parsed.port:
            raise ValueError("invalid etcd endpoint: {!r}".format(raw))
        return parsed

    def _connect(self, index):
        endpoint = self.endpoints[index]
        secure = endpoint.scheme == "https"
        self.client = etcd3.client(
            host=endpoint.hostname,
            port=endpoint.port,
            ca_cert=self.ca if secure else None,
            cert_cert=self.cert if secure else None,
            cert_key=self.key if secure else None,
            user=self.user or None,
            password=self.password or None,
            timeout=10,
        )

    def _call(self, operation):
        last_error = None
        for offset in range(len(self.endpoints)):
            index = (self.endpoint_index + offset) % len(self.endpoints)
            for auth_attempt in range(2):
                try:
                    if self.client is None or index != self.endpoint_index:
                        self._connect(index)
                    result = operation(self.client)
                    self.endpoint_index = index
                    return result
                except grpc.RpcError as error:
                    last_error = error
                    self.client = None
                    if error.code() == grpc.StatusCode.UNAUTHENTICATED and auth_attempt == 0:
                        continue
                    break
                except (OSError, ValueError) as error:
                    last_error = error
                    self.client = None
                    break
        raise RuntimeError("all etcd endpoints failed: {}".format(last_error))

    def get(self, key):
        value, _metadata = self._call(lambda client: client.get(key))
        return value.decode() if value is not None else ""

    def get_prefix(self, prefix):
        output = {}
        rows = self._call(lambda client: list(client.get_prefix(prefix)))
        for value, metadata in rows:
            output[metadata.key.decode()] = value.decode()
        return output

    def put(self, key, value, lease=None):
        self._call(lambda client: client.put(key, str(value), lease=lease))

    def lease(self, ttl):
        return self._call(lambda client: client.lease(ttl))


def fsync_directory(path):
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    descriptor = os.open(path, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def durable_makedirs(path, mode=0o700):
    absolute = os.path.abspath(path)
    missing = []
    current = absolute
    while not os.path.lexists(current):
        missing.append(current)
        parent = os.path.dirname(current)
        if parent == current:
            break
        current = parent
    if os.path.lexists(current):
        info = os.lstat(current)
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise RuntimeError("unsafe Meduza data directory: " + current)
    for directory in reversed(missing):
        parent = os.path.dirname(directory)
        os.mkdir(directory, mode)
        os.chmod(directory, mode)
        fsync_directory(directory)
        fsync_directory(parent)
    info = os.lstat(absolute)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise RuntimeError("unsafe Meduza data directory: " + absolute)
    os.chmod(absolute, mode)
    # Covers a retry after mkdir succeeded but its parent fsync did not.
    fsync_directory(absolute)
    fsync_directory(os.path.dirname(absolute))


def durable_unlink(path):
    directory = os.path.dirname(path)
    try:
        os.unlink(path)
    except FileNotFoundError:
        if os.path.isdir(directory):
            fsync_directory(directory)
        return
    fsync_directory(directory)


def promote_json(source, target):
    directory = os.path.dirname(target)
    try:
        os.replace(source, target)
    except FileNotFoundError:
        # A previous replace may have succeeded while the directory fsync was
        # interrupted.  The source then no longer exists; re-fsync the target
        # directory instead of losing a successfully promoted LKG snapshot.
        if os.path.exists(source) or not os.path.isfile(target):
            raise
    fsync_directory(directory)


def atomic_json(path, value):
    directory = os.path.dirname(path)
    durable_makedirs(directory, 0o700)
    temporary = path + ".tmp." + str(os.getpid())
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL,
                         0o600)
    os.fchmod(descriptor, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        json.dump(value, handle, separators=(",", ":"), ensure_ascii=False)
        handle.flush()
        os.fsync(handle.fileno())
    try:
        with open(path, "rb") as current, open(temporary, "rb") as candidate:
            unchanged = current.read() == candidate.read()
    except OSError:
        unchanged = False
    if unchanged:
        durable_unlink(temporary)
        return
    promote_json(temporary, path)


def cleanup_atomic_json_temps(path):
    directory = os.path.dirname(path)
    prefix = os.path.basename(path) + ".tmp."
    try:
        entries = os.scandir(directory)
    except OSError:
        return
    with entries:
        for entry in entries:
            if entry.name.startswith(prefix) and entry.is_file(follow_symlinks=False):
                try:
                    os.unlink(entry.path)
                except OSError:
                    pass


def read_names(filename):
    try:
        with open(os.path.join(STATE, filename), encoding="utf-8") as handle:
            return handle.read().split()
    except OSError:
        return []


def read_manifest():
    entries = []
    try:
        with open(MANIFEST, encoding="utf-8") as handle:
            for number, line in enumerate(handle, 1):
                fields = line.rstrip("\n").split("\t")
                if not line.strip():
                    continue
                if len(fields) != 5:
                    log("ignoring invalid managed manifest line {}".format(number))
                    continue
                entries.append(tuple(fields))
    except OSError:
        pass
    return entries


def iter_process_argv(executable_names):
    try:
        entries = os.scandir("/proc")
    except OSError:
        return
    with entries:
        for entry in entries:
            if not entry.name.isdigit():
                continue
            try:
                executable = os.path.basename(os.readlink(
                    os.path.join(entry.path, "exe")))
                if executable.endswith(" (deleted)"):
                    executable = executable[:-10]
                if executable not in executable_names:
                    continue
                with open(os.path.join(entry.path, "cmdline"), "rb") as handle:
                    argv = [item.decode(errors="replace") for item in
                            handle.read().split(b"\0") if item]
            except OSError:
                continue
            yield argv


def argv_option_matches(argv, short, long, value):
    for index, argument in enumerate(argv):
        if argument in (short, long) and index + 1 < len(argv):
            if argv[index + 1] == value:
                return True
        if argument in (short + value, long + "=" + value):
            return True
    return False


def openvpn_process_running(logical, config):
    directory, basename = os.path.split(config)
    native = "/var/run/openvpn.{}.conf".format(logical)
    for argv in iter_process_argv({"openvpn"}):
        if argv_option_matches(argv, "--config", "--config", config):
            return True
        if argv_option_matches(argv, "--config", "--config", native):
            return True
        if (argv_option_matches(argv, "--cd", "--cd", directory)
                and argv_option_matches(argv, "--config", "--config", basename)):
            return True
    return False


def tinc_process_running(name, config):
    config_dir = os.path.dirname(config)
    for argv in iter_process_argv({"tincd"}):
        if (argv_option_matches(argv, "-n", "--net", name)
                and argv_option_matches(argv, "-c", "--config", config_dir)):
            return True
    return False


def frr_process_running():
    return next(iter_process_argv({"zebra", "watchfrr"}), None) is not None


def link_up(device):
    if not device:
        return False
    result = run("ip", "-o", "link", "show", "dev", device,
                 check=False, capture=True)
    return result.returncode == 0 and "UP" in result.stdout.split("<", 1)[-1].split(">", 1)[0].split(",")


def interface_up(logical):
    if not logical or shutil.which("ifstatus") is None:
        return False
    result = run("ifstatus", logical, check=False, capture=True)
    if result.returncode:
        return False
    try:
        return bool(json.loads(result.stdout).get("up"))
    except (ValueError, AttributeError):
        return False


def openvpn_state(name, logical, device, config):
    if not os.path.isfile(config):
        return "down"
    running = openvpn_process_running(logical, config)
    if link_up(device) and (interface_up(logical) or running):
        return "up"
    return "connecting" if running else "down"


def read_reported(node_id):
    try:
        with open(REPORTED, encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, ValueError):
        return set()
    if (not isinstance(value, dict) or value.get("version") != 1
            or value.get("node_id") != node_id):
        return set()
    rows = value.get("interfaces")
    if not isinstance(rows, list):
        return set()
    return {
        (row[0], row[1]) for row in rows
        if isinstance(row, list) and len(row) == 2
        and row[0] in ("openvpn", "wireguard")
        and isinstance(row[1], str)
    }


def wireguard_state(device):
    if shutil.which("wg") is None:
        return "unavailable"
    if not link_up(device):
        return "down"
    result = run("wg", "show", device, "latest-handshakes",
                 check=False, capture=True)
    latest = max([int(line.split()[1]) for line in result.stdout.splitlines()
                  if len(line.split()) > 1] or [0])
    return "up" if latest and time.time() - latest <= 180 else "connecting"


def timestamp():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S+0000")


class Agent:
    def __init__(self):
        self.node = uci_get("NODE_ID")
        if not self.node:
            raise RuntimeError("UCI option NODE_ID is required")
        if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", self.node):
            raise RuntimeError("UCI option NODE_ID contains unsafe characters")
        self.etcd = EtcdClient()
        self.commit = None
        self.initialized = False
        self.next_report = 0
        self.cache_retry_at = 0
        self.cache_retry_delay = 1
        self.cache_stop_done = False

    def write_runtime(self, node, global_config, all_nodes):
        atomic_json(os.path.join(STATE, "node.json"), node)
        atomic_json(os.path.join(STATE, "global.json"), global_config)
        atomic_json(os.path.join(STATE, "all-nodes.json"), all_nodes)

    @staticmethod
    def _cache_source():
        if os.path.isfile(CACHE):
            return CACHE
        if os.path.isfile(CACHE_PENDING):
            return CACHE_PENDING
        return None

    def restore_cache(self):
        cleanup_atomic_json_temps(CACHE)
        cleanup_atomic_json_temps(CACHE_PENDING)
        source = self._cache_source()
        if source is None:
            if not self.cache_stop_done:
                log("no persistent configuration cache; waiting for etcd")
                # UCI or a runtime may survive an interrupted first install
                # even if no JSON snapshot was durably created.
                result = run("/usr/libexec/meduza/meduza-generator",
                             "--runtime-stop", check=False)
                if result.returncode == 0:
                    self.cache_stop_done = True
                else:
                    self.cache_retry_at = (time.monotonic()
                                           + self.cache_retry_delay)
                    self.cache_retry_delay = min(self.cache_retry_delay * 2,
                                                 60)
            return False
        try:
            with open(source, encoding="utf-8") as handle:
                cached = json.load(handle)
            if cached.get("version") != 1 or cached.get("node_id") != self.node:
                raise ValueError("cache belongs to a different node or version")
            node = cached["node"]
            global_config = cached["global"]
            all_nodes = cached["all_nodes"]
            if not all(isinstance(value, dict)
                       for value in (node, global_config, all_nodes)):
                raise ValueError("cached etcd values are invalid")
            self.write_runtime(node, global_config, all_nodes)
            run("/usr/libexec/meduza/meduza-generator", "--apply")
            if source == CACHE_PENDING:
                promote_json(CACHE_PENDING, CACHE)
            else:
                durable_unlink(CACHE_PENDING)
            self.commit = cached.get("commit", "")
            self.initialized = True
            self.cache_retry_delay = 1
            log("restored persistent last-known-good configuration")
            return True
        except Exception as error:
            log("persistent configuration cache was not restored: {}".format(error))
            run("/usr/libexec/meduza/meduza-generator", "--runtime-stop",
                check=False)
            self.cache_retry_at = time.monotonic() + self.cache_retry_delay
            self.cache_retry_delay = min(self.cache_retry_delay * 2, 60)
            return False

    def reconcile(self, commit):
        node = self.etcd.get_prefix("/nodes/{}/".format(self.node))
        global_config = self.etcd.get_prefix("/global/")
        all_nodes = self.etcd.get_prefix("/nodes/")
        payload = {
            "version": 1,
            "node_id": self.node,
            "commit": commit,
            "node": node,
            "global": global_config,
            "all_nodes": all_nodes,
        }
        # The pending snapshot is durable before the shell transaction starts.
        # It closes the first-install power-loss window between UCI commit and
        # publishing the stable last-known-good cache.
        atomic_json(CACHE_PENDING, payload)
        self.write_runtime(node, global_config, all_nodes)
        run("/usr/libexec/meduza/meduza-generator", "--apply")
        promote_json(CACHE_PENDING, CACHE)
        self.etcd.put("/updated/{}/last".format(self.node), timestamp())
        log("configuration reconciled")

    def report(self):
        cleanup_atomic_json_temps(REPORTED)
        lease = self.etcd.lease(60)
        if lease:
            self.etcd.put("/updated/{}/online".format(self.node), "1", lease)
        states = {}
        current_reported = set()
        tinc_seen = False
        for kind, name, logical, device, config in read_manifest():
            if kind == "openvpn":
                state = openvpn_state(name, logical, device, config)
                current_reported.add((kind, name))
            elif kind == "wireguard":
                state = wireguard_state(device)
                current_reported.add((kind, name))
            elif kind == "tinc":
                tinc_seen = True
                running = tinc_process_running(name, config)
                state = "up" if running and link_up(device) else "down"
            else:
                continue
            states[(kind, "default" if kind == "tinc" else name)] = state
        if not tinc_seen:
            states[("tinc", "default")] = "down"
        states[("frr", "default")] = "up" if frr_process_running() else "down"
        for removed in read_reported(self.node) - current_reported:
            states[removed] = "down"
        now = timestamp()
        for (kind, name), state in sorted(states.items()):
            self.etcd.put("/updated/{}/{}/{}/status".format(self.node, kind, name),
                          "{} {}".format(state, now))
        atomic_json(REPORTED, {
            "version": 1,
            "node_id": self.node,
            "interfaces": [list(row) for row in sorted(current_reported)],
        })

    def serve(self):
        self.restore_cache()
        delay = 1
        while not _stop_requested:
            try:
                if (not self.initialized
                        and (self._cache_source() is not None
                             or not self.cache_stop_done)
                        and time.monotonic() >= self.cache_retry_at):
                    self.restore_cache()
                commit = self.etcd.get("/commit")
                if not self.initialized or commit != self.commit:
                    try:
                        self.reconcile(commit)
                    except Exception:
                        # A durable pending snapshot now represents a partial
                        # transaction.  Retry it when no stable cache exists;
                        # otherwise re-apply stable LKG and discard pending.
                        self.initialized = False
                        raise
                    self.commit = commit
                    self.initialized = True
                if time.monotonic() >= self.next_report:
                    self.report()
                    self.next_report = time.monotonic() + 15
                delay = 1
                interruptible_sleep(5)
            except StopRequested:
                raise
            except Exception as error:
                log("operation failed: {}; retrying in {}s".format(error, delay))
                interruptible_sleep(delay)
                delay = min(delay * 2, 60)


def main():
    os.umask(0o077)
    try:
        payload_allows_agent()
    except (OSError, RuntimeError, UnicodeError) as error:
        log("refusing to start: {}".format(error))
        raise SystemExit(1)
    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    try:
        os.makedirs(STATE, mode=0o700, exist_ok=True)
        os.chmod(STATE, 0o700)
        Agent().serve()
    except StopRequested:
        log("shutdown requested; active reconciliation stopped")


if __name__ == "__main__":
    main()

#!/usr/bin/python3
"""Reliable Meduza controller for routed OpenWrt side gateways."""

import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from urllib.parse import urlparse

sys.path.insert(0, "/usr/lib/meduza-python")

import etcd3
import grpc


STATE = os.environ.get("MEDUZA_STATE", "/var/run/meduza")


def log(message):
    print("meduza: " + message, file=sys.stderr, flush=True)


def run(*args, check=True, capture=False):
    return subprocess.run(args, check=check, text=True,
                          stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.PIPE if capture else None)


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


def atomic_json(path, value):
    temporary = path + ".tmp." + str(os.getpid())
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(value, handle, separators=(",", ":"), ensure_ascii=False)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def read_names(filename):
    try:
        with open(os.path.join(STATE, filename), encoding="utf-8") as handle:
            return handle.read().split()
    except OSError:
        return []


def process_running(pattern):
    return run("pgrep", "-f", pattern, check=False,
               capture=True).returncode == 0


def link_up(device):
    if not device:
        return False
    result = run("ip", "-o", "link", "show", "dev", device,
                 check=False, capture=True)
    return result.returncode == 0 and "UP" in result.stdout.split("<", 1)[-1].split(">", 1)[0].split(",")


def openvpn_state(name):
    path = "/etc/openvpn/meduza-{}.conf".format(name)
    device = ""
    try:
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                parts = line.split()
                if len(parts) == 2 and parts[0] == "dev":
                    device = parts[1]
                    break
    except OSError:
        return "down"
    if not process_running("openvpn.*meduza-{}.conf".format(name)):
        return "down"
    return "up" if link_up(device) else "connecting"


def wireguard_state(name):
    try:
        with open(os.path.join(STATE, "wireguard.{}.dev".format(name)), encoding="utf-8") as handle:
            device = handle.read().strip()
    except OSError:
        return "down"
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
        self.etcd = EtcdClient()
        self.commit = None
        self.initialized = False
        self.next_report = 0

    def reconcile(self):
        atomic_json(os.path.join(STATE, "node.json"),
                    self.etcd.get_prefix("/nodes/{}/".format(self.node)))
        atomic_json(os.path.join(STATE, "global.json"), self.etcd.get_prefix("/global/"))
        atomic_json(os.path.join(STATE, "all-nodes.json"), self.etcd.get_prefix("/nodes/"))
        run("/usr/libexec/meduza/meduza-generator", "--apply")
        self.etcd.put("/updated/{}/last".format(self.node), timestamp())
        log("configuration reconciled")

    def report(self):
        lease = self.etcd.lease(60)
        if lease:
            self.etcd.put("/updated/{}/online".format(self.node), "1", lease)
        states = []
        states.extend(("openvpn", name, openvpn_state(name)) for name in read_names("openvpn.instances"))
        states.extend(("wireguard", name, wireguard_state(name)) for name in read_names("wireguard.instances"))
        states.append(("tinc", "default", "up" if process_running("tincd") else "down"))
        states.append(("frr", "default", "up" if process_running("/usr/lib/frr/(zebra|watchfrr)") else "down"))
        now = timestamp()
        for kind, name, state in states:
            self.etcd.put("/updated/{}/{}/{}/status".format(self.node, kind, name),
                          "{} {}".format(state, now))

    def serve(self):
        delay = 1
        while True:
            try:
                commit = self.etcd.get("/commit")
                if not self.initialized or commit != self.commit:
                    self.reconcile()
                    self.commit = commit
                    self.initialized = True
                if time.monotonic() >= self.next_report:
                    self.report()
                    self.next_report = time.monotonic() + 15
                delay = 1
                time.sleep(5)
            except Exception as error:
                log("operation failed: {}; retrying in {}s".format(error, delay))
                time.sleep(delay)
                delay = min(delay * 2, 60)


def main():
    os.makedirs(STATE, exist_ok=True)
    Agent().serve()


if __name__ == "__main__":
    main()

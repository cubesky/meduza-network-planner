"""Small, dependency-free model of Meduza's OpenWrt resource lifecycle.

This is deliberately not an implementation shared by the package.  It is a
test oracle: production scripts are expected to have the same observable
ownership, idempotency and cold-boot behaviour.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import copy
import re
from typing import Dict, Iterable, MutableMapping, Set, Tuple


DEVICE_RE = re.compile(r"^[A-Za-z0-9_.-]{1,15}$")
UCI_RE = re.compile(r"^[A-Za-z0-9_]+$")
RESERVED_DEVICES = frozenset({"lo", "utun"})
OWNER = "meduza-openwrt-lite"


class OwnershipError(RuntimeError):
    """Raised before changing state when a desired resource is not ours."""


@dataclass(frozen=True)
class DesiredInterface:
    kind: str
    instance: str
    logical: str
    device: str
    config: str
    files: Tuple[str, ...] = ()

    def validate(self) -> None:
        if self.kind not in {"tinc", "openvpn", "wireguard"}:
            raise ValueError("unsupported VPN kind: {}".format(self.kind))
        if not UCI_RE.fullmatch(self.logical):
            raise ValueError("invalid UCI interface name: {}".format(self.logical))
        if not DEVICE_RE.fullmatch(self.device):
            raise ValueError("invalid Linux device name: {}".format(self.device))
        if self.device in RESERVED_DEVICES:
            raise OwnershipError("reserved device: {}".format(self.device))


@dataclass
class UciInterface:
    device: str
    owner: str = OWNER
    proto: str = "none"


@dataclass
class FakeOpenWrt:
    """Stateful fake used to express the lifecycle's external contract."""

    firewall_zone: str = "vpn"
    interfaces: MutableMapping[str, UciInterface] = field(default_factory=dict)
    devices: MutableMapping[str, str] = field(default_factory=dict)
    files: MutableMapping[str, str] = field(default_factory=dict)
    processes: Set[Tuple[str, str]] = field(default_factory=set)
    zone_members: Set[str] = field(default_factory=set)
    persistent_manifest: MutableMapping[str, DesiredInterface] = field(default_factory=dict)
    pending_manifest: MutableMapping[str, DesiredInterface] = field(default_factory=dict)
    persistent_cache: Tuple[DesiredInterface, ...] = ()
    volatile_desired: Tuple[DesiredInterface, ...] = ()
    network_reloads: int = 0
    firewall_reloads: int = 0

    @staticmethod
    def _key(item: DesiredInterface) -> str:
        return "{}:{}".format(item.kind, item.instance)

    def _validate_transaction(self, desired: Iterable[DesiredInterface]) -> Tuple[DesiredInterface, ...]:
        items = tuple(desired)
        logicals: Dict[str, DesiredInterface] = {}
        devices: Dict[str, DesiredInterface] = {}
        for item in items:
            item.validate()
            if item.logical in logicals:
                raise ValueError("duplicate UCI interface: {}".format(item.logical))
            if item.device in devices:
                raise ValueError("duplicate Linux device: {}".format(item.device))
            logicals[item.logical] = item
            devices[item.device] = item

            current_uci = self.interfaces.get(item.logical)
            if current_uci is not None and current_uci.owner != OWNER:
                raise OwnershipError("user-owned UCI section: {}".format(item.logical))

            current_device_owner = self.devices.get(item.device)
            if current_device_owner not in (None, OWNER):
                raise OwnershipError("user-owned Linux device: {}".format(item.device))
        return items

    def reconcile(self, desired: Iterable[DesiredInterface], *, persist_cache: bool = True) -> None:
        """Atomically reconcile desired state and count externally visible reloads."""

        items = self._validate_transaction(desired)
        old_network = {
            name: (section.device, section.owner, section.proto)
            for name, section in self.interfaces.items()
        }
        old_zone = set(self.zone_members)

        wanted_keys = {self._key(item) for item in items}
        for key, previous in tuple(self.persistent_manifest.items()):
            if key in wanted_keys:
                continue
            self.interfaces.pop(previous.logical, None)
            if self.devices.get(previous.device) == OWNER:
                self.devices.pop(previous.device, None)
            self.processes.discard((previous.kind, previous.instance))
            self.zone_members.discard(previous.logical)
            for path in previous.files:
                if self.files.get(path) == OWNER:
                    self.files.pop(path, None)
            self.persistent_manifest.pop(key, None)

        for item in items:
            self.interfaces[item.logical] = UciInterface(item.device)
            self.devices[item.device] = OWNER
            self.processes.add((item.kind, item.instance))
            self.zone_members.add(item.logical)
            for path in item.files:
                self.files[path] = OWNER
            self.persistent_manifest[self._key(item)] = item

        self.volatile_desired = items
        if persist_cache:
            # Copy represents an atomic replace of a last-known-good cache.
            self.persistent_cache = copy.deepcopy(items)

        new_network = {
            name: (section.device, section.owner, section.proto)
            for name, section in self.interfaces.items()
        }
        if new_network != old_network:
            self.network_reloads += 1
        if self.zone_members != old_zone:
            self.firewall_reloads += 1

    def power_loss(self) -> None:
        """Drop RAM/runtime state while preserving flash and user resources."""

        self.volatile_desired = ()
        self.processes.clear()
        for device, owner in tuple(self.devices.items()):
            if owner == OWNER:
                self.devices.pop(device)

    def restore_last_known_good(self) -> None:
        self.recover_pending_apply()
        if self.persistent_cache:
            self.reconcile(self.persistent_cache, persist_cache=False)

    def simulate_interrupted_apply(self, desired: Iterable[DesiredInterface]) -> None:
        """Create a pending journal and make one partial owned change, then crash."""

        items = self._validate_transaction(desired)
        self.pending_manifest = {self._key(item): item for item in items}
        if not items:
            return
        first = items[0]
        self.interfaces[first.logical] = UciInterface(first.device)
        self.devices[first.device] = OWNER
        self.processes.add((first.kind, first.instance))
        self.zone_members.add(first.logical)
        for path in first.files:
            self.files[path] = OWNER

    def recover_pending_apply(self) -> None:
        """Roll back journaled resources not present in the committed manifest."""

        for key, item in tuple(self.pending_manifest.items()):
            stable = self.persistent_manifest.get(key)
            if stable == item:
                continue
            section = self.interfaces.get(item.logical)
            if section is not None and section.owner == OWNER:
                self.interfaces.pop(item.logical)
            if self.devices.get(item.device) == OWNER:
                self.devices.pop(item.device)
            self.processes.discard((item.kind, item.instance))
            self.zone_members.discard(item.logical)
            for path in item.files:
                if self.files.get(path) == OWNER:
                    self.files.pop(path)
        self.pending_manifest.clear()

    def runtime_stop(self) -> None:
        """Stop runtime links/processes, preserving flash state for restart."""

        managed = tuple(self.persistent_manifest.values())
        for item in managed:
            if self.devices.get(item.device) == OWNER:
                self.devices.pop(item.device)
            self.processes.discard((item.kind, item.instance))
        self.volatile_desired = ()

    def purge(self) -> None:
        """Remove every persistent resource, but never another owner's data."""

        self.runtime_stop()
        old_network = {
            name: (section.device, section.owner, section.proto)
            for name, section in self.interfaces.items()
        }
        old_zone = set(self.zone_members)
        for item in tuple(self.persistent_manifest.values()):
            section = self.interfaces.get(item.logical)
            if section is not None and section.owner == OWNER:
                self.interfaces.pop(item.logical)
            self.zone_members.discard(item.logical)
            for path in item.files:
                if self.files.get(path) == OWNER:
                    self.files.pop(path)
        self.persistent_manifest.clear()
        self.pending_manifest.clear()

        new_network = {
            name: (section.device, section.owner, section.proto)
            for name, section in self.interfaces.items()
        }
        if new_network != old_network:
            self.network_reloads += 1
        if self.zone_members != old_zone:
            self.firewall_reloads += 1

    def manifest_text(self) -> str:
        """Serialize the production five-column TSV ownership manifest."""

        lines = []
        for key in sorted(self.persistent_manifest):
            item = self.persistent_manifest[key]
            lines.append(
                "\t".join(
                    (item.kind, item.instance, item.logical, item.device, item.config)
                )
            )
        return "\n".join(lines) + ("\n" if lines else "")

    @staticmethod
    def parse_manifest(text: str) -> Tuple[DesiredInterface, ...]:
        items = []
        for line_number, line in enumerate(text.splitlines(), 1):
            if not line:
                continue
            columns = line.split("\t")
            if len(columns) != 5:
                raise ValueError("manifest line {} does not have five columns".format(line_number))
            kind, instance, logical, device, config = columns
            items.append(DesiredInterface(kind, instance, logical, device, config))
        return tuple(items)


def three_vpns() -> Tuple[DesiredInterface, ...]:
    return (
        DesiredInterface(
            "tinc",
            "mesh",
            "tinc_mesh",
            "tnc0",
            "/etc/tinc/mesh/tinc.conf",
            ("/etc/tinc/mesh/tinc.conf", "/etc/tinc/mesh/tinc-up"),
        ),
        DesiredInterface(
            "openvpn",
            "site_a",
            "ovpn_site_a",
            "ovpn-site-a",
            "/etc/openvpn/meduza-site_a.conf",
            ("/etc/openvpn/meduza-site_a.conf",),
        ),
        DesiredInterface(
            "wireguard",
            "site_b",
            "wg_site_b",
            "wg-site-b",
            "/etc/meduza/wireguard/site_b.conf",
            ("/etc/meduza/wireguard/site_b.conf",),
        ),
    )

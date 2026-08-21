#!/usr/bin/python3
"""Small UCI CLI facade backed by an isolated rpcd UCI session.

The normal ``uci`` CLI always keeps /tmp/.uci in its delta search path, even
when ``-t`` is used.  A private rpcd session replaces that search path.  The
helper records external section ownership, validates its byte/semantic
baseline, and commits only rpcd scalar or true LIST_ADD/LIST_DEL deltas.  It
never replaces or reconstructs an entire firewall zone membership list.
"""

import fcntl
import hashlib
import json
import os
import re
import secrets
import stat
import subprocess
import sys


SESSION_RE = re.compile(r"^[0-9a-f]{32}$")
NAME_RE = re.compile(r"^[A-Za-z0-9_]+$")
TYPE_RE = re.compile(r"^[A-Za-z0-9_-]+$")
CONFIG_DIR = os.environ.get("MEDUZA_UCI_CONFDIR", "/etc/config")
BASELINE_DIR = os.environ.get(
    "MEDUZA_UCI_BASELINE_DIR", "/var/run/meduza/uci-baseline"
)
OWNERSHIP_PATH = os.environ.get(
    "MEDUZA_UCI_OWNERSHIP", "/etc/meduza/managed/uci-ownership.json"
)
MANIFEST_PATH = os.environ.get(
    "MEDUZA_MANAGED_MANIFEST", "/etc/meduza/managed/interfaces"
)
MIGRATION_PATH = os.environ.get(
    "MEDUZA_UCI_MIGRATION", "/etc/meduza/managed/uci-migration.interfaces"
)
MIGRATION_ZONE_PATH = os.environ.get(
    "MEDUZA_UCI_MIGRATION_ZONE", "/etc/meduza/managed/uci-migration.zone"
)
MIGRATION_ZONE_SEAL_PATH = os.environ.get(
    "MEDUZA_UCI_MIGRATION_ZONE_SEAL",
    "/etc/meduza/managed/uci-migration.zone.seal",
)
MIGRATION_ZONE_DISABLED_PATH = os.environ.get(
    "MEDUZA_UCI_MIGRATION_ZONE_DISABLED",
    "/etc/meduza/managed/uci-migration.zone.disabled",
)
UPGRADE_INTENT_PATH = os.environ.get(
    "MEDUZA_UPGRADE_INTENT", "/etc/meduza/managed/upgrade.intent"
)
LEGACY_AUTH_PATH = os.environ.get(
    "MEDUZA_LEGACY_AUTH", "/etc/meduza/managed/legacy.interfaces"
)
RPCD_SAVEDIR_PREFIX = os.environ.get(
    "MEDUZA_RPCD_UCI_PREFIX", "/var/run/rpcd/uci-"
)
MEDUZA_OWNER = "meduza-openwrt-lite"
GENERATED_ROOT = os.environ.get("MEDUZA_GENERATED_ROOT", "/etc/meduza/generated")


class UbusError(RuntimeError):
    pass


def verify_uci_not_found_context(request):
    """Distinguish a missing UCI value from a dead rpcd object/session."""
    listing = subprocess.run(
        ["ubus", "list", "uci"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if listing.returncode or "uci" not in listing.stdout.split():
        raise UbusError("rpcd UCI object is unavailable")
    session = request.get("ubus_rpc_session")
    if not isinstance(session, str):
        raise UbusError("UCI request omitted rpcd session")
    probe = subprocess.run(
        ["ubus", "-S", "call", "session", "get",
         json.dumps({"ubus_rpc_session": session}, separators=(",", ":"))],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if probe.returncode:
        detail = probe.stderr.strip() or probe.stdout.strip() or "session unavailable"
        raise UbusError("rpcd UCI session is unavailable: {}".format(detail))


def ubus(object_name, method, request, quiet=False):
    process = subprocess.run(
        ["ubus", "-S", "call", object_name, method,
         json.dumps(request, separators=(",", ":"))],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if process.returncode:
        detail = process.stderr.strip() or process.stdout.strip() or "ubus call failed"
        # ubus returns UBUS_STATUS_NOT_FOUND (4) for a missing package,
        # section or option.  Quiet mode suppresses only that exact semantic
        # condition; permission, timeout, transport and parse failures remain
        # fatal so ownership checks never fail open.
        if quiet and process.returncode == 4:
            if object_name == "uci":
                verify_uci_not_found_context(request)
            return None
        raise UbusError("{} {}: {}".format(object_name, method, detail))
    if not process.stdout.strip():
        return {}
    try:
        value = json.loads(process.stdout)
    except json.JSONDecodeError as error:
        raise UbusError("invalid ubus JSON response: {}".format(error)) from error
    if not isinstance(value, dict):
        raise UbusError("unexpected ubus response type")
    return value


def validate_session(value):
    if not SESSION_RE.fullmatch(value):
        raise UbusError("invalid rpcd session identifier")
    return value


def validate_name(value, kind="name"):
    pattern = TYPE_RE if kind == "type" else NAME_RE
    if not pattern.fullmatch(value):
        raise UbusError("invalid UCI {}: {}".format(kind, value))
    return value


def fsync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def ensure_private_directory(path):
    parent = os.path.dirname(path)
    existed = os.path.isdir(path)
    os.makedirs(path, mode=0o700, exist_ok=True)
    current = os.lstat(path)
    if not stat.S_ISDIR(current.st_mode) or stat.S_ISLNK(current.st_mode):
        raise UbusError("unsafe UCI transaction state directory")
    os.chmod(path, 0o700)
    if not existed and parent:
        fsync_directory(parent)
    fsync_directory(path)


def file_digest(path, missing_ok=True):
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except FileNotFoundError:
        if missing_ok:
            return {
                "exists": False,
                "size": 0,
                "sha256": hashlib.sha256(b"").hexdigest(),
            }
        raise
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode):
            raise UbusError("UCI configuration is not a regular file: {}".format(path))
        digest = hashlib.sha256()
        size = 0
        while True:
            block = os.read(descriptor, 65536)
            if not block:
                break
            digest.update(block)
            size += len(block)
    finally:
        os.close(descriptor)
    return {"exists": True, "size": size, "sha256": digest.hexdigest()}


def package_digest(package):
    return file_digest(os.path.join(CONFIG_DIR, validate_name(package, "package")))


def baseline_path(session):
    return os.path.join(BASELINE_DIR, validate_session(session) + ".json")


def atomic_json(path, value):
    ensure_private_directory(os.path.dirname(path))
    temporary = path + ".tmp." + str(os.getpid())
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        offset = 0
        while offset < len(payload):
            offset += os.write(descriptor, payload[offset:])
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    os.replace(temporary, path)
    fsync_directory(os.path.dirname(path))


def save_baseline(session, packages, state, digests=None):
    if digests is None:
        digests = {package: package_digest(package) for package in packages}
    atomic_json(
        baseline_path(session),
        {
            "version": 2,
            "session": session,
            "packages": digests,
            "state": state,
        },
    )


def load_baseline(session):
    with open(baseline_path(session), encoding="utf-8") as handle:
        value = json.load(handle)
    if not isinstance(value, dict) or value.get("version") != 2:
        raise UbusError("invalid UCI transaction baseline")
    if (
        value.get("session") != session
        or not isinstance(value.get("packages"), dict)
        or not isinstance(value.get("state"), dict)
    ):
        raise UbusError("UCI transaction baseline does not match the session")
    packages = value["packages"]
    for package, digest in packages.items():
        validate_name(package, "package")
        if not isinstance(digest, dict) or not isinstance(digest.get("sha256"), str):
            raise UbusError("invalid UCI package baseline")
    return value


def remove_baseline(session):
    path = baseline_path(session)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    if os.path.isdir(BASELINE_DIR):
        fsync_directory(BASELINE_DIR)


def new_session(packages):
    for package in packages:
        validate_name(package, "package")
    reply = ubus("session", "create", {"timeout": 300})
    session = validate_session(str(reply.get("ubus_rpc_session", "")))
    permissions = []
    for package in packages:
        permissions.extend(([package, "read"], [package, "write"]))
    try:
        ubus(
            "session",
            "grant",
            {
                "ubus_rpc_session": session,
                "scope": "uci",
                "objects": permissions,
            },
        )
    except Exception:
        ubus("session", "destroy", {"ubus_rpc_session": session}, quiet=True)
        raise
    return session


def capture_session(packages):
    for _attempt in range(5):
        session = new_session(packages)
        try:
            before = {package: package_digest(package) for package in packages}
            state = session_state(session, packages)
            after = {package: package_digest(package) for package in packages}
            if before != after:
                raise UbusError("UCI changed while capturing transaction baseline")
            save_baseline(session, packages, state, before)
            return session, state, before
        except UbusError as error:
            ubus("session", "destroy", {"ubus_rpc_session": session}, quiet=True)
            remove_baseline(session)
            if "capturing transaction baseline" not in str(error):
                raise
        except Exception:
            ubus("session", "destroy", {"ubus_rpc_session": session}, quiet=True)
            remove_baseline(session)
            raise
    raise UbusError("UCI kept changing while capturing transaction baseline")


def create_session(packages):
    session, _state, _digests = capture_session(packages)
    print(session)


def canonical_section(value):
    """Return a stable, type-sensitive representation of one UCI section."""
    if value is None:
        return None
    if not isinstance(value, dict) or not isinstance(value.get(".type"), str):
        raise UbusError("invalid UCI section state")
    options = {}
    for name in sorted(key for key in value if not key.startswith(".")):
        validate_name(name, "option")
        option = value[name]
        if isinstance(option, list):
            options[name] = {"kind": "list", "value": [str(item) for item in option]}
        elif option is not None:
            options[name] = {"kind": "scalar", "value": str(option)}
    return {
        "type": validate_name(value[".type"], "type"),
        "anonymous": bool(value.get(".anonymous", False)),
        "options": options,
    }


def section_fingerprint(value):
    canonical = canonical_section(value)
    if canonical is None:
        return None
    payload = json.dumps(canonical, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()


def canonical_package(value):
    """Canonicalize a UCI package without ephemeral anonymous cfg ids."""
    named = {}
    anonymous = []
    if value is None:
        return {"named": named, "anonymous": anonymous}
    if not isinstance(value, dict):
        raise UbusError("invalid UCI package state")
    for section, section_value in value.items():
        canonical = canonical_section(section_value)
        if canonical is None:
            continue
        if canonical["anonymous"]:
            anonymous.append(
                json.dumps(canonical, sort_keys=True, separators=(",", ":"))
            )
        else:
            named[validate_name(section, "section")] = canonical
    anonymous.sort()
    return {"named": named, "anonymous": anonymous}


def package_state_equal(left, right):
    return canonical_package(left) == canonical_package(right)


def transaction_state_equal(packages, left, right):
    return all(
        package_state_equal(left.get(package), right.get(package))
        for package in packages
    )


def package_section(state, package, section):
    package_value = state.get(package)
    if not isinstance(package_value, dict):
        return None
    value = package_value.get(section)
    return value if isinstance(value, dict) else None


def empty_ownership():
    return {"version": 1, "sections": {}, "edges": {}}


def load_ownership():
    try:
        info = os.lstat(OWNERSHIP_PATH)
    except FileNotFoundError:
        return empty_ownership()
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise UbusError("unsafe UCI ownership record")
    with open(OWNERSHIP_PATH, encoding="utf-8") as handle:
        value = json.load(handle)
    if (
        not isinstance(value, dict)
        or value.get("version") != 1
        or not isinstance(value.get("sections"), dict)
        or not isinstance(value.get("edges"), dict)
    ):
        raise UbusError("invalid UCI ownership record")
    return value


def save_ownership(value):
    atomic_json(OWNERSHIP_PATH, value)


def interface_rows(path, description):
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        return []
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise UbusError("unsafe {}".format(description))
    rows = []
    with open(path, encoding="utf-8") as handle:
        for raw in handle:
            line = raw.rstrip("\n")
            if not line:
                continue
            values = line.split("\t")
            if len(values) != 5:
                raise UbusError("invalid row in {}".format(description))
            rows.append(tuple(values))
    return rows


def manifest_rows():
    return interface_rows(MANIFEST_PATH, "managed-interface manifest")


def migration_rows():
    return interface_rows(MIGRATION_PATH, "UCI migration manifest")


def generated_record_authorizes(row):
    kind, instance, logical, device, config = row
    filenames = {
        "tinc": "tinc.conf",
        "openvpn": "openvpn.conf",
        "wireguard": "wg.conf",
    }
    if kind not in filenames or not re.fullmatch(r"[A-Za-z0-9_-]+", instance):
        return False
    expected = os.path.join(GENERATED_ROOT, kind, instance, filenames[kind])
    if config != expected:
        return False
    record = os.path.join(
        os.path.dirname(MANIFEST_PATH), "generated.{}.{}.owner".format(kind, instance)
    )
    record_exists = True
    try:
        info = os.lstat(record)
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
            return False
        with open(record, encoding="utf-8") as handle:
            values = handle.read().rstrip("\n").split("\t")
    except FileNotFoundError:
        # The immediately preceding owner-marker release had no external
        # generated.* record.  A startup-sealed pending row may migrate from
        # its exact private path and non-symlink owner/config files once.
        record_exists = False
    except OSError:
        return False
    if record_exists:
        if len(values) != 7 or tuple(values[2:]) != row or values[0] != MEDUZA_OWNER:
            return False
        if values[1] != "owned" and not re.fullmatch(
            r"(?:creating|deleting|empty)-[0-9a-f]{16}", values[1]
        ):
            return False
    directory = os.path.dirname(config)
    marker = os.path.join(directory, ".meduza-owner")
    try:
        directory_info = os.lstat(directory)
        marker_info = os.lstat(marker)
        config_info = os.lstat(config)
        if (
            stat.S_ISLNK(directory_info.st_mode)
            or not stat.S_ISDIR(directory_info.st_mode)
            or stat.S_ISLNK(marker_info.st_mode)
            or not stat.S_ISREG(marker_info.st_mode)
            or stat.S_ISLNK(config_info.st_mode)
            or not stat.S_ISREG(config_info.st_mode)
            or os.path.realpath(directory) != directory
        ):
            return False
        with open(marker, encoding="utf-8") as handle:
            value = handle.read()
            if value.endswith("\n"):
                value = value[:-1]
            return value == MEDUZA_OWNER
    except OSError:
        return False


def migration_manifest_rows():
    stable = set(manifest_rows())
    rows = list(stable)
    for row in migration_rows():
        if row not in stable and generated_record_authorizes(row):
            rows.append(row)
    return rows


def legacy_rows():
    try:
        info = os.lstat(LEGACY_AUTH_PATH)
    except FileNotFoundError:
        return []
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise UbusError("unsafe legacy-interface authorization")
    rows = []
    with open(LEGACY_AUTH_PATH, encoding="utf-8") as handle:
        for raw in handle:
            values = raw.rstrip("\n").split("\t")
            if len(values) == 6 and re.fullmatch(r"[0-9a-f]{64}", values[5]):
                rows.append(tuple(values))
    return rows


def legacy_manifest_authorizes(package, section, value):
    if package != "network":
        return False
    canonical = canonical_section(value)
    if canonical is None or canonical["type"] != "interface":
        return False
    options = canonical["options"]
    scalar_options = {
        name: item.get("value")
        for name, item in options.items()
        if item.get("kind") == "scalar"
    }
    if len(scalar_options) != len(options):
        return False
    for _kind, _instance, logical, device, _config, _digest in legacy_rows():
        if logical == section and scalar_options == {
            "proto": "none",
            "device": device,
            "auto": "1",
            "meduza": "1",
        }:
            return True
    return False


def manifest_authorizes(package, section, value):
    """Strictly authorize migration from the previous owner-marker release."""
    canonical = canonical_section(value)
    if canonical is None or canonical["options"].get("meduza_owner", {}).get("value") != MEDUZA_OWNER:
        return False
    options = canonical["options"]
    scalar_options = {
        name: item.get("value")
        for name, item in options.items()
        if item.get("kind") == "scalar"
    }
    if len(scalar_options) != len(options):
        return False
    for kind, instance, logical, device, config in migration_manifest_rows():
        if logical != section:
            continue
        if package == "network":
            expected = {
                "proto": scalar_options.get("proto"),
                "auto": "0",
                "defaultroute": "0",
                "peerdns": "0",
                "delegate": "0",
                "meduza_owner": MEDUZA_OWNER,
                "meduza_kind": kind,
                "meduza_instance": instance,
                "meduza_device": device,
                "meduza_config": config,
            }
            if scalar_options.get("proto") == "openvpn" and kind == "openvpn":
                expected.update(
                    {
                        "config": config,
                        "script_security": "3",
                        "up": os.path.join(os.path.dirname(config), "link-up"),
                    }
                )
            else:
                expected["proto"] = "none"
                expected["device"] = device
            return canonical["type"] == "interface" and scalar_options == expected
        if package == "openvpn" and kind == "openvpn":
            return canonical["type"] == "openvpn" and scalar_options == {
                "enabled": scalar_options.get("enabled"),
                "config": config,
                "dev": device,
                "meduza_owner": MEDUZA_OWNER,
            } and scalar_options.get("enabled") in ("0", "1")
    return False


def uci_request(session, config, **values):
    validate_session(session)
    validate_name(config, "package")
    request = {"ubus_rpc_session": session, "config": config}
    request.update(values)
    return request


def get_value(session, expression, quiet=False):
    parts = expression.split(".", 2)
    config = parts[0]
    request = uci_request(session, config)
    if len(parts) > 1:
        request["section"] = validate_name(parts[1], "section")
    if len(parts) > 2:
        request["option"] = validate_name(parts[2], "option")
    reply = ubus("uci", "get", request, quiet=quiet)
    if reply is None:
        return None
    if len(parts) > 2:
        if "value" not in reply:
            raise UbusError("UCI get response omitted option value")
        return reply["value"]
    if len(parts) == 2:
        values = reply.get("values")
        if not isinstance(values, dict) or not isinstance(values.get(".type"), str):
            raise UbusError("UCI get response omitted section type")
        return values[".type"]
    values = reply.get("values")
    if not isinstance(values, dict):
        raise UbusError("UCI get response omitted package values")
    return values


def command_get(session, expression, quiet):
    value = get_value(session, expression, quiet=quiet)
    if value is None:
        return 3
    if isinstance(value, list):
        print(" ".join(str(item) for item in value))
    elif isinstance(value, dict):
        print(json.dumps(value, sort_keys=True, separators=(",", ":")))
    else:
        print(str(value))
    return 0


def command_show(session, config, quiet):
    values = get_value(session, config, quiet=quiet)
    if not isinstance(values, dict):
        return 3
    for section, section_values in values.items():
        if not isinstance(section_values, dict):
            continue
        section_type = section_values.get(".type")
        if isinstance(section_type, str):
            print("{}.{}={}".format(config, section, section_type))
        for option, value in section_values.items():
            if option.startswith("."):
                continue
            if isinstance(value, list):
                for item in value:
                    print("{}.{}.{}='{}'".format(config, section, option, item))
            elif value is not None:
                print("{}.{}.{}='{}'".format(config, section, option, value))
    return 0


def split_assignment(expression):
    if "=" not in expression:
        raise UbusError("UCI assignment is missing '='")
    left, value = expression.split("=", 1)
    parts = left.split(".", 2)
    if len(parts) < 2:
        raise UbusError("invalid UCI assignment")
    validate_name(parts[0], "package")
    validate_name(parts[1], "section")
    if len(parts) > 2:
        validate_name(parts[2], "option")
    return parts, value


def command_set(session, expression):
    parts, value = split_assignment(expression)
    config, section = parts[:2]
    if len(parts) == 2:
        validate_name(value, "type")
        current = get_value(session, "{}.{}".format(config, section), quiet=True)
        if current == value:
            return 0
        if current is not None:
            ubus("uci", "delete", uci_request(session, config, section=section))
        ubus(
            "uci",
            "add",
            uci_request(session, config, name=section, type=value),
        )
        return 0
    option = parts[2]
    ubus(
        "uci",
        "set",
        uci_request(session, config, section=section, values={option: value}),
    )
    return 0


def command_delete(session, expression, quiet):
    parts = expression.split(".", 2)
    if len(parts) < 2:
        raise UbusError("invalid UCI delete expression")
    config = validate_name(parts[0], "package")
    section = validate_name(parts[1], "section")
    request = uci_request(session, config, section=section)
    if len(parts) > 2:
        request["option"] = validate_name(parts[2], "option")
    reply = ubus("uci", "delete", request, quiet=quiet)
    return 0 if reply is not None else 3


def command_add_list(session, expression):
    parts, value = split_assignment(expression)
    if len(parts) != 3:
        raise UbusError("add_list requires an option")
    config, section, option = parts
    current = get_value(session, ".".join(parts), quiet=True)
    append_session_delta(
        session,
        config,
        list_delta_lines(config, section, option, current, value, True),
    )
    return 0


def command_del_list(session, expression):
    parts, value = split_assignment(expression)
    if len(parts) != 3:
        raise UbusError("del_list requires an option")
    config, section, option = parts
    current = get_value(session, ".".join(parts), quiet=True)
    append_session_delta(
        session,
        config,
        list_delta_lines(config, section, option, current, value, False),
    )
    return 0


def command_changes(session, config):
    changes = get_changes(session, config)
    for change in changes:
        print(json.dumps(change, separators=(",", ":")))
    return 0


def get_changes(session, config):
    reply = ubus("uci", "changes", uci_request(session, config), quiet=True)
    if reply is None:
        return []
    changes = reply.get("changes", [])
    if not isinstance(changes, list):
        raise UbusError("unexpected UCI changes response")
    return changes


def session_state(session, packages):
    return {
        package: get_value(session, package, quiet=True)
        for package in packages
    }


def section_nonce(value):
    canonical = canonical_section(value)
    if canonical is None:
        return ""
    return canonical["options"].get("meduza_nonce", {}).get("value", "")


def record_authorizes(record, fingerprint):
    if not isinstance(record, dict) or record.get("version") != 1:
        return False
    phase = record.get("phase")
    if phase == "owned":
        return record.get("after") == fingerprint
    if phase in ("creating", "updating", "deleting"):
        return fingerprint in (record.get("before"), record.get("after"))
    return False


def ensure_owned_section(session, package, section):
    """Authorize a section from external state, or reserve a fresh one."""
    transaction = load_baseline(session)
    baseline_value = package_section(transaction["state"], package, section)
    baseline_fp = section_fingerprint(baseline_value)
    current_state = session_state(session, [package])
    current_value = package_section(current_state, package, section)
    canonical = canonical_section(current_value)
    if canonical is None or canonical["options"].get("meduza_owner", {}).get("value") != MEDUZA_OWNER:
        return False

    ownership = load_ownership()
    key = "{}.{}".format(package, section)
    record = ownership["sections"].get(key)
    if (
        isinstance(record, dict)
        and record.get("phase") == "retired"
        and baseline_value is None
    ):
        record = None
    nonce = section_nonce(current_value)
    if record is not None:
        if not record_authorizes(record, baseline_fp):
            return False
        expected_nonce = record.get("nonce")
        if not isinstance(expected_nonce, str) or not re.fullmatch(r"[0-9a-f]{32}", expected_nonce):
            raise UbusError("invalid UCI section ownership nonce")
        if nonce and nonce != expected_nonce:
            return False
        nonce = expected_nonce
    else:
        if baseline_value is not None:
            if not (
                manifest_authorizes(package, section, baseline_value)
                or legacy_manifest_authorizes(package, section, baseline_value)
            ):
                return False
        elif package_section(transaction["state"], package, section) is not None:
            return False
        nonce = secrets.token_hex(16)

    if section_nonce(current_value) != nonce:
        ubus(
            "uci",
            "set",
            uci_request(
                session,
                package,
                section=section,
                values={"meduza_nonce": nonce},
            ),
        )
        current_value = package_section(session_state(session, [package]), package, section)

    current_fp = section_fingerprint(current_value)
    if record is None or record.get("phase") != "owned" or baseline_fp != current_fp:
        ownership["sections"][key] = {
            "version": 1,
            "package": package,
            "section": section,
            "nonce": nonce,
            "phase": "creating" if baseline_value is None else "updating",
            "before": baseline_fp,
            "after": current_fp,
        }
        save_ownership(ownership)
    return True


def command_owned(session, expression):
    parts = expression.split(".", 1)
    if len(parts) != 2:
        raise UbusError("owned requires PACKAGE.SECTION")
    package = validate_name(parts[0], "package")
    section = validate_name(parts[1], "section")
    return 0 if ensure_owned_section(session, package, section) else 3


def verify_live_owned(expression):
    parts = expression.split(".", 1)
    if len(parts) != 2:
        raise UbusError("verify-live requires PACKAGE.SECTION")
    package = validate_name(parts[0], "package")
    section = validate_name(parts[1], "section")
    state = live_state([package])
    value = package_section(state, package, section)
    if not is_meduza_section(value):
        return 3
    fingerprint = section_fingerprint(value)
    record = load_ownership()["sections"].get("{}.{}".format(package, section))
    if record_authorizes(record, fingerprint):
        nonce = record.get("nonce")
        live_nonce = section_nonce(value)
        if live_nonce and live_nonce == nonce:
            return 0
        # A transition that migrates a previous release can authorize its
        # exact before-fingerprint before the nonce itself is committed.
        if fingerprint == record.get("before") and manifest_authorizes(
            package, section, value
        ):
            return 0
        return 3
    if record is not None:
        return 3
    return 0 if manifest_authorizes(package, section, value) else 3


def delta_quote(value):
    value = str(value)
    if "\0" in value or "\n" in value or "\r" in value:
        raise UbusError("unsafe UCI list value")
    return "'" + value.replace("'", "'\\''") + "'"


def append_session_delta(session, package, lines):
    if not lines:
        return
    verify_uci_not_found_context({"ubus_rpc_session": session})
    savedir = RPCD_SAVEDIR_PREFIX + validate_session(session)
    os.makedirs(savedir, mode=0o700, exist_ok=True)
    info = os.lstat(savedir)
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise UbusError("unsafe rpcd UCI savedir")
    os.chmod(savedir, 0o700)
    path = os.path.join(savedir, validate_name(package, "package"))
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_APPEND | getattr(os, "O_NOFOLLOW", 0),
        0o600,
    )
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode):
            raise UbusError("unsafe rpcd UCI delta file")
        os.fchmod(descriptor, 0o600)
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        payload = "".join(line + "\n" for line in lines).encode()
        offset = 0
        while offset < len(payload):
            offset += os.write(descriptor, payload[offset:])
        os.fsync(descriptor)
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)
    fsync_directory(savedir)


def list_delta_lines(package, section, option, current, value, add):
    prefix = "{}.{}.{}=".format(package, section, option)
    lines = []
    if isinstance(current, str) and len(current.split()) > 1:
        # A scalar such as option network 'lan utun' must first become a real
        # token list.  LIST_ADD converts the scalar into a list; removing the
        # original composite item then preserves each existing token.
        seen = set()
        for token in current.split():
            if token not in seen:
                lines.append("|" + prefix + delta_quote(token))
                seen.add(token)
        lines.append("~" + prefix + delta_quote(current))
        current = current.split()
    items = [str(item) for item in current] if isinstance(current, list) else ([str(current)] if current is not None else [])
    if add:
        if value not in items:
            lines.append("|" + prefix + delta_quote(value))
    elif value in items:
        if isinstance(current, str):
            # Convert a single scalar to a list so LIST_DEL remains an
            # element operation instead of deleting the whole option.
            lines.append("|" + prefix + delta_quote(value))
        lines.append("~" + prefix + delta_quote(value))
    return lines


def live_state(packages):
    session = new_session(packages)
    try:
        return session_state(session, packages)
    finally:
        ubus("session", "destroy", {"ubus_rpc_session": session}, quiet=True)


def is_meduza_section(value):
    canonical = canonical_section(value)
    return (
        canonical is not None
        and canonical["options"].get("meduza_owner", {}).get("value")
        == MEDUZA_OWNER
    )


def prepare_section_transitions(session, packages, baseline_state):
    expected = session_state(session, packages)
    for package in ("network", "openvpn"):
        if package not in packages:
            continue
        package_value = expected.get(package)
        if not isinstance(package_value, dict):
            continue
        for section, value in list(package_value.items()):
            if is_meduza_section(value) and not ensure_owned_section(
                session, package, section
            ):
                raise UbusError(
                    "UCI section lacks external ownership: {}.{}".format(
                        package, section
                    )
                )

    expected = session_state(session, packages)
    ownership = load_ownership()
    for package in ("network", "openvpn"):
        if package not in packages:
            continue
        baseline_package = baseline_state.get(package)
        expected_package = expected.get(package)
        baseline_package = baseline_package if isinstance(baseline_package, dict) else {}
        expected_package = expected_package if isinstance(expected_package, dict) else {}
        for section in sorted(set(baseline_package) | set(expected_package)):
            before = baseline_package.get(section)
            after = expected_package.get(section)
            before_owned = is_meduza_section(before)
            after_owned = is_meduza_section(after)
            before_managed = before_owned or legacy_manifest_authorizes(
                package, section, before
            )
            if not before_managed and not after_owned:
                continue
            key = "{}.{}".format(package, section)
            record = ownership["sections"].get(key)
            before_fp = section_fingerprint(before)
            after_fp = section_fingerprint(after)
            if after_owned:
                nonce = section_nonce(after)
                if not re.fullmatch(r"[0-9a-f]{32}", nonce):
                    raise UbusError("managed UCI section is missing its nonce")
                if not record_authorizes(record, before_fp):
                    raise UbusError(
                        "managed UCI section fingerprint changed: {}".format(key)
                    )
                if before_fp == after_fp:
                    ownership["sections"][key] = {
                        "version": 1,
                        "package": package,
                        "section": section,
                        "nonce": nonce,
                        "phase": "owned",
                        "before": after_fp,
                        "after": after_fp,
                    }
                else:
                    ownership["sections"][key] = {
                        "version": 1,
                        "package": package,
                        "section": section,
                        "nonce": nonce,
                        "phase": "creating" if before is None else "updating",
                        "before": before_fp,
                        "after": after_fp,
                    }
            else:
                if not record_authorizes(record, before_fp):
                    if not (
                        manifest_authorizes(package, section, before)
                        or legacy_manifest_authorizes(package, section, before)
                    ):
                        raise UbusError(
                            "refusing to delete UCI section without external ownership: {}".format(
                                key
                            )
                        )
                    nonce = section_nonce(before) or secrets.token_hex(16)
                else:
                    nonce = record["nonce"]
                ownership["sections"][key] = {
                    "version": 1,
                    "package": package,
                    "section": section,
                    "nonce": nonce,
                    "phase": "deleting",
                    "before": before_fp,
                    "after": None,
                }
    save_ownership(ownership)
    return expected


def promote_package_ownership(package, live_package):
    live_package = live_package if isinstance(live_package, dict) else {}
    ownership = load_ownership()
    changed = False
    for key, record in list(ownership["sections"].items()):
        if not isinstance(record, dict) or record.get("package") != package:
            continue
        section = record.get("section")
        live_value = live_package.get(section)
        live_fp = section_fingerprint(live_value)
        phase = record.get("phase")
        if phase in ("creating", "updating"):
            if live_fp == record.get("after") and section_nonce(live_value) == record.get("nonce"):
                record["phase"] = "owned"
                record["before"] = live_fp
                record["after"] = live_fp
                changed = True
            elif live_fp != record.get("before"):
                raise UbusError("managed UCI section activation conflicts with intent: {}".format(key))
        elif phase == "deleting":
            if live_value is None:
                record["phase"] = "retired"
                record["before"] = None
                record["after"] = None
                changed = True
            elif live_fp != record.get("before"):
                raise UbusError("managed UCI section deletion conflicts with intent: {}".format(key))
    if changed:
        save_ownership(ownership)


def list_tokens(value):
    if value is None:
        return []
    if isinstance(value, list):
        return [str(item) for item in value]
    return str(value).split()


def edge_key(zone_name, member):
    """Return an edge key that survives anonymous UCI section renumbering."""
    return "firewall-zone:" + json.dumps(
        [zone_name, member], ensure_ascii=False, separators=(",", ":")
    )


def find_edge_record(ownership, zone_name, member):
    """Find and migrate an edge record by its stable zone identity."""
    key = edge_key(zone_name, member)
    record = ownership["edges"].get(key)
    if record is not None:
        return key, record
    legacy = [
        candidate
        for candidate, value in ownership["edges"].items()
        if isinstance(value, dict)
        and value.get("zone_name") == zone_name
        and value.get("member") == member
    ]
    if len(legacy) > 1:
        raise UbusError("ambiguous firewall edge ownership record")
    if legacy:
        record = ownership["edges"].pop(legacy[0])
        ownership["edges"][key] = record
    return key, record


def find_zone_by_name(package, zone_name):
    """Resolve the current anonymous section id for one uniquely named zone."""
    matches = []
    if isinstance(package, dict):
        for section, value in package.items():
            if not isinstance(value, dict):
                continue
            if value.get(".type") == "zone" and value.get("name") == zone_name:
                matches.append((section, value))
    if len(matches) > 1:
        raise UbusError("multiple firewall zones have the same name: {}".format(zone_name))
    return matches[0] if matches else (None, None)


def baseline_edge_migration_authorized(session, zone_name, member):
    """Prove that an already-present edge belongs to the previous release."""
    try:
        try:
            disabled_info = os.lstat(MIGRATION_ZONE_DISABLED_PATH)
        except FileNotFoundError:
            disabled_info = None
        if disabled_info is not None:
            if stat.S_ISLNK(disabled_info.st_mode) or not stat.S_ISREG(
                disabled_info.st_mode
            ):
                raise UbusError("unsafe disabled firewall-edge migration marker")
            with open(MIGRATION_ZONE_DISABLED_PATH, encoding="utf-8") as handle:
                if handle.read() != "disabled\n":
                    raise UbusError("invalid disabled firewall-edge migration marker")
            return False

        intent_info = os.lstat(UPGRADE_INTENT_PATH)
        if stat.S_ISLNK(intent_info.st_mode) or not stat.S_ISREG(intent_info.st_mode):
            raise UbusError("unsafe package-upgrade migration intent")
        with open(UPGRADE_INTENT_PATH, encoding="utf-8") as handle:
            intent = handle.read().rstrip("\n").split("\t")
        if len(intent) == 3 and intent[:1] == ["v1"] and intent[2] in (
            "pending",
            "disabled",
        ):
            if not re.fullmatch(r"[0-9a-f]{32}", intent[1]):
                raise UbusError("invalid package-upgrade migration intent")
            return False
        if (
            len(intent) != 4
            or intent[0] != "v1"
            or intent[2] != "bound"
            or not re.fullmatch(r"[0-9a-f]{32}", intent[1])
            or not re.fullmatch(r"[0-9a-f]{64}", intent[3])
        ):
            raise UbusError("invalid package-upgrade migration intent")

        seal_info = os.lstat(MIGRATION_ZONE_SEAL_PATH)
        if stat.S_ISLNK(seal_info.st_mode) or not stat.S_ISREG(seal_info.st_mode):
            raise UbusError("unsafe firewall-edge migration seal")
        with open(MIGRATION_ZONE_SEAL_PATH, encoding="utf-8") as handle:
            seal = handle.read().rstrip("\n").split("\t")
        if (
            len(seal) != 4
            or seal[0] != "v1"
            or not re.fullmatch(r"[0-9a-f]{32}", seal[1])
            or not re.fullmatch(r"[0-9a-f]{64}", seal[2])
            or not re.fullmatch(r"[0-9a-f]{64}", seal[3])
        ):
            raise UbusError("invalid firewall-edge migration seal")
        if seal[1] != intent[1] or seal[2] != intent[3]:
            raise UbusError("firewall-edge migration seal belongs to another upgrade")
        info = os.lstat(MIGRATION_ZONE_PATH)
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
            raise UbusError("unsafe firewall-edge migration snapshot")
        if file_digest(MIGRATION_ZONE_PATH, missing_ok=False)["sha256"] != seal[3]:
            raise UbusError("firewall-edge migration snapshot does not match its seal")
        with open(MIGRATION_ZONE_PATH, encoding="utf-8") as handle:
            trusted_edges = set()
            for raw in handle:
                values = raw.rstrip("\n").split("\t")
                if (
                    len(values) != 2
                    or not TYPE_RE.fullmatch(values[0])
                    or not NAME_RE.fullmatch(values[1])
                ):
                    raise UbusError("invalid firewall-edge migration snapshot")
                trusted_edges.add(tuple(values))
    except FileNotFoundError:
        return False
    if (zone_name, member) not in trusted_edges:
        return False
    transaction = load_baseline(session)
    network = package_section(transaction["state"], "network", member)
    if not manifest_authorizes("network", member, network):
        return False
    _section, zone = find_zone_by_name(transaction["state"].get("firewall"), zone_name)
    return isinstance(zone, dict) and member in list_tokens(zone.get("network"))


def edge_tag_option(member):
    digest = hashlib.sha256(member.encode()).hexdigest()[:16]
    return "meduza_edge_{}".format(digest)


def owned_edge_tag(nonce):
    return "owned:" + nonce


def removed_edge_tag(nonce):
    return "removed:" + nonce


def command_edge(session, expression, add):
    parts, member = split_assignment(expression)
    if len(parts) != 3 or parts[0] != "firewall" or parts[2] != "network":
        raise UbusError("firewall edge requires firewall.SECTION.network=LOGICAL")
    _package, section, option = parts
    validate_name(member, "section")
    zone_name = get_value(session, "firewall.{}.name".format(section), quiet=True)
    if not isinstance(zone_name, str) or not zone_name:
        raise UbusError("firewall edge references a zone without a name")
    if add and not ensure_owned_section(session, "network", member):
        raise UbusError("firewall edge references a non-owned network section")
    current = get_value(session, ".".join(parts), quiet=True)
    present = member in list_tokens(current)
    tag_option = edge_tag_option(member)
    tag_expression = "firewall.{}.{}".format(section, tag_option)
    tag = get_value(session, tag_expression, quiet=True)
    ownership = load_ownership()
    key, record = find_edge_record(ownership, zone_name, member)
    if not isinstance(record, dict) and isinstance(tag, str):
        # Anonymous section ids and even the zone's display name may change.
        # The exact per-edge nonce tag is stronger than either, so rebind a
        # uniquely matching record instead of treating the membership as a
        # borrowed/user token or leaving the old edge uncollectable.
        tagged = []
        for candidate_key, candidate in ownership["edges"].items():
            if not isinstance(candidate, dict) or candidate.get("member") != member:
                continue
            candidate_nonce = candidate.get("nonce")
            if not isinstance(candidate_nonce, str):
                continue
            if tag in (
                owned_edge_tag(candidate_nonce),
                removed_edge_tag(candidate_nonce),
            ):
                tagged.append((candidate_key, candidate))
        if len(tagged) > 1:
            raise UbusError("ambiguous tagged firewall edge ownership")
        if tagged:
            old_key, record = tagged[0]
            if old_key != key:
                ownership["edges"].pop(old_key)
                record["zone_name"] = zone_name
                record["section"] = section
                ownership["edges"][key] = record
                save_ownership(ownership)
    section_record = ownership["sections"].get("network." + member)
    network_nonce = (
        section_record.get("nonce") if isinstance(section_record, dict) else ""
    )
    if add:
        if not isinstance(record, dict):
            if present:
                migrated = baseline_edge_migration_authorized(session, zone_name, member)
                if not migrated:
                    ownership["edges"][key] = {
                        "version": 1, "section": section, "zone_name": zone_name,
                        "member": member, "network_nonce": network_nonce,
                        "phase": "borrowed",
                    }
                    save_ownership(ownership)
                    return 0
                if not network_nonce:
                    raise UbusError("migrated firewall edge lacks network ownership")
                record = {
                    "version": 1, "section": section, "zone_name": zone_name,
                    "member": member, "network_nonce": network_nonce,
                    "nonce": secrets.token_hex(16), "tag_option": tag_option,
                    "phase": "creating", "migration": True,
                }
            else:
                record = {
                    "version": 1, "section": section, "zone_name": zone_name,
                    "member": member, "network_nonce": network_nonce,
                    "nonce": secrets.token_hex(16), "tag_option": tag_option,
                    "phase": "creating", "migration": False,
                }
            ownership["edges"][key] = record
            save_ownership(ownership)

        phase = record.get("phase")
        if phase == "borrowed":
            if present:
                return 0
            del ownership["edges"][key]
            save_ownership(ownership)
            return command_edge(session, expression, True)
        if phase == "retired":
            retired_nonce = record.get("nonce")
            if (
                not present
                and isinstance(retired_nonce, str)
                and tag == removed_edge_tag(retired_nonce)
            ):
                nonce = secrets.token_hex(16)
                record.update(
                    {
                        "nonce": nonce,
                        "phase": "creating",
                        "migration": False,
                        "section": section,
                        "network_nonce": network_nonce,
                        "tag_option": tag_option,
                    }
                )
                record.pop("migration_delete", None)
                save_ownership(ownership)
                ubus(
                    "uci", "set",
                    uci_request(
                        session,
                        "firewall",
                        section=section,
                        values={tag_option: owned_edge_tag(nonce)},
                    ),
                )
                append_session_delta(
                    session, "firewall",
                    list_delta_lines("firewall", section, option, current, member, True),
                )
                return 0
            if present:
                if tag is not None:
                    raise UbusError("retired firewall edge tag unexpectedly remains")
                record.update({"phase": "borrowed", "section": section})
                save_ownership(ownership)
                return 0
            del ownership["edges"][key]
            save_ownership(ownership)
            return command_edge(session, expression, True)
        if phase not in ("creating", "owned", "deleting"):
            raise UbusError("invalid firewall edge ownership phase")
        nonce = record.get("nonce")
        if (
            not isinstance(nonce, str)
            or not re.fullmatch(r"[0-9a-f]{32}", nonce)
            or record.get("tag_option") != tag_option
            or record.get("network_nonce") != network_nonce
        ):
            raise UbusError("firewall edge ownership identity changed")
        owned_tag = owned_edge_tag(nonce)
        if phase == "owned":
            if tag != owned_tag:
                raise UbusError("managed firewall edge tag disappeared")
            if not present:
                append_session_delta(
                    session, "firewall",
                    list_delta_lines("firewall", section, option, current, member, True),
                )
                record["phase"] = "creating"
                save_ownership(ownership)
            elif record.get("section") != section:
                record["section"] = section
                save_ownership(ownership)
            return 0
        if phase == "deleting":
            if tag == removed_edge_tag(nonce) and not present:
                # The previous deletion committed before its external phase
                # was promoted.  Start a fresh generation for the now-desired
                # edge instead of publishing MANAGED with no zone membership.
                nonce = secrets.token_hex(16)
                record.update(
                    {
                        "nonce": nonce,
                        "phase": "creating",
                        "migration": False,
                        "section": section,
                        "network_nonce": network_nonce,
                        "tag_option": tag_option,
                    }
                )
                record.pop("migration_delete", None)
                save_ownership(ownership)
                ubus(
                    "uci", "set",
                    uci_request(
                        session,
                        "firewall",
                        section=section,
                        values={tag_option: owned_edge_tag(nonce)},
                    ),
                )
                append_session_delta(
                    session,
                    "firewall",
                    list_delta_lines(
                        "firewall", section, option, current, member, True
                    ),
                )
                return 0
            if tag != owned_tag or not present:
                raise UbusError("cannot safely cancel firewall edge deletion")
            record["phase"] = "owned"
            record["section"] = section
            save_ownership(ownership)
            return 0

        # creating: only the exact tag can prove a previous commit.  A token
        # that appeared without it after the intent was journaled is borrowed,
        # not retrospectively claimed.
        if tag == owned_tag:
            if not present:
                append_session_delta(
                    session, "firewall",
                    list_delta_lines("firewall", section, option, current, member, True),
                )
            return 0
        if tag is not None:
            raise UbusError("firewall edge tag conflicts with creation intent")
        if present and not (
            record.get("migration")
            and baseline_edge_migration_authorized(session, zone_name, member)
        ):
            raise UbusError("firewall member appeared without Meduza edge tag")
        ubus(
            "uci", "set",
            uci_request(session, "firewall", section=section, values={tag_option: owned_tag}),
        )
        if not present:
            append_session_delta(
                session, "firewall",
                list_delta_lines("firewall", section, option, current, member, True),
            )
        return 0

    # Delete only an edge with an external record and its exact inline tag.
    # Tagless memberships from old releases remain borrowed unless they were
    # first migrated by a desired-state add using the pre-upgrade zone snapshot.
    if not isinstance(record, dict):
        if present and baseline_edge_migration_authorized(session, zone_name, member):
            if not network_nonce:
                raise UbusError("migrated firewall edge lacks network ownership")
            nonce = secrets.token_hex(16)
            record = {
                "version": 1,
                "section": section,
                "zone_name": zone_name,
                "member": member,
                "network_nonce": network_nonce,
                "nonce": nonce,
                "tag_option": tag_option,
                "phase": "deleting",
                "migration_delete": True,
            }
            ownership["edges"][key] = record
            save_ownership(ownership)
            ubus(
                "uci", "set",
                uci_request(
                    session,
                    "firewall",
                    section=section,
                    values={tag_option: removed_edge_tag(nonce)},
                ),
            )
            append_session_delta(
                session, "firewall",
                list_delta_lines("firewall", section, option, current, member, False),
            )
        return 0
    phase = record.get("phase")
    if phase == "borrowed":
        del ownership["edges"][key]
        save_ownership(ownership)
        return 0
    if phase == "retired":
        nonce = record.get("nonce")
        if present or tag not in (None, removed_edge_tag(nonce) if isinstance(nonce, str) else None):
            raise UbusError("retired firewall edge was replaced before finalization")
        return 0
    if phase == "creating" and not present and tag is None:
        del ownership["edges"][key]
        save_ownership(ownership)
        return 0
    nonce = record.get("nonce")
    if (
        phase not in ("owned", "creating", "deleting")
        or not isinstance(nonce, str)
        or not re.fullmatch(r"[0-9a-f]{32}", nonce)
        or record.get("tag_option") != tag_option
        or not isinstance(section_record, dict)
        or record.get("network_nonce") != section_record.get("nonce")
    ):
        raise UbusError("firewall edge no longer matches its network generation")
    owned_tag = owned_edge_tag(nonce)
    removed_tag = removed_edge_tag(nonce)
    if tag == removed_tag:
        if present:
            raise UbusError("firewall member was re-added after managed deletion")
        record["phase"] = "deleting"
        save_ownership(ownership)
        return 0
    if (
        tag is None
        and present
        and record.get("migration_delete") is True
        and baseline_edge_migration_authorized(session, zone_name, member)
    ):
        ubus(
            "uci", "set",
            uci_request(
                session,
                "firewall",
                section=section,
                values={tag_option: removed_tag},
            ),
        )
        append_session_delta(
            session, "firewall",
            list_delta_lines("firewall", section, option, current, member, False),
        )
        return 0
    if tag != owned_tag:
        raise UbusError("managed firewall edge tag changed before deletion")
    record["phase"] = "deleting"
    record["section"] = section
    save_ownership(ownership)
    ubus(
        "uci", "set",
        uci_request(session, "firewall", section=section, values={tag_option: removed_tag}),
    )
    if present:
        append_session_delta(
            session, "firewall",
            list_delta_lines("firewall", section, option, current, member, False),
        )
    return 0


def promote_firewall_edges(live_package):
    live_package = live_package if isinstance(live_package, dict) else {}
    ownership = load_ownership()
    changed = False
    for key, record in list(ownership["edges"].items()):
        if not isinstance(record, dict):
            raise UbusError("invalid firewall edge record")
        zone_name = record.get("zone_name")
        member = record.get("member")
        if not isinstance(zone_name, str) or not zone_name or not isinstance(member, str):
            raise UbusError("invalid firewall edge identity")
        section, section_value = find_zone_by_name(live_package, zone_name)
        value = section_value.get("network") if isinstance(section_value, dict) else None
        present = member in list_tokens(value)
        phase = record.get("phase")
        if phase == "borrowed":
            if section_value is None:
                del ownership["edges"][key]
                changed = True
            continue
        network_record = ownership["sections"].get("network." + member)
        if (
            not isinstance(network_record, dict)
            or network_record.get("nonce") != record.get("network_nonce")
            or network_record.get("phase")
            not in ("creating", "updating", "owned", "deleting", "retired")
        ):
            raise UbusError(
                "firewall edge no longer matches its network generation: {}".format(
                    key
                )
            )
        nonce = record.get("nonce")
        tag_option = record.get("tag_option")
        if (
            not isinstance(nonce, str)
            or not re.fullmatch(r"[0-9a-f]{32}", nonce)
            or tag_option != edge_tag_option(member)
        ):
            raise UbusError("invalid firewall edge generation tag")
        tag = section_value.get(tag_option) if isinstance(section_value, dict) else None
        expected_tags = (owned_edge_tag(nonce), removed_edge_tag(nonce))
        if tag not in expected_tags:
            tagged_zones = []
            for candidate_section, candidate in live_package.items():
                if not isinstance(candidate, dict) or candidate.get(".type") != "zone":
                    continue
                if candidate.get(tag_option) in expected_tags:
                    candidate_name = candidate.get("name")
                    if not isinstance(candidate_name, str) or not candidate_name:
                        raise UbusError("tagged firewall edge moved to unnamed zone")
                    tagged_zones.append((candidate_section, candidate_name, candidate))
            if len(tagged_zones) > 1:
                raise UbusError("managed firewall edge tag appears in multiple zones")
            if tagged_zones:
                section, new_zone_name, section_value = tagged_zones[0]
                new_key = edge_key(new_zone_name, member)
                conflict = ownership["edges"].get(new_key)
                if new_key != key and conflict is not None and conflict is not record:
                    raise UbusError("renamed firewall edge conflicts with ownership record")
                if new_key != key:
                    ownership["edges"].pop(key)
                    ownership["edges"][new_key] = record
                    key = new_key
                record["zone_name"] = new_zone_name
                record["section"] = section
                zone_name = new_zone_name
                value = section_value.get("network")
                present = member in list_tokens(value)
                tag = section_value.get(tag_option)
                changed = True
        # If the owned edge and its tag are both gone, there is no live object
        # left to delete.  Retire the record instead of permanently blocking a
        # zone rename/removal; an untagged token that exists elsewhere is never
        # claimed or removed by this path.
        if not present and tag is None and (
            phase in ("owned", "deleting", "retired")
            or (phase == "creating" and section_value is None)
        ):
            if phase != "retired":
                record["phase"] = "retired"
                changed = True
            continue
        if phase == "creating":
            if present and tag == owned_edge_tag(nonce):
                record["phase"] = "owned"
                record["section"] = section
                changed = True
            elif present or tag is not None:
                raise UbusError("firewall edge activation did not complete: {}".format(key))
        elif phase == "deleting":
            if not present and tag == removed_edge_tag(nonce):
                record["phase"] = "retired"
                record["section"] = section
                changed = True
            elif not (
                present
                and (
                    tag == owned_edge_tag(nonce)
                    or (tag is None and record.get("migration_delete") is True)
                )
            ):
                raise UbusError("firewall edge deletion did not complete: {}".format(key))
        elif phase == "retired":
            if present or tag not in (None, removed_edge_tag(nonce)):
                raise UbusError("retired firewall edge reappeared: {}".format(key))
        elif phase == "owned":
            if not present or tag != owned_edge_tag(nonce):
                raise UbusError("managed firewall edge disappeared: {}".format(key))
            if record.get("section") != section:
                record["section"] = section
                changed = True
    if changed:
        save_ownership(ownership)


def command_install(session):
    transaction = load_baseline(session)
    baselines = transaction["packages"]
    baseline_state = transaction["state"]
    packages = sorted(baselines)

    # Durable ownership transitions precede UCI mutation.  This also injects
    # the generation nonce into every managed network/openvpn section.
    expected = prepare_section_transitions(session, packages, baseline_state)
    changed = [package for package in packages if get_changes(session, package)]

    # Commit only the isolated rpcd delta.  Unlike the previous whole-file
    # renderer this lets libuci replay scalar and LIST_ADD/LIST_DEL operations
    # on the newest normally-serialized configuration.
    for package in packages:
        if package_digest(package) != baselines[package]:
            raise UbusError(
                "UCI package changed during Meduza transaction: {}".format(package)
            )

    if not changed:
        actual_state = live_state(packages)
        if not transaction_state_equal(packages, actual_state, expected):
            raise UbusError("UCI changed during no-delta ownership recovery")
        for package in packages:
            actual = actual_state.get(package)
            promote_package_ownership(package, actual)
            if package == "firewall":
                promote_firewall_edges(actual)
        fsync_directory(CONFIG_DIR)
        return 0

    ownership = load_ownership()
    deleting_network = any(
        isinstance(record, dict)
        and record.get("package") == "network"
        and record.get("phase") == "deleting"
        for record in ownership["sections"].values()
    )
    preferred = (
        ["firewall", "openvpn", "network"]
        if deleting_network
        else ["network", "openvpn", "firewall"]
    )
    order = [package for package in preferred if package in changed]
    order.extend(package for package in changed if package not in order)
    for package in order:
        if package_digest(package) != baselines[package]:
            raise UbusError(
                "UCI package changed before commit: {}".format(package)
            )
        ubus("uci", "commit", uci_request(session, package))
        fsync_directory(CONFIG_DIR)
        actual = live_state([package]).get(package)
        if not package_state_equal(actual, expected.get(package)):
            raise UbusError(
                "UCI changed while committing {}; retrying from live state".format(
                    package
                )
            )
        promote_package_ownership(package, actual)
        if package == "firewall":
            promote_firewall_edges(actual)

    # A previous attempt may have committed one package and lost power before
    # its external ownership phase was promoted.  That package has no delta on
    # this retry, so promoting only ``changed`` would strand (for example) a
    # firewall edge in ``deleting`` even though its removed tag is already
    # live.  Re-verify and promote every package in the captured transaction
    # before the outer manifest is allowed to advance.
    actual_state = live_state(packages)
    if not transaction_state_equal(packages, actual_state, expected):
        raise UbusError("UCI changed after committing Meduza transaction")
    for package in packages:
        actual = actual_state.get(package)
        promote_package_ownership(package, actual)
        if package == "firewall":
            promote_firewall_edges(actual)
    fsync_directory(CONFIG_DIR)
    return 0


def command_commit(session, config):
    ubus("uci", "commit", uci_request(session, config))
    fsync_directory(CONFIG_DIR)
    return 0


def finalize_ownership(purge=False):
    """GC retired records only after the outer MANAGED manifest is durable."""
    ownership = load_ownership()
    rows = [] if purge else manifest_rows()
    managed_logicals = {row[2] for row in rows}
    state = live_state(["network", "openvpn", "firewall"])
    changed = False
    tag_cleanup = []
    edge_cleanup = []

    for key, record in list(ownership["edges"].items()):
        if not isinstance(record, dict):
            raise UbusError("invalid firewall edge record")
        zone_name = record.get("zone_name")
        member = record.get("member")
        if not isinstance(zone_name, str) or not isinstance(member, str):
            raise UbusError("invalid firewall edge identity")
        _section, zone = find_zone_by_name(state.get("firewall"), zone_name)
        present = isinstance(zone, dict) and member in list_tokens(zone.get("network"))
        tag_option = record.get("tag_option")
        nonce = record.get("nonce")
        tag = zone.get(tag_option) if isinstance(zone, dict) and isinstance(tag_option, str) else None
        phase = record.get("phase")
        if phase == "creating" and member not in managed_logicals:
            if not present and tag is None:
                edge_cleanup.append(key)
                continue
            raise UbusError("abandoned firewall edge creation is still live: {}".format(key))
        if phase != "retired":
            continue
        if present:
            raise UbusError("retired firewall edge is still live: {}".format(key))
        if tag is None:
            edge_cleanup.append(key)
        elif (
            isinstance(nonce, str)
            and tag_option == edge_tag_option(member)
            and tag == removed_edge_tag(nonce)
        ):
            tag_cleanup.append((key, record))
        else:
            raise UbusError("retired firewall edge tag changed: {}".format(key))

    if tag_cleanup:
        session, firewall_state, digests = capture_session(["firewall"])
        try:
            package = firewall_state.get("firewall")
            for key, record in tag_cleanup:
                section, zone = find_zone_by_name(package, record["zone_name"])
                if not isinstance(zone, dict):
                    raise UbusError("retired firewall zone disappeared during tag cleanup")
                member = record["member"]
                tag_option = record["tag_option"]
                if member in list_tokens(zone.get("network")):
                    raise UbusError("retired firewall member reappeared during tag cleanup")
                if zone.get(tag_option) != removed_edge_tag(record["nonce"]):
                    raise UbusError("retired firewall edge tag changed during cleanup")
                ubus(
                    "uci", "delete",
                    uci_request(
                        session, "firewall", section=section, option=tag_option
                    ),
                )
            expected = session_state(session, ["firewall"])
            if package_digest("firewall") != digests["firewall"]:
                raise UbusError("firewall changed before edge tag cleanup")
            if get_changes(session, "firewall"):
                ubus("uci", "commit", uci_request(session, "firewall"))
                fsync_directory(CONFIG_DIR)
            actual = live_state(["firewall"])
            if not package_state_equal(actual.get("firewall"), expected.get("firewall")):
                raise UbusError("firewall changed during edge tag cleanup")
            for key, record in tag_cleanup:
                _section, zone = find_zone_by_name(
                    actual.get("firewall"), record["zone_name"]
                )
                if isinstance(zone, dict) and (
                    record["member"] in list_tokens(zone.get("network"))
                    or zone.get(record["tag_option"]) is not None
                ):
                    raise UbusError("firewall edge tag cleanup did not complete")
                edge_cleanup.append(key)
            state["firewall"] = actual.get("firewall")
        finally:
            ubus("session", "destroy", {"ubus_rpc_session": session}, quiet=True)
            remove_baseline(session)

    for key in edge_cleanup:
        if key in ownership["edges"]:
            del ownership["edges"][key]
            changed = True

    for key, record in list(ownership["sections"].items()):
        if not isinstance(record, dict):
            continue
        package = record.get("package")
        section = record.get("section")
        phase = record.get("phase")
        if package not in ("network", "openvpn") or not isinstance(section, str):
            raise UbusError("invalid retired UCI section record")
        live_value = package_section(state, package, section)
        if phase == "creating" and live_value is None and (
            package != "network" or section not in managed_logicals
        ):
            del ownership["sections"][key]
            changed = True
            continue
        if phase == "deleting" and live_value is None:
            record["phase"] = "retired"
            record["before"] = None
            record["after"] = None
            phase = "retired"
            changed = True
        if phase != "retired":
            continue
        if package == "network" and section in managed_logicals:
            raise UbusError("retired UCI section remains in managed manifest: {}".format(key))
        if live_value is not None:
            raise UbusError("retired UCI section is still live: {}".format(key))
        if package == "network" and any(
            isinstance(edge, dict)
            and edge.get("network_nonce") == record.get("nonce")
            for edge in ownership["edges"].values()
        ):
            raise UbusError("retired network section still owns firewall edges: {}".format(key))
        del ownership["sections"][key]
        changed = True

    if changed:
        save_ownership(ownership)
    else:
        fsync_directory(os.path.dirname(OWNERSHIP_PATH))
    return 0


def main():
    os.umask(0o077)
    arguments = sys.argv[1:]
    if not arguments:
        raise SystemExit("usage: meduza-uci-session create PACKAGE... | SESSION [-q] COMMAND ARG")
    if arguments[0] == "create":
        if len(arguments) < 2:
            raise SystemExit("at least one UCI package is required")
        create_session(arguments[1:])
        return
    if arguments[0] == "verify-live":
        if len(arguments) != 2:
            raise UbusError("verify-live requires PACKAGE.SECTION")
        raise SystemExit(verify_live_owned(arguments[1]))
    if arguments[0] == "finalize":
        if len(arguments) != 1:
            raise UbusError("finalize does not accept arguments")
        raise SystemExit(finalize_ownership())
    if arguments[0] == "finalize-purge":
        if len(arguments) != 1:
            raise UbusError("finalize-purge does not accept arguments")
        raise SystemExit(finalize_ownership(purge=True))
    session = validate_session(arguments.pop(0))
    quiet = False
    if arguments and arguments[0] == "-q":
        quiet = True
        arguments.pop(0)
    if not arguments:
        raise SystemExit("missing UCI command")
    command = arguments.pop(0)
    argument = arguments[0] if arguments else ""
    if command == "get":
        status = command_get(session, argument, quiet)
    elif command == "show":
        status = command_show(session, argument, quiet)
    elif command == "set":
        status = command_set(session, argument)
    elif command == "delete":
        status = command_delete(session, argument, quiet)
    elif command == "add_list":
        status = command_add_list(session, argument)
    elif command == "del_list":
        status = command_del_list(session, argument)
    elif command == "owned":
        status = command_owned(session, argument)
    elif command == "edge_add":
        status = command_edge(session, argument, True)
    elif command == "edge_del":
        status = command_edge(session, argument, False)
    elif command == "changes":
        status = command_changes(session, argument)
    elif command == "install":
        if argument:
            raise UbusError("install does not accept a package argument")
        status = command_install(session)
    elif command == "commit":
        status = command_commit(session, argument)
    elif command == "destroy":
        ubus("session", "destroy", {"ubus_rpc_session": session}, quiet=True)
        remove_baseline(session)
        status = 0
    else:
        raise UbusError("unsupported UCI command: {}".format(command))
    raise SystemExit(status)


if __name__ == "__main__":
    try:
        main()
    except (OSError, UbusError) as error:
        print("meduza: {}".format(error), file=sys.stderr)
        raise SystemExit(2)

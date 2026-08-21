from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
PACKAGE = HERE.parent
FILES = PACKAGE / "files"


def shell_path(path: Path) -> str:
    resolved = path.resolve()
    if os.name != "nt":
        return resolved.as_posix()
    value = resolved.as_posix()
    drive, remainder = value[0].lower(), value[2:]
    return "/{}{}".format(drive, remainder)


def find_shell() -> str | None:
    if os.name != "nt":
        return shutil.which("sh")
    candidates = (
        Path(r"C:\Program Files\Git\usr\bin\sh.exe"),
        Path(r"C:\Program Files\Git\bin\sh.exe"),
    )
    return next((str(path) for path in candidates if path.is_file()), None)


class SyncHarness:
    def __init__(self, root: Path, shell: str):
        self.host_root = root
        self.root = shell_path(root)
        self.shell = shell
        self.bin = root / "bin"
        self.store = root / "etc" / "config"
        self.default_pending = root / "default-uci-pending"
        self.state = root / "run"
        self.generated = root / "generated"
        self.fail_network_once = False
        for directory in (
            self.bin,
            self.store,
            self.default_pending,
            self.state,
            self.generated,
            root / "lib" / "netifd" / "proto",
            root / "usr" / "libexec" / "meduza",
            root / "etc" / "config",
            root / "etc" / "init.d",
        ):
            directory.mkdir(parents=True, exist_ok=True)

        self._install_mocks()
        self._stage_production_scripts()
        self.write_db(
            "network",
            {
                "lan": "interface",
                "lan.proto": "static",
            },
        )
        self.write_db(
            "firewall",
            {
                "vpn": "zone",
                "vpn.name": "vpn",
                "vpn.network": "lan utun",
            },
        )
        (root / "etc" / "config" / "network").touch()
        (root / "etc" / "config" / "firewall").touch()
        (root / "lib" / "netifd" / "proto" / "openvpn.sh").touch()

    def _write_executable(self, path: Path, text: str) -> None:
        path.write_text(text, encoding="utf-8", newline="\n")
        path.chmod(0o755)

    def _install_mocks(self) -> None:
        shutil.copyfile(HERE / "mocks" / "uci", self.bin / "uci")
        (self.bin / "uci").chmod(0o755)
        self._write_executable(
            self.bin / "ubus",
            """#!/bin/sh
if [ "${MEDUZA_TEST_FAIL_NETWORK_ONCE:-0}" = 1 ] && \
   [ ! -e "$MEDUZA_TEST_FAIL_MARKER" ]; then
    : >"$MEDUZA_TEST_FAIL_MARKER"
    exit 1
fi
printf 'network\n' >>"$MEDUZA_TEST_RELOAD_LOG"
""",
        )
        self._write_executable(self.bin / "logger", "#!/bin/sh\nexit 0\n")
        self._write_executable(self.bin / "fsync", "#!/bin/sh\nexit 0\n")
        self._write_executable(
            self.host_root / "etc" / "init.d" / "firewall",
            "#!/bin/sh\nprintf 'firewall\\n' >>\"$MEDUZA_TEST_RELOAD_LOG\"\n",
        )
        functions = """#!/bin/sh
config_load() { :; }
config_get() {
    variable=$1
    option=$3
    default=${4:-}
    value=$default
    [ "$option" != VPN_FIREWALL_ZONE ] || value=${MEDUZA_TEST_ZONE:-}
    eval "$variable=\\$value"
}
"""
        (self.host_root / "lib" / "functions.sh").write_text(
            functions, encoding="utf-8", newline="\n"
        )

    def _stage_production_scripts(self) -> None:
        lib_source = FILES / "usr" / "libexec" / "meduza" / "meduza-lib.sh"
        lib_target = self.host_root / "usr" / "libexec" / "meduza" / "meduza-lib.sh"
        lib_target.write_text(lib_source.read_text(encoding="utf-8"), encoding="utf-8", newline="\n")

        session_target = self.host_root / "usr" / "libexec" / "meduza" / "meduza-uci-session.py"
        self._write_executable(
            session_target,
            """#!/bin/sh
set -eu
session=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
case "${1:-}" in
create)
    mkdir -p "$MEDUZA_TEST_UCI_SESSION_DIR"
    printf '%s\\n' "$session"
    ;;
*)
    [ "$1" = "$session" ] || exit 2
    shift
    if [ "${1:-}" = destroy ]; then
        rm -rf "$MEDUZA_TEST_UCI_SESSION_DIR"
        exit 0
    fi
    if [ "${1:-}" = install ]; then
        for package in network openvpn firewall; do
            uci -t "$MEDUZA_TEST_UCI_SESSION_DIR" commit "$package"
        done
        exit 0
    fi
    if [ "${1:-}" = owned ]; then
        expression=$2
        owner="$(uci -q -t "$MEDUZA_TEST_UCI_SESSION_DIR" get "$expression.meduza_owner" 2>/dev/null || true)"
        [ "$owner" = meduza-openwrt-lite ] || exit 3
        exit 0
    fi
    if [ "${1:-}" = edge_add ]; then
        shift
        exec uci -t "$MEDUZA_TEST_UCI_SESSION_DIR" add_list "$1"
    fi
    if [ "${1:-}" = edge_del ]; then
        shift
        exec uci -t "$MEDUZA_TEST_UCI_SESSION_DIR" del_list "$1"
    fi
    exec uci -t "$MEDUZA_TEST_UCI_SESSION_DIR" "$@"
    ;;
esac
""",
        )

        sync_source = FILES / "usr" / "libexec" / "meduza" / "meduza-openwrt-sync"
        text = sync_source.read_text(encoding="utf-8")
        replacements = (
            ("/usr/libexec/meduza/meduza-lib.sh", self.root + "/usr/libexec/meduza/meduza-lib.sh"),
            ("/usr/libexec/meduza/meduza-uci-session.py", self.root + "/usr/libexec/meduza/meduza-uci-session.py"),
            ("/lib/functions.sh", self.root + "/lib/functions.sh"),
            ("/lib/netifd/", self.root + "/lib/netifd/"),
            ("/etc/", self.root + "/etc/"),
        )
        for old, new in replacements:
            text = text.replace(old, new)
        self.sync = self.host_root / "sync"
        self._write_executable(self.sync, text)

    def write_db(self, package: str, values: dict[str, str]) -> None:
        text = "".join("{}={}\n".format(key, value) for key, value in values.items())
        (self.store / package).write_text(text, encoding="utf-8", newline="\n")

    def read_db(self, package: str) -> dict[str, str]:
        result = {}
        path = self.store / package
        if not path.exists():
            return result
        for line in path.read_text(encoding="utf-8").splitlines():
            key, value = line.split("=", 1)
            result[key] = value
        return result

    def desired_path(self, rows: tuple[tuple[str, str, str, str, str], ...]) -> Path:
        path = self.host_root / "desired.interfaces"
        path.write_text(
            "".join("\t".join(row) + "\n" for row in rows),
            encoding="utf-8",
            newline="\n",
        )
        return path

    def run(self, desired: Path, check: bool = True) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                # This value is consumed by the POSIX shell, including when the
                # parent Python process runs on Windows via Git for Windows.
                "PATH": shell_path(self.bin) + ":/usr/bin:/bin",
                "MEDUZA_STATE": shell_path(self.state),
                "MEDUZA_DATA": self.root,
                "MEDUZA_GENERATED": shell_path(self.generated),
                "MEDUZA_LIBEXEC": self.root + "/usr/libexec/meduza",
                "MEDUZA_TEST_UCI_STORE": shell_path(self.store),
                "MEDUZA_TEST_UCI_DEFAULT_PENDING": shell_path(self.default_pending),
                "MEDUZA_TEST_UCI_SESSION_DIR": self.root + "/uci-session",
                "MEDUZA_TEST_UCI_WRITE_LOG": self.root + "/uci-writes.log",
                "MEDUZA_TEST_RELOAD_LOG": self.root + "/reload.log",
                "MEDUZA_TEST_FAIL_NETWORK_ONCE": "1" if self.fail_network_once else "0",
                "MEDUZA_TEST_FAIL_MARKER": self.root + "/failed-network-once",
                "MEDUZA_TEST_ZONE": "vpn",
            }
        )
        return subprocess.run(
            [self.shell, shell_path(self.sync), shell_path(desired), "apply"],
            check=check,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
        )

    def reloads(self) -> list[str]:
        path = self.host_root / "reload.log"
        return path.read_text(encoding="utf-8").splitlines() if path.exists() else []

    def uci_writes(self) -> list[str]:
        path = self.host_root / "uci-writes.log"
        return path.read_text(encoding="utf-8").splitlines() if path.exists() else []

    def set_default_pending(self, package: str, text: str | None = None) -> str:
        contents = text or "{} user-pending-change\n".format(package)
        (self.default_pending / package).write_text(
            contents, encoding="utf-8", newline="\n"
        )
        return contents


@unittest.skipUnless(find_shell(), "a POSIX shell is required for integration tests")
class ProductionSyncIntegrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sync-", dir=HERE)
        self.harness = SyncHarness(Path(self.temporary.name), find_shell() or "sh")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def three_rows(self) -> tuple[tuple[str, str, str, str, str], ...]:
        return self._three_rows_for(self.harness)

    def test_real_sync_is_idempotent_and_preserves_openclash_zone_member(self) -> None:
        desired = self.harness.desired_path(self.three_rows())

        self.harness.run(desired)

        network = self.harness.read_db("network")
        for kind, _instance, logical, device, _config in self.three_rows():
            self.assertEqual("interface", network[logical])
            self.assertEqual("meduza-openwrt-lite", network[logical + ".meduza_owner"])
            self.assertEqual(kind, network[logical + ".meduza_kind"])
            self.assertEqual(device, network[logical + ".meduza_device"])
        self.assertEqual("openvpn", network["ovpn_site_a.proto"])
        self.assertEqual("none", network["wg_site_b.proto"])
        self.assertEqual(
            {"lan", "utun", "tinc_mesh", "ovpn_site_a", "wg_site_b"},
            set(self.harness.read_db("firewall")["vpn.network"].split()),
        )
        first_reloads = list(self.harness.reloads())
        self.assertEqual(["network", "firewall"], first_reloads)

        self.harness.run(desired)

        self.assertEqual(first_reloads, self.harness.reloads())

    def test_empty_desired_removes_only_meduza_interfaces(self) -> None:
        desired = self.harness.desired_path(self.three_rows())
        self.harness.run(desired)
        empty = self.harness.desired_path(())

        self.harness.run(empty)

        network = self.harness.read_db("network")
        self.assertEqual("interface", network["lan"])
        self.assertNotIn("tinc_mesh", network)
        self.assertNotIn("ovpn_site_a", network)
        self.assertNotIn("wg_site_b", network)
        self.assertEqual(
            {"lan", "utun"},
            set(self.harness.read_db("firewall")["vpn.network"].split()),
        )

    def test_user_owned_uci_collision_is_rejected_without_reload(self) -> None:
        before = self.harness.read_db("network")
        before.update(
            {
                "wg_bad": "interface",
                "wg_bad.proto": "none",
                "wg_bad.device": "wg-user",
            }
        )
        self.harness.write_db("network", before)
        generated = shell_path(self.harness.generated)
        desired = self.harness.desired_path(
            (("wireguard", "bad", "wg_bad", "wg-user", generated + "/wireguard/bad/wg.conf"),)
        )

        result = self.harness.run(desired, check=False)

        self.assertNotEqual(0, result.returncode)
        self.assertIn("user-owned network interface", result.stderr)
        self.assertEqual(before, self.harness.read_db("network"))
        self.assertFalse(self.harness.reloads())

    def test_default_uci_pending_aborts_before_private_writes_or_reload(self) -> None:
        for package in ("network", "firewall", "openvpn"):
            with self.subTest(package=package), tempfile.TemporaryDirectory(
                prefix="pending-", dir=HERE
            ) as temporary:
                harness = SyncHarness(Path(temporary), find_shell() or "sh")
                harness.write_db(
                    "openvpn",
                    {
                        "user_vpn": "openvpn",
                        "user_vpn.enabled": "1",
                    },
                )
                committed = {
                    name: harness.read_db(name)
                    for name in ("network", "firewall", "openvpn")
                }
                pending = harness.set_default_pending(package)
                desired = harness.desired_path(self._three_rows_for(harness))

                result = harness.run(desired, check=False)

                self.assertNotEqual(0, result.returncode)
                self.assertRegex(
                    result.stderr.lower(), r"(?:pending|uncommitted|uci.*changes)"
                )
                self.assertFalse(
                    harness.uci_writes(),
                    "sync wrote to its private UCI staging before rejecting user changes",
                )
                self.assertFalse(harness.reloads())
                for name, before in committed.items():
                    self.assertEqual(before, harness.read_db(name))
                self.assertEqual(
                    pending,
                    (harness.default_pending / package).read_text(encoding="utf-8"),
                )

    @staticmethod
    def _three_rows_for(
        harness: SyncHarness,
    ) -> tuple[tuple[str, str, str, str, str], ...]:
        generated = shell_path(harness.generated)
        return (
            ("tinc", "mesh", "tinc_mesh", "tnc0", generated + "/tinc/mesh/tinc.conf"),
            (
                "openvpn",
                "site_a",
                "ovpn_site_a",
                "ovpn-site-a",
                generated + "/openvpn/site_a/openvpn.conf",
            ),
            (
                "wireguard",
                "site_b",
                "wg_site_b",
                "wg-site-b",
                generated + "/wireguard/site_b/wg.conf",
            ),
        )

    def test_failed_reload_is_retried_after_uci_was_committed(self) -> None:
        desired = self.harness.desired_path(self.three_rows())
        self.harness.fail_network_once = True

        first = self.harness.run(desired, check=False)

        self.assertNotEqual(0, first.returncode)
        self.assertIn("network reload failed", first.stderr)
        self.assertIn("tinc_mesh", self.harness.read_db("network"))
        self.assertFalse(self.harness.reloads())

        self.harness.run(desired)

        self.assertEqual(["network", "firewall"], self.harness.reloads())


if __name__ == "__main__":
    unittest.main(verbosity=2)

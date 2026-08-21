from __future__ import annotations

import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
PACKAGE = HERE.parent
MAKEFILE = PACKAGE / "Makefile"
FILES = PACKAGE / "files"
INIT = FILES / "etc" / "init.d" / "meduza"
GENERATOR = FILES / "usr" / "libexec" / "meduza" / "meduza-generator"
TRANSACTION = "0123456789abcdef0123456789abcdef"


def package_build_id() -> str:
    source = MAKEFILE.read_text(encoding="utf-8")
    version = re.search(r"^PKG_VERSION:=(\S+)$", source, re.M)
    release = re.search(r"^PKG_RELEASE:=(\S+)$", source, re.M)
    if version is None or release is None:
        raise AssertionError("package version/release is missing")
    return f"{version.group(1)}-r{release.group(1)}"


BUILD_ID = package_build_id()


def find_shell() -> str | None:
    if os.name != "nt":
        return shutil.which("sh")
    candidates = (
        Path(r"C:\Program Files\Git\usr\bin\sh.exe"),
        Path(r"C:\Program Files\Git\bin\sh.exe"),
    )
    return next((str(path) for path in candidates if path.is_file()), None)


def shell_path(path: Path) -> str:
    resolved = path.resolve()
    if os.name != "nt":
        return resolved.as_posix()
    value = resolved.as_posix()
    return "/{}{}".format(value[0].lower(), value[2:])


def postinst_body() -> str:
    source = MAKEFILE.read_text(encoding="utf-8")
    match = re.search(
        r"define Package/meduza-openwrt-lite/postinst\n(?P<body>.*?)\nendef",
        source,
        re.S,
    )
    if match is None:
        raise AssertionError("meduza postinst definition is missing")
    return match.group("body")


def runnable_postinst(root: str) -> str:
    """Render just the package-specific postinst into an isolated root.

    OpenWrt Make escaping doubles every dollar sign.  The path substitution is
    deliberately test-only: it lets the real shell body exercise its file and
    symlink checks without touching the host's /etc or /usr.
    """

    source = postinst_body().replace("$(MEDUZA_BUILD_ID)", BUILD_ID)
    source = source.replace("$$", "$")
    for original, replacement in (
        ("/usr/share/meduza", f"{root}/usr/share/meduza"),
        ("/usr/libexec/meduza", f"{root}/usr/libexec/meduza"),
        ("/etc/init.d", f"{root}/etc/init.d"),
        ("/etc/meduza", f"{root}/etc/meduza"),
        ("/etc/rc.d", f"{root}/etc/rc.d"),
    ):
        source = source.replace(original, replacement)
    return source + "\n"


class PostInstallHarness:
    def __init__(self, directory: Path, shell: str) -> None:
        self.host_root = directory
        self.root = shell_path(directory)
        self.shell = shell
        self.managed = directory / "etc" / "meduza" / "managed"
        self.rc_dir = directory / "etc" / "rc.d"
        self.start_link = self.rc_dir / "S95meduza"
        self.stop_link = self.rc_dir / "K10meduza"
        self.complete = self.managed / "install-complete"
        self.state = self.managed / "upgrade.state"
        self.disabled = self.managed / "upgrade.rc-disabled"
        self.log = directory / "postinst.log"
        self.start_log = directory / "start.log"
        self.bin = directory / "bin"
        for path in (
            self.managed,
            self.rc_dir,
            directory / "etc" / "init.d",
            directory / "usr" / "share" / "meduza",
            directory / "usr" / "libexec" / "meduza",
            self.bin,
        ):
            path.mkdir(parents=True, exist_ok=True)

        (directory / "usr" / "share" / "meduza" / "openwrt-lite-build").write_text(
            BUILD_ID + "\n", encoding="utf-8"
        )
        self.state.write_text(
            f"ready:{TRANSACTION}:{BUILD_ID}\n", encoding="utf-8"
        )
        self.disabled.write_text("disabled\n", encoding="utf-8")
        self._write_executable(self.bin / "fsync", "#!/bin/sh\nexit 0\n")
        self._write_executable(
            self.bin / "logger",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$MEDUZA_TEST_POSTINST_LOG\"\n",
        )
        self._write_executable(
            directory / "usr" / "libexec" / "meduza" / "meduza-recover",
            "#!/bin/sh\n[ \"${1:-}\" = --install-bundle ]\n",
        )
        self._write_executable(
            directory / "etc" / "init.d" / "meduza",
            """#!/bin/sh
printf '%s\n' "$*" >>"$MEDUZA_TEST_START_LOG"
exit "${MEDUZA_TEST_START_STATUS:-0}"
""",
        )
        self.script = directory / "postinst"
        self._write_executable(self.script, runnable_postinst(self.root))

    @staticmethod
    def _write_executable(path: Path, text: str) -> None:
        path.write_text(text, encoding="utf-8", newline="\n")
        path.chmod(0o755)

    def install_default_links(self, *, stop_target: str = "../init.d/meduza") -> None:
        os.symlink("../init.d/meduza", self.start_link)
        os.symlink(stop_target, self.stop_link)

    def run(self, start_status: int = 0) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "IPKG_INSTROOT": "",
                "MEDUZA_TEST_POSTINST_LOG": shell_path(self.log),
                "MEDUZA_TEST_START_LOG": shell_path(self.start_log),
                "MEDUZA_TEST_START_STATUS": str(start_status),
                "PATH": shell_path(self.bin) + ":/usr/bin:/bin",
            }
        )
        return subprocess.run(
            [self.shell, shell_path(self.script)],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

    def diagnostics(self, completed: subprocess.CompletedProcess[str]) -> str:
        logged = self.log.read_text(encoding="utf-8") if self.log.exists() else ""
        return "\n".join((logged, completed.stdout, completed.stderr))


def shell_function(source: str, name: str) -> str:
    match = re.search(
        rf"^{re.escape(name)}\(\)\s*\{{\n(?P<body>.*?)^\}}",
        source,
        re.M | re.S,
    )
    if match is None:
        raise AssertionError(f"shell function is missing: {name}")
    return match.group(0)


class DisabledHandoffHarness:
    def __init__(self, directory: Path, shell: str) -> None:
        self.host_root = directory
        self.root = shell_path(directory)
        self.shell = shell
        self.log = directory / "handoff.log"
        libexec = directory / "usr" / "libexec" / "meduza"
        libexec.mkdir(parents=True)
        generator = libexec / "meduza-generator"
        recovery = libexec / "meduza-recover"
        self._write_executable(
            generator,
            """#!/bin/sh
printf 'generator:%s:%s\n' "${MEDUZA_PRESERVE_PACKAGE_STATE:-}" "$*" >>"$MEDUZA_TEST_HANDOFF_LOG"
exit "${MEDUZA_TEST_GENERATOR_STATUS:-0}"
""",
        )
        self._write_executable(
            recovery,
            """#!/bin/sh
printf 'recovery:%s\n' "$*" >>"$MEDUZA_TEST_HANDOFF_LOG"
exit "${MEDUZA_TEST_RECOVERY_STATUS:-0}"
""",
        )
        function = shell_function(INIT.read_text(encoding="utf-8"), "service_started")
        function = function.replace("/usr/libexec/meduza", f"{self.root}/usr/libexec/meduza")
        function = function.replace("/var/run/meduza", f"{self.root}/var/run/meduza")
        self.script = directory / "disabled-handoff"
        self._write_executable(
            self.script,
            """#!/bin/sh
logger() { :; }
procd_kill() { :; }
wait_meduza_quiescent() { :; }
restore_upgrade_rc_state() {
    printf 'restore\n' >>"$MEDUZA_TEST_HANDOFF_LOG"
    return "${MEDUZA_TEST_RESTORE_STATUS:-0}"
}
clear_upgrade_ready() {
    printf 'clear\n' >>"$MEDUZA_TEST_HANDOFF_LOG"
}
finish_upgrade_rc_state() {
    printf 'finish\n' >>"$MEDUZA_TEST_HANDOFF_LOG"
}
"""
            + function
            + """
MEDUZA_START_STATUS=0
MEDUZA_EXPECT_AGENT=0
MEDUZA_PURGE_REQUESTED=1
MEDUZA_UPGRADE_READY=1
service_started
""",
        )

    @staticmethod
    def _write_executable(path: Path, text: str) -> None:
        path.write_text(text, encoding="utf-8", newline="\n")
        path.chmod(0o755)

    def run(
        self, *, generator_status: int = 0, recovery_status: int = 0
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "MEDUZA_TEST_HANDOFF_LOG": shell_path(self.log),
                "MEDUZA_TEST_GENERATOR_STATUS": str(generator_status),
                "MEDUZA_TEST_RECOVERY_STATUS": str(recovery_status),
                "PATH": "/usr/bin:/bin",
            }
        )
        return subprocess.run(
            [self.shell, shell_path(self.script)],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

    def events(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text(encoding="utf-8").splitlines()


class RcCommonOverrideHarness:
    """Execute the init script's rc.common overrides in an isolated tree."""

    FUNCTIONS = (
        "payload_completion_valid",
        "meduza_rc_link_owned",
        "meduza_rc_path_available",
        "meduza_rc_handoff_active",
        "enable",
        "disable",
        "enabled",
    )

    def __init__(self, directory: Path, shell: str) -> None:
        self.host_root = directory
        self.root = shell_path(directory)
        self.shell = shell
        self.managed = directory / "etc" / "meduza" / "managed"
        self.rc_dir = directory / "etc" / "rc.d"
        self.start_link = self.rc_dir / "S95meduza"
        self.stop_link = self.rc_dir / "K10meduza"
        self.build_file = directory / "usr" / "share" / "meduza" / "openwrt-lite-build"
        self.complete = self.managed / "install-complete"
        self.bin = directory / "bin"
        for path in (self.managed, self.rc_dir, self.build_file.parent, self.bin):
            path.mkdir(parents=True, exist_ok=True)
        self.build_file.write_text(BUILD_ID + "\n", encoding="utf-8")
        self.complete.write_text(
            f"v1\t{BUILD_ID}\t{TRANSACTION}\n", encoding="utf-8"
        )
        self._write_executable(self.bin / "fsync", "#!/bin/sh\nexit 0\n")

        init_source = INIT.read_text(encoding="utf-8")
        body = "\n\n".join(shell_function(init_source, name) for name in self.FUNCTIONS)
        for original, replacement in (
            ("/usr/share/meduza", f"{self.root}/usr/share/meduza"),
            ("/etc/meduza", f"{self.root}/etc/meduza"),
            ("/etc/rc.d", f"{self.root}/etc/rc.d"),
        ):
            body = body.replace(original, replacement)
        self.script = directory / "rc-override"
        self._write_executable(
            self.script,
            "#!/bin/sh\nset -f\n" + body + '\n"$1"\n',
        )

    @staticmethod
    def _write_executable(path: Path, text: str) -> None:
        path.write_text(text, encoding="utf-8", newline="\n")
        path.chmod(0o755)

    def run(self, action: str) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "IPKG_INSTROOT": "",
                "PATH": shell_path(self.bin) + ":/usr/bin:/bin",
            }
        )
        return subprocess.run(
            [self.shell, shell_path(self.script), action],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class PackagePostInstallIntegrationTests(unittest.TestCase):
    def harness(self, directory: Path) -> PostInstallHarness:
        shell = find_shell()
        assert shell is not None
        return PostInstallHarness(directory, shell)

    def test_apk_default_enable_links_are_removed_for_preserved_disabled_state(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-postinst-", dir=HERE
        ) as temporary:
            install = self.harness(Path(temporary))
            # APK's default_postinst creates both links before the package's
            # custom postinst, even though preinst recorded the retained state
            # as disabled.
            install.install_default_links()

            completed = install.run()

            self.assertEqual(0, completed.returncode, install.diagnostics(completed))
            self.assertFalse(os.path.lexists(install.start_link))
            self.assertFalse(os.path.lexists(install.stop_link))
            self.assertEqual(
                f"v1\t{BUILD_ID}\t{TRANSACTION}\n",
                install.complete.read_text(encoding="utf-8"),
            )
            self.assertEqual("start\n", install.start_log.read_text(encoding="utf-8"))

    def test_all_default_enable_links_are_validated_before_any_is_removed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-postinst-", dir=HERE
        ) as temporary:
            install = self.harness(Path(temporary))
            install.install_default_links(stop_target="../init.d/not-meduza")

            completed = install.run()

            self.assertNotEqual(0, completed.returncode)
            self.assertTrue(os.path.lexists(install.start_link))
            self.assertTrue(os.path.lexists(install.stop_link))
            self.assertEqual("../init.d/not-meduza", os.readlink(install.stop_link))
            self.assertFalse(install.complete.exists())
            diagnostics = install.diagnostics(completed)
            self.assertRegex(
                diagnostics,
                r"package post-install failed: stage=[a-z0-9-]+ status=[1-9][0-9]*",
            )

    def test_start_failure_keeps_a_completed_retryable_transaction(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-postinst-", dir=HERE
        ) as temporary:
            install = self.harness(Path(temporary))
            install.install_default_links()

            completed = install.run(start_status=1)

            self.assertEqual(0, completed.returncode, install.diagnostics(completed))
            self.assertEqual(
                f"v1\t{BUILD_ID}\t{TRANSACTION}\n",
                install.complete.read_text(encoding="utf-8"),
            )
            self.assertEqual(
                f"ready:{TRANSACTION}:{BUILD_ID}\n",
                install.state.read_text(encoding="utf-8"),
            )
            self.assertEqual("disabled\n", install.disabled.read_text(encoding="utf-8"))
            self.assertRegex(
                install.diagnostics(completed).lower(),
                r"service.*start.*fail|start.*fail|retry",
            )


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class DisabledPackageHandoffIntegrationTests(unittest.TestCase):
    def harness(self, directory: Path) -> DisabledHandoffHarness:
        shell = find_shell()
        assert shell is not None
        return DisabledHandoffHarness(directory, shell)

    def test_cleanup_preserves_journal_then_clears_it_last(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-postinst-", dir=HERE
        ) as temporary:
            handoff = self.harness(Path(temporary))

            completed = handoff.run()

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertEqual(
                [
                    "restore",
                    "generator:1:--purge",
                    "recovery:--remove-bundle",
                    "clear",
                    "finish",
                ],
                handoff.events(),
            )

    def test_failed_cleanup_never_clears_the_retryable_journal(self) -> None:
        for failure, statuses in (
            ("generator", {"generator_status": 1}),
            ("recovery", {"recovery_status": 1}),
        ):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory(
                prefix="meduza-postinst-", dir=HERE
            ) as temporary:
                handoff = self.harness(Path(temporary))

                completed = handoff.run(**statuses)

                self.assertNotEqual(0, completed.returncode)
                self.assertNotIn("clear", handoff.events())
                self.assertNotIn("finish", handoff.events())


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class RcCommonOverrideIntegrationTests(unittest.TestCase):
    def harness(self, directory: Path) -> RcCommonOverrideHarness:
        shell = find_shell()
        assert shell is not None
        return RcCommonOverrideHarness(directory, shell)

    def test_package_handoff_enable_is_a_noop_over_foreign_paths(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-rc-override-", dir=HERE
        ) as temporary:
            rc = self.harness(Path(temporary))
            (rc.managed / "upgrade.state").write_text(
                f"ready:{TRANSACTION}:{BUILD_ID}\n", encoding="utf-8"
            )
            rc.start_link.write_text("administrator start entry\n", encoding="utf-8")
            rc.stop_link.mkdir()
            marker = rc.stop_link / "foreign"
            marker.write_text("administrator stop entry\n", encoding="utf-8")

            completed = rc.run("enable")

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertEqual(
                "administrator start entry\n",
                rc.start_link.read_text(encoding="utf-8"),
            )
            self.assertEqual(
                "administrator stop entry\n", marker.read_text(encoding="utf-8")
            )

    def test_stable_enable_rejects_foreign_pair_before_creating_any_link(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-rc-override-", dir=HERE
        ) as temporary:
            rc = self.harness(Path(temporary))
            rc.stop_link.write_text("administrator stop entry\n", encoding="utf-8")

            completed = rc.run("enable")

            self.assertNotEqual(0, completed.returncode)
            self.assertFalse(os.path.lexists(rc.start_link))
            self.assertEqual(
                "administrator stop entry\n",
                rc.stop_link.read_text(encoding="utf-8"),
            )

    def test_terminal_first_install_marker_does_not_hide_a_real_conflict(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-rc-override-", dir=HERE
        ) as temporary:
            rc = self.harness(Path(temporary))
            (rc.managed / "upgrade.first-install").write_text(
                f"v1:{TRANSACTION}:{BUILD_ID}\n", encoding="utf-8"
            )
            rc.stop_link.write_text("administrator stop entry\n", encoding="utf-8")

            completed = rc.run("enable")

            self.assertNotEqual(0, completed.returncode)
            self.assertFalse(os.path.lexists(rc.start_link))
            self.assertEqual(
                "administrator stop entry\n",
                rc.stop_link.read_text(encoding="utf-8"),
            )

    def test_disable_rejects_foreign_pair_before_removing_owned_link(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-rc-override-", dir=HERE
        ) as temporary:
            rc = self.harness(Path(temporary))
            try:
                os.symlink("../init.d/meduza", rc.start_link)
            except OSError as error:
                self.skipTest(f"symlinks unavailable: {error}")
            rc.stop_link.write_text("administrator stop entry\n", encoding="utf-8")

            completed = rc.run("disable")

            self.assertNotEqual(0, completed.returncode)
            self.assertTrue(os.path.lexists(rc.start_link))
            self.assertEqual("../init.d/meduza", os.readlink(rc.start_link))
            self.assertEqual(
                "administrator stop entry\n",
                rc.stop_link.read_text(encoding="utf-8"),
            )


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class RecoveryBundleIntegrationTests(unittest.TestCase):
    def test_install_bundle_is_complete_and_idempotent_before_postinst_seal(self) -> None:
        shell = find_shell()
        assert shell is not None
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-", dir=HERE
        ) as temporary:
            root = Path(temporary)
            binary = root / "bin"
            binary.mkdir()
            fsync = binary / "fsync"
            fsync.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8", newline="\n")
            fsync.chmod(0o755)
            python3 = binary / "python3"
            python3.write_text(
                '#!/bin/sh\nexec "{}" "$@"\n'.format(
                    shell_path(Path(sys.executable))
                ),
                encoding="utf-8",
                newline="\n",
            )
            python3.chmod(0o755)
            if os.name == "nt":
                mv = binary / "mv"
                mv.write_text(
                    "#!/bin/sh\n"
                    "[ \"${1:-}\" != -nT ] || { shift; exec /usr/bin/mv -n \"$@\"; }\n"
                    "exec /usr/bin/mv \"$@\"\n",
                    encoding="utf-8",
                    newline="\n",
                )
                mv.chmod(0o755)
            data = root / "etc" / "meduza"
            environment = os.environ.copy()
            environment.update(
                {
                    "MEDUZA_LIBEXEC": shell_path(
                        FILES / "usr" / "libexec" / "meduza"
                    ),
                    "MEDUZA_DATA": shell_path(data),
                    "PATH": shell_path(binary) + ":/usr/bin:/bin",
                }
            )
            recovery = shell_path(
                FILES / "usr" / "libexec" / "meduza" / "meduza-recover"
            )

            for _ in range(2):
                completed = subprocess.run(
                    [shell, recovery, "--install-bundle"],
                    check=False,
                    capture_output=True,
                    text=True,
                    env=environment,
                )
                self.assertEqual(
                    0, completed.returncode, completed.stdout + completed.stderr
                )

            record = data / "managed" / "recovery.bundle"
            ready = data / "recovery" / ".meduza-ready"
            fields = record.read_text(encoding="utf-8").strip().split("\t")
            self.assertEqual("v2", fields[0])
            self.assertEqual("meduza-openwrt-lite-recovery-v1", fields[1])
            self.assertRegex(fields[2], r"^[0-9a-f]{32}$")
            self.assertEqual("owned", fields[3])
            self.assertEqual(6, len(ready.read_text(encoding="utf-8").splitlines()))

            completed = subprocess.run(
                [shell, recovery, "--remove-bundle"],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )
            self.assertEqual(0, completed.returncode, completed.stdout + completed.stderr)
            self.assertFalse(record.exists())
            self.assertFalse((data / "recovery").exists())


class PackagePostInstallSourceContractTests(unittest.TestCase):
    def test_postinst_failure_diagnostic_uses_only_fixed_stage_names(self) -> None:
        body = postinst_body()
        assignments = re.findall(
            r"(?:postinst_stage|(?:MEDUZA_)?POSTINST_STAGE)="
            r"['\"]?([^'\"\s;]+)",
            body,
        )
        self.assertGreaterEqual(len(set(assignments)), 5)
        self.assertTrue(
            all(re.fullmatch(r"[a-z0-9-]+", stage) for stage in assignments),
            assignments,
        )
        self.assertRegex(body, r"trap[^\n]*(?:EXIT|0)")
        self.assertIn("package post-install failed: stage=", body)
        for secret in ("ETCD_USER", "ETCD_PASS", "ETCD_CERT", "ETCD_KEY"):
            self.assertNotIn(secret, body)

    def test_service_start_is_after_completion_and_is_not_package_fatal(self) -> None:
        body = postinst_body()
        completion = body.index('mv -f "$${tmp}" "$${complete}"')
        start = body.index("/etc/init.d/meduza start")
        self.assertLess(completion, start)
        self.assertNotRegex(
            body[start : start + 160],
            r"/etc/init\.d/meduza start\s*\|\|\s*exit 1",
        )
        self.assertRegex(body[start : start + 500], r"logger|postinst_log|log_postinst")

    def test_disabled_rc_links_are_all_validated_before_the_delete_loop(self) -> None:
        body = postinst_body()
        disabled = re.search(
            r"elif \[ -e \"\$\$\{rc_disabled\}\" \]; then"
            r"(?P<body>.*?)\nelse\n\texit 1",
            body,
            re.S,
        )
        self.assertIsNotNone(disabled)
        branch = disabled.group("body")
        loops = list(re.finditer(r"for generated_rc .*?\n\tdone", branch, re.S))
        self.assertGreaterEqual(len(loops), 2)
        validation = loops[0].group(0)
        deletion = loops[1].group(0)
        self.assertIn("readlink", validation)
        self.assertNotIn("rm -f", validation)
        self.assertIn("rm -f", deletion)

    def test_init_overrides_rc_common_without_force_replacing_paths(self) -> None:
        source = INIT.read_text(encoding="utf-8")
        enable = shell_function(source, "enable")
        disable = shell_function(source, "disable")
        enabled = shell_function(source, "enabled")
        self.assertNotIn("ln -sf", enable)
        self.assertIn("meduza_rc_handoff_active", enable)
        self.assertIn("payload_completion_valid", enable)
        self.assertLess(enable.index("meduza_rc_path_available"), enable.index("ln -s"))
        self.assertLess(disable.index("meduza_rc_path_available"), disable.index("rm -f"))
        self.assertEqual(2, enabled.count("meduza_rc_link_owned"))


class DisabledPackageHandoffSourceContractTests(unittest.TestCase):
    def test_journal_is_preserved_during_cleanup_and_cleared_only_after_success(self) -> None:
        service = shell_function(INIT.read_text(encoding="utf-8"), "service_started")
        purge = service.index("MEDUZA_PRESERVE_PACKAGE_STATE=1")
        generator = service.index("meduza-generator --purge", purge)
        recovery = service.index("meduza-recover --remove-bundle", generator)
        clear = service.index("clear_upgrade_ready", recovery)
        finish = service.index("finish_upgrade_rc_state", clear)
        self.assertLess(purge, generator)
        self.assertLess(generator, recovery)
        self.assertLess(recovery, clear)
        self.assertLess(clear, finish)
        success_guard = service.rfind('"$cleanup_status" -eq 0', recovery, clear)
        self.assertNotEqual(-1, success_guard)

    def test_generator_preserve_mode_guards_every_package_journal_removal(self) -> None:
        purge = shell_function(GENERATOR.read_text(encoding="utf-8"), "purge")
        guards = re.findall(
            r'if \[ "\$\{MEDUZA_PRESERVE_PACKAGE_STATE:-0\}" != 1 \]; then'
            r"(?P<body>.*?)\n\tfi",
            purge,
            re.S,
        )
        guarded = "\n".join(guards)
        for journal in (
            "upgrade.rc-link",
            "upgrade.rc-disabled",
            "$UPGRADE_STATE",
            "upgrade.first-install",
        ):
            self.assertIn(journal, guarded)

        unconditional = purge
        for block in guards:
            unconditional = unconditional.replace(block, "")
        self.assertNotRegex(unconditional, r'rm -f[^\n]*"\$UPGRADE_STATE"')
        self.assertNotRegex(unconditional, r'rm -f[^\n]*upgrade\.rc-(?:link|disabled)')


if __name__ == "__main__":
    unittest.main(verbosity=2)

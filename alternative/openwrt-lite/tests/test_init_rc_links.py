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
INIT = PACKAGE / "files" / "etc" / "init.d" / "meduza"
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


def shell_function(source: str, name: str) -> str:
    match = re.search(
        rf"^{re.escape(name)}\(\)\s*\{{\n(?P<body>.*?)^\}}",
        source,
        re.M | re.S,
    )
    if match is None:
        raise AssertionError(f"shell function is missing: {name}")
    return match.group(0)


class InitRcHarness:
    def __init__(self, directory: Path, shell: str) -> None:
        self.root = directory
        self.shell = shell
        self.rc_dir = directory / "etc" / "rc.d"
        self.managed = directory / "etc" / "meduza" / "managed"
        self.start_link = self.rc_dir / "S95meduza"
        self.stop_link = self.rc_dir / "K10meduza"
        self.build_file = directory / "usr" / "share" / "meduza" / "openwrt-lite-build"
        self.complete = self.managed / "install-complete"
        self.binary = directory / "bin"
        for path in (self.rc_dir, self.managed, self.build_file.parent, self.binary):
            path.mkdir(parents=True, exist_ok=True)

        self.build_file.write_text(BUILD_ID + "\n", encoding="utf-8")
        self.complete.write_text(
            f"v1\t{BUILD_ID}\t{TRANSACTION}\n", encoding="utf-8"
        )
        fsync = self.binary / "fsync"
        fsync.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8", newline="\n")
        fsync.chmod(0o755)
        # Git for Windows emulates `ln -s` by copying when native symlinks are
        # disabled.  Use Python so this test exercises the init function's
        # exact link semantics consistently on both host platforms.
        ln = self.binary / "ln"
        ln.write_text(
            "#!/bin/sh\n"
            '[ "$1" = -s ] && [ "$#" -eq 3 ] || exit 2\n'
            'exec "$MEDUZA_TEST_PYTHON" -c '
            "'import os, sys; os.symlink(sys.argv[1], sys.argv[2])' "
            '"$2" "$3"\n',
            encoding="utf-8",
            newline="\n",
        )
        ln.chmod(0o755)

        source = INIT.read_text(encoding="utf-8")
        functions = "\n\n".join(
            shell_function(source, name)
            for name in (
                "payload_completion_valid",
                "meduza_rc_link_owned",
                "meduza_rc_path_available",
                "meduza_rc_handoff_active",
                "enable",
                "disable",
                "enabled",
            )
        )
        root = shell_path(directory)
        for original, replacement in (
            ("/usr/share/meduza", f"{root}/usr/share/meduza"),
            ("/etc/meduza", f"{root}/etc/meduza"),
            ("/etc/rc.d", f"{root}/etc/rc.d"),
        ):
            functions = functions.replace(original, replacement)
        self.script = directory / "init-rc-test"
        self.script.write_text(
            "#!/bin/sh\n" + functions + '\n"$1"\n',
            encoding="utf-8",
            newline="\n",
        )
        self.script.chmod(0o755)

    def run(self, action: str) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "IPKG_INSTROOT": "",
                "MEDUZA_TEST_PYTHON": shell_path(Path(sys.executable)),
                "PATH": shell_path(self.binary) + ":/usr/bin:/bin",
            }
        )
        return subprocess.run(
            [self.shell, shell_path(self.script), action],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

    def set_handoff(self) -> None:
        (self.managed / "upgrade.state").write_text(
            f"blocked:{TRANSACTION}:{BUILD_ID}\n", encoding="utf-8"
        )

    @staticmethod
    def make_foreign(path: Path, kind: str) -> None:
        if kind == "file":
            path.write_text("foreign\n", encoding="utf-8")
        elif kind == "symlink":
            os.symlink("../init.d/not-meduza", path)
        elif kind == "directory":
            path.mkdir()
            (path / "sentinel").write_text("foreign\n", encoding="utf-8")
        else:
            raise AssertionError(f"unknown foreign path kind: {kind}")

    @staticmethod
    def assert_foreign(test: unittest.TestCase, path: Path, kind: str) -> None:
        if kind == "file":
            test.assertTrue(path.is_file())
            test.assertFalse(path.is_symlink())
            test.assertEqual("foreign\n", path.read_text(encoding="utf-8"))
        elif kind == "symlink":
            test.assertTrue(path.is_symlink())
            test.assertEqual("../init.d/not-meduza", os.readlink(path))
        else:
            test.assertTrue(path.is_dir())
            test.assertFalse(path.is_symlink())
            test.assertEqual(["sentinel"], sorted(item.name for item in path.iterdir()))


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class InitRcLinkIntegrationTests(unittest.TestCase):
    def harness(self, directory: Path) -> InitRcHarness:
        shell = find_shell()
        assert shell is not None
        return InitRcHarness(directory, shell)

    def test_package_handoff_enable_is_a_noop_for_every_foreign_path_type(self) -> None:
        for name in ("start_link", "stop_link"):
            for kind in ("file", "symlink", "directory"):
                with self.subTest(path=name, kind=kind), tempfile.TemporaryDirectory(
                    prefix="meduza-init-rc-", dir=HERE
                ) as temporary:
                    init = self.harness(Path(temporary))
                    foreign = getattr(init, name)
                    init.make_foreign(foreign, kind)
                    init.set_handoff()

                    completed = init.run("enable")

                    self.assertEqual(0, completed.returncode, completed.stderr)
                    init.assert_foreign(self, foreign, kind)
                    other = (
                        init.stop_link if name == "start_link" else init.start_link
                    )
                    self.assertFalse(os.path.lexists(other))

    def test_stable_enable_validates_both_paths_before_creating_either(self) -> None:
        for kind in ("file", "symlink", "directory"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory(
                prefix="meduza-init-rc-", dir=HERE
            ) as temporary:
                init = self.harness(Path(temporary))
                init.make_foreign(init.stop_link, kind)

                completed = init.run("enable")

                self.assertNotEqual(0, completed.returncode)
                self.assertFalse(os.path.lexists(init.start_link))
                init.assert_foreign(self, init.stop_link, kind)

    def test_stable_enable_requires_seal_then_creates_the_exact_pair(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-init-rc-", dir=HERE
        ) as temporary:
            init = self.harness(Path(temporary))
            init.complete.unlink()
            completed = init.run("enable")
            self.assertNotEqual(0, completed.returncode)
            self.assertFalse(os.path.lexists(init.start_link))
            self.assertFalse(os.path.lexists(init.stop_link))

            init.complete.write_text(
                f"v1\t{BUILD_ID}\t{TRANSACTION}\n", encoding="utf-8"
            )
            completed = init.run("enable")
            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertEqual("../init.d/meduza", os.readlink(init.start_link))
            self.assertEqual("../init.d/meduza", os.readlink(init.stop_link))
            self.assertEqual(0, init.run("enabled").returncode)

    def test_disable_validates_both_paths_before_removing_either(self) -> None:
        for kind in ("file", "symlink", "directory"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory(
                prefix="meduza-init-rc-", dir=HERE
            ) as temporary:
                init = self.harness(Path(temporary))
                os.symlink("../init.d/meduza", init.start_link)
                init.make_foreign(init.stop_link, kind)

                completed = init.run("disable")

                self.assertNotEqual(0, completed.returncode)
                self.assertEqual("../init.d/meduza", os.readlink(init.start_link))
                init.assert_foreign(self, init.stop_link, kind)

    def test_disable_removes_only_the_exact_complete_pair(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-init-rc-", dir=HERE
        ) as temporary:
            init = self.harness(Path(temporary))
            os.symlink("../init.d/meduza", init.start_link)
            os.symlink("/etc/init.d/meduza", init.stop_link)

            completed = init.run("disable")

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertFalse(os.path.lexists(init.start_link))
            self.assertFalse(os.path.lexists(init.stop_link))
            self.assertNotEqual(0, init.run("enabled").returncode)


if __name__ == "__main__":
    unittest.main(verbosity=2)

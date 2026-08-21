from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
FILES = HERE.parent / "files"
RECOVER = FILES / "usr/libexec/meduza/meduza-recover"
SOURCE = FILES / "usr/libexec/meduza"
OWNER = "meduza-openwrt-lite-recovery-v1"
NONCE = "0123456789abcdef0123456789abcdef"


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


class RecoveryHarness:
    def __init__(self, root: Path, shell: str) -> None:
        self.root = root
        self.shell = shell
        self.data = root / "etc" / "meduza"
        self.managed = self.data / "managed"
        self.record = self.managed / "recovery.bundle"
        self.bundle = self.data / "recovery"
        self.stage = self.data / (".meduza-recovery." + NONCE)
        self.tomb = self.data / (".meduza-recovery-delete." + NONCE)
        self.bin = root / "bin"
        self.managed.mkdir(parents=True)
        self.bin.mkdir()
        fsync = self.bin / "fsync"
        fsync.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8", newline="\n")
        fsync.chmod(0o755)
        python3 = self.bin / "python3"
        python3.write_text(
            '#!/bin/sh\nexec "{}" "$@"\n'.format(
                shell_path(Path(sys.executable))
            ),
            encoding="utf-8",
            newline="\n",
        )
        python3.chmod(0o755)
        if os.name == "nt":
            # MSYS coreutils cannot apply -T to this Windows-backed directory,
            # while the production BusyBox applet supports it.  Preserve the
            # no-clobber behavior for an otherwise uncontended test rename.
            mv = self.bin / "mv"
            mv.write_text(
                "#!/bin/sh\n"
                "[ \"${1:-}\" != -nT ] || { shift; exec /usr/bin/mv -n \"$@\"; }\n"
                "exec /usr/bin/mv \"$@\"\n",
                encoding="utf-8",
                newline="\n",
            )
            mv.chmod(0o755)

    def run(
        self, operation: str = "--install-bundle"
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "MEDUZA_LIBEXEC": shell_path(SOURCE),
                "MEDUZA_DATA": shell_path(self.data),
                "PATH": shell_path(self.bin) + ":/usr/bin:/bin",
            }
        )
        return subprocess.run(
            [self.shell, shell_path(RECOVER), operation],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

    def owned_nonce(self) -> str:
        fields = self.record.read_text(encoding="utf-8").strip().split("\t")
        if len(fields) != 4 or fields[:2] != ["v2", OWNER] or fields[3] != "owned":
            raise AssertionError("expected a v2 owned recovery record")
        return fields[2]

    def set_record(self, nonce: str, phase: str) -> None:
        self.record.write_text(
            "v2\t{}\t{}\t{}\n".format(OWNER, nonce, phase),
            encoding="utf-8",
        )

    def tomb_for(self, nonce: str) -> Path:
        return self.data / (".meduza-recovery-delete." + nonce)

    def assert_complete(self, case: unittest.TestCase) -> None:
        record_value = self.record.read_text(encoding="utf-8").strip()
        if record_value == OWNER:
            expected_owner = OWNER
        else:
            record_fields = record_value.split("\t")
            case.assertEqual(4, len(record_fields))
            case.assertEqual(["v2", OWNER, "owned"], [
                record_fields[0], record_fields[1], record_fields[3]
            ])
            nonce = record_fields[2]
            case.assertRegex(nonce, r"^[0-9a-f]{32}$")
            expected_owner = OWNER + ":" + nonce
        case.assertEqual(
            expected_owner + "\n",
            (self.bundle / ".meduza-owner").read_text(encoding="utf-8"),
        )
        case.assertEqual(
            6,
            len(
                (self.bundle / ".meduza-ready")
                .read_text(encoding="utf-8")
                .splitlines()
            ),
        )


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class RecoveryCreationIntegrationTests(unittest.TestCase):
    def harness(self, root: Path) -> RecoveryHarness:
        shell = find_shell()
        assert shell is not None
        return RecoveryHarness(root, shell)

    def test_nonce_bound_empty_stage_is_safely_resumed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            recovery.record.write_text(
                "v2\t{}\t{}\tcreating\n".format(OWNER, NONCE),
                encoding="utf-8",
            )
            recovery.stage.mkdir()

            completed = recovery.run()

            self.assertEqual(0, completed.returncode, completed.stderr)
            recovery.assert_complete(self)
            self.assertFalse(recovery.stage.exists())

    def test_r27_constant_record_and_empty_target_is_safely_migrated(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            recovery.record.write_text(OWNER + "\n", encoding="utf-8")
            recovery.bundle.mkdir()

            completed = recovery.run()

            self.assertEqual(0, completed.returncode, completed.stderr)
            recovery.assert_complete(self)
            self.assertNotEqual(OWNER, recovery.record.read_text(encoding="utf-8").strip())

    def test_legacy_empty_journal_replays_before_and_after_empty_rmdir(self) -> None:
        for target_exists in (True, False):
            with self.subTest(target_exists=target_exists), tempfile.TemporaryDirectory(
                prefix="meduza-recovery-create-", dir=HERE
            ) as temporary:
                recovery = self.harness(Path(temporary))
                recovery.record.write_text(
                    "v2\t{}\t{}\tlegacy-empty\n".format(OWNER, NONCE),
                    encoding="utf-8",
                )
                if target_exists:
                    recovery.bundle.mkdir()

                completed = recovery.run()

                self.assertEqual(0, completed.returncode, completed.stderr)
                recovery.assert_complete(self)
                self.assertFalse(recovery.stage.exists())

    def test_legacy_empty_journal_never_adopts_nonempty_target(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            recovery.record.write_text(
                "v2\t{}\t{}\tlegacy-empty\n".format(OWNER, NONCE),
                encoding="utf-8",
            )
            recovery.bundle.mkdir()
            foreign = recovery.bundle / "foreign"
            foreign.write_text("administrator data\n", encoding="utf-8")

            completed = recovery.run()

            self.assertNotEqual(0, completed.returncode)
            self.assertEqual("administrator data\n", foreign.read_text(encoding="utf-8"))
            self.assertFalse(recovery.stage.exists())

    def test_legacy_empty_journal_rejects_any_preexisting_stage(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            journal = "v2\t{}\t{}\tlegacy-empty\n".format(OWNER, NONCE)
            recovery.record.write_text(journal, encoding="utf-8")
            recovery.stage.mkdir()

            completed = recovery.run()

            self.assertNotEqual(0, completed.returncode)
            self.assertEqual(journal, recovery.record.read_text(encoding="utf-8"))
            self.assertTrue(recovery.stage.is_dir())
            self.assertFalse(recovery.bundle.exists())

    def test_fresh_install_does_not_publish_intent_over_foreign_target(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            recovery.bundle.mkdir()
            foreign = recovery.bundle / "foreign"
            foreign.write_text("administrator data\n", encoding="utf-8")

            completed = recovery.run()

            self.assertNotEqual(0, completed.returncode)
            self.assertFalse(recovery.record.exists())
            self.assertEqual("administrator data\n", foreign.read_text(encoding="utf-8"))

    def test_exact_legacy_owner_temp_is_promoted_and_completed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            recovery.record.write_text(OWNER + "\n", encoding="utf-8")
            recovery.bundle.mkdir()
            owner_temp = recovery.bundle / ".meduza-owner.meduza.1234"
            owner_temp.write_text(OWNER + "\n", encoding="utf-8")

            completed = recovery.run()

            self.assertEqual(0, completed.returncode, completed.stderr)
            recovery.assert_complete(self)
            self.assertFalse(owner_temp.exists())

    def test_nonce_stage_with_foreign_content_is_not_adopted(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            creation = "v2\t{}\t{}\tcreating\n".format(OWNER, NONCE)
            recovery.record.write_text(creation, encoding="utf-8")
            recovery.stage.mkdir()
            foreign = recovery.stage / "foreign"
            foreign.write_text("administrator data\n", encoding="utf-8")

            completed = recovery.run()

            self.assertNotEqual(0, completed.returncode)
            self.assertEqual("administrator data\n", foreign.read_text(encoding="utf-8"))
            self.assertEqual(creation, recovery.record.read_text(encoding="utf-8"))
            self.assertFalse(recovery.bundle.exists())

    def test_published_bundle_with_foreign_content_is_not_adopted(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-create-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            completed = recovery.run()
            self.assertEqual(0, completed.returncode, completed.stderr)
            foreign = recovery.bundle / "foreign"
            foreign.write_text("administrator data\n", encoding="utf-8")
            creation = "v2\t{}\t{}\tcreating\n".format(OWNER, NONCE)
            recovery.record.write_text(creation, encoding="utf-8")

            completed = recovery.run()

            self.assertNotEqual(0, completed.returncode)
            self.assertEqual("administrator data\n", foreign.read_text(encoding="utf-8"))
            self.assertEqual(creation, recovery.record.read_text(encoding="utf-8"))

    def test_legacy_target_with_foreign_content_or_owner_is_not_adopted(self) -> None:
        for filename, contents in (
            ("foreign", "administrator data\n"),
            (".meduza-owner", "some-other-owner\n"),
        ):
            with self.subTest(filename=filename), tempfile.TemporaryDirectory(
                prefix="meduza-recovery-create-", dir=HERE
            ) as temporary:
                recovery = self.harness(Path(temporary))
                recovery.record.write_text(OWNER + "\n", encoding="utf-8")
                recovery.bundle.mkdir()
                protected = recovery.bundle / filename
                protected.write_text(contents, encoding="utf-8")

                completed = recovery.run()

                self.assertNotEqual(0, completed.returncode)
                self.assertEqual(contents, protected.read_text(encoding="utf-8"))
                self.assertFalse((recovery.bundle / ".meduza-ready").exists())


@unittest.skipUnless(find_shell(), "a POSIX shell is required")
class RecoveryDeletionIntegrationTests(unittest.TestCase):
    def harness(self, root: Path) -> RecoveryHarness:
        shell = find_shell()
        assert shell is not None
        return RecoveryHarness(root, shell)

    def install(self, recovery: RecoveryHarness) -> str:
        completed = recovery.run()
        self.assertEqual(0, completed.returncode, completed.stderr)
        return recovery.owned_nonce()

    def assert_removed(self, recovery: RecoveryHarness, tomb: Path) -> None:
        self.assertFalse(recovery.record.exists())
        self.assertFalse(recovery.bundle.exists())
        self.assertFalse(tomb.exists())

    def test_delete_journal_replays_before_and_after_tomb_rename(self) -> None:
        for renamed in (False, True):
            with self.subTest(renamed=renamed), tempfile.TemporaryDirectory(
                prefix="meduza-recovery-delete-", dir=HERE
            ) as temporary:
                recovery = self.harness(Path(temporary))
                nonce = self.install(recovery)
                tomb = recovery.tomb_for(nonce)
                recovery.set_record(nonce, "deleting")
                if renamed:
                    recovery.bundle.rename(tomb)

                completed = recovery.run("--remove-bundle")

                self.assertEqual(0, completed.returncode, completed.stderr)
                self.assert_removed(recovery, tomb)

    def test_install_finishes_nonce_delete_then_publishes_a_new_generation(self) -> None:
        for renamed in (False, True):
            with self.subTest(renamed=renamed), tempfile.TemporaryDirectory(
                prefix="meduza-recovery-delete-install-", dir=HERE
            ) as temporary:
                recovery = self.harness(Path(temporary))
                old_nonce = self.install(recovery)
                tomb = recovery.tomb_for(old_nonce)
                recovery.set_record(old_nonce, "deleting")
                if renamed:
                    recovery.bundle.rename(tomb)

                completed = recovery.run("--install-bundle")

                self.assertEqual(0, completed.returncode, completed.stderr)
                recovery.assert_complete(self)
                self.assertNotEqual(old_nonce, recovery.owned_nonce())
                self.assertFalse(tomb.exists())

    def test_install_finishes_legacy_delete_then_publishes_v2_bundle(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-install-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            self.install(recovery)
            (recovery.bundle / ".meduza-owner").write_text(
                OWNER + "\n", encoding="utf-8"
            )
            recovery.set_record(NONCE, "deleting-legacy")
            recovery.bundle.rename(recovery.tomb)

            completed = recovery.run("--install-bundle")

            self.assertEqual(0, completed.returncode, completed.stderr)
            recovery.assert_complete(self)
            self.assertFalse(recovery.tomb.exists())

    def test_partial_tomb_without_ready_or_known_file_is_replayed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            nonce = self.install(recovery)
            tomb = recovery.tomb_for(nonce)
            recovery.set_record(nonce, "deleting")
            recovery.bundle.rename(tomb)
            (tomb / ".meduza-ready").unlink()
            (tomb / "meduza-generator").unlink()

            completed = recovery.run("--remove-bundle")

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assert_removed(recovery, tomb)

    def test_empty_tomb_after_owner_removal_is_replayed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            nonce = self.install(recovery)
            tomb = recovery.tomb_for(nonce)
            recovery.set_record(nonce, "deleting")
            recovery.bundle.rename(tomb)
            for entry in tomb.iterdir():
                entry.unlink()

            completed = recovery.run("--remove-bundle")

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assert_removed(recovery, tomb)

    def test_legacy_owned_bundle_uses_nonce_delete_journal(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            self.install(recovery)
            recovery.record.write_text(OWNER + "\n", encoding="utf-8")
            (recovery.bundle / ".meduza-owner").write_text(
                OWNER + "\n", encoding="utf-8"
            )

            completed = recovery.run("--remove-bundle")

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assertFalse(recovery.record.exists())
            self.assertFalse(recovery.bundle.exists())
            self.assertEqual([], list(recovery.data.glob(".meduza-recovery-delete.*")))

    def test_legacy_partial_tomb_is_replayed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            self.install(recovery)
            (recovery.bundle / ".meduza-owner").write_text(
                OWNER + "\n", encoding="utf-8"
            )
            recovery.set_record(NONCE, "deleting-legacy")
            recovery.bundle.rename(recovery.tomb)
            (recovery.tomb / ".meduza-ready").unlink()

            completed = recovery.run("--remove-bundle")

            self.assertEqual(0, completed.returncode, completed.stderr)
            self.assert_removed(recovery, recovery.tomb)

    def test_unknown_regular_file_in_partial_tomb_is_never_deleted(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            nonce = self.install(recovery)
            tomb = recovery.tomb_for(nonce)
            recovery.set_record(nonce, "deleting")
            recovery.bundle.rename(tomb)
            (tomb / ".meduza-ready").unlink()
            foreign = tomb / "foreign"
            foreign.write_text("administrator data\n", encoding="utf-8")

            completed = recovery.run("--remove-bundle")

            self.assertNotEqual(0, completed.returncode)
            self.assertEqual("administrator data\n", foreign.read_text(encoding="utf-8"))
            self.assertTrue((tomb / ".meduza-owner").exists())
            self.assertTrue((tomb / "meduza-generator").exists())

    def test_dangling_symlink_in_partial_tomb_is_never_deleted(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            nonce = self.install(recovery)
            tomb = recovery.tomb_for(nonce)
            recovery.set_record(nonce, "deleting")
            recovery.bundle.rename(tomb)
            dangling = tomb / "foreign-link"
            try:
                dangling.symlink_to(tomb / "missing-target")
            except OSError as error:
                self.skipTest("symlinks unavailable: {}".format(error))

            completed = recovery.run("--remove-bundle")

            self.assertNotEqual(0, completed.returncode)
            self.assertTrue(dangling.is_symlink())
            self.assertTrue((tomb / ".meduza-owner").exists())
            self.assertTrue((tomb / "meduza-generator").exists())

    def test_ownerless_nonempty_tomb_is_never_adopted(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="meduza-recovery-delete-", dir=HERE
        ) as temporary:
            recovery = self.harness(Path(temporary))
            nonce = self.install(recovery)
            tomb = recovery.tomb_for(nonce)
            recovery.set_record(nonce, "deleting")
            recovery.bundle.rename(tomb)
            (tomb / ".meduza-owner").unlink()
            protected = tomb / "meduza-generator"

            completed = recovery.run("--remove-bundle")

            self.assertNotEqual(0, completed.returncode)
            self.assertTrue(protected.exists())
            self.assertTrue(tomb.exists())
            self.assertTrue(recovery.record.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)

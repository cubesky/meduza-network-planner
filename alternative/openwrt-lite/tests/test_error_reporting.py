from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock


HERE = Path(__file__).resolve().parent
FILES = HERE.parent / "files"
GENERATOR = FILES / "usr/libexec/meduza/meduza-generator"
AGENT = FILES / "usr/libexec/meduza/meduza-agent.py"


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


def load_agent():
    etcd3 = types.ModuleType("etcd3")
    grpc = types.ModuleType("grpc")
    spec = importlib.util.spec_from_file_location(
        "meduza_agent_error_reporting_test", AGENT
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load meduza-agent.py")
    module = importlib.util.module_from_spec(spec)
    with mock.patch.dict(sys.modules, {"etcd3": etcd3, "grpc": grpc}):
        spec.loader.exec_module(module)
    return module


class GeneratorErrorReportingTests(unittest.TestCase):
    def test_every_apply_stage_is_a_fixed_non_sensitive_identifier(self) -> None:
        source = GENERATOR.read_text(encoding="utf-8")
        stages = re.findall(r"^\s*apply_stage\s+([^\s#]+)", source, re.M)
        self.assertGreater(len(stages), 20)
        for stage in stages:
            with self.subTest(stage=stage):
                self.assertRegex(stage, r"^[a-z][a-z0-9-]*$")
        self.assertIn("trap 'apply_exit_report \"$?\"' 0", source)
        self.assertIn(
            'message="generator apply failed: stage=$stage status=$status"',
            source,
        )
        self.assertIn('logger -t meduza "$message"', source)
        self.assertIn('printf \'%s\\n\' "meduza: $message" >&2', source)
        self.assertIn(
            "command -v release_generator_lock >/dev/null 2>&1 && "
            "release_generator_lock",
            source,
        )
        apply_main = re.search(
            r"--apply\)(?P<body>.*?)\n\s*;;", source, re.S
        )
        self.assertIsNotNone(apply_main)
        body = apply_main.group("body")
        self.assertLess(body.index("acquire_generator_lock"),
                        body.index("arm_apply_reporting_traps"))
        self.assertLess(body.index("arm_apply_reporting_traps"),
                        body.index("reconcile"))

    def test_posix_exit_trap_reports_stage_without_arguments_or_secrets(self) -> None:
        shell = find_shell()
        if shell is None:
            self.skipTest("a POSIX shell is not available")
        source = GENERATOR.read_text(encoding="utf-8")
        start = source.index("MEDUZA_APPLY_REPORTING=0")
        end = source.index("\njget()", start)
        diagnostic = source[start:end]
        secret = "private-key-material-must-not-appear"
        script = "\n".join(
            (
                "#!/bin/sh",
                "set -e",
                "logger() { :; }",
                diagnostic,
                "begin_apply_reporting",
                "apply_stage uci-validation",
                "ETCD_KEY={}".format(secret),
                "set -- --private-key {}".format(secret),
                "(exit 17)",
            )
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "diagnostic.sh"
            path.write_text(script, encoding="utf-8", newline="\n")
            result = subprocess.run(
                [shell, shell_path(path)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        self.assertEqual(17, result.returncode)
        self.assertIn(
            "generator apply failed: stage=uci-validation status=17",
            result.stderr,
        )
        self.assertNotIn(secret, result.stderr)
        self.assertNotIn("--private-key", result.stderr)


class AgentErrorReportingTests(unittest.TestCase):
    def test_child_failure_exception_does_not_retain_argv_or_output(self) -> None:
        agent = load_agent()
        secret = "private-key-material-must-not-appear"

        class FailedProcess:
            pid = 12345
            returncode = 23

            def __init__(self, *_args, **_kwargs):
                pass

            def communicate(self, timeout=None):
                del timeout
                return ("sensitive stdout " + secret, "sensitive stderr " + secret)

            def poll(self):
                return self.returncode

        with mock.patch.object(agent.subprocess, "Popen", FailedProcess):
            with self.assertRaises(agent.ManagedCommandError) as raised:
                agent.run(
                    "managed-helper",
                    "--private-key",
                    secret,
                    capture=True,
                )

        message = str(raised.exception)
        self.assertEqual("managed helper exited with status 23", message)
        self.assertEqual({"returncode": 23}, raised.exception.__dict__)
        self.assertNotIn(secret, message)
        self.assertNotIn("--private-key", message)

    def test_agent_no_longer_uses_argv_rendering_called_process_error(self) -> None:
        source = AGENT.read_text(encoding="utf-8")
        self.assertNotIn("subprocess.CalledProcessError", source)
        self.assertIn("raise ManagedCommandError(result.returncode)", source)


if __name__ == "__main__":
    unittest.main(verbosity=2)

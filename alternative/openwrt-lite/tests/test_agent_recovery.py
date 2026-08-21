from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock


AGENT_PATH = (
    Path(__file__).resolve().parents[1]
    / "files"
    / "usr"
    / "libexec"
    / "meduza"
    / "meduza-agent.py"
)


def load_agent_module():
    """Load the agent without requiring its OpenWrt-only Python packages."""
    fake_etcd3 = types.ModuleType("etcd3")
    fake_etcd3.client = lambda **_kwargs: None

    fake_grpc = types.ModuleType("grpc")
    fake_grpc.RpcError = Exception
    fake_grpc.StatusCode = types.SimpleNamespace(UNAUTHENTICATED="unauthenticated")

    spec = importlib.util.spec_from_file_location("meduza_agent_recovery_test", AGENT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    with mock.patch.dict(sys.modules, {"etcd3": fake_etcd3, "grpc": fake_grpc}):
        spec.loader.exec_module(module)
    return module


agent_module = load_agent_module()


def bare_agent():
    agent = object.__new__(agent_module.Agent)
    agent.node = "node-a"
    agent.commit = None
    agent.initialized = False
    agent.next_report = float("inf")
    agent.cache_retry_at = 0
    agent.cache_retry_delay = 1
    agent.cache_stop_done = False
    agent.pending_last_ack = None
    return agent


class AgentRecoveryTests(unittest.TestCase):
    def setUp(self) -> None:
        agent_module._stop_requested = False

    def tearDown(self) -> None:
        agent_module._stop_requested = False

    def test_each_failed_cache_apply_is_followed_by_a_safety_stop(self) -> None:
        agent = bare_agent()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / "cache.json"
            pending = root / "cache.pending.json"
            cache.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "node_id": agent.node,
                        "commit": "stable",
                        "node": {},
                        "global": {},
                        "all_nodes": {},
                    }
                ),
                encoding="utf-8",
            )

            calls = []

            def fake_run(*args, **_kwargs):
                calls.append(args)
                if args[-1] == "--apply":
                    raise subprocess.CalledProcessError(1, args)
                return subprocess.CompletedProcess(args, 0)

            with (
                mock.patch.object(agent_module, "CACHE", str(cache)),
                mock.patch.object(agent_module, "CACHE_PENDING", str(pending)),
                mock.patch.object(agent_module, "run", side_effect=fake_run),
                mock.patch.object(agent_module, "log"),
                mock.patch.object(agent, "write_runtime"),
            ):
                self.assertFalse(agent.restore_cache())
                self.assertFalse(agent.restore_cache())

            apply_calls = [call for call in calls if call[-1] == "--apply"]
            stop_calls = [call for call in calls if call[-1] == "--runtime-stop"]
            self.assertEqual(2, len(apply_calls), "the LKG restore should remain retryable")
            self.assertEqual(
                2,
                len(stop_calls),
                "every apply retry can partially mutate state and must be stopped",
            )
            self.assertTrue(agent.cache_stop_done)

    def test_failed_etcd_ack_does_not_undo_successful_local_commit(self) -> None:
        agent = bare_agent()
        agent.cache_stop_done = True

        class RecoveringEtcd:
            ack_attempts = 0

            @staticmethod
            def get(_key):
                return "new-commit"

            @staticmethod
            def get_prefix(_prefix):
                return {}

            @classmethod
            def put(cls, _key, _value, _lease=None):
                cls.ack_attempts += 1
                if cls.ack_attempts == 1:
                    raise RuntimeError("etcd acknowledgement unavailable")

        agent.etcd = RecoveringEtcd()

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / "cache.json"
            pending = root / "cache.pending.json"

            sleep_calls = 0

            def stop_after_retry(_seconds):
                nonlocal sleep_calls
                sleep_calls += 1
                if sleep_calls == 2:
                    raise agent_module.StopRequested()

            with (
                mock.patch.object(agent_module, "CACHE", str(cache)),
                mock.patch.object(agent_module, "CACHE_PENDING", str(pending)),
                mock.patch.object(
                    agent_module,
                    "run",
                    return_value=subprocess.CompletedProcess((), 0),
                ),
                mock.patch.object(agent_module, "fsync_directory"),
                mock.patch.object(agent_module.os, "fchmod", create=True),
                mock.patch.object(
                    agent_module,
                    "interruptible_sleep",
                    side_effect=stop_after_retry,
                ),
                mock.patch.object(agent_module, "log") as mocked_log,
                mock.patch.object(agent, "restore_cache", return_value=False),
                mock.patch.object(agent, "write_runtime"),
            ):
                with self.assertRaises(agent_module.StopRequested):
                    agent.serve()

            self.assertTrue(
                cache.is_file(),
                "the locally applied cache must be promoted; logs={!r}".format(
                    mocked_log.call_args_list
                ),
            )
            self.assertFalse(pending.exists())
            self.assertEqual("new-commit", json.loads(cache.read_text(encoding="utf-8"))["commit"])
            self.assertTrue(
                agent.initialized,
                "a failed remote acknowledgement must not invalidate a committed local LKG",
            )
            self.assertEqual("new-commit", agent.commit)
            self.assertEqual(
                2,
                RecoveringEtcd.ack_attempts,
                "a failed etcd acknowledgement must be retried without re-applying",
            )
            self.assertIsNone(agent.pending_last_ack)


if __name__ == "__main__":
    unittest.main()

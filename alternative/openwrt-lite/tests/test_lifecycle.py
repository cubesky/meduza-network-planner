from __future__ import annotations

import re
from pathlib import Path
import sys
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from lifecycle_model import (
    DesiredInterface,
    FakeOpenWrt,
    OWNER,
    OwnershipError,
    three_vpns,
)


PACKAGE = HERE.parent
FILES = PACKAGE / "files"


def source(relative: str) -> str:
    return (FILES / relative).read_text(encoding="utf-8")


def shell_code(relative: str) -> str:
    """Return shell code with comments and blank lines removed."""

    kept = []
    for line in source(relative).splitlines():
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            kept.append(line)
    return "\n".join(kept)


class LifecycleModelTests(unittest.TestCase):
    def test_three_vpn_types_have_owned_uci_interfaces_and_one_zone(self) -> None:
        router = FakeOpenWrt()
        router.reconcile(three_vpns())

        self.assertEqual(
            {"tinc_mesh", "ovpn_site_a", "wg_site_b"}, set(router.interfaces)
        )
        self.assertEqual(
            {"tnc0", "ovpn-site-a", "wg-site-b"}, set(router.devices)
        )
        self.assertEqual(set(router.interfaces), router.zone_members)
        self.assertEqual(
            {("tinc", "mesh"), ("openvpn", "site_a"), ("wireguard", "site_b")},
            router.processes,
        )

    def test_same_desired_state_does_not_reload_network_or_firewall(self) -> None:
        router = FakeOpenWrt()
        desired = three_vpns()
        router.reconcile(desired)
        counts = (router.network_reloads, router.firewall_reloads)

        router.reconcile(desired)

        self.assertEqual(counts, (router.network_reloads, router.firewall_reloads))

    def test_power_loss_restores_last_known_good_without_etcd(self) -> None:
        router = FakeOpenWrt()
        desired = three_vpns()
        router.reconcile(desired)
        router.power_loss()
        self.assertFalse(router.processes)
        self.assertNotIn("wg-site-b", router.devices)

        router.restore_last_known_good()

        self.assertEqual(set(router.interfaces), router.zone_members)
        self.assertEqual({item.device for item in desired}, set(router.devices))
        self.assertEqual(3, len(router.processes))

    def test_pending_apply_is_recovered_after_power_loss(self) -> None:
        router = FakeOpenWrt()
        stable = three_vpns()
        router.reconcile(stable)
        experimental = DesiredInterface(
            "wireguard",
            "experimental",
            "wg_experimental",
            "wg-experiment",
            "/etc/meduza/wireguard/experimental.conf",
            ("/etc/meduza/wireguard/experimental.conf",),
        )
        router.simulate_interrupted_apply((experimental,))
        self.assertTrue(router.pending_manifest)
        router.power_loss()

        router.restore_last_known_good()

        self.assertFalse(router.pending_manifest)
        self.assertNotIn("wg_experimental", router.interfaces)
        self.assertNotIn("wg-experiment", router.devices)
        self.assertEqual({item.logical for item in stable}, set(router.interfaces))

    def test_rejects_openclash_utun_before_mutation(self) -> None:
        router = FakeOpenWrt()
        router.devices["utun"] = "openclash"
        before = dict(router.devices)

        with self.assertRaises(OwnershipError):
            router.reconcile(
                [DesiredInterface("wireguard", "bad", "wg_bad", "utun", "/tmp/bad")]
            )

        self.assertEqual(before, router.devices)
        self.assertFalse(router.interfaces)

    def test_rejects_user_owned_device_and_uci_section_atomically(self) -> None:
        router = FakeOpenWrt()
        router.devices["wg-user"] = "user"
        harmless = DesiredInterface(
            "tinc", "would_apply", "tinc_would_apply", "tnc9", "/tmp/tinc"
        )
        with self.assertRaises(OwnershipError):
            router.reconcile(
                [
                    harmless,
                    DesiredInterface(
                        "wireguard", "bad", "wg_bad", "wg-user", "/tmp/bad"
                    )
                ]
            )
        self.assertFalse(router.interfaces)
        self.assertNotIn("tnc9", router.devices)

        router.interfaces["ovpn_site"] = type("UserSection", (), {
            "device": "tun-user", "owner": "user", "proto": "none"
        })()
        before_devices = dict(router.devices)
        with self.assertRaises(OwnershipError):
            router.reconcile(
                [
                    DesiredInterface(
                        "openvpn", "site", "ovpn_site", "ovpn-site", "/tmp/bad"
                    )
                ]
            )
        self.assertEqual(before_devices, router.devices)
        self.assertEqual("user", router.interfaces["ovpn_site"].owner)

    def test_runtime_stop_stops_only_owned_runtime_and_keeps_restart_data(self) -> None:
        router = FakeOpenWrt()
        router.devices["utun"] = "openclash"
        router.files["/etc/openclash/config.yaml"] = "openclash"
        router.reconcile(three_vpns())

        router.runtime_stop()

        self.assertEqual({"utun": "openclash"}, dict(router.devices))
        self.assertFalse(router.processes)
        self.assertEqual(3, len(router.interfaces))
        self.assertEqual(3, len(router.zone_members))
        self.assertEqual(3, len(router.persistent_manifest))
        self.assertIn("/etc/openclash/config.yaml", router.files)
        self.assertEqual(3, len(router.persistent_cache))

    def test_purge_removes_managed_files_and_uci_but_preserves_openclash(self) -> None:
        router = FakeOpenWrt()
        router.devices["utun"] = "openclash"
        router.files["/etc/openclash/config.yaml"] = "openclash"
        router.zone_members.add("utun")
        router.reconcile(three_vpns())

        router.purge()

        self.assertEqual({"utun": "openclash"}, dict(router.devices))
        self.assertEqual(
            {"/etc/openclash/config.yaml": "openclash"}, dict(router.files)
        )
        self.assertFalse(router.processes)
        self.assertEqual({"utun"}, router.zone_members)
        self.assertFalse(router.persistent_manifest)

    def test_linux_device_and_uci_name_limits_are_enforced(self) -> None:
        router = FakeOpenWrt()
        with self.assertRaises(ValueError):
            router.reconcile(
                [
                    DesiredInterface(
                        "wireguard",
                        "long",
                        "wg_long",
                        "wg-this-is-over-15",
                        "/tmp/long",
                    )
                ]
            )
        with self.assertRaises(ValueError):
            router.reconcile(
                [
                    DesiredInterface(
                        "openvpn", "dash", "ovpn-has-dash", "ovpn-dash", "/tmp/dash"
                    )
                ]
            )

    def test_manifest_is_exactly_five_tab_separated_columns(self) -> None:
        router = FakeOpenWrt()
        router.reconcile(three_vpns())

        text = router.manifest_text()

        self.assertTrue(text.endswith("\n"))
        self.assertTrue(all(len(line.split("\t")) == 5 for line in text.splitlines()))
        restored = FakeOpenWrt.parse_manifest(text)
        self.assertEqual(
            sorted(
                (item.kind, item.instance, item.logical, item.device, item.config)
                for item in three_vpns()
            ),
            sorted(
                (
                    item.kind,
                    item.instance,
                    item.logical,
                    item.device,
                    item.config,
                )
                for item in restored
            ),
        )


class ProductionSourceContractTests(unittest.TestCase):
    """Guardrails for requirements that the in-memory oracle cannot enforce."""

    def test_sync_has_all_three_vpn_kinds(self) -> None:
        code = "\n".join(
            (
                shell_code("usr/libexec/meduza/meduza-generator"),
                shell_code("usr/libexec/meduza/meduza-openwrt-sync"),
            )
        )
        for token in ("tinc", "openvpn", "wireguard"):
            with self.subTest(token=token):
                self.assertIn(token, code)
        for prefix in ("tinc_", "ovpn_", "wg_"):
            with self.subTest(prefix=prefix):
                self.assertIn(prefix, code)

    def test_persistent_cache_is_outside_tmpfs_and_restored_before_polling(self) -> None:
        agent = source("usr/libexec/meduza/meduza-agent.py")
        self.assertRegex(agent, r'DATA\s*=.*["\']/etc/meduza["\']')
        self.assertTrue(
            "/etc/meduza/cache.json" in agent
            or re.search(
                r'CACHE\s*=\s*os\.path\.join\(DATA,\s*["\']cache\.json["\']\)',
                agent,
            ),
            "persistent cache must resolve to /etc/meduza/cache.json",
        )
        self.assertNotRegex(agent, r'CACHE\s*=\s*["\']/var/(?:run|tmp)')

        cache_functions = re.findall(
            r"def\s+([A-Za-z_]*(?:(?:restore|load)[A-Za-z_]*cache|cache[A-Za-z_]*(?:restore|load))[A-Za-z_]*)\s*\(",
            agent,
            re.I,
        )
        self.assertTrue(cache_functions, "agent needs an explicit startup cache restore")
        self.assertTrue(
            any(len(re.findall(r"\b{}\s*\(".format(re.escape(name)), agent)) >= 2 for name in cache_functions),
            "the cache restore helper must be invoked during startup",
        )

    def test_device_guard_reserves_utun_and_refuses_unowned_existing_links(self) -> None:
        code = "\n".join(
            (
                shell_code("usr/libexec/meduza/meduza-lib.sh"),
                shell_code("usr/libexec/meduza/meduza-generator"),
            )
        )
        self.assertIn(OWNER, code)
        self.assertRegex(code, r"(?:^|[| (])utun(?:$|[| )])")
        self.assertRegex(code, r"ip\s+(?:-[A-Za-z]+\s+)?link\s+show")
        self.assertRegex(
            code,
            r"(?:owned|owner|manifest|managed).*(?:device|dev)|(?:device|dev).*(?:owned|owner|manifest|managed)",
        )

    def test_network_and_firewall_reload_are_change_guarded(self) -> None:
        code = shell_code("usr/libexec/meduza/meduza-openwrt-sync")
        lowered = code.lower()
        network_reloads = list(
            re.finditer(r"(?:ubus[^\n]*network[^\n]*reload|network[^\n]*reload)", lowered)
        )
        if network_reloads:
            self.assertRegex(
                lowered,
                r"network[_A-Za-z]*(?:changed|dirty)|(?:changed|dirty)[_A-Za-z]*network",
            )
        firewall_reloads = list(re.finditer(r"firewall[^\n]*reload", lowered))
        if firewall_reloads:
            self.assertRegex(
                lowered,
                r"firewall[_A-Za-z]*(?:changed|dirty)|(?:changed|dirty)[_A-Za-z]*firewall",
            )

    def test_stop_runs_after_procd_agent_has_stopped(self) -> None:
        init = shell_code("etc/init.d/meduza")
        self.assertIn("service_stopped()", init)
        stopped = re.search(
            r"service_stopped\s*\(\)\s*\{(?P<body>.*?)\n\}", init, re.S
        )
        self.assertIsNotNone(stopped)
        self.assertIn("--runtime-stop", stopped.group("body"))

    def test_wireguard_stop_does_not_wait_for_proto_none_to_delete_link(self) -> None:
        generator = source("usr/libexec/meduza/meduza-generator")
        start = generator.index("stop_runtime_entry()")
        end = generator.index("remove_generated_config()", start)
        body = generator[start:end]
        branch = re.search(
            r"wireguard\)(?P<body>.*?)\n\s*tinc\)", body, re.S
        )
        self.assertIsNotNone(branch)
        self.assertIn("ip link del", branch.group("body"))
        self.assertIn("return 0", branch.group("body"))
        self.assertNotIn("wait_link_absent", branch.group("body"))

    def test_tinc_uses_the_instance_config_directory(self) -> None:
        generator = shell_code("usr/libexec/meduza/meduza-generator")
        self.assertIn('tincd -c "${config%/*}" -n "$instance"', generator)
        self.assertNotIn(
            'tincd -c "$MEDUZA_GENERATED/tinc" -n "$instance"', generator
        )

    def test_runtime_stop_and_purge_use_persistent_ownership_manifest(self) -> None:
        code = "\n".join(
            (
                source("usr/libexec/meduza/meduza-lib.sh"),
                source("usr/libexec/meduza/meduza-generator"),
            )
        )
        self.assertIn("/etc/meduza", code)
        self.assertIn("$MEDUZA_DATA/managed/interfaces", code)
        self.assertIn("$MEDUZA_DATA/managed/interfaces.pending", code)
        self.assertIn("--runtime-stop", code)
        self.assertIn("--purge", code)
        self.assertRegex(code, r"runtime_stop[A-Za-z_]*\s*\(\)|runtime_cleanup[A-Za-z_]*\s*\(\)")
        self.assertRegex(code, r"purge[A-Za-z_]*\s*\(\)|purge_cleanup[A-Za-z_]*\s*\(\)")

    def test_generated_directories_use_external_replayable_phase_records(self) -> None:
        generator = source("usr/libexec/meduza/meduza-generator")
        self.assertIn("generated_record_state()", generator)
        self.assertIn('write_generated_record "creating-$nonce"', generator)
        self.assertIn("write_generated_record owned", generator)
        self.assertIn('write_generated_record "deleting-$nonce"', generator)
        self.assertIn('write_generated_record "empty-$nonce"', generator)
        self.assertIn("secrets.token_hex", generator)
        self.assertIn("finish_generated_delete()", generator)
        self.assertIn(".meduza-delete", generator)
        self.assertIn("clear_generated_tombstone_contents", generator)
        self.assertNotIn("st_ino", generator)
        self.assertIn("generated_dir_is_owned", generator)

    def test_uci_transactions_use_rpcd_sessions_and_validate_not_found(self) -> None:
        helper = source("usr/libexec/meduza/meduza-uci-session.py")
        sync = source("usr/libexec/meduza/meduza-openwrt-sync")
        self.assertIn('ubus("session", "create"', helper)
        self.assertRegex(helper, r'"session",\s*\n\s*"grant"')
        self.assertIn("verify_uci_not_found_context", helper)
        self.assertIn("fcntl.flock", helper)
        self.assertIn("RPCD_SAVEDIR_PREFIX", helper)
        self.assertIn('lines.append("|"', helper)
        self.assertIn('lines.append("~"', helper)
        self.assertIn("uci-ownership.json", helper)
        self.assertIn("meduza_nonce", helper)
        self.assertNotIn("render_package", helper)
        self.assertIn("command_install", helper)
        self.assertIn('ubus("uci", "commit"', helper)
        self.assertIn("edge_add", sync)
        self.assertIn("edge_del", sync)
        self.assertIn("fsync /etc/config", sync)

    def test_frr_origin_and_restore_are_power_loss_replayable(self) -> None:
        generator = source("usr/libexec/meduza/meduza-generator")
        self.assertIn("frr.origin", generator)
        self.assertIn("frr.takeover.pending", generator)
        self.assertIn("frr.restore.reload-needed", generator)
        self.assertIn("origin_action=absent", generator)
        self.assertRegex(generator, r"absent\)\s*\n\s*rm -f /etc/frr/frr\.conf")

    def test_upgrade_is_blocked_until_old_controller_and_snapshot_are_safe(self) -> None:
        makefile = (PACKAGE / "Makefile").read_text(encoding="utf-8")
        init = source("etc/init.d/meduza")
        generator = source("usr/libexec/meduza/meduza-generator")
        helper = source("usr/libexec/meduza/meduza-uci-session.py")
        self.assertIn("upgrade.state", makefile)
        self.assertIn("upgrade.intent", makefile)
        self.assertIn(
            'meduza_atomic_value "blocked:$${tx_nonce}:$${new_build}"',
            makefile,
        )
        self.assertIn("pending", makefile)
        self.assertIn("bound", makefile)
        self.assertIn("force_disable", makefile)
        self.assertIn("meduza_agent_registered", makefile)
        self.assertIn("meduza_signal_processes TERM", makefile)
        self.assertIn("upgrade.state", init)
        self.assertIn("clear_upgrade_ready", init)
        self.assertIn("upgrade_allows_apply", generator)
        self.assertIn("MIGRATION_ZONE_SEAL_PATH", helper)
        self.assertIn("UPGRADE_INTENT_PATH", helper)

    def test_package_completion_seal_is_bound_to_build_and_transaction(self) -> None:
        makefile = (PACKAGE / "Makefile").read_text(encoding="utf-8")
        init = source("etc/init.d/meduza")
        generator = source("usr/libexec/meduza/meduza-generator")
        agent = source("usr/libexec/meduza/meduza-agent.py")

        version = re.search(r"^PKG_VERSION:=(\S+)$", makefile, re.M)
        release = re.search(r"^PKG_RELEASE:=(\S+)$", makefile, re.M)
        self.assertIsNotNone(version)
        self.assertIsNotNone(release)
        build_file = source("usr/share/meduza/openwrt-lite-build").strip()
        self.assertEqual(
            f"{version.group(1)}-r{release.group(1)}",
            build_file,
            "the packaged build identity must track PKG_VERSION/PKG_RELEASE",
        )

        self.assertIn('blocked:$${tx_nonce}:$${new_build}', makefile)
        self.assertIn('ready:$${tx_nonce}:$${new_build}', makefile)
        self.assertIn("printf 'v1\\t%s\\t%s\\n'", makefile)
        self.assertIn("upgrade.first-install", makefile)
        self.assertIn("first_install_replay", makefile)
        self.assertIn(
            'v1:$${tx_nonce}:$${new_build}',
            makefile,
            "a first-install power-loss replay must remain bound to its transaction",
        )

        start = re.search(
            r"start_service\s*\(\)\s*\{(?P<body>.*?)\n\}", init, re.S
        )
        self.assertIsNotNone(start)
        start_body = start.group("body")
        completion = start_body.index("payload_completion_valid")
        self.assertLess(completion, start_body.index("finish_upgrade_rc_state"))
        self.assertLess(completion, start_body.index("config_load meduza"))
        self.assertLess(completion, start_body.index("sysctl -n net.ipv4.ip_forward"))
        self.assertIn('state_tx" = "$MEDUZA_PAYLOAD_TX', start_body)
        self.assertIn('state_build" = "$MEDUZA_PAYLOAD_BUILD', start_body)

        apply = re.search(
            r"--apply\)(?P<body>.*?)\n\s*;;", generator, re.S
        )
        self.assertIsNotNone(apply)
        apply_body = apply.group("body")
        self.assertLess(
            apply_body.index("payload_allows_apply"),
            apply_body.index("upgrade_allows_apply"),
        )
        main = re.search(r"def main\(\):(?P<body>.*?)(?:\n\n|\Z)", agent, re.S)
        self.assertIsNotNone(main)
        self.assertLess(
            main.group("body").index("payload_allows_agent"),
            main.group("body").index("Agent().serve"),
        )
        self.assertIn('state_fields[0] != "ready"', agent)

    def test_install_completion_is_not_removed_by_operational_purge(self) -> None:
        makefile = (PACKAGE / "Makefile").read_text(encoding="utf-8")
        generator = source("usr/libexec/meduza/meduza-generator")
        recovery = source("usr/libexec/meduza/meduza-recover")

        prerm = re.search(
            r"define Package/meduza-openwrt-lite/prerm(?P<body>.*?)\nendef",
            makefile,
            re.S,
        )
        purge = re.search(r"purge\s*\(\)\s*\{(?P<body>.*?)\n\}", generator, re.S)
        self.assertIsNotNone(prerm)
        self.assertIsNotNone(purge)
        self.assertIn("install-complete", prerm.group("body"))
        self.assertNotIn("install-complete", purge.group("body"))
        self.assertIn("remove_install_completion", recovery)
        retry = re.search(
            r"retry_purge\s*\(\)\s*\{(?P<body>.*?)\n\}", recovery, re.S
        )
        self.assertIsNotNone(retry)
        self.assertLess(
            retry.group("body").index('"$BUNDLE/meduza-generator" --purge'),
            retry.group("body").index("remove_install_completion"),
        )

    def test_apk_uninstall_keeps_an_owner_marked_retryable_cleanup_bundle(self) -> None:
        makefile = (PACKAGE / "Makefile").read_text(encoding="utf-8")
        recovery = source("usr/libexec/meduza/meduza-recover")
        generator = source("usr/libexec/meduza/meduza-generator")
        sync = source("usr/libexec/meduza/meduza-openwrt-sync")
        self.assertIn("--install-bundle", makefile)
        self.assertIn("--remove-bundle", makefile)
        self.assertIn("meduza-recover", makefile)
        self.assertIn("meduza-openwrt-lite-recovery-v1", recovery)
        self.assertIn("--purge", recovery)
        self.assertIn("MEDUZA_LIBEXEC", generator)
        self.assertIn("MEDUZA_LIBEXEC", sync)


if __name__ == "__main__":
    unittest.main(verbosity=2)

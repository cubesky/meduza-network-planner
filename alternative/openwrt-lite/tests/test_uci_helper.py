from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import sys
import types
import unittest
from unittest import mock


HELPER = (
    Path(__file__).resolve().parents[1]
    / "files"
    / "usr"
    / "libexec"
    / "meduza"
    / "meduza-uci-session.py"
)
if os.name == "nt":
    sys.modules.setdefault(
        "fcntl",
        types.SimpleNamespace(flock=lambda *_args: None, LOCK_EX=2, LOCK_UN=8),
    )
SPEC = importlib.util.spec_from_file_location("meduza_uci_session", HELPER)
assert SPEC is not None and SPEC.loader is not None
uci = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(uci)


class UciHelperTests(unittest.TestCase):
    def test_scalar_zone_is_normalized_with_element_deltas(self) -> None:
        lines = uci.list_delta_lines(
            "firewall", "vpn", "network", "lan utun", "wg_site", True
        )
        self.assertEqual(
            [
                "|firewall.vpn.network='lan'",
                "|firewall.vpn.network='utun'",
                "~firewall.vpn.network='lan utun'",
                "|firewall.vpn.network='wg_site'",
            ],
            lines,
        )
        self.assertNotIn("-firewall.vpn.network", "\n".join(lines))

    def test_only_owned_member_is_deleted(self) -> None:
        lines = uci.list_delta_lines(
            "firewall",
            "vpn",
            "network",
            ["lan", "utun", "wg_site"],
            "wg_site",
            False,
        )
        self.assertEqual(["~firewall.vpn.network='wg_site'"], lines)

    def test_section_fingerprint_is_full_and_type_sensitive(self) -> None:
        scalar = {
            ".type": "interface",
            "proto": "none",
            "device": "wg0",
            "meduza_owner": "meduza-openwrt-lite",
        }
        listed = dict(scalar, device=["wg0"])
        extended = dict(scalar, metric="20")
        self.assertNotEqual(
            uci.section_fingerprint(scalar), uci.section_fingerprint(listed)
        )
        self.assertNotEqual(
            uci.section_fingerprint(scalar), uci.section_fingerprint(extended)
        )

    def test_package_comparison_ignores_anonymous_cfg_id_changes(self) -> None:
        before = {
            "cfg001": {
                ".type": "zone",
                ".anonymous": True,
                ".name": "cfg001",
                ".index": 1,
                "name": "meduza-vpn",
                "network": ["lan", "wg_site"],
            }
        }
        after = {
            "cfg9abc": {
                ".type": "zone",
                ".anonymous": True,
                ".name": "cfg9abc",
                ".index": 4,
                "name": "meduza-vpn",
                "network": ["lan", "wg_site"],
            }
        }
        self.assertTrue(uci.package_state_equal(before, after))
        after["cfg9abc"]["network"].append("utun")
        self.assertFalse(uci.package_state_equal(before, after))

    def test_transition_record_accepts_only_before_or_after(self) -> None:
        record = {
            "version": 1,
            "phase": "updating",
            "before": "a" * 64,
            "after": "b" * 64,
        }
        self.assertTrue(uci.record_authorizes(record, "a" * 64))
        self.assertTrue(uci.record_authorizes(record, "b" * 64))
        self.assertFalse(uci.record_authorizes(record, "c" * 64))

    def test_firewall_edge_uses_zone_name_across_anonymous_id_changes(self) -> None:
        ownership = {
            "version": 1,
            "sections": {
                "network.wg_site": {
                    "version": 1,
                    "nonce": "a" * 32,
                    "phase": "owned",
                }
            },
            "edges": {},
        }
        saved = []
        current = {"value": ["lan"], "tag": None}

        def get_value(_session, expression, quiet=False):
            del quiet
            if expression.endswith(".name"):
                return "meduza-vpn"
            if expression.endswith(".network"):
                return current["value"]
            if ".meduza_edge_" in expression:
                return current["tag"]
            raise AssertionError(expression)

        with (
            mock.patch.object(uci, "get_value", side_effect=get_value),
            mock.patch.object(uci, "ensure_owned_section", return_value=True),
            mock.patch.object(uci, "load_ownership", return_value=ownership),
            mock.patch.object(uci, "save_ownership", side_effect=lambda value: saved.append(value)),
            mock.patch.object(uci, "append_session_delta"),
            mock.patch.object(uci, "ubus", return_value={}),
        ):
            self.assertEqual(
                0,
                uci.command_edge(
                    "b" * 32, "firewall.cfg001.network=wg_site", True
                ),
            )
            key = uci.edge_key("meduza-vpn", "wg_site")
            nonce = ownership["edges"][key]["nonce"]
            current["value"] = ["lan", "wg_site"]
            current["tag"] = uci.owned_edge_tag(nonce)
            self.assertEqual(
                0,
                uci.command_edge(
                    "b" * 32, "firewall.cfg999.network=wg_site", True
                ),
            )
            uci.promote_firewall_edges(
                {
                    "cfg999": {
                        ".type": "zone",
                        "name": "meduza-vpn",
                        "network": current["value"],
                        uci.edge_tag_option("wg_site"): current["tag"],
                    }
                }
            )

        self.assertEqual([key], list(ownership["edges"]))
        self.assertEqual("cfg999", ownership["edges"][key]["section"])
        self.assertEqual("owned", ownership["edges"][key]["phase"])
        self.assertTrue(saved)

    def test_committed_edge_delete_is_promoted_without_a_new_delta(self) -> None:
        member = "wg_site"
        zone_name = "meduza-vpn"
        nonce = "c" * 32
        tag_option = uci.edge_tag_option(member)
        key = uci.edge_key(zone_name, member)
        ownership = {
            "version": 1,
            "sections": {
                "network." + member: {
                    "version": 1,
                    "nonce": "a" * 32,
                    "phase": "deleting",
                }
            },
            "edges": {
                key: {
                    "version": 1,
                    "section": "cfg001",
                    "zone_name": zone_name,
                    "member": member,
                    "network_nonce": "a" * 32,
                    "nonce": nonce,
                    "tag_option": tag_option,
                    "phase": "deleting",
                }
            },
        }
        live = {
            "cfg999": {
                ".type": "zone",
                "name": zone_name,
                "network": ["lan", "utun"],
                tag_option: uci.removed_edge_tag(nonce),
            }
        }
        with (
            mock.patch.object(uci, "load_ownership", return_value=ownership),
            mock.patch.object(uci, "save_ownership"),
        ):
            uci.promote_firewall_edges(live)
            self.assertEqual("retired", ownership["edges"][key]["phase"])
            # Tag cleanup may have committed before the external JSON record
            # was removed.  That replay state is valid and must reach finalize.
            del live["cfg999"][tag_option]
            uci.promote_firewall_edges(live)

    def test_uncommitted_edge_creation_can_be_cancelled_safely(self) -> None:
        member = "wg_site"
        zone_name = "meduza-vpn"
        key = uci.edge_key(zone_name, member)
        ownership = {
            "version": 1,
            "sections": {
                "network." + member: {
                    "version": 1,
                    "nonce": "a" * 32,
                    "phase": "owned",
                }
            },
            "edges": {
                key: {
                    "version": 1,
                    "section": "cfg001",
                    "zone_name": zone_name,
                    "member": member,
                    "network_nonce": "a" * 32,
                    "nonce": "c" * 32,
                    "tag_option": uci.edge_tag_option(member),
                    "phase": "creating",
                }
            },
        }

        def get_value(_session, expression, quiet=False):
            del quiet
            if expression.endswith(".name"):
                return zone_name
            return None

        with (
            mock.patch.object(uci, "get_value", side_effect=get_value),
            mock.patch.object(uci, "load_ownership", return_value=ownership),
            mock.patch.object(uci, "save_ownership"),
        ):
            self.assertEqual(
                0,
                uci.command_edge(
                    "b" * 32, "firewall.cfg001.network=" + member, False
                ),
            )
        self.assertNotIn(key, ownership["edges"])

    def test_owned_edge_follows_an_exact_tag_when_zone_is_renamed(self) -> None:
        member = "wg_site"
        nonce = "c" * 32
        tag_option = uci.edge_tag_option(member)
        old_key = uci.edge_key("old-zone", member)
        new_key = uci.edge_key("new-zone", member)
        ownership = {
            "version": 1,
            "sections": {
                "network." + member: {
                    "version": 1,
                    "nonce": "a" * 32,
                    "phase": "owned",
                }
            },
            "edges": {
                old_key: {
                    "version": 1,
                    "section": "cfg001",
                    "zone_name": "old-zone",
                    "member": member,
                    "network_nonce": "a" * 32,
                    "nonce": nonce,
                    "tag_option": tag_option,
                    "phase": "owned",
                }
            },
        }
        live = {
            "cfg999": {
                ".type": "zone",
                "name": "new-zone",
                "network": ["lan", member],
                tag_option: uci.owned_edge_tag(nonce),
            }
        }
        with (
            mock.patch.object(uci, "load_ownership", return_value=ownership),
            mock.patch.object(uci, "save_ownership"),
        ):
            uci.promote_firewall_edges(live)
        self.assertNotIn(old_key, ownership["edges"])
        self.assertEqual("new-zone", ownership["edges"][new_key]["zone_name"])
        self.assertEqual("cfg999", ownership["edges"][new_key]["section"])

    def test_edge_is_retired_when_its_zone_and_tag_are_gone(self) -> None:
        member = "wg_site"
        nonce = "c" * 32
        key = uci.edge_key("removed-zone", member)
        ownership = {
            "version": 1,
            "sections": {
                "network." + member: {
                    "version": 1,
                    "nonce": "a" * 32,
                    "phase": "owned",
                }
            },
            "edges": {
                key: {
                    "version": 1,
                    "section": "cfg001",
                    "zone_name": "removed-zone",
                    "member": member,
                    "network_nonce": "a" * 32,
                    "nonce": nonce,
                    "tag_option": uci.edge_tag_option(member),
                    "phase": "owned",
                }
            },
        }
        with (
            mock.patch.object(uci, "load_ownership", return_value=ownership),
            mock.patch.object(uci, "save_ownership"),
        ):
            uci.promote_firewall_edges({})
        self.assertEqual("retired", ownership["edges"][key]["phase"])

    def test_recreated_edge_binds_to_the_new_network_generation(self) -> None:
        member = "wg_site"
        zone_name = "meduza-vpn"
        old_nonce = "a" * 32
        new_network_nonce = "b" * 32
        tag_option = uci.edge_tag_option(member)
        key = uci.edge_key(zone_name, member)
        ownership = {
            "version": 1,
            "sections": {
                "network." + member: {
                    "version": 1,
                    "nonce": new_network_nonce,
                    "phase": "owned",
                }
            },
            "edges": {
                key: {
                    "version": 1,
                    "section": "cfg001",
                    "zone_name": zone_name,
                    "member": member,
                    "network_nonce": old_nonce,
                    "nonce": "c" * 32,
                    "tag_option": tag_option,
                    "phase": "retired",
                    "migration_delete": True,
                }
            },
        }

        def get_value(_session, expression, quiet=False):
            del quiet
            if expression.endswith(".name"):
                return zone_name
            if expression.endswith(".network"):
                return ["lan"]
            if expression.endswith("." + tag_option):
                return uci.removed_edge_tag("c" * 32)
            raise AssertionError(expression)

        with (
            mock.patch.object(uci, "get_value", side_effect=get_value),
            mock.patch.object(uci, "ensure_owned_section", return_value=True),
            mock.patch.object(uci, "load_ownership", return_value=ownership),
            mock.patch.object(uci, "save_ownership"),
            mock.patch.object(uci, "append_session_delta"),
            mock.patch.object(uci, "ubus", return_value={}),
        ):
            self.assertEqual(
                0,
                uci.command_edge(
                    "d" * 32, "firewall.cfg001.network=" + member, True
                ),
            )
        record = ownership["edges"][key]
        self.assertEqual(new_network_nonce, record["network_nonce"])
        self.assertEqual("creating", record["phase"])
        self.assertNotIn("migration_delete", record)

    def test_edge_promotion_rejects_a_different_network_generation(self) -> None:
        member = "wg_site"
        nonce = "c" * 32
        tag_option = uci.edge_tag_option(member)
        key = uci.edge_key("meduza-vpn", member)
        ownership = {
            "version": 1,
            "sections": {
                "network." + member: {
                    "version": 1,
                    "nonce": "b" * 32,
                    "phase": "owned",
                }
            },
            "edges": {
                key: {
                    "version": 1,
                    "section": "cfg001",
                    "zone_name": "meduza-vpn",
                    "member": member,
                    "network_nonce": "a" * 32,
                    "nonce": nonce,
                    "tag_option": tag_option,
                    "phase": "creating",
                }
            },
        }
        live = {
            "cfg001": {
                ".type": "zone",
                "name": "meduza-vpn",
                "network": ["lan", member],
                tag_option: uci.owned_edge_tag(nonce),
            }
        }
        with mock.patch.object(uci, "load_ownership", return_value=ownership):
            with self.assertRaises(uci.UbusError):
                uci.promote_firewall_edges(live)


if __name__ == "__main__":
    unittest.main(verbosity=2)

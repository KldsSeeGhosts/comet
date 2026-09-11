"""Host-independent tests. These do not reproduce a Hyprland input freeze."""
from __future__ import annotations

from contextlib import redirect_stdout
from io import StringIO
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import capture


def lane(**overrides):
    return {"lane": 0, "reserved": False, "lease_active": False,
            "held_button": 0, "held_keys": 0, "drag_active": False,
            "pointer_focus": False, "keyboard_focus": False,
            "seat_resources": 50, "dispatches": 7, **overrides}


def report(*lanes):
    return {"input": {"transport_ready": True, "lanes": list(lanes)}}


class StatusTests(unittest.TestCase):
    def test_absent_plugin_is_not_reported_clean(self):
        for value in (None, {}, [], {"input": None}, {"experiment": {}}):
            with self.subTest(value=value):
                self.assertEqual(capture.summarize_status(value)["state"], "unavailable")

    def test_incomplete_lane_is_not_reported_clean(self):
        for value in (report(), report(None), report({}), {"input": {"lanes": "bad"}}):
            with self.subTest(value=value):
                self.assertEqual(capture.summarize_status(value)["state"], "incomplete")

    def test_missing_each_authority_field_is_rejected(self):
        for key in capture.AUTHORITY_FIELDS:
            value = lane()
            del value[key]
            with self.subTest(key=key):
                self.assertEqual(capture.summarize_status(report(value))["state"], "incomplete")

    def test_boolean_fields_require_booleans(self):
        for key in ("reserved", "lease_active", "drag_active"):
            for value in (0, 1, "false", None):
                with self.subTest(key=key, value=value):
                    self.assertEqual(capture.summarize_status(report(lane(**{key: value})))["state"], "incomplete")

    def test_held_fields_require_nonnegative_integers(self):
        for key in ("held_button", "held_keys"):
            for value in (-1, 0.0, False, "0", None):
                with self.subTest(key=key, value=value):
                    self.assertEqual(capture.summarize_status(report(lane(**{key: value})))["state"], "incomplete")

    def test_passive_hover_and_seat_objects_are_not_active_authority(self):
        result = capture.summarize_status(report(lane(pointer_focus=True, seat_resources=500)))
        self.assertEqual(result["state"], "no_active_authority_observed")
        self.assertTrue(result["lanes"][0]["pointer_focus"])

    def test_reservation_is_distinct_from_active_authority(self):
        result = capture.summarize_status(report(lane(reserved=True)))
        self.assertEqual(result["state"], "reserved_without_active_authority")
        self.assertFalse(result["lanes"][0]["active_authority_or_held_input"])

    def test_each_active_signal_is_reported(self):
        for key, value in (("lease_active", True), ("drag_active", True), ("held_button", 272), ("held_keys", 1)):
            with self.subTest(key=key):
                result = capture.summarize_status(report(lane(**{key: value})))
                self.assertEqual(result["state"], "active_authority_or_held_input")

    def test_second_lane_is_not_ignored(self):
        result = capture.summarize_status(report(lane(), lane(lane=1, held_keys=1)))
        self.assertEqual(result["state"], "active_authority_or_held_input")
        self.assertEqual(len(result["lanes"]), 2)

    def test_input_is_not_mutated(self):
        value = report(lane())
        before = json.dumps(value, sort_keys=True)
        capture.summarize_status(value)
        self.assertEqual(json.dumps(value, sort_keys=True), before)


class CommandTests(unittest.TestCase):
    def test_successful_json(self):
        completed = subprocess.CompletedProcess(["probe"], 0, b'{"ok":true}', b'')
        with patch.object(capture.subprocess, "run", return_value=completed) as run:
            result = capture.command(["probe"])
        self.assertEqual(result["json"], {"ok": True})
        self.assertFalse(result["truncated"])
        self.assertNotIn("shell", run.call_args.kwargs)

    def test_failed_command_does_not_supply_status_json(self):
        completed = subprocess.CompletedProcess(["probe"], 1, b'{"ok":true}', b'failed')
        with patch.object(capture.subprocess, "run", return_value=completed):
            result = capture.command(["probe"])
        self.assertNotIn("json", result)
        self.assertEqual(result["returncode"], 1)

    def test_non_json_and_bad_utf8_do_not_crash(self):
        completed = subprocess.CompletedProcess(["probe"], 0, b'not-json\xff', b'')
        with patch.object(capture.subprocess, "run", return_value=completed):
            result = capture.command(["probe"])
        self.assertNotIn("json", result)
        self.assertIn("not-json", result["stdout"])

    def test_missing_executable_and_timeout_are_observations(self):
        for error in (FileNotFoundError("missing"), subprocess.TimeoutExpired(["probe"], 1)):
            with self.subTest(error=type(error).__name__):
                with patch.object(capture.subprocess, "run", side_effect=error):
                    result = capture.command(["probe"])
                self.assertIn("error", result)
                self.assertNotIn("json", result)

    def test_output_excerpt_is_limited_and_not_parsed(self):
        completed = subprocess.CompletedProcess(["probe"], 0, b'{}extra', b'')
        with patch.object(capture, "MAX_OUTPUT", 2), patch.object(capture.subprocess, "run", return_value=completed):
            result = capture.command(["probe"])
        self.assertEqual(result["stdout"], "{}")
        self.assertTrue(result["truncated"])
        self.assertNotIn("json", result)


class LocalFileTests(unittest.TestCase):
    def test_output_permissions_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as root:
            directory = capture.private_directory(Path(root))
            self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
            target = directory / "test.json"
            capture.write_json(target, {"ok": True})
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)
            self.assertEqual(json.loads(target.read_text()), {"ok": True})
            with self.assertRaises(FileExistsError):
                capture.write_json(target, {"overwritten": True})

    def test_seat_enumeration_excludes_unrelated_interfaces(self):
        text = """interface: 'wl_seat', version: 9, name: 10
    name: seat0
    capabilities: pointer keyboard
interface: 'wl_output', version: 4, name: 11
    name: DP-1
interface: 'wl_seat', version: 9, name: 12
    name: Cua-Agent
    capabilities: pointer keyboard
"""
        seats = capture.seat_sections(text)
        self.assertEqual([entry["seat_name"] for entry in seats], ["seat0", "Cua-Agent"])
        self.assertEqual([entry["registry_id"] for entry in seats], [10, 12])

    def test_inventory_keeps_driver_parent_but_not_arguments(self):
        with tempfile.TemporaryDirectory() as root:
            proc = Path(root)
            for pid, name, parent, args in ((100, "fish", 1, b"fish\0"), (101, "cua-driver", 100, b"cua-driver\0mcp\0--secret\0DO-NOT-RECORD\0"), (102, "unrelated", 1, b"unrelated\0")):
                entry = proc / str(pid)
                entry.mkdir()
                (entry / "comm").write_text(name + "\n")
                (entry / "status").write_text(f"Name:\t{name}\nPPid:\t{parent}\n")
                (entry / "cmdline").write_bytes(args)
                (entry / "exe").symlink_to(f"/usr/bin/{name}")
            (proc / "103").mkdir()  # A disappearing/incomplete process is ignored.
            result = capture.process_inventory(proc)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["parent_comm"], "fish")
        self.assertEqual(result[0]["driver_mode"], "mcp")
        self.assertNotIn("DO-NOT-RECORD", json.dumps(result))

    def test_single_sample_cli_without_desktop(self):
        with tempfile.TemporaryDirectory() as root:
            argv = ["capture.py", "--out-root", root]
            old_umask = os.umask(0o077)
            try:
                with patch.object(capture, "metadata", return_value={}), patch.object(capture, "command", return_value={"json": report(lane())}), patch.object(capture, "process_inventory", return_value=[]), patch("sys.argv", argv), redirect_stdout(StringIO()):
                    self.assertEqual(capture.main(), 0)
                directory = next(Path(root).iterdir())
                summary = json.loads((directory / "summary.json").read_text())
                self.assertEqual(summary["samples"], 1)
                self.assertEqual(summary["states"], {"no_active_authority_observed": 1})
                self.assertEqual(len((directory / "samples.jsonl").read_text().splitlines()), 1)
            finally:
                os.umask(old_umask)


if __name__ == "__main__":
    unittest.main()

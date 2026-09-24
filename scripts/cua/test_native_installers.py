"""Installer tests for the Cua native repairs that follow apply_cua.

CUA_PATCH_TEST_SOURCE points at a Cua Git checkout. Each installer is checked
against the legacy baseline (CUA_PATCH_TEST_LEGACY, default 4af83697) and the
integrated checkout (CUA_PATCH_TEST_INTEGRATED, default HEAD). Installers whose
files are missing from a sparse checkout are skipped. apply_cua and the
Hyprland runtime installer have their own suites.
"""
from contextlib import redirect_stdout
from io import StringIO
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).parent
SOURCE = os.environ.get("CUA_PATCH_TEST_SOURCE")
LEGACY_BASELINE = "4af83697b8425944d668c543851ef6ae3639a130"
LEGACY = os.environ.get("CUA_PATCH_TEST_LEGACY") or LEGACY_BASELINE
INTEGRATED = os.environ.get("CUA_PATCH_TEST_INTEGRATED", "HEAD")
COMPAT = "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_compatibility.rs"
HYPR_INPUT = "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_input.rs"
IMPL = "libs/cua-driver/rust/crates/platform-linux/src/tools/impl_.rs"
HPP = "libs/cua-driver/hyprland-plugin/src/foreground_route.hpp"
INPUT_CPP = "libs/cua-driver/hyprland-plugin/src/input_experiment.cpp"
TEST_CPP = "libs/cua-driver/hyprland-plugin/tests/foreground_route_test.cpp"
GIT_IDENTITY = ("-c", "user.email=installer-tests@example.invalid", "-c", "user.name=Installer Tests",
                "-c", "commit.gpgsign=false")


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / "native" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CASES = {
    "zen-background": {
        "script": "apply_zen_background",
        "module": load("apply_zen_background"),
        "files": (COMPAT,),
        "record": ".git/noches-cua-zen-background.json",
        "record_kind": "file",
        "partial": (COMPAT, "1.22b-1", "9.99x-9"),
    },
    "background-text": {
        "script": "apply_hyprland_text",
        "module": load("apply_hyprland_text"),
        "files": (HYPR_INPUT, IMPL),
        "record": ".git/noches-cua-hyprland-text.json",
        "record_kind": "files",
        "partial": (IMPL, "execute_background_text(", "execute_foreground_text("),
        "parents": (".git/noches-cua-output-patch.json", HYPR_INPUT),
    },
    "duplicate-seat": {
        "script": "apply_hyprland",
        "module": load("apply_hyprland"),
        "files": (HPP, INPUT_CPP, TEST_CPP),
        "record": ".git/noches-cua-hyprland-multibind.json",
        "record_kind": "files",
        "partial": (HPP, "available()", "usable()"),
    },
    "same-client": {
        "script": "apply_hyprland_same_client",
        "module": load("apply_hyprland_same_client"),
        "files": (INPUT_CPP,),
        "record": ".git/noches-cua-hyprland-same-client.json",
        "record_kind": "file",
        "partial": (INPUT_CPP, "pointer == surface || keyboard == surface", "pointer == nullptr"),
        "parents": (".git/noches-cua-hyprland-multibind.json", INPUT_CPP),
    },
}


def git(source, *args):
    return subprocess.run(["git", "-C", source, *args], capture_output=True)


def rev_exists(source, rev):
    return bool(source) and git(source, "cat-file", "-e", rev).returncode == 0


def rev_has_files(source, rev, names):
    return rev_exists(source, rev) and all(
        git(source, "cat-file", "-e", f"{rev}:{name}").returncode == 0 for name in names)


def read_rev(source, rev, name):
    result = git(source, "show", f"{rev}:{name}")
    if result.returncode != 0:
        raise subprocess.CalledProcessError(result.returncode, result.args, result.stdout, result.stderr)
    return result.stdout


def write_fixture(root, source, rev, names):
    for name in names:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(read_rev(source, rev, name))


def commit_all(root, message="fixture"):
    for args in (("add", "-A"), (*GIT_IDENTITY, "commit", "-q", "-m", message)):
        result = git(str(root), *args)
        if result.returncode != 0:
            raise AssertionError(result.stderr.decode())


def init_repo(root):
    result = git(str(root), "init", "-q")
    if result.returncode != 0:
        raise AssertionError(result.stderr.decode())
    commit_all(root)


def state(module, root):
    result = module.integration_state(root)
    return result[0] if isinstance(result, tuple) else result


@unittest.skipUnless(SOURCE, "Set CUA_PATCH_TEST_SOURCE to a Cua checkout")
class ReconciledInstallerTests(unittest.TestCase):
    def available(self, rev):
        return {name: case for name, case in CASES.items() if rev_has_files(SOURCE, rev, case["files"])}

    def make_case(self, case, rev):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        write_fixture(root, SOURCE, rev, case["files"])
        init_repo(root)
        return root

    def invoke(self, case, root, *args):
        script = HERE / "native" / f"{case['script']}.py"
        with patch.object(sys, "argv", [str(script), str(root), *args]), redirect_stdout(StringIO()):
            case["module"].main()

    def snapshot(self, root):
        def relevant(path):
            parts = path.parts
            return ".git" not in parts or any(part.startswith("noches-") for part in parts)
        return {str(path.relative_to(root)): path.read_bytes()
                for path in root.rglob("*") if path.is_file() and relevant(path)}

    def write_stale_record(self, case, root):
        if case["record_kind"] == "files":
            record = {"files": {name: "0" * 64 for name in case["files"]}}
        else:
            record = {"file": case["files"][0], "digest": "0" * 64}
        (root / case["record"]).write_text(json.dumps(record, indent=2) + "\n")
        return root / case["record"]

    def commit_upgrade(self, case, root):
        self.invoke(case, root)
        for name in case["files"]:
            path = root / name
            path.write_text(path.read_text() + "\n")
        commit_all(root, "upstream integration")
        self.assertEqual(state(case["module"], root), "full")

    def test_integrated_check_does_not_write(self):
        cases = self.available(INTEGRATED)
        if not cases:
            self.skipTest("No integrated fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, INTEGRATED)
                before = self.snapshot(root)
                self.invoke(case, root, "--check")
                self.assertEqual(before, self.snapshot(root))
                self.assertFalse((root / case["record"]).exists())

    def test_legacy_check_does_not_write(self):
        cases = self.available(LEGACY)
        if not cases:
            self.skipTest("No legacy fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, LEGACY)
                self.assertEqual(state(case["module"], root), "none")
                before = self.snapshot(root)
                self.invoke(case, root, "--check")
                self.assertEqual(before, self.snapshot(root))

    def test_legacy_install_is_idempotent(self):
        cases = self.available(LEGACY)
        if not cases:
            self.skipTest("No legacy fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, LEGACY)
                self.invoke(case, root)
                first = self.snapshot(root)
                self.assertTrue((root / case["record"]).exists())
                self.assertEqual(state(case["module"], root), "full")
                self.invoke(case, root)
                self.assertEqual(first, self.snapshot(root))

    def test_check_does_not_rewrite_parent_stamps(self):
        cases = {name: case for name, case in self.available(LEGACY).items() if case.get("parents")}
        if not cases:
            self.skipTest("No legacy fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, LEGACY)
                # The install leaves the parent installer's stamp referring to
                # the pre-repair digest; --check must leave it alone.
                self.invoke(case, root)
                record, relative = case["parents"]
                parent = root / record
                parent.write_text(json.dumps({"files": {relative: "0" * 64}}, indent=2) + "\n")
                before = self.snapshot(root)
                self.invoke(case, root, "--check")
                self.assertEqual(before, self.snapshot(root))
                self.assertEqual(parent.read_text(), json.dumps({"files": {relative: "0" * 64}}, indent=2) + "\n")

    def test_stale_record_with_committed_upgrade_is_accepted(self):
        cases = self.available(LEGACY)
        if not cases:
            self.skipTest("No legacy fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, LEGACY)
                self.commit_upgrade(case, root)
                before = self.snapshot(root)
                self.invoke(case, root, "--check")
                self.invoke(case, root)
                self.assertEqual(before, self.snapshot(root))

    def test_uncommitted_edit_after_upgrade_is_rejected(self):
        cases = self.available(LEGACY)
        if not cases:
            self.skipTest("No legacy fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, LEGACY)
                self.commit_upgrade(case, root)
                path = root / case["files"][0]
                path.write_text(path.read_text() + "// local edit after upstream integration\n")
                before = self.snapshot(root)
                with self.assertRaises(SystemExit):
                    self.invoke(case, root, "--check")
                self.assertEqual(before, self.snapshot(root))

    def test_stale_record_is_ignored_for_committed_integration(self):
        cases = self.available(INTEGRATED)
        if not cases:
            self.skipTest("No integrated fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, INTEGRATED)
                stamp = self.write_stale_record(case, root)
                text = stamp.read_text()
                before = self.snapshot(root)
                self.invoke(case, root, "--check")
                self.invoke(case, root)
                self.assertEqual(before, self.snapshot(root))
                self.assertEqual(stamp.read_text(), text)

    def test_integrated_uncommitted_edit_is_rejected(self):
        cases = self.available(INTEGRATED)
        if not cases:
            self.skipTest("No integrated fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, INTEGRATED)
                self.write_stale_record(case, root)
                path = root / case["files"][0]
                path.write_text(path.read_text() + "// local work after install\n")
                before = self.snapshot(root)
                with self.assertRaises(SystemExit):
                    self.invoke(case, root, "--check")
                self.assertEqual(before, self.snapshot(root))

    def test_partial_integration_fails_closed(self):
        cases = self.available(INTEGRATED)
        if not cases:
            self.skipTest("No integrated fixtures in this checkout")
        for name, case in cases.items():
            with self.subTest(installer=name):
                root = self.make_case(case, INTEGRATED)
                relative, old, new = case["partial"]
                path = root / relative
                text = path.read_text()
                self.assertIn(old, text)
                path.write_text(text.replace(old, new, 1))
                before = self.snapshot(root)
                with self.assertRaises(SystemExit):
                    self.invoke(case, root, "--check")
                self.assertEqual(before, self.snapshot(root))


if __name__ == "__main__":
    unittest.main()

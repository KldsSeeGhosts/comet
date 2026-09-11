"""Installer tests for the relogin-safe Hyprland runtime repair.

CUA_PATCH_TEST_SOURCE points at a Cua Git checkout. Legacy fixtures come from
CUA_PATCH_TEST_LEGACY (default: the pinned 4af83697 baseline). The integrated
tests use CUA_PATCH_TEST_INTEGRATED (default: HEAD) and skip when that revision
does not carry the upstream-committed repair. The upgrade class reinstalls on
the legacy baseline, then reformats and commits like upstream, so the
stale-record path stays covered.
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

SCRIPT = Path(__file__).parent / "native/apply_hyprland_runtime.py"
spec = importlib.util.spec_from_file_location("apply_hyprland_runtime", SCRIPT)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
SOURCE = os.environ.get("CUA_PATCH_TEST_SOURCE")
LEGACY_BASELINE = "4af83697b8425944d668c543851ef6ae3639a130"
LEGACY = os.environ.get("CUA_PATCH_TEST_LEGACY") or LEGACY_BASELINE
INTEGRATED = os.environ.get("CUA_PATCH_TEST_INTEGRATED", "HEAD")
FILES = (
    "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland.rs",
    "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_input.rs",
)
RECORD = ".git/noches-cua-hyprland-runtime.json"
GIT_IDENTITY = ("-c", "user.email=installer-tests@example.invalid", "-c", "user.name=Installer Tests",
                "-c", "commit.gpgsign=false")


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


class RuntimeInstallerTestBase(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def invoke(self, *args):
        with patch.object(sys, "argv", [str(SCRIPT), str(self.root), *args]), redirect_stdout(StringIO()):
            installer.main()

    def snapshot(self):
        # Installer records live under .git; other git internals (index,
        # objects) are refreshed by read-only git commands and are not
        # installer state.
        def relevant(path):
            parts = path.parts
            return ".git" not in parts or any(part.startswith("noches-") for part in parts)
        return {str(path.relative_to(self.root)): path.read_bytes()
                for path in self.root.rglob("*") if path.is_file() and relevant(path)}

    def record(self):
        return self.root / RECORD


@unittest.skipUnless(SOURCE and rev_has_files(SOURCE, LEGACY, FILES),
                     "Set CUA_PATCH_TEST_SOURCE to a checkout containing the legacy baseline")
class LegacyRuntimeTests(RuntimeInstallerTestBase):
    def setUp(self):
        super().setUp()
        (self.root / ".git").mkdir()
        write_fixture(self.root, SOURCE, LEGACY, FILES)
        self.assertEqual(installer.integration_state(self.root)[0], "none")

    def test_check_does_not_write(self):
        before = self.snapshot()
        self.invoke("--check")
        self.assertEqual(before, self.snapshot())

    def test_install_is_idempotent_and_preserves_unrelated_changes(self):
        path = self.root / FILES[0]
        path.write_text(path.read_text() + "\n// retained multi-monitor repair\n")
        self.invoke()
        first = self.snapshot()
        self.invoke()
        self.assertEqual(first, self.snapshot())
        self.assertIn("retained multi-monitor repair", path.read_text())
        self.assertIn("active_instance_dir_with_timeout", path.read_text())

    def test_source_drift_refuses_before_writing(self):
        path = self.root / FILES[1]
        path.write_text(path.read_text().replace("fn socket_path(lane: usize)", "fn changed_socket_path(lane: usize)"))
        before = self.snapshot()
        with self.assertRaises(ValueError):
            self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_installed_file_edits_are_not_overwritten(self):
        self.invoke()
        path = self.root / FILES[0]
        path.write_text(path.read_text() + "\n// new work after install\n")
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke()
        self.assertEqual(before, self.snapshot())


@unittest.skipUnless(SOURCE and rev_has_files(SOURCE, INTEGRATED, FILES),
                     "Set CUA_PATCH_TEST_SOURCE or CUA_PATCH_TEST_INTEGRATED to an integrated checkout")
class IntegratedRuntimeTests(RuntimeInstallerTestBase):
    def setUp(self):
        super().setUp()
        write_fixture(self.root, SOURCE, INTEGRATED, FILES)
        init_repo(self.root)
        state, detail = installer.integration_state(self.root)
        self.assertEqual(state, "full", detail)

    def stale_record(self):
        self.record().write_text(json.dumps(
            {"backup": "/nonexistent", "files": {name: "0" * 64 for name in FILES}}, indent=2) + "\n")
        return self.record()

    def test_check_succeeds_without_writing(self):
        before = self.snapshot()
        self.invoke("--check")
        self.assertEqual(before, self.snapshot())

    def test_apply_is_a_noop_and_keeps_no_record(self):
        self.invoke()
        self.assertFalse(self.record().exists())

    def test_stale_record_is_ignored_for_committed_integration(self):
        stamp = self.stale_record()
        text = stamp.read_text()
        before = self.snapshot()
        self.invoke("--check")
        self.invoke()
        self.assertEqual(before, self.snapshot())
        self.assertEqual(stamp.read_text(), text)

    def test_uncommitted_edit_with_stale_record_is_rejected(self):
        self.stale_record()
        path = self.root / FILES[0]
        path.write_text(path.read_text() + "\n// local work after install\n")
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_partial_integration_fails_closed(self):
        path = self.root / FILES[1]
        path.write_text(path.read_text().replace(
            "active_instance_dir_with_timeout(TIMEOUT)", "removed_instance_dir(TIMEOUT)"))
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke("--check")
        self.assertEqual(before, self.snapshot())


@unittest.skipUnless(SOURCE and rev_has_files(SOURCE, LEGACY, FILES),
                     "Set CUA_PATCH_TEST_SOURCE to a checkout containing the legacy baseline")
class RuntimeUpgradeTests(RuntimeInstallerTestBase):
    """Reproduce the upstream upgrade: install, reformat, and commit upstream."""

    def setUp(self):
        super().setUp()
        write_fixture(self.root, SOURCE, LEGACY, FILES)
        init_repo(self.root)
        self.invoke()
        self.assertTrue(self.record().exists())
        for name in FILES:
            path = self.root / name
            path.write_text(path.read_text() + "\n")
        commit_all(self.root, "upstream integration")
        self.assertEqual(installer.integration_state(self.root)[0], "full")

    def test_stale_record_with_committed_integration_is_accepted(self):
        before = self.snapshot()
        self.invoke("--check")
        self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_uncommitted_edit_after_integration_is_rejected(self):
        path = self.root / FILES[0]
        path.write_text(path.read_text() + "// local edit after upstream integration\n")
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke("--check")
        self.assertEqual(before, self.snapshot())


if __name__ == "__main__":
    unittest.main()

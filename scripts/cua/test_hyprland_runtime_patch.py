"""Tests for the relogin-safe Hyprland runtime installer."""
from contextlib import redirect_stdout
from io import StringIO
import importlib.util
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
FILES = (
    "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland.rs",
    "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_input.rs",
)


@unittest.skipUnless(SOURCE, "Set CUA_PATCH_TEST_SOURCE to the pinned Cua checkout")
class RuntimeInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / ".git").mkdir()
        for name in FILES:
            result = subprocess.run(
                ["git", "-C", SOURCE, "show", "HEAD:" + name],
                check=True,
                capture_output=True,
            )
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(result.stdout)

    def invoke(self, *args):
        with patch.object(sys, "argv", [str(SCRIPT), str(self.root), *args]), redirect_stdout(StringIO()):
            installer.main()

    def snapshot(self):
        return {str(path.relative_to(self.root)): path.read_bytes() for path in self.root.rglob("*") if path.is_file()}

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


if __name__ == "__main__":
    unittest.main()

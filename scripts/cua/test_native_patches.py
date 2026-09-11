"""Installer tests against the exact Git baseline, without a compositor or input injection."""
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

SCRIPT = Path(__file__).parent / 'native/apply_cua.py'
spec = importlib.util.spec_from_file_location('apply_cua', SCRIPT)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
SOURCE = os.environ.get('CUA_PATCH_TEST_SOURCE')
FILES = (
    'libs/cua-driver/rust/crates/platform-linux/src/wayland/mod.rs',
    'libs/cua-driver/rust/crates/platform-linux/src/tools/impl_.rs',
    'libs/cua-driver/rust/crates/cua-driver-core/src/action_target.rs',
)
HYPR = 'libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland.rs'


@unittest.skipUnless(SOURCE, 'Set CUA_PATCH_TEST_SOURCE to the pinned Cua checkout')
class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / '.git').mkdir()
        for name in FILES:
            result = subprocess.run(['git', '-C', SOURCE, 'show', 'HEAD:' + name], check=True, capture_output=True)
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(result.stdout)
        (self.root / HYPR).write_text('// existing user multi-monitor patch\n')

    def invoke(self, *args):
        with patch.object(sys, 'argv', [str(SCRIPT), str(self.root), *args]), redirect_stdout(StringIO()):
            installer.main()

    def snapshot(self):
        return {str(p.relative_to(self.root)): p.read_bytes() for p in self.root.rglob('*') if p.is_file()}

    def test_check_does_not_write(self):
        before = self.snapshot()
        self.invoke('--check')
        self.assertEqual(before, self.snapshot())

    def test_existing_hyprland_and_unrelated_edits_are_preserved(self):
        for name in FILES:
            path = self.root / name
            path.write_text(path.read_text() + '\n// unrelated local edit\n')
        self.invoke()
        self.assertEqual((self.root / HYPR).read_text(), '// existing user multi-monitor patch\n')
        for name in FILES:
            self.assertIn('// unrelated local edit', (self.root / name).read_text())

    def test_second_install_is_idempotent(self):
        self.invoke()
        first = self.snapshot()
        self.invoke()
        self.assertEqual(first, self.snapshot())

    def test_installed_file_edits_are_not_overwritten(self):
        self.invoke()
        path = self.root / FILES[0]
        path.write_text(path.read_text() + '\n// new work after install\n')
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_source_drift_refuses_before_writing(self):
        path = self.root / FILES[0]
        path.write_text(path.read_text().replace('struct State {', 'struct DifferentState {'))
        before = self.snapshot()
        with self.assertRaises(ValueError):
            self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_write_failure_restores_original_sources(self):
        before = {name: (self.root / name).read_bytes() for name in FILES}
        write_text = Path.write_text
        def fail_on_core(path, *args, **kwargs):
            if path == self.root / FILES[2]:
                raise OSError('simulated write failure')
            return write_text(path, *args, **kwargs)
        with patch.object(Path, 'write_text', fail_on_core):
            with self.assertRaises(OSError):
                self.invoke()
        for name, data in before.items():
            self.assertEqual((self.root / name).read_bytes(), data)
        self.assertFalse((self.root / '.git/noches-cua-output-patch.json').exists())
        self.assertFalse((self.root / 'libs/cua-driver/rust/crates/platform-linux/src/wayland/noches_display.rs').exists())


if __name__ == '__main__':
    unittest.main()

"""Installer tests against the legacy Git baseline and the integrated checkout.

CUA_PATCH_TEST_SOURCE points at a Cua Git checkout. Legacy fixtures come from
CUA_PATCH_TEST_LEGACY (default: the pinned 4af83697 baseline). The integrated
tests use CUA_PATCH_TEST_INTEGRATED (default: HEAD) and skip when that revision
does not carry the upstream-committed repair. The synthetic upgrade class
reproduces the f82bef47 state from the legacy baseline, so the stale-record
path stays covered even when the upstream checkout is still unintegrated.
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

SCRIPT = Path(__file__).parent / 'native/apply_cua.py'
spec = importlib.util.spec_from_file_location('apply_cua', SCRIPT)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
SOURCE = os.environ.get('CUA_PATCH_TEST_SOURCE')
LEGACY_BASELINE = '4af83697b8425944d668c543851ef6ae3639a130'
LEGACY = os.environ.get('CUA_PATCH_TEST_LEGACY') or LEGACY_BASELINE
INTEGRATED = os.environ.get('CUA_PATCH_TEST_INTEGRATED', 'HEAD')
FILES = (
    'libs/cua-driver/rust/crates/platform-linux/src/wayland/mod.rs',
    'libs/cua-driver/rust/crates/platform-linux/src/tools/impl_.rs',
    'libs/cua-driver/rust/crates/cua-driver-core/src/action_target.rs',
)
CUSTOM = (
    'libs/cua-driver/rust/crates/platform-linux/src/wayland/noches_display.rs',
    'libs/cua-driver/rust/crates/platform-linux/src/tools/noches_desktop.rs',
)
HYPR = 'libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland.rs'
RECORD = '.git/noches-cua-output-patch.json'
GIT_IDENTITY = ('-c', 'user.email=installer-tests@example.invalid', '-c', 'user.name=Installer Tests',
                '-c', 'commit.gpgsign=false')


def git(source, *args):
    return subprocess.run(['git', '-C', source, *args], capture_output=True)


def rev_exists(source, rev):
    return bool(source) and git(source, 'cat-file', '-e', rev).returncode == 0


def read_rev(source, rev, name):
    result = git(source, 'show', f'{rev}:{name}')
    if result.returncode != 0:
        raise subprocess.CalledProcessError(result.returncode, result.args, result.stdout, result.stderr)
    return result.stdout


def write_fixture(root, source, rev, names):
    for name in names:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(read_rev(source, rev, name))


def commit_all(root, message='fixture'):
    for args in (('add', '-A'), (*GIT_IDENTITY, 'commit', '-q', '-m', message)):
        result = git(str(root), *args)
        if result.returncode != 0:
            raise AssertionError(result.stderr.decode())


def init_repo(root):
    result = git(str(root), 'init', '-q')
    if result.returncode != 0:
        raise AssertionError(result.stderr.decode())
    commit_all(root)


def revision_has_integration(source, rev):
    return rev_exists(source, rev) and all(
        git(source, 'cat-file', '-e', f'{rev}:{name}').returncode == 0 for name in CUSTOM)


class InstallerTestBase(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def invoke(self, *args):
        with patch.object(sys, 'argv', [str(SCRIPT), str(self.root), *args]), redirect_stdout(StringIO()):
            installer.main()

    def snapshot(self):
        # Installer records live under .git; other git internals (index, objects)
        # may be refreshed by read-only git commands and are not installer state.
        def relevant(path):
            parts = path.parts
            return '.git' not in parts or any(part.startswith('noches-') for part in parts)
        return {str(p.relative_to(self.root)): p.read_bytes()
                for p in self.root.rglob('*') if p.is_file() and relevant(p)}

    def record(self):
        return self.root / RECORD


@unittest.skipUnless(SOURCE and rev_exists(SOURCE, LEGACY),
                     'Set CUA_PATCH_TEST_SOURCE to a checkout containing the legacy baseline')
class LegacyInstallerTests(InstallerTestBase):
    def setUp(self):
        super().setUp()
        (self.root / '.git').mkdir()
        write_fixture(self.root, SOURCE, LEGACY, FILES)
        (self.root / HYPR).write_text('// existing user multi-monitor patch\n')
        self.assertEqual(installer.integration_state(self.root), ('none', ''))

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

    def test_missing_recorded_file_fails_closed_without_traceback(self):
        # A stale record can name a file the repair no longer contains while
        # the working tree itself is fully integrated; the mismatch must be
        # the fail-closed diagnostic, not a FileNotFoundError.
        self.invoke()
        record = json.loads(self.record().read_text())
        record['files']['libs/cua-driver/rust/crates/platform-linux/src/wayland/removed_after_upgrade.rs'] = '0' * 64
        self.record().write_text(json.dumps(record, indent=2) + '\n')
        with self.assertRaises(SystemExit) as caught:
            self.invoke()
        self.assertIn('changed since install', str(caught.exception))

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
        self.assertFalse(self.record().exists())
        self.assertFalse((self.root / CUSTOM[0]).exists())


@unittest.skipUnless(SOURCE and revision_has_integration(SOURCE, INTEGRATED),
                     'Set CUA_PATCH_TEST_SOURCE or CUA_PATCH_TEST_INTEGRATED to an integrated checkout')
class IntegratedCheckoutTests(InstallerTestBase):
    """The authoritative repair is committed in the checkout under test."""

    def setUp(self):
        super().setUp()
        write_fixture(self.root, SOURCE, INTEGRATED, FILES + CUSTOM)
        init_repo(self.root)
        state, detail = installer.integration_state(self.root)
        self.assertEqual(state, 'full', detail)

    def stale_record(self):
        self.record().write_text(json.dumps(
            {'backup': '/nonexistent', 'files': {name: '0' * 64 for name in FILES + CUSTOM}}, indent=2) + '\n')
        return self.record()

    def test_check_succeeds_without_writing(self):
        before = self.snapshot()
        self.invoke('--check')
        self.assertEqual(before, self.snapshot())

    def test_apply_is_a_noop_and_keeps_no_record(self):
        self.invoke()
        self.assertFalse(self.record().exists())

    def test_stale_record_is_ignored_for_committed_integration(self):
        stamp = self.stale_record()
        text = stamp.read_text()
        before = self.snapshot()
        self.invoke('--check')
        self.invoke()
        self.assertEqual(before, self.snapshot())
        self.assertEqual(stamp.read_text(), text)

    def test_uncommitted_edit_with_stale_record_is_rejected(self):
        self.stale_record()
        path = self.root / FILES[0]
        path.write_text(path.read_text() + '\n// local work after install\n')
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_partial_integration_fails_closed(self):
        path = self.root / FILES[0]
        path.write_text(path.read_text().replace('pub mod noches_display;', '// module wiring removed', 1))
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke('--check')
        self.assertEqual(before, self.snapshot())

    def test_missing_custom_file_fails_closed(self):
        (self.root / CUSTOM[1]).unlink()
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke()
        self.assertEqual(before, self.snapshot())


@unittest.skipUnless(SOURCE and rev_exists(SOURCE, LEGACY),
                     'Set CUA_PATCH_TEST_SOURCE to a checkout containing the legacy baseline')
class CommittedUpgradeTests(InstallerTestBase):
    """Reproduce the f82bef47 upgrade: install, reformat, commit upstream."""

    def setUp(self):
        super().setUp()
        write_fixture(self.root, SOURCE, LEGACY, FILES)
        init_repo(self.root)
        self.invoke()
        self.assertTrue(self.record().exists())
        # Upstream formatting reflows every patched file before committing it.
        for name in FILES:
            path = self.root / name
            path.write_text(path.read_text() + '\n')
        commit_all(self.root, 'upstream integration')
        state, detail = installer.integration_state(self.root)
        self.assertEqual(state, 'full', detail)

    def test_stale_record_with_committed_integration_is_accepted(self):
        before = self.snapshot()
        self.invoke('--check')
        self.invoke()
        self.assertEqual(before, self.snapshot())

    def test_uncommitted_edit_after_integration_is_rejected(self):
        path = self.root / FILES[0]
        path.write_text(path.read_text() + '// local edit after upstream integration\n')
        before = self.snapshot()
        with self.assertRaises(SystemExit):
            self.invoke('--check')
        self.assertEqual(before, self.snapshot())


if __name__ == '__main__':
    unittest.main()

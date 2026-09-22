#!/usr/bin/env python3
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("release", ROOT / "scripts/noches-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_branch_and_ordering(self):
        self.assertEqual(release.build_identity('main', 12, 1), ('stable', '0.3.12'))
        self.assertEqual(release.build_identity('dev', 12, 2), ('dev', '0.3.12-dev.2'))
        with self.assertRaises(ValueError): release.build_identity('feature/test', 12, 1)
        self.assertGreater(release.version_order('0.3.12-dev.10'), release.version_order('0.3.12-dev.9'))

    def test_pr_prepare_uses_explicit_target_branch(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'output'
            env = dict(os.environ, GITHUB_REF_NAME='10/merge', NOCHES_SOURCE_BRANCH='dev',
                       GITHUB_RUN_NUMBER='12', GITHUB_RUN_ATTEMPT='1', GITHUB_OUTPUT=str(output))
            subprocess.run(['python3', str(ROOT / 'scripts/noches-release.py'), 'prepare'], env=env, check=True)
            self.assertEqual(output.read_text(), 'channel=dev\nversion=0.3.12-dev.1\n')

    def test_complete_manifest_uses_immutable_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            with self.assertRaises(ValueError): release.manifest_for(directory, 'owner/noches', 'dev', '0.3.1-dev.1', 'abc')
            for target in ('linux-x86_64.tar.gz', 'linux-aarch64.tar.gz', 'macos-arm64.dmg', 'macos-arm64-app.tar.gz'):
                (directory / f'noches-0.3.1-dev.1-{target}').write_bytes(b'fixture')
            manifest = release.manifest_for(directory, 'owner/noches', 'dev', '0.3.1-dev.1', 'abc')
            for artifact in manifest['files'].values():
                self.assertIn('/download/v0.3.1-dev.1/', artifact['url'])
                self.assertEqual(artifact['size'], 7)
                self.assertEqual(len(artifact['sha256']), 64)

    @unittest.skipUnless(os.uname().sysname == 'Linux', 'Linux installer')
    def test_installer_keeps_channels_and_previous_versions_separate(self):
        with tempfile.TemporaryDirectory(prefix='noches install ') as tmp:
            root = Path(tmp)
            home = root / 'home with spaces'
            home.mkdir()
            env = dict(os.environ, HOME=str(home))
            for slug, channel, version in [('noches', 'stable', '0.3.1'), ('noches-dev', 'dev', '0.3.2-dev.1'), ('noches', 'stable', '0.3.3')]:
                package = root / (slug + version)
                package.mkdir()
                (package / 'zeron').write_text('#!/bin/sh\nprintf "%s" ' + version)
                (package / 'zeron').chmod(0o755)
                (package / 'browser').mkdir()
                (package / 'browser/noches-chromium').write_text(version)
                (package / 'browser/resources.pak').write_bytes(b'chromium resources')
                (package / f'{slug}.desktop').write_text('[Desktop Entry]\nType=Application\nName=Noches\nExec=zeron %u\nTryExec=zeron\n')
                (package / f'{slug}.png').write_bytes(b'icon')
                (package / 'install.json').write_text(json.dumps(dict(slug=slug, channel=channel, version=version)))
                shutil.copy(ROOT / 'scripts/install-linux.sh', package / 'install.sh')
                subprocess.run(['bash', str(package / 'install.sh')], env=env, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.assertEqual(subprocess.check_output([home / '.local/bin/noches'], text=True), '0.3.3')
            self.assertEqual(subprocess.check_output([home / '.local/bin/noches-dev'], text=True), '0.3.2-dev.1')
            self.assertEqual((home / '.local/share/noches/app/previous').resolve().name, '0.3.1')
            for slug, link, version in [('noches', 'current', '0.3.3'), ('noches', 'previous', '0.3.1'), ('noches-dev', 'current', '0.3.2-dev.1')]:
                browser = home / '.local/share' / slug / 'app' / link / 'browser'
                self.assertEqual((browser / 'noches-chromium').read_text(), version)
                self.assertEqual((browser / 'resources.pak').read_bytes(), b'chromium resources')
            desktop = (home / '.local/share/applications/noches.desktop').read_text()
            self.assertIn(f'Exec="{home}/.local/share/noches/app/current/zeron" %u', desktop)


if __name__ == '__main__': unittest.main()

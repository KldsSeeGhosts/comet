#!/usr/bin/env python3
"""Vendor only patched gpui_linux; keep all other ZUI crates at the original Git revision."""
from pathlib import Path
import argparse
import json
import re
import shutil
import tomllib

REV = '07fd941ad72e7edc812fed317aab66adb69fa8cc'
URL = 'https://github.com/zeronsh/zui'


def value(v):
    if isinstance(v, str):
        return json.dumps(v)
    if isinstance(v, bool):
        return str(v).lower()
    if isinstance(v, int):
        return str(v)
    if isinstance(v, list):
        return '[' + ', '.join(value(x) for x in v) + ']'
    if isinstance(v, dict):
        return '{ ' + ', '.join(json.dumps(k) + ' = ' + value(x) for k, x in v.items()) + ' }'
    raise TypeError(type(v))


def make_manifest(zui: Path) -> str:
    root = tomllib.loads((zui / 'Cargo.toml').read_text())
    s = (zui / 'crates/gpui_linux/Cargo.toml').read_text()
    s = s.replace('edition.workspace = true', 'edition = ' + value(root['workspace']['package']['edition']))
    s = s.replace('publish.workspace = true', 'publish = false')
    s = s.replace('[lints]\nworkspace = true\n', '')
    deps = root['workspace']['dependencies']

    def explicit(name, extra=None):
        dep = deps[name]
        dep = {'version': dep} if isinstance(dep, str) else dict(dep)
        if 'path' in dep:
            del dep['path']
            dep['git'], dep['rev'] = URL, REV
        if extra:
            extra = dict(extra)
            del extra['workspace']
            if 'features' in dep and 'features' in extra:
                extra['features'] = sorted(set(dep['features']) | set(extra['features']))
            dep.update(extra)
        return name + ' = ' + value(dep)

    s = re.sub(r'^([\w-]+)\.workspace = true$', lambda m: explicit(m[1]), s, flags=re.M)
    s = re.sub(r'^([\w-]+) = (\{ workspace = true[^\n]*\})$',
               lambda m: explicit(m[1], tomllib.loads('d = ' + m[2])['d']), s, flags=re.M)
    if 'workspace' in s:
        raise ValueError('Unresolved workspace inheritance')
    tomllib.loads(s)
    return s + '\n# Standalone dependency: do not inherit the Noches workspace.\n[workspace]\n'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('zui', type=Path)
    p.add_argument('comet', type=Path)
    args = p.parse_args()
    manifest = make_manifest(args.zui)
    target = args.comet / 'vendor/gpui_linux'
    if target.exists():
        raise SystemExit('vendor/gpui_linux already exists; refusing overwrite')
    native = args.zui / 'crates/gpui_linux'
    if not (native / 'src/linux/wayland/seat_selection.rs').exists():
        raise SystemExit('Apply the reviewed seat patch before vendoring')
    root = args.comet / 'Cargo.toml'
    content = root.read_text()
    patch = '[patch."https://github.com/zeronsh/zui"]'
    if patch in content:
        raise SystemExit('Existing ZUI patch table needs manual reconciliation')
    if 'exclude' in tomllib.loads(content)['workspace']:
        raise SystemExit('Existing workspace exclusions need manual reconciliation')
    content = content.replace('[workspace]\n', '[workspace]\nexclude = ["vendor/gpui_linux"]\n', 1)
    shutil.copytree(native, target)
    (target / 'Cargo.toml').write_text(manifest)
    (target / 'NOCHES-PATCH.md').write_text(
        '# Noches primary-seat repair\n\n'
        'Source: zeronsh/zui at `' + REV + '`.\n'
        'Only gpui_linux is vendored. All sibling crates retain their pinned Git source.\n'
        'The source change is reproducible with scripts/cua/native/zui-primary-seat.patch.\n'
        'LICENSE-APACHE is retained. This is a client input repair, not a compositor plugin replacement.\n')
    root.write_text(content + '\n# Keep Cua synthetic seats out of GPUI physical input.\n' + patch + '\ngpui_linux = { path = "vendor/gpui_linux" }\n')
    print('Vendored patched gpui_linux and installed Cargo patch override')


if __name__ == '__main__':
    main()

#!/usr/bin/env bash
# Included in each desktop tarball. No root or daemon installation required.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readarray -t metadata < <(python3 - "$HERE/install.json" <<'PY'
import json, re, sys
m = json.load(open(sys.argv[1]))
assert m['channel'] in ('stable', 'dev', 'local')
assert re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.]+)?', m['version'])
assert m['slug'] in ('noches', 'noches-dev', 'noches-local')
print(m['version']); print(m['slug']); print(m['channel'])
PY
)
[[ ${#metadata[@]} == 3 ]] || { echo 'Invalid package metadata' >&2; exit 1; }
version="${metadata[0]}"; slug="${metadata[1]}"
root="$HOME/.local/share/$slug/app"
mkdir -p "$root" "$HOME/.local/bin" "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/1024x1024/apps"
stage="$(mktemp -d "$root/.install-XXXXXX")"
trap 'rm -rf "$stage"' EXIT
cp -a "$HERE/." "$stage/"
if [[ -e "$root/$version" ]]; then
  cmp -s "$stage/zeron" "$root/$version/zeron" || { echo 'This version already exists with different contents' >&2; exit 1; }
else
  mv "$stage" "$root/$version"
fi
if [[ -L "$root/current" ]]; then
  ln -s "$(readlink "$root/current")" "$root/.previous-$$"
  mv -Tf "$root/.previous-$$" "$root/previous"
fi
ln -s "$root/$version" "$root/.current-$$"
mv -Tf "$root/.current-$$" "$root/current"
ln -sfn "$root/current/zeron" "$HOME/.local/bin/$slug"
# Desktop launches must not depend on a shell having ~/.local/bin on PATH.
python3 - "$root/current/$slug.desktop" "$HOME/.local/share/applications/$slug.desktop" "$root/current/zeron" <<'PY'
import pathlib, sys
source, dest, exe = sys.argv[1:]
# Desktop-entry quoted arguments require escaping both string and exec syntax.
escaped = exe.replace('\\', '\\\\\\\\').replace('"', '\\\\"').replace('`', '\\\\`').replace('$', '\\\\$').replace('%', '%%')
lines = pathlib.Path(source).read_text().splitlines()
pathlib.Path(dest).write_text('\n'.join('Exec="' + escaped + '" %u' if line.startswith('Exec=') else line for line in lines if not line.startswith('TryExec=')) + '\n')
PY
ln -sfn "$root/current/$slug.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/$slug.png"
if command -v update-desktop-database >/dev/null; then update-desktop-database "$HOME/.local/share/applications" || true; fi
echo "Installed $slug $version. Launch it from your application menu."
echo 'Browser tabs require WebKitGTK 4.1 and JSON-GLib runtime packages.'

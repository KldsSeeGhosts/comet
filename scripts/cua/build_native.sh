#!/usr/bin/env bash
# Run with bash; no root privileges, compositor changes, or global process kills.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CUA="$HOME/AiStack/cua"
INSTALL=0
while (($#)); do
  case "$1" in
    --install) INSTALL=1; shift ;;
    --cua) CUA="${2:?--cua requires a checkout path}"; shift 2 ;;
    --help) printf 'Usage: bash scripts/cua/build_native.sh [--cua PATH] [--install]\n'; exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done
CUA="$(realpath "$CUA")"
[[ "$(git -C "$ROOT" branch --show-current)" == dev ]] || { echo 'Refusing: build Noches dev only from branch dev.' >&2; exit 1; }
python3 - "$ROOT" <<'PY'
from pathlib import Path
import sys, tomllib
root = Path(sys.argv[1])
data = tomllib.loads((root / 'Cargo.toml').read_text())
assert data.get('patch', {}).get('https://github.com/zeronsh/zui', {}).get('gpui_linux', {}).get('path') == 'vendor/gpui_linux', 'The GPUI Cargo override is not installed. Pull the completed dev integration first.'
assert (root / 'vendor/gpui_linux/src/linux/wayland/seat_selection.rs').is_file(), 'Vendored seat repair is missing'
PY
DRIVER_ROOT="$CUA/libs/cua-driver/rust"
DRIVER="$DRIVER_ROOT/target/release/cua-driver"
if ((INSTALL)); then
  python3 - "$HOME/.zeron-dev/env" "$DRIVER" <<'PY'
from pathlib import Path
import os, shlex, sys
file, expected = Path(sys.argv[1]), Path(sys.argv[2]).resolve()
found = None
for line in file.read_text().splitlines():
    key, sep, value = line.partition('=')
    if sep and key.strip() == 'CUA_DRIVER_PATH':
        words = shlex.split(value, comments=True)
        if len(words) == 1:
            found = Path(os.path.expandvars(os.path.expanduser(words[0]))).resolve()
if found != expected:
    raise SystemExit(f'Set CUA_DRIVER_PATH in {file} to {expected} before --install. No service was restarted.')
PY
fi
umask 077
BACKUP_ROOT="${XDG_STATE_HOME:-$HOME/.local/state}/noches-cua-build"
mkdir -p "$BACKUP_ROOT"
BACKUP="$(mktemp -d "$BACKUP_ROOT/build-XXXXXXXX")"
exec > >(tee "$BACKUP/build.log") 2>&1
printf 'Build evidence and old binaries: %s\n' "$BACKUP"
if [[ -f "$DRIVER" ]]; then cp -L --reflink=auto "$DRIVER" "$BACKUP/cua-driver.before"; fi
if [[ -f "$HOME/.local/bin/zeron-dev" ]]; then cp -L --reflink=auto "$HOME/.local/bin/zeron-dev" "$BACKUP/zeron-dev.before"; fi
python3 "$ROOT/scripts/cua/native/apply_cua.py" "$CUA" --check
python3 "$ROOT/scripts/cua/native/apply_cua.py" "$CUA"
(
  cd "$DRIVER_ROOT"
  cargo test --target-dir "$DRIVER_ROOT/target" -p platform-linux --lib noches_display
  cargo test --target-dir "$DRIVER_ROOT/target" -p cua-driver-core --lib action_target
  cargo build --release --target-dir "$DRIVER_ROOT/target" -p cua-driver
)
(
  cd "$ROOT"
  cargo build --locked --release --features dev --target-dir "$ROOT/target"
  cargo tree --locked --features dev -i gpui_linux
)
sha256sum "$DRIVER" "$ROOT/target/release/zeron"
if ((INSTALL)); then
  cd "$ROOT"
  ./install.sh
  echo 'Restarting zeron-dev.service; active Noches dev turns will end.'
  systemctl --user restart zeron-dev.service
  systemctl --user is-active --quiet zeron-dev.service
  PID="$(systemctl --user show -p MainPID --value zeron-dev.service)"
  [[ "$PID" =~ ^[1-9][0-9]*$ ]] || { echo 'No live dev service PID' >&2; exit 1; }
  cmp "$ROOT/target/release/zeron" "$HOME/.local/bin/zeron-dev"
  cmp "$ROOT/target/release/zeron" "/proc/$PID/exe"
  sha256sum "$ROOT/target/release/zeron" "$HOME/.local/bin/zeron-dev" "/proc/$PID/exe"
  echo 'Dev binary installation verified. Test physical input and desktop targeting on both outputs.'
else
  echo 'Both builds completed. No service or compositor was restarted. Use --install to install and restart Noches dev.'
fi

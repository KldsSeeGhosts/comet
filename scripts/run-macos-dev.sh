#!/usr/bin/env bash
# Build and run an isolated macOS development bundle. Noches.app may remain
# open: this bundle has a separate LaunchServices/TCC identity, data directory,
# and engine IPC port.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Cargo can be missing from PATH (GUI-launched shells, rustup installed
# without shims). Resolve it through rustup before giving up.
RUSTUP_BIN=""
if ! command -v cargo >/dev/null 2>&1; then
  RUSTUP_BIN="$(command -v rustup 2>/dev/null || true)"
  if [[ -z "$RUSTUP_BIN" ]]; then
    for candidate in "${CARGO_HOME:-$HOME/.cargo}/bin/rustup" \
      "$HOME/.cargo/bin/rustup" /opt/homebrew/bin/rustup /usr/local/bin/rustup; do
      if [[ -x "$candidate" ]]; then
        RUSTUP_BIN="$candidate"
        break
      fi
    done
  fi
  if [[ -n "$RUSTUP_BIN" ]]; then
    # `rustup which` honors rust-toolchain.toml when run from the repo.
    CARGO_PATH="$(cd "$ROOT" && "$RUSTUP_BIN" which cargo 2>/dev/null || true)"
    if [[ -n "$CARGO_PATH" ]]; then
      PATH="$(dirname "$CARGO_PATH"):$PATH"
      export PATH
    fi
  fi
fi
if ! command -v cargo >/dev/null 2>&1; then
  if [[ -n "$RUSTUP_BIN" ]]; then
    echo "cargo not found: rustup at $RUSTUP_BIN has no usable toolchain; run 'rustup toolchain install stable'" >&2
  else
    echo "cargo not found: install Rust via rustup or add cargo to PATH" >&2
  fi
  exit 1
fi
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
DEV_ROOT="$ROOT/target/macos-dev"
APP="$DEV_ROOT/Noches Dev.app"
CONTENTS="$APP/Contents"
DATA_DIR="${ZERON_DEV_DATA_DIR:-$DEV_ROOT/data}"
IPC_PORT="${ZERON_DEV_IPC_PORT:-49777}"

if pgrep -f -x "$CONTENTS/MacOS/zeron" >/dev/null 2>&1; then
  echo "Noches Dev is already running. Quit it before rebuilding the signed bundle." >&2
  exit 1
fi

cd "$ROOT"
cargo build --locked -p zeron

mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$DATA_DIR"
install -m 755 "$ROOT/target/debug/zeron" "$CONTENTS/MacOS/zeron"
sed "s/__VERSION__/$VERSION/g" "$ROOT/dist/macos/Info-dev.plist" >"$CONTENTS/Info.plist"
plutil -replace LSEnvironment.ZERON_DATA_DIR -string "$DATA_DIR" "$CONTENTS/Info.plist"
plutil -replace LSEnvironment.ZERON_IPC_PORT -string "$IPC_PORT" "$CONTENTS/Info.plist"

if [[ ! -f "$CONTENTS/Resources/noches.icns" ]]; then
  ICONSET="$DEV_ROOT/noches-dev.iconset"
  mkdir -p "$ICONSET"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ROOT/dist/macos/icon-1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    retina=$((size * 2))
    sips -z "$retina" "$retina" "$ROOT/dist/macos/icon-1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/noches.icns"
fi

# A real Apple Development identity gives TCC a stable signing requirement
# across rebuilds. Set ZERON_DEV_CODESIGN_IDENTITY explicitly when more than
# one identity is installed; otherwise fall back to an ad-hoc signature.
IDENTITY="${ZERON_DEV_CODESIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" ]]; then
  IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)"
fi
if [[ -n "$IDENTITY" ]]; then
  codesign --force --sign "$IDENTITY" --identifier app.noches.desktop.dev "$APP"
else
  codesign --force --sign - --identifier app.noches.desktop.dev "$APP"
  echo "warning: no Apple Development signing identity found; macOS may ask for permissions again after a rebuild" >&2
fi

echo "running Noches Dev (bundle app.noches.desktop.dev, data $DATA_DIR, IPC $IPC_PORT)" >&2
# LaunchServices must own the process. Launching Contents/MacOS/zeron directly
# makes TCC attribute Screen Recording to the terminal (Warp, Terminal, etc.).
# -W keeps the script attached until the app exits. Runtime logs remain in the
# isolated data directory (`target/macos-dev/data/logs/zeron-headed.log`).
OPEN_ENV=(
  --env "ZERON_DATA_DIR=$DATA_DIR"
  --env "ZERON_IPC_PORT=$IPC_PORT"
)
if [[ -n "${ZERON_OPEN_ROUTE:-}" ]]; then
  OPEN_ENV+=(--env "ZERON_OPEN_ROUTE=$ZERON_OPEN_ROUTE")
fi
exec open -W "${OPEN_ENV[@]}" "$APP" --args "$@"

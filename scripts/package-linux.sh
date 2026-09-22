#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/noches-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
source "$ROOT/scripts/release-config.sh"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/noches-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --locked --release -p zeron
  BIN="$ROOT/target/release/zeron"
else
  cargo build --locked -p zeron
  BIN="$ROOT/target/debug/zeron"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/zeron"
sed -e "s/^Name=.*/Name=$APP_NAME/" -e "s/^Exec=.*/Exec=$APP_SLUG %u/" \
  -e "s/^TryExec=.*/TryExec=$APP_SLUG/" -e "s/^Icon=.*/Icon=$APP_SLUG/" \
  -e "s|^MimeType=.*|MimeType=x-scheme-handler/$APP_SLUG;|" \
  -e "s/^StartupWMClass=.*/StartupWMClass=$APP_SLUG/" \
  "$ROOT/dist/zeron.desktop" > "$STAGE/$APP_SLUG.desktop"
install -m 644 "$ROOT/dist/zeron.png" "$STAGE/$APP_SLUG.png"
mkdir -p "$STAGE/licenses/fonts"
cp "$ROOT/crates/ui/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"

install -m 755 "$ROOT/scripts/install-linux.sh" "$STAGE/install.sh"
python3 - "$STAGE/install.json" <<'META'
import json, os, sys
with open(sys.argv[1], 'w') as f:
    json.dump(dict(version=os.environ['NOCHES_VERSION'], channel=os.environ['NOCHES_CHANNEL'], slug=os.environ['APP_SLUG']), f)
META
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"

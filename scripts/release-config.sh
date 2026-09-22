#!/usr/bin/env bash
# Source from packaging scripts. Channel is explicit, never inferred from -O.
export NOCHES_CHANNEL="${NOCHES_CHANNEL:-local}"
export NOCHES_VERSION="${NOCHES_VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}"
export NOCHES_REPOSITORY="${NOCHES_REPOSITORY:-KldsSeeGhosts/noches}"
export NOCHES_COMMIT="${NOCHES_COMMIT:-$(git -C "$ROOT" rev-parse HEAD)}"
case "$NOCHES_CHANNEL" in
  stable) APP_NAME=Noches; APP_SLUG=noches; BUNDLE_ID=io.github.kldsseeghosts.noches ;;
  dev) APP_NAME='Noches Dev'; APP_SLUG=noches-dev; BUNDLE_ID=io.github.kldsseeghosts.noches.dev ;;
  local) APP_NAME=Noches; APP_SLUG=noches-local; BUNDLE_ID=io.github.kldsseeghosts.noches.local ;;
  *) echo 'NOCHES_CHANNEL must be stable, dev, or local' >&2; exit 1 ;;
esac
VERSION="$NOCHES_VERSION"
export APP_NAME APP_SLUG BUNDLE_ID
python3 - <<'PY'
import os, re
channel, version = os.environ['NOCHES_CHANNEL'], os.environ['NOCHES_VERSION']
pattern = r'\d+\.\d+\.\d+'
if channel == 'dev': pattern += r'-dev\.\d+'
elif channel == 'local': pattern += r'(?:-[a-zA-Z0-9.]+)?'
if not re.fullmatch(pattern, version):
    raise SystemExit('NOCHES_VERSION must be a safe SemVer matching NOCHES_CHANNEL')
PY

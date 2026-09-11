#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")"/../../.. && pwd)"
ZUI="$(realpath "${1:?ZUI checkout required}")"
CUA="$(realpath "${2:?Cua checkout required}")"
mkdir -p "$ROOT/native-test-results"
exec > >(tee "$ROOT/native-test-results/verify.log") 2>&1
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype-dev libvulkan-dev libasound2-dev libssl-dev libdbus-1-dev libx11-dev libxi-dev libxtst-dev libxcb1-dev libclang-dev pkg-config clang cmake ninja-build
rustup toolchain install 1.97.1 --profile minimal --component rustfmt
export RUSTUP_TOOLCHAIN=1.97.1
export CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_OPT_LEVEL=0
export CARGO_PROFILE_TEST_DEBUG=0
git -C "$ZUI" apply --check "$ROOT/scripts/cua/native/zui-primary-seat.patch"
git -C "$ZUI" apply "$ROOT/scripts/cua/native/zui-primary-seat.patch"
rustc --edition=2024 --test "$ZUI/crates/gpui_linux/src/linux/wayland/seat_selection.rs" -o "$ROOT/native-test-results/seat-tests"
"$ROOT/native-test-results/seat-tests"
(cd "$ZUI" && cargo check -p gpui_linux --no-default-features --features wayland)
if test -f "$ROOT/scripts/cua/native/cua-displays.patch"; then
  git -C "$CUA" apply --check "$ROOT/scripts/cua/native/cua-displays.patch"
  git -C "$CUA" apply "$ROOT/scripts/cua/native/cua-displays.patch"
  (cd "$CUA/libs/cua-driver/rust" && cargo test -p platform-linux --lib noches_display && cargo test -p cua-driver-core --lib action_target)
fi

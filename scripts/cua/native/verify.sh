#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")"/../../.. && pwd)"
ZUI="$(realpath "${1:?ZUI checkout required}")"
CUA="$(realpath "${2:?Cua checkout required}")"
mkdir -p "$ROOT/native-test-results"
exec > >(tee "$ROOT/native-test-results/verify.log") 2>&1
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype-dev libvulkan-dev libasound2-dev libssl-dev libdbus-1-dev libx11-dev libxi-dev libxtst-dev libxcb1-dev libclang-dev libwebkit2gtk-4.1-dev libjson-glib-dev pkg-config clang cmake ninja-build
rustup toolchain install 1.97.1 --profile minimal --component rustfmt
export RUSTUP_TOOLCHAIN=1.97.1
export CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_OPT_LEVEL=0
export CARGO_PROFILE_TEST_DEBUG=0
# Installer regression suites run against this checkout. The legacy revision is
# the pinned baseline the anchor installers were written for; the integrated
# revision is the upstream commit that already carries the repairs. Suites skip
# the revision a checkout does not contain.
export CUA_PATCH_TEST_SOURCE="$CUA"
export CUA_PATCH_TEST_LEGACY=4af83697b8425944d668c543851ef6ae3639a130
export CUA_PATCH_TEST_INTEGRATED=f82bef47563b35ab8986560a5c3fd531f8f32de7
python3 -m unittest discover -s "$ROOT/scripts/cua" -p 'test_native_patches.py' -v
python3 -m unittest discover -s "$ROOT/scripts/cua" -p 'test_hyprland_runtime_patch.py' -v
python3 -m unittest discover -s "$ROOT/scripts/cua" -p 'test_native_installers.py' -v
python3 "$ROOT/scripts/cua/native/apply_cua.py" "$CUA" --check
python3 "$ROOT/scripts/cua/native/apply_cua.py" "$CUA"
python3 "$ROOT/scripts/cua/native/apply_cua.py" "$CUA"
git -C "$CUA" diff --check
(cd "$CUA/libs/cua-driver/rust" && cargo test -p platform-linux --lib noches_display && cargo test -p cua-driver-core --lib action_target)
git -C "$ZUI" apply --check "$ROOT/scripts/cua/native/zui-primary-seat.patch"
git -C "$ZUI" apply "$ROOT/scripts/cua/native/zui-primary-seat.patch"
rustc --edition=2024 --test "$ZUI/crates/gpui_linux/src/linux/wayland/seat_selection.rs" -o "$ROOT/native-test-results/seat-tests"
"$ROOT/native-test-results/seat-tests"
(cd "$ZUI" && cargo check -p gpui_linux --no-default-features --features wayland)

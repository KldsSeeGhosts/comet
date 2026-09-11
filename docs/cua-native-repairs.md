# Noches native input repairs

These are source changes, not the earlier diagnostic-only commits. The initial
source baselines are ZUI `07fd941ad72e7edc812fed317aab66adb69fa8cc` and Cua
`4af83697b8425944d668c543851ef6ae3639a130`, driver 0.27.0. Cua `f82bef47`
integrates the driver changes below as tracked source.

## What changes

The ZUI patch replaces last-advertised-seat selection with registry-identity
and name tracking. Unknown seats cannot acquire input children until their name
arrives. `Cua-Agent` and `Cua-Test-Agent`, including suffixed lanes, cannot
replace the selected ordinary seat. Ordinary-seat selection is sticky; removal
selects an eligible replacement. Capability updates are scoped to their seat,
repeated updates do not recreate devices, and stale pointer, keyboard, and
pinch events are ignored. A real seat change also rebuilds seat-bound clipboard,
IME, gesture, and cursor objects.

Only `gpui_linux` is vendored. The Cargo override is:

```toml
[patch."https://github.com/zeronsh/zui"]
gpui_linux = { path = "vendor/gpui_linux" }
```

The vendor generator resolves the original workspace-inherited dependencies
without changing their versions or features. Sibling ZUI crates keep the original
Git revision. CI checks that there is exactly one `gpui` and one `gpui_linux`,
that `gpui_linux` comes from the vendor directory, and that the Noches dev
configuration compiles. The native patch is retained for review/reproduction.

Cua changes live in the separate driver source tree. `apply_cua.py` installs
real Rust implementations and connects them to the existing tool handlers:

- `platform-linux/src/wayland/noches_display.rs`: per-output discovery,
  output-local native pixel geometry, selected-output capture, and v2 virtual
  pointers explicitly bound to an output and ordinary seat.
- `platform-linux/src/tools/noches_desktop.rs`: named-display observations and
  pointer actions through the normal authorized tool dispatch path.
- `wayland/mod.rs`: selected-output screencopy and a fix for another output's
  mode event overwriting the legacy selected output's dimensions.
- `wayland/hyprland.rs` and `wayland/hyprland_input.rs`: resolve the active
  Hyprland instance by matching its IPC peer to `WAYLAND_DISPLAY`. Noches can
  keep running across a compositor relogin without using an obsolete inherited
  `HYPRLAND_INSTANCE_SIGNATURE`.
- `wayland/hyprland_compatibility.rs`: admit the installed
  `zen-browser-bin 1.22b-1` package to exact-window independent-seat input.
- `platform-linux/src/tools/impl_.rs` and `wayland/hyprland_input.rs`: route
  `type_text` through the same background transaction as hotkeys and key
  presses, with bounded all-ASCII prevalidation and partial-delivery records.
- `cua-driver-core/src/action_target.rs`: preserve a named Linux desktop target
  rather than rejecting it or dropping its identity.

The driver installer checks every expected source anchor before writing,
backs up originals under the Cua checkout's `.git`, and verifies hashes on a
second invocation. A checkout that already contains the integrated repair is
recognized by its semantic markers and custom files, so `--check` and apply
become no-ops there; partial integration fails closed. When an install record
exists but its hashes no longer match, the mismatch is accepted as upstream
integration only if every recorded file is tracked and clean at git HEAD, which
identifies a checkout advanced to the integrated commit; an uncommitted edit
made after installation is refused. Otherwise the installer refuses source
drift rather than resetting the repository or silently overwriting newer work.
Unrelated local edits outside those anchors survive. The runtime repair edits
separate anchors in `wayland/hyprland.rs`, so the existing screen-size patch
remains.

## Build and install on the desktop

The development checkout must be on `dev`. Pull without discarding local edits:

```bash
cd ~/AiStack/comet
git pull --ff-only origin dev
bash scripts/cua/build_native.sh --install
```

The helper requires the completed vendor integration in Cargo.toml. It validates
that `~/.zeron-dev/env` already points `CUA_DRIVER_PATH` at the requested Cua
checkout's release binary. It does not rewrite the environment file, change
Hyprland configuration, unload the plugin, delete its runtime marker, kill
unrelated driver processes, or change ptrace policy.

The helper applies the Cua patch, runs its new Rust regressions, builds the
driver, builds Noches with `--release --features dev`, runs `install.sh`, and
restarts only `zeron-dev.service`. Active Noches dev turns end at that restart.
It compares the build, installed launcher, and running process executable.
Build logs and copies of the previous binaries remain in a private directory
under `~/.local/state/noches-cua-build`.

Omit `--install` to build without restarting the service. The new driver binary
will still be used by subsequently started driver sessions because the existing
CUA_DRIVER_PATH points at that binary. No root command is needed for these
source and user-service changes.

The earlier debugging sysctl is a separate, still-required host cleanup:

```bash
pkexec sysctl kernel.yama.ptrace_scope=1
```

## Desktop coordinate contract

First call `get_screen_size`. On the patched native Hyprland backend its
`displays` array identifies available outputs, and its observation includes a
real `display_id`, native dimensions, logical origin/size, and a `layout_token`.
`primary` remains a compatibility alias for the output containing logical
origin, or the first complete output when no output contains origin. It does
not mean the focused monitor. Prefer the returned output name.

An agent should use this sequence through `noches_cua`:

```json
{"action":"get_desktop_state","args":{"display_id":"DP-1"}}
```

Then use coordinates measured from that PNG and the returned layout token:

```json
{"action":"click","args":{"target":{"kind":"desktop","display_id":"DP-1"},"x":1920,"y":1080,"expected_layout":"COPY_THE_RETURNED_LAYOUT_TOKEN"}}
```

Repeat the observation for DP-2 and target DP-2 explicitly. Do not add monitor
origins to PNG coordinates. The driver binds a compositor virtual pointer to
the selected output. Hyprland routes it through the ordinary seat, so it moves
the user's visible cursor and can change pointer focus. Noches refuses this
route unless the user explicitly allows disruption. For the stated layout, DP-1's native center maps to logical
`3584,720`; DP-2's center maps to `1152,648` when its compositor-reported logical
size is `2304x1296`. The implementation uses xdg-output logical sizes, not a
rounded integer scale or a combined framebuffer bounding box.

`(0,0)` is a literal corner, not a center sentinel. Invalid coordinates are
rejected, not clamped. `expected_layout` rejects a changed selected-output
layout. Each action also rechecks the live output identity and geometry before
input; no fallback silently retargets another monitor.

Named-display pointer operations cover move, click, right/double click through
click's button/count, line scrolling, and a left-button drag within one output.
Rotated outputs, cross-output drags, modified desktop clicks, and non-line
scroll units are explicitly outside this initial repair. Use exact window
targets for keyboard operations and existing window-local operations. Prefer
browser tools for web tasks because CDP actions do not move the physical cursor
or foreground the browser. Zen uses the exact-window Hyprland route because it
does not expose Chromium DevTools. A background Zen window may share its process
with the active Zen window. The plugin compares exact surfaces rather than
rejecting every top-level owned by that process.

## Verification and remaining host checks

The `Cua native fixes` workflow compiles the actual patched Linux platform code,
runs the display and target-normalization tests, compiles the GPUI Wayland
backend, runs eight seat-selection regressions, and checks Noches with the
vendored dependency. It retains logs as `native-test-results`. The installer
suites (run locally by `build_native.sh`, in the `Cua installer tests`
workflow against both the pinned legacy baseline and the integrated checkout,
and in the native verify path) cover preflight, preservation, idempotence,
source drift, write rollback, read-only `--check`, integrated-source detection,
partial-integration refusal, missing-record fail-closed diagnostics, and the
stale-record upgrade from an older baseline.

These checks are not a live reproduction of the reported full-desktop freeze.
The remote desktop connection was offline during implementation. Installation,
physical pointer delivery, and overlay visibility still need host verification.
No plugin lifecycle change is included: upstream already releases grants on
disconnect, while its seat-lifetime marker intentionally survives to compositor
exit. Do not delete that marker to try to recover input.

Use a disposable test window on each output. Verify an observed desktop click,
move, scroll, and same-output drag. Confirm that both the Noches window and the
Caelestia launcher still accept physical input after normal completion and
cancellation. Also verify driver-disconnect cleanup using only the identified
Noches-owned process, not a blanket kill of all cua-driver processes. The older
`scripts/cua/capture.py` remains useful for recording plugin authority and
process ownership during this test.

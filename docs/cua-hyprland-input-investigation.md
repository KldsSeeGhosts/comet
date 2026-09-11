# Noches / Hyprland input investigation

Date: 2026-09-11

## Status and limits

This is a source investigation plus a tested read-only capture tool. It is not a deployed fix or proof of the reported full-desktop freeze.

The inspected Noches baseline is `5796fccd5635900f81c27f485877a6b9c32a873d` on `dev`. Its `Cargo.toml` pins `zeronsh/zui` to `07fd941ad72e7edc812fed317aab66adb69fa8cc`. The upstream Cua source inspected reports driver version 0.27.0. The user's locally modified Cua checkout and installed plugin have NOT been read through a host terminal, so their exact relationship to upstream remains unverified.

Remote Desktop Commander was confirmed installed during the investigation, but this chat's action discovery did not expose its terminal or filesystem actions. No live Noches turn, compositor trace, plugin replacement, native build, service restart, or sysctl change was performed. Do not label the machine repaired on the strength of this report.

## Finding 1: GPUI can replace physical input with an agent seat

Confirmed in the pinned ZUI source:

`crates/gpui_linux/src/linux/wayland/client.rs`

- `WaylandClient::new()` binds every advertised `wl_seat` and overwrites one `seat` variable. The last advertised seat wins, without examining the seat name.
- The runtime `Dispatch<wl_registry::WlRegistry, GlobalListContents>` handler handles a new `wl_seat` by releasing `state.wl_pointer`, `state.wl_keyboard`, and `state.wl_seat`, then installing the new seat. It does not check whether it is a Cua synthetic seat.
- `Dispatch<wl_seat::WlSeat, ()>` processes capability events from any bound seat. It creates new keyboard/pointer objects and replaces the single stored objects without checking `seat == state.wl_seat`.
- Pointer and keyboard event handlers share application-wide focus/state rather than segregating it by seat.

Cua's `libs/cua-driver/hyprland-plugin/src/input_experiment.cpp` publishes two independent `wl_seat` globals named `Cua-Agent` and `Cua-Agent-2`. Their continued existence does not imply that a driver holds active input authority.

Consequently, a Noches GPUI process can discard its physical input objects when these globals appear, or select an agent seat when it starts after the plugin. That mechanism is consistent with a healthy event loop waiting in epoll while physical input stops reaching the application. This is a concrete source defect, not a confirmed explanation of the entire reported freeze. Quickshell is a separate client and requires its own event-delivery evidence.

The driver already contains a useful related implementation in `libs/cua-driver/rust/crates/platform-linux/src/wayland/primary_seat.rs`: it excludes named Cua seats when choosing the ordinary seat. That does not fix GPUI, which is a different Wayland client. Nor should its helper be copied without accounting for unresolved names and hotplug ordering.

### Required GPUI fix

Track advertised seats by registry identity and their names before choosing the ordinary seat. Keep the selected ordinary seat when an unrelated seat appears. Do not bind pointer/keyboard, text-input, clipboard, cursor-shape, or gesture objects to an unclassified synthetic seat. Ignore capability and child-object events that do not belong to the selected ordinary input path.

For this application, physical foreground input must not be replaced by `Cua-Agent`, `Cua-Agent-2`, or the corresponding test seats. Supporting background synthetic input into Noches itself would require a separate per-seat implementation; it must not reuse the single physical focus state.

Do not replace the current bug with a blind first-seat rule. Initial enumeration order is not an identity contract. Account for seat-name events arriving after registry events, seats present before startup, runtime additions, selected-seat removal, and repeated capability announcements. An announcement that preserves a capability should not gratuitously recreate the corresponding input object.

Carry the eventual change in a pinned ZUI fork or properly integrated dependency override. Do not edit a shared Cargo cache checkout. Do not bump to an unrelated upstream revision and attribute all resulting changes to this repair.

## Finding 2: the seat-lifetime marker is intentional

`libs/cua-driver/hyprland-plugin/src/seat_lifetime.hpp` explicitly keeps `cua-input-seat-lifetime` after successful publication. It prevents replacement modules from publishing additional seats before the compositor exits. Removing this directory is not a session cleanup operation.

The inspected `input_experiment.cpp` already handles transport death:

- `client_ready()` detects hangup, error, EOF, and expired connections. If the dead client holds the lease, it calls `revoke("disconnected", true)`; it also clears that client's reservation.
- The periodic path repeats dead-lease cleanup and erases dead client entries.
- `revoke()` stops an in-flight drag, unwinds foreground state, releases owned pointer-button state, clears keyboard state, and retires the grant.
- The `true` argument intentionally retains passive hover. Normal completion likewise allows passive pointer presence without an active grant.
- Primary focus changes can retire conflicting passive hover.
- Seat globals and client-owned protocol resources have compositor-lifetime rules. The retirement comments warn that destroying resources immediately can disconnect applications that still send protocol requests.

Therefore, neither remaining socket pathnames, the marker directory, `seat_resources > 0`, nor `pointer_focus: true` alone proves a leaked session. Socket existence is not proof of a live peer. Do not blindly destroy seats or unlink the marker on driver disconnect.

A lifecycle bug remains possible, especially in the locally installed build. Diagnose it with live `cua:status` samples and process ownership. Persistent `lease_active`, `held_button`, `held_keys`, or `drag_active` after the owning driver has gone would be materially different evidence from passive hover. A fish-parented independent driver must not be mistaken for Noches' managed daemon.

## Read-only capture and clean reproduction

The helper is `scripts/cua/capture.py`. It requires Python 3.10 or newer, uses the standard library, and runs existing read-only commands. It does not inject input, stop processes, delete sockets, read environment variables, take screenshots, or modify sysctls.

After bringing the local checkout up to the relevant `dev` commits without overwriting uncommitted work:

```bash
cd ~/AiStack/comet
python3 -m unittest discover -s scripts/cua -p 'test_*.py' -v
python3 scripts/cua/capture.py --watch 45 --interval 0.5 --wayland-info
```

`wayland-info` is optional. Omit the flag when unavailable. Its output enumerates seats for that observer; it does not prove which seat Noches or Quickshell selected.

The script writes a new mode-0700 directory under `~/.local/state/noches-cua-debug`. JSON files are mode 0600. It records plugin lane status, relevant executable/parent identities, driver serve/mcp mode, monitor/device/layer metadata, service PID, repository HEAD/worktree summaries, and the current ptrace setting. It avoids arbitrary command lines and chat logs. Review output before sharing; device names, paths, and layer metadata can still be sensitive.

The watch period limits the sampling phase, not total runtime including metadata commands. Individual commands have timeouts. Stored stdout/stderr excerpts are capped at 2 MiB each; `subprocess.run` still buffers the command's complete output before excerpting. Do not treat that excerpt cap as a hard subprocess memory limit.

Start with normal turn completion. During capture, run one approved Noches computer-use turn against an expendable test window, establish the agent cursor, then end the turn. Check physical pointer movement, clicking, and typing in both Noches and the Quickshell launcher before, during, and after. Do not click arbitrary real application controls as a test.

Record the exact point of failure and whether it preceded turn cleanup. Only after normal completion is understood should cancellation and abrupt process death be tested, with a working SSH/TTY recovery path. Terminate only the identified test-owned process, never all `cua-driver` processes. Do not disable approvals to make reproduction easier.

If status shows no active authority after the turn but GPUI is unresponsive, obtain client-specific seat-binding evidence. A controlled `WAYLAND_DEBUG=client` run can reveal registry binds, `wl_seat.name`, child creation/release, and input events. Use a disposable instance with no secrets or normal typing; protocol logs can expose sensitive activity. Do not enable verbose protocol logging globally on the user's working session.

A plugin-disabled A/B test requires a backed-up Lua configuration with the plugin load disabled and a full Hyprland session restart, after saving work. Do not assume a config reload unloads published seats. Do not restart the desktop automatically from a diagnostic script.

## Finding 3: output identity is lost below the desktop tools

Confirmed in upstream `libs/cua-driver/rust/crates/platform-linux/src/wayland/mod.rs`:

- `State` stores one `output: Option<WlOutput>`.
- The registry callback binds output objects but retains only the first in that field.
- The output event handler ignores the emitting `WlOutput` identity and overwrites shared `output_w` / `output_h` for every `Mode` event. It also does not restrict the event to the current mode.

This means the screenshot output and dimensions can belong to different outputs or modes. Two equally sized 4K modes conceal part of this bug; their logical coordinate spaces still differ under fractional scaling. Returning a monitor from `screen_size_from_monitors` only removes the initial refusal. It does not establish one identity across capture, input, and cursor placement.

The public desktop contract also describes an exact `display_id`, currently `primary`. A per-output repair must reach the runtime input schema, core target admission, capture, input routing, and returned metadata. Changing only the registry map will not provide a complete tool-level feature.

### Per-output implementation contract

Maintain one output record per live Wayland registry global. Join the `wl_output.name` connector identity to authoritative logical geometry from xdg-output and/or compositor metadata. Preserve current mode, transform, actual capture dimensions, and output-local logical bounds separately. Track completion and removal; invalidate stale observations on layout/scale/transform changes.

Expose a resolved display identity consistently in inspection results and desktop action targets. Connector names such as DP-1 and DP-2 identify outputs within the current session, but a connector name alone must not keep a stale screenshot valid across unplug/replug or mode changes. Resolve any `primary` compatibility alias deterministically, return the concrete target, and do not silently switch it when focus changes between observation and action.

For unrotated images, the conversion is:

```text
local_logical_x = screenshot_x * logical_width / captured_pixel_width
local_logical_y = screenshot_y * logical_height / captured_pixel_height
layout_x = logical_origin_x + local_logical_x
layout_y = logical_origin_y + local_logical_y
```

Use dimensions returned for that exact capture. Handle transform and y-inversion explicitly before using these equations. Do not divide the whole desktop by one monitor's scale, infer logical bounds from the rounded display string `1.667`, or manufacture a native-pixel bounding box for mixed-scale outputs.

With the user-reported unrotated DP-1 geometry, a native 3840x2160 capture point `(1920,1080)` corresponds to output-local logical `(1280,720)`, then layout `(3584,720)` after adding the DP-1 origin `(2304,0)`. This is a derived example, not a measured click result. Read DP-2's exact logical dimensions rather than guessing them from its rounded scale.

Bind capture to the selected output. For virtual-pointer delivery, prefer an output-bound pointer with coordinates normalized against that same output, and verify the compositor honors its output association. Preserve the selected output through click, move, drag, scroll positioning, and button-hold sequences. Reject removed or stale targets instead of falling back to the first output. Preserve the working window-local input path.

## The overlay already has per-output support

The inspected `libs/cua-driver/rust/crates/platform-linux/src/wayland/overlay.rs` creates a layer surface for each enabled output and tracks logical origins and sizes. It already distinguishes painted outputs and configured surfaces. Do not assume the entire overlay renderer is first-output-only just because desktop capture/input is.

Trace the cursor command's coordinate space through the daemon to `NativeOutput::layout` and rendering. Verify that a window-local point receives its window origin exactly once, and an output-local desktop point receives its output origin exactly once. A logically visible cursor state does not prove a committed, visible overlay buffer.

Noches' inspected `Driver::spawn` already starts a private `serve` daemon and an MCP proxy for the real driver. That daemon owns the overlay runloop. Verify the running binaries and their stderr logs before adding another overlay owner or external driver instance.

## Acceptance gates before calling this fixed

1. Physical pointer and keyboard continue working in Noches and Quickshell during and after normal turns, cancellation, and identified-driver death. Plugin authority clears when its owner disappears. Client seat identity remains correct with plugin seats present before startup and added at runtime.
2. Both displays capture the requested output and deliver clicks at center and edges. Test the fractional-scale boundary, a moved window, an output removal, and a stale screenshot. Window-local automation on DP-2 must not regress.
3. The agent cursor visibly renders on both outputs, aligns with delivered input, disappears when the turn ends, and never captures physical input.
4. Build and install only the dev variant, restart `zeron-dev.service`, and follow `AGENTS.md` to compare the build, launcher, and running executable checksums. Do not touch production data.

Host-side cleanup still required after debugging:

```bash
pkexec sysctl kernel.yama.ptrace_scope=1
cat /proc/sys/kernel/yama/ptrace_scope
```

The capture script only records this value. It does not restore it.

## Verification actually performed

The local sandbox copy of `capture.py` was checked against the GitHub blob SHA `e1f5382385f7abea2f7aea24afbd9b7ec8b51e84`. Python compilation succeeded. All 19 standard-library unit tests in `scripts/cua/test_capture.py` passed, covering classification, malformed fields, both lanes, passive hover, command failures, excerpt handling, permissions, process-parent identification, argument exclusion, and a mocked one-sample CLI run.

No Rust/C++ compilation, native integration test, live overlay observation, or physical input test has been performed. The plugin and native driver have not been changed by this investigation.

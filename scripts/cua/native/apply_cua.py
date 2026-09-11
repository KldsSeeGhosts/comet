#!/usr/bin/env python3
"""Apply the reviewed Cua 0.27.0 integration without resetting the checkout.

All anchors are checked before any write. Existing files are backed up first.
The user's hyprland.rs screen-size patch is deliberately untouched.
"""
from pathlib import Path
import argparse
import hashlib
import json
import os
import tempfile

HERE = Path(__file__).resolve().parent


def replace(source: str, before: str, after: str) -> str:
    if source.count(before) != 1:
        raise ValueError(f"Source drift or partial patch: expected one anchor {before[:100]!r}")
    return source.replace(before, after, 1)


def plan(root: Path) -> dict[Path, str]:
    rust = root / "libs/cua-driver/rust"
    changes = {}
    path = rust / "crates/platform-linux/src/wayland/mod.rs"
    s = path.read_text()
    s = replace(s, "pub mod hyprland;", "pub mod hyprland;\npub mod noches_display;")
    s = replace(s, "struct State {\n", "struct State {\n    outputs: noches_display::Outputs,\n")
    s = replace(s, '''        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
''', '''        if let wl_registry::Event::GlobalRemove { name } = &event {
            state.outputs.remove(*name);
        }
        if let wl_registry::Event::Global { name, interface, version } = event {
            state.outputs.global(registry, name, &interface, version, qh);
''')
    a = s.index("impl Dispatch<WlOutput, ()> for State")
    b = s.index("impl Dispatch<ZwlrVirtualPointerManagerV1", a)
    block = replace(s[a:b], "_: &WlOutput,", "output: &WlOutput,")
    block = replace(block, "        if let wl_output::Event::Mode { width, height, .. } = event {", '''        if state.output.as_ref() != Some(output) { return; }
        if let wl_output::Event::Mode { flags: WEnum::Value(flags), width, height, .. } = event {
            if !flags.contains(wl_output::Mode::Current) { return; }''')
    s = s[:a] + block + s[b:]
    s = replace(s, "fn capture_via_screencopy() -> anyhow::Result<Vec<u8>> {\n", '''fn capture_via_screencopy() -> anyhow::Result<Vec<u8>> {
    capture_via_screencopy_selected(None)
}
fn capture_via_screencopy_selected(display: Option<&noches_display::Display>) -> anyhow::Result<Vec<u8>> {
''')
    s = replace(s, "    queue.roundtrip(&mut state)?; // outputs report their Mode\n", "    for _ in 0..3 { queue.roundtrip(&mut state)?; } // output names and xdg logical geometry\n")
    s = replace(s, '''    let output = state
        .output
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor exposed no wl_output to capture"))?;''', '''    let output = match display {
        Some(display) => state.outputs.verify(display)?,
        None => state.output.clone().ok_or_else(|| anyhow::anyhow!("compositor exposed no wl_output to capture"))?,
    };''')
    s = replace(s, "        let w = state.capture.width;\n        let h = state.capture.height;\n", '''        let w = state.capture.width;
        let h = state.capture.height;
        if let Some(display) = display {
            state.outputs.verify(display)?;
            anyhow::ensure!((w, h) == (display.width, display.height), "captured dimensions disagree with selected output");
        }
''')
    changes[path] = s
    path = rust / "crates/cua-driver-core/src/action_target.rs"
    s = path.read_text()
    s = replace(s, '    let Some(target) = object.remove("target") else {', '''    if object.contains_key("display_id") && object.contains_key("target") {
        return Err(invalid_target("target cannot be combined with legacy display_id"));
    }
    let Some(target) = object.remove("target") else {''')
    s = replace(s, '            if display_id != "primary" {', '            if (!cfg!(target_os = "linux") && display_id != "primary") || display_id.len() > 256 || display_id.chars().any(char::is_control) {')
    s = replace(s, '            object.insert("scope".into(), Value::String("desktop".into()));', '''            object.insert("scope".into(), Value::String("desktop".into()));
            if cfg!(target_os = "linux") {
                object.insert("display_id".into(), Value::String(display_id.into()));
            }''')
    s = replace(s, 'json!({"target": {"kind": "desktop", "display_id": "secondary"}}),', 'json!({"target": {"kind": "desktop", "display_id": ""}}),')
    s = replace(s, '    #[test]\n    fn ambiguous_or_unsupported_targets_fail_closed()', '''    #[test]
    #[cfg(target_os = "linux")]
    fn named_display_survives_normalization() {
        let mut args = json!({"target":{"kind":"desktop","display_id":"DP-1"},"x":0,"y":0});
        normalize_action_target("click", &mut args).unwrap();
        assert_eq!(args["display_id"], "DP-1");
        assert_eq!(args["scope"], "desktop");
    }
    #[test]
    fn ambiguous_or_unsupported_targets_fail_closed()''')
    changes[path] = s
    path = rust / "crates/platform-linux/src/tools/impl_.rs"
    s = path.read_text()
    tools = [("ClickTool", "click", True), ("DragTool", "drag", True), ("ScrollTool", "scroll", True),
        ("MoveCursorTool", "move_cursor", True), ("TypeTextTool", "type_text", True),
        ("PressKeyTool", "press_key", True), ("HotkeyTool", "hotkey", True),
        ("GetScreenSizeTool", "get_screen_size", False), ("GetDesktopStateTool", "get_desktop_state", False)]
    for typ, name, state in tools:
        a = s.index("\nimpl Tool for " + typ + " {")
        b = s.index("\n    async fn invoke", a)
        block = replace(s[a:b], "get_or_init(|| ToolDef {", "get_or_init(|| { let mut def = ToolDef {")
        end = block.rfind("        })")
        if end < 0:
            raise ValueError(f"Missing definition boundary: {typ}")
        block = block[:end] + replace(block[end:], "        })", "        }; noches_display_definition(&mut def); def })")
        s = s[:a] + block + s[b:]
        a = s.index("\nimpl Tool for " + typ + " {")
        b = s.index("async fn invoke(&self, args: Value) -> ToolResult {", a)
        old = "async fn invoke(&self, args: Value) -> ToolResult {"
        new = old.replace("args: Value", "mut args: Value") + '\n        if let Some(result) = noches_desktop_tool("' + name + '", &mut args, ' + ("Some(&self.state)" if state else "None") + ').await { return result; }'
        s = s[:b] + s[b:].replace(old, new, 1)
    s += '\ninclude!("noches_desktop.rs");\n'
    changes[path] = s
    for folder, name in [("wayland", "noches_display.rs"), ("tools", "noches_desktop.rs")]:
        path = rust / "crates/platform-linux/src" / folder / name
        if path.exists():
            raise ValueError(f"Existing custom file: {path}; do not overwrite")
        changes[path] = (HERE / "cua" / name).read_text()
    return changes


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.checkout.resolve()
    stamp = root / ".git/noches-cua-output-patch.json"
    if not (root / ".git").is_dir():
        raise SystemExit("Expected an ordinary Cua Git checkout, not a linked worktree")
    if stamp.exists():
        record = json.loads(stamp.read_text())
        for rel, digest in record["files"].items():
            if hashlib.sha256((root / rel).read_bytes()).hexdigest() != digest:
                raise SystemExit(f"Patched file changed since install: {rel}; reconcile manually")
        print("Cua output patch already installed; hashes verified")
        return
    changes = plan(root)
    if args.check:
        print(f"All anchors checked; {len(changes)} files would change")
        return
    backup = Path(tempfile.mkdtemp(prefix="noches-cua-before-", dir=root / ".git"))
    before = {path: path.read_bytes() if path.exists() else None for path in changes}
    for path, data in before.items():
        if data is not None:
            target = backup / path.relative_to(root)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
    try:
        for path, text in changes.items():
            path.write_text(text)
        stamp.write_text(json.dumps({"backup": str(backup), "files": {
            str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest() for path in changes}}, indent=2) + "\n")
    except BaseException:
        for path, data in before.items():
            if data is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(data)
        raise
    print(f"Applied output patch; originals: {backup}")


if __name__ == "__main__":
    main()

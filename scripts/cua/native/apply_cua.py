#!/usr/bin/env python3
"""Apply the reviewed Cua 0.27.0 integration without resetting the checkout.

All anchors are checked before any write. Existing files are backed up first.
The user's hyprland.rs screen-size patch is deliberately untouched.

A checkout that already carries the upstream-committed repair is detected from
semantic markers plus the custom files, so `--check` and apply become no-ops
there. Partial integration fails closed. Older unintegrated baselines still go
through the anchor plan below. When an install record exists but its hashes no
longer match, the mismatch is only accepted as upstream integration if every
recorded file is tracked and clean at git HEAD; a local edit made after
installation is never silently accepted.
"""
from pathlib import Path
import argparse
import hashlib
import json
import os
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
DRIVER = Path("libs/cua-driver/rust/crates")
# Each group must match as a whole. Whitespace is ignored so rustfmt reflows
# and equivalent formatting do not look like drift.
INTEGRATION_GROUPS = (
    (DRIVER / "platform-linux/src/wayland/mod.rs", (
        "pub mod noches_display;",
        "outputs: noches_display::Outputs,",
        "state.outputs.remove(*name);",
        "state.outputs.global(registry, name, &interface, version, qh);",
        "capture_via_screencopy_selected",
        "state.outputs.verify(display)?",
        "captured dimensions disagree with selected output",
        "flags.contains(wl_output::Mode::Current)",
    )),
    (DRIVER / "cua-driver-core/src/action_target.rs", (
        "target cannot be combined with legacy display_id",
        "display_id.len() > 256",
        "char::is_control",
        'object.insert("display_id"',
        "fn named_display_survives_normalization()",
    )),
    (DRIVER / "platform-linux/src/tools/impl_.rs", (
        "if let Some(result) = noches_desktop_tool(",
        "noches_display_definition(&mut def);",
        'include!("noches_desktop.rs");',
    )),
    (DRIVER / "platform-linux/src/wayland/noches_display.rs", (
        "pub struct Display",
        "pub(super) struct Outputs",
        "pub(super) fn verify(&self, display: &Display)",
        "pub fn layout_token(&self)",
    )),
    (DRIVER / "platform-linux/src/tools/noches_desktop.rs", (
        "fn noches_display_definition(def: &mut ToolDef)",
        "async fn noches_desktop_tool(name: &str",
    )),
)


def _compacted(text: str) -> str:
    return "".join(text.split())


def integration_state(root: Path) -> tuple[str, str]:
    """Return ("full" | "none" | "partial", detail) for the output repair."""
    complete, partial, absent = [], [], []
    for rel, markers in INTEGRATION_GROUPS:
        path = root / rel
        if not path.is_file():
            absent.append(str(rel))
            continue
        text = _compacted(path.read_text())
        missing = [marker for marker in markers if _compacted(marker) not in text]
        if not missing:
            complete.append(str(rel))
        elif len(missing) == len(markers):
            absent.append(str(rel))
        else:
            partial.append(str(rel))
    if complete and not partial and not absent:
        return "full", ""
    if not complete and not partial:
        return "none", ""
    detail = []
    if partial:
        detail.append("incomplete marker sets in " + ", ".join(partial))
    if complete and absent:
        detail.append("missing or unmarked " + ", ".join(absent))
    return "partial", "; ".join(detail)


def tracked_clean_at_head(root: Path, paths: tuple[str, ...]) -> bool:
    """True when every path is tracked and unmodified relative to git HEAD."""
    commands = (
        ["git", "-C", str(root), "ls-files", "--error-unmatch", "--", *paths],
        ["git", "-C", str(root), "diff", "--quiet", "HEAD", "--", *paths],
    )
    for command in commands:
        try:
            result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError:
            return False
        if result.returncode != 0:
            return False
    return True


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
    state, detail = integration_state(root)
    if state == "partial":
        raise SystemExit(f"Cua output repair is only partially integrated ({detail}); reconcile manually")
    integrated = state == "full"
    if stamp.exists():
        record = json.loads(stamp.read_text())
        changed = [rel for rel, digest in record["files"].items()
                   if not (root / rel).is_file()
                   or hashlib.sha256((root / rel).read_bytes()).hexdigest() != digest]
        if changed:
            if integrated and tracked_clean_at_head(root, tuple(record["files"])):
                print("Cua checkout has the upstream-committed output changes; stale install record ignored")
                return
            raise SystemExit(f"Patched file changed since install: {changed[0]}; reconcile manually")
        print("Cua output patch already installed; hashes verified")
        return
    if integrated:
        print("Cua checkout already contains the output changes; nothing to apply")
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

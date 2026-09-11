#!/usr/bin/env python3
"""Route type_text through the existing Hyprland background transaction."""
from pathlib import Path
import argparse
import hashlib
import json
import tempfile


BACKGROUND_FUNCTION = '''
pub(crate) fn execute_background_text(
    owner: Option<String>,
    pid: u32,
    address: u64,
    text: &str,
    cancellation: ActionCancellation,
) -> Result<Value> {
    let actions = foreground_text_actions(text)?;
    ensure!(!actions.is_empty(), "background text must not be empty");
    execute_actions_routed(
        owner,
        pid,
        address,
        actions,
        None,
        cancellation,
        DeliveryRoute::Background,
        true,
    )
}
'''

TYPE_TEXT_ROUTE = '''        if isolated_hyprland_background(delivery) {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated text",
                );
            }
            if resolved_elem_idx.is_some()
                || args.get("x").is_some()
                || args.get("y").is_some()
            {
                return isolated_hyprland_refusal(
                    "isolated text addresses the exact top-level; first click the child explicitly",
                );
            }
            match crate::wayland::hyprland_input::foreground_text_actions(&text) {
                Ok(actions) if !actions.is_empty() => {}
                Ok(_) => return isolated_hyprland_refusal("isolated text must not be empty"),
                Err(error) => return isolated_hyprland_refusal(error.to_string()),
            }
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_background_text(
                        owner,
                        pid,
                        xid,
                        &text,
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            return match dispatch.await {
                Ok(result) => isolated_hyprland_result(result),
                Err(error) => isolated_hyprland_task_error(error, false),
            };
        }
'''

BACKGROUND_TEST = '''    #[test]
    fn background_text_reports_background_delivery_for_every_key() {
        let reply = execute_text_actions(
            foreground_text_actions("abc").unwrap(),
            DeliveryRoute::Background,
            |_| Ok(json!({"ok":true})),
        )
        .unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["route"], "synthetic_events");
        assert_eq!(reply["delivery"], json!({"mode":"background","delivered_count":3}));
    }

'''

BACKGROUND_CLIENT_TEST = '''    #[test]
    fn production_background_text_key_reaches_the_compositor() {
        reset_test_attestations();
        let (mut client, peer) = production_test_client();
        let server = std::thread::spawn(move || {
            serve_key_target(&peer, DeliveryRoute::Background, 1, 11);
            peer.send(br#"{"ok":true,"effect":"unverifiable","route":"synthetic_events"}"#)
                .unwrap();
        });
        let reply = client
            .execute_routed_with_attest(
                Action::TextKey {
                    keycode: 30,
                    shift: false,
                },
                None,
                DeliveryRoute::Background,
                record_test_attestation,
            )
            .unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["route"], "synthetic_events");
        assert_eq!(test_attestations(), 1);
        server.join().unwrap();
    }

'''


def replace_once(source: str, old: str, new: str) -> str:
    if source.count(new) == 1:
        return source
    if source.count(old) != 1 or source.count(new) != 0:
        raise ValueError(f"Source drift or partial background-text repair near {old[:80]!r}")
    return source.replace(old, new, 1)


def insert_test_once(source: str, function_name: str, anchor: str, test: str) -> str:
    marker = f"fn {function_name}()"
    count = source.count(marker)
    if count == 1:
        return source
    if count != 0:
        raise ValueError(f"Duplicate generated test: {function_name}")
    return replace_once(source, anchor, test + anchor)


def plan(root: Path) -> dict[Path, str]:
    wayland = root / "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_input.rs"
    source = wayland.read_text()
    foreground_end = '''        true,
    )
}

fn execute_actions_routed('''
    source = replace_once(source, foreground_end, '''        true,
    )
}
''' + BACKGROUND_FUNCTION + '''
fn execute_actions_routed(''')
    source = replace_once(
        source,
        '''            execute_text_actions(actions, |action| {''',
        '''            execute_text_actions(actions, route, |action| {''',
    )
    source = replace_once(
        source,
        '''fn execute_text_actions(
    actions: Vec<Action>,
    mut dispatch: impl FnMut(Action) -> Result<Value>,
) -> Result<Value> {
    let route = DeliveryRoute::Foreground;''',
        '''fn execute_text_actions(
    actions: Vec<Action>,
    route: DeliveryRoute,
    mut dispatch: impl FnMut(Action) -> Result<Value>,
) -> Result<Value> {''',
    )
    old_call = '''execute_text_actions(foreground_text_actions('''
    if source.count(old_call):
        source = source.replace(old_call, '''execute_text_actions(foreground_text_actions(''')
        # Add the route after each first argument. Rustfmt may wrap these calls,
        # so patch the six known test forms rather than parse Rust.
        source = source.replace('''foreground_text_actions(text).unwrap(), |action| {''', '''foreground_text_actions(text).unwrap(), DeliveryRoute::Foreground, |action| {''')
        source = source.replace('''foreground_text_actions("abc").unwrap(), |action| {''', '''foreground_text_actions("abc").unwrap(), DeliveryRoute::Foreground, |action| {''')
        source = source.replace('''foreground_text_actions("abc").unwrap(), |_| {''', '''foreground_text_actions("abc").unwrap(), DeliveryRoute::Foreground, |_| {''')
    if source.count("execute_text_actions(") < 7 or "actions, |action|" in source:
        raise ValueError("Source drift while updating background text tests")
    test_anchor = '''    #[test]
    fn foreground_text_unknown_delivery_keeps_only_acknowledged_key_count() {'''
    source = insert_test_once(
        source,
        "background_text_reports_background_delivery_for_every_key",
        test_anchor,
        BACKGROUND_TEST,
    )
    source = insert_test_once(
        source,
        "production_background_text_key_reaches_the_compositor",
        test_anchor,
        BACKGROUND_CLIENT_TEST,
    )
    source = replace_once(
        source,
        '''            route == DeliveryRoute::Foreground
                || !matches!(&action, Action::Activate | Action::TextKey { .. }),''',
        '''            route == DeliveryRoute::Foreground
                || !matches!(&action, Action::Activate),''',
    )

    tools = root / "libs/cua-driver/rust/crates/platform-linux/src/tools/impl_.rs"
    tool_source = tools.read_text()
    start = tool_source.index("impl Tool for TypeTextTool {")
    end = tool_source.index("// ── press_key", start)
    block = tool_source[start:end]
    anchor = '''        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {'''
    block = replace_once(block, anchor, '''        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
''' + TYPE_TEXT_ROUTE + '''        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {''')
    tool_source = tool_source[:start] + block + tool_source[end:]
    return {wayland: source, tools: tool_source}


def refresh_parent_stamps(root: Path, changes: dict[Path, str]) -> None:
    """Record final digests where this repair layers over earlier repairs."""
    for name in ["noches-cua-output-patch.json", "noches-cua-hyprland-runtime.json"]:
        stamp = root / ".git" / name
        if not stamp.exists():
            continue
        record = json.loads(stamp.read_text())
        updated = False
        for path in changes:
            relative = str(path.relative_to(root))
            if relative in record.get("files", {}):
                record["files"][relative] = hashlib.sha256(path.read_bytes()).hexdigest()
                updated = True
        if updated:
            stamp.write_text(json.dumps(record, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.checkout.resolve()
    stamp = root / ".git/noches-cua-hyprland-text.json"
    if not (root / ".git").is_dir():
        raise SystemExit("Expected an ordinary Cua Git checkout, not a linked worktree")
    if stamp.exists():
        record = json.loads(stamp.read_text())
        for relative, digest in record["files"].items():
            if hashlib.sha256((root / relative).read_bytes()).hexdigest() != digest:
                raise SystemExit(f"Patched file changed since install: {relative}; reconcile manually")
    changes = plan(root)
    pending = {path: source for path, source in changes.items() if path.read_text() != source}
    if not pending:
        refresh_parent_stamps(root, changes)
        print("Hyprland background-text repair already installed; hashes verified")
        return
    if args.check:
        print(f"All anchors checked; {len(pending)} files would change")
        return
    backup = Path(tempfile.mkdtemp(prefix="noches-hyprland-text-before-", dir=root / ".git"))
    before = {path: path.read_bytes() for path in pending}
    try:
        for path, source in pending.items():
            target = backup / path.relative_to(root)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(before[path])
            path.write_text(source)
        stamp.write_text(json.dumps({"backup": str(backup), "files": {
            str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in changes
        }}, indent=2) + "\n")
        refresh_parent_stamps(root, changes)
    except BaseException:
        for path, data in before.items():
            path.write_bytes(data)
        stamp.unlink(missing_ok=True)
        raise
    print(f"Applied Hyprland background-text repair; originals: {backup}")


if __name__ == "__main__":
    main()

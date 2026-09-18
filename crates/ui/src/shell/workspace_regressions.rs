// Included in shell::workspace_persistence to reuse the real GPUI shell fixture.

fn seed_selected_project(shell: &mut Shell, cx: &mut Context<Shell>) {
    shell.state.update(cx, |state, _| {
        state.spaces = vec![serde_json::from_value(space("b")).unwrap()];
        state.chats = vec![
            serde_json::from_value(chat("chat-a", "b")).unwrap(),
            serde_json::from_value(chat("chat-b", "b")).unwrap(),
        ];
        state.selected_space = Some("b".into());
        state.selected_chat = Some("chat-a".into());
        state.auto_selected = true;
        state.chats_synced = true;
        state.spaces_synced = true;
    });
}

#[gpui::test]
fn boot_without_saved_layout_keeps_the_selected_chat(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("chat-a"));
        assert_eq!(shell.active_chat, "chat-a");
        let pane = shell.workspace.focused_pane().unwrap();
        assert_eq!(shell.workspace.layout.pane(pane).unwrap().session_id.as_deref(), Some("chat-a"));
    }).unwrap();
}

#[gpui::test]
fn invalid_layout_file_keeps_the_selected_chat(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        crate::workspace_layout_store::WorkspaceLayoutStore::path(dir.path()),
        "{not valid json",
    ).unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("chat-a"));
        shell.workspace.layout.validate().unwrap();
    }).unwrap();
}

#[gpui::test]
fn same_project_navigation_focuses_existing_pane_without_rebinding_its_neighbor(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        let first = shell.workspace.focused_pane().unwrap();
        let second = shell.workspace.split_focused_pane(Direction::Right).unwrap();
        shell.workspace.set_pane_session(second, Some("chat-b".into())).unwrap();
        shell.focus_workspace_pane(first, cx);
        shell.on_state_changed(&shell.state.clone(), cx);

        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.workspace.focused_pane(), Some(second));
        assert_eq!(shell.workspace.layout.pane(first).unwrap().session_id.as_deref(), Some("chat-a"));
        assert_eq!(shell.workspace.layout.pane(second).unwrap().session_id.as_deref(), Some("chat-b"));
        assert!(shell.pending_explicit_nav.is_none());
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.workspace.focused_pane(), Some(second));
    }).unwrap();
}

#[gpui::test]
fn explicit_cross_project_navigation_wins_over_saved_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    seed_space_layout(dir.path(), "chat-b");
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.pending_explicit_nav = Some(Some("chat-a".into()));
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("chat-a"));
        assert!(shell.pending_explicit_nav.is_none());
        assert_eq!(shell.workspace.layout.views.len(), 2);
        let focused = shell.workspace.focused_pane().unwrap();
        assert_eq!(shell.workspace.layout.pane(focused).unwrap().session_id.as_deref(), Some("chat-a"));
    }).unwrap();
}

#[gpui::test]
fn new_session_intent_clears_only_the_focused_pane_and_is_consumed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        let first = shell.workspace.focused_pane().unwrap();
        let second = shell.workspace.split_focused_pane(Direction::Right).unwrap();
        shell.workspace.set_pane_session(second, Some("chat-b".into())).unwrap();
        shell.focus_workspace_pane(second, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        shell.open_new_session(cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert!(shell.state.read(cx).selected_chat.is_none());
        assert!(shell.workspace.layout.pane(second).unwrap().session_id.is_none());
        assert_eq!(shell.workspace.layout.pane(first).unwrap().session_id.as_deref(), Some("chat-a"));
        assert!(shell.pending_explicit_nav.is_none());
    }).unwrap();
}

/// A re-select of the already-selected, already-seen session is a no-op in
/// `AppState::select_chat` — but the navigation intent `open_chat` armed must
/// not survive it: `select_chat` still has to notify, so the state
/// observation consumes the intent in the click's own cycle instead of some
/// unrelated later frame rebinding focus with it.
#[gpui::test]
fn reselecting_the_active_seen_session_does_not_arm_stale_navigation(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    let nav_notifications = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let (first, second) = window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        let first = shell.workspace.focused_pane().unwrap();
        let second = shell.workspace.split_focused_pane(Direction::Right).unwrap();
        shell.workspace.set_pane_session(second, Some("chat-b".into())).unwrap();
        shell.focus_workspace_pane(first, cx);
        shell.on_state_changed(&shell.state.clone(), cx);

        // chat-a is selected and seen: the click below is a selection no-op.
        let hits = nav_notifications.clone();
        cx.observe(&shell.state, move |_, _, _| hits.set(hits.get() + 1)).detach();
        shell.open_chat("chat-a".into(), cx);
        (first, second)
    }).unwrap();
    window.update(cx, |shell, _, cx| {
        assert!(
            nav_notifications.get() >= 1,
            "a no-op re-select must still notify AppState so the armed \
             navigation intent is consumed now, not on a later frame",
        );
        assert!(shell.pending_explicit_nav.is_none());
        // A later unrelated update has nothing armed to act on: bindings and
        // focus are exactly where the click left them.
        shell.on_state_changed(&shell.state.clone(), cx);
        assert!(shell.pending_explicit_nav.is_none());
        assert_eq!(shell.workspace.focused_pane(), Some(first));
        assert_eq!(shell.workspace.layout.pane(first).unwrap().session_id.as_deref(), Some("chat-a"));
        assert_eq!(shell.workspace.layout.pane(second).unwrap().session_id.as_deref(), Some("chat-b"));
    }).unwrap();
}

/// `+` while already on the new-session canvas re-runs `open_new_session`,
/// whose `select_chat(None)` is a no-op. The re-armed intent must be consumed
/// by the guaranteed notification, not survive into an unrelated frame —
/// where it would unbind whatever the focused pane picked up in between.
#[gpui::test]
fn new_session_request_while_already_on_canvas_does_not_clear_panes_later(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    let nav_notifications = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let (first, second) = window.update(cx, |shell, _, cx| {
        seed_selected_project(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        let first = shell.workspace.focused_pane().unwrap();
        let second = shell.workspace.split_focused_pane(Direction::Right).unwrap();
        shell.workspace.set_pane_session(second, Some("chat-b".into())).unwrap();
        shell.focus_workspace_pane(second, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        // The first request lands on the canvas: the focused pane unbinds.
        shell.open_new_session(cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert!(shell.state.read(cx).selected_chat.is_none());
        assert!(shell.workspace.layout.pane(second).unwrap().session_id.is_none());
        assert_eq!(shell.workspace.layout.pane(first).unwrap().session_id.as_deref(), Some("chat-a"));
        (first, second)
    }).unwrap();
    // A separate update so the canvas re-request is observed on its own —
    // like a real click, with no earlier AppState notification pending.
    window.update(cx, |shell, _, cx| {
        let hits = nav_notifications.clone();
        cx.observe(&shell.state, move |_, _, _| hits.set(hits.get() + 1)).detach();
        shell.open_new_session(cx);
    }).unwrap();
    window.update(cx, |shell, _, cx| {
        assert!(
            nav_notifications.get() >= 1,
            "a no-op new-session request must still notify AppState so the \
             armed navigation intent is consumed now, not on a later frame",
        );
        assert!(shell.pending_explicit_nav.is_none());
        // Damage window: the focused pane picks up a session with no AppState
        // notification in between (a direct binding here; a layout restore or
        // tab drop in the app). The already-consumed intent must not clear it.
        shell.workspace.set_pane_session(second, Some("chat-b".into())).unwrap();
        shell.on_state_changed(&shell.state.clone(), cx);
        assert!(shell.pending_explicit_nav.is_none());
        assert_eq!(shell.workspace.layout.pane(second).unwrap().session_id.as_deref(), Some("chat-b"));
        assert_eq!(shell.workspace.layout.pane(first).unwrap().session_id.as_deref(), Some("chat-a"));
    }).unwrap();
}

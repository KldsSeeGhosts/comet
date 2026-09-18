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

// Included in shell::workspace_persistence to exercise solo navigation over a real split.

#[gpui::test]
fn solo_mint_and_return_preserve_split_bindings_and_live_drafts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_selected_project(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let first = shell.workspace.focused_pane().unwrap();
            let second = shell
                .workspace
                .split_focused_pane(Direction::Right)
                .unwrap();
            shell
                .workspace
                .set_pane_session(second, Some("chat-b".into()))
                .unwrap();
            shell.ensure_pane_chat_surfaces(cx);
            let pane_composer = shell.workspace.chat_surfaces[&first].composer.clone();
            draft(&pane_composer, "keep my split-pane draft", cx);
            shell.focus_workspace_pane(second, cx);
            shell.on_state_changed(&shell.state.clone(), cx);

            shell.open_new_session(cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.solo_session);
            assert!(!shell.workspace_mode());
            assert_ne!(
                shell.active_composer().entity_id(),
                pane_composer.entity_id()
            );
            assert_eq!(draft_text(&pane_composer, cx), "keep my split-pane draft");
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(first)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-a")
            );
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(second)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-b")
            );

            // The first send selects its minted chat. It belongs to the solo
            // surface, not to whichever split pane happened to be focused.
            shell.state.update(cx, |state, cx| {
                state
                    .chats
                    .push(serde_json::from_value(chat("solo-new", "b")).unwrap());
                state.select_chat(Some("solo-new".into()), cx);
            });
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.solo_session);
            assert!(shell.solo_chat_ids.contains("solo-new"));
            assert!(shell.find_pane_with_session("solo-new").is_none());

            shell.open_chat("chat-a".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.workspace_mode());
            assert_eq!(shell.workspace.focused_pane(), Some(first));
            assert_eq!(draft_text(&pane_composer, cx), "keep my split-pane draft");
            shell.open_chat("solo-new".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.solo_session);
            assert!(!shell.workspace_mode());
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(first)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-a")
            );
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(second)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-b")
            );
            shell.workspace.layout.validate().unwrap();
        })
        .unwrap();
}

#[gpui::test]
fn project_group_plus_keeps_the_previous_spaces_split_intact(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_two_spaces(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let original = shell.workspace.focused_pane().unwrap();
            let neighbor = shell
                .workspace
                .split_focused_pane(Direction::Right)
                .unwrap();
            shell
                .workspace
                .set_pane_session(neighbor, Some("chat-a2".into()))
                .unwrap();
            shell.on_state_changed(&shell.state.clone(), cx);
            shell.open_new_session_in_space("b".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.solo_session);
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("b"));
            assert_eq!(shell.active_workspace_space.as_deref(), Some("a"));
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(original)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-a1")
            );
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(neighbor)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-a2")
            );

            shell.open_chat("chat-a2".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(!shell.solo_session);
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("a"));
            assert_eq!(shell.workspace.focused_pane(), Some(neighbor));
            assert_eq!(
                shell
                    .workspace
                    .layout
                    .pane(original)
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("chat-a1")
            );
            shell.workspace.layout.validate().unwrap();
        })
        .unwrap();
}

#[gpui::test]
fn solo_selection_reveals_a_saved_split_in_another_space(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // A saved two-view layout for b exists while the live workspace is a.
    let mut saved = crate::pane::PaneHost::new();
    saved.sync_focused_session(Some("chat-b1"));
    saved.split_focused_view(Direction::Right).unwrap();
    let mut layouts = crate::workspace_layout_store::WorkspaceLayoutStore::load(dir.path());
    layouts.set_layout(Some("b"), saved.layout.clone());
    layouts.flush().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_two_spaces(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            shell.open_new_session_in_space("b".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.solo_session);
            assert_eq!(shell.active_workspace_space.as_deref(), Some("a"));
            shell.open_chat("chat-b1".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(!shell.solo_session);
            assert_eq!(shell.active_workspace_space.as_deref(), Some("b"));
            assert_eq!(shell.workspace.layout.views.len(), 2);
            assert_eq!(
                shell.workspace.focused_pane(),
                shell.find_pane_with_session("chat-b1")
            );
            shell.open_new_session(cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            // A deep link bypasses open_chat and must reveal the bound pane too.
            shell.state.update(cx, |state, cx| {
                state.select_chat(Some("chat-b1".into()), cx)
            });
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(!shell.solo_session);
            assert_eq!(shell.workspace.layout.views.len(), 2);
            assert_eq!(
                shell.workspace.focused_pane(),
                shell.find_pane_with_session("chat-b1")
            );
        })
        .unwrap();
}

#[gpui::test]
fn drop_on_solo_surface_reveals_its_workspace_destination(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_selected_project(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let target = shell.workspace.focused_pane().unwrap();
            shell
                .workspace
                .split_focused_pane(Direction::Right)
                .unwrap();
            shell.on_state_changed(&shell.state.clone(), cx);
            shell.open_new_session(cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert!(shell.solo_session);
            shell.split_drag = Some(sidebar_drop(
                "chat-b",
                crate::pane::hit_test::DropPlan::SplitPane {
                    pane: target,
                    direction: Direction::Down,
                },
            ));
            shell.accept_sidebar_session_drop(&sidebar_payload("chat-b"), cx);
            assert!(!shell.solo_session);
            assert!(shell.workspace_mode());
            assert_eq!(
                shell.workspace.focused_pane(),
                shell.find_pane_with_session("chat-b")
            );
            shell.workspace.layout.validate().unwrap();
        })
        .unwrap();
}

// Included in shell::workspace_persistence; these exercise real GPUI entities.

fn prepare_selected_composer(shell: &mut Shell, cx: &mut Context<Shell>) {
    seed_selected_project(shell, cx);
    shell.on_state_changed(&shell.state.clone(), cx);
    // Complete the selected composer's normal routing before simulating input.
    shell.composer.update(cx, |composer, cx| {
        composer.set_target(crate::state::ChatTarget::Selected, cx);
    });
}

fn draft(composer: &Entity<Composer>, text: &str, cx: &mut Context<Shell>) {
    composer.update(cx, |composer, cx| {
        composer.input.update(cx, |input, cx| input.set_text(text.to_string(), cx));
    });
}

fn draft_text(composer: &Entity<Composer>, cx: &Context<Shell>) -> String {
    composer.read(cx).input.read(cx).text().to_string()
}

#[gpui::test]
fn first_split_preserves_the_original_composer_and_focuses_only_the_new_pane(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        let first = shell.workspace.focused_pane().unwrap();
        let original = shell.composer.clone();
        draft(&original, "keep this unsent draft", cx);
        shell.split_workspace_view(Direction::Right, cx);
        let second = shell.workspace.focused_pane().unwrap();
        assert_ne!(first, second);
        let first_composer = &shell.workspace.chat_surfaces[&first].composer;
        let second_composer = &shell.workspace.chat_surfaces[&second].composer;
        assert_eq!(first_composer.entity_id(), original.entity_id());
        assert_ne!(first_composer.entity_id(), second_composer.entity_id());
        assert_eq!(draft_text(first_composer, cx), "keep this unsent draft");
        assert!(!first_composer.read(cx).focus_pending);
        assert!(second_composer.read(cx).focus_pending);
        assert_eq!(shell.active_composer().entity_id(), second_composer.entity_id());
        assert_eq!(first_composer.read(cx).current_key, "chat-a");
    }).unwrap();
}

#[gpui::test]
fn collapsing_to_one_pane_keeps_its_composer_and_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        let first = shell.workspace.focused_pane().unwrap();
        shell.split_workspace_view(Direction::Right, cx);
        let second = shell.workspace.focused_pane().unwrap();
        let survivor = shell.active_composer();
        draft(&survivor, "draft in the surviving pane", cx);
        shell.close_workspace_pane(first, cx);
        shell.ensure_pane_chat_surfaces(cx);
        assert!(shell.workspace.is_trivial());
        assert!(shell.workspace_mode());
        assert_eq!(shell.workspace.focused_pane(), Some(second));
        assert_eq!(shell.active_composer().entity_id(), survivor.entity_id());
        assert_eq!(draft_text(&survivor, cx), "draft in the surviving pane");
        assert!(!shell.workspace.chat_surfaces.contains_key(&first));
    }).unwrap();
}

#[gpui::test]
fn pane_navigation_restores_drafts_without_replacing_the_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        let composer = shell.active_composer();
        draft(&composer, "new session draft", cx);
        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.active_composer().entity_id(), composer.entity_id());
        assert_eq!(composer.read(cx).current_key, "chat-b");
        assert_eq!(draft_text(&composer, cx), "");
        draft(&composer, "existing session draft", cx);
        shell.open_new_session(cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(shell.active_composer().entity_id(), composer.entity_id());
        assert_eq!(draft_text(&composer, cx), "new session draft");
        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(draft_text(&composer, cx), "existing session draft");
    }).unwrap();
}

#[gpui::test]
fn first_send_binding_before_observers_preserves_the_sender_and_its_own_pane(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        let first = shell.workspace.focused_pane().unwrap();
        shell.split_workspace_view(Direction::Right, cx);
        let second = shell.workspace.focused_pane().unwrap();
        let sender = shell.active_composer();
        shell.focus_workspace_pane(first, cx);
        sender.update(cx, |composer, cx| composer.bind_chat("minted".into(), cx));
        // Force the render/ensure path before either target observer runs.
        shell.ensure_pane_chat_surfaces(cx);
        let surface = &shell.workspace.chat_surfaces[&second];
        assert_eq!(surface.composer.entity_id(), sender.entity_id());
        assert_eq!(surface.chat_id.as_deref(), Some("minted"));
        assert!(surface.transcript.is_some());
        assert_eq!(shell.workspace.layout.pane(second).unwrap().session_id.as_deref(), Some("minted"));
        assert_eq!(shell.workspace.layout.pane(first).unwrap().session_id.as_deref(), Some("chat-a"));
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("chat-a"));
        assert_eq!(shell.workspace.focused_pane(), Some(first));
    }).unwrap();
}

#[gpui::test]
fn first_send_rollback_keeps_the_canvas_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        let pane = shell.workspace.focused_pane().unwrap();
        let composer = shell.active_composer();
        composer.update(cx, |composer, cx| composer.bind_chat("minted".into(), cx));
        shell.ensure_pane_chat_surfaces(cx);
        composer.update(cx, |composer, cx| {
            composer.set_target(crate::state::ChatTarget::Fixed(None), cx);
        });
        draft(&composer, "recovered first message", cx);
        shell.ensure_pane_chat_surfaces(cx);
        let surface = &shell.workspace.chat_surfaces[&pane];
        assert_eq!(surface.composer.entity_id(), composer.entity_id());
        assert!(surface.chat_id.is_none());
        assert!(surface.transcript.is_none());
        assert!(shell.workspace.layout.pane(pane).unwrap().session_id.is_none());
        assert_eq!(draft_text(&composer, cx), "recovered first message");
    }).unwrap();
}

#[gpui::test]
fn explicit_navigation_is_not_overwritten_by_an_older_composer_binding(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        let pane = shell.workspace.focused_pane().unwrap();
        let composer = shell.active_composer();
        composer.update(cx, |composer, cx| composer.bind_chat("older-mint".into(), cx));
        shell.workspace.set_pane_session(pane, Some("chat-b".into())).unwrap();
        shell.ensure_pane_chat_surfaces(cx);
        assert_eq!(shell.workspace.layout.pane(pane).unwrap().session_id.as_deref(), Some("chat-b"));
        assert_eq!(shell.workspace.chat_surfaces[&pane].chat_id.as_deref(), Some("chat-b"));
        assert_eq!(shell.active_composer().entity_id(), composer.entity_id());
        assert_eq!(composer.read(cx).current_key, "chat-b");
    }).unwrap();
}

#[gpui::test]
fn a_late_queue_acknowledgement_does_not_reopen_the_previous_session(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        let pane = shell.workspace.focused_pane().unwrap();
        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        shell.on_pane_composer_event(pane, &ComposerEvent::Queued {
            chat_id: "older-mint".into(),
            message_id: "old-message".into(),
        }, cx);
        assert_eq!(shell.workspace.layout.pane(pane).unwrap().session_id.as_deref(), Some("chat-b"));
        assert_eq!(shell.workspace.chat_surfaces[&pane].chat_id.as_deref(), Some("chat-b"));
    }).unwrap();
}

#[gpui::test]
fn only_the_latest_focus_request_survives_before_paint(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        let first = shell.workspace.focused_pane().unwrap();
        shell.split_workspace_view(Direction::Right, cx);
        let second = shell.workspace.focused_pane().unwrap();
        shell.focus_workspace_pane(first, cx);
        assert!(shell.workspace.chat_surfaces[&first].composer.read(cx).focus_pending);
        assert!(!shell.workspace.chat_surfaces[&second].composer.read(cx).focus_pending);
    }).unwrap();
}

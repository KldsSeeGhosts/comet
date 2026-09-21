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
        assert!(shell.workspace.is_trivial());
        // The collapse handoff adopts the survivor entity as the dock
        // composer (issue #8): glass route back, same live Composer.
        assert!(!shell.workspace_mode());
        assert_eq!(shell.workspace.focused_pane(), Some(second));
        assert_eq!(
            shell.active_composer().entity_id(),
            survivor.entity_id(),
            "the survivor Composer entity is adopted, never rebuilt from a draft snapshot"
        );
        assert_eq!(draft_text(&survivor, cx), "draft in the surviving pane");
        assert_eq!(draft_text(&shell.active_composer(), cx), "draft in the surviving pane");
        assert!(shell.workspace.chat_surfaces.is_empty());
    }).unwrap();
}

/// Issue #8 + review: the collapse handoff must keep the survivor's live
/// Composer state (queue-edit lease, displaced draft) and must not merge the
/// closed neighbor's attachments into the dock.
#[gpui::test]
fn collapsing_preserves_live_composer_state_and_isolates_attachments(
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
        shell.ensure_pane_chat_surfaces(cx);
        let closed = shell.workspace.chat_surfaces[&first].composer.clone();
        let survivor = shell.workspace.chat_surfaces[&second].composer.clone();
        assert_ne!(closed.entity_id(), survivor.entity_id());

        // Closed neighbor stages an attachment that must die with its pane.
        closed.update(cx, |composer, _| {
            let key = composer.current_key.clone();
            composer.attachments.insert(
                key,
                vec![crate::attachments::stage_png_bytes(
                    "closed.png".into(),
                    b"closed".to_vec(),
                )],
            );
        });
        // Survivor holds a live queue-edit lease plus its own attachment.
        survivor.update(cx, |composer, _| {
            let key = composer.current_key.clone();
            composer.attachments.insert(
                key,
                vec![crate::attachments::stage_png_bytes(
                    "survivor.png".into(),
                    b"survivor".to_vec(),
                )],
            );
            composer.editing_queued = Some("row-lease".into());
            composer.queue_edit_draft =
                Some(("displaced words".into(), Vec::new(), Vec::new()));
        });
        draft(&survivor, "the leased row text", cx);

        shell.close_workspace_pane(first, cx);
        assert!(!shell.workspace_mode());
        assert_eq!(shell.active_composer().entity_id(), survivor.entity_id());
        shell.active_composer().update(cx, |composer, cx| {
            assert_eq!(
                composer.editing_queued.as_deref(),
                Some("row-lease"),
                "the queue-edit lease survives the collapse"
            );
            assert_eq!(
                composer.queue_edit_draft.as_ref().map(|(text, ..)| text.as_str()),
                Some("displaced words"),
                "the displaced draft survives the collapse"
            );
            assert_eq!(composer.input.read(cx).text(), "the leased row text");
            let names: Vec<&str> = composer
                .staged()
                .iter()
                .map(|att| att.name.as_str())
                .collect();
            assert_eq!(
                names,
                vec!["survivor.png"],
                "only the survivor's attachments remain"
            );
            assert!(
                !composer.attachments.values().flatten().any(|a| a.name == "closed.png"),
                "the closed pane's attachments must not leak into the dock"
            );
        });
    }).unwrap();
}

/// Issue #8: a live `chat_surfaces` entry must never pin the opaque
/// workspace route. After a split collapses, the glass single-session path
/// stays on even if the cache is repopulated (ensure / a later render pass).
#[gpui::test]
fn a_live_surface_cache_does_not_latch_the_workspace_route(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        let first = shell.workspace.focused_pane().unwrap();
        let original = shell.composer.clone();
        draft(&original, "keep this on the glass canvas", cx);
        shell.split_workspace_view(Direction::Right, cx);
        let second = shell.workspace.focused_pane().unwrap();
        // Close the NEW pane: the survivor is the adopted dock composer.
        shell.close_workspace_pane(second, cx);
        assert!(shell.workspace.is_trivial());
        assert!(!shell.workspace_mode());
        assert_eq!(shell.workspace.focused_pane(), Some(first));
        assert_eq!(shell.active_composer().entity_id(), original.entity_id());
        assert_eq!(draft_text(&shell.active_composer(), cx), "keep this on the glass canvas");
        // Re-populate the cache the way a later ensure/render pass would.
        shell.ensure_pane_chat_surfaces(cx);
        assert!(
            !shell.workspace.chat_surfaces.is_empty(),
            "ensure rebuilds the survivor surface"
        );
        assert!(
            !shell.workspace_mode(),
            "the cache must not latch the opaque workspace route (issue #8)"
        );
        assert!(shell.transcript_underlay_fades_top());
        assert!(!shell.pane_chrome_wins_titlebar_band());
        assert_eq!(draft_text(&shell.active_composer(), cx), "keep this on the glass canvas");
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

#[gpui::test]
fn a_late_first_send_failure_preserves_the_new_session_and_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        let pane = shell.workspace.focused_pane().unwrap();
        let composer = shell.active_composer();
        composer.update(cx, |composer, cx| composer.bind_chat("failed-mint".into(), cx));
        shell.ensure_pane_chat_surfaces(cx);
        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        draft(&composer, "newer session draft", cx);
        composer.update(cx, |composer, cx| {
            composer.restore_failed_send_input("failed-mint", true, "recover first prompt".into(), cx);
        });
        shell.ensure_pane_chat_surfaces(cx);
        assert_eq!(shell.workspace.layout.pane(pane).unwrap().session_id.as_deref(), Some("chat-b"));
        assert_eq!(composer.read(cx).current_key, "chat-b");
        assert_eq!(draft_text(&composer, cx), "newer session draft");
        shell.open_new_session(cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(draft_text(&composer, cx), "recover first prompt");
    }).unwrap();
}

#[gpui::test]
fn a_late_existing_send_failure_restores_only_its_own_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        let composer = shell.active_composer();
        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        shell.open_new_session(cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        draft(&composer, "newer canvas draft", cx);
        composer.update(cx, |composer, cx| {
            composer.restore_failed_send_input("chat-b", false, "recover existing prompt".into(), cx);
        });
        shell.ensure_pane_chat_surfaces(cx);
        assert_eq!(composer.read(cx).current_key, "");
        assert_eq!(draft_text(&composer, cx), "newer canvas draft");
        shell.open_chat("chat-b".into(), cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        assert_eq!(draft_text(&composer, cx), "recover existing prompt");
    }).unwrap();
}

#[gpui::test]
fn pointer_activation_changes_routing_without_requesting_composer_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        let first = shell.workspace.focused_pane().unwrap();
        shell.split_workspace_view(Direction::Right, cx);
        shell.pointer_focus_workspace_pane(first, cx);
        assert_eq!(shell.workspace.focused_pane(), Some(first));
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("chat-a"));
        assert!(shell.workspace.chat_surfaces.values().all(|surface| !surface.composer.read(cx).focus_pending));
    }).unwrap();
}

#[gpui::test]
fn clicking_the_active_pane_does_not_refocus_the_composer(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        prepare_selected_composer(shell, cx);
        shell.split_workspace_view(Direction::Right, cx);
        let pane = shell.workspace.focused_pane().unwrap();
        let revision = shell.workspace.layout.revision;
        shell.pointer_focus_workspace_pane(pane, cx);
        assert_eq!(shell.workspace.layout.revision, revision);
        assert!(!shell.active_composer().read(cx).focus_pending);
    }).unwrap();
}

// ---- project-switch draft parking ----

/// The pane composer bound to `chat_id` (the ensure pass guarantees one).
fn pane_composer_for_chat(shell: &Shell, chat_id: &str) -> Entity<Composer> {
    shell.workspace.chat_surfaces.values()
        .find(|surface| surface.chat_id.as_deref() == Some(chat_id))
        .map(|surface| surface.composer.clone())
        .unwrap_or_else(|| panic!("no pane surface bound to {chat_id}"))
}

/// Seed two spaces with one chat each and land the boot on space `a`.
fn seed_two_spaces(shell: &mut Shell, cx: &mut Context<Shell>) {
    shell.state.update(cx, |state, _| {
        state.spaces = vec![
            serde_json::from_value(space("a")).unwrap(),
            serde_json::from_value(space("b")).unwrap(),
        ];
        state.chats = vec![
            serde_json::from_value(chat("chat-a1", "a")).unwrap(),
            serde_json::from_value(chat("chat-a2", "a")).unwrap(),
            serde_json::from_value(chat("chat-b1", "b")).unwrap(),
        ];
        state.selected_space = Some("a".into());
        state.selected_chat = Some("chat-a1".into());
        state.auto_selected = true;
        state.chats_synced = true;
        state.spaces_synced = true;
    });
}

fn select_space(shell: &mut Shell, id: &str, cx: &mut Context<Shell>) {
    shell.state.update(cx, |state, cx| {
        state.select_space(Some(id.into()), cx)
    });
    shell.on_state_changed(&shell.state.clone(), cx);
}

#[gpui::test]
fn project_switch_preserves_each_panes_unsent_draft(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_two_spaces(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        // Two chat panes in space a, each holding its own unsent draft.
        let first = shell.workspace.focused_pane().unwrap();
        let second = shell.workspace.split_focused_pane(Direction::Right).unwrap();
        shell.workspace.set_pane_session(second, Some("chat-a2".into())).unwrap();
        shell.ensure_pane_chat_surfaces(cx);
        let composer_a1 = shell.workspace.chat_surfaces[&first].composer.clone();
        let composer_a2 = shell.workspace.chat_surfaces[&second].composer.clone();
        draft(&composer_a1, "alpha for a1", cx);
        draft(&composer_a2, "beta for a2", cx);
        shell.flush_workspace_layout(cx);

        // Switching projects tears the pane surfaces (and their drafts) down.
        select_space(shell, "b", cx);
        assert_eq!(shell.active_workspace_space.as_deref(), Some("b"));
        assert!(
            shell.workspace.chat_surfaces.is_empty(),
            "the switch dropped the pane surfaces"
        );

        // Switching back: BOTH drafts rehydrate into the correct panes.
        select_space(shell, "a", cx);
        let restored_a1 = pane_composer_for_chat(shell, "chat-a1");
        let restored_a2 = pane_composer_for_chat(shell, "chat-a2");
        assert_ne!(
            restored_a1.entity_id(),
            composer_a1.entity_id(),
            "the surface rebuilt onto a fresh composer"
        );
        assert_ne!(restored_a2.entity_id(), composer_a2.entity_id());
        assert_eq!(draft_text(&restored_a1, cx), "alpha for a1");
        assert_eq!(draft_text(&restored_a2, cx), "beta for a2");
    }).unwrap();
}

#[gpui::test]
fn parked_drafts_survive_pane_id_reuse_across_spaces(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // Space b's SAVED tree reuses space a's pane numerals: both anchor on
    // PaneId(3), bound to different sessions.
    seed_space_layout(dir.path(), "chat-b1");
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_two_spaces(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        // Space a boots onto its default pane (PaneId(3) → chat-a1); a split
        // keeps the tree non-trivial so pane surfaces exist at all.
        let pane_a = shell.workspace.focused_pane().unwrap();
        shell.workspace.split_focused_pane(Direction::Down).unwrap();
        shell.ensure_pane_chat_surfaces(cx);
        draft(&pane_composer_for_chat(shell, "chat-a1"), "alpha lives in a", cx);
        shell.flush_workspace_layout(cx);

        // a → b: PaneId(3) is REUSED for chat-b1's pane.
        select_space(shell, "b", cx);
        let pane_b = shell.workspace.focused_pane().unwrap();
        assert_eq!(pane_b, pane_a, "both spaces anchor their tree on PaneId(3)");
        let surface_b = shell.workspace.chat_surfaces[&pane_b].composer.clone();
        assert_eq!(
            draft_text(&surface_b, cx),
            "",
            "space b's pane must not inherit space a's draft"
        );
        draft(&surface_b, "beta lives in b", cx);
        shell.flush_workspace_layout(cx);

        // b → a: the parked (a, PaneId(3)) draft comes back, not b's.
        select_space(shell, "a", cx);
        assert_eq!(
            draft_text(&pane_composer_for_chat(shell, "chat-a1"), cx),
            "alpha lives in a"
        );

        // a → b again: b's own draft returns, still uncontaminated.
        select_space(shell, "b", cx);
        assert_eq!(
            draft_text(&pane_composer_for_chat(shell, "chat-b1"), cx),
            "beta lives in b"
        );
    }).unwrap();
}

#[gpui::test]
fn canvas_pane_draft_survives_project_switch_round_trip(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_two_spaces(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        // A split pane left UNBOUND: the new-chat canvas (key "").
        let canvas = shell.workspace.split_focused_pane(Direction::Right).unwrap();
        shell.ensure_pane_chat_surfaces(cx);
        shell.focus_workspace_pane(canvas, cx);
        let canvas_composer = shell.workspace.chat_surfaces[&canvas].composer.clone();
        assert!(shell.workspace.chat_surfaces[&canvas].chat_id.is_none());
        draft(&canvas_composer, "canvas scratch", cx);
        shell.flush_workspace_layout(cx);

        // Away and back: the unbound pane's unsent input rehydrates.
        select_space(shell, "b", cx);
        assert!(shell.workspace.chat_surfaces.is_empty());
        select_space(shell, "a", cx);
        let canvas_surface = shell.workspace.chat_surfaces.values()
            .find(|surface| surface.chat_id.is_none())
            .map(|surface| surface.composer.clone())
            .unwrap_or_else(|| panic!("the canvas pane lost its surface"));
        assert_eq!(draft_text(&canvas_surface, cx), "canvas scratch");
    }).unwrap();
}

#[gpui::test]
fn a_parked_queue_edit_salvages_the_displaced_draft_and_older_maps(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window.update(cx, |shell, _, cx| {
        seed_two_spaces(shell, cx);
        shell.on_state_changed(&shell.state.clone(), cx);
        // A split keeps the tree non-trivial so pane surfaces exist at all.
        shell.workspace.split_focused_pane(Direction::Down).unwrap();
        shell.ensure_pane_chat_surfaces(cx);
        let composer = pane_composer_for_chat(shell, "chat-a1");
        // Accumulate an older per-key draft entry by retargeting the SAME
        // composer the way the ensure pass does (the old text displaces
        // into the drafts map).
        composer.update(cx, |composer, cx| {
            composer.set_target(crate::state::ChatTarget::Fixed(Some("chat-a2".into())), cx);
        });
        draft(&composer, "older a2 words", cx);
        composer.update(cx, |composer, cx| {
            composer.set_target(crate::state::ChatTarget::Fixed(Some("chat-a1".into())), cx);
        });
        assert_eq!(draft_text(&composer, cx), "");
        // A queue edit is in flight: the input holds the HOST's leased row
        // text while the user's own words sit displaced in queue_edit_draft.
        // Both fields are injected directly — the real edit needs a live
        // host to grant the lease (the same shape as the composer's own
        // queue-edit tests).
        composer.update(cx, |composer, _| {
            composer.editing_queued = Some("row-1".into());
            composer.queue_edit_draft =
                Some(("displaced a1 words".into(), Vec::new(), Vec::new()));
        });
        draft(&composer, "the leased row text", cx);
        shell.flush_workspace_layout(cx);

        // Away and back: the DISPLACED words — not the leased row text —
        // come back as the pane's draft, and the older maps survive too.
        select_space(shell, "b", cx);
        select_space(shell, "a", cx);
        let restored = pane_composer_for_chat(shell, "chat-a1");
        assert_eq!(draft_text(&restored, cx), "displaced a1 words");
        restored.update(cx, |composer, cx| {
            composer.set_target(crate::state::ChatTarget::Fixed(Some("chat-a2".into())), cx);
        });
        assert_eq!(draft_text(&restored, cx), "older a2 words");
    }).unwrap();
}

// ---- mixed-space sidebar drops ----

fn sidebar_payload(session: &'static str) -> crate::pane::TabSplitDrag {
    crate::pane::TabSplitDrag {
        source: crate::pane::hit_test::DragSource::SidebarSession,
        mark: crate::pane::chrome::TabMark {
            icon: crate::icons::ZERON_LOGO,
            tint: None,
        },
        title: session.into(),
        session_id: Some(session.into()),
    }
}

fn sidebar_drop(session: &str, plan: crate::pane::hit_test::DropPlan) -> crate::pane::DragSplitState {
    crate::pane::DragSplitState {
        source: crate::pane::hit_test::DragSource::SidebarSession,
        session_id: Some(session.into()),
        root_bounds: gpui::Bounds {
            origin: gpui::point(gpui::px(0.0), gpui::px(0.0)),
            size: gpui::size(gpui::px(960.0), gpui::px(640.0)),
        },
        resolution: crate::pane::hit_test::DropResolution {
            plan,
            preview: None,
            anchor: None,
        },
    }
}

fn layout_counts(shell: &Shell) -> (usize, usize, usize) {
    (
        shell.workspace.layout.views.len(),
        shell
            .workspace
            .layout
            .views
            .values()
            .map(|view| view.ordered_tabs().len())
            .sum(),
        shell
            .workspace
            .layout
            .views
            .values()
            .flat_map(|view| view.tabs.values())
            .map(|tab| tab.panes.len())
            .sum(),
    )
}

/// Layout owner `a`, panes bound to `chat-a1` (space a) and `chat-b1`
/// (space b). Returns the two pane ids.
fn seed_mixed_layout(
    shell: &mut Shell,
    cx: &mut Context<Shell>,
) -> (zeron_workspace::PaneId, zeron_workspace::PaneId) {
    seed_two_spaces(shell, cx);
    shell.on_state_changed(&shell.state.clone(), cx);
    let pane_a = shell.workspace.focused_pane().unwrap();
    shell.split_drag = Some(sidebar_drop(
        "chat-b1",
        crate::pane::hit_test::DropPlan::SplitPane {
            pane: pane_a,
            direction: Direction::Right,
        },
    ));
    shell.commit_split_drop(&sidebar_payload("chat-b1"), cx);
    shell.on_state_changed(&shell.state.clone(), cx);
    let pane_b = shell.find_pane_with_session("chat-b1").unwrap();
    (pane_a, pane_b)
}

#[gpui::test]
fn a_foreign_space_sidebar_drop_docks_under_the_owning_space(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            let (pane_a, pane_b) = seed_mixed_layout(shell, cx);
            assert_ne!(pane_a, pane_b);
            assert_eq!(shell.active_workspace_space.as_deref(), Some("a"));
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("a"));
            assert_eq!(
                shell.state.read(cx).selected_chat.as_deref(),
                Some("chat-b1")
            );
            assert_eq!(
                shell.workspace.layout.pane(pane_a).unwrap().session_id.as_deref(),
                Some("chat-a1")
            );
            assert_eq!(
                shell.workspace.layout.pane(pane_b).unwrap().session_id.as_deref(),
                Some("chat-b1")
            );
        })
        .unwrap();
}

#[gpui::test]
fn focusing_between_mixed_space_panes_keeps_the_layout_owner(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            let (pane_a, pane_b) = seed_mixed_layout(shell, cx);
            shell.focus_workspace_pane(pane_a, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert_eq!(
                shell.state.read(cx).selected_chat.as_deref(),
                Some("chat-a1")
            );
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("a"));
            assert_eq!(shell.active_workspace_space.as_deref(), Some("a"));

            shell.focus_workspace_pane(pane_b, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            assert_eq!(
                shell.state.read(cx).selected_chat.as_deref(),
                Some("chat-b1")
            );
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("a"));
            assert_eq!(shell.active_workspace_space.as_deref(), Some("a"));
            assert_eq!(shell.find_pane_with_session("chat-a1"), Some(pane_a));
            assert_eq!(shell.find_pane_with_session("chat-b1"), Some(pane_b));
        })
        .unwrap();
}

#[gpui::test]
fn a_mixed_space_layout_round_trips_through_the_store(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            let (pane_a, pane_b) = seed_mixed_layout(shell, cx);
            shell.flush_workspace_layout(cx);
            select_space(shell, "b", cx);
            select_space(shell, "a", cx);
            // The restored tree kept BOTH bindings — including the
            // foreign-space one — instead of clearing it on load.
            assert_eq!(shell.find_pane_with_session("chat-a1"), Some(pane_a));
            assert_eq!(shell.find_pane_with_session("chat-b1"), Some(pane_b));
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("a"));
            assert_eq!(shell.active_workspace_space.as_deref(), Some("a"));
        })
        .unwrap();
}

#[gpui::test]
fn an_already_open_session_drop_only_focuses(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_two_spaces(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let pane_a = shell.workspace.focused_pane().unwrap();
            let before = layout_counts(shell);
            let revision = shell.workspace.layout.revision;
            shell.split_drag = Some(sidebar_drop(
                "chat-a1",
                crate::pane::hit_test::DropPlan::SplitPane {
                    pane: pane_a,
                    direction: Direction::Right,
                },
            ));
            shell.commit_split_drop(&sidebar_payload("chat-a1"), cx);
            assert_eq!(layout_counts(shell), before);
            assert_eq!(shell.workspace.layout.revision, revision);
            assert_eq!(shell.workspace.focused_pane(), Some(pane_a));
        })
        .unwrap();
}

#[gpui::test]
fn a_stale_drag_payload_never_mutates_the_layout(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_two_spaces(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let pane_a = shell.workspace.focused_pane().unwrap();
            let before = layout_counts(shell);
            let revision = shell.workspace.layout.revision;

            // Workspace commit entry: state for chat-a1, payload chat-b1.
            shell.split_drag = Some(sidebar_drop(
                "chat-a1",
                crate::pane::hit_test::DropPlan::SplitPane {
                    pane: pane_a,
                    direction: Direction::Right,
                },
            ));
            shell.commit_split_drop(&sidebar_payload("chat-b1"), cx);
            assert!(shell.split_drag.is_none());
            assert_eq!(layout_counts(shell), before);
            assert_eq!(shell.workspace.layout.revision, revision);
            assert!(shell.find_pane_with_session("chat-b1").is_none());

            // Sidebar (legacy single-pane) commit entry, same mismatch.
            shell.split_drag = Some(sidebar_drop(
                "chat-a1",
                crate::pane::hit_test::DropPlan::SplitPane {
                    pane: pane_a,
                    direction: Direction::Right,
                },
            ));
            shell.accept_sidebar_session_drop(&sidebar_payload("chat-b1"), cx);
            assert!(shell.split_drag.is_none());
            assert_eq!(layout_counts(shell), before);
            assert_eq!(shell.workspace.layout.revision, revision);
            assert!(shell.find_pane_with_session("chat-b1").is_none());
        })
        .unwrap();
}

#[gpui::test]
fn an_open_session_resolves_to_focus_only_inside_the_content(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_two_spaces(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let pane = shell.workspace.focused_pane().unwrap();
            let (view, tab) = shell.workspace.layout.pane_location(pane).unwrap();
            let outlet = crate::pane::hit_test::Rect::new(0.0, 0.0, 800.0, 600.0);
            let geometry = crate::pane::hit_test::single_pane_geometry(&outlet, pane, view, tab);

            // Captured sample left of the content (over the sidebar): the
            // existing-session path must decline just like `resolve_drop`.
            assert!(shell
                .existing_sidebar_session_resolution(
                    Some("chat-a1"),
                    &geometry,
                    geometry.content.x - 1.0,
                    geometry.content.y + geometry.content.h / 2.0,
                )
                .is_none());

            // Inside the content: focus the existing pane, full-pane preview.
            let (cx_mid, cy_mid) = geometry.panes[0].rect.center();
            let resolution = shell
                .existing_sidebar_session_resolution(Some("chat-a1"), &geometry, cx_mid, cy_mid)
                .unwrap();
            assert_eq!(
                resolution.plan,
                crate::pane::hit_test::DropPlan::FocusPane { pane }
            );
            assert_eq!(
                resolution.preview,
                Some(crate::pane::hit_test::DropPreview {
                    rect: geometry.panes[0].rect,
                    kind: crate::pane::hit_test::PreviewKind::FullTarget,
                })
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_sidebar_strip_drop_inserts_at_the_requested_position(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_two_spaces(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let view = zeron_workspace::ViewId(1);
            let t1 = shell.workspace.layout.views[&view].ordered_tabs()[0];
            let t2 = shell.workspace.add_tab_to_view(view).unwrap();
            assert_eq!(
                shell.workspace.layout.views[&view].ordered_tabs(),
                vec![t1, t2]
            );
            shell.split_drag = Some(sidebar_drop(
                "chat-b1",
                crate::pane::hit_test::DropPlan::MoveIntoPane {
                    view,
                    tab_before: Some(t1),
                },
            ));
            shell.commit_split_drop(&sidebar_payload("chat-b1"), cx);
            let ordered = shell.workspace.layout.views[&view].ordered_tabs();
            assert_eq!(ordered.len(), 3);
            assert_eq!(&ordered[1..], &[t1, t2]);
            let pane_b = shell.find_pane_with_session("chat-b1").unwrap();
            assert_eq!(
                shell.workspace.layout.pane_location(pane_b),
                Some((view, ordered[0]))
            );
            assert_eq!(shell.workspace.focused_pane(), Some(pane_b));
        })
        .unwrap();
}

#[gpui::test]
fn split_panes_drop_the_underlay_top_ramp_and_win_the_titlebar_band(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let window = cx.add_window(|_, cx| new_shell(dir.path(), cx));
    window
        .update(cx, |shell, _, cx| {
            seed_selected_project(shell, cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            // Legacy single-session route: the primary transcript slides
            // under the mounted pane header, so the shell underlay keeps its
            // top ramp and the window-drag strip stays the band's topmost
            // hitbox.
            assert_eq!(shell.active_chat, "chat-a");
            assert!(!shell.workspace_mode());
            assert!(shell.transcript_underlay_fades_top());
            assert!(!shell.pane_chrome_wins_titlebar_band());

            // A split flips both: pane chrome (header rows, tab strips) now
            // lives inside the outlet top — it must not sit in a zero-alpha
            // fade band (the "faded top bar, no title" bug) — and it must
            // win the titlebar band's clicks over the drag strip.
            shell.split_workspace_view(Direction::Right, cx);
            assert!(shell.workspace_mode());
            assert!(!shell.transcript_underlay_fades_top());
            assert!(shell.pane_chrome_wins_titlebar_band());

            // Collapsing back to one pane returns the glass single-session
            // route (issue #8): the pane-surface cache is a draft-preserving
            // inventory, not a workspace-route latch, so the underlay top
            // ramp and window-drag strip come back with it.
            let second = shell.workspace.focused_pane().unwrap();
            shell.close_workspace_pane(second, cx);
            assert!(shell.workspace.is_trivial());
            assert!(!shell.workspace_mode());
            assert!(shell.transcript_underlay_fades_top());
            assert!(!shell.pane_chrome_wins_titlebar_band());
        })
        .unwrap();
}

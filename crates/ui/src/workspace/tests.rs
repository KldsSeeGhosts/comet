use super::*;
use gpui::{AppContext, TestAppContext};

fn setup(cx: &mut TestAppContext, directory: &std::path::Path) {
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::settings::init(crate::settings::UiSettings::default(), directory, cx);
        crate::history::init(Default::default(), Default::default(), Default::default(), Default::default(), cx);
        crate::composer::init(cx, Default::default());
    });
}

#[gpui::test]
fn panes_keep_independent_drafts_and_selection(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let window = cx.add_window(|_, cx| {
        let source = cx.new(|_| {
            let mut state = AppState::new();
            state.data_dir = Some(directory.path().into());
            state
        });
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    window.update(cx, |workspace, _, cx| {
        let first = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(first, cx);
        let first_chat = workspace.panes[&first].chat.clone();
        first_chat.read(cx).composer.clone().update(cx, |composer, cx| composer.load_text("first draft".into(), cx));
        workspace.split(Direction::Right, false, cx);
        let second = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(second, cx);
        let second_chat = workspace.panes[&second].chat.clone();
        second_chat.read(cx).composer.clone().update(cx, |composer, cx| composer.load_text("second draft".into(), cx));
        assert_ne!(first_chat.read(cx).state.entity_id(), second_chat.read(cx).state.entity_id());
        assert_eq!(first_chat.read(cx).composer.read(cx).input.read(cx).text(), "first draft");
        assert_eq!(second_chat.read(cx).composer.read(cx).input.read(cx).text(), "second draft");
        workspace.focus(first, cx);
        assert_eq!(workspace.layout.active_pane_id(), Some(first));
        assert_eq!(second_chat.read(cx).composer.read(cx).input.read(cx).text(), "second draft");
        workspace.flush().unwrap();
        assert_eq!(WorkspaceLayout::load(directory.path().join("layout.json")).unwrap(), workspace.layout);
    }).unwrap();
}

#[gpui::test]
fn moving_and_closing_panes_preserves_sibling_entities(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let window = cx.add_window(|_, cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    window.update(cx, |workspace, _, cx| {
        let first = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(first, cx);
        let identity = workspace.panes[&first].chat.entity_id();
        workspace.split(Direction::Right, true, cx);
        let second = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(second, cx);
        workspace.apply(|layout| layout.move_pane(second, first, Direction::Down), cx);
        assert_eq!(workspace.layout.views.len(), 1);
        assert_eq!(workspace.panes[&first].chat.entity_id(), identity);
        workspace.close(second, cx);
        assert_eq!(workspace.layout.active_pane_id(), Some(first));
        assert_eq!(workspace.panes[&first].chat.entity_id(), identity);
        workspace.close(first, cx);
        assert!(workspace.error.is_none());
        assert!(workspace.layout.pane(first).is_none());
        let launcher = workspace.layout.active_pane_id().unwrap();
        assert_eq!(workspace.layout.pane(launcher).unwrap(), &PaneState::default());
    }).unwrap();
}

#[test]
fn layout_files_are_account_scoped() {
    let mut state = AppState::new();
    let directory = std::path::Path::new("/tmp/noches-layout-test");
    let local = layout_path(&state, directory);
    state.workspace_scope = Some(zeron_proto::WorkspaceScope::Synced);
    state.auth = Some(zeron_proto::AuthState::SignedIn {
        user: zeron_proto::UserProfile { id: "user-a".into(), email: String::new(), name: None },
        org_id: Some("org-a".into()),
    });
    let first = layout_path(&state, directory);
    state.auth = Some(zeron_proto::AuthState::SignedIn {
        user: zeron_proto::UserProfile { id: "user-b".into(), email: String::new(), name: None },
        org_id: Some("org-a".into()),
    });
    let second = layout_path(&state, directory);
    assert_ne!(local, first);
    assert_ne!(first, second);
    assert_eq!(first.parent(), Some(directory));
}

#[gpui::test]
fn closing_parked_cli_requires_daemon_acknowledgement(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        let mut workspace = Workspace::new(source, directory.path().join("layout.json"), cx);
        let id = workspace.layout.active_pane_id().unwrap();
        let pane = workspace.layout.pane_mut(id).unwrap();
        pane.session_id = Some("parked-pi".into());
        pane.mode = PaneMode::Terminal;
        workspace
    });
    workspace.update(cx, |workspace, cx| {
        let id = workspace.layout.active_pane_id().unwrap();
        assert!(!workspace.panes.contains_key(&id));
        workspace.close(id, cx);
        assert!(workspace.layout.pane(id).is_some());
        let terminal = workspace.panes[&id].terminal.as_ref().unwrap();
        assert!(matches!(terminal.read(cx).session_view_status(), SessionViewStatus::Failed(error) if error == "Engine is not connected"));
    });
    cx.run_until_parked();
    workspace.update(cx, |workspace, _| {
        assert!(workspace.pending_close.is_empty());
        assert_eq!(workspace.layout.pane(workspace.layout.active_pane_id().unwrap()).unwrap().mode, PaneMode::Terminal);
        assert_eq!(workspace.error.as_deref(), Some("Engine is not connected"));
    });
}

#[gpui::test]
fn failed_open_cannot_be_rebound_by_sidebar_selection(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    workspace.update(cx, |workspace, cx| {
        let original = workspace.layout.active_pane_id().unwrap();
        workspace.layout.pane_mut(original).unwrap().session_id = Some("pi-original".into());
        workspace.ensure_pane(original, cx);
        workspace.open_terminal(original, cx);
        assert!(matches!(workspace.panes[&original].terminal.as_ref().unwrap().read(cx).session_view_status(), SessionViewStatus::Failed(_)));
        assert_eq!(workspace.layout.pane(original).unwrap().mode, PaneMode::Chat);
        workspace.select_session(Some("different-chat".into()), cx);
        assert_eq!(workspace.layout.pane(original).unwrap().session_id.as_deref(), Some("pi-original"));
        assert_ne!(workspace.layout.active_pane_id(), Some(original));
        assert_eq!(workspace.layout.pane(workspace.layout.active_pane_id().unwrap()).unwrap().session_id.as_deref(), Some("different-chat"));
    });
}

#[gpui::test]
fn pane_header_center_drop_creates_tab_without_losing_draft(cx: &mut TestAppContext) {
    use gpui::{Modifiers, point, size};
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let (workspace, cx) = cx.add_window_view(|_, cx| {
        let source = cx.new(|_| AppState::new());
        let mut workspace = Workspace::new(source, directory.path().join("layout.json"), cx);
        workspace.split(Direction::Right, false, cx);
        workspace
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    cx.run_until_parked();
    let origin = cx.debug_bounds("pane-drag-3").expect("first pane drag header").center();
    let destination = cx.debug_bounds("pane-4").expect("second pane").center();
    workspace.update(cx, |workspace, cx| {
        workspace.panes[&PaneId(3)].chat.read(cx).composer.clone()
            .update(cx, |composer, cx| composer.load_text("preserve draft".into(), cx));
    });
    cx.simulate_mouse_down(origin, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(origin + point(px(15.0), px(0.0)), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(destination, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(destination, MouseButton::Left, Modifiers::none());
    workspace.update(cx, |workspace, cx| {
        assert!(workspace.error.is_none(), "{:?}", workspace.error);
        assert_eq!(workspace.layout.views[&ViewId(1)].tabs.len(), 2);
        assert_eq!(workspace.layout.active_pane_id(), Some(PaneId(3)));
        assert_eq!(workspace.panes[&PaneId(3)].chat.read(cx).composer.read(cx).input.read(cx).text(), "preserve draft");
    });
}

#[test]
fn outside_ring_does_not_consume_inner_pane_targets() {
    let bounds = Bounds::new(gpui::point(px(100.0), px(50.0)), gpui::size(px(800.0), px(600.0)));
    assert_eq!(outside_ring(gpui::point(px(105.0), px(300.0)), bounds), Some(Direction::Left));
    assert_eq!(outside_ring(gpui::point(px(190.0), px(300.0)), bounds), None);
    assert_eq!(outside_ring(gpui::point(px(500.0), px(645.0)), bounds), Some(Direction::Down));
    assert_eq!(outside_ring(gpui::point(px(90.0), px(300.0)), bounds), None);
}

#[gpui::test]
fn divider_resizes_resets_and_outer_drop_moves_a_full_view(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseDownEvent, point, size};
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let (workspace, cx) = cx.add_window_view(|_, cx| {
        let source = cx.new(|_| AppState::new());
        let mut workspace = Workspace::new(source, directory.path().join("layout.json"), cx);
        workspace.split(Direction::Right, false, cx);
        workspace
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    let divider = "split-Some((ViewId(1), TabId(2)))-[]";
    let start = cx.debug_bounds(divider).unwrap().center();
    let end = point(px(700.0), start.y);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(start + point(px(20.0), px(0.0)), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    workspace.update(cx, |workspace, _| {
        let SplitNode::Split { ratio, .. } = &workspace.layout.views[&ViewId(1)].tabs[&TabId(2)].root else { panic!("split missing") };
        assert!((0.68..0.72).contains(ratio), "ratio {ratio}");
    });
    let reset = cx.debug_bounds(divider).unwrap().center();
    cx.simulate_event(MouseDownEvent { position: reset, button: MouseButton::Left,
        modifiers: Modifiers::none(), click_count: 2, first_mouse: false });
    cx.simulate_mouse_up(reset, MouseButton::Left, Modifiers::none());
    workspace.update(cx, |workspace, _| {
        let SplitNode::Split { ratio, .. } = &workspace.layout.views[&ViewId(1)].tabs[&TabId(2)].root else { panic!("split missing") };
        assert_eq!(*ratio, 0.5);
    });
    let header = cx.debug_bounds("pane-drag-3").unwrap().center();
    let outer_edge = point(px(995.0), px(350.0));
    cx.simulate_mouse_down(header, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(header + point(px(20.0), px(0.0)), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(outer_edge, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(outer_edge, MouseButton::Left, Modifiers::none());
    workspace.update(cx, |workspace, _| {
        assert!(workspace.error.is_none(), "{:?}", workspace.error);
        assert_eq!(workspace.layout.views.len(), 2);
        assert_ne!(workspace.layout.pane_location(PaneId(3)).unwrap().0, ViewId(1));
        assert_eq!(workspace.layout.pane_location(PaneId(4)).unwrap().0, ViewId(1));
        workspace.layout.validate().unwrap();
    });
}

#[gpui::test]
fn tab_drag_inserts_after_target_and_preserves_nested_panes(cx: &mut TestAppContext) {
    use gpui::{Modifiers, point, size};
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let (workspace, cx) = cx.add_window_view(|_, cx| {
        let source = cx.new(|_| AppState::new());
        let mut workspace = Workspace::new(source, directory.path().join("layout.json"), cx);
        workspace.split(Direction::Right, false, cx);
        workspace.new_tab(cx);
        workspace
    });
    cx.simulate_resize(size(px(1000.0), px(700.0)));
    cx.run_until_parked();
    let (first, second, nested) = workspace.update(cx, |workspace, _| {
        let view = &workspace.layout.views[&ViewId(1)];
        let tabs = view.ordered_tabs();
        (tabs[0], tabs[1], view.tabs[&tabs[0]].root.clone())
    });
    let origin = cx.debug_bounds("workspace-tab-2").unwrap().center();
    let target = cx.debug_bounds("workspace-tab-5").unwrap();
    let destination = point(target.origin.x + target.size.width * 0.65, target.center().y);
    cx.simulate_mouse_down(origin, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(origin + point(px(15.0), px(0.0)), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(destination, MouseButton::Left, Modifiers::none());
    workspace.update(cx, |workspace, _| assert_eq!(workspace.tab_preview, Some((ViewId(1), None))));
    cx.simulate_mouse_up(destination, MouseButton::Left, Modifiers::none());
    workspace.update(cx, |workspace, _| {
        assert!(workspace.error.is_none(), "{:?}", workspace.error);
        let view = &workspace.layout.views[&ViewId(1)];
        assert_eq!(view.ordered_tabs(), vec![second, first]);
        assert_eq!(view.tabs[&first].root, nested);
        assert_eq!(workspace.layout.views.len(), 1, "tab rail must not create an outer split");
    });
}

#[test]
fn tab_indicator_priority_and_terminal_mapping() {
    assert_eq!(
        tab_indicator([
            Some(ChatIndicator::Idle),
            Some(ChatIndicator::Completed),
            Some(ChatIndicator::Working),
            Some(ChatIndicator::Errored),
            Some(ChatIndicator::AwaitingInput),
        ]),
        Some(ChatIndicator::AwaitingInput)
    );

    assert_eq!(
        tab_indicator([
            Some(ChatIndicator::Idle),
            Some(ChatIndicator::Completed),
            Some(ChatIndicator::Errored),
        ]),
        Some(ChatIndicator::Errored)
    );

    assert_eq!(
        tab_indicator([
            Some(ChatIndicator::Idle),
            Some(ChatIndicator::Completed),
            Some(ChatIndicator::Working),
        ]),
        Some(ChatIndicator::Working)
    );

    assert_eq!(
        tab_indicator([
            Some(ChatIndicator::Idle),
            Some(ChatIndicator::Completed),
        ]),
        Some(ChatIndicator::Completed)
    );

    assert_eq!(
        tab_indicator([Some(ChatIndicator::Idle)]),
        Some(ChatIndicator::Idle)
    );

    assert_eq!(tab_indicator([None, None]), None);
    assert_eq!(tab_indicator(Vec::<Option<ChatIndicator>>::new()), None);

    assert_eq!(
        pane_indicator(Some(NativeActivity::Busy), Some(ChatIndicator::Idle)),
        Some(ChatIndicator::Working)
    );
    assert_eq!(
        pane_indicator(Some(NativeActivity::Permission), Some(ChatIndicator::Completed)),
        Some(ChatIndicator::AwaitingInput)
    );
    assert_eq!(
        pane_indicator(Some(NativeActivity::Idle), Some(ChatIndicator::Completed)),
        Some(ChatIndicator::Idle)
    );
    assert_eq!(
        pane_indicator(Some(NativeActivity::Ended), Some(ChatIndicator::Completed)),
        Some(ChatIndicator::Completed)
    );
    assert_eq!(
        pane_indicator(Some(NativeActivity::Unknown), Some(ChatIndicator::Completed)),
        Some(ChatIndicator::Completed)
    );
    assert_eq!(
        pane_indicator(None, Some(ChatIndicator::Working)),
        Some(ChatIndicator::Working)
    );
    assert_eq!(pane_indicator(None, None), None);
}

#[test]
fn tab_cycling_wraps_at_boundaries() {
    let t1 = TabId(1);
    let t2 = TabId(2);
    let t3 = TabId(3);
    let tabs = vec![t1, t2, t3];

    assert_eq!(cycle_tab(&tabs, Some(t1), true), Some(t2));
    assert_eq!(cycle_tab(&tabs, Some(t2), true), Some(t3));
    assert_eq!(cycle_tab(&tabs, Some(t3), true), Some(t1));

    assert_eq!(cycle_tab(&tabs, Some(t1), false), Some(t3));
    assert_eq!(cycle_tab(&tabs, Some(t2), false), Some(t1));
    assert_eq!(cycle_tab(&tabs, Some(t3), false), Some(t2));

    assert_eq!(cycle_tab(&tabs, None, true), Some(t1));
    assert_eq!(cycle_tab(&tabs, None, false), Some(t3));

    assert_eq!(cycle_tab(&tabs, Some(TabId(99)), true), Some(t1));
    assert_eq!(cycle_tab(&tabs, Some(TabId(99)), false), Some(t3));

    assert_eq!(cycle_tab(&[], None, true), None);
}

#[test]
fn close_confirmation_gates_on_running_cli_and_setting() {
    let idle_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Ready),
        terminal_activity: Some(NativeActivity::Idle),
    };
    let busy_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Ready),
        terminal_activity: Some(NativeActivity::Busy),
    };
    let permission_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Ready),
        terminal_activity: Some(NativeActivity::Permission),
    };
    let opening_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Opening),
        terminal_activity: None,
    };
    let closing_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Closing),
        terminal_activity: None,
    };
    let ready_unverified_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Ready),
        terminal_activity: None,
    };
    let ready_unknown_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Ready),
        terminal_activity: Some(NativeActivity::Unknown),
    };
    let starting = PaneCloseRunningState {
        starting_cli: true,
        terminal_status: None,
        terminal_activity: None,
    };
    let plain_chat = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: None,
        terminal_activity: None,
    };
    let ended_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Ready),
        terminal_activity: Some(NativeActivity::Ended),
    };
    let idle_status_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Idle),
        terminal_activity: None,
    };
    let failed_cli = PaneCloseRunningState {
        starting_cli: false,
        terminal_status: Some(SessionViewStatus::Failed("failed".into())),
        terminal_activity: None,
    };

    // Setting false: never confirms even if busy or opening
    assert!(!needs_close_confirmation(false, [busy_cli.clone()]));
    assert!(!needs_close_confirmation(false, [opening_cli.clone()]));
    assert!(!needs_close_confirmation(false, [closing_cli.clone()]));
    assert!(!needs_close_confirmation(false, [starting.clone()]));

    // Setting true:
    // Busy CLI requires confirm
    assert!(needs_close_confirmation(true, [busy_cli.clone()]));
    // Permission CLI requires confirm
    assert!(needs_close_confirmation(true, [permission_cli]));
    // Opening / Closing CLI states require confirm
    assert!(needs_close_confirmation(true, [opening_cli.clone()]));
    assert!(needs_close_confirmation(true, [closing_cli]));
    // Ready with no verified idle activity requires confirm
    assert!(needs_close_confirmation(true, [ready_unverified_cli]));
    assert!(needs_close_confirmation(true, [ready_unknown_cli]));
    // In-flight starting_cli requires confirm
    assert!(needs_close_confirmation(true, [starting.clone()]));

    // Idle CLI does NOT require confirm
    assert!(!needs_close_confirmation(true, [idle_cli.clone()]));
    assert!(!needs_close_confirmation(true, [idle_status_cli]));
    // Plain chat does NOT require confirm
    assert!(!needs_close_confirmation(true, [plain_chat.clone()]));
    // Ended CLI does NOT require confirm
    assert!(!needs_close_confirmation(true, [ended_cli]));
    // Failed CLI does NOT require confirm
    assert!(!needs_close_confirmation(true, [failed_cli]));

    // Multi-pane tab: if any pane is running, confirm is required
    assert!(needs_close_confirmation(true, [idle_cli.clone(), busy_cli]));
    assert!(needs_close_confirmation(true, [plain_chat.clone(), opening_cli]));
    assert!(needs_close_confirmation(true, [plain_chat.clone(), starting]));
    assert!(!needs_close_confirmation(true, [idle_cli, plain_chat]));
}

#[gpui::test]
fn closing_pane_while_starting_cli_downgrades_to_silent_noop(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    workspace.update(cx, |workspace, cx| {
        let id = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(id, cx);
        let dummy_task = gpui::Task::ready(());
        workspace.starting_cli.insert(id, dummy_task);
        assert!(workspace.pane_close_running_state(id, cx).starting_cli);
        workspace.close(id, cx);
        assert!(workspace.layout.pane(id).is_none());
        assert!(workspace.error.is_none());
    });
}

#[gpui::test]
fn request_close_tab_with_running_cli_shows_confirm_banner(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    workspace.update(cx, |workspace, cx| {
        let active = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(active, cx);
        workspace.starting_cli.insert(active, gpui::Task::ready(()));

        let view_id = workspace.layout.active_view_id;
        let tab_id = workspace.layout.views[&view_id].active_tab_id;

        workspace.request_close_tab(view_id, tab_id, cx);
        assert!(workspace.pending_close_confirm.is_some());
        assert!(workspace.layout.pane(active).is_some());

        workspace.pending_close_confirm = None;
        assert!(workspace.layout.pane(active).is_some());

        workspace.starting_cli.remove(&active);
        workspace.request_close_tab(view_id, tab_id, cx);
        assert!(workspace.pending_close_confirm.is_none());
        assert!(workspace.layout.pane(active).is_none());
    });
}

#[gpui::test]
fn mixed_tab_close_preserves_layout_integrity_and_awaits_cli_ack(cx: &mut TestAppContext) {
    use futures::FutureExt;
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    let (second, terminal) = workspace.update(cx, |workspace, cx| {
        let first = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(first, cx);

        workspace.split(Direction::Right, false, cx);
        let second = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(second, cx);

        let view_id = workspace.layout.active_view_id;
        let tab_id = workspace.layout.views[&view_id].active_tab_id;
        assert_eq!(workspace.layout.views[&view_id].tabs[&tab_id].panes.len(), 2);

        workspace.layout.pane_mut(second).unwrap().mode = PaneMode::Terminal;
        let terminal = workspace.prepare_terminal(second, cx).unwrap();
        let (_sender, receiver) = futures::channel::oneshot::channel::<()>();
        terminal.update(cx, |t, cx| {
            t.set_session_status(SessionViewStatus::Opening, cx);
            t.set_session_open(Some(cx.spawn(async move |_, _| {
                let _ = receiver.await;
                Ok(())
            }).shared()));
        });

        let state = workspace.pane_close_running_state(second, cx);
        assert!(state.is_running());
        assert_eq!(state.terminal_status, Some(SessionViewStatus::Opening));

        workspace.request_close_tab(view_id, tab_id, cx);
        assert!(workspace.pending_close_confirm.is_some());
        assert_eq!(workspace.layout.views[&view_id].tabs[&tab_id].panes.len(), 2);

        let pending = workspace.pending_close_confirm.take().unwrap();
        assert_eq!(pending.panes.len(), 2);
        for pane in pending.panes {
            workspace.close(pane, cx);
        }

        assert!(workspace.layout.pane(first).is_none());
        assert!(!workspace.panes.contains_key(&first));
        assert!(workspace.pending_close.contains(&second));
        assert!(workspace.layout.pane(second).is_some());
        assert!(workspace.panes.contains_key(&second));

        assert!(workspace.layout.validate().is_ok());

        workspace.request_close_tab(view_id, tab_id, cx);
        assert!(workspace.pending_close_confirm.is_none());

        drop(_sender);
        (second, terminal)
    });
    terminal.update(cx, |t, cx| {
        t.set_session_status(SessionViewStatus::Idle, cx);
    });
    cx.run_until_parked();
    workspace.update(cx, |workspace, _| {
        assert!(workspace.pending_close.is_empty());
        assert!(workspace.layout.pane(second).is_none());
        assert!(!workspace.panes.contains_key(&second));
        assert!(workspace.layout.validate().is_ok());

        let launcher = workspace.layout.active_pane_id().unwrap();
        assert_eq!(workspace.layout.pane(launcher).unwrap(), &PaneState::default());
    });
}

#[test]
fn create_chat_payload_shape() {
    let config = zeron_proto::ChatConfig {
        harness: zeron_proto::HarnessId::Pi,
        model: Some("model-1".into()),
        reasoning: None,
        model_options: Default::default(),
        sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
    };

    let p1 = create_chat_payload("c1", Some("s1"), Some("d1"), "/path", Some("feat"), Some(&config));
    assert_eq!(p1["op"], "createChat");
    assert_eq!(p1["chatId"], "c1");
    assert_eq!(p1["spaceId"], "s1");
    assert!(p1.get("deviceId").is_none());
    assert_eq!(p1["cwd"], "/path");
    assert_eq!(p1["branch"], "feat");
    assert_eq!(p1["config"]["harness"], "pi");
    assert_eq!(p1["config"]["model"], "model-1");

    let p2 = create_chat_payload("c2", None, Some("d2"), "~", None, None);
    assert_eq!(p2["op"], "createChat");
    assert_eq!(p2["chatId"], "c2");
    assert!(p2.get("spaceId").is_none());
    assert_eq!(p2["deviceId"], "d2");
    assert_eq!(p2["cwd"], "~");
    assert!(p2.get("branch").is_none());
    assert!(p2.get("config").is_none());
}

#[gpui::test]
fn new_tab_opens_an_empty_draft_without_minting(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    workspace.update(cx, |workspace, cx| {
        let initial_pane = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(initial_pane, cx);

        workspace.new_tab(cx);
        let new_pane = workspace.layout.active_pane_id().unwrap();
        assert_ne!(new_pane, initial_pane);

        // The draft canvas never mints a chat row on its own — the session
        // is created on first send (or the CLI view's explicit mint).
        assert!(workspace.error.is_none());
        assert_eq!(workspace.layout.pane(new_pane).unwrap().session_id, None);
    });
}

fn drag(chat_id: &str) -> SidebarSessionDrag {
    SidebarSessionDrag { chat_id: chat_id.into(), title: chat_id.into() }
}

#[gpui::test]
fn sidebar_session_drops_split_open_and_focus(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    workspace.update(cx, |workspace, cx| {
        let first = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(first, cx);
        workspace.layout.pane_mut(first).unwrap().session_id = Some("chat-a".into());

        // Edge drop: a new pane splits in beside the target and takes focus.
        workspace.drop_session(&drag("chat-b"), SessionDrop { pane: first, zone: Some(Direction::Right), open_in: None }, cx);
        let split = workspace.layout.active_pane_id().unwrap();
        assert_ne!(split, first);
        assert_eq!(workspace.layout.pane(split).unwrap().session_id.as_deref(), Some("chat-b"));
        assert!(workspace.open_session_ids().contains("chat-b"));

        // Center drop: the session opens in the pane under the pointer.
        workspace.drop_session(&drag("chat-c"), SessionDrop { pane: split, zone: None, open_in: None }, cx);
        assert_eq!(workspace.layout.pane(split).unwrap().session_id.as_deref(), Some("chat-c"));
        assert_eq!(workspace.layout.active_pane_id(), Some(split));

        // Already open: the drop focuses its pane instead of duplicating it.
        workspace.focus(first, cx);
        workspace.drop_session(
            &drag("chat-c"),
            SessionDrop { pane: first, zone: Some(Direction::Down), open_in: workspace.find_pane_by_session("chat-c") },
            cx,
        );
        assert_eq!(workspace.layout.active_pane_id(), Some(split));
        let pane_count: usize = workspace.layout.views.values()
            .flat_map(|view| view.tabs.values()).map(|tab| tab.panes.len()).sum();
        assert_eq!(pane_count, 2, "focusing an open session must not add panes");

        assert!(workspace.error.is_none(), "{:?}", workspace.error);
        workspace.layout.validate().unwrap();
    });
}

#[gpui::test]
fn open_in_split_reuses_the_pane_already_showing_the_session(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    setup(cx, directory.path());
    let workspace = cx.new(|cx| {
        let source = cx.new(|_| AppState::new());
        Workspace::new(source, directory.path().join("layout.json"), cx)
    });
    workspace.update(cx, |workspace, cx| {
        let first = workspace.layout.active_pane_id().unwrap();
        workspace.ensure_pane(first, cx);

        workspace.open_session_in_split("chat-a", cx);
        let split = workspace.layout.active_pane_id().unwrap();
        assert_ne!(split, first);
        assert_eq!(workspace.layout.pane(split).unwrap().session_id.as_deref(), Some("chat-a"));

        // A second open-in-split for the same session focuses, never duplicates.
        workspace.focus(first, cx);
        workspace.open_session_in_split("chat-a", cx);
        assert_eq!(workspace.layout.active_pane_id(), Some(split));
        let pane_count: usize = workspace.layout.views.values()
            .flat_map(|view| view.tabs.values()).map(|tab| tab.panes.len()).sum();
        assert_eq!(pane_count, 2);

        // The tab rail's open: a new tab in the pane's own view.
        workspace.open_session_tab(workspace.layout.active_view_id, "chat-b", cx);
        let view = workspace.layout.active_view_id;
        assert_eq!(workspace.layout.views[&view].tabs.len(), 2);
        let active = workspace.layout.active_pane_id().unwrap();
        assert_eq!(workspace.layout.pane(active).unwrap().session_id.as_deref(), Some("chat-b"));

        assert!(workspace.error.is_none(), "{:?}", workspace.error);
        workspace.layout.validate().unwrap();
    });
}

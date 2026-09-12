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

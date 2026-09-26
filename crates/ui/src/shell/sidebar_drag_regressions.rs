// Included in shell::workspace_persistence to exercise the real Shell event tree.

#[gpui::test]
fn sidebar_pointer_drag_splits_without_starting_gpui_active_drag(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let (shell, cx) = cx.add_window_view(|_, cx| new_shell(dir.path(), cx));
    shell.update(cx, |shell, cx| {
        seed_selected_project(shell, cx);
        shell.debug_gate = Some(GatePhase::Ready);
        shell.on_state_changed(&shell.state.clone(), cx);
    });
    cx.update(|window, cx| window.draw(cx).clear());
    // The sidebar is a cached subview, so GPUI does not expose its child
    // debug_bounds through the parent frame. Arm the row's pointer state
    // directly, then dispatch real window-level moves and release.
    let start = gpui::point(px(120.0), px(200.0));
    let outlet = shell.read_with(cx, |shell, _| shell.sidebar_drop_outlet.get().unwrap());
    let target = gpui::point(outlet.origin.x + px(40.0), outlet.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
    shell.update(cx, |shell, _| {
        shell.sidebar_session_pointer = Some(SidebarSessionPointer {
            session_id: "chat-b".into(),
            origin: start,
            dragging: false,
        });
    });
    cx.simulate_mouse_move(
        start + gpui::point(px(8.0), px(0.0)),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_move(target, Some(MouseButton::Left), gpui::Modifiers::default());
    cx.update(|_, cx| {
        assert!(
            !cx.has_active_drag(),
            "sidebar drag must not use GPUI's full-window repaint path"
        )
    });
    shell.read_with(cx, |shell, _| {
        assert!(shell.sidebar_session_pointer.as_ref().unwrap().dragging);
        assert_ne!(
            shell.split_drag.as_ref().unwrap().resolution.plan,
            crate::pane::hit_test::DropPlan::None
        );
    });
    cx.simulate_mouse_up(target, MouseButton::Left, gpui::Modifiers::default());
    shell.read_with(cx, |shell, cx| {
        assert!(shell.sidebar_session_pointer.is_none());
        assert!(shell.split_drag.is_none());
        assert!(shell.workspace_mode());
        assert!(shell.find_pane_with_session("chat-b").is_some());
        assert_eq!(
            shell.state.read(cx).selected_chat.as_deref(),
            Some("chat-b")
        );
        shell.workspace.layout.validate().unwrap();
    });
}

#[gpui::test]
fn sidebar_pointer_release_without_target_suppresses_click(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| init_app(dir.path(), cx));
    let (shell, cx) = cx.add_window_view(|_, cx| new_shell(dir.path(), cx));
    shell.update(cx, |shell, cx| {
        seed_selected_project(shell, cx);
        shell.debug_gate = Some(GatePhase::Ready);
        shell.on_state_changed(&shell.state.clone(), cx);
    });
    cx.update(|window, cx| window.draw(cx).clear());
    let start = gpui::point(px(120.0), px(200.0));
    cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
    shell.update(cx, |shell, _| {
        shell.sidebar_session_pointer = Some(SidebarSessionPointer {
            session_id: "chat-b".into(),
            origin: start,
            dragging: false,
        });
    });
    cx.simulate_mouse_move(
        start + gpui::point(px(8.0), px(0.0)),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_up(start, MouseButton::Left, gpui::Modifiers::default());
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.state.read(cx).selected_chat.as_deref(),
            Some("chat-a")
        );
        assert!(shell.workspace.is_trivial());
        assert!(shell.sidebar_drag_suppressed_click);
        assert!(shell.sidebar_session_pointer.is_none());
        assert!(shell.split_drag.is_none());
    });
}

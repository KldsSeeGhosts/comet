use serde_json::{Value, json};
use zeron_workspace::*;

fn state(label: &str) -> PaneState {
    PaneState {
        session_id: Some(format!("session-{label}")),
        mode: PaneMode::Terminal,
        label: Some(label.into()),
        group: Some("work".into()),
        ..PaneState::default()
    }
}

#[test]
fn nested_resize_and_close_promote_exact_sibling() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let first = layout.active_pane_id().unwrap();
    let (view, tab) = layout.pane_location(first).unwrap();
    let second = layout.split_pane(first, Direction::Right, state("second"))?;
    let third = layout.split_pane(second, Direction::Down, state("third"))?;
    layout.set_pane_ratio(view, tab, &[], 0.6)?;
    layout.set_pane_ratio(view, tab, &[Branch::Second], 0.3)?;
    let before = layout.clone();
    for ratio in [0.09, 0.91, f64::NAN, f64::INFINITY] {
        assert!(layout.set_pane_ratio(view, tab, &[], ratio).is_err());
        assert_eq!(layout, before);
    }
    assert!(
        layout
            .set_pane_ratio(view, tab, &[Branch::First], 0.4)
            .is_err()
    );
    let sibling = match &layout.views[&view].tabs[&tab].root {
        SplitNode::Split { second, .. } => second.as_ref().clone(),
        _ => panic!("expected split"),
    };
    layout.close_pane(first)?;
    let tree = &layout.views[&view].tabs[&tab];
    assert_eq!(tree.root, sibling);
    assert_eq!(tree.primary_pane_id, second);
    assert_eq!(tree.active_pane_id, third);
    layout.close_pane(third)?;
    assert_eq!(layout.active_pane_id(), Some(second));
    let before = layout.clone();
    assert!(layout.close_pane(second).is_err());
    assert!(layout.close_tab(view, tab).is_err());
    assert!(layout.close_view(view).is_err());
    assert_eq!(layout, before);
    layout.validate()
}

#[test]
fn swap_panes_exchanges_leaves_in_same_tab() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let first = layout.active_pane_id().unwrap();
    let second = layout.split_pane(first, Direction::Right, state("second"))?;
    // Swap: first and second exchange positions in the tree.
    layout.swap_panes(first, second)?;
    // Same tab, same pane count — only the leaf contents moved.
    let (view, tab) = layout.pane_location(first).unwrap();
    assert_eq!(layout.pane_location(second), Some((view, tab)));
    assert_eq!(layout.views[&view].tabs[&tab].panes.len(), 2);
    // The tree structure is preserved: still a horizontal split.
    assert!(matches!(
        &layout.views[&view].tabs[&tab].root,
        SplitNode::Split {
            horizontal: true,
            ..
        }
    ));
    // Self-swap is rejected.
    assert!(layout.swap_panes(first, first).is_err());
    layout.validate()
}

#[test]
fn nested_views_close_and_focus() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let first = layout.active_view_id;
    let second = layout.split_view(first, Direction::Right, state("second"))?;
    let third = layout.split_view(second, Direction::Down, state("third"))?;
    layout.set_view_ratio(&[], 0.7)?;
    layout.set_view_ratio(&[Branch::Second], 0.2)?;
    layout.focus_view(first)?;
    layout.close_view(first)?;
    assert_eq!(layout.active_view_id, second);
    assert!(matches!(
        layout.root,
        SplitNode::Split {
            horizontal: false,
            ratio: 0.2,
            ..
        }
    ));
    layout.close_view(second)?;
    assert_eq!(layout.root, SplitNode::leaf(third));
    layout.validate()
}

#[test]
fn move_preserves_identity_and_state_and_removes_empty_containers() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let first = layout.active_pane_id().unwrap();
    let original_view = layout.active_view_id;
    let moving = layout.split_pane(first, Direction::Right, state("moving"))?;
    let third = layout.split_pane(moving, Direction::Down, state("third"))?;
    let next_id = layout.next_id;
    layout.move_pane(moving, first, Direction::Left)?;
    assert_eq!(layout.next_id, next_id);
    assert_eq!(layout.pane(moving), Some(&state("moving")));
    assert_eq!(layout.active_pane_id(), Some(moving));
    let new_tab = layout.add_tab(original_view, state("target"))?;
    let target = layout.active_pane_id().unwrap();
    layout.move_pane(moving, target, Direction::Up)?;
    assert_eq!(layout.pane_location(moving), Some((original_view, new_tab)));
    layout.close_pane(third)?;
    layout.move_pane(first, target, Direction::Down)?;
    assert_eq!(layout.views[&original_view].tabs.len(), 1);
    let new_view = layout.split_view(original_view, Direction::Left, state("new view"))?;
    let last_source = layout.active_pane_id().unwrap();
    layout.move_pane(last_source, moving, Direction::Right)?;
    assert!(!layout.views.contains_key(&new_view));
    assert_eq!(layout.views.len(), 1);
    assert_eq!(
        layout.pane_location(last_source),
        Some((original_view, new_tab))
    );
    let before = layout.clone();
    assert!(layout.move_pane(moving, moving, Direction::Left).is_err());
    assert!(
        layout
            .move_pane(moving, PaneId(999), Direction::Left)
            .is_err()
    );
    assert_eq!(layout, before);
    layout.validate()
}

#[test]
fn closing_last_tab_collapses_view_and_focus_tracks_ancestors() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let initial_pane = layout.active_pane_id().unwrap();
    let (initial_view, initial_tab) = layout.pane_location(initial_pane).unwrap();
    let tab = layout.add_tab(initial_view, state("extra"))?;
    layout.focus_tab(initial_view, initial_tab)?;
    assert_eq!(layout.active_pane_id(), Some(initial_pane));
    layout.close_tab(initial_view, initial_tab)?;
    assert_eq!(layout.views[&initial_view].active_tab_id, tab);
    let second_view = layout.split_view(initial_view, Direction::Up, state("view"))?;
    let second_pane = layout.active_pane_id().unwrap();
    let (_, second_tab) = layout.pane_location(second_pane).unwrap();
    layout.focus_pane(second_pane)?;
    layout.close_tab(second_view, second_tab)?;
    assert_eq!(layout.active_view_id, initial_view);
    layout.validate()
}

#[test]
fn compose_is_atomic_and_guards_revision_and_ids() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let pane = layout.active_pane_id().unwrap();
    let revision = layout.revision;
    layout.compose(revision, |draft| {
        draft.split_pane(pane, Direction::Right, state("right"))?;
        draft.split_pane(pane, Direction::Up, state("above"))?;
        draft.pane_mut(pane).unwrap().label = Some("edited".into());
        Ok(())
    })?;
    assert_eq!(layout.revision, revision + 1);
    let before = layout.clone();
    let mut called = false;
    assert!(matches!(
        layout.compose(revision, |_| {
            called = true;
            Ok(())
        }),
        Err(LayoutError::RevisionConflict { .. })
    ));
    assert!(!called);
    assert!(
        layout
            .compose(layout.revision, |draft| {
                draft.views.clear();
                Ok(())
            })
            .is_err()
    );
    assert!(
        layout
            .compose(layout.revision, |draft| {
                draft.split_pane(pane, Direction::Right, state("discard"))?;
                Err::<(), _>(LayoutError::Invalid("abort"))
            })
            .is_err()
    );
    assert_eq!(layout, before);
    let removed = layout.split_pane(pane, Direction::Down, PaneState::default())?;
    layout.close_pane(removed)?;
    let before = layout.clone();
    assert!(
        layout
            .compose(layout.revision, |draft| {
                draft.next_id -= 1;
                Ok(())
            })
            .is_err()
    );
    assert!(
        layout
            .compose(layout.revision, |draft| {
                let fresh = draft.split_pane(pane, Direction::Right, PaneState::default())?;
                let (view, tab) = draft.pane_location(fresh).unwrap();
                let tab = draft
                    .views
                    .get_mut(&view)
                    .unwrap()
                    .tabs
                    .get_mut(&tab)
                    .unwrap();
                let state = tab.panes.remove(&fresh).unwrap();
                tab.panes.insert(removed, state);
                fn replace(tree: &mut SplitNode<PaneId>, old: PaneId, new: PaneId) {
                    match tree {
                        SplitNode::Leaf { content } if *content == old => *content = new,
                        SplitNode::Split { first, second, .. } => {
                            replace(first, old, new);
                            replace(second, old, new);
                        }
                        _ => {}
                    }
                }
                replace(&mut tab.root, fresh, removed);
                tab.active_pane_id = removed;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(layout, before);
    Ok(())
}

#[test]
fn roundtrip_and_atomic_save_reject_invalid_replacement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("workspace.json");
    let mut layout = WorkspaceLayout::new();
    let first = layout.active_pane_id().unwrap();
    layout.split_pane(first, Direction::Down, state("terminal"))?;
    layout.save(&path)?;
    assert_eq!(WorkspaceLayout::load(&path)?, layout);
    let json = serde_json::to_string(&layout)?;
    assert_eq!(serde_json::from_str::<WorkspaceLayout>(&json)?, layout);
    let view = layout.active_view_id;
    layout.split_view(view, Direction::Left, state("view"))?;
    layout.save(&path)?;
    assert_eq!(WorkspaceLayout::load(&path)?, layout);
    let good = std::fs::read(&path)?;
    layout.next_id = 1;
    assert!(layout.save(&path).is_err());
    assert_eq!(std::fs::read(&path)?, good);
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
    Ok(())
}

#[test]
fn malformed_schema_and_invariants_are_rejected() -> Result<()> {
    let base = serde_json::to_value(WorkspaceLayout::new())?;
    let cases: Vec<(&str, Value)> = vec![
        ("/next_id", json!(3)),
        ("/active_view_id", json!(999)),
        ("/root/content", json!(999)),
        ("/root/type", json!("unknown")),
        ("/views/1/tabs", json!({})),
        ("/views/1/active_tab_id", json!(999)),
        ("/views/1/rail_width", json!(-1)),
        ("/views/1/tabs/2/active_pane_id", json!(999)),
        ("/views/1/tabs/2/primary_pane_id", json!(999)),
        ("/views/1/tabs/2/panes", json!({})),
        (
            "/views/1/tabs/2/root",
            json!({"type":"split","horizontal":true,"ratio":0.05,"first":{"type":"leaf","content":3},"second":{"type":"leaf","content":3}}),
        ),
        (
            "/views/1/tabs/2/root",
            json!({"type":"split","horizontal":true,"ratio":0.5,"first":{"type":"leaf","content":3},"second":{"type":"leaf","content":3}}),
        ),
    ];
    for (pointer, bad) in cases {
        let mut value = base.clone();
        *value.pointer_mut(pointer).unwrap() = bad;
        assert!(
            serde_json::from_value::<WorkspaceLayout>(value).is_err(),
            "accepted {pointer}"
        );
    }
    // Unknown top-level fields are absorbed into the worktree-level catch-all
    // (WorkspaceLayout::extra) instead of rejected — required so the
    // super_format bridge can carry Super's unrelated keys losslessly.
    let mut unknown = base.clone();
    unknown["some_future_key"] = json!(true);
    let absorbed: WorkspaceLayout = serde_json::from_value(unknown).unwrap();
    assert_eq!(absorbed.extra["some_future_key"], json!(true));
    let mut unreachable = base.clone();
    unreachable["views"]["1"]["tabs"]["2"]["panes"]["4"] = json!(PaneState::default());
    unreachable["next_id"] = json!(5);
    assert!(serde_json::from_value::<WorkspaceLayout>(unreachable).is_err());
    let mut collision = base.clone();
    let pane = collision["views"]["1"]["tabs"]["2"]["panes"]["3"].take();
    collision["views"]["1"]["tabs"]["2"]["panes"] = json!({"1":pane});
    for field in ["active_pane_id", "primary_pane_id"] {
        collision["views"]["1"]["tabs"]["2"][field] = json!(1);
    }
    collision["views"]["1"]["tabs"]["2"]["root"]["content"] = json!(1);
    assert!(serde_json::from_value::<WorkspaceLayout>(collision).is_err());
    let mut duplicate = serde_json::to_string(&base)?;
    let pane_json = serde_json::to_string(&base["views"]["1"]["tabs"]["2"]["panes"]["3"])?;
    duplicate = duplicate.replace(
        &format!("\"3\":{pane_json}"),
        &format!("\"3\":{pane_json},\"3\":{pane_json}"),
    );
    assert_ne!(duplicate, serde_json::to_string(&base)?);
    assert!(serde_json::from_str::<WorkspaceLayout>(&duplicate).is_err());
    Ok(())
}

#[test]
fn strict_and_repaired_counters_and_exhaustion() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("layout.json");
    let mut value = serde_json::to_value(WorkspaceLayout::new())?;
    value["next_id"] = json!(1);
    std::fs::write(&path, serde_json::to_vec(&value)?)?;
    assert!(WorkspaceLayout::load(&path).is_err());
    let mut layout = WorkspaceLayout::load_with_policy(&path, IdPolicy::Repair)?;
    assert_eq!(layout.next_id, 4);
    layout.next_id = u64::MAX;
    let before = layout.clone();
    assert!(matches!(
        layout.add_tab(layout.active_view_id, PaneState::default()),
        Err(LayoutError::Exhausted)
    ));
    assert_eq!(layout, before);
    layout.revision = u64::MAX;
    let before = layout.clone();
    assert!(matches!(
        layout.focus_view(layout.active_view_id),
        Err(LayoutError::Exhausted)
    ));
    assert_eq!(layout, before);
    Ok(())
}

#[test]
fn depth_and_count_limits_leave_original_unchanged() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let pane = layout.active_pane_id().unwrap();
    for _ in 0..MAX_DEPTH {
        layout.split_pane(pane, Direction::Right, PaneState::default())?;
    }
    let before = layout.clone();
    assert!(matches!(
        layout.split_pane(pane, Direction::Right, PaneState::default()),
        Err(LayoutError::Limit("tree depth"))
    ));
    assert_eq!(layout, before);
    // Add tabs without increasing tree depth.
    for _ in 1..MAX_TABS {
        layout.add_tab(layout.active_view_id, PaneState::default())?;
    }
    let before = layout.clone();
    assert!(matches!(
        layout.add_tab(layout.active_view_id, PaneState::default()),
        Err(LayoutError::Limit("tabs"))
    ));
    assert_eq!(layout, before);
    Ok(())
}

#[test]
fn generic_tree_schema_and_edge_detection() -> Result<()> {
    let tree = SplitNode::Split {
        horizontal: true,
        ratio: 0.4,
        first: Box::new(SplitNode::leaf("hello".to_owned())),
        second: Box::new(SplitNode::leaf("world".to_owned())),
    };
    let value = serde_json::to_value(&tree)?;
    assert_eq!(value["type"], "split");
    assert_eq!(value["first"], json!({"type":"leaf","content":"hello"}));
    assert_eq!(serde_json::from_value::<SplitNode<String>>(value)?, tree);
    assert_eq!(edge_zone(20.0, 50.0, 100.0, 100.0), Some(Direction::Left));
    assert_eq!(edge_zone(80.0, 50.0, 100.0, 100.0), Some(Direction::Right));
    assert_eq!(edge_zone(50.0, 5.0, 100.0, 100.0), Some(Direction::Up));
    assert_eq!(edge_zone(50.0, 100.0, 100.0, 100.0), Some(Direction::Down));
    assert_eq!(edge_zone(0.0, 0.0, 100.0, 100.0), Some(Direction::Left));
    assert_eq!(edge_zone(19.0, 1.0, 100.0, 100.0), Some(Direction::Up));
    for (x, y, w, h) in [
        (50.0, 50.0, 100.0, 100.0),
        (-1.0, 0.0, 100.0, 100.0),
        (0.0, 0.0, 0.0, 100.0),
        (f64::NAN, 0.0, 100.0, 100.0),
    ] {
        assert_eq!(edge_zone(x, y, w, h), None);
    }
    Ok(())
}

#[test]
fn closing_active_pane_focuses_the_nearest_sibling() -> Result<()> {
    // Nested on the right: A | (B / C). Closing the active C must focus B
    // (the seam leaf), not the tree's global first leaf A.
    let mut layout = WorkspaceLayout::new();
    let a = layout.active_pane_id().unwrap();
    let b = layout.split_pane(a, Direction::Right, state("b"))?;
    let c = layout.split_pane(b, Direction::Down, state("c"))?;
    assert_eq!(layout.active_pane_id(), Some(c));
    layout.close_pane(c)?;
    assert_eq!(
        layout.active_pane_id(),
        Some(b),
        "closing C focuses the seam sibling B, not the first leaf {a:?}"
    );

    // Nested on the left: (A / B) | C. Closing the active C must focus B
    // (the sibling subtree's last leaf hugging the seam), not A.
    let mut layout = WorkspaceLayout::new();
    let a = layout.active_pane_id().unwrap();
    let b = layout.split_pane(a, Direction::Down, state("b"))?;
    let c = layout.split_pane(b, Direction::Right, state("c"))?;
    assert_eq!(layout.active_pane_id(), Some(c));
    layout.close_pane(c)?;
    assert_eq!(
        layout.active_pane_id(),
        Some(b),
        "closing C focuses the seam sibling B, not the first leaf {a:?}"
    );
    layout.validate()
}

#[test]
fn closing_a_background_pane_keeps_focus() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let a = layout.active_pane_id().unwrap();
    let b = layout.split_pane(a, Direction::Right, state("b"))?;
    let c = layout.split_pane(b, Direction::Down, state("c"))?;
    layout.focus_pane(c)?;
    layout.close_pane(a)?;
    assert_eq!(layout.active_pane_id(), Some(c));
    layout.validate()
}

#[test]
fn closing_active_tab_focuses_the_adjacent_tab() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let view = layout.active_view_id;
    let t1 = layout.views[&view].active_tab_id;
    let t2 = layout.add_tab(view, state("t2"))?;
    let t3 = layout.add_tab(view, state("t3"))?;
    assert_eq!(layout.views[&view].active_tab_id, t3);
    layout.focus_tab(view, t2)?;
    layout.close_tab(view, t2)?;
    assert_eq!(
        layout.views[&view].active_tab_id, t3,
        "the tab after the closed one, not the first tab {t1:?}"
    );
    layout.focus_tab(view, t3)?;
    layout.close_tab(view, t3)?;
    assert_eq!(
        layout.views[&view].active_tab_id, t1,
        "the tab before when there is no tab after"
    );
    layout.validate()
}

#[test]
fn closing_active_view_focuses_the_adjacent_view() -> Result<()> {
    let mut layout = WorkspaceLayout::new();
    let v1 = layout.active_view_id;
    let v2 = layout.split_view(v1, Direction::Right, state("v2"))?;
    let v3 = layout.split_view(v2, Direction::Right, state("v3"))?;
    assert_eq!(layout.active_view_id, v3);
    layout.close_view(v3)?;
    assert_eq!(
        layout.active_view_id, v2,
        "the view before the closed one, not the first view {v1:?}"
    );
    layout.focus_view(v1)?;
    layout.close_view(v1)?;
    assert_eq!(layout.active_view_id, v2, "the view after the closed one");
    layout.validate()
}

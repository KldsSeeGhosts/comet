//! Round-trip acceptance tests for the Super-format serde bridge
//! (`zeron_workspace::super_format`) against the verbatim persisted layout in
//! `super-analysis/reference/session.json`.

use std::path::PathBuf;
// Explicit so it wins over the glob-imported `zeron_workspace::Result` alias.
use std::result::Result;

use serde_json::{Value, json};
use zeron_workspace::super_format::{SuperWorktreeValue, engine_to_super, super_to_engine};
use zeron_workspace::*;

fn reference_path() -> PathBuf {
    // Repo root is two levels up from this crate's manifest dir:
    // <root>/super-analysis/reference/session.json.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../super-analysis/reference/session.json")
}

/// The ground-truth per-worktree value plus the raw `serde_json::Value` used
/// for byte-level (structural) round-trip comparison.
fn reference_value() -> (Value, SuperWorktreeValue) {
    let raw = std::fs::read_to_string(reference_path())
        .expect("super-analysis/reference/session.json must exist relative to the crate");
    let document: Value = serde_json::from_str(&raw).unwrap();
    let original = document["selections"][0]["value"].clone();
    let value: SuperWorktreeValue = serde_json::from_value(original.clone()).unwrap();
    (original, value)
}

#[test]
fn reference_import_matches_super_structure() -> Result<(), Box<dyn std::error::Error>> {
    let (_, value) = reference_value();
    assert_eq!(value.tabs.len(), 3);
    assert_eq!(value.active_tab, 0);
    assert_eq!(value.split_layouts.len(), 3);
    assert!(value.split_layouts[0].is_some());
    assert!(value.split_layouts[1].is_none());
    assert!(value.split_layouts[2].is_some());

    let layout = super_to_engine(&value);
    assert_eq!(layout.revision, 0);
    layout.validate()?;

    assert_eq!(layout.views.len(), 1);
    assert!(matches!(layout.root, SplitNode::Leaf { .. }));
    let (_, view) = layout.views.iter().next().unwrap();
    // Deterministic allocation: Super's pane ids {2,3,4,7,8,9} are reserved,
    // so the view takes 1, tabs take 5/6/11 and the null-layout tab's single
    // pane takes 10. next_id is one past the largest live id.
    let order = view.ordered_tabs();
    assert_eq!(order, vec![TabId(5), TabId(6), TabId(11)]);
    assert_eq!(view.active_tab_id, TabId(5));
    assert_eq!(layout.next_id, 12);

    // Tab 0: horizontal 0.5 over two vertical 0.5 stacks.
    let tab0 = &view.tabs[&TabId(5)];
    let SplitNode::Split {
        horizontal: h0,
        ratio: r0,
        first: f0,
        second: s0,
    } = &tab0.root
    else {
        panic!("tab 0 root must be a split");
    };
    assert!(h0);
    assert_eq!(*r0, 0.5);
    let SplitNode::Split {
        horizontal: h1,
        ratio: r1,
        first: f1,
        second: s1,
    } = f0.as_ref()
    else {
        panic!("tab 0 first branch must be a split");
    };
    assert!(!h1);
    assert_eq!(*r1, 0.5);
    assert_eq!(**f1, SplitNode::leaf(PaneId(4)));
    assert_eq!(**s1, SplitNode::leaf(PaneId(7)));
    let SplitNode::Split {
        horizontal: h2,
        ratio: r2,
        first: f2,
        second: s2,
    } = s0.as_ref()
    else {
        panic!("tab 0 second branch must be a split");
    };
    assert!(!h2);
    assert_eq!(*r2, 0.5);
    assert_eq!(**f2, SplitNode::leaf(PaneId(2)));
    assert_eq!(**s2, SplitNode::leaf(PaneId(3)));
    assert_eq!(tab0.active_pane_id, PaneId(4));
    assert_eq!(tab0.primary_pane_id, PaneId(4));

    // Primary pane (4) carries tab 0's fields; unmapped fields stay in extra.
    let p4 = &tab0.panes[&PaneId(4)];
    assert_eq!(p4.mode, PaneMode::Chat);
    assert_eq!(p4.provider_key.as_deref(), Some("codex"));
    assert_eq!(p4.session_id.as_deref(), Some("codex-17896269968694"));
    assert_eq!(
        p4.conversation_id.as_deref(),
        Some("conv:codex:codex-17896269968694")
    );
    assert_eq!(p4.permission_mode.as_deref(), Some("bypass"));
    assert_eq!(p4.label, None);
    assert!(!p4.title_sc_owned);
    assert_eq!(
        p4.extra["tab_uuid"],
        json!("522129b3-0508-42c8-a986-6008254fc986")
    );
    assert_eq!(p4.extra["messages_snapshot"], json!([]));
    assert_eq!(p4.extra["thinking_enabled"], json!(true));

    // Pane 7 is an embedded secondary tab with an sc-owned title.
    let p7 = &tab0.panes[&PaneId(7)];
    assert_eq!(p7.provider_key.as_deref(), Some("pi"));
    assert_eq!(p7.label.as_deref(), Some("Mac Storage Cleanup"));
    assert!(p7.title_sc_owned);
    assert!(p7.extra["messages_snapshot"].as_array().is_some());
    assert_eq!(p7.extra["context_window"], json!(262000));
    assert_eq!(p7.extra["last_context_tokens"], json!(24034));

    assert_eq!(tab0.panes[&PaneId(2)].provider_key.as_deref(), Some("pi"));
    assert_eq!(
        tab0.panes[&PaneId(3)].provider_key.as_deref(),
        Some("codex")
    );

    // Tab 1: null split layout imports as a single synthesized pane.
    let tab1 = &view.tabs[&TabId(6)];
    assert!(matches!(tab1.root, SplitNode::Leaf { .. }));
    assert_eq!(tab1.panes.len(), 1);
    assert_eq!(tab1.active_pane_id, PaneId(10));
    assert_eq!(tab1.primary_pane_id, PaneId(10));
    assert_eq!(tab1.panes[&PaneId(10)].provider_key.as_deref(), Some("pi"));
    assert_eq!(
        tab1.panes[&PaneId(10)].extra["model_reasoning_effort"],
        json!("max")
    );

    // Tab 2: horizontal 0.5 with the terminal split; active pane is the
    // secondary one (9) while the primary stays 8.
    let tab2 = &view.tabs[&TabId(11)];
    let SplitNode::Split {
        horizontal,
        ratio,
        first,
        second,
    } = &tab2.root
    else {
        panic!("tab 2 root must be a split");
    };
    assert!(horizontal);
    assert_eq!(*ratio, 0.5);
    assert_eq!(**first, SplitNode::leaf(PaneId(8)));
    assert_eq!(**second, SplitNode::leaf(PaneId(9)));
    assert_eq!(tab2.active_pane_id, PaneId(9));
    assert_eq!(tab2.primary_pane_id, PaneId(8));
    assert_eq!(tab2.panes[&PaneId(8)].mode, PaneMode::Terminal);
    assert_eq!(tab2.panes[&PaneId(8)].label.as_deref(), Some("Terminal"));
    assert_eq!(tab2.panes[&PaneId(9)].mode, PaneMode::Terminal);
    assert_eq!(
        tab2.panes[&PaneId(9)].extra["working_directory_path"],
        json!("/Users/kidsseemac/superconductor/projects/open-design-project")
    );
    Ok(())
}

/// The acceptance test named in the plan: import -> export must reproduce the
/// original persisted value exactly (the `extra` catch-alls on
/// `SuperWorktreeValue`, `SuperTab` and `PaneState` carry every field the
/// engine does not model).
#[test]
fn super_round_trip_is_value_identical() -> Result<(), Box<dyn std::error::Error>> {
    let (original, value) = reference_value();
    let layout = super_to_engine(&value);
    let exported = engine_to_super(&layout).expect("single-view import must export");
    assert_eq!(serde_json::to_value(&exported)?, original);
    // And the bridge is stable: importing the export yields the same layout.
    assert_eq!(super_to_engine(&exported), layout);
    Ok(())
}

#[test]
fn engine_native_persistence_preserves_import() -> Result<(), Box<dyn std::error::Error>> {
    let (_, value) = reference_value();
    let layout = super_to_engine(&value);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("workspace.json");
    layout.save(&path)?;
    // The import already sets next_id past every live id, so the strict
    // default policy (IdPolicy::Reject) is the right load policy here;
    // IdPolicy::Repair only exists for files with stale counters.
    let loaded = WorkspaceLayout::load(&path)?;
    assert_eq!(loaded, layout);
    loaded.validate()?;
    // Super-only fields survive engine-native save/load via PaneState::extra.
    assert_eq!(
        loaded.pane(PaneId(7)).unwrap().extra["context_window"],
        json!(262000)
    );
    Ok(())
}

/// Documents the ratio contract. tree.rs performs NO ratio check at
/// deserialization: `SplitNode` is a plain derived `Deserialize`, so any
/// finite f64 parses. The engine enforces MIN_RATIO..=MAX_RATIO only in
/// `validate()`. The bridge preserves ratios verbatim in both directions, so
/// a foreign ratio imports cleanly, fails validation, and still round-trips.
#[test]
fn foreign_ratio_imports_but_fails_validation() -> Result<(), Box<dyn std::error::Error>> {
    let foreign = json!({
        "tabs": [{ "kind": "api-chat", "session_id": "s1" }],
        "active_tab": 0,
        "split_layouts": [{
            "root": {
                "kind": "split", "axis": "horizontal", "ratio": 0.05,
                "first": { "kind": "leaf", "leaf": { "pane_id": 2,
                            "content": { "kind": "primary-tab" } } },
                "second": { "kind": "leaf", "leaf": { "pane_id": 3,
                            "content": { "kind": "tab",
                                         "tab": { "kind": "api-chat", "session_id": "s2" } } } }
            },
            "active_pane_id": 2,
            "primary_pane_id": 2
        }]
    });
    let value: SuperWorktreeValue = serde_json::from_value(foreign.clone())?;
    // The bridge parse itself accepts the foreign ratio untouched.
    let root = value.split_layouts[0].as_ref().unwrap().root.clone();
    let super_format::SuperNode::Split { axis, ratio, .. } = root else {
        panic!("expected a split root");
    };
    assert_eq!(axis, super_format::SuperAxis::Horizontal);
    assert_eq!(ratio, 0.05);

    // The engine tree also deserializes 0.05 without complaint...
    let engine_tree: SplitNode<PaneId> = serde_json::from_value(json!({
        "type": "split", "horizontal": true, "ratio": 0.05,
        "first": { "type": "leaf", "content": 1 },
        "second": { "type": "leaf", "content": 2 }
    }))?;
    assert!(matches!(engine_tree, SplitNode::Split { ratio, .. } if ratio == 0.05));

    // ...and validation is where it is rejected, with tree.rs's exact message.
    let layout = super_to_engine(&value);
    assert!(matches!(
        layout.validate(),
        Err(LayoutError::Invalid(
            "split ratio must be finite and within 0.1..=0.9"
        ))
    ));

    // The bridge itself neither clamps nor rejects: ratios round-trip
    // verbatim, so even this foreign value survives a full round trip.
    let exported = engine_to_super(&layout).expect("single-view import must export");
    assert_eq!(serde_json::to_value(&exported)?, foreign);
    Ok(())
}

#[test]
fn multi_view_layout_is_not_expressible_in_super_format() -> Result<(), Box<dyn std::error::Error>>
{
    let mut layout = WorkspaceLayout::new();
    let view = layout.active_view_id;
    layout.split_view(view, Direction::Right, PaneState::default())?;
    assert_eq!(layout.views.len(), 2);
    assert!(engine_to_super(&layout).is_none());
    // Single-view layouts always export.
    assert!(engine_to_super(&WorkspaceLayout::new()).is_some());
    Ok(())
}

/// PaneState additions are backward compatible: previously-serialized engine
/// files (without the new fields) still parse, and unknown pane fields are
/// absorbed into `extra` instead of rejected.
#[test]
fn legacy_pane_state_still_parses() {
    let legacy = json!({
        "session_id": "s",
        "mode": "terminal",
        "label": "l",
        "group": "g"
    });
    let pane: PaneState = serde_json::from_value(legacy).unwrap();
    assert_eq!(pane.provider_key, None);
    assert_eq!(pane.conversation_id, None);
    assert_eq!(pane.permission_mode, None);
    assert!(!pane.title_sc_owned);
    assert!(pane.extra.is_empty());

    let forward: PaneState =
        serde_json::from_value(json!({ "mode": "chat", "future_field": 1 })).unwrap();
    assert_eq!(forward.mode, PaneMode::Chat);
    assert_eq!(forward.extra["future_field"], json!(1));
}

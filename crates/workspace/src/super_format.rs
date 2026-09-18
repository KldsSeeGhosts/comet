//! Serde bridge between Super's per-worktree persisted value and the engine's
//! [`WorkspaceLayout`].
//!
//! Super persists one value per worktree: `tabs[]` (full tab objects),
//! `active_tab` (an index), and `split_layouts[]` parallel to `tabs` whose
//! entries may be `null`, plus unrelated UI state (`file_tree_*`,
//! `change_list_*`, `diff_*`) that [`SuperWorktreeValue::extra`] absorbs so a
//! parsed value re-serializes losslessly. Tab `i`'s pane tree is
//! `split_layouts[i]`; `null` means the tab has no split (a single pane).
//! Split nodes are internally tagged with `"kind"`: splits carry
//! `axis: "horizontal"|"vertical"` and a float `ratio`; leaves embed
//! `{"pane_id", "content"}` where content is `{"kind": "primary-tab"}` (the
//! pane that renders `tabs[active_tab]`) or `{"kind": "tab", "tab": {...}}`.
//!
//! # Mapping
//!
//! - One `SuperWorktreeValue` imports as ONE engine view holding all Super
//!   tabs as engine tabs in `tabs[]` order. Super has no view/tab ids, so
//!   engine view and tab ids are synthesized in first-free order, skipping
//!   Super's preserved pane ids so the sparse ids (e.g. 2, 3, 4, 7, 8, 9)
//!   stay verbatim. `next_id` is one past the largest id in the imported
//!   layout (synthesized view/tab ids included) and `revision` starts at 0.
//! - A tab's `kind` "terminal" maps to [`PaneMode::Terminal`]; every other
//!   kind maps to [`PaneMode::Chat`]. Exporting maps `Chat` back to
//!   "api-chat", so exotic non-"terminal" kinds do not survive a full
//!   round-trip.
//! - `title` maps to the pane `label` and `title_sc_owned` to the pane field
//!   of the same name.
//! - Tab-object fields without an engine counterpart (`tab_uuid`,
//!   `provider_profile_selection`, `preferred_model_id`,
//!   `model_reasoning_effort`, `api_chat_thread_id`, `messages_snapshot`,
//!   `thinking_enabled`, `context_window`, `last_context_tokens`, terminal
//!   `preset_key`/`working_directory_path`, and anything unrecognized) are
//!   preserved verbatim in [`crate::PaneState::extra`], keeping the bridge
//!   and engine-native persistence lossless for them. Worktree-level extras
//!   (`file_tree_*`, `change_list_*`, `diff_*`) ride on
//!   [`crate::WorkspaceLayout::extra`].
//!
//! # Lossy edges
//!
//! - [`engine_to_super`] returns `None` for multi-view workspaces: Super's
//!   format holds exactly one tab strip per worktree value, while the
//!   engine's native format is the richer multi-view [`WorkspaceLayout`].
//!   The bridge targets the import/validation path only.
//! - A single-pane engine tab always exports as a `null` split layout: the
//!   engine cannot distinguish "no split tree" from a single-leaf tree, and
//!   Super treats the two as equivalent.
//! - `title_sc_owned: false` and an absent `title_sc_owned` both collapse to
//!   `false` in the engine pane and export as absent.
//! - The engine-only pane `group` field has no Super counterpart and is
//!   dropped on export.
//! - A foreign `active_tab` index past the end of `tabs` is clamped to the
//!   last tab on import.
//! - Malformed imports (duplicate pane ids, a dangling `active_pane_id`, a
//!   missing `primary-tab` leaf, ratios outside the engine's range) are not
//!   rejected by [`super_to_engine`]; they surface through
//!   [`WorkspaceLayout::validate`]. Ratios are preserved verbatim in both
//!   directions — the engine enforces `MIN_RATIO..=MAX_RATIO` at validation
//!   time, not at deserialization.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    PaneId, PaneMode, PaneState, SplitNode, SplitTabLayout, TabId, TabPlacement, ViewId, ViewLayout,
    WorkspaceLayout,
};

/// One worktree's persisted value in Super's format: the tab strip plus the
/// per-tab pane trees, with every unrecognized key kept verbatim in `extra`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuperWorktreeValue {
    #[serde(default)]
    pub tabs: Vec<SuperTab>,
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub split_layouts: Vec<Option<SuperSplitLayout>>,
    /// Catch-all for unrelated persisted keys (`file_tree_*`, `change_list_*`,
    /// `diff_*`, ...). Required for a lossless round-trip; `flatten` precludes
    /// `deny_unknown_fields`.
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

/// Super's tab object. Known fields are modeled explicitly; anything else
/// (including terminal-only `preset_key`/`working_directory_path`) lands in
/// `extra` and round-trips unchanged. Absent `Option` fields are skipped on
/// serialization so an absent field stays absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuperTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_uuid: Option<String>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_selection: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_chat_thread_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_sc_owned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_snapshot: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_context_tokens: Option<Value>,
    /// Known-but-unmapped fields and unknown keys live here; the engine
    /// counterpart is [`crate::PaneState::extra`].
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

/// `{root, active_pane_id, primary_pane_id}` — one tab's pane tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuperSplitLayout {
    pub root: SuperNode,
    pub active_pane_id: u64,
    pub primary_pane_id: u64,
}

/// Super's recursive split tree, tagged with `"kind"` (the engine's
/// [`SplitNode`] uses `"type"`/`horizontal: bool`, hence the bridge).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SuperNode {
    Split {
        axis: SuperAxis,
        ratio: f64,
        first: Box<SuperNode>,
        second: Box<SuperNode>,
    },
    Leaf {
        leaf: SuperLeaf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuperAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuperLeaf {
    pub pane_id: u64,
    pub content: SuperContent,
}

/// `primary-tab` marks the pane rendering `tabs[active_tab]`; every other
/// pane embeds its full tab object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SuperContent {
    PrimaryTab,
    Tab {
        tab: SuperTab,
    },
}

/// Tab-object fields the engine pane maps onto named fields; everything else
/// is preserved in `extra`.
const MAPPED_TAB_KEYS: [&str; 7] = [
    "kind",
    "provider_key",
    "session_id",
    "conversation_id",
    "title",
    "title_sc_owned",
    "permission_mode",
];

/// Deterministic first-free id allocator that never collides with ids
/// reserved up front (Super's preserved pane ids).
struct Ids {
    used: BTreeSet<u64>,
    next: u64,
}

impl Ids {
    fn new(reserved: BTreeSet<u64>) -> Self {
        Self { used: reserved, next: 1 }
    }

    fn take(&mut self) -> u64 {
        while self.used.contains(&self.next) {
            self.next += 1;
        }
        let id = self.next;
        self.next += 1;
        self.used.insert(id);
        id
    }

    /// One past the largest allocated id (the engine requires `next_id` to
    /// exceed every live view, tab and pane id).
    fn next_id(&self) -> u64 {
        self.used.iter().next_back().copied().unwrap_or(0).saturating_add(1).max(1)
    }
}

fn collect_pane_ids(node: &SuperNode, ids: &mut BTreeSet<u64>) {
    match node {
        SuperNode::Split { first, second, .. } => {
            collect_pane_ids(first, ids);
            collect_pane_ids(second, ids);
        }
        SuperNode::Leaf { leaf } => {
            ids.insert(leaf.pane_id);
        }
    }
}

fn pane_from_tab(tab: &SuperTab) -> PaneState {
    // Keep every tab-object field that has no engine counterpart verbatim:
    // serialize the tab, then strip the fields mapped onto named pane fields.
    let mut extra = serde_json::to_value(tab)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    for key in MAPPED_TAB_KEYS {
        extra.remove(key);
    }
    PaneState {
        session_id: tab.session_id.clone(),
        mode: if tab.kind == "terminal" { PaneMode::Terminal } else { PaneMode::Chat },
        label: tab.title.clone(),
        group: None,
        provider_key: tab.provider_key.clone(),
        conversation_id: tab.conversation_id.clone(),
        permission_mode: tab.permission_mode.clone(),
        title_sc_owned: tab.title_sc_owned.unwrap_or(false),
        extra,
    }
}

fn tab_from_pane(pane: &PaneState) -> SuperTab {
    SuperTab {
        // Not stored on the pane; survives via `extra` when it was present.
        tab_uuid: None,
        kind: match pane.mode {
            PaneMode::Terminal => "terminal",
            PaneMode::Chat => "api-chat",
        }
        .to_owned(),
        provider_key: pane.provider_key.clone(),
        provider_profile_selection: None,
        preferred_model_id: None,
        model_reasoning_effort: None,
        session_id: pane.session_id.clone(),
        api_chat_thread_id: None,
        conversation_id: pane.conversation_id.clone(),
        title: pane.label.clone(),
        // `false` and "absent" are indistinguishable after the engine round
        // trip, so the field is only emitted when owned.
        title_sc_owned: pane.title_sc_owned.then_some(true),
        messages_snapshot: None,
        thinking_enabled: None,
        permission_mode: pane.permission_mode.clone(),
        context_window: None,
        last_context_tokens: None,
        extra: pane.extra.clone(),
    }
}

fn node_to_engine(
    node: &SuperNode,
    tab: &SuperTab,
    panes: &mut BTreeMap<PaneId, PaneState>,
    primary: &mut Option<PaneId>,
) -> SplitNode<PaneId> {
    match node {
        SuperNode::Split { axis, ratio, first, second } => SplitNode::Split {
            horizontal: *axis == SuperAxis::Horizontal,
            ratio: *ratio,
            first: Box::new(node_to_engine(first, tab, panes, primary)),
            second: Box::new(node_to_engine(second, tab, panes, primary)),
        },
        SuperNode::Leaf { leaf } => {
            let pane_id = PaneId(leaf.pane_id);
            let state = match &leaf.content {
                SuperContent::PrimaryTab => {
                    *primary = Some(pane_id);
                    pane_from_tab(tab)
                }
                SuperContent::Tab { tab: inner } => pane_from_tab(inner),
            };
            panes.insert(pane_id, state);
            SplitNode::leaf(pane_id)
        }
    }
}

fn tab_from_split(split: &SuperSplitLayout, tab: &SuperTab) -> SplitTabLayout {
    let mut panes = BTreeMap::new();
    let mut primary = None;
    let root = node_to_engine(&split.root, tab, &mut panes, &mut primary);
    let fallback = *root.first_leaf();
    let active = PaneId(split.active_pane_id);
    SplitTabLayout {
        root,
        // A dangling active id falls back to the first leaf rather than
        // failing the import; `validate` reports the structural problem.
        active_pane_id: if panes.contains_key(&active) { active } else { fallback },
        primary_pane_id: primary.unwrap_or(fallback),
        panes,
    }
}

/// Import Super's persisted worktree value as a single-view engine layout.
/// Structural problems are not rejected here; run [`WorkspaceLayout::validate`]
/// on the result (see the module's lossy-edges notes).
pub fn super_to_engine(value: &SuperWorktreeValue) -> WorkspaceLayout {
    let mut reserved = BTreeSet::new();
    for split in value.split_layouts.iter().flatten() {
        collect_pane_ids(&split.root, &mut reserved);
    }
    let mut ids = Ids::new(reserved);

    let view_id = ViewId(ids.take());
    let mut tabs = BTreeMap::new();
    let mut order = Vec::new();
    for (index, tab) in value.tabs.iter().enumerate() {
        let tab_id = TabId(ids.take());
        let entry = match value.split_layouts.get(index).and_then(Option::as_ref) {
            Some(split) => tab_from_split(split, tab),
            None => {
                // `null` split layout: a single pane with a synthesized id.
                let pane_id = PaneId(ids.take());
                SplitTabLayout {
                    root: SplitNode::leaf(pane_id),
                    active_pane_id: pane_id,
                    primary_pane_id: pane_id,
                    panes: BTreeMap::from([(pane_id, pane_from_tab(tab))]),
                }
            }
        };
        tabs.insert(tab_id, entry);
        order.push(tab_id);
    }

    let active_tab_id = order
        .get(value.active_tab.min(order.len().saturating_sub(1)))
        .copied()
        .unwrap_or(TabId(0));
    let views = BTreeMap::from([(
        view_id,
        ViewLayout {
            rail_width: 240.0,
            tab_placement: TabPlacement::Top,
            active_tab_id,
            tab_order: order,
            tabs,
        },
    )]);
    WorkspaceLayout {
        root: SplitNode::leaf(view_id),
        active_view_id: view_id,
        views,
        next_id: ids.next_id(),
        revision: 0,
        // Unrelated worktree-level keys (`file_tree_*`, `change_list_*`,
        // `diff_*`, ...) ride along on [`crate::WorkspaceLayout::extra`] so
        // the export side can restore them verbatim.
        extra: value.extra.clone(),
    }
}

fn leaf_from_engine(pane_id: PaneId, tab: &SplitTabLayout) -> SuperNode {
    // The primary pane exports as `primary-tab`; every other leaf embeds the
    // pane's full state as a tab object.
    let content = if pane_id == tab.primary_pane_id {
        SuperContent::PrimaryTab
    } else {
        SuperContent::Tab {
            tab: tab_from_pane(tab.panes.get(&pane_id).unwrap_or(&PaneState::default())),
        }
    };
    SuperNode::Leaf {
        leaf: SuperLeaf { pane_id: pane_id.0, content },
    }
}

fn node_from_engine(node: &SplitNode<PaneId>, tab: &SplitTabLayout) -> SuperNode {
    match node {
        SplitNode::Split { horizontal, ratio, first, second } => SuperNode::Split {
            axis: if *horizontal { SuperAxis::Horizontal } else { SuperAxis::Vertical },
            ratio: *ratio,
            first: Box::new(node_from_engine(first, tab)),
            second: Box::new(node_from_engine(second, tab)),
        },
        SplitNode::Leaf { content } => leaf_from_engine(*content, tab),
    }
}

fn split_from_engine(tab: &SplitTabLayout) -> SuperSplitLayout {
    SuperSplitLayout {
        root: node_from_engine(&tab.root, tab),
        active_pane_id: tab.active_pane_id.0,
        primary_pane_id: tab.primary_pane_id.0,
    }
}

/// Export a single-view engine layout as Super's persisted worktree value.
/// Returns `None` for multi-view workspaces — Super's format has exactly one
/// tab strip per worktree value, so only the single-view case is expressible
/// (the bridge is the import/validation path, not the native persistence
/// format; see the module docs for the remaining lossy edges).
pub fn engine_to_super(layout: &WorkspaceLayout) -> Option<SuperWorktreeValue> {
    if layout.views.len() != 1 {
        return None;
    }
    let (_, view) = layout.views.iter().next()?;
    let order = view.ordered_tabs();
    let mut tabs = Vec::with_capacity(order.len());
    let mut split_layouts = Vec::with_capacity(order.len());
    for tab_id in &order {
        let tab = view.tabs.get(tab_id)?;
        tabs.push(tab_from_pane(
            tab.panes.get(&tab.primary_pane_id).unwrap_or(&PaneState::default()),
        ));
        // A single-pane tab exports as `null`: the engine cannot distinguish
        // "no split tree" from a single-leaf tree and Super equates the two.
        split_layouts.push(if tab.panes.len() <= 1 { None } else { Some(split_from_engine(tab)) });
    }
    let active_tab = order.iter().position(|id| *id == view.active_tab_id).unwrap_or(0);
    Some(SuperWorktreeValue {
        tabs,
        active_tab,
        split_layouts,
        // Restores the worktree-level keys stashed on import (see
        // `WorkspaceLayout::extra`).
        extra: layout.extra.clone(),
    })
}

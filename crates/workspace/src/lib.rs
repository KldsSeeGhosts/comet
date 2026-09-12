//! Serializable workspace trees without UI or engine dependencies.
//!
//! Structural methods validate and commit atomically, incrementing `revision` once.
//! Public fields and `pane_mut` support draft editing; use `compose` to commit those edits.
mod persistence;
mod arrange;
mod tree;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use persistence::IdPolicy;
pub use tree::{Branch, Direction, MAX_DEPTH, MAX_RATIO, MIN_RATIO, SplitNode, edge_zone};

pub type Result<T> = std::result::Result<T, LayoutError>;
pub const MAX_VIEWS: usize = 64;
pub const MAX_TABS: usize = 512;
pub const MAX_PANES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("invalid layout: {0}")]
    Invalid(&'static str),
    #[error("layout limit exceeded: {0}")]
    Limit(&'static str),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("cannot remove the last {0}")]
    Last(&'static str),
    #[error("revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("ID or revision counter exhausted")]
    Exhausted,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

macro_rules! id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}
id!(ViewId);
id!(TabId);
id!(PaneId);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneMode {
    #[default]
    Chat,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabPlacement {
    #[default]
    Top,
    Left,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneState {
    pub session_id: Option<String>,
    pub mode: PaneMode,
    pub label: Option<String>,
    pub group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitTabLayout {
    pub root: SplitNode<PaneId>,
    pub active_pane_id: PaneId,
    pub primary_pane_id: PaneId,
    #[serde(deserialize_with = "persistence::unique_map")]
    pub panes: BTreeMap<PaneId, PaneState>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewLayout {
    pub rail_width: f64,
    pub tab_placement: TabPlacement,
    pub active_tab_id: TabId,
    #[serde(default)]
    pub tab_order: Vec<TabId>,
    #[serde(deserialize_with = "persistence::unique_map")]
    pub tabs: BTreeMap<TabId, SplitTabLayout>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorkspaceLayout {
    pub root: SplitNode<ViewId>,
    pub active_view_id: ViewId,
    pub views: BTreeMap<ViewId, ViewLayout>,
    pub next_id: u64,
    pub revision: u64,
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl SplitTabLayout {
    fn single(id: PaneId, pane: PaneState) -> Self {
        Self {
            root: SplitNode::leaf(id),
            active_pane_id: id,
            primary_pane_id: id,
            panes: BTreeMap::from([(id, pane)]),
        }
    }

    fn remove(&mut self, pane: PaneId) -> Result<PaneState> {
        if self.panes.len() == 1 {
            return Err(LayoutError::Last("pane in tab"));
        }
        let state = self
            .panes
            .remove(&pane)
            .ok_or(LayoutError::NotFound("pane"))?;
        self.root = self
            .root
            .clone()
            .remove(pane)
            .ok_or(LayoutError::Last("pane in tab"))?;
        if self.active_pane_id == pane {
            self.active_pane_id = *self.root.first_leaf();
        }
        if self.primary_pane_id == pane {
            self.primary_pane_id = *self.root.first_leaf();
        }
        Ok(state)
    }
}

impl ViewLayout {
    fn single(tab: TabId, pane: PaneId, state: PaneState) -> Self {
        Self {
            rail_width: 240.0,
            tab_placement: TabPlacement::Top,
            active_tab_id: tab,
            tab_order: vec![tab],
            tabs: BTreeMap::from([(tab, SplitTabLayout::single(pane, state))]),
        }
    }

    pub fn ordered_tabs(&self) -> Vec<TabId> {
        if self.tab_order.is_empty() { self.tabs.keys().copied().collect() }
        else { self.tab_order.clone() }
    }

    fn insert_tab(&mut self, tab: TabId, state: SplitTabLayout) {
        self.tab_order = self.ordered_tabs();
        if !self.tabs.contains_key(&tab) { self.tab_order.push(tab); }
        self.tabs.insert(tab, state);
    }
}

impl WorkspaceLayout {
    pub fn new() -> Self {
        Self {
            root: SplitNode::leaf(ViewId(1)),
            active_view_id: ViewId(1),
            views: BTreeMap::from([(
                ViewId(1),
                ViewLayout::single(TabId(2), PaneId(3), PaneState::default()),
            )]),
            next_id: 4,
            revision: 0,
        }
    }

    pub fn active_pane_id(&self) -> Option<PaneId> {
        let view = self.views.get(&self.active_view_id)?;
        Some(view.tabs.get(&view.active_tab_id)?.active_pane_id)
    }

    pub fn pane_location(&self, id: PaneId) -> Option<(ViewId, TabId)> {
        self.views.iter().find_map(|(view_id, view)| {
            view.tabs.iter().find_map(|(tab_id, tab)| {
                tab.panes.contains_key(&id).then_some((*view_id, *tab_id))
            })
        })
    }

    pub fn pane(&self, id: PaneId) -> Option<&PaneState> {
        let (view, tab) = self.pane_location(id)?;
        self.views.get(&view)?.tabs.get(&tab)?.panes.get(&id)
    }

    /// Raw draft access. Use `compose` when the edit must advance the revision.
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut PaneState> {
        let (view, tab) = self.pane_location(id)?;
        self.views
            .get_mut(&view)?
            .tabs
            .get_mut(&tab)?
            .panes
            .get_mut(&id)
    }

    fn allocate(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(LayoutError::Exhausted)?;
        Ok(id)
    }

    fn tab_mut(&mut self, view: ViewId, tab: TabId) -> Result<&mut SplitTabLayout> {
        self.views
            .get_mut(&view)
            .ok_or(LayoutError::NotFound("view"))?
            .tabs
            .get_mut(&tab)
            .ok_or(LayoutError::NotFound("tab"))
    }

    fn ids(&self) -> BTreeSet<u64> {
        self.views
            .iter()
            .flat_map(|(view, state)| {
                std::iter::once(view.0).chain(state.tabs.iter().flat_map(|(tab, state)| {
                    std::iter::once(tab.0).chain(state.panes.keys().map(|pane| pane.0))
                }))
            })
            .collect()
    }

    /// Commit a validated draft once. Errors leave the original, including its counters, unchanged.
    /// Nested structural calls are allowed; the outer commit advances revision only once.
    pub fn compose<R>(
        &mut self,
        expected_revision: u64,
        edit: impl FnOnce(&mut Self) -> Result<R>,
    ) -> Result<R> {
        if self.revision != expected_revision {
            return Err(LayoutError::RevisionConflict {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        self.validate()?;
        let revision = self.revision.checked_add(1).ok_or(LayoutError::Exhausted)?;
        let mut draft = self.clone();
        let result = edit(&mut draft)?;
        draft.validate()?;
        if draft.next_id < self.next_id
            || draft
                .ids()
                .difference(&self.ids())
                .any(|id| *id < self.next_id)
        {
            return Err(LayoutError::Invalid(
                "compose cannot reuse IDs or lower next_id",
            ));
        }
        draft.revision = revision;
        *self = draft;
        Ok(result)
    }

    pub fn split_view(
        &mut self,
        target: ViewId,
        direction: Direction,
        pane: PaneState,
    ) -> Result<ViewId> {
        self.compose(self.revision, |draft| {
            if !draft.views.contains_key(&target) {
                return Err(LayoutError::NotFound("view"));
            }
            let view = ViewId(draft.allocate()?);
            let tab = TabId(draft.allocate()?);
            let pane_id = PaneId(draft.allocate()?);
            draft.root.insert(target, view, direction);
            draft
                .views
                .insert(view, ViewLayout::single(tab, pane_id, pane));
            draft.active_view_id = view;
            Ok(view)
        })
    }

    pub fn add_tab(&mut self, view: ViewId, pane: PaneState) -> Result<TabId> {
        self.compose(self.revision, |draft| {
            if !draft.views.contains_key(&view) {
                return Err(LayoutError::NotFound("view"));
            }
            let tab = TabId(draft.allocate()?);
            let pane_id = PaneId(draft.allocate()?);
            let state = draft
                .views
                .get_mut(&view)
                .ok_or(LayoutError::NotFound("view"))?;
            state.insert_tab(tab, SplitTabLayout::single(pane_id, pane));
            state.active_tab_id = tab;
            draft.active_view_id = view;
            Ok(tab)
        })
    }

    pub fn split_pane(
        &mut self,
        target: PaneId,
        direction: Direction,
        pane: PaneState,
    ) -> Result<PaneId> {
        self.compose(self.revision, |draft| {
            let (view, tab) = draft
                .pane_location(target)
                .ok_or(LayoutError::NotFound("pane"))?;
            let id = PaneId(draft.allocate()?);
            let state = draft.tab_mut(view, tab)?;
            state.root.insert(target, id, direction);
            state.panes.insert(id, pane);
            draft.focus_pane_inner(id)?;
            Ok(id)
        })
    }

    fn close_view_inner(&mut self, view: ViewId) -> Result<()> {
        if !self.views.contains_key(&view) {
            return Err(LayoutError::NotFound("view"));
        }
        if self.views.len() == 1 {
            return Err(LayoutError::Last("view"));
        }
        self.views.remove(&view);
        self.root = self
            .root
            .clone()
            .remove(view)
            .ok_or(LayoutError::Last("view"))?;
        if self.active_view_id == view {
            self.active_view_id = *self.root.first_leaf();
        }
        Ok(())
    }

    fn close_tab_inner(&mut self, view: ViewId, tab: TabId) -> Result<()> {
        let state = self
            .views
            .get_mut(&view)
            .ok_or(LayoutError::NotFound("view"))?;
        if !state.tabs.contains_key(&tab) {
            return Err(LayoutError::NotFound("tab"));
        }
        if state.tabs.len() == 1 {
            return self.close_view_inner(view);
        }
        state.tab_order = state.ordered_tabs();
        state.tab_order.retain(|id| *id != tab);
        state.tabs.remove(&tab);
        if state.active_tab_id == tab {
            state.active_tab_id = state.tab_order[0];
        }
        Ok(())
    }

    fn detach_pane(&mut self, pane: PaneId) -> Result<PaneState> {
        let (view, tab) = self
            .pane_location(pane)
            .ok_or(LayoutError::NotFound("pane"))?;
        let state = self.tab_mut(view, tab)?;
        if state.panes.len() > 1 {
            return state.remove(pane);
        }
        let pane_state = state
            .panes
            .get(&pane)
            .ok_or(LayoutError::NotFound("pane"))?
            .clone();
        self.close_tab_inner(view, tab)?;
        Ok(pane_state)
    }

    pub fn close_pane(&mut self, pane: PaneId) -> Result<()> {
        self.compose(self.revision, |draft| draft.detach_pane(pane).map(|_| ()))
    }

    pub fn close_tab(&mut self, view: ViewId, tab: TabId) -> Result<()> {
        self.compose(self.revision, |draft| draft.close_tab_inner(view, tab))
    }

    pub fn close_view(&mut self, view: ViewId) -> Result<()> {
        self.compose(self.revision, |draft| draft.close_view_inner(view))
    }

    fn focus_pane_inner(&mut self, pane: PaneId) -> Result<()> {
        let (view, tab) = self
            .pane_location(pane)
            .ok_or(LayoutError::NotFound("pane"))?;
        self.tab_mut(view, tab)?.active_pane_id = pane;
        self.views
            .get_mut(&view)
            .ok_or(LayoutError::NotFound("view"))?
            .active_tab_id = tab;
        self.active_view_id = view;
        Ok(())
    }

    pub fn focus_view(&mut self, view: ViewId) -> Result<()> {
        self.compose(self.revision, |draft| {
            if !draft.views.contains_key(&view) {
                return Err(LayoutError::NotFound("view"));
            }
            draft.active_view_id = view;
            Ok(())
        })
    }

    pub fn focus_tab(&mut self, view: ViewId, tab: TabId) -> Result<()> {
        self.compose(self.revision, |draft| {
            draft.tab_mut(view, tab)?;
            draft
                .views
                .get_mut(&view)
                .ok_or(LayoutError::NotFound("view"))?
                .active_tab_id = tab;
            draft.active_view_id = view;
            Ok(())
        })
    }

    pub fn focus_pane(&mut self, pane: PaneId) -> Result<()> {
        self.compose(self.revision, |draft| draft.focus_pane_inner(pane))
    }

    /// Move an existing pane next to the target without changing its ID or state.
    /// Empty source tabs/views are removed. Moving onto itself is rejected.
    pub fn move_pane(&mut self, pane: PaneId, target: PaneId, direction: Direction) -> Result<()> {
        self.compose(self.revision, |draft| {
            if pane == target {
                return Err(LayoutError::Invalid("move source equals target"));
            }
            let (view, tab) = draft
                .pane_location(target)
                .ok_or(LayoutError::NotFound("target pane"))?;
            let state = draft.detach_pane(pane)?;
            let destination = draft.tab_mut(view, tab)?;
            destination.root.insert(target, pane, direction);
            destination.panes.insert(pane, state);
            draft.focus_pane_inner(pane)
        })
    }

    pub fn set_view_ratio(&mut self, path: &[Branch], ratio: f64) -> Result<()> {
        self.compose(self.revision, |draft| {
            tree::validate_ratio(ratio)?;
            *draft.root.ratio_mut(path)? = ratio;
            Ok(())
        })
    }

    pub fn set_pane_ratio(
        &mut self,
        view: ViewId,
        tab: TabId,
        path: &[Branch],
        ratio: f64,
    ) -> Result<()> {
        self.compose(self.revision, |draft| {
            tree::validate_ratio(ratio)?;
            *draft.tab_mut(view, tab)?.root.ratio_mut(path)? = ratio;
            Ok(())
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.views.is_empty() {
            return Err(LayoutError::Invalid("workspace has no views"));
        }
        if self.views.len() > MAX_VIEWS {
            return Err(LayoutError::Limit("views"));
        }
        if !self.views.contains_key(&self.active_view_id) {
            return Err(LayoutError::Invalid("active view is missing"));
        }
        reachable(&self.root, &self.views)?;
        let mut ids = BTreeSet::new();
        let mut tabs = 0;
        let mut panes = 0;
        for (view_id, view) in &self.views {
            unique_id(&mut ids, view_id.0)?;
            if !view.rail_width.is_finite() || !(0.0..=4096.0).contains(&view.rail_width) {
                return Err(LayoutError::Invalid(
                    "rail width must be finite and within 0..=4096",
                ));
            }
            if view.tabs.is_empty() {
                return Err(LayoutError::Invalid("view has no tabs"));
            }
            let order = view.ordered_tabs();
            if order.len() != view.tabs.len() || order.iter().copied().collect::<BTreeSet<_>>() != view.tabs.keys().copied().collect() {
                return Err(LayoutError::Invalid("tab order must contain each tab exactly once"));
            }
            tabs += view.tabs.len();
            if tabs > MAX_TABS {
                return Err(LayoutError::Limit("tabs"));
            }
            if !view.tabs.contains_key(&view.active_tab_id) {
                return Err(LayoutError::Invalid("active tab is missing"));
            }
            for (tab_id, tab) in &view.tabs {
                unique_id(&mut ids, tab_id.0)?;
                if tab.panes.is_empty() {
                    return Err(LayoutError::Invalid("tab has no panes"));
                }
                panes += tab.panes.len();
                if panes > MAX_PANES {
                    return Err(LayoutError::Limit("panes"));
                }
                reachable(&tab.root, &tab.panes)?;
                if !tab.panes.contains_key(&tab.active_pane_id)
                    || !tab.panes.contains_key(&tab.primary_pane_id)
                {
                    return Err(LayoutError::Invalid("active or primary pane is missing"));
                }
                for pane in tab.panes.keys() {
                    unique_id(&mut ids, pane.0)?;
                }
            }
        }
        if ids.last().is_some_and(|id| self.next_id <= *id) {
            return Err(LayoutError::Invalid(
                "next_id must exceed every allocated ID",
            ));
        }
        Ok(())
    }
}

fn unique_id(ids: &mut BTreeSet<u64>, id: u64) -> Result<()> {
    if id == 0 || !ids.insert(id) {
        return Err(LayoutError::Invalid(
            "IDs must be nonzero and globally unique",
        ));
    }
    Ok(())
}

fn reachable<T: Ord, V>(root: &SplitNode<T>, map: &BTreeMap<T, V>) -> Result<()> {
    let mut leaves = Vec::new();
    root.collect(0, &mut leaves)?;
    let ids: BTreeSet<_> = leaves.iter().copied().collect();
    if ids.len() != leaves.len()
        || ids.len() != map.len()
        || ids.iter().any(|id| !map.contains_key(*id))
    {
        return Err(LayoutError::Invalid(
            "tree leaves must reference every map ID exactly once",
        ));
    }
    Ok(())
}

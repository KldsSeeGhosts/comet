use crate::{Direction, LayoutError, PaneId, Result, SplitNode, SplitTabLayout, TabId, ViewId, WorkspaceLayout};

fn swap_leaf(node: &mut SplitNode<PaneId>, a: PaneId, b: PaneId) {
    match node {
        SplitNode::Leaf { content } => {
            if *content == a { *content = b; } else if *content == b { *content = a; }
        }
        SplitNode::Split { first, second, .. } => {
            swap_leaf(first, a, b);
            swap_leaf(second, a, b);
        }
    }
}

fn replace_with_split(node: &mut SplitNode<PaneId>, target: PaneId, added: &SplitNode<PaneId>, direction: Direction) -> bool {
    match node {
        SplitNode::Leaf { content } if *content == target => {
            let old = node.clone();
            let (first, second) = if matches!(direction, Direction::Left | Direction::Up) {
                (added.clone(), old)
            } else { (old, added.clone()) };
            *node = SplitNode::Split {
                horizontal: matches!(direction, Direction::Left | Direction::Right),
                ratio: 0.5, first: Box::new(first), second: Box::new(second),
            };
            true
        }
        SplitNode::Leaf { .. } => false,
        SplitNode::Split { first, second, .. } => {
            replace_with_split(first, target, added, direction) || replace_with_split(second, target, added, direction)
        }
    }
}

impl WorkspaceLayout {
    /// Keep a fresh chat launcher when a user closes the final pane in a view.
    pub fn close_to_launcher(&mut self, pane: PaneId) -> Result<()> {
        self.compose(self.revision, |draft| {
            let (view, tab) = draft.pane_location(pane).ok_or(LayoutError::NotFound("pane"))?;
            if draft.views[&view].tabs.len() == 1 && draft.views[&view].tabs[&tab].panes.len() == 1 {
                draft.add_tab(view, Default::default())?;
            }
            draft.close_pane(pane)
        })
    }

    pub fn tab_location(&self, tab: TabId) -> Option<ViewId> {
        self.views.iter().find_map(|(id, view)| view.tabs.contains_key(&tab).then_some(*id))
    }

    /// Exchange pane positions, including across views. Identities and session state stay with the panes.
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> Result<()> {
        if a == b { return Err(LayoutError::Invalid("swap source equals target")); }
        self.compose(self.revision, |draft| {
            let (av, at) = draft.pane_location(a).ok_or(LayoutError::NotFound("source pane"))?;
            let (bv, bt) = draft.pane_location(b).ok_or(LayoutError::NotFound("target pane"))?;
            let active = draft.active_pane_id().ok_or(LayoutError::NotFound("active pane"))?;
            if (av, at) != (bv, bt) {
                let a_state = draft.tab_mut(av, at)?.panes.remove(&a).unwrap();
                let b_state = draft.tab_mut(bv, bt)?.panes.remove(&b).unwrap();
                draft.tab_mut(av, at)?.panes.insert(b, b_state);
                draft.tab_mut(bv, bt)?.panes.insert(a, a_state);
            }
            for view in draft.views.values_mut() {
                for tab in view.tabs.values_mut() {
                    swap_leaf(&mut tab.root, a, b);
                    for id in [&mut tab.active_pane_id, &mut tab.primary_pane_id] {
                        if *id == a { *id = b; } else if *id == b { *id = a; }
                    }
                }
            }
            draft.focus_pane_inner(active)
        })
    }

    fn take_tab(&mut self, view: ViewId, tab: TabId) -> Result<SplitTabLayout> {
        let state = self.tab_mut(view, tab)?.clone();
        self.close_tab_inner(view, tab)?;
        Ok(state)
    }

    /// Move a whole tab, preserving all nested panes and their proportions.
    pub fn move_tab(&mut self, tab: TabId, target: ViewId) -> Result<()> {
        self.compose(self.revision, |draft| {
            let source = draft.tab_location(tab).ok_or(LayoutError::NotFound("tab"))?;
            if !draft.views.contains_key(&target) { return Err(LayoutError::NotFound("target view")); }
            if source != target {
                let state = draft.take_tab(source, tab)?;
                draft.views.get_mut(&target).unwrap().insert_tab(tab, state);
            }
            draft.views.get_mut(&target).unwrap().active_tab_id = tab;
            draft.active_view_id = target;
            Ok(())
        })
    }

    /// Place a pane in the destination tab rail without changing its identity.
    pub fn pane_to_tab(&mut self, pane: PaneId, target: ViewId) -> Result<TabId> {
        self.compose(self.revision, |draft| {
            let (source, source_tab) = draft.pane_location(pane).ok_or(LayoutError::NotFound("pane"))?;
            if !draft.views.contains_key(&target) { return Err(LayoutError::NotFound("target view")); }
            if draft.tab_mut(source, source_tab)?.panes.len() == 1 {
                draft.move_tab(source_tab, target)?;
                return Ok(source_tab);
            }
            let state = draft.detach_pane(pane)?;
            let tab = TabId(draft.allocate()?);
            draft.views.get_mut(&target).unwrap().insert_tab(tab, SplitTabLayout::single(pane, state));
            draft.focus_pane_inner(pane)?;
            Ok(tab)
        })
    }

    pub fn reorder_tab(&mut self, tab: TabId, target: ViewId, before: Option<TabId>) -> Result<()> {
        self.compose(self.revision, |draft| {
            if before == Some(tab) && draft.tab_location(tab) == Some(target) { return Ok(()); }
            if before.is_some_and(|id| draft.tab_location(id) != Some(target)) {
                return Err(LayoutError::NotFound("insertion tab"));
            }
            draft.move_tab(tab, target)?;
            let view = draft.views.get_mut(&target).unwrap();
            let mut order = view.ordered_tabs();
            order.retain(|id| *id != tab);
            let index = before.and_then(|id| order.iter().position(|other| *other == id)).unwrap_or(order.len());
            order.insert(index, tab);
            view.tab_order = order;
            Ok(())
        })
    }

    pub fn pane_to_view(&mut self, pane: PaneId, target: ViewId, direction: Direction) -> Result<ViewId> {
        self.compose(self.revision, |draft| {
            if draft.pane(pane).is_none() { return Err(LayoutError::NotFound("pane")); }
            let added = draft.split_view(target, direction, Default::default())?;
            let placeholder = draft.active_pane_id().unwrap();
            draft.move_pane(pane, placeholder, Direction::Left)?;
            draft.close_pane(placeholder)?;
            draft.focus_pane_inner(pane)?;
            Ok(added)
        })
    }

    pub fn tab_to_view(&mut self, tab: TabId, target: ViewId, direction: Direction) -> Result<ViewId> {
        self.compose(self.revision, |draft| {
            if draft.tab_location(tab).is_none() { return Err(LayoutError::NotFound("tab")); }
            let added = draft.split_view(target, direction, Default::default())?;
            let placeholder_tab = draft.views[&added].active_tab_id;
            draft.move_tab(tab, added)?;
            draft.close_tab(added, placeholder_tab)?;
            Ok(added)
        })
    }

    /// Place every pane of a dragged tab beside the target as one unchanged subtree.
    pub fn merge_tab(&mut self, tab: TabId, target: PaneId, direction: Direction) -> Result<()> {
        self.compose(self.revision, |draft| {
            let source = draft.tab_location(tab).ok_or(LayoutError::NotFound("tab"))?;
            let (view, destination) = draft.pane_location(target).ok_or(LayoutError::NotFound("target pane"))?;
            if destination == tab { return Err(LayoutError::Invalid("cannot merge a tab into itself")); }
            let moving = draft.take_tab(source, tab)?;
            let active = moving.active_pane_id;
            let destination = draft.tab_mut(view, destination)?;
            if !replace_with_split(&mut destination.root, target, &moving.root, direction) {
                return Err(LayoutError::NotFound("target pane"));
            }
            destination.panes.extend(moving.panes);
            draft.focus_pane_inner(active)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaneState;

    #[test]
    fn swap_across_views_keeps_identity_focus_and_geometry() {
        let mut layout = WorkspaceLayout::new();
        let a = layout.active_pane_id().unwrap();
        layout.pane_mut(a).unwrap().session_id = Some("alpha".into());
        let first_view = layout.active_view_id;
        let second_view = layout.split_view(first_view, Direction::Right, PaneState { session_id: Some("beta".into()), ..Default::default() }).unwrap();
        let b = layout.active_pane_id().unwrap();
        layout.set_view_ratio(&[], 0.35).unwrap();
        let root = layout.root.clone();
        layout.swap_panes(a, b).unwrap();
        assert_eq!(layout.root, root);
        assert_eq!(layout.active_pane_id(), Some(b));
        assert_eq!(layout.pane_location(a).unwrap().0, second_view);
        assert_eq!(layout.pane_location(b).unwrap().0, first_view);
        assert_eq!(layout.pane(a).unwrap().session_id.as_deref(), Some("alpha"));
        assert_eq!(layout.pane(b).unwrap().session_id.as_deref(), Some("beta"));
        layout.validate().unwrap();
    }

    #[test]
    fn whole_tab_moves_and_merges_without_losing_children() {
        let mut layout = WorkspaceLayout::new();
        let target = layout.active_pane_id().unwrap();
        let destination_view = layout.active_view_id;
        let source_view = layout.split_view(destination_view, Direction::Right, PaneState::default()).unwrap();
        let first = layout.active_pane_id().unwrap();
        let second = layout.split_pane(first, Direction::Down, PaneState::default()).unwrap();
        let tab = layout.views[&source_view].active_tab_id;
        layout.set_pane_ratio(source_view, tab, &[], 0.7).unwrap();
        let subtree = layout.views[&source_view].tabs[&tab].root.clone();
        layout.move_tab(tab, destination_view).unwrap();
        assert_eq!(layout.views.len(), 1);
        assert_eq!(layout.views[&destination_view].tabs[&tab].root, subtree);
        layout.merge_tab(tab, target, Direction::Left).unwrap();
        assert_eq!(layout.views[&destination_view].tabs.len(), 1);
        assert_eq!(layout.active_pane_id(), Some(second));
        let destination = &layout.views[&destination_view].tabs[&layout.views[&destination_view].active_tab_id];
        assert_eq!(destination.panes.len(), 3);
        let SplitNode::Split { first, .. } = &destination.root else { panic!("missing merged split") };
        assert_eq!(**first, subtree);
        layout.validate().unwrap();
        let before = layout.clone();
        let tab = layout.views[&destination_view].active_tab_id;
        assert!(layout.merge_tab(tab, target, Direction::Right).is_err());
        assert_eq!(layout, before);
    }

    #[test]
    fn pane_can_become_tab_then_full_view_without_identity_changes() {
        let mut layout = WorkspaceLayout::new();
        let first = layout.active_pane_id().unwrap();
        let second = layout.split_pane(first, Direction::Right, PaneState::default()).unwrap();
        let view = layout.active_view_id;
        let tab = layout.pane_to_tab(second, view).unwrap();
        assert_eq!(layout.views[&view].tabs.len(), 2);
        assert_eq!(layout.pane_location(second), Some((view, tab)));
        let added = layout.tab_to_view(tab, view, Direction::Down).unwrap();
        assert_eq!(layout.views.len(), 2);
        assert_eq!(layout.pane_location(second), Some((added, tab)));
        assert_eq!(layout.active_pane_id(), Some(second));
        layout.validate().unwrap();
    }

    #[test]
    fn tab_order_survives_moves_and_serde_and_rejects_duplicates() {
        let mut layout = WorkspaceLayout::new();
        let view = layout.active_view_id;
        let first = layout.views[&view].active_tab_id;
        let second = layout.add_tab(view, PaneState::default()).unwrap();
        layout.reorder_tab(second, view, Some(first)).unwrap();
        assert_eq!(layout.views[&view].ordered_tabs(), vec![second, first]);
        let decoded: WorkspaceLayout = serde_json::from_str(&serde_json::to_string(&layout).unwrap()).unwrap();
        assert_eq!(decoded, layout);
        let before = layout.clone();
        assert!(layout.compose(layout.revision, |draft| {
            draft.views.get_mut(&view).unwrap().tab_order = vec![second, second];
            Ok(())
        }).is_err());
        assert_eq!(layout, before);
        layout.close_tab(view, second).unwrap();
        assert_eq!(layout.views[&view].ordered_tabs(), vec![first]);
    }
}

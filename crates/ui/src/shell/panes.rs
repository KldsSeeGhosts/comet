//! Glue between the shell and the pane host ([`crate::pane`]): the per-frame
//! workspace snapshot for the renderer, the split/focus/close handlers behind
//! the `workspace::` actions, the WS3 divider-drag/equalize commit path, the
//! tool picker (the verified ⌘D contract), tab close, the pane-header context
//! menu, the focused-pane retarget loop that keeps the live
//! transcript/composer on the focused pane, and the WS4 tab/pane drag state
//! machine (source → per-sample [`resolve_drop`] preview → commit on
//! mouse-up).
//!
//! This lives in the shell module tree (like `tabs.rs`/`spaces.rs`) because it
//! reads Shell's private fields; everything structural sits in `crate::pane`.

use super::*;

use crate::pane::chrome::{tab_mark, TabChip};
use crate::pane::hit_test::{self, DragSource, DropPlan};
use crate::pane::render::{workspace_outlet, PaneSnap, ViewSnap, WorkspaceSnap};
use crate::pane::{
    ratio_from_pointer, DividerTarget, DragSplitState, PickerCommit, ToolKind, DIVIDER_HIT_PX,
    EQUALIZE_RATIO,
};
use zeron_workspace::{Direction, PaneId, PaneMode, TabId, ViewId};

impl Shell {
    /// Whether the content area renders the workspace tree: any split, extra
    /// tab, or extra pane beyond the untouched default. False = today's exact
    /// single-chat code path (the parity gate).
    pub(super) fn workspace_mode(&self) -> bool {
        !self.workspace.is_trivial()
    }

    /// The workspace tree as the chat outlet. Dormant transcripts are created
    /// lazily here (render pass, like the lazy terminal panel), then the tree
    /// is snapshotted and handed to [`crate::pane::render`] with the live
    /// composer block (WS3 re-homing: the composer lives in the focused pane,
    /// the outer dock is suppressed while `workspace_mode()` holds).
    pub(super) fn render_workspace_outlet(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // Dormant views: one read-only transcript per session-bound pane that
        // isn't the focused one. Idempotent; entries persist across frames.
        let focused = self.workspace.focused_pane();
        let needs: Vec<PaneId> = self
            .workspace
            .layout
            .views
            .values()
            .flat_map(|view| view.tabs.values())
            .flat_map(|tab| tab.panes.iter())
            .filter(|(id, state)| {
                state.session_id.is_some()
                    && focused != Some(**id)
                    && !self.workspace.pane_transcripts.contains_key(id)
            })
            .map(|(id, _)| *id)
            .collect();
        for pane in needs {
            self.workspace
                .transcript_for_pane(pane, self.state.clone(), cx);
        }
        let live = self.transcript.clone();
        let composer_block = self.render_pane_composer_block(cx);
        let dormant = self.workspace.pane_transcripts.clone();
        let snap = Self::workspace_snapshot(&self.workspace, &self.state, &dormant, cx);
        // WS4: the active drag's preview, converted to outlet-relative space.
        let drag_preview = self.split_drag.as_ref().and_then(preview_bounds);
        workspace_outlet(cx, &theme, &snap, &live, composer_block, drag_preview)
    }

    /// The live composer strip hosted INSIDE the focused chat pane (WS3
    /// re-homing): the same `Entity<Composer>` the outer dock used, in a
    /// centered max-width column with the jump-to-bottom pill floating above
    /// it. The pane renderer overlays this at the focused pane's bottom.
    fn render_pane_composer_block(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (has_spaces, no_project, has_selection) = {
            let state = self.state.read(cx);
            (
                !state.spaces.is_empty(),
                state.no_project,
                state.selected_chat.is_some(),
            )
        };
        let has_appshots = !self.composer.read(cx).staged_appshots().is_empty();
        if !(has_spaces || no_project || has_appshots) {
            return None;
        }
        let mut block = div()
            .id("pane-composer")
            .relative()
            .w_full()
            .px(px(10.0))
            .pb(px(10.0))
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(px(crate::composer::COMPOSER_MAX_WIDTH))
                    .mx_auto()
                    .child(self.composer.clone()),
            );
        if has_selection {
            if let Some(pill) = self.render_jump_to_bottom(cx) {
                block = block.child(pill);
            }
        }
        Some(block.into_any_element())
    }

    /// Flatten the layout into the renderer's immutable snapshot. Titles read
    /// AppState once per frame (chat titles for session-bound panes; labels
    /// and mode names otherwise).
    fn workspace_snapshot(
        workspace: &crate::pane::PaneHost,
        state: &Entity<AppState>,
        dormant: &std::collections::HashMap<PaneId, Entity<Transcript>>,
        cx: &App,
    ) -> WorkspaceSnap {
        let layout = &workspace.layout;
        let global_focus = layout.active_pane_id();
        let pane_title = |session: &Option<String>, mode: zeron_workspace::PaneMode, label: &Option<String>| -> SharedString {
            if let Some(label) = label {
                return label.clone().into();
            }
            match session.as_deref().and_then(|id| {
                state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|chat| chat.id == id)
                    .and_then(|chat| chat.title.clone())
            }) {
                Some(title) => SharedString::from(transcript::single_line(&title)),
                None => SharedString::from(match (session.is_some(), mode) {
                    (_, zeron_workspace::PaneMode::Terminal) => "Terminal",
                    (true, _) => "Session",
                    (false, _) => "New session",
                }),
            }
        };
        let views = layout
            .views
            .iter()
            .map(|(view_id, view)| {
                let active_tab_id = view.active_tab_id;
                let chips = view
                    .ordered_tabs()
                    .iter()
                    .filter_map(|tab_id| view.tabs.get(tab_id).map(|tab| (*tab_id, tab)))
                    .map(|(tab_id, tab)| {
                        let active = tab.panes.get(&tab.active_pane_id);
                        let label = active
                            .map(|pane_state| {
                                pane_title(
                                    &pane_state.session_id,
                                    pane_state.mode,
                                    &pane_state.label,
                                )
                            })
                            .unwrap_or_else(|| SharedString::from("Tab"));
                        let mark = active
                            .map(|pane_state| {
                                tab_mark(pane_state.mode, pane_state.provider_key.as_deref())
                            })
                            .unwrap_or_else(|| tab_mark(PaneMode::Chat, None));
                        TabChip {
                            tab_id,
                            label,
                            active: tab_id == active_tab_id,
                            mark,
                        }
                    })
                    .collect();
                let panes = view
                    .tabs
                    .get(&active_tab_id)
                    .map(|tab| {
                        tab.panes
                            .iter()
                            .map(|(pane_id, pane_state)| PaneSnap {
                                pane: *pane_id,
                                mode: pane_state.mode,
                                title: pane_title(
                                    &pane_state.session_id,
                                    pane_state.mode,
                                    &pane_state.label,
                                ),
                                mark: tab_mark(
                                    pane_state.mode,
                                    pane_state.provider_key.as_deref(),
                                ),
                                has_session: pane_state.session_id.is_some(),
                                focused: global_focus == Some(*pane_id),
                                dormant_transcript: dormant.get(pane_id).cloned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ViewSnap {
                    view_id: *view_id,
                    active_tab_id,
                    chips,
                    active_tab_root: view
                        .tabs
                        .get(&active_tab_id)
                        .map(|tab| tab.root.clone())
                        .unwrap_or_else(|| {
                            zeron_workspace::SplitNode::leaf(
                                view.tabs.values().next().map(|tab| tab.active_pane_id).unwrap_or(PaneId(0)),
                            )
                        }),
                    panes,
                }
            })
            .collect();
        WorkspaceSnap {
            root: layout.root.clone(),
            views,
            pane_bounds: workspace.pane_bounds_handle(),
            view_bounds: workspace.view_bounds_handle(),
            chip_bounds: workspace.chip_bounds_handle(),
        }
    }

    // ------------------------------------------------------------------
    // Focus / retarget
    // ------------------------------------------------------------------

    /// Click-to-focus (and the tab-switch fallback): focus the pane in the
    /// engine, then retarget the live views to it.
    pub(crate) fn focus_workspace_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.focus_pane(pane).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// Tab chip click: make the tab active (and its view), then retarget to
    /// that tab's active pane.
    pub(crate) fn switch_workspace_tab(&mut self, view: ViewId, tab: TabId, cx: &mut Context<Self>) {
        if self.workspace.focus_tab(view, tab).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// Pane header ×: engine `close_pane` (an emptied tab closes; an emptied
    /// view closes; the LAST pane/view errors and this no-ops silently).
    pub(crate) fn close_workspace_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.close_pane(pane).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// Tab chip ×: engine `close_tab`. Engine semantics (documented WS3
    /// deviation — the empty-view launcher needs engine changes that are out
    /// of this workstream's scope): closing a view's LAST tab closes the
    /// VIEW, and closing the last remaining view's last tab ERRORS (no-op),
    /// so the app can never be left without a view.
    pub(crate) fn close_workspace_tab(&mut self, view: ViewId, tab: TabId, cx: &mut Context<Self>) {
        if self.workspace.close_tab(view, tab).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    // ------------------------------------------------------------------
    // Divider drag / equalize (WS3)
    // ------------------------------------------------------------------

    /// Divider press: latches the drag so hover fades pause until release.
    /// The ratio math is absolute (pointer position within the split
    /// container's bounds), so no drag anchor is needed. The divider sits
    /// outside every pane container and occludes, so this press never
    /// focuses the pane under it.
    pub(crate) fn begin_divider_drag(&mut self, cx: &mut Context<Self>) {
        self.divider_dragging = true;
        cx.notify();
    }

    /// Hover fades for a divider (rendered from `pane/render.rs`, which
    /// cannot see Shell's private fields): suppressed while any divider drag
    /// is live so the strip never re-fades mid-drag.
    pub(crate) fn note_divider_hover(&mut self, key: &str, hovered: bool) {
        if !self.divider_dragging {
            crate::motion::set_hover(key, hovered, self.reduced_motion);
        }
    }

    /// Mouse-up (or out) releases the latch. No tween on release: the last
    /// dragged ratio IS the resting state (direct manipulation). WS5: the
    /// drag's per-sample ratio commits latched dirty but never armed a save
    /// (mid-gesture flushes are forbidden) — arm it here, on the commit.
    pub(crate) fn end_divider_drag(&mut self, cx: &mut Context<Self>) {
        if !self.divider_dragging {
            return;
        }
        self.divider_dragging = false;
        self.schedule_workspace_layout_save(cx);
        cx.notify();
    }

    /// `on_drag_move` on a split container: convert the pointer sample into a
    /// live ratio commit for the node the payload names. Containers that do
    /// not own the divider (ancestors in capture phase) filter by target.
    pub(crate) fn apply_divider_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::pane::DividerDrag>,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        let target = drag.target.clone();
        let horizontal = drag.horizontal;
        let (origin, length, pointer) = if horizontal {
            (
                f32::from(event.bounds.origin.x),
                f32::from(event.bounds.size.width),
                f32::from(event.event.position.x),
            )
        } else {
            (
                f32::from(event.bounds.origin.y),
                f32::from(event.bounds.size.height),
                f32::from(event.event.position.y),
            )
        };
        let Some(ratio) = ratio_from_pointer(pointer, origin, length, DIVIDER_HIT_PX) else {
            return;
        };
        self.apply_divider_ratio(&target, ratio, cx);
    }

    /// Double-click on a divider: equalize that node to 0.5/0.5 (§1). Direct
    /// snap — the manual-tween plumbing is keyed to the shell's width tweens;
    /// a ratio spring is deferred with the WS6 motion pass.
    pub(crate) fn equalize_divider(&mut self, target: &DividerTarget, cx: &mut Context<Self>) {
        self.apply_divider_ratio(target, EQUALIZE_RATIO, cx);
    }

    fn apply_divider_ratio(
        &mut self,
        target: &DividerTarget,
        ratio: f64,
        cx: &mut Context<Self>,
    ) {
        let result = match target {
            DividerTarget::View { path } => self.workspace.set_view_ratio(path, ratio),
            DividerTarget::Pane { view, tab, path } => {
                self.workspace.set_pane_ratio(*view, *tab, path, ratio)
            }
        };
        if result.is_ok() {
            // Mid-drag samples hit the gesture guard inside; the drag end
            // arms the save. Equalize (double-click) arms immediately.
            self.note_workspace_mutation(cx);
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // WS4: tab/pane drag & drop (tab-to-edge splits, drop ring, re-dock,
    // cross-view moves)
    // ------------------------------------------------------------------

    /// The pure resolution's input: the paint-time registries flattened into
    /// a [`hit_test::WorkspaceGeometry`]. Stale registry entries (a frame
    /// behind an engine change) filter out against the live layout, so a
    /// mid-drag mutation resolves against what is actually on screen.
    fn workspace_geometry(&self, content: gpui::Bounds<gpui::Pixels>) -> hit_test::WorkspaceGeometry {
        let layout = &self.workspace.layout;
        let panes = self
            .workspace
            .pane_bounds
            .borrow()
            .iter()
            .filter_map(|(pane, bounds)| {
                let (view, tab) = layout.pane_location(*pane)?;
                // Only include panes from the view's active tab - stale
                // bounds from inactive tabs must not participate in hit-testing.
                if !layout.views.get(&view).is_some_and(|v| v.active_tab_id == tab) {
                    return None;
                }
                Some(hit_test::PaneRect {
                    pane: *pane,
                    view,
                    tab,
                    rect: hit_test::Rect::from_bounds(*bounds),
                })
            })
            .collect();
        let views = self
            .workspace
            .view_bounds
            .borrow()
            .iter()
            .filter_map(|(view, bounds)| {
                let tabs = layout.views.get(view)?.tabs.len();
                Some(hit_test::ViewRect {
                    view: *view,
                    rect: hit_test::Rect::from_bounds(*bounds),
                    tab_count: tabs,
                })
            })
            .collect();
        let mut strips: std::collections::BTreeMap<ViewId, Vec<hit_test::ChipRect>> = Default::default();
        for ((view, tab), bounds) in self.workspace.chip_bounds.borrow().iter() {
            if layout
                .views
                .get(view)
                .is_some_and(|v| v.tabs.contains_key(tab))
            {
                strips.entry(*view).or_default().push(hit_test::ChipRect {
                    tab: *tab,
                    rect: hit_test::Rect::from_bounds(*bounds),
                });
            }
        }
        for chips in strips.values_mut() {
            // Display order is left→right within a strip row.
            chips.sort_by(|a, b| {
                a.rect
                    .x
                    .total_cmp(&b.rect.x)
                    .then_with(|| a.tab.cmp(&b.tab))
            });
        }
        hit_test::WorkspaceGeometry {
            content: hit_test::Rect::from_bounds(content),
            panes,
            views,
            strips: strips.into_iter().collect(),
        }
    }

    /// `on_drag_move` on the workspace outlet: resolve the sample purely and
    /// store it. The preview overlay + ghost re-render off this state; the
    /// resolution's anchor pane feeds the next sample's flip smoothing
    /// ([`hit_test::FLIP_SMOOTH_PX`]).
    pub(crate) fn apply_split_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::pane::TabSplitDrag>,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        let (source, pointer) = (drag.source, event.event.position);
        let anchor = self.split_drag.as_ref().and_then(|s| s.resolution.anchor);
        let geom = self.workspace_geometry(event.bounds);
        let resolution = hit_test::resolve_drop(
            &geom,
            f32::from(pointer.x),
            f32::from(pointer.y),
            source,
            anchor,
        );
        let next = DragSplitState {
            source,
            pointer,
            root_bounds: event.bounds,
            resolution,
        };
        if self.split_drag.as_ref() != Some(&next) {
            self.split_drag = Some(next);
            cx.notify();
        }
    }

    /// Mouse-up over the outlet: commit the last resolved plan. Invalid
    /// drops ([`DropPlan::None`]) and engine rejections (the guards below)
    /// no-op; a real commit retargets the live views to the newly focused
    /// pane (the engine ops all focus the moved/dropped content).
    pub(crate) fn commit_split_drop(
        &mut self,
        payload: &crate::pane::TabSplitDrag,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.split_drag.take() else {
            return;
        };
        // Sidebar session drags create a new pane bound to the dragged
        // session rather than moving an existing workspace tab/pane.
        if payload.source == DragSource::SidebarSession {
            self.commit_sidebar_split(state.resolution.plan, payload, cx);
            return;
        }
        if self.apply_drop_plan(state.resolution.plan, payload.source) {
            self.retarget_to_focused_pane(cx);
        } else {
            cx.notify();
        }
    }

    /// Commit a sidebar-session drag: split the target pane and bind the
    /// new half to the dragged session. The session_id on the payload
    /// identifies which chat to bind.
    fn commit_sidebar_split(
        &mut self,
        plan: DropPlan,
        payload: &crate::pane::TabSplitDrag,
        cx: &mut Context<Self>,
    ) {
        let session_id = payload.session_id.clone();
        let direction = match plan {
            DropPlan::SplitPane { direction, .. } => direction,
            DropPlan::SplitView { direction, .. } => direction,
            // Center or reorder with no existing split target: split right
            // from the focused pane as the default entry.
            DropPlan::MoveIntoPane { .. } | DropPlan::ReorderStrip { .. } => {
                zeron_workspace::Direction::Right
            }
            DropPlan::None => {
                cx.notify();
                return;
            }
        };
        let target = match plan {
            DropPlan::SplitPane { pane, .. } => pane,
            _ => match self.workspace.focused_pane() {
                Some(p) => p,
                None => {
                    cx.notify();
                    return;
                }
            },
        };
        let new_pane = crate::pane::chat_pane_state();
        if let Ok(pane_id) = self.workspace.layout.split_pane(target, direction, new_pane) {
            let _ = self.workspace.set_pane_session(pane_id, session_id);
            self.workspace.layout.focus_pane(pane_id).ok();
            self.retarget_to_focused_pane(cx);
        } else {
            cx.notify();
        }
    }

    /// Drag ended without a commit (mouse-up outside the outlet — the
    /// sidebar, status bar — or a stray mouse-up after an in-place cancel):
    /// clear the state so no stale preview lingers. Idempotent.
    pub(crate) fn cancel_split_drag(&mut self, cx: &mut Context<Self>) {
        if self.split_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Accept a sidebar session drop on the single-pane content area (the
    /// workspace outlet is not rendered, so this is the entry point for
    /// drag-to-split from the default screen). Splits right from the
    /// focused pane.
    pub(crate) fn accept_sidebar_session_drop(
        &mut self,
        payload: &crate::pane::TabSplitDrag,
        cx: &mut Context<Self>,
    ) {
        self.split_drag = None;
        let session_id = payload.session_id.clone();
        let Some(target) = self.workspace.focused_pane() else {
            cx.notify();
            return;
        };
        let new_pane = crate::pane::chat_pane_state();
        if let Ok(pane_id) = self
            .workspace
            .layout
            .split_pane(target, zeron_workspace::Direction::Right, new_pane)
        {
            let _ = self.workspace.set_pane_session(pane_id, session_id);
            self.workspace.layout.focus_pane(pane_id).ok();
            self.retarget_to_focused_pane(cx);
        } else {
            cx.notify();
        }
    }

    /// The plan → engine-op mapping. Returns whether anything changed.
    /// Every engine error is a silent no-op: the engine commits atomically,
    /// so a rejected drop leaves `validate()` passing and the tree intact.
    fn apply_drop_plan(&mut self, plan: DropPlan, source: DragSource) -> bool {
        let result = match (source, plan) {
            (_, DropPlan::None) => return false,
            // Sidebar session drags are handled in commit_sidebar_split_drop,
            // not here - they need the session_id from the payload.
            (DragSource::SidebarSession, _) => return false,
            // Center drop. Same view + append = the no-op restore (activate
            // the tab); anything else moves the tab (append when
            // `tab_before` is `None`).
            (DragSource::TabChip(tab, src_view), DropPlan::MoveIntoPane { view, tab_before }) => {
                if src_view == view && tab_before.is_none() {
                    self.workspace.focus_tab(view, tab)
                } else {
                    self.workspace.reorder_tab_in_view(tab, view, tab_before)
                }
            }
            // Header on a strip: the pane becomes a tab of that view.
            (DragSource::PaneHeader(pane), DropPlan::MoveIntoPane { view, .. }) => {
                self.workspace.pane_header_to_tab(pane, view).map(|_| ())
            }
            // Interior edge: the dragged content becomes the half-pane beside
            // the target (`merge_tab` for whole tabs, `move_pane` for panes).
            (DragSource::TabChip(tab, _), DropPlan::SplitPane { pane, direction }) => {
                self.workspace.merge_tab_into_pane(tab, pane, direction)
            }
            (DragSource::PaneHeader(src), DropPlan::SplitPane { pane, direction }) => {
                self.workspace.move_pane_beside(src, pane, direction)
            }
            // Workspace-outer edge: a new top-level region is minted and the
            // dragged content re-docks into it.
            (DragSource::TabChip(tab, _), DropPlan::SplitView { view, direction }) => self
                .workspace
                .tab_to_adjacent_view(tab, view, direction)
                .map(|_| ()),
            (DragSource::PaneHeader(pane), DropPlan::SplitView { view, direction }) => self
                .workspace
                .pane_to_adjacent_view(pane, view, direction)
                .map(|_| ()),
            // Same-strip drop: reorder (the engine no-ops in-place restores).
            (DragSource::TabChip(tab, _), DropPlan::ReorderStrip { view, before }) => {
                self.workspace.reorder_tab_in_view(tab, view, before)
            }
            // A header "reordered" on a strip still joins it (append; the
            // engine op carries no insertion position).
            (DragSource::PaneHeader(pane), DropPlan::ReorderStrip { view, .. }) => {
                self.workspace.pane_header_to_tab(pane, view).map(|_| ())
            }
        };
        result.is_ok()
    }

    // ------------------------------------------------------------------
    // Tool picker (the verified ⌘D contract, interaction-truth §2)
    // ------------------------------------------------------------------

    /// ⌘D / ⇧⌘D: OPEN the tool picker anchored to the focused pane's
    /// top-left. NO split happens here — the split commits when a row is
    /// picked, so Esc cancels with zero layout change. Pressing the chord
    /// again toggles the picker closed.
    pub(crate) fn split_workspace_pane(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        let anchor = self.focused_pane_anchor();
        self.tool_picker = Some(crate::pane::ToolPickerState {
            commit: PickerCommit::SplitPane(direction),
            anchor,
        });
        cx.notify();
    }

    /// "+" in a view's tab strip: open the tool picker anchored at the
    /// trigger; a picked row commits as `add_tab` to THAT view (no split).
    /// Re-pressing the SAME trigger toggles the picker closed (checked
    /// before the overlay guard, which would otherwise see our own picker).
    pub(crate) fn open_workspace_tool_picker_for_tab(
        &mut self,
        view: ViewId,
        anchor: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = self.tool_picker {
            if matches!(picker.commit, PickerCommit::AddTab { view: v } if v == view) {
                self.close_tool_picker(cx);
                return;
            }
        }
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        // Anchor just below the trigger; menu_at snaps to the window edge.
        self.tool_picker = Some(crate::pane::ToolPickerState {
            commit: PickerCommit::AddTab { view },
            anchor: gpui::point(anchor.x, anchor.y + gpui::px(26.0)),
        });
        cx.notify();
    }

    /// The picker anchor: the focused pane's last painted top-left (§2).
    /// The untouched default layout has no pane canvases yet (the parity path
    /// renders no tree), so the fallback is the content area's top-left under
    /// the overlaid titlebar — `menu_at` snaps to the window margins, so the
    /// approximation stays on-screen.
    fn focused_pane_anchor(&self) -> gpui::Point<gpui::Pixels> {
        if let Some(bounds) = self.workspace.focused_pane_bounds() {
            return gpui::point(bounds.origin.x, bounds.origin.y);
        }
        gpui::point(gpui::px(12.0), gpui::px(Theme::TITLEBAR_HEIGHT + 12.0))
    }

    pub(crate) fn close_tool_picker(&mut self, cx: &mut Context<Self>) {
        if self.tool_picker.take().is_some() {
            cx.notify();
        }
    }

    /// A picked row: the zero-layout-until-now contract ends here — commit
    /// the split (or the tab add) with the row's tool as the new pane state.
    pub(crate) fn commit_tool_picker(
        &mut self,
        commit: PickerCommit,
        kind: ToolKind,
        cx: &mut Context<Self>,
    ) {
        let result = match commit {
            PickerCommit::SplitPane(direction) => self
                .workspace
                .split_focused_pane_with(direction, crate::pane::tool_pane_state(kind))
                .map(|_| ()),
            PickerCommit::AddTab { view } => self
                .workspace
                .add_tab_with(view, crate::pane::tool_pane_state(kind))
                .map(|_| ()),
        };
        if result.is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// The workspace overlays: the tool picker + the pane-header context
    /// menu (both `menu_at` popovers, mirrored from the chat context menu).
    pub(super) fn render_workspace_overlays(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if let Some(picker) = self.tool_picker {
            overlays.push(self.render_tool_picker(picker, cx));
        }
        if let Some(menu) = self.pane_menu.get().cloned() {
            let closing = self.pane_menu.closing_since();
            overlays.push(Self::render_pane_menu(menu, closing, cx));
        }
        overlays
    }

    fn render_tool_picker(
        &mut self,
        picker: crate::pane::ToolPickerState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let mut card = popover::popover_card(&theme)
            .w(px(236.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_tool_picker(cx)))
            .flex()
            .flex_col()
            .child(popover::menu_heading(&theme, "Choose a tool"));
        for row in crate::pane::TOOL_PICKER_ROWS {
            let row_id = format!("ws-tool-{:?}", row.kind);
            let (kind, commit) = (row.kind, picker.commit);
            card = card.child(
                popover::menu_row(&theme, false, row_id.clone())
                    .id(SharedString::from(row_id))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tool_picker(cx);
                        this.commit_tool_picker(commit, kind, cx);
                    }))
                    .child(icon(row.icon).size(px(14.0)).text_color(theme.text_muted))
                    .child(
                        div()
                            .flex_1()
                            .child(SharedString::from(row.label)),
                    )
                    .children(row.badge.map(|badge| {
                        div()
                            .px(px(5.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(theme.hairline(0.10))
                            .text_size(crate::typography::ui_rems(9.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(badge))
                    })),
            );
        }
        popover::menu_at("ws-tool-picker", picker.anchor, card.into_any_element(), None)
    }

    // ------------------------------------------------------------------
    // Pane-header context menu (§6)
    // ------------------------------------------------------------------

    /// Right-click on a pane header: focus that pane (the composer returns
    /// from its ghost, §6) and open the split/close menu at the pointer.
    pub(crate) fn open_workspace_pane_menu(
        &mut self,
        pane: PaneId,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.focus_workspace_pane(pane, cx);
        self.pane_menu.open(crate::pane::PaneMenuState { pane, position });
        cx.notify();
    }

    pub(crate) fn close_workspace_pane_menu(&mut self, cx: &mut Context<Self>) {
        if self.pane_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.pane_menu);
            cx.notify();
        }
    }

    fn render_pane_menu(
        menu: crate::pane::PaneMenuState,
        closing: Option<std::time::Instant>,
        cx: &Context<'_, Shell>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let pane = menu.pane;
        let row =
            |id: &'static str, label: SharedString, chord: &'static str| {
                popover::menu_row(&theme, false, id)
                    .id(id)
                    .child(
                        div()
                            .flex_1()
                            .text_size(crate::typography::ui_rems(11.5))
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(chord)),
                    )
            };
        let card = popover::popover_card(&theme)
            .w(px(232.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_workspace_pane_menu(cx)))
            .flex()
            .flex_col()
            .child(
                row("ws-menu-split-right", SharedString::from("Split pane right"), "⌘D")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_workspace_pane_menu(cx);
                        this.split_workspace_pane(Direction::Right, cx);
                    })),
            )
            .child(
                row("ws-menu-split-down", SharedString::from("Split pane down"), "⇧⌘D")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_workspace_pane_menu(cx);
                        this.split_workspace_pane(Direction::Down, cx);
                    })),
            )
            .child(
                row("ws-menu-view-right", SharedString::from("Split view right"), "⌥⌘D")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_workspace_pane_menu(cx);
                        this.split_workspace_view(Direction::Right, cx);
                    })),
            )
            .child(
                row("ws-menu-view-down", SharedString::from("Split view down"), "⌥⌘⇧D")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_workspace_pane_menu(cx);
                        this.split_workspace_view(Direction::Down, cx);
                    })),
            )
            .child(popover::menu_separator())
            .child(
                row("ws-menu-close-pane", SharedString::from("Close pane"), "")
                    .text_color(theme.danger)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_workspace_pane_menu(cx);
                        this.close_workspace_pane(pane, cx);
                    })),
            );
        popover::menu_at(
            "ws-pane-menu",
            menu.position,
            card.into_any_element(),
            closing,
        )
    }

    // ------------------------------------------------------------------
    // Actions (workspace::SplitViewRight / SplitViewDown /
    // CloseSplitView — the picker-backed pane splits live above)
    // ------------------------------------------------------------------

    /// ⌥⌘D / ⌥⌘⇧D: split the workspace at the focused view (a second
    /// top-level region with its own tab strip). The engine's `split_view`
    /// provisions the new view's single tab + pane and focuses it. Split
    /// views keep the immediate new-view behavior (Super's new view already
    /// gets a default chat — verified §4), no picker.
    pub(crate) fn split_workspace_view(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        if self.workspace.split_focused_view(direction).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// ⌥⌘W: close the focused view. No-op on the last view (engine guard).
    pub(crate) fn close_workspace_view(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        if self.workspace.close_focused_view().is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    // ------------------------------------------------------------------
    // Retarget loop
    // ------------------------------------------------------------------

    /// Make the shell's live views follow the focused pane: prune dead cache
    /// entries, select the pane's chat in AppState (which re-anchors the
    /// transcript watch and the composer — the exact mechanism a sidebar click
    /// uses), and move composer keyboard focus with it. `None` sessions land
    /// on the new-thread canvas, whose mint-on-send binds the pane via
    /// [`Self::sync_workspace_selection`]. Every mutation path funnels here,
    /// which is also where the WS5 save arms.
    fn retarget_to_focused_pane(&mut self, cx: &mut Context<Self>) {
        self.sync_selection_to_focused_pane(cx);
        // Keyboard focus follows the focused pane's composer (the ghost swap
        // contract); a no-op while an input the user chose keeps focus.
        self.focus_composer(cx);
        cx.notify();
        self.note_workspace_mutation(cx);
    }

    /// The selection half of [`Self::retarget_to_focused_pane`], without the
    /// composer focus steal: prune dead cache entries and select the focused
    /// pane's chat in AppState. Used by the stale-session prune, where focus
    /// must not move just because a remote chat got deleted.
    fn sync_selection_to_focused_pane(&mut self, cx: &mut Context<Self>) {
        self.workspace.prune_caches();
        let session = self
            .workspace
            .focused_pane()
            .and_then(|pane| self.workspace.layout.pane(pane))
            .and_then(|state| state.session_id.clone());
        let selected = self.state.read(cx).selected_chat.clone();
        if selected != session {
            self.state
                .update(cx, |state, cx| state.select_chat(session, cx));
        }
    }

    /// AppState selection → pane binding. Called from the shell's state
    /// observation on chat switches, so every selection path converges here:
    /// sidebar click, jump shortcut, banner, canvas mint-on-send (the
    /// composer selects the new chat id, this binds it to the focused pane).
    pub(crate) fn sync_workspace_selection(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat.clone();
        self.workspace.sync_focused_session(selected.as_deref());
        self.note_workspace_mutation(cx);
    }

    // ------------------------------------------------------------------
    // WS5: per-space layout persistence (workspace-layout.json)
    // ------------------------------------------------------------------

    /// A workspace mutation latched dirty (structure, ratio, focus, tab, or
    /// session binding — every [`PaneHost`] wrapper). Arm the debounced store
    /// write. Never mid-gesture: divider drags commit a ratio per pointer
    /// sample, so a live drag skips the arm and the gesture's END
    /// ([`Self::end_divider_drag`], [`Self::commit_split_drop`]) schedules
    /// the flush instead.
    fn note_workspace_mutation(&mut self, cx: &mut Context<Self>) {
        if !self.workspace.is_dirty() {
            return;
        }
        self.schedule_workspace_layout_save(cx);
    }

    fn schedule_workspace_layout_save(&mut self, cx: &mut Context<Self>) {
        if self.divider_dragging || self.split_drag.is_some() {
            return;
        }
        if !self.workspace.is_dirty() && !self.workspace_layouts.needs_save() {
            return;
        }
        // SettingsStore's debounce, owned by the Shell: a newer arm replaces
        // (and thereby cancels) the pending task, coalescing bursts.
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(
                    crate::workspace_layout_store::SAVE_DEBOUNCE_MS,
                ))
                .await;
            this.update(cx, |shell, cx| shell.flush_workspace_layout(cx));
        });
        let previous = self.workspace_save_task.replace(task);
        drop(previous);
    }

    /// Consume the dirty latch into the store, prune deleted spaces' entries,
    /// and write the store file. Runs on the debounce timer, on every space
    /// switch, and at app quit. I/O failures log only — a failed save must
    /// never break the UI (the store keeps the data pending for the next
    /// flush).
    pub(crate) fn flush_workspace_layout(&mut self, cx: &mut App) {
        self.workspace_save_task = None;
        if self.workspace.take_dirty() {
            self.workspace_layouts.set_layout(
                self.active_workspace_space.as_deref(),
                self.workspace.layout.clone(),
            );
        }
        // A deleted space (here or on another device) drops its layout entry;
        // orphan keys would be harmless but pointless to keep. Gated on the
        // synced frame — the empty pre-sync list must not wipe the saved set.
        if self.state.read(cx).spaces_synced {
            let live: std::collections::BTreeSet<String> = self
                .state
                .read(cx)
                .spaces
                .iter()
                .map(|space| space.id.clone())
                .collect();
            let removed = self.workspace_layouts.retain_spaces(|space| match space {
                None => true,
                Some(id) => live.contains(id),
            });
            if removed > 0 {
                tracing::debug!(removed, "dropped workspace layouts of deleted spaces");
            }
        }
        if !self.workspace_layouts.needs_save() {
            return;
        }
        if let Err(err) = self.workspace_layouts.flush() {
            tracing::warn!(error = %err, "failed to persist workspace layouts");
        }
    }

    /// Swap the content area to `space`'s remembered tree: boot (first synced
    /// frame) and every space switch land here. The OUTGOING tree is
    /// snapshotted into the store first, so a fast switch never loses the
    /// debounce window's changes. The incoming entry falls back to the
    /// default single-pane layout when missing or invalid. Dormant panes
    /// rebuild as identity cards; the FOCUSED pane's session is re-selected
    /// in AppState so the live transcript/composer follow it (the
    /// single-projection model).
    pub(crate) fn restore_workspace_layout(&mut self, space: Option<String>, cx: &mut Context<Self>) {
        if self.workspace.take_dirty() {
            self.workspace_layouts.set_layout(
                self.active_workspace_space.as_deref(),
                self.workspace.layout.clone(),
            );
        }
        self.active_workspace_space = space.clone();
        let layout = self
            .workspace_layouts
            .layout_for(space.as_deref())
            .filter(|layout| layout.validate().is_ok())
            .unwrap_or_default();
        // Restore alignment: a pane bound to a chat of a DIFFERENT space
        // drops its binding before anything re-selects, so a space switch can
        // never yank the app back to another space's session. Chats that have
        // not synced yet keep their binding optimistically — the dead-session
        // prune picks them up once the first chats frame lands.
        let foreign = {
            let state = self.state.read(cx);
            crate::pane::stale_session_panes(&layout.views, |session| {
                state
                    .chats
                    .iter()
                    .find(|chat| chat.id == *session)
                    .map_or(true, |chat| chat.space_id.as_deref() == space.as_deref())
            })
        };
        self.workspace.install_layout(layout);
        for pane in foreign {
            let _ = self.workspace.set_pane_session(pane, None);
        }
        // The clearings above are real changes: persist them (and pick up any
        // space deletions) without waiting for the next mutation.
        self.schedule_workspace_layout_save(cx);
        self.retarget_to_focused_pane(cx);
    }

    /// Once chats are synced, clear pane bindings whose session no longer
    /// exists (a chat deleted here or on another device). The pane STAYS —
    /// it degrades to the new-thread body until focused, and its session_id
    /// is simply gone (Super's stale-session handling; a terminal pane keeps
    /// its placeholder, no PTY to lose). Frequent no-op: cheap per frame.
    pub(crate) fn prune_dead_workspace_sessions(&mut self, cx: &mut Context<Self>) {
        let dead = {
            let state = self.state.read(cx);
            crate::pane::stale_session_panes(&self.workspace.layout.views, |session| {
                state.chats.iter().any(|chat| chat.id == *session)
            })
        };
        if dead.is_empty() {
            return;
        }
        for pane in dead {
            let _ = self.workspace.set_pane_session(pane, None);
        }
        // If the focused pane's session was among the dead, the selection
        // follows it off the stale chat (no composer focus steal).
        self.sync_selection_to_focused_pane(cx);
        cx.notify();
        self.note_workspace_mutation(cx);
    }
}

/// The active drag's preview rect ([`hit_test::DropResolution::preview`] is
/// window-space) converted into the outlet's coordinate space, where the
/// overlay div is absolutely positioned. `None` when the resolution has no
/// preview (center moves, strip drops, invalid drops — the verified §3 rule
/// that the center shows NO indicator).
fn preview_bounds(state: &DragSplitState) -> Option<gpui::Bounds<gpui::Pixels>> {
    let rect = state.resolution.preview?;
    let origin = state.root_bounds.origin;
    Some(gpui::Bounds {
        origin: gpui::point(
            gpui::px(rect.x) - origin.x,
            gpui::px(rect.y) - origin.y,
        ),
        size: gpui::size(gpui::px(rect.w), gpui::px(rect.h)),
    })
}

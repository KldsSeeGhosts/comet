//! Pane-host foundation (WS2 of the split-pane workspace feature).
//!
//! [`PaneHost`] owns the [`WorkspaceLayout`] tree (the `zeron-workspace`
//! engine: view → tab → pane hierarchy) plus the per-pane chat surfaces
//! ([`PaneChatSurface`]), and drives the content area's split panes. See
//! `docs/plans/2026-09-17-split-pane-workspace-analysis.md` §WS2.
//!
//! Hosting model: every Chat-mode pane owns a persistent
//! [`Transcript`] + [`Composer`] pair bound to the pane's session (or the
//! new-chat canvas for an unbound pane) — nothing moves with focus. Pane
//! focus still re-selects the pane's chat in `AppState` so the sidebar and
//! global actions keep their single-selection model. Split actions:
//! `workspace::{SplitPaneRight, SplitPaneDown, SplitViewRight, SplitViewDown,
//! CloseSplitView}` (shell.rs).
//!
//! Submodules: [`render`] (recursive tree renderer + Shell focus/split
//! actions), [`chrome`] (pane header, ghost composer, view tab strip),
//! [`hit_test`] (WS4 pure drag-drop resolution).
//!
//! WS3 adds the divider layer: every split node's children are separated by a
//! draggable divider ([`DividerDrag`] + [`ratio_from_pointer`]) driving the
//! engine's `set_view_ratio`/`set_pane_ratio`, the tab-strip "+"/tool-picker
//! contract ([`ToolKind`]/[`ToolPickerState`], Esc = zero layout change), and
//! the pane-header context menu ([`PaneMenuState`]).
//!
//! WS4 adds the drag layer: tab chips and pane headers are drag sources
//! ([`TabSplitDrag`], ghost = [`SplitDragGhost`]); pointer samples resolve
//! through the pure [`hit_test::resolve_drop`] against the paint-time bound
//! registries ([`PaneHost::pane_bounds`], [`PaneHost::view_bounds`],
//! [`PaneHost::chip_bounds`]) into a [`hit_test::DropPlan`], previewed live
//! and committed on mouse-up via the [`PaneHost`] engine wrappers below.

pub mod chrome;
pub mod hit_test;
pub mod render;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use gpui::{
    Bounds, Entity, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _,
    Subscription, div, px,
};
use zeron_workspace::{
    Branch, Direction, LayoutError, MAX_RATIO, PaneId, PaneMode, PaneState, Result, SplitNode,
    TabId, ViewId, WorkspaceLayout,
};

use crate::composer::Composer;
use crate::icons::{self, icon};
use crate::theme::Theme;
use crate::transcript::Transcript;

pub use zeron_workspace::MIN_RATIO;

/// A divider's logical hit-area size, straddling the node line (§1: ~8px
/// logical; the visual hairline stays a thin centered 1px line).
pub(crate) const DIVIDER_HIT_PX: f32 = 8.0;

/// Double-click equalize target for a split node (§1: equalize to 0.5/0.5).
pub(crate) const EQUALIZE_RATIO: f64 = 0.5;

/// Which split node a divider belongs to — and therefore which engine op a
/// drag drives. The divider belongs to its split NODE, not to individual
/// panes: dragging it resizes that node's two sides only (§1).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DividerTarget {
    /// A view-level split in the workspace root tree (`set_view_ratio`).
    View { path: Vec<Branch> },
    /// A pane-level split inside a view's tab (`set_pane_ratio`).
    Pane {
        view: ViewId,
        tab: TabId,
        path: Vec<Branch>,
    },
}

impl DividerTarget {
    /// The engine operation as a pure description — the half of the drag
    /// mapping that is unit-testable without a Shell. WS4's drag-resolution
    /// matrix consumes this alongside the pane-rect registry.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_view_level(&self) -> bool {
        matches!(self, Self::View { .. })
    }
}

/// Drag marker for pane dividers: carried through GPUI's drag machinery from
/// the divider's `on_drag` to the split container's `on_drag_move`
/// (`shell/panes.rs`), which converts pointer samples into live engine ratio
/// commits — direct manipulation, no tween, exactly like the sidebar seam.
pub(crate) struct DividerDrag {
    pub target: DividerTarget,
    pub horizontal: bool,
}

/// Invisible drag ghost — divider drags render nothing at the cursor
/// (mirrors the shell's `DragGhost` for the resize seams).
pub(crate) struct DividerGhost;

impl gpui::Render for DividerGhost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

// ---------------------------------------------------------------------------
// WS4: tab/pane drag sources
// ---------------------------------------------------------------------------

/// Drag payload for a workspace tab chip or pane header (GPUI `on_drag`;
/// same machinery as [`DividerDrag`]). The source plus everything the
/// floating ghost chip renders.
#[derive(Clone)]
pub(crate) struct TabSplitDrag {
    pub source: hit_test::DragSource,
    pub mark: chrome::TabMark,
    pub title: SharedString,
    /// For sidebar-originated drags: the session to bind into the new pane.
    /// `None` for workspace-internal drags (tab chips, pane headers).
    pub session_id: Option<String>,
}

/// The floating chip trailing the cursor during a workspace drag: provider
/// mark + title on a raised surface with a hairline border. GPUI renders the
/// active drag's view AT the cursor, so this is the whole story — the
/// `SurfaceTabGhost` precedent (shell.rs), no manual window-space overlay.
/// `cursor_offset` is where inside the source row/header the press began:
/// GPUI anchors the drag root at `pointer - cursor_offset`, and the chip
/// renders that offset PLUS 12px inside the root, so the visible chip always
/// sits 12px down/right of the pointer regardless of grab point.
pub(crate) struct SplitDragGhost {
    pub mark: chrome::TabMark,
    pub title: SharedString,
    pub cursor_offset: gpui::Point<Pixels>,
}

/// The chip's offset inside the drag root: the grab point plus a fixed
/// 12px down/right nudge, cancelling GPUI's `pointer - cursor_offset` root
/// placement so the chip trails the pointer at a constant displacement.
pub(crate) fn ghost_render_offset(cursor_offset: gpui::Point<Pixels>) -> gpui::Point<Pixels> {
    gpui::point(cursor_offset.x + px(12.0), cursor_offset.y + px(12.0))
}

impl gpui::Render for SplitDragGhost {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let tint = self.mark.tint.unwrap_or(theme.text_muted.opacity(0.8));
        let offset = ghost_render_offset(self.cursor_offset);
        div()
            .relative()
            .left(offset.x)
            .top(offset.y)
            .h(px(22.0))
            .max_w(px(220.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text)
            .opacity(0.9)
            .child(
                icon(self.mark.icon)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(tint),
            )
            .child(div().min_w_0().truncate().child(self.title.clone()))
    }
}

/// One in-flight workspace drag's live state: the source, the payload's
/// session identity (sidebar drags carry the session to bind; the commit
/// rejects a stale state whose payload no longer matches), the workspace
/// outlet's root bounds (the preview overlay is painted relative to it), and
/// the current pure [`hit_test::DropResolution`]. The raw pointer sample is
/// deliberately NOT stored — it changes every pixel, so holding it would
/// re-render the shell per sample; the resolved plan IS the state that
/// matters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DragSplitState {
    pub source: hit_test::DragSource,
    pub session_id: Option<String>,
    pub root_bounds: Bounds<Pixels>,
    pub resolution: hit_test::DropResolution,
}

/// Ratio from a pointer sample during a divider drag: the pointer position
/// along the container's axis, minus half the divider's hit width (the line
/// sits at the CENTER of the straddling hit area), normalized over the
/// children's combined span (container length minus the divider itself).
/// Clamped to the engine's `MIN_RATIO..=MAX_RATIO` band here so a wild
/// pointer can never produce an engine rejection mid-drag. `None` when the
/// container is too small to host children beside the divider.
pub fn ratio_from_pointer(
    pointer_along_axis: f32,
    container_origin: f32,
    container_length: f32,
    divider_px: f32,
) -> Option<f64> {
    if !pointer_along_axis.is_finite()
        || !container_origin.is_finite()
        || !container_length.is_finite()
        || !divider_px.is_finite()
    {
        return None;
    }
    let usable = f64::from(container_length) - f64::from(divider_px);
    if usable <= 0.0 {
        return None;
    }
    let at =
        f64::from(pointer_along_axis) - f64::from(container_origin) - f64::from(divider_px) / 2.0;
    Some((at / usable).clamp(MIN_RATIO, MAX_RATIO))
}

/// One tool-picker row (the verified ⌘D contract, interaction-truth §2). The
/// picker lists the app's REAL new-session entry points: today that is the
/// default chat (harness selection happens in the composer's model picker at
/// send time, so providers get no rows yet) plus a Terminal pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Chat,
    Terminal,
}

pub(crate) struct ToolPickerRow {
    pub kind: ToolKind,
    pub label: &'static str,
    pub icon: &'static str,
    pub badge: Option<&'static str>,
}

pub(crate) const TOOL_PICKER_ROWS: &[ToolPickerRow] = &[
    ToolPickerRow {
        kind: ToolKind::Chat,
        label: "New chat",
        icon: icons::ZERON_LOGO,
        badge: None,
    },
    ToolPickerRow {
        kind: ToolKind::Terminal,
        label: "Terminal",
        icon: icons::TERMINAL,
        badge: Some("T"),
    },
];

/// The pane state a picker row commits — the pure half of the row → engine-op
/// mapping (`shell/panes.rs` wraps it in split/add-tab calls).
pub(crate) fn tool_pane_state(kind: ToolKind) -> PaneState {
    match kind {
        ToolKind::Chat => chat_pane_state(),
        ToolKind::Terminal => PaneState {
            mode: PaneMode::Terminal,
            ..PaneState::default()
        },
    }
}

/// What a picked tool row commits (no layout happens until a row is picked —
/// ⌘D/⇧⌘D only OPEN the picker; Esc cancels with zero layout change, §2).
#[derive(Clone, Copy, Debug)]
pub(crate) enum PickerCommit {
    /// Split the focused pane (⌘D / ⇧⌘D semantics).
    SplitPane(Direction),
    /// Add a tab to one view ("+" in a tab strip — no split).
    AddTab { view: ViewId },
}

/// Open tool-picker state: the commit contract plus the window-space anchor
/// (the focused pane's top-left for ⌘D/⇧⌘D per §2; the "+" trigger's position
/// for tab strips).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolPickerState {
    pub commit: PickerCommit,
    pub anchor: gpui::Point<Pixels>,
}

/// Pane-header context-menu state (right-click, §6 — the menu also focuses
/// the pane, which routes keyboard focus to its own composer).
#[derive(Clone)]
pub(crate) struct PaneMenuState {
    pub pane: PaneId,
    pub position: gpui::Point<Pixels>,
}

/// The transcript + composer a Chat-mode pane owns. Created by the shell
/// (entity construction needs a `Context`), keyed by pane so the layout can
/// rebind sessions without moving entities between panes. `chat_id` mirrors
/// the layout's `PaneState.session_id` — the validity key the shell's
/// ensure pass compares against — and the pane composer's own target watch
/// keeps it current when the composer binds or resets itself (mint /
/// failed first send) ahead of the layout commit.
pub(crate) struct PaneChatSurface {
    pub chat_id: Option<String>,
    /// `None` on the new-chat canvas: the transcript is minted by the first
    /// send (the composer event handler creates it), not at surface creation.
    pub transcript: Option<Entity<Transcript>>,
    pub composer: Entity<Composer>,
    /// Held for RAII: dropping the surface unsubscribes the event stream.
    #[allow(dead_code)]
    pub composer_events: Subscription,
    /// Mirrors the composer's `ChatTarget` into `chat_id` on bind/reset —
    /// queued ahead of the `select_chat` observers that rebind the layout.
    /// Held for RAII like `composer_events`.
    #[allow(dead_code)]
    pub composer_observation: Subscription,
    pub transcript_events: Option<Subscription>,
}

/// Shell-side workspace state: the layout tree plus per-pane chat surfaces.
pub struct PaneHost {
    pub layout: WorkspaceLayout,
    /// Per-pane transcript+composer pairs for every Chat-mode pane, keyed by
    /// pane. Created lazily by the shell's render pass for every pane in the
    /// tree (focus no longer moves views); dead-pane entries are dropped by
    /// [`PaneHost::prune_caches`].
    pub(crate) chat_surfaces: HashMap<PaneId, PaneChatSurface>,
    /// Last painted pane bounds, recorded by a paint-time canvas in
    /// [`render`](crate::pane::render). Consumed as the tool-picker's anchor
    /// (the focused pane's top-left) and by WS4's drag resolution.
    pub(crate) pane_bounds: Rc<RefCell<BTreeMap<PaneId, Bounds<Pixels>>>>,
    /// WS4: last painted TOP-LEVEL VIEW bounds (each view leaf's whole
    /// column: strip + pane tree). The SplitView preview washes this region.
    pub(crate) view_bounds: Rc<RefCell<BTreeMap<ViewId, Bounds<Pixels>>>>,
    /// WS4: last painted tab-chip bounds keyed `(view, tab)` — the strip
    /// drop/reorder targets of [`hit_test::resolve_drop`].
    pub(crate) chip_bounds: Rc<RefCell<BTreeMap<(ViewId, TabId), Bounds<Pixels>>>>,
    /// WS5 persistence latch: set by every successful engine mutation
    /// (structure, ratios, focus, session bindings). The shell consumes it
    /// through [`Self::take_dirty`] when the debounced store write fires;
    /// gestures only ever arm the save, never flush mid-flight.
    dirty: bool,
}

impl Default for PaneHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneHost {
    pub fn new() -> Self {
        Self {
            layout: WorkspaceLayout::new(),
            chat_surfaces: HashMap::new(),
            pane_bounds: Rc::new(RefCell::new(BTreeMap::new())),
            view_bounds: Rc::new(RefCell::new(BTreeMap::new())),
            chip_bounds: Rc::new(RefCell::new(BTreeMap::new())),
            dirty: false,
        }
    }

    /// Latch the persistence-dirty flag on a successful engine mutation.
    /// Takes the pre-computed result so the engine borrow has ended.
    fn touched<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_ok() {
            self.dirty = true;
        }
        result
    }

    /// Whether an unsaved mutation is latched.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Consume the dirty latch (WS5 flush / space-switch snapshot).
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// WS5 restore: replace the whole tree with a (validated) stored layout.
    /// Every cache resets so pane surfaces and paint registries rebuild
    /// against the restored pane ids. Not a mutation — the dirty latch
    /// CLEARS, because the restored tree is the store's own content.
    pub fn install_layout(&mut self, layout: WorkspaceLayout) {
        self.layout = layout;
        self.chat_surfaces.clear();
        self.pane_bounds.borrow_mut().clear();
        self.view_bounds.borrow_mut().clear();
        self.chip_bounds.borrow_mut().clear();
        self.dirty = false;
    }

    /// The registry handle the renderer's paint-time canvas writes into.
    pub(crate) fn pane_bounds_handle(&self) -> Rc<RefCell<BTreeMap<PaneId, Bounds<Pixels>>>> {
        self.pane_bounds.clone()
    }

    /// WS4: the view-region registry handle (see [`Self::pane_bounds`]).
    pub(crate) fn view_bounds_handle(&self) -> Rc<RefCell<BTreeMap<ViewId, Bounds<Pixels>>>> {
        self.view_bounds.clone()
    }

    /// WS4: the tab-chip registry handle (see [`Self::pane_bounds`]).
    pub(crate) fn chip_bounds_handle(
        &self,
    ) -> Rc<RefCell<BTreeMap<(ViewId, TabId), Bounds<Pixels>>>> {
        self.chip_bounds.clone()
    }

    /// The focused pane's last painted window bounds.
    pub(crate) fn focused_pane_bounds(&self) -> Option<Bounds<Pixels>> {
        self.pane_bounds.borrow().get(&self.focused_pane()?).copied()
    }

    // ---- focus / structure queries ----

    /// The one globally focused pane (active pane of the active tab of the
    /// active view).
    pub fn focused_pane(&self) -> Option<PaneId> {
        self.layout.active_pane_id()
    }

    pub fn focused_view(&self) -> Option<ViewId> {
        self.layout
            .views
            .contains_key(&self.layout.active_view_id)
            .then_some(self.layout.active_view_id)
    }

    /// Whether the layout is still the untouched default (1 view / 1 tab /
    /// 1 pane). The single-pane parity gate: in this state `render_main` takes
    /// today's exact code path — no pane wrapper, no ring, no tab strip.
    pub fn is_trivial(&self) -> bool {
        is_trivial_layout(&self.layout)
    }

    // ---- mutations (all engine-validated; errors are silent no-ops) ----

    /// Split the focused pane, focusing the new pane (engine guarantee).
    /// The new pane is a Chat pane with no session — it renders the new-thread
    /// composition and mints its chat on first send.
    pub fn split_focused_pane(&mut self, direction: Direction) -> Result<PaneId> {
        self.split_focused_pane_with(direction, chat_pane_state())
    }

    /// [`Self::split_focused_pane`] with an explicit pane state — the
    /// tool-picker commit path (Terminal rows, future provider rows).
    pub fn split_focused_pane_with(
        &mut self,
        direction: Direction,
        pane: PaneState,
    ) -> Result<PaneId> {
        let target = self.focused_pane().ok_or(LayoutError::NotFound("pane"))?;
        let result = self.layout.split_pane(target, direction, pane);
        let new_pane = self.touched(result)?;
        Ok(new_pane)
    }

    /// Split the workspace at the focused view. The engine's `split_view`
    /// already provisions the new view's first tab + pane and focuses the new
    /// view, so no separate `add_tab` call is needed (the new view has exactly
    /// one tab, per Super's split-view anatomy — interaction-truth §4).
    pub fn split_focused_view(&mut self, direction: Direction) -> Result<ViewId> {
        let target = self.focused_view().ok_or(LayoutError::NotFound("view"))?;
        let pane = chat_pane_state();
        let result = self.layout.split_view(target, direction, pane);
        let new_view = self.touched(result)?;
        Ok(new_view)
    }

    /// Close the focused view. Errors (the last view cannot close) are
    /// returned to the caller, which treats them as a no-op.
    pub fn close_focused_view(&mut self) -> Result<ViewId> {
        let target = self.focused_view().ok_or(LayoutError::NotFound("view"))?;
        let result = self.layout.close_view(target);
        self.touched(result)?;
        Ok(target)
    }

    /// Close one pane (tab/view follow via the engine when they empty out).
    pub fn close_pane(&mut self, pane: PaneId) -> Result<()> {
        let result = self.layout.close_pane(pane);
        self.touched(result)
    }

    /// Resize a view-level split node (divider drag / double-click equalize).
    /// The engine validates and clamps the ratio (`MIN_RATIO..=MAX_RATIO`).
    pub fn set_view_ratio(&mut self, path: &[Branch], ratio: f64) -> Result<()> {
        let result = self.layout.set_view_ratio(path, ratio);
        self.touched(result)
    }

    /// Resize a pane-level split node inside a view's tab.
    pub fn set_pane_ratio(
        &mut self,
        view: ViewId,
        tab: TabId,
        path: &[Branch],
        ratio: f64,
    ) -> Result<()> {
        let result = self.layout.set_pane_ratio(view, tab, path, ratio);
        self.touched(result)
    }

    /// "+" in a view's tab strip: a fresh Chat tab becomes the view's active
    /// tab (engine focuses it). The tab's pane is a session-less Chat pane
    /// that mints its chat on first send.
    pub fn add_tab_to_view(&mut self, view: ViewId) -> Result<TabId> {
        self.add_tab_with(view, chat_pane_state())
    }

    /// [`Self::add_tab_to_view`] with an explicit pane state (tool picker).
    pub fn add_tab_with(&mut self, view: ViewId, pane: PaneState) -> Result<TabId> {
        let result = self.layout.add_tab(view, pane);
        self.touched(result)
    }

    /// Close a tab chip's ×. Engine semantics (verified against
    /// `crates/workspace`): closing a view's LAST tab closes the VIEW, and
    /// closing the last remaining view's last tab is an error — a view can
    /// never be empty, so the Super empty-view launcher is unreachable
    /// without engine changes (documented WS3 deviation).
    pub fn close_tab(&mut self, view: ViewId, tab: TabId) -> Result<()> {
        let result = self.layout.close_tab(view, tab);
        self.touched(result)
    }

    /// Focus a pane. A no-op when it is already focused (avoids revision
    /// churn on every in-pane click — and no dirty latch either: focus
    /// persistence only records real focus MOVES).
    pub fn focus_pane(&mut self, pane: PaneId) -> Result<()> {
        if self.focused_pane() == Some(pane) {
            return Ok(());
        }
        let result = self.layout.focus_pane(pane);
        self.touched(result)
    }

    /// Switch a view's active tab and focus that view.
    pub fn focus_tab(&mut self, view: ViewId, tab: TabId) -> Result<()> {
        let result = self.layout.focus_tab(view, tab);
        self.touched(result)
    }

    // ---- WS4 drag commits (thin engine wrappers; errors no-op upstream) ----

    /// Tab chip dropped on a pane's CENTER of another view (or appended to a
    /// strip): move the whole tab, preserving panes and ratios.
    pub fn move_tab_to_view(&mut self, tab: TabId, view: ViewId) -> Result<()> {
        let result = self.layout.move_tab(tab, view);
        self.touched(result)
    }

    /// Tab chip dropped on a strip: move/reorder to sit before `before`
    /// (`None` = append). Handles both same-strip reorder and cross-view
    /// moves; a same-position drop early-Ok's (the no-op restore).
    pub fn reorder_tab_in_view(
        &mut self,
        tab: TabId,
        view: ViewId,
        before: Option<TabId>,
    ) -> Result<()> {
        let result = self.layout.reorder_tab(tab, view, before);
        self.touched(result)
    }

    /// Pane header dropped on a strip: the pane becomes a tab of `view`
    /// (`pane_to_tab`; a single-pane tab moves whole instead of detaching).
    pub fn pane_header_to_tab(&mut self, pane: PaneId, view: ViewId) -> Result<TabId> {
        let result = self.layout.pane_to_tab(pane, view);
        self.touched(result)
    }

    /// Tab chip dropped on a pane's INTERIOR edge: the dragged tab's whole
    /// pane subtree becomes the half-pane beside `pane` toward `direction`
    /// (`merge_tab`; the emptied source tab/view close automatically, and the
    /// moved subtree's active pane takes focus).
    pub fn merge_tab_into_pane(
        &mut self,
        tab: TabId,
        pane: PaneId,
        direction: Direction,
    ) -> Result<()> {
        let result = self.layout.merge_tab(tab, pane, direction);
        self.touched(result)
    }

    /// Pane header dropped on a pane's INTERIOR edge: the pane moves beside
    /// the target toward `direction` (`move_pane`; already-sibling panes
    /// swap, emptied tabs/views close, the moved pane takes focus).
    pub fn move_pane_beside(
        &mut self,
        source: PaneId,
        target: PaneId,
        direction: Direction,
    ) -> Result<()> {
        let result = self.layout.move_pane(source, target, direction);
        self.touched(result)
    }

    /// Tab chip dropped on a pane's WORKSPACE-OUTER edge: the tab becomes the
    /// sole tab of a new top-level view adjacent to `view` (`tab_to_view`).
    pub fn tab_to_adjacent_view(
        &mut self,
        tab: TabId,
        view: ViewId,
        direction: Direction,
    ) -> Result<ViewId> {
        let result = self.layout.tab_to_view(tab, view, direction);
        let new_view = self.touched(result)?;
        Ok(new_view)
    }

    /// Pane header dropped on a pane's WORKSPACE-OUTER edge: the pane becomes
    /// the first pane of a new top-level view adjacent to `view`
    /// (`pane_to_view`).
    pub fn pane_to_adjacent_view(
        &mut self,
        pane: PaneId,
        view: ViewId,
        direction: Direction,
    ) -> Result<ViewId> {
        let result = self.layout.pane_to_view(pane, view, direction);
        let new_view = self.touched(result)?;
        Ok(new_view)
    }

    /// Bind a session to a pane (`None` = new-thread pane). Committed through
    /// `compose` so the revision advances exactly once per real change.
    /// Surface lifecycle is NOT decided here: the shell's ensure pass is the
    /// sole authority that compares `PaneChatSurface.chat_id` to the layout
    /// binding and recreates stale surfaces, so a just-bound pane composer
    /// can never be dropped by an observation-order race.
    pub fn set_pane_session(&mut self, pane: PaneId, session_id: Option<String>) -> Result<()> {
        let revision = self.layout.revision;
        let result = self.layout.compose(revision, |draft| {
            if let Some(state) = draft.pane_mut(pane) {
                state.session_id = session_id;
            }
            Ok(())
        });
        self.touched(result)
    }

    /// Latch the persistence flag without a structural mutation — the pane
    /// composer's mint commits its session through the selection-sync path,
    /// which may have landed the identical binding already.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Selection sync (the sidebar ↔ pane bridge): bind the FOCUSED pane's
    /// session to the currently selected chat. Called from the shell's
    /// AppState-observation path on chat switches, so sidebar selection,
    /// canvas mint-on-send, and pane focus all converge on one binding point.
    /// Returns whether the binding changed.
    pub fn sync_focused_session(&mut self, chat_id: Option<&str>) -> bool {
        let Some(pane) = self.focused_pane() else {
            return false;
        };
        let current = self
            .layout
            .pane(pane)
            .and_then(|state| state.session_id.clone());
        if current.as_deref() == chat_id {
            return false;
        }
        self.set_pane_session(pane, chat_id.map(str::to_string))
            .is_ok()
    }

    // ---- per-pane surfaces ----

    /// Every Chat-mode pane's `(id, session binding)` across ALL views and
    /// tabs — the inventory [`PaneHost::chat_surfaces`] must hold live
    /// entities for. The shell's render pass creates or recreates a surface
    /// for each entry whose `chat_id` doesn't match.
    pub(crate) fn chat_pane_sessions(&self) -> Vec<(PaneId, Option<String>)> {
        self.layout
            .views
            .values()
            .flat_map(|view| view.tabs.values())
            .flat_map(|tab| tab.panes.iter())
            .filter(|(_, state)| state.mode == PaneMode::Chat)
            .map(|(id, state)| (*id, state.session_id.clone()))
            .collect()
    }

    /// Drop cache entries whose pane left the tree, and reset to the default
    /// layout if the tree somehow became invalid (defensive; the engine
    /// commits atomically so this should never fire).
    pub fn prune_caches(&mut self) {
        if self.layout.validate().is_err() {
            self.layout = WorkspaceLayout::new();
            self.dirty = true;
        }
        // A split collapsing to one pane does not end that pane's session.
        // Keep its composer (and unsent draft); only remove dead/tool panes.
        let live = live_pane_ids(&self.layout.views);
        let chat_live = self.chat_pane_sessions().into_iter().map(|(pane, _)| pane).collect();
        let stale = stale_cache_keys(&chat_live, self.chat_surfaces.keys().copied());
        for pane in stale {
            self.chat_surfaces.remove(&pane);
        }
        self.pane_bounds
            .borrow_mut()
            .retain(|pane, _| live.contains(pane));
        let live_views: BTreeSet<ViewId> = self.layout.views.keys().copied().collect();
        self.view_bounds
            .borrow_mut()
            .retain(|view, _| live_views.contains(view));
        self.chip_bounds.borrow_mut().retain(|(view, tab), _| {
            live_views.contains(view)
                && self
                    .layout
                    .views
                    .get(view)
                    .is_some_and(|v| v.tabs.contains_key(tab))
        });
    }
}

/// A fresh Chat pane bound to no session — the split/new-tab default.
pub fn chat_pane_state() -> PaneState {
    PaneState {
        mode: zeron_workspace::PaneMode::Chat,
        ..PaneState::default()
    }
}

/// The single-pane parity gate, as a pure predicate.
pub fn is_trivial_layout(layout: &WorkspaceLayout) -> bool {
    if layout.views.len() != 1 {
        return false;
    }
    layout
        .views
        .values()
        .all(|view| view.tabs.len() == 1 && view.tabs.values().all(|tab| {
            tab.panes.len() == 1 && tab.panes.values().all(|pane| pane.mode == PaneMode::Chat)
        }))
}

/// Every pane id reachable from a workspace (leaves of every tab's pane
/// tree). Pure.
pub fn live_pane_ids(
    views: &std::collections::BTreeMap<ViewId, zeron_workspace::ViewLayout>,
) -> BTreeSet<PaneId> {
    views
        .values()
        .flat_map(|view| view.tabs.values().flat_map(|tab| tab.panes.keys().copied()))
        .collect()
}

/// Cache keys not in `live`, in deterministic order — the pure half of
/// [`PaneHost::prune_caches`].
pub fn stale_cache_keys(
    live: &BTreeSet<PaneId>,
    cached: impl Iterator<Item = PaneId>,
) -> Vec<PaneId> {
    let mut stale: Vec<PaneId> = cached.filter(|pane| !live.contains(pane)).collect();
    stale.sort_unstable();
    stale
}

/// WS5: panes whose bound session fails `keep` — the pure half of the
/// stale-session degradation, consumed by the dead-session prune (a chat
/// deleted while a pane still binds it): the pane stays and degrades to the
/// new-thread body, binding cleared on the next prune.
/// Deterministic order; sessions that `keep` accepts (including unknown ones
/// — e.g. chats not synced yet) are preserved.
pub fn stale_session_panes(
    views: &std::collections::BTreeMap<ViewId, zeron_workspace::ViewLayout>,
    keep: impl Fn(&str) -> bool,
) -> Vec<PaneId> {
    let mut stale: Vec<PaneId> = views
        .values()
        .flat_map(|view| view.tabs.values())
        .flat_map(|tab| tab.panes.iter())
        .filter_map(|(pane, state)| {
            let session = state.session_id.as_deref()?;
            (!keep(session)).then_some(*pane)
        })
        .collect();
    stale.sort_unstable();
    stale
}

// ---------------------------------------------------------------------------
// Pure geometry helpers (unit-tested below; consumed by pane::render)
// ---------------------------------------------------------------------------

/// The fraction of a split's FIRST child along its axis, defensively
/// sanitized: the engine clamps ratios to [`MIN_RATIO`]..[`MAX_RATIO`] on
/// every commit, but drafts in flight could hold anything a foreign file
/// wrote. Non-finite collapses to an even split.
pub fn first_fraction(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(zeron_workspace::MIN_RATIO, zeron_workspace::MAX_RATIO)
    } else {
        0.5
    }
}

/// Flex weights for a split's two children: `(first, second)`, summing to 1.
/// Paired with `flex_basis(px(0.0))` so child sizes are purely
/// weight-proportional (the markdown table renderer uses the same trick).
pub fn flex_weights(ratio: f64) -> (f32, f32) {
    let first = first_fraction(ratio) as f32;
    (first, 1.0 - first)
}

/// Whether a split node divides along the horizontal (column) axis. A leaf
/// has no axis; callers only ask for splits.
pub fn split_is_horizontal<T>(node: &SplitNode<T>) -> bool {
    match node {
        SplitNode::Split { horizontal, .. } => *horizontal,
        SplitNode::Leaf { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_workspace::PaneMode;

    // ---- geometry ----

    #[test]
    fn even_split_gives_equal_weights() {
        assert_eq!(flex_weights(0.5), (0.5, 0.5));
        assert_eq!(first_fraction(0.5), 0.5);
    }

    #[test]
    fn split_weights_sum_to_one_and_track_the_ratio() {
        let (a, b) = flex_weights(0.72);
        assert!((a + b - 1.0).abs() < 1e-6);
        assert!((a - 0.72).abs() < 1e-6);
        let (a, b) = flex_weights(0.15);
        assert!((a + b - 1.0).abs() < 1e-6);
        // In-range ratios pass through unchanged.
        assert!((a - 0.15).abs() < 1e-6);
        assert!(b > a);
        // Only genuinely out-of-range drafts clamp (f64::MIN clamps to 0.1).
        let (a, b) = flex_weights(f64::MIN);
        assert!((a - MIN_RATIO as f32).abs() < 1e-6);
        assert!(b > a);
    }

    #[test]
    fn out_of_range_and_non_finite_ratios_are_sanitized() {
        // Engine commits never produce these, but foreign drafts could.
        assert_eq!(first_fraction(f64::NAN), 0.5);
        assert_eq!(first_fraction(f64::INFINITY), 0.5);
        assert_eq!(first_fraction(0.0), MIN_RATIO);
        assert_eq!(first_fraction(1.0), zeron_workspace::MAX_RATIO);
        let (a, b) = flex_weights(f64::NAN);
        assert!((a + b - 1.0).abs() < 1e-6);
    }

    #[test]
    fn nested_weights_compose_multiplicatively() {
        // A 0.75 horizontal split whose left half is a 0.5 vertical split:
        // the top-left leaf should occupy 0.75 * 0.5 of the area.
        let outer = flex_weights(0.75);
        let inner = flex_weights(0.5);
        let top_left = outer.0 * inner.0;
        assert!((top_left - 0.375).abs() < 1e-6);
    }

    #[test]
    fn axis_matches_the_split_node() {
        let node: SplitNode<PaneId> = SplitNode::Split {
            horizontal: true,
            ratio: 0.5,
            first: Box::new(SplitNode::leaf(PaneId(1))),
            second: Box::new(SplitNode::leaf(PaneId(2))),
        };
        assert!(split_is_horizontal(&node));
        let leaf: SplitNode<PaneId> = SplitNode::leaf(PaneId(1));
        assert!(split_is_horizontal(&leaf));
    }

    // ---- default layout / normalization ----

    #[test]
    fn default_layout_is_one_view_one_tab_one_pane_and_trivial() {
        let host = PaneHost::new();
        assert!(host.is_trivial());
        assert_eq!(host.layout.views.len(), 1);
        assert_eq!(host.focused_view(), Some(ViewId(1)));
        assert_eq!(host.focused_pane(), Some(PaneId(3)));
        let view = host.layout.views.get(&ViewId(1)).unwrap();
        assert_eq!(view.tabs.len(), 1);
        let tab = view.tabs.get(&TabId(2)).unwrap();
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(tab.active_pane_id, PaneId(3));
        assert_eq!(tab.primary_pane_id, PaneId(3));
        assert_eq!(host.layout.next_id, 4);
        assert!(host.layout.validate().is_ok());
    }

    #[test]
    fn split_pane_focuses_the_new_pane_and_breaks_triviality() {
        let mut host = PaneHost::new();
        let new_pane = host.split_focused_pane(Direction::Right).unwrap();
        assert_eq!(new_pane, PaneId(4));
        assert_eq!(host.focused_pane(), Some(new_pane));
        assert!(!host.is_trivial());
        // New pane is a session-less Chat pane (the new-thread composition).
        let state = host.layout.pane(new_pane).unwrap();
        assert_eq!(state.mode, PaneMode::Chat);
        assert_eq!(state.session_id, None);
        host.layout.validate().unwrap();
    }

    #[test]
    fn nested_split_view_then_split_pane_shapes_the_tree() {
        let mut host = PaneHost::new();
        let view = host.split_focused_view(Direction::Right).unwrap();
        assert_eq!(host.focused_view(), Some(view));
        assert_eq!(host.layout.views.len(), 2);
        // The new view arrives with exactly one tab (Super split-view truth).
        let new_view = host.layout.views.get(&view).unwrap();
        assert_eq!(new_view.tabs.len(), 1);
        // Further splits nest INSIDE the focused view's tab.
        let pane = host.split_focused_pane(Direction::Down).unwrap();
        let tab = host.layout.pane_location(pane).unwrap().1;
        let new_view = host.layout.views.get(&view).unwrap();
        assert_eq!(new_view.tabs.len(), 1, "split_pane must not add a tab");
        assert_eq!(new_view.tabs.get(&tab).unwrap().panes.len(), 2);
        host.layout.validate().unwrap();
    }

    #[test]
    fn session_binding_via_sync_and_direct_set() {
        let mut host = PaneHost::new();
        assert!(host.sync_focused_session(Some("chat-a")));
        assert_eq!(
            host.layout.pane(PaneId(3)).unwrap().session_id.as_deref(),
            Some("chat-a")
        );
        // Idempotent when unchanged.
        assert!(!host.sync_focused_session(Some("chat-a")));
        assert!(host.sync_focused_session(None));
        assert_eq!(
            host.layout.pane(PaneId(3)).unwrap().session_id,
            None,
            "deselect binds the focused pane back to the new-thread state"
        );
    }

    #[test]
    fn focus_moves_and_selection_syncs_follow_the_active_pane() {
        let mut host = PaneHost::new();
        host.sync_focused_session(Some("chat-a"));
        let new_pane = host.split_focused_pane(Direction::Right).unwrap();
        // Selection sync now targets the NEW focused pane, not pane 3.
        assert!(host.sync_focused_session(Some("chat-b")));
        assert_eq!(
            host.layout.pane(new_pane).unwrap().session_id.as_deref(),
            Some("chat-b")
        );
        assert_eq!(
            host.layout.pane(PaneId(3)).unwrap().session_id.as_deref(),
            Some("chat-a")
        );
        // Refocusing the original pane makes it the sync target again.
        host.focus_pane(PaneId(3)).unwrap();
        assert!(host.sync_focused_session(Some("chat-c")));
        assert_eq!(
            host.layout.pane(PaneId(3)).unwrap().session_id.as_deref(),
            Some("chat-c")
        );
        assert_eq!(
            host.layout.pane(new_pane).unwrap().session_id.as_deref(),
            Some("chat-b")
        );
    }

    #[test]
    fn closing_guards_are_no_ops() {
        let mut host = PaneHost::new();
        // The last view cannot close — the shell surfaces nothing.
        assert!(host.close_focused_view().is_err());
        assert!(host.is_trivial(), "failed close must leave the tree intact");
        // The last pane cannot close either.
        assert!(host.close_pane(PaneId(3)).is_err());
        assert!(host.layout.validate().is_ok());
    }

    #[test]
    fn closing_a_split_pane_keeps_the_sibling() {
        let mut host = PaneHost::new();
        host.sync_focused_session(Some("chat-a"));
        let new_pane = host.split_focused_pane(Direction::Right).unwrap();
        host.sync_focused_session(Some("chat-b"));
        host.close_pane(new_pane).unwrap();
        assert!(host.is_trivial());
        assert_eq!(host.focused_pane(), Some(PaneId(3)));
        assert_eq!(
            host.layout.pane(PaneId(3)).unwrap().session_id.as_deref(),
            Some("chat-a"),
            "the untouched sibling keeps its binding"
        );
        host.layout.validate().unwrap();
    }

    #[test]
    fn closing_the_last_pane_of_a_split_view_closes_the_view() {
        let mut host = PaneHost::new();
        let view = host.split_focused_view(Direction::Right).unwrap();
        let pane = host
            .layout
            .views
            .get(&view)
            .and_then(|v| v.tabs.values().next())
            .map(|tab| tab.active_pane_id)
            .unwrap();
        host.close_pane(pane).unwrap();
        assert_eq!(host.layout.views.len(), 1);
        assert!(host.is_trivial());
        host.layout.validate().unwrap();
    }

    #[test]
    fn stale_cache_keys_returns_only_dead_panes() {
        let live = BTreeSet::from([PaneId(3), PaneId(7)]);
        let stale = stale_cache_keys(&live, [PaneId(3), PaneId(4), PaneId(9)].into_iter());
        assert_eq!(stale, vec![PaneId(4), PaneId(9)]);
    }

    #[test]
    fn live_pane_ids_spans_every_view_and_tab() {
        let mut host = PaneHost::new();
        host.split_focused_view(Direction::Right).unwrap();
        host.split_focused_pane(Direction::Down).unwrap();
        let live = live_pane_ids(&host.layout.views);
        assert_eq!(live.len(), 3);
    }

    // ---- WS4: drag ghost ----

    #[test]
    fn ghost_trails_the_pointer_at_a_constant_offset() {
        // GPUI anchors the drag root at `pointer - cursor_offset`; the chip
        // renders `ghost_render_offset` inside it. The visible chip's
        // displacement from the pointer is therefore
        // `render_offset - cursor_offset` — a constant (12, 12) wherever the
        // press began inside the source row/header.
        for grab in [(0.0, 0.0), (180.0, 34.0)] {
            let cursor_offset = gpui::point(px(grab.0), px(grab.1));
            let rendered = ghost_render_offset(cursor_offset);
            assert_eq!(rendered.x - cursor_offset.x, px(12.0));
            assert_eq!(rendered.y - cursor_offset.y, px(12.0));
        }
    }

    // ---- WS3: divider math ----

    /// A split node's ratio (variant fields aren't publicly readable).
    fn split_ratio<T>(node: &SplitNode<T>) -> f64 {
        match node {
            SplitNode::Split { ratio, .. } => *ratio,
            SplitNode::Leaf { .. } => panic!("expected a split node"),
        }
    }

    #[test]
    fn ratio_from_pointer_tracks_the_divider_center() {
        let hit = DIVIDER_HIT_PX;
        // Geometry: the divider line sits at first_span + hit/2, and the
        // children span (L - hit). An equal split's line is at the middle.
        let ratio = ratio_from_pointer(500.0, 0.0, 1000.0, hit).unwrap();
        assert!((ratio - 0.5).abs() < 1e-9);
        // A ratio of 0.3 puts the line at 0.3 * 992 + 4 = 301.6 (f32 drag
        // math — tolerance is sub-pixel, not ulp).
        let ratio = ratio_from_pointer(0.3 * (1000.0 - hit) + hit / 2.0, 0.0, 1000.0, hit).unwrap();
        assert!((ratio - 0.3).abs() < 1e-4);
        // The left edge of the hit strip (pointer at hit/2) reads ratio 0 —
        // clamped up to the engine's floor.
        let ratio = ratio_from_pointer(4.0, 0.0, 1000.0, hit).unwrap();
        assert_eq!(ratio, MIN_RATIO);
        // Origin offsets (nested containers) subtract cleanly.
        let ratio = ratio_from_pointer(
            1000.0 + 0.3 * (1000.0 - hit) + hit / 2.0,
            1000.0,
            1000.0,
            hit,
        )
        .unwrap();
        assert!((ratio - 0.3).abs() < 1e-4);
    }

    #[test]
    fn ratio_from_pointer_clamps_and_degenerates() {
        // Way outside the children span clamps to the engine band.
        assert_eq!(
            ratio_from_pointer(-500.0, 0.0, 1000.0, DIVIDER_HIT_PX),
            Some(MIN_RATIO)
        );
        assert_eq!(
            ratio_from_pointer(5000.0, 0.0, 1000.0, DIVIDER_HIT_PX),
            Some(zeron_workspace::MAX_RATIO)
        );
        // A container no wider than the divider cannot host children.
        assert_eq!(ratio_from_pointer(4.0, 0.0, 8.0, DIVIDER_HIT_PX), None);
        assert_eq!(ratio_from_pointer(4.0, 0.0, 7.0, DIVIDER_HIT_PX), None);
        // Non-finite input is rejected, never panes.
        assert_eq!(
            ratio_from_pointer(f32::NAN, 0.0, 1000.0, DIVIDER_HIT_PX),
            None
        );
        assert_eq!(
            ratio_from_pointer(4.0, 0.0, f32::INFINITY, DIVIDER_HIT_PX),
            None
        );
    }

    #[test]
    fn divider_targets_resize_their_own_node_only() {
        let mut host = PaneHost::new();
        let view = host.split_focused_view(Direction::Right).unwrap();
        let new_tab = host.layout.views[&view].active_tab_id;
        // Pane-level split inside the NEW view's tab (it is focused).
        let _split = host.split_focused_pane(Direction::Down).unwrap();
        // Drag the VIEW-level divider (root path): only the root ratio moves.
        host.set_view_ratio(&[], 0.7).unwrap();
        assert_eq!(split_ratio(&host.layout.root), 0.7);
        // Drag the PANE-level divider (tab root path): tab ratio moves, the
        // view ratio is untouched — a divider belongs to its split node (§1).
        host.set_pane_ratio(view, new_tab, &[], 0.3).unwrap();
        assert_eq!(split_ratio(&host.layout.root), 0.7);
        assert_eq!(
            split_ratio(&host.layout.views[&view].tabs[&new_tab].root),
            0.3
        );
        host.layout.validate().unwrap();
    }

    #[test]
    fn nested_pane_paths_reach_the_inner_split() {
        use zeron_workspace::{Branch, SplitNode};
        let mut host = PaneHost::new();
        let view = host.split_focused_view(Direction::Right).unwrap();
        let tab = host.layout.views[&view].active_tab_id;
        let original = host.layout.views[&view].tabs[&tab].active_pane_id;
        // Split downward: the new pane becomes the tab root's SECOND leaf.
        let _new = host.split_focused_pane(Direction::Down).unwrap();
        // Refocus the ORIGINAL pane (now the first leaf) and split it again
        // → the inner split lives at path [First].
        host.focus_pane(original).unwrap();
        host.split_focused_pane(Direction::Right).unwrap();
        host.set_pane_ratio(view, tab, &[Branch::First], 0.8)
            .unwrap();
        let root = &host.layout.views[&view].tabs[&tab].root;
        match root {
            SplitNode::Split { ratio, first, .. } => {
                assert_eq!(*ratio, 0.5, "outer node untouched");
                assert_eq!(split_ratio(first), 0.8, "inner node moved");
            }
            SplitNode::Leaf { .. } => panic!("expected a split"),
        }
        host.layout.validate().unwrap();
    }

    #[test]
    fn divider_target_describes_its_level() {
        let view_target = DividerTarget::View { path: vec![] };
        let pane_target = DividerTarget::Pane {
            view: ViewId(1),
            tab: TabId(2),
            path: vec![],
        };
        assert!(view_target.is_view_level());
        assert!(!pane_target.is_view_level());
        assert_eq!(view_target, DividerTarget::View { path: vec![] });
    }

    // ---- WS3: tab close semantics (empty views are unreachable) ----

    #[test]
    fn add_tab_focuses_it_and_closing_the_last_tab_closes_the_view() {
        let mut host = PaneHost::new();
        let view = zeron_workspace::ViewId(1);
        let added = host.add_tab_to_view(view).unwrap();
        assert_eq!(host.layout.views[&view].tabs.len(), 2);
        assert_eq!(host.layout.views[&view].active_tab_id, added);
        assert_eq!(
            host.layout.active_pane_id(),
            Some(host.layout.views[&view].tabs[&added].active_pane_id)
        );
        // Closing the added tab restores the single-tab view.
        host.close_tab(view, added).unwrap();
        assert_eq!(host.layout.views[&view].tabs.len(), 1);
        assert!(host.is_trivial());
        // Closing the LAST tab of the ONLY view is refused: the app can never
        // be left viewless.
        let last = host.layout.views[&view].active_tab_id;
        assert!(host.close_tab(view, last).is_err());
        assert!(host.layout.validate().is_ok());
    }

    #[test]
    fn draining_a_view_tab_by_tab_closes_the_view_never_empties_it() {
        let mut host = PaneHost::new();
        let view = zeron_workspace::ViewId(1);
        let a = host.add_tab_to_view(view).unwrap();
        // Multi-tab view: closing one of two tabs keeps the view alive.
        host.close_tab(view, a).unwrap();
        assert_eq!(host.layout.views.len(), 1);
        // Closing the remaining (last) tab closes the VIEW — but the last
        // view of the workspace is guarded above it, so the default view
        // stays and the tree stays valid.
        let last = host.layout.views[&view].active_tab_id;
        assert!(host.close_tab(view, last).is_err());
        assert_eq!(host.layout.views.len(), 1);
        assert!(host.layout.validate().is_ok());
    }

    #[test]
    fn closing_the_last_tab_of_a_secondary_view_closes_that_view() {
        let mut host = PaneHost::new();
        let view = host.split_focused_view(Direction::Right).unwrap();
        let tab = host.layout.views[&view].active_tab_id;
        host.close_tab(view, tab).unwrap();
        assert_eq!(host.layout.views.len(), 1);
        assert!(host.is_trivial());
        assert_eq!(host.focused_view(), Some(zeron_workspace::ViewId(1)));
        host.layout.validate().unwrap();
    }

    #[test]
    fn tool_picker_rows_commit_their_pane_states() {
        // The pure half of the row → engine-op mapping: rows are exactly the
        // real new-session entry points, and each maps to a coherent pane.
        assert_eq!(TOOL_PICKER_ROWS.len(), 2);
        let chat = tool_pane_state(ToolKind::Chat);
        assert_eq!(chat.mode, PaneMode::Chat);
        assert_eq!(chat.session_id, None);
        let terminal = tool_pane_state(ToolKind::Terminal);
        assert_eq!(terminal.mode, PaneMode::Terminal);
        // A committed Terminal split lands as a Terminal pane.
        let mut host = PaneHost::new();
        let pane = host
            .split_focused_pane_with(Direction::Right, tool_pane_state(ToolKind::Terminal))
            .unwrap();
        assert_eq!(host.layout.pane(pane).unwrap().mode, PaneMode::Terminal);
        host.layout.validate().unwrap();
    }

    // ---- WS4: drag-commit wrappers ----

    #[test]
    fn merging_a_tab_beside_a_pane_moves_the_whole_subtree_and_focuses_it() {
        let mut host = PaneHost::new();
        // Default: V1/tab2/pane3. Split → pane4 (same tab). Add tab → tab5
        // with pane6.
        let _split = host.split_focused_pane(Direction::Down).unwrap();
        let added_tab = host.add_tab_to_view(ViewId(1)).unwrap();
        assert_eq!(host.layout.views[&ViewId(1)].tabs.len(), 2);
        // Drop tab5 on pane3's right edge: the tab disappears, its pane joins
        // pane3's tab as the right half, and the moved subtree's active pane
        // takes focus (engine `merge_tab`).
        host.merge_tab_into_pane(added_tab, PaneId(3), Direction::Right)
            .unwrap();
        assert_eq!(host.layout.views[&ViewId(1)].tabs.len(), 1);
        let tab = host.layout.views[&ViewId(1)].active_tab_id;
        assert_eq!(host.layout.views[&ViewId(1)].tabs[&tab].panes.len(), 3);
        assert_eq!(host.focused_pane(), Some(PaneId(6)));
        host.layout.validate().unwrap();
        // Merging a tab beside its OWN pane is refused and rolls back.
        assert!(
            host.merge_tab_into_pane(tab, PaneId(3), Direction::Right)
                .is_err()
        );
        assert!(host.layout.validate().is_ok());
        assert_eq!(host.layout.views[&ViewId(1)].tabs[&tab].panes.len(), 3);
    }

    #[test]
    fn header_dropped_on_a_strip_moves_a_single_pane_tab_whole() {
        let mut host = PaneHost::new();
        let second_view = host.split_focused_view(Direction::Right).unwrap();
        // pane3 is the ONLY pane of tab2 in view V1: pane_to_tab moves the
        // whole tab into the target view (the emptied source view closes).
        let tab = host.pane_header_to_tab(PaneId(3), second_view).unwrap();
        assert_eq!(host.layout.views.len(), 1);
        assert_eq!(tab, TabId(2));
        assert_eq!(
            host.layout.pane_location(PaneId(3)),
            Some((second_view, TabId(2)))
        );
        assert_eq!(host.focused_view(), Some(second_view));
        host.layout.validate().unwrap();
    }

    #[test]
    fn move_pane_beside_refuses_self_and_keeps_the_tree_valid() {
        let mut host = PaneHost::new();
        assert!(
            host.move_pane_beside(PaneId(3), PaneId(3), Direction::Right)
                .is_err()
        );
        assert!(host.is_trivial());
        host.layout.validate().unwrap();
        let new_pane = host.split_focused_pane(Direction::Right).unwrap();
        // pane4 sits RIGHT of pane3; dropping it on pane3's right edge is the
        // sibling-in-direction case → the panes SWAP, no new split nests.
        host.move_pane_beside(new_pane, PaneId(3), Direction::Right)
            .unwrap();
        assert_eq!(host.focused_pane(), Some(new_pane));
        let tab = host.layout.views[&ViewId(1)].active_tab_id;
        assert_eq!(
            host.layout.views[&ViewId(1)].tabs[&tab].active_pane_id,
            new_pane
        );
        host.layout.validate().unwrap();
    }

    #[test]
    fn strip_reorder_wrapper_no_ops_in_place_and_moves_across_views() {
        let mut host = PaneHost::new();
        let view = ViewId(1);
        let first = host.layout.views[&view].active_tab_id;
        let second = host.add_tab_to_view(view).unwrap();
        // Same position: the engine's in-place early-return — the tree is
        // unchanged (the restore no-op; the revision counter alone ticks).
        host.reorder_tab_in_view(second, view, Some(second))
            .unwrap();
        assert_eq!(host.layout.views[&view].ordered_tabs(), vec![first, second]);
        // Reorder before the first chip.
        host.reorder_tab_in_view(second, view, Some(first)).unwrap();
        assert_eq!(host.layout.views[&view].ordered_tabs(), vec![second, first]);
        // A header dropped on the strip of its own view pops the pane out to
        // its own tab (pane_to_tab on a multi-pane tab).
        let split_pane = host.split_focused_pane(Direction::Right).unwrap();
        let popped = host.pane_header_to_tab(split_pane, view).unwrap();
        assert_eq!(host.layout.views[&view].tabs.len(), 3);
        assert_eq!(host.layout.pane_location(split_pane), Some((view, popped)));
        host.layout.validate().unwrap();
    }

    // ---- WS5: persistence dirty latch + restore ----

    #[test]
    fn every_mutation_path_latches_dirty_and_failures_do_not() {
        let mut host = PaneHost::new();
        assert!(!host.is_dirty());
        host.sync_focused_session(Some("chat-a"));
        assert!(host.is_dirty(), "session binding is a persisted change");
        host.take_dirty();
        host.focus_pane(PaneId(3)).unwrap();
        assert!(
            !host.is_dirty(),
            "refocusing the already-focused pane is not a change"
        );
        host.split_focused_pane(Direction::Right).unwrap();
        assert!(host.is_dirty());
        host.take_dirty();
        // A pane-level ratio commit latches (the root is still a leaf, so
        // the split lives inside the view's tab).
        host.set_pane_ratio(ViewId(1), TabId(2), &[], 0.7).unwrap();
        assert!(host.is_dirty());
        host.take_dirty();
        // A real close latches; the guards (last pane / last view) do not.
        host.close_pane(PaneId(3)).unwrap();
        assert!(host.is_dirty());
        host.take_dirty();
        assert!(
            host.close_pane(PaneId(4)).is_err(),
            "the last pane cannot close"
        );
        assert!(
            host.close_focused_view().is_err(),
            "the last view cannot close"
        );
        assert!(!host.is_dirty(), "engine rejections must not arm a save");
        assert!(host.layout.validate().is_ok());
        // install_layout is a LOAD, not a mutation: it clears the latch.
        host.split_focused_view(Direction::Right).unwrap();
        assert!(host.is_dirty());
        host.install_layout(zeron_workspace::WorkspaceLayout::new());
        assert!(!host.is_dirty());
        assert!(host.is_trivial());
        assert!(host.chat_surfaces.is_empty(), "caches reset with the tree");
    }

    #[test]
    fn layout_survives_a_store_round_trip_with_focus_and_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = PaneHost::new();
        // Build through public ops only: splits, ratios, tabs, focus, and a
        // session binding — the exact surface a real session produces.
        host.sync_focused_session(Some("chat-a"));
        let second_view = host.split_focused_view(Direction::Right).unwrap();
        host.split_focused_pane(Direction::Down).unwrap();
        host.set_view_ratio(&[], 0.68).unwrap();
        let first_tab = host.layout.views[&ViewId(1)].active_tab_id;
        let added_tab = host.add_tab_to_view(ViewId(1)).unwrap();
        host.add_tab_with(second_view, tool_pane_state(ToolKind::Terminal))
            .unwrap();
        // Land focus on the original session's pane and ITS tab.
        host.focus_tab(ViewId(1), first_tab).unwrap();
        host.focus_pane(PaneId(3)).unwrap();
        assert_eq!(host.focused_pane(), Some(PaneId(3)));

        let mut store = crate::workspace_layout_store::WorkspaceLayoutStore::load(dir.path());
        store.set_layout(Some("space-a"), host.layout.clone());
        store.flush().unwrap();
        let reloaded = crate::workspace_layout_store::WorkspaceLayoutStore::load(dir.path());
        let restored = reloaded.layout_for(Some("space-a")).unwrap();

        // The engine's validated Deserialize is part of the round trip, so
        // equality implies structural validity.
        assert_eq!(restored, host.layout);
        restored.validate().unwrap();
        assert_eq!(
            restored.active_pane_id(),
            Some(PaneId(3)),
            "focused pane persists (Super's active_pane_id)"
        );
        assert_eq!(
            restored.views[&ViewId(1)].active_tab_id,
            first_tab,
            "per-view active tab persists"
        );
        assert!(restored.views[&ViewId(1)].tabs.contains_key(&added_tab));
        assert_eq!(
            restored.pane(PaneId(3)).unwrap().session_id.as_deref(),
            Some("chat-a"),
            "session bindings persist"
        );

        // Installing the restored tree reproduces the host exactly.
        host.install_layout(restored);
        assert_eq!(host.focused_pane(), Some(PaneId(3)));
        assert_eq!(host.layout.views[&second_view].tabs.len(), 2);
        assert!(
            host.layout.views[&second_view]
                .tabs
                .values()
                .any(|tab| tab.panes.values().any(|p| p.mode == PaneMode::Terminal)),
            "terminal tabs restore as tabs (never as PTYs)"
        );
        assert!(!host.is_dirty());
    }

    #[test]
    fn stale_session_panes_finds_dead_and_cross_space_bindings() {
        let mut host = PaneHost::new();
        // Pane 3 binds a live session, the split pane a dead one.
        host.sync_focused_session(Some("alive"));
        host.split_focused_pane(Direction::Right).unwrap();
        host.sync_focused_session(Some("dead"));
        let dead_pane = host.focused_pane().unwrap();
        let dead = stale_session_panes(&host.layout.views, |s| s != "dead");
        assert_eq!(dead, vec![dead_pane]);
        // The predicate defines staleness: keep sessions starting "s1".
        let mut host2 = PaneHost::new();
        host2.sync_focused_session(Some("s1-chat"));
        host2.split_focused_pane(Direction::Right).unwrap();
        host2.sync_focused_session(Some("s2-chat"));
        let foreign = stale_session_panes(&host2.layout.views, |s| s.starts_with("s1"));
        assert_eq!(foreign, vec![host2.focused_pane().unwrap()]);
        // Session-less panes are never stale.
        let empty = stale_session_panes(&PaneHost::new().layout.views, |_: &str| false);
        assert!(empty.is_empty());
    }
}

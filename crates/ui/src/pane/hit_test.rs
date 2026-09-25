//! Pane hit-testing for drag & drop (WS4): the pure, window-free half of the
//! tab/pane drag contract, resolving pointer samples against the paint-time
//! bound registries into a [`DropPlan`].
//!
//! The live-verified contract is `super-analysis/13-interaction-truth.md` §3:
//!
//! | Cursor zone                                   | Resolution                        |
//! |-----------------------------------------------|-----------------------------------|
//! | pane CENTER                                   | move into that pane's tab group; preview = the pane's full rect |
//! | INTERIOR edge (engine's outer-20% rule)       | pane-level split; preview = the half adjacent to the hovered edge |
//! | workspace outer edge beyond a boundary pane | view-level split; preview = the incoming half of that view |
//! | a view's tab strip                            | reorder (same view) / move into that strip (other views); preview = a 2px insertion marker |
//!
//! All geometry is plain `f32` rects (no GPUI types in the math), so the
//! whole matrix is unit-testable without a window. [`resolve_drop`] is the
//! single entry point; it is driven per pointer sample by
//! `Shell::apply_split_drag_move` (`shell/panes.rs`) and committed on
//! mouse-up.
//!
//! Constants (the drag feel, all documented here because they have no other
//! home):
//! - [`FLIP_SMOOTH_PX`] - zone-flip smoothing. A preview anchored to a pane
//!   only flips to another target once the pointer is ~4px past that pane's
//!   edge, so hovering the hairline gap/divider between two panes never
//!   flickers the preview between them.
//! - [`BOUNDARY_EPSILON_PX`] - how close a pane edge must sit to the
//!   workspace content edge to count as a "boundary pane" edge. Flush panes
//!   sit exactly ON the content edge; the epsilon keeps sub-pixel rounding
//!   and slop-covered samples on the boundary side.
//! - [`STRIP_BAND_PX`] - slack above/below a strip's chips so drops slightly
//!   off the 22px chips still land on the strip.

use zeron_workspace::{Direction, PaneId, TabId, ViewId, edge_zone};

/// Zone-flip smoothing: a pane keeps owning the preview until the pointer is
/// this far past its edge (§3 "require ~4px past the pane edge before a
/// preview flips zones"). Also the expansion used when no pane exactly
/// contains the pointer (the divider gap): the nearest pane wins.
pub(crate) const FLIP_SMOOTH_PX: f32 = 4.0;

/// A pane edge within this distance of the workspace content edge counts as
/// the workspace-outer edge (→ view-level split). Explicit, NOT derived from
/// `render::PANE_TREE_PAD_PX`: that gutter is now zero, but the boundary
/// tolerance must stay at its historical effective value so flush pane edges
/// (plus sub-pixel snapping) still read as the workspace boundary.
pub(crate) const BOUNDARY_EPSILON_PX: f32 = 8.0;

/// Vertical slack added above/below a tab strip's chips when hit-testing the
/// strip row (chips are 22px tall in a 30px row).
pub(crate) const STRIP_BAND_PX: f32 = 4.0;

/// Maximum distance from the workspace edge for a view split. The pointer
/// must also be on a boundary pane's edge. Drops past the ring, inside a
/// pane, remain pane splits regardless of the number of siblings.
pub(crate) const OUTER_RING_PX: f32 = 18.0;

/// How far outside a pane's rect a pointer may sit and still hit it: the
/// historical pane-tree gutter (6px) plus the zone-flip smoothing slack.
/// Explicit, NOT derived from `render::PANE_TREE_PAD_PX` (now zero): the
/// tolerance must stay at its historical effective value so divider gaps and
/// sub-pixel samples keep resolving to the adjacent pane. The `content`
/// containment check keeps it from reaching the sidebar.
pub(crate) const HIT_SLOP_PX: f32 = 6.0 + FLIP_SMOOTH_PX;

/// An axis-aligned `f32` rect in window coordinates. Inclusive right/bottom
/// edges (matching [`edge_zone`]'s containment).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub(crate) fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// From a GPUI paint-time bound (the registries store window-space
    /// `Bounds<Pixels>`).
    pub(crate) fn from_bounds(b: gpui::Bounds<gpui::Pixels>) -> Self {
        Self {
            x: b.origin.x.into(),
            y: b.origin.y.into(),
            w: b.size.width.into(),
            h: b.size.height.into(),
        }
    }

    pub(crate) fn right(&self) -> f32 {
        self.x + self.w
    }

    pub(crate) fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub(crate) fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub(crate) fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }

    /// The rect grown by `m` on every side.
    pub(crate) fn expanded(&self, m: f32) -> Rect {
        Rect::new(self.x - m, self.y - m, self.w + 2.0 * m, self.h + 2.0 * m)
    }

    /// The rect shrunk by `m` on every side, clamping at zero.
    pub(crate) fn inset(&self, m: f32) -> Rect {
        Rect::new(
            self.x + m,
            self.y + m,
            (self.w - 2.0 * m).max(0.0),
            (self.h - 2.0 * m).max(0.0),
        )
    }

    pub(crate) fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    /// Non-finite or empty rects are skipped by resolution, never panic.
    pub(crate) fn valid(&self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite())
            && self.w > 0.0
            && self.h > 0.0
    }

    /// The incoming split's footprint, excluding the shared resize gutter.
    pub(crate) fn half_adjacent(&self, dir: Direction) -> Rect {
        let width = ((self.w - super::DIVIDER_HIT_PX) / 2.0).max(0.0);
        let height = ((self.h - super::DIVIDER_HIT_PX) / 2.0).max(0.0);
        match dir {
            Direction::Left => Rect::new(self.x, self.y, width, self.h),
            Direction::Right => Rect::new(self.right() - width, self.y, width, self.h),
            Direction::Up => Rect::new(self.x, self.y, self.w, height),
            Direction::Down => Rect::new(self.x, self.bottom() - height, self.w, height),
        }
    }
}

/// The engine's outer-20% rule over a pane rect with a window-space pointer.
pub(crate) fn edge_at(rect: &Rect, x: f32, y: f32) -> Option<Direction> {
    edge_zone(
        f64::from(x - rect.x),
        f64::from(y - rect.y),
        f64::from(rect.w),
        f64::from(rect.h),
    )
}

/// The workspace content region inside an outlet hitbox: the outlet pads the
/// sides and bottom by `pad` and the top by `top_pad` (the unified titlebar
/// band), and the view regions paint exactly inside the remainder. Boundary
/// math must compare pane edges against THIS region - the raw outlet hitbox
/// includes the padding, which pushed every real edge past the boundary
/// epsilon. Degenerate inputs clamp to an empty rect, which [`Rect::valid`]
/// rejects so resolution fails closed.
pub(crate) fn outlet_content(outlet: &Rect, pad: f32, top_pad: f32) -> Rect {
    Rect::new(
        outlet.x + pad,
        outlet.y + top_pad,
        (outlet.w - 2.0 * pad).max(0.0),
        (outlet.h - pad - top_pad).max(0.0),
    )
}

/// The synthetic one-pane workspace the legacy single-pane drop zone
/// resolves against: the outlet's content region becomes the sole view
/// region, and the focused pane sits the pane-tree gutter inside it - the
/// same inset [`pane::render`](super::render) paints (zero now: the pane is
/// flush, so the synthetic rects match what the post-drop workspace shows).
/// Shared by [`resolve_single_pane_drop`] and the shell's existing-session
/// lookup so both see identical geometry.
pub(crate) fn single_pane_geometry(
    outlet: &Rect,
    pane: PaneId,
    view: ViewId,
    tab: TabId,
) -> WorkspaceGeometry {
    let content = outlet_content(
        outlet,
        super::render::OUTLET_PAD_PX,
        super::render::OUTLET_TOP_PAD_PX,
    );
    let pane_rect = content.inset(super::render::PANE_TREE_PAD_PX);
    WorkspaceGeometry {
        content,
        panes: vec![PaneRect {
            pane,
            view,
            tab,
            rect: pane_rect,
        }],
        views: vec![ViewRect {
            view,
            rect: content,
            tab_count: 1,
        }],
        strips: Vec::new(),
    }
}

/// The single-pane (non-workspace) drop zone over the legacy content area:
/// resolved through the same [`resolve_drop`] matrix as the workspace
/// outlet, against the synthetic one-pane geometry the post-drop workspace
/// will paint: the outer edge is a view split, the pane's outer-20% zone
/// a pane split, and the center a `MoveIntoPane` tab join.
/// Previews therefore describe the post-drop inset pane, never the raw
/// window edge.
pub(crate) fn resolve_single_pane_drop(
    outlet: &Rect,
    pane: PaneId,
    view: ViewId,
    tab: TabId,
    x: f32,
    y: f32,
    anchor: Option<PaneId>,
) -> DropResolution {
    resolve_drop(
        &single_pane_geometry(outlet, pane, view, tab),
        x,
        y,
        DragSource::SidebarSession,
        anchor,
    )
}

/// Whether the pane's edge on `dir`'s side sits at the workspace content
/// boundary (within [`BOUNDARY_EPSILON_PX`]) - i.e. a drop there is a
/// view-level split, not a pane-level one (§3).
pub(crate) fn touches_boundary(rect: &Rect, content: &Rect, dir: Direction) -> bool {
    let near = |a: f32, b: f32| (a - b).abs() <= BOUNDARY_EPSILON_PX;
    match dir {
        Direction::Left => near(rect.x, content.x),
        Direction::Right => near(rect.right(), content.right()),
        Direction::Up => near(rect.y, content.y),
        Direction::Down => near(rect.bottom(), content.bottom()),
    }
}

/// What started the drag. `TabChip` carries the tab and the view currently
/// hosting it (same-strip drops restore/reorder; anything else moves it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DragSource {
    TabChip(TabId, ViewId),
    PaneHeader(PaneId),
    /// A sidebar chat row dragged onto the content area. No workspace IDs
    /// exist yet - the commit path creates the split and binds the session.
    /// Never a self-hit.
    SidebarSession,
}

/// One pane's resolved registry entry. `view`/`tab` come from the engine (the
/// active tab is the only one with painted panes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaneRect {
    pub pane: PaneId,
    pub view: ViewId,
    pub tab: TabId,
    pub rect: Rect,
}

/// One top-level view region + its tab count (the "cannot extract the last
/// tab of the last view" guard reads it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ViewRect {
    pub view: ViewId,
    pub rect: Rect,
    pub tab_count: usize,
}

/// One tab chip's rect within a view's strip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChipRect {
    pub tab: TabId,
    pub rect: Rect,
}

/// The paint-time geometry snapshot [`resolve_drop`] consumes. Built per
/// pointer sample from [`crate::pane::PaneHost`]'s registries
/// (`shell/panes.rs`); constructed directly by tests.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorkspaceGeometry {
    pub content: Rect,
    pub panes: Vec<PaneRect>,
    pub views: Vec<ViewRect>,
    /// Per-view strip chips, left→right.
    pub strips: Vec<(ViewId, Vec<ChipRect>)>,
}

/// Where a drop lands - the pure half of the commit. `None` = invalid drop,
/// commit no-ops (drag-over-sidebar, gaps outside every pane, guarded
/// self-drops).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DropPlan {
    None,
    /// Center drop: the dragged tab joins `view`'s strip (`tab_before` from a
    /// strip drop; `None` = append). Header sources mean `pane_to_tab`.
    MoveIntoPane {
        view: ViewId,
        tab_before: Option<TabId>,
    },
    /// Interior-edge drop: split at `pane` toward `direction`; the dragged
    /// content becomes the new half adjacent to the hovered edge.
    SplitPane { pane: PaneId, direction: Direction },
    /// Workspace-outer-edge drop on a boundary pane: split the workspace at
    /// `view` (the adjacent top-level region) toward `direction`.
    ///
    /// Deviation from the spec sketch (`SplitView(direction)`): the engine
    /// op (`tab_to_view`/`pane_to_view`) needs the TARGET view - with ≥3
    /// views, direction alone is ambiguous - and the preview rect is that
    /// view's region, so the id rides along.
    SplitView { view: ViewId, direction: Direction },
    /// Same-strip drop of a tab chip: reorder within the strip (`before`).
    ReorderStrip {
        view: ViewId,
        before: Option<TabId>,
    },
    /// A sidebar session already bound somewhere in the layout: the drop
    /// focuses its existing pane instead of minting a second binding.
    /// Synthesized by the shell (the pure resolver never produces it).
    FocusPane { pane: PaneId },
}

/// How a resolved preview paints - the plan's own visual contract, carried
/// explicitly so the renderer never infers semantics from rect dimensions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PreviewKind {
    /// A pane-level split: the half-pane adjacent to the hovered edge.
    PaneHalf,
    /// A view-level split: the incoming half of the targeted view region.
    ViewHalf,
    /// A center/tab-join or existing-pane focus: a wash over the full pane.
    FullTarget,
    /// A strip drop: the thin insertion line at the resolved position.
    Insertion,
}

/// A resolved preview: the window-space rect to paint plus its kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DropPreview {
    pub rect: Rect,
    pub kind: PreviewKind,
}

/// A resolved sample: the plan plus the preview rect to wash+ring (window
/// space) and the pane the resolution is anchored to (the next sample's
/// hysteresis input - see [`FLIP_SMOOTH_PX`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DropResolution {
    pub plan: DropPlan,
    pub preview: Option<DropPreview>,
    pub anchor: Option<PaneId>,
}

impl DropResolution {
    pub(crate) fn none() -> Self {
        Self {
            plan: DropPlan::None,
            preview: None,
            anchor: None,
        }
    }
}

/// Resolve one pointer sample. `anchor` is the pane the CURRENT preview is
/// anchored to (from the previous [`DropResolution`]); while the pointer
/// stays within [`FLIP_SMOOTH_PX`] of that pane's rect, the resolution keeps
/// it - the zone-flip smoothing of §3.
pub(crate) fn resolve_drop(
    geom: &WorkspaceGeometry,
    x: f32,
    y: f32,
    source: DragSource,
    anchor: Option<PaneId>,
) -> DropResolution {
    if !x.is_finite() || !y.is_finite() || !geom.content.valid() || !geom.content.contains(x, y)
    {
        return DropResolution::none();
    }

    // 1. Tab strips first: dropping "back on the strip" must reliably
    //    restore/reorder, so the strip row wins over any expanded pane edge.
    for (view, chips) in &geom.strips {
        let Some(band) = strip_band(chips) else {
            continue;
        };
        if !band.contains(x, y) {
            continue;
        }
        let before = strip_insert_before(chips, x);
        let plan = match source {
            DragSource::TabChip(_tab, src_view) if src_view == *view => {
                DropPlan::ReorderStrip {
                    view: *view,
                    before,
                }
            }
            // Another view's chip, or a pane header: join this strip.
            DragSource::TabChip(..) | DragSource::PaneHeader(..) | DragSource::SidebarSession => {
                DropPlan::MoveIntoPane {
                    view: *view,
                    tab_before: before,
                }
            }
        };
        if extraction_doomed(geom, source, &plan) {
            return DropResolution::none();
        }
        return DropResolution {
            plan,
            preview: strip_insertion_preview(chips, before).map(|rect| DropPreview {
                rect,
                kind: PreviewKind::Insertion,
            }),
            anchor: None,
        };
    }

    // 2. Pane hit with hysteresis.
    let Some(hit) = hit_pane(geom, x, y, anchor) else {
        return DropResolution::none();
    };

    // 3. Zone per the engine's outer-20% rule. Local coordinates clamp into
    //    the pane rect so a pointer in the smoothing band / divider gap (the
    //    hysteresis and nearest-center hits above land just OUTSIDE the rect,
    //    where `edge_zone` would read `None` = center) holds the EDGE
    //    preview instead of flickering to a no-indicator center move.
    let local_x = x.clamp(hit.rect.x, hit.rect.right());
    let local_y = y.clamp(hit.rect.y, hit.rect.bottom());
    match edge_at(&hit.rect, local_x, local_y) {
        Some(direction) => {
            // Guarded self-drops: a header over its own pane's edge would
            // `move_pane` a pane beside itself; a chip over a pane of ITS OWN
            // tab would merge a tab into itself. Both preview as None.
            if self_hit(source, hit) {
                return DropResolution::none();
            }
            if extraction_doomed(geom, source, &DropPlan::SplitPane {
                pane: hit.pane,
                direction,
            }) {
                return DropResolution::none();
            }
            if view_level_drop(geom, hit, x, y, direction) {
                DropResolution {
                    plan: DropPlan::SplitView {
                        view: hit.view,
                        direction,
                    },
                    preview: geom
                        .views
                        .iter()
                        .find(|v| v.view == hit.view)
                        .map(|v| DropPreview {
                            rect: v.rect.half_adjacent(direction),
                            kind: PreviewKind::ViewHalf,
                        }),
                    anchor: Some(hit.pane),
                }
            } else {
                DropResolution {
                    plan: DropPlan::SplitPane {
                        pane: hit.pane,
                        direction,
                    },
                    preview: Some(DropPreview {
                        rect: hit.rect.half_adjacent(direction),
                        kind: PreviewKind::PaneHalf,
                    }),
                    anchor: Some(hit.pane),
                }
            }
        }
        // Center: move into the hovered pane's tab group, previewed as the
        // pane's full rect - except a self-hit, which stays an honest no-op.
        None => {
            if self_hit(source, hit) {
                return DropResolution::none();
            }
            DropResolution {
                plan: DropPlan::MoveIntoPane {
                    view: hit.view,
                    tab_before: None,
                },
                preview: Some(DropPreview {
                    rect: hit.rect,
                    kind: PreviewKind::FullTarget,
                }),
                anchor: Some(hit.pane),
            }
        }
    }
}

/// Whether the pointer sits within [`OUTER_RING_PX`] of `content`'s edge on
/// `direction`'s side - the outermost workspace ring that resolves to a
/// view-level split even on a single-pane layout.
pub(crate) fn in_outer_ring(content: &Rect, x: f32, y: f32, direction: Direction) -> bool {
    match direction {
        Direction::Left => x - content.x <= OUTER_RING_PX,
        Direction::Right => content.right() - x <= OUTER_RING_PX,
        Direction::Up => y - content.y <= OUTER_RING_PX,
        Direction::Down => content.bottom() - y <= OUTER_RING_PX,
    }
}

/// Whether an edge hit on a boundary pane resolves to a view-level split:
/// the pane's hovered edge must touch the workspace content boundary and the
/// pointer must sit inside the workspace's outer ring. Flush panes have no
/// gutter outside the pane any more, so the ring IS the workspace-edge drop
/// zone (the pointer is necessarily inside the pane there); past the ring,
/// an edge drop splits the pane it is inside.
fn view_level_drop(
    geom: &WorkspaceGeometry,
    hit: &PaneRect,
    x: f32,
    y: f32,
    direction: Direction,
) -> bool {
    touches_boundary(&hit.rect, &geom.content, direction)
        && in_outer_ring(&geom.content, x, y, direction)
}

/// Whether the plan would extract the dragged tab out of a view the engine
/// must refuse to empty - the last tab of the LAST view. Suppressing the
/// preview keeps the drop an honest no-op (engine guards stay the backstop).
fn extraction_doomed(geom: &WorkspaceGeometry, source: DragSource, plan: &DropPlan) -> bool {
    let DragSource::TabChip(_, src_view) = source else {
        return false;
    };
    let Some(entry) = geom.views.iter().find(|v| v.view == src_view) else {
        return false;
    };
    if geom.views.len() != 1 || entry.tab_count > 1 {
        return false;
    }
    match plan {
        // The tab leaves its view for a fresh one / another pane's half.
        DropPlan::SplitView { .. } | DropPlan::SplitPane { .. } => true,
        DropPlan::MoveIntoPane { view, .. } => *view != src_view,
        DropPlan::ReorderStrip { .. } | DropPlan::None | DropPlan::FocusPane { .. } => false,
    }
}

/// Guarded self-hits: the drag source is already part of the hovered pane.
fn self_hit(source: DragSource, hit: &PaneRect) -> bool {
    match source {
        DragSource::PaneHeader(pane) => pane == hit.pane,
        DragSource::TabChip(tab, _) => tab == hit.tab,
        DragSource::SidebarSession => false,
    }
}

/// The pane owning the pointer: the anchored pane while within
/// [`HIT_SLOP_PX`] of its rect (hysteresis + divider-gap coverage), else
/// exact containment, else the nearest center among the slop-expanded panes
/// (the divider gap). `resolve_drop` only calls here for pointers already
/// inside `content`, so the slop can cover a divider band without ever
/// capturing the sidebar.
fn hit_pane<'a>(
    geom: &'a WorkspaceGeometry,
    x: f32,
    y: f32,
    anchor: Option<PaneId>,
) -> Option<&'a PaneRect> {
    if let Some(id) = anchor {
        if let Some(p) = geom
            .panes
            .iter()
            .find(|p| p.pane == id && p.rect.valid() && p.rect.expanded(HIT_SLOP_PX).contains(x, y))
        {
            return Some(p);
        }
    }
    if let Some(p) = geom
        .panes
        .iter()
        .find(|p| p.rect.valid() && p.rect.contains(x, y))
    {
        return Some(p);
    }
    geom.panes
        .iter()
        .filter(|p| {
            p.rect.valid() && p.rect.expanded(HIT_SLOP_PX).contains(x, y)
        })
        .min_by(|a, b| {
            let dist = |p: &&PaneRect| {
                let (cx, cy) = p.rect.center();
                let dx = cx - x;
                let dy = cy - y;
                dx * dx + dy * dy
            };
            dist(a).total_cmp(&dist(b))
        })
}

/// The strip's hit band: the union of its chips, padded vertically by
/// [`STRIP_BAND_PX`]. Chips only - the strip's "+" and empty space resolve to
/// None (a no-op restore) rather than guessing an append.
fn strip_band(chips: &[ChipRect]) -> Option<Rect> {
    let mut band: Option<Rect> = None;
    for chip in chips.iter().filter(|c| c.rect.valid()) {
        band = Some(match band {
            Some(b) => b.union(&chip.rect),
            None => chip.rect,
        });
    }
    band.map(|b| Rect::new(b.x, b.y - STRIP_BAND_PX, b.w, b.h + 2.0 * STRIP_BAND_PX))
}

/// Insertion point within a strip: the first chip whose center is right of
/// the pointer (insert before it); `None` = append after the last chip.
fn strip_insert_before(chips: &[ChipRect], x: f32) -> Option<TabId> {
    chips
        .iter()
        .find(|c| c.rect.valid() && c.rect.center().0 > x)
        .map(|c| c.tab)
}

/// The strip drop's preview: a 2px-wide vertical marker at the insertion
/// point - the target chip's left edge for `before`, the last chip's right
/// edge for an append. Its vertical span is the chips' union inset by 2px
/// top and bottom. `None` when the strip has no valid chips.
fn strip_insertion_preview(chips: &[ChipRect], before: Option<TabId>) -> Option<Rect> {
    let mut band: Option<Rect> = None;
    for chip in chips.iter().filter(|c| c.rect.valid()) {
        band = Some(match band {
            Some(b) => b.union(&chip.rect),
            None => chip.rect,
        });
    }
    let band = band?;
    let edge = match before {
        Some(tab) => chips
            .iter()
            .find(|c| c.tab == tab && c.rect.valid())
            .map(|c| c.rect.x),
        None => chips
            .iter()
            .filter(|c| c.rect.valid())
            .last()
            .map(|c| c.rect.right()),
    }?;
    Some(Rect::new(
        edge - 1.0,
        band.y + 2.0,
        2.0,
        (band.h - 4.0).max(0.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: ViewId = ViewId(1);
    const V2: ViewId = ViewId(2);
    const V3: ViewId = ViewId(3);
    const T1: TabId = TabId(11);
    const T2: TabId = TabId(12);
    const T3: TabId = TabId(13);

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn pane(id: u64, view: ViewId, tab: TabId, r: Rect) -> PaneRect {
        PaneRect {
            pane: PaneId(id),
            view,
            tab,
            rect: r,
        }
    }

    fn preview(r: Rect, kind: PreviewKind) -> Option<DropPreview> {
        Some(DropPreview { rect: r, kind })
    }

    fn view(id: ViewId, r: Rect, tab_count: usize) -> ViewRect {
        ViewRect {
            view: id,
            rect: r,
            tab_count,
        }
    }

    fn chip(tab: TabId, r: Rect) -> ChipRect {
        ChipRect { tab, rect: r }
    }

    /// One view (V1) holding a 2×2 pane grid; strip chips T1, T2.
    fn two_by_two() -> WorkspaceGeometry {
        WorkspaceGeometry {
            content: rect(0.0, 0.0, 1000.0, 800.0),
            panes: vec![
                pane(1, V1, T1, rect(0.0, 30.0, 500.0, 385.0)),
                pane(2, V1, T1, rect(500.0, 30.0, 500.0, 385.0)),
                pane(3, V1, T1, rect(0.0, 415.0, 500.0, 385.0)),
                pane(4, V1, T1, rect(500.0, 415.0, 500.0, 385.0)),
            ],
            views: vec![view(V1, rect(0.0, 0.0, 1000.0, 800.0), 3)],
            strips: vec![(
                V1,
                vec![chip(T1, rect(8.0, 4.0, 120.0, 22.0)), chip(T2, rect(132.0, 4.0, 120.0, 22.0))],
            )],
        }
    }

    /// Three views side by side (1×3), one pane each.
    fn three_views() -> WorkspaceGeometry {
        WorkspaceGeometry {
            content: rect(0.0, 0.0, 1000.0, 800.0),
            panes: vec![
                pane(1, V1, T1, rect(0.0, 30.0, 300.0, 770.0)),
                pane(2, V2, T2, rect(300.0, 30.0, 400.0, 770.0)),
                pane(3, V3, TabId(13), rect(700.0, 30.0, 300.0, 770.0)),
            ],
            views: vec![
                view(V1, rect(0.0, 0.0, 300.0, 800.0), 1),
                view(V2, rect(300.0, 0.0, 400.0, 800.0), 2),
                view(V3, rect(700.0, 0.0, 300.0, 800.0), 1),
            ],
            strips: vec![
                (V1, vec![chip(T1, rect(8.0, 4.0, 120.0, 22.0))]),
                (V2, vec![chip(T2, rect(308.0, 4.0, 120.0, 22.0))]),
                (V3, vec![chip(TabId(13), rect(708.0, 4.0, 120.0, 22.0))]),
            ],
        }
    }

    /// One view, one tab, one full-content pane (the extraction guard layout).
    fn lone_tab() -> WorkspaceGeometry {
        WorkspaceGeometry {
            content: rect(0.0, 0.0, 800.0, 600.0),
            panes: vec![pane(1, V1, T1, rect(0.0, 30.0, 800.0, 570.0))],
            views: vec![view(V1, rect(0.0, 0.0, 800.0, 600.0), 1)],
            strips: vec![(V1, vec![chip(T1, rect(8.0, 4.0, 120.0, 22.0))])],
        }
    }

    // ---- center: move, full-pane preview ----

    #[test]
    fn pane_center_resolves_move_with_full_pane_preview() {
        let geom = two_by_two();
        let r = resolve_drop(&geom, 250.0, 200.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::MoveIntoPane {
                view: V1,
                tab_before: None
            }
        );
        assert_eq!(
            r.preview,
            preview(rect(0.0, 30.0, 500.0, 385.0), PreviewKind::FullTarget)
        );
        assert_eq!(r.anchor, Some(PaneId(1)));
    }

    #[test]
    fn pane_center_self_hit_resolves_to_none() {
        let geom = two_by_two();
        // A chip over a pane of ITS OWN tab at dead center.
        let r = resolve_drop(&geom, 250.0, 200.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(r.plan, DropPlan::None);
        assert_eq!(r.preview, None);
        // A header over its own pane's center likewise.
        let r = resolve_drop(&geom, 250.0, 200.0, DragSource::PaneHeader(PaneId(1)), None);
        assert_eq!(r.plan, DropPlan::None);
    }

    // ---- interior edges: half-pane split ----

    #[test]
    fn interior_edge_resolves_split_pane_with_half_pane_preview() {
        let geom = two_by_two();
        // p1 (0..500 × 30..415): right edge at x=500 is interior.
        let r = resolve_drop(&geom, 497.0, 200.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Right
            }
        );
        assert_eq!(
            r.preview,
            preview(rect(254.0, 30.0, 246.0, 385.0), PreviewKind::PaneHalf)
        );
        // Down edge of p1 (bottom at 415) - interior too.
        let r = resolve_drop(&geom, 250.0, 412.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Down
            }
        );
        assert_eq!(
            r.preview,
            preview(rect(0.0, 226.5, 500.0, 188.5), PreviewKind::PaneHalf)
        );
    }

    #[test]
    fn edges_between_views_are_still_pane_level_splits() {
        // §3 keys off the WORKSPACE boundary only: the seam between V1 and V2
        // is an interior edge even though the neighbor is another view.
        let geom = three_views();
        let r = resolve_drop(&geom, 297.0, 400.0, DragSource::TabChip(T2, V2), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Right
            }
        );
        let r = resolve_drop(&geom, 303.0, 400.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(2),
                direction: Direction::Left
            }
        );
    }

    // ---- workspace-outer edges: view-level split ----

    #[test]
    fn dragging_near_a_pane_edge_stays_local_until_the_outer_ring() {
        let geom = two_by_two();
        for source in [
            DragSource::SidebarSession,
            DragSource::TabChip(T2, V1),
            DragSource::PaneHeader(PaneId(1)),
        ] {
            // Past the 18px ring (30px inside the bottom-left pane) the drop
            // stays local: it splits THAT pane, not the view containing all
            // four panes.
            let local = resolve_drop(&geom, 250.0, 770.0, source, None);
            assert_eq!(local.plan, DropPlan::SplitPane {
                pane: PaneId(3), direction: Direction::Down,
            });
            assert_eq!(local.preview,
                preview(rect(0.0, 611.5, 500.0, 188.5), PreviewKind::PaneHalf));
            // Inside the ring the flush layout has no gutter left, so the
            // boundary pane's edge band targets the whole view. Its preview
            // must show the lower destination, never a full ring.
            let outer = resolve_drop(&geom, 250.0, 799.0, source, local.anchor);
            assert_eq!(outer.plan, DropPlan::SplitView {
                view: V1, direction: Direction::Down,
            });
            assert_eq!(outer.preview,
                preview(rect(0.0, 404.0, 1000.0, 396.0), PreviewKind::ViewHalf));
        }
    }

    #[test]
    fn workspace_outer_edge_resolves_split_view_with_directional_preview() {
        let geom = three_views();
        // V3's right edge is the workspace right boundary.
        let r = resolve_drop(&geom, 1000.0, 400.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V3,
                direction: Direction::Right
            }
        );
        assert_eq!(
            r.preview,
            preview(rect(854.0, 0.0, 146.0, 800.0), PreviewKind::ViewHalf)
        );
        // V1's left edge is the workspace left boundary.
        let r = resolve_drop(&geom, 0.0, 400.0, DragSource::TabChip(T2, V2), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Left
            }
        );
        assert_eq!(
            r.preview,
            preview(rect(0.0, 0.0, 146.0, 800.0), PreviewKind::ViewHalf)
        );
    }

    #[test]
    fn lone_tab_chip_is_guarded_but_a_second_tab_unlocks_extraction() {
        // One view, two panes in DIFFERENT tabs; drag T1 over the other
        // tab's pane at the workspace-outer edge → a view split moves T1 out.
        let mut geom = WorkspaceGeometry {
            content: rect(0.0, 0.0, 800.0, 600.0),
            panes: vec![
                pane(1, V1, T1, rect(0.0, 30.0, 400.0, 570.0)),
                pane(2, V1, T2, rect(400.0, 30.0, 400.0, 570.0)),
            ],
            views: vec![view(V1, rect(0.0, 0.0, 800.0, 600.0), 2)],
            strips: vec![(
                V1,
                vec![chip(T1, rect(8.0, 4.0, 120.0, 22.0)), chip(T2, rect(132.0, 4.0, 120.0, 22.0))],
            )],
        };
        let r = resolve_drop(&geom, 800.0, 300.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Right
            }
        );
        // Collapse the view to its last tab: extracting T1 is refused by the
        // engine (the last view can never empty) - the preview suppresses so
        // the drop no-ops honestly.
        geom.views[0].tab_count = 1;
        let r = resolve_drop(&geom, 800.0, 300.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(r.plan, DropPlan::None);
        // A header over the workspace-outer edge of ANOTHER pane (headers
        // only exist on ≥2-pane tabs, so the hovered pane differs from the
        // source): the pane re-docks as a new top-level region.
        let geom = three_views();
        let r = resolve_drop(&geom, 1000.0, 400.0, DragSource::PaneHeader(PaneId(1)), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V3,
                direction: Direction::Right
            }
        );
    }

    // ---- edge_zone tie order ----

    #[test]
    fn corner_ties_prefer_left_then_right_up_down() {
        // A square, non-boundary pane: at its top-left corner the left and up
        // distances tie; edge_zone's array order (left, right, up, down)
        // picks LEFT. Inset from the content edge so the workspace ring does
        // not turn this into a view-level split.
        let geom = WorkspaceGeometry {
            content: rect(0.0, 0.0, 400.0, 400.0),
            panes: vec![pane(1, V1, T1, rect(150.0, 150.0, 100.0, 100.0))],
            views: vec![view(V1, rect(0.0, 0.0, 400.0, 400.0), 2)],
            strips: vec![],
        };
        let r = resolve_drop(&geom, 160.0, 160.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Left
            }
        );
    }

    // ---- flip smoothing (hysteresis) ----

    #[test]
    fn preview_stays_on_anchor_within_the_flip_threshold() {
        let geom = three_views();
        // Drag V3's chip (tab 13, unrelated to p1/p2's tabs so no self-hit
        // guard fires). 2px past p1's right edge (x=300): the anchored pane
        // keeps the preview while the pointer stays inside its HIT_SLOP_PX
        // band (the gutter + smoothing slack), and the clamped zone read
        // holds the EDGE split, not a center move.
        let chip = DragSource::TabChip(T3, V3);
        let r = resolve_drop(&geom, 302.0, 400.0, chip, Some(PaneId(1)));
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Right
            }
        );
        // 12px past the edge - beyond the slop band: flipped to p2 (whose
        // unexpanded rect owns it).
        let r = resolve_drop(&geom, 312.0, 400.0, chip, Some(PaneId(1)));
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(2),
                direction: Direction::Left
            }
        );
        // The anchor never STEALS: with no anchor, 302 resolves to p2.
        let r = resolve_drop(&geom, 302.0, 400.0, chip, None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(2),
                direction: Direction::Left
            }
        );
    }

    // ---- strips ----

    #[test]
    fn strip_drop_reorders_within_its_own_view() {
        let geom = two_by_two();
        // Over T1's left half → insert before T1.
        let r = resolve_drop(&geom, 50.0, 11.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::ReorderStrip {
                view: V1,
                before: Some(T1)
            }
        );
        // The insertion marker is a 2px sliver at the target chip's left
        // edge, spanning the chips' union inset 2px top/bottom.
        assert_eq!(
            r.preview,
            preview(rect(7.0, 6.0, 2.0, 18.0), PreviewKind::Insertion)
        );
        // Past T2's center, still on the chip band → append.
        let r = resolve_drop(&geom, 240.0, 11.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::ReorderStrip {
                view: V1,
                before: None
            }
        );
        // The append marker sits at the last chip's right edge.
        assert_eq!(
            r.preview,
            preview(rect(251.0, 6.0, 2.0, 18.0), PreviewKind::Insertion)
        );
    }

    #[test]
    fn strip_drop_from_another_view_moves_into_that_strip() {
        let geom = three_views();
        // T1 (from V1) over V2's strip before T2's center.
        let r = resolve_drop(&geom, 350.0, 11.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::MoveIntoPane {
                view: V2,
                tab_before: Some(T2)
            }
        );
        // A move into another strip previews the same insertion marker.
        assert_eq!(
            r.preview,
            preview(rect(307.0, 6.0, 2.0, 18.0), PreviewKind::Insertion)
        );
        // A header over the same strip also joins the strip (pane_to_tab).
        let r = resolve_drop(&geom, 350.0, 11.0, DragSource::PaneHeader(PaneId(1)), None);
        assert_eq!(
            r.plan,
            DropPlan::MoveIntoPane {
                view: V2,
                tab_before: Some(T2)
            }
        );
    }

    // ---- guarded self-hits ----

    #[test]
    fn self_hits_resolve_to_none() {
        let geom = two_by_two();
        // A header over its own pane's interior edge.
        let r = resolve_drop(&geom, 497.0, 200.0, DragSource::PaneHeader(PaneId(1)), None);
        assert_eq!(r.plan, DropPlan::None);
        // A chip over a pane belonging to ITS OWN tab (every pane of the 2×2
        // grid lives in the active tab T1 - merging T1 beside its own pane
        // would be refused, so no preview).
        let r = resolve_drop(&geom, 497.0, 200.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(r.plan, DropPlan::None);
        // The same chip over ANOTHER TAB's pane is fine (merge lands there).
        let geom = three_views();
        let r = resolve_drop(&geom, 303.0, 400.0, DragSource::TabChip(T1, V1), None);
        assert!(matches!(r.plan, DropPlan::SplitPane { pane: PaneId(2), .. }));
    }

    // ---- degenerate / outside ----

    #[test]
    fn outside_content_and_degenerate_geometry_resolve_to_none() {
        let geom = two_by_two();
        // Over the sidebar (outside every rect).
        assert_eq!(
            resolve_drop(&geom, 1500.0, 400.0, DragSource::TabChip(T1, V1), None).plan,
            DropPlan::None
        );
        // Non-finite pointer.
        assert_eq!(
            resolve_drop(&geom, f32::NAN, 400.0, DragSource::TabChip(T1, V1), None).plan,
            DropPlan::None
        );
        assert_eq!(
            resolve_drop(&geom, 250.0, f32::INFINITY, DragSource::TabChip(T1, V1), None).plan,
            DropPlan::None
        );
        // Zero-size content.
        let degenerate = WorkspaceGeometry {
            content: rect(0.0, 0.0, 0.0, 600.0),
            ..lone_tab()
        };
        assert_eq!(
            resolve_drop(&degenerate, 300.0, 300.0, DragSource::TabChip(T1, V1), None).plan,
            DropPlan::None
        );
        // Zero-size panes are skipped, never divide by zero.
        let flat = WorkspaceGeometry {
            content: rect(0.0, 0.0, 800.0, 600.0),
            panes: vec![pane(1, V1, T1, rect(0.0, 30.0, 800.0, 0.0))],
            views: vec![view(V1, rect(0.0, 0.0, 800.0, 600.0), 1)],
            strips: vec![],
        };
        assert_eq!(
            resolve_drop(&flat, 300.0, 300.0, DragSource::TabChip(T1, V1), None).plan,
            DropPlan::None
        );
    }

    #[test]
    fn divider_gap_falls_to_the_nearest_pane() {
        // Panes separated by an 8px divider: the gap resolves (nearest
        // center), so the preview never blanks while crossing.
        let geom = WorkspaceGeometry {
            content: rect(0.0, 0.0, 1008.0, 600.0),
            panes: vec![
                pane(1, V1, T1, rect(0.0, 30.0, 500.0, 570.0)),
                pane(2, V1, T1, rect(508.0, 30.0, 500.0, 570.0)),
            ],
            views: vec![view(V1, rect(0.0, 0.0, 1008.0, 600.0), 2)],
            strips: vec![],
        };
        // Mid-gap: nearest center wins (p1's center is closer from the left).
        let r = resolve_drop(&geom, 504.0, 300.0, DragSource::TabChip(T2, V1), None);
        assert!(matches!(r.plan, DropPlan::SplitPane { pane: PaneId(1), .. }));
        let r = resolve_drop(&geom, 504.0, 300.0, DragSource::TabChip(T2, V1), Some(PaneId(2)));
        assert!(matches!(r.plan, DropPlan::SplitPane { pane: PaneId(2), .. }));
    }

    // ---- outlet content region (the drag geometry's coordinate anchor) ----

    #[test]
    fn outlet_content_strips_the_outlet_padding() {
        // Flush panes: the outlet has no padding, so the content region the
        // view regions paint into IS the outlet hitbox.
        let outlet = rect(0.0, 0.0, 1000.0, 800.0);
        let content = outlet_content(
            &outlet,
            super::super::render::OUTLET_PAD_PX,
            super::super::render::OUTLET_TOP_PAD_PX,
        );
        assert_eq!(content, outlet);
        assert_eq!(super::super::render::OUTLET_PAD_PX, 0.0);
        assert_eq!(
            super::super::render::OUTLET_TOP_PAD_PX,
            super::super::render::OUTLET_PAD_PX,
            "the outlet top pad must not reserve Theme::TITLEBAR_HEIGHT"
        );
        // An offset outlet translates the content, never resizes it.
        let content = outlet_content(&rect(120.0, 40.0, 500.0, 400.0), 3.0, 41.0);
        assert_eq!(content, rect(123.0, 81.0, 494.0, 356.0));
        // Degenerate paddings clamp to an empty rect - rejected by `valid`,
        // so resolution fails closed instead of inventing a boundary.
        assert!(!outlet_content(&rect(0.0, 0.0, 4.0, 10.0), 3.0, 41.0).valid());
    }

    #[test]
    fn drop_tolerances_stay_at_their_historical_effective_values() {
        // The pane tree gutter is zero now, but the hit-test tolerances are
        // explicit constants, NOT derived from it: the boundary epsilon is
        // the old gutter (6) + slack (2), and the slop band the old gutter +
        // flip smoothing. This is the decoupling regression: shrinking the
        // gutter must not shrink drop tolerance.
        assert_eq!(BOUNDARY_EPSILON_PX, 8.0);
        assert_eq!(HIT_SLOP_PX, 6.0 + FLIP_SMOOTH_PX);
        assert_eq!(super::super::render::PANE_TREE_PAD_PX, 0.0);
    }

    #[test]
    fn boundary_epsilon_marks_flush_pane_edges_as_the_workspace_boundary() {
        // Flush panes: every pane edge sits exactly ON the content edge, so
        // all four sides read as the workspace boundary. The epsilon still
        // covers a few pixels of sub-pixel snap / slop.
        let content = rect(0.0, 0.0, 1000.0, 800.0);
        let flush = rect(0.0, 0.0, 1000.0, 800.0);
        for dir in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert!(
                touches_boundary(&flush, &content, dir),
                "flush {dir:?} edge must read as the workspace boundary"
            );
        }
        // Within the epsilon still counts; past it the edge is interior.
        let within = rect(
            BOUNDARY_EPSILON_PX,
            BOUNDARY_EPSILON_PX,
            1000.0 - 2.0 * BOUNDARY_EPSILON_PX,
            800.0 - 2.0 * BOUNDARY_EPSILON_PX,
        );
        for dir in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert!(touches_boundary(&within, &content, dir), "{dir:?}");
        }
        let interior = rect(
            BOUNDARY_EPSILON_PX + 1.0,
            BOUNDARY_EPSILON_PX + 1.0,
            400.0,
            400.0,
        );
        assert!(!touches_boundary(&interior, &content, Direction::Left));
        assert!(!touches_boundary(&interior, &content, Direction::Up));
    }

    // ---- single-pane (legacy) drop zones ----

    /// The legacy outlet's synthetic geometry: content inside the outlet
    /// pads, the lone pane the (zero) tree gutter inside that - the rects
    /// [`single_pane_geometry`] derives. Flush panes: content == pane rect.
    fn single_pane_rects(outlet: Rect) -> (Rect, Rect) {
        let geom = single_pane_geometry(&outlet, PaneId(1), V1, T1);
        (geom.content, geom.panes[0].rect)
    }

    #[test]
    fn single_pane_outer_edge_previews_the_new_view_destination() {
        // Flush panes: the lone pane's edges ARE the content boundary, so the
        // workspace's outer ring inside that edge resolves as a view-level
        // split, previewing the half where the new view lands. Both the exact
        // edge and a sample inside the ring resolve the same plan.
        let outlet = rect(0.0, 0.0, 800.0, 600.0);
        let (content, pane_rect) = single_pane_rects(outlet);
        assert_eq!(pane_rect, content, "the lone pane is flush with the content");
        let cases = [
            (0.0, 300.0, Direction::Left),
            (5.0, 300.0, Direction::Left),
            (800.0, 300.0, Direction::Right),
            (795.0, 300.0, Direction::Right),
            (400.0, 0.0, Direction::Up),
            (400.0, 5.0, Direction::Up),
            (400.0, 600.0, Direction::Down),
            (400.0, 595.0, Direction::Down),
        ];
        for (x, y, direction) in cases {
            let r = resolve_single_pane_drop(&outlet, PaneId(1), V1, T1, x, y, None);
            assert_eq!(
                r.plan,
                DropPlan::SplitView {
                    view: V1,
                    direction
                },
                "({x}, {y})"
            );
            assert_eq!(
                r.preview,
                preview(content.half_adjacent(direction), PreviewKind::ViewHalf),
                "({x}, {y})"
            );
            assert_eq!(r.anchor, Some(PaneId(1)), "({x}, {y})");
        }
    }

    #[test]
    fn single_pane_edge_zone_past_the_ring_resolves_pane_splits() {
        // 30px+ in from each content edge: outside the 18px view-split ring
        // but still inside the pane's outer-20% zone → a pane split with a
        // half-pane wash on the pane rect (flush with the content now).
        let outlet = rect(0.0, 0.0, 800.0, 600.0);
        let (_, pane_rect) = single_pane_rects(outlet);
        let cases = [
            (33.0, 300.0, Direction::Left),
            (767.0, 300.0, Direction::Right),
            (400.0, 33.0, Direction::Up),
            (400.0, 567.0, Direction::Down),
        ];
        for (x, y, direction) in cases {
            let r = resolve_single_pane_drop(&outlet, PaneId(1), V1, T1, x, y, None);
            assert_eq!(
                r.plan,
                DropPlan::SplitPane {
                    pane: PaneId(1),
                    direction
                },
                "({x}, {y})"
            );
            assert_eq!(
                r.preview,
                preview(pane_rect.half_adjacent(direction), PreviewKind::PaneHalf),
                "({x}, {y})"
            );
            assert_eq!(r.anchor, Some(PaneId(1)), "({x}, {y})");
        }
    }

    #[test]
    fn single_pane_drop_center_is_a_move_with_full_pane_preview() {
        let outlet = rect(0.0, 0.0, 800.0, 600.0);
        let (_, pane_rect) = single_pane_rects(outlet);
        // Dead center, and just past the 20% bands in both axes: the dropped
        // session becomes a new tab on the view - not a forced split.
        for (x, y) in [(400.0, 300.0), (300.0, 250.0)] {
            let r = resolve_single_pane_drop(&outlet, PaneId(1), V1, T1, x, y, None);
            assert_eq!(
                r.plan,
                DropPlan::MoveIntoPane {
                    view: V1,
                    tab_before: None
                },
                "({x}, {y})"
            );
            assert_eq!(
                r.preview,
                preview(pane_rect, PreviewKind::FullTarget),
                "({x}, {y})"
            );
            assert_eq!(r.anchor, Some(PaneId(1)), "({x}, {y})");
        }
    }

    #[test]
    fn single_pane_drop_is_none_off_content() {
        let outlet = rect(0.0, 0.0, 800.0, 600.0);
        // Over the sidebar / status strip: off the content entirely - even
        // within HIT_SLOP_PX of the inset pane's edge.
        assert_eq!(
            resolve_single_pane_drop(&outlet, PaneId(1), V1, T1, -1.0, 300.0, None),
            DropResolution::none()
        );
        assert_eq!(
            resolve_single_pane_drop(&outlet, PaneId(1), V1, T1, 400.0, 1000.0, None),
            DropResolution::none()
        );
        // Degenerate content never resolves.
        assert_eq!(
            resolve_single_pane_drop(&rect(0.0, 0.0, 0.0, 600.0), PaneId(1), V1, T1, 0.0, 0.0, None),
            DropResolution::none()
        );
        assert_eq!(
            resolve_single_pane_drop(&outlet, PaneId(1), V1, T1, f32::NAN, 300.0, None),
            DropResolution::none()
        );
    }

    // ---- content containment vs. the slop band ----

    #[test]
    fn outside_content_is_none_even_within_hit_slop_of_a_pane() {
        // A pane flush against the content edge: the slop band reaches past
        // the boundary, but a pointer outside `content` resolves to none - 
        // it belongs to the sidebar, never to a drop plan.
        let geom = WorkspaceGeometry {
            content: rect(0.0, 0.0, 1000.0, 800.0),
            panes: vec![pane(1, V1, T1, rect(0.0, 30.0, 500.0, 770.0))],
            views: vec![view(V1, rect(0.0, 0.0, 1000.0, 800.0), 1)],
            strips: vec![],
        };
        for (x, y) in [(-HIT_SLOP_PX / 2.0, 400.0), (1000.0 + 5.0, 400.0), (400.0, -2.0)] {
            assert_eq!(
                resolve_drop(&geom, x, y, DragSource::SidebarSession, None).plan,
                DropPlan::None,
                "({x}, {y})"
            );
        }
    }

    #[test]
    fn slop_covered_samples_around_a_boundary_pane_still_resolve_it() {
        // Synthetic inset geometry (a stale one-frame registry read or a
        // divider-band sample): a pane sitting a few px inside the content
        // edge is still a BOUNDARY pane, and the 10px slop band covers the
        // band between the content edge and the pane. Real flush panes have
        // no such band, but the tolerance must not regress.
        let geom = WorkspaceGeometry {
            content: rect(0.0, 0.0, 1000.0, 800.0),
            panes: vec![
                pane(1, V1, T1, rect(6.0, 6.0, 488.0, 788.0)),
                pane(2, V1, T1, rect(500.0, 6.0, 494.0, 788.0)),
            ],
            views: vec![view(V1, rect(0.0, 0.0, 1000.0, 800.0), 2)],
            strips: vec![],
        };
        // Left band (x in 0..6): p1's edge, touching the boundary.
        let r = resolve_drop(&geom, 3.0, 400.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Left
            }
        );
        // Right band (x in 994..1000): p2's edge.
        let r = resolve_drop(&geom, 997.0, 400.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Right
            }
        );
        // Top band (y in 0..6): p2's edge.
        let r = resolve_drop(&geom, 900.0, 3.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Up
            }
        );
        // Bottom band (y in 794..800): p1's edge.
        let r = resolve_drop(&geom, 250.0, 797.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Down
            }
        );
    }
}

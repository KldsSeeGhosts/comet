//! Pane hit-testing for drag & drop (WS4): the pure, window-free half of the
//! tab/pane drag contract, resolving pointer samples against the paint-time
//! bound registries into a [`DropPlan`].
//!
//! The live-verified contract is `super-analysis/13-interaction-truth.md` §3:
//!
//! | Cursor zone                                   | Resolution                        |
//! |-----------------------------------------------|-----------------------------------|
//! | pane CENTER                                   | move into that pane's tab group — NO indicator (verified truth) |
//! | INTERIOR edge (engine's outer-20% rule)       | pane-level split; preview = the half adjacent to the hovered edge |
//! | workspace-OUTER edge of a boundary pane       | view-level split of the adjacent top-level region; preview = that view's full rect |
//! | a view's tab strip                            | reorder (same view) / move into that strip (other views); no preview |
//!
//! All geometry is plain `f32` rects (no GPUI types in the math), so the
//! whole matrix is unit-testable without a window. [`resolve_drop`] is the
//! single entry point; it is driven per pointer sample by
//! `Shell::apply_split_drag_move` (`shell/panes.rs`) and committed on
//! mouse-up.
//!
//! Constants (the drag feel, all documented here because they have no other
//! home):
//! - [`FLIP_SMOOTH_PX`] — zone-flip smoothing. A preview anchored to a pane
//!   only flips to another target once the pointer is ~4px past that pane's
//!   edge, so hovering the hairline gap/divider between two panes never
//!   flickers the preview between them.
//! - [`BOUNDARY_EPSILON_PX`] — how close a pane edge must sit to the
//!   workspace content edge to count as a "boundary pane" edge (the outlet
//!   pads 3px per side; 6px covers padding + sub-pixel rounding).
//! - [`STRIP_BAND_PX`] — slack above/below a strip's chips so drops slightly
//!   off the 22px chips still land on the strip.

use zeron_workspace::{Direction, PaneId, TabId, ViewId, edge_zone};

/// Zone-flip smoothing: a pane keeps owning the preview until the pointer is
/// this far past its edge (§3 "require ~4px past the pane edge before a
/// preview flips zones"). Also the expansion used when no pane exactly
/// contains the pointer (the divider gap): the nearest pane wins.
pub(crate) const FLIP_SMOOTH_PX: f32 = 4.0;

/// A pane edge within this distance of the workspace content edge counts as
/// the workspace-outer edge (→ view-level split). The outlet pads 3px per
/// side; 6 covers the padding plus rounding.
pub(crate) const BOUNDARY_EPSILON_PX: f32 = 6.0;

/// Vertical slack added above/below a tab strip's chips when hit-testing the
/// strip row (chips are 22px tall in a 30px row).
pub(crate) const STRIP_BAND_PX: f32 = 4.0;

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

    /// The half of the rect adjacent to `dir` — the SplitPane preview shape
    /// (the split lands the dragged content in the half the cursor hovers).
    pub(crate) fn half_adjacent(&self, dir: Direction) -> Rect {
        match dir {
            Direction::Left => Rect::new(self.x, self.y, self.w / 2.0, self.h),
            Direction::Right => Rect::new(self.x + self.w / 2.0, self.y, self.w / 2.0, self.h),
            Direction::Up => Rect::new(self.x, self.y, self.w, self.h / 2.0),
            Direction::Down => Rect::new(self.x, self.y + self.h / 2.0, self.w, self.h / 2.0),
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

/// Whether the pane's edge on `dir`'s side sits at the workspace content
/// boundary (within [`BOUNDARY_EPSILON_PX`]) — i.e. a drop there is a
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

/// Where a drop lands — the pure half of the commit. `None` = invalid drop,
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
    /// op (`tab_to_view`/`pane_to_view`) needs the TARGET view — with ≥3
    /// views, direction alone is ambiguous — and the preview rect is that
    /// view's region, so the id rides along.
    SplitView { view: ViewId, direction: Direction },
    /// Same-strip drop of a tab chip: reorder within the strip (`before`).
    ReorderStrip {
        view: ViewId,
        before: Option<TabId>,
    },
}

/// A resolved sample: the plan plus the preview rect to wash+ring (window
/// space) and the pane the resolution is anchored to (the next sample's
/// hysteresis input — see [`FLIP_SMOOTH_PX`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DropResolution {
    pub plan: DropPlan,
    pub preview: Option<Rect>,
    pub anchor: Option<PaneId>,
}

impl DropResolution {
    fn none() -> Self {
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
/// it — the zone-flip smoothing of §3.
pub(crate) fn resolve_drop(
    geom: &WorkspaceGeometry,
    x: f32,
    y: f32,
    source: DragSource,
    anchor: Option<PaneId>,
) -> DropResolution {
    if !x.is_finite() || !y.is_finite() || !geom.content.valid() {
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
            DragSource::TabChip(..) | DragSource::PaneHeader(..) => DropPlan::MoveIntoPane {
                view: *view,
                tab_before: before,
            },
        };
        if extraction_doomed(geom, source, &plan) {
            return DropResolution::none();
        }
        return DropResolution {
            plan,
            preview: None,
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
            if touches_boundary(&hit.rect, &geom.content, direction) {
                DropResolution {
                    plan: DropPlan::SplitView {
                        view: hit.view,
                        direction,
                    },
                    preview: geom
                        .views
                        .iter()
                        .find(|v| v.view == hit.view)
                        .map(|v| v.rect),
                    anchor: Some(hit.pane),
                }
            } else {
                DropResolution {
                    plan: DropPlan::SplitPane {
                        pane: hit.pane,
                        direction,
                    },
                    preview: Some(hit.rect.half_adjacent(direction)),
                    anchor: Some(hit.pane),
                }
            }
        }
        // Center: move into the hovered pane's tab group — NO indicator
        // (verified truth, §3).
        None => DropResolution {
            plan: DropPlan::MoveIntoPane {
                view: hit.view,
                tab_before: None,
            },
            preview: None,
            anchor: Some(hit.pane),
        },
    }
}

/// Whether the plan would extract the dragged tab out of a view the engine
/// must refuse to empty — the last tab of the LAST view. Suppressing the
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
        DropPlan::ReorderStrip { .. } | DropPlan::None => false,
    }
}

/// Guarded self-hits: the drag source is already part of the hovered pane.
fn self_hit(source: DragSource, hit: &PaneRect) -> bool {
    match source {
        DragSource::PaneHeader(pane) => pane == hit.pane,
        DragSource::TabChip(tab, _) => tab == hit.tab,
    }
}

/// The pane owning the pointer: the anchored pane while within
/// [`FLIP_SMOOTH_PX`] of its rect (hysteresis), else exact containment, else
/// the nearest center among the smooth-expanded panes (the divider gap).
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
            .find(|p| p.pane == id && p.rect.valid() && p.rect.expanded(FLIP_SMOOTH_PX).contains(x, y))
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
            p.rect.valid() && p.rect.expanded(FLIP_SMOOTH_PX).contains(x, y)
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
/// [`STRIP_BAND_PX`]. Chips only — the strip's "+" and empty space resolve to
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

    // ---- center: move, no indicator ----

    #[test]
    fn pane_center_resolves_move_with_no_preview() {
        let geom = two_by_two();
        let r = resolve_drop(&geom, 250.0, 200.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::MoveIntoPane {
                view: V1,
                tab_before: None
            }
        );
        assert_eq!(r.preview, None, "verified truth: center shows NO indicator");
        assert_eq!(r.anchor, Some(PaneId(1)));
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
        assert_eq!(r.preview, Some(rect(250.0, 30.0, 250.0, 385.0)));
        // Down edge of p1 (bottom at 415) — interior too.
        let r = resolve_drop(&geom, 250.0, 412.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Down
            }
        );
        assert_eq!(r.preview, Some(rect(0.0, 222.5, 500.0, 192.5)));
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
    fn workspace_outer_edge_resolves_split_view_with_full_region_preview() {
        let geom = three_views();
        // V3's right edge is the workspace right boundary.
        let r = resolve_drop(&geom, 997.0, 400.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V3,
                direction: Direction::Right
            }
        );
        assert_eq!(r.preview, Some(rect(700.0, 0.0, 300.0, 800.0)));
        // V1's left edge is the workspace left boundary.
        let r = resolve_drop(&geom, 3.0, 400.0, DragSource::TabChip(T2, V2), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Left
            }
        );
        assert_eq!(r.preview, Some(rect(0.0, 0.0, 300.0, 800.0)));
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
        let r = resolve_drop(&geom, 797.0, 300.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
                direction: Direction::Right
            }
        );
        // Collapse the view to its last tab: extracting T1 is refused by the
        // engine (the last view can never empty) — the preview suppresses so
        // the drop no-ops honestly.
        geom.views[0].tab_count = 1;
        let r = resolve_drop(&geom, 797.0, 300.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(r.plan, DropPlan::None);
        // A header over the workspace-outer edge of ANOTHER pane (headers
        // only exist on ≥2-pane tabs, so the hovered pane differs from the
        // source): the pane re-docks as a new top-level region.
        let geom = three_views();
        let r = resolve_drop(&geom, 997.0, 400.0, DragSource::PaneHeader(PaneId(1)), None);
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
        // A square pane: at the top-left corner the left and up distances tie;
        // edge_zone's array order (left, right, up, down) picks LEFT.
        let geom = WorkspaceGeometry {
            content: rect(0.0, 0.0, 400.0, 400.0),
            panes: vec![pane(1, V1, T1, rect(0.0, 0.0, 100.0, 100.0))],
            views: vec![view(V1, rect(0.0, 0.0, 400.0, 400.0), 2)],
            strips: vec![],
        };
        let r = resolve_drop(&geom, 10.0, 10.0, DragSource::TabChip(T2, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::SplitView {
                view: V1,
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
        // keeps the preview — "require ~4px past the pane edge before a
        // flip" (and the clamped zone read holds the EDGE split, not a
        // center move).
        let chip = DragSource::TabChip(T3, V3);
        let r = resolve_drop(&geom, 302.0, 400.0, chip, Some(PaneId(1)));
        assert_eq!(
            r.plan,
            DropPlan::SplitPane {
                pane: PaneId(1),
                direction: Direction::Right
            }
        );
        // 6px past the edge: flipped to p2 (whose unexpanded rect owns it).
        let r = resolve_drop(&geom, 306.0, 400.0, chip, Some(PaneId(1)));
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
        // Past T2's center, still on the chip band → append.
        let r = resolve_drop(&geom, 240.0, 11.0, DragSource::TabChip(T1, V1), None);
        assert_eq!(
            r.plan,
            DropPlan::ReorderStrip {
                view: V1,
                before: None
            }
        );
        assert_eq!(r.preview, None);
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
        // grid lives in the active tab T1 — merging T1 beside its own pane
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
}

//! Recursive workspace renderer (WS2+WS3): the `SplitNode<ViewId>` tree becomes
//! nested flex column/row containers weighted by each split's ratio, every
//! split node's children are separated by a DRAGGABLE DIVIDER (WS3: live
//! ratio drag + double-click equalize), each view renders a tab strip plus its
//! active tab's pane tree, and each pane renders optional header + body.
//!
//! Hosting rules:
//! - every Chat-mode pane renders its OWN transcript (or an empty canvas
//!   area for an unbound pane) over its OWN composer footer - focus changes
//!   nothing in the element tree; each pane is flush on the same shell glass
//!   as the single-chat route, separated only by divider hairlines;
//! - click-to-focus controls keyboard/selection routing and brightens the
//!   header title without changing any size or dimming other panes;
//! - every pane renders its header - the pane's chat identity row;
//! - a sole top-level view hides its tab strip when it has a single tab;
//!   the strip survives for multi-tab views and any view whose close
//!   control it carries.
//!
//! Sizing: every split child gets `flex_basis(0)` + `flex_grow(weight)` with
//! weights from [`super::flex_weights`] (the markdown table trick), so nested
//! ratios compose multiplicatively and the engine's clamped ratios map 1:1 to
//! on-screen fractions. The seam between two children is a fixed
//! [`super::DIVIDER_SEAM_PX`] (1px) flex-none hairline; children share the
//! remaining span, which [`super::ratio_from_pointer`] accounts for (the
//! engine op lives in `shell/panes.rs`). An [`super::DIVIDER_HIT_PX`] (8px)
//! hit target overlaps both neighboring panes (3.5px each side) in an overlay
//! painted after both children so it wins hit-testing over the second pane.
//!
//! Paths: a divider's [`DividerTarget`] carries the `Vec<Branch>` path from
//! the tree root to ITS node - the first child recurses with `path+[First]`,
//! the second with `path+[Second]`, while the divider between them names
//! `path` itself (the node whose ratio it drags).
//!
//! This module is glue-free of `Shell` internals: the snapshot structs below
//! are built by `shell/panes.rs` (which can see Shell's private fields), and
//! listeners are created here through `cx.listener` on pub(crate) Shell
//! methods.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Bounds, Context, Empty, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement as _, Pixels, SharedString, StatefulInteractiveElement, Styled as _,
    canvas, div, px,
};
use zeron_workspace::{Branch, PaneId, PaneMode, SplitNode, TabId, ViewId};

use crate::composer::Composer;
use crate::shell::Shell;
use crate::theme::Theme;
use crate::transcript::Transcript;

use super::chrome::{self, TabChip};
use super::flex_weights;
use super::hit_test::PreviewKind;
use super::{DIVIDER_HIT_PX, DIVIDER_SEAM_PX, DividerDrag, DividerGhost, DividerTarget};

/// Signed offset of the [`DIVIDER_HIT_PX`] hit strip relative to its
/// [`DIVIDER_SEAM_PX`] anchor: `-3.5px`, so the 8px band straddles the 1px
/// hairline evenly (`3.5px` over the first pane, `1px` over the seam, `3.5px`
/// over the second pane).
const DIVIDER_HIT_OFFSET_PX: f32 = -((DIVIDER_HIT_PX - DIVIDER_SEAM_PX) / 2.0);
/// The workspace outlet's outer padding: zero - panes are flush with the
/// content region and only the dividers separate them. Shared with
/// `shell/panes.rs`'s drag geometry so the hit-test `content` region is
/// derived from the SAME constants the outlet lays out with - the preview
/// overlay, the pane/view rects and the boundary math must all reference one
/// coordinate space, not two.
pub(crate) const OUTLET_PAD_PX: f32 = 0.0;
pub(crate) const OUTLET_TOP_PAD_PX: f32 = OUTLET_PAD_PX;

/// The pane tree's inner gutter: zero - each view's active tab pane tree
/// fills its view region flush. Kept as the one shared layout constant the
/// drag geometry references ([`hit_test`] holds its own explicit drop
/// tolerances so shrinking this to zero cannot silently move them).
pub(crate) const PANE_TREE_PAD_PX: f32 = 0.0;

/// Immutable render-time snapshot of the workspace tree. Built per frame
/// (cheap: small trees, cloned ids/titles only).
pub(crate) struct WorkspaceSnap {
    pub root: SplitNode<ViewId>,
    pub views: Vec<ViewSnap>,
    /// Shared handle the pane containers' paint-time canvases record their
    /// painted bounds into (the tool-picker's anchor source; WS4 drag rects).
    pub pane_bounds: Rc<RefCell<std::collections::BTreeMap<PaneId, Bounds<Pixels>>>>,
    /// WS4: the top-level view regions (SplitView preview washes one).
    pub view_bounds: Rc<RefCell<std::collections::BTreeMap<ViewId, Bounds<Pixels>>>>,
    /// WS4: the tab chips' rects (strip drop/reorder targets).
    pub chip_bounds: Rc<RefCell<std::collections::BTreeMap<(ViewId, TabId), Bounds<Pixels>>>>,
    /// The focused pane's project Action control. Recursive rendering
    /// consumes it exactly once when it reaches that pane.
    pub action_control: Rc<RefCell<Option<AnyElement>>>,
    /// One take-once 14px project badge per bound pane, keyed by pane
    /// (`AnyElement` is not cloneable). A pane without a bound chat renders
    /// none; the pane container removes its own entry.
    pub project_badges: Rc<RefCell<std::collections::BTreeMap<PaneId, AnyElement>>>,
}

pub(crate) struct ViewSnap {
    pub view_id: ViewId,
    /// A view is directly closable only while another top-level view remains.
    pub closable: bool,
    /// The view's active tab (pane-level divider targets need it; only the
    /// active tab renders).
    pub active_tab_id: TabId,
    /// One chip per tab, in display order.
    pub chips: Vec<TabChip>,
    /// Extra left padding ahead of the strip's first chip: nonzero only for
    /// the window's top-left view while the titlebar cluster still overlays
    /// the content edge (sidebar collapsing). Rides the sidebar tween.
    pub leading_inset: f32,
    pub active_tab_root: SplitNode<PaneId>,
    pub panes: Vec<PaneSnap>,
}

pub(crate) struct PaneSnap {
    pub pane: PaneId,
    pub mode: PaneMode,
    pub title: SharedString,
    /// The pane's provider mark (provider drag ghost identity).
    pub mark: chrome::TabMark,
    /// The header's session metadata: mono context line + display state.
    pub meta: chrome::PaneMeta,
    /// Kept in the snapshot contract for the shell's pane bookkeeping; the
    /// header's changes-toggle gate also reads it (focused + bound only).
    pub has_session: bool,
    /// The one globally focused pane - routing and the changes-toggle gate;
    /// the header title reads it (bright vs muted) without any geometry or
    /// pane-body change.
    pub focused: bool,
    /// Extra left padding inside this pane's header: nonzero only for the
    /// window's top-left pane while the titlebar cluster overlays it.
    pub leading_inset: f32,
    /// The pane's own interactive transcript (`None` on the new-chat canvas
    /// or for a pane with no surface).
    pub transcript: Option<Entity<Transcript>>,
    /// The pane's own composer.
    pub composer: Option<Entity<Composer>>,
    /// The pane chat's subagent dock-strip data (chat id + the strip's
    /// visible summaries); `None` when the strip has nothing to show.
    pub agent_strip: Option<(String, Vec<crate::subagents::SubagentSummary>)>,
}

/// The content-area outlet for workspace mode: the whole view tree. Every
/// pane renders the entities it owns ([`PaneSnap::transcript`] /
/// [`PaneSnap::composer`]) - nothing is threaded through the recursion.
///
/// WS4: the outlet is the drag surface. `drag_preview` - the active drag's
/// resolved preview rect in outlet-relative coordinates - paints ABOVE the
/// tree (§3: half-pane for splits, full region for view splits and center
/// moves, a 2px insertion marker for strip drops). The offsets
/// feed an `.absolute()` child, which Taffy measures from this div's
/// border-box origin (its padding does NOT shift absolute children), and the
/// conversion subtracts exactly that origin - `DragMoveEvent::bounds`, this
/// div's paint-time hitbox. The root's `on_drag_move` receives every pointer
/// sample while a [`TabSplitDrag`] is live (GPUI capture dispatch, inside or
/// outside the outlet), `on_drop` commits on mouse-up inside, and the
/// mouse-up/out listeners clear the state when a drag ends without a commit
/// (drop over the sidebar etc.).
pub(crate) fn workspace_outlet(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    snap: &WorkspaceSnap,
    drag_preview: Option<(Bounds<Pixels>, PreviewKind)>,
    sidebar_drop_outlet: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .p(px(OUTLET_PAD_PX))
        .child(
            canvas(
                move |bounds, _, _| sidebar_drop_outlet.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        )
        .child(view_node(cx, theme, &snap.root, &[], snap))
        // Share the shell's backdrop with the single-chat route. Painting
        // theme.bg here made every split workspace substantially darker than
        // the same chat opened alone, especially on frosted macOS windows.
        // The live drop preview (§3): every resolved plan paints one, above
        // everything it covers.
        .children(drag_preview.map(|(b, kind)| split_drop_preview(b, kind, theme)))
        .on_drag_move(cx.listener(
            move |this, event: &gpui::DragMoveEvent<super::TabSplitDrag>, _, cx| {
                this.apply_split_drag_move(event, cx);
            },
        ))
        .on_drop(cx.listener(|this, payload: &super::TabSplitDrag, _, cx| {
            this.commit_split_drop(payload, cx);
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.cancel_split_drag(cx)),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.cancel_split_drag(cx)),
        )
        .into_any_element()
}

/// The one drop preview for every resolved plan - shared by the workspace
/// outlet and the legacy single-pane content area so drag feedback reads
/// identically on both surfaces. The [`PreviewKind`] the resolver produced
/// chooses the paint: a solid accent line for strip insertions, a half-pane
/// wash for pane splits, a fainter full-pane wash for tab joins and
/// existing-pane focuses, and a stronger half-region wash for view splits.
/// No listeners, no animation - direct manipulation stays
/// synchronized with the resolved drop plan.
pub(crate) fn split_drop_preview(
    bounds: Bounds<Pixels>,
    kind: PreviewKind,
    theme: &Theme,
) -> AnyElement {
    let el = div()
        .absolute()
        .left(bounds.origin.x)
        .top(bounds.origin.y)
        .w(bounds.size.width)
        .h(bounds.size.height);
    match kind {
        PreviewKind::Insertion => el.rounded(px(1.0)).bg(theme.accent).into_any_element(),
        // Square like the flush panes they describe (no island radius).
        PreviewKind::PaneHalf => el
            .border_1()
            .border_color(theme.accent)
            .bg(theme.accent.opacity(0.14))
            .into_any_element(),
        PreviewKind::FullTarget => el
            .border_1()
            .border_color(theme.accent)
            .bg(theme.accent.opacity(0.08))
            .into_any_element(),
        PreviewKind::ViewHalf => el
            .border_2()
            .border_color(theme.accent)
            .bg(theme.accent.opacity(0.14))
            .into_any_element(),
    }
}

/// Whether a view renders its tab strip: hidden on the common sole-view /
/// single-tab case (a lone chip is redundant chrome), preserved whenever a
/// second tab exists OR the strip must carry the view's × - the multi-view
/// strip owns the visible close-view control.
pub(crate) fn show_tab_strip(view: &ViewSnap) -> bool {
    view.chips.len() > 1 || view.closable
}

fn view_node(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    node: &SplitNode<ViewId>,
    path: &[Branch],
    snap: &WorkspaceSnap,
) -> AnyElement {
    match node {
        SplitNode::Split {
            horizontal,
            ratio,
            first,
            second,
        } => {
            let target = DividerTarget::View {
                path: path.to_vec(),
            };
            split_container(
                cx,
                theme,
                *horizontal,
                *ratio,
                target,
                view_node(cx, theme, first, &joined(path, Branch::First), snap),
                view_node(cx, theme, second, &joined(path, Branch::Second), snap),
            )
        }
        SplitNode::Leaf { content } => {
            let Some(view) = snap.views.iter().find(|v| v.view_id == *content) else {
                return Empty.into_any_element();
            };
            // View = optional tab strip + the ACTIVE tab's pane tree. Inactive
            // tabs keep their surfaces (each pane owns its entities - only the
            // ELEMENTS unmount). A paint-time canvas records the view region's
            // bounds (WS4: the SplitView preview washes this whole region).
            let view_bounds_cell = snap.view_bounds.clone();
            let view_id = view.view_id;
            let mut col = div()
                .relative()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    canvas(
                        move |bounds, _, _| {
                            view_bounds_cell.borrow_mut().insert(view_id, bounds);
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                .when(show_tab_strip(view), |el| {
                    el.child(chrome::tab_strip(
                        view.view_id,
                        &view.chips,
                        view.leading_inset,
                        theme,
                        &snap.chip_bounds,
                        cx,
                    ))
                });
            let pane_tree = pane_node(cx, theme, &view.active_tab_root, &[], view, snap);
            col = col.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .p(px(PANE_TREE_PAD_PX))
                    .child(pane_tree),
            );
            col.into_any_element()
        }
    }
}

/// `path + [branch]` - the child recursion's path.
fn joined(path: &[Branch], branch: Branch) -> Vec<Branch> {
    let mut next = path.to_vec();
    next.push(branch);
    next
}

/// A stable, target-derived element id for a divider (hover keys key off it).
fn divider_id_of(target: &DividerTarget) -> String {
    let (mut id, path) = match target {
        DividerTarget::View { path } => (String::from("ws-div-v-"), path.as_slice()),
        DividerTarget::Pane { view, tab, path } => {
            (format!("ws-div-p-{}-{}-", view.0, tab.0), path.as_slice())
        }
    };
    for branch in path {
        id.push(match branch {
            Branch::First => 'F',
            Branch::Second => 'S',
        });
    }
    id
}

fn pane_node(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    node: &SplitNode<PaneId>,
    path: &[Branch],
    view: &ViewSnap,
    snap: &WorkspaceSnap,
) -> AnyElement {
    match node {
        SplitNode::Split {
            horizontal,
            ratio,
            first,
            second,
        } => {
            let target = DividerTarget::Pane {
                view: view.view_id,
                tab: view.active_tab_id,
                path: path.to_vec(),
            };
            split_container(
                cx,
                theme,
                *horizontal,
                *ratio,
                target,
                pane_node(cx, theme, first, &joined(path, Branch::First), view, snap),
                pane_node(cx, theme, second, &joined(path, Branch::Second), view, snap),
            )
        }
        SplitNode::Leaf { content } => {
            let Some(pane) = view.panes.iter().find(|p| p.pane == *content) else {
                return Empty.into_any_element();
            };
            pane_container(cx, theme, pane, view.panes.len() > 1, snap)
        }
    }
}

/// A split's two children with a 1px hairline seam between them and an 8px
/// hit-target overlay straddling the seam. Weighted flex with zero basis maps
/// ratios directly to child sizes (`container_length - 1px`), while the
/// interactive hit strip is painted after both children so GPUI's reverse
/// paint-order hit test reaches the divider over both the first and second
/// panes. Dragging commits live ratio updates to THIS node only; double-click
/// equalizes it.
fn split_container(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    horizontal: bool,
    ratio: f64,
    target: DividerTarget,
    first: AnyElement,
    second: AnyElement,
) -> AnyElement {
    let (w_first, w_second) = flex_weights(ratio);
    let id = divider_id_of(&target);
    let hover_key = SharedString::from(format!("{id}-hover"));
    let container = if horizontal {
        div().relative().flex().flex_row()
    } else {
        div().relative().flex().flex_col()
    };
    // The divider consumes a clone for its drag payload; the container keeps
    // one for the ownership filter below.
    let owned = target.clone();
    container
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(split_child(w_first, first))
        .child(divider_seam(theme, &hover_key, horizontal))
        .child(split_child(w_second, second))
        .child(divider_hit_overlay(
            cx, &id, hover_key, horizontal, w_first, w_second, target,
        ))
        // Drag samples arrive here (the container owns the bounds the ratio
        // math needs - `DragMoveEvent::bounds`). Ancestor containers also
        // receive the event during capture; each filters by the drag
        // payload's target, so only the container that OWNS the divider
        // applies a ratio.
        .on_drag_move(cx.listener(
            move |this, event: &gpui::DragMoveEvent<DividerDrag>, _, cx| {
                if event.drag(cx).target != owned {
                    return;
                }
                this.apply_divider_drag_move(event, cx);
            },
        ))
        .into_any_element()
}

/// The 1px visual seam in flex flow between two split children: `theme.border`
/// at rest, blending to `theme.border_strong` while its hit strip is hovered
/// (or latched during a live divider drag).
fn divider_seam(theme: &Theme, hover_key: &SharedString, horizontal: bool) -> AnyElement {
    let color = crate::motion::hover_blend(hover_key, theme.border, theme.border_strong);
    div()
        .flex_none()
        .when(horizontal, |el| el.w(px(DIVIDER_SEAM_PX)).h_full())
        .when(!horizontal, |el| el.h(px(DIVIDER_SEAM_PX)).w_full())
        .bg(color)
        .into_any_element()
}

/// Transparent overlay painted after both split children so the [`DIVIDER_HIT_PX`]
/// hit strip sits above the second pane in GPUI's hit-test and event order.
/// The outer flex container and its two weighted spacers carry no interactivity
/// (`should_insert_hitbox` is false), so only the 8px `.occlude()` strip
/// registers a hitbox (~3.5px over each adjacent pane).
fn divider_hit_overlay(
    cx: &Context<'_, Shell>,
    id: &str,
    hover_key: SharedString,
    horizontal: bool,
    w_first: f32,
    w_second: f32,
    target: DividerTarget,
) -> AnyElement {
    let drag = DividerDrag {
        target: target.clone(),
        horizontal,
    };
    let hit_strip = div()
        .id(SharedString::from(id.to_owned()))
        .absolute()
        .occlude()
        .when(horizontal, |el| {
            el.left(px(DIVIDER_HIT_OFFSET_PX))
                .top_0()
                .bottom_0()
                .w(px(DIVIDER_HIT_PX))
                .cursor_col_resize()
        })
        .when(!horizontal, |el| {
            el.top(px(DIVIDER_HIT_OFFSET_PX))
                .left_0()
                .right_0()
                .h(px(DIVIDER_HIT_PX))
                .cursor_row_resize()
        })
        // Hover feedback pauses while any divider drag is live so the strip
        // never re-fades mid-drag (hover churn reads as flicker). The shell
        // method owns the drag latch (Shell fields stay private to the shell
        // module tree).
        .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
            this.note_divider_hover(&hover_key, *hovered);
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.begin_divider_drag(cx);
            }),
        )
        .on_drag(drag, |_, _point: gpui::Point<gpui::Pixels>, _, cx| {
            cx.stop_propagation();
            cx.new(|_| DividerGhost)
        })
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                if event.click_count == 2 {
                    this.equalize_divider(&target, cx);
                }
                this.end_divider_drag(cx);
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| this.end_divider_drag(cx)),
        );
    let anchor = div()
        .flex_none()
        .relative()
        .when(horizontal, |el| el.w(px(DIVIDER_SEAM_PX)).h_full())
        .when(!horizontal, |el| el.h(px(DIVIDER_SEAM_PX)).w_full())
        .child(hit_strip);
    let overlay = if horizontal {
        div().absolute().inset_0().flex().flex_row()
    } else {
        div().absolute().inset_0().flex().flex_col()
    };
    overlay
        .child(
            div()
                .flex_basis(px(0.0))
                .flex_grow(w_first)
                .min_w_0()
                .min_h_0(),
        )
        .child(anchor)
        .child(
            div()
                .flex_basis(px(0.0))
                .flex_grow(w_second)
                .min_w_0()
                .min_h_0(),
        )
        .into_any_element()
}

fn split_child(weight: f32, child: AnyElement) -> AnyElement {
    div()
        .flex_basis(px(0.0))
        .flex_grow(weight)
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(child)
        .into_any_element()
}

/// One pane: click-to-focus container, its header (the chat identity row -
/// `closable` only gates the ×), and the pane's own transcript + composer
/// body. Each leaf is flush on the same shell backdrop as a lone chat (no
/// radius or island border); the dividers carry the only hairlines. Avoid a
/// pane-local opaque fill that changes the appearance when a chat is split.
/// Focus brightens the header title only, so switching panes never shifts
/// their contents. A paint-time canvas records the pane's bounds for the
/// tool-picker anchor.
fn pane_container(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    pane: &PaneSnap,
    closable: bool,
    snap: &WorkspaceSnap,
) -> AnyElement {
    let pane_id = pane.pane;
    let bounds_cell = snap.pane_bounds.clone();
    let badge = snap.project_badges.borrow_mut().remove(&pane_id);
    let container = div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .relative()
        .flex()
        .flex_col()
        .overflow_hidden()
        // Click anywhere in the pane focuses it (§7); the listener no-ops
        // when the pane is already focused, so scrolling a transcript never
        // yanks keyboard focus. Dividers sit OUTSIDE every pane container,
        // so a divider press never lands here.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.pointer_focus_workspace_pane(pane_id, cx)),
        )
        // Paint-time bounds registry (the picker anchors at the focused
        // pane's top-left, §2; WS4's drag previews read the same map).
        .child(
            canvas(
                move |bounds, _, _| {
                    // No island border: the painted bounds ARE the pane's
                    // drag/drop geometry (flush against its siblings, with
                    // only the divider strip between them).
                    bounds_cell.borrow_mut().insert(pane_id, bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        );
    // Every pane renders its header - the chat identity row now that the
    // window-wide chat header is gone.
    container
        .child(chrome::pane_header(
            pane.pane,
            pane.title.clone(),
            pane.mark,
            &pane.meta,
            badge,
            closable,
            pane.focused,
            pane.focused && pane.has_session,
            pane.focused
                .then(|| snap.action_control.borrow_mut().take())
                .flatten(),
            true,
            pane.leading_inset,
            theme,
            cx,
        ))
        .child(pane_body(cx, theme, pane, snap))
        .into_any_element()
}

fn pane_body(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    pane: &PaneSnap,
    snap: &WorkspaceSnap,
) -> AnyElement {
    match pane.mode {
        PaneMode::Terminal => div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("Terminal panes are not yet available")),
            )
            .into_any_element(),
        PaneMode::Chat => {
            // The pane's own transcript (or, on the new-chat canvas, an empty
            // flexible area that keeps the composer footer pinned to the
            // pane's bottom edge under the header). The wrapper MUST clip:
            // the transcript's virtualized list lays out inside this box, and
            // any overflow would otherwise escape the pane (the tail painted
            // under the composer or past the pane's rounded bottom border).
            let transcript: AnyElement = match &pane.transcript {
                Some(transcript) => div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(transcript.clone())
                    .into_any_element(),
                None => div().flex_1().min_w_0().min_h_0().into_any_element(),
            };
            // The pane's own composer as its FLEX FOOTER - it consumes its
            // own height out of the pane's column so the transcript above
            // ends at the footer's top edge. A paint-time canvas feeds the
            // pane's actual width into the composer's responsive mode (each
            // composer measures against its own pane, not the dock column).
            let composer = pane.composer.clone().map(|composer| {
                // The subagent dock strip rides the same column width; its
                // pills/chat id arrive precomputed in the snapshot.
                let strip = pane.agent_strip.as_ref().map(|(chat_id, summaries)| {
                    let open: crate::subagents::OpenAgent = Rc::new(|this, chat, summary, cx| {
                        this.open_subagent_summary(chat, summary, cx)
                    });
                    let toggle: crate::subagents::TogglePanel =
                        Rc::new(|this, cx| this.toggle_agents_panel(cx));
                    let width = snap
                        .pane_bounds
                        .borrow()
                        .get(&pane.pane)
                        .map(|b| f32::from(b.size.width) - 20.0)
                        .unwrap_or(480.0);
                    crate::subagents::dock_strip(
                        chat_id,
                        summaries,
                        width,
                        cx.entity().read(cx).agents_panel_open(cx),
                        &cx.entity().read(cx).subagent_seen.borrow(),
                        open,
                        toggle,
                        chrono::Utc::now(),
                        theme,
                        cx.entity_id(),
                        cx,
                    )
                });
                div().relative().w_full().px(px(10.0)).pb(px(10.0)).child(
                    div()
                        .relative()
                        .w_full()
                        .max_w(px(crate::composer::COMPOSER_MAX_WIDTH))
                        .mx_auto()
                        .child(
                            canvas(
                                {
                                    let composer = composer.clone();
                                    move |bounds, _, cx| {
                                        composer.update(cx, |composer, cx| {
                                            composer.set_available_width(
                                                f32::from(bounds.size.width),
                                                cx,
                                            );
                                        });
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0(),
                        )
                        .children(strip)
                        .child(composer.clone()),
                )
            });
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(transcript)
                .children(composer)
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::chrome::tab_mark;

    fn view(chip_count: usize, closable: bool) -> ViewSnap {
        ViewSnap {
            view_id: ViewId(1),
            closable,
            active_tab_id: TabId(2),
            chips: (0..chip_count)
                .map(|i| TabChip {
                    tab_id: TabId(2 + i as u64),
                    label: SharedString::from("Tab"),
                    active: i == 0,
                    mark: tab_mark(PaneMode::Chat, None),
                })
                .collect(),
            active_tab_root: SplitNode::leaf(PaneId(3)),
            leading_inset: 0.0,
            panes: Vec::new(),
        }
    }

    #[test]
    fn tab_strip_hides_for_a_lone_chip_and_shows_for_tabs_or_close() {
        // Sole view with one chip: the strip is redundant chrome.
        assert!(!show_tab_strip(&view(1, false)));
        // A second tab needs the strip to switch.
        assert!(show_tab_strip(&view(2, false)));
        // A closable view keeps the strip - it carries the ×.
        assert!(show_tab_strip(&view(1, true)));
    }

    #[test]
    fn divider_hit_strip_straddles_the_one_pixel_seam_and_ids_stay_unique() {
        assert_eq!(DIVIDER_SEAM_PX, 1.0);
        assert_eq!(DIVIDER_HIT_PX, 8.0);
        assert_eq!(DIVIDER_HIT_OFFSET_PX, -3.5);
        assert_eq!(
            DIVIDER_HIT_PX + 2.0 * DIVIDER_HIT_OFFSET_PX,
            DIVIDER_SEAM_PX
        );

        let root_view = DividerTarget::View { path: vec![] };
        let root_pane_a = DividerTarget::Pane {
            view: ViewId(1),
            tab: TabId(2),
            path: vec![],
        };
        let root_pane_b = DividerTarget::Pane {
            view: ViewId(2),
            tab: TabId(3),
            path: vec![],
        };
        let nested_pane = DividerTarget::Pane {
            view: ViewId(1),
            tab: TabId(2),
            path: vec![Branch::First, Branch::Second],
        };
        assert_ne!(divider_id_of(&root_view), divider_id_of(&root_pane_a));
        assert_ne!(divider_id_of(&root_pane_a), divider_id_of(&root_pane_b));
        assert_eq!(divider_id_of(&nested_pane), "ws-div-p-1-2-FS");
    }
}

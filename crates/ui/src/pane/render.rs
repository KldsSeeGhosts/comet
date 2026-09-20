//! Recursive workspace renderer (WS2+WS3): the `SplitNode<ViewId>` tree becomes
//! nested flex column/row containers weighted by each split's ratio, every
//! split node's children are separated by a DRAGGABLE DIVIDER (WS3: live
//! ratio drag + double-click equalize), each view renders a tab strip plus its
//! active tab's pane tree, and each pane renders optional header + body.
//!
//! Hosting rules (see `super-analysis/13-interaction-truth.md`):
//! - exactly ONE pane is focused at a time; it renders the shell's LIVE
//!   transcript (passed in) and the LIVE composer (WS3 re-homing: the
//!   composer is overlaid at the focused pane's bottom instead of the shared
//!   outer dock), and carries the 1px `theme.accent` ring;
//! - unfocused panes carry a `hairline()` border and render the dormant
//!   composition (cached read-only transcript under an identity card, ghost
//!   composer strip at the bottom);
//! - a tab with ≥2 panes gives each pane a header; single-pane tabs have no
//!   header (§4/§6);
//! - the focused pane with no session renders nothing above the composer —
//!   the new-thread hero layer behind the tree IS its body.
//!
//! Sizing: every split child gets `flex_basis(0)` + `flex_grow(weight)` with
//! weights from [`super::flex_weights`] (the markdown table trick), so nested
//! ratios compose multiplicatively and the engine's clamped ratios map 1:1 to
//! on-screen fractions. The divider between two children is a fixed
//! [`super::DIVIDER_HIT_PX`] flex-none strip straddling the node line;
//! children share the remaining span, which [`super::ratio_from_pointer`]
//! accounts for (the engine op lives in `shell/panes.rs`).
//!
//! Paths: a divider's [`DividerTarget`] carries the `Vec<Branch>` path from
//! the tree root to ITS node — the first child recurses with `path+[First]`,
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
    canvas, div, px, AnyElement, AppContext as _, Bounds, Context, Empty, Entity,
    InteractiveElement, IntoElement, MouseButton, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement, Styled as _,
};
use zeron_workspace::{Branch, PaneId, PaneMode, SplitNode, TabId, ViewId};

use crate::shell::Shell;
use crate::theme::Theme;
use crate::transcript::Transcript;

use super::chrome::{self, TabChip};
use super::flex_weights;
use super::{DividerDrag, DividerGhost, DividerTarget, DIVIDER_HIT_PX};
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
    pub chip_bounds:
        Rc<RefCell<std::collections::BTreeMap<(ViewId, TabId), Bounds<Pixels>>>>,
}

pub(crate) struct ViewSnap {
    pub view_id: ViewId,
    /// The view's active tab (pane-level divider targets need it; only the
    /// active tab renders).
    pub active_tab_id: TabId,
    /// One chip per tab, in display order.
    pub chips: Vec<TabChip>,
    pub active_tab_root: SplitNode<PaneId>,
    pub panes: Vec<PaneSnap>,
}

pub(crate) struct PaneSnap {
    pub pane: PaneId,
    pub mode: PaneMode,
    pub title: SharedString,
    /// The pane's provider mark (header dot-side identity + drag ghost).
    pub mark: chrome::TabMark,
    pub has_session: bool,
    /// The one globally focused pane (ring + live transcript + composer).
    pub focused: bool,
    /// The pane's cached dormant transcript, if it has a session.
    pub dormant_transcript: Option<Entity<Transcript>>,
}

/// The content-area outlet for workspace mode: the whole view tree.
/// `composer_block` is the prebuilt live-composer strip (built by
/// `shell/panes.rs`, which can read the shell's dock state) overlaid inside
/// the FOCUSED chat pane — the WS3 composer re-homing. It is threaded down
/// the recursion as a single-use slot (AnyElement is not Clone): exactly one
/// leaf is focused, and that leaf takes the block.
///
/// WS4: the outlet is the drag surface. `drag_preview` — the active drag's
/// resolved preview rect in outlet-relative coordinates — paints ABOVE the
/// tree as an occluding accent wash + 1px ring (§3: half-pane for splits,
/// full top-level region for view splits, nothing at center). The root's
/// `on_drag_move` receives every pointer sample while a [`TabSplitDrag`] is
/// live (GPUI capture dispatch, inside or outside the outlet), `on_drop`
/// commits on mouse-up inside, and the mouse-up/out listeners clear the
/// state when a drag ends without a commit (drop over the sidebar etc.).
pub(crate) fn workspace_outlet(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    snap: &WorkspaceSnap,
    live_transcript: &Entity<Transcript>,
    composer_block: Option<AnyElement>,
    drag_preview: Option<Bounds<Pixels>>,
) -> AnyElement {
    let mut chrome_slot = composer_block;
    div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .p(px(3.0))
        .child(view_node(
            cx,
            theme,
            &snap.root,
            &[],
            snap,
            live_transcript,
            &mut chrome_slot,
        ))
        // The live drop preview: accent wash (~0.12 alpha) + 1px accent ring,
        // rounded ~8px (§3), painted above everything it covers.
        .children(drag_preview.map(|b| {
            div()
                .absolute()
                .left(b.origin.x)
                .top(b.origin.y)
                .w(b.size.width)
                .h(b.size.height)
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.accent)
                .bg(theme.accent.opacity(0.12))
        }))
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

fn view_node(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    node: &SplitNode<ViewId>,
    path: &[Branch],
    snap: &WorkspaceSnap,
    live: &Entity<Transcript>,
    chrome_slot: &mut Option<AnyElement>,
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
                &divider_id_of(path),
                view_node(
                    cx,
                    theme,
                    first,
                    &joined(path, Branch::First),
                    snap,
                    live,
                    chrome_slot,
                ),
                view_node(
                    cx,
                    theme,
                    second,
                    &joined(path, Branch::Second),
                    snap,
                    live,
                    chrome_slot,
                ),
            )
        }
        SplitNode::Leaf { content } => {
            let Some(view) = snap.views.iter().find(|v| v.view_id == *content) else {
                return Empty.into_any_element();
            };
            // View = tab strip + the ACTIVE tab's pane tree. Inactive tabs
            // unmount (their viewports restore from the per-chat cache when
            // they come back). A paint-time canvas records the view region's
            // bounds (WS4: the SplitView preview washes this whole region).
            let view_bounds_cell = snap.view_bounds.clone();
            let view_id = view.view_id;
            let mut col = div()
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
                .child(chrome::tab_strip(
                    view.view_id,
                    &view.chips,
                    theme,
                    &snap.chip_bounds,
                    cx,
                ));
            col = col.child(pane_node(
                cx,
                theme,
                &view.active_tab_root,
                &[],
                view,
                live,
                chrome_slot,
                snap,
            ));
            col.into_any_element()
        }
    }
}

/// `path + [branch]` — the child recursion's path.
fn joined(path: &[Branch], branch: Branch) -> Vec<Branch> {
    let mut next = path.to_vec();
    next.push(branch);
    next
}

/// A stable, path-derived element id for a divider (hover keys key off it).
fn divider_id_of(path: &[Branch]) -> String {
    let mut id = String::from("ws-div-");
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
    live: &Entity<Transcript>,
    chrome_slot: &mut Option<AnyElement>,
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
                &divider_id_of(path),
                pane_node(
                    cx,
                    theme,
                    first,
                    &joined(path, Branch::First),
                    view,
                    live,
                    chrome_slot,
                    snap,
                ),
                pane_node(
                    cx,
                    theme,
                    second,
                    &joined(path, Branch::Second),
                    view,
                    live,
                    chrome_slot,
                    snap,
                ),
            )
        }
        SplitNode::Leaf { content } => {
            let Some(pane) = view.panes.iter().find(|p| p.pane == *content) else {
                return Empty.into_any_element();
            };
            pane_container(cx, theme, pane, view.panes.len() > 1, live, chrome_slot, snap)
        }
    }
}

/// A split's two children with a divider between them: weighted flex with
/// zero basis so ratios map exactly to sizes regardless of content, and a
/// fixed-width invisible hit strip straddling the node line (§1: ~8px hit
/// area; the visual hairline stays a thin centered line). Dragging commits
/// live ratio updates to THIS node only; double-click equalizes it.
fn split_container(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    horizontal: bool,
    ratio: f64,
    target: DividerTarget,
    id: &str,
    first: AnyElement,
    second: AnyElement,
) -> AnyElement {
    let (w_first, w_second) = flex_weights(ratio);
    let container = if horizontal {
        div().flex().flex_row()
    } else {
        div().flex().flex_col()
    };
    // The divider consumes a clone for its drag payload; the container keeps
    // one for the ownership filter below.
    let owned = target.clone();
    container
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(split_child(w_first, first))
        .child(divider(cx, theme, id, horizontal, target))
        .child(split_child(w_second, second))
        // Drag samples arrive here (the container owns the bounds the ratio
        // math needs — `DragMoveEvent::bounds`). Ancestor containers also
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

/// The divider strip: an invisible [`DIVIDER_HIT_PX`] hit area straddling the
/// node line with a 1px hairline at its center that brightens on hover.
/// Starts a GPUI drag ([`DividerDrag`]) so the container's `on_drag_move`
/// receives pointer samples; mouse-up (or double-click) resolves here.
/// `.occlude()` keeps the click that starts a drag from reaching anything
/// under the strip — a divider press never focuses a pane.
fn divider(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    id: &str,
    horizontal: bool,
    target: DividerTarget,
) -> AnyElement {
    let hover_key = SharedString::from(format!("{}-hover", id));
    let line = div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .flex()
        .items_center()
        .justify_center()
        .child(if horizontal {
            // Side-by-side panes: a vertical 1px line.
            div()
                .w(px(1.0))
                .h_full()
                .bg(crate::motion::hover_blend(
                    &hover_key,
                    theme.hairline(0.10),
                    theme.border_strong,
                ))
                .into_any_element()
        } else {
            // Stacked panes: a horizontal 1px line.
            div()
                .h(px(1.0))
                .w_full()
                .bg(crate::motion::hover_blend(
                    &hover_key,
                    theme.hairline(0.10),
                    theme.border_strong,
                ))
                .into_any_element()
        });
    let drag = DividerDrag {
        target: target.clone(),
        horizontal,
    };
    div()
        .id(SharedString::from(format!("{}", id)))
        .flex_none()
        .relative()
        .occlude()
        .when(horizontal, |el| el.w(px(DIVIDER_HIT_PX)).cursor_col_resize())
        .when(!horizontal, |el| el.h(px(DIVIDER_HIT_PX)).cursor_row_resize())
        // Hover feedback pauses while any divider drag is live so the strip
        // never re-fades mid-drag (hover churn reads as flicker). The shell
        // method owns the drag latch (Shell fields stay private to the shell
        // module tree).
        .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
            this.note_divider_hover(&hover_key, *hovered);
        }))
        .child(line)
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

/// One pane: click-to-focus container, conditional header, content body,
/// and (focused chat only) the live composer block overlaid at the bottom. A
/// paint-time canvas records the pane's bounds for the tool-picker anchor.
fn pane_container(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    pane: &PaneSnap,
    multi_pane: bool,
    live: &Entity<Transcript>,
    chrome_slot: &mut Option<AnyElement>,
    snap: &WorkspaceSnap,
) -> AnyElement {
    // Focused: 1px accent ring; unfocused: plain hairline (§6).
    let border = if pane.focused {
        theme.accent
    } else {
        theme.hairline(0.10)
    };
    let pane_id = pane.pane;
    let bounds_cell = snap.pane_bounds.clone();
    let mut container = div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .relative()
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded(px(8.0))
        .border_1()
        .border_color(border)
        // Click anywhere in the pane focuses it (§7); the listener no-ops
        // when the pane is already focused, so scrolling the live transcript
        // never yanks keyboard focus. Dividers sit OUTSIDE every pane
        // container, so a divider press never lands here.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.focus_workspace_pane(pane_id, cx)),
        )
        // Paint-time bounds registry (the picker anchors at the focused
        // pane's top-left, §2; WS4's drag previews read the same map).
        .child(
            canvas(
                move |bounds, _, _| {
                    bounds_cell.borrow_mut().insert(pane_id, bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        );
    // Single-pane tabs render NO header — header chrome is conditional
    // (§4/§6); it first appears the moment a tab splits.
    if multi_pane {
        container = container.child(chrome::pane_header(
            pane.pane,
            pane.title.clone(),
            pane.mark,
            theme.text_muted.opacity(0.55),
            true,
            theme,
            cx,
        ));
    }
    container = container.child(pane_body(theme, pane, live));
    // WS3 composer re-homing: the LIVE composer is overlaid at the focused
    // chat pane's bottom (floating over the transcript's tail, like the old
    // outer dock floated over it). Unfocused panes keep the ghost strip in
    // `pane_body`.
    if pane.focused && pane.mode == PaneMode::Chat {
        // The single-use live-composer strip lands in exactly one pane — the
        // focused one (AnyElement is not Clone, hence the take).
        if let Some(block) = chrome_slot.take() {
            container = container.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(block),
            );
        }
    }
    container.into_any_element()
}

fn pane_body(theme: &Theme, pane: &PaneSnap, live: &Entity<Transcript>) -> AnyElement {
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
        PaneMode::Chat if pane.focused => {
            if pane.has_session {
                // The focused pane's live view: the shell's single transcript,
                // still bound to AppState::selected_chat exactly as before.
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(live.clone())
                    .into_any_element()
            } else {
                // New-thread pane: the hero layer behind the tree is the
                // body; the re-homed live composer at the pane's bottom
                // mints the chat on first send.
                Empty.into_any_element()
            }
        }
        PaneMode::Chat => {
            // Dormant: the cached read-only transcript (blank until per-chat
            // projections land in WS5) under an identity card, ghost composer
            // strip at the bottom. Clicks bubble to the pane container, which
            // swaps this whole composition for the live one.
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .children(
                            pane.dormant_transcript
                                .clone()
                                .map(|t| div().absolute().inset_0().child(t)),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .px(px(12.0))
                                        .text_center()
                                        .truncate()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(pane.title.clone()),
                                ),
                        ),
                )
                .child(chrome::ghost_composer(theme))
                .into_any_element()
        }
    }
}

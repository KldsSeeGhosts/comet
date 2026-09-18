//! Recursive workspace renderer (WS2+WS3): the `SplitNode<ViewId>` tree becomes
//! nested flex column/row containers weighted by each split's ratio, every
//! split node's children are separated by a DRAGGABLE DIVIDER (WS3: live
//! ratio drag + double-click equalize), each view renders a tab strip plus its
//! active tab's pane tree, and each pane renders optional header + body.
//!
//! Hosting rules:
//! - every Chat-mode pane renders its OWN transcript (or an empty canvas
//!   area for an unbound pane) over its OWN composer footer — focus changes
//!   nothing in the element tree; every pane carries the same
//!   `theme.border_strong` 1px border;
//! - pane focus is internal state only (keyboard/selection routing), still
//!   driven by click-to-focus on the pane container;
//! - a tab with ≥2 panes gives each pane a header; single-pane tabs have no
//!   header (§4/§6);
//! - a sole top-level view hides its tab strip when it has a single tab;
//!   the strip survives for multi-tab views and any view whose close
//!   control it carries.
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
use super::{DIVIDER_HIT_PX, DividerDrag, DividerGhost, DividerTarget};
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
    pub active_tab_root: SplitNode<PaneId>,
    pub panes: Vec<PaneSnap>,
}

pub(crate) struct PaneSnap {
    pub pane: PaneId,
    pub mode: PaneMode,
    pub title: SharedString,
    /// The pane's provider mark (header dot-side identity + drag ghost).
    pub mark: chrome::TabMark,
    /// Kept in the snapshot contract for the shell's pane bookkeeping; the
    /// renderer keys off `transcript`/`composer` presence instead.
    #[allow(dead_code)]
    pub has_session: bool,
    /// The one globally focused pane — internal routing state only; it no
    /// longer changes what the pane renders.
    #[allow(dead_code)]
    pub focused: bool,
    /// The pane's own interactive transcript (`None` on the new-chat canvas
    /// or for a pane with no surface).
    pub transcript: Option<Entity<Transcript>>,
    /// The pane's own composer.
    pub composer: Option<Entity<Composer>>,
}

/// The content-area outlet for workspace mode: the whole view tree. Every
/// pane renders the entities it owns ([`PaneSnap::transcript`] /
/// [`PaneSnap::composer`]) — nothing is threaded through the recursion.
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
    drag_preview: Option<Bounds<Pixels>>,
) -> AnyElement {
    div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .p(px(3.0))
        // The unified window titlebar is an overlay. Workspace chrome must
        // begin below it so tab chips and their close controls remain visible
        // and clickable instead of painting underneath the titlebar.
        .pt(px(Theme::TITLEBAR_HEIGHT + 3.0))
        .child(view_node(cx, theme, &snap.root, &[], snap))
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

/// Whether a view renders its tab strip: hidden on the common sole-view /
/// single-tab case (a lone chip is redundant chrome), preserved whenever a
/// second tab exists OR the strip must carry the view's × — the multi-view
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
                &divider_id_of(path),
                view_node(cx, theme, first, &joined(path, Branch::First), snap),
                view_node(cx, theme, second, &joined(path, Branch::Second), snap),
            )
        }
        SplitNode::Leaf { content } => {
            let Some(view) = snap.views.iter().find(|v| v.view_id == *content) else {
                return Empty.into_any_element();
            };
            // View = optional tab strip + the ACTIVE tab's pane tree. Inactive
            // tabs keep their surfaces (each pane owns its entities — only the
            // ELEMENTS unmount). A paint-time canvas records the view region's
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
                .when(show_tab_strip(view), |el| {
                    el.child(chrome::tab_strip(
                        view.view_id,
                        &view.chips,
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
                    .p(px(6.0))
                    .child(pane_tree),
            );
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
        .when(horizontal, |el| {
            el.w(px(DIVIDER_HIT_PX)).cursor_col_resize()
        })
        .when(!horizontal, |el| {
            el.h(px(DIVIDER_HIT_PX)).cursor_row_resize()
        })
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

/// One pane: click-to-focus container, conditional header, and the pane's
/// own transcript + composer body. Focus is internal routing state only —
/// every pane carries the same border and the same element tree. A
/// paint-time canvas records the pane's bounds for the tool-picker anchor.
fn pane_container(
    cx: &Context<'_, Shell>,
    theme: &Theme,
    pane: &PaneSnap,
    multi_pane: bool,
    snap: &WorkspaceSnap,
) -> AnyElement {
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
        .border_color(theme.border_strong)
        .bg(theme.bg)
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
    container.child(pane_body(theme, pane)).into_any_element()
}

fn pane_body(theme: &Theme, pane: &PaneSnap) -> AnyElement {
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
            // The pane's own composer as its FLEX FOOTER — it consumes its
            // own height out of the pane's column so the transcript above
            // ends at the footer's top edge. A paint-time canvas feeds the
            // pane's actual width into the composer's responsive mode (each
            // composer measures against its own pane, not the dock column).
            let composer = pane.composer.clone().map(|composer| {
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
            panes: Vec::new(),
        }
    }

    #[test]
    fn tab_strip_hides_for_a_lone_chip_and_shows_for_tabs_or_close() {
        // Sole view with one chip: the strip is redundant chrome.
        assert!(!show_tab_strip(&view(1, false)));
        // A second tab needs the strip to switch.
        assert!(show_tab_strip(&view(2, false)));
        // A closable view keeps the strip — it carries the ×.
        assert!(show_tab_strip(&view(1, true)));
    }
}

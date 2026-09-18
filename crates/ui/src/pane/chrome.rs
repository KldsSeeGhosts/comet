//! Pane chrome (WS2+WS3+WS4): the conditional pane header, the view tab
//! strip, and the ghost-composer strip for unfocused chat panes.
//!
//! Rules carried over from the live Super capture
//! (`super-analysis/13-interaction-truth.md`):
//! - a tab with ONE pane renders **no header** (§4/§6) — the header is
//!   conditional chrome for split tabs only;
//! - unfocused chat panes render a static composer-shaped strip reading
//!   "Click to focus chat" with a muted, NON-interactive pill row (§7);
//! - tab chips carry a provider mark + title; the ACTIVE chip gets a raised
//!   surface + accent underline (§8); a close × is revealed on chip hover and
//!   closes the tab (engine semantics: last tab of the only view = no-op);
//! - the strip ends in a "+" that opens the TOOL PICKER (§2) committed as an
//!   `add_tab` to that view;
//! - the pane header reveals its close × on hover (functional →
//!   `close_pane`); pop-out/maximize render as decorative chrome pending WS6;
//!   right-click opens the split/close context menu AND focuses the pane (§6).
//!
//! WS4 makes both drag SOURCES: a chip drags as [`DragSource::TabChip`], a
//! header as [`DragSource::PaneHeader`] (payload [`TabSplitDrag`], ghost
//! [`SplitDragGhost`]). Each chip also paints its bounds into the shared
//! registry — the strip's drop/reorder targets (`pane/hit_test.rs`). Drop
//! handling lives on the workspace outlet (`pane/render.rs` +
//! `shell/panes.rs`), NOT here: one commit path for the whole tree.
//!
//! Everything interactive builds Shell listeners through `cx.listener`; the
//! handlers live in `shell/panes.rs`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, AnyElement, AppContext as _, Bounds, Context, FontWeight, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement, Styled as _,
};
use zeron_workspace::{PaneId, PaneMode, TabId, ViewId};

use crate::icons::{self, icon};
use crate::motion;
use crate::shell::Shell;
use crate::theme::Theme;

use super::hit_test::DragSource;
use super::{SplitDragGhost, TabSplitDrag};

/// A tab chip's provider mark: the harness brand icon + optional tint
/// (Claude gets its brand orange; everything else renders in the muted text
/// tone). Default chat panes carry the app logo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TabMark {
    pub icon: &'static str,
    pub tint: Option<Hsla>,
}

/// The pure mark mapping for a pane (unit-tested): provider_key first
/// (engine `PaneState::provider_key`, Super tab-object parity), then mode.
pub(crate) fn tab_mark(mode: PaneMode, provider_key: Option<&str>) -> TabMark {
    match provider_key {
        Some("claude") => TabMark {
            icon: icons::CLAUDE_MARK,
            tint: Some(icons::claude_brand()),
        },
        Some("codex" | "openai") => TabMark {
            icon: icons::OPENAI_MARK,
            tint: None,
        },
        Some("devin") => TabMark {
            icon: icons::DEVIN_MARK,
            tint: None,
        },
        Some("pi") => TabMark {
            icon: icons::PI_MARK,
            tint: None,
        },
        Some("opencode") => TabMark {
            icon: icons::OPENCODE_MARK,
            tint: None,
        },
        Some("cursor") => TabMark {
            icon: icons::CURSOR_MARK,
            tint: None,
        },
        _ => match mode {
            PaneMode::Terminal => TabMark {
                icon: icons::TERMINAL,
                tint: None,
            },
            PaneMode::Chat => TabMark {
                icon: icons::ZERON_LOGO,
                tint: None,
            },
        },
    }
}

/// One tab-strip chip's snapshot (chrome builds elements from these; the
/// renderer owns the live tab data).
pub(crate) struct TabChip {
    pub tab_id: TabId,
    pub label: SharedString,
    pub active: bool,
    pub mark: TabMark,
}

/// The pane header: status dot + truncated title left; pop-out/maximize/close
/// right — revealed on hover (Super §6). The close is functional; pop-out and
/// maximize are decorative until WS6 wires their flows. Right-click opens the
/// split/close context menu (and focuses the pane, §6). Rendered ONLY when
/// the owning tab has ≥2 panes. WS4: the header is a drag source — dragging
/// it moves/re-docks the pane (drop targets resolve in `pane/hit_test.rs`).
pub(crate) fn pane_header(
    pane: PaneId,
    title: SharedString,
    mark: TabMark,
    dot: Hsla,
    closable: bool,
    theme: &Theme,
    cx: &Context<'_, Shell>,
) -> AnyElement {
    let header_key = format!("pane-header-hover-{}", pane.0);
    // Controls hide until the header hovers; the × additionally gets its own
    // hover wash (hitboxes nest, so hovering the × keeps the header "on").
    let reveal = || motion::hover_blend(&header_key, gpui::transparent_black(), theme.text_muted);
    let control = |key: String, child_icon: AnyElement| {
        div()
            .id(SharedString::from(key.clone()))
            .size(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .text_color(reveal())
            .bg(motion::hover_blend(&key, gpui::transparent_black(), theme.wash(0.12)))
            .on_hover(motion::hover_listener(key))
            .child(child_icon)
    };
    div()
        .id(SharedString::from(format!("pane-header-{}", pane.0)))
        .h(px(28.0))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .pl(px(10.0))
        .pr(px(6.0))
        .border_b_1()
        .border_color(theme.hairline(0.08))
        .on_hover(motion::hover_listener(header_key.clone()))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.open_workspace_pane_menu(pane, event.position, cx);
            }),
        )
        // WS4: header drag → re-dock/split/move resolution (§3's ghost chip
        // trails the cursor; the pane's title rides along).
        .on_drag(
            TabSplitDrag {
                source: DragSource::PaneHeader(pane),
                mark,
                title: title.clone(),
                session_id: None,
            },
            |payload, _point, _, cx| {
                cx.new(|_| SplitDragGhost {
                    mark: payload.mark,
                    title: payload.title.clone(),
                })
            },
        )
        .child(div().size(px(6.0)).flex_none().rounded_full().bg(dot))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(title),
        )
        // TODO(WS6): pop-out (detach this tab into its own view) — decorative
        // until the detach flow exists.
        .child(control(
            format!("pane-popout-{}", pane.0),
            icon(icons::EXPAND_ARROWS).size(px(11.0)).into_any_element(),
        ).opacity(0.35).cursor_default())
        // TODO(WS6): maximize (collapse the sibling panes of this split) —
        // decorative until the layout preset presets land.
        .child(control(
            format!("pane-maximize-{}", pane.0),
            icon(icons::WINDOW_MAXIMIZE).size(px(11.0)).into_any_element(),
        ).opacity(0.35).cursor_default())
        .when(closable, |el| {
            el.child(
                control(
                    format!("pane-close-{}", pane.0),
                    icon(icons::CLOSE).size(px(12.0)).into_any_element(),
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, event, window, cx| {
                    // The chip's own click must not double-fire through
                    // the pane's click-to-focus bubble path.
                    cx.stop_propagation();
                    this.close_workspace_pane(pane, cx);
                    window.prevent_default();
                    let _ = event;
                })),
            )
        })
        .into_any_element()
}

/// The view's tab strip (WS3): one chip per tab — provider mark, title, a
/// hover-revealed close ×, the active chip raised with an accent underline —
/// and a trailing "+" that opens the tool picker committed as `add_tab` to
/// this view. WS4: every chip is a drag source (ghost = mark + title) and
/// paints its bounds into `chip_bounds` — the strip's drop/reorder targets
/// for [`super::hit_test::resolve_drop`].
pub(crate) fn tab_strip(
    view: ViewId,
    tabs: &[TabChip],
    theme: &Theme,
    chip_bounds: &Rc<RefCell<BTreeMap<(ViewId, TabId), Bounds<Pixels>>>>,
    cx: &Context<'_, Shell>,
) -> AnyElement {
    let mut strip = div()
        .h(px(30.0))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .border_b_1()
        .border_color(theme.hairline(0.06));
    for chip in tabs {
        let tab = chip.tab_id;
        let chip_hover_key = format!("ws-tab-hover-{}", tab.0);
        let close_hover_key = format!("ws-tab-close-hover-{}", tab.0);
        let mark_tint = chip.mark.tint.unwrap_or(theme.text_muted.opacity(0.8));
        let bounds_cell = chip_bounds.clone();
        strip = strip.child(
            div()
                .id(SharedString::from(format!("ws-tab-{}-{}", view.0, tab.0)))
                .relative()
                .h(px(22.0))
                .px(px(9.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(5.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(11.0))
                .font_weight(if chip.active {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if chip.active {
                    theme.text
                } else {
                    theme.text_muted.opacity(0.75)
                })
                .bg(if chip.active {
                    theme.wash(0.09)
                } else {
                    motion::hover_blend(&chip_hover_key, gpui::transparent_black(), theme.wash(0.07))
                })
                .when(chip.active, |el| {
                    el.border_1().border_color(theme.hairline(0.09))
                })
                .on_hover(motion::hover_listener(chip_hover_key.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.switch_workspace_tab(view, tab, cx);
                }))
                // WS4: chip drag. Drop back on a strip = reorder/restore,
                // anywhere else the outlet's resolution decides (§3); the
                // ghost chip trails the cursor.
                .on_drag(
                    TabSplitDrag {
                        source: DragSource::TabChip(tab, view),
                        mark: chip.mark,
                        title: chip.label.clone(),
                        session_id: None,
                    },
                    |payload, _point, _, cx| {
                        cx.new(|_| SplitDragGhost {
                            mark: payload.mark,
                            title: payload.title.clone(),
                        })
                    },
                )
                // Paint-time chip registry (the strip drop/reorder targets).
                .child(
                    gpui::canvas(
                        move |bounds, _, _| {
                            bounds_cell.borrow_mut().insert((view, tab), bounds);
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                // Active chip: raised surface + accent underline (§8).
                .when(chip.active, |el| {
                    el.child(
                        div()
                            .absolute()
                            .bottom(px(-1.0))
                            .left(px(7.0))
                            .right(px(7.0))
                            .h(px(2.0))
                            .rounded(px(1.0))
                            .bg(theme.accent),
                    )
                })
                .child(icon(chip.mark.icon).size(px(11.0)).flex_none().text_color(mark_tint))
                .child(div().min_w_0().truncate().child(chip.label.clone()))
                // Close ×, revealed on chip hover; closes the tab (last tab
                // of the only view is an engine-guarded no-op).
                .child(
                    div()
                        .id(SharedString::from(format!("ws-tab-close-{}-{}", view.0, tab.0)))
                        .size(px(14.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        // Revealed while the CHIP is hovered (the chip's
                        // hover fade drives the tint); brightens on its own
                        // hover via the background wash.
                        .text_color(motion::hover_blend(
                            &chip_hover_key,
                            gpui::transparent_black(),
                            theme.text_muted,
                        ))
                        .bg(motion::hover_blend(
                            &close_hover_key,
                            gpui::transparent_black(),
                            theme.wash(0.14),
                        ))
                        .on_hover(motion::hover_listener(close_hover_key))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_workspace_tab(view, tab, cx);
                        }))
                        .child(icon(icons::CLOSE).size(px(9.0))),
                ),
        );
    }
    // Trailing "+" → the tool picker, committed as add_tab to THIS view
    // (§2: the same list serves as the tab-strip launcher). Anchored at the
    // trigger's press position.
    let plus_key = format!("ws-tab-add-{}", view.0);
    strip = strip.child(
        div()
            .id(SharedString::from(plus_key.clone()))
            .size(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .text_color(theme.text_muted.opacity(0.8))
            .bg(motion::hover_blend(&plus_key, gpui::transparent_black(), theme.wash(0.09)))
            .on_hover(motion::hover_listener(plus_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_workspace_tool_picker_for_tab(view, event.position, cx);
                }),
            )
            .child(icon(icons::PLUS).size(px(11.0))),
    );
    strip.into_any_element()
}

/// The ghost composer for an UNFOCUSED chat pane (interaction-truth §7): a
/// composer-shaped strip with muted "Click to focus chat" text and a
/// non-interactive pill row mirroring the live composer's footer layout.
/// Purely decorative — the pane container's click-to-focus handler owns the
/// pointer. Unused since every pane renders its own live composer; kept
/// pending the final workspace chrome cleanup.
#[allow(dead_code)]
pub(crate) fn ghost_composer(theme: &Theme) -> AnyElement {
    let pill = |label: &'static str| {
        div()
            .h(px(20.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(theme.hairline(0.08))
            .text_size(crate::typography::ui_rems(10.0))
            .text_color(theme.text_faint)
            .child(SharedString::from(label))
    };
    let round_btn = |label: &'static str| {
        div()
            .size(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(theme.hairline(0.08))
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text_faint)
            .child(SharedString::from(label))
    };
    div()
        .mx_auto()
        .mb(px(10.0))
        .w_full()
        .max_w(px(crate::composer::COMPOSER_MAX_WIDTH))
        .px(px(10.0))
        .flex_none()
        .child(
            div()
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.hairline(0.09))
                .bg(theme.input_bg.opacity(0.6))
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("Click to focus chat")),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(pill("Provider"))
                        .child(pill("Model"))
                        .child(div().flex_1())
                        .child(round_btn("+"))
                        .child(round_btn("↑")),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- WS3: tab-strip model helpers ----

    #[test]
    fn provider_keys_map_to_brand_marks() {
        assert_eq!(
            tab_mark(PaneMode::Chat, Some("claude")).icon,
            icons::CLAUDE_MARK
        );
        assert_eq!(
            tab_mark(PaneMode::Chat, Some("claude")).tint,
            Some(icons::claude_brand())
        );
        assert_eq!(tab_mark(PaneMode::Chat, Some("codex")).icon, icons::OPENAI_MARK);
        assert_eq!(tab_mark(PaneMode::Chat, Some("devin")).icon, icons::DEVIN_MARK);
        assert_eq!(tab_mark(PaneMode::Chat, Some("pi")).icon, icons::PI_MARK);
        assert_eq!(
            tab_mark(PaneMode::Chat, Some("opencode")).icon,
            icons::OPENCODE_MARK
        );
        assert_eq!(tab_mark(PaneMode::Chat, Some("cursor")).icon, icons::CURSOR_MARK);
    }

    #[test]
    fn default_panes_fall_back_to_mode_marks() {
        // No provider_key (every Zeron-minted pane today): chat panes carry
        // the app logo, terminal panes the terminal glyph.
        assert_eq!(tab_mark(PaneMode::Chat, None).icon, icons::ZERON_LOGO);
        assert_eq!(tab_mark(PaneMode::Terminal, None).icon, icons::TERMINAL);
        assert_eq!(tab_mark(PaneMode::Chat, None).tint, None);
        // Unknown provider strings fall through to the mode mark, never panic.
        assert_eq!(
            tab_mark(PaneMode::Chat, Some("holographic")).icon,
            icons::ZERON_LOGO
        );
    }

    #[test]
    fn tool_picker_rows_advertise_the_real_entry_points() {
        use crate::pane::TOOL_PICKER_ROWS;
        let kinds: Vec<_> = TOOL_PICKER_ROWS.iter().map(|row| row.kind).collect();
        assert_eq!(kinds, vec![crate::pane::ToolKind::Chat, crate::pane::ToolKind::Terminal]);
        assert!(TOOL_PICKER_ROWS.iter().all(|row| !row.label.is_empty()));
    }
}

//! Pane chrome (WS2+WS3+WS4): the conditional pane header, the view tab
//! strip, and the ghost-composer strip for unfocused chat panes.
//!
//! Rules carried over from the live Super capture
//! (`super-analysis/13-interaction-truth.md`):
//! - every pane renders a header — it is the pane's chat identity row
//!   (the window-wide chat header is gone);
//! - unfocused chat panes render a static composer-shaped strip reading
//!   "Click to focus chat" with a muted, NON-interactive pill row (§7);
//! - tab chips carry a provider mark + title; the ACTIVE chip gets a raised
//!   surface + accent underline (§8); a visible close ×
//!   closes the tab (engine semantics: last tab of the only view = no-op);
//! - the strip ends in a "+" that opens the TOOL PICKER (§2) committed as an
//!   `add_tab` to that view;
//! - pane header controls stay visible and labelled; close calls `close_pane`;
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
use crate::status_palette::SessionState;
use crate::theme::Theme;

use super::hit_test::DragSource;
use super::{SplitDragGhost, TabSplitDrag};

struct ChromeTooltip(&'static str);

impl gpui::Render for ChromeTooltip {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(11.0))
            .text_color(theme.text)
            .child(self.0)
    }
}

/// The pane header's session metadata: the mono context line and the bound
/// session's display state. Replaces the old buddy avatar (rule 5: no
/// mascots in chrome); an unbound pane (the new-chat canvas, a terminal
/// pane) carries no context and reads idle.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PaneMeta {
    /// Mono 11px `text_faint`: `{project}:{branch}` or `{project}`, plus
    /// ` · {device}` when the session runs on a remote device. `None` when
    /// the pane has no session.
    pub context: Option<SharedString>,
    /// The bound session's display state (send truth included); `Idle` for
    /// an unbound or settled pane - the status label only renders for
    /// non-idle states.
    pub state: SessionState,
}

impl PaneMeta {
    /// A pane with no bound session carries no metadata.
    pub(crate) fn empty() -> Self {
        Self {
            context: None,
            state: SessionState::Idle,
        }
    }
}

/// The pane header's status cell for a live session: the state glyph plus
/// its label, both in the state color (rule 1; Queued stays neutral).
/// `None` for idle/settled sessions, so their headers stay quiet.
fn status_label(state: SessionState, theme: &Theme) -> Option<AnyElement> {
    let label = state.label()?;
    let tone = state.color(theme).unwrap_or(theme.text_muted);
    let glyph: AnyElement = match state {
        // The sidebar's equalizer needs an entity context this renderer does
        // not own; the 6px dot keeps Working chromatic without it.
        SessionState::Working => div()
            .size(px(6.0))
            .flex_none()
            .rounded_full()
            .bg(tone)
            .into_any_element(),
        SessionState::AwaitingInput => icon(icons::CHAT_ROUND_LINE)
            .size(px(12.0))
            .text_color(tone)
            .into_any_element(),
        SessionState::Failed => icon(icons::DANGER_TRIANGLE)
            .size(px(12.0))
            .text_color(tone)
            .into_any_element(),
        SessionState::Completed => icon(icons::CHECK)
            .size(px(12.0))
            .text_color(tone)
            .into_any_element(),
        SessionState::Queued => icon(icons::CLOCK_CIRCLE)
            .size(px(12.0))
            .text_color(tone)
            .into_any_element(),
        SessionState::Idle => return None,
    };
    Some(
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .text_size(crate::typography::ui_rems(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(tone)
            .child(glyph)
            .child(SharedString::from(label))
            .into_any_element(),
    )
}

/// A tab chip's provider mark: the harness brand icon + optional tint
/// (Claude gets its brand orange; everything else renders in the muted text
/// tone). `None` renders no glyph: the ZERON_LOGO fallback is illegible at
/// chip/header sizes, so an unresolvable identity keeps the column empty
/// instead of wearing a dotted smudge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TabMark {
    pub icon: Option<&'static str>,
    pub tint: Option<Hsla>,
}

/// The pure mark mapping for a pane (unit-tested): provider_key first
/// (engine `PaneState::provider_key`, Super tab-object parity), then mode.
pub(crate) fn tab_mark(mode: PaneMode, provider_key: Option<&str>) -> TabMark {
    let mark = |icon, tint| TabMark {
        icon: Some(icon),
        tint,
    };
    match provider_key {
        Some("claude") => mark(icons::CLAUDE_MARK, Some(icons::claude_brand())),
        Some("codex" | "openai") => mark(icons::OPENAI_MARK, None),
        Some("devin") => mark(icons::DEVIN_MARK, None),
        Some("pi") => mark(icons::PI_MARK, None),
        Some("opencode") => mark(icons::OPENCODE_MARK, None),
        Some("cursor") => mark(icons::CURSOR_MARK, None),
        _ => match mode {
            PaneMode::Terminal => mark(icons::TERMINAL, None),
            // Unbound chat panes resolve their real identity from the
            // composer's harness pick at render time (shell/panes.rs); the
            // map alone cannot name one, so it yields the no-glyph mark.
            PaneMode::Chat => TabMark {
                icon: None,
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

/// The pane header's fixed height — also the chat identity row on the
/// legacy single-pane route and the transcript's top fade inset.
pub(crate) const PANE_HEADER_HEIGHT: f32 = 36.0;

/// The pane header: leading harness mark, truncated title, project badge,
/// mono context, status label, then the labelled controls right.
/// Right-click opens the split/close menu. Draggable headers can be
/// re-docked. `leading_inset` pads the leading edge (top-left pane only)
/// past the titlebar cluster while the sidebar collapses; 0 elsewhere.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_header(
    pane: PaneId,
    title: SharedString,
    mark: TabMark,
    meta: &PaneMeta,
    badge: Option<AnyElement>,
    closable: bool,
    focused: bool,
    show_changes: bool,
    action_control: Option<AnyElement>,
    draggable: bool,
    leading_inset: f32,
    theme: &Theme,
    cx: &Context<'_, Shell>,
) -> AnyElement {
    let control = |key: String, path: &'static str, label: &'static str| {
        div()
            .id(SharedString::from(key.clone()))
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .role(gpui::Role::Button)
            .aria_label(label)
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
            })
            .tooltip(move |_, cx| cx.new(|_| ChromeTooltip(label)).into())
            .bg(motion::hover_blend(&key, gpui::transparent_black(), theme.wash(0.12)))
            .on_hover(motion::hover_listener(key))
            // GPUI SVGs require their own color; a parent div's tint is ignored.
            .child(icon(path).size(px(14.0)).text_color(theme.text_muted))
    };
    div()
        .id(SharedString::from(format!("pane-header-{}", pane.0)))
        .h(px(PANE_HEADER_HEIGHT))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .pl(px(10.0 + leading_inset))
        .pr(px(6.0))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.open_workspace_pane_menu(pane, event.position, cx);
            }),
        )
        // WS4: header drag → re-dock/split/move resolution (§3's ghost chip
        // trails the cursor; the pane's title rides along). Non-draggable
        // headers (the legacy single-pane identity row) skip the source.
        .when(draggable, |el| {
            el.on_drag(
                TabSplitDrag {
                    source: DragSource::PaneHeader(pane),
                    mark,
                    title: title.clone(),
                    session_id: None,
                },
                |payload, point, _, cx| {
                    cx.new(|_| SplitDragGhost {
                        mark: payload.mark,
                        title: payload.title.clone(),
                        cursor_offset: point,
                    })
                },
            )
        })
        // Leading 16px harness column: the brand mark identifies the agent at
        // 14px in its brand tint (rule 1: Claude orange, the rest monochrome
        // by design); `text_muted` is the fallback for untinted marks. The
        // column keeps its width with no mark so text aligns.
        .child(
            div()
                .w(px(16.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .children(mark.icon.map(|path| {
                    icon(path)
                        .size(px(14.0))
                        .text_color(mark.tint.unwrap_or(theme.text_muted))
                        .into_any_element()
                })),
        )
        // Title: UI face, 12.5px MEDIUM (rule 4: MEDIUM is the pane title's
        // weight). It truncates and may shrink; the context below gives up
        // space first.
        .child(
            div()
                .flex_initial()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(12.5))
                .font_weight(FontWeight::MEDIUM)
                // Focus cue (rule 3): the focused title is full-strength text;
                // unfocused panes rest muted. No border, no size change.
                .text_color(if focused { theme.text } else { theme.text_muted })
                .child(title),
        )
        // Project identity (rule 1): the bound chat's 14px badge, right after
        // the title. Unbound panes carry none.
        .when_some(badge, |el, badge| el.child(badge))
        // Context: mono 11px text_faint, `{project}:{branch}` (plus the remote
        // device); it owns the remaining row and truncates first. With no
        // session the spacer keeps the controls right-aligned.
        .when_some(meta.context.clone(), |el, context| {
            el.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_faint)
                    .child(context),
            )
        })
        .when(meta.context.is_none(), |el| {
            el.child(div().flex_1().min_w_0())
        })
        // Status label (rule 1): the sidebar slot's vocabulary at the pane
        // header's scale - icon plus label in the state color, hidden when
        // idle so settled panes keep the context line as their last word.
        .when_some(status_label(meta.state, theme), |el, label| el.child(label))
        .when_some(action_control, |el, action| el.child(action))
        // The right-pane toggle lives on the pane header — the window-wide
        // chat header that used to carry it is gone. Shown only on the
        // focused session-bound pane so idle panes stay quiet.
        .when(show_changes, |el| {
            el.child(
                control(
                    format!("pane-changes-{}", pane.0),
                    icons::SIDEBAR_MINIMALISTIC,
                    "Toggle right sidebar",
                )
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_right_pane(cx);
                })),
            )
        })
        .when(closable, |el| {
            el.child(
                control(
                    format!("pane-close-{}", pane.0),
                    icons::CLOSE,
                    "Close pane",
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
/// for [`super::hit_test::resolve_drop`]. `leading_inset` pads before the
/// first chip so the top-left view's strip clears the titlebar cluster.
pub(crate) fn tab_strip(
    view: ViewId,
    tabs: &[TabChip],
    leading_inset: f32,
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
        .pl(px(8.0 + leading_inset))
        .pr(px(8.0))
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
                    |payload, point, _, cx| {
                        cx.new(|_| SplitDragGhost {
                            mark: payload.mark,
                            title: payload.title.clone(),
                            cursor_offset: point,
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
                .children(chip.mark.icon.map(|path| {
                    icon(path)
                        .size(px(11.0))
                        .flex_none()
                        .text_color(mark_tint)
                        .into_any_element()
                }))
                .child(div().min_w_0().truncate().child(chip.label.clone()))
                // Close × closes the tab (last tab
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
                        .role(gpui::Role::Button)
                        .aria_label("Close tab")
                        .tooltip(|_, cx| cx.new(|_| ChromeTooltip("Close tab")).into())
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
                        .child(icon(icons::CLOSE).size(px(11.0)).text_color(theme.text_muted)),
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
            .role(gpui::Role::Button)
            .aria_label("Add tab")
            .tooltip(|_, cx| cx.new(|_| ChromeTooltip("Add tab")).into())
            .bg(motion::hover_blend(&plus_key, gpui::transparent_black(), theme.wash(0.09)))
            .on_hover(motion::hover_listener(plus_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_workspace_tool_picker_for_tab(view, event.position, cx);
                }),
            )
            .child(icon(icons::PLUS).size(px(14.0)).text_color(theme.text_muted)),
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
            Some(icons::CLAUDE_MARK)
        );
        assert_eq!(
            tab_mark(PaneMode::Chat, Some("claude")).tint,
            Some(icons::claude_brand())
        );
        assert_eq!(tab_mark(PaneMode::Chat, Some("codex")).icon, Some(icons::OPENAI_MARK));
        assert_eq!(tab_mark(PaneMode::Chat, Some("devin")).icon, Some(icons::DEVIN_MARK));
        assert_eq!(tab_mark(PaneMode::Chat, Some("pi")).icon, Some(icons::PI_MARK));
        assert_eq!(
            tab_mark(PaneMode::Chat, Some("opencode")).icon,
            Some(icons::OPENCODE_MARK)
        );
        assert_eq!(tab_mark(PaneMode::Chat, Some("cursor")).icon, Some(icons::CURSOR_MARK));
    }

    #[test]
    fn default_panes_fall_back_to_mode_marks() {
        // No provider_key: the chat mark is None (the ZERON_LOGO fallback
        // was an illegible smudge; the composer's harness pick supplies the
        // real identity at render time). Terminal panes keep their glyph.
        assert_eq!(tab_mark(PaneMode::Chat, None).icon, None);
        assert_eq!(tab_mark(PaneMode::Terminal, None).icon, Some(icons::TERMINAL));
        assert_eq!(tab_mark(PaneMode::Chat, None).tint, None);
        // Unknown provider strings fall through to the mode mark, never panic.
        assert_eq!(tab_mark(PaneMode::Chat, Some("holographic")).icon, None);
    }

    #[test]
    fn tool_picker_rows_advertise_the_real_entry_points() {
        use crate::pane::TOOL_PICKER_ROWS;
        let kinds: Vec<_> = TOOL_PICKER_ROWS.iter().map(|row| row.kind).collect();
        assert_eq!(kinds, vec![crate::pane::ToolKind::Chat, crate::pane::ToolKind::Terminal]);
        assert!(TOOL_PICKER_ROWS.iter().all(|row| !row.label.is_empty()));
    }
}

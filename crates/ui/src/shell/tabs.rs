//! Session navigation — the horizontal tab strip is gone (wing 2026-08-10):
//! the activity sidebar IS the session list, and the titlebar names the
//! selected session (harness brand icon + title). A `+` new-session button
//! lives in the titlebar's left control cluster while an existing session is
//! selected. `UiSettings.open_tabs` is legacy — no longer read or written.

use super::*;

/// The chat one step from `selected` in the sidebar `order`, wrapping at both
/// ends. Pure.
///
/// With nothing selected — the new-session canvas — cycling enters the list at
/// the end it would have wrapped to: the first row going forward, the last
/// going back. A selection that has since left the list (archived from another
/// device mid-cycle) is treated the same way rather than dead-ending.
pub(super) fn cycle_target(
    order: &[String],
    selected: Option<&str>,
    forward: bool,
) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let at = selected.and_then(|id| order.iter().position(|c| c == id));
    let next = match (at, forward) {
        (Some(at), true) => (at + 1) % order.len(),
        (Some(at), false) => (at + order.len() - 1) % order.len(),
        (None, true) => 0,
        (None, false) => order.len() - 1,
    };
    Some(order[next].clone())
}

pub(super) fn right_pane_expand_icon(expanded: bool) -> &'static str {
    if expanded {
        icons::COLLAPSE_ARROWS
    } else {
        icons::EXPAND_ARROWS
    }
}

/// Wall-clock span of the todo check-off: check fade-in + the strikethrough
/// bar's left→right wipe. Rows repaint each frame until elapsed passes this.
const TODO_CHECK_OFF: std::time::Duration = std::time::Duration::from_millis(260);

fn todo_card_layer(theme: &Theme, header: AnyElement, body: AnyElement) -> AnyElement {
    let card = popover::popover_card(theme)
        .w(px(300.0))
        .flex()
        .flex_col()
        .child(header)
        .child(body);

    // Do not occlude this surface. GPUI's BlockMouse hitbox stops wheel-event
    // routing at the card chrome, so the transcript cannot scroll while the
    // pointer is over the floating card.
    div()
        .absolute()
        .top(px(Theme::TITLEBAR_HEIGHT + 10.0))
        .right(px(14.0))
        .child(motion::menu_in("todo-card-in", card))
        .into_any_element()
}

impl Shell {
    /// The ZCode-style "Progress" card — a floating details panel pinned to
    /// the top-right of the conversation column. Auto-mounts as soon as the
    /// selected chat has a `ToolCall::Todo` in its transcript (todo updates
    /// re-emit the whole list, so the latest call carries current state) and
    /// stays until the chat loses its todos or is deselected. The card lives
    /// INSIDE the chat column, so the sidebar can never slide under it.
    ///
    /// Header: `Progress` + `done/total` + a collapse chevron (collapse is
    /// per-chat, remembered for the app run). Rows: the in-flight item (first
    /// not-done) is expanded, pending items render a hollow circle, done
    /// items render the animated check + strikethrough.
    pub(super) fn render_todo_card(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let items: Vec<zeron_proto::TodoItem> = self.state.read(cx).latest_todos()?.to_vec();
        if items.is_empty() {
            return None;
        }
        let done = items.iter().filter(|i| i.done).count();
        let total = items.len();
        let chat_id = self.active_chat.clone();
        let collapsed = self.todo_collapsed.get(&chat_id).copied().unwrap_or(false);
        // The first not-done item is "in flight" — expanded in collapsed mode,
        // highlighted in expanded mode. All-done leaves no current row.
        let current_ix = items.iter().position(|i| !i.done);
        // Diff against last frame: items that just flipped done get a
        // completion timestamp; rows animate check-in + strikethrough wipe off
        // the elapsed wall time (the manual-tween pattern — `with_animation`'s
        // id-keyed clock would replay on every remount).
        let now = std::time::Instant::now();
        let prev = self.todo_prev.get(&chat_id);
        let shape_changed = prev.is_none_or(|p| p.len() != items.len());
        for (ix, item) in items.iter().enumerate() {
            let was_done = prev.and_then(|p| p.get(ix)).is_some_and(|p| p.done);
            if item.done && !was_done && !shape_changed {
                self.todo_done_at.insert((chat_id.clone(), ix), now);
            }
            if !item.done {
                self.todo_done_at.remove(&(chat_id.clone(), ix));
            }
        }
        self.todo_prev.insert(chat_id.clone(), items.clone());
        // Any completion stamp younger than the animation still needs frames —
        // flag motion_active so the root render keeps requesting animation
        // frames (the same pump the width tweens ride; `with_animation`'s
        // id-keyed clock would replay on every remount instead).
        let animating = self
            .todo_done_at
            .iter()
            .any(|((chat, _), at)| *chat == chat_id && now.duration_since(*at) < TODO_CHECK_OFF);
        if animating {
            self.motion_active.set(true);
        }

        let header = div()
            .id("todo-card-header")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .pt(px(8.0))
            .pb(px(6.0))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                let chat = this.active_chat.clone();
                let next = !this.todo_collapsed.get(&chat).copied().unwrap_or(false);
                this.todo_collapsed.insert(chat, next);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        icon(icons::CHECKLIST)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Progress")),
                    )
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(if done == total {
                                theme.success
                            } else {
                                theme.text_muted.opacity(0.7)
                            })
                            .child(SharedString::from(format!("{done}/{total}"))),
                    ),
            )
            .child(
                icon(if collapsed {
                    icons::ALT_ARROW_DOWN
                } else {
                    icons::ALT_ARROW_UP
                })
                .size(px(14.0))
                .text_color(theme.text_muted),
            );

        let row = |ix: usize, item: &zeron_proto::TodoItem| -> AnyElement {
            let is_current = current_ix == Some(ix);
            // Check-off animation progress: None = no animation this frame
            // (steady-state done renders the static check + struck text).
            let check_t = item
                .done
                .then(|| {
                    self.todo_done_at.get(&(chat_id.clone(), ix)).map(|at| {
                        (now.duration_since(*at).as_secs_f32() / TODO_CHECK_OFF.as_secs_f32())
                            .clamp(0.0, 1.0)
                    })
                })
                .flatten();
            let mark = if item.done {
                icon(icons::CHECK)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.success)
                    .opacity(check_t.unwrap_or(1.0))
                    .into_any_element()
            } else {
                // Hollow ring for pending; the in-flight row reads brighter.
                icon(icons::CLOCK_CIRCLE)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(if is_current {
                        theme.text
                    } else {
                        theme.text_faint
                    })
                    .into_any_element()
            };
            let label = div()
                .min_w_0()
                .flex_1()
                .relative()
                .truncate()
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(if item.done {
                    theme.text_muted
                } else {
                    theme.text
                })
                // The wipe IS the strike line: during the check-off a 1px
                // muted bar grows left→right over the text; once it lands,
                // `line_through` takes over as the steady-state decoration.
                .when(item.done && check_t.is_none_or(|t| t >= 1.0), |el| {
                    el.line_through()
                })
                .child(SharedString::from(item.text.clone()))
                .when_some(check_t.filter(|t| *t < 1.0), |el, t| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(8.0))
                            .left_0()
                            .h(px(1.0))
                            .w(px(300.0 * motion::EASE_OUT.eval(t)))
                            .bg(theme.text_muted),
                    )
                })
                .into_any_element();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(4.0))
                .child(mark)
                .child(label)
                .into_any_element()
        };

        let body: AnyElement = if collapsed {
            // ZCode collapsed state: just the in-flight row (or the last row
            // when everything is done), so the card shrinks to one line.
            let show = current_ix.unwrap_or(total - 1);
            div()
                .flex()
                .flex_col()
                .pb(px(6.0))
                .child(row(show, &items[show]))
                .into_any_element()
        } else {
            div()
                .id("todo-card-list")
                .max_h(px(300.0))
                .overflow_y_scroll()
                .pt(px(2.0))
                .pb(px(6.0))
                .flex()
                .flex_col()
                .children(items.iter().enumerate().map(|(ix, it)| row(ix, it)))
                .into_any_element()
        };

        Some(todo_card_layer(&theme, header.into_any_element(), body))
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab: step through the sidebar's Sessions list in
    /// the order it is drawn. Selection is immediate (no MRU overlay held open
    /// on the modifier) — one press, one session.
    ///
    /// Chat-scoped chrome, like the panel toggles: gpui dispatches a matched
    /// binding before any `on_key_down`, so an unscoped cycle would fire
    /// underneath Settings (yanking the user off the page mid-record, since
    /// these are the very keys the shortcuts table invites them to press) or
    /// underneath the add-space palette, stranding the overlay over a session
    /// they never picked.
    pub(super) fn cycle_session(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        // The same list `render_active_rows` draws and the jump shortcuts
        // count — one function, so neither can drift from the screen.
        let order = self.sidebar_visible_order(cx);
        let selected = self.state.read(cx).selected_chat.clone();
        if let Some(target) = cycle_target(&order, selected.as_deref(), forward) {
            self.open_chat(target, cx);
        }
    }

    /// Boot landing: the most recently active visible chat once the first
    /// chats frame has synced (manual selection wins; no chats → the
    /// new-session canvas shows).
    pub(super) fn boot_select_chat(&mut self, cx: &mut Context<Self>) {
        let first = {
            let state = self.state.read(cx);
            if !state.chats_synced || state.selected_chat.is_some() || state.auto_selected {
                return;
            }
            state
                .overview_chats(Utc::now())
                .first()
                .map(|(_, c)| c.id.clone())
        };
        if let Some(first) = first {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(first), cx));
        }
    }

    /// Open a session from the sidebar: select it, the main area follows.
    pub(super) fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state
            .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        cx.notify();
    }

    /// bb's Cmd/Ctrl-click and "Open in split": the session gets its own pane
    /// beside the active one instead of retargeting it — or focuses the pane
    /// already showing it.
    pub(super) fn open_chat_in_split(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        if let Some(workspace) = self.workspace.clone() {
            workspace.update(cx, |workspace, cx| workspace.open_session_in_split(&chat_id, cx));
        } else {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        }
        cx.notify();
    }

    /// `+` in the titlebar: open the new-session canvas and the native folder
    /// browser over it, so a session starts with a directory pick. Choosing a
    /// folder mints (or reuses) its Space and lands the draft on it via
    /// `land_in_space`; escaping leaves the blank draft up to type into.
    pub(super) fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.open_new_session_draft(cx);
        self.open_add_space(cx);
    }

    /// The plain new-session canvas without the folder prompt — reached from
    /// the sidebar's session rows and any flow that already knows its target.
    /// A set sidebar filter re-homes the canvas onto that project; under
    /// "All" the current pick (restored from composer defaults) stands. The
    /// chat mints on first send.
    pub(super) fn open_new_session_draft(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        let target = {
            let state = self.state.read(cx);
            self.settings
                .space_filter
                .clone()
                .filter(|id| state.space_row(id).is_some())
        };
        let defaults = crate::settings::composer::ComposerDefaults::load(&self.data_dir);
        self.state.update(cx, |s, cx| {
            if target.is_some() {
                s.select_space(target, cx);
            } else if defaults.no_project {
                // Opening an existing project session (including boot's last
                // session) must not erase the saved new-session opt-out.
                s.select_space(None, cx);
                if let Some(device) = defaults.device {
                    s.select_device(device, cx);
                }
            }
            s.select_chat(None, cx);
        });
        cx.notify();
    }

    /// The unified titlebar in chat mode:
    /// `[new-session +] [harness icon + session title] … [toggle-changes]`.
    /// Replaces the tab strip; inherits its titlebar duties (drag region,
    /// animated left inset, the toggle-changes button on git projects).
    pub(super) fn render_session_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // The canvas titles as NOTHING (user request — a "New session"
        // header over the empty canvas was noise); the bar keeps its height,
        // drag region, and buttons. A session appends its target as a muted
        // "project @ device" tag right of the title (the composer footer no
        // longer carries it).
        let (title, target, harness, on_canvas): (
            SharedString,
            Option<SharedString>,
            Option<zeron_proto::HarnessId>,
            bool,
        ) = {
            let state = self.state.read(cx);
            match state.selected_chat_row() {
                Some(chat) => {
                    let folder = chat
                        .space_id
                        .as_deref()
                        .and_then(|id| state.space_row(id))
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "~".to_string());
                    let device = state
                        .device_name(&chat.device_id)
                        .unwrap_or("Unknown device");
                    (
                        SharedString::from(transcript::single_line(
                            &chat.title.clone().unwrap_or_else(|| "New session".into()),
                        )),
                        Some(SharedString::from(format!("{folder} @ {device}"))),
                        chat.config.as_ref().map(|c| c.harness),
                        false,
                    )
                }
                None => (SharedString::from(""), None, None, true),
            }
        };

        // The new-session `+` renders in the WINDOW-CONTROL CLUSTER whenever a
        // session is selected (`render_titlebar_cluster`) — this row budgets
        // one button slot so the title never sits under it.
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let plus_inset = TITLEBAR_ACTION_SLOT_WIDTH * self.titlebar_plus_alpha(cx);

        // Same glide as the old strip: content starts at the inset card's
        // left edge while the sidebar is open, and slides toward the control
        // cluster as it collapses.
        let content_left =
            (sidebar_now + Theme::SPACE_LG).max(self.title_bar_content_start() + plus_inset);

        // Trailing titlebar section. With the changes pane open this is the
        // PANE'S HEADER — a strip exactly as wide as the pane carrying its
        // controls (scope dropdown, ref selector, fold-all from the Changes
        // entity; expand + close shell-side). It lives up here because the
        // titlebar overlay owns this band's hit-testing: controls mounted in
        // the pane itself would sit under the drag region and never see a
        // click. Closed, it is just the stable open/close toggle. Hidden on
        // the new-session canvas (user request) — nothing to diff yet.
        let takeover = !on_canvas && self.right_pane_open(cx) && self.right_pane_expanded;
        // In takeover the title hides and the strip owns the whole band, so
        // the row's left inset pulls back to the sidebar seam — the title
        // inset would push the scope dropdown off the pane's own left gutter
        // (user report: misaligned dead space). With the sidebar COLLAPSED
        // the seam is the window edge, where the traffic lights + nav
        // cluster overlay lives — the strip must still clear it, but only
        // just: `title_bar_content_start` carries the identity-group margin the
        // strip doesn't want (it brings its own 8px pad), and doubling up
        // read as a hole after the `+` (user report).
        let row_left = if takeover {
            // The surface tabs must LEFT-ALIGN with the pane's own rows (the
            // diff options and stats strip carry an 8px box gutter off the
            // seam — user report: rows started at different insets). The
            // strip's width is capped to `avail`, which subtracts the row's
            // 8px child gap — pulling row_left 8 LEFT of the seam cancels
            // that, so the uncapped strip starts exactly at the seam and its
            // own 8px pad lands the first chip on the pane gutter. The
            // window-control cluster still wins while the sidebar is
            // collapsed (the chips clear it instead of underlapping).
            let cluster_end =
                self.title_bar_content_start() - TITLEBAR_IDENTITY_GAP + plus_inset - 14.0;
            (sidebar_now - 8.0).max(cluster_end)
        } else {
            content_left
        };
        let trailing: Option<gpui::AnyElement> = if on_canvas {
            None
        } else {
            let right_open = self.right_pane_open(cx);
            let mut controls = div()
                .id("right-titlebar-controls")
                .flex_none()
                .h_full()
                .flex()
                .flex_row()
                .items_center();
            if right_open {
                let right_now = self.eval_tween(self.right_tween, self.right_target(cx));
                let pr = self.titlebar_right_pad(TITLEBAR_ACTION_EDGE_INSET);
                // The row's own left padding is part of its content box: a strip
                // wider than what's left after it overflows and clips at the right
                // edge (flex_none never shrinks) — cap to the available width. The
                // row's 8px child gaps sit OUTSIDE the strip's width (one before
                // the strip in takeover, two with the title row present): without
                // budgeting them the capped strip overflows by exactly one gap and
                // the buttons slide right on expand (user report).
                let gap_budget = if takeover { 8.0 } else { 16.0 };
                let avail = self.viewport_width - row_left - pr - gap_budget;
                // The right pane's SURFACE TABS (t3 RightPanelTabs) — the diff
                // options that used to live here moved into the pane's own
                // second row; expand stays in this band (user request).
                let tabs = self.render_right_tab_strip(cx);
                // The toggle is the fixed right-edge anchor, like the left
                // sidebar control. Only the tabs + expand section reveals to
                // its left; including the toggle in this animated width
                // compressed both icons into the same clipped box at open.
                let animated_width = ((right_now - pr).min(avail) - 28.0).max(0.0);
                controls = controls.child(
                    div()
                        .w(px(animated_width))
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .overflow_hidden()
                        // 8 + the trigger's own 8px pad = the pane's 16px
                        // text gutter. The 4px right padding is the stable
                        // gap before the fixed toggle.
                        .pl(px(8.0))
                        .pr(px(4.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .overflow_hidden()
                                .child(tabs),
                        )
                        .child(header_icon_button(
                            "expand-changes",
                            right_pane_expand_icon(self.right_pane_expanded),
                            &theme,
                            cx.listener(|this, _, _, cx| this.toggle_right_pane_expand(cx)),
                        )),
                );
            }
            // The floating todo progress card owns the in-chat todo UI;
            // no titlebar trigger (ZCode pattern — it mounts itself).
            Some(
                controls
                    .child(header_icon_button(
                        "toggle-changes",
                        icons::SIDEBAR_MINIMALISTIC,
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                    ))
                    .into_any_element(),
            )
        };

        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(8.0))
            .pl(px(row_left))
            .pr(px(self.titlebar_right_pad(TITLEBAR_ACTION_EDGE_INSET)))
            // In panel takeover the header strip spans the whole band — the
            // title would sit UNDER it (both flex_none, the row overflows and
            // paint order stacks them), so it hides for the duration.
            .when(!takeover, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .when_some(
                            harness.map(crate::pickers::harness_brand_icon),
                            |el, (path, tint)| {
                                el.child(
                                    icon(path)
                                        .size(px(14.0))
                                        .flex_none()
                                        .text_color(tint.unwrap_or(theme.text_muted)),
                                )
                            },
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(crate::typography::ui_rems(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if on_canvas {
                                    theme.text_muted.opacity(0.7)
                                } else {
                                    theme.text.opacity(0.85)
                                })
                                .child(title),
                        )
                        .when_some(target, |el, target| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_muted.opacity(0.5))
                                    .child(target),
                            )
                        }),
                )
            })
            .child(div().flex_1())
            .children(trailing);

        // The unified window titlebar: full-width on the glass shell, ABOVE
        // the inset card. No bottom border — the card's own hairline is the
        // separation; the glass gutter shows between.
        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("chat-titlebar", bar, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn steps_forward_and_back_through_the_list() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, Some("a"), true).as_deref(), Some("b"));
        assert_eq!(cycle_target(&list, Some("b"), true).as_deref(), Some("c"));
        assert_eq!(cycle_target(&list, Some("c"), false).as_deref(), Some("b"));
        assert_eq!(cycle_target(&list, Some("b"), false).as_deref(), Some("a"));
    }

    #[test]
    fn wraps_at_both_ends() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, Some("c"), true).as_deref(), Some("a"));
        assert_eq!(cycle_target(&list, Some("a"), false).as_deref(), Some("c"));
    }

    #[test]
    fn a_single_session_cycles_to_itself() {
        // Not a no-op by accident: with one row both directions must resolve,
        // so the shortcut never looks broken by dead-ending on `None`.
        let list = order(&["only"]);
        assert_eq!(
            cycle_target(&list, Some("only"), true).as_deref(),
            Some("only")
        );
        assert_eq!(
            cycle_target(&list, Some("only"), false).as_deref(),
            Some("only")
        );
    }

    #[test]
    fn no_selection_enters_the_list_from_the_matching_end() {
        let list = order(&["a", "b", "c"]);
        assert_eq!(cycle_target(&list, None, true).as_deref(), Some("a"));
        assert_eq!(cycle_target(&list, None, false).as_deref(), Some("c"));
        assert_eq!(
            cycle_target(&list, Some("gone"), true).as_deref(),
            Some("a")
        );
        assert_eq!(
            cycle_target(&list, Some("gone"), false).as_deref(),
            Some("c")
        );
    }

    #[test]
    fn an_empty_list_has_nothing_to_select() {
        assert_eq!(cycle_target(&[], None, true), None);
        assert_eq!(cycle_target(&[], Some("a"), true), None);
    }

    // Cycling walks the rows the sidebar is drawing, not every chat: that
    // guarantee is structural now — `cycle_session` reads the same
    // `AppState::sidebar_chats` the sidebar and the jump shortcuts read, and
    // `jump_slots_count_the_rows_the_sidebar_draws` (state.rs) covers the
    // space-filter behaviour for all of them.
}

#[cfg(test)]
mod todo_card_input_tests {
    use super::*;
    use gpui::{MouseButton, ScrollHandle, TestAppContext, point};

    struct TodoCardInputView {
        transcript: ScrollHandle,
        mouse_downs: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Render for TodoCardInputView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mouse_downs = self.mouse_downs.clone();
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("todo-card-test-transcript")
                        .absolute()
                        .inset_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.transcript)
                        .on_mouse_down(MouseButton::Left, move |_, _, _| {
                            mouse_downs.set(mouse_downs.get() + 1)
                        })
                        .child(div().h(px(2_000.0))),
                )
                .child(div().flex_1().min_h_0())
                .child(todo_card_layer(
                    Theme::of(cx),
                    div().h(px(32.0)).child("Progress").into_any_element(),
                    div().h(px(48.0)).child("One task").into_any_element(),
                ))
                .child(div().h(px(40.0)).flex_none())
        }
    }

    #[gpui::test]
    fn floating_todo_card_does_not_block_the_conversation_column(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let (view, cx) = cx.add_window_view(|_, _| TodoCardInputView {
            transcript: ScrollHandle::new(),
            mouse_downs: Default::default(),
        });
        cx.simulate_resize(gpui::size(px(600.0), px(400.0)));
        cx.run_until_parked();

        cx.simulate_event(gpui::MouseDownEvent {
            position: point(px(40.0), px(200.0)),
            button: MouseButton::Left,
            ..Default::default()
        });
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(400.0), px(70.0)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-80.0))),
            ..Default::default()
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert_eq!(view.mouse_downs.get(), 1);
            assert!(
                view.transcript.offset().y < px(0.0),
                "the floating card must not swallow transcript wheel events"
            );
        });
    }
}

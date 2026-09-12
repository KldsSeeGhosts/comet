//! ActivePlanHud — the compact plan-progress strip between the transcript and
//! the composer (ERM-486 UI polish).
//!
//! The agent's plan state is the most recent `ToolCall::Todo` in the
//! transcript: todo updates re-emit the whole list, so the latest call carries
//! the current state. Derivation is pure over the entry snapshot and counts
//! the same items the transcript's "Todo N/M done" chip does — both read
//! `ToolCall::Todo { items }` directly, so the HUD and the chip can never
//! disagree. The chip itself stays put; this strip mirrors it as one line of
//! always-visible chrome: label, `done/total`, the first unfinished step, and
//! a thin progress bar. Hidden entirely while no plan exists; when every step
//! is done it flips to a check-marked "Plan complete" state instead.

use gpui::{AnyElement, IntoElement, SharedString, div, prelude::*, px};

use zeron_doc::{MessagePart, SessionMessageEntry};
use zeron_proto::{TodoItem, ToolCall};

use crate::theme::Theme;

/// Strip height. Constant, so mounting it is an instant one-frame layout
/// change (no reserved-space tween) and the transcript's bottom clearance can
/// add it without measuring.
pub const HUD_HEIGHT: f32 = 28.0;

/// Inline progress bar geometry: a short pill in the label row, not a
/// full-bleed rule — a pane-wide fill reads as a stray divider, not chrome.
const BAR_WIDTH: f32 = 96.0;
const BAR_HEIGHT: f32 = 3.0;

/// The latest Todo tool state, reduced to what the strip paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanProgress {
    /// Total planned steps.
    pub total: usize,
    /// Steps marked done.
    pub done: usize,
    /// First unfinished step's text — the agent's current step. `None` only
    /// when every step is done.
    pub current: Option<String>,
}

impl PlanProgress {
    /// Every step done (a plan always has at least one step, so `None`
    /// current means complete, never empty).
    pub fn is_complete(&self) -> bool {
        self.current.is_none()
    }

    /// Bar fill fraction, `0..=1`.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        self.done as f32 / self.total as f32
    }
}

/// Derive the plan state from a transcript snapshot: the newest entry (then
/// newest part) carrying a `ToolCall::Todo` wins, matching
/// `AppState::latest_todos`. `None` while the transcript has no todo list —
/// including an EMPTY newest list, which reads as the agent having cleared
/// its plan.
pub fn plan_progress(entries: &[SessionMessageEntry]) -> Option<PlanProgress> {
    let items = entries.iter().rev().find_map(|entry| {
        entry.parts.iter().rev().find_map(|part| match part {
            MessagePart::Tool {
                call: ToolCall::Todo { items },
                ..
            } => Some(items.as_slice()),
            _ => None,
        })
    })?;
    plan_progress_from_items(items)
}

/// Reduce one todo list to the HUD's (total, done, current-step) tuple. Pure.
pub fn plan_progress_from_items(items: &[TodoItem]) -> Option<PlanProgress> {
    if items.is_empty() {
        return None;
    }
    Some(PlanProgress {
        total: items.len(),
        done: items.iter().filter(|item| item.done).count(),
        current: items.iter().find(|item| !item.done).map(|item| item.text.clone()),
    })
}

/// The strip itself: opaque chrome matching the composer footer it docks
/// above (same `bg`, same low-alpha hairline the chip cards use), with the
/// progress bar riding its bottom edge.
pub fn render(progress: &PlanProgress, theme: &Theme) -> AnyElement {
    let complete = progress.is_complete();
    let (icon_path, label) = if complete {
        (crate::icons::CHECK, "Plan complete")
    } else {
        (crate::icons::CHECKLIST, "Plan")
    };
    let counter_color = if complete {
        theme.success
    } else {
        theme.text_muted.opacity(0.7)
    };
    let fill = if complete {
        theme.success
    } else {
        theme.accent
    };
    div()
        .id("plan-hud")
        // Test hook: bounds lookups by name (a noop outside tests).
        .debug_selector(|| "plan-hud".into())
        .flex_none()
        .relative()
        .w_full()
        .h(px(HUD_HEIGHT))
        .bg(theme.bg)
        .border_t_1()
        .border_color(crate::theme::hairline(0.08))
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .child(
                    crate::icons::icon(icon_path)
                        .size(px(13.0))
                        .text_color(if complete {
                            theme.success
                        } else {
                            theme.text_muted
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(10.5))
                        .text_color(counter_color)
                        .child(SharedString::from(format!(
                            "{}/{}",
                            progress.done, progress.total
                        ))),
                )
                // Compact inline progress pill: fill fraction at a glance
                // without a pane-wide rule under the strip.
                .child(
                    div()
                        .flex_none()
                        .w(px(BAR_WIDTH))
                        .h(px(BAR_HEIGHT))
                        .rounded_full()
                        .bg(crate::theme::ink(0.08))
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(progress.fraction()))
                                .rounded_full()
                                .bg(fill),
                        ),
                )
                // The in-flight step. All-done strips have nothing in flight
                // — the label says it instead.
                .when_some(progress.current.clone(), |el, step| {
                    el.child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text.opacity(0.9))
                            .child(SharedString::from(step)),
                    )
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::MessageRole;

    fn entry(parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "a1".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn text(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn todo(id: &str, items: &[(&str, bool)]) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Todo {
                items: items
                    .iter()
                    .map(|(text, done)| TodoItem {
                        text: (*text).into(),
                        done: *done,
                    })
                    .collect(),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    #[test]
    fn no_todos_yields_no_hud() {
        assert_eq!(plan_progress(&[]), None);
        assert_eq!(plan_progress(&[entry(vec![text("t0", "hello")])]), None);
    }

    #[test]
    fn derivation_reports_total_done_and_first_open_step() {
        let progress = plan_progress(&[entry(vec![todo("p0", &[
            ("ship the parser", true),
            ("wire the hud", false),
            ("polish", false),
        ])])])
        .unwrap();
        assert_eq!(
            progress,
            PlanProgress {
                total: 3,
                done: 1,
                current: Some("wire the hud".into()),
            }
        );
        assert!(!progress.is_complete());
        assert!((progress.fraction() - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn all_done_leaves_no_current_step() {
        let progress =
            plan_progress(&[entry(vec![todo("p0", &[("one", true), ("two", true)])])]).unwrap();
        assert_eq!(progress.total, 2);
        assert_eq!(progress.done, 2);
        assert_eq!(progress.current, None);
        assert!(progress.is_complete());
        assert_eq!(progress.fraction(), 1.0);
    }

    #[test]
    fn the_newest_todo_call_wins_over_older_ones() {
        // Todo updates re-emit the whole list, so the latest call carries the
        // current state — an older list further in the transcript never wins.
        let entries = [
            entry(vec![todo("p0", &[("stale step", false)])]),
            entry(vec![
                text("t0", "making progress"),
                todo("p1", &[("stale step", true), ("fresh step", false)]),
            ]),
        ];
        let progress = plan_progress(&entries).unwrap();
        assert_eq!(progress.current.as_deref(), Some("fresh step"));
        assert_eq!((progress.total, progress.done), (2, 1));
        // Scanning is newest-first, so equal recency inside one entry also
        // prefers the later part.
        let same_entry = [entry(vec![
            todo("p0", &[("older", false)]),
            todo("p1", &[("newer", false)]),
        ])];
        assert_eq!(plan_progress(&same_entry).unwrap().current.as_deref(), Some("newer"));
    }

    #[test]
    fn an_empty_newest_list_reads_as_no_plan() {
        let entries = [
            entry(vec![todo("p0", &[("was a plan", false)])]),
            entry(vec![todo("p1", &[])]),
        ];
        assert_eq!(plan_progress(&entries), None);
        // An empty list after a populated one is a clear, not a keep-alive:
        // only the NEWEST call is consulted.
        assert_eq!(
            plan_progress(&[entry(vec![todo("p1", &[]), todo("p0", &[("real", false)])])])
                .unwrap()
                .total,
            1
        );
    }

    #[test]
    fn empty_items_never_render_a_strip() {
        assert_eq!(plan_progress_from_items(&[]), None);
    }
}

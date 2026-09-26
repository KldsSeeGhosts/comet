//! Context occupancy is read from the replicated chat snapshot, never local CLI state.
use crate::state::{AppState, ChatTarget};
use crate::theme::Theme;
use gpui::{
    Context, IntoElement, PathBuilder, Render, SharedString, Window, canvas, div, point,
    prelude::*, px,
};
use zeron_proto::ContextUsage;

/// The replicated usage frame belongs to the selected chat. Neither an
/// unbound composer nor another split may borrow its ring or tooltip data.
pub(crate) fn usage_for_target(state: &AppState, target: &ChatTarget) -> Option<ContextUsage> {
    let chat_id = target.chat_id(state)?;
    (state.selected_chat.as_deref() == Some(chat_id))
        .then_some(state.context_usage)
        .flatten()
}

pub(crate) fn render(
    usage: Option<ContextUsage>,
    state: gpui::Entity<crate::state::AppState>,
    target: ChatTarget,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let fraction = usage.and_then(ContextUsage::fraction);
    let color = fill_color(fraction, theme);
    let track = theme.text_faint.opacity(0.25);
    let ring = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let mut arc = |fraction: f32, color| {
                if fraction <= 0.0 {
                    return;
                }
                let steps = (64.0 * fraction).ceil().max(2.0) as usize;
                let mut path = PathBuilder::stroke(px(1.8));
                for i in 0..=steps {
                    let angle = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * fraction * i as f32 / steps as f32;
                    let p = point(
                        center.x + px(6.0 * angle.cos()),
                        center.y + px(6.0 * angle.sin()),
                    );
                    if i == 0 {
                        path.move_to(p);
                    } else {
                        path.line_to(p);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            arc(1.0, track);
            arc(fraction.unwrap_or(0.0).clamp(0.0, 1.0) as f32, color);
        },
    )
    .size(px(16.0));
    let label = fraction
        .map(|f| format!("Context window: {:.0}% used", f * 100.0))
        .unwrap_or_else(|| "Context window: usage not reported".into());
    div()
        .id("context-usage")
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.0))
        .rounded(px(6.0))
        .aria_label(SharedString::from(label))
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .child(ring)
        .tooltip(move |_, cx| {
            cx.new(|cx| UsageCard {
                _subscription: cx.observe(&state, |_, _, cx| cx.notify()),
                state: state.clone(),
                target: target.clone(),
            })
            .into()
        })
}

struct UsageCard {
    state: gpui::Entity<crate::state::AppState>,
    target: ChatTarget,
    _subscription: gpui::Subscription,
}

/// Ring + bar fill share one hue rule: danger at >=90%, warning at >=75%,
/// muted below; the unmeasured state is faint (never an alarming color).
fn fill_color(fraction: Option<f64>, theme: &Theme) -> gpui::Hsla {
    match fraction {
        Some(f) if f >= 0.9 => theme.danger,
        Some(f) if f >= 0.75 => theme.warning,
        Some(_) => theme.text_muted,
        None => theme.text_faint,
    }
}

fn with_separators(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// whether the indicator has anything to measure against: harnesses that
/// never report a window (antigravity) get no indicator at all, rather than a
/// permanently empty ring.
pub fn has_window(usage: Option<ContextUsage>) -> bool {
    usage
        .and_then(|usage| usage.window)
        .is_some_and(|window| window > 0)
}

/// The card's mono lines. `tokens: None` (post-compaction) is "waiting",
/// never 0% and never the stale pre-compaction number.
fn detail_lines(usage: Option<ContextUsage>) -> Vec<String> {
    match usage.unwrap_or_default() {
        ContextUsage {
            tokens: Some(tokens),
            window: Some(window),
            ..
        } if window > 0 => vec![
            format!(
                "{} / {} tokens",
                with_separators(tokens),
                with_separators(window)
            ),
            format!(
                "{} left",
                with_separators(window.saturating_sub(tokens))
            ),
        ],
        ContextUsage {
            tokens: Some(tokens),
            ..
        } => vec![
            format!("{} tokens used", with_separators(tokens)),
            "Context limit not reported".into(),
        ],
        ContextUsage {
            window: Some(window),
            ..
        } if window > 0 => vec![
            format!("{} token capacity", with_separators(window)),
            "Waiting for context usage".into(),
        ],
        _ => vec!["Context usage not reported by this harness yet".into()],
    }
}

impl Render for UsageCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &Theme::of(cx).for_popup();
        let usage = usage_for_target(self.state.read(cx), &self.target);
        let fraction = usage.and_then(ContextUsage::fraction);
        let fill = fill_color(fraction, theme);
        let percent = fraction.map(|f| format!("{:.0}%", f * 100.0));

        let mut card = crate::popover::popover_card(theme)
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(16.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Context window"),
                    )
                    .children(percent.map(|p| {
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .text_color(fill)
                            .child(p)
                    })),
            )
            .child(
                div()
                    .h(px(4.0))
                    .w_full()
                    .rounded(px(2.0))
                    .bg(theme.text_faint.opacity(0.25))
                    .child(
                        div()
                            .h_full()
                            .rounded(px(2.0))
                            .bg(fill)
                            .w(gpui::relative(
                                fraction.unwrap_or(0.0).clamp(0.0, 1.0) as f32,
                            )),
                    ),
            )
            .children(detail_lines(usage).into_iter().map(|line| {
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .line_height(px(16.0))
                    // the lines break only at their own boundaries: a tooltip
                    // sizes from unwrapped text, so soft wrapping clipped it
                    .whitespace_nowrap()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(line))
            }));
        let compact_percent = usage.and_then(|u| {
            let window = u.window.filter(|w| *w > 0)?;
            Some(u.compact_at? as f64 / window as f64 * 100.0)
        });
        if let Some(compaction) = compact_percent {
            card = card.child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .line_height(px(16.0))
                    .whitespace_nowrap()
                    .text_color(theme.text_faint)
                    .child(format!("Auto-compacts at ~{compaction:.0}%")),
            );
        }
        if let Some(session) = usage.and_then(|u| u.session) {
            card = card.child(
                div()
                    .pt(px(4.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .line_height(px(16.0))
                    .whitespace_nowrap()
                    .text_color(theme.text_faint)
                    .child(format!(
                        "Session: {} in · {} out · {} cache",
                        with_separators(session.input),
                        with_separators(session.output),
                        with_separators(session.cache_read),
                    )),
            );
        }
        crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_never_leaks_to_another_pane_or_an_unbound_composer() {
        let mut state = AppState::new();
        state.selected_chat = Some("chat-a".into());
        state.context_usage = Some(ContextUsage {
            tokens: Some(42_000),
            window: Some(200_000),
            ..Default::default()
        });
        let bound = ChatTarget::Fixed(Some("chat-a".into()));
        assert!(has_window(usage_for_target(&state, &bound)));
        assert!(has_window(usage_for_target(&state, &ChatTarget::Selected)));
        assert!(usage_for_target(&state, &ChatTarget::Fixed(Some("chat-b".into()))).is_none());
        assert!(usage_for_target(&state, &ChatTarget::Fixed(None)).is_none());
        // A tooltip left open across navigation must not adopt chat-b's frame.
        state.selected_chat = Some("chat-b".into());
        assert!(usage_for_target(&state, &bound).is_none());
        state.selected_chat = None;
        assert!(usage_for_target(&state, &ChatTarget::Selected).is_none());
    }
    #[test]
    fn indicator_needs_a_reported_window() {
        assert!(!has_window(None));
        assert!(!has_window(Some(ContextUsage {
            tokens: Some(1_200),
            window: None,
            ..Default::default()
        })));
        assert!(!has_window(Some(ContextUsage {
            tokens: Some(1_200),
            window: Some(0),
            ..Default::default()
        })));
        assert!(has_window(Some(ContextUsage {
            tokens: None,
            window: Some(200_000),
            ..Default::default()
        })));
    }

    #[test]
    fn missing_usage_is_distinct_from_zero_and_overflow() {
        assert!(detail_lines(None)[0].contains("not reported"));
        assert_eq!(
            detail_lines(Some(ContextUsage {
                tokens: Some(0),
                window: Some(200),
                ..Default::default()
            }))[1],
            "200 left"
        );
        assert_eq!(
            detail_lines(Some(ContextUsage {
                tokens: Some(250),
                window: Some(200),
                ..Default::default()
            }))[1],
            "0 left"
        );
        assert!(detail_lines(Some(ContextUsage {
            tokens: Some(10),
            window: Some(0),
            ..Default::default()
        }))[1]
            .contains("limit not reported"));
        // Post-compaction `tokens: None` is "waiting", never 0% and never a
        // stale pre-compaction count.
        let waiting = detail_lines(Some(ContextUsage {
            tokens: None,
            window: Some(200_000),
            ..Default::default()
        }));
        assert_eq!(waiting[1], "Waiting for context usage");
    }

    #[test]
    fn token_counts_are_grouped_by_thousands() {
        assert_eq!(with_separators(0), "0");
        assert_eq!(with_separators(999), "999");
        assert_eq!(with_separators(5417), "5,417");
        assert_eq!(with_separators(1_048_576), "1,048,576");
        assert_eq!(
            detail_lines(Some(ContextUsage {
                tokens: Some(5417),
                window: Some(1_048_576),
                ..Default::default()
            })),
            vec![
                "5,417 / 1,048,576 tokens".to_string(),
                "1,043,159 left".to_string()
            ]
        );
    }
}

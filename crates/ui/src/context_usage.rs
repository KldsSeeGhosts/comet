//! Context occupancy is read from the replicated chat snapshot, never local CLI state.
use crate::theme::Theme;
use gpui::{
    Context, IntoElement, PathBuilder, Render, SharedString, Window, canvas, div, point,
    prelude::*, px, relative,
};
use zeron_proto::{ContextComponentKind, ContextUsage};

pub fn render(
    usage: Option<ContextUsage>,
    state: gpui::Entity<crate::state::AppState>,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let fraction = usage.and_then(ContextUsage::fraction);
    let color = match fraction {
        Some(f) if f >= 0.9 => theme.danger,
        Some(f) if f >= 0.75 => theme.warning,
        Some(_) => theme.text_muted,
        None => theme.text_faint,
    };
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
        .map(|f| format!("{:.0}%", f * 100.0))
        .unwrap_or_else(|| "—".into());
    div()
        .id("context-usage")
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .text_color(color)
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .child(ring)
        .child(SharedString::from(label))
        .tooltip(move |_, cx| {
            cx.new(|cx| UsageCard {
                _subscription: cx.observe(&state, |_, _, cx| cx.notify()),
                state: state.clone(),
            })
            .into()
        })
}

struct UsageCard {
    state: gpui::Entity<crate::state::AppState>,
    _subscription: gpui::Subscription,
}

fn details(usage: Option<ContextUsage>) -> String {
    match usage.unwrap_or_default() {
        ContextUsage {
            tokens: Some(tokens),
            window: Some(window),
            ..
        } if window > 0 => {
            format!(
                "{} / {} tokens\n{} tokens remaining",
                tokens,
                window,
                window.saturating_sub(tokens)
            )
        }
        ContextUsage {
            tokens: Some(tokens),
            ..
        } => format!("{tokens} tokens used\nContext limit not reported"),
        ContextUsage {
            window: Some(window),
            ..
        } if window > 0 => format!("{window} token capacity\nWaiting for context usage"),
        _ => "Context usage not reported by this harness yet".into(),
    }
}

/// "41.4K", "1M", "999" — compact token counts for the card header.
fn format_compact(tokens: u64) -> String {
    const K: f64 = 1_000.0;
    const M: f64 = 1_000_000.0;
    if tokens >= 10_000 && tokens < 1_000_000 {
        format!("{:.1}K", tokens as f64 / K)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / M)
    } else {
        tokens.to_string()
    }
}

/// "55.8%" of the context window, one decimal like every row shows.
fn percent_label(tokens: u64, total: u64) -> String {
    if total == 0 {
        return "—".into();
    }
    format!("{:.1}%", tokens as f64 / total as f64 * 100.0)
}

/// Card rows sorted largest-first, as the breakdown reads best.
fn breakdown_rows(usage: &ContextUsage) -> Vec<(ContextComponentKind, u64)> {
    let mut rows: Vec<(ContextComponentKind, u64)> = usage
        .components
        .iter()
        .map(|component| (component.kind, component.tokens))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows
}

/// Header summary "41.4K/1M (4.1%)"; none while either side is unknown.
fn usage_summary(usage: &ContextUsage) -> Option<String> {
    let tokens = usage.tokens?;
    let window = usage.window?;
    let percent = if window > 0 {
        format!(" ({:.1}%)", tokens as f64 / window as f64 * 100.0)
    } else {
        String::new()
    };
    Some(format!(
        "{}/{}{}",
        format_compact(tokens),
        format_compact(window),
        percent
    ))
}

fn component_label(kind: ContextComponentKind) -> &'static str {
    match kind {
        ContextComponentKind::Tools => "Tools",
        ContextComponentKind::SystemPrompt => "System prompt",
        ContextComponentKind::Skills => "Skills",
        ContextComponentKind::ContextFiles => "Context files",
        ContextComponentKind::Messages => "Messages",
    }
}

fn component_color(theme: &Theme, kind: ContextComponentKind) -> gpui::Hsla {
    match kind {
        ContextComponentKind::Tools => theme.accent,
        ContextComponentKind::SystemPrompt => theme.syntax.keyword,
        ContextComponentKind::Skills => theme.syntax.string,
        ContextComponentKind::ContextFiles => theme.syntax.number,
        ContextComponentKind::Messages => theme.syntax.property,
    }
}

impl Render for UsageCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let usage = self.state.read(cx).context_usage.clone();
        let breakdown = usage
            .as_ref()
            .map(breakdown_rows)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, tokens)| *tokens > 0)
            .collect::<Vec<_>>();
        let card = crate::popover::popover_card(theme)
            .w(px(280.0))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0));
        let Some(usage) = usage else {
            return crate::frost::frosted(
                crate::popover::CARD_RADIUS,
                crate::frost::MENU_BLUR,
                card.child(
                    div()
                        .text_size(px(12.0))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(details(None))),
                ),
            );
        };
        let header_right = usage_summary(&usage);
        let mut card = card.child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child("Context window"),
                )
                .children(header_right.map(|summary| {
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(summary))
                })),
        );
        if !breakdown.is_empty() {
            let track = theme.text_faint.opacity(0.25);
            let window = usage.window.unwrap_or(0);
            let mut bar = div()
                .id("context-bar")
                .flex()
                .h(px(6.0))
                .rounded(px(3.0))
                .overflow_hidden()
                .bg(track);
            // Segments paint in the stable ALL order so each category keeps
            // its color and its left-to-right slot between renders.
            for kind in ContextComponentKind::ALL {
                let Some((_, tokens)) = breakdown.iter().find(|(row, _)| *row == kind) else {
                    continue;
                };
                let fraction = if window > 0 {
                    (*tokens as f32 / window as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if fraction <= 0.0 {
                    continue;
                }
                bar = bar.child(
                    div()
                        .w(relative(fraction))
                        .h_full()
                        .bg(component_color(&theme, kind)),
                );
            }
            card = card.child(bar);
            let rows = breakdown.iter().map(|(kind, tokens)| {
                let dot = component_color(&theme, *kind);
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .child(div().size(px(7.0)).rounded_full().bg(dot))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(component_label(*kind))),
                            ),
                    )
                    .child(
                        div()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .text_color(theme.text_dim)
                            .child(SharedString::from(match window {
                                0 => format_compact(*tokens),
                                _ => percent_label(*tokens, window),
                            })),
                    )
            });
            card = card
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .children(rows.collect::<Vec<_>>()),
                )
                .child(div().w_full().h(px(1.0)).bg(theme.hairline(0.08)));
        }
        let footer = match usage {
            ContextUsage {
                tokens: Some(tokens),
                window: Some(window),
                ..
            } if window > 0 => SharedString::from(format!(
                "{} of {} tokens remaining",
                format_compact(window.saturating_sub(tokens)),
                format_compact(window)
            )),
            _ => SharedString::from(details(Some(usage))),
        };
        crate::frost::frosted(
            crate::popover::CARD_RADIUS,
            crate::frost::MENU_BLUR,
            card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(footer),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::{ContextComponent, ContextUsage};

    fn component(kind: ContextComponentKind, tokens: u64) -> ContextComponent {
        ContextComponent { kind, tokens }
    }

    #[test]
    fn missing_usage_is_distinct_from_zero_and_overflow() {
        assert!(details(None).contains("not reported"));
        assert!(
            details(Some(ContextUsage {
                tokens: Some(0),
                window: Some(200),
                components: Vec::new(),
            }))
            .contains("200 tokens remaining")
        );
        assert!(
            details(Some(ContextUsage {
                tokens: Some(250),
                window: Some(200),
                components: Vec::new(),
            }))
            .contains("0 tokens remaining")
        );
        assert!(
            details(Some(ContextUsage {
                tokens: Some(10),
                window: Some(0),
                components: Vec::new(),
            }))
            .contains("limit not reported")
        );
    }

    #[test]
    fn compact_counts_use_k_and_m_suffixes() {
        assert_eq!(format_compact(999), "999");
        assert_eq!(format_compact(41_400), "41.4K");
        assert_eq!(format_compact(1_048_576), "1.0M");
    }

    #[test]
    fn rows_sort_largest_first() {
        let usage = ContextUsage {
            tokens: Some(1000),
            window: Some(1_000_000),
            components: vec![
                component(ContextComponentKind::Messages, 100),
                component(ContextComponentKind::Tools, 550),
                component(ContextComponentKind::SystemPrompt, 350),
            ],
        };
        let rows: Vec<_> = breakdown_rows(&usage)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(
            rows,
            vec![
                ContextComponentKind::Tools,
                ContextComponentKind::SystemPrompt,
                ContextComponentKind::Messages,
            ]
        );
        // format_compact keeps four-digit counts unsuffixed (999 → "999").
        assert_eq!(usage_summary(&usage).as_deref(), Some("1000/1.0M (0.1%)"));
    }

    #[test]
    fn percent_needs_a_window() {
        assert_eq!(percent_label(558, 1000), "55.8%");
        assert_eq!(percent_label(1, 0), "—");
        assert_eq!(
            usage_summary(&ContextUsage {
                tokens: Some(20),
                window: None,
                components: Vec::new(),
            }),
            None
        );
    }
}

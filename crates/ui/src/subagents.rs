//! Codex-style subagent inventory: a pure selector over a chat's transcript
//! (spawn tool parts) plus the surfaces that render it — the composer dock
//! strip, the right-pane Agents panel, and the sidebar's nested child rows.
//! Status hues come only from [`SessionState`]; everything else stays on
//! neutral theme tokens.

use std::collections::HashSet;
use std::rc::Rc;

use chrono::{DateTime, TimeZone, Utc};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, FontWeight, IntoElement, ParentElement, SharedString, Styled, div, px,
};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry, SubagentStatus};
use zeron_proto::ToolCall;

use crate::shell::Shell;
use crate::state::AppState;
use crate::status_palette::SessionState;
use crate::theme::Theme;
use crate::{icons, loaders, transcript};

/// Display lifecycle of one spawned subagent. `Started` is the honest
/// neutral state for a spawn that returned without lifecycle proof — the
/// "eager-done" window and `run_in_background` spawns live here (never
/// "Done" just because the spawn call resolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentPhase {
    Running,
    Started,
    Done,
    Failed,
}

impl SubagentPhase {
    pub fn active(self) -> bool {
        matches!(self, Self::Running | Self::Started)
    }
}

/// One spawned subagent, reduced to what the strip/panel/sidebar draw.
#[derive(Debug, Clone)]
pub struct SubagentSummary {
    /// The spawn tool part id (`parent_tool_use_id` for tagged traffic).
    pub id: String,
    pub title: SharedString,
    #[allow(dead_code)] // read by the Agents panel, landing in the follow-up commit
    pub agent_type: Option<SharedString>,
    #[allow(dead_code)] // read by the Agents panel, landing in the follow-up commit
    pub model: Option<SharedString>,
    pub status: SubagentPhase,
    pub started: Option<DateTime<Utc>>,
    pub finished: Option<DateTime<Utc>>,
    /// First line of the result text (<=120 chars), or the live tail.
    pub summary: Option<SharedString>,
    /// The spawned transcript doc id; `None` for doc-less harnesses (Pi).
    pub doc_ref: Option<SharedString>,
    /// Sits in the chat's latest turn (after the last user entry).
    pub latest_turn: bool,
}

impl SubagentSummary {
    /// `45s` / `2m` / `1h 4m` — live for active phases, frozen at finish.
    pub fn elapsed(&self, now: DateTime<Utc>) -> Option<String> {
        let started = self.started?;
        let end = if self.status.active() {
            now
        } else {
            self.finished.unwrap_or(started)
        };
        Some(crate::shell::format_working_elapsed(
            end.signed_duration_since(started).num_seconds(),
        ))
    }
}

fn spawn_input(call: &ToolCall) -> Option<&serde_json::Value> {
    match call {
        ToolCall::Unknown { input, .. } | ToolCall::Mcp { input, .. } => input.as_ref(),
        _ => None,
    }
}

fn input_str<'a>(input: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    input?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Strip a leading `Agent:`/`Task:` genus from a spawn's name, then take the
/// first non-empty line. Mirrors the spawn chip's title rules.
fn spawn_title(call: &ToolCall) -> SharedString {
    let (name, input) = match call {
        ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
        ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
        _ => return "Agent".into(),
    };
    let bare = name
        .strip_prefix("Agent: ")
        .or_else(|| name.strip_prefix("Task: "))
        .unwrap_or(name);
    let candidates = [
        Some(bare),
        input.and_then(|i| i.get("description")?.as_str()),
    ];
    for text in candidates.into_iter().flatten() {
        let line = transcript::single_line(text);
        if !line.is_empty() && !line.eq_ignore_ascii_case("agent") {
            let capped: String = line.chars().take(40).collect();
            return SharedString::from(capped);
        }
    }
    "Agent".into()
}

/// First meaningful line of a result, capped at 120 chars.
fn one_line(text: &str) -> Option<SharedString> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    Some(SharedString::from(out))
}

fn millis(ms: i64) -> Option<DateTime<Utc>> {
    if ms > 0 {
        Utc.timestamp_millis_opt(ms).single()
    } else {
        None
    }
}

fn part_key(chat_id: &str, part_id: &str) -> String {
    format!("{chat_id}/{part_id}")
}

/// Doc id for a doc-less subagent's synthetic result snapshot (Pi parity:
/// the spawn call's own result opens as a read-only transcript). Matches
/// the engine's `{chat}--sub--{id}` shape without colliding with real docs.
pub fn result_doc_id(chat_id: &str, part_id: &str) -> String {
    format!("{chat_id}--sub--result:{part_id}")
}

/// The chat's spawn calls as display summaries. `chat_id` resolves its
/// transcript: the selected chat's joined transcript, or a pane-opened
/// chat's doc watch (`sub_transcripts`). A chat with no loaded transcript
/// selects nothing — that is the "transcript loaded" gate the sidebar uses.
///
/// Order: running first (oldest first), then finished (newest first).
pub fn subagents_for(state: &AppState, chat_id: &str) -> Vec<SubagentSummary> {
    let entries: &[SessionMessageEntry] = if state.selected_chat.as_deref() == Some(chat_id) {
        &state.transcript
    } else {
        state.sub_transcript(chat_id)
    };
    if entries.is_empty() {
        return Vec::new();
    }
    let last_user = entries
        .iter()
        .rposition(|e| e.role == MessageRole::User)
        .unwrap_or(0);
    let mut out: Vec<SubagentSummary> = Vec::new();
    for (ix, entry) in entries.iter().enumerate() {
        if entry.role == MessageRole::User {
            continue;
        }
        let streaming = entry.status == Some(MessageStatus::Streaming);
        for part in &entry.parts {
            let MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
                output,
                subagent_ref,
                subagent_status,
                subagent_tail,
                ..
            } = part
            else {
                continue;
            };
            if !call.is_subagent_spawn() {
                continue;
            }
            let background = input_str(spawn_input(call), "run_in_background")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
            let status = if *is_error {
                SubagentPhase::Failed
            } else {
                match subagent_status {
                    Some(SubagentStatus::Running) => SubagentPhase::Running,
                    Some(SubagentStatus::Done) => SubagentPhase::Done,
                    Some(SubagentStatus::Failed) => SubagentPhase::Failed,
                    None => {
                        if background {
                            SubagentPhase::Started
                        } else if !resolved {
                            // A turn that ended without the spawn resolving
                            // can never report again — it died with the run.
                            if streaming {
                                SubagentPhase::Running
                            } else {
                                SubagentPhase::Failed
                            }
                        } else if output.is_some() {
                            // The call's own result IS the subagent's final
                            // report (ACP harnesses, e.g. Pi — no nested doc).
                            SubagentPhase::Done
                        } else if streaming {
                            // Eager-done window: the call returned while the
                            // tagged lifecycle is still owed.
                            SubagentPhase::Started
                        } else {
                            SubagentPhase::Done
                        }
                    }
                }
            };
            let doc_ref = subagent_ref.clone().map(SharedString::from);
            let summary = output
                .as_deref()
                .and_then(one_line)
                .or_else(|| subagent_tail.as_deref().and_then(one_line))
                .or_else(|| {
                    doc_ref.as_ref().and_then(|doc| {
                        state.sub_transcript(doc).iter().rev().find_map(|e| {
                            e.parts.iter().find_map(|p| match p {
                                MessagePart::Text { text, .. } => one_line(text),
                                _ => None,
                            })
                        })
                    })
                });
            let finished = if status.active() {
                None
            } else {
                doc_ref
                    .as_ref()
                    .and_then(|doc| state.sub_transcript(doc).iter().map(|e| e.created_at).max())
                    .and_then(millis)
                    .or_else(|| {
                        state
                            .subagent_finished_obs
                            .borrow()
                            .get(&part_key(chat_id, id))
                            .and_then(|ms| millis(*ms))
                    })
            };
            out.push(SubagentSummary {
                id: id.clone(),
                title: spawn_title(call),
                agent_type: input_str(spawn_input(call), "subagent_type").map(SharedString::from),
                model: call.subagent_model().map(SharedString::from),
                status,
                started: millis(entry.created_at),
                finished,
                summary,
                doc_ref,
                latest_turn: ix >= last_user,
            });
        }
    }
    out.sort_by(|a, b| {
        let bucket = |s: &SubagentSummary| !s.status.active();
        bucket(a).cmp(&bucket(b)).then_with(|| {
            if a.status.active() {
                a.started.cmp(&b.started)
            } else {
                b.finished.or(b.started).cmp(&a.finished.or(a.started))
            }
        })
    });
    // Record first-observation finish times so terminal agents without a
    // loaded doc still stop their elapsed clock somewhere stable.
    let mut obs = state.subagent_finished_obs.borrow_mut();
    for s in &out {
        if !s.status.active() && s.finished.is_none() {
            obs.entry(part_key(chat_id, &s.id))
                .or_insert_with(|| Utc::now().timestamp_millis());
        }
    }
    out
}

/// The strip's visible subset: the latest turn's subagents, plus anything
/// still live from earlier turns.
pub fn strip_visible(summaries: &[SubagentSummary]) -> Vec<SubagentSummary> {
    summaries
        .iter()
        .filter(|s| s.latest_turn || s.status.active())
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Status glyph (SessionState hues only)
// ---------------------------------------------------------------------------

/// Equalizer Running, check Done (emerald until seen), danger triangle
/// Failed, neutral dot Started.
pub fn status_glyph(
    key: SharedString,
    phase: SubagentPhase,
    seen: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &App,
) -> AnyElement {
    match phase {
        SubagentPhase::Running => {
            loaders::mini_equalizer(key, SessionState::Working.color(theme).unwrap(), view, cx)
                .into_any_element()
        }
        SubagentPhase::Done => icons::icon(icons::CHECK)
            .size(px(12.0))
            .flex_none()
            .text_color(if seen {
                theme.text_faint
            } else {
                SessionState::Completed.color(theme).unwrap()
            })
            .into_any_element(),
        SubagentPhase::Failed => icons::icon(icons::DANGER_TRIANGLE)
            .size(px(12.0))
            .flex_none()
            .text_color(theme.danger)
            .into_any_element(),
        SubagentPhase::Started => div()
            .size(px(4.0))
            .flex_none()
            .rounded_full()
            .bg(theme.text_faint)
            .into_any_element(),
    }
}

// ---------------------------------------------------------------------------
// Composer dock strip
// ---------------------------------------------------------------------------

/// The strip's footprint above the composer pill (28px row + 6px gap).
pub const STRIP_HEIGHT: f32 = 28.0;
pub const STRIP_BOTTOM_GAP: f32 = 6.0;
const PILL_GAP: f32 = 6.0;

/// Estimated pill width (12px glyph + ≤22ch title + mono elapsed + pads) —
/// the strip packs greedily off this estimate; `+N` covers the rest.
/// `open(chat_id, summary)` — pill/sidebar click → select chat + open thread.
pub type OpenAgent = Rc<dyn Fn(&mut Shell, String, SubagentSummary, &mut Context<Shell>)>;
/// Right-pane Agents tab toggle (the strip's trailing chevron).
pub type TogglePanel = Rc<dyn Fn(&mut Shell, &mut Context<Shell>)>;
/// `open_panel(chat_id)` — sidebar `+N more` → select chat + Agents tab.
pub type OpenPanel = Rc<dyn Fn(&mut Shell, String, &mut Context<Shell>)>;

fn pill_width(title_chars: usize) -> f32 {
    8.0 * 2.0 + 12.0 + 6.0 + title_chars.min(22) as f32 * 6.6 + 6.0 + 34.0
}

/// How many leading pills fit `width` (the composer column's content width),
/// keeping room for the leading label, the `+N` overflow pill and the
/// trailing chevron.
fn fitting(summaries: &[SubagentSummary], width: f32) -> usize {
    let mut used = 74.0 + 28.0 + PILL_GAP;
    let mut shown = 0usize;
    for s in summaries {
        let left_after = summaries.len() - shown - 1;
        let reserve = if left_after > 0 { 44.0 } else { 0.0 };
        if used + pill_width(s.title.chars().count()) + reserve > width {
            break;
        }
        used += pill_width(s.title.chars().count()) + PILL_GAP;
        shown += 1;
    }
    shown.min(summaries.len())
}

/// The strip's pills that fit `width`; `+N` covers the remainder.
pub fn strip_layout(summaries: &[SubagentSummary], width: f32) -> (Vec<SubagentSummary>, usize) {
    let shown = fitting(summaries, width);
    (summaries[..shown].to_vec(), summaries.len() - shown)
}

/// The dock strip: `Agents {done}/{total}` label, fitted pills, `+N`
/// overflow, and the trailing chevron that toggles the Agents panel.
/// `seen` = subagent keys whose thread the user already opened.
#[allow(clippy::too_many_arguments)] // render fn; params are the strip's props
pub fn dock_strip(
    chat_id: &str,
    summaries: &[SubagentSummary],
    width: f32,
    panel_open: bool,
    seen: &HashSet<String>,
    open: OpenAgent,
    toggle_panel: TogglePanel,
    now: DateTime<Utc>,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &Context<Shell>,
) -> AnyElement {
    let done = summaries.iter().filter(|s| !s.status.active()).count();
    let (shown, more) = strip_layout(summaries, width);
    let mut row = div()
        .id("subagent-dock-strip")
        .h(px(STRIP_HEIGHT))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(PILL_GAP))
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .pr(px(2.0))
                .text_size(crate::typography::ui_rems(11.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_faint)
                .child("Agents")
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("{done}/{}", summaries.len()))),
                ),
        );
    for s in shown {
        let summary = s.clone();
        let chat = chat_id.to_string();
        let open = open.clone();
        let elapsed = s.elapsed(now);
        let phase = s.status;
        row = row.child(
            div()
                .id(SharedString::from(format!("agent-pill-{}", s.id)))
                .h(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::wash(0.06)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    open(this, chat.clone(), summary.clone(), cx);
                }))
                .child(status_glyph(
                    SharedString::from(format!("agent-pill-glyph-{}", s.id)),
                    phase,
                    seen.contains(&s.id) || seen.contains(&part_key(chat_id, &s.id)),
                    theme,
                    view,
                    cx,
                ))
                .child(
                    div()
                        .flex_none()
                        .max_w(px(150.0))
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(s.title.clone()),
                )
                .children(elapsed.map(|e| {
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(e)
                        .into_any_element()
                })),
        );
    }
    if more > 0 {
        let toggle = toggle_panel.clone();
        row = row.child(
            div()
                .id("agent-pill-more")
                .h(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .px(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::wash(0.06)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    toggle(this, cx);
                }))
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("+{more}"))),
                ),
        );
    }
    row = row.child(div().flex_1().min_w_0()).child({
        let toggle = toggle_panel.clone();
        div()
            .id("agents-panel-toggle")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::wash(0.08)))
            .on_click(cx.listener(move |this, _, _, cx| {
                toggle(this, cx);
            }))
            .child(
                icons::icon(if panel_open {
                    icons::ALT_ARROW_DOWN
                } else {
                    icons::ALT_ARROW_RIGHT
                })
                .size(px(12.0))
                .text_color(theme.text_muted),
            )
    });
    row.into_any_element()
}

// ---------------------------------------------------------------------------
// Sidebar nested rows
// ---------------------------------------------------------------------------

#[allow(dead_code)] // sidebar nested rows land with the card-height animation
pub const SIDEBAR_CHILD_HEIGHT: f32 = 22.0;
#[allow(dead_code)]
pub const SIDEBAR_CHILD_MAX: usize = 3;

/// Up to `SIDEBAR_CHILD_MAX` running subagents below a chat card, aligned to
/// the card's text start with a 1px hairline tree stub; `+N more` opens the
/// Agents panel.
#[allow(dead_code)] // wired into `render_chat_row` in the sidebar commit
#[allow(clippy::too_many_arguments)]
pub fn sidebar_children(
    chat_id: &str,
    summaries: &[SubagentSummary],
    now: DateTime<Utc>,
    theme: &Theme,
    view: gpui::EntityId,
    open: OpenAgent,
    open_panel: OpenPanel,
    cx: &Context<Shell>,
) -> AnyElement {
    let running: Vec<&SubagentSummary> = summaries.iter().filter(|s| s.status.active()).collect();
    let more = running.len().saturating_sub(SIDEBAR_CHILD_MAX);
    let mut col = div().w_full().flex().flex_col();
    for s in running.iter().take(SIDEBAR_CHILD_MAX) {
        let summary = (*s).clone();
        let chat = chat_id.to_string();
        let open = open.clone();
        col = col.child(
            div()
                .id(SharedString::from(format!("sub-child-{}", s.id)))
                .h(px(SIDEBAR_CHILD_HEIGHT))
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .pl(px(10.0))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    open(this, chat.clone(), summary.clone(), cx);
                }))
                .child(
                    div()
                        .w(px(1.0))
                        .h(px(10.0))
                        .flex_none()
                        .bg(theme.hairline(0.10)),
                )
                .child(status_glyph(
                    SharedString::from(format!("sub-glyph-{}", s.id)),
                    s.status,
                    true,
                    theme,
                    view,
                    cx,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(SIDEBAR_CHILD_HEIGHT))
                        .text_color(theme.text_muted)
                        .child(s.title.clone()),
                )
                .children(s.elapsed(now).map(|elapsed| {
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .line_height(px(SIDEBAR_CHILD_HEIGHT))
                        .text_color(theme.text_faint)
                        .child(elapsed)
                        .into_any_element()
                })),
        );
    }
    if more > 0 {
        let parent = chat_id.to_string();
        col = col.child(
            div()
                .id(SharedString::from(format!("sub-more-{chat_id}")))
                .h(px(SIDEBAR_CHILD_HEIGHT))
                .w_full()
                .flex()
                .items_center()
                .pl(px(10.0 + 7.0 + 12.0))
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_faint)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    open_panel(this, parent.clone(), cx);
                }))
                .child(SharedString::from(format!("+{more} more"))),
        );
    }
    // The tree stub hangs off the card's text-start edge.
    div()
        .relative()
        .w_full()
        .child(
            div()
                .absolute()
                .left(px(1.0))
                .top(px(4.0))
                .bottom(px(4.0))
                .w(px(1.0))
                .bg(theme.hairline(0.10)),
        )
        .child(col)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::{MessagePart, MessageRole};
    use zeron_proto::ToolCall;

    fn entry(id: &str, role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 1_700_000_000_000,
            device_id: "dev".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn spawn(id: &str, resolved: bool, status: Option<SubagentStatus>) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Unknown {
                name: "Agent: scout".into(),
                input: Some(serde_json::json!({"description": "scout"})),
            },
            is_error: false,
            resolved,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: status.map(|_| "doc-1".to_string()),
            subagent_status: status,
            subagent_tail: None,
        }
    }

    #[test]
    fn selector_orders_running_then_finished_and_tags_latest_turn() {
        let mut state = AppState::new();
        state.selected_chat = Some("c".into());
        let mut done = spawn("a", true, Some(SubagentStatus::Done));
        let mut running = spawn("b", false, Some(SubagentStatus::Running));
        let mut started = spawn("s", true, None);
        if let MessagePart::Tool { call, .. } = &mut started {
            *call = ToolCall::Unknown {
                name: "Agent: bg".into(),
                input: Some(serde_json::json!({"run_in_background": "true"})),
            };
        }
        state.transcript = vec![
            entry("u1", MessageRole::User, vec![]),
            entry(
                "m1",
                MessageRole::Assistant,
                vec![done.clone(), running.clone()],
            ),
            entry("u2", MessageRole::User, vec![]),
            entry("m2", MessageRole::Assistant, vec![started]),
        ];
        let out = subagents_for(&state, "c");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].status, SubagentPhase::Running);
        assert_eq!(out[0].id, "b");
        assert_eq!(out[1].status, SubagentPhase::Started);
        assert_eq!(out[2].status, SubagentPhase::Done);
        assert!(out[1].latest_turn);
        assert!(!out[2].latest_turn);
    }

    #[test]
    fn pi_spawn_lifecycle_comes_from_the_tool_call() {
        let mut state = AppState::new();
        state.selected_chat = Some("c".into());
        // Unresolved in a streaming turn = Running; resolved with output = Done.
        let mut unresolved = spawn("p1", false, None);
        let mut resolved = spawn("p2", true, None);
        if let MessagePart::Tool { output, .. } = &mut resolved {
            *output = Some("agent report: all green".into());
        }
        let mut live = entry("m", MessageRole::Assistant, vec![unresolved.clone()]);
        live.status = Some(MessageStatus::Streaming);
        state.transcript = vec![
            entry("u", MessageRole::User, vec![]),
            live,
            entry("m2", MessageRole::Assistant, vec![resolved]),
        ];
        let out = subagents_for(&state, "c");
        assert_eq!(out[0].status, SubagentPhase::Running);
        assert_eq!(out[1].status, SubagentPhase::Done);
        assert_eq!(out[1].summary.as_deref(), Some("agent report: all green"));
    }

    #[test]
    fn eager_done_spawn_reads_started_not_done() {
        let mut state = AppState::new();
        state.selected_chat = Some("c".into());
        let resolved_no_output = spawn("e", true, None);
        let mut live = entry("m", MessageRole::Assistant, vec![resolved_no_output]);
        live.status = Some(MessageStatus::Streaming);
        state.transcript = vec![entry("u", MessageRole::User, vec![]), live];
        let out = subagents_for(&state, "c");
        assert_eq!(out[0].status, SubagentPhase::Started);
    }
}

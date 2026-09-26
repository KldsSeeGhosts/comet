//! Codex-style subagent inventory: a pure selector over a chat's transcript
//! (spawn tool parts) plus the surfaces that render it — the composer agents
//! tray, the right-pane Agents panel, and the sidebar's nested child rows.
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

/// One spawned subagent, reduced to what the tray/panel/sidebar draw.
#[derive(Debug, Clone)]
pub struct SubagentSummary {
    /// The spawn tool part id (`parent_tool_use_id` for tagged traffic).
    pub id: String,
    pub title: SharedString,
    pub agent_type: Option<SharedString>,
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
    /// Settled agents with no observed finish time show nothing rather
    /// than a guessed duration.
    pub fn elapsed(&self, now: DateTime<Utc>) -> Option<String> {
        let started = self.started?;
        let end = if self.status.active() {
            now
        } else {
            self.finished?
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

/// A spawn that returns immediately and keeps running detached. Harnesses
/// send the flag as a JSON bool; tolerate a stringly `"true"` too.
fn is_background_spawn(input: Option<&serde_json::Value>) -> bool {
    match input.and_then(|i| i.get("run_in_background")) {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::String(flag)) => flag.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
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
            let background = is_background_spawn(spawn_input(call));
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
    // loaded doc still stop their elapsed clock somewhere stable. Only keys
    // this session has seen ACTIVE qualify — after a restart a terminal
    // agent's real finish time is gone, and stamping "now" would render a
    // bogus multi-minute elapsed for a seconds-long agent.
    let mut active = state.subagent_active_obs.borrow_mut();
    let mut obs = state.subagent_finished_obs.borrow_mut();
    for s in &out {
        let key = part_key(chat_id, &s.id);
        if s.status.active() {
            active.insert(key);
        } else if s.finished.is_none() && active.contains(&key) {
            obs.entry(key).or_insert_with(|| Utc::now().timestamp_millis());
        }
    }
    out
}

/// The tray's visible subset: the latest turn's subagents, plus anything
/// still live from earlier turns.
pub fn strip_visible(summaries: &[SubagentSummary]) -> Vec<SubagentSummary> {
    summaries
        .iter()
        // Earlier turns only carry agents we KNOW are live: a detached
        // `Started` spawn never reports back, so it must not pin the strip.
        .filter(|s| s.latest_turn || s.status == SubagentPhase::Running)
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
// Composer agents tray
// ---------------------------------------------------------------------------

/// The tray content row: `Agents` label, pills, `+N`, chevron (the surface's
/// own top radius and bottom tuck come from the queue-tray metrics).
pub const TRAY_ROW_HEIGHT: f32 = 32.0;
const PILL_GAP: f32 = 6.0;

/// Estimated pill width (12px glyph + ≤22ch title + mono elapsed + pads) —
/// the tray packs greedily off this estimate; `+N` covers the rest.
/// `open(chat_id, summary)` — sidebar click → select chat + open thread.
pub type OpenAgent = Rc<dyn Fn(&mut Shell, String, SubagentSummary, &mut Context<Shell>)>;
/// `open_panel(chat_id)` — sidebar `+N more` → select chat + Agents tab.
pub type OpenPanel = Rc<dyn Fn(&mut Shell, String, &mut Context<Shell>)>;

fn pill_width(title_chars: usize) -> f32 {
    8.0 * 2.0 + 12.0 + 6.0 + title_chars.min(22) as f32 * 6.6 + 6.0 + 34.0
}

/// How many leading pills fit `width` (the tray's inner width), keeping
/// room for the leading label, the `+N` overflow pill and the trailing
/// chevron.
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

/// The tray's pills that fit `width`; `+N` covers the remainder.
pub fn tray_layout(summaries: &[SubagentSummary], width: f32) -> (Vec<SubagentSummary>, usize) {
    let shown = fitting(summaries, width);
    (summaries[..shown].to_vec(), summaries.len() - shown)
}

/// The agents tray content row: `Agents {done}/{total}` label, fitted
/// pills, `+N` overflow, and the trailing chevron that toggles the Agents
/// panel. Rendered INSIDE the queue-tray surface by the composer itself
/// (its stack order is the composer's), so it is one continuous surface
/// with the queue tray and the pill on every route.
///
/// Pills lose their own borders inside the tray (the tray frames them);
/// fills stay on `wash`. `seen` = subagent keys whose thread the user
/// already opened. Fitting uses the tray's inner width.
#[allow(clippy::too_many_arguments)] // render fn; params are the tray's props
pub fn agents_tray_row(
    chat_id: &str,
    summaries: &[SubagentSummary],
    inner_width: f32,
    panel_open: bool,
    seen: &HashSet<String>,
    now: DateTime<Utc>,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut Context<crate::composer::Composer>,
) -> AnyElement {
    let done = summaries.iter().filter(|s| !s.status.active()).count();
    let (shown, more) = tray_layout(summaries, inner_width);
    let mut row = div()
        .id("subagent-agents-tray")
        .h(px(TRAY_ROW_HEIGHT))
        .w_full()
        .pl(px(12.0))
        .pr(px(6.0))
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
                .bg(crate::theme::wash(0.06))
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::wash(0.10)))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(crate::composer::ComposerEvent::OpenSubagentSummary {
                        chat_id: chat.clone(),
                        summary: summary.clone(),
                    });
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
        row = row.child(
            div()
                .id("agent-pill-more")
                .h(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .px(px(8.0))
                .rounded(px(12.0))
                .bg(crate::theme::wash(0.06))
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::wash(0.10)))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(crate::composer::ComposerEvent::ToggleAgentsPanel);
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
    row = row.child(div().flex_1().min_w_0());
    row.child(
        div()
            .id("agents-panel-toggle")
            .debug_selector(|| "agents-panel-toggle".into())
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::wash(0.08)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(crate::composer::ComposerEvent::ToggleAgentsPanel);
            }))
            .child(
                icons::icon(if panel_open {
                    icons::ALT_ARROW_DOWN
                } else {
                    icons::ALT_ARROW_RIGHT
                })
                .size(px(12.0))
                .text_color(theme.text_muted),
            ),
    )
    .into_any_element()
}

// ---------------------------------------------------------------------------
// Sidebar nested rows
// ---------------------------------------------------------------------------

pub const SIDEBAR_CHILD_HEIGHT: f32 = 22.0;
pub const SIDEBAR_CHILD_MAX: usize = 3;
/// Breathing room between a card's line 3 and its first child row.
pub const SIDEBAR_CHILD_GAP: f32 = 2.0;
/// Bottom inset when the card carries children (the same 10px a bare card
/// gets from `justify_center`; line 3's own row keeps its height).
pub const SIDEBAR_CHILD_PAD_BOTTOM: f32 = 4.0;

/// Up to `SIDEBAR_CHILD_MAX` running subagents as EXTRA LINES inside the
/// chat card (after line 3), sharing its wash and radius. Each row: 12px
/// status glyph at the card's text-start x, 6px gap, 12px `text_muted`
/// title truncating, mono 11px `text_faint` elapsed flush to the card's
/// right edge. No tree stubs, no hairlines. `+N more` opens the Agents
/// panel. Children stop click propagation (they open the thread, not the
/// card's plain select).
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
    let running: Vec<&SubagentSummary> = summaries
        .iter()
        .filter(|s| s.status == SubagentPhase::Running)
        .collect();
    let more = running.len().saturating_sub(SIDEBAR_CHILD_MAX);
    let mut col = div()
        .w_full()
        .flex()
        .flex_col()
        .pt(px(SIDEBAR_CHILD_GAP))
        .pb(px(SIDEBAR_CHILD_PAD_BOTTOM));
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
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    open(this, chat.clone(), summary.clone(), cx);
                }))
                .child(
                    div()
                        .size(px(12.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(status_glyph(
                            SharedString::from(format!("sub-glyph-{}", s.id)),
                            s.status,
                            true,
                            theme,
                            view,
                            cx,
                        )),
                )
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
                // No glyph: indent to the child rows' title start.
                .pl(px(12.0 + 6.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    open_panel(this, parent.clone(), cx);
                }))
                .child(SharedString::from(format!("+{more} more"))),
        );
    }
    col.into_any_element()
}

// ---------------------------------------------------------------------------
// Agents panel (RightSurface::Agents)
// ---------------------------------------------------------------------------

const AGENTS_ROW_HEIGHT: f32 = 44.0;
const AGENTS_ROW_HEIGHT_BARE: f32 = 32.0;
const AGENTS_SECTION_HEIGHT: f32 = 30.0;

/// One 30px "Active" / "Done · N" header, 11.5px text_faint like the
/// sidebar's own section labels.
fn agents_section(label: String, theme: &Theme) -> AnyElement {
    div()
        .h(px(AGENTS_SECTION_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_SM))
        .font_weight(FontWeight::MEDIUM)
        .text_size(crate::typography::ui_rems(11.5))
        .text_color(theme.text_faint)
        .child(SharedString::from(label))
        .into_any_element()
}

/// The right-pane inventory: every subagent the selector sees for `chat_id`
/// (not just the strip-visible subset), Active first, each row clickable to
/// the child thread.
#[allow(clippy::too_many_arguments)]
pub fn agents_panel_body(
    chat_id: &str,
    summaries: &[SubagentSummary],
    seen: &HashSet<String>,
    now: DateTime<Utc>,
    theme: &Theme,
    view: gpui::EntityId,
    open: OpenAgent,
    cx: &Context<Shell>,
) -> AnyElement {
    if summaries.is_empty() {
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_faint)
            .child("No agents yet")
            .into_any_element();
    }
    let active: Vec<&SubagentSummary> = summaries.iter().filter(|s| s.status.active()).collect();
    let done: Vec<&SubagentSummary> = summaries.iter().filter(|s| !s.status.active()).collect();
    let row = |s: &SubagentSummary| -> AnyElement {
        let summary = s.clone();
        let chat = chat_id.to_string();
        let open = open.clone();
        let is_seen = seen.contains(&s.id)
            || s.doc_ref
                .as_ref()
                .is_some_and(|d| seen.contains(d.as_str()));
        // Right meta: `agent_type · model` (mono 11px) - either part may
        // be absent, and the dot is omitted when only one exists.
        let meta = [s.agent_type.as_deref(), s.model.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" \u{00b7} ");
        let summary_line = s.summary.clone();
        // No summary and no meta: the row collapses to the title-only
        // 32px (a line-2 slot with nothing on either side).
        let bare = summary_line.is_none() && meta.is_empty();
        let elapsed = s.elapsed(now);
        div()
            .id(SharedString::from(format!("agents-row-{}", s.id)))
            .h(px(if bare {
                AGENTS_ROW_HEIGHT_BARE
            } else {
                AGENTS_ROW_HEIGHT
            }))
            .w_full()
            .flex()
            .flex_col()
            .justify_center()
            .px(px(Theme::SPACE_SM))
            .cursor_pointer()
            .hover(|el| el.bg(crate::theme::wash(0.06)))
            .on_click(cx.listener(move |this, _, _, cx| {
                open(this, chat.clone(), summary.clone(), cx);
            }))
            // Line 1 (18px): 12px glyph + 8px + 13px title truncating,
            // then the mono elapsed right-aligned on the title's baseline.
            .child(
                div()
                    .w_full()
                    .h(px(18.0))
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(8.0))
                    .child(
                        div()
                            .size(px(12.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(status_glyph(
                                SharedString::from(format!("agents-glyph-{}", s.id)),
                                s.status,
                                is_seen,
                                theme,
                                view,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text)
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
            )
            // Line 2 (16px) starts at the title's x: one-line summary
            // truncating, `agent_type · model` right-aligned. Either side
            // may be absent (empty rows collapse above).
            .when(!bare, |row| {
                row.child(
                    div()
                        .w_full()
                        .h(px(16.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .pl(px(12.0 + 8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(summary_line.unwrap_or_default()),
                        )
                        .when(!meta.is_empty(), |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(meta)),
                            )
                        }),
                )
            })
            .into_any_element()
    };
    let mut children: Vec<AnyElement> = Vec::new();
    if !active.is_empty() {
        children.push(agents_section("Active".into(), theme));
        children.extend(active.iter().map(|s| row(s)));
    }
    if !done.is_empty() {
        children.push(agents_section(
            format!("Done \u{00b7} {}", done.len()),
            theme,
        ));
        children.extend(done.iter().map(|s| row(s)));
    }
    div()
        .id("agents-panel-rows")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .pb(px(Theme::SPACE_SM))
        .children(children)
        .into_any_element()
}

impl crate::composer::Composer {
    /// The agents tray: a queue-tray surface over this composer's chat, or
    /// `None` when nothing qualifies (no chat, or no visible subagents).
    /// `tucked` (queue tray rendered below) drops the tray's own radius:
    /// the queue tray's top edge becomes the shared seam.
    pub(crate) fn render_agents_tray(
        &mut self,
        tucked: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chat_id = self
            .target
            .chat_id(self.state.read(cx))
            .map(str::to_owned)?;
        let summaries = strip_visible(&subagents_for(self.state.read(cx), &chat_id));
        if summaries.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        // Fitting budget: the tray's inner width = the measured composer
        // column minus the tray inset on both sides minus the row pads.
        let inner_width = self
            .last_available_width()
            .unwrap_or(crate::composer::COMPOSER_MAX_WIDTH)
            - 2.0 * crate::composer::QUEUE_SIDE_INSET
            - 12.0
            - 6.0;
        let panel_open = self.agents_panel_open;
        let seen = std::mem::take(&mut self.subagent_seen);
        let row = agents_tray_row(
            &chat_id,
            &summaries,
            inner_width,
            panel_open,
            &seen,
            Utc::now(),
            &theme,
            cx.entity_id(),
            cx,
        );
        self.subagent_seen = seen;
        let surface = crate::queue::queue_panel_surface(&theme)
            .when(tucked, |el| el.rounded_bl(px(0.0)).rounded_br(px(0.0)))
            .child(row);
        Some(
            crate::frost::frosted(crate::queue::PANEL_RADIUS, crate::frost::MENU_BLUR, surface)
                .into_any_element(),
        )
    }
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
                input: Some(serde_json::json!({"run_in_background": true})),
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

    #[test]
    fn finished_observation_requires_an_active_observation_this_session() {
        let mut state = AppState::new();
        state.selected_chat = Some("c".into());
        // A settled agent first observed after an app restart (no doc, no
        // output): never stamp a finish time and never show a guessed
        // elapsed.
        state.transcript = vec![
            entry("u", MessageRole::User, vec![]),
            entry(
                "m",
                MessageRole::Assistant,
                vec![spawn("a", true, Some(SubagentStatus::Done))],
            ),
        ];
        let out = subagents_for(&state, "c");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, SubagentPhase::Done);
        assert!(out[0].finished.is_none());
        assert!(out[0].elapsed(Utc::now()).is_none());
        assert!(state.subagent_finished_obs.borrow().is_empty());

        // The same agent seen Running first, then Done: the transition
        // stamps a finish observation and elapsed freezes.
        let mut live = entry(
            "m",
            MessageRole::Assistant,
            vec![spawn("a", false, Some(SubagentStatus::Running))],
        );
        live.status = Some(MessageStatus::Streaming);
        state.transcript = vec![entry("u", MessageRole::User, vec![]), live];
        let out = subagents_for(&state, "c");
        assert_eq!(out[0].status, SubagentPhase::Running);

        state.transcript = vec![
            entry("u", MessageRole::User, vec![]),
            entry(
                "m",
                MessageRole::Assistant,
                vec![spawn("a", true, Some(SubagentStatus::Done))],
            ),
        ];
        // The terminal frame stamps the observation; the next read returns it.
        subagents_for(&state, "c");
        let out = subagents_for(&state, "c");
        assert_eq!(out[0].status, SubagentPhase::Done);
        assert!(out[0].finished.is_some());
        assert!(out[0].elapsed(Utc::now()).is_some());
    }
}

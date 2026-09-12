//! The terminal panel: session-scoped tabs over engine PTYs.
//!
//! Feature-inventory §1.10: tabs are per selected chat and restored on return
//! (emulators — and their server-side PTYs — survive navigation; detach is not
//! close). Tab bar supports pointer drag-reorder with 150 ms sliding
//! transforms, middle-click close, and a "+" new-tab button; Cmd/Ctrl+J
//! toggles the panel (the shell owns the height animation + persistence).
//!
//! Data path per tab: `OpenTerminal` → `SubscribeTerminal` stream; Data frames
//! (base64) feed the [`Emulator`]; query responses write back; the stream
//! reconnects with exponential backoff resuming from `afterSeq`; Exit appends
//! the "[process exited N]" line and stops. Keyboard bytes coalesce for 12 ms
//! before `WriteTerminal`; viewport-driven resizes debounce 80 ms before
//! `ResizeTerminal` (the emulator resizes immediately).

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use base64::Engine as _;
use futures::{FutureExt, channel::oneshot, future::Shared};
use gpui::{
    App, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render,
    ScrollDelta, SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use zeron_proto::{TerminalEvent, TerminalSession};
use zeron_rpc::methods;

use crate::motion::{self, AnimationExt as _, TAB_SLIDE};
use crate::settings::{TERMINAL_MAX_VH, TERMINAL_MIN_HEIGHT};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

use super::emulator::{CellSnapshot, CursorSnapshot, Emulator, GridPoint, SelectionType, Side};
use super::view::{
    COALESCE_MS, InputCoalescer, RESIZE_DEBOUNCE_MS, SELECTION_DRAG_THRESHOLD, TerminalElement,
    cell_at, keydown_bytes, paste_bytes, terminal_panel_bg,
};

/// Fixed tab width — drag-reorder math stays analytic.
pub const TAB_WIDTH: f32 = 118.0;
pub const TAB_BAR_HEIGHT: f32 = 40.0;
const SELECTION_SCROLL_TICK_MS: u64 = 24;
const SCROLLBAR_TRACK_INSET: f32 = 4.0;
const SCROLLBAR_HIT_WIDTH: f32 = 10.0;
const SCROLLBAR_THUMB_WIDTH: f32 = 3.0;
const SCROLLBAR_HOVER_THUMB_WIDTH: f32 = 4.5;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

fn wheel_mouse_bytes(button: u8, col: usize, row: usize, sgr: bool, utf8: bool) -> Option<Vec<u8>> {
    if sgr {
        return Some(format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes());
    }
    // Legacy mouse coordinates cannot represent cells past 223. Dropping the
    // event matches xterm; clamping would send it to an unrelated widget.
    let max = if utf8 { 2015 } else { 223 };
    if col >= max || row >= max {
        return None;
    }
    let mut bytes = vec![0x1b, b'[', b'M', button + 32];
    for coordinate in [col, row] {
        if utf8 {
            let mut encoded = [0; 4];
            bytes.extend_from_slice(char::from_u32(coordinate as u32 + 33)?.encode_utf8(&mut encoded).as_bytes());
        } else {
            bytes.push(coordinate as u8 + 33);
        }
    }
    Some(bytes)
}

actions!(terminal, [ToggleTerminal]);

/// Bind the terminal keymap (global): Cmd+J on macOS, Ctrl+J elsewhere.
pub fn init(cx: &mut App) {
    let toggle = if cfg!(target_os = "macos") {
        "cmd-j"
    } else {
        "ctrl-j"
    };
    cx.bind_keys([KeyBinding::new(toggle, ToggleTerminal, None)]);
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Panel height clamp: 160 px … 55 % of the viewport (§1.10).
pub fn clamp_terminal_height(height: f32, viewport_h: f32) -> f32 {
    let max = (viewport_h * TERMINAL_MAX_VH).max(TERMINAL_MIN_HEIGHT);
    if height.is_finite() {
        height.clamp(TERMINAL_MIN_HEIGHT, max)
    } else {
        TERMINAL_MIN_HEIGHT
    }
}

/// Reconnect backoff: 500 ms doubling to an 8 s ceiling.
pub fn backoff_ms(attempt: u32) -> u64 {
    (500u64 << attempt.min(4)).min(8_000)
}

/// Move a tab from `from` to `to` (indices into the same vec).
pub fn reorder_tabs<T>(tabs: &mut Vec<T>, from: usize, to: usize) {
    if from >= tabs.len() || to >= tabs.len() || from == to {
        return;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
}

/// Where a drag hovering at `rel_x` inside the tab strip would land.
pub fn drop_index(rel_x: f32, tab_w: f32, count: usize) -> usize {
    if count == 0 || tab_w <= 0.0 {
        return 0;
    }
    ((rel_x / tab_w).floor().max(0.0) as usize).min(count - 1)
}

/// Sliding transform (in tab-width units) for tab `ix` while `from` is dragged
/// over `over`: tabs between the two shift one slot toward the vacated gap.
pub fn slide_offset(ix: usize, from: usize, over: usize) -> f32 {
    if from < over && ix > from && ix <= over {
        -1.0
    } else if over < from && ix >= over && ix < from {
        1.0
    } else {
        0.0
    }
}

/// Active index after a reorder commit.
pub fn active_after_reorder(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// Merge the `targetDeviceId` passthrough into RPC params (no-op for chats on
/// the connected engine's own device).
fn with_target(mut params: serde_json::Value, target: &Option<String>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

/// Active index after closing `closed` (given the new, shorter length).
pub fn active_after_close(active: usize, closed: usize, len_after: usize) -> usize {
    let shifted = if closed < active { active - 1 } else { active };
    if len_after == 0 {
        0
    } else {
        shifted.min(len_after - 1)
    }
}

/// The `[process exited N]` trailer, dimmed (§1.10).
pub fn exit_message(code: i32) -> Vec<u8> {
    format!("\r\n\x1b[90m[process exited {code}]\x1b[0m\r\n").into_bytes()
}

/// Tab title from the session's shell path ("/bin/zsh" → "zsh").
pub fn shell_title(shell: &str) -> String {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell).trim();
    if name.is_empty() {
        "terminal".to_string()
    } else {
        name.to_string()
    }
}

fn decode_base64(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "terminal: dropping undecodable data frame");
            Vec::new()
        })
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// A grid snapshot handed to the paint element.
pub struct GridSnapshot {
    pub lines: Vec<Vec<CellSnapshot>>,
    pub cursor: Option<CursorSnapshot>,
}

/// Where the grid landed this frame, in window coordinates.
///
/// Reported by element prepaint because that is the only place the measured
/// font metrics exist. Mouse events arrive on the wrapping div in window
/// space, so mapping a pointer to a cell needs the glyph origin and the cell
/// size the *current* frame used — a stale one puts the selection a row off
/// after a resize.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeometry {
    /// Full terminal body bounds, used by edge scrolling and the scrollbar.
    pub bounds: gpui::Bounds<Pixels>,
    /// Top-left of the first glyph (bounds origin plus padding).
    pub origin: gpui::Point<Pixels>,
    pub cell_w: f32,
    pub line_h: f32,
    pub cols: u16,
    pub rows: u16,
}

/// An in-flight left-button gesture.
///
/// A press alone does not select. It arms this, and only pointer travel past
/// [`SELECTION_DRAG_THRESHOLD`] promotes it to a real selection — otherwise the
/// click that focuses the panel would leave a one-cell selection behind
/// whenever the hand moves a pixel.
#[derive(Debug, Clone, Copy)]
struct SelectionDrag {
    /// Press position, in window space: both the threshold origin and the
    /// selection's anchor, so the selection starts where the press landed
    /// rather than where the threshold happened to trip.
    origin: gpui::Point<Pixels>,
    /// Latest pointer sample. Edge scrolling keeps using it while the pointer
    /// is stationary, updating the selection after every scrollback step.
    position: gpui::Point<Pixels>,
    armed: bool,
}

#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
    grab_offset: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollbarMetrics {
    track_top: f32,
    track_height: f32,
    thumb_top: f32,
    thumb_height: f32,
    history_lines: usize,
}

impl ScrollbarMetrics {
    fn travel(self) -> f32 {
        (self.track_height - self.thumb_height).max(0.0)
    }
}

fn scrollbar_metrics(
    bounds: gpui::Bounds<Pixels>,
    rows: usize,
    history_lines: usize,
    display_offset: usize,
) -> Option<ScrollbarMetrics> {
    if history_lines == 0 {
        return None;
    }
    let track_height = (f32::from(bounds.size.height) - SCROLLBAR_TRACK_INSET * 2.0).max(0.0);
    if track_height <= 0.0 {
        return None;
    }
    let total_lines = history_lines.saturating_add(rows).max(1);
    let thumb_height = (track_height * rows as f32 / total_lines as f32)
        .max(SCROLLBAR_MIN_THUMB)
        .min(track_height);
    let travel = (track_height - thumb_height).max(0.0);
    let offset = display_offset.min(history_lines);
    let progress_from_top = 1.0 - offset as f32 / history_lines as f32;
    Some(ScrollbarMetrics {
        track_top: f32::from(bounds.top()) + SCROLLBAR_TRACK_INSET,
        track_height,
        thumb_top: travel * progress_from_top,
        thumb_height,
        history_lines,
    })
}

/// Terminal scroll direction for a selection near the grid edge.
///
/// Alacritty uses positive deltas for history (up) and negative deltas for the
/// live bottom. Speed is line-based because the terminal cannot expose partial
/// rows without breaking its fixed grid.
fn selection_scroll_lines(geometry: GridGeometry, position: gpui::Point<Pixels>) -> i32 {
    let grid_height = geometry.line_h * geometry.rows as f32;
    if grid_height <= 0.0 {
        return 0;
    }
    let edge = geometry.line_h.min(grid_height / 3.0);
    let y = f32::from(position.y);
    let top = f32::from(geometry.origin.y);
    let bottom = top + grid_height;
    let speed = |penetration: f32| {
        let t = (penetration / edge).clamp(0.0, 1.0);
        (1.0 + 2.0 * t * t).round() as i32
    };
    if y < top + edge {
        speed(top + edge - y)
    } else if y > bottom - edge {
        -speed(y - (bottom - edge))
    } else {
        0
    }
}

struct TerminalTab {
    key: u64,
    title: SharedString,
    terminal_id: Option<String>,
    emulator: Emulator,
    /// The emulator hides cursor coordinates along with the cursor shape.
    /// Keep the last visible position as an IME anchor while it is hidden.
    input_cursor: CursorSnapshot,
    exited: Option<i32>,
    last_seq: u64,
    coalescer: InputCoalescer,
    wheel_remainder: f32,
    flush_task: Option<Task<()>>,
    resize_task: Option<Task<()>>,
    /// Open + subscribe/reconnect lifecycle; dropping it cancels the stream.
    _run: Option<Task<()>>,
}

impl TerminalTab {
    fn take_input(&mut self) -> Option<(String, Vec<u8>)> {
        if self.exited.is_some() || self.coalescer.is_empty() {
            return None;
        }
        // Leave the bytes queued until the open RPC supplies the PTY id.
        let id = self.terminal_id.clone()?;
        Some((id, self.coalescer.take()))
    }

    fn stop_input(&mut self) {
        self.coalescer.take();
        self.flush_task = None;
        self.resize_task = None;
    }
}

#[derive(Default)]
struct ChatTabs {
    tabs: Vec<TerminalTab>,
    active: usize,
}

/// Drag-reorder state; `epoch` keys the 150 ms slide animation restarts.
struct DragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-tab payload (gpui drag-and-drop).
struct TabDragPayload {
    chat: String,
    from: usize,
    title: SharedString,
}

struct TabGhost {
    title: SharedString,
}

impl Render for TabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(TAB_WIDTH))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(12.0))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Subscribe to these transitions before requesting a session handoff.
/// `Closing` followed by `Idle` confirms that history hydration succeeded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionViewStatus {
    #[default]
    Idle,
    Opening,
    Ready,
    Closing,
    Failed(String),
}

impl SessionViewStatus {
    fn accepts_input(&self) -> bool {
        matches!(self, Self::Opening | Self::Ready)
    }
}

/// Only uncommitted platform text lives here. PTY output is not editable text.
#[derive(Default)]
struct TerminalComposition {
    text: String,
    selection: Range<usize>,
}

impl TerminalComposition {
    fn clear(&mut self) {
        self.text.clear();
        self.selection = 0..0;
    }

    fn replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
    ) {
        let range = range
            .map(|range| utf16_to_bytes(&self.text, range))
            .unwrap_or(0..self.text.len());
        self.text.replace_range(range.clone(), text);
        let selected = selection
            .map(|range| utf16_to_bytes(text, range))
            .unwrap_or(text.len()..text.len());
        self.selection = range.start + selected.start..range.start + selected.end;
    }
}

/// Clamp platform offsets to scalar boundaries, including split surrogate pairs.
fn utf16_to_bytes(text: &str, range: Range<usize>) -> Range<usize> {
    let offset = |target: usize, round_up: bool| {
        let mut utf16 = 0;
        for (byte, ch) in text.char_indices() {
            if target <= utf16 {
                return byte;
            }
            utf16 += ch.len_utf16();
            if target < utf16 {
                return byte + if round_up { ch.len_utf8() } else { 0 };
            }
        }
        text.len()
    };
    let start = offset(range.start, false);
    let end = if range.is_empty() {
        start
    } else {
        offset(range.end.max(range.start), true)
    };
    start..end
}

fn bytes_to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

type SessionHandoff = Shared<Task<Result<(), String>>>;

pub struct TerminalPanel {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    chats: HashMap<String, ChatTabs>,
    /// Shell-driven visibility gate: no RPC happens while closed (lazy).
    open: bool,
    /// Right-pane surface host mode: the SHELL owns the tab strip (surface
    /// tabs), so the internal bar hides, tabs are only ever created
    /// explicitly (no ensure-on-open/chat-switch), and closing the last tab
    /// must not dispatch the bottom drawer's [`ToggleTerminal`].
    embedded: bool,
    session_view: bool,
    session_chat: Option<String>,
    session_status: SessionViewStatus,
    session_open: Option<SessionHandoff>,
    session_close: Option<SessionHandoff>,
    native_activity: Option<String>,
    native_watch: Option<Task<()>>,
    /// The right pane is in its width tween. Keep painting the retained grid
    /// through the changing clip, but do not feed transient widths into the
    /// emulator: alternate-screen rows truncate rather than reflow.
    resize_suspended: bool,
    tab_seq: u64,
    drag: Option<DragState>,
    last_selected: Option<String>,
    /// Last reported grid placement; `None` until the first prepaint.
    geometry: Option<GridGeometry>,
    composition: TerminalComposition,
    /// Shaped preedit and its window-space origin, shared with IME hit testing.
    pub(super) composition_layout: Option<(gpui::Point<Pixels>, gpui::ShapedLine)>,
    /// Left-button gesture in flight, if any.
    selection_drag: Option<SelectionDrag>,
    /// One-shot timer rescheduled only while a live selection remains in an
    /// edge zone.
    selection_scroll_task: Option<Task<()>>,
    /// Active scrollbar thumb/track drag.
    scrollbar_drag: Option<ScrollbarDrag>,
    /// The terminal owns the cursor. The scrollbar is an on-demand affordance
    /// rather than a permanently painted rail beside the panel.
    terminal_hovered: bool,
    scrollbar_hovered: bool,
    _observe: Subscription,
}

impl EntityInputHandler for TerminalPanel {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if !self.accepts_ime_input(cx) {
            return None;
        }
        let range = utf16_to_bytes(&self.composition.text, range);
        *actual_range = Some(bytes_to_utf16(&self.composition.text, range.clone()));
        Some(self.composition.text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        self.accepts_ime_input(cx).then(|| gpui::UTF16Selection {
            range: bytes_to_utf16(&self.composition.text, self.composition.selection.clone()),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text(cx)
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_composition();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_ime_input(cx) {
            self.cancel_composition();
            return;
        }
        // Empty replacement cancels preedit. Never delete already-sent PTY text.
        if !text.is_empty() {
            self.composition.replace(range, text, None);
            let committed = std::mem::take(&mut self.composition.text);
            self.queue_input(committed.as_bytes(), cx);
        }
        self.cancel_composition();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_ime_input(cx) || text.is_empty() {
            self.cancel_composition();
        } else {
            self.composition.replace(range, text, selection);
            self.composition_layout = None;
            self.with_active_emulator(cx, |emulator| emulator.scroll_to_bottom());
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: gpui::Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        if !self.accepts_ime_input(cx) {
            return None;
        }
        let cursor = self.input_cursor_bounds(cx)?;
        if let Some((origin, line)) = &self.composition_layout {
            let range = utf16_to_bytes(&self.composition.text, range);
            let start = line.x_for_index(range.start);
            let end = line.x_for_index(range.end);
            return Some(gpui::Bounds::new(
                gpui::point(origin.x + start, origin.y),
                gpui::size((end - start).max(px(1.0)), cursor.size.height),
            ));
        }
        Some(cursor)
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.accepts_ime_input(cx) {
            return None;
        }
        if let Some((origin, line)) = &self.composition_layout {
            let byte = line.closest_index_for_x(point.x - origin.x);
            return Some(self.composition.text[..byte].encode_utf16().count());
        }
        Some(0)
    }

    fn set_selected_text_range(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.accepts_ime_input(cx) {
            self.composition.selection = utf16_to_bytes(&self.composition.text, range);
            cx.notify();
        }
    }

    fn text_length_utf16(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        self.accepts_ime_input(cx)
            .then(|| self.composition.text.encode_utf16().count())
    }

    fn accepts_text_input(&self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        self.accepts_ime_input(cx)
    }
}

pub(crate) struct HumanTerminalInput {
    pub chat_id: String,
    pub proof: crate::input_origin::HumanInput,
}
impl EventEmitter<HumanTerminalInput> for TerminalPanel {}
impl EventEmitter<SessionViewStatus> for TerminalPanel {}

impl gpui::Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        Self {
            state,
            focus_handle: cx.focus_handle(),
            chats: HashMap::new(),
            open: false,
            embedded: false,
            session_view: false,
            session_chat: None,
            session_status: SessionViewStatus::Idle,
            session_open: None,
            session_close: None,
            native_activity: None,
            native_watch: None,
            resize_suspended: false,
            tab_seq: 0,
            drag: None,
            last_selected: None,
            geometry: None,
            composition: TerminalComposition::default(),
            composition_layout: None,
            selection_drag: None,
            selection_scroll_task: None,
            scrollbar_drag: None,
            terminal_hovered: false,
            scrollbar_hovered: false,
            _observe: observe,
        }
    }

    /// A panel in right-pane surface-host mode (see the `embedded` field).
    pub fn new_embedded(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::new(state, cx);
        panel.embedded = true;
        panel
    }

    /// One provider CLI, bound to the selected chat without opening a process.
    /// Dropping or hiding the view parks the daemon PTY; only an explicit close
    /// releases the CLI and returns the session to the structured renderer.
    pub fn new_session_view(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::new_embedded(state, cx);
        panel.session_view = true;
        panel.session_chat = panel.state.read(cx).selected_chat.clone();
        panel
    }

    pub fn session_view_status(&self) -> &SessionViewStatus {
        &self.session_status
    }

    pub fn handoff_unavailable(&self) -> Option<&'static str> {
        match self.native_activity.as_deref() {
            Some("idle") => None,
            Some("busy" | "permission") => Some("Session is busy"),
            _ => Some("Native session state is not verified"),
        }
    }

    fn watch_native_activity(&mut self, cx: &mut Context<Self>) {
        if self.native_watch.is_some() { return; }
        let Some(chat) = self.session_chat.clone() else { return; };
        let Some(engine) = self.engine(cx) else { return; };
        let target = self.chat_target(&chat, cx);
        self.native_watch = Some(cx.spawn(async move |this, cx| {
            loop {
                let result = engine.client().call(methods::GET_SESSION_VIEW,
                    with_target(serde_json::json!({"chatId": chat}), &target)).await;
                let activity = result.ok().and_then(|view|
                    view["nativeActivity"].as_str().map(str::to_owned));
                if this.update(cx, |panel, cx| {
                    if panel.native_activity != activity {
                        panel.native_activity = activity;
                        cx.notify();
                    }
                }).is_err() { break; }
                cx.background_executor().timer(Duration::from_millis(400)).await;
            }
        }));
    }

    fn set_session_status(&mut self, status: SessionViewStatus, cx: &mut Context<Self>) {
        if self.session_status != status {
            if !status.accepts_input() {
                self.cancel_composition();
            }
            self.session_status = status.clone();
            if status == SessionViewStatus::Ready { self.watch_native_activity(cx); }
            if status == SessionViewStatus::Idle {
                self.native_watch = None;
                self.native_activity = None;
            }
            cx.emit(status);
            cx.notify();
        }
    }

    fn accepts_input(&self) -> bool {
        !self.session_view || self.session_status.accepts_input()
    }

    /// Open or reattach through the idempotent provider-session RPC. Repeated
    /// calls share the pending operation instead of creating additional PTYs.
    /// The panel retains the task even if the caller drops its returned task.
    pub fn open_session_view(&mut self, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if !self.session_view {
            return Task::ready(Err("Not a session terminal view".into()));
        }
        if self.session_status == SessionViewStatus::Closing {
            return Task::ready(Err("Session terminal is closing".into()));
        }
        if matches!(
            self.session_status,
            SessionViewStatus::Opening | SessionViewStatus::Ready
        ) && let Some(open) = self.session_open.clone()
        {
            return cx.spawn(async move |_, _| open.await);
        }
        let Some(chat) = self.selected_chat(cx) else {
            let error = "Select a chat to open its session terminal".to_string();
            self.set_session_status(SessionViewStatus::Failed(error.clone()), cx);
            return Task::ready(Err(error));
        };
        if self.engine(cx).is_none() {
            let error = "Engine is not connected".to_string();
            self.set_session_status(SessionViewStatus::Failed(error.clone()), cx);
            return Task::ready(Err(error));
        }
        self.session_chat = Some(chat.clone());
        self.open = true;
        self.session_close = None;
        // A retry reattaches through OpenSessionTerminal, never CloseTerminal.
        self.chats.remove(&chat);
        self.set_session_status(SessionViewStatus::Opening, cx);
        let (ready, receiver) = oneshot::channel();
        self.open_tab_with_ready(chat, Some(ready), cx);
        let open = cx
            .spawn(async move |_, _| {
                receiver
                    .await
                    .unwrap_or_else(|_| Err("Session terminal view detached".into()))
            })
            .shared();
        self.session_open = Some(open.clone());
        cx.spawn(async move |_, _| open.await)
    }

    /// Stop writes immediately, then release the CLI and hydrate history on
    /// the daemon. Await success, or the `Idle` event, before switching views.
    /// A failed close leaves the view attached and can be retried.
    pub fn close_session_view(&mut self, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if !self.session_view {
            return Task::ready(Err("Not a session terminal view".into()));
        }
        if self.session_status == SessionViewStatus::Closing
            && let Some(close) = self.session_close.clone()
        {
            return cx.spawn(async move |_, _| close.await);
        }
        let Some(chat) = self.session_chat.clone() else {
            self.set_session_status(SessionViewStatus::Idle, cx);
            return Task::ready(Ok(()));
        };
        let Some(engine) = self.engine(cx) else {
            let error = "Engine is not connected".to_string();
            self.set_session_status(SessionViewStatus::Failed(error.clone()), cx);
            return Task::ready(Err(error));
        };
        if self.chats.get(&chat).is_some_and(|tabs| tabs.tabs.iter().any(|tab| !tab.coalescer.is_empty())) {
            return Task::ready(Err("Wait for buffered terminal input before switching views".into()));
        }
        let target = self.chat_target(&chat, cx);
        let opening = self.session_open.clone();
        self.set_session_status(SessionViewStatus::Closing, cx);
        if let Some(tabs) = self.chats.get_mut(&chat) {
            for tab in &mut tabs.tabs {
                tab.stop_input();
            }
        }
        let close = cx
            .spawn(async move |this, cx| {
                // Do not race close against an open that has not reached the daemon.
                if let Some(opening) = opening {
                    let _ = opening.await;
                }
                let result = engine
                    .client()
                    .call(
                        "CloseSessionTerminal",
                        with_target(serde_json::json!({ "chatId": chat }), &target),
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                // A refused busy handoff leaves the daemon's CLI alive. Restore
                // input only when the daemon confirms it still owns the session.
                let still_cli = if result.is_err() {
                    engine.client().call(methods::GET_SESSION_VIEW,
                        with_target(serde_json::json!({"chatId": chat}), &target)).await
                        .is_ok_and(|view| view["owner"] == "cli")
                } else { false };
                let _ = this.update(cx, |panel, cx| match &result {
                    Ok(()) => {
                        panel.chats.remove(&chat);
                        panel.session_chat = None;
                        panel.session_open = None;
                        panel.open = false;
                        panel.set_session_status(SessionViewStatus::Idle, cx);
                    }
                    Err(error) => {
                        if still_cli { panel.set_session_status(SessionViewStatus::Ready, cx); }
                        else { panel.set_session_status(SessionViewStatus::Failed(error.clone()), cx); }
                    }
                });
                result
            })
            .shared();
        self.session_close = Some(close.clone());
        cx.spawn(async move |_, _| close.await)
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_resize_suspended(&mut self, suspended: bool) {
        self.resize_suspended = suspended;
    }

    /// Shell toggle hook. Opening lazily creates the first tab for the
    /// selected chat (drawer mode; embedded tabs are explicit); closing
    /// keeps every session alive (detach ≠ close).
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if !open {
            self.cancel_composition();
        }
        if open && !self.embedded {
            self.ensure_tab(cx);
        }
        cx.notify();
    }

    /// A tab's display label: the live OSC 0/2 title when the running
    /// program set one (shells title themselves with the cwd / running
    /// command — the contextual name, user request), else the fixed
    /// "Terminal N".
    fn display_title(tab: &TerminalTab) -> SharedString {
        match tab.emulator.title().map(str::trim) {
            Some(title) if !title.is_empty() => title.to_string().into(),
            _ => tab.title.clone(),
        }
    }

    // ---- embedded (right-pane surface) API — the shell's tab strip drives
    // ---- these; keys are stable across reorders/closes.

    /// `(key, title, exited)` for the selected chat's tabs, in tab order.
    pub fn tab_summaries(&self, cx: &App) -> Vec<(u64, SharedString, bool)> {
        let Some(chat) = self.selected_chat(cx) else {
            return Vec::new();
        };
        self.chats
            .get(&chat)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .map(|t| (t.key, Self::display_title(t), t.exited.is_some()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Open a fresh tab for the selected chat and return its key.
    pub fn open_tab_for_selected(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        if self.session_view {
            return None;
        }
        let chat = self.selected_chat(cx)?;
        self.open_tab(chat, cx);
        Some(self.tab_seq)
    }

    /// Make `key` the rendered tab of the selected chat.
    pub fn select_tab_by_key(&mut self, key: u64, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(ix) = self
            .chats
            .get(&chat)
            .and_then(|tabs| tabs.tabs.iter().position(|t| t.key == key))
        else {
            return;
        };
        self.select_tab(&chat, ix, cx);
    }

    /// Close the selected chat's tab `key` (surface-tab ✕).
    pub fn close_tab_by_key(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        self.close_tab(&chat, key, window, cx);
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat.clone();
        let switched = selected != self.last_selected;
        if switched {
            self.last_selected = selected;
            self.drag = None;
            self.cancel_composition();
        }
        if self.open && !self.embedded {
            // Returning to a chat with tabs restores them; a fresh chat (or an
            // engine that only just finished booting) gets its first tab —
            // ensure_tab is idempotent, so calling on every state change is safe.
            // Embedded: surface tabs are explicit — a chat switch just shows
            // that chat's own tabs (or the shell's surface picker).
            self.ensure_tab(cx);
        }
        if switched {
            cx.notify();
        }
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// The chat's host device when it differs from the connected engine's own —
    /// the PTY lives on the chat's device (feature-inventory §2.1 "terminals
    /// live on the chat's host device"), so every terminal RPC for a remote
    /// chat needs the `targetDeviceId` passthrough. Without it the local
    /// engine checks the chat's cwd against its OWN filesystem and fails with
    /// "Session working directory is unavailable" (user report).
    fn chat_target(&self, chat: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.chats.iter().find(|c| c.id == chat)?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    fn selected_chat(&self, cx: &App) -> Option<String> {
        if self.session_view && self.session_chat.is_some() {
            return self.session_chat.clone();
        }
        self.state.read(cx).selected_chat.clone()
    }

    fn ensure_tab(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        if self.chats.get(&chat).is_none_or(|c| c.tabs.is_empty()) {
            self.open_tab(chat, cx);
        }
    }

    fn tab_mut(&mut self, chat: &str, key: u64) -> Option<&mut TerminalTab> {
        self.chats
            .get_mut(chat)?
            .tabs
            .iter_mut()
            .find(|t| t.key == key)
    }

    fn active_tab(&self, cx: &App) -> Option<&TerminalTab> {
        let chat = self.selected_chat(cx)?;
        let tabs = self.chats.get(&chat)?;
        tabs.tabs.get(tabs.active)
    }

    // ---- open / stream lifecycle ----

    fn open_tab(&mut self, chat: String, cx: &mut Context<Self>) {
        if self.session_view {
            return;
        }
        self.open_tab_with_ready(chat, None, cx);
    }

    fn open_tab_with_ready(
        &mut self,
        chat: String,
        ready: Option<oneshot::Sender<Result<(), String>>>,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.cancel_composition();
        self.tab_seq += 1;
        let key = self.tab_seq;
        let entry = self.chats.entry(chat.clone()).or_default();
        let tab_no = entry.tabs.len() + 1;
        entry.tabs.push(TerminalTab {
            key,
            title: format!("Terminal {tab_no}").into(),
            terminal_id: None,
            emulator: Emulator::new(80, 24),
            input_cursor: CursorSnapshot { row: 0, col: 0 },
            exited: None,
            last_seq: 0,
            coalescer: InputCoalescer::default(),
            wheel_remainder: 0.0,
            flush_task: None,
            resize_task: None,
            _run: None,
        });
        entry.active = entry.tabs.len() - 1;

        let target = self.chat_target(&chat, cx);
        let run = Self::spawn_session(chat.clone(), key, engine, target, ready, cx);
        if let Some(tab) = self.tab_mut(&chat, key) {
            tab._run = Some(run);
        }
        cx.notify();
    }

    /// OpenTerminal, then pump SubscribeTerminal with reconnect backoff.
    fn spawn_session(
        chat: String,
        key: u64,
        engine: EngineHandle,
        target: Option<String>,
        ready: Option<oneshot::Sender<Result<(), String>>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let session_view = ready.is_some();
        cx.spawn(async move |this, cx| {
            let (cols, rows) = this
                .update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.emulator.cols() as u16, t.emulator.rows() as u16))
                        .unwrap_or((80, 24))
                })
                .unwrap_or((80, 24));

            let opened = engine
                .client()
                .call_as::<TerminalSession>(
                    if session_view { "OpenSessionTerminal" } else { methods::OPEN_TERMINAL },
                    with_target(
                        serde_json::json!({ "chatId": chat, "cols": cols, "rows": rows }),
                        &target,
                    ),
                )
                .await;
            let session = match opened {
                Ok(session) => session,
                Err(err) => {
                    tracing::warn!(error = %err, "OpenTerminal failed");
                    let _ = this.update(cx, |panel, cx| {
                        if let Some(tab) = panel.tab_mut(&chat, key) {
                            tab.emulator.feed(
                                format!("\x1b[31mfailed to open terminal: {err}\x1b[0m\r\n")
                                    .as_bytes(),
                            );
                            tab.exited = Some(-1);
                            cx.notify();
                        }
                        if session_view && panel.session_status == SessionViewStatus::Opening {
                            panel.set_session_status(SessionViewStatus::Failed(err.to_string()), cx);
                        }
                    });
                    if let Some(ready) = ready {
                        let _ = ready.send(Err(err.to_string()));
                    }
                    return;
                }
            };
            let terminal_id = session.id.clone();
            let attached = this
                .update(cx, |panel, cx| {
                    if let Some(tab) = panel.tab_mut(&chat, key) {
                        tab.terminal_id = Some(terminal_id.clone());
                        if session_view && panel.session_status == SessionViewStatus::Opening {
                            panel.set_session_status(SessionViewStatus::Ready, cx);
                        }
                        panel.flush_input(chat.clone(), key, cx);
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !attached {
                if session_view {
                    // Parking or dropping a view must not release the provider CLI.
                    return;
                }
                // Tab was closed before the open completed — release the PTY.
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id }),
                            &target,
                        ),
                    )
                    .await;
                return;
            }
            // The viewport can resize while OpenSessionTerminal is pending.
            // Its debounce may fire before there is a PTY id. Send the latest
            // dimensions again on attachment instead of leaving the CLI at 80x24.
            if let Ok(Some((current_cols, current_rows))) = this.update(cx, |panel, _| {
                panel.tab_mut(&chat, key).map(|tab| (tab.emulator.cols(), tab.emulator.rows()))
            }) && (current_cols, current_rows) != (cols as usize, rows as usize) {
                let _ = engine.client().call(methods::RESIZE_TERMINAL,
                    with_target(serde_json::json!({"terminalId": terminal_id,
                        "cols": current_cols, "rows": current_rows}), &target)).await;
            }
            if let Some(ready) = ready {
                let _ = ready.send(Ok(()));
            }

            let mut attempt: u32 = 0;
            loop {
                let Ok(after_seq) = this.update(cx, |panel, _| {
                    panel.tab_mut(&chat, key).map(|t| t.last_seq)
                }) else {
                    return; // entity released
                };
                let Some(after_seq) = after_seq else { return }; // tab closed

                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::SUBSCRIBE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "afterSeq": after_seq }),
                            &target,
                        ),
                    )
                    .await;
                let mut rx = match subscribed {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, attempt, "SubscribeTerminal failed; backing off");
                        cx.background_executor()
                            .timer(Duration::from_millis(backoff_ms(attempt)))
                            .await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                };

                while let Some(value) = rx.recv().await {
                    let event: TerminalEvent = match serde_json::from_value(value) {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!(error = %err, "terminal: malformed stream frame");
                            continue;
                        }
                    };
                    attempt = 0;
                    let outcome = this.update(cx, |panel, cx| {
                        panel.apply_stream_event(&chat, key, &engine, event, cx)
                    });
                    match outcome {
                        Ok(StreamDisposition::Continue) => {}
                        Ok(StreamDisposition::Stop) => return,
                        Err(_) => return,
                    }
                }

                // Stream dropped without an exit — reconnect from afterSeq.
                let done = this
                    .update(cx, |panel, _| {
                        panel.tab_mut(&chat, key).map(|t| t.exited.is_some()).unwrap_or(true)
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(backoff_ms(attempt)))
                    .await;
                attempt = attempt.saturating_add(1);
            }
        })
    }

    fn apply_stream_event(
        &mut self,
        chat: &str,
        key: u64,
        engine: &EngineHandle,
        event: TerminalEvent,
        cx: &mut Context<Self>,
    ) -> StreamDisposition {
        let target = self.chat_target(chat, cx);
        let session_view = self.session_view;
        let accepts_input = self.accepts_input();
        let Some(tab) = self.tab_mut(chat, key) else {
            return StreamDisposition::Stop;
        };
        match event {
            TerminalEvent::Data { seq, data } => {
                tab.last_seq = seq;
                let responses = tab.emulator.feed(&decode_base64(&data));
                if accepts_input
                    && !responses.is_empty()
                    && let Some(id) = tab.terminal_id.clone()
                {
                    // Query responses (DSR etc.) go straight back, no coalescing.
                    let engine = engine.clone();
                    let data = encode_base64(&responses);
                    let chat = chat.to_string();
                    cx.spawn(async move |this, cx| {
                        if session_view
                            && !this
                                .update(cx, |panel, _| {
                                    panel.accepts_input()
                                        && panel.tab_mut(&chat, key).is_some_and(|tab| {
                                            tab.exited.is_none()
                                                && tab.terminal_id.as_ref() == Some(&id)
                                        })
                                })
                                .unwrap_or(false)
                        {
                            return;
                        }
                        let _ = engine
                            .client()
                            .call(
                                methods::WRITE_TERMINAL,
                                with_target(
                                    serde_json::json!({ "terminalId": id, "data": data }),
                                    &target,
                                ),
                            )
                            .await;
                    })
                    .detach();
                }
                cx.notify();
                StreamDisposition::Continue
            }
            TerminalEvent::Exit { seq, exit_code, .. } => {
                tab.last_seq = seq;
                tab.exited = Some(exit_code);
                tab.emulator.feed(&exit_message(exit_code));
                if session_view {
                    tab.stop_input();
                    if self.session_status != SessionViewStatus::Closing {
                        self.set_session_status(SessionViewStatus::Failed(format!(
                            "Session terminal exited with code {exit_code}. Close it to restore history."
                        )), cx);
                    }
                }
                cx.notify();
                StreamDisposition::Stop
            }
        }
    }

    // ---- input ----

    pub(super) fn cancel_composition(&mut self) {
        self.composition.clear();
        self.composition_layout = None;
    }

    fn accepts_ime_input(&self, cx: &App) -> bool {
        self.open
            && self.accepts_input()
            && self.active_tab(cx).is_some_and(|tab| tab.exited.is_none())
    }

    pub(super) fn marked_text(&self, cx: &App) -> Option<&str> {
        (self.accepts_ime_input(cx) && !self.composition.text.is_empty())
            .then_some(self.composition.text.as_str())
    }

    pub(super) fn input_cursor_bounds(&self, cx: &App) -> Option<gpui::Bounds<Pixels>> {
        let geometry = self.geometry?;
        let tab = self.active_tab(cx)?;
        let cursor = tab.emulator.cursor().unwrap_or(tab.input_cursor);
        let col = cursor.col.min(geometry.cols.saturating_sub(1) as usize);
        let row = cursor.row.min(geometry.rows.saturating_sub(1) as usize);
        Some(gpui::Bounds::new(
            gpui::point(
                geometry.origin.x + px(geometry.cell_w * col as f32),
                geometry.origin.y + px(geometry.line_h * row as f32),
            ),
            gpui::size(px(geometry.cell_w), px(geometry.line_h)),
        ))
    }

    /// Queue keyboard bytes on the active tab (12 ms coalescing window).
    fn queue_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if !self.accepts_input() {
            return;
        }
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.exited.is_some() {
            return;
        }
        // A keypress while scrolled back snaps to the live bottom (xterm).
        if tab.emulator.display_offset() > 0 {
            tab.emulator.scroll_to_bottom();
        }
        let key = tab.key;
        if tab.coalescer.push(bytes) {
            tab.flush_task = Some(Self::schedule_flush(chat.clone(), key, cx));
        }
        if self.session_view && bytes.contains(&b'\r') && let Some(proof) = crate::input_origin::capture() {
            cx.emit(HumanTerminalInput { chat_id: chat, proof });
        }
    }

    fn schedule_flush(chat: String, key: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(COALESCE_MS))
                .await;
            let _ = this.update(cx, |panel, cx| panel.flush_input(chat, key, cx));
        })
    }

    fn flush_input(&mut self, chat: String, key: u64, cx: &mut Context<Self>) {
        if !self.accepts_input() {
            return;
        }
        let session_view = self.session_view;
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.chat_target(&chat, cx);
        let Some(tab) = self.tab_mut(&chat, key) else {
            return;
        };
        let Some((id, bytes)) = tab.take_input() else {
            // The open completion flushes these bytes without polling timers.
            return;
        };
        let data = encode_base64(&bytes);
        cx.spawn(async move |this, cx| {
            // Close may have started after this write was queued.
            if session_view
                && !this
                    .update(cx, |panel, _| {
                        panel.accepts_input()
                            && panel.tab_mut(&chat, key).is_some_and(|tab| {
                                tab.exited.is_none() && tab.terminal_id.as_ref() == Some(&id)
                            })
                    })
                    .unwrap_or(false)
            {
                return;
            }
            let _ = engine
                .client()
                .call(
                    methods::WRITE_TERMINAL,
                    with_target(
                        serde_json::json!({ "terminalId": id, "data": data }),
                        &target,
                    ),
                )
                .await;
        })
        .detach();
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self
            .active_tab(cx)
            .map(|tab| tab.emulator.bracketed_paste_mode())
            .unwrap_or(false);
        let bytes = paste_bytes(&text, bracketed);
        self.queue_input(&bytes, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        // AltGr can carry Control+Alt while still producing platform text.
        if event.prefer_character_input {
            return;
        }
        // Paste: Cmd+V (macOS) / Ctrl+Shift+V.
        if ks.key == "v" && (mods.platform || (mods.control && mods.shift)) {
            self.paste_clipboard(cx);
            cx.stop_propagation();
            return;
        }
        // Copy: Cmd+C (macOS) / Ctrl+Shift+C. Only swallowed when it actually
        // copied — so Ctrl+Shift+C with nothing selected still falls through
        // to the interrupt, and plain Ctrl+C (no shift) never reaches here.
        if ks.key == "c"
            && (mods.platform || (mods.control && mods.shift))
            && self.copy_selection(cx)
        {
            cx.stop_propagation();
            return;
        }
        // Main-screen history belongs to the emulator. Fullscreen applications
        // keep their own PageUp/PageDown bindings and receive them unchanged.
        if mods.shift && !mods.control && !mods.alt && !mods.platform
            && self.active_tab(cx).is_some_and(|tab| !tab.emulator.alternate_screen())
            && matches!(ks.key.as_str(), "pageup" | "pagedown")
        {
            let rows = self.active_tab(cx).map_or(1, |tab| tab.emulator.rows()) as i32;
            self.scroll_active(if ks.key == "pageup" { rows } else { -rows }, cx);
            cx.stop_propagation();
            return;
        }
        let app_cursor = self
            .active_tab(cx)
            .map(|tab| tab.emulator.app_cursor_mode())
            .unwrap_or(false);
        // Printable text, including dead keys, is committed by the platform handler.
        // During preedit the IME owns navigation, backspace and confirmation too.
        if !self.composition.text.is_empty() && !mods.control && !mods.alt && !mods.platform {
            return;
        }
        if let Some(bytes) = keydown_bytes(&ks.key, ks.key_char.as_deref(), mods, app_cursor) {
            self.queue_input(&bytes, cx);
            cx.stop_propagation();
        }
    }

    // ---- grid metrics / element hooks ----

    /// Called from element prepaint with the frame's grid placement. Resizes
    /// the emulator immediately; the `ResizeTerminal` RPC debounces 80 ms.
    pub fn on_grid_metrics(&mut self, geometry: GridGeometry, cx: &mut Context<Self>) {
        // Stash unconditionally, before the early returns below: pointer
        // mapping needs the placement even on frames where nothing resized,
        // which is almost all of them.
        self.geometry = Some(geometry);
        if self.resize_suspended || !self.accepts_input() {
            return;
        }
        let (cols, rows) = (geometry.cols, geometry.rows);
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.emulator.cols() == cols as usize && tab.emulator.rows() == rows as usize {
            return;
        }
        tab.emulator.resize(cols, rows);
        let key = tab.key;
        let engine = self.engine(cx);
        let target = self.chat_target(&chat, cx);
        if let (Some(engine), Some(tab)) = (engine, self.tab_mut(&chat, key)) {
            let id = tab.terminal_id.clone();
            tab.resize_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_DEBOUNCE_MS))
                    .await;
                // Re-read the *current* size — later prepaints may have
                // resized again inside the debounce window.
                let Ok(current) = this.update(cx, |panel, _| {
                    if !panel.accepts_input() {
                        return None;
                    }
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.terminal_id.clone(), t.emulator.cols(), t.emulator.rows()))
                }) else {
                    return;
                };
                let Some((stored_id, cols, rows)) = current else {
                    return;
                };
                let Some(id) = stored_id.or(id) else { return };
                let _ = engine
                    .client()
                    .call(
                        methods::RESIZE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": id, "cols": cols, "rows": rows }),
                            &target,
                        ),
                    )
                    .await;
            }));
        }
        // Deliberately no cx.notify(): this runs during prepaint of the
        // current frame, which already paints the resized grid.
    }

    /// Snapshot for the paint element.
    pub fn active_grid_snapshot(&mut self, cx: &App) -> Option<GridSnapshot> {
        let chat = self.selected_chat(cx)?;
        let tabs = self.chats.get_mut(&chat)?;
        let tab = tabs.tabs.get_mut(tabs.active)?;
        let cursor = tab.emulator.cursor();
        if let Some(cursor) = cursor {
            tab.input_cursor = cursor;
        }
        Some(GridSnapshot {
            lines: tab.emulator.lines(),
            cursor,
        })
    }

    // ---- selection ----

    /// Run `f` against the active tab's emulator.
    fn with_active_emulator<R>(
        &mut self,
        cx: &App,
        f: impl FnOnce(&mut Emulator) -> R,
    ) -> Option<R> {
        let chat = self.selected_chat(cx)?;
        let tabs = self.chats.get_mut(&chat)?;
        let active = tabs.active;
        tabs.tabs.get_mut(active).map(|tab| f(&mut tab.emulator))
    }

    /// Window position → grid point, using this frame's placement. `None`
    /// before the first prepaint, or when no tab is active.
    fn grid_point_at(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &App,
    ) -> Option<(GridPoint, Side)> {
        let geometry = self.geometry?;
        let hit = cell_at(
            f32::from(position.x - geometry.origin.x),
            f32::from(position.y - geometry.origin.y),
            geometry.cell_w,
            geometry.line_h,
            geometry.cols as usize,
            geometry.rows as usize,
        );
        let point = self.with_active_emulator(cx, |emu| emu.grid_point(hit.row, hit.col))?;
        Some((point, hit.side))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some((point, side)) = self.grid_point_at(event.position, cx) else {
            return;
        };
        // Click count picks the granularity, the same mapping every terminal
        // uses: drag, word, line.
        let ty = match event.click_count {
            0 => return,
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        let shift = event.modifiers.shift;
        if ty == SelectionType::Simple {
            // Shift+click extends an existing selection instead of replacing
            // it — the one gesture that reaches text off the bottom of a long
            // drag without redoing the whole thing.
            let extended = shift
                && self
                    .with_active_emulator(cx, |emu| {
                        let extend = emu.has_selection();
                        if extend {
                            emu.update_selection(point, side);
                        }
                        extend
                    })
                    .unwrap_or(false);
            if extended {
                self.selection_drag = Some(SelectionDrag {
                    origin: event.position,
                    position: event.position,
                    armed: true,
                });
                cx.notify();
                return;
            }
            // A plain press clears and arms; the selection itself only begins
            // once the pointer travels far enough to mean it.
            self.with_active_emulator(cx, |emu| emu.clear_selection());
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                position: event.position,
                armed: false,
            });
        } else {
            // Word and line selections are complete on the press, so they need
            // no threshold — but keep the drag live so the pointer can extend
            // them at that granularity.
            self.with_active_emulator(cx, |emu| emu.start_selection(ty, point, side));
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                position: event.position,
                armed: true,
            });
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(drag) = self.scrollbar_drag {
            if event.dragging() {
                self.scrollbar_to_pointer(event.position.y, drag.grab_offset, cx);
            } else {
                self.scrollbar_drag = None;
            }
            return;
        }
        if !event.dragging() {
            return;
        }
        let Some(mut drag) = self.selection_drag else {
            return;
        };
        drag.position = event.position;
        self.selection_drag = Some(drag);
        if !drag.armed {
            let dx = f32::from(event.position.x - drag.origin.x);
            let dy = f32::from(event.position.y - drag.origin.y);
            if dx.hypot(dy) < SELECTION_DRAG_THRESHOLD {
                return;
            }
            // Threshold tripped: anchor at the *press*, not here, so the
            // selection covers the whole gesture.
            let Some((anchor, side)) = self.grid_point_at(drag.origin, cx) else {
                return;
            };
            self.with_active_emulator(cx, |emu| {
                emu.start_selection(SelectionType::Simple, anchor, side)
            });
            self.selection_drag = Some(SelectionDrag {
                armed: true,
                ..drag
            });
        }
        let Some((point, side)) = self.grid_point_at(event.position, cx) else {
            return;
        };
        self.with_active_emulator(cx, |emu| emu.update_selection(point, side));
        cx.notify();
        self.schedule_selection_scroll(cx);
    }

    fn on_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.selection_drag = None;
        self.selection_scroll_task = None;
        self.scrollbar_drag = None;
    }

    /// Copy the selection. Returns whether anything was copied, so the caller
    /// can decide whether to swallow the keystroke.
    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self
            .with_active_emulator(cx, |emu| emu.selection_text())
            .flatten()
        else {
            return false;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        true
    }

    fn scroll_active(&mut self, delta_lines: i32, cx: &mut Context<Self>) {
        if delta_lines == 0 {
            return;
        }
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        if let Some(tab) = tabs.tabs.get_mut(active) {
            tab.emulator.scroll(delta_lines);
            cx.notify();
        }
    }

    fn on_scroll_wheel(&mut self, event: &gpui::ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_h = self.geometry.map_or(super::view::TERM_LINE_HEIGHT, |g| g.line_h);
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / line_h,
        };
        if !lines.is_finite() || lines == 0.0 { return; }
        let Some(chat) = self.selected_chat(cx) else { return; };
        let Some(tabs) = self.chats.get_mut(&chat) else { return; };
        let Some(tab) = tabs.tabs.get_mut(tabs.active) else { return; };
        // Keep sub-line trackpad deltas rather than rounding every event to zero.
        if lines.signum() != tab.wheel_remainder.signum() { tab.wheel_remainder = 0.0; }
        tab.wheel_remainder += lines;
        let step = tab.wheel_remainder.trunc().clamp(-120.0, 120.0) as i32;
        tab.wheel_remainder -= step as f32;
        if step == 0 {
            cx.stop_propagation();
            return;
        }
        let reports = tab.emulator.mouse_reporting() && !event.modifiers.shift;
        let sgr = tab.emulator.sgr_mouse();
        let utf8 = tab.emulator.utf8_mouse();
        let alternate_scroll = tab.emulator.alternate_scroll() && !event.modifiers.shift;
        let app_cursor = tab.emulator.app_cursor_mode();
        if reports {
            if let Some(g) = self.geometry {
                let hit = cell_at(f32::from(event.position.x - g.origin.x),
                    f32::from(event.position.y - g.origin.y), g.cell_w, g.line_h,
                    g.cols as usize, g.rows as usize);
                let button = if step > 0 { 64 } else { 65 }
                    | if event.modifiers.alt { 8 } else { 0 }
                    | if event.modifiers.control { 16 } else { 0 };
                if let Some(bytes) = wheel_mouse_bytes(button, hit.col, hit.row, sgr, utf8) {
                    self.queue_input(&bytes.repeat(step.unsigned_abs() as usize), cx);
                }
            }
        } else if alternate_scroll {
            let bytes: &[u8] = match (step > 0, app_cursor) {
                (true, false) => b"\x1b[A",
                (false, false) => b"\x1b[B",
                (true, true) => b"\x1bOA",
                (false, true) => b"\x1bOB",
            };
            self.queue_input(&bytes.repeat(step.unsigned_abs() as usize), cx);
        } else {
            self.scroll_active(step, cx);
        }
        cx.stop_propagation();
    }

    fn schedule_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if self.selection_scroll_task.is_some() {
            return;
        }
        let (Some(drag), Some(geometry)) = (self.selection_drag, self.geometry) else {
            return;
        };
        if !drag.armed || selection_scroll_lines(geometry, drag.position) == 0 {
            return;
        }
        self.selection_scroll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SELECTION_SCROLL_TICK_MS))
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.selection_scroll_task = None;
                panel.step_selection_scroll(cx);
            });
        }));
    }

    fn step_selection_scroll(&mut self, cx: &mut Context<Self>) {
        let (Some(drag), Some(geometry)) = (self.selection_drag, self.geometry) else {
            return;
        };
        if !drag.armed {
            return;
        }
        let lines = selection_scroll_lines(geometry, drag.position);
        if lines == 0 {
            return;
        }
        self.scroll_active(lines, cx);
        if let Some((point, side)) = self.grid_point_at(drag.position, cx) {
            self.with_active_emulator(cx, |emu| emu.update_selection(point, side));
        }
        self.schedule_selection_scroll(cx);
    }

    fn active_scrollbar_metrics(&self, cx: &App) -> Option<ScrollbarMetrics> {
        let geometry = self.geometry?;
        let tab = self.active_tab(cx)?;
        scrollbar_metrics(
            geometry.bounds,
            tab.emulator.rows(),
            tab.emulator.history_lines(),
            tab.emulator.display_offset(),
        )
    }

    fn scrollbar_to_pointer(
        &mut self,
        pointer_y: Pixels,
        grab_offset: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(metrics) = self.active_scrollbar_metrics(cx) else {
            return;
        };
        let thumb_top =
            (f32::from(pointer_y) - metrics.track_top - grab_offset).clamp(0.0, metrics.travel());
        let offset = if metrics.travel() <= 0.0 {
            0
        } else {
            ((1.0 - thumb_top / metrics.travel()) * metrics.history_lines as f32).round() as usize
        };
        self.with_active_emulator(cx, |emu| emu.scroll_to_offset(offset));
        cx.notify();
    }

    fn on_scrollbar_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(metrics) = self.active_scrollbar_metrics(cx) else {
            return;
        };
        window.focus(&self.focus_handle, cx);
        let pointer_on_track = f32::from(event.position.y) - metrics.track_top;
        let grab_offset = if (metrics.thumb_top..=metrics.thumb_top + metrics.thumb_height)
            .contains(&pointer_on_track)
        {
            pointer_on_track - metrics.thumb_top
        } else {
            metrics.thumb_height / 2.0
        };
        self.scrollbar_drag = Some(ScrollbarDrag { grab_offset });
        self.scrollbar_to_pointer(event.position.y, grab_offset, cx);
        cx.stop_propagation();
    }

    fn on_terminal_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_hovered != *hovered {
            self.terminal_hovered = *hovered;
            if !*hovered {
                self.scrollbar_hovered = false;
            }
            cx.notify();
        }
    }

    fn render_scrollbar(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.terminal_hovered {
            return None;
        }
        let metrics = self.active_scrollbar_metrics(cx)?;
        let thumb_width = if self.scrollbar_hovered {
            SCROLLBAR_HOVER_THUMB_WIDTH
        } else {
            SCROLLBAR_THUMB_WIDTH
        };
        Some(
            div()
                .id("terminal-scrollbar")
                .absolute()
                .top(px(0.0))
                .bottom(px(0.0))
                .right(px(0.0))
                .w(px(SCROLLBAR_HIT_WIDTH))
                .cursor_pointer()
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if this.scrollbar_hovered != *hovered {
                        this.scrollbar_hovered = *hovered;
                        cx.notify();
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(Self::on_scrollbar_mouse_down),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(SCROLLBAR_TRACK_INSET + metrics.thumb_top))
                        .right(px(2.0))
                        // This is an absolute child inside a fixed-width hit
                        // rail, so the hover expansion changes only paint
                        // geometry and never reflows the terminal.
                        .w(px(thumb_width))
                        .h(px(metrics.thumb_height))
                        .rounded(px(thumb_width / 2.0))
                        .bg(theme.text_faint.opacity(0.52)),
                )
                .into_any_element(),
        )
    }

    // ---- tab management ----

    fn select_tab(&mut self, chat: &str, ix: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat)
            && ix < tabs.tabs.len()
            && tabs.active != ix
        {
            tabs.active = ix;
            self.cancel_composition();
            cx.notify();
        }
    }

    fn close_tab(&mut self, chat: &str, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_view {
            self.close_session_view(cx).detach();
            return;
        }
        let engine = self.engine(cx);
        let target = self.chat_target(chat, cx);
        let Some(tabs) = self.chats.get_mut(chat) else {
            return;
        };
        let Some(ix) = tabs.tabs.iter().position(|t| t.key == key) else {
            return;
        };
        let tab = tabs.tabs.remove(ix);
        tabs.active = active_after_close(tabs.active, ix, tabs.tabs.len());
        let now_empty = tabs.tabs.is_empty();
        self.cancel_composition();
        self.drag = None;
        // Closing the LAST terminal closes the drawer too — an empty dock is
        // dead space (user request). Same path as the collapse chevron.
        // Embedded, the SHELL owns emptiness (it falls back to the surface
        // picker) — dispatching here would toggle the bottom drawer instead.
        if now_empty && self.open && !self.embedded {
            window.dispatch_action(Box::new(ToggleTerminal), cx);
        }
        if let (Some(engine), Some(id)) = (engine, tab.terminal_id.clone()) {
            cx.spawn(async move |_, _| {
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(serde_json::json!({ "terminalId": id }), &target),
                    )
                    .await;
            })
            .detach();
        }
        cx.notify();
    }

    fn commit_reorder(&mut self, chat: &str, from: usize, to: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat) {
            let active = tabs.active;
            reorder_tabs(&mut tabs.tabs, from, to);
            tabs.active = active_after_reorder(active, from, to);
        }
        self.drag = None;
        cx.notify();
    }

    fn update_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.drag = Some(DragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    // ---- render ----

    fn render_tab_bar(&mut self, chat: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::of(cx).clone();
        let tabs = self.chats.get(chat);
        let (active, count) = tabs.map(|t| (t.active, t.tabs.len())).unwrap_or((0, 0));
        let drag = self
            .drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));
        let chat_owned = chat.to_string();

        let tab_elements: Vec<_> = tabs
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .enumerate()
                    .map(|(ix, tab)| {
                        let selected = ix == active;
                        let key = tab.key;
                        // Contextual label (user request): the OSC title —
                        // the shell's own cwd/command name — wins over the
                        // fixed "Terminal N" fallback.
                        let title = Self::display_title(tab);
                        let exited = tab.exited.is_some();
                        (ix, key, title, selected, exited)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let bar_chat = chat_owned.clone();
        let drop_chat = chat_owned.clone();
        // Zeron terminal-panel.tsx: `flex h-10 items-center border-b
        // border-white/[0.07] pl-2 pr-1.5` on the #090909 panel — no separate
        // bar fill.
        div()
            .id("terminal-tab-bar")
            .h(px(TAB_BAR_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.07))
            .on_drag_move::<TabDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<TabDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != bar_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x) - f32::from(event.bounds.left());
                    let over = drop_index(rel_x, TAB_WIDTH, count);
                    this.update_drag_over(from, over, cx);
                },
            ))
            .on_drop::<TabDragPayload>(cx.listener(move |this, payload: &TabDragPayload, _, cx| {
                if payload.chat != drop_chat {
                    this.drag = None;
                    cx.notify();
                    return;
                }
                let to = this.drag.as_ref().map(|d| d.over).unwrap_or(payload.from);
                let chat = drop_chat.clone();
                this.commit_reorder(&chat, payload.from, to, cx);
            }))
            .children(
                tab_elements
                    .into_iter()
                    .map(|(ix, key, title, selected, exited)| {
                        let chat_select = chat_owned.clone();
                        let chat_close = chat_owned.clone();
                        let chat_close2 = chat_owned.clone();
                        let chat_drag = chat_owned.clone();
                        let ghost_title = title.clone();
                        // Zeron tab: `h-7 rounded-lg pl-2 pr-1 gap-1.5 text-xs`,
                        // terminal glyph + label + close; active = white/8 wash.
                        let (text_color, bg, glyph_alpha) = if selected {
                            (theme.text, crate::theme::ink(0.08), 0.8)
                        } else {
                            (
                                theme.text_muted.opacity(0.6),
                                gpui::transparent_black(),
                                0.6,
                            )
                        };
                        let close_btn = div()
                            .id(("terminal-tab-close", key))
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .when(!selected, |el| el.invisible())
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::ink(0.09)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(&chat_close2, key, window, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.8)),
                            );
                        let tab_el = div()
                            .id(("terminal-tab", key))
                            .w(px(TAB_WIDTH))
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .pl(px(8.0))
                            .pr(px(4.0))
                            .rounded(px(8.0))
                            // zeron terminal-panel.tsx tab: `transition-colors`.
                            .bg(motion::hover_blend(
                                &format!("term-tab-{key}"),
                                bg,
                                theme.element_hover,
                            ))
                            .on_hover(motion::hover_listener(format!("term-tab-{key}")))
                            .text_size(px(12.0))
                            .text_color(text_color)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_tab(&chat_select, ix, cx);
                            }))
                            // Middle-click closes (§1.10).
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(move |this, _, window, cx| {
                                    this.close_tab(&chat_close, key, window, cx);
                                }),
                            )
                            .on_drag(
                                TabDragPayload {
                                    chat: chat_drag,
                                    from: ix,
                                    title: ghost_title,
                                },
                                |payload, _point, _, cx| {
                                    let title = payload.title.clone();
                                    cx.stop_propagation();
                                    cx.new(|_| TabGhost { title })
                                },
                            )
                            .when(exited, |el| el.opacity(0.55))
                            .child(
                                crate::icons::icon(crate::icons::TERMINAL)
                                    .size(px(16.0))
                                    .text_color(text_color.opacity(glyph_alpha)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .child(close_btn);

                        // Sliding transform while a sibling is dragged over: animate
                        // 150 ms between committed offsets.
                        match drag {
                            Some((from, over, epoch, prev_over)) if ix != from => {
                                let target = slide_offset(ix, from, over) * TAB_WIDTH;
                                let start = slide_offset(ix, from, prev_over) * TAB_WIDTH;
                                div()
                                    .relative()
                                    .child(tab_el.with_animation(
                                        ("terminal-tab-slide", key | ((epoch as u64) << 32)),
                                        TAB_SLIDE.animation(),
                                        move |el, t| el.left(px(motion::lerp(start, target, t))),
                                    ))
                                    .into_any_element()
                            }
                            // Invisible spacer — the ghost carries the tab; a
                            // dimmed original overlapped the sibling that
                            // slides into the vacated slot.
                            Some((from, ..)) if ix == from => div()
                                .w(px(TAB_WIDTH))
                                .h(px(28.0))
                                .flex_none()
                                .into_any_element(),
                            _ => tab_el.into_any_element(),
                        }
                    }),
            )
            .child(
                div()
                    .id("terminal-new-tab")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    // zeron terminal-panel.tsx icon buttons: `transition-colors`.
                    .bg(motion::hover_blend(
                        "term-new-tab",
                        gpui::transparent_black(),
                        crate::theme::ink(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-new-tab"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(chat) = this.selected_chat(cx) {
                            this.open_tab(chat, cx);
                        }
                    }))
                    .child(
                        crate::icons::icon(crate::icons::PLUS)
                            .size(px(16.0))
                            .text_color(theme.text_muted.opacity(0.6)),
                    ),
            )
            // Collapse chevron pinned right (zeron "Hide terminal" ⌘J).
            .child(div().flex_1())
            .child(
                div()
                    .id("terminal-collapse")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "term-collapse",
                        gpui::transparent_black(),
                        crate::theme::ink(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-collapse"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleTerminal), cx);
                    })
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(13.0))
                            .text_color(theme.text_muted.opacity(0.55)),
                    ),
            )
    }
}

enum StreamDisposition {
    Continue,
    Stop,
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the bar.
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
        // Embedded, the RIGHT PANE's own surface shows through — a second
        // fill here stacked another shade on the pane (user report); the
        // drawer keeps its own tone.
        let panel_bg: Option<gpui::Hsla> = (!self.embedded).then(|| terminal_panel_bg(&theme));
        let Some(chat) = self.selected_chat(cx) else {
            return div()
                .size_full()
                .when_some(panel_bg, |el, bg| el.bg(bg))
                .font_family(theme.font_sans_fixed.clone())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("Select a chat to open a terminal"))
                .into_any_element();
        };
        let focused = self.focus_handle.is_focused(window);
        let scrollbar = self.render_scrollbar(&theme, cx);

        // Embedded (right-pane surface host): the shell's surface tabs
        // replace the internal bar.
        let tab_bar: Option<gpui::AnyElement> =
            (!self.embedded).then(|| self.render_tab_bar(&chat, cx).into_any_element());
        div()
            .size_full()
            .flex()
            .flex_col()
            // Terminal chrome is fixed Geist; TerminalElement measures and
            // paints its viewport independently with the technical mono role.
            .font_family(theme.font_sans_fixed.clone())
            .when_some(panel_bg, |el, bg| el.bg(bg))
            .children(tab_bar)
            .child(
                div()
                    .id("terminal-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .key_context("Terminal")
                    .track_focus(&self.focus_handle)
                    .on_hover(cx.listener(Self::on_terminal_hover))
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    // Bound on the window, not the element: a drag that ends
                    // outside the panel still has to end the gesture, or the
                    // next unrelated pointer move keeps extending a selection
                    // the user let go of.
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(TerminalElement::new(cx.entity(), focused))
                    .children(scrollbar),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_tab() -> TerminalTab {
        TerminalTab {
            key: 1,
            title: "Session".into(),
            terminal_id: None,
            emulator: Emulator::new(80, 24),
            input_cursor: CursorSnapshot { row: 0, col: 0 },
            exited: None,
            last_seq: 0,
            coalescer: InputCoalescer::default(),
            wheel_remainder: 0.0,
            flush_task: None,
            resize_task: None,
            _run: None,
        }
    }

    fn ime_window(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<TerminalPanel> {
        cx.update(|cx| cx.set_global(Theme::default()));
        cx.add_window(|window, cx| {
            let state = cx.new(|_| AppState::new());
            let mut panel = TerminalPanel::new_session_view(state, cx);
            panel.open = true;
            panel.session_chat = Some("ime".into());
            panel.session_status = SessionViewStatus::Opening;
            panel.chats.insert(
                "ime".into(),
                ChatTabs {
                    tabs: vec![TerminalTab {
                        key: 0,
                        ..pending_tab()
                    }],
                    active: 0,
                },
            );
            window.focus(&panel.focus_handle, cx);
            panel
        })
    }

    #[gpui::test]
    fn wheel_reaches_pi_through_the_rendered_terminal(cx: &mut gpui::TestAppContext) {
        let handle = ime_window(cx);
        handle.update(cx, |panel, _, _| {
            // Pi's real fullscreen startup sequence enables SGR mouse events.
            panel.tab_mut("ime", 0).unwrap().emulator.feed(
                b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h",
            );
        }).unwrap();
        cx.update_window(handle.into(), |_, window, cx| { let _ = window.draw(cx); }).unwrap();
        let position = handle.update(cx, |panel, _, _| {
            let geometry = panel.geometry.unwrap();
            geometry.origin + gpui::point(px(geometry.cell_w * 2.5), px(geometry.line_h * 3.5))
        }).unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position,
                delta: ScrollDelta::Lines(gpui::point(0.0, 2.0)),
                ..Default::default()
            }), cx);
        }).unwrap();
        handle.update(cx, |panel, _, _| {
            assert_eq!(panel.tab_mut("ime", 0).unwrap().coalescer.take(), b"\x1b[<64;3;4M\x1b[<64;3;4M");
        }).unwrap();
    }

    #[gpui::test]
    fn wheel_trackpad_history_and_alternate_screen_modes(cx: &mut gpui::TestAppContext) {
        let handle = ime_window(cx);
        handle.update(cx, |panel, window, cx| {
            panel.geometry = Some(test_geometry());
            let tab = panel.tab_mut("ime", 0).unwrap();
            tab.emulator.resize(20, 4);
            tab.emulator.feed(b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n");
            let mut event = gpui::ScrollWheelEvent {
                position: test_geometry().origin,
                delta: ScrollDelta::Pixels(gpui::point(px(0.0), px(test_geometry().line_h / 4.0))),
                ..Default::default()
            };
            for _ in 0..4 { panel.on_scroll_wheel(&event, window, cx); }
            let tab = panel.tab_mut("ime", 0).unwrap();
            assert_eq!(tab.emulator.display_offset(), 1);
            assert!(tab.coalescer.is_empty());
            tab.emulator.feed(b"\x1b[?1049h\x1b[?1h");
            event.delta = ScrollDelta::Lines(gpui::point(0.0, -2.0));
            panel.on_scroll_wheel(&event, window, cx);
            assert_eq!(panel.tab_mut("ime", 0).unwrap().coalescer.take(), b"\x1bOB\x1bOB");
            panel.tab_mut("ime", 0).unwrap().emulator.feed(b"\x1b[?1007l");
            panel.on_scroll_wheel(&event, window, cx);
            assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
            panel.tab_mut("ime", 0).unwrap().emulator.feed(b"\x1b[?1000h\x1b[?1006h");
            panel.on_scroll_wheel(&event, window, cx);
            assert_eq!(panel.tab_mut("ime", 0).unwrap().coalescer.take(), b"\x1b[<65;1;1M\x1b[<65;1;1M");
            event.modifiers.shift = true;
            panel.on_scroll_wheel(&event, window, cx);
            assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
        }).unwrap();
    }

    #[test]
    fn wheel_mouse_protocol_coordinates() {
        assert_eq!(wheel_mouse_bytes(64, 0, 0, false, false).unwrap(), b"\x1b[M`!!");
        assert!(wheel_mouse_bytes(64, 223, 0, false, false).is_none());
        assert_eq!(wheel_mouse_bytes(65, 499, 10, true, false).unwrap(), b"\x1b[<65;500;11M");
        assert_eq!(wheel_mouse_bytes(64, 223, 0, false, true).unwrap(), "\x1b[M`Ā!".as_bytes());
    }

    #[gpui::test]
    fn history_page_keys_leave_native_cli_navigation_available(cx: &mut gpui::TestAppContext) {
        let handle = ime_window(cx);
        handle.update(cx, |panel, _, _| {
            let tab = panel.tab_mut("ime", 0).unwrap();
            // Enough output to exceed the test window's measured viewport.
            tab.emulator.feed(&b"history\r\n".repeat(200));
        }).unwrap();
        cx.simulate_keystrokes(handle.into(), "shift-pageup");
        handle.update(cx, |panel, _, _| {
            let tab = panel.tab_mut("ime", 0).unwrap();
            assert!(tab.emulator.display_offset() > 0);
            assert!(tab.coalescer.is_empty());
        }).unwrap();
        cx.simulate_keystrokes(handle.into(), "pageup pagedown");
        handle.update(cx, |panel, _, _| {
            let tab = panel.tab_mut("ime", 0).unwrap();
            assert_eq!(tab.coalescer.take(), b"\x1b[5~\x1b[6~");
            assert_eq!(tab.emulator.display_offset(), 0);
            tab.emulator.feed(b"\x1b[?1049h");
        }).unwrap();
        cx.simulate_keystrokes(handle.into(), "pageup pagedown ctrl-c");
        handle.update(cx, |panel, _, _| {
            assert_eq!(panel.tab_mut("ime", 0).unwrap().coalescer.take(), b"\x1b[5~\x1b[6~\x03");
        }).unwrap();
    }

    #[gpui::test]
    fn ime_commits_unicode_once_through_painted_input_handler(cx: &mut gpui::TestAppContext) {
        let window = ime_window(cx);
        cx.simulate_keystrokes(window.into(), "é 界 😀 space shift-a ctrl-c alt-b");
        window
            .update(cx, |panel, _, cx| {
                let tab = panel.tab_mut("ime", 0).unwrap();
                assert_eq!(tab.coalescer.take(), "é界😀 A\u{3}\u{1b}b".as_bytes());
                panel.set_session_status(SessionViewStatus::Ready, cx);
            })
            .unwrap();
        cx.simulate_keystrokes(window.into(), "é");
        window
            .update(cx, |panel, _, _| {
                assert_eq!(
                    panel.tab_mut("ime", 0).unwrap().coalescer.take(),
                    "é".as_bytes()
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn ime_dead_keys_altgr_and_clipboard_keep_separate_routes(cx: &mut gpui::TestAppContext) {
        let window = ime_window(cx);
        window.update(cx, |panel, window, cx| {
            panel.replace_and_mark_text_in_range(None, "´", None, window, cx);
            panel.on_key_down(&KeyDownEvent {
                keystroke: gpui::Keystroke { key: "dead_acute".into(), key_char: None, modifiers: Default::default() },
                is_held: false,
                prefer_character_input: false,
            }, window, cx);
            assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
            panel.replace_text_in_range(None, "é", window, cx);
            panel.on_key_down(&KeyDownEvent {
                keystroke: gpui::Keystroke {
                    key: "q".into(), key_char: Some("@".into()),
                    modifiers: gpui::Modifiers { control: true, alt: true, ..Default::default() },
                },
                is_held: false,
                prefer_character_input: true,
            }, window, cx);
            panel.replace_text_in_range(None, "@", window, cx);
            assert_eq!(panel.tab_mut("ime", 0).unwrap().coalescer.take(), "é@".as_bytes());
            panel.tab_mut("ime", 0).unwrap().emulator.feed(b"\x1b[?2004h");
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("貼付".into()));
        }).unwrap();
        cx.simulate_keystrokes(window.into(), "ctrl-shift-v");
        window.update(cx, |panel, _, _| {
            assert_eq!(panel.tab_mut("ime", 0).unwrap().coalescer.take(), "\x1b[200~貼付\x1b[201~".as_bytes());
        }).unwrap();
    }

    #[gpui::test]
    fn ime_preedit_ranges_commit_and_cancellation(cx: &mut gpui::TestAppContext) {
        let window = ime_window(cx);
        window
            .update(cx, |panel, window, cx| {
                panel.replace_and_mark_text_in_range(None, "a😀é", Some(1..3), window, cx);
                assert_eq!(panel.marked_text_range(window, cx), Some(0..4));
                assert_eq!(
                    panel.selected_text_range(false, window, cx).unwrap().range,
                    1..3
                );
                let mut actual = None;
                assert_eq!(
                    panel.text_for_range(2..3, &mut actual, window, cx),
                    Some("😀".into())
                );
                assert_eq!(actual, Some(1..3));
                assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
                panel.replace_and_mark_text_in_range(Some(1..3), "界", Some(0..1), window, cx);
                assert_eq!(panel.composition.text, "a界é");
                assert_eq!(
                    panel.selected_text_range(false, window, cx).unwrap().range,
                    1..2
                );
                panel.replace_text_in_range(None, "確定😀", window, cx);
                assert_eq!(
                    panel.tab_mut("ime", 0).unwrap().coalescer.take(),
                    "確定😀".as_bytes()
                );
                assert_eq!(panel.marked_text_range(window, cx), None);
                assert_eq!(
                    panel.selected_text_range(false, window, cx).unwrap().range,
                    0..0
                );
                panel.replace_and_mark_text_in_range(None, "cancel", None, window, cx);
                panel.unmark_text(window, cx);
                panel.replace_and_mark_text_in_range(None, "cancel", None, window, cx);
                panel.replace_text_in_range(None, "", window, cx);
                panel.replace_and_mark_text_in_range(None, "cancel", None, window, cx);
                panel.replace_and_mark_text_in_range(None, "", None, window, cx);
                assert!(panel.composition.text.is_empty());
                assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn ime_closing_and_failed_sessions_reject_all_input(cx: &mut gpui::TestAppContext) {
        let window = ime_window(cx);
        window
            .update(cx, |panel, window, cx| {
                panel.replace_and_mark_text_in_range(None, "preedit", None, window, cx);
                for status in [
                    SessionViewStatus::Closing,
                    SessionViewStatus::Failed("failed".into()),
                ] {
                    panel.set_session_status(status, cx);
                    assert!(!panel.accepts_text_input(window, cx));
                    assert_eq!(panel.marked_text_range(window, cx), None);
                    panel.replace_and_mark_text_in_range(None, "blocked", None, window, cx);
                    panel.replace_text_in_range(None, "blocked", window, cx);
                    panel.queue_input(b"blocked", cx);
                    assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
                    assert!(panel.composition.text.is_empty());
                }
            })
            .unwrap();
    }

    #[gpui::test]
    fn ime_candidate_bounds_follow_terminal_cursor_and_preedit(cx: &mut gpui::TestAppContext) {
        let window = ime_window(cx);
        window
            .update(cx, |panel, window, cx| {
                panel.geometry = Some(test_geometry());
                panel.tab_mut("ime", 0).unwrap().emulator.feed(b"abc\r\nxy");
                let bounds = panel
                    .bounds_for_range(0..0, test_geometry().bounds, window, cx)
                    .unwrap();
                assert_eq!(bounds.origin, gpui::point(px(34.0), px(48.0)));
                panel.active_grid_snapshot(cx);
                panel.tab_mut("ime", 0).unwrap().emulator.feed(b"\x1b[?25l");
                assert_eq!(panel.input_cursor_bounds(cx), Some(bounds));
                panel.replace_and_mark_text_in_range(None, "😀é", Some(2..2), window, cx);
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let _ = window.draw(cx);
        })
        .unwrap();
        window
            .update(cx, |panel, window, cx| {
                let (origin, line) = panel.composition_layout.as_ref().unwrap();
                let expected = gpui::point(origin.x + line.x_for_index("😀".len()), origin.y);
                let bounds = panel
                    .bounds_for_range(2..2, test_geometry().bounds, window, cx)
                    .unwrap();
                assert_eq!(bounds.origin, expected);
                assert_eq!(
                    panel.character_index_for_point(expected, window, cx),
                    Some(2)
                );
                assert!(panel.tab_mut("ime", 0).unwrap().coalescer.is_empty());
            })
            .unwrap();
    }

    #[test]
    fn input_waits_for_open_and_flushes_once() {
        let mut tab = pending_tab();
        assert!(tab.coalescer.push(b"hello"));
        assert_eq!(tab.take_input(), None);
        assert!(!tab.coalescer.push(b"\r"));
        assert_eq!(tab.take_input(), None);
        tab.terminal_id = Some("provider-pty".into());
        assert_eq!(
            tab.take_input(),
            Some(("provider-pty".into(), b"hello\r".to_vec()))
        );
        assert_eq!(tab.take_input(), None);
    }

    #[test]
    fn stopping_or_exiting_never_flushes_pending_input() {
        let mut tab = pending_tab();
        tab.coalescer.push(b"queued before close");
        tab.stop_input();
        tab.terminal_id = Some("provider-pty".into());
        assert_eq!(tab.take_input(), None);
        tab.coalescer.push(b"queued before exit");
        tab.exited = Some(0);
        assert_eq!(tab.take_input(), None);
        for status in [
            SessionViewStatus::Idle,
            SessionViewStatus::Closing,
            SessionViewStatus::Failed("failed".into()),
        ] {
            assert!(!status.accepts_input());
        }
        assert!(SessionViewStatus::Opening.accepts_input());
        assert!(SessionViewStatus::Ready.accepts_input());
    }

    #[gpui::test]
    fn session_mode_does_not_create_shell_tabs_or_follow_chat_switches(
        cx: &mut gpui::TestAppContext,
    ) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_chat = Some("first".into());
            state
        });
        let panel = cx.new(|cx| TerminalPanel::new_session_view(state.clone(), cx));
        panel.update(cx, |panel, cx| {
            assert!(panel.embedded && panel.session_view);
            assert_eq!(panel.session_view_status(), &SessionViewStatus::Idle);
            assert_eq!(panel.session_chat.as_deref(), Some("first"));
            panel.set_open(true, cx);
            assert_eq!(panel.open_tab_for_selected(cx), None);
            panel.open_tab("first".into(), cx);
            assert!(panel.chats.is_empty());
            panel.set_open(false, cx);
            assert_eq!(panel.session_chat.as_deref(), Some("first"));
        });
        state.update(cx, |state, cx| {
            state.selected_chat = Some("second".into());
            cx.notify();
        });
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.selected_chat(cx).as_deref(), Some("first"));
            assert!(panel.chats.is_empty());
        });
    }

    #[gpui::test]
    async fn session_open_shares_pending_result_and_reports_errors(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| AppState::new());
        let panel = cx.new(|cx| TerminalPanel::new_session_view(state, cx));
        let failed = panel.update(cx, |panel, cx| panel.open_session_view(cx));
        assert!(failed.await.is_err());
        panel.update(cx, |panel, _| {
            assert!(matches!(
                panel.session_view_status(),
                SessionViewStatus::Failed(_)
            ));
        });
        let (sender, receiver) = oneshot::channel();
        let first = panel.update(cx, |panel, cx| {
            panel.session_status = SessionViewStatus::Opening;
            panel.session_open = Some(cx.spawn(async move |_, _| receiver.await.unwrap()).shared());
            panel.open_session_view(cx)
        });
        let second = panel.update(cx, |panel, cx| panel.open_session_view(cx));
        sender.send(Ok(())).unwrap();
        assert_eq!(first.await, Ok(()));
        assert_eq!(second.await, Ok(()));
        panel.update(cx, |panel, _| assert!(panel.chats.is_empty()));
    }

    #[gpui::test]
    fn session_status_emits_only_transitions(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| AppState::new());
        let panel = cx.new(|cx| TerminalPanel::new_session_view(state, cx));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&panel, move |_, event: &SessionViewStatus, _| {
                observed.borrow_mut().push(event.clone());
            })
        });
        panel.update(cx, |panel, cx| {
            for status in [
                SessionViewStatus::Opening,
                SessionViewStatus::Ready,
                SessionViewStatus::Ready,
                SessionViewStatus::Closing,
                SessionViewStatus::Failed("hydrate failed".into()),
                SessionViewStatus::Closing,
                SessionViewStatus::Idle,
            ] {
                panel.set_session_status(status, cx);
            }
        });
        assert_eq!(
            *events.borrow(),
            vec![
                SessionViewStatus::Opening,
                SessionViewStatus::Ready,
                SessionViewStatus::Closing,
                SessionViewStatus::Failed("hydrate failed".into()),
                SessionViewStatus::Closing,
                SessionViewStatus::Idle
            ]
        );
    }

    #[test]
    fn height_clamps_between_160_and_55vh() {
        assert_eq!(clamp_terminal_height(300.0, 900.0), 300.0);
        assert_eq!(clamp_terminal_height(10.0, 900.0), 160.0);
        assert_eq!(clamp_terminal_height(4000.0, 900.0), 900.0 * 0.55);
        // Tiny windows: min wins over the 55vh cap.
        assert_eq!(clamp_terminal_height(200.0, 100.0), 160.0);
        assert_eq!(clamp_terminal_height(f32::NAN, 900.0), 160.0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_ms(0), 500);
        assert_eq!(backoff_ms(1), 1000);
        assert_eq!(backoff_ms(2), 2000);
        assert_eq!(backoff_ms(3), 4000);
        assert_eq!(backoff_ms(4), 8000);
        assert_eq!(backoff_ms(10), 8000);
        assert_eq!(backoff_ms(u32::MAX), 8000);
    }

    fn test_geometry() -> GridGeometry {
        GridGeometry {
            bounds: gpui::Bounds::new(
                gpui::point(px(10.0), px(20.0)),
                gpui::size(px(300.0), px(200.0)),
            ),
            origin: gpui::point(px(18.0), px(28.0)),
            cell_w: 8.0,
            line_h: 20.0,
            cols: 35,
            rows: 9,
        }
    }

    #[test]
    fn selection_edge_scroll_uses_terminal_direction() {
        let geometry = test_geometry();
        assert!(selection_scroll_lines(geometry, gpui::point(px(20.0), px(28.0))) > 0);
        assert_eq!(
            selection_scroll_lines(geometry, gpui::point(px(20.0), px(100.0))),
            0
        );
        assert!(selection_scroll_lines(geometry, gpui::point(px(20.0), px(208.0))) < 0);
    }

    #[test]
    fn scrollbar_thumb_maps_history_top_and_bottom() {
        let bounds = test_geometry().bounds;
        assert!(scrollbar_metrics(bounds, 20, 0, 0).is_none());

        let bottom = scrollbar_metrics(bounds, 20, 80, 0).unwrap();
        let top = scrollbar_metrics(bounds, 20, 80, 80).unwrap();
        assert!((bottom.thumb_height - 38.4).abs() < 0.01);
        assert!((bottom.thumb_top - bottom.travel()).abs() < 0.01);
        assert_eq!(top.thumb_top, 0.0);
        assert_eq!(top.thumb_height, bottom.thumb_height);
    }

    #[test]
    fn reorder_moves_forward_and_backward() {
        let mut v = vec!["a", "b", "c", "d"];
        reorder_tabs(&mut v, 0, 2);
        assert_eq!(v, ["b", "c", "a", "d"]);
        reorder_tabs(&mut v, 3, 0);
        assert_eq!(v, ["d", "b", "c", "a"]);
        // Out-of-range / no-op moves leave the vec untouched.
        reorder_tabs(&mut v, 9, 0);
        reorder_tabs(&mut v, 1, 1);
        assert_eq!(v, ["d", "b", "c", "a"]);
    }

    #[test]
    fn drop_index_quantizes_and_clamps() {
        assert_eq!(drop_index(-10.0, 150.0, 3), 0);
        assert_eq!(drop_index(0.0, 150.0, 3), 0);
        assert_eq!(drop_index(149.0, 150.0, 3), 0);
        assert_eq!(drop_index(150.0, 150.0, 3), 1);
        assert_eq!(drop_index(700.0, 150.0, 3), 2);
        assert_eq!(drop_index(50.0, 150.0, 0), 0);
    }

    #[test]
    fn slide_offsets_shift_toward_the_gap() {
        // Dragging 0 over 2: tabs 1 and 2 slide left one slot.
        assert_eq!(slide_offset(0, 0, 2), 0.0);
        assert_eq!(slide_offset(1, 0, 2), -1.0);
        assert_eq!(slide_offset(2, 0, 2), -1.0);
        assert_eq!(slide_offset(3, 0, 2), 0.0);
        // Dragging 3 over 1: tabs 1 and 2 slide right.
        assert_eq!(slide_offset(0, 3, 1), 0.0);
        assert_eq!(slide_offset(1, 3, 1), 1.0);
        assert_eq!(slide_offset(2, 3, 1), 1.0);
        assert_eq!(slide_offset(3, 3, 1), 0.0);
        // Hovering the origin: nothing moves.
        for ix in 0..4 {
            assert_eq!(slide_offset(ix, 2, 2), 0.0);
        }
    }

    #[test]
    fn active_index_tracks_reorders() {
        // The active tab itself moves.
        assert_eq!(active_after_reorder(1, 1, 3), 3);
        // A tab hopping over the active one from the left shifts it down.
        assert_eq!(active_after_reorder(2, 0, 3), 1);
        // …and from the right shifts it up.
        assert_eq!(active_after_reorder(1, 3, 0), 2);
        // Disjoint moves leave it alone.
        assert_eq!(active_after_reorder(0, 2, 3), 0);
    }

    #[test]
    fn active_index_tracks_closes() {
        assert_eq!(active_after_close(2, 0, 3), 1); // close left of active
        assert_eq!(active_after_close(1, 1, 2), 1); // close active mid-list
        assert_eq!(active_after_close(2, 2, 2), 1); // close active at tail
        assert_eq!(active_after_close(0, 0, 0), 0); // last tab closed
    }

    #[test]
    fn exit_message_format() {
        let text = String::from_utf8(exit_message(0)).unwrap();
        assert!(text.contains("[process exited 0]"));
        let text = String::from_utf8(exit_message(137)).unwrap();
        assert!(text.contains("[process exited 137]"));
        assert!(text.starts_with("\r\n"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn shell_titles() {
        assert_eq!(shell_title("/bin/zsh"), "zsh");
        assert_eq!(shell_title("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_title("C:\\Windows\\System32\\cmd.exe"), "cmd.exe");
        assert_eq!(shell_title("bash"), "bash");
        assert_eq!(shell_title(""), "terminal");
    }

    #[test]
    fn stream_events_deserialize_per_contract() {
        let data: TerminalEvent =
            serde_json::from_str(r#"{"type":"data","seq":7,"data":"aGk="}"#).unwrap();
        assert_eq!(
            data,
            TerminalEvent::Data {
                seq: 7,
                data: "aGk=".into()
            }
        );
        let exit: TerminalEvent =
            serde_json::from_str(r#"{"type":"exit","seq":8,"exitCode":130}"#).unwrap();
        assert_eq!(
            exit,
            TerminalEvent::Exit {
                seq: 8,
                exit_code: 130,
                signal: None
            }
        );
        let session: TerminalSession =
            serde_json::from_str(r#"{"id":"t1","cwd":"/w","shell":"/bin/zsh"}"#).unwrap();
        assert_eq!(session.id, "t1");
        assert_eq!(session.shell, "/bin/zsh");
    }

    #[test]
    fn base64_round_trip_and_tolerance() {
        assert_eq!(decode_base64("aGk="), b"hi".to_vec());
        assert_eq!(
            decode_base64("aGk"),
            b"hi".to_vec(),
            "unpadded input tolerated"
        );
        assert_eq!(
            decode_base64("!!!"),
            Vec::<u8>::new(),
            "garbage decodes to nothing"
        );
        assert_eq!(encode_base64(b"hi"), "aGk=");
    }

    #[test]
    fn exit_message_feeds_cleanly_through_the_emulator() {
        let mut emulator = Emulator::new(40, 4);
        emulator.feed(b"$ done");
        emulator.feed(&exit_message(1));
        assert_eq!(emulator.row_text(1), "[process exited 1]");
    }
}

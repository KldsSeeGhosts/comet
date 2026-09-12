use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Empty, Entity, EventEmitter, Focusable,
    IntoElement, KeyBinding, MouseButton, Pixels, Render, SharedString, Subscription,
    Task, Window, actions, div, prelude::*, px,
};
use zeron_workspace::{
    Branch, Direction, PaneId, PaneMode, PaneState, TabId, TabPlacement,
    ViewId, WorkspaceLayout, edge_zone,
};

use crate::composer::Composer;
use crate::session_pane::{ChatView, ChatViewEvent};
use crate::state::AppState;
use crate::terminal::panel::{SessionViewStatus, TerminalPanel};
use crate::theme::Theme;
use crate::transcript::Transcript;

mod animation;
mod launch;
mod control;
use animation::{TreeMotion, VisualNode};
#[cfg(test)]
use zeron_workspace::SplitNode;
#[cfg(test)]
mod tests;

pub fn layout_path(state: &AppState, directory: &std::path::Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let identity = match (&state.workspace_scope, &state.auth) {
        (Some(zeron_proto::WorkspaceScope::Synced), Some(zeron_proto::AuthState::SignedIn { user, org_id })) => {
            format!("synced:{}:{}", user.id, org_id.as_deref().unwrap_or_default())
        }
        (Some(zeron_proto::WorkspaceScope::Development), _) => "development".into(),
        _ => return directory.join("workspace-layout.json"),
    };
    let digest = Sha256::digest(identity.as_bytes());
    directory.join(format!("workspace-layout-{digest:x}.json"))
}

actions!(workspace, [SplitRight, SplitDown, SplitViewRight, SplitViewDown, NewTab, ClosePane, ToggleSessionView]);

pub fn bind_keys(cx: &mut App) {
    let modifier = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
    cx.bind_keys([
        KeyBinding::new(&format!("{modifier}-e"), ToggleSessionView, Some("NochesWorkspace")),
        KeyBinding::new(&format!("{modifier}-alt-right"), SplitRight, Some("NochesWorkspace")),
        KeyBinding::new(&format!("{modifier}-alt-down"), SplitDown, Some("NochesWorkspace")),
        KeyBinding::new(&format!("{modifier}-alt-shift-right"), SplitViewRight, Some("NochesWorkspace")),
        KeyBinding::new(&format!("{modifier}-alt-shift-down"), SplitViewDown, Some("NochesWorkspace")),
        KeyBinding::new(&format!("{modifier}-alt-t"), NewTab, Some("NochesWorkspace")),
        KeyBinding::new(&format!("{modifier}-alt-w"), ClosePane, Some("NochesWorkspace")),
    ]);
}

pub enum WorkspaceEvent {
    ActivePane {
        chat: Option<String>,
        transcript: Entity<Transcript>,
        composer: Entity<Composer>,
    },
}

/// A pane changes renderer without changing its conversation identity.
pub enum TabItem {
    Chat(Entity<ChatView>),
    Terminal(Entity<TerminalPanel>),
}

struct PaneRuntime {
    chat: Entity<ChatView>,
    item: TabItem,
    terminal: Option<Entity<TerminalPanel>>,
    _chat_events: Subscription,
    terminal_events: Option<Subscription>,
    terminal_state: Option<Subscription>,
    terminal_human_events: Option<Subscription>,
}

#[derive(Clone, Copy)]
enum DragTarget { Pane(PaneId), Tab(TabId) }

#[derive(Clone)]
struct LayoutDrag { target: DragTarget, title: String }

struct PaneGhost { title: String, tab: bool }
impl Render for PaneGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div().max_w(px(220.0)).px(px(10.0)).h(px(32.0)).rounded(px(6.0))
            .border_1().border_color(theme.border_strong).bg(theme.surface_overlay).shadow_md()
            .font_family(theme.font_sans_fixed.clone()).text_color(theme.text).text_size(px(12.0))
            .flex().items_center().gap(px(7.0))
            .child(crate::icons::icon(if self.tab { crate::icons::WIDGET } else { crate::icons::CHAT_ROUND_LINE })
                .size(px(13.0)).text_color(theme.text_muted))
            .child(div().truncate().child(self.title.clone()))
    }
}

#[derive(Clone)]
struct SplitResize {
    tier: Option<(ViewId, TabId)>,
    path: Vec<Branch>,
}

#[derive(Clone)]
struct RailResize(ViewId);

fn outside_ring(position: gpui::Point<Pixels>, bounds: Bounds<Pixels>) -> Option<Direction> {
    if !bounds.contains(&position) { return None; }
    let offset = position - bounds.origin;
    let distances = [
        (f32::from(offset.x), Direction::Left),
        (f32::from(bounds.size.width - offset.x), Direction::Right),
        (f32::from(offset.y), Direction::Up),
        (f32::from(bounds.size.height - offset.y), Direction::Down),
    ];
    distances.into_iter().filter(|(distance, _)| *distance <= 14.0)
        .min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, direction)| direction)
}

struct Ghost;
impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement { Empty }
}

struct ControlTooltip(SharedString);
impl Render for ControlTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().px(px(8.0)).py(px(5.0)).rounded(px(4.0)).text_size(px(12.0))
            .bg(Theme::of(cx).surface_overlay).text_color(Theme::of(cx).text).child(self.0.clone())
    }
}

pub struct Workspace {
    pub layout: WorkspaceLayout,
    source: Entity<AppState>,
    source_selected: Option<String>,
    path: PathBuf,
    panes: BTreeMap<PaneId, PaneRuntime>,
    pending_close: BTreeSet<PaneId>,
    error: Option<String>,
    drop_preview: Option<(PaneId, Option<Direction>)>,
    outer_preview: Option<(ViewId, Direction)>,
    tab_preview: Option<(ViewId, Option<TabId>)>,
    tab_bounds: BTreeMap<TabId, Bounds<Pixels>>,
    starting_cli: BTreeMap<PaneId, Task<()>>,
    control_consent: control::Consent,
    orchestration: Option<zeron_orchestration::Store>,
    save_task: Option<Task<()>>,
    publish_active: bool,
    focus_pending: bool,
    control: Option<zeron_local_api::ControlPlane>,
    control_task: Option<Task<()>>,
    control_watches: BTreeMap<String, Task<()>>,
    events: Option<zeron_local_api::EventHub>,
    pane_menu: crate::popover::Popup<(PaneId, gpui::Point<Pixels>)>,
    view_motion: TreeMotion<ViewId>,
    tab_motion: BTreeMap<(ViewId, TabId), TreeMotion<PaneId>>,
    snap_motion: bool,
    animating: bool,
    _source: Subscription,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(source: Entity<AppState>, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let selected = source.read(cx).selected_chat.clone();
        let (mut layout, error) = if path.exists() {
            match WorkspaceLayout::load(&path) {
                Ok(layout) => (layout, None),
                Err(error) => (WorkspaceLayout::new(), Some(format!("Could not restore layout: {error}"))),
            }
        } else {
            (WorkspaceLayout::new(), None)
        };
        if !path.exists() {
            let active = layout.active_pane_id().unwrap();
            layout.pane_mut(active).unwrap().session_id = selected.clone();
        }
        let observation = cx.observe(&source, |this: &mut Self, source, cx| {
            let selected = source.read(cx).selected_chat.clone();
            if selected != this.source_selected {
                this.source_selected = selected.clone();
                this.select_session(selected, cx);
            }
        });
        Self {
            view_motion: TreeMotion::new(&layout.root, Instant::now()),
            tab_motion: BTreeMap::new(), snap_motion: false, animating: false,
            layout, source, source_selected: selected, path, panes: BTreeMap::new(),
            pending_close: BTreeSet::new(), error, drop_preview: None, save_task: None,
            outer_preview: None, tab_preview: None, tab_bounds: BTreeMap::new(), starting_cli: BTreeMap::new(),
            control_consent: Default::default(), orchestration: None,
            publish_active: true, focus_pending: true, _source: observation,
            control: None, control_task: None, control_watches: BTreeMap::new(), events: None,
            pane_menu: Default::default(),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let layout = self.layout.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let result = cx.background_executor().spawn(async move { layout.save(path) }).await;
            if let Err(error) = result {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not save layout: {error}"));
                    cx.notify();
                });
            }
        }));
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.save_task = None;
        self.layout.save(&self.path).map_err(|error| error.to_string())
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.publish_active = true;
        self.save(cx);
        cx.notify();
    }

    fn apply(&mut self, operation: impl FnOnce(&mut WorkspaceLayout) -> zeron_workspace::Result<()>, cx: &mut Context<Self>) {
        match operation(&mut self.layout) {
            Ok(()) => { self.error = None; self.changed(cx); }
            Err(error) => { self.error = Some(error.to_string()); cx.notify(); }
        }
    }

    fn focus(&mut self, id: PaneId, cx: &mut Context<Self>) {
        if self.layout.active_pane_id() != Some(id) {
            self.focus_pending = true;
            self.apply(|layout| layout.focus_pane(id), cx);
        }
    }

    fn select_session(&mut self, chat: Option<String>, cx: &mut Context<Self>) {
        if let Some(chat_id) = &chat {
            let existing = self.layout.views.values().flat_map(|view| view.tabs.values())
                .flat_map(|tab| tab.panes.iter()).find(|(_, pane)| pane.session_id.as_ref() == Some(chat_id))
                .map(|(id, _)| *id);
            if let Some(existing) = existing {
                self.focus(existing, cx);
                return;
            }
        }
        let Some(id) = self.layout.active_pane_id() else { return };
        let switching = self.panes.get(&id).and_then(|pane| pane.terminal.as_ref())
            .is_some_and(|terminal| !matches!(terminal.read(cx).session_view_status(), SessionViewStatus::Idle));
        if switching || self.layout.pane(id).is_some_and(|pane| pane.mode == PaneMode::Terminal) {
            // A new sidebar selection opens a tab rather than orphaning a CLI writer.
            let view = self.layout.active_view_id;
            self.apply(|layout| layout.add_tab(view, PaneState { session_id: chat, ..Default::default() }).map(|_| ()), cx);
            return;
        }
        self.apply(|layout| layout.compose(layout.revision, |draft| {
            draft.pane_mut(id).unwrap().session_id = chat.clone();
            Ok(())
        }), cx);
        if let Some(runtime) = self.panes.get(&id) {
            runtime.chat.update(cx, |pane, cx| pane.select(chat, cx));
        }
    }

    fn split(&mut self, direction: Direction, view: bool, cx: &mut Context<Self>) {
        self.focus_pending = true;
        let Some(pane) = self.layout.active_pane_id() else { return };
        let active_view = self.layout.active_view_id;
        self.apply(|layout| {
            if view { layout.split_view(active_view, direction, PaneState::default()).map(|_| ()) }
            else { layout.split_pane(pane, direction, PaneState::default()).map(|_| ()) }
        }, cx);
    }

    fn new_tab(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        let view = self.layout.active_view_id;
        self.apply(|layout| layout.add_tab(view, PaneState::default()).map(|_| ()), cx);
    }

    fn close(&mut self, id: PaneId, cx: &mut Context<Self>) {
        self.focus_pending = true;
        let mut draft = self.layout.clone();
        if let Err(error) = draft.close_to_launcher(id) {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        // A parked pane has no renderer, but its daemon PTY still owns the chat.
        // Bind a close-only panel rather than reopening a provider to close it.
        if self.layout.pane(id).is_some_and(|pane| pane.mode == PaneMode::Terminal) {
            self.ensure_pane_runtime(id, false, cx);
        }
        if let Some(terminal) = self.panes.get(&id).and_then(|pane| pane.terminal.clone()) {
            self.pending_close.insert(id);
            terminal.update(cx, |terminal, cx| terminal.close_session_view(cx)).detach();
        } else {
            self.apply(|layout| layout.close_to_launcher(id), cx);
            self.panes.remove(&id);
        }
    }

    fn ensure_pane(&mut self, id: PaneId, cx: &mut Context<Self>) {
        self.ensure_pane_runtime(id, true, cx);
    }

    fn ensure_pane_runtime(&mut self, id: PaneId, reopen: bool, cx: &mut Context<Self>) {
        if self.panes.contains_key(&id) { return; }
        let Some(state) = self.layout.pane(id).cloned() else { return };
        let source = self.source.clone();
        let chat = cx.new(|cx| ChatView::new(&source, state.session_id, cx));
        let events = cx.subscribe(&chat, move |this, _, event, cx| match event {
            ChatViewEvent::Focused => this.focus(id, cx),
            ChatViewEvent::HumanSubmitted(chat, proof) => this.control_mark_human_input(chat, *proof, cx),
            ChatViewEvent::Selected(selected) => {
                if this.layout.pane(id).is_some_and(|pane| pane.session_id != *selected) {
                    this.apply(|layout| layout.compose(layout.revision, |draft| {
                        draft.pane_mut(id).unwrap().session_id = selected.clone();
                        Ok(())
                    }), cx);
                }
            }
        });
        self.panes.insert(id, PaneRuntime {
            item: TabItem::Chat(chat.clone()), chat, terminal: None,
            _chat_events: events, terminal_events: None, terminal_state: None, terminal_human_events: None,
        });
        if state.mode == PaneMode::Terminal {
            if reopen { self.open_terminal(id, cx); }
            else { self.prepare_terminal(id, cx); }
        }
    }

    fn open_terminal(&mut self, id: PaneId, cx: &mut Context<Self>) {
        if self.layout.pane(id).is_some_and(|pane| pane.session_id.is_none()) {
            self.start_empty_cli(id, cx);
            return;
        }
        if let Some(terminal) = self.prepare_terminal(id, cx) {
            terminal.update(cx, |terminal, cx| terminal.open_session_view(cx)).detach();
        }
    }

    fn prepare_terminal(&mut self, id: PaneId, cx: &mut Context<Self>) -> Option<Entity<TerminalPanel>> {
        let runtime = self.panes.get_mut(&id)?;
        if runtime.terminal.is_none() {
            let state = runtime.chat.read(cx).state.clone();
            let terminal = cx.new(|cx| TerminalPanel::new_session_view(state, cx));
            runtime.terminal_events = Some(cx.subscribe(&terminal, move |this, terminal, status, cx| {
                match status {
                    SessionViewStatus::Opening => {
                        if let Some(runtime) = this.panes.get_mut(&id) { runtime.item = TabItem::Terminal(terminal.clone()); }
                        this.focus_pending = this.layout.active_pane_id() == Some(id);
                    }
                    SessionViewStatus::Ready => {
                        this.pending_close.remove(&id);
                        if let Some(runtime) = this.panes.get_mut(&id) { runtime.item = TabItem::Terminal(terminal.clone()); }
                        this.set_mode(id, PaneMode::Terminal, cx);
                    }
                    SessionViewStatus::Idle => {
                        if this.pending_close.remove(&id) {
                            this.apply(|layout| layout.close_to_launcher(id), cx);
                            this.panes.remove(&id);
                        } else {
                            if let Some(runtime) = this.panes.get_mut(&id) { runtime.item = TabItem::Chat(runtime.chat.clone()); }
                            this.set_mode(id, PaneMode::Chat, cx);
                        }
                    }
                    SessionViewStatus::Failed(error) => {
                        this.pending_close.remove(&id);
                        this.error = Some(error.clone());
                    }
                    _ => {}
                }
                cx.notify();
            }));
            runtime.terminal_human_events = Some(cx.subscribe(&terminal, |this, _, event: &crate::terminal::panel::HumanTerminalInput, cx| {
                this.control_mark_human_input(&event.chat_id, event.proof, cx);
            }));
            runtime.terminal_state = Some(cx.observe(&terminal, |_, _, cx| cx.notify()));
            runtime.terminal = Some(terminal);
        }
        runtime.terminal.clone()
    }

    fn set_mode(&mut self, id: PaneId, mode: PaneMode, cx: &mut Context<Self>) {
        self.focus_pending = self.layout.active_pane_id() == Some(id);
        if self.layout.pane(id).is_some_and(|pane| pane.mode != mode) {
            self.apply(|layout| layout.compose(layout.revision, |draft| {
                draft.pane_mut(id).unwrap().mode = mode;
                Ok(())
            }), cx);
        }
    }

    fn toggle(&mut self, id: PaneId, cx: &mut Context<Self>) {
        // Failed handoffs can retain native ownership even before mode is committed.
        // The Chat control must recover that handoff, never retry opening CLI.
        if let Some(terminal) = self.panes.get(&id).filter(|runtime| matches!(runtime.item, TabItem::Terminal(_)))
            .and_then(|runtime| runtime.terminal.clone())
            .filter(|terminal| matches!(terminal.read(cx).session_view_status(), SessionViewStatus::Failed(_))) {
            terminal.update(cx, |terminal, cx| terminal.close_session_view(cx)).detach();
            return;
        }
        if let Some(reason) = self.toggle_unavailable(id, cx) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        self.focus(id, cx);
        self.ensure_pane(id, cx);
        if self.layout.pane(id).is_some_and(|pane| pane.mode == PaneMode::Terminal) {
            if let Some(terminal) = self.panes.get(&id).and_then(|pane| pane.terminal.clone()) {
                terminal.update(cx, |terminal, cx| terminal.close_session_view(cx)).detach();
            }
        } else { self.open_terminal(id, cx); }
    }

    fn toggle_unavailable(&self, id: PaneId, cx: &App) -> Option<&'static str> {
        let pane = self.layout.pane(id)?;
        if self.starting_cli.contains_key(&id) { return Some("Preparing CLI session"); }
        if self.panes.get(&id).and_then(|runtime| runtime.terminal.as_ref()).is_some_and(|terminal|
            matches!(terminal.read(cx).session_view_status(), SessionViewStatus::Opening | SessionViewStatus::Closing)) {
            return Some("Wait for the current view switch to finish");
        }
        if pane.mode == PaneMode::Terminal {
            return self.panes.get(&id).and_then(|runtime| runtime.terminal.as_ref())
                .map_or(Some("Native session state is not verified"), |terminal| terminal.read(cx).handoff_unavailable());
        }
        let Some(session) = pane.session_id.as_ref() else { return self.empty_cli_unavailable(id, cx); };
        let state = self.source.read(cx);
        if state.sessions.iter().any(|s| &s.chat_id == session && matches!(s.status,
            zeron_proto::SessionStatus::Working | zeron_proto::SessionStatus::AwaitingInput)) {
            return Some("Wait for the agent to become idle before switching views");
        }
        let Some(chat) = state.chats.iter().find(|chat| &chat.id == session) else { return Some("Session is unavailable"); };
        if chat.config.as_ref().is_none_or(|config| config.harness != zeron_proto::HarnessId::Pi) {
            return Some("CLI switching is available for Pi sessions");
        }
        None
    }

    fn close_pane_menu(&mut self, cx: &mut Context<Self>) {
        if self.pane_menu.begin_close() {
            crate::popover::reap_popup(cx, |this: &mut Self| &mut this.pane_menu);
            cx.notify();
        }
    }

    fn render_pane_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let &(pane, position) = self.pane_menu.get()?;
        let closing = self.pane_menu.closing_since();
        let theme = Theme::of(cx).clone();
        let terminal = self.layout.pane(pane)?.mode == PaneMode::Terminal;
        let reason = self.toggle_unavailable(pane, cx);
        let mut menu = crate::popover::popover_card(&theme).w(px(238.0)).flex().flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_pane_menu(cx)));
        for (action, label) in [
            if terminal { "Switch to Chat view" } else { "Switch to CLI view" },
            "Split pane right", "Split pane below", "Split view right", "Split view below",
            "Move pane to a tab", "Close pane", "Move tab rail",
            if self.control_has_allow(cx) { "Revoke API agent access" } else { "Allow API agent access" },
            if self.control_has_orchestration(cx) { "Revoke API orchestration in this workspace" } else { "Allow API orchestration in this workspace" },
        ].into_iter().enumerate() {
            let enabled = action != 0 || reason.is_none();
            menu = menu.child(div().id(SharedString::from(format!("pane-menu-{action}")))
                .role(gpui::Role::MenuItem).aria_label(label).px(px(10.0)).py(px(7.0)).rounded(px(4.0))
                .text_size(px(12.0)).text_color(if enabled { theme.text } else { theme.text_muted })
                .when(enabled, |e| e.cursor_pointer().hover(|s| s.bg(theme.surface_raised))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_pane_menu(cx);
                        this.focus(pane, cx);
                        match action {
                            0 => this.toggle(pane, cx),
                            1 => this.split(Direction::Right, false, cx),
                            2 => this.split(Direction::Down, false, cx),
                            3 => this.split(Direction::Right, true, cx),
                            4 => this.split(Direction::Down, true, cx),
                            5 => { let view = this.layout.active_view_id;
                                this.apply(|layout| layout.pane_to_tab(pane, view).map(|_| ()), cx); },
                            6 => this.close(pane, cx),
                            7 => { let view = this.layout.active_view_id;
                                this.apply(|layout| layout.compose(layout.revision, |draft| {
                                    let view = draft.views.get_mut(&view).unwrap();
                                    view.tab_placement = if view.tab_placement == TabPlacement::Top { TabPlacement::Left } else { TabPlacement::Top };
                                    Ok(())
                                }), cx); },
                            8 => this.control_toggle_allow(cx),
                            _ => this.control_toggle_orchestration(cx),
                        }
                        cx.stop_propagation();
                    })))
                .child(label));
        }
        if let Some(reason) = reason {
            menu = menu.child(div().px(px(10.0)).py(px(6.0)).text_size(px(11.0)).text_color(theme.text_muted).child(reason));
        }
        Some(crate::popover::menu_at("workspace-pane-menu", position, menu.into_any_element(), closing))
    }

    fn title(&self, id: PaneId, cx: &App) -> String {
        let Some(pane) = self.layout.pane(id) else { return "Closed pane".into() };
        if let Some(label) = &pane.label { return label.clone(); }
        pane.session_id.as_ref().and_then(|id| self.source.read(cx).chats.iter()
            .find(|chat| &chat.id == id).and_then(|chat| chat.title.clone()))
            .unwrap_or_else(|| "New session".into())
    }

    fn render_pane_tools(&self, id: PaneId, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let terminal = self.panes.get(&id).is_some_and(|p| matches!(p.item, TabItem::Terminal(_)))
            || self.layout.pane(id).is_some_and(|p| p.mode == PaneMode::Terminal);
        let recovering = self.panes.get(&id).and_then(|p| p.terminal.as_ref())
            .is_some_and(|terminal| matches!(terminal.read(cx).session_view_status(), SessionViewStatus::Failed(_)));
        let reason = if recovering { None } else { self.toggle_unavailable(id, cx) };
        let mut modes = div().flex().items_center().gap(px(2.0)).flex_none();
        for (is_cli, label, icon) in [(false, "Chat", crate::icons::CHAT_ROUND_LINE), (true, "CLI", crate::icons::TERMINAL)] {
            let selected = terminal == is_cli;
            let enabled = selected || reason.is_none();
            let tooltip: SharedString = reason.filter(|_| !selected).unwrap_or(if is_cli { "Open CLI view" } else { "Open Chat view" }).into();
            modes = modes.child(div().id(SharedString::from(format!("pane-mode-{}-{label}", id.0)))
                .role(gpui::Role::Button).aria_label(format!("{label} view"))
                .h(px(26.0)).px(px(7.0)).rounded(px(5.0)).flex().items_center().gap(px(5.0))
                .text_size(px(11.0)).text_color(if selected { theme.text } else { theme.text_muted })
                .when(selected, |e| e.bg(theme.surface_raised))
                .when(!enabled, |e| e.opacity(0.45))
                .tooltip(move |_, cx| cx.new(|_| ControlTooltip(tooltip.clone())).into())
                .when(enabled && !selected, |e| e.cursor_pointer().hover(|s| s.bg(theme.surface_raised))
                    .on_click(cx.listener(move |this, _, _, cx| { cx.stop_propagation(); this.toggle(id, cx); })))
                .child(crate::icons::icon(icon).size(px(12.0))).child(label));
        }
        modes.child(self.button(format!("pane-menu-{}", id.0), "Pane actions", move |this, window, cx| {
            this.pane_menu.open((id, window.mouse_position())); cx.notify();
        }, cx)).into_any_element()
    }

    fn button(&self, id: String, label: &'static str, action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let caption = match label {
            "Split right" => "◫",
            "Split down" => "⊟",
            "View right" => "View ◫",
            "View down" => "View ⊟",
            "Pane actions" => "⋯",
            _ => label,
        };
        div().id(SharedString::from(id)).flex_none().px(px(7.0)).h(px(26.0))
            .flex().items_center().rounded(px(4.0)).cursor_pointer()
            .role(gpui::Role::Button).aria_label(label)
            .tooltip(move |_, cx| cx.new(|_| ControlTooltip(label.into())).into())
            .text_size(px(11.0)).text_color(theme.text_muted)
            .hover(|style| style.bg(theme.surface_raised).text_color(theme.text))
            .on_click(cx.listener(move |this, _, window, cx| { cx.stop_propagation(); action(this, window, cx); }))
            .child(caption).into_any_element()
    }

    fn drop_outer(&mut self, drag: &LayoutDrag, cx: &mut Context<Self>) -> bool {
        let Some((view, direction)) = self.outer_preview.take() else { return false; };
        self.drop_preview = None;
        self.tab_preview = None;
        self.focus_pending = true;
        self.apply(|layout| match drag.target {
            DragTarget::Pane(pane) => layout.pane_to_view(pane, view, direction).map(|_| ()),
            DragTarget::Tab(tab) => layout.tab_to_view(tab, view, direction).map(|_| ()),
        }, cx);
        cx.stop_propagation();
        true
    }

    fn render_pane(&mut self, id: PaneId, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_pane(id, cx);
        let theme = Theme::of(cx).clone();
        let active = self.layout.active_pane_id() == Some(id);
        let (view, tab) = self.layout.pane_location(id).unwrap();
        let show_header = self.layout.views[&view].tabs[&tab].panes.len() > 1
            || self.layout.views[&view].tab_placement == TabPlacement::Left;
        let title = self.title(id, cx);
        self.panes[&id].chat.update(cx, |chat, cx| chat.set_active(active, cx));
        if let Some(terminal) = &self.panes[&id].terminal {
            terminal.update(cx, |terminal, _| terminal.set_resize_suspended(self.animating));
        }
        let content = match &self.panes[&id].item {
            TabItem::Chat(chat) => chat.clone().into_any_element(),
            TabItem::Terminal(terminal) => terminal.clone().into_any_element(),
        };
        let failed = self.panes[&id].terminal.as_ref().is_some_and(|terminal|
            matches!(terminal.read(cx).session_view_status(), SessionViewStatus::Failed(_)));
        let transition = self.panes[&id].terminal.as_ref().and_then(|terminal| {
            match terminal.read(cx).session_view_status() {
                SessionViewStatus::Opening => Some("Opening CLI session…".to_owned()),
                SessionViewStatus::Closing => Some("Restoring chat history…".to_owned()),
                SessionViewStatus::Failed(error) => Some(format!("View switch failed: {error}")),
                _ => None,
            }
        }).or_else(|| self.starting_cli.contains_key(&id).then(|| "Preparing CLI session…".to_owned()));
        let header = div().id(SharedString::from(format!("pane-header-{}", id.0)))
            .h(px(36.0)).flex_none().flex().items_center().px(px(10.0)).gap(px(8.0))
            .border_b_1().border_color(if active { theme.border_strong } else { theme.border })
            .bg(theme.bg)
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                this.pane_menu.open((id, event.position));
                cx.stop_propagation(); cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.focus(id, cx)))
            .child(div().id(SharedString::from(format!("pane-drag-{}", id.0)))
                .debug_selector(|| format!("pane-drag-{}", id.0))
                .flex_1().min_w_0().truncate().cursor_grab().text_size(px(12.0))
                .text_color(if active { theme.text } else { theme.text_muted })
                .on_drag(LayoutDrag { target: DragTarget::Pane(id), title: title.clone() }, |drag, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| PaneGhost { title: drag.title.clone(), tab: false })
                })
                .child(title))
            .child(self.render_pane_tools(id, cx));
        div().id(SharedString::from(format!("pane-{}", id.0))).size_full().min_w_0().min_h_0()
            .debug_selector(|| format!("pane-{}", id.0))
            .relative().flex().flex_col().overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.focus(id, cx)))
            .on_drag_move::<LayoutDrag>(cx.listener(move |this, event: &gpui::DragMoveEvent<LayoutDrag>, _, cx| {
                if event.bounds.contains(&event.event.position) { this.tab_preview = None; }
                let position = event.event.position - event.bounds.origin;
                let direction = edge_zone(f32::from(position.x) as f64, f32::from(position.y) as f64,
                    f32::from(event.bounds.size.width) as f64, f32::from(event.bounds.size.height) as f64);
                let valid = match event.drag(cx).target {
                    DragTarget::Pane(source) => source != id,
                    DragTarget::Tab(source) => this.layout.pane_location(id).is_some_and(|(_, tab)| tab != source),
                };
                let preview = (valid && event.bounds.contains(&event.event.position)).then_some((id, direction));
                if (preview.is_some() || this.drop_preview.is_some_and(|(target, _)| target == id))
                    && this.drop_preview != preview { this.drop_preview = preview; cx.notify(); }
            }))
            .on_drop::<LayoutDrag>(cx.listener(move |this, drag: &LayoutDrag, _, cx| {
                if this.drop_outer(drag, cx) { return; }
                if let Some((target, direction)) = this.drop_preview.take().filter(|(target, _)| *target == id) {
                    this.focus_pending = true;
                    this.apply(|layout| match (drag.target, direction) {
                        (DragTarget::Pane(source), Some(direction)) => layout.move_pane(source, target, direction),
                        (DragTarget::Pane(source), None) => {
                            let (view, _) = layout.pane_location(target).ok_or(zeron_workspace::LayoutError::NotFound("target pane"))?;
                            layout.pane_to_tab(source, view).map(|_| ())
                        }
                        (DragTarget::Tab(source), Some(direction)) => layout.merge_tab(source, target, direction),
                        (DragTarget::Tab(source), None) => {
                            let (view, _) = layout.pane_location(target).ok_or(zeron_workspace::LayoutError::NotFound("target pane"))?;
                            layout.move_tab(source, view)
                        }
                    }, cx);
                    cx.stop_propagation();
                }
            }))
            .when(show_header, |e| e.child(header))
            .child(div().flex_1().min_w_0().min_h_0().relative().child(content)
                .when_some(transition, |element, message| element.child(
                    div().absolute().top(px(8.0)).left(px(12.0)).right(px(12.0)).rounded(px(6.0)).bg(theme.surface_overlay)
                        .flex().flex_col().gap(px(8.0)).p(px(10.0)).items_center().justify_center().text_size(px(12.0))
                        .text_color(theme.text_muted).child(message)
                        .when(failed, |e| e.child(self.button(format!("pane-recover-{}", id.0), "Return to Chat", move |this, _, cx| {
                            if let Some(terminal) = this.panes.get(&id).and_then(|pane| pane.terminal.clone()) {
                                terminal.update(cx, |terminal, cx| terminal.close_session_view(cx)).detach();
                            }
                        }, cx))))))
            .when_some(self.drop_preview.filter(|(target, _)| *target == id && self.outer_preview.is_none() && cx.has_active_drag()), |element, (_, direction)| {
                element.child(div().absolute().bg(theme.accent.opacity(0.18)).border_2().border_color(theme.accent)
                    .rounded(px(8.0)).flex().items_center().justify_center()
                    .when(direction.is_none(), |e| e.inset(px(8.0)))
                    .when(direction == Some(Direction::Left), |e| e.left_0().top_0().bottom_0().w(gpui::relative(0.5)))
                    .when(direction == Some(Direction::Right), |e| e.right_0().top_0().bottom_0().w(gpui::relative(0.5)))
                    .when(direction == Some(Direction::Up), |e| e.left_0().right_0().top_0().h(gpui::relative(0.5)))
                    .when(direction == Some(Direction::Down), |e| e.left_0().right_0().bottom_0().h(gpui::relative(0.5)))
                    .child(div().px(px(10.0)).py(px(6.0)).rounded(px(5.0)).bg(theme.surface_overlay)
                        .text_color(theme.text).text_size(px(12.0)).child(match direction {
                            None => "Move into tab rail",
                            Some(Direction::Left) => "Move left",
                            Some(Direction::Right) => "Move right",
                            Some(Direction::Up) => "Move above",
                            Some(Direction::Down) => "Move below",
                        })))
            }).into_any_element()
    }

    fn render_view(&mut self, id: ViewId, cx: &mut Context<Self>) -> AnyElement {
        let view = self.layout.views[&id].clone();
        let theme = Theme::of(cx).clone();
        let top = view.tab_placement == TabPlacement::Top;
        let active_pane = view.tabs[&view.active_tab_id].active_pane_id;
        self.ensure_pane(active_pane, cx);
        let mut rail = div().id(SharedString::from(format!("view-tabs-{}", id.0)))
            .relative().flex().gap(px(3.0)).min_w_0().min_h_0()
            .when(top, |e| e.flex_row().items_center().flex_1().overflow_x_scroll())
            .when(!top, |e| e.flex_col().flex_1().overflow_y_scroll())
            .on_drag_move::<LayoutDrag>(cx.listener(move |this, event: &gpui::DragMoveEvent<LayoutDrag>, _, cx| {
                if !event.bounds.contains(&event.event.position) { return; }
                let ordered = this.layout.views[&id].ordered_tabs();
                let before = ordered.iter().enumerate().find_map(|(index, tab)| {
                    let bounds = this.tab_bounds.get(tab)?;
                    if !bounds.contains(&event.event.position) { return None; }
                    let offset = event.event.position - bounds.origin;
                    let first_half = if top { offset.x < bounds.size.width / 2.0 } else { offset.y < bounds.size.height / 2.0 };
                    Some(if first_half { Some(*tab) } else { ordered.get(index + 1).copied() })
                }).flatten();
                if this.tab_preview != Some((id, before)) {
                    this.tab_preview = Some((id, before)); this.drop_preview = None; this.outer_preview = None; cx.notify();
                }
            }))
            .on_drop::<LayoutDrag>(cx.listener(move |this, drag: &LayoutDrag, _, cx| {
                // Tab rails always mean insertion, even inside the outer split ring.
                let before = this.tab_preview.take().filter(|(view, _)| *view == id).and_then(|(_, before)| before);
                this.outer_preview = None; this.drop_preview = None; this.focus_pending = true;
                this.apply(|layout| layout.compose(layout.revision, |draft| {
                    let tab = match drag.target {
                        DragTarget::Tab(tab) => tab,
                        DragTarget::Pane(pane) => draft.pane_to_tab(pane, id)?,
                    };
                    if before != Some(tab) { draft.reorder_tab(tab, id, before)?; }
                    Ok(())
                }), cx);
                cx.stop_propagation();
            }));
        let ordered = view.ordered_tabs();
        for tab_id in ordered.iter().copied() {
            let tab = &view.tabs[&tab_id];
            let pane_id = tab.active_pane_id;
            let title = self.title(pane_id, cx);
            let selected = view.active_tab_id == tab_id;
            let measured = cx.weak_entity();
            let insert_before = self.tab_preview == Some((id, Some(tab_id))) && cx.has_active_drag();
            rail = rail.child(div().id(SharedString::from(format!("view-{}-tab-{}", id.0, tab_id.0)))
                .debug_selector(|| format!("workspace-tab-{}", tab_id.0))
                .group("workspace-tab").relative().flex().items_center().gap(px(7.0)).flex_none()
                .min_w(px(72.0)).max_w(px(220.0)).h(px(34.0)).px(px(9.0))
                .text_size(px(12.0)).cursor_pointer().text_color(if selected { theme.text } else { theme.text_muted })
                .border_b_2().border_color(if selected { theme.accent } else { gpui::transparent_black() })
                .when(selected, |e| e.font_weight(gpui::FontWeight::MEDIUM))
                .hover(|s| s.bg(theme.surface.opacity(0.5)))
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.pane_menu.open((pane_id, event.position)); cx.stop_propagation(); cx.notify();
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.focus_pending = true; this.apply(|layout| layout.focus_tab(id, tab_id), cx);
                }))
                .on_drag(LayoutDrag { target: DragTarget::Tab(tab_id), title: title.clone() }, |drag, _, _, cx| {
                    cx.stop_propagation(); cx.new(|_| PaneGhost { title: drag.title.clone(), tab: true })
                })
                .child(gpui::canvas(move |bounds, _, cx| {
                    let _ = measured.update(cx, |this, _| { this.tab_bounds.insert(tab_id, bounds); });
                }, |_, _, _, _| {}).absolute().inset_0())
                .child(crate::icons::icon(if tab.panes.len() > 1 { crate::icons::WIDGET } else { crate::icons::CHAT_ROUND_LINE })
                    .size(px(13.0)).flex_none().text_color(theme.text_muted))
                .child(div().min_w_0().truncate().child(title))
                .child(div().id(SharedString::from(format!("tab-close-{}", tab_id.0)))
                    .role(gpui::Role::Button).aria_label("Close tab").size(px(18.0)).flex_none()
                    .flex().items_center().justify_center().rounded(px(3.0)).invisible().group_hover("workspace-tab", |s| s.visible())
                    .hover(|s| s.bg(theme.surface_raised).text_color(theme.text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        // Every nested CLI must acknowledge closure before its pane disappears.
                        let panes: Vec<_> = this.layout.views.get(&id).and_then(|v| v.tabs.get(&tab_id))
                            .map(|tab| tab.panes.keys().copied().collect()).unwrap_or_default();
                        for pane in panes { this.close(pane, cx); }
                    }))
                    .child(crate::icons::icon(crate::icons::CLOSE).size(px(10.0))))
                .when(insert_before, |e| e.child(div().absolute().bg(theme.accent)
                    .when(top, |e| e.left_0().top(px(5.0)).bottom(px(5.0)).w(px(2.0)))
                    .when(!top, |e| e.top_0().left(px(5.0)).right(px(5.0)).h(px(2.0))))));
        }
        rail = rail.child(div().relative().flex_none()
            .child(self.button(format!("view-new-tab-{}", id.0), "+", move |this, _, cx| {
                this.layout.focus_view(id).ok(); this.new_tab(cx);
            }, cx))
            .when(self.tab_preview == Some((id, None)) && cx.has_active_drag(), |e| e.child(
                div().absolute().bg(theme.accent)
                    .when(top, |e| e.left_0().top_0().bottom_0().w(px(2.0)))
                    .when(!top, |e| e.top_0().left_0().right_0().h(px(2.0))))));
        let tabs = div().flex().flex_none().min_w_0().min_h_0().gap(px(8.0)).px(px(6.0))
            .border_color(theme.border).bg(theme.bg)
            .when(top, |e| e.flex_row().items_center().h(px(40.0)).border_b_1())
            .when(!top, |e| e.flex_col().w(px(view.rail_width as f32)).py(px(5.0)))
            .child(rail)
            .when(top && view.tabs[&view.active_tab_id].panes.len() == 1, |e| e.child(self.render_pane_tools(active_pane, cx)));
        let now = Instant::now();
        let target = &view.tabs[&view.active_tab_id].root;
        let motion = self.tab_motion.entry((id, view.active_tab_id))
            .or_insert_with(|| TreeMotion::new(target, now));
        motion.update(target, now, self.snap_motion || cx.reduce_motion());
        self.animating |= motion.active(now);
        let tree = motion.sample(now);
        let content = self.render_tree(&tree, Some((id, view.active_tab_id)), &|this, pane, cx| this.render_pane(pane, cx), cx);
        div().id(SharedString::from(format!("split-view-{}", id.0)))
            .relative().size_full().min_w_0().min_h_0().flex().overflow_hidden()
            .when(view.tab_placement == TabPlacement::Top, |e| e.flex_col())
            .when(view.tab_placement == TabPlacement::Left, |e| e.flex_row())
            .on_drag_move::<LayoutDrag>(cx.listener(move |this, event: &gpui::DragMoveEvent<LayoutDrag>, _, cx| {
                let offset = event.event.position - event.bounds.origin;
                let in_rail = if top { offset.y < px(40.0) } else { offset.x < px(view.rail_width as f32) };
                let preview = if in_rail { None } else { outside_ring(event.event.position, event.bounds).map(|direction| (id, direction)) };
                if (preview.is_some() || this.outer_preview.is_some_and(|(target, _)| target == id))
                    && this.outer_preview != preview { this.outer_preview = preview; cx.notify(); }
            }))
            .on_drop::<LayoutDrag>(cx.listener(move |this, drag: &LayoutDrag, _, cx| {
                this.drop_outer(drag, cx);
            }))
            .on_drag_move::<RailResize>(cx.listener(move |this, event: &gpui::DragMoveEvent<RailResize>, _, cx| {
                if event.drag(cx).0 != id { return; }
                let width = f32::from(event.event.position.x - event.bounds.origin.x)
                    .clamp(96.0, (f32::from(event.bounds.size.width) * 0.65).clamp(96.0, 600.0));
                this.apply(|layout| layout.compose(layout.revision, |draft| {
                    draft.views.get_mut(&id).unwrap().rail_width = width as f64;
                    Ok(())
                }), cx);
            }))
            .child(tabs)
            .when(view.tab_placement == TabPlacement::Left, |e| e.child(
                div().id(SharedString::from(format!("rail-divider-{}", id.0)))
                    .w(px(5.0)).h_full().flex_none().cursor_col_resize().bg(theme.border)
                    .hover(|s| s.bg(theme.border_strong))
                    .on_drag(RailResize(id), |_, _, _, cx| cx.new(|_| Ghost))))
            .child(div().flex_1().min_w_0().min_h_0().child(content))
            .when_some(self.outer_preview.filter(|(view, _)| *view == id && cx.has_active_drag()), |e, (_, direction)| {
                e.child(div().absolute().border_2().border_color(theme.accent).bg(theme.accent.opacity(0.18))
                    .rounded(px(8.0)).flex().items_center().justify_center()
                    .when(direction == Direction::Left, |e| e.left_0().top_0().bottom_0().w(gpui::relative(0.5)))
                    .when(direction == Direction::Right, |e| e.right_0().top_0().bottom_0().w(gpui::relative(0.5)))
                    .when(direction == Direction::Up, |e| e.left_0().right_0().top_0().h(gpui::relative(0.5)))
                    .when(direction == Direction::Down, |e| e.left_0().right_0().bottom_0().h(gpui::relative(0.5)))
                    .child(div().rounded(px(5.0)).px(px(10.0)).py(px(6.0)).bg(theme.surface_overlay)
                        .text_size(px(12.0)).text_color(theme.text).child("Create independent view")))
            }).into_any_element()
    }

    fn render_tree<T: Copy>(&mut self, node: &VisualNode<T>, tier: Option<(ViewId, TabId)>, leaf: &impl Fn(&mut Self, T, &mut Context<Self>) -> AnyElement, cx: &mut Context<Self>) -> AnyElement {
        match node {
            VisualNode::Leaf(content) => leaf(self, *content, cx),
            VisualNode::Empty => Empty.into_any_element(),
            VisualNode::Split { horizontal, ratio, path, first, second, .. } => {
                let horizontal = *horizontal;
                let ratio = *ratio;
                let interactive = path.is_some();
                let path = path.clone().unwrap_or_default();
                let first = self.render_tree(first, tier, leaf, cx);
                let second = self.render_tree(second, tier, leaf, cx);
                let bounds = std::rc::Rc::new(std::cell::Cell::new(Bounds::<Pixels>::default()));
                let measured = bounds.clone();
                let drag_path = path.clone();
                let reset_path = path.clone();
                let handle = div().id(SharedString::from(format!("split-{tier:?}-{path:?}{}", if interactive { "" } else { "-exit" })))
                    .debug_selector(|| format!("split-{tier:?}-{path:?}{}", if interactive { "" } else { "-exit" }))
                    .flex_none().flex().items_center().justify_center()
                    .when(horizontal, |e| e.w(px(if interactive { 6.0 } else { 0.0 })).h_full().cursor_col_resize())
                    .when(!horizontal, |e| e.h(px(if interactive { 6.0 } else { 0.0 })).w_full().cursor_row_resize())
                    .when(interactive, |e| e.hover(|style| style.bg(Theme::of(cx).accent.opacity(0.5))))
                    .child(div().bg(Theme::of(cx).border)
                        .when(horizontal, |e| e.w(px(1.0)).h_full())
                        .when(!horizontal, |e| e.h(px(1.0)).w_full()))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        if interactive && event.click_count == 2 {
                            this.apply(|layout| if let Some((view, tab)) = tier {
                                layout.set_pane_ratio(view, tab, &reset_path, 0.5)
                            } else { layout.set_view_ratio(&reset_path, 0.5) }, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_drag(SplitResize { tier, path: drag_path }, move |_, _, _, cx| {
                        cx.new(|_| Ghost)
                    });
                // The container handles movement so both branch sizes remain live under the pointer.
                div().id(SharedString::from(format!("split-container-{tier:?}-{path:?}{}", if interactive { "" } else { "-exit" })))
                    .relative().size_full().min_w_0().min_h_0().flex().overflow_hidden()
                    .when(horizontal, |e| e.flex_row()).when(!horizontal, |e| e.flex_col())
                    .child(gpui::canvas(move |rect, _, _| measured.set(rect), |_, _, _, _| {}).absolute().inset_0())
                    .on_drag_move::<SplitResize>(cx.listener(move |this, event: &gpui::DragMoveEvent<SplitResize>, _, cx| {
                        let drag = event.drag(cx);
                        if !interactive || drag.tier != tier || drag.path != path { return; }
                        let rect = bounds.get();
                        let offset = event.event.position - rect.origin;
                        let ratio = if horizontal { f32::from(offset.x) / f32::from(rect.size.width) }
                            else { f32::from(offset.y) / f32::from(rect.size.height) };
                        if !ratio.is_finite() { return; }
                        let ratio = ratio.clamp(0.1, 0.9) as f64;
                        this.snap_motion = true;
                        this.apply(|layout| if let Some((view, tab)) = tier {
                            layout.set_pane_ratio(view, tab, &path, ratio)
                        } else { layout.set_view_ratio(&path, ratio) }, cx);
                    }))
                    .child(div().flex_grow(ratio).flex_basis(px(0.0)).min_w_0().min_h_0()
                        .when(horizontal, |e| e.h_full()).when(!horizontal, |e| e.w_full()).child(first))
                    .child(handle)
                    .child(div().flex_grow(1.0 - ratio).flex_basis(px(0.0)).min_w_0().min_h_0()
                        .when(horizontal, |e| e.h_full()).when(!horizontal, |e| e.w_full()).child(second))
                    .into_any_element()
            }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.control_task.is_none() && self.source.read(cx).engine().is_some() {
            self.start_control(cx);
        }
        if !cx.has_active_drag() { self.tab_preview = None; self.drop_preview = None; self.outer_preview = None; }
        let now = Instant::now();
        self.view_motion.update(&self.layout.root, now, self.snap_motion || cx.reduce_motion());
        self.animating = self.view_motion.active(now);
        let tree = self.view_motion.sample(now);
        let content = self.render_tree(&tree, None, &|this, view, cx| this.render_view(view, cx), cx);
        self.snap_motion = false;
        self.tab_motion.retain(|(view, tab), _| self.layout.views.get(view).is_some_and(|v| v.tabs.contains_key(tab)));
        if self.animating { window.request_animation_frame(); }
        if self.publish_active {
            self.publish_active = false;
            if let Some(id) = self.layout.active_pane_id() {
                self.ensure_pane(id, cx);
                let pane = self.panes[&id].chat.read(cx);
                let chat = pane.state.read(cx).selected_chat.clone();
                self.source_selected = chat.clone();
                cx.emit(WorkspaceEvent::ActivePane { chat, transcript: pane.transcript.clone(), composer: pane.composer.clone() });
            }
        }
        if self.focus_pending {
            self.focus_pending = false;
            if let Some(runtime) = self.layout.active_pane_id().and_then(|id| self.panes.get(&id)) {
                let focus = match &runtime.item {
                    TabItem::Chat(chat) => chat.focus_handle(cx),
                    TabItem::Terminal(terminal) => terminal.read(cx).focus_handle(),
                };
                window.focus(&focus, cx);
            }
        }
        // Park mounted panes without losing selection, undo, scroll or drafts.
        // Restored tabs remain lazy because their entities do not exist yet.
        self.panes.retain(|id, _| self.layout.pane(*id).is_some());
        let pane_menu = self.render_pane_menu(cx);
        div().id("noches-workspace").key_context("NochesWorkspace").size_full().min_w_0().min_h_0().flex().flex_col()
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| this.split(Direction::Right, false, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| this.split(Direction::Down, false, cx)))
            .on_action(cx.listener(|this, _: &SplitViewRight, _, cx| this.split(Direction::Right, true, cx)))
            .on_action(cx.listener(|this, _: &SplitViewDown, _, cx| this.split(Direction::Down, true, cx)))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.new_tab(cx)))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| { if let Some(id) = this.layout.active_pane_id() { this.close(id, cx); } }))
            .on_action(cx.listener(|this, _: &ToggleSessionView, _, cx| { if let Some(id) = this.layout.active_pane_id() { this.toggle(id, cx); } }))
            .when_some(self.control_pending_deletion(), |element, description| element.child(
                div().px(px(12.0)).py(px(8.0)).flex().items_center().gap(px(10.0))
                    .bg(Theme::of(cx).surface_overlay).text_color(Theme::of(cx).text).text_size(px(12.0))
                    .child(div().flex_1().child(description))
                    .child(self.button("control-delete-cancel".into(), "Cancel", |this, _, cx| this.control_cancel_delete(cx), cx))
                    .child(self.button("control-delete-confirm".into(), "Delete", |this, _, cx| this.control_confirm_delete(cx), cx))))
            .when_some(self.error.clone(), |element, error| element.child(
                div().px(px(10.0)).py(px(6.0)).text_size(px(12.0)).text_color(Theme::of(cx).danger).child(error)))
            .child(div().flex_1().min_w_0().min_h_0().child(content))
            .children(pane_menu)
    }
}

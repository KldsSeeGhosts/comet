//! Glue between the shell and the pane host ([`crate::pane`]): the per-frame
//! workspace snapshot for the renderer, the split/focus/close handlers behind
//! the `workspace::` actions, the WS3 divider-drag/equalize commit path, the
//! tool picker (the verified ⌘D contract), tab close, the pane-header context
//! menu, the per-pane chat surfaces (every Chat pane owns its transcript +
//! composer) plus the focus→selection retarget loop that keeps global
//! routing on the focused pane, and the WS4 tab/pane drag state machine
//! (source → per-sample [`resolve_drop`] preview → commit on mouse-up).
//!
//! This lives in the shell module tree (like `tabs.rs`/`spaces.rs`) because it
//! reads Shell's private fields; everything structural sits in `crate::pane`.

use super::project_icon::ProjectIconRequest;
use super::*;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::pane::chrome::{self, TabChip, tab_mark};
use crate::pane::hit_test::{self, DragSource, DropPlan};
use crate::pane::render::{
    OUTLET_PAD_PX, OUTLET_TOP_PAD_PX, PaneSnap, ViewSnap, WorkspaceSnap, workspace_outlet,
};
use crate::pane::{
    DIVIDER_SEAM_PX, DividerTarget, DragSplitState, EQUALIZE_RATIO, PaneChatSurface, ToolKind,
    ratio_from_pointer,
};
use crate::state::ChatTarget;
use crate::status_palette::SessionState;
use zeron_workspace::{Direction, PaneId, PaneMode, TabId, ViewId};

/// The pane header's metadata for a bound session: the mono
/// `{project}:{branch}` (plus ` · {device}` for a remote device) context line
/// and the session's display state. Computed here because it reads AppState;
/// the shapes match the sidebar's line-2 derivation in `shell/spaces.rs`. An
/// unbound pane (no session) carries no metadata.
fn pane_meta(session: Option<&str>, state: &AppState) -> chrome::PaneMeta {
    let Some(chat) = session.and_then(|id| state.chats.iter().find(|chat| chat.id == id)) else {
        return chrome::PaneMeta::empty();
    };
    // Project: the owning space's display name; project-less sessions read as
    // their home-dir cwd `~` (or `?` when the space is unknown).
    let space = state.space_for_chat(chat);
    let mut context = match (space, chat.space_id.as_deref()) {
        (Some(space), _) => space.display_name().to_string(),
        (None, None) => "~".to_string(),
        (None, Some(_)) => "?".to_string(),
    };
    // The branch shows whenever the engine has stamped one - main-checkout
    // sessions included, not just worktrees.
    if let Some(branch) = crate::change_requests::conversation_branch(chat, &state.spaces)
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        context.push(':');
        context.push_str(branch);
    }
    // Device only for a session that is NOT on this machine.
    let local_device_id = state.local_device_id.as_deref();
    let remote_device = (local_device_id != Some(chat.device_id.as_str()))
        .then(|| state.device_name(&chat.device_id))
        .flatten();
    if let Some(device) = remote_device {
        context.push_str(" · ");
        context.push_str(device);
    }
    let now = Utc::now();
    let status = state.display_status_for(chat, now);
    let undelivered = state.send_undelivered(&chat.id, now);
    let queued = state.send_queued(&chat.id, now) && !undelivered;
    chrome::PaneMeta {
        context: Some(context.into()),
        // Send truth decides the state (undelivered -> failed, degraded
        // delivery -> queued), same as the sidebar's slot.
        state: SessionState::resolve(status, queued, undelivered),
    }
}

/// The pane header's identity mark: a bound chat's harness brand icon + tint
/// when the chat names one (rule 1: Claude orange, the rest monochrome by
/// design), else the provider/mode tab mark. For an unbound chat pane the
/// composer's harness pick supplies the identity (same mark the model chip
/// shows); `composer_harness` carries that pick or None. Tab chips always
/// keep [`tab_mark`].
fn header_mark(
    chat: Option<&zeron_proto::Chat>,
    mode: PaneMode,
    provider_key: Option<&str>,
    composer_harness: Option<zeron_proto::HarnessId>,
) -> chrome::TabMark {
    let harness = chat
        .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
        .or(composer_harness);
    match harness {
        Some(harness) => {
            let (icon, tint) = crate::pickers::harness_brand_icon(harness);
            chrome::TabMark {
                icon: Some(icon),
                tint,
            }
        }
        None => tab_mark(mode, provider_key),
    }
}

impl Shell {
    /// Whether the content area renders the workspace tree: any split, extra
    /// tab, or extra pane beyond the untouched default. False = today's exact
    /// single-chat code path (the parity gate).
    ///
    /// The per-pane `chat_surfaces` cache is deliberately NOT part of this
    /// gate. It keeps a survivor composer (and unsent draft) alive when a
    /// split collapses, but routing on that inventory latched the opaque
    /// pane-island surface after every split→close cycle (issue #8). The
    /// collapse handoff in [`Self::promote_trivial_chat_surface_to_dock`]
    /// moves that draft into the shared dock and clears the cache so a
    /// single session always takes the glass path.
    pub(super) fn workspace_mode(&self) -> bool {
        !self.solo_session && !self.workspace.is_trivial()
    }

    /// Show the normal full-width dock without changing the workspace tree.
    /// The first split may have adopted the dock's composer as a pane's live
    /// composer. Detach it before selecting None so that pane keeps its draft,
    /// attachments, and in-flight send intact.
    pub(super) fn reveal_workspace_session(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        let owner = if self.find_pane_with_session(chat_id).is_some() {
            Some(self.active_workspace_space.clone())
        } else {
            let native_space = self.state.read(cx).chats.iter()
                .find(|chat| chat.id == chat_id)
                .and_then(|chat| chat.space_id.as_deref());
            self.workspace_layouts.space_for_session(chat_id, native_space)
        };
        let Some(owner) = owner else { return false; };
        self.solo_chat_ids.remove(chat_id);
        self.solo_session = false;
        if self.state.read(cx).selected_space != owner {
            self.state.update(cx, |state, cx| state.select_space(owner, cx));
        }
        true
    }

    pub(super) fn enter_solo_session(&mut self, cx: &mut Context<Self>) {
        if self.solo_session {
            return;
        }
        if !self.workspace.is_trivial() {
            self.ensure_pane_chat_surfaces(cx);
        }
        if self.workspace.chat_surfaces.values().any(|surface| {
            surface.composer.entity_id() == self.composer.entity_id()
        }) {
            self.composer = cx.new(|cx| Composer::new(self.state.clone(), cx));
            self._composer_events =
                Self::dock_composer_events(&self.composer, self.transcript.clone(), cx);
        }
        self.solo_session = true;
        cx.notify();
    }

    /// Whether the shell's transcript-underlay fade may take a TOP ramp.
    /// Legacy single-pane route only: there the primary transcript slides
    /// under the mounted pane header, so content must be fully faded by the
    /// header's bottom edge. The workspace route must NOT take it - its panes
    /// carry their own chrome (header row, tab strip) inside the outlet, and
    /// a zero band across the outlet's top erased those glyphs (the "faded
    /// top bar, no title" split-view bug) while every pane transcript is an
    /// override instance that paints its own scroll-gated fade scope,
    /// replacing the shell's.
    pub(super) fn transcript_underlay_fades_top(&self) -> bool {
        !self.workspace_mode()
    }

    /// Whether pane chrome wins the titlebar band's hit-tests: on a split
    /// workspace the chat drag strip paints UNDER the content row so header
    /// rows and tab strips stay clickable, draggable and hoverable inside the
    /// band; the strip still catches the chrome-free outlet padding and
    /// gutters, so the band keeps dragging the window.
    pub(super) fn pane_chrome_wins_titlebar_band(&self) -> bool {
        matches!(self.route, Route::Chat) && self.workspace_mode()
    }

    /// The workspace tree as the chat outlet. Every Chat-mode pane in the
    /// layout owns a live transcript+composer pair bound to its session -
    /// created lazily here (render pass, like the lazy terminal panel) for
    /// panes across ALL tabs/views, not only visible or focused ones - then
    /// the tree is snapshotted and handed to [`crate::pane::render`]. The
    /// outer dock stays suppressed while `workspace_mode()` holds.
    pub(super) fn render_workspace_outlet(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        self.ensure_pane_chat_surfaces(cx);
        let available = self
            .workspace
            .focused_pane_bounds()
            .map(|bounds| (f32::from(bounds.size.width) - 100.0).max(0.0))
            .unwrap_or_else(|| {
                (self.viewport_width - self.sidebar_now() - self.right_now(cx) - 24.0).max(0.0)
            });
        let action_control =
            self.render_project_actions_control(available, px(self.viewport_height), cx);
        let project_badges = self.render_pane_project_badges(cx);
        // The window's top-left view/pane keeps its leading content past the
        // titlebar cluster while the sidebar tweens shut; everything else
        // gets 0 (right splits never touch the traffic lights).
        let leading_inset = crate::shell::pane_header_leading_inset(
            self.title_bar_content_start(),
            crate::shell::TITLEBAR_ACTION_SLOT_WIDTH * self.titlebar_plus_alpha(cx),
            self.sidebar_now(),
            10.0,
        );
        let snap = Self::workspace_snapshot(
            &self.workspace,
            &self.state,
            action_control,
            project_badges,
            leading_inset,
            cx,
        );
        // WS4: the active drag's preview, converted to outlet-relative space.
        let drag_preview = self.split_drag_preview();
        workspace_outlet(
            cx,
            &theme,
            &snap,
            drag_preview,
            self.sidebar_drop_outlet.clone(),
        )
    }

    /// One 14px project badge per session-bound chat pane, keyed by pane.
    /// `AnyElement` is not cloneable, so the snapshot hands them to the
    /// renderer through a take-once registry (the same pattern as
    /// `action_control`); each pane container removes its own entry.
    fn render_pane_project_badges(
        &self,
        cx: &mut Context<Self>,
    ) -> Rc<RefCell<BTreeMap<PaneId, AnyElement>>> {
        let badges = self
            .workspace
            .chat_pane_sessions()
            .into_iter()
            .filter_map(|(pane, session)| {
                let chat_id = session?;
                let state = self.state.read(cx);
                let badge = match state.chats.iter().find(|chat| chat.id == chat_id) {
                    Some(chat) => {
                        ProjectIconRequest::resolve(state, chat, state.space_for_chat(chat))
                    }
                    // A pane can be bound before its chat lands (the prune
                    // pass that clears dead sessions runs after); the monogram
                    // keeps the header's badge slot from collapsing.
                    None => ProjectIconRequest::monogram_fallback(&chat_id, "Home"),
                };
                Some((pane, self.render_project_icon(badge, 14.0, false, cx)))
            })
            .collect();
        Rc::new(RefCell::new(badges))
    }

    /// The legacy single-pane route's chat identity row: the same pane
    /// header the workspace tree renders per pane, mounted above the
    /// transcript when `workspace_mode()` is off (the workspace outlet then
    /// supplies its own headers). Not closable and not a drag source - the
    /// trivial layout has no splits to re-dock.
    pub(super) fn render_primary_pane_header(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(pane) = self.workspace.focused_pane() else {
            return Empty.into_any_element();
        };
        let (title, chat_id, meta, mark, badge) = {
            let state = self.state.read(cx);
            let row = state.selected_chat_row();
            let title = row
                .and_then(|chat| chat.title.clone())
                .map(|title| SharedString::from(transcript::single_line(&title)))
                .unwrap_or_else(|| SharedString::from("New session"));
            let chat_id = row.map(|chat| chat.id.clone());
            let meta = pane_meta(chat_id.as_deref(), state);
            let composer_harness = self
                .composer
                .read(cx)
                .pickers()
                .read(cx)
                .effective_harness(cx);
            let mark = header_mark(row, PaneMode::Chat, None, composer_harness);
            let badge = row
                .map(|chat| ProjectIconRequest::resolve(state, chat, state.space_for_chat(chat)));
            (title, chat_id, meta, mark, badge)
        };
        let has_selection = chat_id.is_some();
        let badge = badge.map(|badge| self.render_project_icon(badge, 14.0, false, cx));
        let available =
            (self.viewport_width - self.sidebar_now() - self.right_now(cx) - 24.0).max(0.0);
        let action_control =
            self.render_project_actions_control(available, px(self.viewport_height), cx);
        chrome::pane_header(
            pane,
            title,
            mark,
            &meta,
            badge,
            false,
            // The legacy route has one pane and it is always the active one.
            true,
            has_selection,
            action_control,
            false,
            crate::shell::pane_header_leading_inset(
                self.title_bar_content_start(),
                crate::shell::TITLEBAR_ACTION_SLOT_WIDTH * self.titlebar_plus_alpha(cx),
                self.sidebar_now(),
                10.0,
            ),
            theme,
            cx,
        )
    }

    /// Keep a pane's composer entity alive through navigation and first-send
    /// binding. Replacing it drops drafts, queue editors and in-flight tasks.
    pub(super) fn ensure_pane_chat_surfaces(&mut self, cx: &mut Context<Self>) {
        self.workspace.prune_caches();
        for (pane, _) in self.workspace.chat_pane_sessions() {
            if let Some(composer) = self
                .workspace
                .chat_surfaces
                .get(&pane)
                .map(|surface| surface.composer.clone())
            {
                self.sync_pane_composer_target(pane, &composer, cx);
            }
            let session = self
                .workspace
                .layout
                .pane(pane)
                .and_then(|state| state.session_id.clone());
            if !self.workspace.chat_surfaces.contains_key(&pane) {
                let surface = self.create_pane_chat_surface(pane, session, cx);
                self.workspace.chat_surfaces.insert(pane, surface);
                continue;
            }
            if self.workspace.chat_surfaces[&pane].chat_id != session {
                let surface = self.workspace.chat_surfaces.get_mut(&pane).unwrap();
                surface.chat_id = session.clone();
                surface.transcript = None;
                surface.transcript_events = None;
                let composer = surface.composer.clone();
                composer.update(cx, |composer, cx| {
                    composer.set_target(ChatTarget::Fixed(session.clone()), cx);
                });
            }
            if self.workspace.chat_surfaces[&pane].transcript.is_none()
                && let Some(chat_id) = session
            {
                let (transcript, events) = self.create_pane_transcript(chat_id, cx);
                let surface = self.workspace.chat_surfaces.get_mut(&pane).unwrap();
                surface.transcript = Some(transcript);
                surface.transcript_events = Some(events);
            }
        }
    }

    /// A first send (or its rollback) changes the composer target before
    /// AppState's observer runs. Reconcile that change with its OWN pane,
    /// without overwriting a newer explicit navigation or a replacement
    /// surface. This is also called during ensure so observer order is safe.
    fn sync_pane_composer_target(
        &mut self,
        pane: PaneId,
        composer: &Entity<Composer>,
        cx: &mut Context<Self>,
    ) {
        let ChatTarget::Fixed(target) = &composer.read(cx).target else {
            return;
        };
        let target = target.clone();
        let Some(surface) = self.workspace.chat_surfaces.get(&pane) else {
            return;
        };
        if surface.composer.entity_id() != composer.entity_id() || surface.chat_id == target {
            return;
        }
        let Some(binding) = self.workspace.layout.pane(pane) else {
            return;
        };
        if binding.mode != PaneMode::Chat
            || (binding.session_id != surface.chat_id && binding.session_id != target)
        {
            return;
        }
        if binding.session_id != target {
            let _ = self.workspace.set_pane_session(pane, target.clone());
        }
        let surface = self.workspace.chat_surfaces.get_mut(&pane).unwrap();
        surface.chat_id = target;
        surface.transcript = None;
        surface.transcript_events = None;
        self.note_workspace_mutation(cx);
        cx.notify();
    }

    /// A pane's fixed interactive transcript for `chat_id`, fed by the same
    /// doc watch the subagent tabs use (single-flight). Shared by the ensure
    /// pass and the mint path in [`Self::on_pane_composer_event`].
    fn create_pane_transcript(
        &mut self,
        chat_id: String,
        cx: &mut Context<Self>,
    ) -> (Entity<Transcript>, Subscription) {
        self.state.update(cx, |s, cx| {
            s.watch_subagent_doc(chat_id.clone(), cx);
            s.watch_pane_queue(chat_id.clone(), cx);
        });
        let transcript =
            cx.new(|cx| Transcript::for_session(self.state.clone(), chat_id.clone(), cx));
        let links = Self::session_links(Some(chat_id), cx);
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links)
        });
        let events = cx.subscribe(&transcript, Self::on_transcript_event);
        (transcript, events)
    }

    /// The transcript+composer pair one pane owns. `session` is the layout
    /// binding; `None` is the pane's new-chat canvas (no transcript until a
    /// send mints one).
    fn create_pane_chat_surface(
        &mut self,
        pane: PaneId,
        session: Option<String>,
        cx: &mut Context<Self>,
    ) -> PaneChatSurface {
        let (transcript, transcript_events) = match session.clone() {
            Some(chat_id) => {
                let (transcript, events) = self.create_pane_transcript(chat_id, cx);
                (Some(transcript), Some(events))
            }
            None => (None, None),
        };
        // On the first split, keep the original input, attachments, picker
        // choices and pending send by moving the shared composer into its
        // matching pane. The shared event listener ignores fixed targets.
        let adopt_shared = {
            let composer = self.composer.read(cx);
            !self.solo_session && matches!(composer.target, ChatTarget::Selected)
                && composer.current_key == session.as_deref().unwrap_or_default()
        };
        let composer = if adopt_shared {
            let composer = self.composer.clone();
            composer.update(cx, |composer, cx| {
                composer.set_target(ChatTarget::Fixed(session.clone()), cx);
            });
            composer
        } else {
            cx.new(|cx| Composer::for_pane(self.state.clone(), session.clone(), cx))
        };
        let focused = self.workspace.focused_pane() == Some(pane);
        composer.update(cx, |composer, _| composer.focus_pending = focused);
        // Project-switch parking: rehydrate the draft state parked for this
        // (space, pane). Adopted composers rehydrate too - `restore_draft_state`
        // only fills an EMPTY input, so the adopted dock composer keeps any
        // text it already carries for this key (never clobbered) while a
        // stranded park (the adopt fired after the park) still lands.
        self.rehydrate_pane_draft(pane, &composer, cx);
        let composer_observation = cx.observe(&composer, move |shell: &mut Shell, composer, cx| {
            shell.sync_pane_composer_target(pane, &composer, cx);
        });
        let composer_events = cx.subscribe(&composer, move |shell, composer, event, cx| {
            if shell
                .workspace
                .chat_surfaces
                .get(&pane)
                .is_some_and(|surface| surface.composer.entity_id() == composer.entity_id())
            {
                shell.sync_pane_composer_target(pane, &composer, cx);
                shell.on_pane_composer_event(pane, event, cx);
            }
        });
        PaneChatSurface {
            chat_id: session,
            transcript,
            composer,
            composer_events,
            composer_observation,
            transcript_events,
        }
    }

    /// Snapshot every pane composer's unsent state into
    /// [`Shell::parked_pane_drafts`], keyed by the space being left. Runs
    /// before `install_layout` drops the surfaces; the space is captured
    /// before `restore_workspace_layout` re-points `active_workspace_space`.
    fn park_pane_drafts(&mut self, cx: &mut Context<Self>) {
        let space = self.active_workspace_space.clone();
        let panes: Vec<PaneId> = self.workspace.chat_surfaces.keys().copied().collect();
        for pane in panes {
            let composer = self.workspace.chat_surfaces[&pane].composer.clone();
            let snapshot = composer.read(cx).snapshot_draft_state(cx);
            self.parked_pane_drafts
                .insert((space.clone(), pane), snapshot);
        }
    }

    /// Consume the parked draft for `(active space, pane)` into a freshly
    /// created pane composer. Snapshots from other spaces (and unknown
    /// panes) never match, so ordinary splits and tab adds stay untouched;
    /// entries are removed on restore so the cache stays bounded.
    fn rehydrate_pane_draft(
        &mut self,
        pane: PaneId,
        composer: &Entity<Composer>,
        cx: &mut Context<Self>,
    ) {
        let key = (self.active_workspace_space.clone(), pane);
        if let Some(state) = self.parked_pane_drafts.remove(&key) {
            composer.update(cx, |composer, cx| composer.restore_draft_state(state, cx));
        }
    }

    /// A workspace pane's composer event stream. Unlike the shell composer,
    /// pane composers never drive the global dock transition - `Sent` /
    /// `Queued` bind the layout pane to the minted chat and hand the
    /// own-turn marker to THAT pane's transcript.
    pub(super) fn on_pane_composer_event(
        &mut self,
        pane: PaneId,
        event: &ComposerEvent,
        cx: &mut Context<Self>,
    ) {
        if let ComposerEvent::Sent { chat_id, .. } | ComposerEvent::Queued { chat_id, .. } = event {
            // A queue acknowledgement can arrive after this same composer
            // navigated elsewhere. Do not rebind the pane to the old send.
            let bound = self
                .workspace
                .layout
                .pane(pane)
                .is_some_and(|state| state.session_id.as_deref() == Some(chat_id.as_str()));
            let current = self
                .workspace
                .chat_surfaces
                .get(&pane)
                .is_some_and(|surface| surface.chat_id.as_deref() == Some(chat_id.as_str()));
            if !bound || !current {
                return;
            }
        }
        match event {
            ComposerEvent::NewThreadTransitionStarted => {
                // Route observation drives the global dock once selection
                // commits; workspace panes stay out of that choreography.
                cx.notify();
            }
            ComposerEvent::Sent { chat_id, .. } | ComposerEvent::Queued { chat_id, .. } => {
                // Bind the layout pane to the minted chat (idempotent - the
                // selection sync may already have landed it) and arm the
                // workspace persistence write.
                let bound = self
                    .workspace
                    .layout
                    .pane(pane)
                    .and_then(|state| state.session_id.clone());
                if bound.as_deref() != Some(chat_id.as_str()) {
                    let _ = self.workspace.set_pane_session(pane, Some(chat_id.clone()));
                }
                self.workspace.mark_dirty();
                self.note_workspace_mutation(cx);
                // The composer already bound itself during the mint - keep
                // that entity, mirror the session, and ensure the pane's
                // fixed transcript exists before the marker lands.
                let needs_transcript = self
                    .workspace
                    .chat_surfaces
                    .get(&pane)
                    .is_some_and(|surface| surface.transcript.is_none());
                let transcript = if needs_transcript {
                    Some(self.create_pane_transcript(chat_id.clone(), cx))
                } else {
                    None
                };
                if let Some(surface) = self.workspace.chat_surfaces.get_mut(&pane) {
                    surface.chat_id = Some(chat_id.clone());
                    if let Some((transcript, events)) = transcript {
                        surface.transcript = Some(transcript);
                        surface.transcript_events = Some(events);
                    }
                }
                if let Some(transcript) = self
                    .workspace
                    .chat_surfaces
                    .get(&pane)
                    .and_then(|surface| surface.transcript.clone())
                {
                    transcript.update(cx, |t, cx| match event {
                        ComposerEvent::Sent { message_id, .. } => {
                            t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                        }
                        ComposerEvent::Queued { message_id, .. } => {
                            t.on_own_queued_send(chat_id.clone(), message_id.clone(), cx)
                        }
                        ComposerEvent::NewThreadTransitionStarted
                        | ComposerEvent::WorktreeSetup { .. } => {}
                    });
                }
                cx.notify();
            }
            ComposerEvent::WorktreeSetup {
                chat_id,
                setup_action,
                setup_error,
                target_device_id,
            } => self.attach_worktree_setup(
                chat_id.clone(),
                setup_action.clone(),
                setup_error.clone(),
                target_device_id.clone(),
                cx,
            ),
        }
    }

    /// Flatten the layout into the renderer's immutable snapshot. Titles read
    /// AppState once per frame (chat titles for session-bound panes; labels
    /// and mode names otherwise). Transcript/composer clones come from each
    /// pane's owned [`PaneChatSurface`].
    fn workspace_snapshot(
        workspace: &crate::pane::PaneHost,
        state: &Entity<AppState>,
        action_control: Option<AnyElement>,
        project_badges: Rc<RefCell<BTreeMap<PaneId, AnyElement>>>,
        leading_inset: f32,
        cx: &App,
    ) -> WorkspaceSnap {
        let layout = &workspace.layout;
        let global_focus = layout.active_pane_id();
        // The view leaf at the tree's top-left edge, and (resolved per view
        // below) the active tab's top-left pane.
        let top_left_view = *layout.root.first_leaf();
        let pane_title = |session: &Option<String>,
                          mode: zeron_workspace::PaneMode,
                          label: &Option<String>|
         -> SharedString {
            if let Some(label) = label {
                return label.clone().into();
            }
            match session.as_deref().and_then(|id| {
                state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|chat| chat.id == id)
                    .and_then(|chat| chat.title.clone())
            }) {
                Some(title) => SharedString::from(transcript::single_line(&title)),
                None => SharedString::from(match (session.is_some(), mode) {
                    (_, zeron_workspace::PaneMode::Terminal) => "Terminal",
                    (true, _) => "Session",
                    (false, _) => "New session",
                }),
            }
        };
        let views = layout
            .views
            .iter()
            .map(|(view_id, view)| {
                let active_tab_id = view.active_tab_id;
                let chips = view
                    .ordered_tabs()
                    .iter()
                    .filter_map(|tab_id| view.tabs.get(tab_id).map(|tab| (*tab_id, tab)))
                    .map(|(tab_id, tab)| {
                        let active = tab.panes.get(&tab.active_pane_id);
                        let label = active
                            .map(|pane_state| {
                                pane_title(
                                    &pane_state.session_id,
                                    pane_state.mode,
                                    &pane_state.label,
                                )
                            })
                            .unwrap_or_else(|| SharedString::from("Tab"));
                        let mark = active
                            .map(|pane_state| {
                                tab_mark(pane_state.mode, pane_state.provider_key.as_deref())
                            })
                            .unwrap_or_else(|| tab_mark(PaneMode::Chat, None));
                        TabChip {
                            tab_id,
                            label,
                            active: tab_id == active_tab_id,
                            mark,
                        }
                    })
                    .collect();
                let panes = view
                    .tabs
                    .get(&active_tab_id)
                    .map(|tab| {
                        let top_left_pane = (*view_id == top_left_view)
                            .then(|| *tab.root.first_leaf());
                        tab.panes
                            .iter()
                            .map(|(pane_id, pane_state)| {
                                let surface = workspace.chat_surfaces.get(pane_id);
                                let chat = pane_state.session_id.as_deref().and_then(|id| {
                                    state.read(cx).chats.iter().find(|chat| chat.id == id)
                                });
                                PaneSnap {
                                    pane: *pane_id,
                                    mode: pane_state.mode,
                                    title: pane_title(
                                        &pane_state.session_id,
                                        pane_state.mode,
                                        &pane_state.label,
                                    ),
                                    mark: header_mark(
                                        chat,
                                        pane_state.mode,
                                        pane_state.provider_key.as_deref(),
                                        surface.and_then(|surface| {
                                            surface
                                                .composer
                                                .read(cx)
                                                .pickers()
                                                .read(cx)
                                                .effective_harness(cx)
                                        }),
                                    ),
                                    meta: pane_meta(
                                        pane_state.session_id.as_deref(),
                                        state.read(cx),
                                    ),
                                    has_session: pane_state.session_id.is_some(),
                                    focused: global_focus == Some(*pane_id),
                                    leading_inset: (top_left_pane == Some(*pane_id))
                                        .then_some(leading_inset)
                                        .unwrap_or(0.0),
                                    transcript: surface
                                        .and_then(|surface| surface.transcript.clone()),
                                    composer: surface.map(|surface| surface.composer.clone()),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ViewSnap {
                    view_id: *view_id,
                    // The strip's × closes THIS view; the engine guards the
                    // last one, but hide the control rather than offering a
                    // no-op.
                    closable: layout.views.len() > 1,
                    active_tab_id,
                    chips,
                    leading_inset: (*view_id == top_left_view)
                        .then_some(leading_inset)
                        .unwrap_or(0.0),
                    active_tab_root: view
                        .tabs
                        .get(&active_tab_id)
                        .map(|tab| tab.root.clone())
                        .unwrap_or_else(|| {
                            zeron_workspace::SplitNode::leaf(
                                view.tabs
                                    .values()
                                    .next()
                                    .map(|tab| tab.active_pane_id)
                                    .unwrap_or(PaneId(0)),
                            )
                        }),
                    panes,
                }
            })
            .collect();
        WorkspaceSnap {
            root: layout.root.clone(),
            views,
            pane_bounds: workspace.pane_bounds_handle(),
            view_bounds: workspace.view_bounds_handle(),
            chip_bounds: workspace.chip_bounds_handle(),
            action_control: std::rc::Rc::new(std::cell::RefCell::new(action_control)),
            project_badges,
        }
    }

    // ------------------------------------------------------------------
    // Focus / retarget
    // ------------------------------------------------------------------

    /// Click-to-focus (and the tab-switch fallback): focus the pane in the
    /// engine, then sync selection + keyboard routing to it.
    pub(crate) fn focus_workspace_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.focus_pane(pane).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// Pointer activation changes routing, not the control's keyboard focus.
    /// Text selection, queue editors and pickers retain the focus they chose.
    pub(crate) fn pointer_focus_workspace_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.focused_pane() != Some(pane) {
            if self.workspace.focus_pane(pane).is_err() {
                return;
            }
            self.sync_selection_to_focused_pane(cx);
            self.note_workspace_mutation(cx);
        }
        // A pointer action wins over an earlier, not-yet-painted focus request.
        for surface in self.workspace.chat_surfaces.values() {
            surface
                .composer
                .update(cx, |composer, _| composer.focus_pending = false);
        }
        cx.notify();
    }

    /// Apply one user-requested navigation after any required workspace
    /// restore. Existing sessions keep their pane bindings and only move
    /// focus; a target not yet open replaces the focused pane.
    pub(crate) fn apply_explicit_workspace_navigation(
        &mut self,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat_id) = target.as_deref()
            && let Some(pane) = self.find_pane_with_session(chat_id)
        {
            self.focus_workspace_pane(pane, cx);
            return;
        }

        self.workspace.sync_focused_session(target.as_deref());
        let selected = self.state.read(cx).selected_chat.clone();
        if selected != target {
            self.state
                .update(cx, |state, cx| state.select_chat(target, cx));
        }
        // Rebind off the render path: realize the replacement surface now and
        // hand it keyboard focus, rather than pointing focus at the composer
        // the binding change just invalidated.
        self.focus_composer(cx);
        self.note_workspace_mutation(cx);
    }

    /// Tab chip click: make the tab active (and its view), then retarget to
    /// that tab's active pane.
    pub(crate) fn switch_workspace_tab(
        &mut self,
        view: ViewId,
        tab: TabId,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.focus_tab(view, tab).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// Pane header ×: engine `close_pane` (an emptied tab closes; an emptied
    /// view closes; the LAST pane/view errors and this no-ops silently).
    pub(crate) fn close_workspace_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.close_pane(pane).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// Tab chip ×: engine `close_tab`. Engine semantics (documented WS3
    /// deviation - the empty-view launcher needs engine changes that are out
    /// of this workstream's scope): closing a view's LAST tab closes the
    /// VIEW, and closing the last remaining view's last tab ERRORS (no-op),
    /// so the app can never be left without a view.
    pub(crate) fn close_workspace_tab(&mut self, view: ViewId, tab: TabId, cx: &mut Context<Self>) {
        if self.workspace.close_tab(view, tab).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    // ------------------------------------------------------------------
    // Divider drag / equalize (WS3)
    // ------------------------------------------------------------------

    /// Divider press: latches the drag so hover fades pause until release.
    /// The ratio math is absolute (pointer position within the split
    /// container's bounds), so no drag anchor is needed. The divider sits
    /// outside every pane container and occludes, so this press never
    /// focuses the pane under it.
    pub(crate) fn begin_divider_drag(&mut self, cx: &mut Context<Self>) {
        self.divider_dragging = true;
        cx.notify();
    }

    /// Hover fades for a divider (rendered from `pane/render.rs`, which
    /// cannot see Shell's private fields): suppressed while any divider drag
    /// is live so the strip never re-fades mid-drag.
    pub(crate) fn note_divider_hover(&mut self, key: &str, hovered: bool) {
        if !self.divider_dragging {
            crate::motion::set_hover(key, hovered, self.reduced_motion);
        }
    }

    /// Mouse-up (or out) releases the latch. No tween on release: the last
    /// dragged ratio IS the resting state (direct manipulation). WS5: the
    /// drag's per-sample ratio commits latched dirty but never armed a save
    /// (mid-gesture flushes are forbidden) - arm it here, on the commit.
    pub(crate) fn end_divider_drag(&mut self, cx: &mut Context<Self>) {
        if !self.divider_dragging {
            return;
        }
        self.divider_dragging = false;
        self.schedule_workspace_layout_save(cx);
        cx.notify();
    }

    /// `on_drag_move` on a split container: convert the pointer sample into a
    /// live ratio commit for the node the payload names. Containers that do
    /// not own the divider (ancestors in capture phase) filter by target.
    pub(crate) fn apply_divider_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::pane::DividerDrag>,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        let target = drag.target.clone();
        let horizontal = drag.horizontal;
        let (origin, length, pointer) = if horizontal {
            (
                f32::from(event.bounds.origin.x),
                f32::from(event.bounds.size.width),
                f32::from(event.event.position.x),
            )
        } else {
            (
                f32::from(event.bounds.origin.y),
                f32::from(event.bounds.size.height),
                f32::from(event.event.position.y),
            )
        };
        let Some(ratio) = ratio_from_pointer(pointer, origin, length, DIVIDER_SEAM_PX) else {
            return;
        };
        self.apply_divider_ratio(&target, ratio, cx);
    }

    /// Double-click on a divider: equalize that node to 0.5/0.5 (§1). Direct
    /// snap - the manual-tween plumbing is keyed to the shell's width tweens;
    /// a ratio spring is deferred with the WS6 motion pass.
    pub(crate) fn equalize_divider(&mut self, target: &DividerTarget, cx: &mut Context<Self>) {
        self.apply_divider_ratio(target, EQUALIZE_RATIO, cx);
    }

    fn apply_divider_ratio(&mut self, target: &DividerTarget, ratio: f64, cx: &mut Context<Self>) {
        let result = match target {
            DividerTarget::View { path } => self.workspace.set_view_ratio(path, ratio),
            DividerTarget::Pane { view, tab, path } => {
                self.workspace.set_pane_ratio(*view, *tab, path, ratio)
            }
        };
        if result.is_ok() {
            // Mid-drag samples hit the gesture guard inside; the drag end
            // arms the save. Equalize (double-click) arms immediately.
            self.note_workspace_mutation(cx);
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // WS4: tab/pane drag & drop (tab-to-edge splits, drop ring, re-dock,
    // cross-view moves)
    // ------------------------------------------------------------------

    /// The pure resolution's input: the paint-time registries flattened into
    /// a [`hit_test::WorkspaceGeometry`]. Stale registry entries (a frame
    /// behind an engine change) filter out against the live layout, so a
    /// mid-drag mutation resolves against what is actually on screen. The
    /// outlet hitbox (re-read every sample, so a resize mid-drag cannot drag
    /// stale edges along) shrinks by the outlet's own padding - the same
    /// constants `pane::render` lays out with - into the content region the
    /// view regions paint into, which is the edge boundary detection compares
    /// against.
    fn workspace_geometry(
        &self,
        outlet: gpui::Bounds<gpui::Pixels>,
    ) -> hit_test::WorkspaceGeometry {
        let content = hit_test::outlet_content(
            &hit_test::Rect::from_bounds(outlet),
            OUTLET_PAD_PX,
            OUTLET_TOP_PAD_PX,
        );
        let layout = &self.workspace.layout;
        let panes = self
            .workspace
            .pane_bounds
            .borrow()
            .iter()
            .filter_map(|(pane, bounds)| {
                let (view, tab) = layout.pane_location(*pane)?;
                // Only include panes from the view's active tab - stale
                // bounds from inactive tabs must not participate in hit-testing.
                if !layout
                    .views
                    .get(&view)
                    .is_some_and(|v| v.active_tab_id == tab)
                {
                    return None;
                }
                Some(hit_test::PaneRect {
                    pane: *pane,
                    view,
                    tab,
                    rect: hit_test::Rect::from_bounds(*bounds),
                })
            })
            .collect();
        let views = self
            .workspace
            .view_bounds
            .borrow()
            .iter()
            .filter_map(|(view, bounds)| {
                let tabs = layout.views.get(view)?.tabs.len();
                Some(hit_test::ViewRect {
                    view: *view,
                    rect: hit_test::Rect::from_bounds(*bounds),
                    tab_count: tabs,
                })
            })
            .collect();
        let mut strips: std::collections::BTreeMap<ViewId, Vec<hit_test::ChipRect>> =
            Default::default();
        for ((view, tab), bounds) in self.workspace.chip_bounds.borrow().iter() {
            if layout
                .views
                .get(view)
                .is_some_and(|v| v.tabs.contains_key(tab))
            {
                strips.entry(*view).or_default().push(hit_test::ChipRect {
                    tab: *tab,
                    rect: hit_test::Rect::from_bounds(*bounds),
                });
            }
        }
        for chips in strips.values_mut() {
            // Display order is left→right within a strip row.
            chips.sort_by(|a, b| {
                a.rect
                    .x
                    .total_cmp(&b.rect.x)
                    .then_with(|| a.tab.cmp(&b.tab))
            });
        }
        hit_test::WorkspaceGeometry {
            content,
            panes,
            views,
            strips: strips.into_iter().collect(),
        }
    }

    /// `on_drag_move` on the workspace outlet: resolve the sample purely and
    /// store it. The preview overlay + ghost re-render off this state; the
    /// resolution's anchor pane feeds the next sample's flip smoothing
    /// ([`hit_test::FLIP_SMOOTH_PX`]).
    pub(crate) fn apply_split_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::pane::TabSplitDrag>,
        cx: &mut Context<Self>,
    ) {
        let (source, session_id, pointer) = {
            let drag = event.drag(cx);
            (drag.source, drag.session_id.clone(), event.event.position)
        };
        let anchor = self.split_drag.as_ref().and_then(|s| s.resolution.anchor);
        let geom = self.workspace_geometry(event.bounds);
        let x = f32::from(pointer.x);
        let y = f32::from(pointer.y);
        // A sidebar session already open anywhere in the layout always
        // resolves to focusing its existing pane - never a duplicate.
        let resolution = self
            .existing_sidebar_session_resolution(session_id.as_deref(), &geom, x, y)
            .unwrap_or_else(|| hit_test::resolve_drop(&geom, x, y, source, anchor));
        let next = DragSplitState {
            source,
            session_id,
            root_bounds: event.bounds,
            resolution,
        };
        if self.split_drag.as_ref() != Some(&next) {
            self.split_drag = Some(next);
            cx.notify();
        }
    }

    /// The drag resolution for a sidebar session that is already bound to a
    /// pane somewhere in the layout: focus that pane (`DropPlan::FocusPane`)
    /// rather than minting a second binding. Previews the pane's painted
    /// rect as a `FullTarget` wash when the geometry knows it (a pane in an
    /// inactive tab has no painted rect - the commit activates it).
    pub(crate) fn existing_sidebar_session_resolution(
        &self,
        session_id: Option<&str>,
        geometry: &hit_test::WorkspaceGeometry,
        x: f32,
        y: f32,
    ) -> Option<hit_test::DropResolution> {
        if !x.is_finite() || !y.is_finite() || !geometry.content.contains(x, y) {
            return None;
        }
        let pane = self.find_pane_with_session(session_id?)?;
        let preview =
            geometry
                .panes
                .iter()
                .find(|p| p.pane == pane)
                .map(|p| hit_test::DropPreview {
                    rect: p.rect,
                    kind: hit_test::PreviewKind::FullTarget,
                });
        Some(hit_test::DropResolution {
            plan: DropPlan::FocusPane { pane },
            preview,
            anchor: Some(pane),
        })
    }

    /// `on_drag_move` on the SINGLE-PANE content area (the workspace outlet
    /// is not rendered there, so the legacy container is the only drag
    /// surface): the whole area is the focused pane, so the sample resolves
    /// through [`hit_test::resolve_single_pane_drop`] - the same outer-20%
    /// edge rule as the workspace matrix, with the center resolving to a
    /// tab-joining `MoveIntoPane` - and stores the same [`DragSplitState`]
    /// the workspace path uses, so the preview overlay paints and
    /// [`Self::accept_sidebar_session_drop`] commits the resolved plan.
    /// Workspace mode must win when both surfaces are live (the outlet owns
    /// the geometry); only sidebar sessions drag here (chips and headers
    /// exist only inside the outlet).
    pub(crate) fn apply_single_pane_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::pane::TabSplitDrag>,
        cx: &mut Context<Self>,
    ) {
        let (source, session_id, pointer) = {
            let drag = event.drag(cx);
            (drag.source, drag.session_id.clone(), event.event.position)
        };
        if self.workspace_mode() || source != DragSource::SidebarSession {
            return;
        }
        let outlet = hit_test::Rect::from_bounds(event.bounds);
        let anchor = self.split_drag.as_ref().and_then(|s| s.resolution.anchor);
        let x = f32::from(pointer.x);
        let y = f32::from(pointer.y);
        let located = self
            .workspace
            .focused_pane()
            .and_then(|pane| self.workspace.layout.pane_location(pane).map(|l| (pane, l)));
        let resolution = match located {
            Some((pane, (view, tab))) => self
                .existing_sidebar_session_resolution(
                    session_id.as_deref(),
                    &hit_test::single_pane_geometry(&outlet, pane, view, tab),
                    x,
                    y,
                )
                .unwrap_or_else(|| {
                    hit_test::resolve_single_pane_drop(&outlet, pane, view, tab, x, y, anchor)
                }),
            None => hit_test::DropResolution::none(),
        };
        let next = DragSplitState {
            source,
            session_id,
            root_bounds: event.bounds,
            resolution,
        };
        if self.split_drag.as_ref() != Some(&next) {
            self.split_drag = Some(next);
            cx.notify();
        }
    }

    /// Resolve a sidebar pointer against the same geometry and plans used by
    /// GPUI tab/header drags. The outlet's canvas records its actual bounds at
    /// paint time, so scrolling/resizing during a drag never uses an estimated
    /// coordinate. Only a changed resolution schedules a frame.
    fn resolve_sidebar_pointer(
        &mut self,
        session_id: &str,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.sidebar_drop_outlet.get() else {
            self.cancel_split_drag(cx);
            return;
        };
        let source = DragSource::SidebarSession;
        let x = f32::from(position.x);
        let y = f32::from(position.y);
        let anchor = self.split_drag.as_ref().and_then(|s| s.resolution.anchor);
        let resolution = if self.workspace_mode() {
            let geom = self.workspace_geometry(bounds);
            self.existing_sidebar_session_resolution(Some(session_id), &geom, x, y)
                .unwrap_or_else(|| hit_test::resolve_drop(&geom, x, y, source, anchor))
        } else {
            let outlet = hit_test::Rect::from_bounds(bounds);
            self.workspace
                .focused_pane()
                .and_then(|pane| {
                    self.workspace
                        .layout
                        .pane_location(pane)
                        .map(|(view, tab)| (pane, view, tab))
                })
                .map(|(pane, view, tab)| {
                    self.existing_sidebar_session_resolution(
                        Some(session_id),
                        &hit_test::single_pane_geometry(&outlet, pane, view, tab),
                        x,
                        y,
                    )
                    .unwrap_or_else(|| {
                        hit_test::resolve_single_pane_drop(&outlet, pane, view, tab, x, y, anchor)
                    })
                })
                .unwrap_or_else(hit_test::DropResolution::none)
        };
        let next = DragSplitState {
            source,
            session_id: Some(session_id.to_owned()),
            root_bounds: bounds,
            resolution,
        };
        if self.split_drag.as_ref() != Some(&next) {
            self.split_drag = Some(next);
            cx.notify();
        }
    }

    pub(super) fn move_sidebar_session_pointer(
        &mut self,
        event: &gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        // A release outside the window may not deliver MouseUp here. Heal on
        // the first subsequent move without the left button, and never keep a
        // stuck drag cursor or stale preview on re-entry.
        if event.pressed_button != Some(gpui::MouseButton::Left) || cx.has_active_drag() {
            if self.sidebar_session_pointer.take().is_some() {
                self.cancel_split_drag(cx);
                cx.notify();
            }
            return;
        }
        let Some(pointer) = self.sidebar_session_pointer.as_mut() else {
            return;
        };
        let was_dragging = pointer.dragging;
        if pointer.advance(event.position) {
            let session_id = pointer.session_id.clone();
            if !was_dragging {
                // Switch to the native closed-hand cursor once, not on every
                // pointer sample. The target highlight keeps its own equality
                // guard in resolve_sidebar_pointer.
                cx.notify();
            }
            self.resolve_sidebar_pointer(&session_id, event.position, cx);
        }
    }

    /// Re-resolve on mouse-up: the last move can precede a resize, an outlet
    /// transition or a jump across zones. A release outside produces None and
    /// must never focus an already-open session.
    pub(super) fn release_sidebar_session_pointer(
        &mut self,
        event: &gpui::MouseUpEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pointer) = self.sidebar_session_pointer.take() else {
            return false;
        };
        if !pointer.dragging {
            return false;
        }
        self.sidebar_drag_suppressed_click = true;
        self.resolve_sidebar_pointer(&pointer.session_id, event.position, cx);
        let has_valid_target = self
            .split_drag
            .as_ref()
            .and_then(|state| sidebar_commit_plan(state, &pointer.session_id))
            .is_some();
        if has_valid_target {
            let payload = crate::pane::TabSplitDrag {
                source: DragSource::SidebarSession,
                session_id: Some(pointer.session_id),
                mark: crate::pane::chrome::TabMark {
                    icon: Some(crate::icons::ZERON_LOGO),
                    tint: None,
                },
                title: "".into(),
            };
            self.accept_sidebar_session_drop(&payload, cx);
        } else {
            self.cancel_split_drag(cx);
        }
        // Clear the preview and the window-wide cursor even for an invalid
        // target or an outlet that unmounted during the gesture.
        cx.notify();
        true
    }

    /// The active drag's preview rect and kind in its paint surface's local
    /// coordinates - the workspace outlet in workspace mode, the single-pane
    /// content area otherwise. Both render the same accent overlay from this.
    pub(crate) fn split_drag_preview(
        &self,
    ) -> Option<(gpui::Bounds<gpui::Pixels>, hit_test::PreviewKind)> {
        self.split_drag.as_ref().and_then(preview_bounds)
    }

    /// Mouse-up over the outlet: commit the last resolved plan. Invalid
    /// drops ([`DropPlan::None`]) and engine rejections (the guards below)
    /// no-op; a real commit syncs selection + keyboard routing to the newly
    /// focused pane (the engine ops all focus the moved/dropped content).
    pub(crate) fn commit_split_drop(
        &mut self,
        payload: &crate::pane::TabSplitDrag,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.split_drag.take() else {
            return;
        };
        // The stored resolution belongs to the payload that produced it - a
        // mismatched drop commits nothing.
        if !split_drag_matches_payload(&state, payload) {
            cx.notify();
            return;
        }
        // Sidebar session drags create a new pane bound to the dragged
        // session rather than moving an existing workspace tab/pane.
        if payload.source == DragSource::SidebarSession {
            self.commit_sidebar_split(state.resolution.plan, payload, cx);
            return;
        }
        if self.apply_drop_plan(state.resolution.plan, payload.source) {
            self.retarget_to_focused_pane(cx);
        } else {
            cx.notify();
        }
    }

    /// Commit a sidebar-session drag using the full resolved drop plan.
    /// Each plan variant targets the pane/view/tab the resolver identified,
    /// not the focused pane. A session already open in the tree focuses its
    /// pane instead of minting a second binding for the same chat; sessions
    /// from ANY space dock here - the layout stays owned by the space it was
    /// opened from, and pane focus no longer mutates the selected space.
    fn commit_sidebar_split(
        &mut self,
        plan: DropPlan,
        payload: &crate::pane::TabSplitDrag,
        cx: &mut Context<Self>,
    ) {
        // The duplicate-session focus guard only applies to a VALID target.
        // Without it, an outside/sidebar release focuses an existing session.
        if plan == DropPlan::None {
            cx.notify();
            return;
        }
        // A drop onto the full-width solo surface explicitly returns to the
        // workspace before applying its plan. It must never modify a hidden
        // tree while leaving the destination invisible.
        self.solo_session = false;
        if let Some(session_id) = payload.session_id.as_deref()
            && let Some(pane) = self.find_pane_with_session(session_id)
        {
            // Two panes bound to one chat would double its transcript and
            // split the single-projection selection model; Super reopens the
            // existing home for the session, so focus (and retarget) it.
            let _ = self.workspace.focus_pane(pane);
            self.retarget_to_focused_pane(cx);
            return;
        }
        // The resolver's synthesized already-open plan focuses/retargets
        // without creating anything (the guard above already covers a bound
        // payload session; this arm also reaches tests and defensive paths).
        if let DropPlan::FocusPane { pane } = plan {
            let _ = self.workspace.focus_pane(pane);
            self.retarget_to_focused_pane(cx);
            return;
        }
        let session_id = payload.session_id.clone();
        let succeeded = (|| -> bool {
            let new_pane = crate::pane::chat_pane_state();
            match plan {
                DropPlan::None => return false,
                DropPlan::SplitPane { pane, direction } => self
                    .workspace
                    .layout
                    .split_pane(pane, direction, new_pane)
                    .is_ok_and(|pane_id| self.bind_sidebar_session(pane_id, session_id)),
                DropPlan::SplitView { view, direction } => self
                    .workspace
                    .layout
                    .split_view(view, direction, new_pane)
                    .is_ok_and(|new_view| {
                        let pane_id = self
                            .workspace
                            .layout
                            .views
                            .get(&new_view)
                            .and_then(|v| v.tabs.values().next())
                            .and_then(|tab| tab.panes.keys().next())
                            .copied();
                        match pane_id {
                            Some(pane_id) => self.bind_sidebar_session(pane_id, session_id),
                            None => false,
                        }
                    }),
                // Strip/center drop: the session becomes a new tab, at the
                // hovered insertion point when the strip named one.
                DropPlan::MoveIntoPane { view, tab_before } => {
                    self.add_sidebar_session_tab(view, tab_before, session_id)
                }
                // A ReorderStrip plan from a sidebar source only reaches here
                // defensively - append is the honest fallback.
                DropPlan::ReorderStrip { view, .. } => {
                    self.add_sidebar_session_tab(view, None, session_id)
                }
                DropPlan::FocusPane { .. } => return false,
            }
        })();
        if succeeded {
            if let Some(id) = payload.session_id.as_deref() {
                self.solo_chat_ids.remove(id);
            }
            self.retarget_to_focused_pane(cx);
        } else {
            cx.notify();
        }
    }

    /// Bind the dragged session to a pane the commit just created and focus
    /// it (the engine split ops focus their new pane; this mirrors that for
    /// the raw `layout` calls the resolver targets).
    fn bind_sidebar_session(&mut self, pane_id: PaneId, session_id: Option<String>) -> bool {
        let _ = self.workspace.set_pane_session(pane_id, session_id);
        let _ = self.workspace.focus_pane(pane_id);
        true
    }

    /// Sidebar-session commit on a tab strip (or a pane center): mint a tab
    /// in `view` - at `tab_before` when the strip drop resolved a position -
    /// bind the session to its pane, and focus it.
    fn add_sidebar_session_tab(
        &mut self,
        view: ViewId,
        tab_before: Option<TabId>,
        session_id: Option<String>,
    ) -> bool {
        let Ok(tab_id) = self
            .workspace
            .add_tab_with(view, crate::pane::chat_pane_state())
        else {
            return false;
        };
        if let Some(before) = tab_before {
            let _ = self
                .workspace
                .reorder_tab_in_view(tab_id, view, Some(before));
        }
        let pane_id = self
            .workspace
            .layout
            .views
            .get(&view)
            .and_then(|v| v.tabs.get(&tab_id))
            .and_then(|tab| tab.panes.keys().next())
            .copied();
        match pane_id {
            Some(pane_id) => self.bind_sidebar_session(pane_id, session_id),
            None => false,
        }
    }

    /// Drag ended without a commit (mouse-up outside the outlet - the
    /// sidebar, status bar - or a stray mouse-up after an in-place cancel):
    /// clear the state so no stale preview lingers. Idempotent.
    pub(crate) fn cancel_split_drag(&mut self, cx: &mut Context<Self>) {
        if self.split_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Accept a sidebar session drop on the single-pane content area (the
    /// workspace outlet is not rendered, so this is the entry point for
    /// drag-to-split from the default screen). Commits the plan the preview
    /// tracked via `apply_single_pane_drag_move` - a view/pane split, a new
    /// tab for a center drop, or a focus for an already-open session. A
    /// missing state, a mismatched payload, or `DropPlan::None` is an honest
    /// no-op.
    pub(crate) fn accept_sidebar_session_drop(
        &mut self,
        payload: &crate::pane::TabSplitDrag,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.split_drag.take() else {
            cx.notify();
            return;
        };
        if !split_drag_matches_payload(&state, payload) {
            cx.notify();
            return;
        }
        self.commit_sidebar_split(state.resolution.plan, payload, cx);
    }

    /// The plan → engine-op mapping. Returns whether anything changed.
    /// Every engine error is a silent no-op: the engine commits atomically,
    /// so a rejected drop leaves `validate()` passing and the tree intact.
    fn apply_drop_plan(&mut self, plan: DropPlan, source: DragSource) -> bool {
        let result = match (source, plan) {
            (_, DropPlan::None) => return false,
            // Sidebar session drags are handled in commit_sidebar_split_drop,
            // not here - they need the session_id from the payload. FocusPane
            // is only ever synthesized for that same path.
            (DragSource::SidebarSession, _) | (_, DropPlan::FocusPane { .. }) => return false,
            // Center drop. Same view + append = the no-op restore (activate
            // the tab); anything else moves the tab (append when
            // `tab_before` is `None`).
            (DragSource::TabChip(tab, src_view), DropPlan::MoveIntoPane { view, tab_before }) => {
                if src_view == view && tab_before.is_none() {
                    self.workspace.focus_tab(view, tab)
                } else {
                    self.workspace.reorder_tab_in_view(tab, view, tab_before)
                }
            }
            // Header on a strip: the pane becomes a tab of that view.
            (DragSource::PaneHeader(pane), DropPlan::MoveIntoPane { view, .. }) => {
                self.workspace.pane_header_to_tab(pane, view).map(|_| ())
            }
            // Interior edge: the dragged content becomes the half-pane beside
            // the target (`merge_tab` for whole tabs, `move_pane` for panes).
            (DragSource::TabChip(tab, _), DropPlan::SplitPane { pane, direction }) => {
                self.workspace.merge_tab_into_pane(tab, pane, direction)
            }
            (DragSource::PaneHeader(src), DropPlan::SplitPane { pane, direction }) => {
                self.workspace.move_pane_beside(src, pane, direction)
            }
            // Workspace-outer edge: a new top-level region is minted and the
            // dragged content re-docks into it.
            (DragSource::TabChip(tab, _), DropPlan::SplitView { view, direction }) => self
                .workspace
                .tab_to_adjacent_view(tab, view, direction)
                .map(|_| ()),
            (DragSource::PaneHeader(pane), DropPlan::SplitView { view, direction }) => self
                .workspace
                .pane_to_adjacent_view(pane, view, direction)
                .map(|_| ()),
            // Same-strip drop: reorder (the engine no-ops in-place restores).
            (DragSource::TabChip(tab, _), DropPlan::ReorderStrip { view, before }) => {
                self.workspace.reorder_tab_in_view(tab, view, before)
            }
            // A header "reordered" on a strip still joins it (append; the
            // engine op carries no insertion position).
            (DragSource::PaneHeader(pane), DropPlan::ReorderStrip { view, .. }) => {
                self.workspace.pane_header_to_tab(pane, view).map(|_| ())
            }
        };
        result.is_ok()
    }

    // ------------------------------------------------------------------
    // Split actions + the tab-strip tool picker
    // ------------------------------------------------------------------

    /// ⌘D / ⇧⌘D and the context-menu split rows: split the focused pane and
    /// commit a NEW CHAT pane immediately. (§2's open-the-picker-first
    /// contract is superseded by product decision: every split grows a chat -
    /// terminals have no surface yet anyway - so a popup that could only ever
    /// mint the same pane is friction. The picker lives on solely as the
    /// tab-strip "+" launcher.)
    pub(crate) fn split_workspace_pane(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        self.solo_session = false;
        if self
            .workspace
            .split_focused_pane_with(direction, crate::pane::tool_pane_state(ToolKind::Chat))
            .is_err()
        {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// "+" in a view's tab strip: open the tool picker anchored at the
    /// trigger; a picked row commits as `add_tab` to THAT view (no split).
    /// Re-pressing the SAME trigger toggles the picker closed (checked
    /// before the overlay guard, which would otherwise see our own picker).
    pub(crate) fn open_workspace_tool_picker_for_tab(
        &mut self,
        view: ViewId,
        anchor: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = self.tool_picker {
            if picker.view == view {
                self.close_tool_picker(cx);
                return;
            }
        }
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        // Anchor just below the trigger; menu_at snaps to the window edge.
        self.tool_picker = Some(crate::pane::ToolPickerState {
            view,
            anchor: gpui::point(anchor.x, anchor.y + gpui::px(26.0)),
        });
        cx.notify();
    }

    pub(crate) fn close_tool_picker(&mut self, cx: &mut Context<Self>) {
        if self.tool_picker.take().is_some() {
            cx.notify();
        }
    }

    /// A picked row: commit the row's tool as a new tab added to the picker's
    /// view (the only commit the picker owns since splits went direct).
    pub(crate) fn commit_tool_picker(
        &mut self,
        view: ViewId,
        kind: ToolKind,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace
            .add_tab_with(view, crate::pane::tool_pane_state(kind))
            .is_err()
        {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// The workspace overlays: the tool picker + the pane-header context
    /// menu (both `menu_at` popovers, mirrored from the chat context menu).
    pub(super) fn render_workspace_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut overlays = Vec::new();
        if let Some(picker) = self.tool_picker {
            overlays.push(self.render_tool_picker(picker, cx));
        }
        if let Some(menu) = self.pane_menu.get().cloned() {
            let closing = self.pane_menu.closing_since();
            overlays.push(Self::render_pane_menu(menu, closing, cx));
        }
        overlays
    }

    fn render_tool_picker(
        &mut self,
        picker: crate::pane::ToolPickerState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let mut card = popover::popover_card(&theme)
            .w(px(236.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_tool_picker(cx)))
            .flex()
            .flex_col()
            .child(popover::menu_heading(&theme, "Choose a tool"));
        for row in crate::pane::TOOL_PICKER_ROWS {
            let row_id = format!("ws-tool-{:?}", row.kind);
            let (kind, view) = (row.kind, picker.view);
            card = card.child(
                popover::menu_row(&theme, false, row_id.clone())
                    .id(SharedString::from(row_id))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tool_picker(cx);
                        this.commit_tool_picker(view, kind, cx);
                    }))
                    .child(icon(row.icon).size(px(14.0)).text_color(theme.text_muted))
                    .child(div().flex_1().child(SharedString::from(row.label)))
                    .children(row.badge.map(|badge| {
                        div()
                            .px(px(5.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(theme.hairline(0.10))
                            .text_size(crate::typography::ui_rems(9.5))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(badge))
                    })),
            );
        }
        popover::menu_at(
            "ws-tool-picker",
            picker.anchor,
            card.into_any_element(),
            None,
        )
    }

    // ------------------------------------------------------------------
    // Pane-header context menu (§6)
    // ------------------------------------------------------------------

    /// Right-click on a pane header: focus that pane (keyboard routing moves
    /// to its own composer, §6) and open the split/close menu at the pointer.
    pub(crate) fn open_workspace_pane_menu(
        &mut self,
        pane: PaneId,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.focus_workspace_pane(pane, cx);
        self.pane_menu
            .open(crate::pane::PaneMenuState { pane, position });
        cx.notify();
    }

    pub(crate) fn close_workspace_pane_menu(&mut self, cx: &mut Context<Self>) {
        if self.pane_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.pane_menu);
            cx.notify();
        }
    }

    fn render_pane_menu(
        menu: crate::pane::PaneMenuState,
        closing: Option<std::time::Instant>,
        cx: &Context<'_, Shell>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let pane = menu.pane;
        let row = |id: &'static str, label: SharedString, chord: &'static str| {
            popover::menu_row(&theme, false, id)
                .id(id)
                .child(
                    div()
                        .flex_1()
                        .text_size(crate::typography::ui_rems(11.5))
                        .child(label),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(chord)),
                )
        };
        let card = popover::popover_card(&theme)
            .w(px(232.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_workspace_pane_menu(cx)))
            .flex()
            .flex_col()
            .child(
                row(
                    "ws-menu-split-right",
                    SharedString::from("Split pane right"),
                    "⌘D",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_workspace_pane_menu(cx);
                    this.split_workspace_pane(Direction::Right, cx);
                })),
            )
            .child(
                row(
                    "ws-menu-split-down",
                    SharedString::from("Split pane down"),
                    "⇧⌘D",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_workspace_pane_menu(cx);
                    this.split_workspace_pane(Direction::Down, cx);
                })),
            )
            .child(
                row(
                    "ws-menu-view-right",
                    SharedString::from("Split view right"),
                    "⌥⌘D",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_workspace_pane_menu(cx);
                    this.split_workspace_view(Direction::Right, cx);
                })),
            )
            .child(
                row(
                    "ws-menu-view-down",
                    SharedString::from("Split view down"),
                    "⌥⌘⇧D",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_workspace_pane_menu(cx);
                    this.split_workspace_view(Direction::Down, cx);
                })),
            )
            .child(popover::menu_separator())
            .child(
                row("ws-menu-close-pane", SharedString::from("Close pane"), "")
                    .text_color(theme.danger)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_workspace_pane_menu(cx);
                        this.close_workspace_pane(pane, cx);
                    })),
            );
        popover::menu_at(
            "ws-pane-menu",
            menu.position,
            card.into_any_element(),
            closing,
        )
    }

    // ------------------------------------------------------------------
    // Actions (workspace::SplitViewRight / SplitViewDown /
    // CloseSplitView - the picker-backed pane splits live above)
    // ------------------------------------------------------------------

    /// ⌥⌘D / ⌥⌘⇧D: split the workspace at the focused view (a second
    /// top-level region with its own tab strip). The engine's `split_view`
    /// provisions the new view's single tab + pane and focuses it. Split
    /// views keep the immediate new-view behavior (Super's new view already
    /// gets a default chat - verified §4), no picker.
    pub(crate) fn split_workspace_view(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        self.solo_session = false;
        if self.workspace.split_focused_view(direction).is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    /// ⌥⌘W: close the focused view. No-op on the last view (engine guard).
    pub(crate) fn close_workspace_view(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.overlay_owns_keyboard(cx) {
            return;
        }
        if self.workspace.close_focused_view().is_err() {
            return;
        }
        self.retarget_to_focused_pane(cx);
    }

    // ------------------------------------------------------------------
    // Retarget loop
    // ------------------------------------------------------------------

    /// Sync global state to the focused pane: prune dead cache entries,
    /// select the pane's chat in AppState (the sidebar/global routing model
    /// - pane surfaces are unaffected, each keeps its own transcript and
    /// composer), and route keyboard focus to that pane's composer. `None`
    /// sessions land on the new-thread canvas, whose mint-on-send binds the
    /// pane via [`Self::sync_workspace_selection`]. Every mutation path
    /// funnels here, which is also where the WS5 save arms.
    fn retarget_to_focused_pane(&mut self, cx: &mut Context<Self>) {
        if self.workspace_mode() {
            // Adopt the original input before selecting the newly split canvas.
            self.ensure_pane_chat_surfaces(cx);
            self.sync_selection_to_focused_pane(cx);
        } else {
            // Selection first: promote re-seats the dock composer on Selected
            // and restores the survivor draft under that key.
            self.sync_selection_to_focused_pane(cx);
            self.promote_trivial_chat_surface_to_dock(cx);
        }
        // Keyboard focus follows the focused pane's own composer; a no-op
        // while an input the user chose keeps focus.
        self.focus_composer(cx);
        cx.notify();
        self.note_workspace_mutation(cx);
    }

    /// Collapse-to-single-session handoff. `chat_surfaces` keeps a survivor
    /// composer (and unsent draft) alive while a split exists; once the
    /// layout is trivial again that entity is adopted as the shared dock
    /// composer so the glass single-session route keeps the same live state.
    /// The cache is then dropped - it must never pin `workspace_mode`.
    ///
    /// Never rebuild from [`crate::composer::ComposerDraftState`]: that
    /// snapshot is intentionally lossy (no in-flight send/interrupt tasks,
    /// no queue-edit lease) and `restore_draft_state` merges maps, so a
    /// closed neighbor's attachments could leak into the dock.
    ///
    /// The pane transcript is deliberately NOT adopted: `Transcript::for_session`
    /// pins `doc_override`, so `Transcript::sync()` ignores selection changes
    /// and the glass route would stay welded to the survivor session. The
    /// primary `Transcript::new` already tracks selection and is kept.
    fn promote_trivial_chat_surface_to_dock(&mut self, cx: &mut Context<Self>) {
        if !self.workspace.is_trivial() {
            return;
        }
        self.workspace.prune_caches();
        let survivor = self
            .workspace
            .focused_pane()
            .and_then(|pane| self.workspace.chat_surfaces.remove(&pane));
        self.workspace.chat_surfaces.clear();
        let Some(surface) = survivor else {
            return;
        };
        // Preserve the survivor entity itself (in-flight sends, queue-edit
        // lease, failure banners, staged attachments all live on it).
        if surface.composer.entity_id() != self.composer.entity_id() {
            self.composer = surface.composer;
        }
        self.reseat_composer_as_dock(cx);
        self._composer_events =
            Self::dock_composer_events(&self.composer, self.transcript.clone(), cx);
    }

    /// Flip the adopted composer onto `ChatTarget::Selected` so the dock
    /// event stream and the next first-split adopt see a normal dock
    /// composer - without dropping a still-valid queue-edit lease across the
    /// Fixed→Selected projection flip.
    fn reseat_composer_as_dock(&self, cx: &mut Context<Self>) {
        self.composer.update(cx, |composer, cx| {
            let key = ChatTarget::Selected.key(composer.state.read(cx));
            if composer.current_key == key {
                composer.target = ChatTarget::Selected;
                composer.pickers().clone().update(cx, |pickers, cx| {
                    pickers.set_target(ChatTarget::Selected, cx)
                });
                cx.notify();
                return;
            }
            // Key changes: park the live queue-edit lease, run the normal
            // navigation swap, then put the lease back if hygiene dropped it.
            let editing_queued = composer.editing_queued.take();
            let queue_edit_draft = composer.queue_edit_draft.take();
            composer.set_target(ChatTarget::Selected, cx);
            if composer.editing_queued.is_none() {
                composer.editing_queued = editing_queued;
                composer.queue_edit_draft = queue_edit_draft;
            }
        });
    }

    /// Search the current workspace layout for a pane already bound to
    /// `session_id`. Returns the first match. Used by the explicit-nav
    /// restore path to focus an existing pane rather than duplicating a
    /// session binding.
    pub(crate) fn find_pane_with_session(&self, session_id: &str) -> Option<PaneId> {
        for view in self.workspace.layout.views.values() {
            for tab in view.tabs.values() {
                for (pane_id, pane_state) in &tab.panes {
                    if pane_state.session_id.as_deref() == Some(session_id) {
                        return Some(*pane_id);
                    }
                }
            }
        }
        None
    }

    /// The selection half of [`Self::retarget_to_focused_pane`], without the
    /// composer focus steal: prune dead cache entries and select the focused
    /// pane's chat in AppState. Used by the stale-session prune, where focus
    /// must not move just because a remote chat got deleted.
    fn sync_selection_to_focused_pane(&mut self, cx: &mut Context<Self>) {
        self.workspace.prune_caches();
        let session = self
            .workspace
            .focused_pane()
            .and_then(|pane| self.workspace.layout.pane(pane))
            .and_then(|state| state.session_id.clone());
        let selected = self.state.read(cx).selected_chat.clone();
        if selected != session {
            // Pane focus selects the pane's chat but does NOT follow it into
            // its space: the layout stays owned by the space it was opened
            // from, so focusing a pane bound to another space's session must
            // not trigger a layout restore that swaps the tree out.
            self.state.update(cx, |state, cx| {
                state.select_workspace_pane_chat(session, cx)
            });
        }
    }

    /// AppState selection → pane binding. Called from the shell's state
    /// observation on chat switches, so every selection path converges here:
    /// sidebar click, jump shortcut, banner, canvas mint-on-send (the
    /// composer selects the new chat id, this binds it to the focused pane).
    pub(crate) fn sync_workspace_selection(&mut self, cx: &mut Context<Self>) {
        if self.solo_session {
            return;
        }
        let selected = self.state.read(cx).selected_chat.clone();
        self.workspace.sync_focused_session(selected.as_deref());
        self.note_workspace_mutation(cx);
    }

    // ------------------------------------------------------------------
    // WS5: per-space layout persistence (workspace-layout.json)
    // ------------------------------------------------------------------

    /// A workspace mutation latched dirty (structure, ratio, focus, tab, or
    /// session binding - every [`PaneHost`] wrapper). Arm the debounced store
    /// write. Never mid-gesture: divider drags commit a ratio per pointer
    /// sample, so a live drag skips the arm and the gesture's END
    /// ([`Self::end_divider_drag`], [`Self::commit_split_drop`]) schedules
    /// the flush instead.
    fn note_workspace_mutation(&mut self, cx: &mut Context<Self>) {
        if !self.workspace.is_dirty() {
            return;
        }
        self.schedule_workspace_layout_save(cx);
    }

    fn schedule_workspace_layout_save(&mut self, cx: &mut Context<Self>) {
        if self.divider_dragging || self.split_drag.is_some() {
            return;
        }
        if !self.workspace.is_dirty() && !self.workspace_layouts.needs_save() {
            return;
        }
        // SettingsStore's debounce, owned by the Shell: a newer arm replaces
        // (and thereby cancels) the pending task, coalescing bursts.
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(
                    crate::workspace_layout_store::SAVE_DEBOUNCE_MS,
                ))
                .await;
            let _ = this.update(cx, |shell, cx| shell.flush_workspace_layout(cx));
        });
        let previous = self.workspace_save_task.replace(task);
        drop(previous);
    }

    /// Consume the dirty latch into the store, prune deleted spaces' entries,
    /// and write the store file. Runs on the debounce timer, on every space
    /// switch, and at app quit. I/O failures log only - a failed save must
    /// never break the UI (the store keeps the data pending for the next
    /// flush).
    pub(crate) fn flush_workspace_layout(&mut self, cx: &mut App) {
        self.workspace_save_task = None;
        if self.workspace.take_dirty() {
            self.workspace_layouts.set_layout(
                self.active_workspace_space.as_deref(),
                self.workspace.layout.clone(),
            );
        }
        // A deleted space (here or on another device) drops its layout entry;
        // orphan keys would be harmless but pointless to keep. Gated on the
        // synced frame - the empty pre-sync list must not wipe the saved set.
        if self.state.read(cx).spaces_synced {
            let live: std::collections::BTreeSet<String> = self
                .state
                .read(cx)
                .spaces
                .iter()
                .map(|space| space.id.clone())
                .collect();
            let removed = self.workspace_layouts.retain_spaces(|space| match space {
                None => true,
                Some(id) => live.contains(id),
            });
            if removed > 0 {
                tracing::debug!(removed, "dropped workspace layouts of deleted spaces");
            }
        }
        if !self.workspace_layouts.needs_save() {
            return;
        }
        if let Err(err) = self.workspace_layouts.flush() {
            tracing::warn!(error = %err, "failed to persist workspace layouts");
        }
    }

    /// Swap the content area to `space`'s remembered tree: boot (first synced
    /// frame) and every space switch land here. The OUTGOING tree is
    /// snapshotted into the store first, so a fast switch never loses the
    /// debounce window's changes. The incoming entry falls back to the
    /// default single-pane layout when missing or invalid. Pane surfaces
    /// rebuild lazily in the ensure pass, rehydrating the draft state parked
    /// on the way out, so unsent input survives the round trip; the FOCUSED
    /// pane's session is re-selected in AppState so sidebar/global routing
    /// follows it.
    pub(crate) fn restore_workspace_layout(
        &mut self,
        space: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Park every pane's unsent composer state before install_layout
        // drops the surfaces, keyed by the space being LEFT (captured before
        // `active_workspace_space` flips below; pane-id numerals repeat
        // across spaces' trees, so the space is part of the key).
        self.park_pane_drafts(cx);
        if self.workspace.take_dirty() {
            self.workspace_layouts.set_layout(
                self.active_workspace_space.as_deref(),
                self.workspace.layout.clone(),
            );
        }
        self.active_workspace_space = space.clone();
        let saved_layout = self
            .workspace_layouts
            .layout_for(space.as_deref())
            .filter(|layout| layout.validate().is_ok());
        // A missing layout is not a request to deselect the boot/session
        // target. Seed its default pane before retargeting; a real saved
        // layout still owns its remembered focus, including an empty canvas.
        let initial_session = if saved_layout.is_none() {
            self.state
                .read(cx)
                .selected_chat_row()
                .filter(|chat| chat.space_id.as_deref() == space.as_deref())
                .map(|chat| chat.id.clone())
        } else {
            None
        };
        let layout = saved_layout.unwrap_or_default();
        self.composer.update(cx, |composer, cx| {
            composer.set_target(ChatTarget::Selected, cx);
        });
        self.workspace.install_layout(layout);
        if let Some(session) = initial_session {
            self.workspace.sync_focused_session(Some(&session));
        }
        // Pane bindings of ANY space survive the install: a mixed-space
        // layout is legal (sidebar drops dock any session into the owning
        // space's tree), and dead sessions are still degraded by
        // `prune_dead_workspace_sessions` once chats sync.
        self.schedule_workspace_layout_save(cx);
        self.retarget_to_focused_pane(cx);
    }

    /// Once chats are synced, clear pane bindings whose session no longer
    /// exists (a chat deleted here or on another device). The pane STAYS -
    /// it degrades to the new-thread body until focused, and its session_id
    /// is simply gone (Super's stale-session handling; a terminal pane keeps
    /// its placeholder, no PTY to lose). Frequent no-op: cheap per frame.
    pub(crate) fn prune_dead_workspace_sessions(&mut self, cx: &mut Context<Self>) {
        let dead = {
            let state = self.state.read(cx);
            crate::pane::stale_session_panes(&self.workspace.layout.views, |session| {
                state.chats.iter().any(|chat| chat.id == *session)
            })
        };
        if dead.is_empty() {
            return;
        }
        for pane in dead {
            let _ = self.workspace.set_pane_session(pane, None);
        }
        // If the focused pane's session was among the dead, the selection
        // follows it off the stale chat (no composer focus steal).
        if !self.solo_session {
            self.sync_selection_to_focused_pane(cx);
        }
        cx.notify();
        self.note_workspace_mutation(cx);
    }
}

/// Fail closed on stale samples and outside drops, including an already-open
/// session whose FocusPane preview was last painted inside the outlet.
fn sidebar_commit_plan(state: &DragSplitState, session_id: &str) -> Option<DropPlan> {
    (state.source == DragSource::SidebarSession
        && state.session_id.as_deref() == Some(session_id)
        && state.resolution.plan != DropPlan::None)
        .then_some(state.resolution.plan)
}

/// Whether the stored drag state still belongs to the payload being
/// committed - the source plus the session identity the resolver stored. A
/// mismatch means the state predates this drop: clear and no-op rather than
/// committing a plan resolved against a different session.
fn split_drag_matches_payload(state: &DragSplitState, payload: &crate::pane::TabSplitDrag) -> bool {
    state.source == payload.source && state.session_id == payload.session_id
}

/// The active drag's preview rect and kind ([`hit_test::DropResolution::preview`]
/// is window-space) converted into the coordinate space of the surface that
/// paints it (the workspace outlet, or the single-pane content area), where
/// the overlay div is absolutely positioned. GPUI/Taffy measure an
/// `.absolute()` child's `.left()/.top()` insets from the containing block's
/// PADDING box - its border-box origin plus its border - and both surfaces
/// are borderless, so subtracting `root_bounds.origin` (the surface's
/// paint-time hitbox origin, `DragMoveEvent::bounds`) is exact; the
/// container's padding is deliberately NOT subtracted (it does not shift
/// absolute children). `None` when the resolution has no preview (invalid
/// drops and guarded self-hits).
fn preview_bounds(
    state: &DragSplitState,
) -> Option<(gpui::Bounds<gpui::Pixels>, hit_test::PreviewKind)> {
    let preview = state.resolution.preview?;
    let rect = preview.rect;
    let origin = state.root_bounds.origin;
    Some((
        gpui::Bounds {
            origin: gpui::point(gpui::px(rect.x) - origin.x, gpui::px(rect.y) - origin.y),
            size: gpui::size(gpui::px(rect.w), gpui::px(rect.h)),
        },
        preview.kind,
    ))
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn sidebar_release_rejects_outside_and_stale_drag_even_for_existing_session() {
        let mut state = DragSplitState {
            source: DragSource::SidebarSession,
            session_id: Some("chat-a".into()),
            root_bounds: root_bounds(),
            resolution: hit_test::DropResolution {
                plan: DropPlan::FocusPane { pane: PaneId(3) },
                preview: None,
                anchor: Some(PaneId(3)),
            },
        };
        assert_eq!(
            sidebar_commit_plan(&state, "chat-a"),
            Some(DropPlan::FocusPane { pane: PaneId(3) })
        );
        assert_eq!(sidebar_commit_plan(&state, "chat-b"), None);
        state.resolution = hit_test::DropResolution::none();
        assert_eq!(sidebar_commit_plan(&state, "chat-a"), None);
        state.source = DragSource::PaneHeader(PaneId(3));
        assert_eq!(sidebar_commit_plan(&state, "chat-a"), None);
    }

    /// The outlet hitbox (sidebar + titlebar in front of it) the previews in
    /// these tests convert from.
    fn root_bounds() -> gpui::Bounds<gpui::Pixels> {
        gpui::Bounds {
            origin: gpui::point(gpui::px(320.0), gpui::px(70.0)),
            size: gpui::size(gpui::px(1000.0), gpui::px(800.0)),
        }
    }

    #[test]
    fn preview_bounds_converts_window_space_to_the_overlay_surface() {
        // A SplitPane resolution's window-space half-pane converts by
        // origin subtraction only - the overlay's `.absolute()` insets are
        // measured from the surface's border-box origin (its padding does
        // not shift absolute children), so subtracting padding here would
        // double-count it.
        let state = DragSplitState {
            source: DragSource::SidebarSession,
            session_id: Some("chat-a".into()),
            root_bounds: root_bounds(),
            resolution: hit_test::DropResolution {
                plan: DropPlan::SplitPane {
                    pane: PaneId(3),
                    direction: Direction::Right,
                },
                preview: Some(hit_test::DropPreview {
                    rect: hit_test::Rect::new(500.0, 200.0, 250.0, 192.5),
                    kind: hit_test::PreviewKind::PaneHalf,
                }),
                anchor: Some(PaneId(3)),
            },
        };
        let (bounds, kind) = preview_bounds(&state).unwrap();
        assert_eq!(bounds.origin, gpui::point(gpui::px(180.0), gpui::px(130.0)));
        assert_eq!(bounds.size, gpui::size(gpui::px(250.0), gpui::px(192.5)));
        assert_eq!(kind, hit_test::PreviewKind::PaneHalf);
    }

    #[test]
    fn preview_bounds_is_none_without_a_preview() {
        // Invalid drops and self-hits carry no preview - nothing to convert.
        let state = DragSplitState {
            source: DragSource::PaneHeader(PaneId(1)),
            session_id: None,
            root_bounds: root_bounds(),
            resolution: hit_test::DropResolution {
                plan: DropPlan::MoveIntoPane {
                    view: ViewId(1),
                    tab_before: None,
                },
                preview: None,
                anchor: Some(PaneId(1)),
            },
        };
        assert_eq!(preview_bounds(&state), None);
    }
}

#[cfg(test)]
mod pane_meta_tests {
    use super::*;
    use zeron_proto::{Chat, Session, SessionStatus};

    const PROJECT: &str = "project";

    fn chat(id: &str, device: &str, branch: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": device,
            "spaceId": PROJECT,
            "title": "Build the Fieldnotes workspace",
            "archived": false,
            "createdAt": "2026-09-08T00:00:00Z",
            // Unseen (no lastSeenAt): Errored sessions must surface instead
            // of decaying to Completed/Idle.
            "lastMessageAt": "2026-09-08T00:00:00Z",
            "sourceContext": branch.map(|branch| serde_json::json!({
                "checkoutId": "checkout",
                "repoRoot": "/tmp",
                "cwd": "/tmp",
                "branch": branch,
                "observedAt": "2026-09-08T00:00:00Z",
            })),
        }))
        .unwrap()
    }

    fn seeded_state() -> AppState {
        let mut state = AppState::new();
        state.spaces = vec![
            serde_json::from_value(serde_json::json!({
                "id": PROJECT,
                "deviceId": "local",
                "path": "/tmp/project",
                "gitDetected": true,
                "createdAt": "2026-09-08T00:00:00Z",
            }))
            .unwrap(),
        ];
        state.local_device_id = Some("local".into());
        state
    }

    #[test]
    fn unbound_panes_carry_no_header_metadata() {
        let meta = pane_meta(None, &seeded_state());
        assert_eq!(meta, chrome::PaneMeta::empty());
        // A session id with no matching chat is the same empty state (a
        // just-minted canvas send).
        let meta = pane_meta(Some("missing-chat"), &seeded_state());
        assert_eq!(meta, chrome::PaneMeta::empty());
    }

    #[test]
    fn context_is_project_colon_branch_plus_a_remote_device_only() {
        let mut state = seeded_state();
        state.chats = vec![
            chat("local-chat", "local", Some("feat/auth")),
            chat("remote-chat", "studio", Some("main")),
            chat("branchless", "local", None),
        ];
        state.devices = vec![
            serde_json::from_value(serde_json::json!({
                "id": "studio",
                "name": "Mac Studio",
                "platform": "macos",
                "createdAt": "2026-09-08T00:00:00Z",
            }))
            .unwrap(),
        ];

        // Local session: project:branch, no device fragment.
        let local = pane_meta(Some("local-chat"), &state);
        assert_eq!(local.context.as_deref(), Some("project:feat/auth"));
        // Remote session: the device is appended after a middot.
        let remote = pane_meta(Some("remote-chat"), &state);
        assert_eq!(remote.context.as_deref(), Some("project:main · Mac Studio"));
        // No branch stamped: project alone.
        let branchless = pane_meta(Some("branchless"), &state);
        assert_eq!(branchless.context.as_deref(), Some("project"));
    }

    #[test]
    fn state_resolves_from_the_session_and_send_truth() {
        let mut state = seeded_state();
        state.chats = vec![
            chat("working", "local", None),
            chat("awaiting", "local", None),
            chat("errored", "local", None),
        ];
        state.set_sessions(vec![
            Session {
                last_completed_turn: None,
                chat_id: "working".into(),
                device_id: "local".into(),
                status: SessionStatus::Working,
                started_at: None,
                updated_at: Utc::now(),
            },
            Session {
                last_completed_turn: None,
                chat_id: "awaiting".into(),
                device_id: "local".into(),
                status: SessionStatus::AwaitingInput,
                started_at: None,
                updated_at: Utc::now(),
            },
            Session {
                last_completed_turn: None,
                chat_id: "errored".into(),
                device_id: "local".into(),
                status: SessionStatus::Errored,
                started_at: None,
                updated_at: Utc::now(),
            },
        ]);

        // The engine's indicator reaches the header verbatim; working is a
        // live (chromatic) state, idle stays quiet.
        assert_eq!(
            pane_meta(Some("working"), &state).state,
            SessionState::Working
        );
        assert_eq!(
            pane_meta(Some("awaiting"), &state).state,
            SessionState::AwaitingInput
        );
        assert_eq!(
            pane_meta(Some("errored"), &state).state,
            SessionState::Failed
        );
        // No session row: the unseen chat settles to Completed, which still
        // carries a label (idle is the only label-less state).
        state.set_sessions(Vec::new());
        let settled = pane_meta(Some("working"), &state);
        assert_eq!(settled.state, SessionState::Completed);
        assert_eq!(settled.state.label(), Some("Completed"));
        // A send stuck past the delivery grace overrides the display status
        // and reads as failed (danger), same as the sidebar slot.
        state.begin_pending_send(
            "working",
            "msg-1",
            Utc::now() - chrono::Duration::seconds(300),
        );
        let undelivered = pane_meta(Some("working"), &state);
        assert_eq!(undelivered.state, SessionState::Failed);
        assert_eq!(undelivered.context.as_deref(), Some("project"));
    }

    #[test]
    fn header_mark_prefers_the_bound_harness_brand() {
        let mut state = seeded_state();
        let mut claude = chat("claude-chat", "local", None);
        claude.config = Some(zeron_proto::ChatConfig {
            harness: zeron_proto::HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        });
        state.chats = vec![claude];

        // A bound chat's known harness wins over the provider/mode mark.
        let mark = header_mark(state.chats.first(), PaneMode::Chat, None, None);
        assert_eq!(mark.icon, Some(icons::CLAUDE_MARK));
        assert_eq!(mark.tint, Some(icons::claude_brand()));
        // An unbound pane adopts the composer's harness pick (the model
        // chip's identity); with no pick it keeps the provider/mode mark.
        let composer_mark = header_mark(
            None,
            PaneMode::Chat,
            None,
            Some(zeron_proto::HarnessId::Pi),
        );
        assert_eq!(composer_mark.icon, Some(icons::PI_MARK));
        assert_eq!(
            header_mark(None, PaneMode::Chat, Some("opencode"), None),
            tab_mark(PaneMode::Chat, Some("opencode"))
        );
        assert_eq!(
            header_mark(None, PaneMode::Terminal, None, None),
            tab_mark(PaneMode::Terminal, None)
        );
    }
}

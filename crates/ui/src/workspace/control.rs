//! UI mutations run on the UI thread. Read waits never block other control requests.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use gpui::{AsyncApp, Context, WeakEntity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_doc::{SessionCommandPayload, TranscriptFrame, TranscriptUpdate, apply_transcript_frame};
use zeron_local_api::{ControlPlane, Request, Subscription};
use zeron_proto::{ChatConfig, RunRequest, SandboxLevel, Session, SessionStatus};
use zeron_rpc::{RpcSubscription, methods};
use zeron_workspace::{
    Direction, PaneId, PaneMode, PaneState, SplitNode, TabId, ViewId, WorkspaceLayout,
};

use super::Workspace;
use crate::state::EngineHandle;
use crate::terminal::panel::SessionViewStatus;

/// Grants are minted only by human UI actions and expire with this window.
#[derive(Default)]
pub(super) struct Consent {
    allow: BTreeSet<String>,
    orchestrate: BTreeSet<String>,
    sessions: BTreeMap<String, String>,
    reports: BTreeMap<String, (String, String, String)>,
    app_sessions: BTreeMap<String, (String, String)>,
    human_input: BTreeSet<String>,
    pending: Option<Deletion>,
    team_operations: std::sync::Arc<tokio::sync::Mutex<()>>,
}

struct Deletion {
    workspace: String,
    description: String,
    rpc: &'static str,
    payload: Value,
}

impl Consent {
    fn revoke(&mut self, workspace: &str) {
        self.allow.remove(workspace);
        self.orchestrate.remove(workspace);
        self.sessions.retain(|_, scope| scope != workspace);
        self.human_input
            .retain(|session| self.sessions.contains_key(session));
        self.app_sessions.retain(|_, (_, scope)| scope != workspace);
        self.reports
            .retain(|_, (_, _, session)| self.sessions.contains_key(session));
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.workspace == workspace)
        {
            self.pending = None;
        }
    }

    fn verify_app_session(&self, workspace: &str, session: &str, capability: &str) -> Result<()> {
        ensure!(
            self.app_sessions
                .get(capability)
                .is_some_and(|(id, scope)| id == session && scope == workspace),
            "denied: worktree creation requires a verified app-launched session with human UI consent"
        );
        ensure!(
            self.human_input.contains(session),
            "denied: a human must submit input in this session before worktree creation"
        );
        Ok(())
    }

    fn verify_report(
        &self,
        workspace: &str,
        team: &str,
        role: &str,
        capability: &str,
    ) -> Result<String> {
        let binding = self
            .reports
            .get(capability)
            .context("unverified app session reporting capability")?;
        ensure!(
            binding.0 == team && binding.1 == role,
            "report identity mismatch"
        );
        ensure!(
            self.sessions
                .get(&binding.2)
                .is_some_and(|scope| scope == workspace),
            "app session grant revoked"
        );
        Ok(binding.2.clone())
    }

    fn check(&self, workspace: &str, selected: Option<&str>, orchestration: bool) -> Result<()> {
        ensure!(
            selected == Some(workspace) && self.allow.contains(workspace),
            "denied: selected workspace requires a human Allow grant"
        );
        ensure!(
            !orchestration || self.orchestrate.contains(workspace),
            "denied: a human must explicitly Allow API orchestration in this workspace"
        );
        Ok(())
    }
}

const MAX_BATCH: usize = 64;
const MAX_RECIPE_BYTES: u64 = 8 * 1024 * 1024;

impl Workspace {
    /// Called only by genuine human composer/terminal submission handlers.
    /// API-injected prompts never invoke this method or unlock worktree creation.
    pub(super) fn control_mark_human_input(
        &mut self,
        chat: &str,
        _proof: crate::input_origin::HumanInput,
        cx: &mut Context<Self>,
    ) {
        if self.control_consent.sessions.contains_key(chat) {
            self.control_consent.human_input.insert(chat.to_owned());
            cx.notify();
        }
    }

    pub(super) fn start_control(&mut self, cx: &mut Context<Self>) {
        if self.control_task.is_some() {
            return;
        }
        let Some(data_dir) = self.source.read(cx).data_dir.clone() else {
            self.error = Some("Control API requires the instance data directory".into());
            return;
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let start = gpui_tokio::Tokio::spawn(cx, async move {
            // Acquire the instance lock before recovering interrupted runs.
            let server =
                ControlPlane::start(data_dir.clone(), crate::APP_ID.into(), sender).await?;
            let db = data_dir.join("orchestration.db");
            let store = tokio::task::spawn_blocking(move || -> Result<_> {
                no_symlinks(&db)?;
                let store = zeron_orchestration::Store::open(db)?;
                store.recover_interrupted()?;
                Ok(store)
            })
            .await??;
            Ok::<_, anyhow::Error>((server, store))
        });
        self.control_task = Some(cx.spawn(async move |this, cx| {
            let (server, store) = match start.await {
                Ok(Ok(server)) => server,
                result => {
                    let error = match result {
                        Ok(Err(error)) => error.to_string(),
                        Err(error) => error.to_string(),
                        _ => unreachable!(),
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(format!("Control API: {error}"));
                        cx.notify();
                    });
                    return;
                }
            };
            if this
                .update(cx, |this, _| {
                    this.orchestration = Some(store);
                    this.events = Some(server.events());
                    this.control = Some(server);
                })
                .is_err()
            {
                return;
            }
            let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_BATCH));
            while let Some(Request {
                method,
                params,
                reply,
            }) = receiver.recv().await
            {
                if reply.is_closed() {
                    continue;
                }
                let Ok(permit) = capacity.clone().try_acquire_owned() else {
                    let _ = reply.send(Err(
                        "Control API is busy; too many concurrent requests".into()
                    ));
                    continue;
                };
                let this = this.clone();
                cx.spawn(async move |cx| {
                    let _permit = permit;
                    if reply.is_closed() {
                        return;
                    }
                    let result = dispatch(&this, cx, &method, params)
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let _ = reply.send(result);
                })
                .detach();
            }
        }));
    }

    fn control_state(&self) -> Value {
        let panes: Vec<_> = pane_order(&self.layout)
            .into_iter()
            .map(|id| {
                let state = self.layout.pane(id).expect("validated layout pane");
                let (view, tab) = self
                    .layout
                    .pane_location(id)
                    .expect("validated layout location");
                json!({"id": id, "target": format!("id:{}", id.0), "viewId": view,
                "tabId": tab, "sessionId": state.session_id, "mode": state.mode,
                "label": state.label, "group": state.group, "parked": !self.panes.contains_key(&id),
                "active": self.layout.active_pane_id() == Some(id)})
            })
            .collect();
        json!({"revision": self.layout.revision, "layout": self.layout, "panes": panes,
            "apiAllowWorkspaces": self.control_consent.allow, "orchestrationWorkspaces": self.control_consent.orchestrate})
    }

    pub(super) fn control_has_allow(&self, cx: &Context<Self>) -> bool {
        self.source
            .read(cx)
            .selected_space
            .as_ref()
            .is_some_and(|id| self.control_consent.allow.contains(id))
    }

    pub(super) fn control_has_orchestration(&self, cx: &Context<Self>) -> bool {
        self.source
            .read(cx)
            .selected_space
            .as_ref()
            .is_some_and(|id| self.control_consent.orchestrate.contains(id))
    }

    pub(super) fn control_toggle_allow(&mut self, cx: &mut Context<Self>) {
        if !crate::input_origin::is_human_input() {
            return;
        }
        if let Some(id) = self.source.read(cx).selected_space.clone() {
            if !self.control_consent.allow.remove(&id) {
                self.control_consent.allow.insert(id);
            } else {
                self.control_consent.revoke(&id);
            }
        }
        cx.notify();
    }

    pub(super) fn control_toggle_orchestration(&mut self, cx: &mut Context<Self>) {
        if !crate::input_origin::is_human_input() {
            return;
        }
        if let Some(id) = self.source.read(cx).selected_space.clone()
            && !self.control_consent.orchestrate.remove(&id)
        {
            self.control_consent.allow.insert(id.clone());
            self.control_consent.orchestrate.insert(id);
        }
        cx.notify();
    }

    fn control_allow(&self, cx: &Context<Self>) -> Result<()> {
        ensure!(
            self.control_has_allow(cx),
            "denied: a human must Allow API access in the selected workspace"
        );
        Ok(())
    }

    fn control_scope(
        &self,
        workspace: &str,
        cx: &Context<Self>,
        orchestration: bool,
    ) -> Result<()> {
        self.control_consent.check(
            workspace,
            self.source.read(cx).selected_space.as_deref(),
            orchestration,
        )
    }

    pub(super) fn control_pending_deletion(&self) -> Option<String> {
        self.control_consent
            .pending
            .as_ref()
            .map(|pending| pending.description.clone())
    }

    pub(super) fn control_cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.control_consent.pending = None;
        cx.notify();
    }

    pub(super) fn control_confirm_delete(&mut self, cx: &mut Context<Self>) {
        if !crate::input_origin::is_human_input() {
            return;
        }
        let Some(pending) = self.control_consent.pending.take() else {
            return;
        };
        let prepared = (|| -> Result<_> {
            self.control_scope(&pending.workspace, cx, false)?;
            let source = self.source.read(cx);
            ensure!(
                source.spaces.iter().any(|s| s.id == pending.workspace),
                "workspace no longer exists"
            );
            if pending.rpc == methods::MUTATE {
                ensure!(source.spaces.len() > 1, "cannot delete the last workspace");
            } else {
                let path = pending.payload["worktreePath"]
                    .as_str()
                    .context("missing worktree path")?;
                for chat in source.chats.iter().filter(|chat| {
                    chat.space_id.as_deref() == Some(&pending.workspace)
                        && chat.cwd.as_deref() == Some(path)
                }) {
                    ensure!(
                        !source
                            .sessions
                            .iter()
                            .any(|session| session.chat_id == chat.id
                                && matches!(
                                    session.status,
                                    SessionStatus::Working | SessionStatus::AwaitingInput
                                )),
                        "stop worktree sessions before deletion"
                    );
                    ensure!(
                        !pane_order(&self.layout).iter().any(|id| self
                            .layout
                            .pane(*id)
                            .is_some_and(|pane| pane.session_id.as_deref() == Some(&chat.id)
                                && pane.mode == PaneMode::Terminal)),
                        "close worktree CLI panes before deletion"
                    );
                }
            }
            let engine = source.engine().cloned().context("engine disconnected")?;
            Ok(engine)
        })();
        match prepared {
            Err(error) => self.error = Some(error.to_string()),
            Ok(engine) => {
                cx.spawn(async move |this, cx| {
                    let result = engine.client().call(pending.rpc, pending.payload).await;
                    let _ = this.update(cx, |this, cx| {
                        if let Err(error) = result {
                            this.error = Some(error.to_string());
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn control_commit(&mut self, draft: WorkspaceLayout, cx: &mut Context<Self>) -> Result<Value> {
        // A detached terminal still owns its session. Never discard or rebind its runtime.
        for id in pane_order(&self.layout) {
            let old = self.layout.pane(id).unwrap();
            if old.mode == PaneMode::Terminal
                || self
                    .panes
                    .get(&id)
                    .and_then(|p| p.terminal.as_ref())
                    .is_some_and(|terminal| {
                        !matches!(
                            terminal.read(cx).session_view_status(),
                            SessionViewStatus::Idle
                        )
                    })
            {
                ensure!(draft.pane(id).is_some_and(|new| new.session_id == old.session_id && new.mode == old.mode),
                    "denied: close the CLI session in the UI before removing or rebinding pane id:{}", id.0);
            }
        }
        let mut changed_sessions = Vec::new();
        for id in pane_order(&draft) {
            let new = draft.pane(id).unwrap();
            let old = self.layout.pane(id);
            ensure!(
                old.is_none_or(|old| old.mode == new.mode),
                "denied: change existing pane renderers through the human UI, not compose"
            );
            if new.mode == PaneMode::Terminal && old != Some(new) {
                let session = new
                    .session_id
                    .as_deref()
                    .context("terminal cells require an explicit sessionId")?;
                let source = self.source.read(cx);
                let workspace = source
                    .chats
                    .iter()
                    .find(|chat| chat.id == session)
                    .and_then(|chat| chat.space_id.as_deref())
                    .or_else(|| {
                        self.control_consent
                            .sessions
                            .get(session)
                            .map(String::as_str)
                    })
                    .context("terminal cell session must belong to a known workspace")?;
                self.control_scope(workspace, cx, true)?;
            }
            if old.is_some_and(|old| old.session_id != new.session_id) {
                changed_sessions.push((id, new.session_id.clone()));
            }
        }
        let revision = self.layout.revision;
        self.layout.compose(revision, |layout| {
            *layout = draft;
            Ok(())
        })?;
        self.panes.retain(|id, _| self.layout.pane(*id).is_some());
        let topics: BTreeSet<_> = pane_order(&self.layout)
            .into_iter()
            .filter_map(|id| {
                self.layout
                    .pane(id)
                    .unwrap()
                    .session_id
                    .as_ref()
                    .map(|id| format!("agent:{id}"))
            })
            .collect();
        self.control_watches
            .retain(|topic, _| !topic.starts_with("agent:") || topics.contains(topic));
        for (id, chat) in changed_sessions {
            if let Some(runtime) = self.panes.get(&id) {
                runtime.chat.update(cx, |pane, cx| pane.select(chat, cx));
            }
        }
        self.focus_pending = true;
        self.changed(cx);
        let state = self.control_state();
        if let Some(events) = &self.events {
            events.publish("layout", state.clone());
        }
        Ok(state)
    }

    fn control_visual(
        &mut self,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) -> Result<Value> {
        match method {
            "layout.views" | "layout.state" | "layout.get" | "agents.list" => {
                Ok(self.control_state())
            }
            "layout.subscribe" | "layout.watch" => Ok(serde_json::to_value(Subscription {
                topics: vec!["layout".into()],
                snapshot: self.control_state(),
            })?),
            "chat.list" => Ok(json!({"chats": self.source.read(cx).chats})),
            "tab.split" | "tab.split-view" => {
                let target = one(&self.layout, target(params)?)?;
                let mode = explicit_ui(params)?;
                let pane = new_pane(params, mode)?;
                let direction = direction(params)?;
                let mut draft = self.layout.clone();
                check_guard(&draft, params, false)?;
                if method == "tab.split-view" {
                    let view = draft.pane_location(target).context("target disappeared")?.0;
                    draft.split_view(view, direction, pane)?;
                } else {
                    draft.split_pane(target, direction, pane)?;
                }
                self.control_commit(draft, cx)
            }
            "layout.compose" => {
                let plan = params.get("plan").or_else(|| params.get("composition"));
                if plan.is_none() && flag(params, "refreshGuard", "refresh_guard")? {
                    return Ok(json!({"revision": self.layout.revision}));
                }
                let dry_run = flag(params, "dryRun", "dry_run")?;
                let draft = compose_plan(&self.layout, plan.context("plan is required")?, params)?;
                if dry_run {
                    return Ok(
                        json!({"dryRun": true, "revision": self.layout.revision, "layout": draft}),
                    );
                }
                self.control_commit(draft, cx)
            }
            "layout.move" => {
                let from = one(&self.layout, required_str(params, "from")?)?;
                let to = one(&self.layout, required_str(params, "to")?)?;
                let mut draft = self.layout.clone();
                check_guard(&draft, params, false)?;
                draft.move_pane(from, to, direction(params)?)?;
                self.control_commit(draft, cx)
            }
            "agents.label" | "agents.group" => {
                let ids = resolve(
                    &self.layout,
                    target(params)?,
                    method == "agents.group" && bool_param(params, "multi")?,
                )?;
                let field = if method == "agents.label" {
                    "label"
                } else {
                    "group"
                };
                let value = params.get(field).context("label/group is required")?;
                let value = if value.is_null() {
                    None
                } else {
                    let s = value
                        .as_str()
                        .context("label/group must be a string or null")?;
                    ensure!(
                        !s.trim().is_empty() && s.len() <= 128,
                        "label/group must contain 1..128 bytes"
                    );
                    Some(s.to_owned())
                };
                if field == "label"
                    && let Some(label) = &value
                {
                    ensure!(
                        !pane_order(&self.layout).iter().any(|id| !ids.contains(id)
                            && self.layout.pane(*id).unwrap().label.as_ref() == Some(label)),
                        "label already exists"
                    );
                }
                let mut draft = self.layout.clone();
                draft.compose(draft.revision, |draft| {
                    for id in &ids {
                        let pane = draft.pane_mut(*id).unwrap();
                        if field == "label" {
                            pane.label = value.clone();
                        } else {
                            pane.group = value.clone();
                        }
                    }
                    Ok(())
                })?;
                self.control_commit(draft, cx)
            }
            "chat.select" => {
                let chat = required_str(params, "sessionId")
                    .or_else(|_| required_str(params, "chatId"))?;
                ensure!(
                    self.source.read(cx).chats.iter().any(|c| c.id == chat),
                    "unknown session {chat}"
                );
                let id = one(&self.layout, target(params)?)?;
                let mut draft = self.layout.clone();
                draft.compose(draft.revision, |draft| {
                    draft.pane_mut(id).unwrap().session_id = Some(chat.into());
                    draft.focus_pane(id)
                })?;
                self.control_commit(draft, cx)
            }
            "chat.close" => {
                let id = one(&self.layout, target(params)?)?;
                let mut draft = self.layout.clone();
                draft.close_pane(id)?;
                self.control_commit(draft, cx)
            }
            _ => bail!("unsupported visual operation {method}"),
        }
    }
}

async fn dispatch(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    ensure!(params.is_object(), "params must be an object");
    match method {
        method if method.starts_with("team.") || method.starts_with("coordination-state.") => {
            orchestration(this, cx, method, params).await
        }
        method if method.starts_with("section.") => section_control(this, cx, method, params).await,
        method if method.starts_with("workspace.") || method.starts_with("worktree.") => {
            workspace_control(this, cx, method, params).await
        }
        "layout.save" | "layout.apply" | "layout.list" | "layout.delete" => {
            recipe(this, cx, method, params).await
        }
        "chat.new" | "layout.run" => launch(this, cx, method, params).await,
        "chat.providers" => {
            let engine = engine(this, cx)?;
            Ok(engine
                .client()
                .call(methods::LIST_HARNESSES, json!({}))
                .await?)
        }
        "agent.read" | "agent.subscribe" | "agent.wait" | "agent.should-stop" | "agent.send"
        | "agent.stop" | "agent.interrupt" | "layout.stop" => agent(this, cx, method, params).await,
        _ => this.update(cx, |this, cx| this.control_visual(method, &params, cx))?,
    }
}

fn engine(this: &WeakEntity<Workspace>, cx: &mut AsyncApp) -> Result<EngineHandle> {
    this.update(cx, |this, cx| {
        this.source
            .read(cx)
            .engine()
            .cloned()
            .context("engine is not connected")
    })?
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("{key} must be a nonempty string"))
}

fn target(params: &Value) -> Result<&str> {
    if params.get("to").is_none() {
        Ok("active-pane")
    } else {
        required_str(params, "to")
    }
}

fn bool_param(params: &Value, key: &str) -> Result<bool> {
    match params.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        _ => bail!("{key} must be a boolean"),
    }
}

fn flag(params: &Value, camel: &str, snake: &str) -> Result<bool> {
    Ok(bool_param(params, camel)? || bool_param(params, snake)?)
}

fn explicit_ui(params: &Value) -> Result<PaneMode> {
    match required_str(params, "ui")? {
        "chat" => Ok(PaneMode::Chat),
        "terminal" | "cli" => Ok(PaneMode::Terminal),
        _ => bail!("ui must explicitly be chat or terminal; auto is not supported"),
    }
}

fn direction(params: &Value) -> Result<Direction> {
    match required_str(params, "direction")? {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => bail!("direction must be left, right, up or down"),
    }
}

fn new_pane(params: &Value, mode: PaneMode) -> Result<PaneState> {
    let optional = |key| -> Result<Option<String>> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
            _ => bail!("{key} must be a nonempty string or null"),
        }
    };
    Ok(PaneState {
        mode,
        session_id: optional("sessionId")?,
        label: optional("label")?,
        group: optional("group")?,
    })
}

fn leaves<T: Copy>(node: &SplitNode<T>, result: &mut Vec<T>) {
    match node {
        SplitNode::Leaf { content } => result.push(*content),
        SplitNode::Split { first, second, .. } => {
            leaves(first, result);
            leaves(second, result);
        }
    }
}

fn view_order(layout: &WorkspaceLayout) -> Vec<ViewId> {
    let mut ids = Vec::new();
    leaves(&layout.root, &mut ids);
    ids
}

fn pane_order(layout: &WorkspaceLayout) -> Vec<PaneId> {
    let mut result = Vec::new();
    for view in view_order(layout) {
        for tab in layout.views[&view].ordered_tabs() {
            leaves(&layout.views[&view].tabs[&tab].root, &mut result);
        }
    }
    result
}

fn index(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .context("positions are 1-based positive integers")
}

/// Positional tab/pane addresses are local to the active view/tab. IDs never are.
fn resolve(layout: &WorkspaceLayout, address: &str, multi: bool) -> Result<Vec<PaneId>> {
    let active_view = &layout.views[&layout.active_view_id];
    let active_tab = &active_view.tabs[&active_view.active_tab_id];
    let active_of_view = |view: ViewId| {
        let view = &layout.views[&view];
        view.tabs[&view.active_tab_id].active_pane_id
    };
    let ids = match address {
        "active-pane" | "active-tab" => vec![active_tab.active_pane_id],
        "active-view" => vec![active_of_view(layout.active_view_id)],
        _ => {
            let (kind, value) = address.split_once(':').context(
                "use label:, id:, group:, view:N, tab:N, pane:N or active-pane/tab/view",
            )?;
            ensure!(!value.is_empty(), "empty target");
            match kind {
                "id" => {
                    let id = PaneId(
                        value
                            .parse()
                            .context("id must be a stable numeric pane ID")?,
                    );
                    ensure!(layout.pane(id).is_some(), "unknown pane id:{value}");
                    vec![id]
                }
                "label" | "group" => {
                    if kind == "group" {
                        ensure!(
                            multi,
                            "group targets require an explicitly multi-target operation with multi:true"
                        );
                    }
                    pane_order(layout)
                        .into_iter()
                        .filter(|id| {
                            let p = layout.pane(*id).unwrap();
                            (if kind == "label" { &p.label } else { &p.group }).as_deref()
                                == Some(value)
                        })
                        .collect()
                }
                "view" => vec![active_of_view(
                    *view_order(layout)
                        .get(index(value)?)
                        .context("view position out of range")?,
                )],
                "tab" => vec![
                    active_view.tabs[active_view
                        .ordered_tabs()
                        .get(index(value)?)
                        .context("tab position out of range")?]
                    .active_pane_id,
                ],
                "pane" => {
                    let mut ids = Vec::new();
                    leaves(&active_tab.root, &mut ids);
                    vec![
                        *ids.get(index(value)?)
                            .context("pane position out of range")?,
                    ]
                }
                _ => bail!("unknown target kind {kind}"),
            }
        }
    };
    ensure!(!ids.is_empty(), "target {address} matched no panes");
    ensure!(multi || ids.len() == 1, "target {address} is ambiguous");
    ensure!(ids.len() <= MAX_BATCH, "target exceeds {MAX_BATCH} panes");
    Ok(ids)
}

fn one(layout: &WorkspaceLayout, address: &str) -> Result<PaneId> {
    Ok(resolve(layout, address, false)?[0])
}

fn check_guard(layout: &WorkspaceLayout, params: &Value, required: bool) -> Result<()> {
    let guard = params
        .get("revision")
        .or_else(|| params.get("expectedRevision"));
    if let Some(guard) = guard {
        let revision = guard
            .as_u64()
            .context("revision must be an unsigned integer")?;
        ensure!(
            revision == layout.revision,
            "revision conflict: expected {revision}, actual {}",
            layout.revision
        );
    } else {
        ensure!(
            !required,
            "revision is required; read layout.state or request refreshGuard first"
        );
    }
    Ok(())
}

/// Plans are operation arrays, {operations:[...]}, or a complete serialized layout.
/// Each new cell in an operation plan must carry ui, never a provider-derived default.
fn compose_plan(
    current: &WorkspaceLayout,
    plan: &Value,
    params: &Value,
) -> Result<WorkspaceLayout> {
    let refresh = flag(params, "refreshGuard", "refresh_guard")?;
    check_guard(current, params, !refresh && plan.get("revision").is_none())?;
    if let Some(revision) = plan.get("revision") {
        check_guard(current, &json!({"revision": revision}), true)?;
    }
    let operations = plan
        .as_array()
        .or_else(|| plan.get("operations").and_then(Value::as_array));
    let mut draft = current.clone();
    if let Some(operations) = operations {
        ensure!(
            !operations.is_empty() && operations.len() <= MAX_BATCH,
            "plan needs 1..{MAX_BATCH} operations"
        );
        // Stage every operation, then commit once against the original IDs and revision.
        for op in operations {
            match required_str(op, "op")? {
                "split" | "split-view" | "tab" => {
                    let target = one(&draft, target(op)?)?;
                    let pane = new_pane(op, explicit_ui(op)?)?;
                    let view = draft.pane_location(target).unwrap().0;
                    match required_str(op, "op")? {
                        "split" => {
                            draft.split_pane(target, direction(op)?, pane)?;
                        }
                        "split-view" => {
                            draft.split_view(view, direction(op)?, pane)?;
                        }
                        _ => {
                            draft.add_tab(view, pane)?;
                        }
                    }
                }
                "move" => {
                    let from = one(&draft, required_str(op, "from")?)?;
                    let to = one(&draft, required_str(op, "to")?)?;
                    draft.move_pane(from, to, direction(op)?)?;
                }
                "focus" => {
                    let id = one(&draft, target(op)?)?;
                    draft.focus_pane(id)?;
                }
                "close" => {
                    let id = one(&draft, target(op)?)?;
                    draft.close_pane(id)?;
                }
                op => bail!("unsupported compose operation {op}"),
            }
        }
    } else {
        draft = serde_json::from_value(plan.clone()).context("invalid layout plan")?;
        let new_ids: Vec<_> = pane_order(&draft)
            .into_iter()
            .filter(|id| current.pane(*id).is_none())
            .collect();
        if !new_ids.is_empty() {
            let ui = explicit_ui(params)?;
            ensure!(
                new_ids.iter().all(|id| draft.pane(*id).unwrap().mode == ui),
                "new cells must match the explicitly requested ui; use operations for mixed renderers"
            );
        }
    }
    let mut validated = current.clone();
    validated.compose(current.revision, |layout| {
        *layout = draft;
        Ok(())
    })?;
    Ok(validated)
}

async fn within<T>(
    cx: &AsyncApp,
    seconds: u64,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let timeout = cx.background_executor().timer(Duration::from_secs(seconds));
    match futures::future::select(Box::pin(future), Box::pin(timeout)).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => bail!("timed out after {seconds}s"),
    }
}

async fn transcript(engine: &EngineHandle, chat: &str) -> Result<(Value, RpcSubscription)> {
    let mut watch = engine
        .client()
        .subscribe_checked(methods::WATCH_DOC_MESSAGES, json!({"chatId": chat}))
        .await?;
    let value = watch
        .recv()
        .await
        .context("transcript stream closed before its reset")?;
    let update: TranscriptUpdate =
        serde_json::from_value(value).context("invalid transcript reset")?;
    ensure!(
        matches!(update.frame, TranscriptFrame::Reset { .. }),
        "transcript stream did not start with a reset"
    );
    let mut entries = Vec::new();
    apply_transcript_frame(&mut entries, update.frame)?;
    Ok((
        json!({"sessionId": chat, "transcript": entries, "contextUsage": update.context_usage}),
        watch,
    ))
}

async fn chat_owner(engine: &EngineHandle, chat: &str) -> Result<()> {
    let view = engine
        .client()
        .call(methods::GET_SESSION_VIEW, json!({"chatId": chat}))
        .await?;
    ensure!(
        view.get("owner").and_then(Value::as_str) == Some("chat"),
        "denied: session {chat} is CLI-owned or changing owner; use the human UI to restore Chat mode"
    );
    Ok(())
}

async fn agent(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    let is_send = method == "agent.send";
    let multi = bool_param(&params, "multi")?
        && matches!(
            method,
            "agent.send" | "agent.stop" | "agent.interrupt" | "layout.stop"
        );
    let (engine, targets) = this.update(cx, |this, cx| -> Result<_> {
        if is_send {
            this.control_allow(cx)?;
        }
        let ids = resolve(&this.layout, target(&params)?, multi)?;
        let mut sessions = BTreeSet::new();
        let mut targets = Vec::new();
        for id in ids {
            let pane = this.layout.pane(id).unwrap();
            let chat_id = pane
                .session_id
                .as_ref()
                .context("target pane has no session")?;
            let chat = this
                .source
                .read(cx)
                .chats
                .iter()
                .find(|c| &c.id == chat_id)
                .context("unknown session")?
                .clone();
            if is_send {
                this.control_scope(
                    chat.space_id
                        .as_deref()
                        .context("API sends require a workspace session")?,
                    cx,
                    false,
                )?;
                ensure!(
                    pane.mode == PaneMode::Chat,
                    "denied: CLI-owned targets cannot receive API prompts"
                );
            }
            if sessions.insert(chat_id.clone()) {
                targets.push((id, chat));
            }
        }
        Ok((
            this.source
                .read(cx)
                .engine()
                .cloned()
                .context("engine is not connected")?,
            targets,
        ))
    })??;
    if matches!(method, "agent.read" | "agent.subscribe") {
        let (id, chat) = &targets[0];
        let (snapshot, mut watch) = within(cx, 20, transcript(&engine, &chat.id)).await?;
        if method == "agent.read" {
            return Ok(snapshot);
        }
        let topic = format!("agent:{}", chat.id);
        let topic_for_task = topic.clone();
        let initial = snapshot.clone();
        let chat_id = chat.id.clone();
        let pane_id = *id;
        this.update(cx, |this, cx| -> Result<()> {
            ensure!(this.control_watches.contains_key(&topic) || this.control_watches.len() < MAX_BATCH,
                "subscription limit reached");
            let hub = this.events.clone().context("event hub is not running")?;
            let task = cx.spawn(async move |this, cx| {
                let mut entries = match serde_json::from_value(initial["transcript"].clone()) {
                    Ok(entries) => entries,
                    Err(_) => return,
                };
                while let Some(value) = watch.recv().await {
                    if !this.update(cx, |this, _| this.layout.pane(pane_id)
                        .is_some_and(|p| p.session_id.as_deref() == Some(&chat_id))).unwrap_or(false) { break; }
                    let result = serde_json::from_value::<TranscriptUpdate>(value).map_err(anyhow::Error::from)
                        .and_then(|update| {
                            apply_transcript_frame(&mut entries, update.frame)?;
                            Ok(json!({"sessionId": chat_id, "transcript": entries, "contextUsage": update.context_usage}))
                        });
                    match result {
                        Ok(snapshot) => { hub.publish(topic_for_task.clone(), snapshot); }
                        Err(error) => {
                            hub.publish(topic_for_task.clone(), json!({"error": error.to_string(), "resubscribe": true}));
                            return;
                        }
                    }
                }
                hub.publish(topic_for_task, json!({"closed": true, "resubscribe": true}));
            });
            this.control_watches.insert(topic.clone(), task);
            Ok(())
        })??;
        return Ok(serde_json::to_value(Subscription {
            topics: vec![topic],
            snapshot,
        })?);
    }
    if matches!(method, "agent.wait" | "agent.should-stop") {
        ensure!(
            method != "agent.wait" || bool_param(&params, "idle")?,
            "only wait with idle:true is implemented"
        );
        let seconds = params
            .get("timeout")
            .map(|v| v.as_u64().context("timeout must be integer seconds"))
            .transpose()?
            .unwrap_or(20);
        ensure!(
            (1..=25).contains(&seconds),
            "timeout must be 1..25 seconds, below the HTTP deadline"
        );
        let chat = targets[0].1.id.clone();
        return within(cx, seconds, async {
            let mut watch = engine.client().subscribe_checked(methods::WATCH_SESSIONS, json!({})).await?;
            while let Some(frame) = watch.recv().await {
                let sessions: Vec<Session> = serde_json::from_value(frame).context("invalid sessions snapshot")?;
                // The chat row was checked above. A missing live row means no active turn,
                // not a made-up completed run and not an unknown chat.
                let session = sessions.iter().find(|s| s.chat_id == chat);
                let status = session.map(|s| s.status).unwrap_or(SessionStatus::Idle);
                if method == "agent.should-stop" || status != SessionStatus::Working {
                    return Ok(json!({"sessionId": chat, "status": status,
                        "idle": status == SessionStatus::Idle, "awaitingInput": status == SessionStatus::AwaitingInput,
                        "shouldStop": status != SessionStatus::Working,
                        "activeTurnSettled": matches!(status, SessionStatus::Idle | SessionStatus::Errored)}));
                }
            }
            bail!("sessions stream closed before the active turn settled")
        }).await;
    }
    // Validate every destination before admitting any fanout command.
    let text = if is_send {
        Some(
            required_str(&params, "message")
                .or_else(|_| required_str(&params, "text"))?
                .to_owned(),
        )
    } else {
        None
    };
    let queue = bool_param(&params, "queue")?;
    let mut commands = Vec::new();
    for (id, chat) in &targets {
        chat_owner(&engine, &chat.id).await?;
        let command = if let Some(text) = &text {
            if queue {
                None
            } else {
                let config = chat
                    .config
                    .as_ref()
                    .context("session has no provider config; configure it in the UI first")?;
                let cwd = chat
                    .cwd
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .context("session has no cwd")?;
                Some(SessionCommandPayload::Run {
                    request: RunRequest {
                        prompt: text.clone(),
                        harness: Some(config.harness),
                        model: config.model.clone(),
                        reasoning: config.reasoning,
                        model_options: config.model_options.clone(),
                        cwd: cwd.clone(),
                        sandbox: SandboxLevel::WorkspaceWrite,
                        auto_approve: false,
                        resume: None,
                        attachments: Vec::new(),
                        worktree: None,
                    },
                    message_id: uuid::Uuid::new_v4().to_string(),
                })
            }
        } else {
            Some(SessionCommandPayload::Interrupt {})
        };
        commands.push((*id, chat.id.clone(), command));
    }
    let mut results = Vec::new();
    for (id, chat, command) in commands {
        let allowed = this.update(cx, |this, cx| -> Result<()> {
            if is_send {
                this.control_allow(cx)?;
                let source = this.source.read(cx);
                let row = source
                    .chats
                    .iter()
                    .find(|row| row.id == chat)
                    .context("session disappeared")?;
                this.control_scope(
                    row.space_id
                        .as_deref()
                        .context("session has no workspace")?,
                    cx,
                    false,
                )?;
            }
            ensure!(
                this.layout
                    .pane(id)
                    .is_some_and(|p| p.session_id.as_deref() == Some(&chat)),
                "target changed while preparing command"
            );
            Ok(())
        })?;
        let result = match allowed {
            Err(error) => Err(error),
            Ok(()) => {
                if let Some(command) = command {
                    engine
                        .client()
                        .call(
                            methods::QUEUE_COMMAND,
                            json!({"chatId": chat, "command": command}),
                        )
                        .await
                        .map_err(Into::into)
                } else {
                    engine
                        .client()
                        .call(
                            methods::QUEUE_MESSAGE,
                            json!({"chatId": chat, "text": text, "holdForTurnEnd": true}),
                        )
                        .await
                        .map_err(Into::into)
                }
            }
        };
        match result {
            Ok(receipt) => results.push(json!({"paneId": id, "sessionId": chat, "admitted": true, "completed": false, "receipt": receipt})),
            Err(error) => {
                if results.is_empty() { return Err(error); }
                results.push(json!({"paneId": id, "sessionId": chat, "admitted": false, "error": error.to_string()}));
                return Ok(json!({"partial": true, "results": results}));
            }
        }
    }
    Ok(json!({"results": results, "admitted": true, "completed": false}))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    version: u32,
    layout: WorkspaceLayout,
}

fn recipe_name(name: &str) -> Result<&str> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "recipe name must be 1..64 ASCII letters, digits, hyphens or underscores"
    );
    Ok(name)
}

fn recipe_root(this: &Workspace, params: &Value, cx: &Context<Workspace>) -> Result<PathBuf> {
    let root = this
        .source
        .read(cx)
        .data_dir
        .as_ref()
        .context("instance data directory is unavailable")?
        .join("layout-recipes");
    match if params.get("scope").is_some() {
        required_str(params, "scope")?
    } else {
        "user"
    } {
        "user" => Ok(root.join("user")),
        "worktree" => {
            use sha2::{Digest, Sha256};
            let id = one(&this.layout, target(params)?)?;
            let chat_id = this
                .layout
                .pane(id)
                .unwrap()
                .session_id
                .as_ref()
                .context("worktree scope requires a session")?;
            let chat = this
                .source
                .read(cx)
                .chats
                .iter()
                .find(|c| &c.id == chat_id)
                .context("unknown session")?;
            let cwd = chat
                .cwd
                .as_ref()
                .context("worktree scope requires a session cwd")?;
            let identity = format!("{}\0{}", chat.device_id, cwd);
            Ok(root.join(format!(
                "worktree-{:x}",
                Sha256::digest(identity.as_bytes())
            )))
        }
        _ => bail!("scope must be user or worktree"),
    }
}

fn no_symlinks(path: &Path) -> Result<()> {
    let mut walked = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, std::path::Component::ParentDir),
            "parent path components are forbidden"
        );
        walked.push(component);
        match std::fs::symlink_metadata(&walked) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "recipe paths cannot contain symlinks"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn recipe(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    let dry_run = flag(&params, "dryRun", "dry_run")?;
    let (root, layout) = this.update(cx, |this, cx| -> Result<_> {
        Ok((recipe_root(this, &params, cx)?, this.layout.clone()))
    })??;
    let name = if method == "layout.list" {
        None
    } else {
        Some(recipe_name(required_str(&params, "name")?)?.to_owned())
    };
    let method = method.to_owned();
    let operation = method.clone();
    let saved_layout = if method == "layout.save" {
        match params.get("plan").or_else(|| params.get("composition")) {
            Some(plan) => compose_plan(&layout, plan, &params)?,
            None => layout.clone(),
        }
    } else {
        layout.clone()
    };
    let loaded: Result<Value> = cx
        .background_executor()
        .spawn(async move {
            use std::io::{Read, Write};
            no_symlinks(&root)?;
            if operation == "layout.list" {
                if !root.exists() {
                    return Ok(json!({"recipes": []}));
                }
                let mut names = Vec::new();
                for entry in std::fs::read_dir(&root)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("json") {
                        continue;
                    }
                    if let Some(name) = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .filter(|s| recipe_name(s).is_ok())
                    {
                        names.push(name.to_owned());
                    }
                }
                names.sort();
                return Ok(json!({"recipes": names}));
            }
            let name = name.unwrap();
            let path = root.join(format!("{name}.json"));
            no_symlinks(&path)?;
            match operation.as_str() {
                "layout.save" => {
                    if dry_run {
                        return Ok(json!({"dryRun": true, "wouldSave": name}));
                    }
                    std::fs::create_dir_all(&root)?;
                    let mut layout = saved_layout;
                    for id in pane_order(&layout) {
                        layout.pane_mut(id).unwrap().session_id = None;
                    }
                    let bytes = serde_json::to_vec_pretty(&Recipe { version: 1, layout })?;
                    ensure!(
                        bytes.len() as u64 <= MAX_RECIPE_BYTES,
                        "recipe exceeds size limit"
                    );
                    // Publish a complete file without overwriting another instance's recipe.
                    let temporary = root.join(format!(".recipe-{}.tmp", uuid::Uuid::new_v4()));
                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut file = options.open(&temporary)?;
                    let result = file
                        .write_all(&bytes)
                        .and_then(|_| file.sync_all())
                        .and_then(|_| std::fs::hard_link(&temporary, &path));
                    drop(file);
                    let _ = std::fs::remove_file(&temporary);
                    result.context("recipe already exists or cannot be created")?;
                    Ok(json!({"saved": name}))
                }
                "layout.delete" => {
                    ensure!(path.is_file(), "unknown recipe {name}");
                    if dry_run {
                        return Ok(json!({"dryRun": true, "wouldDelete": name}));
                    }
                    std::fs::remove_file(path)?;
                    Ok(json!({"deleted": name}))
                }
                "layout.apply" => {
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)?
                        .take(MAX_RECIPE_BYTES + 1)
                        .read_to_end(&mut bytes)?;
                    ensure!(
                        bytes.len() as u64 <= MAX_RECIPE_BYTES,
                        "recipe exceeds size limit"
                    );
                    let recipe: Recipe = serde_json::from_slice(&bytes)?;
                    ensure!(recipe.version == 1, "unsupported recipe version");
                    Ok(serde_json::to_value(recipe.layout)?)
                }
                _ => unreachable!(),
            }
        })
        .await;
    let loaded = loaded?;
    if method != "layout.apply" {
        return Ok(loaded);
    }
    let recipe: WorkspaceLayout = serde_json::from_value(loaded)?;
    this.update(cx, |this, cx| -> Result<Value> {
        ensure!(
            this.layout.revision == layout.revision,
            "layout changed while reading the recipe; retry"
        );
        check_guard(&this.layout, &params, false)?;
        let draft = remap_recipe(&this.layout, recipe, explicit_ui(&params)?)?;
        if dry_run {
            return Ok(json!({"dryRun": true, "revision": this.layout.revision, "layout": draft}));
        }
        this.control_commit(draft, cx)
    })?
}

fn map_tree<T: Copy, U>(node: SplitNode<T>, map: &impl Fn(T) -> U) -> SplitNode<U> {
    match node {
        SplitNode::Leaf { content } => SplitNode::Leaf {
            content: map(content),
        },
        SplitNode::Split {
            horizontal,
            ratio,
            first,
            second,
        } => SplitNode::Split {
            horizontal,
            ratio,
            first: Box::new(map_tree(*first, map)),
            second: Box::new(map_tree(*second, map)),
        },
    }
}

fn remap_recipe(
    current: &WorkspaceLayout,
    recipe: WorkspaceLayout,
    mode: PaneMode,
) -> Result<WorkspaceLayout> {
    ensure!(
        mode == PaneMode::Chat,
        "a saved recipe has no sessions; apply with ui:chat, then launch explicitly"
    );
    recipe.validate()?;
    let mut next = current.next_id;
    let mut ids = std::collections::BTreeMap::new();
    for (view_id, view) in &recipe.views {
        let mut old = vec![view_id.0];
        for (tab_id, tab) in &view.tabs {
            old.push(tab_id.0);
            old.extend(tab.panes.keys().map(|p| p.0));
        }
        for id in old {
            ids.insert(id, next);
            next = next.checked_add(1).context("ID space exhausted")?;
        }
    }
    let mut draft = recipe;
    draft.root = map_tree(draft.root, &|id| ViewId(ids[&id.0]));
    draft.active_view_id = ViewId(ids[&draft.active_view_id.0]);
    draft.views = draft
        .views
        .into_iter()
        .map(|(id, mut view)| {
            view.active_tab_id = TabId(ids[&view.active_tab_id.0]);
            view.tab_order = view
                .ordered_tabs()
                .into_iter()
                .map(|id| TabId(ids[&id.0]))
                .collect();
            view.tabs = view
                .tabs
                .into_iter()
                .map(|(id, mut tab)| {
                    tab.root = map_tree(tab.root, &|id| PaneId(ids[&id.0]));
                    tab.active_pane_id = PaneId(ids[&tab.active_pane_id.0]);
                    tab.primary_pane_id = PaneId(ids[&tab.primary_pane_id.0]);
                    tab.panes = tab
                        .panes
                        .into_iter()
                        .map(|(id, mut pane)| {
                            pane.session_id = None;
                            pane.mode = mode;
                            (PaneId(ids[&id.0]), pane)
                        })
                        .collect();
                    (TabId(ids[&id.0]), tab)
                })
                .collect();
            (ViewId(ids[&id.0]), view)
        })
        .collect();
    draft.next_id = next;
    let mut validated = current.clone();
    validated.compose(current.revision, |layout| {
        *layout = draft;
        Ok(())
    })?;
    Ok(validated)
}

/// Planned runs launch only new empty cells. Validate the entire set before
/// assigning session IDs so a rejected plan cannot partially bind sessions.
fn prepare_planned_sessions(
    current: &WorkspaceLayout,
    draft: &mut WorkspaceLayout,
    mode: PaneMode,
) -> Result<Vec<(PaneId, String)>> {
    draft.validate()?;
    let cells: Vec<_> = pane_order(draft)
        .into_iter()
        .filter(|id| current.pane(*id).is_none())
        .collect();
    ensure!(
        (1..=MAX_BATCH).contains(&cells.len()),
        "planned run needs 1..{MAX_BATCH} new cells"
    );
    ensure!(
        cells
            .iter()
            .all(|id| draft.pane(*id).unwrap().session_id.is_none()),
        "planned launch cells must not supply sessionId"
    );
    Ok(cells
        .into_iter()
        .map(|id| {
            let session = uuid::Uuid::new_v4().to_string();
            let pane = draft.pane_mut(id).unwrap();
            pane.session_id = Some(session.clone());
            pane.mode = mode;
            (id, session)
        })
        .collect())
}

fn launch_config(params: &Value) -> Result<ChatConfig> {
    let mut config = params
        .get("config")
        .context("explicit config is required, including harness")?
        .as_object()
        .cloned()
        .context("config must be an object")?;
    // This control route always uses workspace-write, even when a caller
    // supplies a broader sandbox. Provider-only CLI flags remain sufficient.
    config.insert(
        "sandbox".into(),
        serde_json::to_value(SandboxLevel::WorkspaceWrite)?,
    );
    Ok(serde_json::from_value(Value::Object(config))?)
}

async fn launch(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    ensure!(
        params.get("worktree").is_none(),
        "create the worktree with worktree.create before launching its task"
    );
    let named = params.get("name").is_some();
    let plan = params.get("plan").or_else(|| params.get("composition"));
    let planned = named || plan.is_some();
    ensure!(
        !(named && plan.is_some()),
        "choose a named recipe or an inline composition"
    );
    ensure!(
        !planned || method == "layout.run",
        "planned launches require layout.run"
    );
    ensure!(
        !planned || (params.get("count").is_none() && params.get("into").is_none()),
        "a recipe or composition defines its own cells; omit count and into"
    );
    let mode = explicit_ui(&params)?;
    let count = params
        .get("count")
        .map(|v| v.as_u64().context("count must be an integer"))
        .transpose()?
        .unwrap_or(1);
    ensure!(
        (1..=MAX_BATCH as u64).contains(&count),
        "count must be 1..{MAX_BATCH}"
    );
    ensure!(
        method != "chat.new" || count == 1,
        "chat.new creates one session; use layout.run for count"
    );
    let config = launch_config(&params)?;
    let cwd = required_str(&params, "cwd")?.to_owned();
    ensure!(
        Path::new(&cwd).is_absolute(),
        "cwd must be explicit and absolute"
    );
    let prompt = params
        .get("prompt")
        .map(|_| required_str(&params, "prompt").map(str::to_owned))
        .transpose()?;
    ensure!(
        mode != PaneMode::Terminal || prompt.is_none(),
        "terminal launches do not accept prompt injection; use chat ui for API prompts"
    );
    let into = if params.get("into").is_some() {
        required_str(&params, "into")?
    } else {
        "tabs"
    };
    ensure!(
        matches!(into, "tabs" | "panes" | "views"),
        "into must be tabs, panes or views"
    );
    let split_direction = if into == "tabs" {
        Direction::Right
    } else {
        direction(&params)?
    };
    let (engine, mut draft, revision, device, space) =
        this.update(cx, |this, cx| -> Result<_> {
            this.control_allow(cx)?;
            check_guard(&this.layout, &params, false)?;
            let source = this.source.read(cx);
            let space = params
                .get("spaceId")
                .map(|_| required_str(&params, "spaceId").map(str::to_owned))
                .transpose()?;
            let workspace = space
                .as_deref()
                .context("API launches require explicit spaceId")?;
            this.control_scope(workspace, cx, true)?;
            let device = if let Some(id) = &space {
                source
                    .spaces
                    .iter()
                    .find(|s| &s.id == id)
                    .context("unknown spaceId")?
                    .device_id
                    .clone()
            } else {
                required_str(&params, "deviceId")?.to_owned()
            };
            ensure!(
                source.devices.iter().any(|d| d.id == device),
                "unknown deviceId"
            );
            Ok((
                source
                    .engine()
                    .cloned()
                    .context("engine is not connected")?,
                this.layout.clone(),
                this.layout.revision,
                device,
                space,
            ))
        })??;
    let launches = if planned {
        let current = draft.clone();
        draft = if named {
            let mut load_params = params.clone();
            load_params["dryRun"] = json!(true);
            load_params["ui"] = json!("chat");
            let loaded = recipe(this, cx, "layout.apply", load_params).await?;
            ensure!(
                loaded["revision"].as_u64() == Some(revision),
                "layout changed while loading recipe"
            );
            serde_json::from_value(loaded["layout"].clone())?
        } else {
            compose_plan(&current, plan.expect("inline plan"), &params)?
        };
        prepare_planned_sessions(&current, &mut draft, mode)?
    } else {
        let mut target_id = one(&draft, target(&params)?)?;
        let mut launches = Vec::new();
        for index in 0..count {
            let chat_id = uuid::Uuid::new_v4().to_string();
            let pane = PaneState {
                session_id: Some(chat_id.clone()),
                mode,
                label: params
                    .get("label")
                    .map(|_| {
                        required_str(&params, "label").map(|label| {
                            if count == 1 {
                                label.to_owned()
                            } else {
                                format!("{label}-{}", index + 1)
                            }
                        })
                    })
                    .transpose()?,
                ..Default::default()
            };
            let view = draft.pane_location(target_id).unwrap().0;
            match into {
                "tabs" => {
                    draft.add_tab(view, pane)?;
                }
                "panes" => {
                    draft.split_pane(target_id, split_direction, pane)?;
                }
                _ => {
                    draft.split_view(view, split_direction, pane)?;
                }
            }
            target_id = draft.active_pane_id().unwrap();
            launches.push((target_id, chat_id));
        }
        launches
    };
    if flag(&params, "dryRun", "dry_run")? {
        return Ok(json!({"dryRun": true, "revision": revision, "layout": draft}));
    }
    let mut created = Vec::new();
    for (_, chat) in &launches {
        this.update(cx, |this, cx| -> Result<()> {
            this.control_scope(space.as_deref().expect("validated workspace"), cx, true)?;
            ensure!(
                this.layout.revision == revision,
                "layout changed before session creation"
            );
            Ok(())
        })??;
        if let Err(error) = engine
            .client()
            .call(
                methods::MUTATE,
                json!({"op": "createChat", "chatId": chat,
            "spaceId": space, "deviceId": device, "config": config, "cwd": cwd}),
            )
            .await
        {
            bail!(
                "session creation failed: {error}; already created sessions: {}",
                json!(created)
            );
        }
        this.update(cx, |this, _| {
            this.control_consent
                .sessions
                .insert(chat.clone(), space.clone().expect("validated workspace"));
        })?;
        created.push(chat.clone());
    }
    let state = this
        .update(cx, |this, cx| -> Result<_> {
            this.control_scope(space.as_deref().expect("validated workspace"), cx, true)?;
            ensure!(
                this.layout.revision == revision,
                "layout changed during session creation; created sessions remain in chat.list"
            );
            this.control_commit(draft, cx)
        })?
        .with_context(|| format!("created sessions: {}", json!(created)))?;
    let mut receipts = Vec::new();
    for (pane, chat) in launches {
        this.update(cx, |this, cx| {
            this.control_scope(space.as_deref().expect("validated workspace"), cx, true)
        })??;
        if mode == PaneMode::Terminal {
            // Opening the engine-owned terminal is itself gated agent work.
            let terminal = engine
                .client()
                .call(methods::OPEN_SESSION_TERMINAL, json!({"chatId": chat}))
                .await;
            match terminal {
                Ok(terminal) => receipts.push(json!({"paneId": pane, "sessionId": chat, "terminal": terminal, "completed": false})),
                Err(error) => receipts.push(json!({"paneId": pane, "sessionId": chat, "error": error.to_string()})),
            }
        } else if let Some(prompt) = &prompt {
            chat_owner(&engine, &chat).await?;
            let command = SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: prompt.clone(),
                    harness: Some(config.harness),
                    model: config.model.clone(),
                    reasoning: config.reasoning,
                    model_options: config.model_options.clone(),
                    cwd: cwd.clone(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: false,
                    resume: None,
                    attachments: Vec::new(),
                    worktree: None,
                },
                message_id: uuid::Uuid::new_v4().to_string(),
            };
            match engine.client().call(methods::QUEUE_COMMAND, json!({"chatId": chat, "command": command})).await {
                Ok(receipt) => receipts.push(json!({"paneId": pane, "sessionId": chat, "admitted": true, "completed": false, "receipt": receipt})),
                Err(error) => receipts.push(json!({"paneId": pane, "sessionId": chat, "admitted": false, "error": error.to_string()})),
            }
        } else {
            receipts.push(
                json!({"paneId": pane, "sessionId": chat, "created": true, "completed": false}),
            );
        }
    }
    let capabilities = this.update(cx, |this, _| {
        created
            .into_iter()
            .map(|session| {
                let capability = uuid::Uuid::new_v4().to_string();
                this.control_consent.app_sessions.insert(
                    capability.clone(),
                    (session.clone(), space.clone().expect("validated workspace")),
                );
                json!({"sessionId":session, "sessionCapability":capability})
            })
            .collect::<Vec<_>>()
    })?;
    Ok(json!({"state": state, "results": receipts, "sessions": capabilities}))
}

async fn orchestration(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    use zeron_orchestration::{Scope, TeamSpec};
    // A cancellation must not race the gap between binding a role and admitting
    // its prompt. Reads and watches remain available while a launch is pending.
    let _team_operation = if matches!(method, "team.run" | "team.cancel" | "team.report") {
        let operations = this.update(cx, |this, _| this.control_consent.team_operations.clone())?;
        Some(operations.lock_owned().await)
    } else {
        None
    };
    let scope: Scope = serde_json::from_value(
        params
            .get("scope")
            .or_else(|| params.get("spec").and_then(|s| s.get("scope")))
            .context("explicit scope is required")?
            .clone(),
    )?;
    let store = this.update(cx, |this, cx| -> Result<_> {
        let source = this.source.read(cx);
        let space = source
            .spaces
            .iter()
            .find(|s| s.id == scope.workspace)
            .context("unknown workspace")?;
        ensure!(
            source
                .chats
                .iter()
                .any(|c| c.space_id.as_deref() == Some(&scope.workspace)
                    && c.cwd.as_deref() == Some(&scope.worktree))
                || space.path == scope.worktree,
            "worktree is not attached to this workspace"
        );
        this.orchestration
            .clone()
            .context("orchestration store is not ready")
    })??;
    if method == "team.run" {
        let spec: TeamSpec =
            serde_json::from_value(params.get("spec").context("spec is required")?.clone())?;
        ensure!(spec.scope == scope, "team scope differs from request scope");
        spec.validate()?;
        // Validate all role configs and launch arguments before creating a durable run.
        let mut launches = Vec::new();
        ensure!(
            (1..=zeron_orchestration::MAX_ROLES).contains(&spec.roles.len()),
            "team requires 1..8 roles"
        );
        let mut labels = BTreeSet::new();
        for role in &spec.roles {
            ensure!(labels.insert(&role.label), "duplicate role label");
            let harness: zeron_proto::HarnessId = serde_json::from_value(json!(role.provider))?;
            let mut launch_params = params.clone();
            launch_params.as_object_mut().unwrap().remove("spec");
            launch_params["spaceId"] = json!(scope.workspace);
            launch_params["cwd"] = json!(scope.worktree);
            launch_params["count"] = json!(1);
            launch_params["label"] = json!(role.label);
            launch_params["ui"] = json!("chat");
            launch_params["config"] = serde_json::to_value(ChatConfig {
                harness,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            })?;
            launch_params["dryRun"] = json!(true);
            launch(this, cx, "layout.run", launch_params.clone()).await?;
            launch_params["dryRun"] = json!(false);
            launch_params.as_object_mut().unwrap().remove("revision");
            launches.push(launch_params);
        }
        this.update(cx, |this, cx| {
            this.control_scope(&scope.workspace, cx, true)
        })??;
        if flag(&params, "dryRun", "dry_run")? {
            return Ok(json!({"dryRun":true, "spec":spec}));
        }
        let launch_engine = engine(this, cx)?;
        let create_store = store.clone();
        let team = cx
            .background_executor()
            .spawn(async move { create_store.team_create(spec) })
            .await?;
        let mut results = Vec::new();
        let admitted: Result<()> = async {
            for (role, mut launch_params) in team.roles.iter().zip(launches) {
                let capability = uuid::Uuid::new_v4().to_string();
                let prompt = format!("{}\n\nReport completion through team.report with id {}, scope {}, label {}, and reportCapability {}. Supply a report object with summary, and optional result_file.", role.prompt, team.id, serde_json::to_string(&scope)?, role.label, capability);
                launch_params.as_object_mut().unwrap().remove("prompt");
                let launched = launch(this, cx, "layout.run", launch_params).await?;
                let session = launched["results"][0]["sessionId"].as_str().context("launch returned no session")?.to_owned();
                let bind_store = store.clone(); let bind_scope = scope.clone(); let id = team.id.clone(); let label = role.label.clone(); let bound = session.clone();
                cx.background_executor().spawn(async move { bind_store.bind_role_session(&bind_scope, &id, &label, &bound) }).await?;
                results.push(launched);
                this.update(cx, |this, _| {
                    this.control_consent.reports.insert(capability, (team.id.clone(), role.label.clone(), session.clone()));
                })?;
                this.update(cx, |this, cx| this.control_scope(&scope.workspace, cx, true))??;
                let command = SessionCommandPayload::Run {
                    request: RunRequest { prompt, harness: Some(serde_json::from_value(json!(role.provider))?),
                        model: None, reasoning: None, model_options: Default::default(), cwd: scope.worktree.clone(),
                        sandbox: SandboxLevel::WorkspaceWrite, auto_approve: false, resume: None,
                        attachments: Vec::new(), worktree: None },
                    message_id: uuid::Uuid::new_v4().to_string(),
                };
                launch_engine.client().call(methods::QUEUE_COMMAND, json!({"chatId":session,"command":command})).await?;
            }
            Ok(())
        }.await;
        if let Err(error) = admitted {
            let cancel_store = store.clone();
            let cancel_scope = scope.clone();
            let id = team.id.clone();
            let cancelled = cx
                .background_executor()
                .spawn(async move { cancel_store.team_cancel(&cancel_scope, &id) })
                .await
                .with_context(|| {
                    format!(
                        "team {} launch failed: {error}; cancellation also failed",
                        team.id
                    )
                })?;
            // Roll back process admission too, including when the grant was revoked.
            // These are exactly the sessions admitted by this failed operation.
            let failures = interrupt_team_roles(&launch_engine, &cancelled).await;
            bail!(
                "team {} launch failed: {error}; run cancelled; created sessions: {}; interruption failures: {}",
                team.id,
                json!(results),
                json!(failures)
            );
        }
        return Ok(json!({"id": team.id, "results":results, "completed":false}));
    }
    if matches!(method, "coordination-state.watch" | "team.watch") {
        let watch_store = store.clone();
        let watch_scope = scope.clone();
        let mut subscription = cx
            .background_executor()
            .spawn(async move { watch_store.subscribe(&watch_scope) })
            .await?;
        let snapshot = serde_json::to_value(&subscription.snapshot)?;
        let topic = format!("coordination:{}", serde_json::to_string(&scope)?);
        this.update(cx, |this, cx| -> Result<()> {
            ensure!(
                this.control_watches.contains_key(&topic) || this.control_watches.len() < MAX_BATCH,
                "subscription limit reached"
            );
            let hub = this.events.clone().context("event hub unavailable")?;
            let publish_topic = topic.clone();
            let task = cx.spawn(async move |_, _| {
                loop {
                    match subscription.events.recv().await {
                        Ok(event) => {
                            hub.publish(publish_topic.clone(), json!(event));
                        }
                        Err(error) => {
                            hub.publish(
                                publish_topic,
                                json!({"error":error.to_string(), "resubscribe":true}),
                            );
                            break;
                        }
                    }
                }
            });
            this.control_watches.insert(topic.clone(), task);
            Ok(())
        })??;
        return Ok(serde_json::to_value(Subscription {
            topics: vec![topic],
            snapshot,
        })?);
    }
    let mut authenticated_session = None;
    if matches!(
        method,
        "team.report" | "team.cancel" | "coordination-state.set" | "coordination-state.delete"
    ) {
        this.update(cx, |this, cx| -> Result<()> {
            this.control_scope(&scope.workspace, cx, false)?;
            if method == "team.report" {
                authenticated_session = Some(this.control_consent.verify_report(
                    &scope.workspace,
                    required_str(&params, "id")?,
                    required_str(&params, "label")?,
                    required_str(&params, "reportCapability")?,
                )?);
            }
            Ok(())
        })??;
    }
    if method == "team.cancel" {
        let cancel_store = store.clone();
        let cancel_scope = scope.clone();
        let id = required_str(&params, "id")?.to_owned();
        // Commit cancellation first. Completed teams must never have their sessions
        // interrupted, and concurrent launches must fail their next role binding.
        let team = cx
            .background_executor()
            .spawn(async move { cancel_store.team_cancel(&cancel_scope, &id) })
            .await?;
        let engine = engine(this, cx)?;
        let failures = interrupt_team_roles(&engine, &team).await;
        ensure!(
            failures.is_empty(),
            "team {} is cancelled, but some sessions could not be interrupted: {}",
            team.id,
            json!(failures)
        );
        return Ok(json!(team));
    }
    let operation = method.to_owned();
    cx.background_executor()
        .spawn(async move {
            let id = || required_str(&params, "id");
            let key = || required_str(&params, "key");
            let version = || {
                params
                    .get("if_version")
                    .or_else(|| params.get("ifVersion"))
                    .and_then(Value::as_i64)
                    .context("exact if_version is required")
            };
            match operation.as_str() {
                "team.list" => Ok(json!({"teams":store.team_list(&scope)?})),
                "team.status" => Ok(json!(
                    store.team_get(&scope, id()?)?.context("unknown team")?
                )),
                "team.report" => Ok(json!(
                    store.team_report(
                        &scope,
                        id()?,
                        required_str(&params, "label")?,
                        authenticated_session
                            .as_deref()
                            .context("unauthenticated report")?,
                        serde_json::from_value(
                            params.get("report").context("report required")?.clone()
                        )?
                    )?
                )),
                "coordination-state.get" => Ok(json!(store.coordination_get(&scope, key()?)?)),
                "coordination-state.set" => Ok(json!(store.coordination_set(
                    &scope,
                    key()?,
                    version()?,
                    params.get("value").context("value required")?.clone()
                )?)),
                "coordination-state.delete" => Ok(json!(store.coordination_delete(
                    &scope,
                    key()?,
                    version()?
                )?)),
                _ => bail!("unsupported orchestration operation {operation}"),
            }
        })
        .await
}

async fn interrupt_team_roles(
    engine: &EngineHandle,
    team: &zeron_orchestration::Team,
) -> Vec<Value> {
    let mut failures = Vec::new();
    for role in &team.roles {
        if let Some(session) = &role.session_id
            && let Err(error) = engine
                .client()
                .call(
                    methods::QUEUE_COMMAND,
                    json!({"chatId":session,"command":SessionCommandPayload::Interrupt {}}),
                )
                .await
        {
            failures.push(json!({"sessionId":session,"error":error.to_string()}));
        }
    }
    failures
}

async fn workspace_control(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    let (engine, space, all) = this.update(cx, |this, cx| -> Result<_> {
        let source = this.source.read(cx);
        let id = params
            .get("workspaceId")
            .or_else(|| params.get("spaceId"))
            .and_then(Value::as_str)
            .or(source.selected_space.as_deref());
        Ok((
            source.engine().cloned().context("engine is disconnected")?,
            source
                .spaces
                .iter()
                .find(|s| Some(s.id.as_str()) == id)
                .cloned(),
            source.spaces.clone(),
        ))
    })??;
    if method == "workspace.list" {
        return Ok(json!({"workspaces":all}));
    }
    if method == "workspace.watch" {
        let mut watch = engine
            .client()
            .subscribe_checked(methods::WATCH_SPACES, json!({}))
            .await?;
        let snapshot = watch.recv().await.context("workspace stream closed")?;
        let topic = "workspaces".to_owned();
        this.update(cx, |this, cx| -> Result<()> {
            let hub = this.events.clone().context("event hub unavailable")?;
            let task = cx.spawn(async move |_, _| {
                while let Some(value) = watch.recv().await {
                    hub.publish("workspaces", json!({"workspaces":value}));
                }
                hub.publish("workspaces", json!({"closed":true,"resubscribe":true}));
            });
            this.control_watches.insert(topic.clone(), task);
            Ok(())
        })??;
        return Ok(serde_json::to_value(Subscription {
            topics: vec![topic],
            snapshot: json!({"workspaces":snapshot}),
        })?);
    }
    let space = space.context("unknown workspace")?;
    match method {
        "workspace.get" => return Ok(json!(space)),
        "workspace.select" | "workspace.open" => {
            return this.update(cx, |this, cx| -> Result<Value> {
                this.control_allow(cx)?;
                if flag(&params, "dryRun", "dry_run")? {
                    return Ok(json!({"dryRun":true,"workspace":space}));
                }
                this.source.update(cx, |source, cx| {
                    source.select_space(Some(space.id.clone()), cx)
                });
                cx.notify();
                Ok(json!(space))
            })?;
        }
        "worktree.status" | "worktree.list" => {
            return Ok(engine
                .client()
                .call(methods::LIST_BRANCHES, json!({"repoPath":space.path}))
                .await?);
        }
        _ => {}
    }
    this.update(cx, |this, cx| this.control_scope(&space.id, cx, false))??;
    let (rpc, payload) = match method {
        "workspace.update" => (
            methods::MUTATE,
            json!({"op":"renameSpace", "spaceId":space.id, "name":required_str(&params,"name")?}),
        ),
        "workspace.create" | "workspace.add" => {
            let path = required_str(&params, "path")?;
            ensure!(
                Path::new(path).is_absolute(),
                "workspace path must be absolute"
            );
            (
                methods::MUTATE,
                json!({"op":"createSpace","spaceId":uuid::Uuid::new_v4().to_string(),"deviceId":space.device_id,"path":path,"name":params.get("name")}),
            )
        }
        "workspace.delete" => {
            ensure!(all.len() > 1, "denied: cannot delete the last workspace");
            let payload = json!({"op":"deleteSpace", "spaceId":space.id});
            return queue_deletion(
                this,
                cx,
                &space.id,
                format!(
                    "Delete workspace {} and all of its chats?",
                    space.display_name()
                ),
                methods::MUTATE,
                payload,
                &params,
            );
        }
        "worktree.open" | "worktree.select" => {
            let chat = this.update(cx, |this, cx| -> Result<_> {
                let pane = one(&this.layout, target(&params)?)?;
                let id = this
                    .layout
                    .pane(pane)
                    .unwrap()
                    .session_id
                    .as_deref()
                    .context("target has no session")?;
                let source = this.source.read(cx);
                let chat = source
                    .chats
                    .iter()
                    .find(|chat| chat.id == id && chat.space_id.as_deref() == Some(&space.id))
                    .context("target is outside workspace")?;
                ensure!(
                    !source.sessions.iter().any(|s| s.chat_id == chat.id
                        && matches!(
                            s.status,
                            SessionStatus::Working | SessionStatus::AwaitingInput
                        )),
                    "stop session before selecting a worktree"
                );
                Ok(chat.id.clone())
            })??;
            chat_owner(&engine, &chat).await?;
            (
                methods::MUTATE,
                json!({"op":"setChatCwd","chatId":chat,"cwd":required_str(&params,"worktreePath")?}),
            )
        }
        "worktree.close" => {
            let path = required_str(&params, "worktreePath")?;
            return this.update(cx, |this, cx| -> Result<_> {
                let source = this.source.read(cx);
                let sessions: BTreeSet<_> = source
                    .chats
                    .iter()
                    .filter(|chat| {
                        chat.space_id.as_deref() == Some(&space.id)
                            && chat.cwd.as_deref() == Some(path)
                    })
                    .map(|chat| chat.id.as_str())
                    .collect();
                let mut draft = this.layout.clone();
                for id in pane_order(&this.layout) {
                    if draft.pane(id).is_some_and(|pane| {
                        pane.session_id
                            .as_deref()
                            .is_some_and(|id| sessions.contains(id))
                    }) {
                        draft.close_pane(id)?;
                    }
                }
                if flag(&params, "dryRun", "dry_run")? {
                    return Ok(json!({"dryRun":true,"layout":draft}));
                }
                this.control_commit(draft, cx)
            })?;
        }
        "worktree.create" => {
            this.update(cx, |this, cx| -> Result<()> {
                this.control_scope(&space.id, cx, true)?;
                let session = required_str(&params, "sessionId")?;
                let capability = required_str(&params, "sessionCapability")?;
                this.control_consent
                    .verify_app_session(&space.id, session, capability)?;
                Ok(())
            })??;
            ensure!(
                params.get("prompt").is_none() && params.get("task").is_none(),
                "create the worktree first, then launch its task explicitly"
            );
            (
                methods::CREATE_WORKTREE,
                json!({"repoPath":space.path,"branch":required_str(&params,"branch")?}),
            )
        }
        "worktree.delete" => {
            let path = required_str(&params, "worktreePath")?;
            ensure!(
                Path::new(path).is_absolute() && path != space.path,
                "cannot delete the workspace root"
            );
            let payload = json!({"repoPath":space.path,"worktreePath":path});
            return queue_deletion(
                this,
                cx,
                &space.id,
                format!("Delete worktree {path}?"),
                methods::DELETE_WORKTREE,
                payload,
                &params,
            );
        }
        _ => bail!("unsupported workspace operation {method}"),
    };
    if flag(&params, "dryRun", "dry_run")? {
        return Ok(json!({"dryRun":true,"request":payload}));
    }
    this.update(cx, |this, cx| this.control_scope(&space.id, cx, false))??;
    Ok(engine.client().call(rpc, payload).await?)
}

fn queue_deletion(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    workspace: &str,
    description: String,
    rpc: &'static str,
    payload: Value,
    params: &Value,
) -> Result<Value> {
    if flag(params, "dryRun", "dry_run")? {
        return Ok(json!({"dryRun":true,"request":payload,"requiresHumanConfirmation":true}));
    }
    ensure!(
        bool_param(params, "confirm")?,
        "destructive requests require confirm to stage a human confirmation"
    );
    this.update(cx, |this, cx| -> Result<()> {
        ensure!(
            this.control_consent.pending.is_none(),
            "another deletion is awaiting human confirmation"
        );
        this.control_consent.pending = Some(Deletion {
            workspace: workspace.to_owned(),
            description,
            rpc,
            payload,
        });
        cx.notify();
        Ok(())
    })??;
    Ok(json!({"confirmationRequired":true,"completed":false}))
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sections {
    sections: Vec<Section>,
    assignments: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    id: String,
    name: String,
}

fn edit_sections(sections: &mut Sections, method: &str, params: &Value) -> Result<Value> {
    let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = || -> Result<String> {
        let name = required_str(params, "name")?;
        ensure!(name.len() <= 128, "section name exceeds 128 bytes");
        Ok(name.to_owned())
    };
    match method {
        "section.create" => {
            ensure!(sections.sections.len() < 256, "section limit reached");
            let id = uuid::Uuid::new_v4().to_string();
            sections.sections.push(Section {
                id: id.clone(),
                name: name()?,
            });
            Ok(json!({"id":id}))
        }
        "section.edit" => {
            sections
                .sections
                .iter_mut()
                .find(|s| s.id == id)
                .context("unknown section")?
                .name = name()?;
            Ok(json!({"id":id}))
        }
        "section.delete" => {
            ensure!(
                sections.sections.iter().any(|s| s.id == id),
                "unknown section"
            );
            sections.sections.retain(|s| s.id != id);
            sections.assignments.retain(|_, s| s != id);
            Ok(json!({"deleted":id}))
        }
        "section.move" => {
            let index = params
                .get("index")
                .and_then(Value::as_u64)
                .context("index required")? as usize;
            ensure!(index < sections.sections.len(), "index out of range");
            let old = sections
                .sections
                .iter()
                .position(|s| s.id == id)
                .context("unknown section")?;
            let section = sections.sections.remove(old);
            sections.sections.insert(index, section);
            Ok(json!({"id":id,"index":index}))
        }
        "section.assign" => {
            ensure!(
                sections.sections.iter().any(|s| s.id == id),
                "unknown section"
            );
            sections.assignments.insert(
                required_str(params, "workspaceId")?.to_owned(),
                id.to_owned(),
            );
            Ok(json!({"assigned":true}))
        }
        "section.unassign" => {
            sections
                .assignments
                .remove(required_str(params, "workspaceId")?);
            Ok(json!({"unassigned":true}))
        }
        _ => bail!("unsupported section operation {method}"),
    }
}

async fn section_control(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    method: &str,
    params: Value,
) -> Result<Value> {
    let store = this.update(cx, |this, cx| -> Result<_> {
        if method != "section.list" {
            this.control_allow(cx)?;
            if matches!(method, "section.assign" | "section.unassign") {
                this.control_scope(required_str(&params, "workspaceId")?, cx, false)?;
            }
        }
        this.orchestration
            .clone()
            .context("orchestration store unavailable")
    })??;
    let operation = method.to_owned();
    cx.background_executor().spawn(async move {
        let scope = zeron_orchestration::Scope { workspace:"@instance".into(),worktree:"@sections".into() };
        let entry = store.coordination_get(&scope,"sections")?;
        let mut sections: Sections = entry.value.clone().map(serde_json::from_value).transpose()?.unwrap_or_default();
        if operation == "section.list" { return Ok(json!({"version":entry.version,"sections":sections.sections,"assignments":sections.assignments})); }
        let expected = params.get("if_version").or_else(||params.get("ifVersion")).and_then(Value::as_i64).context("exact if_version required")?;
        ensure!(expected == entry.version,"section version conflict");
        let result = edit_sections(&mut sections,&operation,&params)?;
        if flag(&params,"dryRun","dry_run")? { return Ok(json!({"dryRun":true,"result":result,"state":sections})); }
        let written = store.coordination_set(&scope,"sections",expected,json!(sections))?;
        Ok(json!({"version":written.version,"result":result,"state":written.value}))
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn act_and_orchestration_grants_are_distinct_and_workspace_scoped() {
        let mut consent = Consent::default();
        assert!(consent.check("one", Some("one"), false).is_err());
        consent.allow.insert("one".into());
        assert!(consent.check("one", Some("one"), false).is_ok());
        assert!(consent.check("one", Some("one"), true).is_err());
        assert!(consent.check("two", Some("two"), false).is_err());
        assert!(consent.check("one", Some("two"), false).is_err());
        consent.orchestrate.insert("one".into());
        assert!(consent.check("one", Some("one"), true).is_ok());
        consent.revoke("one");
        assert!(consent.check("one", Some("one"), true).is_err());
    }

    #[test]
    fn session_ids_cannot_forge_capabilities_and_revocation_destroys_bindings() {
        let mut consent = Consent::default();
        consent
            .sessions
            .insert("session".into(), "workspace".into());
        consent
            .app_sessions
            .insert("secret".into(), ("session".into(), "workspace".into()));
        consent.reports.insert(
            "report-secret".into(),
            ("team".into(), "role".into(), "session".into()),
        );
        assert!(
            consent
                .verify_app_session("workspace", "session", "session")
                .is_err()
        );
        assert!(
            consent
                .verify_app_session("other", "session", "secret")
                .is_err()
        );
        assert!(
            consent
                .verify_app_session("workspace", "other-session", "secret")
                .is_err()
        );
        assert!(
            consent
                .verify_app_session("workspace", "session", "secret")
                .is_err()
        );
        consent.human_input.insert("session".into());
        assert!(
            consent
                .verify_app_session("workspace", "session", "secret")
                .is_ok()
        );
        assert_eq!(
            consent
                .verify_report("workspace", "team", "role", "report-secret")
                .unwrap(),
            "session"
        );
        assert!(
            consent
                .verify_report("workspace", "team", "other-role", "report-secret")
                .is_err()
        );
        assert!(
            consent
                .verify_report("other-workspace", "team", "role", "report-secret")
                .is_err()
        );
        consent.revoke("workspace");
        assert!(!consent.human_input.contains("session"));
        assert!(
            consent
                .verify_report("workspace", "team", "role", "report-secret")
                .is_err()
        );
        assert!(
            consent
                .verify_app_session("workspace", "session", "secret")
                .is_err()
        );
    }

    #[test]
    fn section_deletion_removes_only_its_assignments_and_order_is_stable() {
        let mut sections = Sections::default();
        let first = edit_sections(&mut sections, "section.create", &json!({"name":"Active"}))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let second = edit_sections(&mut sections, "section.create", &json!({"name":"Later"}))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        edit_sections(
            &mut sections,
            "section.assign",
            &json!({"id":first,"workspaceId":"one"}),
        )
        .unwrap();
        edit_sections(
            &mut sections,
            "section.assign",
            &json!({"id":second,"workspaceId":"two"}),
        )
        .unwrap();
        edit_sections(
            &mut sections,
            "section.move",
            &json!({"id":second,"index":0}),
        )
        .unwrap();
        assert_eq!(sections.sections[0].id, second);
        let before = serde_json::to_value(&sections).unwrap();
        assert!(
            edit_sections(
                &mut sections,
                "section.move",
                &json!({"id":first,"index":99})
            )
            .is_err()
        );
        assert_eq!(serde_json::to_value(&sections).unwrap(), before);
        edit_sections(&mut sections, "section.delete", &json!({"id":first})).unwrap();
        assert!(!sections.assignments.contains_key("one"));
        assert_eq!(sections.assignments["two"], second);
        assert_eq!(sections.sections.len(), 1);
    }

    #[test]
    fn section_persistence_uses_cas_without_overwriting_a_concurrent_editor() {
        let store = zeron_orchestration::Store::open(":memory:").unwrap();
        let scope = zeron_orchestration::Scope {
            workspace: "@instance".into(),
            worktree: "@sections".into(),
        };
        let mut sections = Sections::default();
        edit_sections(&mut sections, "section.create", &json!({"name":"Active"})).unwrap();
        store
            .coordination_set(&scope, "sections", 0, json!(sections))
            .unwrap();
        assert!(
            store
                .coordination_set(&scope, "sections", 0, json!(Sections::default()))
                .is_err()
        );
        let restored: Sections = serde_json::from_value(
            store
                .coordination_get(&scope, "sections")
                .unwrap()
                .value
                .unwrap(),
        )
        .unwrap();
        assert_eq!(restored.sections.len(), 1);
        assert_eq!(restored.sections[0].name, "Active");
    }

    #[test]
    fn launch_config_requires_provider_and_fixes_sandbox_at_workspace_write() {
        assert!(launch_config(&json!({"config":{}})).is_err());
        let config = launch_config(&json!({"config":{"harness":"pi"}})).unwrap();
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
        let config =
            launch_config(&json!({"config":{"harness":"pi","sandbox":"danger-full-access"}}))
                .unwrap();
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn planned_run_preserves_topology_and_assigns_only_fresh_sessions() {
        let mut current = WorkspaceLayout::new();
        let original = current.active_pane_id().unwrap();
        current.pane_mut(original).unwrap().session_id = Some("existing".into());
        let mut draft = compose_plan(
            &current,
            &json!([
                {"op":"split-view","direction":"right","ui":"chat"},
                {"op":"split","direction":"down","ui":"chat"},
                {"op":"tab","ui":"chat"}
            ]),
            &json!({"revision":current.revision}),
        )
        .unwrap();
        let root = draft.root.clone();
        let launches = prepare_planned_sessions(&current, &mut draft, PaneMode::Chat).unwrap();
        assert_eq!(launches.len(), 3);
        assert_eq!(draft.root, root);
        assert_eq!(
            draft.pane(original).unwrap().session_id.as_deref(),
            Some("existing")
        );
        let ids: BTreeSet<_> = launches.iter().map(|(_, session)| session).collect();
        assert_eq!(ids.len(), 3);
        for (pane, session) in launches {
            assert_eq!(draft.pane(pane).unwrap().session_id, Some(session));
        }
        draft.validate().unwrap();

        let recipe = remap_recipe(&current, draft, PaneMode::Chat).unwrap();
        let mut terminal_recipe = recipe.clone();
        let launches =
            prepare_planned_sessions(&current, &mut terminal_recipe, PaneMode::Terminal).unwrap();
        assert_eq!(launches.len(), 4);
        assert!(
            launches
                .iter()
                .all(|(id, _)| terminal_recipe.pane(*id).unwrap().mode == PaneMode::Terminal)
        );
        assert_eq!(terminal_recipe.root, recipe.root);
    }

    #[test]
    fn planned_run_rejects_bound_cells_without_changing_any_cell() {
        let current = WorkspaceLayout::new();
        let mut draft = current.clone();
        let active = draft.active_pane_id().unwrap();
        draft
            .split_pane(active, Direction::Right, PaneState::default())
            .unwrap();
        draft
            .split_pane(
                active,
                Direction::Down,
                PaneState {
                    session_id: Some("caller-supplied".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let before = draft.clone();
        assert!(prepare_planned_sessions(&current, &mut draft, PaneMode::Chat).is_err());
        assert_eq!(draft, before);
        let mut no_new_cells = current.clone();
        assert!(prepare_planned_sessions(&current, &mut no_new_cells, PaneMode::Chat).is_err());
    }

    #[test]
    fn stable_ids_survive_moves_and_positions_follow_layout() {
        let mut layout = WorkspaceLayout::new();
        let first = layout.active_pane_id().unwrap();
        let second = layout
            .split_pane(
                first,
                Direction::Right,
                PaneState {
                    label: Some("reviewer".into()),
                    group: Some("review".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        layout.move_pane(second, first, Direction::Left).unwrap();
        assert_eq!(one(&layout, &format!("id:{}", second.0)).unwrap(), second);
        assert_eq!(one(&layout, "label:reviewer").unwrap(), second);
        assert_eq!(one(&layout, "pane:1").unwrap(), second);
        assert!(one(&layout, "pane:0").is_err());
        assert!(one(&layout, "group:review").is_err());
        assert_eq!(
            resolve(&layout, "group:review", true).unwrap(),
            vec![second]
        );
    }

    #[test]
    fn compose_is_atomic_and_requires_guard_and_ui() {
        let layout = WorkspaceLayout::new();
        let before = layout.clone();
        let plan = json!([{"op":"split", "ui":"chat", "direction":"right"}, {"op":"close", "to":"id:99999"}]);
        assert!(compose_plan(&layout, &plan, &json!({"revision":0})).is_err());
        assert_eq!(layout, before);
        let plan = json!([{"op":"split", "ui":"chat", "direction":"right"}]);
        assert!(compose_plan(&layout, &plan, &json!({})).is_err());
        assert!(compose_plan(&layout, &plan, &json!({"revision":1})).is_err());
        assert_eq!(
            compose_plan(&layout, &plan, &json!({"revision":0}))
                .unwrap()
                .revision,
            1
        );
        assert!(
            compose_plan(
                &layout,
                &json!([{"op":"split", "direction":"right"}]),
                &json!({"revision":0})
            )
            .is_err()
        );
    }

    #[test]
    fn compose_rejects_reused_ids_and_recipes_allocate_fresh_ids() {
        let mut layout = WorkspaceLayout::new();
        let active = layout.active_pane_id().unwrap();
        let retired = layout
            .split_pane(active, Direction::Right, PaneState::default())
            .unwrap();
        let stale = layout.clone();
        layout.close_pane(retired).unwrap();
        let mut wire = serde_json::to_value(stale).unwrap();
        wire["revision"] = json!(layout.revision);
        assert!(
            compose_plan(
                &layout,
                &wire,
                &json!({"revision": layout.revision, "ui": "chat"})
            )
            .is_err()
        );
        let applied = remap_recipe(&layout, WorkspaceLayout::new(), PaneMode::Chat).unwrap();
        assert!(pane_order(&applied).iter().all(|id| id.0 >= layout.next_id));
    }

    #[test]
    fn malformed_recipe_is_rejected_before_remapping_ids() {
        let current = WorkspaceLayout::new();
        let mut recipe = WorkspaceLayout::new();
        recipe.root = SplitNode::leaf(ViewId(u64::MAX));
        assert!(remap_recipe(&current, recipe, PaneMode::Chat).is_err());
        let mut recipe = WorkspaceLayout::new();
        recipe
            .views
            .get_mut(&recipe.active_view_id)
            .unwrap()
            .active_tab_id = TabId(u64::MAX);
        assert!(remap_recipe(&current, recipe, PaneMode::Chat).is_err());
    }

    #[test]
    fn recipe_names_cannot_escape_the_store() {
        for name in [
            "",
            ".",
            "..",
            "../secret",
            "/tmp/a",
            "a/b",
            "a\\b",
            "a.json",
            "a\0b",
        ] {
            assert!(recipe_name(name).is_err());
        }
        assert!(recipe_name("review-grid_2").is_ok());
    }
}

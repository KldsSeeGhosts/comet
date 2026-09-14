//! Create a provider session from the empty composer without submitting a prompt.
use super::*;
use crate::pickers::CheckoutPlan;
use anyhow::{Result, bail};
use serde_json::json;
use zeron_proto::{Chat, HarnessId, Worktree};
use zeron_rpc::methods;

pub fn create_chat_payload(
    chat_id: &str,
    space_id: Option<&str>,
    device_id: Option<&str>,
    cwd: &str,
    branch: Option<&str>,
    config: Option<&zeron_proto::ChatConfig>,
) -> serde_json::Value {
    let mut obj = json!({
        "op": "createChat",
        "chatId": chat_id,
        "cwd": cwd,
    });
    if let Some(space_id) = space_id {
        obj["spaceId"] = serde_json::Value::String(space_id.to_string());
    } else if let Some(device_id) = device_id {
        obj["deviceId"] = serde_json::Value::String(device_id.to_string());
    }
    if let Some(branch) = branch {
        obj["branch"] = serde_json::Value::String(branch.to_string());
    }
    if let Some(config) = config
        && let Ok(config_val) = serde_json::to_value(config)
    {
        obj["config"] = config_val;
    }
    obj
}

impl Workspace {
    pub(super) fn empty_cli_unavailable(&self, id: PaneId, cx: &App) -> Option<&'static str> {
        let runtime = self.panes.get(&id)?;
        let chat = runtime.chat.read(cx);
        let state = chat.state.read(cx);
        if state.engine().is_none() {
            return Some("Engine is not connected");
        }
        if state.effective_device_id() != state.local_device_id {
            return Some("Open CLI on the session's host device");
        }
        let config = chat.composer.read(cx).pickers().read(cx).resolved(cx);
        if config.harness != Some(HarnessId::Pi) {
            return Some("Select Pi to start a new CLI session");
        }
        if !config.model_options.is_empty() {
            return Some("CLI does not support the selected model options");
        }
        None
    }

    pub(super) fn start_empty_cli(&mut self, id: PaneId, cx: &mut Context<Self>) {
        self.mint_session_for_pane(id, true, cx);
    }

    pub(super) fn mint_session_for_pane(
        &mut self,
        id: PaneId,
        open_terminal: bool,
        cx: &mut Context<Self>,
    ) {
        if self.starting_cli.contains_key(&id) {
            return;
        }
        if open_terminal && let Some(reason) = self.empty_cli_unavailable(id, cx) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let Some(runtime) = self.panes.get(&id) else {
            return;
        };
        let chat = runtime.chat.read(cx);
        let state = chat.state.read(cx);
        let Some(engine) = state.engine().cloned() else {
            self.error = Some(if open_terminal {
                "Could not open CLI: Engine is not connected".into()
            } else {
                "Could not create session: Engine is not connected".into()
            });
            cx.notify();
            return;
        };
        let config = chat
            .composer
            .read(cx)
            .pickers()
            .read(cx)
            .resolved(cx)
            .chat_config();
        let plan = chat.composer.read(cx).pickers().read(cx).checkout_plan();
        let space = state.selected_space_row().cloned();
        let device = state
            .effective_device_id()
            .or(state.local_device_id.clone());
        let chat_id = uuid::Uuid::new_v4().to_string();
        let create = gpui_tokio::Tokio::spawn(cx, async move {
            tokio::time::timeout(Duration::from_secs(120), async move {
                let mut cwd = space.as_ref().map(|s| s.path.clone()).unwrap_or_else(|| "~".into());
                let mut branch = None;
                if space.is_some() {
                    match plan {
                        CheckoutPlan::CurrentCheckout { branch: selected } => branch = selected,
                        CheckoutPlan::ReuseWorktree { path, branch: selected } => { cwd = path; branch = Some(selected); }
                        CheckoutPlan::NewWorktree { base } => {
                            let value = engine.client().call(methods::CREATE_WORKTREE,
                                json!({"repoPath": cwd, "branch": base.unwrap_or_else(|| "HEAD".into())})).await?;
                            let worktree: Worktree = serde_json::from_value(value)?;
                            cwd = worktree.path; branch = Some(worktree.branch);
                        }
                    }
                }
                let payload = create_chat_payload(
                    &chat_id,
                    space.as_ref().map(|s| s.id.as_str()),
                    device.as_deref(),
                    &cwd,
                    branch.as_deref(),
                    config.as_ref(),
                );
                engine.client().call(methods::MUTATE, payload).await?;
                // Read the committed row before opening the terminal. No optimistic session
                // identity and no model turn are needed to initialize the native CLI.
                let mut watch = engine.client().subscribe_checked(methods::WATCH_CHATS, json!({})).await?;
                while let Some(frame) = watch.recv().await {
                    let chats: Vec<Chat> = serde_json::from_value(frame)?;
                    if let Some(chat) = chats.into_iter().find(|chat| chat.id == chat_id) {
                        return Ok::<_, anyhow::Error>(chat);
                    }
                }
                bail!("Chat watch closed before the new session became available")
            }).await.map_err(|_| anyhow::anyhow!("Creating the session timed out"))?
        });
        self.starting_cli.insert(
            id,
            cx.spawn(async move |this, cx| {
                let result: Result<Chat> = create
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                let _ = this.update(cx, |this, cx| {
                    this.starting_cli.remove(&id);
                    match result {
                        Ok(chat) => {
                            let pane_valid = this
                                .layout
                                .pane(id)
                                .is_some_and(|pane| pane.session_id.is_none());
                            let runtime_exists = this.panes.contains_key(&id);
                            if pane_valid && runtime_exists {
                                let runtime = this.panes.get(&id).unwrap();
                                let pane_state = runtime.chat.read(cx).state.clone();
                                this.source.update(cx, |state, cx| {
                                    if !state.chats.iter().any(|c| c.id == chat.id) {
                                        state.chats.push(chat.clone());
                                    }
                                    cx.notify();
                                });
                                pane_state.update(cx, |state, cx| {
                                    if !state.chats.iter().any(|c| c.id == chat.id) {
                                        state.chats.push(chat.clone());
                                    }
                                    state.select_chat(Some(chat.id.clone()), cx);
                                });
                                runtime
                                    .chat
                                    .update(cx, |p, cx| p.select(Some(chat.id.clone()), cx));
                                this.apply(
                                    |layout| {
                                        layout.compose(layout.revision, |draft| {
                                            draft.pane_mut(id).unwrap().session_id = Some(chat.id);
                                            Ok(())
                                        })
                                    },
                                    cx,
                                );
                                if open_terminal {
                                    this.open_terminal(id, cx);
                                }
                            }
                        }
                        Err(error) => {
                            this.error = Some(if open_terminal {
                                format!("Could not open CLI: {error:#}")
                            } else {
                                format!("Could not create session: {error:#}")
                            });
                        }
                    }
                    cx.notify();
                });
            }),
        );
        cx.notify();
    }
}

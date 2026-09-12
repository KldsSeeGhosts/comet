//! Create a provider session from the empty composer without submitting a prompt.
use super::*;
use crate::pickers::CheckoutPlan;
use anyhow::{Result, bail, ensure};
use serde_json::json;
use zeron_proto::{Chat, HarnessId, Worktree};
use zeron_rpc::methods;

impl Workspace {
    pub(super) fn empty_cli_unavailable(&self, id: PaneId, cx: &App) -> Option<&'static str> {
        let runtime = self.panes.get(&id)?;
        let chat = runtime.chat.read(cx);
        let state = chat.state.read(cx);
        if state.engine().is_none() { return Some("Engine is not connected"); }
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
        if self.starting_cli.contains_key(&id) { return; }
        let Some(runtime) = self.panes.get(&id) else { return; };
        let chat = runtime.chat.read(cx);
        let state = chat.state.read(cx);
        let Some(engine) = state.engine().cloned() else { return; };
        let Some(config) = chat.composer.read(cx).pickers().read(cx).resolved(cx).chat_config() else { return; };
        let plan = chat.composer.read(cx).pickers().read(cx).checkout_plan();
        let space = state.selected_space_row().cloned();
        let device = state.effective_device_id().or(state.local_device_id.clone());
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
                engine.client().call(methods::MUTATE, json!({"op":"createChat", "chatId":chat_id,
                    "spaceId":space.map(|s|s.id), "deviceId":device, "cwd":cwd,
                    "branch":branch, "config":config})).await?;
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
            }).await.map_err(|_| anyhow::anyhow!("Creating the CLI session timed out"))?
        });
        self.starting_cli.insert(id, cx.spawn(async move |this, cx| {
            let result: Result<Chat> = create.await.map_err(anyhow::Error::from).and_then(|result| result);
            let _ = this.update(cx, |this, cx| {
                this.starting_cli.remove(&id);
                let result = result.and_then(|chat| {
                    ensure!(this.layout.pane(id).is_some_and(|pane| pane.session_id.is_none()),
                        "Pane changed while CLI was being prepared; the new session is in history");
                    let runtime = this.panes.get(&id).ok_or_else(|| anyhow::anyhow!("Pane was closed"))?;
                    let pane_state = runtime.chat.read(cx).state.clone();
                    this.source.update(cx, |state, cx| {
                        if !state.chats.iter().any(|c| c.id == chat.id) { state.chats.push(chat.clone()); }
                        cx.notify();
                    });
                    pane_state.update(cx, |state, cx| {
                        if !state.chats.iter().any(|c| c.id == chat.id) { state.chats.push(chat.clone()); }
                        state.select_chat(Some(chat.id.clone()), cx);
                    });
                    this.apply(|layout| layout.compose(layout.revision, |draft| {
                        draft.pane_mut(id).unwrap().session_id = Some(chat.id);
                        Ok(())
                    }), cx);
                    this.open_terminal(id, cx);
                    Ok(())
                });
                if let Err(error) = result { this.error = Some(format!("Could not open CLI: {error:#}")); }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

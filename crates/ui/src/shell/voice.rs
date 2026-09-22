use super::voice_actions::{self, Target};
use super::*;
use noches_voice::{Call, CallConfig, Command, Event, ToolCall};
use serde_json::{Value, json};

const CREDENTIAL: &str = "https://api.openai.com/noches/voice";
#[derive(Default)]
pub(super) struct VoiceUi {
    call: Option<Call>,
    connecting: bool,
    live: bool,
    muted: bool,
    pub(super) panel: bool,
    last_context: String,
    generation: u64,
    message: String,
    user_caption: String,
    assistant_caption: String,
    last_action: String,
    usage: Option<Value>,
    pending: Option<Pending>,
    task: Option<Task<()>>,
    credential_task: Option<Task<()>>,
}
struct Pending {
    name: String,
    args: Value,
    selection: Value,
    focus: Option<FocusHandle>,
    reply: tokio::sync::oneshot::Sender<Value>,
}

enum Work {
    Done(Value),
    Rpc {
        method: &'static str,
        args: Value,
        snapshot: bool,
    },
    Create {
        args: Value,
        request: Option<zeron_proto::RunRequest>,
    },
}
impl Shell {
    pub(super) fn dismiss_voice_panel(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.voice.panel {
            return false;
        }
        self.voice.panel = false;
        if let Some(pending) = self.voice.pending.take() {
            let _ = pending
                .reply
                .send(json!({"error":"User declined this action"}));
        }
        cx.notify();
        true
    }
    pub(super) fn toggle_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.voice.call.is_some() || self.voice.connecting {
            self.stop_voice(cx);
        } else {
            self.start_voice(window, cx);
        }
    }
    pub(super) fn stop_voice(&mut self, cx: &mut Context<Self>) {
        self.voice.connecting = false;
        if let Some(call) = &self.voice.call {
            call.stop();
            self.voice.message = "Ending voice…".into();
        } else {
            self.voice.generation += 1;
            self.voice.task = None;
            self.voice.message = "Voice ended".into();
        }
        self.voice.live = false;
        if let Some(pending) = self.voice.pending.take() {
            let _ = pending.reply.send(json!({"error":"Voice call ended"}));
        }
        cx.notify();
    }
    pub(super) fn mute_voice(&mut self, cx: &mut Context<Self>) {
        if let Some(call) = &self.voice.call
            && self.voice.live
        {
            let muted = !self.voice.muted;
            match call.command(Command::Mute(muted)) {
                Ok(()) => self.voice.muted = muted,
                Err(error) => {
                    if muted {
                        self.voice.muted = true;
                    }
                    self.voice.message = error.to_string();
                }
            }
            cx.notify();
        }
    }
    fn start_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.voice.call.is_some() || self.voice.connecting {
            return;
        }
        self.voice.connecting = true;
        self.voice.panel = true;
        self.voice.muted = false;
        self.voice.generation += 1;
        let generation = self.voice.generation;
        self.voice.message = "Connecting to GPT-Live…".into();
        self.voice.user_caption.clear();
        self.voice.assistant_caption.clear();
        self.voice.last_action.clear();
        self.voice.usage = None;
        let credentials = cx.read_credentials(CREDENTIAL);
        self.voice.task = Some(cx.spawn_in(window, async move |this, cx| {
            let key = match std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.trim().is_empty()) {
                Some(key) => Ok(key),
                None => match credentials.await {
                    Ok(Some((_, bytes))) => String::from_utf8(bytes).map_err(|_| "Stored API key is invalid".to_string()),
                    Ok(None) => Err("Copy an OpenAI API key, then choose Paste API key. GPT-Live uses API billing, separate from a ChatGPT subscription.".into()),
                    Err(_) => Err("Cannot read the system credential store. Use OPENAI_API_KEY or unlock the credential store.".into()),
                },
            };
            let events = this.update_in(cx, |shell, _, cx| {
                if shell.voice.generation != generation || !shell.voice.connecting { return None; }
                let result = key.and_then(|key| {
                    let config = CallConfig::new(key, voice_actions::tool_schemas(), voice_actions::INSTRUCTIONS.into());
                    Call::start(config).map_err(|e| e.to_string())
                });
                match result {
                    Ok((call, events)) => { shell.voice.call = Some(call); Some(events) }
                    Err(error) => { shell.voice.connecting = false; shell.voice.message = error; cx.notify(); None }
                }
            }).ok().flatten();
            let Some(mut events) = events else { return; };
            while let Some(event) = events.recv().await {
                let ended = matches!(event, Event::Ended(_));
                if this.update_in(cx, |shell, window, cx| {
                    if shell.voice.generation != generation { return; }
                    match event {
                        Event::Started => {
                            if shell.voice.call.as_ref().is_some_and(|call| call.is_stopping()) { return; }
                            shell.voice.connecting = false; shell.voice.live = true;
                            shell.voice.message = "Listening".into();
                            let context = shell.voice_context(cx);
                            if let Some(call) = &shell.voice.call {
                                let _ = call.command(Command::Context(format!("Current Noches selection: {}. Use get_context before acting.", compact_selection(&context))));
                            }
                        }
                        Event::Transcript { user, delta } => {
                            let caption = if user { &mut shell.voice.user_caption } else { &mut shell.voice.assistant_caption };
                            caption.push_str(&delta); retain_tail(caption, 1200);
                        }
                        Event::Tool(call) => shell.receive_voice_tool(call, window, cx),
                        Event::Usage(usage) => shell.voice.usage = Some(usage),
                        Event::Ended(error) => {
                            shell.voice.call = None; shell.voice.live = false; shell.voice.connecting = false;
                            shell.voice.pending = None;
                            shell.voice.message = error.unwrap_or_else(|| "Voice ended".into());
                        }
                    }
                    cx.notify();
                }).is_err() { break; }
                if ended { break; }
            }
            // A panicking audio driver can close its channel without an Ended event.
            this.update_in(cx, |shell, _, cx| {
                if shell.voice.generation == generation && shell.voice.call.is_some() {
                    shell.voice.call = None;
                    shell.voice.live = false;
                    shell.voice.connecting = false;
                    shell.voice.pending = None;
                    shell.voice.message = "Voice connection closed".into();
                    cx.notify();
                }
            }).ok();
        }));
        cx.notify();
    }
    fn paste_voice_key(&mut self, cx: &mut Context<Self>) {
        let Some(key) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.voice.message = "Copy your OpenAI API key first".into();
            cx.notify();
            return;
        };
        let key = key.trim();
        if !key.starts_with("sk-") || key.len() < 20 || key.chars().any(char::is_whitespace) {
            self.voice.message = "The clipboard does not contain an OpenAI API key".into();
            cx.notify();
            return;
        }
        let write = cx.write_credentials(CREDENTIAL, "OpenAI", key.as_bytes());
        self.voice.credential_task = Some(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |shell, cx| {
                shell.voice.message = if result.is_ok() {
                    "API key saved in the system credential store. You can start voice.".into()
                } else {
                    "Could not save the key in the system credential store".into()
                };
                cx.notify();
            })
            .ok();
        }));
    }
    fn forget_voice_key(&mut self, cx: &mut Context<Self>) {
        self.stop_voice(cx);
        let delete = cx.delete_credentials(CREDENTIAL);
        self.voice.credential_task = Some(cx.spawn(async move |this, cx| {
            let result = delete.await;
            this.update(cx, |shell, cx| {
                shell.voice.message = if result.is_ok() {
                    "Stored API key removed. OPENAI_API_KEY, if set, still applies.".into()
                } else {
                    "Could not remove the stored API key".into()
                };
                cx.notify();
            })
            .ok();
        }));
    }
    pub(super) fn sync_voice_context(&mut self, cx: &mut Context<Self>) {
        if !self.voice.live {
            return;
        }
        if self.state.read(cx).engine().is_none() {
            self.stop_voice(cx);
            return;
        }
        let context = compact_selection(&self.voice_context(cx)).to_string();
        if context != self.voice.last_context {
            if let Some(call) = &self.voice.call {
                if call
                    .command(Command::Context(format!(
                        "Noches selection changed: {context}. Get fresh context before acting."
                    )))
                    .is_ok()
                {
                    self.voice.last_context = context;
                }
            }
        }
    }
    fn voice_context(&self, cx: &App) -> Value {
        let s = self.state.read(cx);
        json!({"selectedChat":s.selected_chat_row(),"selectedSpace":s.selected_space_row(),
            "deviceId":s.effective_device_id(),"localDeviceId":s.local_device_id,
            "route":match self.route { Route::Chat => "chat", Route::Settings(_) => "settings" },
            "composerText":self.composer.read(cx).input.read(cx).text(),
            "activeSurface":format!("{:?}",self.resolved_right_active(cx)),
            "defaultConfig":self.composer.read(cx).voice_config(cx),
            "voiceModel":"gpt-live-1", "backendModel":"gpt-5.6-luna"})
    }
    fn receive_voice_tool(&mut self, call: ToolCall, window: &mut Window, cx: &mut Context<Self>) {
        if call.reply.is_closed()
            || !self.voice.live
            || self.voice.call.as_ref().is_none_or(|c| c.is_stopping())
        {
            return;
        }
        let result = match call.name.as_str() {
            "get_context" => {
                let _ = call.reply.send(self.voice_context(cx));
                return;
            }
            "list_actions" => {
                let query = call.arguments["query"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase();
                let actions: Vec<_> = voice_actions::catalog()
                    .iter()
                    .filter(|a| {
                        format!("{} {}", a.name, a.description)
                            .to_lowercase()
                            .contains(&query)
                    })
                    .map(|a| a.descriptor())
                    .collect();
                let _ = call.reply.send(json!({"actions":actions}));
                return;
            }
            "execute_action" => (|| {
                let name = call.arguments["name"]
                    .as_str()
                    .ok_or("Missing action name")?;
                let raw = call.arguments["arguments"]
                    .as_str()
                    .ok_or("arguments must be a JSON object encoded as a string")?;
                let args: Value = serde_json::from_str(raw).map_err(|_| "Invalid action JSON")?;
                let action = voice_actions::catalog()
                    .into_iter()
                    .find(|a| a.name == name)
                    .ok_or("Unknown Noches action")?;
                action.validate(&args)?;
                Ok((name.to_string(), args, action.confirm))
            })(),
            _ => Err("Unknown voice tool".to_string()),
        };
        match result {
            Err(error) => {
                let _ = call.reply.send(json!({"error":error}));
            }
            Ok((name, args, confirm)) => {
                self.voice.last_action = name.replace('_', " ");
                if confirm {
                    if self.voice.pending.is_some() {
                        let _ = call
                            .reply
                            .send(json!({"error":"Another action is awaiting confirmation"}));
                        return;
                    }
                    self.voice.pending = Some(Pending {
                        name,
                        args,
                        selection: compact_selection(&self.voice_context(cx)),
                        focus: window.focused(cx),
                        reply: call.reply,
                    });
                    self.voice.panel = true;
                } else {
                    self.execute_voice_action(name, args, call.reply, window, cx);
                }
            }
        }
    }
    fn execute_voice_action(
        &mut self,
        name: String,
        args: Value,
        reply: tokio::sync::oneshot::Sender<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reply.is_closed() || !self.voice.live {
            return;
        }
        let work = match self.prepare_voice_action(&name, args, window, cx) {
            Ok(work) => work,
            Err(error) => {
                let _ = reply.send(json!({"error":error}));
                return;
            }
        };
        if let Work::Done(value) = work {
            let _ = reply.send(value);
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            let _ = reply.send(json!({"error":"Engine disconnected"}));
            return;
        };
        let task = Tokio::spawn(cx, async move {
            if reply.is_closed() {
                return;
            }
            let operation = async {
                match work {
                    Work::Rpc {
                        method,
                        args,
                        snapshot,
                    } => {
                        if snapshot {
                            let mut subscription = engine
                                .client()
                                .subscribe_checked(method, args)
                                .await
                                .map_err(|e| e.to_string())?;
                            subscription
                                .recv()
                                .await
                                .map(snapshot_tail)
                                .ok_or_else(|| "Snapshot stream ended".to_string())
                        } else {
                            engine
                                .client()
                                .call(method, args)
                                .await
                                .map_err(|e| e.to_string())
                        }
                    }
                    Work::Create { args, request } => {
                        let id = args["chatId"].clone();
                        // Verify config against the target host's actual model catalog before creation.
                        if let Some(config) = args.get("config") {
                            let models = engine.client().call(methods::LIST_MODELS, json!({"harness":config["harness"],"targetDeviceId":args["deviceId"]})).await.map_err(|e|e.to_string())?;
                            if let Some(model) = config["model"].as_str()
                                && !models
                                    .as_array()
                                    .is_some_and(|m| m.iter().any(|m| m["id"] == model))
                            {
                                return Err(
                                    "Requested model is not available on the selected device"
                                        .into(),
                                );
                            }
                        }
                        if reply.is_closed() {
                            return Err("Voice call ended".into());
                        }
                        engine
                            .client()
                            .call(methods::MUTATE, args)
                            .await
                            .map_err(|e| e.to_string())?;
                        if let Some(request) = request {
                            if reply.is_closed() {
                                return Ok(
                                    json!({"chatId":id,"created":true,"promptQueued":false}),
                                );
                            }
                            let result = engine.client().call(methods::QUEUE_COMMAND, json!({"chatId":id,"command":{
                                "kind":"run","request":request,"messageId":uuid::Uuid::new_v4().to_string()
                            }})).await;
                            return Ok(match result {
                                Ok(ack) => {
                                    json!({"chatId":id,"created":true,"promptQueued":true,"ack":ack})
                                }
                                Err(error) => {
                                    json!({"chatId":id,"created":true,"promptQueued":false,"error":error.to_string()})
                                }
                            });
                        }
                        Ok(json!({"chatId":id,"created":true,"promptQueued":false}))
                    }
                    Work::Done(value) => Ok(value),
                }
            };
            let result = match tokio::time::timeout(Duration::from_secs(45), operation).await {
                Ok(Ok(value)) => bounded_result(value),
                Ok(Err(error)) => json!({"error":error}),
                Err(_) => {
                    json!({"error":"Action timed out. Outcome unknown; inspect state before retrying.","outcomeUnknown":true})
                }
            };
            let _ = reply.send(result);
        });
        cx.spawn(async move |_, _| {
            let _ = task.await;
        })
        .detach();
    }
    fn prepare_voice_action(
        &mut self,
        name: &str,
        mut args: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Work, String> {
        let action = voice_actions::catalog()
            .into_iter()
            .find(|a| a.name == name)
            .ok_or("Unknown action")?;
        action.validate(&args)?;
        // IDs must resolve in this workspace. Never route a file operation to
        // a guessed host, or execute it after a chat/project disappeared.
        let state = self.state.read(cx);
        let chat = if let Some(id) = args["chatId"].as_str().filter(|id| *id != "new") {
            Some(
                state
                    .chats
                    .iter()
                    .find(|c| c.id == id)
                    .cloned()
                    .ok_or("Session no longer exists")?,
            )
        } else {
            None
        };
        let space = if let Some(id) = args["spaceId"].as_str() {
            Some(
                state
                    .spaces
                    .iter()
                    .find(|s| s.id == id)
                    .cloned()
                    .ok_or("Project no longer exists")?,
            )
        } else {
            None
        };
        if let (Some(chat), Some(space)) = (&chat, &space)
            && chat.space_id.as_deref() != Some(space.id.as_str())
        {
            return Err("Session does not belong to that project".into());
        }
        let host = chat
            .as_ref()
            .map(|c| c.device_id.clone())
            .or_else(|| space.as_ref().map(|s| s.device_id.clone()));
        if let Some(target) = args["targetDeviceId"].as_str() {
            if host.as_deref().is_some_and(|host| host != target) {
                return Err("Target device does not own this session/project".into());
            }
            if !state.devices.iter().any(|d| d.id == target)
                && state.local_device_id.as_deref() != Some(target)
            {
                return Err("Unknown device".into());
            }
        }
        if name == "set_session_config" {
            let config: zeron_proto::ChatConfig =
                serde_json::from_value(args["config"].clone()).map_err(|e| e.to_string())?;
            let old = chat
                .as_ref()
                .and_then(|c| c.config.as_ref())
                .ok_or("Session has no saved config")?;
            if config.harness != old.harness || config.sandbox != old.sandbox {
                return Err(
                    "Change the harness or sandbox through the normal session controls".into(),
                );
            }
        }
        match action.target {
            Target::Rpc(method) => {
                if action.parameters["properties"]
                    .get("targetDeviceId")
                    .is_some()
                    && args.get("targetDeviceId").is_none()
                {
                    if let Some(host) = host.or_else(|| state.effective_device_id()) {
                        args["targetDeviceId"] = json!(host);
                    }
                }
                return Ok(Work::Rpc {
                    method,
                    args,
                    snapshot: false,
                });
            }
            Target::Snapshot(method) => {
                return Ok(Work::Rpc {
                    method,
                    args,
                    snapshot: true,
                });
            }
            Target::Mutation(op) => {
                args["op"] = json!(op);
                return Ok(Work::Rpc {
                    method: methods::MUTATE,
                    args,
                    snapshot: false,
                });
            }
            Target::Ui => {}
        }
        let ok = || Work::Done(json!({"ok":true}));
        match name {
            "list_sessions" => {
                let query = args["query"].as_str().unwrap_or("").to_lowercase();
                let sessions: Vec<_> = state.chats.iter().filter(|chat| {
                    let project = state.space_for_chat(chat).map(|s|s.display_name()).unwrap_or("");
                    format!("{} {project} {}", chat.title.as_deref().unwrap_or("New session"), chat.last_message_preview.as_deref().unwrap_or("")).to_lowercase().contains(&query)
                }).take(100).map(|chat|json!({"chat":chat,"status":state.display_status_for(chat,Utc::now())})).collect();
                Ok(Work::Done(json!({"sessions":sessions,"limit":100})))
            }
            "list_projects" => Ok(Work::Done(json!({"projects":state.spaces}))),
            "list_devices" => Ok(Work::Done(json!({"devices":state.devices}))),
            "focus_session" => {
                self.open_chat(chat.unwrap().id, cx);
                Ok(ok())
            }
            "select_project" => {
                self.open_new_session(cx);
                self.state
                    .update(cx, |s, cx| s.select_space(Some(space.unwrap().id), cx));
                Ok(ok())
            }
            "create_session" => {
                let projectless = args["projectless"].as_bool().unwrap_or(false);
                if projectless && space.is_some() {
                    return Err("Choose a project or projectless, not both".into());
                }
                let space = if projectless {
                    None
                } else {
                    space.or_else(|| state.selected_space_row().cloned())
                };
                let device = args["deviceId"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| space.as_ref().map(|s| s.device_id.clone()))
                    .or_else(|| state.effective_device_id())
                    .ok_or("No target device")?;
                if !state.devices.iter().any(|d| d.id == device)
                    && state.local_device_id.as_deref() != Some(&device)
                {
                    return Err("Unknown device".into());
                }
                if space.as_ref().is_some_and(|s| s.device_id != device) {
                    return Err("Project belongs to a different device".into());
                }
                let config: zeron_proto::ChatConfig = if let Some(config) = args.get("config") {
                    serde_json::from_value(config.clone()).map_err(|e| e.to_string())?
                } else {
                    self.composer
                        .read(cx)
                        .voice_config(cx)
                        .ok_or("Choose an agent in the composer first")?
                };
                if config.sandbox != zeron_proto::SandboxLevel::WorkspaceWrite {
                    return Err(
                        "Voice-created sessions use the normal workspace-write sandbox".into(),
                    );
                }
                let cwd = space
                    .as_ref()
                    .map(|s| s.path.clone())
                    .unwrap_or_else(|| "~".into());
                let request = args["prompt"].as_str().map(|prompt| {
                    let mut run = run_request(prompt, &config, cwd.clone());
                    if let Some(base) = args["baseRef"].as_str() {
                        run.worktree = Some(zeron_proto::WorktreeSpec {
                            repo_path: cwd.clone(),
                            base: base.into(),
                            space_id: space.as_ref().map(|s| s.id.clone()),
                        });
                    }
                    run
                });
                if args.get("baseRef").is_some() && (space.is_none() || request.is_none()) {
                    return Err("An isolated worktree requires a project and prompt".into());
                }
                Ok(Work::Create {
                    args: json!({"op":"createChat","chatId":uuid::Uuid::new_v4().to_string(),
                    "spaceId":space.map(|s|s.id),"deviceId":device,"config":config}),
                    request,
                })
            }
            "send_message" | "steer_session" | "stop_session" | "respond_to_question" => {
                let chat = chat.ok_or("Choose a session")?;
                let text = args["text"].as_str().unwrap_or("");
                if name == "send_message"
                    && matches!(
                        state.indicator_for(&chat.id, Utc::now()),
                        Indicator::Working | Indicator::AwaitingInput
                    )
                {
                    return Ok(Work::Rpc {
                        method: methods::QUEUE_MESSAGE,
                        args: json!({"chatId":chat.id,"text":text,"holdForTurnEnd":true}),
                        snapshot: false,
                    });
                }
                let command = match name {
                    "send_message" => {
                        let config = chat.config.as_ref().ok_or(
                            "This session has no saved agent config; select one in the composer",
                        )?;
                        let cwd = chat
                            .cwd
                            .clone()
                            .or_else(|| state.space_for_chat(&chat).map(|s| s.path.clone()))
                            .unwrap_or_else(|| "~".into());
                        json!({"kind":"run","request":run_request(text,config,cwd),"messageId":uuid::Uuid::new_v4().to_string()})
                    }
                    "steer_session" => {
                        json!({"kind":"steer","prompt":text,"messageId":uuid::Uuid::new_v4().to_string()})
                    }
                    "stop_session" => json!({"kind":"interrupt"}),
                    _ => {
                        json!({"kind":"respondInput","requestId":args["requestId"],"answers":args["answers"]})
                    }
                };
                Ok(Work::Rpc {
                    method: methods::QUEUE_COMMAND,
                    args: json!({"chatId":chat.id,"command":command}),
                    snapshot: false,
                })
            }
            "add_project" => {
                let device = args["deviceId"].as_str().unwrap();
                if !state.devices.iter().any(|d| d.id == device)
                    && state.local_device_id.as_deref() != Some(device)
                {
                    return Err("Unknown device".into());
                }
                args["op"] = json!("createSpace");
                args["spaceId"] = json!(uuid::Uuid::new_v4().to_string());
                Ok(Work::Rpc {
                    method: methods::MUTATE,
                    args,
                    snapshot: false,
                })
            }
            "set_composer_text" => {
                if args["chatId"].as_str() != Some(state.selected_chat.as_deref().unwrap_or("new"))
                {
                    return Err("Selection changed; draft was not modified".into());
                }
                let text = args["text"].as_str().unwrap().to_string();
                self.composer
                    .read(cx)
                    .input
                    .clone()
                    .update(cx, |input, cx| input.set_text(text, cx));
                Ok(ok())
            }
            "show_diff" => {
                if state.selected_chat.is_none() {
                    return Err("Select a session first".into());
                }
                if !self.right_pane_open(cx) {
                    self.toggle_right_pane(cx);
                }
                self.add_diff_surface(cx);
                Ok(ok())
            }
            "open_file" => {
                if state.selected_chat.is_none() {
                    return Err("Select a session first".into());
                }
                self.add_file_surface(args["path"].as_str().unwrap().into(), window, cx);
                if !self.right_pane_open(cx) {
                    self.toggle_right_pane(cx);
                }
                Ok(ok())
            }
            "open_browser" => {
                if state.selected_chat.is_none() {
                    return Err("Select a session first".into());
                }
                let url = args["url"].as_str().unwrap();
                let parsed = url::Url::parse(url).map_err(|_| "Invalid browser URL")?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err("Browser URL must use http or https".into());
                }
                self.add_browser_surface(Some(url.into()), window, cx);
                if !self.right_pane_open(cx) {
                    self.toggle_right_pane(cx);
                }
                Ok(ok())
            }
            "open_settings" => {
                let section = match args["section"].as_str().unwrap_or("devices") {
                    "devices" => SettingsSection::Devices,
                    "accounts" => SettingsSection::Agents,
                    "harnesses" => SettingsSection::Harnesses,
                    "appearance" => SettingsSection::Appearance,
                    "notifications" => SettingsSection::Notifications,
                    "shortcuts" => SettingsSection::Shortcuts,
                    "appshots" => SettingsSection::Appshots,
                    "files" => SettingsSection::Files,
                    "connections" => SettingsSection::Connections,
                    "archived" => SettingsSection::Archived,
                    _ => return Err("Unknown settings section".into()),
                };
                self.open_settings(section, cx);
                Ok(ok())
            }
            "list_native_actions" => {
                let available = window.available_actions(cx);
                let query = args["query"].as_str().unwrap_or("").to_lowercase();
                let mut generator = Default::default();
                let actions: Vec<_> = cx
                    .action_schemas(&mut generator)
                    .into_iter()
                    .filter(|(name, _)| {
                        name.to_lowercase().contains(&query)
                            && available.iter().any(|action| action.name() == *name)
                    })
                    .map(|(name, schema)| json!({"name":name,"parameters":schema}))
                    .collect();
                Ok(Work::Done(
                    json!({"actions":actions,"definitions":generator.definitions()}),
                ))
            }
            "dispatch_native_action" => {
                let name = args["name"].as_str().unwrap();
                if !window
                    .available_actions(cx)
                    .iter()
                    .any(|a| a.name() == name)
                {
                    return Err("Action is not available in the focused view".into());
                }
                let action = cx
                    .build_action(name, args.get("data").cloned())
                    .map_err(|e| e.to_string())?;
                window.dispatch_action(action, cx);
                Ok(Work::Done(json!({"dispatched":true})))
            }
            "set_theme" => {
                let mode = match args["mode"].as_str().unwrap() {
                    "dark" => crate::appearance::AppearanceMode::Dark,
                    "light" => crate::appearance::AppearanceMode::Light,
                    "system" => crate::appearance::AppearanceMode::System,
                    _ => return Err("Unknown theme".into()),
                };
                crate::appearance::set_mode(mode, cx);
                Ok(ok())
            }
            "end_voice" => {
                self.stop_voice(cx);
                Ok(ok())
            }
            _ => Err("Action is not implemented".into()),
        }
    }
    pub(super) fn render_voice_bar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.voice.live || self.voice.call.is_some() || self.voice.connecting;
        div()
            .id("voice-bar")
            .flex_none()
            .mx(px(8.))
            .my(px(4.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(crate::theme::wash(if busy { 0.08 } else { 0.03 }))
            .flex()
            .items_center()
            .justify_between()
            .text_size(crate::typography::ui_rems(12.))
            .text_color(theme.text)
            .child(
                div()
                    .id("voice-controls")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.voice.panel = !this.voice.panel;
                        cx.notify();
                    }))
                    .child(if self.voice.pending.is_some() {
                        "Voice · confirm action"
                    } else if self.voice.live {
                        if self.voice.muted {
                            "Voice · muted"
                        } else {
                            "Voice · live"
                        }
                    } else if busy {
                        "Voice · connecting"
                    } else {
                        "Voice"
                    }),
            )
            .when(self.voice.live, |el| {
                el.child(
                    div()
                        .id("voice-mute")
                        .cursor_pointer()
                        .child(if self.voice.muted { "Unmute" } else { "Mute" })
                        .on_click(cx.listener(|this, _, _, cx| this.mute_voice(cx))),
                )
            })
            .child(
                div()
                    .id("voice-toggle")
                    .cursor_pointer()
                    .child(if busy { "End" } else { "Start" })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_voice(window, cx))),
            )
            .into_any_element()
    }
    pub(super) fn render_voice_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.voice.panel {
            return None;
        }
        if self
            .voice
            .pending
            .as_ref()
            .is_some_and(|p| p.reply.is_closed())
        {
            self.voice.pending = None;
        }
        let theme = Theme::of(cx).for_popup();
        let busy = self.voice.live || self.voice.call.is_some() || self.voice.connecting;
        let card = popover::popover_card(&theme).id("voice-panel-scroll").overflow_y_scroll().w(px(500.)).max_h((viewport.height-px(80.)).max(px(200.)))
            .p(px(20.)).flex().flex_col().gap(px(12.)).text_color(theme.text)
            .child(div().flex().justify_between().child("Noches voice · GPT-Live")
                .child(popover::btn_ghost(&theme,"Hide","voice-hide").id("voice-hide").on_click(cx.listener(|this,_,_,cx|{this.dismiss_voice_panel(cx);}))))
            .child(div().text_size(crate::typography::ui_rems(12.)).text_color(theme.text_muted)
                .child("Talk to your agents and control Noches. Audio and requested app context go to OpenAI while connected. Use headphones to avoid speaker echo. API billing continues while muted."))
            .child(div().text_size(crate::typography::ui_rems(12.)).child(self.voice.message.clone()))
            .child(div().flex().gap(px(8.))
                .child(popover::btn_primary(&theme,if busy {"End voice"} else {"Start voice"}).id("voice-start-stop")
                    .on_click(cx.listener(|this,_,window,cx|this.toggle_voice(window,cx))))
                .when(self.voice.live,|el|el.child(popover::btn_ghost(&theme,if self.voice.muted {"Unmute"} else {"Mute"},"voice-panel-mute")
                    .id("voice-panel-mute").on_click(cx.listener(|this,_,_,cx|this.mute_voice(cx))))))
            .when(!busy,|el|el.child(div().flex().gap(px(8.))
                .child(popover::btn_ghost(&theme,"Paste API key","voice-paste-key").id("voice-paste-key").on_click(cx.listener(|this,_,_,cx|this.paste_voice_key(cx))))
                .child(popover::btn_ghost(&theme,"Remove saved key","voice-forget-key").id("voice-forget-key").on_click(cx.listener(|this,_,_,cx|this.forget_voice_key(cx))))))
            .when(!self.voice.user_caption.is_empty(),|el|el.child(div().text_size(crate::typography::ui_rems(12.)).child(format!("You: {}",self.voice.user_caption))))
            .when(!self.voice.assistant_caption.is_empty(),|el|el.child(div().text_size(crate::typography::ui_rems(12.)).child(format!("Noches: {}",self.voice.assistant_caption))))
            .when(!self.voice.last_action.is_empty(),|el|el.child(div().text_size(crate::typography::ui_rems(11.)).text_color(theme.text_muted).child(format!("Last action: {}",self.voice.last_action))));
        let card = card.when_some(
            self.voice
                .usage
                .as_ref()
                .and_then(|v| v["seconds"].as_f64()),
            |el, seconds| {
                el.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.))
                        .text_color(theme.text_muted)
                        .child(format!(
                            "Call duration: {}m {:02}s",
                            seconds as u64 / 60,
                            seconds as u64 % 60
                        )),
                )
            },
        );
        let card = if let Some(pending) = &self.voice.pending {
            let preview = serde_json::to_string_pretty(&pending.args).unwrap_or_default();
            card.child(div().border_t_1().border_color(theme.border).pt(px(12.)).flex().flex_col().gap(px(8.))
                .child(format!("Allow {}?",pending.name.replace('_', " ")))
                .child(div().id("voice-action-details").max_h(px(180.)).overflow_y_scroll().text_size(crate::typography::ui_rems(11.)).child(preview))
                .child(div().flex().gap(px(8.))
                    .child(popover::btn_primary(&theme,"Allow").id("voice-allow").on_click(cx.listener(|this,_,window,cx|{
                        if let Some(pending)=this.voice.pending.take() {
                            if pending.name == "dispatch_native_action" && (pending.selection != compact_selection(&this.voice_context(cx)) || pending.focus != window.focused(cx)) {
                                let _ = pending.reply.send(json!({"error":"Focused view changed; action was not dispatched"}));
                            } else { this.execute_voice_action(pending.name,pending.args,pending.reply,window,cx); }
                        } cx.notify();
                    })))
                    .child(popover::btn_ghost(&theme,"Cancel","voice-deny").id("voice-deny").on_click(cx.listener(|this,_,_,cx|{
                        if let Some(pending)=this.voice.pending.take() {let _=pending.reply.send(json!({"error":"User declined this action"}));} cx.notify();
                    })))))
        } else {
            card
        };
        Some(popover::modal(
            "voice-panel",
            viewport,
            card.into_any_element(),
        ))
    }
}
fn run_request(
    prompt: &str,
    config: &zeron_proto::ChatConfig,
    cwd: String,
) -> zeron_proto::RunRequest {
    zeron_proto::RunRequest {
        prompt: prompt.into(),
        harness: Some(config.harness),
        model: config.model.clone(),
        reasoning: config.reasoning,
        model_options: config.model_options.clone(),
        cwd,
        sandbox: config.sandbox,
        auto_approve: false,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
    }
}
fn compact_selection(context: &Value) -> Value {
    json!({"chatId":context["selectedChat"]["id"],"spaceId":context["selectedSpace"]["id"],"deviceId":context["deviceId"],"route":context["route"],"surface":context["activeSurface"]})
}
fn retain_tail(text: &mut String, max_chars: usize) {
    if let Some((index, _)) = text.char_indices().rev().nth(max_chars.saturating_sub(1)) {
        text.drain(..index);
    }
}
fn bounded_result(value: Value) -> Value {
    let text = value.to_string();
    if text.len() <= 32_000 {
        value
    } else {
        let mut end = 24_000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        json!({"truncated":true,"preview":&text[..end],"note":"Result exceeded the voice context limit. Narrow the query or inspect the UI."})
    }
}

fn snapshot_tail(mut value: Value) -> Value {
    if let Some(reset) = value.get_mut("reset").and_then(Value::as_array_mut) {
        let total = reset.len();
        if total > 20 {
            reset.drain(..total - 20);
        }
        value["totalMessages"] = json!(total);
        value["tailLimit"] = json!(20);
        value.as_object_mut().unwrap().remove("replayBaseline");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    fn initialize(dir: &std::path::Path, cx: &mut App) {
        settings::init(UiSettings::default(), dir, cx);
        crate::history::init(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            cx,
        );
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::app_menus::init(cx);
    }
    fn shell(dir: &std::path::Path, cx: &mut Context<Shell>) -> Shell {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.spaces = vec![serde_json::from_value(json!({"id":"project","deviceId":"remote","path":"/repo","gitDetected":true,"createdAt":Utc::now()})).unwrap()];
            state.chats = vec![serde_json::from_value(json!({"id":"chat","deviceId":"remote","spaceId":"project","cwd":"/repo/tree","archived":false,"createdAt":Utc::now(),"config":{
                "harness":"codex","model":"example","reasoning":null,"modelOptions":{},"sandbox":"workspace-write"
            }})).unwrap()];
            state
        });
        Shell::new(
            state,
            EngineBootConfig {
                remote: None,
                data_dir: dir.into(),
                ipc_port: 0,
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zeron_proto::HarnessId::Mock,
            },
            cx,
        )
    }
    #[gpui::test]
    fn voice_escape_declines_pending_action_and_keeps_call_state(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| initialize(dir.path(), cx));
        let window = cx.add_window(|_, cx| shell(dir.path(), cx));
        window
            .update(cx, |shell, _, cx| {
                let (reply, mut result) = tokio::sync::oneshot::channel();
                shell.voice.panel = true;
                shell.voice.live = true;
                shell.voice.pending = Some(Pending {
                    name: "delete_session".into(),
                    args: json!({"chatId":"chat"}),
                    selection: Value::Null,
                    focus: None,
                    reply,
                });
                assert!(shell.capture_escape_surface(cx));
                assert!(!shell.voice.panel);
                assert!(shell.voice.live);
                assert!(shell.voice.pending.is_none());
                assert_eq!(
                    result.try_recv().unwrap()["error"],
                    "User declined this action"
                );
                assert!(!shell.dismiss_voice_panel(cx));
            })
            .unwrap();
    }
    #[gpui::test]
    fn voice_file_operations_follow_the_session_host_and_reject_mismatches(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| initialize(dir.path(), cx));
        let window = cx.add_window(|_, cx| shell(dir.path(), cx));
        window
            .update(cx, |shell, window, cx| {
                let work = shell
                    .prepare_voice_action(
                        "read_file",
                        json!({"chatId":"chat","path":"src/lib.rs"}),
                        window,
                        cx,
                    )
                    .unwrap();
                let Work::Rpc { method, args, .. } = work else {
                    panic!("expected RPC")
                };
                assert_eq!(method, methods::READ_WORKSPACE_FILE);
                assert_eq!(args["targetDeviceId"], "remote");
                assert!(
                    shell
                        .prepare_voice_action(
                            "read_file",
                            json!({"chatId":"chat","path":"a","targetDeviceId":"local"}),
                            window,
                            cx
                        )
                        .is_err()
                );
                assert!(
                    shell
                        .prepare_voice_action(
                            "read_file",
                            json!({"chatId":"missing","path":"a"}),
                            window,
                            cx
                        )
                        .is_err()
                );
            })
            .unwrap();
    }
    #[gpui::test]
    fn voice_send_uses_saved_config_and_durable_commands_without_auto_approval(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| initialize(dir.path(), cx));
        let window = cx.add_window(|_, cx| shell(dir.path(), cx));
        window
            .update(cx, |shell, window, cx| {
                let Work::Rpc { method, args, .. } = shell
                    .prepare_voice_action(
                        "send_message",
                        json!({"chatId":"chat","text":"Please add tests"}),
                        window,
                        cx,
                    )
                    .unwrap()
                else {
                    panic!("expected RPC")
                };
                assert_eq!(method, methods::QUEUE_COMMAND);
                assert_eq!(args["command"]["request"]["prompt"], "Please add tests");
                assert_eq!(args["command"]["request"]["harness"], "codex");
                assert_eq!(args["command"]["request"]["cwd"], "/repo/tree");
                assert_eq!(args["command"]["request"]["sandbox"], "workspace-write");
                assert_eq!(args["command"]["request"]["autoApprove"], false);
                assert!(args["command"]["messageId"].as_str().is_some());
            })
            .unwrap();
    }
    #[gpui::test]
    fn voice_cannot_inject_mutations_or_escalate_session_permissions(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| initialize(dir.path(), cx));
        let window = cx.add_window(|_, cx| shell(dir.path(), cx));
        window.update(cx,|shell,window,cx| {
            assert!(shell.prepare_voice_action("rename_session",json!({"chatId":"chat","title":"Renamed","op":"deleteChat"}),window,cx).is_err());
            assert!(shell.prepare_voice_action("set_session_config",json!({"chatId":"chat","config":{
                "harness":"codex","model":"example","reasoning":null,"modelOptions":{},"sandbox":"danger-full-access"
            }}),window,cx).is_err());
            assert!(shell.prepare_voice_action("SignOut",json!({}),window,cx).is_err());
        }).unwrap();
    }
    #[test]
    fn voice_keeps_recent_transcript_and_valid_unicode_when_bounded() {
        let entries: Vec<_> = (0..30).map(|n| json!({"id":n})).collect();
        let result = snapshot_tail(json!({"reset":entries,"replayBaseline":{"large":"metadata"}}));
        assert_eq!(result["reset"].as_array().unwrap().len(), 20);
        assert_eq!(result["reset"][0]["id"], 10);
        assert_eq!(result["totalMessages"], 30);
        assert!(result.get("replayBaseline").is_none());
        let result = bounded_result(json!({"text":"語".repeat(40_000)}));
        assert_eq!(result["truncated"], true);
        let mut text = "語".repeat(3000);
        retain_tail(&mut text, 1200);
        assert!(text.chars().count() <= 1200);
    }
}

#[cfg(feature = "voice-fixture")]
impl Shell {
    /// Layout evidence only. Never acquires a microphone or connects to OpenAI.
    pub fn fixture_voice_panel(&mut self, live: bool, cx: &mut Context<Self>) {
        self.voice.panel = true;
        self.voice.live = live;
        self.voice.message = if live {
            "Listening"
        } else {
            "Add an OpenAI API key to start a conversation."
        }
        .into();
        self.voice.user_caption = if live {
            "What's running right now? Show the session fixing CI."
        } else {
            ""
        }
        .into();
        self.voice.assistant_caption = if live {
            "Two agents are working. I've opened the CI session. It is checking the test results."
        } else {
            ""
        }
        .into();
        self.voice.last_action = if live { "focus session" } else { "" }.into();
        cx.notify();
    }
}

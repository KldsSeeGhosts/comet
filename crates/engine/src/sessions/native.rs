//! Same-conversation handoff between structured chat and the provider's TUI.
//! The durable checkpoint also blocks chat sends after a host restart until
//! native history has been reconciled. No fresh-session fallback is allowed.
use super::native_history::{self, Checkpoint, error};
use super::*;
use crate::terminals::Terminals;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs, io::Write, path::PathBuf};
use zeron_proto::{SessionSurface, SessionSurfaceState, TerminalSession};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Binding {
    pub surface: SessionSurface,
    pub harness: HarnessId,
    pub session_id: String,
    pub request: RunRequest,
    checkpoint: Checkpoint,
    terminal: Option<TerminalSession>,
    activity_file: PathBuf,
    /// A submitted key must be acknowledged by a later provider lifecycle
    /// record before returning to chat can stop the CLI.
    input_offset: Option<u64>,
}

impl Binding {
    pub(super) fn checkpoint_path(&self) -> PathBuf {
        self.checkpoint.path.clone()
    }
}

impl SessionsEngine {
    pub(super) fn native_path(&self, chat: &str) -> PathBuf {
        self.inner
            .journal
            .directory()
            .join(format!("{:x}.native.json", Sha256::digest(chat.as_bytes())))
    }

    pub(super) fn native_binding(&self, chat: &str) -> Result<Option<Binding>, EngineError> {
        match fs::read(self.native_path(chat)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
                error("Native session recovery state is unreadable; chat remains locked")
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save_binding(&self, chat: &str, binding: &Binding) -> Result<(), EngineError> {
        let mut file = tempfile::NamedTempFile::new_in(self.inner.journal.directory())?;
        serde_json::to_writer(&mut file, binding).map_err(|e| error(e.to_string()))?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.native_path(chat)).map_err(|e| e.error)?;
        #[cfg(unix)]
        fs::File::open(self.inner.journal.directory())?.sync_all()?;
        Ok(())
    }

    pub fn native_owns_chat(&self, chat: &str) -> bool {
        self.native_binding(chat)
            .map(|b| b.is_some_and(|b| b.surface == SessionSurface::Cli))
            .unwrap_or(true)
    }

    pub(super) fn native_gate(&self, chat: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.inner.native_gates)
            .entry(chat.into())
            .or_default()
            .clone()
    }

    pub(super) fn require_chat_owner(&self, chat: &str) -> Result<(), EngineError> {
        if self.native_owns_chat(chat) {
            return Err(error(
                "This session is open in CLI. Switch back to Chat before sending.",
            ));
        }
        Ok(())
    }

    fn require_native_host(&self, chat: &str) -> Result<(), EngineError> {
        let row = self
            .inner
            .workspace()
            .ok_or_else(|| error("Workspace is unavailable"))?
            .chat(chat)?
            .ok_or_else(|| error("Session not found"))?;
        if row.device_id != self.inner.device_id {
            return Err(error("Switch this session on its owning device"));
        }
        Ok(())
    }

    fn native_target(&self, chat: &str) -> Result<(HarnessId, String, RunRequest), EngineError> {
        if cfg!(windows) {
            return Err(error(
                "Native CLI switching is not yet supported on Windows",
            ));
        }
        let row = self
            .inner
            .workspace()
            .ok_or_else(|| error("Workspace is unavailable"))?
            .chat(chat)?
            .ok_or_else(|| error("Session not found"))?;
        if row.device_id != self.inner.device_id {
            return Err(error("Switch this session on its owning device"));
        }
        let config = row
            .config
            .ok_or_else(|| error("Send a message before opening this session in CLI"))?;
        if !matches!(config.harness, HarnessId::ClaudeCode | HarnessId::Codex) {
            return Err(error(
                "Native CLI switching is currently available for Claude Code and Codex",
            ));
        }
        let cwd = row
            .cwd
            .ok_or_else(|| error("Session working directory is unavailable"))?;
        let cwd = expand_home(&cwd);
        let id = self
            .inner
            .resume_for(chat, &cwd)
            .ok_or_else(|| error("Send a message before opening this session in CLI"))?;
        let recorded = self
            .inner
            .journal
            .replay(chat, 0)?
            .into_iter()
            .rev()
            .find_map(|(_, event)| match event {
                AgentEvent::SessionStarted {
                    harness,
                    session_id,
                    ..
                } if session_id == id => Some(harness),
                _ => None,
            });
        if recorded != Some(config.harness) {
            return Err(error(
                "The saved provider session does not match this chat's harness",
            ));
        }
        let saved = self
            .native_binding(chat)?
            .filter(|b| b.session_id == id && b.harness == config.harness)
            .map(|b| b.request);
        let request = self
            .last_request(chat)
            .or(saved)
            .filter(|request| request.cwd == cwd)
            .unwrap_or(RunRequest {
                prompt: String::new(),
                harness: Some(config.harness),
                model: config.model,
                reasoning: config.reasoning,
                model_options: config.model_options,
                cwd,
                sandbox: config.sandbox,
                auto_approve: false,
                resume: Some(id.clone()),
                attachments: vec![],
                worktree: None,
            });
        Ok((config.harness, id, request))
    }

    fn native_idle(&self, binding: &Binding, terminals: &Terminals) -> Result<bool, EngineError> {
        let Some(terminal) = &binding.terminal else {
            return Ok(true);
        };
        if terminals.exited(&terminal.id) {
            return Ok(true);
        }
        match binding.harness {
            HarnessId::ClaudeCode => {
                let events = fs::read(&binding.activity_file)?;
                let start = binding.input_offset.unwrap_or(0) as usize;
                let Some(tail) = events.get(start..) else {
                    return Ok(false);
                };
                let text =
                    std::str::from_utf8(tail).map_err(|_| error("CLI activity is unreadable"))?;
                let events: Vec<_> = text.lines().collect();
                Ok(events
                    .last()
                    .is_some_and(|event| matches!(*event, "idle" | "start"))
                    && (binding.input_offset.is_none() || events.contains(&"submit")))
            }
            HarnessId::Codex => {
                let rows = binding.checkpoint.tail()?;
                let rows: Vec<_> = rows
                    .into_iter()
                    .filter(|(offset, _)| {
                        binding
                            .input_offset
                            .is_none_or(|input| *offset as u64 >= input)
                    })
                    .collect();
                // An untouched CLI is safe to close. After input, only a
                // persisted provider completion (or process exit) releases it.
                let accepted = rows.iter().any(|(_, row)| {
                    row["type"] == "event_msg" && row["payload"]["type"] == "user_message"
                });
                Ok(
                    native_history::codex_idle(&rows).unwrap_or(binding.input_offset.is_none())
                        && (binding.input_offset.is_none() || accepted),
                )
            }
            _ => Ok(false),
        }
    }

    pub fn session_surface(
        &self,
        chat: &str,
        terminals: &Terminals,
    ) -> Result<SessionSurfaceState, EngineError> {
        self.require_native_host(chat)?;
        if let Some(binding) = self.native_binding(chat)?
            && binding.surface == SessionSurface::Cli
        {
            let idle = self.native_idle(&binding, terminals);
            let reason = match &idle {
                Ok(true) => None,
                Ok(false) => {
                    Some("Finish the CLI turn or exit the CLI before returning to Chat".into())
                }
                Err(e) => Some(e.to_string()),
            };
            self.set_status(
                chat,
                if idle.as_ref().is_ok_and(|idle| *idle) {
                    SessionStatus::Idle
                } else {
                    SessionStatus::Working
                },
                false,
            );
            return Ok(SessionSurfaceState {
                surface: SessionSurface::Cli,
                terminal: binding.terminal.filter(|t| !terminals.exited(&t.id)),
                can_switch: idle.unwrap_or(false),
                reason,
            });
        }
        let reason = if self.turn_in_flight(chat) {
            Some("Finish or stop the current turn to switch views".into())
        } else {
            self.native_target(chat)
                .and_then(|(harness, id, request)| {
                    self.inner
                        .registry
                        .resolve(harness)?
                        .native_cli(&request, &id)?;
                    Ok(())
                })
                .err()
                .map(|e| e.to_string())
        };
        Ok(SessionSurfaceState {
            surface: SessionSurface::Chat,
            terminal: None,
            can_switch: reason.is_none(),
            reason,
        })
    }

    pub async fn switch_session_surface(
        &self,
        chat: &str,
        target: SessionSurface,
        terminals: &Terminals,
        cols: u16,
        rows: u16,
    ) -> Result<SessionSurfaceState, EngineError> {
        self.require_native_host(chat)?;
        let gate = self.native_gate(chat);
        let _gate = gate.lock().await;
        let binding = self.native_binding(chat)?;
        let current = binding.as_ref().map(|b| b.surface).unwrap_or_default();
        if current == target {
            return self.session_surface(chat, terminals);
        }
        if target == SessionSurface::Cli {
            if self.turn_in_flight(chat) {
                return Err(error("Finish or stop the current turn to switch views"));
            }
            let (harness_id, session_id, mut request) = self.native_target(chat)?;
            let harness = self.inner.registry.resolve(harness_id)?;
            let mut command = harness.native_cli(&request, &session_id)?;
            if let Some(root) = self.inner.browser_root.get() {
                let socket = zeron_browser::socket_path(root);
                if socket.exists() {
                    command.with_browser(
                        harness_id,
                        &zeron_browser::Connection {
                            executable: std::env::current_exe()?,
                            socket,
                            session: chat.into(),
                        },
                    );
                }
            }
            let checkpoint = if let Some(previous) = &binding {
                if previous.session_id != session_id || previous.harness != harness_id {
                    return Err(error(
                        "The provider session changed since its last CLI handoff",
                    ));
                }
                native_history::checkpoint_at(
                    previous.checkpoint.path.clone(),
                    harness_id,
                    &session_id,
                    &request.cwd,
                )?
            } else {
                native_history::checkpoint(harness_id, &session_id, &request.cwd)?
            };
            let exited = harness.native_runtime_exited(&session_id);
            if lock(&self.inner.runs).contains_key(chat) && exited.is_none() {
                return Err(error(
                    "The active chat runtime cannot confirm shutdown; stop it before switching",
                ));
            }
            let activity_file = self
                .inner
                .journal
                .directory()
                .join(format!("{}.native-activity", new_id()));
            // Claude reports lifecycle through per-launch hooks. This does not
            // modify the user's settings or inspect terminal screen contents.
            if harness_id == HarnessId::ClaudeCode {
                fs::write(&activity_file, "")?;
                let path = shell_quote(&activity_file.to_string_lossy());
                let hook = |state: &str| serde_json::json!([{"matcher":"", "hooks":[{"type":"command","command":format!("printf '%s\\n' {state} >> {path}")}]}]);
                let mut settings = serde_json::json!({"hooks": {
                    "SessionStart":hook("start"), "UserPromptSubmit":hook("submit"),
                    "PreToolUse":hook("busy"), "PermissionRequest":hook("busy"), "Stop":hook("idle")
                }});
                for (key, setting) in [
                    ("fastMode", "fastMode"),
                    ("thinking", "alwaysThinkingEnabled"),
                ] {
                    if binding_option_on(&request, key) {
                        settings[setting] = true.into();
                    }
                }
                if request.reasoning == Some(zeron_proto::ReasoningLevel::Ultracode) {
                    settings["ultracode"] = true.into();
                }
                command
                    .args
                    .extend(["--settings".into(), settings.to_string()]);
            }
            request.prompt.clear();
            request.attachments.clear();
            request.worktree = None;
            request.resume = Some(session_id.clone());
            let mut binding = Binding {
                surface: SessionSurface::Cli,
                harness: harness_id,
                session_id: session_id.clone(),
                request,
                checkpoint,
                terminal: None,
                activity_file,
                input_offset: None,
            };
            self.save_binding(chat, &binding)?;
            self.interrupt(chat).await?;
            if let Some(exited) = exited {
                tokio::time::timeout(std::time::Duration::from_secs(12), exited.cancelled()).await
                    .map_err(|_| error("The chat runtime has not finished closing; return to Chat and try again"))?;
            }
            if lock(&self.inner.runs).contains_key(chat) {
                return Err(error("The chat runtime still owns this session"));
            }
            // Include final provider flushes in the pre-CLI checkpoint.
            binding.checkpoint = native_history::checkpoint_at(
                binding.checkpoint.path.clone(),
                harness_id,
                &session_id,
                &binding.request.cwd,
            )?;
            self.save_binding(chat, &binding)?;
            match terminals.open_program(&binding.request.cwd, cols, rows, &command) {
                Ok(terminal) => {
                    binding.terminal = Some(terminal.clone());
                    lock(&self.inner.native_terminal_owners)
                        .insert(terminal.id.clone(), chat.into());
                    if let Err(e) = self.save_binding(chat, &binding) {
                        let _ = terminals.terminate_and_wait(&terminal.id).await;
                        return Err(e);
                    }
                }
                Err(e) => {
                    binding.surface = SessionSurface::Chat;
                    self.save_binding(chat, &binding)?;
                    return Err(e);
                }
            }
            self.set_status(chat, SessionStatus::Idle, false);
        } else {
            let mut binding =
                binding.ok_or_else(|| error("Native session binding is unavailable"))?;
            if !self.native_idle(&binding, terminals)? {
                return Err(error(
                    "Finish the CLI turn or exit the CLI before returning to Chat",
                ));
            }
            if let Some(exited) = self
                .inner
                .registry
                .resolve(binding.harness)?
                .native_runtime_exited(&binding.session_id)
            {
                tokio::time::timeout(std::time::Duration::from_secs(12), exited.cancelled())
                    .await
                    .map_err(|_| error("The previous chat runtime is still closing"))?;
            }
            if let Some(terminal) = &binding.terminal {
                terminals.terminate_and_wait(&terminal.id).await?;
            }
            // A failed pre-launch handoff may still contain final structured
            // runtime flushes. Only an actually launched CLI owns new history.
            let rows = if binding.terminal.is_some() {
                binding.checkpoint.tail()?
            } else {
                Vec::new()
            };
            let imported = native_history::messages(
                binding.harness,
                &binding.session_id,
                &self.inner.device_id,
                &rows,
            )?;
            let handle = self.doc_handle(chat)?;
            let mut known: HashSet<_> = handle
                .doc()
                .read_entries()?
                .into_iter()
                .map(|e| e.id)
                .collect();
            let mut preview = None;
            for message in imported {
                if known.insert(message.id.clone()) {
                    preview = message
                        .parts
                        .iter()
                        .rev()
                        .find_map(|p| match p {
                            zeron_doc::MessagePart::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        })
                        .or(preview);
                    handle.doc().push_message(&message)?;
                }
            }
            self.inner
                .doc_host()
                .ok_or_else(|| error("Chat storage is unavailable"))?
                .persist_native_import(&handle)?;
            if let Some(preview) = preview {
                self.inner.note_message(chat, &preview);
            }
            self.inner
                .remember_harness_session(chat, &binding.session_id, &binding.request.cwd);
            binding.surface = SessionSurface::Chat;
            let terminal = binding.terminal.take();
            self.save_binding(chat, &binding)?;
            if let Some(terminal) = terminal {
                let _ = terminals.close(&terminal.id);
                lock(&self.inner.native_terminal_owners).remove(&terminal.id);
            }
            let _ = fs::remove_file(&binding.activity_file);
            self.set_status(chat, SessionStatus::Idle, false);
        }
        self.session_surface(chat, terminals)
    }

    pub fn close_session_terminal(
        &self,
        terminals: &Terminals,
        id: &str,
    ) -> Result<(), EngineError> {
        if lock(&self.inner.native_terminal_owners).contains_key(id) {
            return Err(error(
                "Use the Chat toggle or exit the CLI to close this session",
            ));
        }
        terminals.close(id)
    }

    /// All native PTY input goes through the same gate as switching. Capturing
    /// a submit before writing closes the completion-record polling race.
    pub async fn write_session_terminal(
        &self,
        terminals: &Terminals,
        id: &str,
        data: &str,
    ) -> Result<(), EngineError> {
        use base64::Engine as _;
        let owner = lock(&self.inner.native_terminal_owners).get(id).cloned();
        let Some(chat) = owner else {
            return terminals.write(id, data);
        };
        let gate = self.native_gate(&chat);
        let _gate = gate.lock().await;
        let mut binding = self
            .native_binding(&chat)?
            .ok_or_else(|| error("Native session is no longer attached"))?;
        if binding.surface != SessionSurface::Cli
            || binding.terminal.as_ref().map(|t| t.id.as_str()) != Some(id)
        {
            return Err(error("Native session is no longer accepting input"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap_or_else(|_| data.as_bytes().to_vec());
        if bytes.iter().any(|b| matches!(b, b'\r' | b'\n')) {
            let activity_path = if binding.harness == HarnessId::ClaudeCode {
                &binding.activity_file
            } else {
                &binding.checkpoint.path
            };
            binding.input_offset = Some(fs::metadata(activity_path)?.len());
            self.save_binding(&chat, &binding)?;
        }
        terminals.write_bytes(id, &bytes)
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn binding_option_on(request: &RunRequest, key: &str) -> bool {
    request
        .model_options
        .get(key)
        .is_some_and(|v| v == true || v == "on" || v == "true")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{EngineCore, HarnessRegistry};
    use serde_json::json;
    use std::{os::unix::fs::PermissionsExt, path::Path};
    use zeron_harness::CodexHarness;
    use zeron_proto::{ChatConfig, SandboxLevel};

    const CHAT: &str = "native-chat";
    const ID: &str = "11111111-1111-4111-8111-111111111111";

    fn fixture(root: &Path) -> (EngineCore, Binding) {
        let executable = root.join("fake cli");
        fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\" > native-argv.tmp\nmv native-argv.tmp native-argv\nexec sleep 60\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(Arc::new(CodexHarness::new().with_executable(executable)));
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let core = EngineCore::assemble(&data, registry, HarnessId::Codex, None).unwrap();
        let request = RunRequest {
            prompt: String::new(),
            harness: Some(HarnessId::Codex),
            model: Some("test $(touch INJECTION) with spaces".into()),
            reasoning: None,
            model_options: Default::default(),
            cwd: root.to_str().unwrap().into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            resume: Some(ID.into()),
            attachments: vec![],
            worktree: None,
        };
        core.workspace
            .create_chat(
                CHAT,
                None,
                Some(&core.device_id),
                Some(ChatConfig {
                    harness: HarnessId::Codex,
                    model: request.model.clone(),
                    reasoning: None,
                    model_options: Default::default(),
                    sandbox: request.sandbox,
                }),
                Some(request.cwd.clone()),
            )
            .unwrap();
        core.sessions
            .inner
            .journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Codex,
                    model: "test".into(),
                    tools: vec![],
                    cwd: request.cwd.clone(),
                    session_id: ID.into(),
                    assistant_message_id: "old-assistant".into(),
                },
            )
            .unwrap();
        core.sessions
            .inner
            .remember_harness_session(CHAT, ID, &request.cwd);
        let history = root.join("history.jsonl");
        fs::write(
            &history,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":ID,"cwd":root}})
            ),
        )
        .unwrap();
        let checkpoint =
            native_history::checkpoint_at(history, HarnessId::Codex, ID, &request.cwd).unwrap();
        let binding = Binding {
            surface: SessionSurface::Chat,
            harness: HarnessId::Codex,
            session_id: ID.into(),
            request,
            checkpoint,
            terminal: None,
            activity_file: root.join("activity"),
            input_offset: None,
        };
        core.sessions.save_binding(CHAT, &binding).unwrap();
        (core, binding)
    }

    fn append(binding: &Binding, row: serde_json::Value) {
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(&binding.checkpoint.path)
                .unwrap(),
            "{row}"
        )
        .unwrap();
    }

    #[tokio::test]
    async fn claude_native_activity_requires_submission_acknowledgement_and_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _) = fixture(dir.path());
        core.sessions
            .switch_session_surface(CHAT, SessionSurface::Cli, &core.terminals, 80, 24)
            .await
            .unwrap();
        let mut binding = core.sessions.native_binding(CHAT).unwrap().unwrap();
        binding.harness = HarnessId::ClaudeCode;
        fs::write(&binding.activity_file, "start\n").unwrap();
        assert!(
            core.sessions
                .native_idle(&binding, &core.terminals)
                .unwrap()
        );
        binding.input_offset = Some(6);
        assert!(
            !core
                .sessions
                .native_idle(&binding, &core.terminals)
                .unwrap()
        );
        fs::write(&binding.activity_file, "start\nsubmit\nbusy\n").unwrap();
        assert!(
            !core
                .sessions
                .native_idle(&binding, &core.terminals)
                .unwrap()
        );
        fs::write(&binding.activity_file, "start\nsubmit\nbusy\nidle\n").unwrap();
        assert!(
            core.sessions
                .native_idle(&binding, &core.terminals)
                .unwrap()
        );
        fs::write(&binding.activity_file, "start\nidle\n").unwrap();
        assert!(
            !core
                .sessions
                .native_idle(&binding, &core.terminals)
                .unwrap()
        );
        core.sessions
            .switch_session_surface(CHAT, SessionSurface::Chat, &core.terminals, 80, 24)
            .await
            .unwrap();
        core.shutdown().await;
    }

    #[tokio::test]
    async fn native_handoff_serializes_toggles_blocks_sends_and_imports_once() {
        let dir = tempfile::tempdir().unwrap();
        let (core, binding) = fixture(dir.path());
        let (a, b) = tokio::join!(
            core.sessions.switch_session_surface(
                CHAT,
                SessionSurface::Cli,
                &core.terminals,
                80,
                24
            ),
            core.sessions.switch_session_surface(
                CHAT,
                SessionSurface::Cli,
                &core.terminals,
                80,
                24
            ),
        );
        let a = a.unwrap();
        let b = b.unwrap();
        let terminal = a.terminal.unwrap();
        assert_eq!(terminal.id, b.terminal.unwrap().id);
        assert!(core.sessions.turn_in_flight(CHAT));
        assert!(
            core.sessions
                .dispatch(CHAT, HarnessId::Codex, binding.request.clone(), None)
                .await
                .is_err()
        );
        assert!(
            core.sessions
                .steer(CHAT, "competing prompt", None)
                .await
                .is_err()
        );
        assert!(
            core.sessions
                .close_session_terminal(&core.terminals, &terminal.id)
                .is_err()
        );
        assert!(
            core.doc_host
                .open(CHAT)
                .unwrap()
                .doc()
                .read_entries()
                .unwrap()
                .is_empty()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join("native-argv").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let argv = fs::read_to_string(dir.path().join("native-argv")).unwrap();
        assert!(argv.contains(ID));
        assert!(argv.contains("test $(touch INJECTION) with spaces"));
        assert!(!dir.path().join("INJECTION").exists());

        core.sessions
            .write_session_terminal(&core.terminals, &terminal.id, "DQ==")
            .await
            .unwrap();
        assert!(
            !core
                .sessions
                .session_surface(CHAT, &core.terminals)
                .unwrap()
                .can_switch
        );
        assert!(
            core.sessions
                .switch_session_surface(CHAT, SessionSurface::Chat, &core.terminals, 80, 24)
                .await
                .is_err()
        );
        append(
            &binding,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"from CLI"}}),
        );
        append(
            &binding,
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"CLI reply"}]}}),
        );
        append(
            &binding,
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        );
        assert!(
            core.sessions
                .session_surface(CHAT, &core.terminals)
                .unwrap()
                .can_switch
        );
        let state = core
            .sessions
            .switch_session_surface(CHAT, SessionSurface::Chat, &core.terminals, 80, 24)
            .await
            .unwrap();
        assert_eq!(state.surface, SessionSurface::Chat);
        assert!(core.terminals.exited(&terminal.id));
        let handle = core.doc_host.open(CHAT).unwrap();
        assert_eq!(handle.doc().read_entries().unwrap().len(), 2);
        core.sessions
            .switch_session_surface(CHAT, SessionSurface::Chat, &core.terminals, 80, 24)
            .await
            .unwrap();
        assert_eq!(handle.doc().read_entries().unwrap().len(), 2);

        // Replaying a saved CLI checkpoint after interruption must not duplicate
        // entries already imported and persisted before the ownership commit.
        let mut retry = binding.clone();
        retry.surface = SessionSurface::Cli;
        retry.terminal = Some(terminal.clone());
        core.sessions.save_binding(CHAT, &retry).unwrap();
        core.sessions
            .switch_session_surface(CHAT, SessionSurface::Chat, &core.terminals, 80, 24)
            .await
            .unwrap();
        assert_eq!(handle.doc().read_entries().unwrap().len(), 2);
        assert_eq!(
            core.workspace
                .chat(CHAT)
                .unwrap()
                .unwrap()
                .harness_session_id
                .as_deref(),
            Some(ID)
        );
    }

    #[tokio::test]
    async fn native_failed_import_and_restart_keep_chat_locked_until_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let (core, mut binding) = fixture(dir.path());
        binding.surface = SessionSurface::Cli;
        binding.terminal = Some(TerminalSession {
            id: "previous-process".into(),
            cwd: binding.request.cwd.clone(),
            shell: "codex".into(),
        });
        core.sessions.save_binding(CHAT, &binding).unwrap();
        append(
            &binding,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"durable CLI turn"}}),
        );
        let valid = fs::read(&binding.checkpoint.path).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&binding.checkpoint.path)
            .unwrap()
            .write_all(b"{torn")
            .unwrap();
        assert!(
            core.sessions
                .switch_session_surface(CHAT, SessionSurface::Chat, &core.terminals, 80, 24)
                .await
                .is_err()
        );
        assert!(core.sessions.native_owns_chat(CHAT));
        core.workspace.flush();
        let registry = core.registry.clone();
        drop(core);
        let recovered =
            EngineCore::assemble(&dir.path().join("data"), registry, HarnessId::Codex, None)
                .unwrap();
        assert!(recovered.sessions.native_owns_chat(CHAT));
        assert!(
            recovered
                .sessions
                .dispatch(CHAT, HarnessId::Codex, binding.request.clone(), None)
                .await
                .is_err()
        );
        fs::write(&binding.checkpoint.path, valid).unwrap();
        recovered
            .sessions
            .switch_session_surface(CHAT, SessionSurface::Chat, &recovered.terminals, 80, 24)
            .await
            .unwrap();
        assert_eq!(
            recovered
                .doc_host
                .open(CHAT)
                .unwrap()
                .doc()
                .read_entries()
                .unwrap()
                .len(),
            1
        );
        assert!(!recovered.sessions.native_owns_chat(CHAT));
        let registry = recovered.registry.clone();
        drop(recovered);
        let persisted =
            EngineCore::assemble(&dir.path().join("data"), registry, HarnessId::Codex, None)
                .unwrap();
        assert_eq!(
            persisted
                .doc_host
                .open(CHAT)
                .unwrap()
                .doc()
                .read_entries()
                .unwrap()
                .len(),
            1
        );
        assert!(!persisted.sessions.native_owns_chat(CHAT));
        fs::write(persisted.sessions.native_path(CHAT), b"invalid").unwrap();
        assert!(persisted.sessions.native_owns_chat(CHAT));
    }

    #[tokio::test]
    async fn native_busy_and_new_sessions_do_not_open_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let (core, _) = fixture(dir.path());
        core.sessions
            .set_status(CHAT, SessionStatus::Working, false);
        assert!(
            !core
                .sessions
                .session_surface(CHAT, &core.terminals)
                .unwrap()
                .can_switch
        );
        assert!(
            core.sessions
                .switch_session_surface(CHAT, SessionSurface::Cli, &core.terminals, 80, 24)
                .await
                .is_err()
        );
        assert!(!core.sessions.native_owns_chat(CHAT));
        core.workspace
            .create_chat("new", None, Some(&core.device_id), None, None)
            .unwrap();
        assert!(
            !core
                .sessions
                .session_surface("new", &core.terminals)
                .unwrap()
                .can_switch
        );
    }
}

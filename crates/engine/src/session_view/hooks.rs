//! Authenticated native lifecycle, scoped to this engine and each CLI launch.

use super::{EngineError, failed, lock};
use serde::{Serialize, Deserialize};
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex}};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{UnixListener, UnixStream}, task::JoinHandle};
use zeron_harness::provider_history::ResumeCommand;
use zeron_proto::HarnessId;
use zeron_shell_hooks::{Binding, GenerationOptions, HookEnvelope, HookReceiver, Outcome, Peer, Provider, TurnEvent};

/// Transcript support alone cannot make a live Chat/CLI round trip safe.
/// Claude runs Stop hooks in parallel and another hook can continue the turn;
/// UserPromptSubmit does not cover those continuations. Its Stop transcript may
/// also lag the final message. Keep it unavailable until an adapter can prove
/// settled state and freeze every source of new work before process teardown.
pub(super) fn managed_provider(harness: HarnessId) -> Result<Provider, EngineError> {
    match harness {
        HarnessId::Pi => Ok(Provider::Pi),
        HarnessId::ClaudeCode => Err(failed(
            "Claude CLI handoff is unavailable: its hooks cannot yet verify settled state and freeze automatic continuations. Continue in Chat.",
        )),
        _ => Err(failed(format!(
            "{harness:?} CLI handoff is unavailable: native history and a verified input freeze are required. Continue in Chat.",
        ))),
    }
}

/// The generated launcher and relay call one fixed interpreter path; no
/// `/usr/bin/env` lookup runs in the hook child. Resolving it is a
/// precondition of every handoff, so a missing interpreter must surface as a
/// retryable error instead of RecoveryRequired.
pub(super) fn resolve_python() -> Result<PathBuf, EngineError> {
    let mut candidates: Vec<PathBuf> = ["/usr/bin/python3", "/usr/local/bin/python3", "/usr/bin/python"]
        .into_iter().map(PathBuf::from).collect();
    let paths = std::env::var_os("PATH")
        .into_iter()
        .chain(zeron_harness::shell_env::login_shell_path().map(ToOwned::to_owned));
    for path in paths {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("python3")));
    }
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
        failed("handoff unavailable: no Python interpreter; install python3 and retry")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeActivity { Unknown, Busy, Idle, Permission, Ended }

struct Registered {
    activity: NativeActivity,
    frozen: bool,
    directory: PathBuf,
}

#[derive(Default)]
struct State {
    receiver: HookReceiver,
    sessions: HashMap<String, Registered>,
}

pub(super) struct HookRuntime {
    state: Arc<Mutex<State>>,
    root: tempfile::TempDir,
    socket: PathBuf,
    task: JoinHandle<()>,
}

impl Drop for HookRuntime {
    fn drop(&mut self) { self.task.abort(); }
}

impl HookRuntime {
    pub fn start() -> Result<Self, EngineError> {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::Builder::new().prefix("noches-hooks-").tempdir()?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = root.path().canonicalize()?.join("events.sock");
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let state = Arc::new(Mutex::new(State::default()));
        let receiver = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        if connections.len() >= 32 { continue; }
                        let state = receiver.clone();
                        connections.spawn(async move {
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), serve(stream, state)).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { state, root, socket, task })
    }

    pub fn prepare(
        &self,
        chat: &str,
        command: &ResumeCommand,
        native_id: &str,
        python: &std::path::Path,
    ) -> Result<String, EngineError> {
        let provider = managed_provider(command.harness)?;
        let binding = Binding {
            session_id: chat.into(), terminal_id: uuid::Uuid::new_v4().to_string(),
            worktree_path: PathBuf::from(&command.cwd).canonicalize()?,
            provider, provider_session_id: Some(native_id.into()),
        };
        let mut state = lock(&self.state);
        let registration = state.receiver.register(binding).map_err(|e| failed(e.to_string()))?;
        let result = zeron_shell_hooks::generate(&registration, &GenerationOptions {
            parent_dir: self.root.path().canonicalize()?, hook_socket: self.socket.clone(),
            python: python.to_path_buf(), provider_executable: PathBuf::from(&command.program),
            instructions: String::new(), history_path: Some(command.history_path.clone()),
            path_prefix: vec![], notifier_argv: None,
        });
        match result {
            Ok(generated) => {
                state.sessions.insert(chat.into(), Registered { activity: NativeActivity::Unknown,
                    frozen: false, directory: generated.directory });
                Ok(generated.launcher.to_string_lossy().into_owned())
            }
            Err(error) => { state.receiver.revoke(chat); Err(failed(error.to_string())) }
        }
    }

    pub fn activity(&self, chat: &str) -> Option<NativeActivity> {
        lock(&self.state).sessions.get(chat).map(|s| s.activity)
    }

    // Serialized with prompt admission. A provider cannot acknowledge a new
    // turn after this succeeds, even if bytes were already queued in its PTY.
    pub fn freeze_idle(&self, chat: &str) -> Result<(), EngineError> {
        let mut state = lock(&self.state);
        let session = state.sessions.get_mut(chat)
            .ok_or_else(|| failed("Native session state is not verified"))?;
        match session.activity {
            NativeActivity::Idle if !session.frozen => { session.frozen = true; Ok(()) }
            NativeActivity::Busy | NativeActivity::Permission => Err(failed("Session is busy")),
            _ => Err(failed("Native session state is not verified")),
        }
    }

    pub fn revoke(&self, chat: &str) {
        let mut state = lock(&self.state);
        state.receiver.revoke(chat);
        if let Some(session) = state.sessions.remove(chat) {
            let _ = std::fs::remove_dir_all(session.directory);
        }
    }
}

fn accept(state: &mut State, token: &str, body: &[u8]) -> Result<(), EngineError> {
    // Authenticate before using any request identity or changing activity.
    let outcome = state.receiver.handle(Peer::Unix, token, body).map_err(|e| failed(e.to_string()))?;
    let envelope: HookEnvelope = serde_json::from_slice(body).map_err(|e| failed(e.to_string()))?;
    let session = state.sessions.get_mut(&envelope.binding.session_id)
        .ok_or_else(|| failed("Hook registration is no longer active"))?;
    if session.frozen { return Err(failed("Session handoff has frozen native input")); }
    let Outcome::Accepted(event) = outcome else { return Ok(()); };
    session.activity = match event.kind {
        TurnEvent::SessionStarted if envelope.payload["is_idle"] == true => NativeActivity::Idle,
        TurnEvent::TurnCompleted if envelope.payload["is_idle"] == true
            && !event.demoted && !event.background_unknown && event.background_tasks == 0 => NativeActivity::Idle,
        TurnEvent::TurnCompleted => NativeActivity::Busy,
        TurnEvent::SessionEnded => NativeActivity::Ended,
        TurnEvent::PermissionRequested => NativeActivity::Permission,
        TurnEvent::SessionStarted => NativeActivity::Unknown,
        TurnEvent::PromptSubmitted | TurnEvent::RunStarted | TurnEvent::ToolStarted
        | TurnEvent::SubagentStarted | TurnEvent::PermissionResolved => NativeActivity::Busy,
        _ => session.activity,
    };
    Ok(())
}

async fn serve(mut stream: UnixStream, state: Arc<Mutex<State>>) -> Result<(), EngineError> {
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } { return Err(failed("Hook peer UID mismatch")); }
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let n = stream.read(&mut buffer).await?;
        if n == 0 { return Err(failed("Incomplete hook request")); }
        bytes.extend_from_slice(&buffer[..n]);
        if bytes.len() > 73728 { return Err(failed("Hook request too large")); }
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") { break end; }
        if bytes.len() > 8192 { return Err(failed("Hook headers too large")); }
    };
    let header = std::str::from_utf8(&bytes[..end]).map_err(|e| failed(e.to_string()))?;
    if !header.starts_with("POST /hooks HTTP/1.1\r\n") { return Err(failed("Unknown hook route")); }
    let mut length = None;
    let mut token = None;
    for line in header.lines().skip(1) {
        let (key, value) = line.split_once(':').ok_or_else(|| failed("Invalid hook header"))?;
        match key.to_ascii_lowercase().as_str() {
            "content-length" => {
                if length.is_some() { return Err(failed("Duplicate content length")); }
                length = Some(value.trim().parse::<usize>().map_err(|e| failed(e.to_string()))?);
            }
            "authorization" => {
                if token.is_some() { return Err(failed("Duplicate authorization")); }
                token = value.trim().strip_prefix("Bearer ").map(str::to_owned);
            }
            "transfer-encoding" => return Err(failed("Chunked hooks are not supported")),
            _ => {}
        }
    }
    let length = length.filter(|n| *n <= 65536).ok_or_else(|| failed("Invalid hook body size"))?;
    let token = token.ok_or_else(|| failed("Missing hook authorization"))?;
    while bytes.len() < end + 4 + length {
        let mut buffer = [0; 4096];
        let n = stream.read(&mut buffer).await?;
        if n == 0 { return Err(failed("Incomplete hook body")); }
        bytes.extend_from_slice(&buffer[..n]);
    }
    let result = accept(&mut lock(&state), &token, &bytes[end + 4..end + 4 + length]);
    let (code, body) = if result.is_ok() { ("200 OK", "{\"ok\":true}") }
        else { ("409 Conflict", "{\"ok\":false}") };
    stream.write_all(format!("HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
    stream.shutdown().await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_providers_with_verified_admission_and_settlement_are_managed() {
        assert_eq!(managed_provider(HarnessId::Pi).unwrap(), Provider::Pi);
        for harness in [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Cursor,
            HarnessId::Devin, HarnessId::Grok, HarnessId::Hermes, HarnessId::Opencode, HarnessId::Mock] {
            assert!(managed_provider(harness).is_err(), "{harness:?} has no verified lifecycle adapter");
        }
    }

    #[test]
    fn python_resolution_finds_a_real_absolute_interpreter() {
        let python = resolve_python().unwrap();
        assert!(python.is_absolute() && python.is_file(), "{python:?}");
    }

    #[tokio::test]
    async fn lifecycle_authentication_and_freeze_are_atomic() {
        let runtime = HookRuntime::start().unwrap();
        let binding = Binding { session_id: "chat".into(), terminal_id: "terminal".into(),
            worktree_path: runtime.root.path().canonicalize().unwrap(), provider: Provider::Pi,
            provider_session_id: Some("native".into()) };
        let mut state = lock(&runtime.state);
        let registration = state.receiver.register(binding.clone()).unwrap();
        state.sessions.insert("chat".into(), Registered { activity: NativeActivity::Unknown,
            frozen: false, directory: runtime.root.path().join("unused") });
        let body = |name, id| serde_json::to_vec(&HookEnvelope { binding: binding.clone(), event_id: id,
            event_name: name, payload: json!({"session_id":"native", "is_idle":true}) }).unwrap();
        assert!(accept(&mut state, &"0".repeat(64), &body("session_start".into(), "0".into())).is_err());
        assert_eq!(state.sessions["chat"].activity, NativeActivity::Unknown);
        accept(&mut state, &registration.token, &body("session_start".into(), "1".into())).unwrap();
        accept(&mut state, &registration.token, &body("before_agent_start".into(), "2".into())).unwrap();
        drop(state);
        assert!(runtime.freeze_idle("chat").is_err());
        accept(&mut lock(&runtime.state), &registration.token, &body("ui_prompt_start".into(), "permission".into())).unwrap();
        assert_eq!(runtime.activity("chat"), Some(NativeActivity::Permission));
        assert!(runtime.freeze_idle("chat").is_err());
        // A completion event with pending work must not admit teardown.
        let mut pending: serde_json::Value = serde_json::from_slice(&body("agent_settled".into(), "pending".into())).unwrap();
        pending["payload"]["is_idle"] = json!(false);
        accept(&mut lock(&runtime.state), &registration.token, &serde_json::to_vec(&pending).unwrap()).unwrap();
        assert_eq!(runtime.activity("chat"), Some(NativeActivity::Busy));
        assert!(runtime.freeze_idle("chat").is_err());
        accept(&mut lock(&runtime.state), &registration.token, &body("agent_settled".into(), "3".into())).unwrap();
        runtime.freeze_idle("chat").unwrap();
        assert!(accept(&mut lock(&runtime.state), &registration.token, &body("before_agent_start".into(), "4".into())).is_err());
        runtime.revoke("chat");
        assert_eq!(runtime.activity("chat"), None);
    }
}

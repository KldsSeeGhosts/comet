//! Native Pi harness: speaks the CLI's newline-delimited RPC protocol directly.
//! Pi owns model/provider configuration and session files. Comet stores the
//! session file path as its opaque harness session id so later processes can
//! resume with `switch_session`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use zeron_proto::{
    AgentEvent, ContextComponent, ContextComponentKind, DoneStatus, HarnessId, Model,
    ReasoningLevel, RunRequest, SlashCommand, SteeringMode, TodoItem, ToolCall, ToolDiff,
};

use crate::{Harness, HarnessError, RunControls, StderrTail, crash_message, shutdown_child};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(3);
const OUTPUT_CAP: usize = 32 * 1024;

pub fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PI_EXECUTABLE")
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let exe = if cfg!(windows) { "pi.exe" } else { "pi" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(path) = crate::shell_env::login_shell_path() {
        candidates.extend(
            std::env::split_paths(path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(exe)),
        );
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/bin").join(exe));
        candidates.push(home.join(".npm-global/bin").join(exe));
        let pi_node = home.join(".local/share/pi-node");
        if let Ok(entries) = std::fs::read_dir(pi_node) {
            candidates.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path().join("bin").join(exe)),
            );
        }
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(exe));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|dir| dir.join(exe)),
    );
    candidates.into_iter().find(|path| path.exists())
}

pub struct PiHarness {
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    models: tokio::sync::OnceCell<Vec<Model>>,
    commands: tokio::sync::OnceCell<Vec<SlashCommand>>,
}

impl Default for PiHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: INTERRUPT_GRACE,
            kill_grace: KILL_GRACE,
            models: tokio::sync::OnceCell::new(),
            commands: tokio::sync::OnceCell::new(),
        }
    }
}

impl PiHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        self.executable.clone().or_else(resolve_pi_executable).ok_or_else(|| {
            HarnessError::NotInstalled(
                "pi (searched PATH, the login shell's PATH, npm and pi-node install dirs; set PI_EXECUTABLE to override)".into(),
            )
        })
    }
}

#[async_trait]
impl Harness for PiHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn display_name(&self) -> &str {
        "Pi"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    }

    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }

    fn deterministic_turn_end(&self) -> bool {
        true
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe = self.resolve_executable()?;
        self.models
            .get_or_try_init(|| discover_models(&exe))
            .await
            .cloned()
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let exe = self.resolve_executable()?;
        self.commands
            .get_or_try_init(|| discover_commands(&exe))
            .await
            .cloned()
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let receiver = start_session(
            &exe,
            request,
            controls,
            self.interrupt_grace,
            self.kill_grace,
        )
        .await?;
        Ok(
            futures::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            })
            .boxed(),
        )
    }
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Debug)]
enum Incoming {
    Event(Value),
    Eof,
}

#[derive(Clone)]
struct PiRpcClient {
    next_id: Arc<AtomicU64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<String>,
}

impl PiRpcClient {
    fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer, writer_rx) = mpsc::unbounded_channel();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending = Pending::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), incoming_tx));
        (
            Self {
                next_id: Arc::new(AtomicU64::new(0)),
                pending,
                writer,
            },
            incoming_rx,
        )
    }

    async fn request(&self, mut request: Value) -> Result<Value, HarnessError> {
        let id = format!("comet-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        request["id"] = Value::String(id.clone());
        let command = request
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("request")
            .to_owned();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pi pending lock")
            .insert(id.clone(), tx);
        if self.writer.send(request.to_string()).is_err() {
            self.pending.lock().expect("pi pending lock").remove(&id);
            return Err(HarnessError::Protocol(format!(
                "{command}: pi stdin closed"
            )));
        }
        let response = match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(error))) => {
                return Err(HarnessError::Protocol(format!("{command}: {error}")));
            }
            Ok(Err(_)) => {
                return Err(HarnessError::Protocol(format!(
                    "{command}: pi exited before responding"
                )));
            }
            Err(_) => {
                self.pending.lock().expect("pi pending lock").remove(&id);
                return Err(HarnessError::Protocol(format!("{command}: timed out")));
            }
        };
        if response.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(HarnessError::Protocol(format!(
                "{command}: {}",
                pi_error_message(&response)
            )));
        }
        Ok(response)
    }

    fn send(&self, mut command: Value) -> Result<String, HarnessError> {
        let id = format!("comet-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        command["id"] = Value::String(id.clone());
        self.writer
            .send(command.to_string())
            .map_err(|_| HarnessError::Protocol("pi stdin closed".into()))?;
        Ok(id)
    }

    fn send_unidentified(&self, command: Value) {
        let _ = self.writer.send(command.to_string());
    }
}

async fn write_loop(mut stdin: ChildStdin, mut receiver: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = receiver.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err()
            || stdin.write_all(b"\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            return;
        }
    }
}

async fn read_loop(stdout: ChildStdout, pending: Pending, sender: mpsc::Sender<Incoming>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(target: "zeron_harness::pi", "non-JSON stdout line skipped");
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("response")
            && let Some(id) = value.get("id").and_then(Value::as_str)
            && let Some(waiter) = pending.lock().expect("pi pending lock").remove(id)
        {
            let _ = waiter.send(Ok(value));
            continue;
        }
        if sender.send(Incoming::Event(value)).await.is_err() {
            return;
        }
    }
    for (_, waiter) in pending.lock().expect("pi pending lock").drain() {
        let _ = waiter.send(Err("pi RPC process exited".into()));
    }
    let _ = sender.send(Incoming::Eof).await;
}

fn probe_command(exe: &Path, models_only: bool) -> Command {
    let mut command = Command::new(exe);
    crate::compose_child_path(&mut command, exe);
    command.args(["--mode", "rpc", "--no-session", "--no-context-files"]);
    if models_only {
        // Providers may come from extensions, so keep extensions enabled.
        // Skills and prompt templates are irrelevant to model discovery.
        command.args(["--no-skills", "--no-prompt-templates"]);
    }
    command
        .env("PI_SKIP_VERSION_CHECK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn start_probe(
    exe: &Path,
    models_only: bool,
) -> Result<(Child, PiRpcClient, mpsc::Receiver<Incoming>), HarnessError> {
    let mut child = probe_command(exe, models_only)
        .spawn()
        .map_err(HarnessError::Io)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("pi probe has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("pi probe has no stdout".into()))?;
    let (rpc, incoming) = PiRpcClient::new(stdin, stdout);
    Ok((child, rpc, incoming))
}

async fn discover_models(exe: &Path) -> Result<Vec<Model>, HarnessError> {
    let (mut child, rpc, _incoming) = start_probe(exe, true).await?;
    let models = rpc.request(json!({"type": "get_available_models"})).await;
    let state = rpc.request(json!({"type": "get_state"})).await.ok();
    shutdown_child(&mut child, KILL_GRACE).await;
    let models = models?;
    let parsed = parse_pi_models(&models, state.as_ref());
    if parsed.is_empty() {
        return Err(HarnessError::Protocol(
            "pi returned no available models".into(),
        ));
    }
    Ok(parsed)
}

async fn discover_commands(exe: &Path) -> Result<Vec<SlashCommand>, HarnessError> {
    let (mut child, rpc, _incoming) = start_probe(exe, false).await?;
    let response = rpc.request(json!({"type": "get_commands"})).await;
    shutdown_child(&mut child, KILL_GRACE).await;
    response.map(|value| parse_pi_commands(&value))
}

fn parse_pi_models(response: &Value, state: Option<&Value>) -> Vec<Model> {
    let default = state.and_then(|state| {
        Some(format!(
            "{}/{}",
            state.pointer("/data/model/provider")?.as_str()?,
            state.pointer("/data/model/id")?.as_str()?
        ))
    });
    let mut models: Vec<Model> = response
        .pointer("/data/models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let provider = model.get("provider")?.as_str()?.trim();
            let id = model.get("id")?.as_str()?.trim();
            if provider.is_empty() || id.is_empty() {
                return None;
            }
            Some(Model {
                id: format!("{provider}/{id}"),
                label: model
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(id)
                    .to_owned(),
                description: Some(provider.to_owned()),
                reasoning_levels: parse_reasoning_levels(model),
                options: Vec::new(),
            })
        })
        .collect();
    if let Some(default) = default
        && let Some(index) = models.iter().position(|model| model.id == default)
    {
        let model = models.remove(index);
        models.insert(0, model);
    }
    models
}

fn parse_reasoning_levels(model: &Value) -> Vec<ReasoningLevel> {
    if model.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let map = model.get("thinkingLevelMap").and_then(Value::as_object);
    [
        ("minimal", ReasoningLevel::Minimal),
        ("low", ReasoningLevel::Low),
        ("medium", ReasoningLevel::Medium),
        ("high", ReasoningLevel::High),
        ("xhigh", ReasoningLevel::XHigh),
        ("max", ReasoningLevel::Max),
    ]
    .into_iter()
    .filter(|(name, _)| {
        let value = map.and_then(|map| map.get(*name));
        if matches!(*name, "xhigh" | "max") {
            value.is_some_and(|value| !value.is_null())
        } else {
            value.is_none_or(|value| !value.is_null())
        }
    })
    .map(|(_, level)| level)
    .collect()
}

fn command_array(value: &Value) -> &[Value] {
    value
        .pointer("/data/commands")
        .or_else(|| value.get("commands"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn parse_pi_commands(value: &Value) -> Vec<SlashCommand> {
    command_array(value)
        .iter()
        .filter_map(|command| {
            let raw = command.get("name")?.as_str()?.trim();
            if raw.is_empty() {
                return None;
            }
            let name = raw.strip_prefix("skill:").unwrap_or(raw).to_owned();
            Some(SlashCommand {
                name,
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_hint: command
                    .pointer("/input/hint")
                    .or_else(|| command.get("argumentHint"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|hint| !hint.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn reasoning_name(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High | ReasoningLevel::Ultrathink => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode => "xhigh",
        ReasoningLevel::Max | ReasoningLevel::Ultra => "max",
    }
}

fn model_parts(model: &str) -> Result<(&str, &str), HarnessError> {
    let (provider, id) = model.trim().split_once('/').ok_or_else(|| {
        HarnessError::Protocol(format!("pi model must use provider/model format: {model}"))
    })?;
    if provider.is_empty() || id.is_empty() {
        return Err(HarnessError::Protocol(format!(
            "pi model must use provider/model format: {model}"
        )));
    }
    Ok((provider, id))
}

async fn start_session(
    exe: &Path,
    request: RunRequest,
    controls: RunControls,
    interrupt_grace: Duration,
    kill_grace: Duration,
) -> Result<mpsc::Receiver<Result<AgentEvent, HarnessError>>, HarnessError> {
    let mut command = Command::new(exe);
    crate::compose_child_path(&mut command, exe);
    command
        .args(["--mode", "rpc", "--approve"])
        .env("PI_SKIP_VERSION_CHECK", "1")
        .current_dir(&request.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Load on top of the user's own extensions; a failed install only costs
    // the context breakdown, never the run.
    if let Some(extension) = install_context_extension() {
        command.arg("-e").arg(extension);
    }
    let mut child = command.spawn().map_err(HarnessError::Io)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("pi child has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("pi child has no stdout".into()))?;
    let stderr_tail = StderrTail::default();
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "zeron_harness::pi", "stderr: {line}");
                tail.push(&line);
            }
        });
    }
    let (rpc, incoming) = PiRpcClient::new(stdin, stdout);

    let initial_state = rpc.request(json!({"type": "get_state"})).await;
    let initial_state = match initial_state {
        Ok(state) => state,
        Err(error) => {
            shutdown_child(&mut child, kill_grace).await;
            return Err(error);
        }
    };
    if let Some(session_path) = request.resume.as_deref() {
        let response = match rpc
            .request(json!({
                "type": "switch_session",
                "sessionPath": session_path,
            }))
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                shutdown_child(&mut child, kill_grace).await;
                return Err(err);
            }
        };
        if response.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            shutdown_child(&mut child, kill_grace).await;
            return Err(HarnessError::Protocol(
                "Pi session switch was cancelled".into(),
            ));
        }
    }
    if let Some(model) = request.model.as_deref() {
        let (provider, model_id) = match model_parts(model) {
            Ok(parts) => parts,
            Err(err) => {
                shutdown_child(&mut child, kill_grace).await;
                return Err(err);
            }
        };
        if let Err(err) = rpc
            .request(json!({
                "type": "set_model",
                "provider": provider,
                "modelId": model_id,
            }))
            .await
        {
            shutdown_child(&mut child, kill_grace).await;
            return Err(err);
        }
    }
    if let Some(reasoning) = request.reasoning {
        if let Err(err) = rpc
            .request(json!({
                "type": "set_thinking_level",
                "level": reasoning_name(reasoning),
            }))
            .await
        {
            shutdown_child(&mut child, kill_grace).await;
            return Err(err);
        }
    }
    let state = rpc
        .request(json!({"type": "get_state"}))
        .await
        .unwrap_or(initial_state);
    let native_session = match state
        .pointer("/data/sessionFile")
        .and_then(Value::as_str)
        .or_else(|| state.pointer("/data/sessionId").and_then(Value::as_str))
    {
        Some(session) => session.to_owned(),
        None => {
            shutdown_child(&mut child, kill_grace).await;
            return Err(HarnessError::Protocol(
                "pi did not report a session id or file".into(),
            ));
        }
    };
    let model = state
        .pointer("/data/model/provider")
        .and_then(Value::as_str)
        .zip(state.pointer("/data/model/id").and_then(Value::as_str))
        .map(|(provider, id)| format!("{provider}/{id}"))
        .or_else(|| request.model.clone())
        .unwrap_or_else(|| "default".into());
    let commands = rpc
        .request(json!({"type": "get_commands"}))
        .await
        .ok()
        .map(|value| parse_pi_commands(&value))
        .unwrap_or_default();
    let context = rpc
        .request(json!({"type": "get_session_stats"}))
        .await
        .ok()
        .and_then(|stats| context_usage(&state, Some(&stats)))
        .or_else(|| context_usage(&state, None));

    let (event_tx, event_rx) = mpsc::channel(256);
    let assistant_id = new_message_id();
    if event_tx
        .send(Ok(AgentEvent::SessionStarted {
            harness: HarnessId::Pi,
            model,
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: native_session.clone(),
            assistant_message_id: assistant_id.clone(),
        }))
        .await
        .is_err()
    {
        shutdown_child(&mut child, kill_grace).await;
        return Err(HarnessError::Protocol("pi event consumer closed".into()));
    }
    if !commands.is_empty() {
        let _ = event_tx
            .send(Ok(AgentEvent::AvailableCommands { commands }))
            .await;
    }
    // Live stats own the totals; the persisted entry contributes the breakdown.
    let entry = read_context_entry(&native_session).unwrap_or_default();
    let (mut tokens, mut window) = context.unwrap_or((None, None));
    if tokens.is_none() {
        tokens = entry.tokens;
    }
    if window.is_none() {
        window = entry.window;
    }
    if tokens.is_some() || window.is_some() || !entry.components.is_empty() {
        let _ = event_tx
            .send(Ok(AgentEvent::ContextUsage {
                tokens,
                window,
                components: entry.components,
            }))
            .await;
    }
    let prompt_id = match rpc.send(json!({"type": "prompt", "message": request.prompt})) {
        Ok(id) => id,
        Err(err) => {
            shutdown_child(&mut child, kill_grace).await;
            return Err(err);
        }
    };
    tokio::spawn(session_loop(Session {
        child,
        rpc,
        incoming,
        event_tx,
        controls,
        native_session,
        assistant_id,
        prompt_id: Some(prompt_id),
        interrupt_grace,
        kill_grace,
        stderr_tail,
    }));
    Ok(event_rx)
}

struct Session {
    child: Child,
    rpc: PiRpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    native_session: String,
    assistant_id: String,
    prompt_id: Option<String>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: StderrTail,
}

async fn session_loop(session: Session) {
    let Session {
        mut child,
        rpc,
        mut incoming,
        event_tx,
        controls,
        native_session,
        mut assistant_id,
        mut prompt_id,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input: _,
        mut steering,
        interrupt,
    } = controls;
    let mut steering_open = true;
    let mut active_turn = true;
    let mut done_emitted = false;
    let mut failed = false;
    let mut saw_text = false;
    let mut saw_reasoning = false;
    let mut tools: HashMap<String, ToolCall> = HashMap::new();
    let mut last_tool_results: HashMap<String, (Option<String>, Option<ToolDiff>, bool)> =
        HashMap::new();

    'main: loop {
        tokio::select! {
            incoming_message = incoming.recv() => match incoming_message {
                Some(Incoming::Event(value)) => {
                    let event_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
                    if event_type == "response" {
                        let id = value.get("id").and_then(Value::as_str);
                        if id == prompt_id.as_deref() {
                            let success = value.get("success").and_then(Value::as_bool) != Some(false);
                            let local_only = value.pointer("/data/agentInvoked").and_then(Value::as_bool) == Some(false);
                            if !success || local_only {
                                active_turn = false;
                                prompt_id = None;
                                if !success {
                                    failed = true;
                                    let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
                                }
                                if !done_emitted {
                                    done_emitted = true;
                                    let _ = send_done(&event_tx, !failed, &native_session).await;
                                }
                                if !steering_open { break 'main; }
                            }
                        } else if value.get("success").and_then(Value::as_bool) == Some(false) {
                            if active_turn || !done_emitted {
                                let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
                            }
                        }
                        continue;
                    }
                    if is_terminal_settle(event_type, &value) {
                        if active_turn && !done_emitted {
                            active_turn = false;
                            prompt_id = None;
                            done_emitted = true;
                            // The companion extension appends the measured
                            // composition at settle; publish it before Done so
                            // the card tracks the turn that just finished.
                            if let Some(entry) = read_context_entry(&native_session)
                                && (!entry.components.is_empty() || entry.tokens.is_some())
                            {
                                let _ = send_event(
                                    &event_tx,
                                    AgentEvent::ContextUsage {
                                        tokens: entry.tokens,
                                        window: entry.window,
                                        components: entry.components,
                                    },
                                )
                                .await;
                            }
                            let _ = send_done(&event_tx, !failed, &native_session).await;
                            failed = false;
                            if !steering_open { break 'main; }
                        }
                        continue;
                    }
                    match event_type {
                        "agent_start" | "turn_start" => active_turn = true,
                        "command_output" => {
                            if let Some(text) = value.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                let _ = send_event(&event_tx, AgentEvent::TextDelta { text: format!("{text}\n\n") }).await;
                            }
                        }
                        "message_start" if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") => {
                            saw_text = false;
                            saw_reasoning = false;
                        }
                        "message_update" => {
                            let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
                            match update.get("type").and_then(Value::as_str) {
                                Some("text_delta") => if let Some(text) = update.get("delta").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                    saw_text = true;
                                    let _ = send_event(&event_tx, AgentEvent::TextDelta { text: text.into() }).await;
                                },
                                Some("thinking_delta") => if let Some(text) = update.get("delta").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                    saw_reasoning = true;
                                    let _ = send_event(&event_tx, AgentEvent::ReasoningDelta { text: text.into() }).await;
                                },
                                Some("error") => {
                                    failed = true;
                                    let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(update) }).await;
                                }
                                _ => {}
                            }
                        }
                        "message_end" if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") => {
                            if let Some(tokens) = value.get("message").and_then(message_context_tokens) {
                                let _ = send_event(&event_tx, AgentEvent::ContextUsage { tokens: Some(tokens), window: None, components: Vec::new() }).await;
                            }
                            emit_message_fallback(&event_tx, value.get("message"), &mut saw_text, &mut saw_reasoning).await;
                            let prev = std::mem::replace(&mut assistant_id, new_message_id());
                            let _ = send_event(&event_tx, AgentEvent::AssistantMessageCompleted { assistant_message_id: prev }).await;
                        }
                        "tool_execution_start" => {
                            let id = value.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_owned();
                            let name = value.get("toolName").and_then(Value::as_str).unwrap_or("tool");
                            let call = map_tool_call(name, value.get("args").unwrap_or(&Value::Null));
                            tools.insert(id.clone(), call.clone());
                            let _ = send_event(&event_tx, AgentEvent::ToolCall { id, call }).await;
                        }
                        "tool_execution_update" => {
                            if let Some(event) = handle_tool_update(&tools, &mut last_tool_results, &value) {
                                let _ = send_event(&event_tx, event).await;
                            }
                        }
                        "tool_execution_end" => {
                            if let Some(event) = handle_tool_end(&mut tools, &mut last_tool_results, &value) {
                                let _ = send_event(&event_tx, event).await;
                            }
                        }
                        "auto_retry_end" => {
                            failed = value.get("success").and_then(Value::as_bool) != Some(true);
                            if failed {
                                let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
                            }
                        }
                        "available_commands_update" => {
                            let _ = send_event(&event_tx, AgentEvent::AvailableCommands { commands: parse_pi_commands(&value) }).await;
                        }
                        "extension_ui_request" => if let Some(id) = value.get("id").and_then(Value::as_str) {
                            rpc.send_unidentified(json!({"type": "extension_ui_response", "id": id, "cancelled": true}));
                        },
                        "extension_error" => {
                            let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
                        }
                        _ => {}
                    }
                }
                Some(Incoming::Eof) | None => {
                    if active_turn && !done_emitted {
                        let status = child.try_wait().ok().flatten();
                        let _ = send_event(
                            &event_tx,
                            AgentEvent::Done {
                                status: DoneStatus::Errored,
                                result: None,
                                error: Some(crash_message("pi", status, &stderr_tail)),
                                session_id: Some(native_session.clone()),
                            },
                        )
                        .await;
                    }
                    break 'main;
                }
            },
            steer = steering.recv(), if steering_open => match steer {
                Some(message) => {
                    let next = new_message_id();
                    let _ = send_event(&event_tx, AgentEvent::Steered {
                        assistant_message_id: Some(assistant_id.clone()),
                        next_assistant_message_id: Some(next.clone()),
                    }).await;
                    assistant_id = next;
                    if active_turn {
                        if let Err(err) = rpc.send(json!({"type": "steer", "message": message.prompt})) {
                            let _ = send_event(
                                &event_tx,
                                AgentEvent::Error {
                                    message: err.to_string(),
                                },
                            )
                            .await;
                        }
                    } else {
                        saw_text = false;
                        saw_reasoning = false;
                        failed = false;
                        active_turn = true;
                        done_emitted = false;
                        match rpc.send(json!({"type": "prompt", "message": message.prompt})) {
                            Ok(id) => {
                                prompt_id = Some(id);
                            }
                            Err(err) => {
                                let err_msg = err.to_string();
                                let _ = send_event(&event_tx, AgentEvent::Error { message: err_msg.clone() }).await;
                                active_turn = false;
                                prompt_id = None;
                                done_emitted = true;
                                let _ = send_event(
                                    &event_tx,
                                    AgentEvent::Done {
                                        status: DoneStatus::Errored,
                                        result: None,
                                        error: Some(err_msg),
                                        session_id: Some(native_session.clone()),
                                    },
                                ).await;
                                if !steering_open { break 'main; }
                            }
                        }
                    }
                }
                None => {
                    steering_open = false;
                    if !active_turn { break 'main; }
                }
            },
            _ = interrupt.cancelled() => {
                if !active_turn || done_emitted {
                    break 'main;
                }

                let (abort_tx, mut abort_rx) = oneshot::channel();
                let abort_id = format!("comet-{}", rpc.next_id.fetch_add(1, Ordering::Relaxed) + 1);
                rpc.pending
                    .lock()
                    .expect("pi pending lock")
                    .insert(abort_id.clone(), abort_tx);
                if rpc
                    .writer
                    .send(json!({"id": abort_id, "type": "abort"}).to_string())
                    .is_err()
                {
                    rpc.pending.lock().expect("pi pending lock").remove(&abort_id);
                }

                let mut ack_received = false;
                let mut settled_received = false;
                let grace_deadline = tokio::time::Instant::now() + interrupt_grace;

                while !ack_received || !settled_received {
                    tokio::select! {
                        _ = tokio::time::sleep_until(grace_deadline) => {
                            break;
                        }
                        _ = event_tx.closed() => {
                            break;
                        }
                        res = &mut abort_rx, if !ack_received => {
                            ack_received = true;
                            match res {
                                Ok(Ok(resp)) => {
                                    if resp.get("success").and_then(Value::as_bool) == Some(false) {
                                        let _ = send_event(&event_tx, AgentEvent::Error {
                                            message: pi_error_message(&resp),
                                        }).await;
                                    }
                                }
                                Ok(Err(err)) => {
                                    let _ = send_event(&event_tx, AgentEvent::Error {
                                        message: err,
                                    }).await;
                                }
                                Err(_) => {}
                            }
                        }
                        incoming_msg = incoming.recv(), if !settled_received => {
                            match incoming_msg {
                                Some(Incoming::Event(value)) => {
                                    let event_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
                                    if is_terminal_settle(event_type, &value) {
                                        settled_received = true;
                                    } else {
                                        match event_type {
                                            "tool_execution_update" => {
                                                if let Some(event) = handle_tool_update(&tools, &mut last_tool_results, &value) {
                                                    let _ = send_event(&event_tx, event).await;
                                                }
                                            }
                                            "tool_execution_end" => {
                                                if let Some(event) = handle_tool_end(&mut tools, &mut last_tool_results, &value) {
                                                    let _ = send_event(&event_tx, event).await;
                                                }
                                            }
                                            "auto_retry_end" => {
                                                if value.get("success").and_then(Value::as_bool) == Some(false) {
                                                    let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
                                                }
                                            }
                                            "extension_error" => {
                                                let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Some(Incoming::Eof) | None => {
                                    break;
                                }
                            }
                        }
                    }
                }

                rpc.pending.lock().expect("pi pending lock").remove(&abort_id);

                if !done_emitted {
                    let _ = send_event(
                        &event_tx,
                        AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            result: None,
                            error: None,
                            session_id: Some(native_session.clone()),
                        },
                    )
                    .await;
                }
                break 'main;
            },
            _ = event_tx.closed() => break 'main,
        }
    }
    shutdown_child(&mut child, kill_grace).await;
}

fn is_terminal_settle(event_type: &str, value: &Value) -> bool {
    match event_type {
        "agent_settled" => value.get("isTerminal").and_then(Value::as_bool) != Some(false),
        "agent_end" => value.get("isTerminal").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

fn handle_tool_update(
    tools: &HashMap<String, ToolCall>,
    last_tool_results: &mut HashMap<String, (Option<String>, Option<ToolDiff>, bool)>,
    value: &Value,
) -> Option<AgentEvent> {
    let id = value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let call = tools.get(&id);
    let partial = value.get("partialResult").or_else(|| value.get("result"));
    let diff = tool_diff(call, partial);
    let output = partial
        .and_then(render_value)
        .filter(|text| !text.is_empty());
    let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
    if output.is_some() || diff.is_some() {
        let entry = (output.clone(), diff.clone(), is_error);
        if last_tool_results.get(&id) != Some(&entry) {
            last_tool_results.insert(id.clone(), entry);
            return Some(AgentEvent::ToolResult {
                id,
                is_error,
                output,
                diff,
            });
        }
    }
    None
}

fn handle_tool_end(
    tools: &mut HashMap<String, ToolCall>,
    last_tool_results: &mut HashMap<String, (Option<String>, Option<ToolDiff>, bool)>,
    value: &Value,
) -> Option<AgentEvent> {
    let id = value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let call = tools.remove(&id);
    let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
    let result = value.get("result");
    let diff = tool_diff(call.as_ref(), result);
    let output = result
        .and_then(render_value)
        .filter(|text| !text.is_empty());
    let prev = last_tool_results.remove(&id);
    let entry = (output, diff, is_error);
    if let Some(prev) = prev {
        if prev != entry {
            let (output, diff, is_error) = entry;
            return Some(AgentEvent::ToolResult {
                id,
                is_error,
                output,
                diff,
            });
        }
    } else {
        let (output, diff, is_error) = entry;
        return Some(AgentEvent::ToolResult {
            id,
            is_error,
            output,
            diff,
        });
    }
    None
}

async fn send_event(
    sender: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    event: AgentEvent,
) -> Result<(), ()> {
    sender.send(Ok(event)).await.map_err(|_| ())
}

async fn send_done(
    sender: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    success: bool,
    session_id: &str,
) -> Result<(), ()> {
    send_event(
        sender,
        AgentEvent::Done {
            status: if success {
                DoneStatus::Completed
            } else {
                DoneStatus::Errored
            },
            result: None,
            error: (!success).then(|| "Pi could not complete the turn".into()),
            session_id: Some(session_id.into()),
        },
    )
    .await
}

fn new_message_id() -> String {
    format!("pi-{}", uuid::Uuid::new_v4())
}

/// Companion extension bundled with this build: it measures what each turn
/// sends to the model and appends the breakdown to the session file as a
/// custom entry, which we read back to fill the usage card.
const CONTEXT_EXTENSION: &str = include_str!("zeron-context.ts");
const CONTEXT_ENTRY_TYPE: &str = "zeron:context-usage";
/// Entries land at the file tail every turn, so a bounded tail read is enough
/// even for very long sessions.
const CONTEXT_TAIL_BYTES: u64 = 256 * 1024;

fn context_extension_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".zeron")
            .join("pi")
            .join("extensions")
            .join("zeron-context.ts"),
    )
}

/// Install the bundled extension under the zeron-owned prefix, refreshing the
/// file only when the bundled source changed. Best-effort: `None` simply means
/// the run proceeds without the context breakdown.
fn install_context_extension() -> Option<PathBuf> {
    let path = context_extension_path()?;
    let dir = path.parent()?.to_owned();
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == CONTEXT_EXTENSION) {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let staging = dir.join(format!(
        "{}.new-{}",
        path.file_name()?.to_str()?,
        std::process::id()
    ));
    std::fs::write(&staging, CONTEXT_EXTENSION).ok()?;
    if std::fs::rename(&staging, &path).is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    Some(path)
}

/// Latest breakdown persisted by the companion extension.
#[derive(Debug, Default, Clone, PartialEq)]
struct PiContextEntry {
    components: Vec<ContextComponent>,
    tokens: Option<u64>,
    window: Option<u64>,
}

fn parse_context_entry(data: &Value) -> Option<PiContextEntry> {
    let field = |key: &str| {
        data.get(key)
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
    };
    let mut components = Vec::new();
    for (kind, key) in [
        (ContextComponentKind::Tools, "tools"),
        (ContextComponentKind::SystemPrompt, "systemPrompt"),
        (ContextComponentKind::Skills, "skills"),
        (ContextComponentKind::ContextFiles, "contextFiles"),
        (ContextComponentKind::Messages, "messages"),
    ] {
        if let Some(tokens) = field(key) {
            components.push(ContextComponent { kind, tokens });
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(PiContextEntry {
        components,
        tokens: field("totalTokens"),
        window: field("contextWindow"),
    })
}

fn read_context_entry(session_file: &str) -> Option<PiContextEntry> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(session_file).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(CONTEXT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(CONTEXT_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    // A capped read can split the first line; drop it unless the file fit.
    let lines: Vec<&str> = tail.lines().skip(usize::from(start > 0)).collect();
    let entry = lines
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|value| {
            value.get("type").and_then(Value::as_str) == Some("custom")
                && value.get("customType").and_then(Value::as_str) == Some(CONTEXT_ENTRY_TYPE)
        })?;
    parse_context_entry(entry.get("data")?)
}

fn context_usage(state: &Value, stats: Option<&Value>) -> Option<(Option<u64>, Option<u64>)> {
    let context = stats.and_then(|stats| stats.pointer("/data/contextUsage"));
    let tokens = context
        .and_then(|value| value.get("tokens"))
        .and_then(Value::as_u64);
    let window = context
        .and_then(|value| value.get("contextWindow"))
        .and_then(Value::as_u64)
        .or_else(|| {
            state
                .pointer("/data/model/contextWindow")
                .and_then(Value::as_u64)
        })
        .filter(|window| *window > 0);
    (tokens.is_some() || window.is_some()).then_some((tokens, window))
}

fn message_context_tokens(message: &Value) -> Option<u64> {
    let usage = message.get("usage")?;
    usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .or_else(|| {
            let total = ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0, u64::saturating_add);
            (total > 0).then_some(total)
        })
}

async fn emit_message_fallback(
    sender: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    message: Option<&Value>,
    saw_text: &mut bool,
    saw_reasoning: &mut bool,
) {
    let Some(content) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") if !*saw_text => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    *saw_text = true;
                    let _ = send_event(sender, AgentEvent::TextDelta { text: text.into() }).await;
                }
            }
            Some("thinking") if !*saw_reasoning => {
                if let Some(text) = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    *saw_reasoning = true;
                    let _ =
                        send_event(sender, AgentEvent::ReasoningDelta { text: text.into() }).await;
                }
            }
            _ => {}
        }
    }
}

fn pi_error_message(value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("errorMessage").and_then(Value::as_str))
        .or_else(|| value.get("finalError").and_then(Value::as_str))
        .or_else(|| value.get("reason").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("Pi reported an error")
        .to_owned()
}

fn render_value(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let text = if let Some(text) = value.as_str() {
        text.to_owned()
    } else if let Some(text) = value.get("text").and_then(Value::as_str) {
        text.to_owned()
    } else if let Some(content) = value.get("content") {
        if let Some(text) = content.as_str() {
            text.to_owned()
        } else if let Some(array) = content.as_array() {
            let joined = array
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .or_else(|| item.get("text").and_then(Value::as_str).map(str::to_owned))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.is_empty() {
                serde_json::to_string(content).unwrap_or_default()
            } else {
                joined
            }
        } else {
            serde_json::to_string(content).unwrap_or_default()
        }
    } else if let Some(array) = value.as_array() {
        let joined = array
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .or_else(|| item.get("text").and_then(Value::as_str).map(str::to_owned))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() {
            serde_json::to_string(value).unwrap_or_default()
        } else {
            joined
        }
    } else {
        value.to_string()
    };

    if text.is_empty() {
        return None;
    }

    let mut text = text;
    if text.len() > OUTPUT_CAP {
        text.truncate(OUTPUT_CAP);
        text.push_str("\n… output truncated");
    }
    Some(text)
}

fn string_field(value: &Value, names: &[&str]) -> String {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn optional_string(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::to_owned)
}

fn map_tool_call(name: &str, args: &Value) -> ToolCall {
    match name.to_ascii_lowercase().as_str() {
        "bash" | "exec" => ToolCall::Exec {
            command: string_field(args, &["command", "cmd"]),
        },
        "read" | "read_file" => ToolCall::ReadFile {
            path: string_field(args, &["path", "file_path"]),
        },
        "write" | "write_file" => ToolCall::WriteFile {
            path: string_field(args, &["path", "file_path"]),
            content: optional_string(args, &["content"]),
        },
        "edit" | "edit_file" => {
            let old_string =
                optional_string(args, &["oldText", "old_string", "old_text"]).or_else(|| {
                    args.get("edits")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|e| optional_string(e, &["oldText", "old_string", "old_text"]))
                });
            let new_string =
                optional_string(args, &["newText", "new_string", "new_text"]).or_else(|| {
                    args.get("edits")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|e| optional_string(e, &["newText", "new_string", "new_text"]))
                });
            ToolCall::EditFile {
                path: string_field(args, &["path", "file_path"]),
                old_string,
                new_string,
            }
        }
        "grep" | "search" => ToolCall::Search {
            pattern: string_field(args, &["pattern", "query"]),
            path: optional_string(args, &["path"]),
        },
        "find" | "glob" => ToolCall::Glob {
            pattern: string_field(args, &["pattern", "glob"]),
        },
        "web_search" => ToolCall::WebSearch {
            query: string_field(args, &["query"]),
        },
        "web_fetch" => ToolCall::WebFetch {
            url: string_field(args, &["url"]),
            prompt: optional_string(args, &["prompt"]),
        },
        "todo" => ToolCall::Todo {
            items: args
                .get("items")
                .or_else(|| args.get("todos"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|item| TodoItem {
                    text: string_field(item, &["text", "content"]),
                    done: item.get("done").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect(),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input: (!args.is_null()).then(|| args.clone()),
        },
    }
}

fn tool_diff(call: Option<&ToolCall>, result: Option<&Value>) -> Option<ToolDiff> {
    let path = match call? {
        ToolCall::EditFile { path, .. } | ToolCall::WriteFile { path, .. } => path.clone(),
        _ => return None,
    };
    let result = result?;
    let new_text = result
        .get("newText")
        .or_else(|| result.get("new_text"))
        .or_else(|| result.pointer("/details/diff"))
        .or_else(|| result.pointer("/details/patch"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| match call? {
            ToolCall::WriteFile {
                content: Some(content),
                ..
            } => Some(content.clone()),
            ToolCall::EditFile {
                new_string: Some(new_string),
                ..
            } => Some(new_string.clone()),
            _ => None,
        })
        .or_else(|| {
            result
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })?;

    let old_text = result
        .get("oldText")
        .or_else(|| result.get("old_text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| match call? {
            ToolCall::EditFile {
                old_string: Some(old_string),
                ..
            } => Some(old_string.clone()),
            _ => None,
        });

    Some(ToolDiff {
        path,
        old_text,
        new_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_entry_is_read_from_the_session_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let entry = |tokens: &str| {
            format!(
                r#"{{"type":"custom","id":"e1","customType":"{CONTEXT_ENTRY_TYPE}","data":{tokens}}}"#
            )
        };
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n{}\n",
                r#"{"type":"message","message":{"role":"user"}}"#,
                entry(r#"{"v":1,"tools":900,"systemPrompt":250,"skills":40,"contextFiles":0,"messages":810,"totalTokens":2000,"contextWindow":1048576}"#),
                // Malformed and unrelated lines must be skipped silently.
                "{not json",
                entry(r#"{"v":1,"tools":910,"systemPrompt":250,"skills":40,"contextFiles":12,"messages":838,"totalTokens":2050,"contextWindow":1048576}"#),
            ),
        )
        .unwrap();
        let parsed = read_context_entry(path.to_str().unwrap()).unwrap();
        assert_eq!(parsed.tokens, Some(2050));
        assert_eq!(parsed.window, Some(1_048_576));
        // Zero categories are dropped; the rest keep the stable wire order.
        assert_eq!(
            parsed.components,
            vec![
                ContextComponent {
                    kind: ContextComponentKind::Tools,
                    tokens: 910
                },
                ContextComponent {
                    kind: ContextComponentKind::SystemPrompt,
                    tokens: 250
                },
                ContextComponent {
                    kind: ContextComponentKind::Skills,
                    tokens: 40
                },
                ContextComponent {
                    kind: ContextComponentKind::ContextFiles,
                    tokens: 12
                },
                ContextComponent {
                    kind: ContextComponentKind::Messages,
                    tokens: 838
                },
            ]
        );
    }

    #[test]
    fn context_entry_survives_a_split_tail_and_stays_absent_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let entry = r#"{"type":"custom","customType":"zeron:context-usage","data":{"tools":10,"messages":15,"totalTokens":25}}"#;
        // The entry sits beyond the tail cap behind one oversized line.
        let padding = "x".repeat(CONTEXT_TAIL_BYTES as usize);
        std::fs::write(&path, format!("{padding}\n{entry}\n")).unwrap();
        let parsed = read_context_entry(path.to_str().unwrap()).unwrap();
        assert_eq!(parsed.tokens, Some(25));
        // An entry without measurable categories is no breakdown at all.
        std::fs::write(
            &path,
            r#"{"type":"custom","customType":"zeron:context-usage","data":{"messages":0}}"#,
        )
        .unwrap();
        assert_eq!(read_context_entry(path.to_str().unwrap()), None);
        assert_eq!(read_context_entry("/nonexistent/session.jsonl"), None);
    }

    #[test]
    fn models_use_provider_slugs_and_native_thinking_map() {
        let response = json!({"data": {"models": [
            {"provider": "anthropic", "id": "sonnet", "name": "Sonnet", "reasoning": true,
             "thinkingLevelMap": {"minimal": "low", "low": "low", "medium": "medium", "high": "high", "xhigh": "high", "max": null}},
            {"provider": "openai", "id": "gpt", "reasoning": false}
        ]}});
        let state = json!({"data": {"model": {"provider": "openai", "id": "gpt"}}});
        let models = parse_pi_models(&response, Some(&state));
        assert_eq!(models[0].id, "openai/gpt");
        assert_eq!(models[1].id, "anthropic/sonnet");
        assert_eq!(
            models[1].reasoning_levels.last(),
            Some(&ReasoningLevel::XHigh)
        );
        assert!(!models[1].reasoning_levels.contains(&ReasoningLevel::Max));
    }

    #[test]
    fn commands_and_common_tools_are_typed() {
        let commands = parse_pi_commands(&json!({"data": {"commands": [
            {"name": "skill:review", "source": "skill", "description": "Review", "input": {"hint": "<file>"}}
        ]}}));
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].input_hint.as_deref(), Some("<file>"));
        assert_eq!(
            map_tool_call("bash", &json!({"command": "cargo check"})),
            ToolCall::Exec {
                command: "cargo check".into()
            }
        );
        assert_eq!(
            map_tool_call("read", &json!({"path": "src/main.rs"})),
            ToolCall::ReadFile {
                path: "src/main.rs".into()
            }
        );
    }

    #[test]
    fn context_tokens_follow_pi_component_fallback() {
        let message = json!({"usage": {
            "input": 33, "output": 27, "cacheRead": 5888, "cacheWrite": 4, "totalTokens": 0
        }});
        assert_eq!(message_context_tokens(&message), Some(5952));
    }

    #[test]
    fn edit_tool_supports_edits_array_and_diff() {
        let call = map_tool_call(
            "edit",
            &json!({
                "path": "src/lib.rs",
                "edits": [{"oldText": "fn old() {}", "newText": "fn new() {}"}]
            }),
        );
        assert_eq!(
            call,
            ToolCall::EditFile {
                path: "src/lib.rs".into(),
                old_string: Some("fn old() {}".into()),
                new_string: Some("fn new() {}".into()),
            }
        );
        let diff = tool_diff(
            Some(&call),
            Some(&json!({"details": {"diff": "--- a\n+++ b\n"}})),
        );
        assert_eq!(
            diff,
            Some(ToolDiff {
                path: "src/lib.rs".into(),
                old_text: Some("fn old() {}".into()),
                new_text: "--- a\n+++ b\n".into(),
            })
        );
    }

    #[test]
    fn render_value_extracts_structured_content() {
        let structured = json!({
            "content": [{"type": "text", "text": "cleaned tool output"}],
            "details": {"exitCode": 0}
        });
        assert_eq!(
            render_value(&structured).as_deref(),
            Some("cleaned tool output")
        );
    }

    #[test]
    fn terminal_settle_distinguishes_standard_pi_and_oh_my_pi() {
        assert!(is_terminal_settle("agent_settled", &json!({})));
        assert!(!is_terminal_settle(
            "agent_settled",
            &json!({"isTerminal": false})
        ));
        assert!(!is_terminal_settle(
            "agent_end",
            &json!({"messages": [], "willRetry": false})
        ));
        assert!(is_terminal_settle(
            "agent_end",
            &json!({"isTerminal": true})
        ));
    }
}

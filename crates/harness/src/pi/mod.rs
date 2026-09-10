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
    AgentEvent, CommandScope, ContextComponent, ContextComponentKind, DoneStatus, HarnessId, Model,
    ReasoningLevel, RunRequest, SessionOptions, SlashCommand, SteeringMode, TodoItem, ToolCall,
    ToolDiff, UserInputAnswer, UserInputQuestion,
};

use crate::{
    Harness, HarnessError, RunCommand, RunControls, StderrTail, crash_message, shutdown_child,
};

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
            ReasoningLevel::Off,
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

    fn supports_live_options(&self) -> bool {
        true
    }

    fn supports_session_fork(&self) -> bool {
        true
    }

    async fn fork_session(
        &self,
        cwd: &Path,
        session_id: &str,
        turns_to_remove: usize,
    ) -> Result<String, HarnessError> {
        let exe = self.resolve_executable()?;
        fork_native_session(&exe, cwd, Path::new(session_id), turns_to_remove).await
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
        let known_commands = self.commands.get().cloned().unwrap_or_default();
        let receiver = start_session(
            &exe,
            request,
            controls,
            known_commands,
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

/// The engine's input bridge, as [`RunControls::request_input`] hands it to
/// the run: questions in, answers back on a oneshot. Boxed in `RunControls`,
/// `Arc`'d here so each `extension_ui_request` subtask can hold its own.
type RequestInputFn =
    Box<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync>;

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

    async fn request(&self, request: Value) -> Result<Value, HarnessError> {
        self.request_timeout(request, RPC_TIMEOUT).await
    }

    async fn request_timeout(
        &self,
        mut request: Value,
        timeout: Duration,
    ) -> Result<Value, HarnessError> {
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
        let response = match tokio::time::timeout(timeout, rx).await {
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
    // Pi's own picker scopes the catalog to its `enabledModels` setting;
    // mirror that working set, keeping the session default selectable.
    let default = state.as_ref().and_then(pi_state_default_model);
    Ok(filter_pi_scoped_models(
        parsed,
        &pi_enabled_model_patterns(),
        default.as_deref(),
    ))
}

async fn discover_commands(exe: &Path) -> Result<Vec<SlashCommand>, HarnessError> {
    let (mut child, rpc, _incoming) = start_probe(exe, false).await?;
    let response = rpc.request(json!({"type": "get_commands"})).await;
    shutdown_child(&mut child, KILL_GRACE).await;
    response.map(|value| parse_pi_commands(&value))
}

fn parse_pi_models(response: &Value, state: Option<&Value>) -> Vec<Model> {
    let default = state.and_then(pi_state_default_model);
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
        ("off", ReasoningLevel::Off),
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
            // The extended levels only exist when the provider map names them.
            value.is_some_and(|value| !value.is_null())
        } else {
            // Base levels (including `off`) ride unless explicitly disabled.
            value.is_none_or(|value| !value.is_null())
        }
    })
    .map(|(_, level)| level)
    .collect()
}

/// Pi's thinking-level ladder in the order its picker offers it. `off`
/// bypasses the provider effort mapping but is always accepted.
const PI_THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The `provider/model` slug [`get_state`](crate) reports as the session's model.
fn pi_state_default_model(state: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        state.pointer("/data/model/provider")?.as_str()?,
        state.pointer("/data/model/id")?.as_str()?
    ))
}

/// The agent directory Pi resolves its own settings from:
/// `PI_CODING_AGENT_DIR` when set (with `~` expansion), else `~/.pi/agent`.
fn pi_agent_directory() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let configured = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    pi_agent_directory_for(configured, home)
}

fn pi_agent_directory_for(configured: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    match configured {
        Some(path) => {
            if path == Path::new("~") {
                return home;
            }
            match (home, path.strip_prefix("~")) {
                (Some(home), Ok(suffix)) => Some(home.join(suffix)),
                _ => Some(path),
            }
        }
        None => home.map(|home| home.join(".pi").join("agent")),
    }
}

/// Pi's `enabledModels` setting — the working set its own picker offers. An
/// absent, empty, or unreadable list means Pi scopes nothing, so every
/// available model stays selectable.
fn pi_enabled_model_patterns() -> Vec<String> {
    let Some(agent) = pi_agent_directory() else {
        return Vec::new();
    };
    pi_enabled_model_patterns_at(&agent.join("settings.json"))
}

fn pi_enabled_model_patterns_at(path: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    value
        .get("enabledModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Narrows a discovered Pi catalog to the models Pi itself would offer. A
/// pattern set that resolves to nothing falls back to the full catalog — what
/// Pi's picker does when none of its configured patterns match — and the
/// session's default stays selectable inside any non-empty scope.
fn filter_pi_scoped_models(
    models: Vec<Model>,
    patterns: &[String],
    default: Option<&str>,
) -> Vec<Model> {
    if patterns.is_empty() {
        return models;
    }
    let mut scoped: std::collections::HashSet<String> = std::collections::HashSet::new();
    for pattern in patterns {
        scoped.extend(
            resolve_pi_pattern(&models, pattern)
                .into_iter()
                .map(str::to_owned),
        );
    }
    if scoped.is_empty() {
        return models;
    }
    scoped.extend(default.map(str::to_owned));
    models
        .into_iter()
        .filter(|model| scoped.contains(model.id.as_str()))
        .collect()
}

/// The model ids a single `enabledModels` pattern selects, following Pi's
/// resolver: an exact `provider/model` reference wins; a pattern carrying glob
/// syntax matches every slug it fits; a plain pattern falls back to Pi's
/// substring match on the id or display name.
fn resolve_pi_pattern<'a>(models: &'a [Model], pattern: &str) -> Vec<&'a str> {
    let pattern = strip_pi_thinking_suffix(pattern);
    let exact = exact_pi_model(models, pattern);
    if is_glob_pattern(pattern) {
        return match exact {
            Some(model) => vec![model.id.as_str()],
            None => models
                .iter()
                .filter(|model| {
                    let bare = split_pi_slug(&model.id)
                        .map(|(_, id)| id)
                        .unwrap_or(&model.id);
                    glob_matches(pattern, &model.id) || glob_matches(pattern, bare)
                })
                .map(|model| model.id.as_str())
                .collect(),
        };
    }
    match exact.or_else(|| fuzzy_pi_model(models, pattern)) {
        Some(model) => vec![model.id.as_str()],
        None => Vec::new(),
    }
}

/// `provider` and the bare model id behind a `provider/model` slug. Pi provider
/// ids never contain `/`, so the first separator is the split.
fn split_pi_slug(slug: &str) -> Option<(&str, &str)> {
    let (provider, id) = slug.split_once('/')?;
    (!provider.is_empty() && !id.is_empty()).then_some((provider, id))
}

/// Pi accepts a trailing `:<thinking level>` on any pattern; it selects a
/// thinking level rather than a different model.
fn strip_pi_thinking_suffix(pattern: &str) -> &str {
    match pattern.rsplit_once(':') {
        Some((prefix, suffix)) if PI_THINKING_LEVELS.contains(&suffix) => prefix,
        _ => pattern,
    }
}

fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// The model a bare id or canonical `provider/model` reference names, rejecting
/// ambiguous matches the way Pi does.
fn exact_pi_model<'a>(models: &'a [Model], pattern: &str) -> Option<&'a Model> {
    let pattern = pattern.to_lowercase();
    let mut slugs = models
        .iter()
        .filter(|model| model.id.to_lowercase() == pattern);
    match (slugs.next(), slugs.next()) {
        (Some(model), None) => return Some(model),
        (_, Some(_)) => return None,
        (None, None) => {}
    }
    let mut ids = models.iter().filter(|model| {
        split_pi_slug(&model.id).is_some_and(|(_, id)| id.to_lowercase() == pattern)
    });
    match (ids.next(), ids.next()) {
        (Some(model), None) => Some(model),
        _ => None,
    }
}

/// Pi's substring fallback, preferring an alias over a dated build and then
/// the highest-sorting id.
fn fuzzy_pi_model<'a>(models: &'a [Model], pattern: &str) -> Option<&'a Model> {
    let pattern = pattern.to_lowercase();
    let matches: Vec<&Model> = models
        .iter()
        .filter(|model| {
            model.id.to_lowercase().contains(&pattern)
                || model.label.to_lowercase().contains(&pattern)
        })
        .collect();
    let aliases: Vec<&Model> = matches
        .iter()
        .copied()
        .filter(|model| pi_model_is_alias(&model.id))
        .collect();
    let candidates = if aliases.is_empty() { matches } else { aliases };
    candidates
        .into_iter()
        .max_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()))
}

/// A dated build ends in `-YYYYMMDD`; everything else, plus `-latest`, is an
/// alias.
fn pi_model_is_alias(id: &str) -> bool {
    let Some((_, digits)) = id.rsplit_once('-') else {
        return true;
    };
    !(digits.len() == 8 && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Case-insensitive glob with minimatch's shape: `*` and `?` stay inside one
/// path segment, `**` may cross `/`, and `[...]` is a character class. Pi
/// resolves model patterns with minimatch (`nocase: true`); syntax beyond this
/// simply matches nothing, and an unmatched pattern set falls back to the full
/// catalog.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    glob_matches_at(&pattern, &text)
}

fn glob_matches_at(pattern: &[char], text: &[char]) -> bool {
    let Some((&head, rest)) = pattern.split_first() else {
        return text.is_empty();
    };
    match head {
        '*' => {
            let stars = pattern.iter().take_while(|char| **char == '*').count();
            let rest = &pattern[stars..];
            let crosses_segments = stars >= 2;
            // `**/` also matches no directory at all.
            if crosses_segments && rest.first() == Some(&'/') && glob_matches_at(&rest[1..], text) {
                return true;
            }
            (0..=text.len()).any(|consumed| {
                (crosses_segments || !text[..consumed].contains(&'/'))
                    && glob_matches_at(rest, &text[consumed..])
            })
        }
        '?' => !text.is_empty() && text[0] != '/' && glob_matches_at(rest, &text[1..]),
        '[' => match parse_glob_class(rest) {
            Some((class, consumed)) => {
                !text.is_empty()
                    && text[0] != '/'
                    && class.contains(text[0])
                    && glob_matches_at(&rest[consumed..], &text[1..])
            }
            // An unterminated class is a literal `[`, as in minimatch.
            None => text.first() == Some(&'[') && glob_matches_at(rest, &text[1..]),
        },
        literal => text.first() == Some(&literal) && glob_matches_at(rest, &text[1..]),
    }
}

struct GlobClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl GlobClass {
    fn contains(&self, value: char) -> bool {
        let hit = self
            .ranges
            .iter()
            .any(|(low, high)| (*low..=*high).contains(&value));
        hit != self.negated
    }
}

/// Parses a class body past its `[` and reports how many characters it spans,
/// including the closing `]`. `None` means there is no usable class.
fn parse_glob_class(pattern: &[char]) -> Option<(GlobClass, usize)> {
    let negated = matches!(pattern.first(), Some('!') | Some('^'));
    let mut index = usize::from(negated);
    let start = index;
    let mut ranges = Vec::new();
    while index < pattern.len() && pattern[index] != ']' {
        let low = pattern[index];
        let is_range = pattern.get(index + 1) == Some(&'-')
            && pattern.get(index + 2).is_some_and(|high| *high != ']');
        if is_range {
            ranges.push((low, pattern[index + 2]));
            index += 3;
        } else {
            ranges.push((low, low));
            index += 1;
        }
    }
    if index >= pattern.len() || index == start {
        return None;
    }
    Some((GlobClass { negated, ranges }, index + 1))
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
            let source = command.get("source").and_then(Value::as_str);
            let raw = command.get("name")?.as_str()?.trim();
            if raw.is_empty() {
                return None;
            }
            let name = if source == Some("skill") {
                raw.strip_prefix("skill:").unwrap_or(raw).to_owned()
            } else {
                raw.to_owned()
            };
            let scope = match source {
                Some("skill") => CommandScope::Skill,
                // Pi reports prompt templates with a scope of their own.
                Some("prompt") => {
                    match command.pointer("/sourceInfo/scope").and_then(Value::as_str) {
                        Some("project") => CommandScope::Project,
                        _ => CommandScope::User,
                    }
                }
                _ => CommandScope::Builtin,
            };
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
                scope,
            })
        })
        .collect()
}

fn reasoning_name(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "off",
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

// ---------------------------------------------------------------------------
// Slash-command resolution: prompt templates and skills
// ---------------------------------------------------------------------------

/// Prompt files contribute `name → body` (namespaced `a/b.md` → `a:b`) from
/// the project then the user dir, so project-local files shadow user ones.
fn discover_prompt_templates(cwd: &Path) -> HashMap<String, String> {
    const SCAN_CAP: usize = 500;
    const FILE_MAX_BYTES: u64 = 64 * 1024;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = vec![cwd.join(".pi/prompts")];
    if let Some(home) = home.as_deref() {
        roots.push(home.join(".pi/agent/prompts"));
    }
    let mut templates = HashMap::new();
    // User scope first so the project scan below wins equal names.
    for root in roots.into_iter().rev() {
        let mut stack = vec![(root, Vec::<String>::new())];
        while let Some((dir, namespace)) = stack.pop() {
            if namespace.len() > 3 || templates.len() >= SCAN_CAP {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if templates.len() >= SCAN_CAP {
                    break;
                }
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                // `Path::is_dir` follows symlinks — prompt trees are routinely
                // symlinked out of dotfile repos.
                if path.is_dir() {
                    let mut namespace = namespace.clone();
                    namespace.push(name.to_owned());
                    stack.push((path, namespace));
                    continue;
                }
                let Some(stem) = name.strip_suffix(".md") else {
                    continue;
                };
                if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > FILE_MAX_BYTES) {
                    continue;
                }
                let Ok(contents) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let name = namespace
                    .iter()
                    .map(String::as_str)
                    .chain([stem])
                    .collect::<Vec<_>>()
                    .join(":");
                templates.insert(name, strip_frontmatter(&contents).trim().to_owned());
            }
        }
    }
    templates
}

/// The skill names invocable as `/name`: one directory per skill holding a
/// `SKILL.md`, named by its frontmatter `name` when present.
fn discover_skill_names(cwd: &Path) -> std::collections::HashSet<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = vec![cwd.join(".pi/skills")];
    if let Some(home) = home.as_deref() {
        roots.push(home.join(".pi/agent/skills"));
    }
    let mut names = std::collections::HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            // Follows symlinks — skills are routinely linked into place.
            if !entry.path().is_dir() {
                continue;
            }
            let Some(dir_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if dir_name.starts_with('.') {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(entry.path().join("SKILL.md")) else {
                continue;
            };
            names.insert(frontmatter_name(&contents).unwrap_or(dir_name));
        }
    }
    names
}

/// The body after a leading `---` YAML-lite frontmatter block. Line-based on
/// purpose: command metadata is `key: value` pairs, and an unparsable block
/// must still leave the prompt usable, not eat it.
fn strip_frontmatter(contents: &str) -> &str {
    let Some(rest) = contents.strip_prefix("---") else {
        return contents;
    };
    let Some((_, body)) = rest.split_once("\n---") else {
        return contents;
    };
    body.trim_start()
}

/// The `name:` frontmatter field of a SKILL.md, when declared.
fn frontmatter_name(contents: &str) -> Option<String> {
    let rest = contents.strip_prefix("---")?;
    let (block, _) = rest.split_once("\n---")?;
    block.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == "name")
            .then(|| value.trim().trim_matches(['"', '\'']).to_owned())
            .filter(|name| !name.is_empty())
    })
}

/// Fill a prompt template's placeholders: `$ARGUMENTS` and `$@` get the whole
/// argument string, `$1..$9` their word, and a template that consumes nothing
/// gets the arguments appended — the provider's own expansion rules.
fn expand_command_template(template: &str, args: &str) -> String {
    let mut consumed = false;
    let mut out = String::with_capacity(template.len() + args.len());
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let mut word = String::new();
        while let Some(&next) = chars.peek() {
            if next.is_ascii_alphanumeric() || next == '@' || next == '_' {
                word.push(next);
                chars.next();
            } else {
                break;
            }
        }
        match word.as_str() {
            "ARGUMENTS" | "@" => {
                out.push_str(args);
                consumed = true;
            }
            digit
                if digit.len() == 1 && digit.bytes().next().is_some_and(|b| b.is_ascii_digit()) =>
            {
                let index = digit.parse::<usize>().unwrap_or(0);
                if index >= 1 {
                    if let Some(arg) = args.split_whitespace().nth(index - 1) {
                        out.push_str(arg);
                    }
                    consumed = true;
                }
            }
            _ => {
                out.push('$');
                out.push_str(&word);
            }
        }
    }
    if args.is_empty() {
        return out.trim_end().to_owned();
    }
    if consumed {
        out.trim_end().to_owned()
    } else {
        format!("{}\n\n{args}", out.trim_end())
    }
}

/// The wire text for a typed `/name args` submission. Skills go through Pi's
/// native `/skill:` invocation; a prompt template expands to its body;
/// everything else — built-ins included — passes through untouched.
fn resolve_prompt_submission(
    prompt: &str,
    skills: &std::collections::HashSet<String>,
    templates: &HashMap<String, String>,
) -> String {
    let Some(rest) = prompt.strip_prefix('/') else {
        return prompt.to_owned();
    };
    if rest.is_empty() || rest.starts_with([' ', '/']) {
        return prompt.to_owned();
    }
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(split) => (&rest[..split], rest[split..].trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return prompt.to_owned();
    }
    if let Some(name) = name.strip_prefix("skill:") {
        // Already the native spelling — the user typed it verbatim.
        let _ = name;
        return prompt.to_owned();
    }
    if skills.contains(name) {
        return match args.is_empty() {
            true => format!("/skill:{name}"),
            false => format!("/skill:{name} {args}"),
        };
    }
    if let Some(template) = templates.get(name) {
        return expand_command_template(template, args);
    }
    prompt.to_owned()
}

/// The skill names a reported catalog knows, folded into the scanned set.
fn skill_names(commands: &[SlashCommand]) -> std::collections::HashSet<String> {
    commands
        .iter()
        .filter(|command| command.scope == CommandScope::Skill)
        .map(|command| command.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Session fork / rewind
// ---------------------------------------------------------------------------

/// Fork the native session at `session_file` so the copy drops its last
/// `turns_to_remove` user turns (0 = whole-session `clone`). Runs a throwaway
/// RPC process; any live run on the session must already be settled.
async fn fork_native_session(
    exe: &Path,
    cwd: &Path,
    session_file: &Path,
    turns_to_remove: usize,
) -> Result<String, HarnessError> {
    if !session_file.exists() {
        return Err(HarnessError::Protocol(format!(
            "pi session file is missing: {}",
            session_file.display()
        )));
    }
    let mut command = Command::new(exe);
    crate::compose_child_path(&mut command, exe);
    command
        .args(["--mode", "rpc", "--approve"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env("PI_SKIP_VERSION_CHECK", "1");
    let mut child = command.spawn().map_err(HarnessError::Io)?;
    let result = async {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi has no stdout".into()))?;
        let (rpc, _incoming) = PiRpcClient::new(stdin, stdout);
        let switched = rpc
            .request(json!({
                "type": "switch_session",
                "sessionPath": session_file.to_string_lossy(),
            }))
            .await?;
        if switched.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(HarnessError::Protocol(
                "pi session switch was cancelled".into(),
            ));
        }
        let messages = rpc.request(json!({"type": "get_fork_messages"})).await?;
        let messages = messages
            .pointer("/data/messages")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                HarnessError::Protocol("pi returned an invalid fork-message list".into())
            })?;
        if turns_to_remove > messages.len() {
            return Err(HarnessError::Protocol(format!(
                "pi has only {} native turns, but the rewind needs to remove {turns_to_remove}",
                messages.len()
            )));
        }
        let request = if turns_to_remove == 0 {
            json!({"type": "clone"})
        } else {
            let entry_id = messages
                .get(messages.len() - turns_to_remove)
                .and_then(|message| message.get("entryId"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    HarnessError::Protocol("pi returned a fork message without an entry ID".into())
                })?;
            json!({"type": "fork", "entryId": entry_id})
        };
        let fork = rpc.request(request).await?;
        if fork.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(HarnessError::Protocol(
                "pi session fork was cancelled".into(),
            ));
        }
        let state = rpc.request(json!({"type": "get_state"})).await?;
        state
            .pointer("/data/sessionFile")
            .and_then(Value::as_str)
            .or_else(|| state.pointer("/data/sessionId").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| HarnessError::Protocol("pi did not report the forked session".into()))
    }
    .await;
    shutdown_child(&mut child, KILL_GRACE).await;
    result
}

async fn start_session(
    exe: &Path,
    request: RunRequest,
    mut controls: RunControls,
    known_commands: Vec<SlashCommand>,
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
    // Always install the managed adapter, including when bridge startup
    // failed. Otherwise the legacy global tool could silently take over.
    let extension = install_bundled_extension("noches-cua.ts", CUA_EXTENSION).ok_or_else(|| {
        HarnessError::Protocol("cannot install the managed computer-use adapter".into())
    })?;
    command
        .env_remove("NOCHES_CUA_SOCKET")
        .arg("-e")
        .arg(extension);
    if let Some(socket) = controls.computer_use_socket.take() {
        command.env("NOCHES_CUA_SOCKET", socket);
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
    if let Some(reasoning) = request.reasoning
        && let Err(err) = rpc
            .request(json!({
                "type": "set_thinking_level",
                "level": reasoning_name(reasoning),
            }))
            .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return Err(err);
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
    let mut skills = discover_skill_names(Path::new(&request.cwd));
    skills.extend(skill_names(&known_commands));
    skills.extend(skill_names(&commands));
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
    // Slash resolution state: templates come from Pi's own files;
    // skills from the scanned dirs plus whatever catalog the session (or the
    // earlier probe) reported — a skill the running agent knows always wins.
    let templates = discover_prompt_templates(Path::new(&request.cwd));
    let prompt_text = resolve_prompt_submission(&request.prompt, &skills, &templates);
    let prompt_id = match rpc.send(json!({"type": "prompt", "message": prompt_text})) {
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
        skills,
        templates,
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
    skills: std::collections::HashSet<String>,
    templates: HashMap<String, String>,
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
        mut skills,
        templates,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
        computer_use_socket: _,
    } = controls;
    let request_input: Arc<RequestInputFn> = Arc::new(request_input);
    let mut steering_open = true;
    let mut active_turn = true;
    let mut done_emitted = false;
    let mut failed = false;
    let mut saw_text = false;
    let mut saw_reasoning = false;
    let mut tools: HashMap<String, ToolCall> = HashMap::new();
    let mut last_tool_results: HashMap<String, (Option<String>, Option<ToolDiff>, bool)> =
        HashMap::new();
    // The session name the agent last reported: a title only crosses the
    // wire when it actually renamed the session to something new.
    let mut last_title: Option<String> = None;

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
                        } else if value.get("success").and_then(Value::as_bool) == Some(false)
                            && (active_turn || !done_emitted) {
                                let _ = send_event(&event_tx, AgentEvent::Error { message: pi_error_message(&value) }).await;
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
                            for event in handle_tool_update(&mut tools, &mut last_tool_results, &value) {
                                let _ = send_event(&event_tx, event).await;
                            }
                        }
                        "tool_execution_end" => {
                            for event in handle_tool_end(&mut tools, &mut last_tool_results, &value) {
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
                            let parsed = parse_pi_commands(&value);
                            skills.extend(skill_names(&parsed));
                            let _ = send_event(&event_tx, AgentEvent::AvailableCommands { commands: parsed }).await;
                        }
                        "session_info_changed" => {
                            if let Some(name) = session_info_name(&value)
                                && last_title.as_deref() != Some(name.as_str())
                            {
                                last_title = Some(name.clone());
                                let _ = send_event(&event_tx, AgentEvent::TitleUpdated { title: name }).await;
                            }
                        }
                        "extension_ui_request" => {
                            // A notify with warning/error weight is the one
                            // fire-and-forget worth surfacing — the extension
                            // meant the user to see it. Info/status/widget
                            // lines have no Comet surface; dropping is the
                            // honest no-op (Pi no-ops them in RPC mode too).
                            if is_error_notify(&value) {
                                let message = value
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("extension notification")
                                    .to_owned();
                                let _ = send_event(&event_tx, AgentEvent::Error { message }).await;
                            }
                            handle_extension_ui_request(&value, &rpc, &request_input);
                        }
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
                Some(RunCommand::Options(options)) => {
                    apply_session_options(&rpc, &event_tx, options).await;
                }
                Some(RunCommand::Steer(message)) => {
                    let next = new_message_id();
                    let _ = send_event(&event_tx, AgentEvent::Steered {
                        assistant_message_id: Some(assistant_id.clone()),
                        next_assistant_message_id: Some(next.clone()),
                    }).await;
                    assistant_id = next;
                    if active_turn {
                        let prompt = resolve_prompt_submission(&message.prompt, &skills, &templates);
                        if let Err(err) = rpc.send(json!({"type": "steer", "message": prompt})) {
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
                        let prompt = resolve_prompt_submission(&message.prompt, &skills, &templates);
                        match rpc.send(json!({"type": "prompt", "message": prompt})) {
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
                                                for event in handle_tool_update(&mut tools, &mut last_tool_results, &value) {
                                                    let _ = send_event(&event_tx, event).await;
                                                }
                                            }
                                            "tool_execution_end" => {
                                                for event in handle_tool_end(&mut tools, &mut last_tool_results, &value) {
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

/// Apply a mid-run options change to the resident session — the whole point
/// of `supports_live_options`: model and effort land in place rather than
/// costing the chat a process restart. `set_model` also moves the context
/// window, so publish the fresh one for the usage card.
async fn apply_session_options(
    rpc: &PiRpcClient,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    options: SessionOptions,
) {
    let mut window = None;
    if let Some(model) = options.model.as_deref() {
        match model_parts(model) {
            Ok((provider, model_id)) => {
                match rpc
                    .request(json!({
                        "type": "set_model",
                        "provider": provider,
                        "modelId": model_id,
                    }))
                    .await
                {
                    Ok(response) => {
                        window = response
                            .pointer("/data/model/contextWindow")
                            .and_then(Value::as_u64);
                    }
                    Err(err) => {
                        let _ = send_event(
                            event_tx,
                            AgentEvent::Error {
                                message: format!("Pi could not switch models: {err}"),
                            },
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                let _ = send_event(
                    event_tx,
                    AgentEvent::Error {
                        message: err.to_string(),
                    },
                )
                .await;
            }
        }
    }
    if let Some(level) = options.reasoning
        && let Err(err) = rpc
            .request(json!({
                "type": "set_thinking_level",
                "level": reasoning_name(level),
            }))
            .await
    {
        let _ = send_event(
            event_tx,
            AgentEvent::Error {
                message: format!("Pi could not switch thinking level: {err}"),
            },
        )
        .await;
    }
    if window.is_none() {
        window = rpc
            .request(json!({"type": "get_state"}))
            .await
            .ok()
            .and_then(|state| {
                state
                    .pointer("/data/model/contextWindow")
                    .and_then(Value::as_u64)
            });
    }
    if let Some(window) = window.filter(|window| *window > 0) {
        let _ = send_event(
            event_tx,
            AgentEvent::ContextUsage {
                tokens: None,
                window: Some(window),
                components: Vec::new(),
            },
        )
        .await;
    }
}

/// The session name a `session_info_changed` event carries — the live twin of
/// the session-file `session_info` entry's `name` field. The nested spellings
/// are accepted so a payload shape change degrades to no-op, never a wrong or
/// empty rename.
fn session_info_name(value: &Value) -> Option<String> {
    ["/name", "/sessionInfo/name", "/data/name", "/title"]
        .into_iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Pi's todo tool is CRUD-shaped: `tool_execution_start` args carry only
/// `{action, id, subject, status, …}` — never the task list — so the chip
/// opens empty. The authoritative full list arrives on the end frame's
/// `result.details.tasks`. Re-emitting `ToolCall` for the same id refreshes
/// the chip in place (the doc fold rewrites an in-segment `call`), so the
/// todo row settles showing the real list rather than `0/0 done`.
fn handle_tool_update(
    tools: &mut HashMap<String, ToolCall>,
    last_tool_results: &mut HashMap<String, (Option<String>, Option<ToolDiff>, bool)>,
    value: &Value,
) -> Vec<AgentEvent> {
    let id = value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let partial = value.get("partialResult").or_else(|| value.get("result"));
    let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
    let mut events = Vec::new();
    // A todo partial result may already carry the growing task list —
    // refresh the chip the same way the end frame does.
    if matches!(tools.get(&id), Some(ToolCall::Todo { .. }))
        && let Some(items) = todo_items_from_result(partial)
        && let Some(ToolCall::Todo { items: slot }) = tools.get_mut(&id)
        && *slot != items
    {
        *slot = items.clone();
        events.push(AgentEvent::ToolCall {
            id: id.clone(),
            call: ToolCall::Todo { items },
        });
    }
    let call = tools.get(&id);
    let diff = tool_diff(call, partial);
    let output = partial
        .and_then(render_value)
        .filter(|text| !text.is_empty());
    if output.is_some() || diff.is_some() {
        let entry = (output.clone(), diff.clone(), is_error);
        if last_tool_results.get(&id) != Some(&entry) {
            last_tool_results.insert(id.clone(), entry);
            events.push(AgentEvent::ToolResult {
                id,
                is_error,
                output,
                diff,
            });
        }
    }
    events
}

fn handle_tool_end(
    tools: &mut HashMap<String, ToolCall>,
    last_tool_results: &mut HashMap<String, (Option<String>, Option<ToolDiff>, bool)>,
    value: &Value,
) -> Vec<AgentEvent> {
    let id = value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let call = tools.remove(&id);
    let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
    let result = value.get("result");
    let mut events = Vec::new();
    // Resolve the todo list off the end frame and refresh the chip before
    // its result lands, so the settled row reads `N/M done`.
    let call = match (call, todo_items_from_result(result)) {
        (Some(ToolCall::Todo { .. }), Some(items)) => {
            let call = ToolCall::Todo { items };
            events.push(AgentEvent::ToolCall {
                id: id.clone(),
                call: call.clone(),
            });
            Some(call)
        }
        (call, _) => call,
    };
    let diff = tool_diff(call.as_ref(), result);
    let output = result
        .and_then(render_value)
        .filter(|text| !text.is_empty());
    let prev = last_tool_results.remove(&id);
    let entry = (output, diff, is_error);
    let changed = match &prev {
        Some(prev) => *prev != entry,
        None => true,
    };
    if changed {
        let (output, diff, is_error) = entry;
        events.push(AgentEvent::ToolResult {
            id,
            is_error,
            output,
            diff,
        });
    }
    events
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

// ---------------------------------------------------------------------------
// Extension UI bridge
// ---------------------------------------------------------------------------
//
// In RPC mode Pi translates an extension's `ctx.ui.*` call into an
// `extension_ui_request` frame on stdout. Dialog methods (`select`,
// `confirm`, `input`, `editor`) block the agent until a matching
// `extension_ui_response` arrives on stdin; the fire-and-forget methods
// (`notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`) expect
// none. Comet owns a real question surface through the engine's input bridge
// — the same `InputRequested`/`InputResolved` lifecycle Claude's
// `AskUserQuestion` drives — so dialogs route there instead of being
// auto-cancelled, and a Comet answer maps back onto the wire shape the
// method expects.

/// Serve one `extension_ui_request`. Dialog methods spawn a subtask that
/// awaits the user's answers through `request_input` and replies on Pi's
/// stdin; the session loop keeps streaming meanwhile (a blocked select must
/// not stall the transcript, and a parked run must still interrupt).
/// Fire-and-forget methods need no response and are dropped.
fn handle_extension_ui_request(
    value: &Value,
    rpc: &PiRpcClient,
    request_input: &Arc<RequestInputFn>,
) {
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return;
    };
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let id = id.to_owned();
    match method {
        "select" | "confirm" | "input" | "editor" => {
            let rpc = rpc.clone();
            let request_input = Arc::clone(request_input);
            let request = value.clone();
            let method = method.to_owned();
            tokio::spawn(async move {
                let response = match extension_dialog(&request, &method, request_input).await {
                    Some(response) => response,
                    // The bridge dropped (run torn down): cancel so the agent
                    // unblocks instead of wedging on a dialog nobody can see.
                    None => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
                };
                rpc.send_unidentified(response);
            });
        }
        // notify/setStatus/setWidget/setTitle/set_editor_text: display-only,
        // no response expected — the agent doesn't park on them. The status
        // lines have no dedicated Comet surface yet; dropping is the honest
        // no-op (Pi itself no-ops them when it owns the terminal).
        _ => {}
    }
}

/// Await the Comet answer to a dialog request and shape the
/// `extension_ui_response` the method expects. `None` means the bridge went
/// away; `Some(cancelled)` means the user dismissed.
async fn extension_dialog(
    request: &Value,
    method: &str,
    request_input: Arc<RequestInputFn>,
) -> Option<Value> {
    let question = extension_question(request, method);
    let answers = (request_input)(vec![question]).await.ok()?;
    let id = request.get("id").and_then(Value::as_str)?.to_owned();
    // The wizard emits one answer per question; a dialog is a single page.
    // Empty labels are a dismissal (Esc or submit-without-a-pick), never a
    // real selection — cancel so the extension sees `undefined`/`false`,
    // not a phantom empty choice.
    let labels = answers
        .into_iter()
        .next()
        .map(|answer| answer.labels)
        .unwrap_or_default();
    let picked = labels.into_iter().next().filter(|label| !label.is_empty());
    let response = match method {
        // A confirm is boolean: only an explicit affirmative confirms true
        // and an explicit negative confirms false. A typed free-text answer
        // that is neither is ambiguous — cancel rather than guess a boolean
        // the user never picked.
        "confirm" => match picked.as_deref() {
            Some(label) if is_affirmative_label(label) => {
                json!({"type": "extension_ui_response", "id": id, "confirmed": true})
            }
            Some(label) if is_negative_label(label) => {
                json!({"type": "extension_ui_response", "id": id, "confirmed": false})
            }
            _ => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        },
        // select / input / editor all answer on `value`; the typed free-text
        // row already lands in labels[0] the same as a picked option does.
        _ => match picked {
            Some(value) => {
                json!({"type": "extension_ui_response", "id": id, "value": value})
            }
            None => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        },
    };
    Some(response)
}

/// Build the single `UserInputQuestion` a dialog becomes. `select` lists its
/// options; `confirm` becomes a yes/no pick; `input`/`editor` carry no
/// options so the wizard's free-text row is the whole answer. `header` is
/// the dialog's method/title context, `question` its prompt text.
fn extension_question(request: &Value, method: &str) -> UserInputQuestion {
    let title = request
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let message = request
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let question_text = match (title, message) {
        (Some(title), Some(message)) => format!("{title}\n\n{message}"),
        (Some(title), None) => title.to_owned(),
        (None, Some(message)) => message.to_owned(),
        (None, None) => method.to_owned(),
    };
    let options = match method {
        "select" => request
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| {
                option
                    .as_str()
                    .or_else(|| {
                        option
                            .get("label")
                            .or_else(|| option.get("value"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_owned)
            })
            .collect(),
        "confirm" => vec!["Yes".to_owned(), "No".to_owned()],
        _ => Vec::new(),
    };
    UserInputQuestion {
        id: uuid::Uuid::new_v4().to_string(),
        header: title.unwrap_or(method).to_owned(),
        question: question_text,
        options,
        multi_select: false,
    }
}

/// Whether a fire-and-forget request is a `notify` the user should see —
/// `warning`/`error` only; routine `info` notifications stay silent.
fn is_error_notify(value: &Value) -> bool {
    value.get("method").and_then(Value::as_str) == Some("notify")
        && matches!(
            value.get("notifyType").and_then(Value::as_str),
            Some("warning") | Some("error")
        )
}

/// Whether a confirm-dialog pick reads as the affirmative. Kept narrow and
/// explicit so a stray typed answer can't be mistaken for consent.
fn is_affirmative_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "yes" | "y" | "allow" | "approve" | "ok" | "confirm" | "accept" | "proceed" | "continue"
    )
}

/// Whether a confirm-dialog pick reads as the negative.
fn is_negative_label(label: &str) -> bool {
    matches!(
        label.trim().to_ascii_lowercase().as_str(),
        "no" | "n" | "deny" | "denied" | "block" | "reject" | "cancel" | "decline" | "disallow"
    )
}

/// Companion extension bundled with this build: it measures what each turn
/// sends to the model and appends the breakdown to the session file as a
/// custom entry, which we read back to fill the usage card.
const CONTEXT_EXTENSION: &str = include_str!("zeron-context.ts");
/// Managed computer-use adapter. A missing socket disables computer use;
/// it never falls back to a user-installed desktop tool.
const CUA_EXTENSION: &str = include_str!("noches-cua.ts");
const CONTEXT_ENTRY_TYPE: &str = "zeron:context-usage";
/// Entries land at the file tail every turn, so a bounded tail read is enough
/// even for very long sessions.
const CONTEXT_TAIL_BYTES: u64 = 256 * 1024;

/// The variant-correct data dir root for bundled-extension state. Honors
/// `ZERON_DATA_DIR` first (explicit override, what apps/zeron reads), then the
/// dev build's `~/.zeron-dev`, then production `~/.zeron` — so a dev engine
/// never writes a prod-owned path.
pub(crate) fn zeron_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ZERON_DATA_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    let dir = if cfg!(feature = "dev") {
        ".zeron-dev"
    } else {
        ".zeron"
    };
    Some(PathBuf::from(home).join(dir))
}

/// Install a bundled extension under the zeron-owned prefix, refreshing the
/// file only when the bundled source changed. Best-effort: `None` simply
/// means the run proceeds without it.
fn install_bundled_extension(name: &str, source: &str) -> Option<PathBuf> {
    let dir = zeron_data_dir()?.join("pi").join("extensions");
    let path = dir.join(name);
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == source) {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let staging = dir.join(format!("{}.new-{}", name, uuid::Uuid::new_v4()));
    std::fs::write(&staging, source).ok()?;
    if std::fs::rename(&staging, &path).is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    Some(path)
}

fn install_context_extension() -> Option<PathBuf> {
    install_bundled_extension("zeron-context.ts", CONTEXT_EXTENSION)
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
            // CRUD args carry no list — seed the chip with the task being
            // touched so the row never reads `0/0 done` mid-flight; the end
            // frame's `details.tasks` replaces it with the real list.
            items: todo_items_from(args)
                .into_iter()
                .chain(todo_seed_from_crud(args))
                .collect(),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input: (!args.is_null()).then(|| args.clone()),
        },
    }
}

/// Decode a todo list out of whatever shape the value carries. Pi's todo
/// tool is CRUD-shaped (`{action, id, subject, status, …}`) rather than a
/// whole-list write like Claude's `TodoWrite`: the call args never hold the
/// list, but `tool_execution_end`'s `result.details.tasks` carries the FULL
/// task list every call. A whole-list shape (`items`/`todos`, or a task
/// array directly) is also accepted so the same decoder serves both the
/// in-flight args and any list-carrying payload.
fn todo_items_from(value: &Value) -> Vec<TodoItem> {
    // Whole-list shapes first: an explicit array wins over the CRUD fields.
    let list = value
        .get("items")
        .or_else(|| value.get("todos"))
        .or_else(|| value.pointer("/details/tasks"))
        .or_else(|| value.get("tasks"))
        .and_then(Value::as_array);
    if let Some(list) = list {
        return list.iter().map(todo_item).collect();
    }
    if let Some(array) = value.as_array() {
        return array.iter().map(todo_item).collect();
    }
    Vec::new()
}

/// One todo row. `done` reads the explicit boolean when present, else the
/// Pi task `status` (`completed`/`cancelled` count as done; `pending` and
/// `in_progress` stay open).
fn todo_item(item: &Value) -> TodoItem {
    let text = string_field(item, &["text", "content", "subject", "title"]);
    let done = item
        .get("done")
        .or_else(|| item.get("completed"))
        .and_then(Value::as_bool)
        .or_else(|| {
            item.get("status").and_then(Value::as_str).map(|status| {
                matches!(
                    status.trim().to_ascii_lowercase().as_str(),
                    "completed" | "complete" | "done" | "cancelled" | "canceled"
                )
            })
        })
        .unwrap_or(false);
    TodoItem { text, done }
}

/// A one-row seed for a CRUD todo call's args (`{action, subject, status}`).
/// `create`/`update` name a `subject`; `list`/`get`/`delete`/`clear` carry
/// none worth showing, so they seed nothing. `Some(vec)` only when there's
/// a real subject to render.
fn todo_seed_from_crud(args: &Value) -> Vec<TodoItem> {
    let subject = args
        .get("subject")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|subject| !subject.is_empty());
    match subject {
        Some(text) => vec![todo_item(
            &json!({"subject": text, "status": args.get("status")}),
        )],
        None => Vec::new(),
    }
}

/// The todo items a `tool_execution_update`/`tool_execution_end` result
/// resolves to — `result.details.tasks` is Pi's authoritative full list.
/// Returns `None` when the result carries no task list at all (a non-todo
/// tool, or an error frame), so callers don't blank the chip on noise.
fn todo_items_from_result(result: Option<&Value>) -> Option<Vec<TodoItem>> {
    let result = result?;
    let items = todo_items_from(result);
    (!items.is_empty() || result.pointer("/details/tasks").is_some()).then_some(items)
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
    fn pi_ladder_leads_with_off_and_wires_it_verbatim() {
        assert_eq!(
            PiHarness::new().reasoning_levels(),
            &[
                ReasoningLevel::Off,
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ]
        );
        assert_eq!(reasoning_name(ReasoningLevel::Off), "off");
    }

    #[test]
    fn parse_reasoning_levels_includes_off_unless_the_map_disables_it() {
        use ReasoningLevel::*;
        let levels = |map: Value| {
            parse_reasoning_levels(&json!({"reasoning": true, "thinkingLevelMap": map}))
        };
        // Base levels ride when the map says nothing about them — `off` too.
        assert_eq!(levels(json!({})), vec![Off, Minimal, Low, Medium, High]);
        // A non-null entry keeps the level…
        assert!(levels(json!({"off": "off"})).contains(&Off));
        // …a null entry (the provider cannot do it) drops it.
        assert!(!levels(json!({"off": null})).contains(&Off));
        // xhigh/max keep the requires-explicit-entry rule.
        assert!(!levels(json!({})).contains(&XHigh));
        assert!(levels(json!({"xhigh": "xhigh"})).contains(&XHigh));
    }

    fn catalog_model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        }
    }

    #[test]
    fn pi_scope_narrows_the_catalog_to_the_enabled_models() {
        let models = vec![
            catalog_model("anthropic/claude-opus-5", "Claude Opus 5"),
            catalog_model("cpa/opencode-go/muse-spark-1.3-contributor", "Muse Spark"),
            catalog_model("openai-codex/gpt-5.6-sol", "GPT-5.6 Sol"),
            catalog_model("cpa/gpt-5.6-sol", "GPT 5.6 Sol (CPA)"),
            catalog_model("tokenrouter/z-ai/glm-5.3-free", "GLM 5.3 Free"),
        ];

        let scoped = filter_pi_scoped_models(
            models,
            &[
                "cpa/gpt-5.6-sol".to_owned(),
                "cpa/opencode-go/muse-spark-1.3-contributor".to_owned(),
            ],
            Some("anthropic/claude-opus-5"),
        );

        // Enabled models keep catalog order, and the session default survives
        // a scope that does not name it.
        assert_eq!(
            scoped
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            [
                "anthropic/claude-opus-5",
                "cpa/opencode-go/muse-spark-1.3-contributor",
                "cpa/gpt-5.6-sol"
            ]
        );
    }

    #[test]
    fn pi_scope_patterns_match_globs_bare_ids_and_thinking_suffixes() {
        let models = vec![
            catalog_model("github-copilot/gpt-5.4", "GPT-5.4"),
            catalog_model("openai-codex/gpt-5.6-sol", "GPT-5.6 Sol"),
            catalog_model("cpa/claude-sonnet-4-6-thinking", "Claude Sonnet 4.6"),
            catalog_model("cpa/opencode-go/glm-5.3", "GLM-5.3"),
        ];

        assert_eq!(
            resolve_pi_pattern(&models, "github-copilot/*"),
            ["github-copilot/gpt-5.4"]
        );
        // `*` stays inside a path segment: it reaches a single-segment model id
        // but not a nested one, exactly as minimatch resolves it.
        assert_eq!(
            resolve_pi_pattern(&models, "cpa/*"),
            ["cpa/claude-sonnet-4-6-thinking"]
        );
        assert_eq!(
            resolve_pi_pattern(&models, "cpa/**"),
            ["cpa/claude-sonnet-4-6-thinking", "cpa/opencode-go/glm-5.3"]
        );
        assert_eq!(
            resolve_pi_pattern(&models, "gpt-5.6-sol"),
            ["openai-codex/gpt-5.6-sol"]
        );
        assert_eq!(
            resolve_pi_pattern(&models, "claude-sonnet-4-6-thinking:high"),
            ["cpa/claude-sonnet-4-6-thinking"]
        );
        assert_eq!(
            resolve_pi_pattern(&models, "sonnet"),
            ["cpa/claude-sonnet-4-6-thinking"]
        );
    }

    #[test]
    fn pi_scope_falls_back_to_the_full_catalog_when_nothing_matches() {
        let models = vec![
            catalog_model("anthropic/claude-opus-5", "Claude Opus 5"),
            catalog_model("cpa/gpt-5.6-sol", "GPT 5.6 Sol"),
        ];

        assert_eq!(
            filter_pi_scoped_models(
                models.clone(),
                &["missing/*".to_owned()],
                Some("anthropic/claude-opus-5")
            )
            .len(),
            2
        );
        // No setting at all: unfiltered.
        assert_eq!(filter_pi_scoped_models(models, &[], None).len(), 2);
    }

    #[test]
    fn glob_follows_minimatch_segments_classes_and_case() {
        // Case-insensitive on plain ids.
        assert!(glob_matches("GPT-*", "gpt-5.6-sol"));
        // `*` never crosses `/`.
        assert!(glob_matches("OpenAI/*", "openai/gpt"));
        assert!(!glob_matches("*", "openai/gpt"));
        assert!(glob_matches("**", "openai/gpt"));
        assert!(glob_matches("openai/**", "openai/gpt"));
        // `?` is exactly one char, never `/`.
        assert!(glob_matches("gpt-?.6", "gpt-5.6"));
        assert!(!glob_matches("gpt-?.6", "gpt-56"));
        // Character classes, including negation.
        assert!(glob_matches("gpt-[45].*", "gpt-5.6"));
        assert!(!glob_matches("gpt-[4].*", "gpt-5.6"));
        assert!(glob_matches("gpt-[!4].*", "gpt-5.6"));
        // An unterminated class is a literal `[`.
        assert!(glob_matches("weird[", "weird["));
    }

    #[test]
    fn fuzzy_match_prefers_an_alias_over_a_dated_build() {
        let models = vec![
            catalog_model("cpa/glm-5.3-20260801", "GLM 5.3 (Aug build)"),
            catalog_model("cpa/glm-5.3", "GLM 5.3"),
        ];
        assert_eq!(
            fuzzy_pi_model(&models, "glm").map(|model| model.id.as_str()),
            Some("cpa/glm-5.3")
        );
    }

    #[test]
    fn reads_enabled_models_from_pi_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"defaultProvider":"cpa","enabledModels":[" cpa/gpt-5.6-sol ","","anthropic/*"]}"#,
        )
        .unwrap();

        assert_eq!(
            pi_enabled_model_patterns_at(&path),
            ["cpa/gpt-5.6-sol", "anthropic/*"]
        );

        std::fs::remove_file(&path).unwrap();
        // Unreadable, malformed, or empty settings all mean "no scope".
        assert!(pi_enabled_model_patterns_at(&path).is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(pi_enabled_model_patterns_at(&path).is_empty());
        std::fs::write(&path, r#"{"enabledModels":[]}"#).unwrap();
        assert!(pi_enabled_model_patterns_at(&path).is_empty());
    }

    #[test]
    fn pi_agent_directory_resolves_the_env_override_and_tilde() {
        let home = Some(PathBuf::from("/home/dev"));
        // Default: ~/.pi/agent.
        assert_eq!(
            pi_agent_directory_for(None, home.clone()),
            Some(PathBuf::from("/home/dev/.pi/agent"))
        );
        // Plain override.
        assert_eq!(
            pi_agent_directory_for(Some(PathBuf::from("/opt/pi")), home.clone()),
            Some(PathBuf::from("/opt/pi"))
        );
        // `~` alone and `~/suffix` expand against HOME; no HOME keeps `~`.
        assert_eq!(
            pi_agent_directory_for(Some(PathBuf::from("~")), home.clone()),
            home
        );
        assert_eq!(
            pi_agent_directory_for(Some(PathBuf::from("~/pi")), home.clone()),
            Some(PathBuf::from("/home/dev/pi"))
        );
        assert_eq!(pi_agent_directory_for(Some(PathBuf::from("~")), None), None);
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
    fn terminal_settle_requires_terminal_or_unflagged_settle() {
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

    #[test]
    fn command_scopes_follow_the_reported_source() {
        let commands = parse_pi_commands(&json!({"data": {"commands": [
            {"name": "skill:review", "source": "skill", "description": "Review"},
            {"name": "ship", "source": "prompt", "description": "Ship it",
             "sourceInfo": {"scope": "project"}},
            {"name": "notes", "source": "prompt", "description": "Notes",
             "sourceInfo": {"scope": "user"}},
            {"name": "compact", "source": "extension", "description": "Compact"}
        ]}}));
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].scope, CommandScope::Skill);
        assert_eq!(commands[1].scope, CommandScope::Project);
        assert_eq!(commands[2].scope, CommandScope::User);
        assert_eq!(commands[3].scope, CommandScope::Builtin);
    }

    #[test]
    fn template_expansion_substitutes_arguments() {
        // $ARGUMENTS gets the whole tail; $1..$9 pick their word.
        assert_eq!(
            expand_command_template("Say $ARGUMENTS loudly", "hello there"),
            "Say hello there loudly"
        );
        assert_eq!(
            expand_command_template("$1 fixes $2", "bug code"),
            "bug fixes code"
        );
        // A template that consumes nothing gets the args appended.
        assert_eq!(
            expand_command_template("Summarize", "this file"),
            "Summarize\n\nthis file"
        );
        // A missing positional arg substitutes empty (and counts as consumed,
        // so nothing is appended); unknown $-words pass through untouched.
        assert_eq!(
            expand_command_template("cost is $5 and $TERM stays", "x"),
            "cost is  and $TERM stays"
        );
        assert_eq!(expand_command_template("noop", ""), "noop");
    }

    #[test]
    fn submission_resolution_prefers_skills_then_templates() {
        let skills = ["review"].into_iter().map(str::to_owned).collect();
        let templates = [("deploy".to_owned(), "Deploy $ARGUMENTS".to_owned())]
            .into_iter()
            .collect();
        // A skill goes through the native /skill: invocation.
        assert_eq!(
            resolve_prompt_submission("/review src/main.rs", &skills, &templates),
            "/skill:review src/main.rs"
        );
        // A template expands to its body.
        assert_eq!(
            resolve_prompt_submission("/deploy prod", &skills, &templates),
            "Deploy prod"
        );
        // Built-ins and ordinary text pass through verbatim.
        assert_eq!(
            resolve_prompt_submission("/compact", &skills, &templates),
            "/compact"
        );
        assert_eq!(
            resolve_prompt_submission("plain text", &skills, &templates),
            "plain text"
        );
        // Already-native spelling is left alone.
        assert_eq!(
            resolve_prompt_submission("/skill:review now", &skills, &templates),
            "/skill:review now"
        );
    }

    #[test]
    fn prompt_template_and_skill_discovery_scan_the_pi_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join(".pi/prompts/ops")).unwrap();
        std::fs::create_dir_all(cwd.join(".pi/skills/review")).unwrap();
        std::fs::write(
            cwd.join(".pi/prompts/ship.md"),
            "---\nname: ship\ndescription: Ship it\n---\nShip $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(cwd.join(".pi/prompts/ops/restart.md"), "Restart now").unwrap();
        std::fs::write(
            cwd.join(".pi/skills/review/SKILL.md"),
            "---\nname: review\ndescription: Review code\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(cwd.join(".pi/skills/.hidden")).unwrap();
        std::fs::write(cwd.join(".pi/skills/.hidden/SKILL.md"), "hidden").unwrap();

        let templates = discover_prompt_templates(cwd);
        assert_eq!(
            templates.get("ship").map(String::as_str),
            Some("Ship $ARGUMENTS")
        );
        assert_eq!(
            templates.get("ops:restart").map(String::as_str),
            Some("Restart now")
        );

        let skills = discover_skill_names(cwd);
        assert!(skills.contains("review"));
        assert!(!skills.contains(".hidden"));
    }

    #[test]
    fn frontmatter_name_reads_the_field_and_strips_it_from_the_body() {
        let md = "---\nname: custom-name\ndescription: A skill\n---\nDo the thing";
        assert_eq!(frontmatter_name(md).as_deref(), Some("custom-name"));
        assert_eq!(strip_frontmatter(md), "Do the thing");
        assert_eq!(strip_frontmatter("no frontmatter"), "no frontmatter");
    }
}

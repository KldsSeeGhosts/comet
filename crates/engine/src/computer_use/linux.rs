//! Computer use owned by the engine. The Pi adapter only forwards tool calls.
//!
//! A lease covers one active turn, not the lifetime of a parked Pi process.
//! Interrupts, disconnected callers and turn boundaries cancel outstanding work
//! before reaping the private Linux driver and releasing that lease. Approval
//! is session-scoped: it survives that teardown so a resumed turn re-acquires
//! the lease and driver without asking again. On a real host the serve daemon
//! carries the existing-profile grant only once that approval exists; metadata
//! asked before approval runs on a daemon spawned without the grant.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use zeron_harness::CancellationToken;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

const REQUEST_LIMIT: usize = 1024 * 1024;
const RESPONSE_LIMIT: usize = 32 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(120);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(750);
const DESCRIBE_NAMES_LIMIT: usize = 32;

/// MCP revision this client advertises and prefers. A driver may negotiate
/// any entry in `SUPPORTED_MCP_PROTOCOL_VERSIONS`; nothing else is trusted.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// MCP revisions whose initialize, tools/list and tools/call contract this
/// client implements. A negotiated version outside this list fails the
/// handshake instead of being retained as trusted metadata.
const SUPPORTED_MCP_PROTOCOL_VERSIONS: &[&str] =
    &[MCP_PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

// New driver tools need an explicit review before the agent can invoke them.
const ACTIONS: &[&str] = &[
    "list_apps",
    "list_windows",
    "get_window_state",
    "verify_state",
    "get_accessibility_tree",
    "get_screen_size",
    "get_desktop_state",
    "get_cursor_position",
    "health_report",
    "check_permissions",
    "launch_app",
    "kill_app",
    "bring_to_front",
    "set_window_frame",
    "click",
    "double_click",
    "right_click",
    "drag",
    "scroll",
    "mouse_button_down",
    "mouse_button_up",
    "mouse_drag",
    "move_cursor",
    "type_text",
    "press_key",
    "hotkey",
    "set_value",
    "invoke_menu",
    "clipboard_read",
    "clipboard_write",
    "get_browser_state",
    "browser_prepare",
    "browser_navigate",
    "browser_click",
    "browser_type",
    "browser_dialog",
    "browser_set_input_files",
    "browser_download",
    "browser_pointer",
    "zoom",
    "get_agent_cursor_state",
];

const NON_DISRUPTIVE_POLICY: &str = "Only non-disruptive computer use is allowed. Physical focus, mouse and keyboard must remain untouched. Desktop capture is read-only. Supported window input uses background delivery with an exact pid/window_id; unsupported background routes must refuse, never fall back to foreground.";

const BLOCKED_ACTIONS: &[&str] = &[
    "bring_to_front",
    "launch_app",
    "kill_app",
    "set_window_frame",
    "mouse_button_down",
    "mouse_drag",
    "mouse_button_up",
    "invoke_menu",
    "clipboard_write",
];
const WINDOW_INPUT_ACTIONS: &[&str] = &[
    "click",
    "double_click",
    "right_click",
    "drag",
    "scroll",
    "type_text",
    "press_key",
    "hotkey",
];

/// This check runs before acquiring a lease, asking approval or starting the driver.
/// The reviewed driver routes must still verify the target and refuse unsupported
/// background delivery. A session grant cannot relax this policy.
fn validate_non_disruptive(
    action: &str,
    map: &mut serde_json::Map<String, Value>,
) -> CuaResult<()> {
    if map.contains_key("allow_user_input_disruption") {
        return Err("Disruption overrides are forbidden, regardless of session approval".into());
    }
    if let Some(mode) = map.get("delivery_mode") {
        if !mode
            .as_str()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("background"))
        {
            return Err(
                "delivery_mode must be background; foreground and unknown modes are forbidden"
                    .into(),
            );
        }
        map.insert("delivery_mode".into(), json!("background"));
    }
    if let Some(scope) = map.get("scope") {
        let scope = scope.as_str().map(|s| s.trim().to_ascii_lowercase());
        if !matches!(scope.as_deref(), Some("window" | "desktop")) {
            return Err("scope must be window or desktop".into());
        }
        map.insert("scope".into(), json!(scope.unwrap()));
    }
    let desktop_capture = matches!(action, "get_desktop_state" | "get_screen_size");
    if let Some(target) = map.get_mut("target") {
        let target = target.as_object_mut().ok_or("target must be an object")?;
        let kind = target
            .get("kind")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase());
        if !matches!(kind.as_deref(), Some("window" | "desktop")) {
            return Err("target.kind must be window or desktop".into());
        }
        if kind.as_deref() == Some("desktop") && !desktop_capture {
            return Err("Desktop input is forbidden, including keyboard input".into());
        }
        target.insert("kind".into(), json!(kind.unwrap()));
    }
    if !desktop_capture
        && (map.get("scope") == Some(&json!("desktop"))
            || map.contains_key("display_id")
            || map.contains_key("expected_layout"))
    {
        return Err(
            "Desktop routes are restricted to read-only get_desktop_state/get_screen_size".into(),
        );
    }
    if action == "browser_prepare" {
        // The audited schema has no attach-only switch. allow_launch=false
        // still permits existing-profile setup through global keyboard input.
        return Err("browser_prepare is disabled: the driver has no guaranteed attach-only, non-disruptive route. Set up the browser and DevTools manually, then use get_browser_state. Automatic setup and launch are forbidden.".into());
    }
    if BLOCKED_ACTIONS.contains(&action) {
        return Err(format!(
            "{action} is forbidden: no verified non-disruptive background route"
        ));
    }
    if action == "browser_dialog" && map.get("action").and_then(Value::as_str) != Some("inspect") {
        return Err("Only browser_dialog action=inspect is allowed; resolving native browser dialogs can change physical focus. Handle the dialog manually.".into());
    }
    if WINDOW_INPUT_ACTIONS.contains(&action) || matches!(action, "set_value" | "move_cursor") {
        // Do not infer an active window or trust an alternate target to select it.
        for key in ["pid", "window_id"] {
            if !map
                .get(key)
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0 && n <= 9_007_199_254_740_991)
            {
                return Err(format!(
                    "{action} requires an exact positive integer {key} for background input"
                ));
            }
        }
        if map.contains_key("target") {
            return Err(
                "Window input requires top-level pid/window_id, not an alternate target".into(),
            );
        }
        if WINDOW_INPUT_ACTIONS.contains(&action) {
            map.insert("delivery_mode".into(), json!("background"));
        }
    }
    Ok(())
}

/// Driver schemas can suggest unsafe fallback routes. Keep their structure but
/// replace prose with the engine policy and narrow exposed delivery modes.
fn managed_schema(mut tool: Value) -> Value {
    fn strip_prose(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("description");
                for value in map.values_mut() {
                    strip_prose(value);
                }
            }
            Value::Array(values) => values.iter_mut().for_each(strip_prose),
            _ => {}
        }
    }
    strip_prose(&mut tool);
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    tool["description"] = json!(if name == "browser_prepare" {
        "Disabled: no guaranteed attach-only route. Set up the browser and DevTools manually, then use get_browser_state."
    } else if BLOCKED_ACTIONS.contains(&name.as_str()) {
        "Disabled: no verified non-disruptive background route."
    } else {
        NON_DISRUPTIVE_POLICY
    });
    if let Some(properties) = tool
        .pointer_mut("/inputSchema/properties")
        .and_then(Value::as_object_mut)
    {
        if name == "browser_dialog" {
            properties.insert(
                "action".into(),
                json!({"type":"string", "enum":["inspect"]}),
            );
        }
        if properties.contains_key("delivery_mode") {
            properties.insert(
                "delivery_mode".into(),
                json!({"type":"string", "enum":["background"], "default":"background"}),
            );
        }
        if properties.contains_key("scope")
            && !matches!(name.as_str(), "get_desktop_state" | "get_screen_size")
        {
            properties.insert(
                "scope".into(),
                json!({"type":"string", "enum":["window"], "default":"window"}),
            );
        }
    }
    tool
}

pub type RequestInput =
    Arc<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync>;

type CuaResult<T> = Result<T, String>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn error(message: impl ToString) -> Value {
    json!({"isError": true, "content": [{"type":"text", "text":message.to_string()}]})
}

/// Structured CUA outcome classes from the driver's versioned result model.
/// Only refusals and explicit error statuses are failed tool executions;
/// partial, unknown and unverifiable outcomes stay non-errors so callers do
/// not replay input whose delivery was never established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriverOutcome {
    Delivered,
    Refused,
    Partial,
    Unknown,
    Unverifiable,
    Cancelled,
    Error,
}

fn classify_driver_outcome(result: &Value) -> DriverOutcome {
    let structured = result.get("structuredContent");
    let status = structured
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let effect = structured
        .and_then(|value| value.get("effect"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let refused_flag = structured
        .and_then(|value| value.get("refused"))
        .and_then(Value::as_bool)
        == Some(true);
    let driver_is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    let refused = |token: Option<&str>| {
        token.is_some_and(|value| {
            matches!(
                value,
                "refused" | "denied" | "rejected" | "blocked" | "forbidden"
            )
        })
    };

    // A structured refusal outranks the generic execution-error flag: a
    // refusal reported with isError is still an exact refusal. The driver's
    // outcome-only variant carries the same signal in `effect`.
    if refused_flag || refused(status.as_deref()) || refused(effect.as_deref()) {
        return DriverOutcome::Refused;
    }
    match status.as_deref() {
        Some("error" | "failed" | "failure") => return DriverOutcome::Error,
        Some("cancelled" | "canceled") => return DriverOutcome::Cancelled,
        // Uncertain deliveries stay non-errors only while the driver did not
        // flag the call as failed; an explicit isError makes it an error.
        Some("partial" | "partially_delivered" | "incomplete") => {
            return if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Partial
            };
        }
        Some("unknown" | "unobserved" | "indeterminate") => {
            return if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Unknown
            };
        }
        Some("unverifiable" | "unverified") => {
            return if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Unverifiable
            };
        }
        _ => {}
    }
    match effect.as_deref() {
        Some("error" | "failed" | "failure") => DriverOutcome::Error,
        Some("cancelled" | "canceled") => DriverOutcome::Cancelled,
        Some("unverifiable" | "unverified") => {
            if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Unverifiable
            }
        }
        Some("partial") => {
            if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Partial
            }
        }
        Some("unknown") => {
            if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Unknown
            }
        }
        _ => {
            if driver_is_error {
                DriverOutcome::Error
            } else {
                DriverOutcome::Delivered
            }
        }
    }
}

/// Compatibility normalizer for drivers that report a structured refusal
/// without the MCP execution-error flag. The refusal payload is preserved
/// verbatim; only the top-level `isError` flag is added. Partial, unknown and
/// unverifiable outcomes are deliberately left untouched.
fn normalize_driver_outcome(mut result: Value) -> Value {
    if matches!(
        classify_driver_outcome(&result),
        DriverOutcome::Refused | DriverOutcome::Error
    ) && result.get("isError").and_then(Value::as_bool) != Some(true)
        && let Some(map) = result.as_object_mut()
    {
        map.insert("isError".into(), Value::Bool(true));
    }
    result
}

/// Negotiated MCP initialize contract, retained after the handshake instead of
/// being discarded. `instructions` is optional server prose; it is hashed for
/// diagnostics and never forwarded into model-visible text.
#[derive(Clone, Debug, Default)]
struct InitializeInfo {
    protocol_version: String,
    server_info: ServerInfo,
    capabilities: Value,
    instructions: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ServerInfo {
    name: String,
    version: Option<String>,
}

fn parse_initialize_result(result: &Value) -> CuaResult<InitializeInfo> {
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("cua-driver initialize result is missing a non-empty protocolVersion")?;
    if !SUPPORTED_MCP_PROTOCOL_VERSIONS.contains(&protocol_version) {
        return Err(format!(
            "cua-driver negotiated unsupported MCP protocol version {protocol_version}; supported versions: {}",
            SUPPORTED_MCP_PROTOCOL_VERSIONS.join(", ")
        ));
    }
    let protocol_version = protocol_version.to_string();
    let server_info = result
        .get("serverInfo")
        .and_then(Value::as_object)
        .ok_or("cua-driver initialize result is missing serverInfo")?;
    let name = server_info
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("cua-driver initialize result serverInfo.name must be a non-empty string")?
        .to_string();
    let version = match server_info.get("version") {
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .ok_or(
                    "cua-driver initialize result serverInfo.version must be a non-empty string",
                )?
                .to_string(),
        ),
        None => None,
    };
    let capabilities = result
        .get("capabilities")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or("cua-driver initialize result is missing a capabilities object")?;
    let instructions = match result.get("instructions") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err("cua-driver initialize result instructions must be a string".into()),
        None => None,
    };
    Ok(InitializeInfo {
        protocol_version,
        server_info: ServerInfo { name, version },
        capabilities,
        instructions,
    })
}

fn attach_driver_metadata(result: &mut Value, metadata: Value) {
    if let Some(structured) = result
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        structured.insert("driver".into(), metadata);
    }
}

/// Canonical path of the executable the engine is about to spawn. A
/// symlinked invocation path resolves to the file itself, so the engine
/// reports and hashes the target rather than the link. An unresolvable path
/// fails the spawn instead of falling back to the unresolved link.
fn canonical_executable(path: &Path) -> CuaResult<PathBuf> {
    std::fs::canonicalize(path)
        .map_err(|e| format!("Cannot resolve executable {}: {e}", path.display()))
}

/// SHA-256 of a driver executable file. Package metadata is never consulted;
/// an unreadable binary fails the spawn. The native spawn path re-hashes the
/// running image from `/proc/<pid>/exe`; this digest remains authoritative for
/// injected script fixtures, which execute through an interpreter.
async fn hash_executable(path: &Path) -> CuaResult<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .map_err(|e| format!("Cannot read {} for hashing: {e}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|e| format!("Cannot hash {}: {e}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| format!("cua-driver hashing task failed: {e}"))?
}

/// Verify that a spawned pid is executing the canonical file the engine
/// resolved before spawn. `/proc/<pid>/exe` is the kernel's link to the
/// executed inode, so an atomic replacement that lands between the pre-spawn
/// hash and exec is visible here instead of being silently accepted.
fn verify_running_executable(pid: u32, canonical: &Path) -> CuaResult<PathBuf> {
    let proc_exe = PathBuf::from(format!("/proc/{pid}/exe"));
    let resolved = std::fs::canonicalize(&proc_exe)
        .map_err(|e| format!("Cannot resolve {}: {e}", proc_exe.display()))?;
    if resolved != canonical {
        return Err(format!(
            "cua-driver process {pid} is running {} instead of the verified {}; refusing to report an unverified executable",
            resolved.display(),
            canonical.display()
        ));
    }
    Ok(resolved)
}

/// SHA-256 of the image a spawned pid is actually running, taken from
/// `/proc/<pid>/exe` after the identity check. Hashing the running path means
/// a replacement between the pre-spawn hash and exec updates the reported
/// digest rather than leaving a stale one behind.
async fn running_executable_sha256(pid: u32, canonical: &Path) -> CuaResult<String> {
    verify_running_executable(pid, canonical)?;
    hash_executable(&PathBuf::from(format!("/proc/{pid}/exe"))).await
}

/// Reap a partially spawned native driver after a post-spawn identity check
/// fails. Killing both processes keeps the private socket and overlay gone;
/// the spawn caller reports the verification error instead of a stale path.
async fn reap_spawned(child: &mut Child, daemon: Option<&mut Child>) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    if let Some(daemon) = daemon {
        let _ = daemon.start_kill();
        let _ = daemon.wait().await;
    }
}

pub struct ComputerUseManager {
    lease: Arc<Mutex<Option<String>>>,
    approvals: Arc<Mutex<HashSet<String>>>,
    device_id: String,
    // Tests inject an executable rather than modifying the process environment.
    driver_path: Option<PathBuf>,
}

impl ComputerUseManager {
    pub fn new(device_id: String) -> Self {
        Self {
            lease: Arc::new(Mutex::new(None)),
            approvals: Arc::new(Mutex::new(HashSet::new())),
            device_id,
            driver_path: None,
        }
    }

    /// Revoke session computer-use approval for a specific chat.
    ///
    /// Returns true if an approval grant was present and removed.
    pub fn forget_computer_use_approval(&self, chat_id: &str) -> bool {
        lock(&self.approvals).remove(chat_id)
    }

    pub async fn start_bridge(
        &self,
        chat_id: &str,
        run_id: &str,
        request_input: RequestInput,
        interrupt: CancellationToken,
    ) -> CuaResult<(PathBuf, RunBridge)> {
        if !cfg!(target_os = "linux") {
            return Err("Managed computer use currently requires a Linux host".into());
        }
        let dir = tempfile::Builder::new()
            .prefix("noches-cua-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let socket = dir.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        // Private endpoint for the per-run serve daemon that owns the agent
        // cursor overlay runloop; the MCP child proxies through it.
        let daemon_socket = dir.path().join("driver.sock");
        let granted = lock(&self.approvals).contains(chat_id);
        let state = Arc::new(BridgeState {
            chat_id: chat_id.to_string(),
            owner: format!("chat {chat_id}, run {run_id}"),
            label: format!("noches-{}", uuid::Uuid::new_v4()),
            device_id: self.device_id.clone(),
            lease: self.lease.clone(),
            approvals: self.approvals.clone(),
            request_input,
            driver_path: self.driver_path.clone(),
            daemon_socket,
            turn: Mutex::new(Turn {
                active: true,
                cancel: Arc::new(CancellationToken::new()),
            }),
            stop: CancellationToken::new(),
            runtime: AsyncMutex::new(Runtime {
                granted,
                ..Runtime::default()
            }),
        });
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let _dir = dir; // Socket lifetime includes driver cleanup.
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = task_state.stop.cancelled() => break,
                    _ = interrupt.cancelled() => break,
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept(), if connections.len() < 16 => {
                        match accepted {
                            Ok((stream, _)) => {
                                let state = task_state.clone();
                                let turn = lock(&state.turn).cancel.clone();
                                connections.spawn(serve_connection(state, turn, stream));
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            task_state.stop.cancel();
            lock(&task_state.turn).cancel.cancel();
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            task_state.cleanup(false).await;
        });
        Ok((
            socket,
            RunBridge {
                state,
                task: Some(task),
            },
        ))
    }
}

struct Turn {
    active: bool,
    cancel: Arc<CancellationToken>,
}

#[derive(Default)]
struct Runtime {
    driver: Option<Driver>,
    // One session-wide grant covers inspection and permitted background input.
    // It survives per-turn driver/lease teardown so a resumed turn does not
    // re-ask. A denial stays per-turn and resets here, so a fresh turn may
    // ask again.
    granted: bool,
    denied: bool,
    lease_held: bool,
}

struct BridgeState {
    chat_id: String,
    owner: String,
    label: String,
    device_id: String,
    lease: Arc<Mutex<Option<String>>>,
    approvals: Arc<Mutex<HashSet<String>>>,
    request_input: RequestInput,
    driver_path: Option<PathBuf>,
    daemon_socket: PathBuf,
    turn: Mutex<Turn>,
    stop: CancellationToken,
    // Covers policy, approval and delivery, including parallel tool batches.
    runtime: AsyncMutex<Runtime>,
}

impl BridgeState {
    async fn cleanup(&self, orderly: bool) {
        let mut runtime = self.runtime.lock().await;
        self.clean_runtime(&mut runtime, orderly).await;
    }

    async fn clean_runtime(&self, runtime: &mut Runtime, orderly: bool) {
        if let Some(mut driver) = runtime.driver.take() {
            if orderly {
                let _ = tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    driver.call("end_session", json!({"session": self.label})),
                )
                .await;
            }
            // A kill error retains the lease. Never claim control was released
            // while the process might still be delivering input.
            if let Err(err) = driver.kill().await {
                tracing::error!(error = %err, "computer-use driver could not be reaped; lease retained");
                runtime.driver = Some(driver);
                return;
            }
        }
        if let Err(err) = remove_socket_if_present(&self.daemon_socket) {
            tracing::warn!(error = %err, path = %self.daemon_socket.display(), "computer-use daemon socket cleanup failed");
        }
        if runtime.lease_held {
            let mut owner = lock(&self.lease);
            if owner.as_deref() == Some(self.owner.as_str()) {
                *owner = None;
            }
        }
        // The positive grant is session/chat-scoped and survives teardown; everything
        // else (driver, lease marker, a denial) belongs to the turn or attempt.
        let granted = lock(&self.approvals).contains(&self.chat_id);
        *runtime = Runtime {
            granted,
            ..Runtime::default()
        };
    }

    fn active(&self, turn: &CancellationToken) -> bool {
        !self.stop.is_cancelled() && !turn.is_cancelled() && lock(&self.turn).active
    }
}

pub struct RunBridge {
    state: Arc<BridgeState>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RunBridge {
    pub fn turn_started(&self) {
        let mut turn = lock(&self.state.turn);
        if !turn.active && !self.state.stop.is_cancelled() {
            *turn = Turn {
                active: true,
                cancel: Arc::new(CancellationToken::new()),
            };
        }
    }

    pub async fn turn_ended(&self) {
        {
            let mut turn = lock(&self.state.turn);
            turn.active = false;
            turn.cancel.cancel();
        }
        self.state.cleanup(true).await;
    }

    pub async fn finish(mut self) {
        self.state.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RunBridge {
    fn drop(&mut self) {
        self.state.stop.cancel();
    }
}

// Read incrementally: checking a Vec's length after read_until is too late.
async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R, limit: usize) -> CuaResult<Value> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            return Err("connection closed before a complete response".into());
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let len = newline.map_or(chunk.len(), |i| i + 1);
        if bytes.len() + len > limit {
            return Err("protocol frame exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk[..len]);
        reader.consume(len);
        if newline.is_some() {
            return serde_json::from_slice(&bytes).map_err(|e| e.to_string());
        }
    }
}

async fn serve_connection(
    state: Arc<BridgeState>,
    turn: Arc<CancellationToken>,
    stream: UnixStream,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let request = tokio::select! {
        biased;
        _ = state.stop.cancelled() => return,
        result = tokio::time::timeout(START_TIMEOUT, read_frame(&mut reader, REQUEST_LIMIT)) => {
            match result { Ok(Ok(request)) => request, _ => return }
        }
    };
    let action = request.get("action").and_then(Value::as_str).unwrap_or("");
    let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
    let response = tokio::select! {
        biased;
        _ = state.stop.cancelled() => None,
        _ = turn.cancelled() => None,
        // The adapter keeps its write half open until it has the response.
        // EOF or unexpected extra data means this caller has abandoned its call.
        _ = reader.read_u8() => None,
        result = tokio::time::timeout(IO_TIMEOUT, handle_call(&state, &turn, action, args)) => {
            result.ok()
        }
    };
    let response = match response {
        Some(result) => result,
        None => {
            // Partial input may have landed. End this turn's authority rather
            // than allowing an automatic retry with an uncertain desktop state.
            turn.cancel();
            let mut runtime = state.runtime.lock().await;
            let current = Arc::ptr_eq(&lock(&state.turn).cancel, &turn);
            if current {
                state.clean_runtime(&mut runtime, false).await;
            }
            error(
                "Computer use cancelled. An action may have partially completed; inspect before retrying in a new turn.",
            )
        }
    };
    let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
    bytes.push(b'\n');
    let _ = tokio::time::timeout(START_TIMEOUT, writer.write_all(&bytes)).await;
}

async fn approve(state: &BridgeState, runtime: &mut Runtime, action: &str) -> CuaResult<()> {
    if runtime.denied {
        return Err("Computer use denied for this turn".into());
    }
    if lock(&state.approvals).contains(&state.chat_id) {
        runtime.granted = true;
        return Ok(());
    }
    runtime.granted = false;
    let id = uuid::Uuid::new_v4().to_string();
    let question = approval_question(&state.device_id, action);
    let answers = (state.request_input)(vec![UserInputQuestion {
        id: id.clone(),
        header: "Computer use".into(),
        question,
        options: vec!["Deny".into(), "Allow".into()],
        multi_select: false,
    }])
    .await
    .unwrap_or_default();
    if answers
        .iter()
        .any(|a| a.question_id == id && a.labels == ["Allow"])
    {
        lock(&state.approvals).insert(state.chat_id.clone());
        runtime.granted = true;
        Ok(())
    } else {
        runtime.denied = true;
        Err("Computer use denied for this turn".into())
    }
}

async fn handle_call(
    state: &BridgeState,
    turn: &CancellationToken,
    action: &str,
    mut args: Value,
) -> Value {
    if !ACTIONS.contains(&action) && action != "help" && action != "describe" {
        return error(format!(
            "Unsupported or engine-managed computer-use action: {action}"
        ));
    }
    let Some(map) = args.as_object_mut() else {
        return error("args must be an object");
    };
    if map
        .keys()
        .any(|k| k.starts_with('_') || k == "capture_scope")
    {
        return error("Reserved session/policy arguments are not accepted");
    }
    if let Err(reason) = validate_non_disruptive(action, map) {
        return error(format!("{reason}. {NON_DISRUPTIVE_POLICY}"));
    }
    map.insert("session".into(), json!(state.label));
    let requested_names = if action == "describe" {
        let has_name = map.contains_key("name");
        let has_names = map.contains_key("names");
        if has_name && has_names {
            return error("describe accepts either name or names, not both");
        }
        if !has_name && !has_names {
            return error("describe requires either name or names");
        }
        if let Some(unexpected) = map
            .keys()
            .find(|k| k.as_str() != "name" && k.as_str() != "names" && k.as_str() != "session")
        {
            return error(format!("Unexpected argument for describe: {unexpected}"));
        }
        if has_name {
            let Some(name_str) = map.get("name").and_then(Value::as_str) else {
                return error("args.name must be a string");
            };
            if !ACTIONS.contains(&name_str) {
                return error(format!("Action is not exposed by Noches: {name_str}"));
            }
            vec![name_str.to_string()]
        } else {
            let Some(arr) = map.get("names").and_then(Value::as_array) else {
                return error("args.names must be an array of action names");
            };
            if arr.is_empty() {
                return error("args.names must not be empty");
            }
            if arr.len() > DESCRIBE_NAMES_LIMIT {
                return error(format!(
                    "args.names cannot exceed {DESCRIBE_NAMES_LIMIT} actions"
                ));
            }
            let mut names = Vec::with_capacity(arr.len());
            for item in arr {
                let Some(item_str) = item.as_str() else {
                    return error("args.names must contain only string action names");
                };
                if !ACTIONS.contains(&item_str) {
                    return error(format!("Action is not exposed by Noches: {item_str}"));
                }
                names.push(item_str.to_string());
            }
            names
        }
    } else {
        Vec::new()
    };
    let mut runtime = state.runtime.lock().await;
    if !state.active(turn) {
        return error("Computer use is parked or cancelled; start a new turn");
    }
    let metadata = matches!(action, "help" | "describe" | "health_report");
    if !metadata {
        // A daemon started for metadata before approval runs without the
        // existing-profile grant. Restart it ungracefully before touching the
        // lease so the approved action runs on a granted daemon. Injected
        // fixtures run no daemon and skip this entirely.
        if let Some(false) = runtime.driver.as_ref().and_then(Driver::daemon_grant) {
            state.clean_runtime(&mut runtime, false).await;
        }
        // The cleanup must not leave an ungranted daemon behind; if one
        // survives, fail closed instead of routing the action through it.
        if let Some(false) = runtime.driver.as_ref().and_then(Driver::daemon_grant) {
            return error(
                "An ungranted daemon is still running after cleanup; restart computer use and start a new turn",
            );
        }
        if !runtime.lease_held {
            let mut lease = lock(&state.lease);
            if let Some(owner) = lease.as_ref() {
                return error(format!("Computer use is busy on this host: {owner}"));
            }
            *lease = Some(state.owner.clone());
            runtime.lease_held = true;
        }
        if let Err(err) = approve(state, &mut runtime, action).await {
            // A denial must not reserve the desktop for a parked chat.
            if !runtime.granted {
                let denied = runtime.denied;
                state.clean_runtime(&mut runtime, false).await;
                runtime.denied = denied;
            }
            return error(err);
        }
    }
    if runtime.driver.is_none() {
        // Metadata runs before approval on an ungranted daemon; once approval
        // exists (persisted or freshly given this turn) the daemon gets the
        // grant. There is no pre-approval daemon with the grant.
        match Driver::spawn(
            state.driver_path.as_ref(),
            &state.daemon_socket,
            runtime.granted,
        )
        .await
        {
            Ok(driver) => runtime.driver = Some(driver),
            Err(err) => {
                tracing::warn!(error = %err, "computer-use driver spawn failed");
                state.clean_runtime(&mut runtime, false).await;
                return error(err);
            }
        }
        let result = tokio::time::timeout(
            START_TIMEOUT,
            runtime.driver.as_mut().expect("spawned").handshake(),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            let detail = match &result {
                Err(_) => format!("handshake timed out after {}s", START_TIMEOUT.as_secs()),
                Ok(Err(err)) => format!("handshake failed: {err}"),
                Ok(Ok(())) => unreachable!(),
            };
            tracing::warn!(error = %detail, "computer-use driver init failed");
            state.clean_runtime(&mut runtime, false).await;
            return error(format!("Could not initialize cua-driver: {detail}"));
        }
    }
    let driver = runtime.driver.as_mut().expect("initialized");
    let marker_pid = args.get("pid").and_then(Value::as_u64);
    let result = if action == "help" {
        driver.catalog(None).await
    } else if action == "describe" {
        driver.catalog(Some(&requested_names)).await
    } else if action == "health_report" {
        let metadata = driver.initialize_metadata();
        driver.call(action, args).await.map(|mut result| {
            attach_driver_metadata(&mut result, metadata);
            result
        })
    } else {
        driver.call(action, args).await
    };
    match result {
        Ok(result) => attach_seat_marker_evidence(normalize_driver_outcome(result), marker_pid),
        Err(err) => {
            turn.cancel();
            state.clean_runtime(&mut runtime, false).await;
            error(format!(
                "Computer-use transport failed: {err}. No action was retried; start a new turn after inspecting the target."
            ))
        }
    }
}

struct Driver {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    // The serve daemon that owns the agent cursor overlay runloop. Present
    // only for the real driver; injected test fixtures stay on direct mcp.
    daemon: Option<Child>,
    // Whether that daemon was spawned with --grant existing-profile. Fixed
    // at spawn and only meaningful when daemon is Some; the engine tracks
    // this explicitly instead of reading the child's argv back.
    existing_profile_granted: bool,
    // Negotiated MCP initialize contract, retained from the handshake.
    initialize: InitializeInfo,
    // Full tools/list result reused while the negotiated capabilities prove
    // the list cannot change. Cleared with the driver at turn teardown.
    tools_list_cache: Option<Value>,
    // Configured or resolved invocation path, reported for operators.
    exe_path: PathBuf,
    // Canonical target of `exe_path`; this is the file hashed and spawned.
    exe_canonical_path: PathBuf,
    exe_sha256: String,
}

impl Driver {
    /// Grant state of the live serve daemon, if this driver runs one.
    /// Drivers without a daemon (injected fixtures) return None.
    fn daemon_grant(&self) -> Option<bool> {
        self.daemon.as_ref().map(|_| self.existing_profile_granted)
    }

    /// A driver whose retained capabilities declare a `tools` capability
    /// without `listChanged: true` cannot change its tool list for the life of
    /// this connection: MCP only publishes list-change notifications when the
    /// capability sets `listChanged`. That covers the real cua-driver
    /// `capabilities: {"tools": {}}` handshake, where the optional flag is
    /// absent and therefore false. A missing tools capability or
    /// `listChanged: true` keeps discovery on every help/describe call.
    fn tools_list_cacheable(&self) -> bool {
        self.initialize
            .capabilities
            .get("tools")
            .and_then(Value::as_object)
            .is_some_and(|tools| tools.get("listChanged").and_then(Value::as_bool) != Some(true))
    }

    fn base_command(exe: &std::path::Path) -> Command {
        let mut command = Command::new(exe);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Inherited driver settings must not enable an external service,
        // approval bypass or a second HTTP interface behind the engine's back.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("CUA_") {
                command.env_remove(key);
            }
        }
        // Select CUA's production Hyprland backend and the native input-v3
        // route that this host explicitly enables in its compositor config.
        // The driver still requires an exact target and the engine still
        // permits background-only input; this only avoids silently restoring
        // the driver's narrow package matrix after the engine sanitizes CUA_*.
        // Setting CUA_DRIVER_EXPERIMENTAL_HYPRLAND_INPUT would instead select
        // the test protocol and look for cua-input-test.sock, so it must stay
        // unset.
        command
            .env("CUA_DRIVER_PERMISSION_MODE", "standard")
            .env("CUA_DRIVER_RS_ENABLE_WAYLAND", "1")
            .env("CUA_HYPRLAND_OPEN_INPUT", "1");
        command
    }

    fn mcp_command(exe: &std::path::Path, daemon_socket: Option<&PathBuf>) -> Command {
        let mut command = Self::base_command(exe);
        command.arg("mcp");
        if let Some(socket) = daemon_socket {
            command.arg("--socket").arg(socket);
        }
        command
    }

    /// Pure argument construction for the real driver's serve daemon, so
    /// tests can assert policy-critical flags without spawning a process.
    fn serve_command(exe: &std::path::Path, daemon_socket: &PathBuf, grant: bool) -> Command {
        let mut command = Self::base_command(exe);
        command
            .arg("serve")
            .arg("--socket")
            .arg(daemon_socket)
            .arg("--permission-mode")
            .arg("standard");
        // The engine's own approval question already disclosed attaching
        // to an existing logged-in Chromium profile; this narrow grant
        // admits only that standard-mode boundary. It is not unrestricted
        // mode and does not bypass driver-level refusals. A daemon started
        // for metadata before approval runs without it.
        if grant {
            command.arg("--grant").arg("existing-profile");
        }
        command
            // serve owns the overlay; it does not read stdin.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        command
    }

    async fn spawn(
        path: Option<&PathBuf>,
        daemon_socket: &PathBuf,
        grant_existing_profile: bool,
    ) -> CuaResult<Self> {
        let exe = match path {
            Some(path) => path.clone(),
            None => resolve_driver_exe()?,
        };
        // Resolve the invocation path before hashing and spawning, so the
        // digest covers the file the kernel executes rather than a symlink.
        // Never derive this from package metadata; an unreadable binary
        // fails here.
        let canonical_exe = canonical_executable(&exe)?;
        let exe_sha256 = hash_executable(&canonical_exe).await?;
        // The real driver runs as a per-run serve daemon plus an mcp proxy so
        // the agent cursor overlay has a UI runloop. Injected test fixtures
        // only implement stdio mcp, so they keep the single-process path and
        // have no daemon whose grant state could matter.
        let (mut daemon, mcp_socket, existing_profile_granted) = if path.is_none() {
            remove_socket_if_present(daemon_socket)?;
            let mut daemon =
                Self::serve_command(&canonical_exe, daemon_socket, grant_existing_profile)
                    .spawn()
                    .map_err(|e| format!("Cannot start {} serve: {e}", canonical_exe.display()))?;
            if let Some(mut stderr) = daemon.stderr.take() {
                tokio::spawn(async move {
                    let mut line = String::new();
                    let mut reader = BufReader::new(&mut stderr);
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let trimmed = line.trim_end();
                        if !trimmed.is_empty() {
                            tracing::warn!(target: "cua-driver-serve", "{trimmed}");
                        }
                        line.clear();
                    }
                });
            }
            // Wait for the daemon's socket to accept before proxying into it.
            let deadline = tokio::time::Instant::now() + START_TIMEOUT;
            loop {
                if daemon_socket.exists() {
                    break;
                }
                if let Some(status) = daemon.try_wait().map_err(|e| e.to_string())? {
                    return Err(format!("cua-driver serve exited early: {status}"));
                }
                if tokio::time::Instant::now() >= deadline {
                    let _ = daemon.start_kill();
                    let _ = daemon.wait().await;
                    return Err("cua-driver serve did not create its socket in time".into());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            (
                Some(daemon),
                Some(daemon_socket.clone()),
                grant_existing_profile,
            )
        } else {
            // Injected fixtures record true: they run no serve daemon, so the
            // ungranted-daemon restart logic in handle_call never applies.
            (None, None, true)
        };
        let mut child = Self::mcp_command(&canonical_exe, mcp_socket.as_ref())
            .spawn()
            .map_err(|e| format!("Cannot start {}: {e}", canonical_exe.display()))?;
        // The real driver runs as native binaries; injected test fixtures are
        // scripts executed through an interpreter, so only the native path
        // verifies what the kernel executed. The verification reaps the child
        // and daemon on failure instead of returning a hash the process never
        // ran.
        let exe_sha256 = if path.is_none() {
            let daemon_pid = daemon.as_ref().and_then(Child::id);
            let mcp_pid = child.id();
            let Some((daemon_pid, mcp_pid)) = daemon_pid.zip(mcp_pid) else {
                reap_spawned(&mut child, daemon.as_mut()).await;
                return Err("cua-driver process id missing after spawn".into());
            };
            let identity = verify_running_executable(daemon_pid, &canonical_exe)
                .and_then(|_| verify_running_executable(mcp_pid, &canonical_exe));
            if let Err(err) = identity {
                reap_spawned(&mut child, daemon.as_mut()).await;
                return Err(err);
            }
            match running_executable_sha256(mcp_pid, &canonical_exe).await {
                Ok(sha256) => sha256,
                Err(err) => {
                    reap_spawned(&mut child, daemon.as_mut()).await;
                    return Err(err);
                }
            }
        } else {
            exe_sha256
        };
        let stdin = child.stdin.take().ok_or("driver stdin missing")?;
        let stdout = child.stdout.take().ok_or("driver stdout missing")?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut line = String::new();
                let mut reader = BufReader::new(&mut stderr);
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        tracing::warn!(target: "cua-driver-mcp", "{trimmed}");
                    }
                    line.clear();
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
            daemon,
            existing_profile_granted,
            initialize: InitializeInfo::default(),
            tools_list_cache: None,
            exe_path: exe,
            exe_canonical_path: canonical_exe,
            exe_sha256,
        })
    }

    async fn send(&mut self, value: Value) -> CuaResult<()> {
        let mut bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|e| e.to_string())
    }

    async fn request(&mut self, method: &str, params: Value) -> CuaResult<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
            .await?;
        loop {
            let message = read_frame(&mut self.reader, RESPONSE_LIMIT).await?;
            if message.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(err) = message.get("error") {
                return Err(err.to_string());
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| "MCP result missing".into());
        }
    }

    async fn handshake(&mut self) -> CuaResult<()> {
        let result = self
            .request(
                "initialize",
                json!({"protocolVersion": MCP_PROTOCOL_VERSION, "capabilities":{},
                "clientInfo":{"name":"noches", "version":env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
        self.initialize = parse_initialize_result(&result)?;
        self.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .await
    }

    /// Trusted handshake metadata for managed help/diagnostic results. The
    /// raw `instructions` prose stays in the driver struct; only a digest and
    /// byte count are exposed so untrusted server text never reaches prompts.
    fn initialize_metadata(&self) -> Value {
        let instructions = self.initialize.instructions.as_ref().map(|text| {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            json!({
                "present": true,
                "bytes": text.len(),
                "sha256": format!("{:x}", hasher.finalize()),
            })
        });
        json!({
            "protocolVersion": &self.initialize.protocol_version,
            "serverInfo": {
                "name": &self.initialize.server_info.name,
                "version": &self.initialize.server_info.version,
            },
            "capabilities": &self.initialize.capabilities,
            "instructions": instructions,
            "executable": {
                "path": self.exe_path.to_string_lossy(),
                "canonicalPath": self.exe_canonical_path.to_string_lossy(),
                "sha256": &self.exe_sha256,
            },
        })
    }

    async fn call(&mut self, action: &str, args: Value) -> CuaResult<Value> {
        self.request("tools/call", json!({"name":action,"arguments":args}))
            .await
    }

    async fn catalog(&mut self, filter: Option<&[String]>) -> CuaResult<Value> {
        let list = match &self.tools_list_cache {
            Some(list) => list.clone(),
            None => {
                let list = self.request("tools/list", json!({})).await?;
                if self.tools_list_cacheable() {
                    self.tools_list_cache = Some(list.clone());
                }
                list
            }
        };
        let tools: Vec<_> = match filter {
            Some(wanted) => {
                let available = list
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|tool| {
                        tool.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| ACTIONS.contains(&n))
                    })
                    .collect::<Vec<_>>();
                let mut ordered = Vec::new();
                for name in wanted {
                    if let Some(tool) = available
                        .iter()
                        .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
                        && !ordered
                            .iter()
                            .any(|t: &Value| t.get("name").and_then(Value::as_str) == Some(name))
                    {
                        ordered.push((*tool).clone());
                    }
                }
                ordered
            }
            None => list
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|tool| {
                    tool.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| ACTIONS.contains(&n))
                })
                .cloned()
                .collect(),
        };
        let tools = tools
            .into_iter()
            .filter(|tool| {
                // Describe can explain a disabled action, but help must not advertise it.
                filter.is_some()
                    || tool
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| {
                            name != "browser_prepare" && !BLOCKED_ACTIONS.contains(&name)
                        })
            })
            .map(managed_schema)
            .collect();
        let text = NON_DISRUPTIVE_POLICY;
        // Keep the tools/list top-level contract metadata (`schema_version`,
        // `capability_version`, enforcement inventory) alongside the filtered
        // managed tools; the negotiated contract is part of the result.
        let mut structured = match list {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        structured.remove("tools");
        structured.insert("tools".into(), Value::Array(tools));
        structured.insert("driver".into(), self.initialize_metadata());
        Ok(json!({"content":[{"type":"text","text":text}],
            "structuredContent": Value::Object(structured)}))
    }

    async fn kill(&mut self) -> CuaResult<()> {
        // Reap the mcp proxy, then the serve daemon that owns the overlay.
        // Either failing to die retains the lease.
        let mut children = vec![&mut self.child];
        children.extend(self.daemon.as_mut());
        for child in children {
            if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                child.start_kill().map_err(|e| e.to_string())?;
            }
            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .map_err(|_| "driver reap timed out".to_string())?
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn approval_question(device_id: &str, action: &str) -> String {
    format!(
        "Allow computer use on host {device_id} for this session? Requested action: {action}. One approval covers inspection, screenshots, clipboard reads and supported background input for the rest of this session. Physical focus, mouse and keyboard must remain untouched; foreground delivery and desktop input are forbidden even after approval. The driver may attach DevTools to an existing logged-in Chromium-family browser profile only if it is already configured. Browser setup and launch must be done manually.",
    )
}

fn remove_socket_if_present(path: &std::path::Path) -> CuaResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("Could not remove stale {}: {err}", path.display())),
    }
}

/// The dev GPUI app publishes an agent-seat compatibility marker at
/// `$XDG_RUNTIME_DIR/noches-gpui-input/<pid>`. Its payload line is one JSON
/// object `{"state":"ready"|"primary_client_busy","reason":..,"pid":..}`;
/// older builds wrote a bare state token, which still parses. The identity
/// header lines above the payload are ignored.
fn parse_agent_seat_marker(contents: &str) -> Option<Value> {
    let payload = contents
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    if payload.starts_with('{') {
        let value: Value = serde_json::from_str(payload).ok()?;
        value
            .get("state")
            .and_then(Value::as_str)
            .filter(|state| !state.trim().is_empty())?;
        Some(value)
    } else if matches!(payload, "ready" | "primary_client_busy") {
        // Legacy payload: the state token alone, without reason or pid.
        Some(json!({ "state": payload }))
    } else {
        None
    }
}

/// Reads the marker of the GPUI process a refused action targeted, so the
/// reason a client was not qualified survives into the caller's evidence.
fn agent_seat_marker(pid: u64) -> Option<Value> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    let contents =
        std::fs::read_to_string(runtime.join("noches-gpui-input").join(pid.to_string())).ok()?;
    parse_agent_seat_marker(&contents)
}

/// Enriches a refused window action with the target's agent-seat marker. A
/// background refusal then names whether the physical seat held the window
/// (`primary_client_busy`/`physical_seat_present`) or the target never
/// qualified (`no_qualified_target`), instead of only a driver-side token.
fn attach_seat_marker_evidence(mut result: Value, pid: Option<u64>) -> Value {
    if classify_driver_outcome(&result) == DriverOutcome::Refused
        && let Some(marker) = pid.and_then(agent_seat_marker)
        && let Some(structured) = result
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
    {
        structured.insert("agentSeatMarker".into(), marker);
    }
    result
}

fn resolve_driver_exe() -> CuaResult<PathBuf> {
    if let Some(path) = std::env::var_os("CUA_DRIVER_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err("Configured CUA_DRIVER_PATH is not a file".into());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let path = dir.join("cua-driver");
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".local/bin/cua-driver");
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("Install cua-driver or configure CUA_DRIVER_PATH on the engine host".into())
}

#[cfg(all(test, target_os = "linux"))]
#[path = "tests.rs"]
mod tests;

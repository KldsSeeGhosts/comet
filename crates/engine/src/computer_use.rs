//! Computer use owned by the engine. The Pi adapter only forwards tool calls.
//!
//! A lease covers one active turn, not the lifetime of a parked Pi process.
//! Interrupts, disconnected callers and turn boundaries cancel outstanding work
//! before reaping the private Linux driver and releasing that lease.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
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

pub struct ComputerUseManager {
    lease: Arc<Mutex<Option<String>>>,
    device_id: String,
    // Tests inject an executable rather than modifying the process environment.
    driver_path: Option<PathBuf>,
}

impl ComputerUseManager {
    pub fn new(device_id: String) -> Self {
        Self {
            lease: Arc::new(Mutex::new(None)),
            device_id,
            driver_path: None,
        }
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
        let state = Arc::new(BridgeState {
            owner: format!("chat {chat_id}, run {run_id}"),
            label: format!("noches-{}", uuid::Uuid::new_v4()),
            device_id: self.device_id.clone(),
            lease: self.lease.clone(),
            request_input,
            driver_path: self.driver_path.clone(),
            daemon_socket,
            turn: Mutex::new(Turn {
                active: true,
                cancel: Arc::new(CancellationToken::new()),
            }),
            stop: CancellationToken::new(),
            runtime: AsyncMutex::new(Runtime::default()),
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
    // One whole-turn grant covers inspection, input and desktop control.
    granted: bool,
    denied: bool,
    lease_held: bool,
}

struct BridgeState {
    owner: String,
    label: String,
    device_id: String,
    lease: Arc<Mutex<Option<String>>>,
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
        *runtime = Runtime::default();
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
    if runtime.granted {
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let question = format!(
        "Allow computer use on host {} for this turn? Requested action: {action}. One approval covers inspecting and controlling apps, pointer and keyboard input, screenshots and clipboard access on that host for the rest of this turn, including visible desktop control.",
        state.device_id
    );
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
    map.insert("session".into(), json!(state.label));
    let mut runtime = state.runtime.lock().await;
    if !state.active(turn) {
        return error("Computer use is parked or cancelled; start a new turn");
    }
    let metadata = matches!(action, "help" | "describe" | "health_report");
    if !metadata {
        if !runtime.lease_held {
            let mut lease = lock(&state.lease);
            if let Some(owner) = lease.as_ref() {
                return error(format!("Computer use is busy on this host: {owner}"));
            }
            *lease = Some(state.owner.clone());
            runtime.lease_held = true;
        }
        if let Err(err) = approve(state, &mut runtime, action).await {
            // A first-use denial must not reserve the desktop for a parked chat.
            if !runtime.granted {
                *lock(&state.lease) = None;
                runtime.lease_held = false;
            }
            return error(err);
        }
    }
    if runtime.driver.is_none() {
        match Driver::spawn(state.driver_path.as_ref(), &state.daemon_socket).await {
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
    let result = if action == "help" || action == "describe" {
        driver
            .catalog(if action == "describe" {
                args.get("name").and_then(Value::as_str)
            } else {
                None
            })
            .await
    } else {
        driver.call(action, args).await
    };
    match result {
        Ok(result) => result,
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
}

impl Driver {
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
        // CUA_DRIVER_RS_ENABLE_WAYLAND alone selects the production Hyprland
        // isolated-input lanes (cua-input-v3.sock) bound by the
        // cua-hyprland-plugin. Setting CUA_DRIVER_EXPERIMENTAL_HYPRLAND_INPUT
        // would switch the driver to the experimental test protocol and look
        // for cua-input-test.sock instead, so it must stay unset.
        command
            .env("CUA_DRIVER_PERMISSION_MODE", "standard")
            .env("CUA_DRIVER_RS_ENABLE_WAYLAND", "1");
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

    async fn spawn(path: Option<&PathBuf>, daemon_socket: &PathBuf) -> CuaResult<Self> {
        let exe = match path {
            Some(path) => path.clone(),
            None => resolve_driver_exe()?,
        };
        // The real driver runs as a per-run serve daemon plus an mcp proxy so
        // the agent cursor overlay has a UI runloop. Injected test fixtures
        // only implement stdio mcp, so they keep the single-process path.
        let (daemon, mcp_socket) = if path.is_none() {
            remove_socket_if_present(daemon_socket)?;
            let mut daemon = Self::base_command(&exe);
            daemon
                .arg("serve")
                .arg("--socket")
                .arg(daemon_socket)
                .arg("--permission-mode")
                .arg("standard")
                // serve owns the overlay; it does not read stdin.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            let mut daemon = daemon
                .spawn()
                .map_err(|e| format!("Cannot start {} serve: {e}", exe.display()))?;
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
            (Some(daemon), Some(daemon_socket.clone()))
        } else {
            (None, None)
        };
        let mut child = Self::mcp_command(&exe, mcp_socket.as_ref())
            .spawn()
            .map_err(|e| format!("Cannot start {}: {e}", exe.display()))?;
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
        self.request(
            "initialize",
            json!({"protocolVersion":"2024-11-05", "capabilities":{},
            "clientInfo":{"name":"noches", "version":env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        self.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .await
    }

    async fn call(&mut self, action: &str, args: Value) -> CuaResult<Value> {
        self.request("tools/call", json!({"name":action,"arguments":args}))
            .await
    }

    async fn catalog(&mut self, name: Option<&str>) -> CuaResult<Value> {
        if name.is_some_and(|name| !ACTIONS.contains(&name)) {
            return Err("Action is not exposed by Noches".into());
        }
        let list = self.request("tools/list", json!({})).await?;
        let tools: Vec<_> = list
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| ACTIONS.contains(&n) && name.is_none_or(|wanted| n == wanted))
            })
            .cloned()
            .collect();
        Ok(
            json!({"content":[{"type":"text","text":"Available managed computer-use tools. Use describe with args.name for a schema."}],
            "structuredContent":{"tools":tools}}),
        )
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

fn remove_socket_if_present(path: &std::path::Path) -> CuaResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("Could not remove stale {}: {err}", path.display())),
    }
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
mod tests;

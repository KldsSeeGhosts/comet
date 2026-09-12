//! Real installed Pi, with only the model's HTTP endpoint replaced.
//! Run with `bash scripts/tests/run-pi-continuity.sh`; never point this at a user profile.
#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Instant, sleep, timeout},
};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{
    PiHarness,
    provider_history::{NativeHistory, parse_history},
};
use zeron_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, TerminalEvent,
};

const CHAT: &str = "installed-pi-continuity";
const MODEL: &str = "continuity/continuity-model";
const PROMPTS: [&str; 3] = [
    "Remember amber.",
    "Remember cobalt too.",
    "Recall both words.",
];
const ANSWERS: [&str; 3] = [
    "FIRST amber",
    "SECOND amber cobalt",
    "THIRD amber cobalt confirmed",
];
const DEADLINE: Duration = Duration::from_secs(45);

fn conversation(turns: usize, pending: bool) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for turn in 0..turns {
        result.push(("user".into(), PROMPTS[turn].into()));
        if !pending || turn + 1 < turns {
            result.push(("assistant".into(), ANSWERS[turn].into()));
        }
    }
    result
}

fn content_text(value: &Value) -> Result<String> {
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    value
        .as_array()
        .context("text content must be a string or blocks")?
        .iter()
        .map(|block| {
            ensure!(block["type"] == "text", "unexpected content block: {block}");
            Ok(block["text"]
                .as_str()
                .context("text block without text")?
                .to_owned())
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.concat())
}

struct ModelServer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    second_started: Arc<tokio::sync::Notify>,
    release_second: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl ModelServer {
    async fn start(root: &Path) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let second_started = Arc::new(tokio::sync::Notify::new());
        let release_second = Arc::new(tokio::sync::Notify::new());
        let started = second_started.clone();
        let release = release_second.clone();
        let root = root.to_owned();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await?;
                // A stray local prober (health checks, port scanners) must not
                // kill the accept loop. Serve it and keep accepting; only a
                // real model request drives `requests`.
                timeout(DEADLINE, async {
                    let mut bytes = Vec::new();
                    // First, classify the request. Any read error or malformed
                    // request on a stray connection is swallowed so the accept
                    // loop keeps serving the real model calls.
                    let header_end = loop {
                        let mut buffer = [0; 8192];
                        let n = match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return Ok::<_, anyhow::Error>(()),
                            Ok(n) => n,
                        };
                        bytes.extend_from_slice(&buffer[..n]);
                        if bytes.len() > 2 * 1024 * 1024 { return Ok(()); }
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end;
                        }
                    };
                    let header = match std::str::from_utf8(&bytes[..header_end]) {
                        Ok(header) => header,
                        Err(_) => return Ok(()),
                    };
                    if !header.starts_with("POST /v1/chat/completions HTTP/1.1\r\n") {
                        eprintln!("mock server: ignoring non-completions request: {}", header.lines().next().unwrap_or_default());
                        let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        let _ = socket.shutdown().await;
                        return Ok(());
                    }
                    let length: usize = header.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length").then_some(value.trim())
                    }).context("missing Content-Length")?.parse()?;
                    ensure!(length <= 2 * 1024 * 1024, "mock body exceeded limit");
                    let header_end = header_end + 4;
                    while bytes.len() < header_end + length {
                        let mut buffer = [0; 8192];
                        let n = socket.read(&mut buffer).await?;
                        ensure!(n > 0, "HTTP body ended early");
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    let request: Value = serde_json::from_slice(&bytes[header_end..header_end + length])?;
                    let turn = {
                        let mut requests = recorded.lock().unwrap();
                        requests.push(request.clone());
                        std::fs::write(root.join("model-requests.json"), serde_json::to_vec_pretty(&*requests)?)?;
                        requests.len()
                    };
                    ensure!(turn <= 3, "unexpected duplicate model request {turn}");
                    ensure!(request["model"] == "continuity-model", "wrong model: {request}");
                    ensure!(request["reasoning_effort"] == "medium", "effort lost: {request}");
                    ensure!(request["stream"] == true, "Pi did not request SSE");
                    let messages = request["messages"].as_array().context("missing model messages")?.iter()
                        .filter(|m| m["role"] != "system" && m["role"] != "developer")
                        .map(|m| Ok((m["role"].as_str().context("missing role")?.to_owned(), content_text(&m["content"])?)))
                        .collect::<Result<Vec<_>>>()?;
                    ensure!(messages == conversation(turn, true), "turn {turn} lost or duplicated context: {messages:?}");
                    if turn == 2 { started.notify_one(); release.notified().await; }
                    let chunk = json!({"id":format!("mock-{turn}"), "object":"chat.completion.chunk", "created":1,
                        "model":"continuity-model", "choices":[{"index":0,"delta":{"role":"assistant","content":ANSWERS[turn-1]},"finish_reason":null}]});
                    let finish = json!({"id":format!("mock-{turn}"), "object":"chat.completion.chunk", "created":1,
                        "model":"continuity-model", "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}});
                    let body = format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                    socket.shutdown().await?;
                    eprintln!("model turn {turn}: exact incoming history verified; {}", ANSWERS[turn-1]);
                    Ok::<_, anyhow::Error>(())
                }).await.context("mock HTTP timeout")??;
            }
        });
        Ok(Self {
            url,
            requests,
            second_started, release_second,
            task,
        })
    }
}

impl Drop for ModelServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn config() -> ChatConfig {
    ChatConfig {
        harness: HarnessId::Pi,
        model: Some(MODEL.into()),
        reasoning: Some(ReasoningLevel::Medium),
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
    }
}

fn request(cwd: &str, turn: usize) -> RunRequest {
    RunRequest {
        prompt: PROMPTS[turn - 1].into(),
        harness: Some(HarnessId::Pi),
        model: Some(MODEL.into()),
        reasoning: Some(ReasoningLevel::Medium),
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume: None,
        attachments: vec![],
        worktree: None,
    }
}

fn entries(core: &EngineCore) -> Result<Vec<SessionMessageEntry>> {
    Ok(core.doc_host.open(CHAT)?.doc().read_entries()?)
}

fn assert_entries(core: &EngineCore, turns: usize) -> Result<Vec<SessionMessageEntry>> {
    let entries = entries(core)?;
    let actual = entries
        .iter()
        .map(|entry| {
            ensure!(
                entry.status == Some(MessageStatus::Complete),
                "incomplete Chat entry: {entry:?}"
            );
            ensure!(
                entry.device_id == core.device_id,
                "Chat entry device changed"
            );
            let text = entry
                .parts
                .iter()
                .map(|part| match part {
                    MessagePart::Text { text, .. } => Ok(text.clone()),
                    part => bail!("unexpected Chat part: {part:?}"),
                })
                .collect::<Result<Vec<_>>>()?
                .concat();
            Ok((
                if entry.role == MessageRole::User {
                    "user"
                } else {
                    "assistant"
                }
                .to_owned(),
                text,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        actual == conversation(turns, false),
        "Chat transcript mismatch: {actual:?}"
    );
    ensure!(
        entries.iter().map(|e| &e.id).collect::<HashSet<_>>().len() == entries.len(),
        "duplicate Chat ids"
    );
    Ok(entries)
}

fn native(path: &Path, turns: usize, cwd: &str) -> Result<NativeHistory> {
    let raw = std::fs::read_to_string(path)?;
    let history = parse_history(HarnessId::Pi, &raw)?;
    ensure!(history.cwd == cwd, "native cwd changed");
    ensure!(
        history.model.as_deref() == Some(MODEL),
        "native model changed: {:?}",
        history.model
    );
    ensure!(
        history.reasoning == Some(ReasoningLevel::Medium),
        "native effort changed"
    );
    let actual = history
        .messages
        .iter()
        .map(|m| {
            ensure!(!m.aborted, "native turn aborted: {m:?}");
            Ok((m.role.clone(), content_text(&m.content)?))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        actual == conversation(turns, false),
        "native history mismatch: {actual:?}"
    );
    let records = raw
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        records.iter().filter(|r| r["type"] == "session").count() == 1,
        "duplicate session header"
    );
    ensure!(
        records.iter().filter(|r| r["type"] == "message").count() == turns * 2,
        "duplicate or branched native messages"
    );
    Ok(history)
}

async fn wait_native(path: &Path, turns: usize, cwd: &str) -> Result<NativeHistory> {
    let start = Instant::now();
    loop {
        match native(path, turns, cwd) {
            Ok(history) => return Ok(history),
            Err(error) if start.elapsed() >= DEADLINE => {
                return Err(error).context("waiting for complete native history");
            }
            Err(_) => sleep(Duration::from_millis(50)).await,
        }
    }
}

async fn rpc_turn(core: &EngineCore, root: &Path, cwd: &str, turn: usize) -> Result<String> {
    let (_, mut events) = core.sessions.subscribe(CHAT, 0)?;
    core.sessions
        .dispatch(CHAT, HarnessId::Pi, request(cwd, turn), None)
        .await?;
    let mut log = Vec::new();
    let mut text = String::new();
    let mut started = None;
    timeout(DEADLINE, async {
        loop {
            let event = events.recv().await?.event;
            log.push(serde_json::to_value(&event)?);
            std::fs::write(
                root.join(format!("rpc-turn-{turn}.json")),
                serde_json::to_vec_pretty(&log)?,
            )?;
            match event {
                AgentEvent::SessionStarted {
                    harness,
                    session_id,
                    model,
                    cwd: actual_cwd,
                    ..
                } => {
                    ensure!(
                        harness == HarnessId::Pi && model == MODEL && actual_cwd == cwd,
                        "SessionStarted identity/config mismatch"
                    );
                    started = Some(session_id);
                }
                AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                AgentEvent::Error { message } => bail!("Pi RPC error: {message}"),
                AgentEvent::Done { status, .. } => {
                    ensure!(status == DoneStatus::Completed, "Pi RPC ended {status:?}");
                    break;
                }
                _ => {}
            }
        }
        loop {
            if core
                .sessions
                .session_status(CHAT)
                .is_some_and(|s| s.status == SessionStatus::Idle)
                && !core.sessions.turn_in_flight(CHAT)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("waiting for Pi RPC Done and true engine idle")??;
    ensure!(
        text == ANSWERS[turn - 1],
        "RPC turn {turn} output mismatch: {text:?}"
    );
    let session = started.context("Pi did not emit SessionStarted")?;
    let chat = core.workspace.chat(CHAT)?.context("Chat disappeared")?;
    ensure!(
        chat.harness_session_id.as_deref() == Some(&session),
        "native identity not recorded"
    );
    ensure!(
        chat.harness_session_cwd.as_deref() == Some(cwd),
        "native cwd not recorded"
    );
    ensure!(
        Path::new(&session).starts_with(root.join("agent/sessions"))
            || Path::new(&session).starts_with(root.join("engine/orgs/org/user/native-sessions")),
        "session escaped isolated Pi directory: {session}"
    );
    eprintln!("RPC turn {turn}: true idle, output {text:?}, native {session}");
    Ok(session)
}

async fn blank_pi_cli_round_trip(core: &EngineCore, root: &Path, cwd: &str, server: &ModelServer) -> Result<String> {
    let chat_id = CHAT;
    let views = core.sessions.session_views();
    let mut expected_session = None;
    for cycle in 0..2 {
        let terminal = views.open(core.sessions.clone(), core.workspace.clone(), core.terminals.clone(), chat_id.into(), 120, 40).await
            .context("open blank Pi CLI without a prompt")?;
        let result = async {
            ensure!(terminal.cwd == cwd, "blank CLI lost chosen cwd");
            let chat = core.workspace.chat(chat_id)?.context("blank chat missing")?;
            let session = chat.harness_session_id.context("blank chat has no persisted native identity")?;
            ensure!(Path::new(&session).starts_with(root.join("engine")), "blank session escaped isolated engine profile");
            if let Some(expected) = &expected_session {
                ensure!(&session == expected, "reopening blank CLI changed native session");
            } else {
                expected_session = Some(session.clone());
            }
            let mut output = core.terminals.subscribe(&terminal.id, None)?;
            let mut pty = Vec::new();
            timeout(DEADLINE, async {
                while let Some(event) = output.recv().await {
                    match event {
                        TerminalEvent::Data { data, .. } => {
                            pty.extend(STANDARD.decode(data)?);
                            std::fs::write(root.join(format!("blank-pty-{cycle}.bin")), &pty)?;
                            if String::from_utf8_lossy(&pty).contains("continuity-model") { return Ok(()); }
                        }
                        TerminalEvent::Exit { .. } => bail!("blank Pi PTY exited before showing configured model"),
                    }
                }
                bail!("blank Pi output closed before ready")
            }).await.context("waiting for real blank Pi TUI")??;
            timeout(DEADLINE, async {
                loop {
                    if serde_json::to_value(views.get(chat_id))?["nativeActivity"] == "idle" { return Ok::<_, anyhow::Error>(()); }
                    sleep(Duration::from_millis(10)).await;
                }
            }).await.context("waiting for blank Pi authenticated idle")??;
            let outcome = views.close(core.sessions.clone(), core.workspace.clone(), core.doc_host.clone(), core.terminals.clone(), chat_id.into()).await
                .context("return blank Pi CLI to Chat")?;
            ensure!(outcome.imported_messages == 0, "blank CLI manufactured chat messages");
            ensure!(serde_json::to_value(&outcome.view)?["owner"] == "chat", "blank Chat ownership not restored");
            let doc = core.doc_host.open(chat_id)?;
            ensure!(doc.doc().read_entries()?.is_empty() && doc.doc().read_commands()?.is_empty(), "blank round trip created a prompt or command");
            let chat = core.workspace.chat(chat_id)?.context("blank Chat missing after close")?;
            ensure!(chat.config == Some(config()) && chat.cwd.as_deref() == Some(cwd), "blank round trip lost config or cwd");
            ensure!(chat.harness_session_id.as_deref() == Some(&session) && chat.harness_session_cwd.as_deref() == Some(cwd), "blank round trip lost native identity");
            let history = parse_history(HarnessId::Pi, &std::fs::read_to_string(&session)?)?;
            ensure!(history.messages.is_empty() && history.cwd == cwd, "blank native history changed");
            ensure!(history.model.as_deref() == Some(MODEL) && history.reasoning == Some(ReasoningLevel::Medium), "blank native config changed");
            ensure!(server.requests.lock().unwrap().is_empty(), "blank CLI made a model request");
            std::fs::copy(&session, root.join(format!("blank-native-{cycle}.jsonl")))?;
            Ok::<_, anyhow::Error>(())
        }.await;
        let _ = core.terminals.terminate_and_wait(&terminal.id).await;
        result?;
    }
    eprintln!("Blank Pi: real CLI opened twice and returned to empty Chat with the same native identity, cwd, model and effort; zero model requests");
    expected_session.context("blank Pi never recorded a native session")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a real installed Pi executable; run scripts/tests/run-pi-continuity.sh"]
async fn installed_pi_chat_cli_chat_continuity() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("PI_CONTINUITY_ROOT").context("use scripts/tests/run-pi-continuity.sh")?,
    )
    .canonicalize()?;
    ensure!(
        root.starts_with(std::env::temp_dir()),
        "fixture root must be temporary"
    );
    for (key, suffix) in [
        ("HOME", "home"),
        ("PI_CODING_AGENT_DIR", "agent"),
        ("ZERON_DATA_DIR", "engine"),
    ] {
        ensure!(
            std::env::var_os(key).context(key)? == root.join(suffix).as_os_str(),
            "unsafe {key}; use isolated runner"
        );
    }
    let wrapper =
        PathBuf::from(std::env::var_os("PI_EXECUTABLE").context("missing isolated Pi wrapper")?);
    ensure!(
        wrapper == root.join("pi-wrapper"),
        "PI_EXECUTABLE must be the fixture wrapper"
    );
    let mut server = ModelServer::start(&root).await?;
    std::fs::write(
        root.join("agent/models.json"),
        serde_json::to_vec_pretty(&json!({"providers":{"continuity":{
            "baseUrl":server.url, "api":"openai-completions", "apiKey":"local-test-only",
            "compat":{"supportsDeveloperRole":false,"supportsReasoningEffort":true},
            "models":[{"id":"continuity-model","reasoning":true,"contextWindow":128000,"maxTokens":1024}]
        }}}))?,
    )?;
    std::fs::write(
        root.join("agent/settings.json"),
        serde_json::to_vec_pretty(&json!({
            "defaultProvider":"continuity", "defaultModel":"continuity-model", "defaultThinkingLevel":"medium",
            "enableInstallTelemetry":false, "compaction":{"enabled":false}, "retry":{"enabled":false},
            "packages":[], "extensions":[], "skills":[], "prompts":[], "quietStartup":true
        }))?,
    )?;
    let cwd = root.join("worktree").to_string_lossy().into_owned();
    std::fs::create_dir_all(&cwd)?;
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(PiHarness::new().with_executable(wrapper)));
    let core = EngineCore::assemble_with_identity(
        &root.join("engine"),
        registry,
        HarnessId::Pi,
        None,
        "org",
        "user",
    )?;
    core.workspace.create_chat(
        CHAT,
        None,
        Some(&core.device_id),
        Some(config()),
        Some(cwd.clone()),
    )?;
    // Naming is outside this test; prevent the engine's auto-titler making extra model calls.
    core.workspace
        .rename_chat(CHAT, "Pi continuity integration")?;
    let views = core.sessions.session_views();
    let mut terminal_id = None;
    let result = async {
        let blank_session = blank_pi_cli_round_trip(&core, &root, &cwd, &server).await?;
        let session = rpc_turn(&core, &root, &cwd, 1).await?;
        ensure!(session == blank_session, "first Chat prompt forked the blank CLI session");
        let baseline = wait_native(Path::new(&session), 1, &cwd).await?;
        let first_entries = assert_entries(&core, 1)?;
        std::fs::copy(&session, root.join("native-after-chat-1.jsonl"))?;
        let terminal = views.open(core.sessions.clone(), core.workspace.clone(), core.terminals.clone(), CHAT.into(), 120, 40).await.context("OpenSessionTerminal")?;
        terminal_id = Some(terminal.id.clone());
        ensure!(terminal.cwd == cwd, "PTY cwd changed");
        ensure!(serde_json::to_value(views.get(CHAT))?["owner"] == "cli", "CLI did not acquire ownership");
        let mut output = core.terminals.subscribe(&terminal.id, None)?;
        let mut pty = Vec::new();
        timeout(DEADLINE, async {
            while let Some(event) = output.recv().await {
                match event {
                    TerminalEvent::Data { data, .. } => {
                        pty.extend(STANDARD.decode(data)?);
                        std::fs::write(root.join("pty-output.bin"), &pty)?;
                        let visible = String::from_utf8_lossy(&pty);
                        if visible.contains(ANSWERS[0]) && visible.contains("continuity-model") { return Ok(()); }
                    }
                    TerminalEvent::Exit { .. } => bail!("Pi PTY exited before showing resumed history"),
                }
            }
            bail!("Pi PTY output closed before ready")
        }).await.context("waiting for real Pi TUI resumed transcript and model footer")??;
        eprintln!("OpenSessionTerminal: real PTY showed first assistant and configured model");
        core.terminals.write(&terminal.id, &STANDARD.encode(format!("{}\r", PROMPTS[1])))?;
        let artifact = root.join("pty-output.bin");
        let drain = tokio::spawn(async move {
            while let Some(event) = output.recv().await {
                if let TerminalEvent::Data { data, .. } = event {
                    pty.extend(STANDARD.decode(data)?);
                    std::fs::write(&artifact, &pty)?;
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        timeout(DEADLINE, server.second_started.notified()).await?;
        let before = serde_json::to_value(views.get(CHAT))?;
        ensure!(before["nativeActivity"] == "busy", "native model request was not marked busy: {before}");
        let refused = views.close(core.sessions.clone(), core.workspace.clone(), core.doc_host.clone(), core.terminals.clone(), CHAT.into()).await;
        ensure!(refused.is_err_and(|e| e.to_string().contains("Session is busy")), "busy native close was not refused");
        ensure!(serde_json::to_value(views.get(CHAT))?["owner"] == "cli", "busy refusal changed ownership");
        core.terminals.write(&terminal.id, &STANDARD.encode(b""))
            .context("busy refusal closed the PTY")?;
        ensure!(entries(&core)? == first_entries, "busy refusal hydrated incomplete history");
        eprintln!("Native busy close refused before teardown; original CLI ownership and Chat history preserved");
        server.release_second.notify_one();
        let second = wait_native(Path::new(&session), 2, &cwd).await?;
        timeout(DEADLINE, async {
            loop {
                if serde_json::to_value(views.get(CHAT))?["nativeActivity"] == "idle" { return Ok::<_, anyhow::Error>(()); }
                sleep(Duration::from_millis(10)).await;
            }
        }).await.context("waiting for authenticated agent_settled")??;
        ensure!(second.identity == baseline.identity, "CLI forked the native session");
        ensure!(second.messages.starts_with(&baseline.messages), "CLI rewrote native prefix");
        std::fs::copy(&session, root.join("native-after-cli-2.jsonl"))?;
        let outcome = views.close(core.sessions.clone(), core.workspace.clone(), core.doc_host.clone(), core.terminals.clone(), CHAT.into()).await.context("CloseSessionTerminal")?;
        timeout(DEADLINE, drain).await.context("waiting for PTY exit")???;
        ensure!(outcome.imported_messages == 2, "wrong imported message count: {outcome:?}");
        ensure!(outcome.native_session_id.as_deref() == Some(&session), "hydration changed native path");
        ensure!(serde_json::to_value(&outcome.view)?["owner"] == "chat", "Chat ownership not restored");
        let hydrated = assert_entries(&core, 2)?;
        ensure!(hydrated.starts_with(&first_entries), "hydration rewrote original Chat entries");
        let chat = core.workspace.chat(CHAT)?.context("hydrated Chat missing")?;
        ensure!(chat.config == Some(config()) && chat.cwd.as_deref() == Some(&cwd), "hydration lost model/effort/cwd: {chat:?}");
        ensure!(chat.harness_session_id.as_deref() == Some(&session) && chat.harness_session_cwd.as_deref() == Some(&cwd), "hydration lost native identity");
        std::fs::write(root.join("hydrated-chat.json"), serde_json::to_vec_pretty(&json!({"chat":chat,"entries":hydrated,"outcome":outcome}))?)?;
        let again = views.close(core.sessions.clone(), core.workspace.clone(), core.doc_host.clone(), core.terminals.clone(), CHAT.into()).await?;
        ensure!(again.imported_messages == 0 && entries(&core)? == hydrated, "repeated close duplicated Chat history");
        eprintln!("CloseSessionTerminal: exactly 2 imported messages; identity/cwd/model/medium preserved; repeated close imported 0");
        let third_session = rpc_turn(&core, &root, &cwd, 3).await?;
        ensure!(third_session == session, "third Chat turn started a different native session");
        let third = wait_native(Path::new(&session), 3, &cwd).await?;
        ensure!(third.identity == baseline.identity && third.messages.starts_with(&second.messages), "third Chat turn rewrote native history");
        assert_entries(&core, 3)?;
        std::fs::copy(&session, root.join("native-after-chat-3.jsonl"))?;
        ensure!(server.requests.lock().unwrap().len() == 3, "expected exactly three model requests");
        std::fs::write(root.join("final-chat.json"), serde_json::to_vec_pretty(&entries(&core)?)?)?;
        Ok::<_, anyhow::Error>(())
    }.await;
    std::fs::write(root.join("outcome.txt"), format!("{result:#?}\n"))?;
    eprintln!("Pi continuity artifacts: {}", root.display());
    if let Err(error) = &result {
        eprintln!("Continuity failure: {error:#}");
    }
    // Always reap children, including on an assertion failure, without hiding its cause.
    if let Some(id) = terminal_id {
        let _ = core.terminals.terminate_and_wait(&id).await;
    }
    core.sessions.shutdown().await;
    core.doc_host.shutdown_workers().await;
    core.sessions.clear_doc_host();
    if server.task.is_finished() {
        let failure = (&mut server.task)
            .await
            .context("model server task panicked")?;
        failure.context("deterministic model request validation failed")?;
    }
    result
}

use crate::{
    audio::Audio,
    protocol::{ToolBatches, tool_output},
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt, stream::FuturesUnordered};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

static CALL_ACTIVE: AtomicBool = AtomicBool::new(false);

// Deliberately not Debug or Serialize: the credential never belongs in logs/config.
pub struct CallConfig {
    pub api_key: String,
    pub backend_model: String,
    pub voice: String,
    pub tools: Vec<Value>,
    pub instructions: String,
}
impl CallConfig {
    pub fn new(api_key: String, tools: Vec<Value>, instructions: String) -> Self {
        Self {
            api_key,
            tools,
            instructions,
            backend_model: "gpt-5.6-luna".into(),
            voice: "marin".into(),
        }
    }
    fn session(&self) -> Value {
        json!({"type":"session.start", "session": {
            "model":"gpt-live-1", "store":false,
            "instructions":"You are the voice assistant inside Noches, a desktop app for coding agents. Keep speech brief and natural. Delegate every question about app state and every app action to the backend. Report only confirmed results. Never invent what is on screen or read code, identifiers, or secrets aloud.",
            "audio":{"format":{"type":"audio/pcm","rate":24000},"output":{"voice":self.voice}},
            "delegation":{"type":"responses","responses":{
                "model":self.backend_model,"instructions":self.instructions,
                "tools":self.tools,"tool_choice":"auto","parallel_tool_calls":false
            }}
        }})
    }
}

pub enum Command {
    Mute(bool),
    Context(String),
}
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    /// A closed receiver means the originating call has ended; do not execute.
    pub reply: oneshot::Sender<Value>,
}
pub enum Event {
    Started,
    Transcript {
        user: bool,
        delta: String,
    },
    Tool(ToolCall),
    Usage(Value),
    /// None means a clean close or cancelled setup. Errors are safe UI text.
    Ended(Option<String>),
}
pub struct Call {
    stop: watch::Sender<bool>,
    commands: mpsc::Sender<Command>,
    cancelled: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
}
impl Drop for Call {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Call {
    pub fn start(config: CallConfig) -> Result<(Self, mpsc::Receiver<Event>)> {
        if config.api_key.trim().is_empty() {
            bail!("Add an OpenAI API key before starting voice");
        }
        if CALL_ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!("A voice call is already active in another Noches window");
        }
        let (stop, stopped) = watch::channel(false);
        let (commands, rx) = mpsc::channel(16);
        let (events, events_rx) = mpsc::channel(128);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let muted = Arc::new(AtomicBool::new(false));
        let audio_muted = muted.clone();
        let thread = std::thread::Builder::new()
            .name("noches-voice".into())
            .spawn(move || {
                struct Release;
                impl Drop for Release {
                    fn drop(&mut self) {
                        CALL_ACTIVE.store(false, Ordering::SeqCst);
                    }
                }
                let release = Release;
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        runtime.block_on(run(
                            config,
                            stopped,
                            rx,
                            events.clone(),
                            cancel,
                            audio_muted,
                        ))
                    });
                drop(release);
                let _ = events.blocking_send(Event::Ended(result.err().map(|e| e.to_string())));
            });
        if let Err(error) = thread {
            CALL_ACTIVE.store(false, Ordering::SeqCst);
            return Err(error.into());
        }
        Ok((
            Self {
                stop,
                commands,
                cancelled,
                muted,
            },
            events_rx,
        ))
    }
    pub fn stop(&self) {
        self.muted.store(true, Ordering::SeqCst);
        self.cancelled.store(true, Ordering::SeqCst);
        let _ = self.stop.send(true);
    }
    pub fn is_stopping(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub fn command(&self, command: Command) -> Result<()> {
        if self.is_stopping() {
            bail!("Voice call is ending");
        }
        let requested_mute = match &command {
            Command::Mute(value) => Some(*value),
            _ => None,
        };
        // Mute locally before any network work. On enqueue failure, stay muted.
        if requested_mute == Some(true) {
            self.muted.store(true, Ordering::SeqCst);
        }
        self.commands
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("Voice connection is busy or closed"))?;
        if requested_mute == Some(false) {
            self.muted.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
}

async fn run(
    config: CallConfig,
    mut stop: watch::Receiver<bool>,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    cancelled: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
) -> Result<()> {
    let mut request = "wss://api.openai.com/v1/live/sessions".into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", config.api_key.trim())
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid API key"))?,
    );
    // Do not format handshake errors: a request can contain the Authorization header.
    let (socket, _) = tokio::select! {
        _ = stop.changed() => return Ok(()),
        result = timeout(Duration::from_secs(20), connect_async(request)) =>
            result.context("Voice connection timed out")?.map_err(|_| anyhow::anyhow!("Cannot connect to GPT-Live. Check your OpenAI API key, model access, and network."))?,
    };
    let (mut write, mut read) = socket.split();
    timeout(
        Duration::from_secs(5),
        write.send(Message::Text(config.session().to_string())),
    )
    .await??;
    drop(config);
    let mut audio = None;
    let mut microphone: Option<mpsc::Receiver<Vec<u8>>> = None;
    let mut batches = ToolBatches::default();
    let mut tools = FuturesUnordered::new();
    let mut ready = false;
    let mut closing = false;
    let mut deadline = Instant::now() + Duration::from_secs(20);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            biased;
            _ = stop.changed(), if !closing => {
                closing = true;
                // Revoke pending tools and release hardware immediately on hangup.
                tools.clear(); audio = None; microphone = None;
                if !ready { return Ok(()); }
                deadline = Instant::now() + Duration::from_secs(5);
                timeout(Duration::from_secs(2), write.send(Message::Text(json!({"type":"session.close"}).to_string()))).await??;
            }
            _ = tick.tick() => {
                if (!ready || closing) && Instant::now() >= deadline {
                    bail!(if closing { "Voice ended without final usage confirmation" } else { "GPT-Live did not start the session" });
                }
                if audio.as_ref().is_some_and(|a: &Audio| a.failed.load(Ordering::Relaxed)) {
                    bail!("Audio device disconnected or capture fell behind. Reconnect the call.");
                }
            }
            incoming = read.next() => {
                let Some(incoming) = incoming else { bail!("Voice disconnected without final usage confirmation"); };
                let message = incoming.context("Voice connection lost")?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(_) => bail!("Voice disconnected without final usage confirmation"),
                    Message::Ping(data) => { timeout(Duration::from_secs(2), write.send(Message::Pong(data))).await??; continue; }
                    _ => continue,
                };
                let event: Value = serde_json::from_str(&text).context("Invalid voice service event")?;
                match event["type"].as_str().unwrap_or("") {
                    "session.started" if !ready && !closing => {
                        let (device, rx) = Audio::open(muted.clone(), cancelled.clone())?;
                        audio = Some(device); microphone = Some(rx); ready = true;
                        emit(&events, Event::Started)?;
                    }
                    "session.output_audio.delta" if !closing => {
                        let bytes = STANDARD.decode(event["delta"].as_str().unwrap_or("")).context("Invalid voice audio")?;
                        if let Some(audio) = audio.as_mut() { audio.play(&bytes)?; }
                    }
                    "session.input_transcript.delta" | "session.output_transcript.delta" if !closing => {
                        emit(&events, Event::Transcript { user: event["type"] == "session.input_transcript.delta",
                            delta: event["delta"].as_str().unwrap_or("").into() })?;
                    }
                    "session.usage.updated" => emit(&events, Event::Usage(event["usage"].clone()))?,
                    "session.closed" => { emit(&events, Event::Usage(event["usage"].clone()))?; return Ok(()); }
                    "error" => {
                        // Never expose service echoes of prompts/credentials in the UI or logs.
                        bail!("GPT-Live rejected a command. Check model access and voice configuration, then reconnect.");
                    }
                    _ => {}
                }
                if !closing && let Some(batch) = batches.accept(&event)? {
                    let events = events.clone(); let cancelled = cancelled.clone();
                    tools.push(async move {
                        let mut results = Vec::new();
                        for call in batch {
                            if cancelled.load(Ordering::SeqCst) { break; }
                            let (reply, result) = oneshot::channel();
                            events.send(Event::Tool(ToolCall {name:call.name, arguments:call.arguments, reply})).await
                                .map_err(|_| anyhow::anyhow!("Noches window closed"))?;
                            let result = timeout(Duration::from_secs(60), result).await;
                            let output = match result {
                                Ok(Ok(value)) => value,
                                _ => json!({"error":"Action did not return a result. Its outcome is unknown; inspect state before retrying."}),
                            };
                            results.push(tool_output(&call.id, output));
                        }
                        Ok::<_, anyhow::Error>(results)
                    });
                }
            }
            Some(results) = tools.next(), if !tools.is_empty() && !closing => {
                for result in results? { timeout(Duration::from_secs(2), write.send(Message::Text(result.to_string()))).await??; }
                timeout(Duration::from_secs(2), write.send(Message::Text(json!({"type":"response.create"}).to_string()))).await??;
            }
            Some(command) = commands.recv(), if ready && !closing => {
                let event = match command {
                    Command::Mute(value) => {
                        // The Call handle already changed the local capture gate synchronously.
                        json!({"type":if value {"session.input_audio.mute"} else {"session.input_audio.unmute"}})
                    }
                    Command::Context(content) => json!({"type":"session.thinking.append", "delegation_id":null,"content":content}),
                };
                timeout(Duration::from_secs(2), write.send(Message::Text(event.to_string()))).await??;
            }
            Some(mut packet) = async { match microphone.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } }, if ready && !closing => {
                if muted.load(Ordering::SeqCst) { packet.fill(0); }
                timeout(Duration::from_secs(2), write.send(Message::Text(json!({"type":"session.input_audio.append","audio":STANDARD.encode(packet)}).to_string()))).await??;
            }
        }
    }
}
fn emit(events: &mpsc::Sender<Event>, event: Event) -> Result<()> {
    events
        .try_send(event)
        .map_err(|_| anyhow::anyhow!("Voice UI is unavailable or fell behind"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mute_and_hangup_close_local_audio_gates_even_with_a_full_command_queue() {
        let (stop, _stopped) = watch::channel(false);
        let (commands, _commands_rx) = mpsc::channel(1);
        commands
            .try_send(Command::Context("queued".into()))
            .unwrap();
        let call = Call {
            stop,
            commands,
            cancelled: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
        };
        assert!(call.command(Command::Mute(true)).is_err());
        assert!(call.muted.load(Ordering::SeqCst));
        assert!(call.command(Command::Mute(false)).is_err());
        assert!(call.muted.load(Ordering::SeqCst));
        call.stop();
        assert!(call.is_stopping());
        assert!(call.command(Command::Mute(false)).is_err());
    }
    #[test]
    fn session_uses_live_delegation_without_serializing_credentials() {
        let config = CallConfig::new(
            "secret-key-do-not-persist".into(),
            vec![json!({"type":"function","name":"get_context"})],
            "Use app tools".into(),
        );
        let event = config.session();
        assert_eq!(event["type"], "session.start");
        assert_eq!(event["session"]["model"], "gpt-live-1");
        assert_eq!(event["session"]["store"], false);
        assert_eq!(
            event["session"]["delegation"]["responses"]["parallel_tool_calls"],
            false
        );
        assert!(!event.to_string().contains("secret-key-do-not-persist"));
        assert_eq!(event["session"]["audio"]["format"]["rate"], 24_000);
    }
    #[test]
    fn invalid_credentials_fail_before_network_access_without_echoing_them() {
        let (call, mut events) = Call::start(CallConfig::new(
            "private\ninvalid-header".into(),
            vec![],
            String::new(),
        ))
        .unwrap();
        let event = events.blocking_recv().unwrap();
        let Event::Ended(Some(error)) = event else {
            panic!("expected safe error")
        };
        assert_eq!(error, "Invalid API key");
        drop(call);
    }
}

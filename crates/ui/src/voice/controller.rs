//! The workspace-owned live voice session: one realtime connection, one
//! always-open capture, one playback queue per window.
//!
//! [`Workspace`](crate::workspace::Workspace) owns one [`VoiceController`];
//! every pane composer renders the same [`VoiceStatus`] snapshot and forwards
//! clicks back up, so the session is identical no matter which composer the
//! user presses. The controller itself is thin: a tokio runner task that owns
//! the whole session, and an update channel back out. All CPAL and WebSocket
//! work happens inside the runner, never on the GPUI thread; dropping the
//! controller aborts the runner, which drops the session and audio engine.
//!
//! Live lifecycle:
//! - idle/error press → connect, then open the capture gate for good
//! - while the session runs, server VAD ends each utterance and creates the
//!   response itself; the user never presses to submit speech
//! - `input_audio_buffer.speech_started` clears local playback so a barge-in
//!   is audible and switches the phase back to Listening; the server's
//!   `interrupt_response` owns cancelling the interrupted response
//! - any press while the session is connecting, listening, thinking, or
//!   speaking stops the whole live session

use std::sync::Arc;

use gpui::{Context, SharedString, Task, WeakEntity};
use tokio::sync::mpsc;
use zeron_voice::{
    AudioCapture, AudioEngine, CaptureFrame, ClientEvent, ServerEvent, SessionConfig, VoiceConfig,
    VoiceSession, VoiceToolSink, base64_to_pcm16, pcm16_to_base64,
};

use crate::workspace::Workspace;

use super::{WorkspaceToolSink, spawn_tool_sink, tool_definitions};

/// Realtime model, overridable with `OPENAI_REALTIME_MODEL`.
const DEFAULT_MODEL: &str = "gpt-realtime";
/// Output voice, overridable with `OPENAI_REALTIME_VOICE`.
const DEFAULT_VOICE: &str = "marin";
/// Local CPA realtime endpoint, overridable with `CPA_REALTIME_ENDPOINT`.
/// `cpa-tunnel.service` already forwards this to the CPA proxy.
const DEFAULT_CPA_ENDPOINT: &str = "ws://127.0.0.1:8317/v1/realtime?model=gpt-realtime";
/// `error.code` CPA emits when the active credential's quota is exhausted;
/// the socket then closes with code 4429 and the next connect lands on the
/// next priority account.
const CPA_EXHAUSTED_CODE: &str = "cpa_credential_exhausted";
/// Retained uncommitted input cap: about 60 s of 24 kHz mono PCM16. Capture
/// chunks arrive as 20 ms slices, so the buffer stays a list of small chunks
/// rather than one giant frame. A VAD commit marks the buffer consumed, so
/// the retained copy only ever holds the current utterance's uncommitted
/// audio - enough to re-append on a fresh socket after a credential swap.
const REPLAY_SAMPLES: usize = 24_000 * 60;

const INSTRUCTIONS: &str = "You are the voice interface of Noches, a local \
    coding-workspace app. Replies are spoken aloud, so answer briefly and \
    plainly - no markdown, no lists longer than a few items. Use the provided \
    tools to inspect threads and act in the workspace instead of guessing; \
    read tools are always safe. send_to_thread requires the user's Allow \
    grant in the workspace - if a call comes back denied, say so and stop \
    instead of retrying.";

/// The minimal phase every composer mirrors on its mic button.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VoicePhase {
    /// No live session.
    #[default]
    Idle,
    /// First press in flight: credential lookup, device open, WebSocket.
    Connecting,
    /// Microphone gate open; server VAD is listening for the next utterance.
    Listening,
    /// The model is generating or running tools.
    Thinking,
    /// Assistant audio is arriving or playing.
    Speaking,
    /// Last attempt failed; the next press reconnects.
    Error,
}

/// The snapshot pushed to every pane composer. `mic` is the live capture
/// handle while a session runs so the button can render the meter level; it
/// is `None` before the first successful connect and after the session ends.
#[derive(Clone, Default)]
pub struct VoiceStatus {
    pub phase: VoicePhase,
    pub error: Option<SharedString>,
    pub mic: Option<AudioCapture>,
}

impl VoiceStatus {
    /// Decaying mic peak on `[0.0, 1.0]`; zero unless the gate is open.
    pub fn level(&self) -> f32 {
        self.mic.as_ref().map_or(0.0, AudioCapture::level)
    }
}

impl PartialEq for VoiceStatus {
    fn eq(&self, other: &Self) -> bool {
        self.phase == other.phase
            && self.error == other.error
            && self.mic.is_some() == other.mic.is_some()
    }
}

/// What a mic-button press does, decided purely from the current phase.
/// Live mode is a strict toggle: from idle or an error a press starts the
/// session; while it runs - connecting, listening, thinking, or speaking - a
/// press stops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Press {
    /// No live session: connect, then open the microphone for good.
    Connect,
    /// The live session is running: tear it down.
    Stop,
}

fn press_decision(alive: bool, phase: VoicePhase) -> Press {
    match (alive, phase) {
        (true, VoicePhase::Idle | VoicePhase::Error) | (false, _) => Press::Connect,
        (true, _) => Press::Stop,
    }
}

/// Runner-side updates back to the workspace.
pub(crate) enum VoiceUpdate {
    Phase(VoicePhase),
    Failure(String),
    /// Marks the phase Error and sets the status text. Used when a CPA
    /// credential swap interrupts a turn that cannot be replayed safely; the
    /// notice must outlive the turn instead of being cleared by a phase push.
    Notify(String),
    Mic(AudioCapture),
}

/// One voice session's UI-side state: the latest status plus the update
/// drain. All device and network work lives in the spawned runner; the only
/// way to talk to it is dropping the controller, which stops the session.
pub(crate) struct VoiceController {
    pub(crate) status: VoiceStatus,
    /// False once the runner has exited; the next press reconnects.
    pub(crate) alive: bool,
    /// Kept so dropping the controller aborts the runner and with it the
    /// session and audio engine.
    _runner: Task<Result<(), gpui_tokio::JoinError>>,
    _drain: Task<()>,
}

impl VoiceController {
    /// Spawns the runner and its update drain.
    pub(crate) fn start(workspace: WeakEntity<Workspace>, cx: &mut Context<Workspace>) -> Self {
        let sink = spawn_tool_sink(workspace, cx);
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        let runner = gpui_tokio::Tokio::spawn(cx, run(sink, updates));
        let drain = cx.spawn(async move |this, cx| {
            while let Some(update) = update_rx.recv().await {
                if this
                    .update(cx, |this, cx| this.voice_update(update, cx))
                    .is_err()
                {
                    return;
                }
            }
            // The runner exited (socket closed or a fatal error); later
            // presses must reconnect rather than write into a dead channel.
            let _ = this.update(cx, |this, cx| this.voice_stopped(cx));
        });
        Self {
            status: VoiceStatus {
                phase: VoicePhase::Connecting,
                ..VoiceStatus::default()
            },
            alive: true,
            _runner: runner,
            _drain: drain,
        }
    }
}

impl Workspace {
    /// The shared status every pane composer renders.
    pub(crate) fn voice_status(&self) -> VoiceStatus {
        self.voice
            .as_ref()
            .map_or_else(VoiceStatus::default, |voice| voice.status.clone())
    }

    /// One mic-button press, identical from any pane composer: from idle or
    /// an error it starts the live session; while the session runs it stops
    /// it. Turns are driven by server VAD, never by clicks.
    pub(crate) fn voice_toggle(&mut self, cx: &mut Context<Self>) {
        let phase = self.voice.as_ref().map(|voice| voice.status.phase);
        let alive = self.voice.as_ref().is_some_and(|voice| voice.alive);
        match press_decision(alive, phase.unwrap_or_default()) {
            Press::Connect => {
                self.voice = Some(VoiceController::start(cx.weak_entity(), cx));
            }
            Press::Stop => {
                // Dropping the controller aborts the runner, which drops the
                // session and the audio engine. A mid-stop runner update may
                // still land before the drain notices; reset the snapshot
                // first so nothing repaints a dead phase.
                self.voice = None;
            }
        }
        self.voice_publish(cx);
        cx.notify();
    }

    /// Applies one runner update and repaints the composers.
    pub(crate) fn voice_update(&mut self, update: VoiceUpdate, cx: &mut Context<Self>) {
        if let Some(voice) = self.voice.as_mut() {
            match update {
                VoiceUpdate::Phase(phase) => {
                    voice.status.phase = phase;
                    voice.status.error = None;
                }
                VoiceUpdate::Failure(message) => {
                    voice.status.phase = VoicePhase::Error;
                    voice.status.error = Some(message.into());
                }
                VoiceUpdate::Notify(message) => {
                    voice.status.phase = VoicePhase::Error;
                    voice.status.error = Some(message.into());
                }
                VoiceUpdate::Mic(capture) => voice.status.mic = Some(capture),
            }
        }
        self.voice_publish(cx);
        cx.notify();
    }

    /// Runner exit: keep the last phase visible (an error stays on screen),
    /// mark the session dead so the next press reconnects.
    pub(crate) fn voice_stopped(&mut self, cx: &mut Context<Self>) {
        if let Some(voice) = self.voice.as_mut() {
            voice.alive = false;
            voice.status.mic = None;
            if matches!(
                voice.status.phase,
                VoicePhase::Connecting
                    | VoicePhase::Listening
                    | VoicePhase::Thinking
                    | VoicePhase::Speaking
            ) {
                voice.status.phase = VoicePhase::Error;
                voice.status.error = Some("the voice session ended".into());
            }
        }
        self.voice_publish(cx);
        cx.notify();
    }

    /// Pushes the shared status into every mounted pane composer.
    fn voice_publish(&self, cx: &mut Context<Self>) {
        let status = self.voice_status();
        for runtime in self.panes.values() {
            runtime
                .chat
                .update(cx, |chat, cx| chat.set_voice_status(status.clone(), cx));
        }
    }
}

fn env_key(env: Option<&str>) -> Option<String> {
    env.map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

/// Which realtime backend the session is talking to. CPA is preferred: it
/// fronts a pool of accounts and swaps credentials server-side on quota
/// exhaustion (see [`CPA_EXHAUSTED_CODE`]). Direct OpenAI is the fallback
/// when CPA is not configured or its first connect fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// `CPA_API_KEY` against `CPA_REALTIME_ENDPOINT` or the local default.
    Cpa,
    /// `OPENAI_API_KEY` against the stock `wss://api.openai.com` endpoint.
    OpenAi,
}

/// The resolved backend plus its socket URL. The key stays in `config`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    backend: Backend,
    url: String,
}

/// Picks the backend and endpoint without touching the network. CPA wins when
/// `CPA_API_KEY` is present; `OPENAI_API_KEY` alone selects direct OpenAI.
/// Pure so the credential precedence is testable without devices or sockets.
fn resolve_backend(cpa_key: Option<&str>, openai_key: Option<&str>) -> Result<Endpoint, String> {
    if env_key(cpa_key).is_some() {
        return Ok(Endpoint {
            backend: Backend::Cpa,
            url: env_or("CPA_REALTIME_ENDPOINT", DEFAULT_CPA_ENDPOINT),
        });
    }
    if env_key(openai_key).is_some() {
        return Ok(Endpoint {
            backend: Backend::OpenAi,
            url: String::new(),
        });
    }
    Err("set CPA_API_KEY or OPENAI_API_KEY to use voice".to_owned())
}

/// Builds the connection config for `backend`; `OpenAi` leaves the endpoint
/// unset so the crate's default `?model=` URL applies. The key is never
/// persisted or logged; `VoiceConfig`'s `Debug` redacts it.
fn voice_config(backend: Backend, endpoint: &str, key: String) -> VoiceConfig {
    let config = VoiceConfig::new(key, env_or("OPENAI_REALTIME_MODEL", DEFAULT_MODEL))
        .with_session(session_config());
    match backend {
        Backend::Cpa => config.with_endpoint(endpoint),
        Backend::OpenAi => config,
    }
}

/// What a `cpa_credential_exhausted` error means for the live session. A
/// reconnect always lands on the next priority CPA account; what differs is
/// how much of the in-flight utterance can move to the fresh socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exhaustion {
    /// The committed turn produced no output or tool activity yet: reconnect,
    /// resend the retained audio, then commit and request the response
    /// explicitly because the fresh session's VAD never saw the speech.
    ReplayCommitted,
    /// The socket died mid-utterance before any VAD commit: re-append the
    /// retained chunks uncommitted and let the new session's VAD finish the
    /// turn.
    Reappend,
    /// Replaying would duplicate work - assistant output or tool activity
    /// already ran, or the retained audio is truncated. Reconnect and keep
    /// the repeat notice on screen until the next turn.
    Notify,
    /// Nothing was committed and no retained audio is worth moving: reconnect
    /// and keep listening.
    Reconnect,
}

fn exhaustion_decision(committed: bool, safe: bool, has_audio: bool, complete: bool) -> Exhaustion {
    match (committed, safe, has_audio, complete) {
        // A committed turn may be resent once, and only while it produced
        // nothing: replaying past output or tool activity would double it.
        (true, true, _, true) => Exhaustion::ReplayCommitted,
        (true, ..) => Exhaustion::Notify,
        // Mid-utterance: uncommitted chunks seed the fresh input buffer.
        (false, _, true, true) => Exhaustion::Reappend,
        (false, _, true, false) => Exhaustion::Notify,
        (false, _, false, _) => Exhaustion::Reconnect,
    }
}

/// Bounded copy of the current utterance's uncommitted input, kept so a CPA
/// credential swap can move it to a fresh socket. Chunks stay chunks; the cap
/// is in samples, not bytes. A `speech_stopped` commit marks the buffer
/// consumed: the audio stays retained until the turn's `response.done`, but
/// only as a committed-turn replay.
struct ReplayBuffer {
    chunks: Vec<Box<[i16]>>,
    samples: usize,
    /// False once a chunk overflowed the cap: the retained audio is then a
    /// truncated copy and must never be replayed as the whole turn.
    complete: bool,
    /// True once server VAD committed the retained audio on the socket it
    /// was appended to. A committed copy is only safe as a `ReplayCommitted`
    /// resend, never as a mid-utterance `Reappend`.
    committed: bool,
}

impl ReplayBuffer {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
            samples: 0,
            complete: true,
            committed: false,
        }
    }

    /// Stores `pcm` unless it would push the buffer past [`REPLAY_SAMPLES`];
    /// a full buffer marks the copy incomplete so a truncated turn is never
    /// replayed as if whole.
    fn push(&mut self, pcm: &[i16]) {
        if self.samples + pcm.len() > REPLAY_SAMPLES {
            self.complete = false;
            return;
        }
        self.samples += pcm.len();
        self.chunks.push(pcm.to_vec().into_boxed_slice());
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.samples = 0;
        self.complete = true;
        self.committed = false;
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Server-VAD live session: PCM16 in/out, near-field input noise reduction,
/// the reviewed tool allowlist, serial tool calls. `create_response` and
/// `interrupt_response` make each ended utterance its own turn and let a new
/// utterance barge in over an active response.
fn session_config() -> SessionConfig {
    SessionConfig::default()
        .pcm16_audio()
        .with_voice(env_or("OPENAI_REALTIME_VOICE", DEFAULT_VOICE))
        .with_turn_detection(serde_json::json!({
            "type": "server_vad",
            "threshold": 0.75,
            "prefix_padding_ms": 300,
            "silence_duration_ms": 700,
            "create_response": true,
            "interrupt_response": true,
        }))
        .with_noise_reduction(serde_json::json!({"type": "near_field"}))
        .with_instructions(INSTRUCTIONS)
        .with_tools(tool_definitions())
        .with_parallel_tool_calls(false)
}

fn phase(updates: &mpsc::UnboundedSender<VoiceUpdate>, phase: VoicePhase) {
    let _ = updates.send(VoiceUpdate::Phase(phase));
}

fn fail(updates: &mpsc::UnboundedSender<VoiceUpdate>, message: String) {
    let _ = updates.send(VoiceUpdate::Failure(message));
}

/// Re-appends the retained turn audio to a freshly connected session.
fn replay_input(session: &VoiceSession, replay: &ReplayBuffer) {
    for chunk in &replay.chunks {
        let _ = session.send(ClientEvent::InputAudioBufferAppend {
            audio: pcm16_to_base64(chunk),
        });
    }
}

/// The session's whole life: devices, socket, capture frames, playback. Runs
/// on the tokio runtime until the update channel closes - the controller was
/// dropped - or the server ends the session.
async fn run(sink: Arc<WorkspaceToolSink>, updates: mpsc::UnboundedSender<VoiceUpdate>) {
    let mut backend = match resolve_backend(
        std::env::var("CPA_API_KEY").ok().as_deref(),
        std::env::var("OPENAI_API_KEY").ok().as_deref(),
    ) {
        Ok(backend) => backend,
        Err(message) => return fail(&updates, message),
    };
    let mut engine = match AudioEngine::start() {
        Ok(engine) => engine,
        Err(error) => return fail(&updates, format!("voice audio: {error}")),
    };
    let Some(mut frames) = engine.take_capture_frames() else {
        return fail(&updates, "voice capture did not start".into());
    };
    let mut session = match connect(&backend, &sink).await {
        Ok(session) => session,
        Err(error) => {
            // The initial CPA connect failed; direct OpenAI still applies
            // when its key is configured. A failed OpenAi connect is fatal.
            let openai_key = env_key(std::env::var("OPENAI_API_KEY").ok().as_deref());
            match (backend.backend, openai_key) {
                (Backend::Cpa, Some(key)) => {
                    tracing::warn!(%error, "CPA voice connect failed; using OpenAI");
                    backend = Endpoint {
                        backend: Backend::OpenAi,
                        url: String::new(),
                    };
                    match connect_config(voice_config(Backend::OpenAi, "", key), &sink).await {
                        Ok(session) => session,
                        Err(error) => {
                            return fail(&updates, format!("voice session: {error}"));
                        }
                    }
                }
                _ => return fail(&updates, format!("voice session: {error}")),
            }
        }
    };

    // Connected: hand the meter handle to the UI, then open the microphone
    // for the life of the session. The server's VAD ends each utterance and
    // creates the response; no press is ever needed to submit speech.
    let _ = updates.send(VoiceUpdate::Mic(engine.capture().clone()));
    engine.begin_push_to_talk();
    phase(&updates, VoicePhase::Listening);

    // `response.created` ran but `response.done` has not: a committed turn
    // with audio worth replaying when a CPA credential swap hits.
    let mut response_active = false;
    // A `response.done` carrying a function call still has its follow-up
    // `response.created` coming while the tool runs; keep Thinking through
    // that gap instead of flashing back to Listening.
    let mut tool_pending = false;
    // Retained uncommitted utterance plus the committed turn, kept so a CPA
    // credential swap can move it to a fresh socket; cleared when the turn's
    // `response.done` lands.
    let mut replay = ReplayBuffer::new();
    // True until the current response produces assistant output or tool
    // activity; replaying past that point would double-speak or re-run tools.
    let mut replay_safe = true;

    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
                match frame {
                    CaptureFrame::Audio(pcm) => {
                        // Once VAD committed the utterance, later chunks belong
                        // to the next turn but `response.done` has not cleared
                        // the buffer yet; retaining them would mix two turns
                        // into one committed replay.
                        if !replay.committed {
                            replay.push(&pcm);
                        }
                        let _ = session.append_audio(&pcm);
                    }
                    // The gate never closes in live mode, so no release
                    // boundary is owed. A stale one from an earlier
                    // transition means nothing to commit here: server VAD
                    // owns the turn lifecycle now.
                    CaptureFrame::Released => {}
                }
            }
            event = session.recv() => {
                let Some(event) = event else { break };
                match event {
                    ServerEvent::InputAudioBufferSpeechStarted { .. } => {
                        // Barge-in: drop queued assistant playback so the
                        // interruption is audible at once. The server's
                        // `interrupt_response` cancels the response itself.
                        engine.playback().clear();
                        if !replay.committed {
                            // A fresh utterance is filling the input buffer;
                            // the retained copy starts over with it.
                            replay.clear();
                        }
                        phase(&updates, VoicePhase::Listening);
                    }
                    ServerEvent::InputAudioBufferSpeechStopped { .. } => {
                        // Server VAD committed the utterance and will create
                        // the response itself.
                        replay.committed = true;
                        response_active = true;
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::ResponseCreated { .. } => {
                        response_active = true;
                        // The follow-up response after a tool result: the
                        // pending tool gap is over. `response.created` alone
                        // is not output, so it does not end replay safety.
                        tool_pending = false;
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::FunctionCallArgumentsDelta { .. } => {
                        replay_safe = false;
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::FunctionCallArgumentsDone { .. } => {
                        // `response.done` is next, but the tool itself still
                        // has to run; hold Thinking until the follow-up
                        // `response.created` arrives.
                        tool_pending = true;
                        replay_safe = false;
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::OutputAudioDelta { delta, .. } => {
                        replay_safe = false;
                        if let Ok(pcm) = base64_to_pcm16(&delta) {
                            let _ = engine.playback().play(&pcm);
                        }
                        phase(&updates, VoicePhase::Speaking);
                    }
                    ServerEvent::OutputAudioDone { .. } => {
                        replay_safe = false;
                        let _ = engine.playback().flush();
                    }
                    // Assistant text the UI shows or a transcript of spoken
                    // output: replaying past either would double it.
                    ServerEvent::OutputTextDelta { .. }
                    | ServerEvent::OutputTextDone { .. }
                    | ServerEvent::OutputAudioTranscriptDelta { .. }
                    | ServerEvent::OutputAudioTranscriptDone { .. } => {
                        replay_safe = false;
                    }
                    ServerEvent::ResponseDone { .. } => {
                        response_active = false;
                        replay.clear();
                        replay_safe = true;
                        // Live capture never closes, so the session falls
                        // back to listening for the next utterance. A tool
                        // result's follow-up `response.created` is still
                        // coming; keep Thinking through that gap.
                        if !tool_pending {
                            phase(&updates, VoicePhase::Listening);
                        }
                    }
                    ServerEvent::Error { error } => {
                        if backend.backend == Backend::Cpa
                            && error.code.as_deref() == Some(CPA_EXHAUSTED_CODE)
                        {
                            match exhaustion_decision(
                                replay.committed && response_active,
                                replay_safe,
                                !replay.chunks.is_empty(),
                                replay.complete,
                            ) {
                                Exhaustion::ReplayCommitted => {
                                    session.cancel();
                                    match connect(&backend, &sink).await {
                                        Ok(next) => {
                                            session = next;
                                            replay_input(&session, &replay);
                                            // The fresh socket's VAD never
                                            // saw the utterance, so commit
                                            // the resent audio and request
                                            // the response explicitly.
                                            let _ = session.commit_audio();
                                            let _ = session.request_response();
                                            phase(&updates, VoicePhase::Thinking);
                                        }
                                        Err(error) => {
                                            fail(&updates, format!("voice session: {error}"));
                                            return;
                                        }
                                    }
                                    continue;
                                }
                                Exhaustion::Reappend => {
                                    // The socket died mid-utterance: the
                                    // retained chunks seed the new input
                                    // buffer, capture keeps appending, and
                                    // the new session's VAD ends the turn.
                                    session.cancel();
                                    match connect(&backend, &sink).await {
                                        Ok(next) => {
                                            session = next;
                                            replay_input(&session, &replay);
                                            phase(&updates, VoicePhase::Listening);
                                        }
                                        Err(error) => {
                                            fail(&updates, format!("voice session: {error}"));
                                            return;
                                        }
                                    }
                                    continue;
                                }
                                Exhaustion::Notify => {
                                    // Replaying would duplicate work the
                                    // user already saw or heard. Reconnect
                                    // and keep the repeat notice on screen
                                    // until the next utterance lands.
                                    session.cancel();
                                    match connect(&backend, &sink).await {
                                        Ok(next) => session = next,
                                        Err(error) => {
                                            fail(&updates, format!("voice session: {error}"));
                                            return;
                                        }
                                    }
                                    response_active = false;
                                    tool_pending = false;
                                    replay.clear();
                                    replay_safe = true;
                                    let _ = updates.send(VoiceUpdate::Notify(
                                        "voice account switched mid-turn - please repeat"
                                            .to_owned(),
                                    ));
                                    continue;
                                }
                                Exhaustion::Reconnect => {
                                    // Nothing was committed and nothing
                                    // worth retaining is in flight: just
                                    // move to the next CPA credential.
                                    session.cancel();
                                    match connect(&backend, &sink).await {
                                        Ok(next) => session = next,
                                        Err(error) => {
                                            fail(&updates, format!("voice session: {error}"));
                                            return;
                                        }
                                    }
                                    phase(&updates, VoicePhase::Listening);
                                    continue;
                                }
                            }
                        }
                        let message = error
                            .message
                            .unwrap_or_else(|| "the realtime API returned an error".into());
                        fail(&updates, message);
                        // A server error ends the session; leaving the loop
                        // here keeps a later event from repainting over the
                        // failure.
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
    // Dropping `session` closes the outbound channel, which makes the driver
    // send a close frame; `engine` stops both device streams on drop.
}

/// Dials the resolved backend. The key is read fresh from the environment on
/// every connect so a credential swap never goes through stored state.
async fn connect(
    endpoint: &Endpoint,
    sink: &Arc<WorkspaceToolSink>,
) -> Result<VoiceSession, zeron_voice::VoiceError> {
    let key = match endpoint.backend {
        Backend::Cpa => std::env::var("CPA_API_KEY")
            .ok()
            .as_deref()
            .and_then(|key| env_key(Some(key))),
        Backend::OpenAi => std::env::var("OPENAI_API_KEY")
            .ok()
            .as_deref()
            .and_then(|key| env_key(Some(key))),
    };
    let Some(key) = key else {
        return Err(zeron_voice::VoiceError::Disconnected);
    };
    connect_config(voice_config(endpoint.backend, &endpoint.url, key), sink).await
}

async fn connect_config(
    config: VoiceConfig,
    sink: &Arc<WorkspaceToolSink>,
) -> Result<VoiceSession, zeron_voice::VoiceError> {
    VoiceSession::connect(config, Arc::clone(sink) as Arc<dyn VoiceToolSink>).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_voice::ClientEvent;

    #[test]
    fn backend_prefers_cpa_then_openai_then_errors() {
        // CPA wins whenever its key is set, even alongside an OpenAI key.
        let endpoint = resolve_backend(Some("cpa-key"), Some("sk-live")).unwrap();
        assert_eq!(endpoint.backend, Backend::Cpa);
        assert!(
            endpoint.url.starts_with("ws://"),
            "CPA dials the configured endpoint, got {}",
            endpoint.url
        );

        let endpoint = resolve_backend(None, Some("  sk-only  ")).unwrap();
        assert_eq!(endpoint.backend, Backend::OpenAi);

        let error = resolve_backend(Some("  "), Some("")).unwrap_err();
        assert_eq!(error, "set CPA_API_KEY or OPENAI_API_KEY to use voice");
        let error = resolve_backend(None, None).unwrap_err();
        assert_eq!(error, "set CPA_API_KEY or OPENAI_API_KEY to use voice");
    }

    #[test]
    fn exhaustion_replays_only_what_a_fresh_socket_can_redo() {
        // A committed turn with nothing emitted yet: resend the retained
        // audio, then commit and request the response because the new
        // session's VAD never saw the speech.
        assert_eq!(
            exhaustion_decision(true, true, true, true),
            Exhaustion::ReplayCommitted
        );
        // Output or tool activity already ran: replaying would double it.
        for (safe, has_audio, complete) in [
            (false, true, true),
            (false, false, true),
            (false, false, false),
            (true, true, false),
        ] {
            assert_eq!(
                exhaustion_decision(true, safe, has_audio, complete),
                Exhaustion::Notify,
                "safe={safe} has_audio={has_audio} complete={complete}"
            );
        }
        // Mid-utterance with complete retained audio: re-append it and let
        // the new session's VAD finish the turn.
        assert_eq!(
            exhaustion_decision(false, true, true, true),
            Exhaustion::Reappend
        );
        assert_eq!(
            exhaustion_decision(false, false, true, true),
            Exhaustion::Reappend
        );
        // Truncated uncommitted audio can never be replayed as a whole turn.
        assert_eq!(
            exhaustion_decision(false, true, true, false),
            Exhaustion::Notify
        );
        // Nothing in flight: reconnect without a notice and keep listening.
        for (safe, complete) in [(true, true), (false, false)] {
            assert_eq!(
                exhaustion_decision(false, safe, false, complete),
                Exhaustion::Reconnect,
                "safe={safe} complete={complete}"
            );
        }
    }

    #[test]
    fn replay_buffer_stays_bounded_and_flags_truncation() {
        let mut buffer = ReplayBuffer::new();
        buffer.push(&vec![0i16; REPLAY_SAMPLES]);
        assert_eq!(buffer.samples, REPLAY_SAMPLES);
        assert!(buffer.complete);
        assert!(!buffer.committed);

        buffer.push(&[1, 2, 3]);
        assert_eq!(buffer.samples, REPLAY_SAMPLES, "a full buffer drops");
        assert!(!buffer.complete, "an overflow marks the copy truncated");
        assert_eq!(
            exhaustion_decision(false, true, true, buffer.complete),
            Exhaustion::Notify,
            "a truncated utterance is never re-appended as if complete"
        );

        buffer.clear();
        assert_eq!(buffer.samples, 0);
        assert!(buffer.chunks.is_empty());
        assert!(buffer.complete, "clearing readies the next utterance");
        assert!(!buffer.committed);
    }

    #[test]
    fn session_config_is_server_vad_live_pcm16_with_tools() {
        let json = serde_json::to_value(ClientEvent::SessionUpdate {
            session: session_config(),
        })
        .unwrap();
        let session = &json["session"];
        assert_eq!(session["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(
            session["audio"]["input"]["turn_detection"],
            serde_json::json!({
                "type": "server_vad",
                "threshold": 0.75,
                "prefix_padding_ms": 300,
                "silence_duration_ms": 700,
                "create_response": true,
                "interrupt_response": true,
            }),
            "server VAD ends each utterance and creates the response"
        );
        assert_eq!(
            session["audio"]["input"]["noise_reduction"],
            serde_json::json!({"type": "near_field"})
        );
        assert_eq!(session["audio"]["output"]["format"]["type"], "audio/pcm");
        assert_eq!(session["audio"]["output"]["voice"], "marin");
        assert_eq!(session["parallel_tool_calls"], false);
        assert_eq!(session["tools"].as_array().unwrap().len(), 5);
        assert!(
            session["instructions"].as_str().unwrap().len() > 20,
            "coding-assistant instructions are set"
        );
    }

    #[test]
    fn press_decision_is_a_pure_toggle() {
        // A dead session or an error reconnects from any phase.
        for phase in [
            VoicePhase::Idle,
            VoicePhase::Connecting,
            VoicePhase::Listening,
            VoicePhase::Thinking,
            VoicePhase::Speaking,
            VoicePhase::Error,
        ] {
            assert_eq!(
                press_decision(false, phase),
                Press::Connect,
                "dead {phase:?}"
            );
        }
        for phase in [VoicePhase::Idle, VoicePhase::Error] {
            assert_eq!(press_decision(true, phase), Press::Connect, "{phase:?}");
        }
        // Every live phase stops the session; there is no click-to-submit.
        for phase in [
            VoicePhase::Connecting,
            VoicePhase::Listening,
            VoicePhase::Thinking,
            VoicePhase::Speaking,
        ] {
            assert_eq!(press_decision(true, phase), Press::Stop, "{phase:?}");
        }
    }
}

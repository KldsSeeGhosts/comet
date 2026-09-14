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
//! - a live-session mute only closes the capture gate: the socket, playback,
//!   and tool plumbing stay up, and unmuting reopens the same gate

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, SharedString, Task, WeakEntity};
use tokio::sync::mpsc;
use zeron_voice::{
    AudioCapture, AudioEngine, CaptureFrame, ClientEvent, ServerEvent, VoiceConfig, VoiceError,
    VoiceSession, VoiceToolSink, base64_to_pcm16, pcm16_to_base64,
};

use crate::settings::{self, VoiceStartSnapshot};
use crate::workspace::Workspace;

use super::{WorkspaceToolSink, spawn_tool_sink};

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
/// Longest a single WebSocket dial - initial or CPA reconnect - may take
/// before the session is declared failed. CPA failovers dial localhost, so
/// the bound only needs to cover a stalled TCP/TLS handshake, not a long
/// network path.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// A dial that may still be running while capture frames keep landing.
type PendingDial = Pin<Box<dyn Future<Output = Result<VoiceSession, VoiceError>> + Send + 'static>>;

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
/// `muted` is the live-session mic gate: the composer mirrors it and
/// [`Workspace::voice_mute_toggle`] flips it.
#[derive(Clone, Default)]
pub struct VoiceStatus {
    pub phase: VoicePhase,
    pub error: Option<SharedString>,
    pub mic: Option<AudioCapture>,
    pub muted: bool,
}

impl VoiceStatus {
    /// Decaying mic peak on `[0.0, 1.0]`; zero unless the gate is open and
    /// unmuted.
    pub fn level(&self) -> f32 {
        if self.muted {
            return 0.0;
        }
        self.mic.as_ref().map_or(0.0, AudioCapture::level)
    }
}

impl PartialEq for VoiceStatus {
    fn eq(&self, other: &Self) -> bool {
        self.phase == other.phase
            && self.error == other.error
            && self.muted == other.muted
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

/// What a mute request does, decided purely from the session state. Muting
/// is only meaningful on a live session: Connecting has no gate to close yet
/// (the runner opens it when the mic arrives), Error has no session left to
/// hush, and a dead runner owns nothing. In every live phase the request
/// flips the gate; the UI reflects the new flag back through the published
/// status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mute {
    /// Flip the capture gate on the live session.
    Toggle,
    /// No live session, or a phase where muting means nothing: ignore.
    Skip,
}

fn mute_decision(alive: bool, phase: VoicePhase) -> Mute {
    match (alive, phase) {
        (true, VoicePhase::Listening | VoicePhase::Thinking | VoicePhase::Speaking) => {
            Mute::Toggle
        }
        _ => Mute::Skip,
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
    /// Spawns the runner and its update drain. The start-time snapshot
    /// freezes `settings::current(cx).voice` plus the process environment
    /// here, synchronously, so nothing the session dials with can drift
    /// mid-session.
    pub(crate) fn start(workspace: WeakEntity<Workspace>, cx: &mut Context<Workspace>) -> Self {
        let sink = spawn_tool_sink(workspace, cx);
        let snapshot = VoiceStartSnapshot::new(&settings::current(cx).voice);
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        let runner = gpui_tokio::Tokio::spawn(cx, run(sink, snapshot, updates));
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

    /// The global mic-mute toggle (shortcut wiring lives elsewhere). Only
    /// the capture gate moves: the session, playback, and tool plumbing stay
    /// up, so an unmute reopens the same gate on the same socket. No-op
    /// without a live session, and while Connecting or Error there is no
    /// open gate to close.
    pub(crate) fn voice_mute_toggle(&mut self, cx: &mut Context<Self>) {
        let Some(voice) = self.voice.as_mut() else {
            return;
        };
        if mute_decision(voice.alive, voice.status.phase) == Mute::Skip {
            return;
        }
        voice.status.muted = !voice.status.muted;
        // `mic` is a clone of the runner's gate; the engine keeps owning the
        // streams. It is `Some` on every live phase - the runner sends it
        // with the Listening phase before any event can advance the turn -
        // but a same-instant stop may not have landed yet.
        if let Some(mic) = voice.status.mic.as_ref() {
            mic.set_capturing(!voice.status.muted);
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
                VoiceUpdate::Mic(capture) => {
                    // The runner opens the gate when it hands the handle over;
                    // a queued mute from before the mic arrived still applies.
                    if voice.status.muted {
                        capture.set_capturing(false);
                    }
                    voice.status.mic = Some(capture);
                }
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
            voice.status.muted = false;
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
    /// `CPA_API_KEY` against the snapshot's resolved CPA endpoint.
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

/// Picks the backend and endpoint without touching the network. CPA wins
/// when `CPA_API_KEY` is present; `OPENAI_API_KEY` alone selects direct
/// OpenAI. Pure so the credential precedence is testable without devices or
/// sockets.
fn resolve_backend(
    cpa_key: Option<&str>,
    openai_key: Option<&str>,
    snapshot: &VoiceStartSnapshot,
) -> Result<Endpoint, String> {
    if env_key(cpa_key).is_some() {
        return Ok(Endpoint {
            backend: Backend::Cpa,
            url: snapshot.cpa_endpoint(),
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
fn voice_config(
    backend: Backend,
    endpoint: &str,
    key: String,
    snapshot: &VoiceStartSnapshot,
) -> VoiceConfig {
    let config = VoiceConfig::new(key, snapshot.model.clone())
        .with_session(snapshot.session_config());
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

/// One live turn's bookkeeping: which response is in flight, whether the
/// committed utterance's resend window is still open, and the retained
/// audio a credential swap may move to a fresh socket.
///
/// `response.done` events are matched against [`Turn::active_response`]:
/// a canceled response's late `done` must not clear the newer utterance's
/// retained audio or drop the phase back to Listening mid-turn.
struct Turn {
    /// The in-flight response's id from `response.created`, `Some("")` when
    /// the event carried no id. `None` before creation, after the matching
    /// `response.done`, and after a barge-in invalidates it.
    active_response: Option<String>,
    /// A committed utterance whose response has not completed: the window in
    /// which a credential swap may still resend the retained audio. Set by
    /// `speech_stopped`, cleared by the matching `response.done` or by a
    /// barge-in superseding the turn.
    turn_open: bool,
    /// A `response.done` carrying a function call still has its follow-up
    /// `response.created` coming while the tool runs; hold Thinking through
    /// that gap instead of flashing back to Listening.
    tool_pending: bool,
    /// Retained uncommitted utterance plus the committed turn.
    replay: ReplayBuffer,
    /// Bounded capture gathered while a reconnect dial runs on top of an
    /// already committed replay: the committed copy must stay
    /// byte-identical, so those frames join the next utterance and
    /// re-append after the old turn's resend. Stays empty on every path
    /// where the retained audio is uncommitted - mid-dial frames then join
    /// `replay` itself and are never held twice.
    pending: ReplayBuffer,
    /// True until the current response produces assistant output or tool
    /// activity; replaying past that point would double-speak or re-run
    /// tools.
    replay_safe: bool,
}

impl Turn {
    fn new() -> Self {
        Self {
            active_response: None,
            turn_open: false,
            tool_pending: false,
            replay: ReplayBuffer::new(),
            pending: ReplayBuffer::new(),
            replay_safe: true,
        }
    }

    /// A new utterance starts. The server's `interrupt_response` cancels the
    /// active response itself; here its resend window closes and the retained
    /// copy restarts with the new speech - whether it held uncommitted
    /// partial audio or a committed turn whose response is now superseded.
    /// The caller clears queued playback so the interruption is audible.
    fn speech_started(&mut self) {
        self.replay.clear();
        self.pending.clear();
        self.active_response = None;
        self.turn_open = false;
    }

    /// Server VAD committed the utterance; the response it creates next owns
    /// the resend window.
    fn speech_stopped(&mut self) {
        self.replay.committed = true;
        self.turn_open = true;
    }

    fn response_created(&mut self, response_id: Option<String>) {
        // `Some("")` marks an active response whose id the event did not
        // carry; it still accepts the next `response.done`.
        self.active_response = Some(response_id.unwrap_or_default());
        self.tool_pending = false;
    }

    /// Closes the active response when `response_id` names it. Returns
    /// `false` for a stale `done` - a canceled or different response's -
    /// which must not touch the newer turn's retained audio or phase.
    fn response_done(&mut self, response_id: Option<&str>) -> bool {
        let Some(active) = self.active_response.as_deref() else {
            // No live response: a canceled response's late `done`.
            return false;
        };
        if !active.is_empty() && response_id.is_some_and(|id| id != active) {
            // A different response finished; the tracked one is still open.
            return false;
        }
        self.active_response = None;
        self.turn_open = false;
        self.replay.clear();
        self.replay_safe = true;
        true
    }

    /// The inputs [`exhaustion_decision`] reads from the turn.
    fn exhaustion(&self) -> Exhaustion {
        exhaustion_decision(
            self.turn_open,
            self.replay_safe,
            !self.replay.chunks.is_empty(),
            self.replay.complete,
        )
    }

    /// Reconnect housekeeping for the exhaustion paths that keep no turn
    /// state on the fresh socket.
    fn reset_for_reconnect(&mut self) {
        self.active_response = None;
        self.turn_open = false;
        self.tool_pending = false;
        self.replay.clear();
        self.pending.clear();
        self.replay_safe = true;
    }
}

fn phase(updates: &mpsc::UnboundedSender<VoiceUpdate>, phase: VoicePhase) {
    let _ = updates.send(VoiceUpdate::Phase(phase));
}

fn fail(updates: &mpsc::UnboundedSender<VoiceUpdate>, message: String) {
    let _ = updates.send(VoiceUpdate::Failure(message));
}

/// The ordered client events a landed reconnect sends before listening
/// resumes. Pure so the committed-replay ordering - the old turn's
/// append(s), an explicit commit, the response request, then the
/// pending-dial append(s) - is testable without a socket. A truncated
/// buffer emits nothing: replaying a partial copy would present it as the
/// whole utterance.
fn reconnect_events(
    reconnect: Exhaustion,
    replay: &ReplayBuffer,
    pending: &ReplayBuffer,
) -> Vec<ClientEvent> {
    fn appends(buffer: &ReplayBuffer) -> Vec<ClientEvent> {
        if !buffer.complete {
            return Vec::new();
        }
        buffer
            .chunks
            .iter()
            .map(|chunk| ClientEvent::InputAudioBufferAppend {
                audio: pcm16_to_base64(chunk),
            })
            .collect()
    }
    match reconnect {
        // The committed turn first - its append(s), an explicit commit,
        // and the response request because the fresh session's VAD never
        // saw the speech - then the mid-dial capture as the next
        // uncommitted utterance.
        Exhaustion::ReplayCommitted => appends(replay)
            .into_iter()
            .chain([
                ClientEvent::InputAudioBufferCommit,
                ClientEvent::ResponseCreate { response: None },
            ])
            .chain(appends(pending))
            .collect(),
        // Uncommitted retained audio - including chunks that arrived
        // mid-dial - seeds the fresh input buffer exactly once.
        Exhaustion::Reappend => appends(replay),
        // The committed turn itself cannot move safely, but mid-dial
        // capture is a fresh uncommitted utterance and still re-appends
        // rather than being silently dropped.
        Exhaustion::Notify => appends(pending),
        // Nothing was retained when the socket died; only chunks that
        // arrived mid-dial can be in the buffer, as uncommitted new
        // speech.
        Exhaustion::Reconnect => appends(replay),
    }
}

/// Ends the session over a send that can no longer reach the driver: the
/// outbound channel only closes once the driver task exited, so the socket
/// is already gone and retrying cannot repair it.
fn fail_send(
    updates: &mpsc::UnboundedSender<VoiceUpdate>,
    context: &'static str,
    error: VoiceError,
) {
    tracing::warn!(%error, context, "voice session send failed; ending session");
    fail(updates, "the voice connection dropped".to_owned());
}

/// The session's whole life: devices, socket, capture frames, playback. Runs
/// on the tokio runtime until the update channel closes - the controller was
/// dropped - or the server ends the session. `snapshot` is the start-time
/// freeze of settings + environment; everything the session dials with
/// comes from it.
async fn run(
    sink: Arc<WorkspaceToolSink>,
    snapshot: VoiceStartSnapshot,
    updates: mpsc::UnboundedSender<VoiceUpdate>,
) {
    let mut backend = match resolve_backend(
        std::env::var("CPA_API_KEY").ok().as_deref(),
        std::env::var("OPENAI_API_KEY").ok().as_deref(),
        &snapshot,
    ) {
        Ok(backend) => backend,
        Err(message) => return fail(&updates, message),
    };
    let mut engine = match AudioEngine::start_with(snapshot.audio_config.clone()) {
        Ok(engine) => engine,
        Err(error) => return fail(&updates, format!("voice audio: {error}")),
    };
    let Some(mut frames) = engine.take_capture_frames() else {
        return fail(&updates, "voice capture did not start".into());
    };
    let mut session = match connect(&backend, &snapshot, &sink).await {
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
                    match dial(
                        voice_config(Backend::OpenAi, "", key, &snapshot),
                        &sink,
                    )
                    .await
                    {
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

    let mut turn = Turn::new();
    // A CPA reconnect in flight: capture frames keep filling the retained
    // buffer while the dial runs, but nothing is sent to the dead session.
    let mut dial: Option<PendingDial> = None;
    // The exhaustion decision the pending dial applies once it lands.
    let mut reconnect = Exhaustion::Reconnect;

    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
                match frame {
                    CaptureFrame::Audio(pcm) => {
                        if turn.replay.committed {
                            // The committed replay must stay byte-identical,
                            // so while the reconnect dial runs these frames
                            // join the bounded pending buffer as the next
                            // utterance; outside a dial nothing retains them
                            // yet.
                            if dial.is_some() {
                                turn.pending.push(&pcm);
                            }
                        } else {
                            // Uncommitted audio keeps filling the same
                            // bounded buffer the fresh socket re-appends,
                            // mid-dial frames included, so a Reappend never
                            // sends the same chunk twice.
                            turn.replay.push(&pcm);
                        }
                        // While a reconnect dial runs the old session is
                        // dead: the chunk only lives in the retained
                        // buffers and reaches the fresh socket through
                        // reconnect_events.
                        if dial.is_none()
                            && let Err(error) = session.append_audio(&pcm)
                        {
                            fail_send(&updates, "capture append", error);
                            return;
                        }
                    }
                    // The gate never closes in live mode, so no release
                    // boundary is owed. A stale one from an earlier
                    // transition means nothing to commit here: server VAD
                    // owns the turn lifecycle now.
                    CaptureFrame::Released => {}
                }
            }
            event = session.recv(), if dial.is_none() => {
                let Some(event) = event else { break };
                match event {
                    ServerEvent::InputAudioBufferSpeechStarted { .. } => {
                        // Barge-in: drop queued assistant playback so the
                        // interruption is audible at once, then invalidate
                        // the interrupted response before the new turn.
                        engine.playback().clear();
                        turn.speech_started();
                        phase(&updates, VoicePhase::Listening);
                    }
                    ServerEvent::InputAudioBufferSpeechStopped { .. } => {
                        // Server VAD committed the utterance and will create
                        // the response itself.
                        turn.speech_stopped();
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::ResponseCreated { response_id } => {
                        // `response.created` alone is not output, so it does
                        // not end replay safety.
                        turn.response_created(response_id);
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::FunctionCallArgumentsDelta { .. } => {
                        turn.replay_safe = false;
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::FunctionCallArgumentsDone { .. } => {
                        // `response.done` is next, but the tool itself still
                        // has to run; hold Thinking until the follow-up
                        // `response.created` arrives.
                        turn.tool_pending = true;
                        turn.replay_safe = false;
                        phase(&updates, VoicePhase::Thinking);
                    }
                    ServerEvent::OutputAudioDelta { delta, .. } => {
                        turn.replay_safe = false;
                        let pcm = match base64_to_pcm16(&delta) {
                            Ok(pcm) => pcm,
                            Err(error) => {
                                engine
                                    .note_error(format!("output audio decode: {error}"));
                                fail(&updates, "voice audio decode failed".into());
                                return;
                            }
                        };
                        if let Err(error) = engine.playback().play(&pcm) {
                            engine.note_error(format!("output audio play: {error}"));
                            fail(&updates, "voice playback failed".into());
                            return;
                        }
                        phase(&updates, VoicePhase::Speaking);
                    }
                    ServerEvent::OutputAudioDone { .. } => {
                        turn.replay_safe = false;
                        if let Err(error) = engine.playback().flush() {
                            engine.note_error(format!("output audio flush: {error}"));
                            fail(&updates, "voice playback failed".into());
                            return;
                        }
                    }
                    // Assistant text the UI shows or a transcript of spoken
                    // output: replaying past either would double it.
                    ServerEvent::OutputTextDelta { .. }
                    | ServerEvent::OutputTextDone { .. }
                    | ServerEvent::OutputAudioTranscriptDelta { .. }
                    | ServerEvent::OutputAudioTranscriptDone { .. } => {
                        turn.replay_safe = false;
                    }
                    ServerEvent::ResponseDone { response } => {
                        // A canceled response's late `done` must not clear
                        // the newer utterance's state or repaint Listening
                        // over a newer turn.
                        if turn.response_done(response.id.as_deref())
                            && !turn.tool_pending
                        {
                            phase(&updates, VoicePhase::Listening);
                        }
                    }
                    ServerEvent::Error { error } => {
                        if backend.backend == Backend::Cpa
                            && error.code.as_deref() == Some(CPA_EXHAUSTED_CODE)
                        {
                            // The socket is about to close: keep receiving
                            // capture into the retained buffer while the
                            // next credential's dial runs. The decision the
                            // dial applies is fixed now, when the turn's
                            // replay state is the one that exhausted.
                            reconnect = turn.exhaustion();
                            session.cancel();
                            dial = Some(redial(&backend, &snapshot, &sink));
                            continue;
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
            next = async { dial.as_mut().expect("dial pending").await }, if dial.is_some() => {
                dial = None;
                match next {
                    Ok(next) => {
                        session = next;
                        // A mid-dial overflow truncates the retained
                        // utterance; replaying a partial copy would
                        // present it as whole, so fall back to the
                        // repeat notice.
                        let decision = match reconnect {
                            Exhaustion::Reappend | Exhaustion::Reconnect
                                if !turn.replay.complete =>
                            {
                                Exhaustion::Notify
                            }
                            other => other,
                        };
                        if let Err(error) = reconnect_events(
                            decision,
                            &turn.replay,
                            &turn.pending,
                        )
                        .into_iter()
                        .try_for_each(|event| session.send(event))
                        {
                            fail_send(&updates, "reconnect replay", error);
                            return;
                        }
                        match decision {
                            Exhaustion::ReplayCommitted => {
                                // The old turn's resend is on the wire;
                                // the pending utterance keeps collecting
                                // on the fresh socket through VAD.
                                turn.pending.clear();
                                phase(&updates, VoicePhase::Thinking);
                            }
                            Exhaustion::Reappend => {
                                // The socket died mid-utterance: the
                                // retained chunks - including any
                                // captured while the dial ran - seeded
                                // the new input buffer and its VAD ends
                                // the turn.
                                phase(&updates, VoicePhase::Listening);
                            }
                            Exhaustion::Notify => {
                                // Replaying the turn would duplicate
                                // work the user already saw or heard;
                                // the mid-dial utterance still seeded
                                // the fresh socket. Keep the repeat
                                // notice on screen until the next turn.
                                turn.reset_for_reconnect();
                                let _ = updates.send(VoiceUpdate::Notify(
                                    "voice account switched mid-turn - please repeat"
                                        .to_owned(),
                                ));
                            }
                            Exhaustion::Reconnect => {
                                // Nothing was committed: only mid-dial
                                // capture, if any, moved to the next CPA
                                // credential.
                                turn.reset_for_reconnect();
                                phase(&updates, VoicePhase::Listening);
                            }
                        }
                    }
                    Err(error) => {
                        fail(&updates, format!("voice session: {error}"));
                        return;
                    }
                }
            }
        }
    }
    // Dropping `session` closes the outbound channel, which makes the driver
    // send a close frame; `engine` stops both device streams on drop.
}

/// Dials the resolved backend. The key is read fresh from the environment on
/// every connect so a credential swap never goes through stored state; the
/// endpoint and session config come from the start-time snapshot.
async fn connect(
    endpoint: &Endpoint,
    snapshot: &VoiceStartSnapshot,
    sink: &Arc<WorkspaceToolSink>,
) -> Result<VoiceSession, VoiceError> {
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
        return Err(VoiceError::Disconnected);
    };
    dial(
        voice_config(endpoint.backend, &endpoint.url, key, snapshot),
        sink,
    )
    .await
}

/// Bounds one connect attempt by [`DIAL_TIMEOUT`]. The endpoint URL carries
/// no credential material and is safe to report; the key never leaves
/// `config`.
async fn dial(
    config: VoiceConfig,
    sink: &Arc<WorkspaceToolSink>,
) -> Result<VoiceSession, VoiceError> {
    bounded_connect(DIAL_TIMEOUT, connect_config(config, sink)).await
}

/// The timeout wrapper, generic over the connect future so a never-resolving
/// dial can be exercised without a socket. A deadline hit maps to
/// [`VoiceError::Timeout`]; an inner result passes through untouched.
async fn bounded_connect(
    deadline: Duration,
    connect: impl Future<Output = Result<VoiceSession, VoiceError>>,
) -> Result<VoiceSession, VoiceError> {
    match tokio::time::timeout(deadline, connect).await {
        Ok(result) => result,
        Err(_) => Err(VoiceError::Timeout),
    }
}

async fn connect_config(
    config: VoiceConfig,
    sink: &Arc<WorkspaceToolSink>,
) -> Result<VoiceSession, VoiceError> {
    VoiceSession::connect(config, Arc::clone(sink) as Arc<dyn VoiceToolSink>).await
}

/// Starts a credential-swap reconnect as a future the session loop polls
/// alongside capture frames: the dial holds owned state so it stays
/// `'static` and needs no borrow of the runner's locals.
fn redial(
    endpoint: &Endpoint,
    snapshot: &VoiceStartSnapshot,
    sink: &Arc<WorkspaceToolSink>,
) -> PendingDial {
    let endpoint = endpoint.clone();
    let snapshot = snapshot.clone();
    let sink = Arc::clone(sink);
    Box::pin(async move { connect(&endpoint, &snapshot, &sink).await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::voice::VoiceSettings;
    use zeron_voice::ClientEvent;

    fn snapshot() -> VoiceStartSnapshot {
        VoiceStartSnapshot::with_env(&VoiceSettings::default(), None, None, None)
    }

    #[test]
    fn backend_prefers_cpa_then_openai_then_errors() {
        let snapshot = snapshot();
        // CPA wins whenever its key is set, even alongside an OpenAI key.
        let endpoint = resolve_backend(Some("cpa-key"), Some("sk-live"), &snapshot).unwrap();
        assert_eq!(endpoint.backend, Backend::Cpa);
        assert_eq!(
            endpoint.url,
            "ws://127.0.0.1:8317/v1/realtime?model=gpt-realtime-2.1",
            "CPA dials the snapshot's resolved endpoint"
        );

        let endpoint = resolve_backend(None, Some("  sk-only  "), &snapshot).unwrap();
        assert_eq!(endpoint.backend, Backend::OpenAi);

        let error = resolve_backend(Some("  "), Some(""), &snapshot).unwrap_err();
        assert_eq!(error, "set CPA_API_KEY or OPENAI_API_KEY to use voice");
        let error = resolve_backend(None, None, &snapshot).unwrap_err();
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
            session: snapshot().session_config(),
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
    fn the_session_json_carries_the_snapshot_voice_and_never_a_key() {
        // A picked voice reaches the session payload; the connection config
        // only ever carries the key through `VoiceConfig`, which redacts it.
        let mut settings = VoiceSettings::default();
        settings.voice = Some("cedar".to_owned());
        let snapshot = VoiceStartSnapshot::with_env(&settings, None, None, None);
        let json = serde_json::to_value(ClientEvent::SessionUpdate {
            session: snapshot.session_config(),
        })
        .unwrap();
        assert_eq!(json["session"]["audio"]["output"]["voice"], "cedar");

        let config = voice_config(Backend::OpenAi, "", "sk-secret".to_owned(), &snapshot);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-secret"));
        let debug = format!("{snapshot:?}");
        assert!(
            !debug.contains("sk-"),
            "the snapshot never carries credential material: {debug}"
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

    #[test]
    fn a_stale_response_done_cannot_clear_a_newer_turn() {
        // A committed turn whose response is in flight: its `done` is the
        // only event allowed to clear the retained audio.
        let mut turn = Turn::new();
        turn.replay.push(&[1, 2, 3]);
        turn.speech_stopped();
        turn.response_created(Some("resp_a".to_owned()));
        turn.replay_safe = false;

        // A done for a different response is stale: the turn stays open and
        // the retained audio survives.
        assert!(!turn.response_done(Some("resp_stale")));
        assert!(turn.turn_open);
        assert!(!turn.replay.chunks.is_empty());
        assert!(!turn.replay_safe);

        // The matching done closes the turn and clears the retained copy.
        assert!(turn.response_done(Some("resp_a")));
        assert!(!turn.turn_open);
        assert!(turn.replay.chunks.is_empty());
        assert!(turn.replay_safe);
        assert_eq!(turn.exhaustion(), Exhaustion::Reconnect);
    }

    #[test]
    fn a_barge_in_invalidates_the_interrupted_response() {
        // The committed turn's response is superseded by new speech: its
        // late `done` is stale and must not touch the new utterance.
        let mut turn = Turn::new();
        turn.replay.push(&[1, 2, 3]);
        turn.speech_stopped();
        turn.response_created(Some("resp_a".to_owned()));
        assert_eq!(turn.exhaustion(), Exhaustion::ReplayCommitted);

        turn.speech_started();
        turn.replay.push(&[4, 5, 6]);

        assert!(
            !turn.response_done(Some("resp_a")),
            "canceled response's done"
        );
        assert!(!turn.response_done(None), "no response is tracked yet");
        assert!(!turn.replay.committed, "the new utterance is uncommitted");
        assert_eq!(
            turn.exhaustion(),
            Exhaustion::Reappend,
            "the new utterance's retained chunks still re-append"
        );
    }

    #[test]
    fn mute_only_toggles_a_live_session() {
        for phase in [
            VoicePhase::Listening,
            VoicePhase::Thinking,
            VoicePhase::Speaking,
        ] {
            assert_eq!(mute_decision(true, phase), Mute::Toggle, "{phase:?}");
            assert_eq!(mute_decision(false, phase), Mute::Skip, "dead {phase:?}");
        }
        for phase in [VoicePhase::Idle, VoicePhase::Connecting, VoicePhase::Error] {
            assert_eq!(mute_decision(true, phase), Mute::Skip, "{phase:?}");
        }

        let status = VoiceStatus {
            phase: VoicePhase::Listening,
            muted: true,
            ..VoiceStatus::default()
        };
        assert_eq!(status.level(), 0.0, "a muted status never exposes a meter peak");
    }

    #[test]
    fn response_done_matching_rules() {
        let mut turn = Turn::new();
        // No response was ever created: every done is stale.
        assert!(!turn.response_done(Some("resp_x")));
        assert!(!turn.response_done(None));

        // A created response that carried no id accepts the next done
        // unconditionally: it is the only candidate.
        turn.response_created(None);
        assert!(turn.response_done(Some("resp_any")));

        turn.response_created(Some("resp_b".to_owned()));
        assert!(
            turn.response_done(None),
            "an id-less done cannot be checked"
        );
    }

    #[tokio::test]
    async fn the_dial_deadline_bounds_a_stalled_connect() {
        let stalled = bounded_connect(
            Duration::from_millis(5),
            std::future::pending::<Result<VoiceSession, VoiceError>>(),
        )
        .await;
        assert!(matches!(stalled, Err(VoiceError::Timeout)));

        // An inner result passes through untouched on either side of the
        // deadline.
        let ready = bounded_connect(
            Duration::from_secs(5),
            std::future::ready(Err(VoiceError::Disconnected)),
        )
        .await;
        assert!(matches!(ready, Err(VoiceError::Disconnected)));
    }

    #[test]
    fn the_retained_buffer_keeps_filling_while_a_dial_is_pending() {
        // The reconnect path is a state property, not a socket test: while
        // the dial runs, capture chunks keep landing in the same bounded
        // buffer the fresh socket later re-appends.
        let mut turn = Turn::new();
        turn.replay.push(&[1, 2, 3]);
        assert_eq!(turn.exhaustion(), Exhaustion::Reappend);

        // Frames that arrive mid-dial join the uncommitted utterance, so
        // the re-append emits each chunk exactly once.
        turn.replay.push(&[4, 5, 6]);
        assert_eq!(turn.replay.chunks.len(), 2);
        assert_eq!(turn.replay.samples, 6);
        assert_eq!(turn.exhaustion(), Exhaustion::Reappend);
        let events = reconnect_events(Exhaustion::Reappend, &turn.replay, &turn.pending);
        assert_eq!(events.len(), 2, "each retained chunk appends once");
        assert!(
            events
                .iter()
                .all(|event| matches!(event, ClientEvent::InputAudioBufferAppend { .. })),
            "an uncommitted re-append emits only appends"
        );
    }

    #[test]
    fn committed_replay_keeps_mid_dial_capture_as_the_next_utterance() {
        // A committed replay must stay byte-identical, so frames that
        // arrive while the reconnect dial runs join the bounded pending
        // buffer instead.
        let mut turn = Turn::new();
        turn.replay.push(&[1, 2, 3]);
        turn.speech_stopped();
        assert!(turn.replay.committed);
        assert_eq!(turn.exhaustion(), Exhaustion::ReplayCommitted);

        turn.pending.push(&[4, 5, 6]);
        turn.pending.push(&[7, 8]);
        assert_eq!(turn.replay.chunks.len(), 1);
        assert_eq!(turn.replay.samples, 3, "the committed replay is unchanged");
        assert_eq!(turn.pending.chunks.len(), 2);
        assert_eq!(turn.pending.samples, 5, "pending capture stays bounded");

        // The landed reconnect sends the old turn first - its append(s),
        // an explicit commit, and the response request - then the
        // pending-dial append(s) as the next uncommitted utterance.
        let events = reconnect_events(Exhaustion::ReplayCommitted, &turn.replay, &turn.pending);
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match event {
                ClientEvent::InputAudioBufferAppend { .. } => "append",
                ClientEvent::InputAudioBufferCommit => "commit",
                ClientEvent::ResponseCreate { .. } => "response",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["append", "commit", "response", "append", "append"]
        );
        let audio: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                ClientEvent::InputAudioBufferAppend { audio } => Some(audio),
                _ => None,
            })
            .collect();
        assert_eq!(
            audio,
            [
                pcm16_to_base64(&[1, 2, 3]),
                pcm16_to_base64(&[4, 5, 6]),
                pcm16_to_base64(&[7, 8]),
            ]
            .iter()
            .collect::<Vec<_>>(),
            "old committed audio precedes the pending utterance"
        );
    }

    #[test]
    fn reconnect_moves_mid_dial_capture_instead_of_dropping_it() {
        // Nothing was retained when the socket died, so the decision is
        // Reconnect; frames that then arrive mid-dial are uncommitted new
        // speech and still seed the fresh socket.
        let mut turn = Turn::new();
        assert_eq!(turn.exhaustion(), Exhaustion::Reconnect);
        turn.replay.push(&[9, 9]);
        let events = reconnect_events(Exhaustion::Reconnect, &turn.replay, &turn.pending);
        assert_eq!(events.len(), 1, "mid-dial capture is re-appended");

        // Notify moves only the pending utterance: the committed turn
        // itself cannot be replayed, but mid-dial capture is not dropped.
        let mut turn = Turn::new();
        turn.replay.push(&[1, 2, 3]);
        turn.speech_stopped();
        turn.replay_safe = false;
        assert_eq!(turn.exhaustion(), Exhaustion::Notify);
        turn.pending.push(&[4, 5]);
        let events = reconnect_events(Exhaustion::Notify, &turn.replay, &turn.pending);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(
                &events[0],
                ClientEvent::InputAudioBufferAppend { audio }
                    if audio == &pcm16_to_base64(&[4, 5])
            ),
            "the mid-dial utterance re-appends uncommitted"
        );

        // A truncated retained copy emits nothing: a partial utterance is
        // never presented as whole.
        let mut turn = Turn::new();
        turn.replay.push(&vec![0i16; REPLAY_SAMPLES]);
        turn.replay.push(&[1]);
        assert!(!turn.replay.complete);
        let events = reconnect_events(Exhaustion::Reappend, &turn.replay, &turn.pending);
        assert!(events.is_empty(), "truncated audio is never re-appended");
    }
}

//! Persisted live-voice preferences and the owned start-of-session snapshot.
//!
//! [`VoiceSettings`] is the `voice` block of `ui-settings.json`: the saved
//! microphone, the explicitly picked model and voice, assistant-speech
//! notifications, editable instructions, server-VAD tuning, noise reduction,
//! and the playback pipeline's gain/limiter/buffer knobs. Everything is
//! serde-defaulted and healed in [`VoiceSettings::clamped`] on load and on
//! every store update, so a hand-edited file degrades field-by-field instead
//! of discarding the whole settings file.
//!
//! Model and voice persist only an *explicit* pick (`Option`): until the user
//! chooses one, resolution falls through to the trimmed
//! `OPENAI_REALTIME_MODEL`/`OPENAI_REALTIME_VOICE` environment variables and
//! then the compiled defaults, so environment compatibility survives
//! untouched while an ordinary save never serializes env-derived values.
//!
//! Never persist credentials, OAuth material, or endpoint URLs here: keys are
//! read fresh from the environment at connect time and the CPA socket URL is
//! derived from the resolved model.
//!
//! [`VoiceStartSnapshot`] freezes the settings + environment a session was
//! started with; the runner builds its `SessionConfig`, `VoiceConfig`, and
//! `AudioEngineConfig` from it so nothing can drift mid-session.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeron_voice::{AudioDevicePreferences, AudioEngineConfig, PlaybackTuning, SessionConfig};

/// Realtime model used when neither a persisted pick nor
/// `OPENAI_REALTIME_MODEL` names one.
pub const DEFAULT_MODEL: &str = "gpt-realtime-2.1";
/// Output voice used when neither a persisted pick nor
/// `OPENAI_REALTIME_VOICE` names one.
pub const DEFAULT_VOICE: &str = "marin";
/// CPA realtime socket base. `cpa-tunnel.service` forwards it to the CPA
/// proxy; the resolved model is appended as `?model=` per dial.
/// `CPA_REALTIME_ENDPOINT` overrides the whole URL.
const DEFAULT_CPA_ENDPOINT_BASE: &str = "ws://127.0.0.1:8317/v1/realtime";

/// Default spoken-assistant persona; persisted so the instructions row can
/// reset to it and so the file shows what the session actually hears.
pub const DEFAULT_INSTRUCTIONS: &str = "You are the voice interface of \
    Noches, a local coding-workspace app. Replies are spoken aloud, so \
    answer briefly and plainly - no markdown, no lists longer than a few \
    items. Use the provided tools to inspect threads and act in the \
    workspace instead of guessing; read tools are always safe. \
    send_to_thread requires the user's Allow grant in the workspace - if a \
    call comes back denied, say so and stop instead of retrying.";

/// Server-VAD persistence bounds. The Realtime API accepts threshold
/// `0.0..=1.0` and millisecond paddings into the thousands; the compiled
/// defaults keep Handsfree's reviewed tuning.
pub const VAD_THRESHOLD_MIN: f32 = 0.0;
pub const VAD_THRESHOLD_MAX: f32 = 1.0;
pub const VAD_THRESHOLD_DEFAULT: f32 = 0.75;
pub const VAD_PREFIX_MS_MIN: u32 = 0;
pub const VAD_PREFIX_MS_MAX: u32 = 5_000;
pub const VAD_PREFIX_MS_DEFAULT: u32 = 300;
pub const VAD_SILENCE_MS_MIN: u32 = 0;
pub const VAD_SILENCE_MS_MAX: u32 = 5_000;
pub const VAD_SILENCE_MS_DEFAULT: u32 = 700;

/// `AudioEngineConfig::validate` bounds, mirrored so persistence clamps heal
/// into exactly the range the engine accepts. The ceiling's lower bound is
/// exclusive on the engine side (`(0.0, 1.0]`), so a persisted 0.0 heals up
/// to a small positive floor rather than being left invalid.
pub const GAIN_MIN: f32 = 0.0;
pub const GAIN_MAX: f32 = 8.0;
pub const GAIN_DEFAULT: f32 = 1.0;
pub const LIMITER_CEILING_MIN: f32 = 0.05;
pub const LIMITER_CEILING_MAX: f32 = 1.0;
pub const LIMITER_CEILING_DEFAULT: f32 = 0.95;
pub const RING_MS_MIN: u64 = 20;
pub const RING_MS_MAX: u64 = 5_000;
pub const RING_MS_DEFAULT: u64 = 200;
pub const PREBUFFER_MS_DEFAULT: u64 = 120;

/// Server-side input noise reduction for the realtime session.
///
/// `NearField` doubles as serde's `other` catch-all (which serde requires
/// on the last variant), so an unrecognized persisted spelling - a mode a
/// newer build added, or a hand-edited value - heals to the safe default
/// for this one field instead of failing the whole `UiSettings` parse.
/// Serialization still emits only the three real variants: `other` is a
/// deserialization fallback, not a persisted fourth value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseReductionMode {
    /// Speakerphone-style capture: the server suppresses ambient/far noise.
    FarField,
    /// No server-side noise reduction.
    Off,
    /// Headset-style capture: the server suppresses close-talk noise.
    #[default]
    #[serde(other)]
    NearField,
}

impl NoiseReductionMode {
    /// The `session.audio.input.noise_reduction` payload, or `None` when the
    /// setting is off so the field is omitted entirely.
    fn payload(self) -> Option<serde_json::Value> {
        match self {
            Self::NearField => Some(serde_json::json!({"type": "near_field"})),
            Self::FarField => Some(serde_json::json!({"type": "far_field"})),
            Self::Off => None,
        }
    }
}

/// The `voice` block of `ui-settings.json`. Field-missing tolerant
/// (`#[serde(default)]` on the struct); numeric healing lives in
/// [`Self::clamped`], which `UiSettings::clamped` runs on load and after
/// every store update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VoiceSettings {
    /// Saved microphone choice: CPAL device id plus the remembered label so
    /// a restart that rotates ids still re-matches by name. Both empty means
    /// the system default input.
    pub microphone: AudioDevicePreferences,
    /// The explicitly picked realtime model; `None` until the user picks
    /// one, keeping `OPENAI_REALTIME_MODEL` in the resolution chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The explicitly picked output voice; `None` keeps
    /// `OPENAI_REALTIME_VOICE`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Speak a short notice when a voice turn finishes (Handsfree parity).
    pub notifications: bool,
    /// Editable system instructions sent as `session.instructions`.
    pub instructions: String,
    /// Server-VAD speech probability threshold.
    pub vad_threshold: f32,
    /// Audio kept ahead of the detected speech start (ms).
    pub vad_prefix_padding_ms: u32,
    /// Trailing silence that ends an utterance (ms).
    pub vad_silence_duration_ms: u32,
    /// Server-side input noise reduction mode.
    pub noise_reduction: NoiseReductionMode,
    /// Linear output gain applied before the limiter.
    pub output_gain: f32,
    /// Level above which the soft limiter folds peaks toward 1.0.
    pub limiter_ceiling: f32,
    /// Whether the soft limiter shapes loud output. Disabling it keeps the
    /// stored ceiling untouched - [`VoiceStartSnapshot::engine_config`] maps
    /// a disabled limiter to a 1.0 ceiling at session start, which
    /// degenerates to the hard clamp the API already documents
    /// (`PlaybackTuning::limiter_ceiling`), so re-enabling restores the
    /// user's chosen value.
    pub limiter_enabled: bool,
    /// Output ring capacity (ms): playback-scheduling jitter absorption.
    pub playback_ring_ms: u64,
    /// Audio buffered before playback unmutes, and again after each
    /// mid-stream starvation (ms). Never exceeds the ring after healing.
    pub playback_prebuffer_ms: u64,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            microphone: AudioDevicePreferences::default(),
            model: None,
            voice: None,
            notifications: true,
            instructions: DEFAULT_INSTRUCTIONS.to_owned(),
            vad_threshold: VAD_THRESHOLD_DEFAULT,
            vad_prefix_padding_ms: VAD_PREFIX_MS_DEFAULT,
            vad_silence_duration_ms: VAD_SILENCE_MS_DEFAULT,
            noise_reduction: NoiseReductionMode::default(),
            output_gain: GAIN_DEFAULT,
            limiter_ceiling: LIMITER_CEILING_DEFAULT,
            limiter_enabled: true,
            playback_ring_ms: RING_MS_DEFAULT,
            playback_prebuffer_ms: PREBUFFER_MS_DEFAULT,
        }
    }
}

impl VoiceSettings {
    /// Heals persisted values into the ranges
    /// [`AudioEngineConfig::validate`] and the Realtime API accept: NaN and
    /// infinities fall back to defaults, the prebuffer never exceeds the
    /// ring, and the limiter ceiling clamps into `(0.05, 1.0]` regardless
    /// of the enable flag - the flag only changes which ceiling the engine
    /// config hands the worker, so a disabled limiter never loses the
    /// user's chosen ceiling.
    pub fn clamped(mut self) -> Self {
        self.model = Self::model_pick(self.model.take());
        self.voice = Self::voice_pick(self.voice.take());
        let defaults = Self::default();
        self.vad_threshold = clamp_or(
            self.vad_threshold,
            VAD_THRESHOLD_MIN,
            VAD_THRESHOLD_MAX,
            defaults.vad_threshold,
        );
        self.vad_prefix_padding_ms = self
            .vad_prefix_padding_ms
            .clamp(VAD_PREFIX_MS_MIN, VAD_PREFIX_MS_MAX);
        self.vad_silence_duration_ms = self
            .vad_silence_duration_ms
            .clamp(VAD_SILENCE_MS_MIN, VAD_SILENCE_MS_MAX);
        self.output_gain = clamp_or(self.output_gain, GAIN_MIN, GAIN_MAX, defaults.output_gain);
        self.limiter_ceiling = clamp_or(
            self.limiter_ceiling,
            LIMITER_CEILING_MIN,
            LIMITER_CEILING_MAX,
            defaults.limiter_ceiling,
        );
        self.playback_ring_ms = self.playback_ring_ms.clamp(RING_MS_MIN, RING_MS_MAX);
        self.playback_prebuffer_ms = self.playback_prebuffer_ms.min(self.playback_ring_ms);
        self
    }

    /// The model a session started now would use: an explicit persisted pick
    /// first, then trimmed `OPENAI_REALTIME_MODEL`, then
    /// [`DEFAULT_MODEL`]. Pure apart from the env read; callers that compare
    /// resolutions should prefer [`resolve_model`].
    pub fn resolved_model(&self) -> String {
        resolve_model(
            self.model.as_deref(),
            std::env::var("OPENAI_REALTIME_MODEL").ok(),
        )
    }

    /// The voice a session started now would use: explicit pick, then
    /// trimmed `OPENAI_REALTIME_VOICE`, then [`DEFAULT_VOICE`].
    pub fn resolved_voice(&self) -> String {
        resolve_voice(
            self.voice.as_deref(),
            std::env::var("OPENAI_REALTIME_VOICE").ok(),
        )
    }

    /// The persisted model pick healed for persistence: trimmed, or `None`
    /// when blank so a cleared picker row restores env/default resolution
    /// instead of saving an empty string.
    pub fn model_pick(model: Option<String>) -> Option<String> {
        model.and_then(|model| {
            let trimmed = model.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
    }

    /// Same healing as [`Self::model_pick`] for the voice pick.
    pub fn voice_pick(voice: Option<String>) -> Option<String> {
        Self::model_pick(voice)
    }
}

/// Explicit pick first, then a trimmed environment value, then the default.
/// The env parameter is an `Option` (not a `&str` read) so the precedence is
/// testable without mutating process state.
fn resolve_model(pick: Option<&str>, env: Option<String>) -> String {
    pick.and_then(trimmed)
        .or_else(|| env.and_then(|value| trimmed(&value)))
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

fn resolve_voice(pick: Option<&str>, env: Option<String>) -> String {
    pick.and_then(trimmed)
        .or_else(|| env.and_then(|value| trimmed(&value)))
        .unwrap_or_else(|| DEFAULT_VOICE.to_owned())
}

fn trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn clamp_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

/// The default CPA socket URL for `model`. Kept as a helper (not a constant)
/// because the model is user-selectable: the URL carries no credential
/// material, and the model is the only query parameter.
///
/// The model is persisted/env-derived and therefore arbitrary text: it is
/// appended through the form-urlencoded serializer so a value containing
/// `&`, `?`, `#`, spaces, or `%` stays ONE `model` value instead of
/// mutating the URL with extra keys or a fragment. Ordinary model ids are
/// unreserved characters, so their URL is byte-identical to the old
/// `?model={model}` interpolation.
pub fn default_cpa_endpoint(model: &str) -> String {
    let mut url = url::Url::parse(DEFAULT_CPA_ENDPOINT_BASE)
        .expect("DEFAULT_CPA_ENDPOINT_BASE is a valid absolute URL");
    url.query_pairs_mut().append_pair("model", model);
    url.into()
}

/// The CPA endpoint a dial uses: `CPA_REALTIME_ENDPOINT` verbatim when set
/// (it is already the operator's full URL, query included), else the default
/// base plus `?model=` for the resolved model.
fn cpa_endpoint(env_override: Option<String>, model: &str) -> String {
    env_override
        .and_then(|value| trimmed(&value))
        .unwrap_or_else(|| default_cpa_endpoint(model))
}

/// The `session.update` payload a snapshot starts with: PCM16 in/out,
/// server VAD with the persisted tuning, the picked instructions and voice,
/// and the reviewed tool allowlist.
fn session_config(snapshot: &VoiceStartSnapshot) -> SessionConfig {
    let mut config = SessionConfig::default()
        .pcm16_audio()
        .with_voice(snapshot.voice.clone())
        .with_turn_detection(serde_json::json!({
            "type": "server_vad",
            "threshold": snapshot.vad_threshold,
            "prefix_padding_ms": snapshot.vad_prefix_padding_ms,
            "silence_duration_ms": snapshot.vad_silence_duration_ms,
            "create_response": true,
            "interrupt_response": true,
        }))
        .with_instructions(snapshot.instructions.clone())
        .with_tools(crate::voice::tool_definitions())
        .with_parallel_tool_calls(false);
    if let Some(noise_reduction) = snapshot.noise_reduction.payload() {
        config = config.with_noise_reduction(noise_reduction);
    }
    config
}

/// The owned settings + environment a voice session was started with. Built
/// synchronously in [`crate::voice::VoiceController::start`] from
/// `settings::current(cx).voice` plus `std::env`, then moved into the tokio
/// runner, so a settings save or environment difference can never change a
/// live session's model, voice, instructions, VAD, noise reduction,
/// microphone, or DSP.
///
/// Carries no credential material: keys are read fresh at connect and
/// reconnect time so a rotation can pick up a swapped `CPA_API_KEY`.
#[derive(Debug, Clone)]
pub struct VoiceStartSnapshot {
    /// Resolved model id used for both the session and the default CPA URL.
    pub model: String,
    /// Resolved output voice.
    pub voice: String,
    /// Session instructions, verbatim from the persisted editable row.
    pub instructions: String,
    /// Speak a notice when a voice turn completes.
    pub notifications: bool,
    /// Server-VAD threshold (persisted, clamped).
    pub vad_threshold: f32,
    /// Server-VAD prefix padding (ms).
    pub vad_prefix_padding_ms: u32,
    /// Server-VAD trailing silence (ms).
    pub vad_silence_duration_ms: u32,
    /// Server-side input noise reduction.
    pub noise_reduction: NoiseReductionMode,
    /// Engine input + playback tuning, pre-validated by construction.
    pub audio_config: AudioEngineConfig,
    /// `CPA_REALTIME_ENDPOINT` verbatim when the operator set it.
    pub cpa_endpoint_override: Option<String>,
}

impl VoiceStartSnapshot {
    /// Freezes `settings` plus the process environment. The settings are
    /// expected to have passed [`VoiceSettings::clamped`] already (the store
    /// clamps on load and on every update); the engine config is built
    /// through the same bounds so `AudioEngine::start_with` cannot reject it.
    pub fn new(settings: &VoiceSettings) -> Self {
        let model = resolve_model(
            settings.model.as_deref(),
            std::env::var("OPENAI_REALTIME_MODEL").ok(),
        );
        let voice = resolve_voice(
            settings.voice.as_deref(),
            std::env::var("OPENAI_REALTIME_VOICE").ok(),
        );
        Self {
            model,
            voice,
            instructions: settings.instructions.clone(),
            notifications: settings.notifications,
            vad_threshold: settings.vad_threshold,
            vad_prefix_padding_ms: settings.vad_prefix_padding_ms,
            vad_silence_duration_ms: settings.vad_silence_duration_ms,
            noise_reduction: settings.noise_reduction,
            audio_config: Self::engine_config(settings),
            cpa_endpoint_override: std::env::var("CPA_REALTIME_ENDPOINT")
                .ok()
                .and_then(|value| trimmed(&value)),
        }
    }

    /// Snapshot build for an explicit environment - the pure core of
    /// [`Self::new`], so precedence and URL derivation are testable without
    /// mutating process env in parallel tests.
    pub fn with_env(
        settings: &VoiceSettings,
        model_env: Option<String>,
        voice_env: Option<String>,
        cpa_endpoint: Option<String>,
    ) -> Self {
        let mut snapshot = Self::new(settings);
        snapshot.model = resolve_model(settings.model.as_deref(), model_env);
        snapshot.voice = resolve_voice(settings.voice.as_deref(), voice_env);
        snapshot.cpa_endpoint_override = cpa_endpoint.and_then(|value| trimmed(&value));
        snapshot
    }

    /// The engine configuration the snapshot opens: the saved microphone
    /// preference plus the persisted playback tuning, with the limiter
    /// toggle folded into the ceiling the worker reads.
    fn engine_config(settings: &VoiceSettings) -> AudioEngineConfig {
        AudioEngineConfig {
            input: settings.microphone.clone(),
            playback: PlaybackTuning {
                ring: Duration::from_millis(settings.playback_ring_ms),
                start_buffer: Duration::from_millis(settings.playback_prebuffer_ms),
                gain: settings.output_gain,
                limiter_ceiling: if settings.limiter_enabled {
                    settings.limiter_ceiling
                } else {
                    LIMITER_CEILING_MAX
                },
            },
        }
    }

    /// The session.update payload this snapshot dials with.
    pub fn session_config(&self) -> SessionConfig {
        session_config(self)
    }

    /// The CPA socket URL for this snapshot: the operator override verbatim
    /// when set, else the default base carrying the resolved model.
    pub fn cpa_endpoint(&self) -> String {
        cpa_endpoint(self.cpa_endpoint_override.clone(), &self.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_voice::ClientEvent;

    fn settings() -> VoiceSettings {
        VoiceSettings::default()
    }

    #[test]
    fn files_from_before_voice_settings_load_with_defaults() {
        // A ui-settings.json written before the voice block existed must
        // keep every other key and default the whole block, not fail the
        // file or leak env-derived picks into it.
        let loaded: crate::settings::UiSettings =
            serde_json::from_str(r#"{"sidebarWidth": 300, "soundEnabled": false}"#).unwrap();
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled);
        assert_eq!(loaded.voice, VoiceSettings::default());
        assert_eq!(loaded.voice.model, None);
        assert_eq!(loaded.voice.voice, None);
        assert_eq!(loaded.voice.instructions, DEFAULT_INSTRUCTIONS);
    }

    #[test]
    fn a_persisted_voice_block_round_trips() {
        let mut settings = settings();
        settings.model = Some("gpt-realtime-2.1".to_owned());
        settings.voice = Some("cedar".to_owned());
        settings.notifications = false;
        settings.instructions = "Be terse.".to_owned();
        settings.vad_threshold = 0.5;
        settings.vad_prefix_padding_ms = 200;
        settings.vad_silence_duration_ms = 500;
        settings.noise_reduction = NoiseReductionMode::FarField;
        settings.output_gain = 2.0;
        settings.limiter_ceiling = 0.8;
        settings.limiter_enabled = false;
        settings.playback_ring_ms = 400;
        settings.playback_prebuffer_ms = 100;
        settings.microphone = AudioDevicePreferences {
            input_device_id: "alsa:hw:1,0".to_owned(),
            input_label: "USB Mic".to_owned(),
        };
        // Store semantics: every update heals first. The disabled limiter
        // keeps its chosen ceiling - only the session's engine config maps
        // disabled to the degenerate 1.0 hard clamp.
        let settings = settings.clamped();
        let parsed: VoiceSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(parsed, settings);
        assert_eq!(parsed.limiter_ceiling, 0.8);
    }

    #[test]
    fn resolution_prefers_the_explicit_pick_then_env_then_default() {
        // No pick: the env value wins, trimmed.
        let mut settings = settings();
        assert_eq!(
            resolve_model(settings.model.as_deref(), Some("  gpt-live  ".into())),
            "gpt-live"
        );
        // Blank env falls through to the default.
        assert_eq!(
            resolve_model(settings.model.as_deref(), Some("   ".into())),
            DEFAULT_MODEL
        );
        assert_eq!(
            resolve_model(settings.model.as_deref(), None),
            DEFAULT_MODEL
        );
        // An explicit pick beats any environment.
        settings.model = Some(" picked ".to_owned());
        assert_eq!(
            resolve_model(
                VoiceSettings::model_pick(settings.model.clone()).as_deref(),
                Some("gpt-live".into())
            ),
            "picked"
        );
        // A blank pick heals to None rather than serializing "".
        assert_eq!(VoiceSettings::model_pick(Some("  ".into())), None);
        assert_eq!(VoiceSettings::voice_pick(None), None);
    }

    #[test]
    fn cpa_url_derives_from_the_resolved_model_unless_overridden() {
        let mut settings = settings();
        settings.model = Some("gpt-realtime-2.1".to_owned());
        let snapshot = VoiceStartSnapshot::with_env(&settings, None, None, None);
        assert_eq!(
            snapshot.cpa_endpoint(),
            "ws://127.0.0.1:8317/v1/realtime?model=gpt-realtime-2.1"
        );

        // The operator override is used verbatim, query string included.
        let snapshot = VoiceStartSnapshot::with_env(
            &settings,
            None,
            None,
            Some("ws://10.0.0.2:8317/v1/realtime?model=custom".to_owned()),
        );
        assert_eq!(
            snapshot.cpa_endpoint(),
            "ws://10.0.0.2:8317/v1/realtime?model=custom"
        );

        // A blank override falls back to the derived default.
        let snapshot = VoiceStartSnapshot::with_env(&settings, None, None, Some("   ".to_owned()));
        assert_eq!(
            snapshot.cpa_endpoint(),
            "ws://127.0.0.1:8317/v1/realtime?model=gpt-realtime-2.1"
        );
    }

    #[test]
    fn a_hostile_model_stays_one_encoded_query_value() {
        // A persisted/env model is arbitrary text: `&` must not split into a
        // second parameter, `#` must not start a fragment, and spaces/`%`/`?`
        // must not mutate the URL shape. The whole string survives as the
        // single `model` value.
        let hostile = "gpt &evil=1#frag ?x%25";
        let url = default_cpa_endpoint(hostile);
        assert!(
            url.starts_with("ws://127.0.0.1:8317/v1/realtime?model="),
            "{url}"
        );
        assert!(!url.contains('#'), "fragment injected: {url}");
        let query = url.split_once('?').unwrap().1;
        let pairs = url::form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
        assert_eq!(pairs.len(), 1, "extra query key injected: {url}");
        assert_eq!(pairs[0].0, "model");
        assert_eq!(pairs[0].1, hostile);

        // Ordinary model ids serialize exactly as before.
        assert_eq!(
            default_cpa_endpoint("gpt-realtime-2.1"),
            "ws://127.0.0.1:8317/v1/realtime?model=gpt-realtime-2.1"
        );

        // The operator override is a full URL already - never re-encoded.
        let snapshot = VoiceStartSnapshot::with_env(
            &settings(),
            None,
            None,
            Some("ws://10.0.0.2:8317/v1/realtime?model=a%20b&x=1".to_owned()),
        );
        assert_eq!(
            snapshot.cpa_endpoint(),
            "ws://10.0.0.2:8317/v1/realtime?model=a%20b&x=1"
        );
    }

    #[test]
    fn clamps_match_the_engine_ranges_and_prebuffer_never_beats_ring() {
        let mut settings = settings();
        settings.vad_threshold = 4.0;
        settings.vad_prefix_padding_ms = 99_999;
        settings.vad_silence_duration_ms = 0;
        settings.output_gain = f32::NAN;
        settings.limiter_ceiling = -2.0;
        settings.playback_ring_ms = 0;
        settings.playback_prebuffer_ms = 9_999;

        let healed = settings.clamped();
        assert_eq!(healed.vad_threshold, VAD_THRESHOLD_MAX);
        assert_eq!(healed.vad_prefix_padding_ms, VAD_PREFIX_MS_MAX);
        assert_eq!(healed.vad_silence_duration_ms, VAD_SILENCE_MS_MIN);
        assert_eq!(healed.output_gain, GAIN_DEFAULT);
        assert_eq!(healed.limiter_ceiling, LIMITER_CEILING_MIN);
        assert_eq!(healed.playback_ring_ms, RING_MS_MIN);
        assert!(
            healed.playback_prebuffer_ms <= healed.playback_ring_ms,
            "prebuffer {} exceeds ring {}",
            healed.playback_prebuffer_ms,
            healed.playback_ring_ms
        );

        // The healed engine config always validates.
        VoiceStartSnapshot::with_env(&healed, None, None, None)
            .audio_config
            .validate()
            .unwrap();
    }

    #[test]
    fn a_disabled_limiter_keeps_its_ceiling_but_the_engine_gets_1_0() {
        // The persisted ceiling is preserved across healing so toggling the
        // limiter back on restores it; the runtime still gets the degenerate
        // 1.0 hard clamp via the engine config.
        let mut settings = settings();
        settings.limiter_enabled = false;
        settings.limiter_ceiling = 0.5;
        let healed = settings.clamped();
        assert_eq!(healed.limiter_ceiling, 0.5);
        assert!(!healed.limiter_enabled);
        assert_eq!(
            VoiceStartSnapshot::with_env(&healed, None, None, None)
                .audio_config
                .playback
                .limiter_ceiling,
            1.0
        );

        // Re-enabling restores the stored ceiling exactly.
        let mut reenabled = healed.clone();
        reenabled.limiter_enabled = true;
        assert_eq!(
            VoiceStartSnapshot::with_env(&reenabled, None, None, None)
                .audio_config
                .playback
                .limiter_ceiling,
            0.5
        );
    }

    #[test]
    fn unknown_or_legacy_fields_degrade_without_losing_the_file() {
        // Missing keys default and extra keys are ignored; an unrecognized
        // noiseReduction spelling heals per-field via serde's `other`
        // fallback on the default variant (covered below).
        let parsed: VoiceSettings = serde_json::from_str(
            r#"{"notifications": false, "unknownKnob": 1, "vadThreshold": 0.5}"#,
        )
        .unwrap();
        assert!(!parsed.notifications);
        assert_eq!(parsed.vad_threshold, 0.5);
        assert_eq!(parsed.model, None);
        assert_eq!(parsed.instructions, DEFAULT_INSTRUCTIONS);
    }

    #[test]
    fn an_unknown_noise_reduction_mode_heals_to_near_field() {
        // A persisted mode this build does not know (a newer variant, or a
        // hand edit) must fail only its own field: the rest of the voice
        // block and every unrelated UiSettings key survive, and the mode
        // falls back to the safe default.
        let loaded: crate::settings::UiSettings = serde_json::from_str(
            r#"{
                "sidebarWidth": 300,
                "soundEnabled": false,
                "voice": {
                    "noiseReduction": "beam_forming",
                    "vadThreshold": 0.5,
                    "notifications": false
                }
            }"#,
        )
        .unwrap();
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled);
        assert_eq!(loaded.voice.noise_reduction, NoiseReductionMode::NearField);
        assert_eq!(loaded.voice.vad_threshold, 0.5);
        assert!(!loaded.voice.notifications);

        // The fallback is not a persisted fourth variant: healing and
        // re-serializing only ever emit the three real spellings.
        let json = serde_json::to_value(loaded.voice.noise_reduction).unwrap();
        assert_eq!(json, "near_field");
    }

    #[test]
    fn nothing_secret_or_env_derived_serializes() {
        let mut settings = settings();
        settings.model = Some("gpt-realtime-2.1".to_owned());
        settings.voice = Some("marin".to_owned());
        let json = serde_json::to_value(&settings).unwrap();
        let text = json.to_string();
        for needle in [
            "apiKey", "api_key", "token", "endpoint", "ws://", "wss://", "cpa",
        ] {
            assert!(!text.contains(needle), "serialized `{needle}`: {text}");
        }
        // Unset picks stay absent entirely - no env-derived value is ever
        // written back.
        let json = serde_json::to_value(&VoiceSettings::default()).unwrap();
        assert!(json.get("model").is_none());
        assert!(json.get("voice").is_none());
    }

    #[test]
    fn snapshot_builds_the_session_payload_and_engine_config() {
        let mut settings = settings();
        settings.model = Some("gpt-realtime-2.1".to_owned());
        settings.voice = Some("marin".to_owned());
        settings.microphone = AudioDevicePreferences {
            input_device_id: "alsa:hw:1,0".to_owned(),
            input_label: "USB Mic".to_owned(),
        };
        settings.vad_threshold = 0.6;
        settings.vad_prefix_padding_ms = 250;
        settings.vad_silence_duration_ms = 900;
        settings.output_gain = 1.5;
        settings.limiter_ceiling = 0.9;
        settings.playback_ring_ms = 300;
        settings.playback_prebuffer_ms = 150;
        let settings = settings.clamped();
        let snapshot = VoiceStartSnapshot::with_env(&settings, None, None, None);

        let audio = &snapshot.audio_config;
        assert_eq!(audio.input.input_device_id, "alsa:hw:1,0");
        assert_eq!(audio.input.input_label, "USB Mic");
        assert_eq!(audio.playback.ring, Duration::from_millis(300));
        assert_eq!(audio.playback.start_buffer, Duration::from_millis(150));
        assert_eq!(audio.playback.gain, 1.5);
        assert_eq!(audio.playback.limiter_ceiling, 0.9);
        audio.validate().unwrap();

        let json = serde_json::to_value(ClientEvent::SessionUpdate {
            session: snapshot.session_config(),
        })
        .unwrap();
        let session = &json["session"];
        let turn_detection = &session["audio"]["input"]["turn_detection"];
        assert_eq!(turn_detection["type"], "server_vad");
        // f32 round-trips through JSON as the f64 nearest the stored value;
        // compare the f32 view so 0.6 does not differ in the last digit.
        assert_eq!(turn_detection["threshold"].as_f64().unwrap() as f32, 0.6f32);
        assert_eq!(turn_detection["prefix_padding_ms"], 250);
        assert_eq!(turn_detection["silence_duration_ms"], 900);
        assert_eq!(turn_detection["create_response"], true);
        assert_eq!(turn_detection["interrupt_response"], true);
        assert_eq!(
            session["audio"]["input"]["noise_reduction"],
            serde_json::json!({"type": "near_field"})
        );
        assert_eq!(session["audio"]["output"]["voice"], "marin");
        assert_eq!(session["instructions"], DEFAULT_INSTRUCTIONS);
        assert_eq!(session["parallel_tool_calls"], false);
    }

    #[test]
    fn far_field_and_off_noise_reduction_serialize_correctly() {
        let mut settings = settings();
        settings.noise_reduction = NoiseReductionMode::FarField;
        let snapshot = VoiceStartSnapshot::with_env(&settings, None, None, None);
        let session = serde_json::to_value(snapshot.session_config()).unwrap();
        assert_eq!(
            session["audio"]["input"]["noise_reduction"],
            serde_json::json!({"type": "far_field"})
        );

        settings.noise_reduction = NoiseReductionMode::Off;
        let snapshot = VoiceStartSnapshot::with_env(&settings, None, None, None);
        let session = serde_json::to_value(snapshot.session_config()).unwrap();
        assert!(
            session["audio"]["input"].get("noise_reduction").is_none(),
            "off omits the field entirely"
        );
    }
}

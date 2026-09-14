//! Typed codec for the OpenAI Realtime WebSocket protocol.
//!
//! Only the events this core handles are typed. Unknown event types decode to
//! [`ServerEvent::Unknown`] with the raw payload preserved, and known events
//! tolerate missing fields, so a protocol revision cannot abort a session.
//! Both the current GA names (`response.output_text.delta`,
//! `response.output_audio.delta`, ...) and the earlier beta aliases
//! (`response.text.delta`, `response.audio.delta`, ...) decode to the same
//! variants.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;

/// Extra fields preserved verbatim so newer protocol revisions keep working.
type Extra = BTreeMap<String, Value>;

/// Client -> server events.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ClientEvent {
    #[serde(rename = "session.update")]
    SessionUpdate { session: SessionConfig },
    #[serde(rename = "input_audio_buffer.append")]
    InputAudioBufferAppend { audio: String },
    #[serde(rename = "input_audio_buffer.commit")]
    InputAudioBufferCommit,
    #[serde(rename = "input_audio_buffer.clear")]
    InputAudioBufferClear,
    #[serde(rename = "conversation.item.create")]
    ConversationItemCreate {
        item: ConversationItem,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.truncate")]
    ConversationItemTruncate {
        item_id: String,
        content_index: u32,
        audio_end_ms: u32,
    },
    #[serde(rename = "response.create")]
    ResponseCreate {
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<ResponseConfig>,
    },
    #[serde(rename = "response.cancel")]
    ResponseCancel,
}

/// `session.update` payload. Every field is optional; unknown keys round-trip
/// through [`SessionConfig::extra`] so newer knobs can be set before this crate
/// grows typed support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Always `realtime`; the Realtime API requires it on session objects.
    #[serde(rename = "type", default = "realtime_session_type")]
    pub session_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    /// Nested `session.audio` input/output configuration.
    #[serde(default, skip_serializing_if = "AudioConfig::is_empty")]
    pub audio: AudioConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Set `false` to make the server hand over one function call at a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn realtime_session_type() -> String {
    "realtime".to_owned()
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            session_type: realtime_session_type(),
            instructions: None,
            output_modalities: None,
            audio: AudioConfig::default(),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            temperature: None,
            extra: Extra::default(),
        }
    }
}

/// Nested `session.audio` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default, skip_serializing_if = "AudioInput::is_empty")]
    pub input: AudioInput,
    #[serde(default, skip_serializing_if = "AudioOutput::is_empty")]
    pub output: AudioOutput,
}

impl AudioConfig {
    fn is_empty(&self) -> bool {
        self.input.is_empty() && self.output.is_empty()
    }
}

/// `session.audio.input`: capture format, turn detection, and server-side
/// noise reduction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<AudioFormat>,
    /// Server VAD configuration, e.g. `{"type": "server_vad"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_detection: Option<Value>,
    /// Server-side input noise reduction, e.g. `{"type": "near_field"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise_reduction: Option<Value>,
}

impl AudioInput {
    fn is_empty(&self) -> bool {
        self.format.is_none() && self.turn_detection.is_none() && self.noise_reduction.is_none()
    }
}

/// `session.audio.output`: playback format and voice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<AudioFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

impl AudioOutput {
    fn is_empty(&self) -> bool {
        self.format.is_none() && self.voice.is_none()
    }
}

/// Audio format descriptor. The Realtime API only supports 24 kHz `audio/pcm`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
}

impl AudioFormat {
    pub fn pcm(rate: Option<u32>) -> Self {
        Self {
            kind: "audio/pcm".to_owned(),
            rate,
        }
    }
}

impl SessionConfig {
    /// True when no field would be sent; `connect` skips the initial update.
    /// The always-present `type` field does not count.
    pub fn is_empty(&self) -> bool {
        self.instructions.is_none()
            && self.output_modalities.is_none()
            && self.audio.is_empty()
            && self.tools.is_none()
            && self.tool_choice.is_none()
            && self.parallel_tool_calls.is_none()
            && self.temperature.is_none()
            && self.extra.is_empty()
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<Value>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn with_tool_choice(mut self, tool_choice: Value) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Set `false` to request strict tool-call sequencing.
    pub fn with_parallel_tool_calls(mut self, parallel: bool) -> Self {
        self.parallel_tool_calls = Some(parallel);
        self
    }

    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.audio.output.voice = Some(voice.into());
        self
    }

    pub fn with_turn_detection(mut self, turn_detection: Value) -> Self {
        self.audio.input.turn_detection = Some(turn_detection);
        self
    }

    /// Server-side input noise reduction, e.g.
    /// `serde_json::json!({"type": "near_field"})` for headset-style
    /// near-field capture.
    pub fn with_noise_reduction(mut self, noise_reduction: Value) -> Self {
        self.audio.input.noise_reduction = Some(noise_reduction);
        self
    }

    /// Text-only replies; used by the headless text harness. Audio arrives with
    /// the later `pcm16_audio` configuration.
    pub fn text_only(mut self) -> Self {
        self.output_modalities = Some(vec!["text".to_owned()]);
        self
    }

    /// 24 kHz PCM16 in and out, matching the codec helpers in [`crate::audio`].
    pub fn pcm16_audio(mut self) -> Self {
        self.audio.input.format = Some(AudioFormat::pcm(Some(24_000)));
        self.audio.output.format = Some(AudioFormat::pcm(Some(24_000)));
        self
    }
}

/// Optional `response.create` overrides, e.g. a per-turn instruction or tool set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Conversation item carried by `conversation.item.create`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationItem {
    Message {
        role: String,
        content: Vec<ContentPart>,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

/// Content part of a message item.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText { text: String },
    Text { text: String },
}

impl ContentPart {
    pub fn input_text(text: impl Into<String>) -> Self {
        Self::InputText { text: text.into() }
    }
}

/// Server -> client events. Anything not listed decodes to
/// [`ServerEvent::Unknown`]; the session forwards all variants to the caller.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    SessionCreated {
        session: SessionInfo,
    },
    SessionUpdated {
        session: SessionInfo,
    },
    InputAudioBufferSpeechStarted {
        item_id: Option<String>,
        audio_start_ms: Option<u64>,
    },
    InputAudioBufferSpeechStopped {
        item_id: Option<String>,
        audio_end_ms: Option<u64>,
    },
    InputTranscriptionDelta {
        item_id: Option<String>,
        delta: String,
    },
    InputTranscriptionCompleted {
        item_id: Option<String>,
        transcript: String,
    },
    ResponseCreated {
        response_id: Option<String>,
    },
    ResponseDone {
        response: ResponseInfo,
    },
    ResponseOutputItemAdded {
        response_id: Option<String>,
        output_index: Option<u32>,
        item: OutputItem,
    },
    ResponseOutputItemDone {
        response_id: Option<String>,
        output_index: Option<u32>,
        item: OutputItem,
    },
    OutputTextDelta {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        content_index: Option<u32>,
        delta: String,
    },
    OutputTextDone {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        content_index: Option<u32>,
        text: String,
    },
    /// Base64 PCM16 delta; decode with [`crate::audio::base64_to_pcm16`].
    OutputAudioDelta {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        content_index: Option<u32>,
        delta: String,
    },
    OutputAudioDone {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        content_index: Option<u32>,
    },
    OutputAudioTranscriptDelta {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        content_index: Option<u32>,
        delta: String,
    },
    OutputAudioTranscriptDone {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        content_index: Option<u32>,
        transcript: String,
    },
    FunctionCallArgumentsDelta {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        call_id: Option<String>,
        delta: String,
    },
    /// The GA event omits the function name; the session resolves it from the
    /// preceding `response.output_item.added`/`done` for the same item.
    FunctionCallArgumentsDone {
        response_id: Option<String>,
        item_id: Option<String>,
        output_index: Option<u32>,
        call_id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    RateLimitsUpdated {
        rate_limits: Value,
    },
    Error {
        error: RealtimeError,
    },
    Unknown {
        kind: String,
        raw: Value,
    },
}

impl ServerEvent {
    /// Decodes one server frame. Only non-JSON payloads or payloads without a
    /// string `type` are errors; unknown types and evolving fields decode
    /// tolerantly instead of failing the session.
    pub fn decode(json: &str) -> Result<Self, DecodeError> {
        let raw: Value = serde_json::from_str(json)?;
        let kind = raw
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(DecodeError::MissingType)?;
        Ok(Self::from_value(&kind, raw))
    }

    fn from_value(kind: &str, raw: Value) -> Self {
        match kind {
            "session.created" => Self::SessionCreated {
                session: field(&raw, "session"),
            },
            "session.updated" => Self::SessionUpdated {
                session: field(&raw, "session"),
            },
            "input_audio_buffer.speech_started" => Self::InputAudioBufferSpeechStarted {
                item_id: field(&raw, "item_id"),
                audio_start_ms: field(&raw, "audio_start_ms"),
            },
            "input_audio_buffer.speech_stopped" => Self::InputAudioBufferSpeechStopped {
                item_id: field(&raw, "item_id"),
                audio_end_ms: field(&raw, "audio_end_ms"),
            },
            "conversation.item.input_audio_transcription.delta" => Self::InputTranscriptionDelta {
                item_id: field(&raw, "item_id"),
                delta: field(&raw, "delta"),
            },
            "conversation.item.input_audio_transcription.completed" => {
                Self::InputTranscriptionCompleted {
                    item_id: field(&raw, "item_id"),
                    transcript: field(&raw, "transcript"),
                }
            }
            "response.created" => Self::ResponseCreated {
                response_id: field::<ResponseInfo>(&raw, "response").id,
            },
            "response.done" => Self::ResponseDone {
                response: field(&raw, "response"),
            },
            "response.output_item.added" => Self::ResponseOutputItemAdded {
                response_id: field(&raw, "response_id"),
                output_index: field(&raw, "output_index"),
                item: field(&raw, "item"),
            },
            "response.output_item.done" => Self::ResponseOutputItemDone {
                response_id: field(&raw, "response_id"),
                output_index: field(&raw, "output_index"),
                item: field(&raw, "item"),
            },
            "response.output_text.delta" | "response.text.delta" => Self::OutputTextDelta {
                response_id: field(&raw, "response_id"),
                item_id: field(&raw, "item_id"),
                output_index: field(&raw, "output_index"),
                content_index: field(&raw, "content_index"),
                delta: field(&raw, "delta"),
            },
            "response.output_text.done" | "response.text.done" => Self::OutputTextDone {
                response_id: field(&raw, "response_id"),
                item_id: field(&raw, "item_id"),
                output_index: field(&raw, "output_index"),
                content_index: field(&raw, "content_index"),
                text: field(&raw, "text"),
            },
            "response.output_audio.delta" | "response.audio.delta" => Self::OutputAudioDelta {
                response_id: field(&raw, "response_id"),
                item_id: field(&raw, "item_id"),
                output_index: field(&raw, "output_index"),
                content_index: field(&raw, "content_index"),
                delta: field(&raw, "delta"),
            },
            "response.output_audio.done" | "response.audio.done" => Self::OutputAudioDone {
                response_id: field(&raw, "response_id"),
                item_id: field(&raw, "item_id"),
                output_index: field(&raw, "output_index"),
                content_index: field(&raw, "content_index"),
            },
            "response.output_audio_transcript.delta" | "response.audio_transcript.delta" => {
                Self::OutputAudioTranscriptDelta {
                    response_id: field(&raw, "response_id"),
                    item_id: field(&raw, "item_id"),
                    output_index: field(&raw, "output_index"),
                    content_index: field(&raw, "content_index"),
                    delta: field(&raw, "delta"),
                }
            }
            "response.output_audio_transcript.done" | "response.audio_transcript.done" => {
                Self::OutputAudioTranscriptDone {
                    response_id: field(&raw, "response_id"),
                    item_id: field(&raw, "item_id"),
                    output_index: field(&raw, "output_index"),
                    content_index: field(&raw, "content_index"),
                    transcript: field(&raw, "transcript"),
                }
            }
            "response.function_call_arguments.delta" => Self::FunctionCallArgumentsDelta {
                response_id: field(&raw, "response_id"),
                item_id: field(&raw, "item_id"),
                output_index: field(&raw, "output_index"),
                call_id: field(&raw, "call_id"),
                delta: field(&raw, "delta"),
            },
            "response.function_call_arguments.done" => Self::FunctionCallArgumentsDone {
                response_id: field(&raw, "response_id"),
                item_id: field(&raw, "item_id"),
                output_index: field(&raw, "output_index"),
                call_id: field(&raw, "call_id"),
                name: field(&raw, "name"),
                arguments: field(&raw, "arguments"),
            },
            "rate_limits.updated" => Self::RateLimitsUpdated {
                rate_limits: raw.get("rate_limits").cloned().unwrap_or(Value::Null),
            },
            "error" => Self::Error {
                error: field(&raw, "error"),
            },
            _ => Self::Unknown {
                kind: kind.to_owned(),
                raw,
            },
        }
    }
}

/// Decode failure for a server frame. The session logs and skips these instead
/// of terminating.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("server event is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server event has no string `type` field")]
    MissingType,
}

/// `session.created` / `session.updated` session object.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Response object from `response.done`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResponseInfo {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub usage: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Output item from `response.output_item.added` / `.done`. Kept as a struct so
/// item types this crate does not know still decode.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OutputItem {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl OutputItem {
    pub fn is_function_call(&self) -> bool {
        self.kind == "function_call"
    }
}

/// Error object carried by the `error` server event.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RealtimeError {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn field<T: DeserializeOwned + Default>(raw: &Value, key: &str) -> T {
    raw.get(key)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_item_serializes_as_input_text() {
        let event = ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: "user".to_owned(),
                content: vec![ContentPart::input_text("hello")],
            },
            previous_item_id: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "conversation.item.create");
        assert_eq!(json["item"]["type"], "message");
        assert_eq!(json["item"]["content"][0]["type"], "input_text");
        assert_eq!(json["item"]["content"][0]["text"], "hello");
        assert!(json.get("previous_item_id").is_none());
    }

    #[test]
    fn pcm16_audio_serializes_nested_audio_config() {
        let event = ClientEvent::SessionUpdate {
            session: SessionConfig::default()
                .pcm16_audio()
                .with_voice("marin")
                .with_turn_detection(serde_json::json!({
                    "type": "server_vad",
                    "threshold": 0.75,
                    "prefix_padding_ms": 300,
                    "silence_duration_ms": 700,
                    "create_response": true,
                    "interrupt_response": true,
                }))
                .with_noise_reduction(serde_json::json!({"type": "near_field"}))
                .with_parallel_tool_calls(false)
                .with_tools(vec![
                    serde_json::json!({"type": "function", "name": "ping"}),
                ]),
        };
        let json = serde_json::to_value(&event).unwrap();
        let session = &json["session"];

        assert_eq!(session["type"], "realtime");
        assert_eq!(session["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(session["audio"]["input"]["format"]["rate"], 24_000);
        let turn_detection = &session["audio"]["input"]["turn_detection"];
        assert_eq!(turn_detection["type"], "server_vad");
        assert_eq!(turn_detection["threshold"], 0.75);
        assert_eq!(turn_detection["prefix_padding_ms"], 300);
        assert_eq!(turn_detection["silence_duration_ms"], 700);
        assert_eq!(turn_detection["create_response"], true);
        assert_eq!(turn_detection["interrupt_response"], true);
        assert_eq!(
            session["audio"]["input"]["noise_reduction"]["type"],
            "near_field"
        );
        assert_eq!(session["audio"]["output"]["format"]["type"], "audio/pcm");
        assert_eq!(session["audio"]["output"]["format"]["rate"], 24_000);
        assert_eq!(session["audio"]["output"]["voice"], "marin");
        assert_eq!(session["parallel_tool_calls"], false);
        assert_eq!(session["tools"][0]["name"], "ping");

        for legacy in [
            "voice",
            "input_audio_format",
            "output_audio_format",
            "turn_detection",
        ] {
            assert!(session.get(legacy).is_none(), "legacy key `{legacy}`");
        }
    }

    #[test]
    fn session_extra_fields_round_trip() {
        let session: SessionConfig = serde_json::from_value(serde_json::json!({
            "type": "realtime",
            "audio": {"input": {"format": {"type": "audio/pcm", "rate": 24000}}},
            "future_knob": true,
        }))
        .unwrap();
        assert_eq!(session.extra["future_knob"], true);
        assert_eq!(
            session.audio.input.format.as_ref().unwrap().rate,
            Some(24_000)
        );

        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["future_knob"], true);
        assert_eq!(json["audio"]["input"]["format"]["rate"], 24_000);
    }

    #[test]
    fn response_created_reads_nested_response_id() {
        let event = ServerEvent::decode(
            r#"{"type":"response.created","response":{"id":"resp_1","object":"realtime.response","status":"in_progress"}}"#,
        )
        .unwrap();
        match event {
            ServerEvent::ResponseCreated { response_id } => {
                assert_eq!(response_id.as_deref(), Some("resp_1"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decodes_session_and_audio_deltas() {
        let event = ServerEvent::decode(
            r#"{"type":"session.created","session":{"id":"sess_1","model":"gpt-realtime","voice":"marin"}}"#,
        )
        .unwrap();
        match event {
            ServerEvent::SessionCreated { session } => {
                assert_eq!(session.id.as_deref(), Some("sess_1"));
                assert_eq!(session.model.as_deref(), Some("gpt-realtime"));
                assert_eq!(session.extra["voice"], "marin");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let event = ServerEvent::decode(
            r#"{"type":"response.output_audio.delta","response_id":"r1","item_id":"i1","output_index":0,"content_index":0,"delta":"AAA="}"#,
        )
        .unwrap();
        match event {
            ServerEvent::OutputAudioDelta { item_id, delta, .. } => {
                assert_eq!(item_id.as_deref(), Some("i1"));
                assert_eq!(delta, "AAA=");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decodes_legacy_audio_alias_to_same_variant() {
        let event =
            ServerEvent::decode(r#"{"type":"response.audio.delta","item_id":"i2","delta":"AQID"}"#)
                .unwrap();
        match event {
            ServerEvent::OutputAudioDelta { item_id, delta, .. } => {
                assert_eq!(item_id.as_deref(), Some("i2"));
                assert_eq!(delta, "AQID");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn function_call_done_keeps_call_id_and_arguments() {
        let event = ServerEvent::decode(
            r#"{"type":"response.function_call_arguments.done","response_id":"r2","item_id":"fc_1","output_index":0,"call_id":"call_9","arguments":"{\"chat_id\":\"c1\"}"}"#,
        )
        .unwrap();
        match event {
            ServerEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(call_id.as_deref(), Some("call_9"));
                assert_eq!(name, None);
                assert_eq!(arguments.as_deref(), Some(r#"{"chat_id":"c1"}"#));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn unknown_events_are_preserved_not_fatal() {
        let event =
            ServerEvent::decode(r#"{"type":"response.something.new","payload":42}"#).unwrap();
        match event {
            ServerEvent::Unknown { kind, raw } => {
                assert_eq!(kind, "response.something.new");
                assert_eq!(raw["payload"], 42);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn known_events_tolerate_missing_fields() {
        let event = ServerEvent::decode(r#"{"type":"response.output_text.delta"}"#).unwrap();
        assert!(matches!(
            event,
            ServerEvent::OutputTextDelta { ref delta, .. } if delta.is_empty()
        ));
    }

    #[test]
    fn malformed_frames_are_errors() {
        assert!(matches!(
            ServerEvent::decode(r#"{"event_id":"e"}"#),
            Err(DecodeError::MissingType)
        ));
        assert!(matches!(
            ServerEvent::decode("not json"),
            Err(DecodeError::Json(_))
        ));
    }
}

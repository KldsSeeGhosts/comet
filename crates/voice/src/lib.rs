//! Lean OpenAI Realtime core for Noches voice.
//!
//! Transport, typed event codec, text turns, the serialized function-call
//! loop, and the local default-device audio layer. No GPUI, no credential
//! storage, no transcript persistence: the UI layer owns those and implements
//! [`VoiceToolSink`] to route calls into the workspace control plane.
//!
//! [`AudioEngine`] captures the default microphone behind a push-to-talk gate
//! and plays decoded assistant audio through the default output device. The
//! capture side emits 24 kHz mono PCM16 chunks for [`VoiceSession::append_audio`];
//! the playback side accepts the same format from
//! [`ServerEvent::OutputAudioDelta`]. [`AudioEngine::begin_push_to_talk`] and
//! [`AudioEngine::end_push_to_talk`] document the half-duplex sequence.

mod audio;
mod client;
mod error;
mod events;
mod tools;

pub use audio::{
    AudioCapture, AudioEngine, AudioError, AudioPlayback, AudioStats, CaptureFrame, DeviceConfig,
    DeviceReport, VOICE_CHUNK_FRAMES, VOICE_SAMPLE_RATE, base64_to_pcm16, pcm16_to_base64,
};
pub use client::{VoiceConfig, VoiceSession};
pub use error::VoiceError;
pub use events::{
    AudioConfig, AudioFormat, AudioInput, AudioOutput, ClientEvent, ContentPart, ConversationItem,
    DecodeError, OutputItem, RealtimeError, ResponseConfig, ResponseInfo, ServerEvent,
    SessionConfig, SessionInfo,
};
pub use tools::{
    ToolCall, ToolOutput, VoiceToolSink, execute_tool_call, parse_arguments, tool_result_events,
};

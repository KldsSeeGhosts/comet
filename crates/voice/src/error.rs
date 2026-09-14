//! Crate error type. Variants never embed credentials or raw headers.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("websocket error: {0}")]
    WebSocket(#[source] Box<tokio_tungstenite::tungstenite::Error>),
    #[error("failed to encode a client event: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("API key cannot be used as an Authorization header value")]
    InvalidAuthorization,
    #[error("invalid base64 audio payload: {0}")]
    DecodeAudio(#[from] base64::DecodeError),
    #[error("PCM16 payload has an odd byte length")]
    OddPcm16,
    #[error("voice session is no longer running")]
    Disconnected,
    #[error("voice session task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl From<tokio_tungstenite::tungstenite::Error> for VoiceError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

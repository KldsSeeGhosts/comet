//! Device and worker failures from the local audio layer.

use thiserror::Error;

/// Failure to open or run the default audio devices.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no default audio input device")]
    NoDefaultInput,
    #[error("no default audio output device")]
    NoDefaultOutput,
    #[error("audio device error: {0}")]
    Device(#[from] cpal::Error),
    #[error("unsupported device sample format `{0}`")]
    UnsupportedFormat(cpal::SampleFormat),
    #[error("resampler failed: {0}")]
    Resampler(String),
    #[error("failed to start the {name} audio worker: {error}")]
    Worker { name: &'static str, error: String },
    #[error("audio playback is not running")]
    PlaybackStopped,
}

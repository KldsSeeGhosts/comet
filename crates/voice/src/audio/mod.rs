//! Local audio I/O: default-device capture and playback for the Realtime session.
//!
//! Two independent halves, each built on CPAL's native default host (ALSA on
//! Linux, CoreAudio on macOS) and a small worker thread:
//!
//! - [`AudioCapture`] gates the microphone behind a push-to-talk flag and
//!   emits bounded 20 ms chunks of 24 kHz mono PCM16 through a bounded channel.
//!   While the gate is closed no sample leaves the callback.
//! - [`AudioPlayback`] accepts 24 kHz mono PCM16 chunks - the decoded
//!   `response.output_audio.delta` payloads - resamples them to the default
//!   output device, and lets the caller drop everything queued with
//!   [`AudioPlayback::clear`].
//!
//! There is no echo cancellation, gain control, or device picker: this is a
//! headphones-first, push-to-talk half-duplex layer. Both callbacks are
//! nonblocking and allocation-free on the sample path; samples cross the
//! callback/worker boundary through bounded SPSC rings and are dropped, with a
//! counter, when the pipeline falls behind. Resampling runs on the worker
//! threads, never in a callback. The device's native `f32`, `i32`, `i16`, or
//! `u16` format is converted; any other format fails with
//! [`AudioError::UnsupportedFormat`].
//!
//! # Push-to-talk sequence
//!
//! ```no_run
//! use zeron_voice::{AudioEngine, CaptureFrame, VoiceSession};
//!
//! # async fn run(
//! #     engine: &AudioEngine,
//! #     frames: &mut tokio::sync::mpsc::Receiver<CaptureFrame>,
//! #     session: &VoiceSession,
//! # ) -> anyhow::Result<()> {
//! // On key press: silence queued assistant audio, then open the mic.
//! engine.begin_push_to_talk();
//!
//! // On key release: close the mic. Nothing captured after this point is
//! // emitted, and any chunks from before it are still in flight.
//! engine.end_push_to_talk();
//!
//! // The task that owns the capture receiver appends every chunk, then acts
//! // on the boundary the pipeline emits once the released audio has drained.
//! while let Some(frame) = frames.recv().await {
//!     match frame {
//!         CaptureFrame::Audio(pcm) => session.append_audio(&pcm)?,
//!         CaptureFrame::Released => {
//!             session.commit_audio()?;
//!             session.request_response()?;
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod capture;
mod codec;
mod convert;
mod engine;
mod error;
mod playback;
mod resample;

pub use capture::{AudioCapture, CaptureFrame};
pub use codec::{base64_to_pcm16, pcm16_to_base64};
pub use engine::{AudioEngine, AudioStats, DeviceConfig, DeviceReport};
pub use error::AudioError;
pub use playback::AudioPlayback;

/// Sample rate of the Realtime PCM16 protocol, in Hz.
pub const VOICE_SAMPLE_RATE: u32 = 24_000;

/// Frames per emitted capture chunk and per queued playback chunk: 20 ms at
/// [`VOICE_SAMPLE_RATE`].
pub const VOICE_CHUNK_FRAMES: usize = 480;

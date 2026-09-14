//! Device discovery, engine configuration, shared counters, and the
//! half-duplex engine facade.

use crate::audio::{
    capture::{AudioCapture, CaptureFrame, CaptureParts},
    devices::{self, AudioDevicePreferences, DeviceMatch, InputDeviceInfo},
    error::AudioError,
    playback::{AudioPlayback, PlaybackParts, PlaybackTuning},
};
use cpal::traits::{HostTrait, StreamTrait};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
};
use tokio::sync::mpsc;

/// Tunables for [`AudioEngine::start_with`]: the input device preference plus
/// the playback pipeline's buffering and output level.
///
/// ```
/// # use zeron_voice::AudioEngineConfig;
/// let config = AudioEngineConfig::default();
/// assert_eq!(config.playback.ring.as_millis(), 200);
/// assert_eq!(config.playback.start_buffer.as_millis(), 120);
/// ```
#[derive(Debug, Clone, Default)]
pub struct AudioEngineConfig {
    /// Saved microphone choice; empty means the system default.
    pub input: AudioDevicePreferences,
    /// Playback buffering, gain, and limiter settings.
    pub playback: PlaybackTuning,
}

impl AudioEngineConfig {
    /// Rejects combinations the pipeline cannot honor. Checked by
    /// [`AudioEngine::start_with`] before any device is touched.
    pub fn validate(&self) -> Result<(), AudioError> {
        let playback = &self.playback;
        if playback.ring < PlaybackTuning::MIN_RING || playback.ring > PlaybackTuning::MAX_RING {
            return Err(AudioError::InvalidConfig(
                "playback ring must be between 20ms and 5s",
            ));
        }
        if playback.start_buffer > playback.ring {
            return Err(AudioError::InvalidConfig(
                "playback start buffer must not exceed the ring",
            ));
        }
        if !playback.gain.is_finite() || !(0.0..=8.0).contains(&playback.gain) {
            return Err(AudioError::InvalidConfig(
                "output gain must be finite and within 0.0..=8.0",
            ));
        }
        if !playback.limiter_ceiling.is_finite()
            || !(0.0..=1.0).contains(&playback.limiter_ceiling)
            || playback.limiter_ceiling == 0.0
        {
            return Err(AudioError::InvalidConfig(
                "limiter ceiling must be within (0.0, 1.0]",
            ));
        }
        Ok(())
    }
}

/// The devices in use and the formats the audio layer negotiated with them.
#[derive(Debug, Clone)]
pub struct DeviceReport {
    /// CPAL host in use: `ALSA`, `CoreAudio`, and so on.
    pub host: String,
    pub input: DeviceConfig,
    pub output: DeviceConfig,
}

/// One device's name and negotiated stream format.
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    /// The device's native sample format. The audio layer converts between it
    /// and the 24 kHz mono PCM16 protocol format.
    pub sample_format: cpal::SampleFormat,
}

/// Drop and error counters, for a diagnostics line or a future UI badge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioStats {
    /// 20 ms capture chunks delivered to the receiver.
    pub input_chunks: u64,
    /// Samples dropped when the capture ring overflowed.
    pub input_dropped_samples: u64,
    /// Chunks dropped when the capture receiver was full.
    pub input_dropped_chunks: u64,
    /// Chunks dropped when the playback queue was full, including flush tails
    /// and end-of-audio marks that lost their reserved slots.
    pub output_dropped_chunks: u64,
    /// Resampled samples dropped when the output ring overflowed.
    pub output_dropped_samples: u64,
    /// Times the output ran dry mid-stream and re-entered the startup buffer.
    pub output_underruns: u64,
    /// Times playback recovered from an underrun: a starved buffer filled back
    /// to the startup threshold and unmuted again.
    pub output_rebuffers: u64,
    /// Times an end-of-audio mark reached the callback: responses that played
    /// out fully instead of starving or being cleared.
    pub output_ended: u64,
    /// Samples the output limiter had to fold back under its ceiling.
    pub output_limited_samples: u64,
    /// Stream error callbacks seen on either device.
    pub stream_errors: u64,
    /// Most recent stream error message, if any.
    pub last_error: Option<String>,
}

/// The input-only slice of [`AudioStats`], for surfaces that run capture
/// without a session - a settings microphone test, a picker level meter.
/// Snapshotted from the same counters; holds no device label or id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputDiagnostics {
    /// 20 ms capture chunks delivered to the receiver.
    pub chunks: u64,
    /// Samples dropped when the capture ring overflowed.
    pub dropped_samples: u64,
    /// Chunks dropped when the capture receiver was full.
    pub dropped_chunks: u64,
    /// Stream error callbacks seen on the input device.
    pub stream_errors: u64,
    /// Most recent stream error message, if any.
    pub last_error: Option<String>,
}

impl Counters {
    /// The input-only counters as a diagnostics snapshot.
    pub(crate) fn input_snapshot(&self) -> InputDiagnostics {
        InputDiagnostics {
            chunks: self.input_chunks.load(Ordering::Relaxed),
            dropped_samples: self.input_dropped_samples.load(Ordering::Relaxed),
            dropped_chunks: self.input_dropped_chunks.load(Ordering::Relaxed),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|slot| (*slot).clone()),
        }
    }
}

/// Default-device capture and playback for one voice session.
///
/// `start` opens the system default output and the configured (or default)
/// input and starts streaming; the microphone gate is closed until
/// [`AudioEngine::begin_push_to_talk`]. Dropping the engine (or calling
/// [`AudioEngine::stop`]) stops the streams and joins the resample workers.
pub struct AudioEngine {
    capture: AudioCapture,
    playback: AudioPlayback,
    devices: DeviceReport,
    /// How the configured input preference resolved against present devices.
    input_match: DeviceMatch,
    capture_frames: Option<mpsc::Receiver<CaptureFrame>>,
    counters: Arc<Counters>,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    input_worker: Option<JoinHandle<()>>,
    output_worker: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Opens the configured input and the default output device and starts
    /// streaming.
    pub fn start_with(config: AudioEngineConfig) -> Result<Self, AudioError> {
        config.validate()?;
        let host = cpal::default_host();
        let (input_device, input_match) = devices::resolve_input_device(&host, &config.input)?;
        let output_device = host
            .default_output_device()
            .ok_or(AudioError::NoDefaultOutput)?;

        let counters = Arc::new(Counters::default());
        let CaptureParts {
            handle: capture,
            frames: capture_frames,
            stream: input_stream,
            worker: input_worker,
        } = AudioCapture::open(&input_device, Arc::clone(&counters))?;
        let PlaybackParts {
            handle: playback,
            stream: output_stream,
            worker: output_worker,
        } = match AudioPlayback::open(&output_device, Arc::clone(&counters), config.playback) {
            Ok(parts) => parts,
            Err(error) => {
                capture.stop();
                drop(input_stream);
                let _ = input_worker.join();
                return Err(error);
            }
        };

        let devices = DeviceReport {
            host: host.id().name().to_owned(),
            input: capture.config().clone(),
            output: playback.config().clone(),
        };
        Ok(Self {
            capture,
            playback,
            devices,
            input_match,
            capture_frames: Some(capture_frames),
            counters,
            input_stream: Some(input_stream),
            output_stream: Some(output_stream),
            input_worker: Some(input_worker),
            output_worker: Some(output_worker),
        })
    }

    /// Opens the default input and output devices and starts streaming.
    pub fn start() -> Result<Self, AudioError> {
        Self::start_with(AudioEngineConfig::default())
    }

    /// Lists the host's input devices for a picker. No stream is opened and no
    /// audio flows: the enumeration only queries ids and names, so it is safe
    /// to call before the engine exists and on a locked-down host.
    pub fn input_devices() -> Result<Vec<InputDeviceInfo>, AudioError> {
        devices::enumerate_input_devices(&cpal::default_host())
    }

    /// The devices in use and the negotiated stream formats.
    pub fn devices(&self) -> &DeviceReport {
        &self.devices
    }

    /// How the configured input preference resolved against the devices
    /// present at start: exact id, remembered label, or the system default.
    pub fn input_match(&self) -> DeviceMatch {
        self.input_match
    }

    /// The push-to-talk gate, meter level, and input device config.
    pub fn capture(&self) -> &AudioCapture {
        &self.capture
    }

    /// The assistant playback queue and output device config.
    pub fn playback(&self) -> &AudioPlayback {
        &self.playback
    }

    /// Takes the capture frame receiver; available once. Hand it to the task
    /// that appends capture chunks to the [`VoiceSession`](crate::VoiceSession).
    ///
    /// The channel is bounded, so without a receiver the capture worker drops
    /// chunks (counted in [`AudioStats`]) instead of buffering without limit.
    pub fn take_capture_frames(&mut self) -> Option<mpsc::Receiver<CaptureFrame>> {
        self.capture_frames.take()
    }

    /// Press: drop queued assistant playback, then open the microphone.
    ///
    /// Clearing first keeps the half-duplex promise: with the speaker silent,
    /// the microphone cannot pick up the model's own voice. Every chunk that
    /// arrives while the gate is open should be appended to the session.
    pub fn begin_push_to_talk(&self) {
        self.playback.clear();
        self.capture.set_capturing(true);
    }

    /// Release: close the microphone gate.
    ///
    /// No sample captured after this point is emitted, and audio captured
    /// before it may still arrive. The capture pipeline ends the release with
    /// [`CaptureFrame::Released`]; commit the input buffer and request the
    /// response when it arrives.
    pub fn end_push_to_talk(&self) {
        self.capture.set_capturing(false);
    }

    /// A snapshot of drop and error counters.
    pub fn stats(&self) -> AudioStats {
        self.counters.snapshot()
    }

    /// Records a session-level failure in the diagnostics counters so it
    /// surfaces in [`AudioEngine::stats`] alongside device stream errors.
    /// The caller composes the message; secrets and endpoint URLs must
    /// never reach it.
    pub fn note_error(&self, message: String) {
        self.counters.note_error(message);
    }

    /// Stops both streams and joins the resample workers.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.capture.stop();
        self.playback.stop();
        if let Some(stream) = self.input_stream.take() {
            let _ = stream.pause();
        }
        if let Some(stream) = self.output_stream.take() {
            let _ = stream.pause();
        }
        if let Some(worker) = self.input_worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.output_worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Atomics shared by the device callbacks, the resample workers, and the
/// engine. Every write is a relaxed counter or a message slot; ordering for the
/// gates and clear generations lives in the owning modules.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) input_chunks: AtomicU64,
    pub(crate) input_dropped_samples: AtomicU64,
    pub(crate) input_dropped_chunks: AtomicU64,
    pub(crate) output_dropped_chunks: AtomicU64,
    pub(crate) output_dropped_samples: AtomicU64,
    pub(crate) output_underruns: AtomicU64,
    pub(crate) output_rebuffers: AtomicU64,
    pub(crate) output_ended: AtomicU64,
    pub(crate) output_limited_samples: AtomicU64,
    /// Playback queue slots in flight: producers cap themselves below the
    /// channel's hard bound so a flush tail and its end mark always fit.
    pub(crate) playback_queued: AtomicU64,
    pub(crate) stream_errors: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Counters {
    /// Records a stream error. Runs on a callback thread, so it only formats
    /// the message and fills the slot when it is uncontended: no logging, no
    /// blocking wait.
    pub(crate) fn note_error(&self, message: String) {
        self.stream_errors.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_error.try_lock() {
            *slot = Some(message);
        }
    }

    pub(crate) fn snapshot(&self) -> AudioStats {
        AudioStats {
            input_chunks: self.input_chunks.load(Ordering::Relaxed),
            input_dropped_samples: self.input_dropped_samples.load(Ordering::Relaxed),
            input_dropped_chunks: self.input_dropped_chunks.load(Ordering::Relaxed),
            output_dropped_chunks: self.output_dropped_chunks.load(Ordering::Relaxed),
            output_dropped_samples: self.output_dropped_samples.load(Ordering::Relaxed),
            output_underruns: self.output_underruns.load(Ordering::Relaxed),
            output_rebuffers: self.output_rebuffers.load(Ordering::Relaxed),
            output_ended: self.output_ended.load(Ordering::Relaxed),
            output_limited_samples: self.output_limited_samples.load(Ordering::Relaxed),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|slot| (*slot).clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tuning() -> PlaybackTuning {
        PlaybackTuning::default()
    }

    #[test]
    fn the_default_config_validates() {
        AudioEngineConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_tunings_are_named_specifically() {
        let cases: [PlaybackTuning; 5] = [
            PlaybackTuning {
                ring: Duration::from_millis(5),
                ..tuning()
            },
            PlaybackTuning {
                ring: Duration::from_secs(30),
                ..tuning()
            },
            PlaybackTuning {
                start_buffer: Duration::from_millis(500),
                ring: Duration::from_millis(200),
                ..tuning()
            },
            PlaybackTuning {
                gain: f32::NAN,
                ..tuning()
            },
            PlaybackTuning {
                limiter_ceiling: 0.0,
                ..tuning()
            },
        ];
        for playback in cases {
            let config = AudioEngineConfig {
                playback,
                ..AudioEngineConfig::default()
            };
            assert!(
                matches!(config.validate(), Err(AudioError::InvalidConfig(_))),
                "{playback:?} must be rejected"
            );
        }

        // Gain of 0 (muted output) and a hard-clamp ceiling of 1.0 are legal.
        AudioEngineConfig {
            playback: PlaybackTuning {
                gain: 0.0,
                limiter_ceiling: 1.0,
                ..tuning()
            },
            ..AudioEngineConfig::default()
        }
        .validate()
        .unwrap();
    }
}

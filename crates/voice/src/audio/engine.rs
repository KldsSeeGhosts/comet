//! Device discovery, shared counters, and the half-duplex engine facade.

use crate::audio::{
    capture::{AudioCapture, CaptureFrame, CaptureParts},
    error::AudioError,
    playback::{AudioPlayback, PlaybackParts},
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

/// The default devices and the formats the audio layer negotiated with them.
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
    /// Chunks dropped when the playback queue was full.
    pub output_dropped_chunks: u64,
    /// Resampled samples dropped when the output ring overflowed.
    pub output_dropped_samples: u64,
    /// Times the output device ran dry between played samples.
    pub output_underruns: u64,
    /// Stream error callbacks seen on either device.
    pub stream_errors: u64,
    /// Most recent stream error message, if any.
    pub last_error: Option<String>,
}

/// Default-device capture and playback for one voice session.
///
/// `start` opens both default devices and starts streaming; the microphone
/// gate is closed until [`AudioEngine::begin_push_to_talk`]. Dropping the
/// engine (or calling [`AudioEngine::stop`]) stops the streams and joins the
/// resample workers.
pub struct AudioEngine {
    capture: AudioCapture,
    playback: AudioPlayback,
    devices: DeviceReport,
    capture_frames: Option<mpsc::Receiver<CaptureFrame>>,
    counters: Arc<Counters>,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    input_worker: Option<JoinHandle<()>>,
    output_worker: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Opens the default input and output devices and starts streaming.
    pub fn start() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .ok_or(AudioError::NoDefaultInput)?;
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
        } = match AudioPlayback::open(&output_device, Arc::clone(&counters)) {
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
            capture_frames: Some(capture_frames),
            counters,
            input_stream: Some(input_stream),
            output_stream: Some(output_stream),
            input_worker: Some(input_worker),
            output_worker: Some(output_worker),
        })
    }

    /// The default devices and the negotiated stream formats.
    pub fn devices(&self) -> &DeviceReport {
        &self.devices
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
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|slot| (*slot).clone()),
        }
    }
}

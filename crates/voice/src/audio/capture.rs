//! Default-device capture: input callback -> SPSC ring -> resample worker -> channel.
//!
//! The callback only mixes the device frame to mono, honours the push-to-talk
//! gate, updates the level, and pushes into a bounded ring. Everything else -
//! format conversion to PCM16, resampling to 24 kHz, channel sends - happens on
//! the worker thread, so a slow consumer can only cause counted drops.

use crate::audio::{
    VOICE_CHUNK_FRAMES, convert,
    engine::{Counters, DeviceConfig},
    error::AudioError,
    resample,
};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, StreamTrait},
};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use rubato::{Fft, Resampler};
use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::mpsc;

/// Capture ring capacity in milliseconds of device frames. Absorbs worker
/// scheduling jitter; anything beyond that is dropped, never queued.
const RING_MS: usize = 80;

/// Capture channel capacity in 20 ms chunks (160 ms of audio).
const CHANNEL_CHUNKS: usize = 8;

/// Worker sleep while the ring holds fewer frames than the next chunk needs.
const POLL: Duration = Duration::from_millis(2);

/// Polls the capture ring must stay empty after the gate closes before the
/// release boundary is emitted. Covers a callback that had already sampled the
/// open gate (~30 ms at [`POLL`]).
const RELEASE_QUIET_CHECKS: usize = 15;

/// Bounded wait for the backend to start the input stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// The capture side of [`AudioEngine`](crate::audio::AudioEngine).
///
/// Cheap to clone; the device stream itself stays with the engine. The handle
/// exposes the push-to-talk gate and the meter level, but not the samples:
/// those arrive on the receiver from
/// [`AudioEngine::take_capture_frames`](crate::audio::AudioEngine::take_capture_frames).
#[derive(Clone)]
pub struct AudioCapture {
    shared: Arc<CaptureShared>,
}

/// One message from the capture pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureFrame {
    /// 20 ms of 24 kHz mono PCM16, ready for
    /// [`VoiceSession::append_audio`](crate::VoiceSession::append_audio).
    Audio(Box<[i16]>),
    /// Every sample captured before the gate closed has been emitted. This is
    /// the point to commit the input buffer and request a response.
    Released,
}

pub(crate) struct CaptureParts {
    pub(crate) handle: AudioCapture,
    pub(crate) frames: mpsc::Receiver<CaptureFrame>,
    pub(crate) stream: cpal::Stream,
    pub(crate) worker: JoinHandle<()>,
}

impl AudioCapture {
    /// Opens the default input configuration of `device` and starts streaming.
    pub(crate) fn open(
        device: &cpal::Device,
        counters: Arc<Counters>,
    ) -> Result<CaptureParts, AudioError> {
        let supported = device.default_input_config()?;
        let config = DeviceConfig {
            name: device.to_string(),
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            sample_format: supported.sample_format(),
        };

        let shared = Arc::new(CaptureShared {
            capturing: AtomicBool::new(false),
            level: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            counters,
            config: config.clone(),
        });

        let (producer, consumer) = HeapRb::<f32>::new(ring_frames(config.sample_rate)).split();
        let (sender, frames) = mpsc::channel(CHANNEL_CHUNKS);

        // Build and start the stream before spawning the worker: a failed build
        // then leaves nothing behind that needs a stop signal.
        let stream = build_stream(
            device,
            supported.config(),
            config.sample_format,
            config.channels as usize,
            Arc::clone(&shared),
            producer,
        )?;
        stream.play()?;
        let worker = spawn_worker(consumer, Arc::clone(&shared), sender, config.sample_rate)?;

        Ok(CaptureParts {
            handle: Self { shared },
            frames,
            stream,
            worker,
        })
    }

    /// Opens or closes the push-to-talk gate. While closed, the callback drops
    /// captured frames without touching the ring.
    pub fn set_capturing(&self, capturing: bool) {
        self.shared.capturing.store(capturing, Ordering::Release);
    }

    pub fn is_capturing(&self) -> bool {
        self.shared.capturing.load(Ordering::Acquire)
    }

    /// Decaying peak of the most recent callback, on the `[0.0, 1.0]` float
    /// scale. Zero while the gate is closed.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// Name and negotiated format of the default input device.
    pub fn config(&self) -> &DeviceConfig {
        &self.shared.config
    }

    /// Signals the worker to exit at its next poll.
    pub(crate) fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

/// State shared with the input callback and the resample worker.
struct CaptureShared {
    config: DeviceConfig,
    capturing: AtomicBool,
    /// Last held peak as `f32` bits, read by [`AudioCapture::level`].
    level: AtomicU32,
    stop: AtomicBool,
    counters: Arc<Counters>,
}

/// Ring capacity in device frames for `rate`.
fn ring_frames(rate: u32) -> usize {
    (rate as usize * RING_MS / 1000).max(VOICE_CHUNK_FRAMES)
}

/// The input callback's world: a mono mixer and a bounded ring producer.
struct InputCallback<T> {
    channels: usize,
    producer: HeapProd<f32>,
    shared: Arc<CaptureShared>,
    level: f32,
    marker: PhantomData<T>,
}

impl<T> InputCallback<T>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    /// Mixes one device buffer to mono and pushes it into the ring. The gate is
    /// sampled once per callback, so a buffer captured while pressed is emitted
    /// even if the key is released mid-buffer.
    fn ingest(&mut self, data: &[T]) {
        if !self.shared.capturing.load(Ordering::Acquire) {
            self.level = 0.0;
            self.shared.level.store(0f32.to_bits(), Ordering::Relaxed);
            return;
        }

        let channels = self.channels.max(1);
        let mut peak = 0.0f32;
        let mut dropped = 0usize;
        for frame in data.chunks_exact(channels) {
            let mut mono = 0.0f32;
            for &sample in frame {
                mono += f32::from_sample(sample);
            }
            mono /= channels as f32;
            peak = peak.max(mono.abs());
            if self.producer.try_push(mono).is_err() {
                dropped += 1;
            }
        }

        self.level = convert::decayed_level(self.level, peak);
        self.shared
            .level
            .store(self.level.to_bits(), Ordering::Relaxed);
        if dropped > 0 {
            self.shared
                .counters
                .input_dropped_samples
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }
}

/// Dispatches the device's native sample format to the typed callback.
fn build_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: SampleFormat,
    channels: usize,
    shared: Arc<CaptureShared>,
    producer: HeapProd<f32>,
) -> Result<cpal::Stream, AudioError> {
    macro_rules! typed {
        ($sample:ty) => {
            build_typed_stream::<$sample>(device, config, channels, shared, producer)
        };
    }
    // The formats a default input device actually negotiates. Anything else is
    // reported rather than half-supported.
    match format {
        SampleFormat::F32 => typed!(f32),
        SampleFormat::I32 => typed!(i32),
        SampleFormat::I16 => typed!(i16),
        SampleFormat::U16 => typed!(u16),
        other => Err(AudioError::UnsupportedFormat(other)),
    }
}

fn build_typed_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: Arc<CaptureShared>,
    producer: HeapProd<f32>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + Copy + Send + 'static,
    f32: FromSample<T>,
{
    let mut callback = InputCallback::<T> {
        channels,
        producer,
        shared: Arc::clone(&shared),
        level: 0.0,
        marker: PhantomData,
    };
    let errors = Arc::clone(&shared);
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _info| callback.ingest(data),
            move |error| errors.counters.note_error(format!("input stream: {error}")),
            Some(STREAM_TIMEOUT),
        )
        .map_err(AudioError::from)
}

fn spawn_worker(
    consumer: HeapCons<f32>,
    shared: Arc<CaptureShared>,
    frames: mpsc::Sender<CaptureFrame>,
    device_rate: u32,
) -> Result<JoinHandle<()>, AudioError> {
    let resampler = resample::to_voice(device_rate)?;
    thread::Builder::new()
        .name("zeron-voice-capture".to_owned())
        .spawn(move || {
            CaptureWorker {
                consumer,
                shared,
                frames,
                resampler,
                staging: Vec::new(),
                chunk: vec![0.0; VOICE_CHUNK_FRAMES],
                filled: 0,
                capturing: false,
                release_pending: false,
                release_quiet: 0,
            }
            .run();
        })
        .map_err(|error| AudioError::Worker {
            name: "capture",
            error: error.to_string(),
        })
}

/// Resamples device frames to 24 kHz and forwards full chunks. Never blocks the
/// callback; a full channel drops the chunk and counts it.
struct CaptureWorker {
    consumer: HeapCons<f32>,
    shared: Arc<CaptureShared>,
    frames: mpsc::Sender<CaptureFrame>,
    resampler: Fft<f32>,
    /// Device frames waiting for the next resampler input, at most one round of
    /// [`Fft::input_frames_next`].
    staging: Vec<f32>,
    /// One 20 ms chunk of resampled 24 kHz samples.
    chunk: Vec<f32>,
    filled: usize,
    capturing: bool,
    /// A `Released` boundary is owed to the receiver.
    release_pending: bool,
    /// Polls the ring has stayed empty since the gate closed.
    release_quiet: usize,
}

impl CaptureWorker {
    fn run(&mut self) {
        while !self.shared.stop.load(Ordering::Relaxed) {
            self.track_gate();

            let wanted = self.resampler.input_frames_next();
            if self.filled > wanted {
                // The resampler's next input shrank below the staged window.
                // Drop the partial window instead of feeding a malformed chunk.
                self.resampler.reset();
                self.filled = 0;
            }
            if self.staging.len() < wanted {
                self.staging.resize(wanted, 0.0);
            }
            self.filled += self
                .consumer
                .pop_slice(&mut self.staging[self.filled..wanted]);

            if self.filled == wanted {
                self.resample_and_send(wanted);
                self.filled = 0;
            }

            self.settle_release();

            if self.filled < self.resampler.input_frames_next() {
                thread::park_timeout(POLL);
            }
        }
    }

    /// Resets pipeline state at a gate transition so a turn never inherits
    /// samples or resampler history from the previous one. A close keeps the
    /// staged partial so [`CaptureWorker::settle_release`] can flush it.
    fn track_gate(&mut self) {
        let capturing = self.shared.capturing.load(Ordering::Acquire);
        if capturing == self.capturing {
            return;
        }
        self.capturing = capturing;
        if capturing {
            if self.release_pending {
                // The next press arrived before the previous release settled.
                // Emit the turn's tail and its boundary rather than signing
                // the turn-1 audio over to turn 2. If the boundary does not
                // fit the channel it stays pending: `settle_release` retries
                // while the ring is empty, so the new turn's chunks queue
                // behind it.
                self.flush_partial();
                if self.frames.try_send(CaptureFrame::Released).is_ok() {
                    self.release_pending = false;
                }
            }
            self.resampler.reset();
            self.filled = 0;
        } else {
            self.release_pending = true;
        }
        self.release_quiet = 0;
    }

    fn resample_and_send(&mut self, wanted: usize) {
        match resample::resample_chunk(
            &mut self.resampler,
            &self.staging[..wanted],
            &mut self.chunk,
        ) {
            Ok(written) => {
                let pcm: Vec<i16> = self.chunk[..written]
                    .iter()
                    .map(|&sample| convert::f32_to_i16(sample))
                    .collect();
                match self
                    .frames
                    .try_send(CaptureFrame::Audio(pcm.into_boxed_slice()))
                {
                    Ok(()) => {
                        self.shared
                            .counters
                            .input_chunks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.shared
                            .counters
                            .input_dropped_chunks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "voice capture resampling failed");
                self.shared.counters.note_error(error.to_string());
                self.resampler.reset();
            }
        }
    }

    /// Tells the receiver that everything captured before the gate closed has
    /// been sent. Waits out a quiet window first so a callback that had already
    /// sampled the open gate can still deliver its buffer, then drains the last
    /// partial chunk with silence. A pending boundary left over from a rapid
    /// re-press also arrives through here once the ring is empty: the fresh
    /// turn's chunks queue in the bounded channel behind it, so the boundary
    /// still lands before the new turn's audio.
    fn settle_release(&mut self) {
        if !self.release_pending {
            return;
        }
        if self.consumer.occupied_len() != 0 {
            self.release_quiet = 0;
            return;
        }
        self.release_quiet += 1;
        if self.release_quiet < RELEASE_QUIET_CHECKS {
            return;
        }
        self.flush_partial();
        if self.frames.try_send(CaptureFrame::Released).is_ok() {
            self.release_pending = false;
        }
    }

    /// Resamples the tail that did not reach a full input chunk, padded with
    /// silence. The resampler is reset on the next press, so the silence cannot
    /// bleed into the following turn.
    fn flush_partial(&mut self) {
        if self.filled == 0 {
            return;
        }
        let wanted = self.resampler.input_frames_next();
        if self.staging.len() < wanted {
            self.staging.resize(wanted, 0.0);
        }
        self.staging[self.filled..wanted].fill(0.0);
        self.resample_and_send(wanted);
        self.filled = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::engine::Counters;

    fn shared() -> Arc<CaptureShared> {
        Arc::new(CaptureShared {
            config: DeviceConfig {
                name: "test".to_owned(),
                channels: 2,
                sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
            capturing: AtomicBool::new(false),
            level: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            counters: Arc::new(Counters::default()),
        })
    }

    fn callback(shared: &Arc<CaptureShared>, producer: HeapProd<f32>) -> InputCallback<f32> {
        InputCallback {
            channels: shared.config.channels as usize,
            producer,
            shared: Arc::clone(shared),
            level: 0.0,
            marker: PhantomData,
        }
    }

    #[test]
    fn closed_gate_emits_nothing_and_clears_the_level() {
        let (producer, consumer) = HeapRb::<f32>::new(64).split();
        let shared = shared();
        let mut callback = callback(&shared, producer);

        // Pretend a previous turn left a live meter, then ingest while closed.
        callback.level = 0.9;
        shared.level.store(0.9f32.to_bits(), Ordering::Relaxed);
        callback.ingest(&[0.5, 0.5, 0.25, 0.25]);

        assert!(consumer.is_empty());
        assert_eq!(shared.level.load(Ordering::Relaxed), 0f32.to_bits());
    }

    #[test]
    fn open_gate_mixes_to_mono_and_drops_on_overflow() {
        let (producer, mut consumer) = HeapRb::<f32>::new(2).split();
        let shared = shared();
        shared.capturing.store(true, Ordering::Release);
        let mut callback = callback(&shared, producer);

        // Stereo frames: 0.5/0.5 -> 0.5, 0.25/-0.25 -> 0.0.
        callback.ingest(&[0.5, 0.5, 0.25, -0.25]);
        let mut mono = [0.0f32; 4];
        let available = consumer.pop_slice(&mut mono);
        assert_eq!(&mono[..available], &[0.5, 0.0][..available]);
        assert!(f32::from_bits(shared.level.load(Ordering::Relaxed)) > 0.0);

        // A buffer larger than the ring drops the excess and counts it.
        callback.ingest(&[1.0; 8]);
        let dropped = shared
            .counters
            .input_dropped_samples
            .load(Ordering::Relaxed);
        assert_eq!(consumer.occupied_len() as u64 + dropped, 4);
        assert!(dropped > 0);
    }

    fn worker(
        shared: &Arc<CaptureShared>,
        consumer: HeapCons<f32>,
        frames: mpsc::Sender<CaptureFrame>,
    ) -> CaptureWorker {
        CaptureWorker {
            consumer,
            shared: Arc::clone(shared),
            frames,
            resampler: resample::to_voice(shared.config.sample_rate).unwrap(),
            staging: Vec::new(),
            chunk: vec![0.0; VOICE_CHUNK_FRAMES],
            filled: 0,
            capturing: false,
            release_pending: false,
            release_quiet: 0,
        }
    }

    #[test]
    fn a_repress_with_a_full_channel_still_emits_the_boundary_first() {
        let (_producer, consumer) = HeapRb::<f32>::new(64).split();
        let shared = shared();
        // Fill the frame channel so the first boundary attempt cannot land.
        let (sender, mut frames) = mpsc::channel(1);
        sender
            .try_send(CaptureFrame::Audio(
                vec![0; VOICE_CHUNK_FRAMES].into_boxed_slice(),
            ))
            .unwrap();
        let mut worker = worker(&shared, consumer, sender);

        // Close and re-press the gate with a boundary still owed.
        shared.capturing.store(true, Ordering::Release);
        worker.track_gate();
        shared.capturing.store(false, Ordering::Release);
        worker.track_gate();
        shared.capturing.store(true, Ordering::Release);
        worker.track_gate();
        assert!(worker.release_pending);

        // Make room, then let the boundary settle against an empty ring.
        frames.try_recv().unwrap();
        for _ in 0..RELEASE_QUIET_CHECKS {
            worker.settle_release();
        }
        assert!(!worker.release_pending);

        // The boundary is in the channel before any audio of the new turn.
        assert_eq!(frames.try_recv().unwrap(), CaptureFrame::Released);
    }
}

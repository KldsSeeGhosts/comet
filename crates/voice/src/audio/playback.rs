//! Default-device playback: bounded queue -> resample worker -> SPSC ring -> output callback.
//!
//! [`AudioPlayback::play`] takes decoded 24 kHz mono PCM16 chunks (the
//! `response.output_audio.delta` payloads), buffers them into 20 ms pieces -
//! deltas end at arbitrary boundaries, so partial frames carry over between
//! calls - and hands them to a worker that resamples to the device rate. The output
//! callback only pops from a bounded ring and converts to the device format,
//! so it never allocates or blocks.
//!
//! Clearing is by generation. [`AudioPlayback::clear`] bumps an epoch; the
//! callback silences from its next buffer on, and the worker resets the
//! resampler and pushes an epoch-tagged marker before any sample of the new
//! generation. The callback stays silent until it has consumed that marker, so
//! audio already in flight cannot leak past a clear. A marker consumed before
//! the callback noticed the epoch still re-arms it, so a clear can never leave
//! playback muted.

use crate::audio::{
    VOICE_CHUNK_FRAMES, convert,
    engine::{Counters, DeviceConfig},
    error::AudioError,
    resample,
};
use cpal::{
    FromSample, SampleFormat, SizedSample,
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
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Output ring capacity in milliseconds of device frames.
const RING_MS: usize = 80;

/// Playback queue capacity in 20 ms chunks: ~480 ms of 24 kHz audio. The
/// server streams faster than realtime, so this is the maximum lip-sync
/// backlog; whatever does not fit is dropped and counted.
const QUEUE_CHUNKS: usize = 24;

/// Worker wait for the next queued chunk.
const QUEUE_WAIT: Duration = Duration::from_millis(20);

/// Wait between attempts to push a generation marker into a full ring.
const MARKER_RETRY: Duration = Duration::from_millis(2);

/// Sleep quantum while the worker waits for ring space. Short enough that a
/// clear or stop is noticed within a couple of milliseconds; the callback
/// itself never waits.
const RING_WAIT: Duration = Duration::from_millis(2);

/// Bounded wait for the backend to start the output stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// The playback side of [`AudioEngine`](super::AudioEngine).
#[derive(Clone)]
pub struct AudioPlayback {
    shared: Arc<PlaybackShared>,
}

pub(crate) struct PlaybackParts {
    pub(crate) handle: AudioPlayback,
    pub(crate) stream: cpal::Stream,
    pub(crate) worker: JoinHandle<()>,
}

impl AudioPlayback {
    /// Opens the default output configuration of `device` and starts streaming.
    pub(crate) fn open(
        device: &cpal::Device,
        counters: Arc<Counters>,
    ) -> Result<PlaybackParts, AudioError> {
        let supported = device.default_output_config()?;
        let config = DeviceConfig {
            name: device.to_string(),
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            sample_format: supported.sample_format(),
        };

        let (queue, queued) = mpsc::sync_channel(QUEUE_CHUNKS);
        let shared = Arc::new(PlaybackShared {
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            queue,
            pending: Mutex::new(Vec::new()),
            counters,
            config: config.clone(),
        });

        let (producer, consumer) = HeapRb::<f32>::new(ring_frames(config.sample_rate)).split();

        // Build and start the stream before spawning the worker: a failed build
        // then leaves nothing behind that needs a stop signal.
        let stream = build_stream(
            device,
            supported.config(),
            config.sample_format,
            config.channels as usize,
            Arc::clone(&shared),
            consumer,
        )?;
        stream.play()?;
        let worker = spawn_worker(queued, producer, Arc::clone(&shared), config.sample_rate)?;

        Ok(PlaybackParts {
            handle: Self { shared },
            stream,
            worker,
        })
    }

    /// Queues 24 kHz mono PCM16 for playback. Samples accumulate in a small
    /// pending buffer and only complete 20 ms chunks are queued, so deltas
    /// split at arbitrary boundaries do not insert silence between them.
    /// Chunks that do not fit the bounded queue are dropped and counted
    /// instead of blocking the caller. The pending buffer is shared between
    /// clones, so one handle can be drained or cleared through another.
    pub fn play(&self, samples: &[i16]) -> Result<(), AudioError> {
        if samples.is_empty() {
            return Ok(());
        }
        let stamp = self.shared.epoch.load(Ordering::Acquire);
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut result = Ok(());
        let mut dropped = 0u64;
        pending.extend_from_slice(samples);
        while pending.len() >= VOICE_CHUNK_FRAMES {
            let chunk: Vec<i16> = pending.drain(..VOICE_CHUNK_FRAMES).collect();
            match self.shared.queue.try_send((stamp, chunk)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => dropped += 1,
                Err(TrySendError::Disconnected(_)) => {
                    result = Err(AudioError::PlaybackStopped);
                    break;
                }
            }
        }
        if dropped > 0 {
            self.shared
                .counters
                .output_dropped_chunks
                .fetch_add(dropped, Ordering::Relaxed);
        }
        result
    }

    /// Queues the pending partial chunk, padded with silence. Realtime deltas
    /// end at arbitrary boundaries, so `response.output_audio.done` should
    /// call this to release the tail instead of letting it prepend the next
    /// response's audio.
    pub fn flush(&self) -> Result<(), AudioError> {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.is_empty() {
            return Ok(());
        }
        pending.resize(VOICE_CHUNK_FRAMES, 0);
        let chunk: Vec<i16> = pending.drain(..VOICE_CHUNK_FRAMES).collect();
        let stamp = self.shared.epoch.load(Ordering::Acquire);
        match self.shared.queue.try_send((stamp, chunk)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.shared
                    .counters
                    .output_dropped_chunks
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(AudioError::PlaybackStopped),
        }
    }

    /// Drops every queued chunk - including the held partial - and silences
    /// the output from its next buffer.
    ///
    /// Called when push-to-talk begins so the model's own voice cannot reach
    /// the microphone while it is open.
    pub fn clear(&self) {
        self.shared.epoch.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.clear();
        }
    }

    /// Name and negotiated format of the default output device.
    pub fn config(&self) -> &DeviceConfig {
        &self.shared.config
    }

    /// Signals the worker to exit at its next poll.
    pub(crate) fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

struct PlaybackShared {
    config: DeviceConfig,
    /// Clear generation; bumped by [`AudioPlayback::clear`].
    epoch: AtomicU64,
    stop: AtomicBool,
    /// Chunks tagged with the generation they were queued in.
    queue: SyncSender<(u64, Vec<i16>)>,
    /// PCM from `play` that did not fill a chunk yet; always shorter than
    /// [`VOICE_CHUNK_FRAMES`] after a `play` call returns.
    pending: Mutex<Vec<i16>>,
    counters: Arc<Counters>,
}

/// Ring capacity in device frames for `rate`.
fn ring_frames(rate: u32) -> usize {
    (rate as usize * RING_MS / 1000).max(VOICE_CHUNK_FRAMES)
}

/// The output callback's world: a bounded ring consumer and format conversion.
struct OutputCallback<T> {
    /// Negotiated device channels per output frame; every mono ring frame is
    /// duplicated across all of them.
    channels: usize,
    consumer: HeapCons<f32>,
    shared: Arc<PlaybackShared>,
    /// Generation whose marker has been consumed from the ring.
    seen: Option<u64>,
    /// True until the marker for the current generation has been consumed.
    muted: bool,
    starving: bool,
    marker: PhantomData<T>,
}

impl<T> OutputCallback<T>
where
    T: SizedSample + Copy,
    T: FromSample<f32>,
{
    fn fill(&mut self, data: &mut [T]) {
        let slot = convert::generation_slot(self.shared.epoch.load(Ordering::Acquire));
        self.muted = self.seen != Some(slot);

        let mut starved = false;
        for frame in data.chunks_exact_mut(self.channels.max(1)) {
            // One mono ring frame fills every channel of the device frame.
            let mut value = T::EQUILIBRIUM;
            match self.consumer.try_pop() {
                Some(sample) => match convert::marker_epoch(sample) {
                    // Generation marker: everything after it is the new
                    // generation. A stale marker leaves playback muted.
                    Some(marker) => {
                        self.seen = Some(marker);
                        self.muted = marker != slot;
                        self.starving = false;
                    }
                    None if !self.muted => {
                        self.starving = false;
                        value = T::from_sample(sample);
                    }
                    None => {}
                },
                None => starved |= !self.muted,
            }
            frame.fill(value);
        }

        // Count one counter per starvation episode, not per silent callback.
        if starved && !self.starving {
            self.starving = true;
            self.shared
                .counters
                .output_underruns
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn build_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: SampleFormat,
    channels: usize,
    shared: Arc<PlaybackShared>,
    consumer: HeapCons<f32>,
) -> Result<cpal::Stream, AudioError> {
    macro_rules! typed {
        ($sample:ty) => {
            build_typed_stream::<$sample>(device, config, channels, shared, consumer)
        };
    }
    // The formats a default output device actually negotiates. Anything else is
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
    shared: Arc<PlaybackShared>,
    consumer: HeapCons<f32>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + Copy + Send + 'static,
    T: FromSample<f32>,
{
    let mut callback = OutputCallback::<T> {
        channels,
        consumer,
        seen: None,
        muted: true,
        starving: false,
        shared: Arc::clone(&shared),
        marker: PhantomData,
    };
    let errors = Arc::clone(&shared);
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _info| callback.fill(data),
            move |error| {
                errors
                    .counters
                    .note_error(format!("output stream: {error}"))
            },
            Some(STREAM_TIMEOUT),
        )
        .map_err(AudioError::from)
}

fn spawn_worker(
    queue: Receiver<(u64, Vec<i16>)>,
    producer: HeapProd<f32>,
    shared: Arc<PlaybackShared>,
    device_rate: u32,
) -> Result<JoinHandle<()>, AudioError> {
    let resampler = resample::from_voice(device_rate)?;
    let output = vec![0.0f32; resampler.output_frames_max()];
    thread::Builder::new()
        .name("zeron-voice-playback".to_owned())
        .spawn(move || {
            PlaybackWorker {
                shared,
                queue,
                producer,
                resampler,
                input: vec![0.0; VOICE_CHUNK_FRAMES],
                output,
                marker: None,
            }
            .run();
        })
        .map_err(|error| AudioError::Worker {
            name: "playback",
            error: error.to_string(),
        })
}

/// Resamples queued 24 kHz chunks to the device rate and pushes them into the
/// output ring. Never blocks the callback; when the ring is full the worker
/// waits in short bounded sleeps, checking stop and the clear epoch, instead
/// of dropping fresh audio.
struct PlaybackWorker {
    shared: Arc<PlaybackShared>,
    queue: Receiver<(u64, Vec<i16>)>,
    producer: HeapProd<f32>,
    resampler: Fft<f32>,
    /// One 20 ms chunk of 24 kHz input as floats.
    input: Vec<f32>,
    /// Resampled device-rate frames.
    output: Vec<f32>,
    /// Generation whose marker has been pushed into the ring.
    marker: Option<u64>,
}

impl PlaybackWorker {
    fn run(&mut self) {
        while !self.shared.stop.load(Ordering::Relaxed) {
            if !self.sync_generation() {
                // The ring is full (or the worker is stopping): retry later
                // instead of spinning.
                thread::park_timeout(MARKER_RETRY);
                continue;
            }
            match self.queue.recv_timeout(QUEUE_WAIT) {
                Ok((stamp, samples)) => {
                    if Some(convert::generation_slot(stamp)) != self.marker {
                        // Queued for a superseded generation; the next loop
                        // iteration resets and re-arms before anything plays.
                        continue;
                    }
                    self.resample_and_push(&samples);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Makes the ring carry the marker for the current generation, resetting
    /// resampler history and dropping stale queued chunks first. Returns
    /// `false` when the marker could not be pushed yet.
    fn sync_generation(&mut self) -> bool {
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        let slot = convert::generation_slot(epoch);
        if self.marker == Some(slot) {
            return true;
        }
        self.resampler.reset();
        while self.queue.try_recv().is_ok() {}
        if self.shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        if self
            .producer
            .try_push(convert::generation_marker(epoch))
            .is_err()
        {
            return false;
        }
        self.marker = Some(slot);
        true
    }

    fn resample_and_push(&mut self, samples: &[i16]) {
        for chunk in samples.chunks(VOICE_CHUNK_FRAMES) {
            for (slot, &sample) in self.input.iter_mut().zip(chunk) {
                *slot = convert::i16_to_f32(sample);
            }
            if chunk.len() < VOICE_CHUNK_FRAMES {
                self.input[chunk.len()..].fill(0.0);
            }

            match resample::resample_chunk(&mut self.resampler, &self.input, &mut self.output) {
                Ok(written) => {
                    self.wait_for_space(written);
                    let pushed = self.producer.push_slice(&self.output[..written]);
                    let dropped = written - pushed;
                    if dropped > 0 {
                        self.shared
                            .counters
                            .output_dropped_samples
                            .fetch_add(dropped as u64, Ordering::Relaxed);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "voice playback resampling failed");
                    self.shared.counters.note_error(error.to_string());
                    self.resampler.reset();
                }
            }
        }
    }

    /// Parks until the ring can fit `frames` more samples, the generation
    /// changes, or the worker is told to stop. The device callback drains the
    /// ring in realtime, so a transiently full ring clears within a few
    /// milliseconds; waiting here keeps faster-than-realtime deltas from
    /// being dropped just because the worker outran the device.
    fn wait_for_space(&self, frames: usize) {
        while self.producer.vacant_len() < frames
            && !self.shared.stop.load(Ordering::Relaxed)
            && Some(convert::generation_slot(
                self.shared.epoch.load(Ordering::Acquire),
            )) == self.marker
        {
            thread::park_timeout(RING_WAIT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback(queue: SyncSender<(u64, Vec<i16>)>) -> (AudioPlayback, Arc<PlaybackShared>) {
        let shared = Arc::new(PlaybackShared {
            config: DeviceConfig {
                name: "test".to_owned(),
                channels: 1,
                sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            queue,
            pending: Mutex::new(Vec::new()),
            counters: Arc::new(Counters::default()),
        });
        (
            AudioPlayback {
                shared: Arc::clone(&shared),
            },
            shared,
        )
    }

    fn callback(
        shared: &Arc<PlaybackShared>,
        consumer: HeapCons<f32>,
        channels: usize,
    ) -> OutputCallback<f32> {
        OutputCallback {
            channels,
            consumer,
            shared: Arc::clone(shared),
            seen: None,
            muted: true,
            starving: false,
            marker: PhantomData,
        }
    }

    #[test]
    fn markers_arm_playback_and_clears_silence_it() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(1);
        let (_playback, shared) = playback(queue);
        let mut callback = callback(&shared, consumer, 1);
        let mut out = [0.0f32; 8];

        // Nothing queued and no marker yet: silence.
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);

        // A marker starts generation 0.
        producer.push_slice(&[convert::generation_marker(0), 0.25, 0.5]);
        callback.fill(&mut out);
        assert_eq!(out[..3], [0.0, 0.25, 0.5]); // The ring ran dry mid-buffer while unmuted: one underrun edge.
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 1);

        // Samples already queued are discarded once the clear is visible.
        producer.push_slice(&[0.75, 1.0]);
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);

        // The new generation stays silent until its marker arrives.
        producer.push_slice(&[0.75, 0.75]);
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);

        producer.push_slice(&[convert::generation_marker(1), 0.5, 0.5, 0.5, 0.5]);
        callback.fill(&mut out);
        assert_eq!(out[..5], [0.0, 0.5, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn stereo_fill_duplicates_each_mono_frame_across_channels() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(1);
        let (_playback, shared) = playback(queue);
        let mut callback = callback(&shared, consumer, 2);
        let mut out = [0.0f32; 8];

        // Three mono frames fill three stereo frames; the fourth is dry.
        producer.push_slice(&[convert::generation_marker(0), 0.25, 0.5, -0.5]);
        callback.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 0.25, 0.25, 0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn a_marker_consumed_before_the_epoch_bump_still_arms_playback() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(1);
        let (_playback, shared) = playback(queue);
        let mut callback = callback(&shared, consumer, 1);
        let mut out = [0.0f32; 8];

        // The callback consumes the generation-1 marker while it still
        // believes the current generation is 0, so the frames after it stay
        // silent for this buffer.
        producer.push_slice(&[convert::generation_marker(1), 0.25, 0.25]);
        callback.fill(&mut out);
        assert_eq!(out[..4], [0.0, 0.0, 0.0, 0.0]);

        // Once the bump becomes visible, playback is armed again instead of
        // muting a generation whose marker was already consumed.
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        producer.push_slice(&[0.25, 0.25]);
        callback.fill(&mut out);
        assert_eq!(out[..2], [0.25, 0.25]);
    }

    #[test]
    fn play_carries_partial_chunks_across_calls_and_flush_releases_them() {
        let (queue, queued) = mpsc::sync_channel(4);
        let (playback, shared) = playback(queue);

        // The first delta ends mid-chunk: the head is queued complete and the
        // tail is held, not padded.
        let first: Vec<i16> = (0..VOICE_CHUNK_FRAMES as i16 + 1).collect();
        playback.play(&first).unwrap();
        let (stamp, head) = queued.recv().unwrap();
        assert_eq!(stamp, 0);
        assert_eq!(head, first[..VOICE_CHUNK_FRAMES]);
        assert!(queued.try_recv().is_err());

        // The second delta completes the held tail, then leaves its own
        // remainder pending: nothing but 480-frame chunks reach the queue.
        let second: Vec<i16> = vec![-1; VOICE_CHUNK_FRAMES];
        playback.play(&second).unwrap();
        let (stamp, joined) = queued.recv().unwrap();
        assert_eq!(stamp, 0);
        assert_eq!(joined[0], VOICE_CHUNK_FRAMES as i16);
        assert!(joined[1..].iter().all(|&sample| sample == -1));
        assert!(queued.try_recv().is_err());

        // The still-pending remainder (one -1 sample) takes the generation it
        // flushes in and is padded with silence.
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        playback.flush().unwrap();
        let (stamp, tail) = queued.recv().unwrap();
        assert_eq!(stamp, 1);
        assert_eq!(tail[0], -1);
        assert!(tail[1..].iter().all(|&sample| sample == 0));
        assert!(playback.flush().is_ok());
        assert!(queued.try_recv().is_err());

        // Clear discards the held partial along with the queued chunks.
        playback.play(&[7]).unwrap();
        playback.clear();
        assert_eq!(shared.epoch.load(Ordering::Acquire), 2);
        playback.play(&[8; VOICE_CHUNK_FRAMES - 1]).unwrap();
        playback.flush().unwrap();
        let (_stamp, tail) = queued.recv().unwrap();
        assert!(
            tail[..VOICE_CHUNK_FRAMES - 1]
                .iter()
                .all(|&sample| sample == 8)
        );

        assert!(playback.play(&[]).is_ok());
    }
}

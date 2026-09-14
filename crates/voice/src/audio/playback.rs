//! Default-device playback: bounded queue -> resample/gain/limit worker -> SPSC ring -> output callback.
//!
//! [`AudioPlayback::play`] takes decoded 24 kHz mono PCM16 chunks (the
//! `response.output_audio.delta` payloads), buffers them into 20 ms pieces -
//! deltas end at arbitrary boundaries, so partial frames carry over between
//! calls - and hands them to a worker that resamples to the device rate,
//! applies the configured output gain and a soft limiter, and pushes into the
//! ring. The output callback only pops from the bounded ring and converts to
//! the device format, so it never allocates or blocks.
//!
//! The callback holds fresh audio in a small stash until the configured
//! startup buffer ([`PlaybackTuning::start_buffer`]) has accumulated, and
//! re-enters that buffer whenever the ring runs dry mid-stream, so a bursty
//! delta stream plays out smoothly instead of crackling per underrun. The
//! worker marks the end of each response's audio in the ring; draining dry
//! after that mark is a normal end, not an underrun.
//!
//! Clearing is by generation. [`AudioPlayback::clear`] bumps an epoch; the
//! callback silences from its next buffer on, and the worker resets the
//! resampler and pushes an epoch-tagged marker before any sample of the new
//! generation. Queued chunks carry the generation they were pushed in, so the
//! worker skips stale ones as it walks the queue instead of draining it - a
//! chunk queued after a clear always survives. The callback stays silent until
//! it has consumed the marker, so audio already in flight cannot leak past a
//! clear. A marker consumed before the callback noticed the epoch still
//! re-arms it, so a clear can never leave playback muted.
//!
//! The queue reserves its last two slots for [`AudioPlayback::flush`]: the
//! response's tail chunk and its end-of-audio mark always land without
//! waiting, even when faster-than-realtime deltas filled every steady slot.

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

/// Playback queue capacity in 20 ms chunks: 60 s of 24 kHz audio, about
/// 2.9 MB of PCM16. Realtime deltas arrive faster than the device drains
/// them, so the queue is deep enough to hold a normal spoken response
/// without ever dropping; only chunks past this hard cap are dropped and
/// counted, never reordered or truncated.
const QUEUE_CHUNKS: usize = 3_000;

/// Queue slots kept free for [`AudioPlayback::flush`]: one for the response's
/// tail chunk and one for its end-of-audio mark. [`AudioPlayback::play`]
/// refuses to fill them, so a flush never parks behind a full queue.
const RESERVED_TAIL: usize = 2;

/// Worker wait for the next queued chunk.
const QUEUE_WAIT: Duration = Duration::from_millis(20);

/// Wait between attempts to push a marker into a full ring.
const MARKER_RETRY: Duration = Duration::from_millis(2);

/// Sleep quantum while the worker waits for ring space. Short enough that a
/// clear or stop is noticed within a couple of milliseconds; the callback
/// itself never waits.
const RING_WAIT: Duration = Duration::from_millis(2);

/// Bounded wait for the backend to start the output stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// Ring items one output frame may consume. Control marks and stashed samples
/// can stack a few deep per device frame; the bound keeps a pathological run
/// of marks from stretching one frame's work.
const MAX_POPS_PER_FRAME: usize = 32;

/// Playback tuning carried by
/// [`AudioEngineConfig`](crate::audio::AudioEngineConfig).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaybackTuning {
    /// Output ring capacity; absorbs worker scheduling jitter between the
    /// resampler and the device callback. Default 200 ms.
    pub ring: Duration,
    /// Audio the callback stashes before unmuting, and again after every
    /// mid-stream starvation. Default 120 ms: long enough to smooth delta
    /// bursts, short enough to stay ahead of the ring.
    pub start_buffer: Duration,
    /// Linear output gain applied on the worker, before the limiter.
    /// Default 1.0.
    pub gain: f32,
    /// Level above which the soft limiter folds peaks into the remaining
    /// headroom toward 1.0. Default 0.95; 1.0 degenerates to a hard clamp.
    pub limiter_ceiling: f32,
}

impl PlaybackTuning {
    pub(crate) const MIN_RING: Duration = Duration::from_millis(20);
    pub(crate) const MAX_RING: Duration = Duration::from_secs(5);
}

impl Default for PlaybackTuning {
    fn default() -> Self {
        Self {
            ring: Duration::from_millis(200),
            start_buffer: Duration::from_millis(120),
            gain: 1.0,
            limiter_ceiling: 0.95,
        }
    }
}

/// One message on the playback queue.
#[derive(Debug, PartialEq)]
enum PlaybackMsg {
    /// One 20 ms chunk of 24 kHz mono PCM16, tagged with the generation it was
    /// queued in.
    Chunk(u64, Vec<i16>),
    /// The response's audio ends after every chunk queued before this mark.
    /// Tagged with its generation so a clear can drain it with the rest.
    EndOfAudio(u64),
}

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
        tuning: PlaybackTuning,
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
            queue_chunks: QUEUE_CHUNKS,
            pending: Mutex::new(Vec::new()),
            counters,
            config: config.clone(),
            tuning,
        });

        let (producer, consumer) =
            HeapRb::<f32>::new(ring_frames(config.sample_rate, tuning.ring)).split();

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
    /// Chunks that do not fit the queue's steady capacity - the hard cap minus
    /// the slots reserved for a flush - are dropped and counted instead of
    /// blocking the caller. The pending buffer is shared between clones, so
    /// one handle can be drained or cleared through another.
    pub fn play(&self, samples: &[i16]) -> Result<(), AudioError> {
        if samples.is_empty() {
            return Ok(());
        }
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Read under the pending lock so the stamp serializes against
        // `clear`: a chunk either predates the clear (dropped by the worker)
        // or postdates it, never straddles it.
        let stamp = self.shared.epoch.load(Ordering::Acquire);
        let mut result = Ok(());
        let mut dropped = 0u64;
        pending.extend_from_slice(samples);
        let complete = pending.len() / VOICE_CHUNK_FRAMES * VOICE_CHUNK_FRAMES;
        if complete > 0 {
            // One pass over the completed prefix: drain it once, then hand each
            // 20 ms slice out in order. Per-chunk `drain(..CHUNK)` would shift
            // the whole buffer every iteration - quadratic on a large delta.
            let drained: Vec<i16> = pending.drain(..complete).collect();
            let limit = self.shared.queue_chunks.saturating_sub(RESERVED_TAIL);
            for chunk in drained.as_chunks::<VOICE_CHUNK_FRAMES>().0 {
                match self.send(PlaybackMsg::Chunk(stamp, chunk.to_vec()), limit) {
                    Ok(true) => {}
                    Ok(false) => dropped += 1,
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
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

    /// Queues the pending partial chunk, padded with silence, then the mark
    /// that tells the callback this response's audio is complete. Realtime
    /// deltas end at arbitrary boundaries, so `response.output_audio.done`
    /// should call this to release the tail instead of letting it prepend the
    /// next response's audio.
    ///
    /// Nonblocking: the tail and the end mark go into the queue's two reserved
    /// slots, so a queue full of steady audio can never park the caller. A
    /// genuinely full queue drops the piece and counts it; a dead worker
    /// propagates [`AudioError::PlaybackStopped`].
    pub fn flush(&self) -> Result<(), AudioError> {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Same lock rule as `play`: the stamp cannot straddle a clear, and the
        // occupancy counter serializes against other producers under it.
        let stamp = self.shared.epoch.load(Ordering::Acquire);
        if !pending.is_empty() {
            pending.resize(VOICE_CHUNK_FRAMES, 0);
            let tail: Vec<i16> = pending.drain(..VOICE_CHUNK_FRAMES).collect();
            // The tail owns the first reserved slot.
            if !self.send(
                PlaybackMsg::Chunk(stamp, tail),
                self.shared.queue_chunks.saturating_sub(1),
            )? {
                self.shared
                    .counters
                    .output_dropped_chunks
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        // The end mark owns the very last slot: the response's end reaches the
        // ring even when its audio overshot every other slot.
        if !self.send(PlaybackMsg::EndOfAudio(stamp), self.shared.queue_chunks)? {
            self.shared
                .counters
                .output_dropped_chunks
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Sends one message if occupancy - the counter every producer increments
    /// under the pending lock and the worker decrements on receive - is below
    /// `limit`. `Ok(false)` means the queue was full at this limit;
    /// [`AudioError::PlaybackStopped`] means the worker is gone. The counter
    /// can only overstate occupancy (a worker decrement may not be visible
    /// yet), so `Ok(true)` implies the bounded channel itself has room.
    fn send(&self, message: PlaybackMsg, limit: usize) -> Result<bool, AudioError> {
        if self.shared.counters.playback_queued.load(Ordering::Relaxed) as usize >= limit {
            return Ok(false);
        }
        match self.shared.queue.try_send(message) {
            Ok(()) => {
                self.shared
                    .counters
                    .playback_queued
                    .fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
            Err(TrySendError::Full(_)) => Ok(false),
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
    tuning: PlaybackTuning,
    /// Clear generation; bumped by [`AudioPlayback::clear`].
    epoch: AtomicU64,
    stop: AtomicBool,
    /// Chunks and end marks tagged with the generation they were queued in.
    queue: SyncSender<PlaybackMsg>,
    /// The channel's capacity: producers fill it only up to
    /// `queue_chunks - RESERVED_TAIL` so a flush always fits.
    queue_chunks: usize,
    /// PCM from `play` that did not fill a chunk yet; always shorter than
    /// [`VOICE_CHUNK_FRAMES`] after a `play` call returns.
    pending: Mutex<Vec<i16>>,
    counters: Arc<Counters>,
}

/// Ring capacity in device frames for `rate`.
fn ring_frames(rate: u32, ring: Duration) -> usize {
    (rate as usize * ring.as_millis() as usize / 1000).max(VOICE_CHUNK_FRAMES)
}

/// Startup/rebuffer threshold in device frames for `rate`.
fn start_frames(rate: u32, start_buffer: Duration) -> usize {
    (rate as usize * start_buffer.as_millis() as usize / 1000).max(1)
}

/// What the callback is doing with the ring right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputState {
    /// No marker for the current generation has been consumed (or a stale one
    /// was): emit silence, discard samples.
    Muted,
    /// Armed but collecting the startup/rebuffer threshold into the stash.
    Buffering,
    /// Emitting stashed samples oldest-first.
    Playing,
    /// The response's end mark was consumed and its audio fully drained.
    /// Silence here is a normal end, not an underrun.
    Ended,
}

/// The output callback's world: a bounded ring consumer, the startup stash,
/// and format conversion. The stash is a fixed `Vec` capped at
/// `start_frames + 1` - one callback never allocates.
struct OutputCallback<T> {
    /// Negotiated device channels per output frame; every mono ring frame is
    /// duplicated across all of them.
    channels: usize,
    consumer: HeapCons<f32>,
    shared: Arc<PlaybackShared>,
    /// Generation whose marker has been consumed from the ring.
    seen: Option<u64>,
    state: OutputState,
    /// Real samples collected while `Buffering`, emitted from `stash_head`
    /// while `Playing`. The full `start_frames + 1` capacity is reserved
    /// before the stream is built, and the Vec never grows past it: it fills
    /// only in `Buffering`, where a hit flips to `Playing` at the threshold,
    /// and `Playing` pops the ring directly once the stash drains.
    stash: Vec<f32>,
    /// Read cursor into `stash`; `stash[stash_head..]` is undrained audio.
    stash_head: usize,
    /// Frames the stash must hold before `Playing` (re)starts.
    start_frames: usize,
    /// The current generation's end-of-audio mark has been consumed: running
    /// dry afterwards is the response's normal end, not an underrun.
    ended: bool,
    /// Set while `Buffering` after a starvation so recovery to `Playing`
    /// counts as a rebuffer rather than a first start.
    recovering: bool,
    marker: PhantomData<T>,
}

impl<T> OutputCallback<T>
where
    T: SizedSample + Copy,
    T: FromSample<f32>,
{
    fn fill(&mut self, data: &mut [T]) {
        let slot = convert::generation_slot(self.shared.epoch.load(Ordering::Acquire));
        if self.seen != Some(slot) {
            // Cleared or never armed: silence until this generation's marker.
            self.state = OutputState::Muted;
            self.stash.clear();
            self.stash_head = 0;
            self.recovering = false;
        } else if self.state == OutputState::Muted {
            // The marker was consumed before the epoch bump became visible;
            // re-arm instead of muting a generation whose marker is gone.
            self.state = OutputState::Buffering;
        }

        for frame in data.chunks_exact_mut(self.channels.max(1)) {
            // One mono ring frame fills every channel of the device frame.
            frame.fill(T::from_sample(self.next_sample(slot)));
        }
    }

    /// Advances one mono frame. The callback pops at most what it can use
    /// this frame: one real sample to emit, plus any control marks it passes
    /// on the way. That keeps the stash bounded - while `Buffering` it fills
    /// to the threshold and flips to `Playing`; while `Playing` an undrained
    /// stash head is returned directly without touching the ring, so marks
    /// and samples behind it keep their order for the next frame. A pop
    /// loop that greedily drained the ring into the stash would let it grow
    /// far past the threshold and allocate on the audio thread.
    fn next_sample(&mut self, slot: u64) -> f32 {
        // While playing out a stashed prefix, this frame's sample is already
        // in hand: emit it and leave the ring alone. Marks interleaved after
        // it are consumed on the following frames, still in order.
        if self.state == OutputState::Playing && self.stash_head < self.stash.len() {
            return self.take_stashed();
        }

        for _ in 0..MAX_POPS_PER_FRAME {
            let Some(raw) = self.consumer.try_pop() else {
                break;
            };
            match convert::ring_signal(raw) {
                Some(convert::RingSignal::Generation(marker)) => {
                    self.seen = Some(marker);
                    self.stash.clear();
                    self.stash_head = 0;
                    self.ended = false;
                    self.recovering = false;
                    self.state = if marker == slot {
                        OutputState::Buffering
                    } else {
                        OutputState::Muted
                    };
                }
                Some(convert::RingSignal::EndOfAudio(epoch)) if epoch == slot => {
                    self.ended = true;
                    self.shared
                        .counters
                        .output_ended
                        .fetch_add(1, Ordering::Relaxed);
                    if self.state == OutputState::Buffering {
                        // A response shorter than the startup buffer still
                        // plays its tail out rather than waiting for audio
                        // that will never come.
                        self.state = if self.stash_head == self.stash.len() {
                            OutputState::Ended
                        } else {
                            OutputState::Playing
                        };
                    }
                }
                // A mark from a superseded generation is inert: it cannot end
                // or arm audio it did not travel behind.
                Some(_) => {}
                None => match self.state {
                    OutputState::Muted => {}
                    OutputState::Ended => {
                        // A later response in the same generation: rebuffer
                        // before it rather than crackling it awake.
                        self.state = OutputState::Buffering;
                        self.ended = false;
                        self.stash.push(raw);
                    }
                    OutputState::Buffering => {
                        self.stash.push(raw);
                        if self.stash.len() - self.stash_head >= self.start_frames {
                            self.state = OutputState::Playing;
                            if self.recovering {
                                self.recovering = false;
                                self.shared
                                    .counters
                                    .output_rebuffers
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            // The stash just reached its threshold: stop
                            // popping so the rest of the ring keeps its
                            // place for the next frame and the stash stays
                            // capped at the threshold.
                            break;
                        }
                    }
                    // Playing with an empty stash (guaranteed by the early
                    // return above at loop entry): the sample is this
                    // frame's output. A mid-loop Buffering->Playing flip
                    // breaks out above before this arm can be reached with
                    // an undrained stash, so the ring stays the stash's
                    // overflow buffer.
                    OutputState::Playing => return raw,
                },
            }
        }

        match self.state {
            OutputState::Playing => {
                if self.stash_head == self.stash.len() {
                    if self.ended {
                        // The response drained completely: a normal end.
                        self.state = OutputState::Ended;
                    } else {
                        // The stream starved mid-response: count the underrun
                        // and rebuffer; the next threshold fill is a rebuffer
                        // recovery, not a first start.
                        self.state = OutputState::Buffering;
                        self.recovering = true;
                        self.shared
                            .counters
                            .output_underruns
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    0.0
                } else {
                    self.take_stashed()
                }
            }
            _ => 0.0,
        }
    }

    /// Emits the next stashed sample and resets the stash once it drains.
    fn take_stashed(&mut self) -> f32 {
        let sample = self.stash[self.stash_head];
        self.stash_head += 1;
        if self.stash_head == self.stash.len() {
            // Fully drained: reset so the next buffer starts clean.
            self.stash.clear();
            self.stash_head = 0;
        }
        sample
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
    // The stash is sized before the callback exists: its only growth is one
    // push per frame while `Buffering`, and the flip to `Playing` happens at
    // the threshold, so `start_frames + 1` covers the largest fill outright.
    // Reserving it here keeps the first `stash.push` from allocating on the
    // audio thread.
    let start_frames = start_frames(shared.config.sample_rate, shared.tuning.start_buffer);
    let mut callback = OutputCallback::<T> {
        channels,
        consumer,
        seen: None,
        state: OutputState::Muted,
        stash: Vec::with_capacity(start_frames + 1),
        stash_head: 0,
        start_frames,
        ended: false,
        recovering: false,
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
    queue: Receiver<PlaybackMsg>,
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
                pending_end: false,
            }
            .run();
        })
        .map_err(|error| AudioError::Worker {
            name: "playback",
            error: error.to_string(),
        })
}

/// Resamples queued 24 kHz chunks to the device rate, applies output gain and
/// the soft limiter, and pushes them into the output ring. Never blocks the
/// callback; when the ring is full the worker waits in short bounded sleeps,
/// checking stop and the clear epoch, instead of dropping fresh audio.
struct PlaybackWorker {
    shared: Arc<PlaybackShared>,
    queue: Receiver<PlaybackMsg>,
    producer: HeapProd<f32>,
    resampler: Fft<f32>,
    /// One 20 ms chunk of 24 kHz input as floats.
    input: Vec<f32>,
    /// Resampled device-rate frames.
    output: Vec<f32>,
    /// Generation whose marker has been pushed into the ring.
    marker: Option<u64>,
    /// An end-of-audio mark for the current generation is waiting for ring
    /// space; no later chunk may pass it.
    pending_end: bool,
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
            if self.pending_end {
                // The end mark precedes every chunk queued behind it: push it
                // before receiving more, or hold until the ring has room.
                if self.push_pending_end() {
                    continue;
                }
                thread::park_timeout(MARKER_RETRY);
                continue;
            }
            match self.queue.recv_timeout(QUEUE_WAIT) {
                Ok(message) => {
                    self.shared
                        .counters
                        .playback_queued
                        .fetch_sub(1, Ordering::Relaxed);
                    self.handle(message);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Pushes the marker for the current generation into the ring and makes
    /// it the worker's armed generation, resetting resampler history first.
    /// The queue is never drained: every message stays in FIFO order and
    /// [`PlaybackWorker::handle`] drops the ones stamped before this
    /// generation, so a chunk queued after a clear but before this pass
    /// survives where a blind drain would discard it. A clear that lands
    /// between the epoch read and the marker push leaves the marker armed
    /// one generation behind, which the next pass notices and corrects.
    /// Returns `false` when the marker could not be pushed yet.
    fn sync_generation(&mut self) -> bool {
        loop {
            if self.marker
                == Some(convert::generation_slot(
                    self.shared.epoch.load(Ordering::Acquire),
                ))
            {
                return true;
            }
            self.resampler.reset();
            self.pending_end = false;
            if self.shared.stop.load(Ordering::Relaxed) {
                return false;
            }
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            if self
                .producer
                .try_push(convert::generation_marker(epoch))
                .is_err()
            {
                return false;
            }
            let slot = convert::generation_slot(epoch);
            self.marker = Some(slot);
            // A clear that landed between the read and the push armed a
            // stale marker: keep the loop turning so the marker the ring
            // carries is the current generation's, not one already dead.
            if slot
                == convert::generation_slot(self.shared.epoch.load(Ordering::Acquire))
            {
                return true;
            }
        }
    }

    /// Pushes the held end-of-audio mark into the ring. Returns `false` when
    /// the ring has no room; the caller must retry before dequeuing more.
    fn push_pending_end(&mut self) -> bool {
        let Some(marker) = self.marker else {
            // An end mark with no armed generation is stale: drop it rather
            // than pushing an epoch-less mark the callback would ignore.
            self.pending_end = false;
            return true;
        };
        if self
            .producer
            .try_push(convert::end_of_audio_marker(marker))
            .is_err()
        {
            return false;
        }
        self.pending_end = false;
        true
    }

    fn handle(&mut self, message: PlaybackMsg) {
        match message {
            PlaybackMsg::Chunk(stamp, samples) => {
                if Some(convert::generation_slot(stamp)) != self.marker {
                    // Queued for a superseded generation; the next loop
                    // iteration resets and re-arms before anything plays.
                    return;
                }
                self.resample_and_push(&samples);
            }
            PlaybackMsg::EndOfAudio(stamp) => {
                if Some(convert::generation_slot(stamp)) == self.marker {
                    self.pending_end = true;
                }
            }
        }
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
                    self.shape_output(written);
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

    /// Applies output gain and the soft limiter to the resampled frames. Runs
    /// on the worker, not the callback, so the callback path stays trivial.
    /// The limiter also hard-clamps into [-1.0, 1.0], so every sample the
    /// ring carries stays in range no matter how the resampler overshoots
    /// or how far the configured gain pushes it.
    fn shape_output(&mut self, frames: usize) {
        let gain = self.shared.tuning.gain;
        let ceiling = self.shared.tuning.limiter_ceiling;
        let mut limited = 0u64;
        for sample in &mut self.output[..frames] {
            if sample.abs() * gain > ceiling {
                limited += 1;
            }
            *sample = convert::limit_output(*sample, gain, ceiling);
        }
        if limited > 0 {
            self.shared
                .counters
                .output_limited_samples
                .fetch_add(limited, Ordering::Relaxed);
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

    fn tuning() -> PlaybackTuning {
        PlaybackTuning {
            ring: Duration::from_millis(200),
            start_buffer: Duration::from_millis(120),
            gain: 1.0,
            limiter_ceiling: 0.95,
        }
    }

    /// Builds a playback handle over `queue`; `queue_chunks` must be the
    /// capacity the channel was created with, since the channel cannot report
    /// it back.
    fn playback(
        queue: SyncSender<PlaybackMsg>,
        queue_chunks: usize,
    ) -> (AudioPlayback, Arc<PlaybackShared>) {
        playback_with(queue, queue_chunks, tuning())
    }

    fn playback_with(
        queue: SyncSender<PlaybackMsg>,
        queue_chunks: usize,
        tuning: PlaybackTuning,
    ) -> (AudioPlayback, Arc<PlaybackShared>) {
        let shared = Arc::new(PlaybackShared {
            config: DeviceConfig {
                name: "test".to_owned(),
                channels: 1,
                sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
            tuning,
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            queue,
            queue_chunks,
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

    /// Builds a callback the way `build_typed_stream` does: the stash's full
    /// `start_frames + 1` capacity reserved up front so no push inside `fill`
    /// ever allocates.
    fn callback(
        shared: &Arc<PlaybackShared>,
        consumer: HeapCons<f32>,
        channels: usize,
        start_frames: usize,
    ) -> OutputCallback<f32> {
        OutputCallback {
            channels,
            consumer,
            shared: Arc::clone(shared),
            seen: None,
            state: OutputState::Muted,
            stash: Vec::with_capacity(start_frames + 1),
            stash_head: 0,
            start_frames,
            ended: false,
            recovering: false,
            marker: PhantomData,
        }
    }

    /// Receives one queued message and keeps the occupancy counter honest,
    /// the way the worker's receive does.
    fn drain(receiver: &Receiver<PlaybackMsg>, shared: &PlaybackShared) -> PlaybackMsg {
        let message = receiver.recv().unwrap();
        shared
            .counters
            .playback_queued
            .fetch_sub(1, Ordering::Relaxed);
        message
    }

    fn chunk(msg: PlaybackMsg) -> (u64, Vec<i16>) {
        match msg {
            PlaybackMsg::Chunk(stamp, samples) => (stamp, samples),
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    fn worker(
        shared: &Arc<PlaybackShared>,
        queue: Receiver<PlaybackMsg>,
        producer: HeapProd<f32>,
    ) -> PlaybackWorker {
        let resampler = resample::from_voice(shared.config.sample_rate).unwrap();
        PlaybackWorker {
            shared: Arc::clone(shared),
            queue,
            producer,
            output: vec![0.0; resampler.output_frames_max()],
            resampler,
            input: vec![0.0; VOICE_CHUNK_FRAMES],
            marker: None,
            pending_end: false,
        }
    }

    #[test]
    fn markers_arm_playback_and_clears_silence_it() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 1);
        let mut out = [0.0f32; 8];

        // Nothing queued and no marker yet: silence.
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);

        // A marker starts generation 0; with a one-frame startup buffer the
        // first sample plays immediately.
        producer.push_slice(&[convert::generation_marker(0), 0.25, 0.5]);
        callback.fill(&mut out);
        assert_eq!(out[..3], [0.25, 0.5, 0.0]);
        // The ring ran dry mid-buffer while unmuted: one underrun episode,
        // which re-enters the startup buffer in the recovering state.
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 1);

        // Refilling to the threshold recovers playback and counts the
        // rebuffer - distinct from the underrun that opened the gap.
        producer.push_slice(&[0.9]);
        callback.fill(&mut out);
        assert_eq!(shared.counters.output_rebuffers.load(Ordering::Relaxed), 1);
        assert_eq!(out[0], 0.9);

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
        assert_eq!(out[..5], [0.5, 0.5, 0.5, 0.5, 0.0]);
    }

    #[test]
    fn stereo_fill_duplicates_each_mono_frame_across_channels() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 2, 1);
        let mut out = [0.0f32; 8];

        // Three mono frames fill three stereo frames; the fourth starves.
        producer.push_slice(&[convert::generation_marker(0), 0.25, 0.5, -0.5]);
        callback.fill(&mut out);
        assert_eq!(out, [0.25, 0.25, 0.5, 0.5, -0.5, -0.5, 0.0, 0.0]);
    }

    #[test]
    fn a_marker_consumed_before_the_epoch_bump_still_arms_playback() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 1);
        let mut out = [0.0f32; 8];

        // The callback consumes the generation-1 marker while it still
        // believes the current generation is 0, so the frames after it stay
        // silent for this buffer.
        producer.push_slice(&[convert::generation_marker(1), 0.25, 0.25]);
        callback.fill(&mut out);
        assert_eq!(out[..4], [0.0, 0.0, 0.0, 0.0]);

        // Once the bump becomes visible, playback re-arms through the startup
        // buffer instead of muting a generation whose marker was consumed.
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        producer.push_slice(&[0.25, 0.25]);
        callback.fill(&mut out);
        assert_eq!(out[..2], [0.25, 0.25]);
    }

    #[test]
    fn the_startup_buffer_holds_audio_until_its_threshold() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        // Four frames of startup buffer: audio arriving below it stays silent.
        let mut callback = callback(&shared, consumer, 1, 4);
        let mut out = [0.0f32; 8];

        producer.push_slice(&[convert::generation_marker(0), 0.1, 0.2]);
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);
        // Buffering is not an underrun.
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 0);

        // Reaching the threshold releases everything in order.
        producer.push_slice(&[0.3, 0.4]);
        callback.fill(&mut out);
        assert_eq!(out[..6], [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    }

    #[test]
    fn a_normal_end_never_counts_as_an_underrun() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 1);
        let mut out = [0.0f32; 8];

        producer.push_slice(&[
            convert::generation_marker(0),
            0.5,
            0.5,
            convert::end_of_audio_marker(0),
        ]);
        callback.fill(&mut out);
        assert_eq!(out[..3], [0.5, 0.5, 0.0]);
        assert_eq!(callback.state, OutputState::Ended);
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 0);
        assert_eq!(shared.counters.output_ended.load(Ordering::Relaxed), 1);

        // The ring staying dry after the end is still a normal end.
        callback.fill(&mut out);
        assert_eq!(out, [0.0; 8]);
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_response_shorter_than_the_startup_buffer_still_plays() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 8);
        let mut out = [0.0f32; 8];

        // Two frames never reach the eight-frame threshold, but the end mark
        // flushes them instead of leaving them stashed forever.
        producer.push_slice(&[
            convert::generation_marker(0),
            0.5,
            0.5,
            convert::end_of_audio_marker(0),
        ]);
        callback.fill(&mut out);
        assert_eq!(out[..4], [0.5, 0.5, 0.0, 0.0]);
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_stale_end_mark_cannot_end_a_new_generations_response() {
        let (mut producer, consumer) = HeapRb::<f32>::new(64).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 1);
        let mut out = [0.0f32; 8];

        // Arm generation 0 and end its response, then clear before the new
        // generation's audio is consumed.
        producer.push_slice(&[
            convert::generation_marker(0),
            0.5,
            convert::end_of_audio_marker(0),
        ]);
        callback.fill(&mut out);
        assert_eq!(callback.state, OutputState::Ended);
        shared.epoch.fetch_add(1, Ordering::AcqRel);

        // An end-of-audio mark from generation 0 still sits in the ring ahead
        // of generation 1's marker and its audio. Consuming it while already
        // cleared must not end the new generation's response early.
        producer.push_slice(&[
            convert::end_of_audio_marker(0), // stale: queued before the clear
            convert::generation_marker(1),
            0.7,
            0.7,
        ]);
        callback.fill(&mut out);
        // The stale mark is inert; generation 1's audio plays.
        assert_eq!(out[..4], [0.7, 0.7, 0.0, 0.0]);
        // Only generation-0's own mark was counted as ended, not the stale one
        // a second time.
        assert_eq!(shared.counters.output_ended.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_stash_stays_bounded_while_playing() {
        // A ring that always has more samples than one frame can emit: the
        // stash must not grow past the startup threshold even across many
        // device frames, or the callback would allocate on the audio path.
        let (mut producer, consumer) = HeapRb::<f32>::new(4096).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 4);
        let mut out = [0.0f32; 4];

        // Arm generation 0 and flood it with far more samples than the ring
        // threshold; each `fill` only needs one mono frame per device frame.
        producer.push_slice(&[convert::generation_marker(0)]);
        let audio: Vec<f32> = (0..4000).map(|i| (i as f32 % 8.0) / 8.0).collect();
        producer.push_slice(&audio);

        for _ in 0..64 {
            callback.fill(&mut out);
            assert!(
                callback.stash.len() <= callback.start_frames,
                "stash grew to {} past the {}-frame threshold",
                callback.stash.len(),
                callback.start_frames
            );
        }
    }

    #[test]
    fn the_stash_capacity_is_reserved_before_use_and_never_grows() {
        // The callback's stash must arrive fully reserved: the first push
        // while `Buffering` cannot allocate on the audio thread, and no fill
        // path may grow it past the startup threshold.
        let (mut producer, consumer) = HeapRb::<f32>::new(4096).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 64);
        let reserved = callback.stash.capacity();
        assert_eq!(
            reserved,
            callback.start_frames + 1,
            "stash capacity is established before the first fill"
        );
        let mut out = [0.0f32; 8];

        // Arm generation 0 and fill to exactly the threshold across several
        // device frames: capacity must not change while the stash grows.
        producer.push_slice(&[convert::generation_marker(0)]);
        let audio: Vec<f32> = (0..64).map(|i| (i as f32 + 1.0) / 128.0).collect();
        producer.push_slice(&audio);
        while callback.state != OutputState::Playing {
            callback.fill(&mut out);
            assert_eq!(
                callback.stash.capacity(),
                reserved,
                "stash reallocated while filling"
            );
        }
        assert_eq!(callback.stash.len(), 64);

        // Draining dry without an end mark re-enters `Buffering`, and the
        // rebuffer refill reuses the same reservation.
        while callback.state != OutputState::Buffering {
            callback.fill(&mut out);
        }
        assert_eq!(
            shared.counters.output_underruns.load(Ordering::Relaxed),
            1,
            "the drain starved exactly once into a rebuffer"
        );
        producer.push_slice(&audio);
        while callback.state != OutputState::Playing {
            callback.fill(&mut out);
            assert_eq!(callback.stash.capacity(), reserved);
        }
        assert_eq!(
            shared.counters.output_rebuffers.load(Ordering::Relaxed),
            1,
            "the refill counted as a rebuffer recovery"
        );
    }

    #[test]
    fn an_end_mark_for_the_wrong_generation_is_ignored() {
        let (mut producer, consumer) = HeapRb::<f32>::new(64).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 1);
        let mut out = [0.0f32; 8];

        // An end mark that claims a generation the callback has not armed is
        // inert: it neither ends current audio nor arms the new one.
        producer.push_slice(&[
            convert::generation_marker(0),
            0.5,
            convert::end_of_audio_marker(1), // for a generation not yet armed
            0.9,
        ]);
        callback.fill(&mut out);
        // The mismatched end mark does not end generation 0's audio; the
        // trailing sample still plays.
        assert_eq!(out[..4], [0.5, 0.9, 0.0, 0.0]);
        assert_eq!(shared.counters.output_ended.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn audio_after_a_normal_end_rebuffers_before_playing() {
        let (mut producer, consumer) = HeapRb::<f32>::new(32).split();
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        let mut callback = callback(&shared, consumer, 1, 2);
        let mut out = [0.0f32; 8];

        producer.push_slice(&[
            convert::generation_marker(0),
            0.5,
            convert::end_of_audio_marker(0),
        ]);
        callback.fill(&mut out);
        assert_eq!(callback.state, OutputState::Ended);

        // The next response in the same generation re-enters the startup
        // buffer instead of resuming instantly.
        producer.push_slice(&[0.7, 0.7]);
        callback.fill(&mut out);
        assert_eq!(out[..4], [0.7, 0.7, 0.0, 0.0]);
        // That burst had no end mark, so running dry afterwards starves.
        assert_eq!(shared.counters.output_underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn play_carries_partial_chunks_across_calls_and_flush_releases_them() {
        let (queue, queued) = mpsc::sync_channel(8);
        let (playback, shared) = playback(queue, 8);

        // The first delta ends mid-chunk: the head is queued complete and the
        // tail is held, not padded.
        let first: Vec<i16> = (0..VOICE_CHUNK_FRAMES as i16 + 1).collect();
        playback.play(&first).unwrap();
        let (stamp, head) = chunk(drain(&queued, &shared));
        assert_eq!(stamp, 0);
        assert_eq!(head, first[..VOICE_CHUNK_FRAMES]);
        assert!(queued.try_recv().is_err());

        // The second delta completes the held tail, then leaves its own
        // remainder pending: nothing but 480-frame chunks reach the queue.
        let second: Vec<i16> = vec![-1; VOICE_CHUNK_FRAMES];
        playback.play(&second).unwrap();
        let (stamp, joined) = chunk(drain(&queued, &shared));
        assert_eq!(stamp, 0);
        assert_eq!(joined[0], VOICE_CHUNK_FRAMES as i16);
        assert!(joined[1..].iter().all(|&sample| sample == -1));
        assert!(queued.try_recv().is_err());

        // The still-pending remainder (one -1 sample) takes the generation it
        // flushes in and is padded with silence; the end mark rides the
        // reserved slot behind it.
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        playback.flush().unwrap();
        let (stamp, tail) = chunk(drain(&queued, &shared));
        assert_eq!(stamp, 1);
        assert_eq!(tail[0], -1);
        assert!(tail[1..].iter().all(|&sample| sample == 0));
        assert_eq!(
            drain(&queued, &shared),
            PlaybackMsg::EndOfAudio(1),
            "flush always lands its end mark"
        );
        assert!(playback.flush().is_ok());
        assert_eq!(drain(&queued, &shared), PlaybackMsg::EndOfAudio(1));
        assert!(queued.try_recv().is_err());

        // Clear discards the held partial along with the queued chunks.
        playback.play(&[7]).unwrap();
        playback.clear();
        assert_eq!(shared.epoch.load(Ordering::Acquire), 2);
        playback.play(&[8; VOICE_CHUNK_FRAMES - 1]).unwrap();
        playback.flush().unwrap();
        let (_stamp, tail) = chunk(drain(&queued, &shared));
        assert!(
            tail[..VOICE_CHUNK_FRAMES - 1]
                .iter()
                .all(|&sample| sample == 8)
        );

        assert!(playback.play(&[]).is_ok());
    }

    #[test]
    fn a_faster_than_realtime_burst_never_drops_or_reorders() {
        // 10 s of uniquely numbered 20 ms chunks arriving in one burst, the
        // shape of a Realtime response streamed faster than the device drains.
        let (queue, queued) = mpsc::sync_channel(QUEUE_CHUNKS);
        let (playback, shared) = playback(queue, QUEUE_CHUNKS);
        let chunks = 500;
        for index in 0..chunks {
            playback
                .play(&vec![index as i16; VOICE_CHUNK_FRAMES])
                .unwrap();
        }
        assert_eq!(
            shared
                .counters
                .output_dropped_chunks
                .load(Ordering::Relaxed),
            0
        );
        for index in 0..chunks {
            let (stamp, chunk) = chunk(drain(&queued, &shared));
            assert_eq!(stamp, 0);
            assert!(chunk.iter().all(|&sample| sample == index as i16));
        }
    }

    #[test]
    fn only_chunks_past_the_steady_cap_are_dropped() {
        // A capacity-4 queue: the two steady slots take the first chunks, the
        // rest of the burst is counted, and FIFO order survives around the
        // drops.
        let (queue, queued) = mpsc::sync_channel(4);
        let (playback, shared) = playback(queue, 4);
        for index in 0..5 {
            playback
                .play(&vec![index as i16; VOICE_CHUNK_FRAMES])
                .unwrap();
        }
        assert_eq!(
            shared
                .counters
                .output_dropped_chunks
                .load(Ordering::Relaxed),
            3
        );
        let first = chunk(drain(&queued, &shared)).1;
        let second = chunk(drain(&queued, &shared)).1;
        assert_eq!(first[0], 0);
        assert_eq!(second[0], 1);

        // Once the queue has room the next chunk still lands in order.
        playback.play(&vec![9i16; VOICE_CHUNK_FRAMES]).unwrap();
        assert_eq!(chunk(drain(&queued, &shared)).1[0], 9);
    }

    #[test]
    fn flush_uses_the_reserved_slots_a_burst_cannot_fill() {
        // Capacity 4: `play` owns the first two slots, the tail and the end
        // mark own the last two - so a flush never parks, even mid-burst.
        let (queue, queued) = mpsc::sync_channel(4);
        let (playback, shared) = playback(queue, 4);

        playback.play(&vec![1i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.play(&vec![2i16; VOICE_CHUNK_FRAMES]).unwrap();
        // This chunk wants a reserved slot and is dropped instead.
        playback.play(&vec![3i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.play(&[5; 7]).unwrap();
        assert_eq!(
            shared
                .counters
                .output_dropped_chunks
                .load(Ordering::Relaxed),
            1
        );

        // The tail and the end mark land in the reserved slots without
        // waiting for the device to drain.
        playback.flush().unwrap();
        assert_eq!(chunk(drain(&queued, &shared)).1[0], 1);
        assert_eq!(chunk(drain(&queued, &shared)).1[0], 2);
        let (_stamp, tail) = chunk(drain(&queued, &shared));
        assert!(tail[..7].iter().all(|&sample| sample == 5));
        assert_eq!(drain(&queued, &shared), PlaybackMsg::EndOfAudio(0));
    }

    #[test]
    fn a_full_queue_drops_flush_pieces_instead_of_parking() {
        // Two steady chunks plus a flush fill all four slots; a second flush
        // finds its reserved slots taken and counts the loss immediately.
        let (queue, queued) = mpsc::sync_channel(4);
        let (playback, shared) = playback(queue, 4);

        playback.play(&vec![1i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.play(&vec![2i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.play(&[7; 3]).unwrap();
        playback.flush().unwrap();
        assert_eq!(
            shared.counters.playback_queued.load(Ordering::Relaxed),
            4,
            "tail and end mark filled the reserved slots"
        );

        playback.play(&[9; 5]).unwrap();
        playback.flush().unwrap();
        assert_eq!(
            shared
                .counters
                .output_dropped_chunks
                .load(Ordering::Relaxed),
            2,
            "tail and end mark were counted, not parked"
        );

        // The original four messages are still queued in order.
        assert_eq!(chunk(drain(&queued, &shared)).1[0], 1);
        assert_eq!(chunk(drain(&queued, &shared)).1[0], 2);
        assert!(matches!(drain(&queued, &shared), PlaybackMsg::Chunk(0, _)));
        assert_eq!(drain(&queued, &shared), PlaybackMsg::EndOfAudio(0));
    }

    #[test]
    fn flush_and_play_propagate_a_dead_worker() {
        let (queue, queued) = mpsc::sync_channel(4);
        let (playback, _shared) = playback(queue, 4);
        drop(queued);
        assert!(matches!(
            playback.play(&[1; VOICE_CHUNK_FRAMES]),
            Err(AudioError::PlaybackStopped)
        ));
        assert!(matches!(playback.flush(), Err(AudioError::PlaybackStopped)));
    }

    #[test]
    fn sync_generation_marks_the_current_epoch_without_draining_the_queue() {
        let (queue, queued) = mpsc::sync_channel(8);
        let (playback, shared) = playback(queue, 8);
        let (producer, mut consumer) = HeapRb::<f32>::new(16).split();
        let mut worker = worker(&shared, queued, producer);

        // Generation 0 arms on the first pass and pushes its marker.
        assert!(worker.sync_generation());
        assert_eq!(worker.marker, Some(0));
        assert_eq!(
            consumer.try_pop().map(convert::ring_signal),
            Some(Some(convert::RingSignal::Generation(0)))
        );

        // A clear invalidates queued generation-0 audio. The sync leaves the
        // queue alone and pushes the current epoch's marker; `handle` is what
        // skips the stale messages when their turn comes.
        playback.play(&vec![1i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.flush().unwrap();
        shared.epoch.fetch_add(1, Ordering::AcqRel);
        assert!(worker.sync_generation());
        assert_eq!(worker.marker, Some(1));
        assert!(!worker.pending_end, "a held stale end mark is dropped");
        assert_eq!(
            consumer.try_pop().map(convert::ring_signal),
            Some(Some(convert::RingSignal::Generation(1)))
        );

        // The stale chunk and its end mark are still queued; handling them
        // skips both without touching the ring.
        for _ in 0..2 {
            worker.handle(worker.queue.recv().unwrap());
        }
        assert!(worker.queue.try_recv().is_err(), "nothing else is queued");
        assert!(consumer.try_pop().is_none(), "stale audio never plays");
        assert!(!worker.pending_end, "stale end marks are ignored");

        // A no-op pass keeps the marker and leaves the resampler alone.
        assert!(worker.sync_generation());
        assert_eq!(worker.marker, Some(1));

        // Post-clear chunks stay queued for the new generation.
        playback.play(&vec![2i16; VOICE_CHUNK_FRAMES]).unwrap();
        let message = worker.queue.recv().unwrap();
        worker
            .shared
            .counters
            .playback_queued
            .fetch_sub(1, Ordering::Relaxed);
        assert_eq!(chunk(message).1[0], 2);
    }

    #[test]
    fn a_chunk_queued_after_a_clear_survives_the_next_sync() {
        // The race a draining sync loses: generation-0 audio sits queued, the
        // clear bumps to generation 1, and a fresh generation-1 chunk lands
        // before the worker's next pass. The sync must arm generation 1 and
        // leave the fresh chunk queued so its audio reaches the ring.
        let (queue, queued) = mpsc::sync_channel(8);
        let (playback, shared) = playback(queue, 8);
        let (producer, mut consumer) = HeapRb::<f32>::new(4_096).split();
        let mut worker = worker(&shared, queued, producer);

        // Queue stale generation-0 audio, then clear, then queue a fresh
        // generation-1 chunk - all before the worker runs again.
        playback.play(&vec![1i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.clear();
        playback
            .play(&vec![16_000i16; VOICE_CHUNK_FRAMES])
            .unwrap();

        assert!(worker.sync_generation());
        assert_eq!(worker.marker, Some(1));

        // FIFO handling drops the generation-0 chunk and resamples the
        // generation-1 one into the ring behind the marker.
        worker.handle(worker.queue.recv().unwrap());
        worker.handle(worker.queue.recv().unwrap());

        let mut raw = Vec::new();
        while let Some(sample) = consumer.try_pop() {
            raw.push(sample);
        }
        assert_eq!(
            convert::ring_signal(raw[0]),
            Some(convert::RingSignal::Generation(1)),
            "the generation-1 marker leads the ring"
        );
        assert!(
            raw[1..].iter().all(|sample| convert::ring_signal(*sample).is_none()),
            "no stale markers follow"
        );
        // The resampled 20 ms of generation-1 audio made it into the ring:
        // 480 frames at 24 kHz become 960 at the 48 kHz test rate. The FFT
        // resampler's group delay puts the energy mid-chunk rather than at
        // the head, so the proof is amplitude, not position.
        let audio = &raw[1..];
        assert_eq!(audio.len(), VOICE_CHUNK_FRAMES * 2);
        assert!(
            audio.iter().any(|&s| s > 0.1),
            "generation-1 audio reached the ring after the stale chunk was skipped"
        );
    }

    #[test]
    fn the_worker_orders_end_marks_behind_their_audio() {
        let (queue, queued) = mpsc::sync_channel(8);
        let (playback, shared) = playback(queue, 8);
        let (producer, mut consumer) = HeapRb::<f32>::new(4_096).split();
        let mut worker = worker(&shared, queued, producer);

        assert!(worker.sync_generation());
        playback.play(&vec![3i16; VOICE_CHUNK_FRAMES]).unwrap();
        playback.flush().unwrap();

        // The chunk resamples into the ring first; the end mark waits its
        // turn behind it even though both were already queued.
        worker.handle(worker.queue.recv().unwrap());
        worker.handle(worker.queue.recv().unwrap());
        assert!(worker.pending_end);
        assert!(worker.push_pending_end());

        let mut signals = Vec::new();
        while let Some(raw) = consumer.try_pop() {
            if let Some(signal) = convert::ring_signal(raw) {
                signals.push(signal);
            }
        }
        assert_eq!(
            signals,
            [
                convert::RingSignal::Generation(0),
                convert::RingSignal::EndOfAudio(0)
            ],
            "generation marker first, end mark after the audio"
        );
        assert!(!worker.pending_end);
    }

    #[test]
    fn a_ring_overshoot_drops_and_counts_the_excess() {
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback(queue, 8);
        // A ring smaller than one resampled chunk: 16 frames against 960.
        let (producer, _consumer) = HeapRb::<f32>::new(16).split();
        let resampler = resample::from_voice(48_000).unwrap();
        let mut worker = PlaybackWorker {
            shared: Arc::clone(&shared),
            queue: _queued,
            producer,
            output: vec![0.0; resampler.output_frames_max()],
            resampler,
            input: vec![0.0; VOICE_CHUNK_FRAMES],
            marker: Some(0),
            pending_end: false,
        };
        // A stopped worker does not wait out the full ring.
        shared.stop.store(true, Ordering::Release);

        worker.resample_and_push(&vec![1i16; VOICE_CHUNK_FRAMES]);
        assert_eq!(
            shared
                .counters
                .output_dropped_samples
                .load(Ordering::Relaxed),
            960 - 16,
            "only what fit the ring was kept"
        );
    }

    #[test]
    fn output_gain_scales_samples_and_the_limiter_folds_peaks() {
        let (queue, _queued) = mpsc::sync_channel(8);
        let (_playback, shared) = playback_with(
            queue,
            8,
            PlaybackTuning {
                gain: 2.0,
                ..tuning()
            },
        );
        let (producer, _consumer) = HeapRb::<f32>::new(16).split();
        let resampler = resample::from_voice(48_000).unwrap();
        let mut worker = PlaybackWorker {
            shared: Arc::clone(&shared),
            queue: _queued,
            producer,
            output: vec![0.0; resampler.output_frames_max()],
            resampler,
            input: vec![0.0; VOICE_CHUNK_FRAMES],
            marker: Some(0),
            pending_end: false,
        };

        worker.output[..4].copy_from_slice(&[0.4, -0.4, 0.6, -0.55]);
        worker.shape_output(4);

        // Quiet samples see plain gain; loud ones fold under the ceiling.
        assert_eq!(worker.output[..2], [0.8, -0.8]);
        assert!(worker.output[2] > 0.95 && worker.output[2] < 1.0);
        assert!(worker.output[3] < -0.95 && worker.output[3] > -1.0);
        assert_eq!(
            shared
                .counters
                .output_limited_samples
                .load(Ordering::Relaxed),
            2
        );
    }
}

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;

pub const RATE: u32 = 24_000;
const PACKET: usize = 480; // 20 ms, paced by the hardware clock.
const MAX_PLAYBACK: usize = RATE as usize * 2;

/// Streaming linear interpolation with a continuous phase across callbacks.
/// Input is downmixed before resampling; output is duplicated across channels.
struct Resampler {
    step: f64,
    next: f64,
    index: u64,
    previous: f32,
}
impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            next: 0.0,
            index: 0,
            previous: 0.0,
        }
    }
    fn push(&mut self, value: f32, mut output: impl FnMut(f32)) {
        let index = self.index as f64;
        while self.next <= index {
            let alpha = (self.next - (index - 1.0)).clamp(0.0, 1.0) as f32;
            output(self.previous + (value - self.previous) * alpha);
            self.next += self.step;
        }
        self.previous = value;
        self.index += 1;
    }
}

pub(crate) struct Audio {
    _input: cpal::Stream,
    _output: cpal::Stream,
    pub failed: Arc<AtomicBool>,
    playback: Arc<Mutex<VecDeque<f32>>>,
    resampler: Resampler,
    output_rate: u32,
}
impl Audio {
    pub fn open(
        muted: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<(Self, mpsc::Receiver<Vec<u8>>)> {
        let host = cpal::default_host();
        let input = host
            .default_input_device()
            .context("No microphone is available")?;
        let output = host
            .default_output_device()
            .context("No speaker is available")?;
        let input_config = input
            .default_input_config()
            .context("Cannot open microphone; check microphone permission in system settings")?;
        let output_config = output
            .default_output_config()
            .context("Cannot open audio output")?;
        let failed = Arc::new(AtomicBool::new(false));
        let playback = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_PLAYBACK)));
        let (tx, rx) = mpsc::channel(25); // Fail on >500ms backlog instead of replaying stale input.
        let input_stream = match input_config.sample_format() {
            cpal::SampleFormat::F32 => capture::<f32>(
                &input,
                &input_config.into(),
                tx,
                muted.clone(),
                failed.clone(),
            )?,
            cpal::SampleFormat::I16 => capture::<i16>(
                &input,
                &input_config.into(),
                tx,
                muted.clone(),
                failed.clone(),
            )?,
            cpal::SampleFormat::U16 => capture::<u16>(
                &input,
                &input_config.into(),
                tx,
                muted.clone(),
                failed.clone(),
            )?,
            format => bail!("Unsupported microphone sample format: {format}"),
        };
        let output_rate = output_config.sample_rate().0;
        let output_stream = match output_config.sample_format() {
            cpal::SampleFormat::F32 => speaker::<f32>(
                &output,
                &output_config.into(),
                playback.clone(),
                failed.clone(),
                cancelled.clone(),
            )?,
            cpal::SampleFormat::I16 => speaker::<i16>(
                &output,
                &output_config.into(),
                playback.clone(),
                failed.clone(),
                cancelled.clone(),
            )?,
            cpal::SampleFormat::U16 => speaker::<u16>(
                &output,
                &output_config.into(),
                playback.clone(),
                failed.clone(),
                cancelled.clone(),
            )?,
            format => bail!("Unsupported speaker sample format: {format}"),
        };
        output_stream.play().context("Cannot start speaker")?;
        input_stream.play().context("Cannot start microphone")?;
        Ok((
            Self {
                _input: input_stream,
                _output: output_stream,
                failed,
                playback,
                resampler: Resampler::new(RATE, output_rate),
                output_rate,
            },
            rx,
        ))
    }
    pub fn play(&mut self, bytes: &[u8]) -> Result<()> {
        if !bytes.len().is_multiple_of(2) {
            bail!("Invalid PCM audio from voice service");
        }
        let mut queue = self
            .playback
            .lock()
            .map_err(|_| anyhow::anyhow!("Audio output failed"))?;
        let limit = self.output_rate as usize * 2;
        let projected = bytes.len() / 2 * self.output_rate as usize / RATE as usize;
        if queue.len() + projected > limit {
            bail!("Voice playback fell behind. Reconnect the call.");
        }
        for sample in bytes.as_chunks::<2>().0 {
            let value = i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32768.0;
            self.resampler.push(value, |v| queue.push_back(v));
        }
        Ok(())
    }
}
fn capture<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Vec<u8>>,
    muted: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(config.sample_rate.0, RATE);
    let mut packet = Vec::with_capacity(PACKET * 2);
    let error = failed.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            let is_muted = muted.load(Ordering::Relaxed);
            for frame in data.chunks_exact(channels) {
                let value = if is_muted {
                    0.0
                } else {
                    frame
                        .iter()
                        .map(|s| <f32 as cpal::Sample>::from_sample(*s))
                        .sum::<f32>()
                        / channels as f32
                };
                resampler.push(value, |v| {
                    let sample = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
                    packet.extend_from_slice(&sample.to_le_bytes());
                    if packet.len() == PACKET * 2 {
                        let full = std::mem::replace(&mut packet, Vec::with_capacity(PACKET * 2));
                        if tx.try_send(full).is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                    }
                });
            }
        },
        move |_| error.store(true, Ordering::Relaxed),
        None,
    )?)
}
fn speaker<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    playback: Arc<Mutex<VecDeque<f32>>>,
    failed: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            if cancelled.load(Ordering::SeqCst) {
                data.fill(T::from_sample(0.0));
                return;
            }
            // Never block the hardware callback on the websocket's producer.
            let mut queue = playback.try_lock().ok();
            for frame in data.chunks_exact_mut(channels) {
                let value = queue.as_mut().and_then(|q| q.pop_front()).unwrap_or(0.0);
                frame.fill(T::from_sample(value));
            }
        },
        move |_| failed.store(true, Ordering::Relaxed),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampling_preserves_duration_at_common_hardware_rates() {
        for rate in [16_000, 24_000, 44_100, 48_000, 96_000] {
            let mut r = Resampler::new(rate, RATE);
            let mut samples = Vec::new();
            for _ in 0..rate {
                r.push(0.25, |s| samples.push(s));
            }
            assert!((samples.len() as i64 - RATE as i64).abs() <= 2);
            assert!(samples.iter().all(|s| (*s - 0.25).abs() < 0.0001));
        }
    }
    #[test]
    fn resampling_interpolates_across_packet_boundaries() {
        let mut r = Resampler::new(24_000, 48_000);
        let mut samples = Vec::new();
        for s in [0.0, 1.0, 0.0] {
            r.push(s, |s| samples.push(s));
        }
        assert_eq!(samples, [0.0, 0.5, 1.0, 0.5, 0.0]);
    }
}

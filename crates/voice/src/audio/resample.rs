//! Rubato resampler setup and the adapter plumbing for one mono chunk.
//!
//! Both directions chunk on the 24 kHz side: the capture resampler is fixed to
//! one 20 ms output chunk per call, the playback resampler to one 20 ms input
//! chunk. `libsamplerate`-style variable ratios are not needed, so the FFT
//! resampler keeps this small and deterministic per device rate.

use crate::audio::{VOICE_CHUNK_FRAMES, VOICE_SAMPLE_RATE, error::AudioError};
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::direct::InterleavedSlice};

/// Resampler from the capture device rate to 24 kHz, fixed at one chunk of
/// [`VOICE_CHUNK_FRAMES`] output frames per call.
pub(crate) fn to_voice(device_rate: u32) -> Result<Fft<f32>, AudioError> {
    Fft::new(
        device_rate as usize,
        VOICE_SAMPLE_RATE as usize,
        VOICE_CHUNK_FRAMES,
        1,
        FixedSync::Output,
    )
    .map_err(|error| AudioError::Resampler(error.to_string()))
}

/// Resampler from 24 kHz to the playback device rate, fixed at one chunk of
/// [`VOICE_CHUNK_FRAMES`] input frames per call.
pub(crate) fn from_voice(device_rate: u32) -> Result<Fft<f32>, AudioError> {
    Fft::new(
        VOICE_SAMPLE_RATE as usize,
        device_rate as usize,
        VOICE_CHUNK_FRAMES,
        1,
        FixedSync::Input,
    )
    .map_err(|error| AudioError::Resampler(error.to_string()))
}

/// Resamples one mono chunk into `output` and returns the number of frames
/// written. The caller sizes `input` to the resampler's next input length and
/// `output` to `output_frames_max`.
pub(crate) fn resample_chunk(
    resampler: &mut Fft<f32>,
    input: &[f32],
    output: &mut [f32],
) -> Result<usize, AudioError> {
    let input = InterleavedSlice::new(input, 1, input.len())
        .map_err(|error| AudioError::Resampler(error.to_string()))?;
    let mut output = InterleavedSlice::new_mut(output, 1, output.len())
        .map_err(|error| AudioError::Resampler(error.to_string()))?;
    let (_, written) = resampler
        .process_into_buffer(&input, &mut output, None)
        .map_err(|error| AudioError::Resampler(error.to_string()))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, frames: usize, hz: f32) -> Vec<f32> {
        (0..frames)
            .map(|frame| (2.0 * std::f32::consts::PI * hz * frame as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn capture_resampler_emits_exact_voice_chunks() {
        let mut resampler = to_voice(48_000).unwrap();
        assert_eq!(resampler.output_frames_next(), VOICE_CHUNK_FRAMES);

        let mut out = vec![0.0f32; VOICE_CHUNK_FRAMES];
        let mut peak = 0.0f32;
        for _ in 0..4 {
            let input_frames = resampler.input_frames_next();
            let input = sine(48_000, input_frames, 440.0);
            let written = resample_chunk(&mut resampler, &input, &mut out).unwrap();
            assert_eq!(written, VOICE_CHUNK_FRAMES);
            peak = out.iter().fold(peak, |peak, sample| peak.max(sample.abs()));
        }
        // The FFT delay eats the first chunk; a 440 Hz tone must survive.
        assert!(peak > 0.3, "resampled peak was {peak}");
    }

    #[test]
    fn playback_resampler_tracks_the_device_rate() {
        // 20 ms of 24 kHz input becomes 20 ms at the device rate: exactly two
        // chunks at 48 kHz, and 882 frames (rounded) at 44.1 kHz.
        for (device_rate, expected) in [(48_000u32, VOICE_CHUNK_FRAMES * 2), (44_100, 882)] {
            let mut resampler = from_voice(device_rate).unwrap();
            assert_eq!(resampler.input_frames_next(), VOICE_CHUNK_FRAMES);

            let input = sine(VOICE_SAMPLE_RATE, VOICE_CHUNK_FRAMES, 440.0);
            let mut out = vec![0.0f32; resampler.output_frames_max()];
            let written = resample_chunk(&mut resampler, &input, &mut out).unwrap();
            assert_eq!(written, expected, "{device_rate} Hz chunk");
        }
    }
}

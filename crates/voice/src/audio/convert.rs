//! Pure sample math shared by the capture and playback paths.
//!
//! Nothing here touches a device, a ring, or a thread, so the behaviour the
//! audio callbacks depend on can be tested directly.

/// How much of the held level survives one callback without a new peak. The
/// capture callback runs every few milliseconds, so a meter falls over roughly
/// 100 ms instead of flickering per buffer.
pub(crate) const LEVEL_DECAY: f32 = 0.85;

/// Bit pattern of the lowest quiet NaN, used as the base of every playback
/// ring signal.
const MARKER_BITS: u32 = 0x7fc0_0000;

/// Payload bit that distinguishes a ring signal's kind. Clear marks a
/// generation marker; set marks the end of one response's audio.
const SIGNAL_TAG: u32 = 0x0020_0000;

/// Payload bits of a generation marker, holding the generation.
const MARKER_MASK: u32 = 0x001f_ffff;

/// Converts a float sample on the `[-1.0, 1.0]` scale to PCM16, clamping
/// out-of-range and non-finite input to the representable range. Non-finite
/// input maps to silence rather than to a loud click.
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    let scaled = sample * i16::MAX as f32;
    scaled.clamp(i16::MIN as f32, i16::MAX as f32).round() as i16
}

/// Converts PCM16 to the float scale used inside the audio pipeline.
pub(crate) fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32_768.0
}

/// Peak-hold with decay: a new peak shows immediately, otherwise the previous
/// value fades by [`LEVEL_DECAY`] per call.
pub(crate) fn decayed_level(previous: f32, peak: f32) -> f32 {
    if peak >= previous {
        peak
    } else {
        (previous * LEVEL_DECAY).max(peak)
    }
}

/// The comparable slot of a playback generation: markers carry only the low
/// bits of the counter, so every comparison goes through this mask.
pub(crate) fn generation_slot(epoch: u64) -> u64 {
    epoch & u64::from(MARKER_MASK)
}

/// A control value pushed through the playback ring instead of a sample.
/// Neither can be produced by the i16 -> f32 conversion or by the resampler,
/// so [`ring_signal`] recognises them unambiguously. Both kinds carry the
/// generation they were pushed for, so a mark from before a clear can never
/// arm or end the new generation's audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingSignal {
    /// First frame of generation `epoch`: everything before it in the ring is
    /// stale audio from before a clear.
    Generation(u64),
    /// The response's final chunk of generation `epoch` was pushed: audio
    /// that follows belongs to a later response and must re-arm the startup
    /// buffer, not be treated as an underrun while the ring runs dry.
    EndOfAudio(u64),
}

/// The playback ring value that marks the first frame of generation `epoch`.
pub(crate) fn generation_marker(epoch: u64) -> f32 {
    f32::from_bits(MARKER_BITS | (epoch as u32 & MARKER_MASK))
}

/// The playback ring value that marks the end of a response's audio in
/// generation `epoch`.
pub(crate) fn end_of_audio_marker(epoch: u64) -> f32 {
    f32::from_bits(MARKER_BITS | SIGNAL_TAG | (epoch as u32 & MARKER_MASK))
}

/// The signal carried by a playback marker, or `None` for a real sample.
pub(crate) fn ring_signal(sample: f32) -> Option<RingSignal> {
    if !sample.is_nan() {
        return None;
    }
    let payload = sample.to_bits() & (SIGNAL_TAG | MARKER_MASK);
    let epoch = u64::from(payload & MARKER_MASK);
    if payload & SIGNAL_TAG != 0 {
        Some(RingSignal::EndOfAudio(epoch))
    } else {
        Some(RingSignal::Generation(epoch))
    }
}

/// Output gain plus a soft limiter: the gain scales the sample, then a tanh
/// knee above `ceiling` folds the overshoot into the remaining headroom toward
/// but never past 1.0, so boosted audio saturates smoothly instead of clipping.
/// A `ceiling` of 1.0 has no headroom and degenerates to a hard clamp. Whatever
/// the tuning, the result is always finite and inside `[-1.0, 1.0]`: resampler
/// overshoot and gain cannot push an out-of-range (or non-finite) sample into
/// the ring or the device conversion.
pub(crate) fn limit_output(sample: f32, gain: f32, ceiling: f32) -> f32 {
    let scaled = sample * gain;
    let abs = scaled.abs();
    if !abs.is_finite() {
        // Non-finite input becomes silence rather than a click or a NaN that
        // would collide with the ring's control marks.
        return 0.0;
    }
    if abs <= ceiling {
        return scaled.clamp(-1.0, 1.0);
    }
    let headroom = 1.0 - ceiling;
    if headroom <= 0.0 {
        return scaled.clamp(-1.0, 1.0);
    }
    scaled.signum() * (ceiling + headroom * ((abs - ceiling) / headroom).tanh())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_and_pcm16_round_trip_on_scale() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -i16::MAX);
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), i16::MIN);
        // Non-finite input becomes silence rather than a loud click.
        assert_eq!(f32_to_i16(f32::NAN), 0);
        assert_eq!(f32_to_i16(f32::INFINITY), i16::MAX);

        assert_eq!(i16_to_f32(-32_768), -1.0);
        for sample in [-32_768i16, -16_384, -1, 0, 1, 16_384, 32_767] {
            let round_trip = f32_to_i16(i16_to_f32(sample));
            assert!((round_trip - sample).abs() <= 2, "{sample} -> {round_trip}");
        }
    }

    #[test]
    fn level_holds_peaks_and_decays() {
        assert_eq!(decayed_level(0.0, 0.8), 0.8);
        assert_eq!(decayed_level(0.8, 1.0), 1.0);
        assert!((decayed_level(1.0, 0.0) - LEVEL_DECAY).abs() < f32::EPSILON);
        // The held level never falls below the current peak.
        assert_eq!(decayed_level(0.1, 0.5), 0.5);
    }

    #[test]
    fn markers_round_trip_and_leave_real_samples_alone() {
        for epoch in [0u64, 1, 42, u64::MAX] {
            assert_eq!(
                ring_signal(generation_marker(epoch)),
                Some(RingSignal::Generation(generation_slot(epoch)))
            );
            assert_eq!(
                ring_signal(end_of_audio_marker(epoch)),
                Some(RingSignal::EndOfAudio(generation_slot(epoch)))
            );
        }
        for sample in [0.0f32, -1.0, 1.0, f32::MIN, -0.5] {
            assert_eq!(ring_signal(sample), None);
        }
        // A generation marker never collides with the end-of-audio tag.
        assert_eq!(
            ring_signal(generation_marker(u64::from(MARKER_MASK))),
            Some(RingSignal::Generation(u64::from(MARKER_MASK)))
        );
    }

    #[test]
    fn the_output_limiter_passes_quiet_samples_and_soft_clips_loud_ones() {
        // Below the ceiling the limiter is a plain gain stage.
        assert_eq!(limit_output(0.25, 1.0, 0.95), 0.25);
        assert_eq!(limit_output(0.4, 2.0, 0.95), 0.8);
        // Exactly at the ceiling the knee has not engaged yet.
        assert_eq!(limit_output(0.95, 1.0, 0.95), 0.95);

        // Above it, the knee compresses toward but never past 1.0, in sign.
        for (sample, gain) in [(0.98, 1.0), (0.6, 4.0), (1.5, 2.0), (100.0, 1.0)] {
            for sign in [1.0f32, -1.0] {
                let limited = limit_output(sign * sample, gain, 0.95);
                assert!(limited.abs() <= 1.0, "{sample} * {gain} -> {limited}");
                assert!(limited.abs() > 0.95, "{sample} * {gain} -> {limited}");
                assert_eq!(limited.signum(), sign);
            }
        }
        // The knee is monotonic: more in, more out.
        assert!(limit_output(0.97, 1.0, 0.95) < limit_output(0.99, 1.0, 0.95));
        // A ceiling of 1.0 has no headroom: the knee degenerates to a hard
        // clamp, which still tames resampler and gain overshoot.
        assert_eq!(limit_output(1.02, 1.0, 1.0), 1.0);
        assert_eq!(limit_output(-3.0, 1.0, 1.0), -1.0);
        // Whatever the tuning, the output is always finite and in range:
        // non-finite input becomes silence, never a marker payload the ring
        // would misread or a click in the device conversion.
        assert_eq!(limit_output(f32::NAN, 1.0, 0.95), 0.0);
        assert_eq!(limit_output(f32::INFINITY, 1.0, 0.95), 0.0);
        assert_eq!(limit_output(f32::NEG_INFINITY, 1.0, 0.95), 0.0);
    }
}

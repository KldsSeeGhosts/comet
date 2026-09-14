//! Pure sample math shared by the capture and playback paths.
//!
//! Nothing here touches a device, a ring, or a thread, so the behaviour the
//! audio callbacks depend on can be tested directly.

/// How much of the held level survives one callback without a new peak. The
/// capture callback runs every few milliseconds, so a meter falls over roughly
/// 100 ms instead of flickering per buffer.
pub(crate) const LEVEL_DECAY: f32 = 0.85;

/// Bit pattern of the lowest quiet NaN, used as the base of a playback
/// generation marker.
const MARKER_BITS: u32 = 0x7fc0_0000;

/// Payload bits of a playback generation marker, holding the generation.
const MARKER_MASK: u32 = 0x003f_ffff;

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

/// The playback ring value that marks the first frame of generation `epoch`.
///
/// A NaN payload cannot be produced by the i16 -> f32 conversion or by the
/// resampler, so [`marker_epoch`] recognises it unambiguously.
pub(crate) fn generation_marker(epoch: u64) -> f32 {
    f32::from_bits(MARKER_BITS | (epoch as u32 & MARKER_MASK))
}

/// The generation carried by a playback marker, or `None` for a real sample.
pub(crate) fn marker_epoch(sample: f32) -> Option<u64> {
    sample
        .is_nan()
        .then(|| u64::from(sample.to_bits() & MARKER_MASK))
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
                marker_epoch(generation_marker(epoch)),
                Some(generation_slot(epoch))
            );
        }
        for sample in [0.0f32, -1.0, 1.0, f32::MIN, -0.5] {
            assert_eq!(marker_epoch(sample), None);
        }
    }
}

//! PCM16 <-> base64 codec for the Realtime audio events.
//!
//! Codec only: no capture, playback, resampling, or device handling lives here.
//! The protocol carries 24 kHz mono signed 16-bit little-endian samples.

use crate::error::VoiceError;
use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Encodes interleaved PCM16 samples for `input_audio_buffer.append`.
pub fn pcm16_to_base64(samples: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    STANDARD.encode(bytes)
}

/// Decodes a base64 `output_audio.delta` payload into PCM16 samples.
pub fn base64_to_pcm16(encoded: &str) -> Result<Vec<i16>, VoiceError> {
    let bytes = STANDARD.decode(encoded)?;
    if bytes.len() % 2 != 0 {
        return Err(VoiceError::OddPcm16);
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm16_roundtrip_preserves_samples() {
        let samples = [-32768i16, -1, 0, 1, 32767];
        let encoded = pcm16_to_base64(&samples);
        assert_eq!(base64_to_pcm16(&encoded).unwrap(), samples);
        assert_eq!(pcm16_to_base64(&[]), "");
    }

    #[test]
    fn odd_byte_payloads_are_rejected() {
        let encoded = STANDARD.encode([0u8; 3]);
        assert!(matches!(
            base64_to_pcm16(&encoded),
            Err(VoiceError::OddPcm16)
        ));
        assert!(matches!(
            base64_to_pcm16("not base64!"),
            Err(VoiceError::DecodeAudio(_))
        ));
    }
}

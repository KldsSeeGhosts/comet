//! Device and worker failures from the local audio layer.

use thiserror::Error;

/// The portable category an [`AudioError`] falls into, so a UI can react
/// without matching on message strings.
///
/// The mapping is only as precise as the backend underneath: CPAL 0.18
/// reports a structured [`cpal::ErrorKind`], but several hosts still collapse
/// distinct failures into [`cpal::ErrorKind::BackendError`] or
/// [`cpal::ErrorKind::Other`] - on ALSA, for example, a busy device and a
/// vanished card can both surface as [`AudioErrorKind::BusyOrUnavailable`]
/// instead of a dedicated variant. Callers should treat the classification as
/// a best-effort hint and keep the rendered message for detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioErrorKind {
    /// No usable input or output device exists: an empty host device list,
    /// no system default, or a host that reports the device is gone.
    NoDevice,
    /// The OS or audio subsystem refused access - a microphone privacy
    /// toggle, a missing `audio` group, a file permission.
    PermissionDenied,
    /// The device or host cannot serve the request right now: busy,
    /// unplugged, the audio daemon is down, or the backend reported a
    /// failure CPAL could not classify further.
    BusyOrUnavailable,
    /// The negotiated stream configuration or sample format is not
    /// supported by the device.
    UnsupportedFormat,
    /// The stream or its worker failed after, or while, being set up:
    /// resampler construction, worker spawn, a stopped pipeline, or an
    /// invalid engine configuration.
    StreamFailed,
}

/// Failure to open or run the default audio devices.
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no default audio input device")]
    NoDefaultInput,
    #[error("no default audio output device")]
    NoDefaultOutput,
    #[error("audio device error: {0}")]
    Device(#[from] cpal::Error),
    #[error("unsupported device sample format `{0}`")]
    UnsupportedFormat(cpal::SampleFormat),
    #[error("resampler failed: {0}")]
    Resampler(String),
    #[error("failed to start the {name} audio worker: {error}")]
    Worker { name: &'static str, error: String },
    #[error("audio playback is not running")]
    PlaybackStopped,
    #[error("invalid audio engine config: {0}")]
    InvalidConfig(&'static str),
}

impl AudioError {
    /// The portable [`AudioErrorKind`] for this failure.
    ///
    /// CPAL errors classify through their structured [`cpal::ErrorKind`];
    /// backend failures CPAL leaves as `BackendError`/`Other` fold into
    /// [`AudioErrorKind::BusyOrUnavailable`] rather than guessing from the
    /// message text.
    pub fn classify(&self) -> AudioErrorKind {
        use cpal::ErrorKind as Cpal;
        match self {
            Self::NoDefaultInput | Self::NoDefaultOutput => AudioErrorKind::NoDevice,
            Self::Device(error) => match error.kind() {
                Cpal::DeviceNotAvailable | Cpal::HostUnavailable => AudioErrorKind::NoDevice,
                Cpal::PermissionDenied => AudioErrorKind::PermissionDenied,
                Cpal::UnsupportedConfig | Cpal::UnsupportedOperation => {
                    AudioErrorKind::UnsupportedFormat
                }
                // DeviceBusy, BackendError, Other, Xrun, DeviceChanged,
                // RealtimeDenied, ResourceExhausted, StreamInvalidated,
                // InvalidInput, and future variants: unavailable without a
                // more precise portable claim.
                _ => AudioErrorKind::BusyOrUnavailable,
            },
            Self::UnsupportedFormat(_) => AudioErrorKind::UnsupportedFormat,
            Self::Resampler(_)
            | Self::Worker { .. }
            | Self::PlaybackStopped
            | Self::InvalidConfig(_) => AudioErrorKind::StreamFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::ErrorKind as Cpal;

    fn device(kind: Cpal) -> AudioError {
        AudioError::Device(cpal::Error::new(kind))
    }

    #[test]
    fn absence_and_host_failures_are_no_device() {
        assert_eq!(
            AudioError::NoDefaultInput.classify(),
            AudioErrorKind::NoDevice
        );
        assert_eq!(
            AudioError::NoDefaultOutput.classify(),
            AudioErrorKind::NoDevice
        );
        assert_eq!(
            device(Cpal::DeviceNotAvailable).classify(),
            AudioErrorKind::NoDevice
        );
        assert_eq!(
            device(Cpal::HostUnavailable).classify(),
            AudioErrorKind::NoDevice
        );
    }

    #[test]
    fn permission_denied_keeps_its_own_category() {
        assert_eq!(
            device(Cpal::PermissionDenied).classify(),
            AudioErrorKind::PermissionDenied
        );
    }

    #[test]
    fn unsupported_configs_and_formats_classify_as_unsupported_format() {
        assert_eq!(
            device(Cpal::UnsupportedConfig).classify(),
            AudioErrorKind::UnsupportedFormat
        );
        assert_eq!(
            device(Cpal::UnsupportedOperation).classify(),
            AudioErrorKind::UnsupportedFormat
        );
        assert_eq!(
            AudioError::UnsupportedFormat(cpal::SampleFormat::I8).classify(),
            AudioErrorKind::UnsupportedFormat
        );
    }

    #[test]
    fn busy_and_unclassified_backend_errors_share_one_bucket() {
        // CPAL collapses these differently per host; the portable surface
        // stays honest and groups them instead of guessing from strings.
        for kind in [
            Cpal::DeviceBusy,
            Cpal::BackendError,
            Cpal::Other,
            Cpal::Xrun,
            Cpal::DeviceChanged,
            Cpal::ResourceExhausted,
            Cpal::RealtimeDenied,
            Cpal::StreamInvalidated,
            Cpal::InvalidInput,
        ] {
            assert_eq!(
                device(kind).classify(),
                AudioErrorKind::BusyOrUnavailable,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn pipeline_failures_classify_as_stream_failed() {
        assert_eq!(
            AudioError::Resampler("bad ratio".to_owned()).classify(),
            AudioErrorKind::StreamFailed
        );
        assert_eq!(
            AudioError::Worker {
                name: "capture",
                error: "spawn failed".to_owned(),
            }
            .classify(),
            AudioErrorKind::StreamFailed
        );
        assert_eq!(
            AudioError::PlaybackStopped.classify(),
            AudioErrorKind::StreamFailed
        );
        assert_eq!(
            AudioError::InvalidConfig("nope").classify(),
            AudioErrorKind::StreamFailed
        );
    }
}

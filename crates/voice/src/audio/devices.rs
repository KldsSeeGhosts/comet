//! Input-device enumeration, saved preferences, and resolution.
//!
//! The microphone is the only user-selectable device; speaker output always
//! uses the system default (routing a specific sink adds little over the OS
//! default and proved unreliable in the reference implementation). A saved
//! microphone is resolved against the devices actually present without ever
//! throwing: an exact CPAL [`DeviceId`] match first, then the remembered label
//! so a restart that rotates ids still finds it, then the system default.
//!
//! Enumeration only lists what CPAL reports - it queries device ids and names
//! and never opens a stream, so it is safe to call before the user grants mic
//! access and cheap enough to refresh a settings picker.

use crate::audio::error::AudioError;
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// One enumerable input device: a stable id string plus a display label.
///
/// `id` is the CPAL [`DeviceId`] in its `Display`/`FromStr` form
/// (`"host:device"`, e.g. `"alsa:hw:0,0"`); it is empty when the backend could
/// not produce one. `label` is the human-readable device name and may be
/// shared by several devices, so it is only a fallback match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceInfo {
    pub id: String,
    pub label: String,
}

/// The saved microphone choice. Both fields empty means "system default".
///
/// The label is stored alongside the id so a device whose id rotated across a
/// restart - a re-enumerated USB mic, a rebooted audio server - can still be
/// re-matched by name before falling back to the system default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevicePreferences {
    #[serde(default)]
    pub input_device_id: String,
    #[serde(default)]
    pub input_label: String,
}

/// How a saved input device was resolved against the devices present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMatch {
    /// Exact CPAL id match.
    Id,
    /// The id was gone or unparseable but the remembered label matched.
    Label,
    /// No saved choice, or neither id nor label resolved: the system default.
    Default,
}

/// The resolution of a saved preference against an enumerated device list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputResolution {
    /// Index into the enumerated list, or `None` for the system default.
    pub index: Option<usize>,
    pub matched_by: DeviceMatch,
}

/// Resolves a saved preference against an enumerated list, mirroring the
/// reference `resolveDevice`: an id present in the list wins; a remembered
/// label recovers a device whose id rotated - and, on its own, names a mic
/// when no id was ever saved; anything else falls back to the system default.
/// Pure and total - the fallible device lookups live in
/// [`resolve_input_device`].
pub fn resolve_input(
    devices: &[InputDeviceInfo],
    preferences: &AudioDevicePreferences,
) -> InputResolution {
    if !preferences.input_device_id.is_empty()
        && let Some(index) = devices
            .iter()
            .position(|device| !device.id.is_empty() && device.id == preferences.input_device_id)
    {
        return InputResolution {
            index: Some(index),
            matched_by: DeviceMatch::Id,
        };
    }
    if !preferences.input_label.is_empty()
        && let Some(index) = devices
            .iter()
            .position(|device| !device.label.is_empty() && device.label == preferences.input_label)
    {
        return InputResolution {
            index: Some(index),
            matched_by: DeviceMatch::Label,
        };
    }
    InputResolution {
        index: None,
        matched_by: DeviceMatch::Default,
    }
}

/// Lists the host's input devices without opening a stream on any of them.
pub(crate) fn enumerate_input_devices(
    host: &cpal::Host,
) -> Result<Vec<InputDeviceInfo>, AudioError> {
    let mut devices = Vec::new();
    for device in host.input_devices()? {
        devices.push(InputDeviceInfo {
            id: device.id().map(|id| id.to_string()).unwrap_or_default(),
            label: device.to_string(),
        });
    }
    Ok(devices)
}

/// Resolves the saved preference to a live CPAL device: exact id first, then
/// the remembered label across currently present inputs, then the host's
/// default input. Fails only when no input device exists at all. Every match
/// is taken from [`HostTrait::input_devices`], so an output-only device can
/// never satisfy even an exact saved id.
pub(crate) fn resolve_input_device(
    host: &cpal::Host,
    preferences: &AudioDevicePreferences,
) -> Result<(cpal::Device, DeviceMatch), AudioError> {
    let default = || {
        host.default_input_device()
            .ok_or(AudioError::NoDefaultInput)
            .map(|device| (device, DeviceMatch::Default))
    };
    if preferences.input_device_id.is_empty() && preferences.input_label.is_empty() {
        return default();
    }
    // Every match comes from `input_devices`, never `device_by_id`: a saved id
    // that names an output-only device must not resolve to it.
    let inputs: Vec<cpal::Device> = match host.input_devices() {
        Ok(devices) => devices.collect(),
        Err(_) => Vec::new(),
    };
    if !preferences.input_device_id.is_empty()
        && let Ok(id) = cpal::DeviceId::from_str(&preferences.input_device_id)
        && let Some(index) = inputs
            .iter()
            .position(|device| device.id().ok().as_ref() == Some(&id))
    {
        return Ok((inputs[index].clone(), DeviceMatch::Id));
    }
    if !preferences.input_label.is_empty()
        && let Some(index) = inputs
            .iter()
            .position(|device| device.to_string() == preferences.input_label)
    {
        return Ok((inputs[index].clone(), DeviceMatch::Label));
    }
    default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, label: &str) -> InputDeviceInfo {
        InputDeviceInfo {
            id: id.to_owned(),
            label: label.to_owned(),
        }
    }

    fn prefs(id: &str, label: &str) -> AudioDevicePreferences {
        AudioDevicePreferences {
            input_device_id: id.to_owned(),
            input_label: label.to_owned(),
        }
    }

    #[test]
    fn an_empty_preference_resolves_to_the_system_default() {
        let devices = [device("alsa:hw:0,0", "Built-in Mic")];
        assert_eq!(
            resolve_input(&devices, &AudioDevicePreferences::default()),
            InputResolution {
                index: None,
                matched_by: DeviceMatch::Default,
            }
        );
        // An empty list resolves the same way: default is not a list entry.
        assert_eq!(
            resolve_input(&[], &prefs("", "")),
            InputResolution {
                index: None,
                matched_by: DeviceMatch::Default,
            }
        );
    }

    #[test]
    fn a_saved_id_wins_over_every_other_match() {
        let devices = [
            device("alsa:hw:0,0", "USB Mic"),
            device("alsa:hw:1,0", "Saved Mic"),
        ];
        // The id matches even when the remembered label points elsewhere.
        assert_eq!(
            resolve_input(&devices, &prefs("alsa:hw:1,0", "USB Mic")),
            InputResolution {
                index: Some(1),
                matched_by: DeviceMatch::Id,
            }
        );
    }

    #[test]
    fn a_rotated_id_is_recovered_by_the_remembered_label() {
        // The saved id is gone - re-enumerated across a restart - but the
        // label still identifies the same microphone.
        let devices = [
            device("alsa:hw:3,0", "Saved Mic"),
            device("alsa:hw:0,0", "Built-in Mic"),
        ];
        assert_eq!(
            resolve_input(&devices, &prefs("alsa:hw:1,0", "Saved Mic")),
            InputResolution {
                index: Some(0),
                matched_by: DeviceMatch::Label,
            }
        );
    }

    #[test]
    fn a_gone_device_and_a_missing_label_both_fall_back_to_default() {
        let devices = [device("alsa:hw:0,0", "Built-in Mic")];
        assert_eq!(
            resolve_input(&devices, &prefs("alsa:hw:9,9", "Gone Mic")),
            InputResolution {
                index: None,
                matched_by: DeviceMatch::Default,
            }
        );
        // A saved id with no remembered label gets no second chance.
        assert_eq!(
            resolve_input(&devices, &prefs("alsa:hw:9,9", "")),
            InputResolution {
                index: None,
                matched_by: DeviceMatch::Default,
            }
        );
        // Blank labels never satisfy a label match.
        let unlabeled = [device("alsa:hw:0,0", "")];
        assert_eq!(
            resolve_input(&unlabeled, &prefs("alsa:hw:9,9", "")),
            InputResolution {
                index: None,
                matched_by: DeviceMatch::Default,
            }
        );
    }

    #[test]
    fn a_label_alone_names_a_mic_when_no_id_was_saved() {
        let devices = [
            device("alsa:hw:0,0", "Built-in Mic"),
            device("alsa:hw:1,0", "USB Mic"),
        ];
        assert_eq!(
            resolve_input(&devices, &prefs("", "USB Mic")),
            InputResolution {
                index: Some(1),
                matched_by: DeviceMatch::Label,
            }
        );
        // But a label alone on an absent mic still falls back, never guesses.
        assert_eq!(
            resolve_input(&devices, &prefs("", "Gone Mic")),
            InputResolution {
                index: None,
                matched_by: DeviceMatch::Default,
            }
        );
    }

    #[test]
    fn preferences_round_trip_through_json_and_tolerate_missing_fields() {
        let saved = prefs("alsa:hw:1,0", "USB Mic");
        let parsed: AudioDevicePreferences =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert_eq!(parsed, saved);

        // A partial or absent payload degrades to the system default rather
        // than failing the settings read.
        let partial: AudioDevicePreferences =
            serde_json::from_str(r#"{"input_device_id":"alsa:hw:1,0"}"#).unwrap();
        assert_eq!(partial.input_label, "");
        let empty: AudioDevicePreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, AudioDevicePreferences::default());
    }
}

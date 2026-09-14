//! Capture-only microphone probe: device resolution plus a level meter for
//! settings surfaces, without a session or an output stream.
//!
//! [`MicProbe`] opens exactly one input stream - resolved through the same
//! id -> label -> default chain as [`AudioEngine`] - and never touches an
//! output device, playback pipeline, or realtime transport. The capture gate
//! starts closed and the level stays zero until
//! [`MicProbe::set_testing`] opens it.
//!
//! While testing, an internal task drains the bounded capture channel so a
//! live meter cannot fill it and manufacture drop counters; the probe exists
//! for the level and the diagnostics, not the samples.

use crate::audio::{
    capture::{AudioCapture, CaptureFrame},
    devices::{self, AudioDevicePreferences, DeviceMatch},
    engine::{Counters, DeviceConfig, InputDiagnostics},
    error::AudioError,
};
use cpal::traits::StreamTrait;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
use tokio::sync::mpsc;

/// A capture-only microphone handle for device pickers and level tests.
///
/// Created by [`MicProbe::open`]; dropping the probe (or calling
/// [`MicProbe::close`]) pauses the input stream and joins both workers.
pub struct MicProbe {
    capture: AudioCapture,
    config: DeviceConfig,
    input_match: DeviceMatch,
    counters: Arc<Counters>,
    stream: Option<cpal::Stream>,
    worker: Option<JoinHandle<()>>,
    drainer: Option<JoinHandle<()>>,
    drainer_stop: Arc<AtomicBool>,
}

impl MicProbe {
    /// Resolves `preferences` against the host's input devices - saved id,
    /// then remembered label, then the system default - and opens the winner
    /// for capture only.
    ///
    /// The microphone gate is closed on return: nothing is captured and the
    /// level reads zero until [`MicProbe::set_testing`] opens it. No output
    /// device is opened and no samples leave the process.
    pub fn open(preferences: &AudioDevicePreferences) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let (device, input_match) = devices::resolve_input_device(&host, preferences)?;

        let counters = Arc::new(Counters::default());
        let parts = AudioCapture::open(&device, Arc::clone(&counters))?;
        let config = parts.handle.config().clone();
        let (drainer, drainer_stop) = spawn_drainer(parts.frames);

        Ok(Self {
            capture: parts.handle,
            config,
            input_match,
            counters,
            stream: Some(parts.stream),
            worker: Some(parts.worker),
            drainer,
            drainer_stop,
        })
    }

    /// Name and negotiated format of the resolved input device.
    pub fn config(&self) -> &DeviceConfig {
        &self.config
    }

    /// How `preferences` resolved at open: exact id, remembered label, or
    /// the system default.
    pub fn input_match(&self) -> DeviceMatch {
        self.input_match
    }

    /// Decaying peak of the most recent callback on `[0.0, 1.0]`. Zero while
    /// the testing gate is closed. Cheap enough to poll per frame.
    pub fn level(&self) -> f32 {
        self.capture.level()
    }

    /// Opens or closes the testing gate. While closed, captured frames never
    /// reach the ring and the level reads zero.
    pub fn set_testing(&self, testing: bool) {
        self.capture.set_capturing(testing);
    }

    /// Whether the testing gate is open.
    pub fn is_testing(&self) -> bool {
        self.capture.is_capturing()
    }

    /// A snapshot of the input counters: chunks delivered, ring and channel
    /// drops, and stream errors. Because the probe drains its own channel,
    /// `dropped_chunks` should stay at zero - a nonzero value means the
    /// pipeline itself fell behind, not that the UI stopped reading.
    pub fn diagnostics(&self) -> InputDiagnostics {
        self.counters.input_snapshot()
    }

    /// Stops the stream and joins both workers.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.capture.stop();
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.drainer_stop.store(true, Ordering::Release);
        if let Some(drainer) = self.drainer.take() {
            let _ = drainer.join();
        }
    }
}

impl Drop for MicProbe {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawns the task that keeps the capture channel empty. It owns the
/// receiver and drops every frame until `stop` is set or the sender side
/// closes with the capture worker. A spawn failure degrades to no drainer:
/// the channel then fills and the drop counters report it honestly instead
/// of panicking the probe open.
fn spawn_drainer(
    mut frames: mpsc::Receiver<CaptureFrame>,
) -> (Option<JoinHandle<()>>, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    let drainer = thread::Builder::new()
        .name("zeron-voice-probe".to_owned())
        .spawn(move || drain_frames(&mut frames, &signal))
        .ok();
    (drainer, stop)
}

fn drain_frames(frames: &mut mpsc::Receiver<CaptureFrame>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match frames.try_recv() {
            Ok(_) => continue,
            Err(mpsc::error::TryRecvError::Empty) => thread::park_timeout(PARK),
            // The capture worker is gone; nothing more will arrive.
            Err(mpsc::error::TryRecvError::Disconnected) => return,
        }
    }
}

/// Drainer sleep while the capture channel is empty. Well under the 20 ms a
/// chunk takes to produce, so the bounded channel never fills on a healthy
/// pipeline.
const PARK: std::time::Duration = std::time::Duration::from_millis(2);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_drainer_empties_the_channel_and_exits_on_stop() {
        let (sender, mut frames) = mpsc::channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn({
            let stop = Arc::clone(&stop);
            move || drain_frames(&mut frames, &stop)
        });

        // Keep producing past the channel bound; the drainer keeps it empty.
        for _ in 0..32 {
            while sender
                .try_send(CaptureFrame::Audio(vec![0; 8].into_boxed_slice()))
                .is_err()
            {
                thread::yield_now();
            }
        }
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn the_drainer_exits_when_the_capture_side_disconnects() {
        let (sender, mut frames) = mpsc::channel::<CaptureFrame>(1);
        drop(sender);
        let stop = AtomicBool::new(false);
        drain_frames(&mut frames, &stop);
        // Returned on its own: no stop flag was needed.
    }

    #[test]
    fn a_slow_drainer_still_terminates() {
        let (sender, mut frames) = mpsc::channel(1);
        sender
            .try_send(CaptureFrame::Audio(vec![0; 8].into_boxed_slice()))
            .unwrap();
        drop(sender);
        let stop = AtomicBool::new(false);
        let started = std::time::Instant::now();
        drain_frames(&mut frames, &stop);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

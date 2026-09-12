#![cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;

pub use linux::current_platform;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputOrigin { Unknown, Physical, Synthetic }

thread_local! {
    static INPUT_ORIGIN: std::cell::Cell<InputOrigin> = const { std::cell::Cell::new(InputOrigin::Unknown) };
}

pub(crate) struct InputOriginGuard(InputOrigin);
impl InputOriginGuard {
    pub(crate) fn enter(origin: InputOrigin) -> Self { Self(INPUT_ORIGIN.replace(origin)) }
}
impl Drop for InputOriginGuard {
    fn drop(&mut self) { INPUT_ORIGIN.set(self.0); }
}

/// True only during synchronous dispatch from the selected ordinary Wayland
/// pointer or keyboard. Unknown and accessibility requests are not physical.
/// Capture this value before deferring a user action to an asynchronous task.
pub fn is_physical_input_dispatch() -> bool {
    INPUT_ORIGIN.get() == InputOrigin::Physical
}

/// Whether the current synchronous callback came from the isolated agent seat.
pub fn is_synthetic_input_dispatch() -> bool {
    INPUT_ORIGIN.get() == InputOrigin::Synthetic
}

#[cfg(test)]
mod input_origin_tests {
    use super::*;

    #[test]
    fn nested_dispatch_restores_origin_even_after_unwind() {
        assert!(!is_physical_input_dispatch());
        assert!(!is_synthetic_input_dispatch());
        {
            let _physical = InputOriginGuard::enter(InputOrigin::Physical);
            assert!(is_physical_input_dispatch());
            let _ = std::panic::catch_unwind(|| {
                let _synthetic = InputOriginGuard::enter(InputOrigin::Synthetic);
                assert!(is_synthetic_input_dispatch());
                assert!(!is_physical_input_dispatch());
                panic!("exercise dispatch unwinding");
            });
            assert!(is_physical_input_dispatch());
            assert!(!is_synthetic_input_dispatch());
        }
        assert!(!is_physical_input_dispatch());
        assert!(!is_synthetic_input_dispatch());
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the running Wayland compositor advertised a background-blur
/// interface (`org_kde_kwin_blur_manager`, or `ext_background_effect_manager_v1`
/// on compositors that have moved to it). Set once when the platform client's
/// registry binds its globals — before any window opens — and read by the app's
/// theme layer to decide whether a translucent window will actually be blurred.
///
/// This matters because `WindowBackgroundAppearance::Blurred` clears the
/// surface's opaque region even when no blur manager exists, so a translucent
/// shell on a blur-less compositor would show the raw desktop with no frosting.
static COMPOSITOR_BLUR_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Record that the compositor advertised a blur interface. Called by the
/// Wayland client during registry setup; a no-op on X11/headless.
pub(crate) fn set_compositor_blur_supported(supported: bool) {
    COMPOSITOR_BLUR_SUPPORTED.store(supported, Ordering::Relaxed);
}

/// Whether the current compositor can blur the region behind a translucent
/// window. Always false off Wayland (X11 has no standard window-background
/// blur, headless has no compositor at all).
pub fn compositor_blur_supported() -> bool {
    COMPOSITOR_BLUR_SUPPORTED.load(Ordering::Relaxed)
}

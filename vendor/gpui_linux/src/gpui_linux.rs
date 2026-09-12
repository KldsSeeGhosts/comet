#![cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;

pub use linux::current_platform;

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

//! Positive physical-input proof retained across deferred UI callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanInput(());

pub fn is_human_input() -> bool {
    #[cfg(target_os = "linux")]
    { gpui_linux::is_physical_input_dispatch() }
    #[cfg(not(target_os = "linux"))]
    { false }
}

pub fn capture() -> Option<HumanInput> {
    is_human_input().then_some(HumanInput(()))
}

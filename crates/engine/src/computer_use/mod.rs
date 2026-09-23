//! Engine-owned computer use. The Pi adapter only forwards tool calls.
//!
//! Linux runs the reviewed `cua-driver`. Other platforms keep the same types
//! so session dispatch can ask for a bridge and get a clear refusal.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(not(target_os = "linux"))]
pub use unsupported::*;

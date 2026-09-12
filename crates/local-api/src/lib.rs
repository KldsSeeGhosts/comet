//! Unix HTTP transport for the UI control plane. See the crate README for the wire contract.
#![cfg(unix)]

mod client;
mod identity;
mod server;

pub use client::{Client, InstanceStatus, ServerEvent, SubscriptionStream, discover_instances};
pub use identity::{InstanceLockedError, Manifest, SocketIdentity, is_instance_locked};
pub use server::{ControlPlane, EventHub, PublishedEvent, Request, Subscription};

pub const PROTOCOL_VERSION: u32 = 1;
pub const INSTANCE_HEADER: &str = "x-noches-instance";

/// Chooses one variant without inspecting the other variant's files.
pub fn default_data_dir(dev: bool) -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(std::path::PathBuf::from(home).join(if dev { ".zeron-dev" } else { ".zeron" }))
}

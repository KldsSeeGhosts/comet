//! Authenticated, resumable engine connections. The host keeps its local engine
//! connection alive through client network changes. Sequence numbers acknowledge
//! delivery in each direction; requests are never blindly rerun after host loss.
mod client;
mod config;
mod server;

pub use client::{ConnectionState, RemoteClient, connect};
pub use config::{ConnectionProfile, Connections, Credentials, private_write, validate_endpoint};
pub use server::{ServerOptions, serve};

use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;
const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum Frame {
    Hello {
        version: u32,
        session: String,
        cursor: u64,
        resume: bool,
    },
    Welcome {
        version: u32,
        resumed: bool,
        received: u64,
    },
    Data {
        seq: u64,
        payload: String,
        end: bool,
    },
    Ack {
        seq: u64,
    },
    Ping,
    Pong,
    Reset,
    Goodbye,
}

fn encode(frame: &Frame) -> Result<String, crate::RpcError> {
    serde_json::to_string(frame).map_err(|e| crate::RpcError::Transport(e.to_string()))
}

pub(crate) fn private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_loopback() || (u32::from(ip) & 0xffc00000 == 0x64400000),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback() || ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0]
        }
    }
}

// Bound individual writes on slow links. Acknowledged chunks survive reconnect,
// including a partially received transcript or file request.
fn chunks(payload: &str) -> Vec<(String, bool)> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < payload.len() {
        let mut end = (start + 32 * 1024).min(payload.len());
        while !payload.is_char_boundary(end) {
            end -= 1;
        }
        result.push((payload[start..end].to_owned(), end == payload.len()));
        start = end;
    }
    if result.is_empty() {
        result.push((String::new(), true));
    }
    result
}

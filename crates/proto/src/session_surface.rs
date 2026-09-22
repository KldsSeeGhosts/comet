//! Host-owned presentation of an existing provider conversation.
use crate::TerminalSession;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionSurface {
    #[default]
    Chat,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSurfaceState {
    pub surface: SessionSurface,
    pub terminal: Option<TerminalSession>,
    pub can_switch: bool,
    pub reason: Option<String>,
}

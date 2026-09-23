use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::oneshot;
use zeron_harness::CancellationToken;
use zeron_proto::{UserInputAnswer, UserInputQuestion};

pub type RequestInput =
    Arc<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync>;

pub struct ComputerUseManager;

impl ComputerUseManager {
    pub fn new(_device_id: String) -> Self {
        Self
    }

    pub fn forget_computer_use_approval(&self, _chat_id: &str) -> bool {
        false
    }

    pub async fn start_bridge(
        &self,
        _chat_id: &str,
        _run_id: &str,
        _request_input: RequestInput,
        _interrupt: CancellationToken,
    ) -> Result<(PathBuf, RunBridge), String> {
        Err("Managed computer use currently requires a Linux host".into())
    }
}

/// No managed driver on this platform. Still `Clone` so a session can share the
/// handle the way Linux does: `steer` re-arms the turn before the next prompt.
/// `start_bridge` never succeeds here, so the handle stays `None` and these
/// methods are not called.
#[derive(Clone)]
pub struct RunBridge;

impl RunBridge {
    pub fn turn_started(&self) {}

    pub async fn turn_ended(&self) {}

    pub async fn finish(self) {}
}

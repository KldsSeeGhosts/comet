//! Native CLI launch descriptions and runtime teardown acknowledgements.
use crate::CancellationToken;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Executed directly in a PTY, never interpolated into a shell command.
pub struct NativeCliCommand {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

#[derive(Default)]
pub(crate) struct NativeRuntimes(Mutex<HashMap<String, CancellationToken>>);

impl NativeRuntimes {
    pub fn remember(&self, id: &str, exited: CancellationToken) {
        let mut runtimes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        runtimes.retain(|_, token| !token.is_cancelled());
        runtimes.insert(id.to_owned(), exited);
    }

    pub fn exited(&self, id: &str) -> Option<CancellationToken> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
}

/// Registers the provider-assigned id while preserving the original stream.
pub(crate) fn track(
    stream: futures::stream::BoxStream<
        'static,
        Result<zeron_proto::AgentEvent, crate::HarnessError>,
    >,
    runtimes: Arc<NativeRuntimes>,
    exited: CancellationToken,
) -> futures::stream::BoxStream<'static, Result<zeron_proto::AgentEvent, crate::HarnessError>> {
    use futures::StreamExt;
    stream
        .inspect(move |event| {
            if let Ok(zeron_proto::AgentEvent::SessionStarted { session_id, .. }) = event {
                runtimes.remember(session_id, exited.clone());
            }
        })
        .boxed()
}

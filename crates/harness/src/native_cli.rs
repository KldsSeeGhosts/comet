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

impl NativeCliCommand {
    /// Preserve the conversation's desktop browser tools across the handoff.
    pub fn with_browser(
        &mut self,
        harness: zeron_proto::HarnessId,
        browser: &zeron_browser::Connection,
    ) {
        match harness {
            zeron_proto::HarnessId::ClaudeCode => self.args.extend([
                "--mcp-config".into(),
                serde_json::json!({"mcpServers":{"noches_browser":browser.config()}}).to_string(),
            ]),
            zeron_proto::HarnessId::Codex => {
                self.args.extend([
                    "-c".into(),
                    format!(
                        "mcp_servers.noches_browser.command={}",
                        serde_json::to_string(&browser.executable).unwrap()
                    ),
                    "-c".into(),
                    format!(
                        "mcp_servers.noches_browser.args={}",
                        serde_json::to_string(&browser.args()).unwrap()
                    ),
                ]);
            }
            _ => {}
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_browser_tools_keep_the_conversation_and_literal_paths() {
        let browser = zeron_browser::Connection {
            executable: "/Applications/Noches App/zeron".into(),
            socket: "/tmp/browser fixture/control.sock".into(),
            session: "chat-browser".into(),
        };
        for provider in [
            zeron_proto::HarnessId::ClaudeCode,
            zeron_proto::HarnessId::Codex,
        ] {
            let mut command = NativeCliCommand {
                executable: "provider".into(),
                args: vec![],
            };
            command.with_browser(provider, &browser);
            let arguments = command.args.join("\n");
            assert!(arguments.contains("noches_browser"));
            assert!(arguments.contains("chat-browser"));
            assert!(arguments.contains("/Applications/Noches App/zeron"));
            assert!(arguments.contains("/tmp/browser fixture/control.sock"));
        }
    }
}

//! Realtime WebSocket client: connect, exchange typed events, run tool calls.
//!
//! One driver task owns the socket. It forwards every decoded server event to
//! the caller and runs at most one tool call at a time in a spawned task, so
//! socket reads, pings, and cancellation stay responsive while the workspace
//! dispatch runs. Cancellation and close frames shut the socket down cleanly;
//! reconnects and idempotency are out of scope for this local MVP.

use crate::{
    audio::pcm16_to_base64,
    error::VoiceError,
    events::{ClientEvent, ServerEvent, SessionConfig},
    tools::{ToolOutput, VoiceToolSink, execute_tool_call, text_turn_events, tool_result_events},
};
use futures::{SinkExt, StreamExt};
use std::{collections::HashMap, fmt, sync::Arc};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    task::{JoinError, JoinHandle},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::header::{AUTHORIZATION, HeaderValue},
    },
};
use tokio_util::sync::CancellationToken;

const DEFAULT_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";

/// Connection settings. `Debug` never prints the API key.
pub struct VoiceConfig {
    /// API key sent as `Authorization: Bearer ...`. Never logged or persisted.
    pub api_key: String,
    /// Model name, appended to the default endpoint as `?model=`.
    pub model: String,
    /// Overrides the default endpoint when set, e.g. a compatible proxy.
    pub endpoint: Option<String>,
    /// Initial `session.update` payload; skipped when empty.
    pub session: SessionConfig,
}

impl VoiceConfig {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            endpoint: None,
            session: SessionConfig::default(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn with_session(mut self, session: SessionConfig) -> Self {
        self.session = session;
        self
    }

    /// The WebSocket URL `connect` dials: `endpoint` verbatim when set, else
    /// the default OpenAI Realtime endpoint with `?model=` appended.
    pub fn endpoint_url(&self) -> String {
        match &self.endpoint {
            Some(endpoint) => endpoint.clone(),
            None => format!("{DEFAULT_ENDPOINT}?model={}", self.model),
        }
    }
}

impl fmt::Debug for VoiceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceConfig")
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("api_key", &"<redacted>")
            .field("session", &self.session)
            .finish()
    }
}

/// A live Realtime session. Outbound sends are queued and never block;
/// [`VoiceSession::recv`] yields decoded server events.
pub struct VoiceSession {
    outbound: mpsc::UnboundedSender<ClientEvent>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    cancel: CancellationToken,
    driver: JoinHandle<Result<(), VoiceError>>,
}

impl VoiceSession {
    /// Opens the WebSocket, sends the initial session configuration when one
    /// was provided, and starts the driver task.
    pub async fn connect(
        config: VoiceConfig,
        sink: Arc<dyn VoiceToolSink>,
    ) -> Result<Self, VoiceError> {
        let mut request = config.endpoint_url().into_client_request()?;
        // The key only exists in this scope; nothing below logs the request.
        let authorization = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .map_err(|_| VoiceError::InvalidAuthorization)?;
        request.headers_mut().insert(AUTHORIZATION, authorization);

        let (mut socket, _) = connect_async(request).await?;
        if !config.session.is_empty() {
            let update = ClientEvent::SessionUpdate {
                session: config.session.clone(),
            };
            socket
                .send(Message::Text(serde_json::to_string(&update)?))
                .await?;
        }

        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let driver = tokio::spawn(drive(socket, outbound_rx, events_tx, sink, cancel.clone()));
        Ok(Self {
            outbound,
            events,
            cancel,
            driver,
        })
    }

    /// Next server event, or `None` once the driver task has stopped.
    pub async fn recv(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    /// Queues any client event, including ones this crate does not originate.
    pub fn send(&self, event: ClientEvent) -> Result<(), VoiceError> {
        self.outbound
            .send(event)
            .map_err(|_| VoiceError::Disconnected)
    }

    pub fn configure_session(&self, session: SessionConfig) -> Result<(), VoiceError> {
        self.send(ClientEvent::SessionUpdate { session })
    }

    /// Sends a user text turn and asks the model to respond.
    pub fn send_text(&self, text: impl Into<String>) -> Result<(), VoiceError> {
        let (item, response) = text_turn_events(text);
        self.send(item)?;
        self.send(response)
    }

    pub fn request_response(&self) -> Result<(), VoiceError> {
        self.send(ClientEvent::ResponseCreate { response: None })
    }

    /// Cancels the in-flight response; playback truncation is a separate call.
    pub fn cancel_response(&self) -> Result<(), VoiceError> {
        self.send(ClientEvent::ResponseCancel)
    }

    /// Appends 24 kHz mono PCM16 input without committing it.
    pub fn append_audio(&self, samples: &[i16]) -> Result<(), VoiceError> {
        self.send(ClientEvent::InputAudioBufferAppend {
            audio: pcm16_to_base64(samples),
        })
    }

    pub fn commit_audio(&self) -> Result<(), VoiceError> {
        self.send(ClientEvent::InputAudioBufferCommit)
    }

    pub fn clear_audio(&self) -> Result<(), VoiceError> {
        self.send(ClientEvent::InputAudioBufferClear)
    }

    /// Cuts playback of an assistant audio item at `audio_end_ms`, used when
    /// the user interrupts.
    pub fn truncate_playback(
        &self,
        item_id: impl Into<String>,
        content_index: u32,
        audio_end_ms: u32,
    ) -> Result<(), VoiceError> {
        self.send(ClientEvent::ConversationItemTruncate {
            item_id: item_id.into(),
            content_index,
            audio_end_ms,
        })
    }

    /// Signals the driver to close the socket at the next loop turn.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Cancels, closes the WebSocket, and waits for the driver task to finish.
    pub async fn shutdown(self) -> Result<(), VoiceError> {
        let Self {
            outbound,
            cancel,
            driver,
            ..
        } = self;
        drop(outbound);
        cancel.cancel();
        match driver.await {
            Ok(result) => result,
            Err(error) => Err(VoiceError::Task(error)),
        }
    }
}

async fn drive(
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut outbound: mpsc::UnboundedReceiver<ClientEvent>,
    events: mpsc::UnboundedSender<ServerEvent>,
    sink: Arc<dyn VoiceToolSink>,
    cancel: CancellationToken,
) -> Result<(), VoiceError> {
    let (mut write, mut read) = socket.split();
    let mut names = FunctionNames::default();
    let mut active: Option<ActiveTool> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                close(&mut write).await;
                return Ok(());
            }
            outgoing = outbound.recv() => {
                let Some(event) = outgoing else {
                    close(&mut write).await;
                    return Ok(());
                };
                let payload = serde_json::to_string(&event)?;
                write.send(Message::Text(payload)).await?;
            }
            incoming = read.next() => {
                let Some(incoming) = incoming else {
                    return Ok(());
                };
                match incoming? {
                    Message::Text(text) => {
                        let event = match ServerEvent::decode(text.as_str()) {
                            Ok(event) => event,
                            Err(error) => {
                                tracing::debug!(%error, "ignoring undecodable realtime server event");
                                continue;
                            }
                        };
                        names.record(&event);
                        let call = names.pending_call(&event);
                        if events.send(event).is_err() {
                            close(&mut write).await;
                            return Ok(());
                        }
                        if let Some((call_id, name, arguments)) = call {
                            match active.as_ref() {
                                // Sequential calls are requested, but a race
                                // can still deliver a second one. Answer it
                                // with an error result; the running call's
                                // `response.create` continues the turn.
                                Some(running) => {
                                    tracing::warn!(
                                        call_id = %call_id,
                                        running = %running.call_id,
                                        "ignoring overlapping realtime tool call"
                                    );
                                    let item = overlapping_call_item(&call_id, &running.call_id);
                                    write.send(Message::Text(serde_json::to_string(&item)?)).await?;
                                }
                                None => {
                                    let handle = tokio::spawn({
                                        let sink = Arc::clone(&sink);
                                        let call_id = call_id.clone();
                                        async move {
                                            execute_tool_call(
                                                sink.as_ref(),
                                                &call_id,
                                                name.as_deref(),
                                                arguments.as_deref(),
                                            )
                                            .await
                                        }
                                    });
                                    active = Some(ActiveTool { call_id, handle });
                                }
                            }
                        }
                    }
                    Message::Ping(payload) => write.send(Message::Pong(payload)).await?,
                    Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        close(&mut write).await;
                        return Ok(());
                    }
                }
            }
            (call_id, finished) = await_active_tool(&mut active) => {
                active = None;
                let (item, response) = match finished {
                    Ok(tool_events) => tool_events,
                    Err(error) => {
                        tracing::warn!(%error, "voice tool call task failed");
                        tool_result_events(
                            &call_id,
                            &ToolOutput::error(format!("tool call `{call_id}` failed: {error}")),
                        )
                    }
                };
                for event in [item, response] {
                    let payload = serde_json::to_string(&event)?;
                    write.send(Message::Text(payload)).await?;
                }
            }
        }
    }
}

/// One in-flight tool call, running off the socket read path.
struct ActiveTool {
    call_id: String,
    handle: JoinHandle<(ClientEvent, ClientEvent)>,
}

/// Resolves when the active tool call finishes; stays pending while idle.
async fn await_active_tool(
    active: &mut Option<ActiveTool>,
) -> (String, Result<(ClientEvent, ClientEvent), JoinError>) {
    match active.as_mut() {
        Some(call) => (call.call_id.clone(), (&mut call.handle).await),
        None => std::future::pending().await,
    }
}

/// Error result for a second tool call that completes while one is running.
fn overlapping_call_item(call_id: &str, running_call_id: &str) -> ClientEvent {
    tool_result_events(
        call_id,
        &ToolOutput::error(format!(
            "tool call {call_id} ignored: {running_call_id} is still running"
        )),
    )
    .0
}

async fn close(
    write: &mut futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
) {
    let _ = write.send(Message::Close(None)).await;
    let _ = write.close().await;
}

/// Remembers function names from `response.output_item.*`, because the GA
/// `response.function_call_arguments.done` event does not carry the name.
#[derive(Default)]
struct FunctionNames {
    names: HashMap<String, String>,
}

impl FunctionNames {
    fn record(&mut self, event: &ServerEvent) {
        let item = match event {
            ServerEvent::ResponseOutputItemAdded { item, .. }
            | ServerEvent::ResponseOutputItemDone { item, .. } => item,
            _ => return,
        };
        if !item.is_function_call() {
            return;
        }
        let Some(name) = item.name.as_deref() else {
            return;
        };
        // Bound growth on long sessions; entries are also taken on dispatch.
        if self.names.len() > 256 {
            self.names.clear();
        }
        for key in [item.id.as_deref(), item.call_id.as_deref()]
            .into_iter()
            .flatten()
        {
            self.names.insert(key.to_owned(), name.to_owned());
        }
    }

    /// Returns `(call_id, name, arguments)` when `event` is a completed call.
    fn pending_call(
        &mut self,
        event: &ServerEvent,
    ) -> Option<(String, Option<String>, Option<String>)> {
        let ServerEvent::FunctionCallArgumentsDone {
            item_id,
            call_id,
            name,
            arguments,
            ..
        } = event
        else {
            return None;
        };
        let call_id = call_id.clone()?;
        let name = match name {
            Some(name) => Some(name.clone()),
            None => item_id
                .as_deref()
                .and_then(|id| self.names.remove(id))
                .or_else(|| self.names.remove(&call_id)),
        };
        Some((call_id, name, arguments.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_api_key() {
        let config = VoiceConfig::new("sk-do-not-log-me", "gpt-realtime");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-do-not-log-me"));
        assert!(rendered.contains("gpt-realtime"));
    }

    #[test]
    fn default_endpoint_carries_the_model() {
        let config = VoiceConfig::new("key", "gpt-realtime");
        assert_eq!(
            config.endpoint_url(),
            "wss://api.openai.com/v1/realtime?model=gpt-realtime"
        );
        let config = config.with_endpoint("ws://127.0.0.1:9999/v1/realtime");
        assert_eq!(config.endpoint_url(), "ws://127.0.0.1:9999/v1/realtime");
    }

    #[test]
    fn session_update_serializes_configured_fields_only() {
        let event = ClientEvent::SessionUpdate {
            session: SessionConfig::default().text_only(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "session.update");
        assert_eq!(json["session"]["type"], "realtime");
        assert_eq!(json["session"]["output_modalities"][0], "text");
        assert!(json["session"].get("audio").is_none());
        assert!(json["session"].get("voice").is_none());
    }

    #[test]
    fn overlapping_tool_call_gets_an_error_result() {
        let item = overlapping_call_item("call_b", "call_a");
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["item"]["type"], "function_call_output");
        assert_eq!(json["item"]["call_id"], "call_b");
        let output = json["item"]["output"].as_str().unwrap();
        assert!(output.contains("call_a"));
        assert!(output.contains("still running"));
    }

    #[test]
    fn function_names_resolve_from_output_item() {
        let mut names = FunctionNames::default();
        let added = ServerEvent::decode(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"layout_state"}}"#,
        )
        .unwrap();
        names.record(&added);
        let done = ServerEvent::decode(
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","call_id":"call_1","arguments":"{}"}"#,
        )
        .unwrap();
        let (call_id, name, arguments) = names.pending_call(&done).expect("call pending");
        assert_eq!(call_id, "call_1");
        assert_eq!(name.as_deref(), Some("layout_state"));
        assert_eq!(arguments.as_deref(), Some("{}"));
    }

    #[test]
    fn explicit_name_wins_over_the_cache() {
        let mut names = FunctionNames::default();
        let done = ServerEvent::decode(
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_9","call_id":"call_9","name":"inline_name","arguments":null}"#,
        )
        .unwrap();
        let (_, name, _) = names.pending_call(&done).expect("call pending");
        assert_eq!(name.as_deref(), Some("inline_name"));
    }
}

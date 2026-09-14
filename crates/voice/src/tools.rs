//! Function-call execution: validate arguments, run the sink, answer the model.
//!
//! Execution is deliberately serial: the session runs one call at a time (off
//! the socket read path) and answers each `response.function_call_arguments.done`
//! with a `function_call_output` item plus a fresh `response.create`. Parallel
//! tool calls are not modelled; the session config can set
//! `parallel_tool_calls: false` for the same reason.

use crate::events::{ClientEvent, ContentPart, ConversationItem};
use async_trait::async_trait;
use serde_json::{Map, Value};

/// A validated function call from the realtime model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

/// Output sent back as the `function_call_output` item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub output: String,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            output: text.into(),
        }
    }

    pub fn json(value: &Value) -> Self {
        Self::text(value.to_string())
    }

    /// Speakable error text; the model decides how to relay it.
    pub fn error(message: impl AsRef<str>) -> Self {
        Self::json(&serde_json::json!({ "error": message.as_ref() }))
    }
}

/// Executes tool calls for a session. The UI layer implements this by routing
/// into the same workspace control-plane `dispatch` the HTTP bridge uses, which
/// keeps consent semantics identical across transports.
#[async_trait]
pub trait VoiceToolSink: Send + Sync {
    /// Runs `call` and returns the string sent to the model as tool output.
    /// Errors become error output rather than killing the session.
    async fn call_tool(&self, call: ToolCall) -> anyhow::Result<ToolOutput>;
}

/// Parses the raw `arguments` string. Realtime sends a JSON object; anything
/// else is rejected before a sink ever sees it.
pub fn parse_arguments(raw: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(arguments)) => Ok(arguments),
        Ok(_) => Err("arguments must be a JSON object".to_owned()),
        Err(error) => Err(format!("arguments are not valid JSON: {error}")),
    }
}

/// The two events that return a tool result and continue the response.
pub fn tool_result_events(call_id: &str, output: &ToolOutput) -> (ClientEvent, ClientEvent) {
    (
        ClientEvent::ConversationItemCreate {
            item: ConversationItem::FunctionCallOutput {
                call_id: call_id.to_owned(),
                output: output.output.clone(),
            },
            previous_item_id: None,
        },
        ClientEvent::ResponseCreate { response: None },
    )
}

/// Validates and executes one function call, always returning the pair of
/// events that reports the result back to the model. Missing names, malformed
/// arguments, and sink failures all become error output; no path drops the
/// call or blocks the response.
pub async fn execute_tool_call(
    sink: &dyn VoiceToolSink,
    call_id: &str,
    name: Option<&str>,
    raw_arguments: Option<&str>,
) -> (ClientEvent, ClientEvent) {
    let output = match (name, raw_arguments) {
        (Some(name), Some(raw)) => match parse_arguments(raw) {
            Ok(arguments) => {
                let call = ToolCall {
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    arguments,
                };
                match sink.call_tool(call).await {
                    Ok(output) => output,
                    Err(error) => ToolOutput::error(format!("tool `{name}` failed: {error}")),
                }
            }
            Err(problem) => {
                ToolOutput::error(format!("tool `{name}` rejected its arguments: {problem}"))
            }
        },
        (None, _) => ToolOutput::error(format!("tool call {call_id} has no function name")),
        (Some(name), None) => {
            ToolOutput::error(format!("tool `{name}` call {call_id} is missing arguments"))
        }
    };
    tool_result_events(call_id, &output)
}

/// Convenience for the headless text harness: a user message plus a response.
pub fn text_turn_events(text: impl Into<String>) -> (ClientEvent, ClientEvent) {
    (
        ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: "user".to_owned(),
                content: vec![ContentPart::input_text(text)],
            },
            previous_item_id: None,
        },
        ClientEvent::ResponseCreate { response: None },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        calls: Mutex<Vec<ToolCall>>,
        fail: bool,
    }

    #[async_trait]
    impl VoiceToolSink for RecordingSink {
        async fn call_tool(&self, call: ToolCall) -> anyhow::Result<ToolOutput> {
            self.calls.lock().unwrap().push(call);
            if self.fail {
                anyhow::bail!("backend exploded");
            }
            Ok(ToolOutput::text("ok"))
        }
    }

    #[tokio::test]
    async fn tool_result_correlates_call_id_and_continues_response() {
        let sink = RecordingSink::default();
        let (item, response) = execute_tool_call(
            &sink,
            "call_7",
            Some("focus_thread"),
            Some(r#"{"chat_id":"c1"}"#),
        )
        .await;

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "call_7");
        assert_eq!(calls[0].name, "focus_thread");
        assert_eq!(calls[0].arguments["chat_id"], "c1");
        drop(calls);

        let item = serde_json::to_value(&item).unwrap();
        assert_eq!(item["type"], "conversation.item.create");
        assert_eq!(item["item"]["type"], "function_call_output");
        assert_eq!(item["item"]["call_id"], "call_7");
        assert_eq!(item["item"]["output"], "ok");

        let response = serde_json::to_value(&response).unwrap();
        assert_eq!(response["type"], "response.create");
    }

    #[tokio::test]
    async fn non_object_arguments_never_reach_the_sink() {
        let sink = RecordingSink::default();
        let (item, _) =
            execute_tool_call(&sink, "call_8", Some("focus_thread"), Some(r#"["nope"]"#)).await;
        assert!(sink.calls.lock().unwrap().is_empty());

        let item = serde_json::to_value(&item).unwrap();
        assert_eq!(item["item"]["type"], "function_call_output");
        assert_eq!(item["item"]["call_id"], "call_8");
        assert!(
            item["item"]["output"]
                .as_str()
                .unwrap()
                .contains("JSON object")
        );
    }

    #[tokio::test]
    async fn sink_errors_and_missing_names_still_answer_the_call() {
        let sink = RecordingSink {
            fail: true,
            ..RecordingSink::default()
        };
        let (item, _) = execute_tool_call(&sink, "call_9", Some("focus_thread"), Some("{}")).await;
        let item = serde_json::to_value(&item).unwrap();
        assert!(
            item["item"]["output"]
                .as_str()
                .unwrap()
                .contains("backend exploded")
        );

        let sink = RecordingSink::default();
        let (item, _) = execute_tool_call(&sink, "call_10", None, Some("{}")).await;
        assert!(sink.calls.lock().unwrap().is_empty());
        let item = serde_json::to_value(&item).unwrap();
        assert_eq!(item["item"]["call_id"], "call_10");
        assert!(
            item["item"]["output"]
                .as_str()
                .unwrap()
                .contains("no function name")
        );
    }

    #[test]
    fn parse_arguments_accepts_objects_only() {
        assert!(parse_arguments("{}").is_ok());
        assert!(parse_arguments(r#"{"a":1}"#).is_ok());
        assert!(parse_arguments("[]").is_err());
        assert!(parse_arguments("null").is_err());
        assert!(parse_arguments("nope").is_err());
    }

    #[test]
    fn text_turns_create_input_text_and_response() {
        let (item, response) = text_turn_events("hello");
        let item = serde_json::to_value(&item).unwrap();
        assert_eq!(item["item"]["role"], "user");
        assert_eq!(item["item"]["content"][0]["type"], "input_text");
        assert_eq!(item["item"]["content"][0]["text"], "hello");
        let response = serde_json::to_value(&response).unwrap();
        assert_eq!(response["type"], "response.create");
    }
}

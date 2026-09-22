//! GPT-Live's Responses delegation is separate from Realtime function calls.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Do not continue a backend response as soon as its first tool finishes.
/// Collect output items until response.completed, then return every result
/// before response.create. The completion event intentionally has output: [].
#[derive(Default)]
pub struct ToolBatches {
    responses: HashMap<String, Vec<FunctionCall>>,
    delegations: HashMap<String, String>,
    seen: HashSet<String>,
}
impl ToolBatches {
    pub fn accept(&mut self, envelope: &Value) -> Result<Option<Vec<FunctionCall>>> {
        if envelope["type"] != "response.event" {
            return Ok(None);
        }
        let event = &envelope["event"];
        let delegation = envelope["delegation_id"].as_str().unwrap_or("");
        match event["type"].as_str().unwrap_or("") {
            "response.created" => {
                let id = event["response"]["id"]
                    .as_str()
                    .context("Backend response has no id")?;
                self.delegations.insert(delegation.into(), id.into());
                self.responses.entry(id.into()).or_default();
            }
            "response.output_item.done" if event["item"]["type"] == "function_call" => {
                let item = &event["item"];
                let id = item["call_id"].as_str().context("Tool call has no id")?;
                if self.seen.contains(id) {
                    return Ok(None);
                }
                if self.seen.len() >= 4096 {
                    bail!("Voice tool limit reached; start a new call");
                }
                let response = event["response_id"]
                    .as_str()
                    .or_else(|| self.delegations.get(delegation).map(String::as_str))
                    .context("Tool call has no response")?
                    .to_owned();
                let name = item["name"].as_str().context("Tool call has no name")?;
                let raw = item["arguments"]
                    .as_str()
                    .context("Tool arguments are missing")?;
                if raw.len() > 128 * 1024 {
                    bail!("Voice tool arguments are too large");
                }
                // Malformed JSON must never silently become empty arguments.
                let arguments =
                    serde_json::from_str(raw).context("Invalid voice tool arguments")?;
                self.seen.insert(id.into());
                self.responses
                    .entry(response)
                    .or_default()
                    .push(FunctionCall {
                        id: id.into(),
                        name: name.into(),
                        arguments,
                    });
            }
            "response.completed" => {
                let id = event["response"]["id"]
                    .as_str()
                    .context("Completed response has no id")?;
                self.delegations.retain(|_, response| response != id);
                return Ok(self.responses.remove(id).filter(|calls| !calls.is_empty()));
            }
            "response.failed" | "response.cancelled" | "response.incomplete" => {
                if let Some(id) = event["response"]["id"].as_str() {
                    self.responses.remove(id);
                    self.delegations.retain(|_, response| response != id);
                }
            }
            _ => {}
        }
        Ok(None)
    }
}
pub fn tool_output(id: &str, output: Value) -> Value {
    json!({"type":"response.item.create", "item": {
        "type":"function_call_output", "call_id":id, "output":output.to_string()
    }})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(value: Value) -> Value {
        json!({"type":"response.event", "delegation_id":"d", "event":value})
    }
    fn created(id: &str) -> Value {
        event(json!({"type":"response.created", "response":{"id":id}}))
    }
    fn tool(id: &str, args: &str) -> Value {
        event(json!({"type":"response.output_item.done", "item":{
            "type":"function_call", "call_id":id, "name":"get_context", "arguments":args
        }}))
    }
    fn completed(id: &str) -> Value {
        event(json!({"type":"response.completed", "response":{"id":id,"output":[]}}))
    }
    #[test]
    fn waits_for_complete_batch_and_deduplicates() {
        let mut b = ToolBatches::default();
        b.accept(&created("r")).unwrap();
        assert!(b.accept(&tool("a", "{}")).unwrap().is_none());
        b.accept(&tool("a", "{}")).unwrap();
        b.accept(&tool("b", "{}")).unwrap();
        let calls = b.accept(&completed("r")).unwrap().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(b.accept(&completed("r")).unwrap().is_none());
    }
    #[test]
    fn failed_response_never_executes_pending_tools() {
        let mut b = ToolBatches::default();
        b.accept(&created("r")).unwrap();
        b.accept(&tool("a", "{}")).unwrap();
        b.accept(&event(
            json!({"type":"response.failed","response":{"id":"r"}}),
        ))
        .unwrap();
        assert!(b.accept(&completed("r")).unwrap().is_none());
    }
    #[test]
    fn rejects_malformed_arguments() {
        let mut b = ToolBatches::default();
        b.accept(&created("r")).unwrap();
        assert!(b.accept(&tool("a", "{oops")).is_err());
    }
    #[test]
    fn tool_results_use_live_protocol() {
        let result = tool_output("a", json!({"ok":true}));
        assert_eq!(result["type"], "response.item.create");
        assert_eq!(result["item"]["output"], "{\"ok\":true}");
    }
}

//! Voice tool bridge: the reviewed allowlist a realtime model may call, and the
//! [`VoiceToolSink`] that routes those calls into the workspace control
//! dispatcher on the GPUI thread.
//!
//! `zeron-voice` runs tool calls off the GPUI thread. This module keeps every
//! [`Workspace`](crate::workspace::Workspace) access on the GPUI executor: a
//! foreground task drains a channel of [`ControlStep`]s and runs each one
//! through the same `dispatch()` the local HTTP bridge uses, so consent
//! checks, CLI-pane refusal, and workspace routing stay in one place. The
//! channel closes and the task ends when the last [`WorkspaceToolSink`] clone
//! drops.
//!
//! No microphone, audio device, credential, transcript, or session UI lives
//! here. This is only the model-to-control-plane seam.

#[cfg(test)]
mod tests;

mod controller;

pub(crate) use controller::VoiceController;
pub use controller::{VoicePhase, VoiceStatus};

use std::sync::Arc;

use gpui::{App, AsyncApp, WeakEntity};
use serde_json::{Map, Value, json};
use zeron_voice::{ToolCall, ToolOutput, VoiceToolSink};

use crate::workspace::{Workspace, control::dispatch};

/// One control-plane method plus its params, run in order on the GPUI thread.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlStep {
    pub method: &'static str,
    pub params: Value,
}

impl ControlStep {
    fn new(method: &'static str, params: Value) -> Self {
        Self { method, params }
    }
}

/// Maps validated model arguments to the ordered control steps to run.
type ToolPlan = fn(&Map<String, Value>) -> Result<Vec<ControlStep>, String>;

/// A function the realtime model may call, with its fixed control mapping.
struct VoiceTool {
    name: &'static str,
    description: &'static str,
    parameters: fn() -> Value,
    plan: ToolPlan,
}

/// The reviewed allowlist. Adding an entry here is the only way the model can
/// reach a control method from voice; there is no generic method forwarder.
static TOOLS: &[VoiceTool] = &[
    VoiceTool {
        name: "get_context",
        description: "Read the current workspace layout: views, tabs, panes with their session id, label, group, renderer mode, and which pane is active. The `target` strings returned here are the `to` values other tools accept.",
        parameters: no_parameters,
        plan: get_context,
    },
    VoiceTool {
        name: "list_threads",
        description: "List the chat threads known to the workspace, with id, workspace, title, and provider configuration. Read-only.",
        parameters: no_parameters,
        plan: list_threads,
    },
    VoiceTool {
        name: "read_thread",
        description: "Read a chat thread's transcript and context usage. Read-only; pass `to` to read a pane other than the active one.",
        parameters: read_thread_parameters,
        plan: read_thread,
    },
    VoiceTool {
        name: "focus_thread",
        description: "Focus a chat thread: select its session into a pane and raise the app window. Does not submit any input.",
        parameters: focus_thread_parameters,
        plan: focus_thread,
    },
    VoiceTool {
        name: "send_to_thread",
        description: "Submit a message to a chat thread. Requires the human Allow grant in that thread's workspace, and CLI-owned panes are refused; a denial comes back as a speakable error. Never sends to a terminal pane.",
        parameters: send_to_thread_parameters,
        plan: send_to_thread,
    },
];

/// OpenAI Realtime function definitions for the allowlist, ready for
/// [`zeron_voice::SessionConfig::with_tools`].
pub fn tool_definitions() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": (tool.parameters)(),
            })
        })
        .collect()
}

/// A [`VoiceToolSink`] that executes allowlisted tools through the workspace
/// control dispatcher on the GPUI executor.
#[derive(Debug)]
pub struct WorkspaceToolSink {
    calls: tokio::sync::mpsc::UnboundedSender<VoiceCall>,
}

struct VoiceCall {
    steps: Vec<ControlStep>,
    reply: tokio::sync::oneshot::Sender<Result<Value, String>>,
}

/// Starts the GPUI-side tool executor and returns the sink the voice session
/// owns. Dropping the last returned `Arc` ends the executor.
///
/// Call from the GPUI thread; the returned sink itself is `Send + Sync` and
/// does no GPUI work, so the voice session may call it from any task.
pub fn spawn_tool_sink(workspace: WeakEntity<Workspace>, cx: &mut App) -> Arc<WorkspaceToolSink> {
    let (calls, mut queued) = tokio::sync::mpsc::unbounded_channel();
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(VoiceCall { steps, reply }) = queued.recv().await {
            let result = run_steps(&workspace, cx, steps).await;
            let _ = reply.send(result);
        }
    })
    .detach();
    Arc::new(WorkspaceToolSink { calls })
}

async fn run_steps(
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
    steps: Vec<ControlStep>,
) -> Result<Value, String> {
    let mut last = Value::Null;
    for step in steps {
        last = dispatch(workspace, cx, step.method, step.params)
            .await
            .map_err(|error| format!("{error:#}"))?;
    }
    Ok(last)
}

#[async_trait::async_trait]
impl VoiceToolSink for WorkspaceToolSink {
    async fn call_tool(&self, call: ToolCall) -> anyhow::Result<ToolOutput> {
        let Some(tool) = TOOLS.iter().find(|tool| tool.name == call.name) else {
            let names: Vec<_> = TOOLS.iter().map(|tool| tool.name).collect();
            return Ok(ToolOutput::error(format!(
                "unknown voice tool `{}`; available: {}",
                call.name,
                names.join(", ")
            )));
        };
        let steps = match (tool.plan)(&call.arguments) {
            Ok(steps) => steps,
            Err(problem) => {
                return Ok(ToolOutput::error(format!(
                    "tool `{}` rejected its arguments: {problem}",
                    call.name
                )));
            }
        };
        let (reply, result) = tokio::sync::oneshot::channel();
        if self.calls.send(VoiceCall { steps, reply }).is_err() {
            return Ok(ToolOutput::error("workspace UI is not available"));
        }
        match result.await {
            Ok(Ok(value)) => Ok(ToolOutput::json(&value)),
            Ok(Err(error)) => Ok(ToolOutput::error(error)),
            // The executor went away between send and reply; no panic.
            Err(_) => Ok(ToolOutput::error("workspace UI dropped the tool call")),
        }
    }
}

/// Reads and validates the model arguments for one tool.
struct Args<'a>(&'a Map<String, Value>);

impl Args<'_> {
    fn reject_unknown(&self, allowed: &[&str]) -> Result<(), String> {
        for key in self.0.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("unknown argument `{key}`"));
            }
        }
        Ok(())
    }

    fn required_str(&self, key: &str) -> Result<String, String> {
        match self.0.get(key) {
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
            Some(_) => Err(format!("`{key}` must be a nonempty string")),
            None => Err(format!("`{key}` is required")),
        }
    }

    /// Params for a control method: the given fields plus `to` when supplied.
    /// Omitting `to` leaves the dispatcher's own default (the active pane).
    fn control_params(&self, fields: &[(&str, Value)]) -> Result<Value, String> {
        let mut params = Map::new();
        for (key, value) in fields {
            params.insert((*key).to_owned(), value.clone());
        }
        if let Some(value) = self.0.get("to") {
            let to = value
                .as_str()
                .filter(|to| !to.trim().is_empty())
                .ok_or("`to` must be a nonempty string")?;
            params.insert("to".to_owned(), Value::String(to.to_owned()));
        }
        Ok(Value::Object(params))
    }
}

fn get_context(args: &Map<String, Value>) -> Result<Vec<ControlStep>, String> {
    Args(args).reject_unknown(&[])?;
    Ok(vec![ControlStep::new("layout.state", json!({}))])
}

fn list_threads(args: &Map<String, Value>) -> Result<Vec<ControlStep>, String> {
    Args(args).reject_unknown(&[])?;
    Ok(vec![ControlStep::new("chat.list", json!({}))])
}

fn read_thread(args: &Map<String, Value>) -> Result<Vec<ControlStep>, String> {
    let args = Args(args);
    args.reject_unknown(&["to"])?;
    Ok(vec![ControlStep::new(
        "agent.read",
        args.control_params(&[])?,
    )])
}

fn focus_thread(args: &Map<String, Value>) -> Result<Vec<ControlStep>, String> {
    let args = Args(args);
    args.reject_unknown(&["sessionId", "to"])?;
    let session = args.required_str("sessionId")?;
    let select = args.control_params(&[("sessionId", Value::String(session))])?;
    Ok(vec![
        ControlStep::new("chat.select", select),
        // chat.select only moves focus inside the layout; activating the
        // window is what brings the app forward when it is behind others.
        ControlStep::new("window.activate", json!({})),
    ])
}

fn send_to_thread(args: &Map<String, Value>) -> Result<Vec<ControlStep>, String> {
    let args = Args(args);
    args.reject_unknown(&["message", "to"])?;
    let message = args.required_str("message")?;
    let params = args.control_params(&[("message", Value::String(message))])?;
    Ok(vec![ControlStep::new("agent.send", params)])
}

fn no_parameters() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn target_property() -> Value {
    json!({
        "type": "string",
        "description": "Pane target from get_context, for example `id:3`, `label:review`, or `active-pane`. Omit to use the active pane.",
    })
}

fn read_thread_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {"to": target_property()},
        "additionalProperties": false,
    })
}

fn focus_thread_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessionId": {
                "type": "string",
                "description": "Chat session id from get_context or list_threads.",
            },
            "to": target_property(),
        },
        "required": ["sessionId"],
        "additionalProperties": false,
    })
}

fn send_to_thread_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Message text to submit to the thread.",
            },
            "to": target_property(),
        },
        "required": ["message"],
        "additionalProperties": false,
    })
}

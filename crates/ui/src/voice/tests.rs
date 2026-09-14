//! Focused tests for the pure allowlist, schema, and argument mapping, plus
//! the sink's channel behavior. No GPUI app is constructed here.

use super::*;

fn plan(name: &str, arguments: Value) -> Result<Vec<ControlStep>, String> {
    let tool = TOOLS
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("{name} is not allowlisted"));
    (tool.plan)(arguments.as_object().expect("object arguments"))
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        call_id: "call_1".into(),
        name: name.into(),
        arguments: arguments.as_object().expect("object arguments").clone(),
    }
}

#[test]
fn sink_is_send_and_sync() {
    // The voice session drives the sink from its own tokio task, so the
    // adapter must be able to cross threads even though it only ever talks
    // to the GPUI executor over a channel.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WorkspaceToolSink>();
    assert_send_sync::<Arc<WorkspaceToolSink>>();
}

#[test]
fn definitions_cover_the_reviewed_allowlist() {
    let names: Vec<_> = TOOLS.iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        [
            "get_context",
            "list_threads",
            "read_thread",
            "focus_thread",
            "send_to_thread",
        ]
    );

    let definitions = tool_definitions();
    assert_eq!(definitions.len(), TOOLS.len());
    for (definition, tool) in definitions.iter().zip(TOOLS) {
        assert_eq!(definition["type"], "function");
        assert_eq!(definition["name"], tool.name);
        assert!(!definition["description"].as_str().unwrap().is_empty());
        assert_eq!(definition["parameters"]["type"], "object");
        assert_eq!(definition["parameters"]["additionalProperties"], false);
        assert!(
            definition.get("function").is_none(),
            "Realtime function definitions are flat, not nested like chat completions"
        );
    }

    let required = |name: &str| -> Vec<String> {
        definitions
            .iter()
            .find(|definition| definition["name"] == name)
            .unwrap()["parameters"]["required"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .map(|key| key.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(required("focus_thread"), ["sessionId"]);
    assert_eq!(required("send_to_thread"), ["message"]);
    for name in ["get_context", "list_threads", "read_thread"] {
        assert!(required(name).is_empty(), "{name} takes no required args");
    }
}

#[test]
fn read_tools_map_to_single_control_methods() {
    assert_eq!(
        plan("get_context", json!({})).unwrap(),
        vec![ControlStep::new("layout.state", json!({}))]
    );
    assert_eq!(
        plan("list_threads", json!({})).unwrap(),
        vec![ControlStep::new("chat.list", json!({}))]
    );
    assert_eq!(
        plan("read_thread", json!({})).unwrap(),
        vec![ControlStep::new("agent.read", json!({}))]
    );
    assert_eq!(
        plan("read_thread", json!({"to": "pane:1"})).unwrap(),
        vec![ControlStep::new("agent.read", json!({"to": "pane:1"}))]
    );

    for name in ["get_context", "list_threads"] {
        assert!(
            plan(name, json!({"to": "active-pane"})).is_err(),
            "{name} takes no arguments"
        );
    }
    assert!(plan("read_thread", json!({"to": 7})).is_err());
}

#[test]
fn focus_thread_selects_then_activates() {
    let steps = plan(
        "focus_thread",
        json!({"sessionId": "s-1", "to": "label:review"}),
    )
    .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].method, "chat.select");
    assert_eq!(
        steps[0].params,
        json!({"sessionId": "s-1", "to": "label:review"})
    );
    assert_eq!(
        steps[1],
        ControlStep::new("window.activate", json!({})),
        "activation follows the selection"
    );

    let steps = plan("focus_thread", json!({"sessionId": "s-1"})).unwrap();
    assert_eq!(steps[0].params, json!({"sessionId": "s-1"}));
    assert!(
        steps[0].params.get("to").is_none(),
        "omitting `to` leaves the dispatcher's active-pane default in place"
    );

    assert!(plan("focus_thread", json!({})).is_err());
    assert!(plan("focus_thread", json!({"sessionId": "  "})).is_err());
    assert!(plan("focus_thread", json!({"sessionId": "s-1", "extra": 1})).is_err());
}

#[test]
fn send_to_thread_maps_exact_agent_send_params() {
    let steps = plan(
        "send_to_thread",
        json!({"to": "id:2", "message": "run the tests"}),
    )
    .unwrap();
    assert_eq!(
        steps,
        vec![ControlStep::new(
            "agent.send",
            json!({"to": "id:2", "message": "run the tests"})
        )]
    );
    assert!(
        steps[0].params.get("text").is_none(),
        "the control method takes `message`, not `text`"
    );

    let steps = plan("send_to_thread", json!({"message": "hi"})).unwrap();
    assert_eq!(steps[0].params, json!({"message": "hi"}));

    assert!(plan("send_to_thread", json!({})).is_err());
    assert!(plan("send_to_thread", json!({"message": 7})).is_err());
    assert!(plan("send_to_thread", json!({"message": "hi", "queue": true})).is_err());
}

#[tokio::test]
async fn sink_rejects_unlisted_tools_and_bad_arguments_before_dispatch() {
    let (calls, mut queued) = tokio::sync::mpsc::unbounded_channel();
    let sink = WorkspaceToolSink { calls };

    for name in [
        "agent.stop",
        "layout.compose",
        "window.activate",
        "chat.new",
    ] {
        let output = sink
            .call_tool(call(name, json!({"to": "active-pane"})))
            .await
            .unwrap();
        assert!(
            output.output.contains("unknown voice tool"),
            "{name}: {}",
            output.output
        );
    }
    assert!(
        queued.try_recv().is_err(),
        "no unlisted tool may reach the executor"
    );

    let output = sink
        .call_tool(call("send_to_thread", json!({})))
        .await
        .unwrap();
    assert!(
        output.output.contains("rejected its arguments"),
        "{}",
        output.output
    );
    assert!(
        queued.try_recv().is_err(),
        "invalid arguments must not reach the executor"
    );
}

#[tokio::test]
async fn sink_returns_compact_json_and_preserves_full_errors() {
    let (calls, mut queued) = tokio::sync::mpsc::unbounded_channel();
    let sink = WorkspaceToolSink { calls };

    let executor = tokio::spawn(async move {
        let first = queued.recv().await.expect("first call");
        assert_eq!(
            first.steps,
            vec![ControlStep::new("layout.state", json!({}))]
        );
        first.reply.send(Ok(json!({"panes": [1, 2]}))).unwrap();

        let second = queued.recv().await.expect("second call");
        assert_eq!(second.steps[0].method, "agent.send");
        second
            .reply
            .send(Err(
                "denied: a human must Allow API access in the selected workspace".into(),
            ))
            .unwrap();
    });

    let output = sink
        .call_tool(call("get_context", json!({})))
        .await
        .unwrap();
    assert_eq!(output.output, r#"{"panes":[1,2]}"#);

    let output = sink
        .call_tool(call("send_to_thread", json!({"message": "hi"})))
        .await
        .unwrap();
    assert_eq!(
        output.output,
        r#"{"error":"denied: a human must Allow API access in the selected workspace"}"#
    );
    executor.await.unwrap();
}

#[tokio::test]
async fn sink_stays_a_tool_result_when_the_executor_is_gone() {
    let (calls, queued) = tokio::sync::mpsc::unbounded_channel();
    drop(queued);
    let sink = WorkspaceToolSink { calls };
    let output = sink
        .call_tool(call("list_threads", json!({})))
        .await
        .unwrap();
    assert!(output.output.contains("not available"), "{}", output.output);

    let (calls, mut queued) = tokio::sync::mpsc::unbounded_channel();
    let sink = WorkspaceToolSink { calls };
    tokio::spawn(async move {
        let pending = queued.recv().await.expect("call");
        drop(pending.reply);
    });
    let output = sink
        .call_tool(call("list_threads", json!({})))
        .await
        .unwrap();
    assert!(output.output.contains("dropped"), "{}", output.output);
}

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{
    CancellationToken, Harness, HarnessError, PiHarness, RunCommand, RunControls, SteerMessage,
};
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, ToolCall};

fn fixture() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-rpc.py");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn base_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Pi),
        model: Some("test/native".into()),
        reasoning: Some(zeron_proto::ReasoningLevel::High),
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
    }
}

async fn collect_events(
    mut stream: futures::stream::BoxStream<'static, Result<AgentEvent, HarnessError>>,
) -> Vec<AgentEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let event = event.expect("valid event");
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("events settle within timeout")
}

#[tokio::test]
async fn native_rpc_runs_a_complete_pi_turn() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("inspect the file");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("native Pi starts");

    let events = collect_events(stream).await;

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { session_id, model, .. }
            if session_id == "/tmp/fake-pi-session.jsonl" && model == "test/native"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCall { call: ToolCall::ReadFile { path }, .. } if path == "src/main.rs"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TextDelta { text } if text == "done"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

#[tokio::test]
async fn switch_session_cancelled_fails_startup() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let mut request = base_request("resume test");
    request.resume = Some("cancelled-session.jsonl".into());

    let result = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await;

    match result {
        Err(HarnessError::Protocol(msg)) => {
            assert!(msg.contains("cancelled"), "expected cancelled in {msg}");
        }
        Err(other) => panic!("expected Protocol error with cancelled, got: {other:?}"),
        Ok(_) => panic!("expected failure, got Ok stream"),
    }
}

#[tokio::test]
async fn ignore_intermediate_non_terminal_settle() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("intermediate_settle");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let text_deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_deltas, vec!["delta1", "delta2"]);

    let done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .count();
    assert_eq!(done_count, 1, "exactly one Done must be emitted");
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

#[tokio::test]
async fn tool_execution_update_deduplicates_identical_final_result() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("tool_update_dedup");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let tool_calls: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .collect();
    let tool_results: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .collect();

    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        tool_results.len(),
        1,
        "identical result from end event must not duplicate update event"
    );
}

#[tokio::test]
async fn tool_execution_update_streams_and_updates_on_different_final_result() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("tool_update_progress");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let tool_results: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .collect();

    assert_eq!(
        tool_results.len(),
        2,
        "differing results should emit both update and final"
    );
    if let AgentEvent::ToolResult { output, .. } = tool_results[0] {
        assert!(output.as_deref().unwrap().contains("partial"));
    }
    if let AgentEvent::ToolResult { output, .. } = tool_results[1] {
        assert!(output.as_deref().unwrap().contains("final"));
    }
}

#[tokio::test]
async fn cancellation_issues_correlated_abort_and_drains_with_exactly_one_done() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = base_request("abort_normal");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let is_started = matches!(event, AgentEvent::SessionStarted { .. });
        events.push(event);
        if is_started {
            interrupt.cancel();
            break;
        }
    }

    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    let done_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(
        done_events.len(),
        1,
        "exactly one Done event must be emitted on cancellation"
    );
    assert!(matches!(
        done_events[0],
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    ));
}

#[tokio::test]
async fn cancellation_handles_reverse_abort_order_without_duplicate_done() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = base_request("abort_reverse");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let is_started = matches!(event, AgentEvent::SessionStarted { .. });
        events.push(event);
        if is_started {
            interrupt.cancel();
            break;
        }
    }

    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    let done_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(
        done_events.len(),
        1,
        "exactly one Done must be emitted even if settle arrives before ack"
    );
    assert!(matches!(
        done_events[0],
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    ));
}

#[tokio::test]
async fn cancellation_handles_abort_request_error() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = base_request("abort_error");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let is_started = matches!(event, AgentEvent::SessionStarted { .. });
        events.push(event);
        if is_started {
            interrupt.cancel();
            break;
        }
    }

    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error { message } if message.contains("abort request failed")
    )));
    let done_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(done_events.len(), 1);
    assert!(matches!(
        done_events[0],
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    ));
}

#[tokio::test]
async fn cancellation_after_completion_does_not_emit_duplicate_done() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = base_request("inspect the file");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    // Cancel interrupt after completion
    interrupt.cancel();

    // Stream should end without emitting more Done events
    while let Some(event) = stream.next().await {
        events.push(event.expect("valid event"));
    }

    let done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .count();
    assert_eq!(done_count, 1, "no duplicate Done after completion");
}

#[tokio::test]
async fn prompt_error_response_terminates_with_error_and_done() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("prompt_error");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error { message } if message.contains("prompt rejected by provider")
    )));
    let done_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(done_events.len(), 1);
    assert!(matches!(
        done_events[0],
        AgentEvent::Done {
            status: DoneStatus::Errored,
            ..
        }
    ));
}

#[tokio::test]
async fn child_crash_eof_terminates_with_crash_message() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("crash_eof");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let done_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(done_events.len(), 1);
    assert!(matches!(
        done_events[0],
        AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(_),
            ..
        }
    ));
}

#[tokio::test]
async fn local_only_prompt_completes_without_agent_invoked() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("local_only");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TextDelta { text } if text.contains("local output")
    )));
    let done_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(done_events.len(), 1);
    assert!(matches!(
        done_events[0],
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn extension_error_is_forwarded() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("extension_error");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error { message } if message.contains("extension hook failed")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

#[tokio::test]
async fn steer_during_turn_delivers_steer_and_settles() {
    let (steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("steer_active");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let is_started = matches!(event, AgentEvent::SessionStarted { .. });
        events.push(event);
        if is_started {
            let _ = steer_tx
                .send(RunCommand::Steer(SteerMessage {
                    prompt: "continue with next step".into(),
                    message_id: None,
                }))
                .await;
            break;
        }
    }

    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TextDelta { text } if text == "steered"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

#[tokio::test]
async fn steering_race_error_response_is_handled_and_emitted() {
    let (steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("steer_reject");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let is_started = matches!(event, AgentEvent::SessionStarted { .. });
        events.push(event);
        if is_started {
            let _ = steer_tx
                .send(RunCommand::Steer(SteerMessage {
                    prompt: "steer into settling agent".into(),
                    message_id: None,
                }))
                .await;
            break;
        }
    }

    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error { message } if message.contains("steer rejected")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

#[tokio::test]
async fn standard_pi_agent_end_does_not_settle_until_agent_settled() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("standard_agent_end");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let text_deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_deltas, vec!["before", "after"]);

    let done_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .count();
    assert_eq!(done_count, 1, "exactly one Done must be emitted");
}

#[tokio::test]
async fn steering_after_settlement_starts_next_turn_with_done() {
    let (steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("inspect the file");
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let mut first_turn_events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        first_turn_events.push(event);
        if done {
            break;
        }
    }

    assert!(first_turn_events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));

    // Send steer after first turn is fully completed and settled
    steer_tx
        .send(RunCommand::Steer(SteerMessage {
            prompt: "post_settle_next".into(),
            message_id: None,
        }))
        .await
        .expect("steer send succeeds");

    let mut second_turn_events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        second_turn_events.push(event);
        if done {
            break;
        }
    }

    assert!(
        second_turn_events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. }))
    );
    assert!(second_turn_events.iter().any(|e| matches!(
        e,
        AgentEvent::TextDelta { text } if text == "turn2_done"
    )));
    assert!(second_turn_events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));

    // Dropping steer_tx should cleanly close the persistent session
    drop(steer_tx);
    let tail = stream.next().await;
    assert!(tail.is_none(), "stream ends cleanly when steering drops");
}

#[tokio::test]
async fn tool_execution_update_followed_by_error_end_emits_error_result() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("tool_update_error");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let tool_results: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .collect();

    assert_eq!(tool_results.len(), 2);
    assert!(matches!(
        tool_results[0],
        AgentEvent::ToolResult {
            is_error: false,
            ..
        }
    ));
    assert!(matches!(
        tool_results[1],
        AgentEvent::ToolResult { is_error: true, .. }
    ));
}

#[tokio::test]
async fn structured_tool_content_is_extracted() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("structured_content");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let tool_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult { output, .. } => output.as_deref(),
            _ => None,
        })
        .expect("tool result emitted");

    assert_eq!(tool_result, "cleaned tool output");
}

#[tokio::test]
async fn session_info_changed_renames_the_chat_once_per_name() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    // The fixture emits the same session name twice before settling.
    let request = base_request("auto_title");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;

    let titles: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TitleUpdated { title } => Some(title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, vec!["Pi picked a title"]);
}

#[tokio::test]
async fn context_breakdown_flows_from_the_session_entry() {
    use zeron_proto::ContextComponentKind;

    // The fixture reports this file as the pi session; seed it with the
    // custom entry the zeron-context extension appends after a real turn.
    let session_file = "/tmp/fake-pi-session.jsonl";
    std::fs::write(
        session_file,
        concat!(
            r#"{"type":"message","id":"m1","message":{"role":"user"}}"#,
            "\n",
            r#"{"type":"custom","id":"z1","customType":"zeron:context-usage","data":{"v":1,"systemPrompt":250,"tools":900,"skills":40,"contextFiles":12,"messages":838,"totalTokens":2050,"contextWindow":1048576}}"#,
            "\n",
        ),
    )
    .unwrap();

    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = base_request("inspect the file");
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");

    let events = collect_events(stream).await;
    let _ = std::fs::remove_file(session_file);

    let usages: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ContextUsage {
                tokens,
                window,
                components,
            } => Some((*tokens, *window, components.clone())),
            _ => None,
        })
        .collect();
    assert!(usages.len() >= 2, "start and settle usage expected");

    // At run start the persisted breakdown rides along with live stats totals.
    let (tokens, window, components) = &usages[0];
    assert_eq!(*tokens, Some(25));
    assert_eq!(*window, Some(100_000));
    assert_eq!(components.len(), 5);
    assert_eq!(components[0].kind, ContextComponentKind::Tools);
    assert_eq!(components[0].tokens, 900);
    assert_eq!(components[4].kind, ContextComponentKind::Messages);

    // At settle the persisted entry replaces the fallback with authoritative
    // totals, so the card tracks the turn that just finished.
    let (tokens, window, components) = usages.last().unwrap();
    assert_eq!(*tokens, Some(2050));
    assert_eq!(*window, Some(1_048_576));
    assert_eq!(components.len(), 5);
}

/// The wire text a `RunCommand::Options` change pushes onto the live RPC
/// session — the fixture answers set_model with a distinct context window so
/// the ContextUsage event proves the switch actually went through.
#[tokio::test]
async fn options_command_applies_model_live_and_publishes_window() {
    let (steer_tx, steering) = mpsc::channel(4);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let mut stream = PiHarness::new()
        .with_executable(fixture())
        .run(base_request("inspect the file"), controls)
        .await
        .expect("starts");

    // Let the first turn settle, then change the model mid-session.
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("valid event");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }
    steer_tx
        .send(RunCommand::Options(zeron_proto::SessionOptions {
            model: Some("test/native".into()),
            reasoning: Some(zeron_proto::ReasoningLevel::Low),
            ..Default::default()
        }))
        .await
        .expect("options send succeeds");

    // The model switch publishes its fresh context window (set_model's
    // answer carries it — the fixture's 777000 marker proves the request
    // landed, versus the startup stats' 100000).
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            let event = event.expect("valid event");
            if matches!(
                event,
                AgentEvent::ContextUsage {
                    window: Some(777_000),
                    ..
                }
            ) {
                return;
            }
        }
        panic!("stream ended before the options-driven ContextUsage");
    })
    .await
    .expect("options publish the new window within timeout");
}

#[tokio::test]
async fn prompt_template_expands_before_the_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".pi/prompts")).unwrap();
    std::fs::write(
        dir.path().join(".pi/prompts/echo_wire.md"),
        "---\ndescription: echo\n---\nWIRE:$ARGUMENTS",
    )
    .unwrap();
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let mut request = base_request("/echo_wire hello there");
    request.cwd = dir.path().to_string_lossy().into_owned();
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TextDelta { text } if text == "WIRE:hello there"
    )));
}

#[tokio::test]
async fn slash_skill_routes_to_the_native_invocation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill = dir.path().join(".pi/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nReview carefully",
    )
    .unwrap();
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let mut request = base_request("/review src/lib.rs");
    request.cwd = dir.path().to_string_lossy().into_owned();
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(request, controls)
        .await
        .expect("starts");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TextDelta { text } if text == "/skill:review src/lib.rs"
    )));
}

/// A command the catalog knows but no template/skill claims passes through
/// verbatim — Pi itself decides what `/compact` does.
#[tokio::test]
async fn builtin_slash_command_passes_through_verbatim() {
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let stream = PiHarness::new()
        .with_executable(fixture())
        .run(base_request("/compact"), controls)
        .await
        .expect("starts");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TextDelta { text } if text == "/compact"
    )));
}

/// Rewind: fork dropping the last turn lands on `fork` with the boundary
/// entry and returns the fork's new session file.
#[tokio::test]
async fn fork_session_rolls_back_to_a_prior_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, "{}\n").unwrap();
    let forked = PiHarness::new()
        .with_executable(fixture())
        .fork_session(dir.path(), session.to_str().unwrap(), 1)
        .await
        .expect("fork succeeds");
    assert_eq!(forked, format!("{}.fork", session.display()));
}

/// `turns_to_remove == 0` is the whole-session clone path.
#[tokio::test]
async fn fork_session_with_zero_turns_clones_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, "{}\n").unwrap();
    let forked = PiHarness::new()
        .with_executable(fixture())
        .fork_session(dir.path(), session.to_str().unwrap(), 0)
        .await
        .expect("clone succeeds");
    assert_eq!(forked, format!("{}.fork", session.display()));
}

#[tokio::test]
async fn fork_session_rejects_an_overrun_and_a_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, "{}\n").unwrap();
    let harness = PiHarness::new().with_executable(fixture());
    let err = harness
        .fork_session(dir.path(), session.to_str().unwrap(), 5)
        .await
        .expect_err("removing more turns than the session has must fail");
    assert!(err.to_string().contains("only 2 native turns"));
    let err = harness
        .fork_session(dir.path(), "/nonexistent/session.jsonl", 1)
        .await
        .expect_err("a missing session file must fail before spawn");
    assert!(err.to_string().contains("missing"));
}

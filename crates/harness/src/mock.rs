//! Mock harness for engine/UI tests: replays a scripted event sequence.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls};

pub struct MockHarness {
    pub script: Vec<AgentEvent>,
}

/// The scripted question set for the `ZERON_MOCK_QUESTION` variant (exercises
/// the QuestionPanel end-to-end: single-select page, multi-select page).
fn question_script() -> Vec<UserInputQuestion> {
    vec![
        UserInputQuestion {
            id: "q-sync".into(),
            header: "Question".into(),
            question: "Which sync strategy should the rewrite use?".into(),
            options: vec![
                "Poll the doc host every 120ms".into(),
                "Event-driven fold with coalesced commits".into(),
                "Hybrid: event-driven with a polling fallback".into(),
            ],
            multi_select: false,
        },
        UserInputQuestion {
            id: "q-gates".into(),
            header: "Question".into(),
            question: "Which suites should gate the merge?".into(),
            options: vec![
                "Unit tests".into(),
                "End-to-end (two-device)".into(),
                "Golden screenshots".into(),
            ],
            multi_select: true,
        },
    ]
}

#[async_trait]
impl Harness for MockHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Mock"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![
            Model {
                id: "mock-1".into(),
                label: "Mock 1".into(),
                description: None,
                reasoning_levels: vec![ReasoningLevel::Medium],
                options: vec![],
            },
            // Claude-mirroring demo model: lets scripted runs carry the same
            // chip labels ("Fable 5 · High") as a real Claude session.
            Model {
                id: "mock-fable-5".into(),
                label: "Fable 5".into(),
                description: None,
                reasoning_levels: vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ],
                options: vec![],
            },
        ])
    }
    async fn run_title(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.run(request, controls).await
    }

    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        // Optional pacing knob for demos/manual testing: `ZERON_MOCK_DELAY_MS`
        // spaces the scripted events out so live-run UI states (working
        // indicator, streaming fade, trailing tool-group auto-open) are
        // observable. Unset (the default, and in tests) streams instantly.
        let delay_ms = std::env::var("ZERON_MOCK_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let delay = std::time::Duration::from_millis(delay_ms);

        // Dev/testing knob: `ZERON_MOCK_QUESTION=1` swaps in a run that asks
        // the user questions mid-stream via `controls.request_input` (the
        // engine mints the request id, emits `InputRequested`, and resolves it
        // from the `RespondInput` doc command) — the only data-side way to put
        // the QuestionPanel on screen.
        let question_mode = std::env::var("ZERON_MOCK_QUESTION")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        if question_mode {
            let request_input = controls.request_input;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            tokio::spawn(async move {
                let pause = if delay_ms == 0 {
                    std::time::Duration::from_millis(50)
                } else {
                    delay
                };
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text:
                        "Before I wire the reconciliation path I need two decisions from you.\n\n"
                            .into(),
                });
                tokio::time::sleep(pause).await;
                let answers = request_input(question_script()).await.unwrap_or_default();
                let picked: Vec<String> = answers
                    .iter()
                    .flat_map(|a| a.labels.iter().cloned())
                    .collect();
                tokio::time::sleep(pause).await;
                let _ = tx.send(AgentEvent::TextDelta {
                    text: format!(
                        "Locked in: **{}**. Proceeding with the plan.",
                        if picked.is_empty() {
                            "your defaults".to_string()
                        } else {
                            picked.join("**, **")
                        }
                    ),
                });
                let _ = tx.send(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                });
            });
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (Ok(event), rx))
            });
            return Ok(stream.boxed());
        }

        // Dev/testing knob: `ZERON_MOCK_REPEAT=N` loops the script body N times
        // before the final Done — long single-reply streams for frame-cost /
        // smoothness measurement (the terminal `Done` is emitted exactly once,
        // at the very end).
        let repeat = std::env::var("ZERON_MOCK_REPEAT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        // Dev/testing knob: `ZERON_MOCK_ERROR=1` appends a scripted error
        // before the terminal Done — the only data-side way to put the
        // transcript ErrorChip on screen with the mock harness.
        let mock_error = std::env::var("ZERON_MOCK_ERROR")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        // Dev/testing knob: `ZERON_MOCK_TABLE=1` appends scripted GFM tables
        // before the terminal Done — a plain 3-column grid plus a wide/uneven
        // one (long prose cell beside short cells, mixed alignment) for
        // table-styling checks against the reference app.
        let mock_table = std::env::var("ZERON_MOCK_TABLE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let done_ix = self
            .script
            .iter()
            .position(|e| matches!(e, AgentEvent::Done { .. }))
            .unwrap_or(self.script.len());
        let (body, tail) = self.script.split_at(done_ix);
        let error_event = mock_error.then(|| AgentEvent::Error {
            message: "Claude usage limit reached — try again after the limit resets.".into(),
        });
        // Dev/testing knob: `ZERON_MOCK_CODE=1` appends rust + ts code blocks
        // (keywords, strings, numbers, comments) plus inline code — for
        // syntax-palette and inline-code styling checks against the reference.
        let mock_code = std::env::var("ZERON_MOCK_CODE")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let code_event = mock_code.then(|| AgentEvent::TextDelta {
            text: concat!(
                "\n### Code check\n\n",
                "The `fold_event_into_parts` helper feeds `writer.sync` on a `120ms` cadence:\n\n",
                "```rust\n",
                "// Fold one event into the accumulated parts.\n",
                "pub fn fold(mut acc: Vec<Part>, event: &AgentEvent) -> Vec<Part> {\n",
                "    let label = \"delta\";\n",
                "    if acc.len() > 128 {\n",
                "        acc.truncate(64); // keep the tail hot\n",
                "    }\n",
                "    acc\n",
                "}\n",
                "```\n\n",
                "```ts\n",
                "// Subscribe and fold on the client.\n",
                "const room = await connect(\"wss://mesh.local\", { retries: 3 });\n",
                "export function fold(parts: Part[], event: AgentEvent): Part[] {\n",
                "    return event.kind === \"delta\" ? [...parts, event] : parts;\n",
                "}\n",
                "```\n\n",
            )
            .into(),
        });
        let table_event = mock_table.then(|| AgentEvent::TextDelta {
            text: "\n### Table check\n\n\
                | Column A | Column B | Column C |\n\
                |---|---|---|\n\
                | a1 | b1 | c1 |\n\
                | a2 | b2 | c2 |\n\n\
                And a wide, uneven one:\n\n\
                | Stage | What happens | p95 |\n\
                |:--|:--|--:|\n\
                | Fold | Events fold into parts and diff into the Loro doc on a 120ms coalesced commit cadence, keeping the oplog RLE-merged across devices | 4.2ms |\n\
                | Sync | Session-room fan-out | 18ms |\n\n"
                .into(),
        });
        // Dev/testing knob: `ZERON_MOCK_MEND=1` appends a link/list-heavy
        // passage — bold-led list items, inline links, emphasis, strikethrough
        // — the shapes whose half-streamed markers the display mend
        // (crates/ui markdown/mend.rs) must hold steady while streaming.
        let mock_mend = std::env::var("ZERON_MOCK_MEND")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let mend_event = mock_mend.then(|| AgentEvent::TextDelta {
            text: concat!(
                "\n### Streaming mend check\n\n",
                "Inline styles hold while text arrives: **bold stays bold**, ",
                "*italic stays italic*, `code stays code`, and ~~this stays struck~~.\n\n",
                "- **Fold** — parts diff into the [Loro doc](https://loro.dev) on a 120ms cadence\n",
                "- **Relay** — commits fan out through the [session room](https://developers.cloudflare.com/durable-objects/) to every device\n",
                "- **Paint** — the [display tree](https://github.com/pulldown-cmark/pulldown-cmark) mends hanging markers in the last block only\n\n",
                "Links above never flash their URLs, and closing markers never reflow the paragraph.\n",
            )
            .into(),
        });
        // Dev/testing knob: `ZERON_MOCK_THINKING=1` appends a markdown-heavy
        // reasoning stream plus a command — the thought-process chip inside a
        // tool-group accordion, for checking that thinking renders styled
        // (bold/lists/inline code) instead of literal `**` markers.
        let mock_thinking = std::env::var("ZERON_MOCK_THINKING")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let thinking_events = mock_thinking
            .then(|| {
                [
                    AgentEvent::ReasoningDelta {
                        text: concat!(
                            "**Planning message rollback helper extraction**\n\n",
                            "The `discard_message` helper needs to roll back optimistic sends on error. ",
                            "Three constraints stand out:\n\n",
                            "1. **Rollback** — remove the user message only when the server never persisted it\n",
                            "2. **Credit state** — clear the exhaustion flag per workspace on auth changes\n",
                            "3. **Paywall copy** — resolve icon conflicts, *then* update the tests\n\n",
                            "Checking call sites before patching:\n\n",
                            "```rust\n",
                            "let rolled_back = ledger.discard(message_id)?;\n",
                            "```\n\n",
                            "The [ledger docs](https://example.com) say discard is idempotent, so ",
                            "re-running the rollback on a retry is safe.",
                        )
                        .into(),
                    },
                    AgentEvent::ToolCall {
                        id: "mock-think-tool".into(),
                        call: zeron_proto::ToolCall::Exec {
                            command: "rg -n walletInsufficient apps/word/src | wc -l".into(),
                        },
                    },
                    AgentEvent::ToolResult {
                        id: "mock-think-tool".into(),
                        is_error: false,
                        output: None,
                        diff: None,
                    },
                ]
            })
            .into_iter()
            .flatten();
        // Dev/testing knob: `ZERON_MOCK_SUBAGENT=1` appends two spawn chips
        // whose nested traffic arrives as tagged `AgentEvent::Subagent`
        // events — the only data-side way to put spawn chips (running → done)
        // AND their openable subagent docs on screen with the mock harness.
        // The second subagent finishes after a beat of nested activity, so a
        // paced run (`ZERON_MOCK_DELAY_MS`) holds a Running chip long enough
        // to observe.
        let mock_subagent = std::env::var("ZERON_MOCK_SUBAGENT")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let subagent_events = mock_subagent
            .then(|| {
                let tag = |parent: &str, event: AgentEvent| AgentEvent::Subagent {
                    parent_tool_use_id: parent.into(),
                    event: Box::new(event),
                };
                let spawn = |id: &str, description: &str, prompt: &str| AgentEvent::ToolCall {
                    id: id.into(),
                    // The claude-driver spawn shape: `Agent: {description}`
                    // with the task in the input (names the chip AND the tab).
                    call: zeron_proto::ToolCall::Unknown {
                        name: format!("Agent: {description}"),
                        input: Some(serde_json::json!({
                            "description": description,
                            "prompt": prompt,
                        })),
                    },
                };
                let resolve = |id: &str| AgentEvent::ToolResult {
                    id: id.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                };
                let done = AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                };
                vec![
                    AgentEvent::TextDelta {
                        text: "\n### Subagent check\n\nFanning out two scouts before the fold rewrite.\n\n".into(),
                    },
                    spawn(
                        "mock-sub-1",
                        "Audit the fold path",
                        "Read crates/doc and list every call site of fold_event_into_parts, checking each holds the byte cap.",
                    ),
                    spawn(
                        "mock-sub-2",
                        "Verify the commit cadence",
                        "Measure the 120ms coalesced commit cadence under a scripted delta burst.",
                    ),
                    // The spawn prompts seed each subagent's opening user
                    // entry (like the claude driver's Task-prompt seeding).
                    tag(
                        "mock-sub-1",
                        AgentEvent::UserMessage {
                            text: "Read crates/doc and list every call site of fold_event_into_parts, checking each holds the byte cap.".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::UserMessage {
                            text: "Measure the 120ms coalesced commit cadence under a scripted delta burst.".into(),
                        },
                    ),
                    tag(
                        "mock-sub-1",
                        AgentEvent::TextDelta {
                            text: "Scanning `crates/doc` for fold call sites.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-sub-1",
                        AgentEvent::ToolCall {
                            id: "sub1-grep".into(),
                            call: zeron_proto::ToolCall::Exec {
                                command: "grep -rn fold_event_into_parts crates".into(),
                            },
                        },
                    ),
                    tag("mock-sub-1", resolve("sub1-grep")),
                    tag(
                        "mock-sub-1",
                        AgentEvent::TextDelta {
                            text: "Three call sites: the live fold, the rebuild, and the subagent sink — every one applies the byte cap before persisting.".into(),
                        },
                    ),
                    resolve("mock-sub-1"),
                    tag("mock-sub-1", done.clone()),
                    // A steer AFTER the subagent settled (claude's queued
                    // SendMessage shape): resurrects the chip and reopens
                    // the frozen transcript for the resumed segment.
                    tag(
                        "mock-sub-1",
                        AgentEvent::UserMessage {
                            text: "One more: confirm the rebuild path holds the byte cap too."
                                .into(),
                        },
                    ),
                    tag(
                        "mock-sub-1",
                        AgentEvent::TextDelta {
                            text: "Rebuild path checked — same cap, applied before persisting."
                                .into(),
                        },
                    ),
                    tag("mock-sub-1", done.clone()),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Driving a 2k-delta burst through the writer.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::ToolCall {
                            id: "sub2-burst".into(),
                            call: zeron_proto::ToolCall::Exec {
                                command: "cargo test -p zeron-doc cadence_burst -- --nocapture".into(),
                            },
                        },
                    ),
                    resolve("mock-sub-2"),
                    tag("mock-sub-2", resolve("sub2-burst")),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Commits land on the 120ms cadence; no commit carried more than one burst.".into(),
                        },
                    ),
                    // A parent→subagent steer: splits the transcript into a
                    // user entry + fresh assistant segment (like the claude
                    // driver's tagged user text blocks).
                    tag(
                        "mock-sub-2",
                        AgentEvent::UserMessage {
                            text: "Also verify the cadence holds while a steer lands mid-burst.".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Re-running with a mid-burst steer injected.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-sub-2",
                        AgentEvent::ToolCall {
                            id: "sub2-steer-burst".into(),
                            call: zeron_proto::ToolCall::Exec {
                                command: "cargo test -p zeron-doc cadence_steer -- --nocapture"
                                    .into(),
                            },
                        },
                    ),
                    tag("mock-sub-2", resolve("sub2-steer-burst")),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "Watching the commit log while the burst drains: ".into(),
                        },
                    ),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 1 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 2 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 3 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 4 clean, ".into() }),
                    tag("mock-sub-2", AgentEvent::TextDelta { text: "batch 5 clean — ".into() }),
                    tag(
                        "mock-sub-2",
                        AgentEvent::TextDelta {
                            text: "every window under 120ms.\n\nSteer landed between commits; the cadence held.".into(),
                        },
                    ),
                    tag("mock-sub-2", done),
                ]
            })
            .into_iter()
            .flatten();
        // Dev/testing knob: `ZERON_MOCK_AGENTS=1` appends three spawn chips
        // — one whose tagged subagent traffic settles fast (done), one that
        // errors (failed), and one whose nested traffic is paced by
        // `ZERON_MOCK_SUBAGENT_DELAY_MS` so it stays RUNNING through the
        // demo (set that knob to e.g. 2000 to hold it). Unlike
        // `ZERON_MOCK_SUBAGENT` (which ends all of its subagents before the
        // run's Done), this fixture leaves one live at run end — the shape
        // the dock strip / Agents panel / sidebar children are built for.
        let mock_agents = std::env::var("ZERON_MOCK_AGENTS")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let agent_events = mock_agents
            .then(|| {
                let tag = |parent: &str, event: AgentEvent| AgentEvent::Subagent {
                    parent_tool_use_id: parent.into(),
                    event: Box::new(event),
                };
                let spawn = |id: &str, description: &str, prompt: &str, ty: &str| {
                    AgentEvent::ToolCall {
                        id: id.into(),
                        call: zeron_proto::ToolCall::Unknown {
                            name: format!("Agent: {description}"),
                            input: Some(serde_json::json!({
                                "description": description,
                                "prompt": prompt,
                                "subagent_type": ty,
                            })),
                        },
                    }
                };
                let resolve = |id: &str| AgentEvent::ToolResult {
                    id: id.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                };
                let done = |status: DoneStatus| AgentEvent::Done {
                    status,
                    result: None,
                    error: None,
                    session_id: None,
                };
                vec![
                    AgentEvent::TextDelta {
                        text: "\n### Agents pass\n\nThree scouts are out; the fold rewrite waits on the slowest.\n\n".into(),
                    },
                    spawn(
                        "mock-agt-run",
                        "Sweep sidebar state hues",
                        "Check every session-state hue in the sidebar against the palette.",
                        "explorer",
                    ),
                    spawn(
                        "mock-agt-done",
                        "Audit composer padding",
                        "Verify the composer column keeps its 4px rhythm at every width.",
                        "reviewer",
                    ),
                    spawn(
                        "mock-agt-fail",
                        "Probe the dock edge",
                        "Measure the dock strip's gap above the composer pill.",
                        "reviewer",
                    ),
                    resolve("mock-agt-run"),
                    resolve("mock-agt-done"),
                    resolve("mock-agt-fail"),
                    tag(
                        "mock-agt-run",
                        AgentEvent::UserMessage {
                            text: "Check every session-state hue in the sidebar against the palette.".into(),
                        },
                    ),
                    tag(
                        "mock-agt-done",
                        AgentEvent::UserMessage {
                            text: "Verify the composer column keeps its 4px rhythm at every width.".into(),
                        },
                    ),
                    tag(
                        "mock-agt-fail",
                        AgentEvent::UserMessage {
                            text: "Measure the dock strip's gap above the composer pill.".into(),
                        },
                    ),
                    // The quick one finishes inside the first beats.
                    tag(
                        "mock-agt-done",
                        AgentEvent::TextDelta {
                            text: "Padding holds at every measured width — 4px rhythm intact.".into(),
                        },
                    ),
                    tag("mock-agt-done", done(DoneStatus::Completed)),
                    // The probe one errors almost immediately.
                    tag(
                        "mock-agt-fail",
                        AgentEvent::TextDelta {
                            text: "Dock probe hit a stale layout cache.".into(),
                        },
                    ),
                    tag("mock-agt-fail", done(DoneStatus::Errored)),
                    // The runner streams slowly — every tagged beat keeps
                    // its chip Running while the parent turn has settled.
                    tag(
                        "mock-agt-run",
                        AgentEvent::TextDelta {
                            text: "Sweeping sidebar rows for hue drift.\n\n".into(),
                        },
                    ),
                    tag(
                        "mock-agt-run",
                        AgentEvent::ToolCall {
                            id: "agt-run-read".into(),
                            call: zeron_proto::ToolCall::ReadFile {
                                path: "crates/ui/src/shell/spaces.rs".into(),
                            },
                        },
                    ),
                    tag("mock-agt-run", resolve("agt-run-read")),
                    tag(
                        "mock-agt-run",
                        AgentEvent::ToolCall {
                            id: "agt-run-grep".into(),
                            call: zeron_proto::ToolCall::Search {
                                pattern: "SessionState::".into(),
                                path: Some("crates/ui/src".into()),
                            },
                        },
                    ),
                    tag("mock-agt-run", resolve("agt-run-grep")),
                    tag(
                        "mock-agt-run",
                        AgentEvent::TextDelta {
                            text: "Working through each state; sky Working reads correctly on the live card.".into(),
                        },
                    ),
                    // No Done for mock-agt-run: it stays Running past the
                    // parent's settle, the whole point of the fixture.
                ]
            })
            .into_iter()
            .flatten();
        // With the code knob, also exercise a MULTILINE Exec command — the
        // round-9 chip breaker shape ("set -e\nfixture_in_original=0"): the
        // Run chip must stay one 30px line.
        let code_tool_events = mock_code
            .then(|| {
                [
                    AgentEvent::ToolCall {
                        id: "mock-code-tool".into(),
                        call: zeron_proto::ToolCall::Exec {
                            command: "set -e\nfixture_in_original=0\ngrep -rn \"veil\" crates/ui/src | wc -l".into(),
                        },
                    },
                    AgentEvent::ToolResult {
                        id: "mock-code-tool".into(),
                        is_error: false,
                        output: None,
                        diff: None,
                    },
                ]
            })
            .into_iter()
            .flatten();
        // Dev/testing knob: `ZERON_MOCK_TOOLS=1` appends a representative
        // tool-family mix for palette/typing QA: explore (read/search/glob),
        // change (edit), neutral (exec/todo/unknown), a delegate spawn, and
        // one FAILED exec (icon + trailing tag in danger, row stays muted).
        let mock_tools = std::env::var("ZERON_MOCK_TOOLS")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0");
        let tool_mix_events = mock_tools
            .then(|| {
                use zeron_proto::ToolCall;
                let call = |id: &str, call: ToolCall| AgentEvent::ToolCall {
                    id: id.into(),
                    call,
                };
                let ok = |id: &str| AgentEvent::ToolResult {
                    id: id.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                };
                vec![
                    AgentEvent::TextDelta {
                        text: "\n### Tool pass\n\nSweeping the workspace, then patching and probing.\n\n".into(),
                    },
                    call(
                        "mix-read",
                        ToolCall::ReadFile {
                            path: "crates/ui/src/transcript.rs".into(),
                        },
                    ),
                    call(
                        "mix-grep",
                        ToolCall::Search {
                            pattern: "tool_icon_path".into(),
                            path: Some("crates/ui".into()),
                        },
                    ),
                    call(
                        "mix-glob",
                        ToolCall::Glob {
                            pattern: "crates/ui/src/**/*.rs".into(),
                        },
                    ),
                    call(
                        "mix-web",
                        ToolCall::WebSearch {
                            query: "gpui text_color hsla".into(),
                        },
                    ),
                    call(
                        "mix-exec",
                        ToolCall::Exec {
                            command: "cargo test -p zeron-ui --lib".into(),
                        },
                    ),
                    call(
                        "mix-edit",
                        ToolCall::EditFile {
                            path: "crates/ui/src/transcript.rs".into(),
                            old_string: None,
                            new_string: None,
                        },
                    ),
                    call(
                        "mix-todo",
                        ToolCall::Todo {
                            items: vec![
                                zeron_proto::TodoItem {
                                    text: "type kind-other tools by title".into(),
                                    done: true,
                                },
                                zeron_proto::TodoItem {
                                    text: "tint icons by tool family".into(),
                                    done: false,
                                },
                            ],
                        },
                    ),
                    call(
                        "mix-unknown",
                        ToolCall::Unknown {
                            name: "codex-research".into(),
                            input: Some(serde_json::json!({"query": "tool palette"})),
                        },
                    ),
                    call(
                        "mix-spawn",
                        ToolCall::Unknown {
                            name: "Agent: Sweep the sidebar states".into(),
                            input: Some(serde_json::json!({
                                "description": "Sweep the sidebar states",
                                "prompt": "Check every session-state hue against the palette",
                            })),
                        },
                    ),
                    call(
                        "mix-fail",
                        ToolCall::Exec {
                            command: "cargo test -p zeron-ui tool_rows".into(),
                        },
                    ),
                    ok("mix-read"),
                    ok("mix-grep"),
                    ok("mix-glob"),
                    ok("mix-web"),
                    ok("mix-exec"),
                    ok("mix-edit"),
                    ok("mix-todo"),
                    ok("mix-unknown"),
                    ok("mix-spawn"),
                    AgentEvent::ToolResult {
                        id: "mix-fail".into(),
                        is_error: true,
                        output: Some("error[E0308]: mismatched types".into()),
                        diff: None,
                    },
                ]
            })
            .into_iter()
            .flatten();
        let events: Vec<Result<AgentEvent, HarnessError>> = body
            .iter()
            .cycle()
            .take(body.len() * repeat)
            .cloned()
            .chain(thinking_events)
            .chain(code_tool_events)
            .chain(subagent_events)
            .chain(agent_events)
            .chain(tool_mix_events)
            .chain(code_event)
            .chain(table_event)
            .chain(mend_event)
            .chain(error_event)
            .chain(tail.iter().cloned())
            .map(Ok)
            .collect();
        // Dev/testing knob: `ZERON_MOCK_CHARS=N` re-chunks every TextDelta
        // into N-char deltas, so `ZERON_MOCK_DELAY_MS` paces *characters*
        // instead of whole scripted blocks — delta boundaries then land inside
        // inline markers and links, which is the streaming shape real
        // harnesses produce and the display mend exists for.
        let chunk_chars = std::env::var("ZERON_MOCK_CHARS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0);
        let events: Vec<Result<AgentEvent, HarnessError>> = match chunk_chars {
            None => events,
            Some(n) => events
                .into_iter()
                .flat_map(|event| match event {
                    Ok(AgentEvent::TextDelta { text }) => {
                        let chars: Vec<char> = text.chars().collect();
                        chars
                            .chunks(n)
                            .map(|c| {
                                Ok(AgentEvent::TextDelta {
                                    text: c.iter().collect(),
                                })
                            })
                            .collect::<Vec<_>>()
                    }
                    // Reasoning streams the same char-paced way — thought
                    // chips have their own streaming display (mend + live
                    // tail), which whole-block deltas would never exercise.
                    Ok(AgentEvent::ReasoningDelta { text }) => {
                        let chars: Vec<char> = text.chars().collect();
                        chars
                            .chunks(n)
                            .map(|c| {
                                Ok(AgentEvent::ReasoningDelta {
                                    text: c.iter().collect(),
                                })
                            })
                            .collect::<Vec<_>>()
                    }
                    other => vec![other],
                })
                .collect(),
        };
        // Dev/testing knob: `ZERON_MOCK_SUBAGENT_DELAY_MS` paces TAGGED
        // (subagent) events on their own clock — the parent turn settles at
        // `ZERON_MOCK_DELAY_MS` speed while the background subagents stream
        // on slowly, which is exactly the eager-done shape live tabs are
        // observed under (and the only way a rig click can reliably land
        // inside a subagent's streaming window).
        let sub_delay_ms = std::env::var("ZERON_MOCK_SUBAGENT_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        if delay_ms == 0 && sub_delay_ms.is_none() {
            return Ok(futures::stream::iter(events).boxed());
        }
        Ok(futures::stream::iter(events)
            .then(move |event| async move {
                let pause = match (&event, sub_delay_ms) {
                    (Ok(AgentEvent::Subagent { .. }), Some(ms)) => {
                        std::time::Duration::from_millis(ms)
                    }
                    _ => delay,
                };
                tokio::time::sleep(pause).await;
                event
            })
            .boxed())
    }
}

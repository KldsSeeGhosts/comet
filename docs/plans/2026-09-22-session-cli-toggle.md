# Same-session Chat / CLI switching

Research and implementation proposal, 2026-09-22. Inspected Noches at
`4ac9880db699e4cc9ba390ce7b65129525341c51` in the isolated
`codex/session-cli-toggle` worktree. This document does not enable the feature.

## What Super actually does

Super supports changing an existing session between its rendered conversation
and the provider's interactive CLI, in the same tab and worktree. The documented
switch requires an idle session and a validated provider resume target.

| Source inspected | Evidence | What it establishes |
| --- | --- | --- |
| [Super website](https://super.engineering/) | Sessions are described as "Terminal + chat"; tasks retain their worktree, history, and review context. | Product context, not process-level implementation. |
| [Terminal and chat](https://super.engineering/docs/terminal-and-chat/#switch-chat-ui-and-terminal-in-place) | The tab view menu has "Switch to Chat View" and "Switch to Terminal View". Busy sessions cannot switch. Missing or invalid resume targets disable the action. | The switching contract. |
| [Official X post, September 21](https://x.com/superdoteng/status/2102119933828284643) | "TUI or chat UI. Toggle mid-session." The video visibly opens a compact menu beside the active session tab, selects "Switch to Terminal View", and shows Grok's native terminal in the conversation area. | Direct visual evidence of the requested interaction. |
| [Official X post, September 17](https://x.com/superdoteng/status/2100627591371641077) | "New Super chat UI. Fluid replies. Smooth transitions. Every message in motion." | Recent conversation presentation work; this post does not establish switching internals. |
| [Session history and restore](https://super.engineering/docs/session-history-and-restore/) | The app stores provider resume identifiers and worktree/tab metadata. Provider history remains provider-owned. | Same-session identity is distinct from app layout and rendered history. |

The documented switch supports Claude Code, Codex, Cursor, Grok, OpenCode, Pi,
Oh My Pi, and Kimi Code when a usable resume target exists. This is Super's
support list, not a claim about Noches adapters.

"Mid-session" does not mean "during a running turn". The documentation explicitly
disallows switching while busy. Neither the video nor the docs proves that both
views share the same operating-system process, or exposes how CLI history is
reconciled. Treat those as implementation questions, not observed facts.

Super's general cold-restore documentation permits a fresh-session fallback.
Its explicit view-switch documentation instead disables switching when the
provider session cannot be validated. Noches should preserve that distinction.

The earlier [terminal research](../../super-analysis/05-terminal-and-cli-views.md)
already described this behavior. The live website and X video corroborate it.

## Proposed Noches interaction

Put a compact `Chat | CLI` segmented control beside the session title in the pane
header. Keep the existing provider mark and status indicator. The active segment
uses the existing neutral selected treatment; the other uses the normal hover
wash. On narrow panes, collapse it into a view-menu item with the same action.
The segmented control is a Noches proposal, not an exact copy of Super's menu.

Selecting CLI replaces the transcript and composer inside that pane with the
provider's actual interactive terminal. Selecting Chat restores the transcript
and the unsent composer draft. The session, tab, pane, sidebar item, worktree,
branch, and review selection keep their identities. The ordinary shell drawer
remains an independent tool.

| Condition | Presentation and behavior |
| --- | --- |
| Established, idle, supported session | Both segments available. |
| New chat without a durable provider session | CLI disabled: "Send a message before opening this session in CLI." |
| Working or waiting for an answer | Switch disabled: "Finish or stop the current turn to switch views." |
| Provider has no verified round-trip support | CLI unavailable with a provider-specific explanation. |
| Owning device disconnected | Switch disabled; retain the current view and cached transcript. |
| Switch in progress | Show a short busy state in the control; reject duplicate clicks. |
| Resume, teardown, or history loading fails | Show the error in the same pane; retain the known binding and draft. Never create an unrelated conversation. |

Preserve transcript scroll position and terminal scrollback across ordinary view
navigation. Returning after CLI activity should make the newly imported turns
discoverable without unconditionally jumping a reader who was inspecting older
messages. Move keyboard focus only after the destination view is ready. Do not
send a prompt as a side effect of switching.

Start with idle-only switching, matching Super's documented contract. Unknown
CLI activity is not idle. A provider without a reliable activity signal needs
that signal before it can offer the seamless return button; an explicit
stop-and-return action would be a separate, less seamless behavior.

## What Noches already has

| Area | Existing code | Reuse and limitation |
| --- | --- | --- |
| Durable provider identity | `crates/proto/src/entities.rs`: `Chat.harness_session_id`, `harness_session_cwd`; `crates/engine/src/workspace_host.rs`: session persistence | Reuse the exact ID and cwd. Bind the provider and effective profile/store too; mutable chat settings alone cannot prove which provider created an old ID. |
| Resume selection | `crates/engine/src/sessions.rs`: `resume_for`, `remember_harness_session` | Already checks cwd and honors an empty-ID tombstone. The method is private and not a complete validated CLI binding. |
| Turn state | `sessions.rs`: `turn_in_flight`, `RunHandle`, `RuntimeConfig`, `interrupt` | Distinguishes a busy turn from a warm idle process. Existing interruption is not a transactional view handoff. |
| Durable sends and queues | `crates/engine/src/doc_host.rs`: `drain_queue`, `execute`, steering paths | Must participate in the ownership gate. Disabling one composer cannot stop remote or queued sends. |
| Managed PTY | `crates/engine/src/terminals.rs` | Portable PTY, bounded replay, input, resize, close, and process lifetime already exist. Hiding a terminal does not close it. |
| Host routing | `crates/engine/src/rpc.rs`, `crates/rpc/src/lib.rs` | Terminal RPCs are relay-forwardable. New session-switch RPCs must execute on the session's host. |
| Terminal rendering | `crates/ui/src/terminal/panel.rs`, `emulator.rs`, `view.rs` | Native GPUI rendering exists. `reserve_tab_for_chat` and `attach_reserved_session` can attach a host-created PTY. |
| Pane identity/layout | `crates/workspace/src/lib.rs`: `PaneState`, `PaneMode`; `crates/ui/src/pane/*` | Layout already models chat and terminal modes, but split terminal bodies currently display "Terminal panes are not yet available". |
| Pane chat state | `crates/ui/src/pane/mod.rs`, `crates/ui/src/shell/panes.rs` | Transcript and composer entities are cached by pane. Current pruning only retains Chat-mode surfaces; a naive mode flip can discard the draft. |
| Historical presentation | `crates/engine/src/transcript_history.rs` | Tracks Loro replay provenance for rendering. It is not an importer for provider CLI history. |

The terminal panel currently resolves its chat through global selection. Reusing
it unchanged for multiple session panes would risk targeting the wrong session
after focus changes. Add an explicit bound-chat mode or factor out a terminal
session view that receives immutable chat, device, and terminal IDs.

The single-chat route also has separate rendering and composer ownership from
the split-workspace route. Both must use the same switching action and session
controller. A feature that works only while split is incomplete.

## Two viable runtime strategies

### Resume handoff

The engine releases an idle structured runtime, starts the interactive CLI with
the exact saved provider session, and later resumes the structured runtime after
the CLI has exited. Provider history supplies turns entered in the CLI.

This fits the current architecture most closely. It still requires strict
resume, an ownership gate, provider activity reporting, and history import.
There is no generic operation that converts a process's structured stdout into
the provider's interactive TUI.

### Attach both clients to one provider server

The engine keeps the provider server and event subscription alive, while an
embedded native TUI attaches to that same server and session. Chat displays the
same events and disables its input while CLI owns interaction. This may avoid
process restarts and history gaps for providers that support it.

Read-only local checks found:

- `codex-cli 0.155.1`: `codex resume --help` advertises an explicit session ID
  and `--remote` with WebSocket or Unix-socket endpoints. `codex app-server
  --help` advertises `--listen`; Noches currently launches app-server over stdio.
- `opencode v2.0.12`: `opencode --help` advertises `--server` and `--session`.
  Noches already starts a private authenticated HTTP/SSE server, but its lifetime
  is owned by the current run.
- `claude --help` could not run because `claude` was absent from this shell's
  PATH. Noches has additional executable discovery, so this does not prove the
  provider is absent from the device.

These help checks establish advertised flags only. They do not prove concurrent
client event delivery, exact-ID attachment, approval ownership, or compatibility
with the app's effective configuration. Do not enable attach mode until those
properties pass a controlled probe. Any provider endpoint must stay private to
the owning device; terminal traffic can continue using Noches's existing relay.

## Provider readiness

| Noches provider | Current transport | Required proof before enabling CLI |
| --- | --- | --- |
| Claude Code | Native `--print` stream-json | Exact session ID usable by interactive Claude, effective config directory preserved, history importer and authoritative CLI activity signal. Current resume command construction lives in `claude/mod.rs`. |
| Codex | Native app-server over stdio | Exact thread round trip; strict resume; either attach support with shared events or history reconciliation after process handoff. First candidate for a controlled attach probe. |
| OpenCode | Native HTTP/SSE, v1 and v2 handling | TUI connects to the same authenticated server and ID; lifecycle and auth behavior verified per supported CLI generation. Promising attach candidate. |
| Grok | ACP via `grok agent stdio` | ACP session ID maps to a native CLI resume target; activity and history round trip. Super's demo proves their integration, not ours. |
| Pi | Community `pi-acp` adapter | Map adapter session ID to Pi's actual session file and branch. Do not assume the ACP ID is a native CLI ID. |
| Cursor | Pinned `@cursor/sdk` Node shim with Noches-owned storage | Prove SDK store/ID compatibility with `cursor-agent` first. `cursor/state.rs` and `shim.mjs` own the store and lease; an SDK agent ID cannot simply be passed to an unrelated CLI. |
| Devin | Native ACP | Establish native CLI identity, history, and activity compatibility. Keep unavailable until verified. |
| Hermes | Native ACP | Same identity/history/activity proof; independent of general ACP resume support. |
| Antigravity | `agy_acp_server` | Establish that a compatible interactive CLI exists and can resume this exact conversation. |
| Mock | Test driver | Exercise ownership and failure transitions deterministically; never show in production. |

Oh My Pi and Kimi Code appear in Super's support list but are not current
`HarnessId` variants in this checkout. Adding those providers is separate work.

## Engine contract

Add a provider capability interface, preferably in `crates/harness`, with
operations conceptually equivalent to:

```text
validate_native_session(binding) -> validated binding or explicit error
native_launch(binding, effective_config) -> executable + argv + environment
observe_native_session(binding) -> activity + identity + history updates
load_history(binding, cursor) -> normalized entries + next cursor
```

Providers may implement an attach strategy instead of process handoff. Defaults
must report unsupported. No implementation may use "latest session", a history
picker, or a fresh conversation as the result of this action.

The host owns a per-chat controller and durable binding:

```text
binding = chat_id + device_id + provider + provider_session_id
          + cwd + effective profile/store reference
state   = Chat | SwitchingToCli | Cli | SwitchingToChat | RecoveryRequired
epoch   = monotonically increasing ownership generation
```

Keep a separate provider activity value: idle, busy, awaiting input, unknown.
Include the terminal ID only while it refers to a live PTY. Do not persist a
bare terminal ID as if it were a resumable provider conversation. Keep credentials
out of synchronized bindings and logs.

Suggested host API, with names finalized during implementation:

```text
GetSessionSurface(chatId) -> state, activity, capabilities, unavailableReason
SwitchSessionSurface(chatId, target, expectedEpoch, operationId) -> result
WatchSessionSurface(chatId) -> authoritative updates
```

Requests carry identity and intent, not caller-provided shell commands or cwd.
The host resolves the executable, exact arguments, config, and working directory.
Launch the program directly in a PTY with an argument vector. The existing
project-action shell-script path is useful precedent for PTY attachment but
should not be repurposed as an arbitrary CLI command RPC. Preserve the effective
model, reasoning, permission, and sandbox settings; never add broader permission
flags merely to make the switch work.

### Chat to CLI

1. Acquire the same per-chat ownership gate used by dispatch, steering, and queue
   draining. Check the expected epoch and deduplicate the operation ID.
2. Resolve the host-owned binding. Validate provider compatibility, history,
   cwd, executable, effective account/store, and idle state. Prevent a queued
   prompt or background wake from taking ownership during the handoff.
3. Reserve the transition. Preserve the composer, attachments, and transcript
   entities. Defer queue delivery without consuming or acknowledging its rows.
4. For a handoff, release the warm runtime and wait for confirmed process and
   stream teardown. `interrupt()` currently has a bounded settle wait and can
   return without proving teardown. A timeout must fail the switch.
5. Start the exact native session in the host PTY, or attach to the same provider
   server. Confirm identity and readiness; a PTY opening successfully is not
   evidence that resume succeeded.
6. Commit CLI ownership and publish it to all observers. Only then switch the
   pane body and transfer focus. A failure keeps a recoverable state and the
   original binding; it must not silently launch a new conversation.

### CLI to Chat

1. Serialize against PTY input and other switch requests. Reject busy,
   awaiting-input, or unknown activity. Quiesce terminal input before teardown.
2. Detach the TUI for a proven shared-server strategy, or gracefully stop the
   native runtime and confirm it has released the session. Wait for final
   provider output and history writes.
3. Read and reconcile any CLI-only turns into the existing SessionDoc. Preserve
   stable provider message/tool IDs, ordering, compaction/branch semantics, and
   attachment references. Do not parse ANSI screen contents into a transcript.
4. Load the structured session with strict identity validation and without
   sending a prompt. A handoff failure keeps Chat non-editable with an explicit
   retry/recovery action. History failure must not unlock a stale transcript.
5. Commit Chat ownership, restore the same draft, and allow queue delivery
   according to the existing queue policy. CLI keystrokes must never become a
   duplicated chat send.

Normal navigation away from a CLI pane only detaches the view. Closing a view,
stopping a run, switching its input owner, and deleting a conversation need
distinct engine operations. Other panes/devices viewing the same chat observe
the shared ownership state and cannot independently launch a second writer.

### History and restart behavior

Use provider-native history readers or retained structured events. Record a
handoff checkpoint and an imported-message ledger so repeated view changes and
crash retries cannot duplicate old turns. Existing Noches message IDs are
app-generated, so mapping pre-handoff provider messages requires explicit
provenance or a validated history boundary, not text equality. Handle partial
JSONL writes, tool results, compacted histories, and session branches explicitly.

Persist the requested view and resumable binding separately from current process
ownership. On restart, validate actual processes and provider storage before
enabling input. Reuse a live host-owned PTY when reconnecting to the same engine;
do not trust an old terminal ID after the engine restarts. A crashed native run
must be visible and recoverable without a fresh-session fallback.

## Implementation sequence and integration boundaries

1. **Provider probes.** Use disposable sessions to verify exact-ID round trips,
   CLI busy state, history visibility, and effective config for Claude and Codex.
   Probe Codex/OpenCode attachment before choosing their strategy. Never test by
   switching an unrelated active user conversation.
2. **Engine ownership and strict resume.** Add the shared gate, operation
   deduplication, transition states, queue deferral, rollback, and host RPCs.
   Codex, OpenCode, and ACP currently have fresh-start fallback paths. The new
   strict path must not inherit those defaults.
3. **Provider history and PTY integration.** Implement one complete provider
   round trip, including idle detection and import, then the second. A toggle
   that opens a CLI but loses its new turns on return is not a completed slice.
4. **Pane integration.** Add an explicitly bound native-session view, retain chat
   entities across mode changes, fill the terminal pane outlet, and connect the
   header control. Cover the single-chat route as well as split panes.
5. **Restore and remote validation.** Reconnect the same PTY on renderer restart,
   recover provider state after engine restart, and route switching through the
   owning host. Roll out additional providers only after their round-trip tests.

Keep most new code in dedicated modules, for example
`crates/engine/src/session_surface.rs`, provider-specific native-session modules,
and `crates/ui/src/shell/session_surface.rs`. Existing files still need small
integration changes. The likely overlap with the current UI agents is
`shell.rs`, `shell/panes.rs`, `pane/chrome.rs`, `pane/render.rs`, and
`pane/mod.rs`. Rebase and resolve those call sites after their work lands.

## Acceptance checks

- Chat -> CLI -> Chat retains the same Noches chat, native session, cwd, branch,
  review context, and unsent draft. A CLI-entered turn appears once in chat, and
  the next chat prompt sees that turn's context.
- Busy, awaiting-input, unknown, unsupported, missing-ID, wrong-cwd, offline,
  missing-binary, and wrong-store cases never launch a fresh session.
- Simultaneous toggle requests, a racing send, queued sends, and background
  agent wakes cannot create concurrent writers. Retrying an operation reuses its
  result rather than creating another PTY.
- Failed spawn, failed resume, teardown timeout, truncated history, and renderer
  disconnect leave the draft and binding recoverable. No orphan process remains.
- Two visible panes bound to different chats receive their own keyboard input,
  resize events, and output. Two views of the same chat share ownership state.
- Repeated toggles do not leak processes or duplicate transcript entries.
  Restart while switching recovers to an explicit, validated state.
- Validate ANSI alternate screen, keyboard shortcuts, selection, scrollback,
  resize, and theme rendering with real native CLIs. Test Windows launch behavior
  separately from Unix when that platform is enabled.

Use fake provider processes and temporary stores for lifecycle/failure tests,
then a controlled real-provider smoke test. Planned suites are `zeron-harness`,
`zeron-engine`, `zeron-workspace`, and `zeron-ui`, with focused tests first. This
research-only change does not require a Cargo build.

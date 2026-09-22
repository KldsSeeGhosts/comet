# Same-session Chat / CLI switching

Research and implementation notes, 2026-09-22. Inspected Noches at
`4ac9880db699e4cc9ba390ce7b65129525341c51` in the isolated
`codex/session-cli-toggle` worktree.

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

## Implemented interaction

A compact `Chat | CLI` toggle appears beside the session title on both the
single-chat and split-pane routes. CLI replaces the conversation body with the
provider's native terminal. The pane keeps its original transcript and composer
entities, including an unsent chat draft. The ordinary shell drawer is separate.
The terminal is bound to its chat rather than the sidebar's current selection.

The first adapters support Claude Code and Codex on Unix hosts. Other providers,
Windows hosts, fresh chats without a provider session, and busy chat turns show
an unavailable reason. The host validates the history file and working directory
again before launch. A failed validation never opens a new conversation.

This is a process handoff. It resumes the exact provider session in a new native
CLI process. It does not attach two clients to one running provider server.

## Ownership and recovery

`GetSessionSurface` returns the view, terminal attachment, availability, and
reason. `SwitchSessionSurface` accepts a chat ID and a target view. Both use the
existing host-device RPC forwarding. Executables, arguments, and cwd come from
the owning engine, never a caller-supplied shell command.

A shared per-chat gate serializes switching, chat dispatch, steering, and native
terminal input. Repeated requests for the current view are idempotent. CLI
ownership blocks queue draining and competing chat sends. Automatic engine
updates wait until native terminals return to Chat, even when their panes are
hidden.

Before native launch, the engine records a durable binding with the provider,
exact session ID, effective request, history filename, byte offset, and prefix
hash. It interrupts the warm structured runtime and waits for its child to exit.
The executable and argument vector run directly in the existing PTY backend.
The native launch inherits the host's provider configuration environment.

Returning to Chat requires an idle CLI or an exited process. The engine prevents
new terminal input, terminates and reaps the idle CLI, validates the history
prefix, imports only records after the checkpoint, and confirms transcript
persistence before releasing ownership. Deterministic message IDs make retries
idempotent. A torn file, rewritten prefix, or failed persistence keeps the chat
locked so the original provider history can be reconciled on retry.

A restarted engine retains the CLI binding and can reconcile its saved history
through the Chat toggle. PTY processes themselves are not restored. Once a chat
has used native mode, structured sends retain the exact provider binding. Codex
resume errors and replacement thread IDs cannot fall back to a fresh thread.
Claude uses its explicit `--resume` path, which has no app-side fresh fallback.

## Provider activity and imported history

- Codex uses persisted `task_started`, `user_message`, `task_complete`, and
  `turn_aborted` lifecycle events. Only `user_message` events become user chat
  entries; injected instructions in `response_item` records are excluded.
- Claude uses per-launch SessionStart, UserPromptSubmit, PreToolUse,
  PermissionRequest, and Stop hooks. These append activity to an engine-owned
  file without changing the user's settings files. Sidechain records are
  excluded from the main transcript.
- A submitted Enter must have a subsequent provider acknowledgement before
  Chat becomes available. Unknown activity stays locked. Slash-command menus
  and pasted multiline drafts can therefore require exiting the CLI first.
- Text, readable Claude reasoning, and tool calls/results are imported. Tool
  chips follow the existing document policy: inputs are sanitized and full
  outputs remain in provider history. Native attachment types are represented
  by a notice when the chat renderer has no matching import path.

History lookup is bounded to 50,000 directory entries and a 64 MiB session file.
The importer requires complete JSONL records and an unchanged pre-CLI prefix.
Provider format changes or history compaction can block automatic reconciliation;
the feature does not guess around those failures or delete source history.

## Validation scope

The implementation has deterministic tests for concurrent switches, competing
chat sends, pending CLI input, exact argv handling, process teardown, repeated
imports, interrupted history writes, restart recovery, and persisted imported
messages. Harness fixtures cover strict Codex resume failure, replacement IDs,
and runtime teardown acknowledgement. A GPUI regression checks that view changes
retain the pane, transcript, composer, and draft.

The automated tests use isolated fake provider processes and synthetic provider
history. They do not constitute a live authenticated Claude/Codex round trip.
The real installed Codex CLI's resume flags were checked. Claude's executable
was unavailable on the research shell's PATH. Live provider compatibility and
visual QA remain useful follow-up checks before merging.

## Future adapters

OpenCode's installed v2 CLI advertises `--server` and `--session`, and Codex
0.155.1 advertises `resume --remote` plus app-server listen endpoints. Those may
support shared-server attachment, but concurrent events, authentication, and
approval ownership still need verification. ACP providers also need proof that
their adapter session IDs map to native CLI history before enabling this toggle.

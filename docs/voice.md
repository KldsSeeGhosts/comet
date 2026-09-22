# Noches voice

Noches connects its native microphone and speaker to OpenAI's `gpt-live-1`.
A `gpt-5.6-luna` Responses backend reads app state and executes Noches actions
while GPT-Live continues the conversation. The coding agents remain the same
agents you already use in your sessions.

## Start a call

1. Open **Voice** at the bottom of the sidebar, or search **Open voice controls**
   in the command palette.
2. Copy an OpenAI project API key and choose **Paste API key**. Noches saves it
   through the platform credential store. `OPENAI_API_KEY` in the app's process
   environment takes precedence over the saved key. A ChatGPT subscription login
   is not used for this integration.
3. Select **Start voice** and grant microphone permission. Use headphones. This
   first native audio implementation does not perform acoustic echo cancellation.
4. Speak, navigate, and continue talking. **Hide** closes the panel while the call
   stays connected. Escape also hides the panel. Hiding declines any pending
   action confirmation. **End** in the sidebar hangs up. Voice controls also appear
   in the settings sidebar.

`Cmd+Shift+H` starts/ends a call and `Cmd+Shift+U` mutes/unmutes. Use Ctrl on
Windows/Linux. These defaults yield to customized app shortcuts. Voice shortcuts
are currently fixed rather than editable rows in Shortcut settings.

Audio and tool-requested app context go to OpenAI. Noches does not write call audio
or captions to disk. GPT-Live session storage is disabled. Keys never enter tool
results or `ui-settings.json`. Mute silences outgoing microphone audio locally;
it does not end billing or stop the assistant's playback. Hangup immediately
silences local input/output and revokes pending tool requests, then attempts a
graceful close for final usage. Actions already submitted to the engine can still
finish. There is no automatic reconnect that could repeat a mutation.

## What you can ask

- "What's running? Open the session about the failing CI test."
- "Read its latest answer. Tell it to add a regression test."
- "Start a session in this project, in an isolated worktree, and fix the build."
- "Which models can Codex use on the server?"
- "Show the diff. Open src/main.rs."
- "Answer that agent's question with the second option."
- "Rename this session. Archive it."
- "Open settings. Switch to the light theme."
- "Show the available pane actions and split this view."

The action catalog covers session discovery, transcripts, creation, prompting,
steering, interruption, input answers, rename/archive/delete/configuration;
project/device discovery and management; repository refs and worktrees;
file browsing/reading/writing; terminals and project actions; queued messages;
agent/model/account discovery; navigation, appearance, and composer drafts.

`list_native_actions` additionally discovers the currently focused view's GPUI
actions and their parameter schemas. `dispatch_native_action` invokes those same
handlers used by keyboard shortcuts. This includes pane and editor operations
without duplicating their implementation. Its result means dispatched, not that
an asynchronous operation finished. It checks the selection and focus again when
the user confirms the action.

This is broad application control, not a claim of complete mouse-control parity.
Mouse-only controls without a catalog entry or GPUI action still require a new
adapter. Authentication and credential entry stay in the normal UI. Dangerous
operations and generic native action dispatch require confirmation in Noches.
The voice backend cannot grant itself that confirmation.

## Implementation

- `crates/voice`: GPUI-independent audio/transport runtime on a dedicated thread.
  CPAL captures the default microphone, downmixes and resamples to mono PCM16 at
  24 kHz, and plays output on the default speaker. Bounded queues fail visibly
  rather than accumulate stale audio. One call is allowed per app process.
- `crates/voice/src/protocol.rs`: collect nested Responses function calls until
  `response.completed`, deduplicate call IDs, submit every result, then continue
  the backend. Failed/cancelled responses discard unexecuted calls.
- `crates/ui/src/shell/voice_actions.rs`: discoverable action contracts and an
  allowlisted dispatcher. Arbitrary engine method names and mutation `op` values
  are rejected. Existing RPC validation and remote-device routing remain in use.
- `crates/ui/src/shell/voice.rs`: native controls, platform credentials, captions,
  selection updates, confirmation, and execution against the current engine.
  Remote file operations resolve the session/project owner. File saves retain
  the engine's checkout/content hash conflict checks. Sends use durable commands
  and preserve the session config without enabling auto-approval.

A local voice implementation can reuse the `ToolCall`/`Event` boundary and the
same Noches action catalog. A local model transport, speech synthesis, and echo
cancellation are not included in this PR. Model/voice selection is currently
configured through `CallConfig`, with Marin as the voice default.

## Validation

```sh
cargo test -p noches-voice
cargo clippy -p noches-voice --all-targets -- -D warnings
cargo test -p zeron-ui -- --test-threads=1
cargo run -p zeron-ui --features voice-fixture --example voice-fixture -- /tmp/noches-voice-evidence
```

On macOS, the UI suite must run serially because existing native keyboard tests
call the Text Input Sources API. Parallel execution aborts in HIToolbox when
those tests access that API concurrently.

The fixture uses synthetic captions and performs no microphone capture or API
calls. [Dark](screenshots/voice/voice-live-dark.png) and
[light](screenshots/voice/voice-live-light.png) renders are checked in for review.
The macOS development and release bundles include the microphone purpose string;
the hardened release signing path adds the audio-input entitlement. Linux builds
need ALSA development headers, already installed by the UI/release workflows.

Before taking the PR out of draft, run a real call with an API key and headphones:
verify microphone denial, simultaneous speech, local and remote session tools,
queued sends, a declined native confirmation, file conflict handling, mute,
rapid hangup/restart, device removal, and network loss. Automated checks do not
establish real microphone quality, account/model access, or service latency.

## Sources

The interaction model was informed by [bb-handsfree](https://github.com/swairshah/bb-handsfree).
Noches uses a native Rust implementation rather than its browser plugin code.

- [GPT-Live overview](https://developers.openai.com/api/docs/guides/live)
- [GPT-Live WebSockets](https://developers.openai.com/api/docs/guides/voice-websockets?api=live)
- [Delegation and tool results](https://developers.openai.com/api/docs/guides/live-delegation)
- [Session lifecycle and context](https://developers.openai.com/api/docs/guides/live-conversations)

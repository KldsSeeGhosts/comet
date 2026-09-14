# Handsfree → Noches voice port: findings and implementation plan

Review of the bb "Handsfree" plugin analysis (voice operator: OpenAI Realtime
API + IDE-control tools, ~8.2k lines TS/TSX, MIT) against this codebase, with
each claim verified in-tree and a concrete port shape.

Date: 2026-09-13. Branch context: `dev` (dev variant only per
`docs/dev-prod-workflow.md`; never let dev work touch `~/.zeron`).

## Verified claims

| Claim in analysis | Status |
|---|---|
| GPUI app, control-plane IPC | Confirmed — `zeron-local-api`, axum over Unix socket, SSE subscriptions, instance manifests + flock, human-consent model (`crates/local-api/README.md`) |
| Domain bridge in `crates/ui/src/workspace/control.rs` | Confirmed (~3.2k lines). Richer than listed: also `workspace.*`, `worktree.*`, `section.*`, `tab.*`, `layout.*` (compose/move/run/stop/watch), `team.*`, `coordination-state.*`, `chat.providers`, `window.activate` |
| `chat.list/select/new`, `agent.send/read/subscribe/wait/stop/interrupt`, `worktree.*` | Confirmed, near-1:1 with Handsfree's thread tools |
| webrtc-rs 0.20 in `crates/preview` | Confirmed — but **DataChannel-only** (`crates/preview/src/peer.rs`: "Preview bytes only use the resulting reliable, ordered DataChannel"). No media tracks, no Opus. Strengthens the WebSocket recommendation |
| No mic capture / TTS / STT | Confirmed — no cpal/rodio/alsa/pipewire dep anywhere; `crates/ui/src/sound.rs` shells out to `paplay`/`pw-play`/`aplay`/`ffplay`/`mpv` |
| Reads `~/.codex/auth.json` | Confirmed — `parse_codex_auth` in `crates/engine/src/agent_accounts.rs` (ChatGPT OAuth claims, `chatgpt_account_id`, plan, `OPENAI_API_KEY` fallback). Slot stores the full auth blob including `tokens.access_token` |
| Rebindable shortcuts | Confirmed — `crates/ui/src/settings/shortcuts.rs` (record/capture, conflict detection, `ShortcutId` + `KeymapConfig`) |
| `noches` CLI speaks local-api | Confirmed — `apps/zeron/src/bin/noches.rs` (`noches`/`noches-dev`), plus a generic extension-verb forwarder, so a `voice` domain is reachable without new CLI code |
| session_view handoff + durable steering | Confirmed — `crates/engine/src/session_view.rs` (engine-owned Chat/CLI handoff); `QueueMessage`/`SteerQueuedMessageNow` in `crates/rpc/src/lib.rs` methods |
| No runtime plugin host | Confirmed — only agent-CLI plugin references (opencode/cursor harnesses) |

## Corrections to the analysis

1. **"SQLite journal" conflates two precedents.** `crates/engine/src/run_journal.rs`
   is append-only **JSONL** per chat (`{data_dir}/journals/{chat_id}.jsonl`, seq'd,
   torn-tail tolerant). SQLite lives in `crates/sync` (`docs.sqlite3`) and
   `crates/orchestration` (`state.sqlite`). For voice-session event transcripts the
   JSONL pattern is the closer fit: `voice/{session_id}.jsonl`.
2. **Consent bites harder than "respect grants."** `agent.send/stop/interrupt` and
   launches require a per-workspace **Allow** grant; launches separately need the
   **orchestration** grant. Only `is_human_input()` can mint either
   (`control.rs:300-346`) — voice can never grant itself access. Additionally
   `agent.send` refuses CLI-owned panes outright (`control.rs:1144-1147`,
   `PaneMode::Chat` required). Correct behavior: voice calls the same `dispatch()`
   the bridge uses; denied verbs return speakable error strings
   ("denied: a human must Allow API access in the selected workspace").
3. **Codex token refresh is not portable.** The single-use-refresh machinery
   (`inflight_refreshes`) is for **Claude** slots; codex auth is refreshed by the
   `codex` CLI itself. Noches can *read* `tokens.access_token` but cannot refresh
   it — expiry means "run `codex login`."
4. **Two tools lack control verbs.** `archive_thread`/`rename_thread` exist only as
   `Mutate` ops (`setChatArchived`, `renameChat` — see `set_chat_archived` in
   `shell.rs:3542`). `set_composer_text` needs a new `composer.*` verb on the
   focused pane's composer entity. Each is a ~50-line `control.rs` addition.
5. **`set_pane` (spotlight/maximize) has no analog.** `focus_pane` exists; there is
   no pane-zoom concept. Port as `chat.select` + `window.activate`; add zoom later
   if wanted.

## The gap the analysis missed: echo cancellation

Browser `getUserMedia` gave Handsfree AEC for free. `cpal` returns raw capture:
open mic + speaker playback means the model hears itself and self-interrupts
(or worse, loops). Options, by effort:

- `webrtc-audio-processing` crate — WebRTC APM bindings (AEC + NS + AGC); the real
  cross-platform fix.
- Linux: route capture through PipeWire/PulseAudio `module-echo-cancel` source
  (host-config dependent; not portable).
- v1 fallback: push-to-talk + headphones-first; server-side VAD alone does NOT
  cancel echo.

This decision gates the audio module's design — pick before writing `audio.rs`.

## Recommended shape

New `crates/voice` (pure Rust, no GPUI) + thin `crates/ui/src/voice/`:

```
crates/voice/
  audio.rs      cpal in/out, device enum, 24kHz resample, level meter, underrun
  realtime.rs   tokio-tungstenite WS client, session.update, event codec,
                function_call_arguments.done → dispatch → function_call_output,
                usage capture
  session.rs    orchestrator: speech_started → truncate playback + response.cancel
  store.rs      {data_dir}/voice/*.jsonl transcripts + usage (run_journal shape)
  text.rs       headless text-session harness (port of scripts/text-session.mjs)
crates/ui/src/voice/
  mod.rs        Entity<VoiceSession>: owns session, implements VoiceToolSink by
                calling Workspace::dispatch — NOT the HTTP socket
  pill.rs       composer waveform button (beside render_send_button,
                composer.rs:6862)
  console.rs    live-call overlay
  settings.rs   model/voice/mic picker + level meter + PTT keybind
```

**Key wiring decision:** the tool executor calls the same internal `dispatch()`
the HTTP bridge calls — one domain layer, identical consent semantics, and
multi-device routing for free (sessions on remote engines resolve via
`space_id` → relayed RPC; `dispatch` already does this). Expose it as
`pub(crate)` with a caller tag (`http` | `voice`) for audit.

**"Thread finished" feed:** ride the existing staleness/send-pending-gated
transition detector in `shell.rs:1767-1845` (`sound_for_transition` edges) — the
same hook that drives chimes and banners.

## Tool map

| Handsfree | Noches |
|---|---|
| get_context | `layout.state` + `chat.list` + selected pane |
| list_projects / list_machines | `workspace.list` / `WATCH_DEVICES` snapshot |
| list_live_threads / list_threads / search_threads | `chat.list` + `state.sessions` statuses, local title filter |
| read_thread | `agent.read` (transcript snapshot) |
| focus_thread | `chat.select` + `window.activate` |
| send_to_thread | `agent.send` (Allow-gated; CLI panes refused — speakable) |
| start_thread | `chat.new` (orchestration-gated) |
| stop_thread | `agent.stop` / `agent.interrupt` |
| archive_thread / rename_thread | **new** `chat.archive` / `chat.rename` → `Mutate` |
| show_diff | `GetCheckoutDiff` + focus Changes surface (`crates/ui/src/changes.rs`) |
| update_instructions | new `voice_instructions` field in `UiSettings` (`settings.rs:344`) |
| set/append_composer_text | **new** `composer.*` verbs — keep draft-then-human-sends, it's the right safety default |
| run_plugin_command | drop — no plugin host |

## Port verbatim as logic (not syntax)

System prompt/instruction set; tool JSON schemas + descriptions; VAD tuning
(threshold 0.75, 700ms silence, near-field noise reduction); cost accounting
(~$32/1M audio-in); shortcut-validation semantics; the text-session test
harness.

## Do not port (~35-40% of the plugin)

All cross-realm presence/nonce/mirroring/`forceStop`/mobile-suspension code —
Noches is one process, every window shares the session trivially. Mobile
drawer. `client.hello` device identity. WebRTC/SDP/Opus entirely (OpenAI
Realtime speaks WS + PCM16/G.711; `tokio-tungstenite` 0.24 already in-tree).

## New platform work

- `cpal` + `rubato` (resample) deps; 24kHz mono i16 ↔ base64 audio events.
- macOS: `NSMicrophoneUsageDescription` in `dist/macos/Info.plist` (currently
  absent — mic hard-fails without it).
- AEC per above.
- OpenAI credential: API key first-class (env or 0600 file under data dir —
  plaintext in `ui-settings.json` is not ideal). Codex `access_token` fallback
  needs a **live check** that `wss://api.openai.com/v1/realtime` accepts ChatGPT
  OAuth before being promised.

## Sequencing

1. `crates/voice` WS client + text-mode tool loop against `dispatch` — testable
   with zero audio (the `text-session.mjs` port doubles as the integration
   test harness).
2. cpal capture/playback + level meter + interruption handling.
3. Composer pill + call console.
4. Sessions/usage store + settings section + `voice.*` CLI verbs.
5. AEC.

Steps 1–2 ship independently behind a `voice` settings flag. New control verbs
(`chat.archive`, `chat.rename`, `composer.*`, `voice.*`) are small and land in
step 1.

## Handoff for the next agent

```text
Continue refining the Noches GPT Live voice implementation.

Repositories
- Noches: /home/kidsseeghosts/AiStack/comet
- Branch: dev
- Voice commit: 4a6cf35aa9ce8ab69ef462961cab7bf4399d1b7d
- Pushed to: origin/dev
- CPA: /home/kidsseeghosts/Projects/CLIProxyAPI
- Branch: erm/zai-oauth
- Failover commit: f4c71129e4bbfeee5177ec2f1faa233251ce269d
- Pushed to: fork/erm/zai-oauth
- CPA upstream origin is read-only for this user.

Important user instruction
- Do not launch, click, automate, or interact with the Noches app to test voice.
- The user performs all interactive microphone and playback testing.
- Use code inspection, unit tests, protocol-only probes, clippy, and builds.
- Preserve every unrelated dirty change in both repositories.
- Work only on the Noches dev variant. Do not touch ~/.zeron or production.
- Use SWE 2 subagents for implementation work.

Current interaction
- One mic click starts a persistent live session.
- The microphone remains active across turns.
- Server VAD submits speech automatically.
- Clicking while active stops the session.
- New speech clears assistant playback and uses server-side interruption.
- response.done returns the UI to Listening rather than Idle.

Realtime configuration
- Endpoint: ws://127.0.0.1:8317/v1/realtime?model=gpt-realtime
- CPA endpoint can be overridden with CPA_REALTIME_ENDPOINT.
- CPA_API_KEY is preferred.
- OPENAI_API_KEY is the optional direct OpenAI fallback.
- Voice defaults to marin.
- PCM16 input and output are both 24 kHz.
- Server VAD:
  - threshold: 0.75
  - prefix_padding_ms: 300
  - silence_duration_ms: 700
  - create_response: true
  - interrupt_response: true
- Input noise reduction: near_field

Main Noches files
- crates/ui/src/voice/controller.rs
- crates/ui/src/voice/mod.rs
- crates/ui/src/composer.rs
- crates/ui/src/session_pane.rs
- crates/ui/src/workspace.rs
- crates/voice/src/client.rs
- crates/voice/src/events.rs
- crates/voice/src/audio/capture.rs
- crates/voice/src/audio/playback.rs
- crates/voice/src/audio/resample.rs

Audio fixes already made
- The old playback ingress queue held only 480 ms. Realtime output arrived
  faster than playback and all later PCM chunks were dropped.
- Playback now has a bounded 60-second FIFO queue.
- Capture delivery buffering increased from 160 ms to 5 seconds.
- Arbitrarily split PCM deltas retain partial samples between events.
- Output mono samples are duplicated correctly across device channels.
- OutputAudioDone flushes the final partial chunk.
- A playback generation race that could drain new audio was fixed.
- Playback and capture callbacks remain bounded and nonblocking.

CPA behavior
- One OAuth credential remains pinned for each WebSocket.
- Handshake 401, 403, 402, 429, and recognized usage-limit errors can rotate
  to another eligible credential before the downstream upgrade.
- Selection excludes attempted auth IDs and honors max-retry-credentials.
- In-band usage_limit_reached and websocket_connection_limit_reached update
  credential cooldown state.
- rate_limit_exceeded is scoped to gpt-live-1-codex.
- CPA rewrites terminal quota events to:
  error.code = "cpa_credential_exhausted"
  error.retryable = true
- CPA then closes with private WebSocket code 4429.
- Noches may replay one retained turn only before assistant output or tool
  activity. It asks the user to repeat rather than risk duplicate speech or
  tool side effects.

Verification completed
- cargo test -p zeron-voice
  - 39 tests and 1 doctest passed
- cargo test -p zeron-ui --features dev voice
  - 13 tests passed
- cargo clippy -p zeron-voice --all-targets -- -D warnings
- cargo clippy -p zeron-ui --features dev --lib --no-deps -- -D warnings
- cargo check -p zeron --features dev
- CPA:
  - go test ./internal/client/codex/live/... ./sdk/cliproxy/auth/...
  - go build ./cmd/server
- A protocol-only CPA WebSocket probe accepted the full PCM, near-field,
  and server-VAD session.update payload.

Deployment state
- Noches dev service and cpa-tunnel.service were active after installation.
- Installed Noches dev binary checksum:
  961bc9821ebff19424ff48bf281ab525a7e78b829bbbabb70a5443562af876ab
- The installed dev build came from the still-dirty working tree, so it is not
  a clean byte-for-byte build of commit 4a6cf35a alone.
- Deployed CPA binary:
  v7.2.146-12-gc6aceec7+voice-failover
- Deployed CPA checksum:
  83537c44dcc2855f999688e1facf8f936267863f87645a282b0cabb5add58069

Next work
1. Wait for the user's interactive findings from the new persistent mode.
2. Fix specific capture, VAD, playback, timing, or state issues based on those
   findings. Do not test the app yourself.
3. Refactor CPA reconnect dialing. connect(...).await currently happens inside
   the event arm, temporarily pausing frame processing. Race the connection
   attempt with capture and control events or move dialing into a separate task.
4. Audit barge-in response ordering. A canceled response.done can race with a
   newly detected utterance and clear replay state belonging to the new turn.
   Track response and utterance IDs where available.
5. Consider native echo cancellation. BB Desktop receives browser
   echoCancellation, noiseSuppression, and autoGainControl through
   getUserMedia. Raw CPAL capture does not provide AEC. This matters when the
   user uses speakers rather than headphones.
6. Replace the blocking playback flush retry if long responses can fill the
   60-second hard cap. Normal responses do not approach it, but flush can wait
   for up to roughly 6 seconds at the cap.
7. Keep agent.send behind the existing Allow consent check. Voice must never
   mark input as human or mint consent.
8. Re-run focused tests and build the dev variant after each correction.
9. Commit only selected voice or CPA hunks. Both working trees contain many
   unrelated modifications and untracked files.
```

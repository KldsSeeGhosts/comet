# Noches implementation and verification

The Noches project extends the existing Zeron app on `dev`. Production sessions
and `~/.zeron` are outside the development and test boundary.

References:

- `/home/kidsseeghosts/AiStack/SUPERCONDUCTOR_ARCHITECTURE.md`
- `/home/kidsseeghosts/AiStack/SUPERCONDUCTOR_FINDINGS.md`
- `docs/super-split-and-cli-spec/SPEC.md` and its `screenshots/` directory
- https://linear.app/ermin-zeherovic/project/noches-0f13a90616f4

## Requirements

These are acceptance requirements, not completion claims.

The first continuity verification round targets Pi, per the user's updated
scope. Keep existing provider work, but defer further Codex and Claude work
until the Pi Chat-to-CLI-to-Chat path is verified. The remaining Noches project
requirements still apply.

| Issue | Required behavior |
| --- | --- |
| ERM-482 | Two recursive split trees, independent view tabs and rails, pane focus, draggable ratios, 20% edge drop zones, persisted layouts |
| ERM-483 | Canonical provider/session/cwd/model/effort identity, per-pane Chat/CLI toggle, provider-history hydration, exclusive process ownership, parking and lazy restore |
| ERM-477 | Native PTY and Alacritty grid, batched GPUI drawing, keyboard/IME/clipboard, queued boot input |
| ERM-486 | Existing rich transcript/composer in each pane, markdown, thinking, tools, diffs, images, mentions, slash completion, input wizard, undo/redo |
| ERM-485 | GPUI app shell and separated workspace, terminal, provider, storage and control-plane responsibilities |
| ERM-479 | Unix-socket API and CLI, discovery with process and socket identity, explicit socket override, stable addressing, layout/agent/chat/team/workspace/worktree/section routes |
| ERM-484 | Atomic declarative compose, exact revision/identity guard, dry run, launches into views/tabs/panes, scoped named recipes |
| ERM-480 | Provider hooks and wrappers, normalized lifecycle events, dedupe, background-stop handling, safe bounded notifier queue |
| ERM-481 | Send/read/wait/subscribe/stop/interrupt, durable 1-8-role teams with bounded reports and restart interruption, versioned coordination state and watch |
| ERM-478 | Explicit orchestration consent, verified app sessions and human-input provenance, per-workspace act grants, destructive confirmation and last-workspace protection |

## Verification gates

- Model tests cover nested layouts, stable identities, malformed restores,
  resizing, cross-tab moves, collapse, serialization and atomic guarded apply.
- Handoff tests cover native resume arguments, single-writer ownership, failed
  spawn rollback, provider history and terminal process teardown.
- Runtime tests use actual split panes and both renderers. Confirm independent
  prompts, selection, focus and scrolling. Drag nested dividers and edge zones.
- Restart the development UI and check restored topology, selections and modes.
- Exercise the socket API and CLI against the running development instance.
- Verify consent failures do not mutate state or launch processes.
- Run dev checks, tests and clippy. Install with `./dev.sh`, verify the service,
  launcher and build checksums, then inspect the actual dev window.
- Run `./install.sh --dev --release` and verify the optimized dev installation.

The extracted `/tmp/sc-instructions` manuals and `/tmp/sc-dmg` sources were not
present at the start of this implementation. The two referenced documents and
the Linear issue descriptions remain available.

## Current product references

The current official documentation refines the earlier binary notes:

- [Layout](https://super.engineering/docs/layout-sidebars-and-pip/): tab edge
  drops create splits; center drops join a view as a tab. An outside drop ring
  creates a full view. Pane headers can move into tab rails. Double-clicking a
  divider equalizes its branches. Closing the last tab leaves a launcher.
- [Terminal and chat](https://super.engineering/docs/terminal-and-chat/): only
  idle sessions with verified resume targets can switch renderers. Busy sessions
  cannot switch. The menu exposes the transition rather than silently starting
  a replacement conversation.
- Official demo downloaded to `/tmp/noches-reference/app-dark.mp4` from
  `https://super.engineering/_astro/app-flex-dark.CM9gM1tQ.mp4`. Video interpretation
  is assigned to a Gemini subagent; no interaction claims depend on the parent
  model viewing it.

The user permits foreground desktop testing. Product background computer use
must still refuse unsupported targets without changing physical focus or input.
A test that explicitly requests foreground delivery does not prove background support.

## Verified continuation, September 12

- Split presentation uses 200ms expo-out motion for insertion, collapse and
  equalization. Persistent ratios and API revisions do not change per frame.
  Manual resizing and reduced motion snap directly to the logical geometry.
- Inactive chat panes retain their drafts, selection and scroll entities.
  Their empty composer reads "Click to focus chat" without replacing the
  wizard's underlying placeholder. Native Shift+Tab remains provider input.
- Malformed saved recipes fail validation before ID remapping.
- Engine-owned Pi hooks authenticate per-launch bindings over a private Unix
  socket. Unknown or busy native state refuses handoff before PTY teardown.
  A successful idle freeze rejects subsequent prompt admission.
- `cargo clippy --workspace --all-targets --features dev -- -D warnings` passed.
  Cargo separately reports a future-incompatibility notice for the external
  `proc-macro-error2` dependency.
- `cargo test --workspace --all-targets --features dev` passed. Its ignored
  installed-Pi test was run separately with `bash scripts/tests/run-pi-continuity.sh`.
- The installed Pi 0.85.1 test used a real PTY and a deterministic local model
  endpoint. It verified exact history across all three turns, preserved native
  ID/cwd/model/effort, exactly two imported CLI messages, idempotent hydration,
  and busy-close refusal with the original CLI still writable. Artifacts:
  `/tmp/zeron-pi-continuity.oXgTXk`.
- The debug dev build was installed and `zeron-dev.service` restarted. Build,
  launcher and service executable hashes matched. The actual dev window
  restored two views with nested panes and a native Pi terminal. The local CLI
  discovered its verified Unix socket and read the persisted layout.
- Background CUA on this GPUI dev window returned
  `background_unavailable (client_not_qualified)`. This is a known missing
  capability, not a passed background-input test.

Optimized dev build installed via `./install.sh --dev --release`;
`zeron-dev.service` restarted and build/launcher/service checksums match.
Live CUA verification on the running dev window (pid 1928907, patched debug
driver): background clicks reach the GPUI surface and open the pane menu, but
synthetic clicks on "Allow API agent access" and "Allow API orchestration in
this workspace" mint no grant (`apiAllowWorkspaces`/`orchestrationWorkspaces`
stay empty) because consent requires positive physical-input provenance. While
the physical seat holds the window, the seat publishes `primary_client_busy`
and the driver returns `background_unavailable` without dispatching.

Still open: remaining provider integration beyond Pi, and a fuller runtime
pass over team/workspace/worktree/section control. No Linear issue has been
marked complete by this verification pass.

## Control-plane continuation verification

The UI now owns the durable orchestration store and its startup interruption
recovery. Team creation, reporting and cancellation serialize prompt admission;
failed creation cancels the run and interrupts its admitted roles. Coordination
state and sections persist with exact CAS versions and scoped watch events.

Workspace/worktree routes include scoped human Allow grants and pending deletion
requests tied to exact targets. Staging a `workspace.delete` or
`worktree.delete` additionally requires an explicit `confirm` request field (the
CLI's `--confirm`); a human confirmation in the app remains the final gate.
App-created session capabilities alone cannot
unlock worktree creation; a recorded real human submission in that session is
also required. Sealed input proofs survive deferred callbacks. Synthetic input
cannot grant permission or confirm a deletion. The engine atomically rejects
last-workspace deletion, including concurrent requests.

Named and inline planned runs preserve their proposed topology, assign fresh
sessions only to new empty cells, and commit the layout once after validation.
The CLI supports `layout run views|tabs|panes`, provider/UI/count/label/prompt
flags, named recipes and team report/result-file flags.

Focused checks passed: 12 UI control tests, 5 CLI routing tests, 9 real Unix
socket transport tests, 10 durable orchestration tests, and the engine workspace
cascade regression with a retained second workspace. The concurrent
last-workspace deletion test also passed in the full development test run.
These checks do not replace the final installed-application interaction pass.

## Close-out verification round, September 12 (evening)

A full audit pass over every Noches ticket produced the following fixes and
verifications, all on `dev` with the debug dev build installed and restarted:

- Consent model: `agent.stop`/`agent.interrupt`/`layout.stop` now require the
  same per-workspace Allow grant as `agent.send`; the human-input unlock that
  enables worktree creation expires after 15 minutes; team-role report
  capabilities are no longer embedded in role prompts (transcript readers
  cannot forge reports) — they are delivered per role as a 0600 capability
  file plus the `team run` response, and `noches team report --capability-file`
  consumes them.
- Control plane: the instance-lock bind retries through the restart-swap
  window (a transient `InstanceLockedError`) instead of permanently disabling
  the API with the "locked by another process" banner; resubscribes abort the
  superseded watch task; startup recovery skips a corrupt orchestration row
  instead of killing the whole store; duplicate labels are rejected on every
  pane-creation path.
- CLI/server contract: `layout apply --ui chat|terminal` and `--force`
  (overwrite) exist as first-class flags, the bogus `--ui auto` choice was
  removed, `tab close|move|reorder` verbs are wired, and `team.run` strips
  placement keys from per-role launch parameters.
- Terminal: per-row fingerprint render caches (snapshot + shaping) remove
  full-grid reshape per frame; the engine reaps never-attached sessions after
  a 10-minute TTL (configurable in tests); failed opens drain queued boot
  keystrokes; mouse-mode wheel reports are clamped; tab numbering stays
  unique after closes.
- Shell hooks: a failed notify delivery now retries on subsequent drains
  (5 attempts, attempt count in the on-disk name) before quarantining, and
  invalid names no longer consume the drain budget; the Python interpreter
  for generated wrappers is resolved from PATH candidates before the handoff
  journal is written, so a missing interpreter surfaces a retryable
  precondition instead of RecoveryRequired; the bogus Claude `SubagentStart`
  hook was removed; GetSessionView now carries the canonical identity
  (provider, native session id, worktree, model, effort).
- CUA safety: agent-seat cancellation synthesizes the matching MouseUp before
  exit events; window closure cancels agent held state for that surface;
  keyboard rebind flushes stale held keys; click counters reset on mismatched
  release and capability loss; the input marker publishes a JSON payload with
  a machine-readable reason (`physical_seat_present`, `no_qualified_target`)
  that legacy readers still parse.
- Chat UI: an ActivePlanHud strip (label, done/total, current step, compact
  progress pill) docks above the composer and hides without a plan; the
  transcript gained a hover/drag overlay scrollbar; the reference-typography
  pass (15px prose, 13px code, reference heading scale, monochrome markers,
  keyword-hue links, composer radius) is pinned by updated tests.
- Workspace: a chat pane's committed session is no longer stripped when its
  fresh fork drops a selection the engine has not confirmed yet (`chat.new`
  panes keep their session; real selections still land; close still clears).
- dev.sh: terminate stale `zeron-dev` windows by pid (this machine's hyprctl
  dispatch shim rejects `closewindow address:...`, which left the previous
  UI holding the instance lock and wedged every relaunch).

Verification evidence: `cargo clippy --workspace --all-targets --features dev
-- -D warnings` clean; `cargo test --workspace --all-targets --features dev`
1830 passed / 0 failed; installed-Pi continuity (`scripts/tests/run-
pi-continuity.sh`, Pi 0.85.1) passed the full Chat→CLI→Chat round trip with
exact history, preserved identity/cwd/model/effort, busy-close refusal and
idempotent import; live CLI battery against the running dev instance covered
discovery/identity, read verbs, consent denial without Allow, grant-minted
`chat.new` on the mock provider, and the recipe save/apply(dry-run)/delete
round trip; the input marker on the live window carries the JSON reason
payload.

Known scope limits, recorded honestly: background computer use against the
GPUI window works only while the window is not physically focused (full
physical/agent concurrency is a GPUI single-focus-context limitation tracked
for the vendored fork, not a local hack); Claude/Codex handoff beyond
resume-args generation remains gated (Pi is the verified provider); pane
TabItem variants beyond Chat/Terminal (Browser/Diff/FilePreview) are not
wired as pane modes — no product entry point exists yet.

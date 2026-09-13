# Electron migration

Parallel **Electron / React / TypeScript** viewport over the existing Rust
engine — a second surface for evaluation, not a GPUI replacement. The core
contract stays:

```
Electron / React / TypeScript
        │  existing typed RPC / WebSocket boundary
        ▼
   Noches Rust engine          ← engine owns truth
        │
   Pi / Devin / Codex / …      ← harnesses own execution
```

## Base

- Worktree: `../noches-electron` (branch `feat/electron-react-client`)
- Base SHA: `bdab164b00dc3e9968d111c823a586acd2618975` on `origin/dev`
- App root: `apps/desktop` — `electron-vite` + React 19 + TS + Tailwind 4
- Commands: `pnpm dev` (root or `apps/desktop`), `pnpm typecheck`, `pnpm test`

## Architecture decisions

- **One socket, in main.** The engine's WS acceptor rejects any handshake
  with an `Origin` header (`crates/rpc/src/server.rs`), and browser clients
  can't suppress `Origin`. The `ws` connection therefore lives in the
  Electron **main** process (`src/main/engine.ts`), which also keeps the
  transport — and every live subscription — alive across renderer reloads
  and HMR. A renderer reload re-subscribes from `reset` frames; runs continue.
- **Sandboxed renderer, narrow bridge.** `nodeIntegration: false`,
  `contextIsolation: true`, `sandbox: true`. The preload exposes only
  `window.noches.{call, subscribe, getStatus, onStatus, retry}` — generic
  RPC + status, nothing else. No fs/process/pty/git surface.
- **Wire format is ndjson-over-WebSocket**, one frame per message:
  `{id, method, params}` out; `{id, ok|err|item|done}` in; `{id, cancel:true}`
  cancels. Implemented in `src/shared/` (types, protocol constants,
  transcript-delta applier, view derivations — all ports of the Rust sources,
  each named in comments).
- **Dev engine only.** Default endpoint `ws://127.0.0.1:27655`; an override
  pointing at prod `27654` is refused outright (`resolveEndpoint`). Main can
  spawn `zeron-dev headless` when the port is dead (disable with
  `NOCHES_ENGINE_SPAWN=0`). Electron userData is `~/.config/noches-electron-dev`.
- **Linux/Wayland host quirks.** Editor-inherited `ELECTRON_RUN_AS_NODE=1`
  is scrubbed in `electron.vite.config.ts`; Chromium's zygote sandbox and GPU
  process are disabled by default on Linux (`NOCHES_OS_SANDBOX=1`,
  `NOCHES_GPU=1` opt back in). Renderer `sandbox: true` — the boundary that
  matters — is unaffected by either.

## Implemented RPC surface

`EngineReady`, `EngineInfo`, `WatchSpaces`, `WatchChats`, `WatchSessions`,
`WatchConnectivity`, `WatchDocMessages`, `WatchQueue`, `GetSessionView`,
`ListHarnesses`, `ListModels`, `Mutate` (`createChat`, `markChatSeen`),
`QueueCommand` (`run` / `interrupt` / `respondInput`), `QueueMessage`,
`SteerQueuedMessageNow`, `SendQueuedMessageNow`, `RemoveQueuedMessage`.

## Parity status (slice)

Works: space/session sidebar with live indicators and unseen state, chat
open, transcript render (text/reasoning/tool/input/error parts), live
streaming, composer send → `QueueCommand`, busy-chat queue, steer (gated on
`steeringMode === "step-boundary"`), interrupt, respondInput panel,
harness/model header metadata, context-usage meter, reconnect with backoff
and honest connecting/reconnecting/failed states, runs survive renderer
reload (socket + subs live in main; reload re-subscribes from reset).

Not yet: terminal (SubscribeTerminal + xterm.js), workspace file search,
checkout diff surfaces, CUA viewport, settings/auth, pane/split layout,
attachments, model/modelOption pickers in the composer, packaging.

## Run side by side

```bash
# GPUI dev client (unchanged)
./dev.sh                        # builds + installs + restarts zeron-dev

# Electron dev client
cd apps/desktop && pnpm install && pnpm dev
```

Both point at the same dev engine (`27655`, `~/.zeron-dev`); the same
sessions, queues, and transcripts appear in both. Production (`zeron`,
`27654`, `~/.zeron`) is never touched.

## Known issues

- `ready-to-show` can lag on software-rendered Wayland; the window force-shows
  after 4s rather than holding hidden.
- Dev CSP allows `unsafe-inline` scripts for the vite react-refresh preamble.
- Chat creation without a space uses `Mutate createChat` with `deviceId`;
  space assignment is the only tested path.
- The hang-detector "Application Not Responding" dialog on Hyprland fires
  occasionally under software rendering; cosmetic, dismissed by the compositor.

## Next milestone

Terminal via `SubscribeTerminal` + xterm.js; `WatchCheckoutDiffs` file list;
model/reasoning pickers off `ListModels`; attachments in the composer.

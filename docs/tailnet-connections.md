# Tailnet connections

Noches can drive an engine on another computer over Tailscale. A saved computer opens in its own window, and the agent tasks, terminals, and file operations in that window run on the host that owns the workspace. The desktop dials the host's Tailscale address itself, so there is no SSH tunnel to manage and no relay service run by Noches. Tailscale encrypts the connection with WireGuard; when the two machines cannot build a direct path, Tailscale's DERP relays forward those encrypted packets.

## Scope

Each saved computer gets its own window with its own engine connection, sidebar selection, and subscriptions. A window that closes and tears down its client releases the session, and the host engine keeps running either way. Work on the host continues while the client is offline, and a reconnect resumes the stream instead of restarting it.

This transport does not cover:

- A merged sidebar across computers. Each window lists the projects and sessions of the engine it is connected to.
- Data replication. The remote engine keeps its chats, spaces, files, and settings. The client stores only its own window state.
- Moving queued work between computers. A task runs on the computer where it was created.
- Cold start while the host is offline. Opening a window requires the host engine and its gateway to be reachable.
- Preview forwarding. Localhost previews between devices travel over the account preview channel described in `docs/preview-networking.md`. A `localhost` address in a remote window opens on the client.

## Architecture

```
+---------------------------+                   +-----------------------------------+
| Noches desktop (Mac)      |                   | Remote host (kidsseeghosts)       |
|                           |                   |                                   |
| remote window             | Tailscale         | gateway  noches-connect           |
|  RemoteClient  ===============================>>  100.114.177.75:27657            |
|  resumable stream         | WireGuard         |          | loopback               |
|                           | (ws://...:27657)  |          v                        |
| ~/.zeron/connections.json |                   | engine   127.0.0.1:27656          |
+---------------------------+                   +-----------------------------------+
```

The transport lives in `crates/rpc/src/remote/`. The host side is a standalone binary, `crates/rpc/src/bin/noches-connect.rs`, so it can be updated without rebuilding the desktop app. A remote window speaks the same JSON-RPC protocol as a local one; only the transport underneath differs.

## Transport protocol

Protocol version 1 has eight frame types.

| Frame | Direction | Purpose |
| --- | --- | --- |
| `Hello` | client to server | Opens the protocol: session id, requested cursor, resume flag. |
| `Welcome` | server to client | Version, whether the session resumed, and the highest sequence the server has received. |
| `Data` | both | Payload slice with a sequence number and an `end` flag on the last slice of a message. |
| `Ack` | both | Highest sequence received. |
| `Ping` | both | Heartbeat when a side has been quiet. |
| `Pong` | both | Answer to `Ping`. |
| `Reset` | both | Session ended or no longer valid. Pending calls fail. |
| `Goodbye` | client to server | Window or client shutting down; the gateway releases the session right away. |

### Chunking and heartbeats

Large messages are split into 32 KiB chunks, cut on character boundaries so multi-byte text is never split mid-character. The send loops write one chunk, then service incoming frames, pending writes, and the heartbeat timer before the next one, so a long transcript does not starve `Ping`, `Pong`, or `Ack` traffic while it streams.

Chunking bounds each write. It does not remove TCP head-of-line blocking. The chunks still travel in order over one connection, and a large transfer occupies the link while everything queued behind it waits. What keeps this transport usable is the resume path below, not the chunk size.

A single frame is capped at 32 MiB. The gateway accepts no more than that from the WebSocket layer, and the client answers an oversized outgoing request with an inline error ("Remote request exceeds the 32 MiB limit") while keeping the connection open.

### Resume and replay

Each client opens with a fresh session id. If the socket drops, the client reconnects with the same session id, its receive cursor, and `resume: true`. The gateway resumes when it still holds the session and the cursor falls inside the retained range (`acknowledged <= cursor <= sent`), and the retained runtime keeps its sequence numbers and in-flight work. Otherwise it answers `Reset`, and the client starts a new session that fails any pending mutation rather than repeating it.

`resume: false` is an explicit fresh start, and the client sends cursor 0. The gateway retires any runtime already held in that slot, tells its reader `Reset`, and dials a new upstream session, so nothing from the retired runtime is inherited or replayed. A fresh hello that carries a nonzero cursor is answered with `Reset`, since the replies before that cursor would otherwise be skipped.

- Server replay buffer: unacknowledged frames up to 32 MiB. Overflow ends the session, the watch streams close, and the next RPC call opens a fresh session.
- Session retention: one hour without traffic, refreshed on every inbound frame. The session map holds at most 32 sessions.
- Client outbound queue: the client stops reading new requests once unacknowledged data reaches 8 MiB, which pushes back on the UI. Eight megabytes is a pause threshold, not a cap. The request accepted just below the line can be as large as the 32 MiB frame limit, so the queue can sit above the threshold until the host acknowledges.

Reconnect backoff starts at 250 ms and doubles to a 5 s cap. A connection that stayed up for more than 20 s resets the delay to 250 ms, so a long-lived session that drops reconnects quickly. A 401 response stops the retry loop.

### Non-blind mutation safety

If the host restarts while a mutation is in flight, the session state is gone. The client fails the pending call instead of retrying it, with:

`Remote connection was reset. The last action may have completed; check its result before retrying.`

Streaming consumers then resubscribe for a fresh snapshot from the engine. Agent prompts, file writes, and shell commands are never replayed into a new session.

## Security model

### Endpoint restriction

Plaintext `ws://` is allowed only to private interfaces: Tailscale CGNAT (`100.64.0.0/10`), Tailscale IPv6 (`fd7a:115c:a1e0::/48`), and loopback. Public addresses must use `wss://`. Endpoints with credentials, a path, or a query string are rejected before a dial.

The gateway enforces the same rule from the other side. It refuses to bind anything except a Tailscale or loopback address, and it requires its upstream to be the local engine's loopback WebSocket (`ws://127.0.0.1:PORT`).

### Handshake checks

The gateway rejects any handshake that carries an `Origin` header, and requires the path `/` with no query. With the endpoint rule above, that keeps a browser page from reaching a local or tailnet RPC port. A socket that connects and then stalls without sending its `Hello` frame is closed after eight seconds.

### Device pinning

Pairing reads `deviceId` from the host engine's `EngineInfo` and stores it in the connection profile. The gateway queries `EngineInfo` again for every new session and refuses to proxy when the id does not match ("Engine identity changed; pair this computer again"). The desktop repeats the check when it opens a window, so another engine on the same address is rejected instead of quietly serving the wrong computer.

### Tokens and files

Tokens are 64 hexadecimal characters, sent as `Authorization: Bearer <token>`. The gateway compares each candidate byte by byte with no early exit, so a mismatch does not reveal which byte differed.

Credentials (`access.json`) and saved desktop connections (`connections.json`) go through `private_write`, which writes a temporary file in the same directory, sets mode 0600, syncs, and renames. A reader never sees a half-written file, and the permissions do not depend on umask. Connection profiles print without their token.

## Gateway CLI

`noches-connect` is the whole host-side interface.

### serve

Proxies authenticated clients to the local engine. It refuses to start with no paired clients, and it opens one upstream connection to the engine per client session.

```bash
noches-connect serve \
  --bind 100.114.177.75:27657 \
  --upstream ws://127.0.0.1:27656 \
  --credentials ~/.local/share/noches-connections/access.json
```

### pair

```bash
noches-connect pair \
  --name "Linux Studio" \
  --endpoint ws://100.114.177.75:27657 \
  --upstream ws://127.0.0.1:27656 \
  --credentials ~/.local/share/noches-connections/access.json \
  --out ~/.local/share/noches-connections/macbook.connection
```

It calls `EngineInfo` for the device id, generates the token, appends the client to the credentials file, and writes the code file with mode 0600. The code is a secret, so the command writes it to a file and prints only the client id.

### revoke

```bash
noches-connect revoke \
  --credentials ~/.local/share/noches-connections/access.json \
  --id <client-id>
```

Removes the client. Active sessions for that token close within five seconds.

### import

Runs on the desktop and adds a code to an app data directory, which is `~/.zeron` on macOS and Linux (`ZERON_DATA_DIR` overrides it; Windows uses `%LOCALAPPDATA%\Zeron`).

```bash
noches-connect import \
  --code-file ~/.local/share/noches-connections/macbook.connection \
  --data-dir ~/.zeron
```

### probe

```bash
noches-connect probe --code-file ~/.local/share/noches-connections/macbook.connection
```

Verifies the pinned device id, then subscribes to chats, spaces, and devices and prints a one-line JSON summary. A run against the live host returns:

```json
{"host":"kidsseeghosts","identityVerified":true,"rows":{"WatchChats":4,"WatchDevices":1,"WatchSpaces":1},"scope":"local"}
```

### Service deployment

On the current host the gateway runs as a systemd user service:

```ini
# ~/.config/systemd/user/noches-connections.service
[Unit]
Description=Noches authenticated tailnet connections
After=network-online.target zeron-dev.service
Wants=network-online.target

[Service]
ExecStart=%h/.local/share/noches-connections/noches-connect serve --bind 100.114.177.75:27657 --upstream ws://127.0.0.1:27656 --credentials %h/.local/share/noches-connections/access.json
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
UMask=0077

[Install]
WantedBy=default.target
```

The upstream is the development engine. `zeron-dev.service` runs `~/.local/bin/zeron-upstream headless` with `ZERON_DATA_DIR=~/.zeron-upstream` and `ZERON_IPC_PORT=27656`. The installed engine on `127.0.0.1:27654` keeps serving the desktop session on that machine and is not touched by the gateway.

## Connection codes

A code is `noches-connect:` followed by URL-safe base64 (no padding) JSON holding `id`, `name`, `endpoint`, `token`, and `deviceId`. Paste one into Settings > Connections to save it; the desktop validates the endpoint and field lengths before writing. Treat a code as a secret, since it carries the token.

## Desktop interface

### Windows

Each computer opens in its own window (`open_connection_window` in `crates/ui/src/lib.rs`). A remote window binds an `AppState` to a remote `EngineBootConfig`, connects through the tailnet transport, and never falls back to a local engine. If the host is unreachable, the window reports that the computer is unavailable and suggests checking remote access and Tailscale.

Selections, panels, and subscriptions belong to the window, and its layout file lives under `remote-layouts/<hash of device id>` in the local data directory, so two hosts cannot overwrite each other's layout. Everything the window displays comes from the remote engine.

### Connections page

Settings > Connections (`crates/ui/src/settings/connections.rs`) lists saved computers with a live status: Connected, Connecting, Reconnecting, Access revoked, or Saved computer. Each row has Remove and Open computer. Below the list are Open local window and Paste connection code, which reads the clipboard, validates the code, and saves it to `connections.json`. The sidebar footer shows the same status next to the host name when the window is remote.

### Sidebar Remote button

The Remote button in the sidebar footer (`crates/ui/src/shell.rs`) jumps to the Connections page.

### Preview bundle

`~/.local/share/noches-connections/Noches Connections.app` is a development bundle that wraps a Noches build under its own bundle id, `local.noches.connections-preview`, so it runs alongside the installed app. `Contents/MacOS/zeron` is the real executable and `Contents/Info.plist` carries the identity plus the environment the app starts with:

```xml
<key>LSEnvironment</key>
<dict>
  <key>NOCHES_CONNECTION</key>
  <string>kidsseeghosts</string>
  <key>ZERON_DATA_DIR</key>
  <string>/Users/kidsseeghosts/.zeron-connections-preview</string>
</dict>
```

Double-clicking the bundle opens the paired computer window. macOS applies `LSEnvironment` when it launches the bundle, the plist supplies the name when nothing is passed on the command line, and the separate data directory keeps the preview profile and layout out of `~/.zeron`. The app reads `NOCHES_CONNECTION` only when no explicit target is given, so `--connection <name-or-id>` still wins.

Running the executable inside the bundle from a shell does not read the plist, so set the data directory yourself when you want a saved host that lives in the preview profile:

```bash
ZERON_DATA_DIR="$HOME/.zeron-connections-preview" \
  "$HOME/.local/share/noches-connections/Noches Connections.app/Contents/MacOS/zeron" \
  --connection other-computer
```

To fill the preview profile from a code, import with `--data-dir ~/.zeron-connections-preview`.

## Session lifecycle

### Closing, drops, and expiry

A client that is dropped or shut down while its socket is connected sends `Goodbye`, waits up to a second for the close handshake, and exits. The gateway drops the session from its map immediately, closes the upstream connection to the engine, and stops holding replay data for it. The host engine keeps running, so an agent task started from that window continues.

Not every window close reaches that path. On macOS the first window is kept alive by `ReopenState` after a window close, with the process still running behind the menu bar, so its client and session stay open until the app quits. And a connection that simply goes away, because the network dropped or the machine slept, gets no close handshake at all. Its session stays in the gateway map, valid for the retention hour so the client can resume, and the sweep expires it after that.

### Sweeps

The gateway scans its session map every five seconds and drops sessions that are invalid, revoked, or past their one-hour retention, closing their upstream connections. A session is invalidated when its upstream connection ends, when its client is revoked, or when its replay buffer overflows. A newer connection for the same session id takes the slot over, and the older connection is told `Reset` and dropped.

### Revocation

Every session re-reads the credentials file on a five second tick. When the token is gone, the session is invalidated and the gateway sends `Reset`. The desktop fails pending calls with the uncertainty message, moves to the `Unauthorized` state, and stops retrying, so a revoked window shows Access revoked instead of looping.

A window that boots against a revoked key fails with a typed error as soon as the gateway answers 401, before the pending call errors, and shows pairing copy: "Access to <host> was revoked or its connection key is invalid. Pair this computer again." When a connected window flips to `Unauthorized`, its status watch fails the connection, retires the standing subscriptions so their resubscribe loops stop retrying a link that cannot attach, and leaves cached chats and transcripts in place. Either way, reconnecting needs a fresh connection code and a new window; the revoked token is never accepted again.

### Control frames

Both reader loops handle WebSocket `Ping` and `Pong` frames explicitly: a `Ping` is answered through a flush, and a stray `Pong` is ignored. Tailscale relay hops and mobile carriers forward the encrypted packets without inspecting them, so they do not inject WebSocket control frames.

### Cancelled watches

UI watches use `subscribe_checked`, which returns an `RpcSubscription` that cancels the server stream on drop (`{id, cancel: true}`). Leaving a chat or closing a panel stops the engine's work for that stream instead of waiting for backpressure to be noticed.

## Verification

`crates/rpc/tests/remote_connection.rs` holds 17 tests plus the ignored live smoke test. They run the gateway against an in-process engine behind a proxy that can be taken offline, so drops and slow links are simulated without touching the network.

| Test | Covers |
| --- | --- |
| `network_loss_resumes_stream_and_does_not_repeat_a_mutation` | An outage during a mutation: the stream resumes at the next sequence and the mutation runs once. |
| `host_restart_reports_uncertainty_instead_of_repeating_work` | A restarted gateway fails the in-flight call with the uncertainty error and runs no second mutation. |
| `wrong_key_is_rejected_and_revocation_ends_an_active_session` | A bad token reaches `Unauthorized`, and deleting the client from the credentials file moves a connected client to `Unauthorized`. |
| `origin_header_is_rejected_even_with_a_valid_key` | A handshake carrying `Origin` fails even with a valid token. |
| `cellular_rate_large_unicode_message_resumes_mid_transfer` | A slow link (8 KiB per 50 ms each way, about 1.3 Mbps) with a drop mid-transfer: a 100,000-rune payload arrives intact and the mutation runs once. |
| `rapid_connection_flapping_resumes_stream_cleanly` | Three quick offline and online cycles leave no gaps or repeats in the stream. |
| `concurrent_calls_queued_during_network_outage_all_complete_once` | Four calls queued while offline all complete once, each with its own result. |
| `upstream_engine_crash_reports_uncertainty_and_resets` | An engine that dies mid-call surfaces the uncertainty error. |
| `payload_exceeding_limit_is_rejected_without_dropping_connection` | A 33 MiB request fails inline, and the connection keeps working afterwards. |
| `client_shutdown_terminates_gracefully` | Forty connect and shutdown cycles, each leaving a client that fails cleanly instead of hanging. |
| `dropping_the_wrapper_stops_reconnects_behind_a_held_client_clone` | Dropping the wrapper while a clone of the RPC client is still held ends the transport: the held clone fails promptly and no replacement session is dialed. |
| `shutdown_cancels_a_stalled_handshake_and_stops_redialing` | A stop request cancels a dial parked in the WebSocket handshake in under a second and leaves no dialer behind. |
| `replay_overflow_ends_watch_then_allows_a_fresh_session` | With a 512-byte replay budget, the watch ends and a new call succeeds under a fresh session. |
| `lost_ack_replayed_request_executes_once_and_control_ping_survives` | A resent sequence executes once, and a WebSocket `Ping` gets a `Pong`. |
| `fresh_hello_for_a_retained_session_starts_a_new_runtime` | `resume: false` on a retained slot starts a fresh runtime, tells the old reader `Reset`, and replays nothing from the retired session. |
| `reopening_a_retained_slot_is_allowed_at_the_session_cap` | A fresh hello replaces its own retained slot even when the other 31 slots are taken, and sequence 1 executes once for the new reader. |
| `fresh_hello_with_a_stale_cursor_is_reset` | `resume: false` with a nonzero cursor gets `Reset`, and the retained session keeps running untouched. |
| `live_tailnet_transcript_files_and_terminal` | Ignored by default. Needs `NOCHES_REMOTE_CODE_FILE` and an authorized host. Opens a terminal on an existing project chat, checks the output, and closes only that terminal. |

Unit tests cover endpoint validation and code round trips in `crates/rpc/src/remote/config.rs`, and subscription cancellation on drop in `crates/rpc/src/lib.rs`.

The preview bundle was built and validated from a snapshot of this tree that left out the unrelated in-progress pane edits (`crates/ui/src/pane/chrome.rs`, `crates/ui/src/pane/render.rs`); at validation time the working checkout did not compile, with four errors in `pane/chrome.rs` and `shell/panes.rs`. On that snapshot the UI suite passes with the tests serialized (1162 tests, 62 of them state tests); the default parallel run aborts with SIGABRT, the standing baseline for this checkout.

Nothing here covers several clients on one host under load, sessions older than the one-hour retention, or a real mobile network. Buffer sizes, the session count, and retention are fields on `ServerOptions` if you need to change them.

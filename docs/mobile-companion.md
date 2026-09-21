# Noches mobile companion

The iOS app in `apps/ios` doubles as a Noches companion. Pair it with a Mac or
Linux host over Tailscale and drive that host's sessions from the phone: no
account, no edge deployment, and no engine on the phone. Agents, files, and
transcripts stay on the host, and sessions keep running there while the app is
closed.

The gateway and its transport are in
[Tailnet connections](tailnet-connections.md).
[apps/ios/README.md](../apps/ios/README.md) covers the cloud path: sign-in,
Loro doc rooms, and attachments.

## Host setup

Run Noches on the host first. `zeron daemon install` installs `zeron headless`
as a launchd agent on macOS or a systemd user service on Linux;
`zeron daemon start|stop|restart|status` manages it, and `zeron status` reports
the workspace mode. An open desktop window embeds the same engine, so a
desktop window is also a host.

The engine serves its WebSocket RPC on loopback only, at
`ws://127.0.0.1:PORT`. `PORT` is whatever `ZERON_IPC_PORT` held when the engine
started, `27654` by default. Since `zeron daemon install` captures that variable
into the service definition, a given host may use another port. Confirm the
listener with `lsof -nP -iTCP:PORT -sTCP:LISTEN` on macOS or
`ss -ltnp | grep PORT` on Linux.

Then join the host to the tailnet. `tailscale ip -4` prints the address the
phone will dial, a `100.x.y.z` address. Create the pair code and serve:

```sh
cargo build -p zeron-rpc --bin noches-connect
CONNECT=./target/debug/noches-connect
TAILNET_IP=100.x.y.z                                  # tailscale ip -4 on the host
ENGINE_WS="ws://127.0.0.1:${ZERON_IPC_PORT:-27654}"   # this host engine's IPC port
STORE="${XDG_DATA_HOME:-$HOME/.local/share}/noches-connections"
mkdir -p "$STORE"

"$CONNECT" pair --name "Linux studio" --endpoint "ws://$TAILNET_IP:27657" \
  --upstream "$ENGINE_WS" --credentials "$STORE/access.json" \
  --out "$STORE/phone.code"
"$CONNECT" serve --bind "$TAILNET_IP:27657" --upstream "$ENGINE_WS" \
  --credentials "$STORE/access.json"
```

`pair` mints a 64-character hexadecimal key, registers the client in the
credentials file, and writes the code to `--out`. Neither write prints the key
to the terminal, and both files land with mode `0600` through atomic writes.
Keep the code file private, since it grants the phone the same session control
you have, and delete it once the phone has it.

`serve` refuses to start until at least one client is paired, with "Pair a
client before enabling remote access". The gateway ignores requests that carry
a browser `Origin` header, requires the key as a `Bearer` token, and checks the
upstream engine's `deviceId` before it forwards traffic, so a code stops
working if a different engine takes over that address. Management commands:

```sh
"$CONNECT" revoke --credentials "$STORE/access.json" --id CLIENT_ID
"$CONNECT" probe --code-file "$STORE/phone.code"   # host, identity, scope, and row counts
```

`revoke` removes one client's key, and that client's sessions close within five
seconds. `probe` prints the host name, the identity check, the workspace scope,
and watch row counts for a code file.

## Pairing the phone

Tap **Pair a computer** on the signed-out companion home, or pair from the
toolbar sheet that also holds appearance settings. Paste the code:
`noches-connect:` followed by URL-safe base64 of the profile JSON, which holds
the `id`, `name`, `endpoint`, `token`, and `deviceId`.

The key is stored in the phone's Keychain, under the `noches.companion`
service, accessible only while the device is unlocked, and never in
preferences or logs. The phone accepts a plain `ws://` endpoint only for
private addresses: Tailscale CGNAT `100.64.0.0/10`, the Tailscale ULA prefix
`fd7a:115c:a1e0::/48`, or loopback. Anything else needs `wss://`, and an
endpoint must not carry a path, credentials, or a query. Several computers can
be paired and switched in the same sheet. **Forget** removes one locally;
`revoke` on the host disables its key everywhere.

## What the phone supports today

Transport is one WebSocket per computer, using the gateway's version-1 framing:
hello and welcome, sequence-numbered data frames with an `end` flag,
acknowledgements, ping and pong, and reset. Replies arrive as 32 KiB chunks
that the phone reassembles into one UTF-8 payload, capped at 32 MiB, and the
phone marks its own frames `end: true`; a data frame without `end` fails as an
incompatible protocol. A reset frame means the host restarted or reset the
connection, and the app refreshes session state.

Reconnects send a fresh hello with `resume: false`, which the gateway treats as
an explicit new start: it retires the retained session for that slot and dials
a new upstream session. Reads resubscribe and nothing is replayed. A mutation
whose delivery was not confirmed closes the connection with a "check the
session before resending" error instead of being retried. Deadlines are 12
seconds to connect and 20 seconds for a call or an initial snapshot, with a
ping every 8 seconds and a close after 24 seconds without a frame. A dropped
link retries every 4 seconds while foregrounded; backgrounding closes it and
the app reconnects on return.

Sessions are scoped to the selected computer, and archived rows are hidden:

- live catalogs from `WatchChats`, `WatchSpaces`, `WatchSessions`, and
  `ListHarnesses`, with installed and enabled agents only;
- create via `Mutate {op: "createChat"}` against a project or the home folder,
  with the chosen harness, sandbox `workspace-write`, and the agent's default
  model;
- send: `QueueMessage` with `holdForTurnEnd` while a session is working or
  awaiting input, otherwise a `run` command carrying the prompt, working
  directory, sandbox, `autoApprove: false`, and the session's harness, model,
  and reasoning options. Queued rows render read-only above the composer;
- stop: `interrupt`;
- approvals: `respondInput` with the question panel's `{questionId, labels}`
  answers.

Transcript: `WatchDocMessages` frames apply incrementally as reset, upsert,
append, or remove changes, with byte-length and row-count checks. Text parts
reuse the app's markdown renderer, tool parts render as a labeled row that
turns red on error, and a frame that fails verification surfaces "Transcript
needs a refresh." before the view resubscribes. The host writes every entry;
the phone never edits the transcript.

Not implemented today:

- the host engine and the gateway must already be running, so the phone cannot
  wake a sleeping host, start a stopped service, or reach a machine that is off
  the tailnet;
- no attachments and no generated-image rendering: the composer has no picker,
  and `image` parts show "View generated image on your computer";
- no terminal, diff, or file browser, and no PR badges;
- queue rows cannot be edited, reordered, or removed from the phone;
- no session archiving or renaming, and no space creation;
- no QR pairing: the code is pasted or typed.

## Theme and appearance

The companion uses the desktop theme catalog.
`apps/ios/Zeron/Theme/DesktopThemes.json` is generated from `crates/theme` and
carries 19 families: Zeron, VS Code Default, Catppuccin, Tokyo Night, Dracula,
GitHub, Ayu, Gruvbox, Rosé Pine, Nord, One Dark Pro, Atom One Dark, Night Owl,
Winter is Coming, Palenight, SynthWave '84, Shades of Purple, Cobalt2,
Andromeda, with their light and/or dark variants, several of them dark-only,
plus resolved color roles, syntax colors, and the seven accent presets:
Noches, orange, amber, green, cyan, blue, and pink. Regenerate it after a
theme change:

```sh
cargo run -q -p zeron-theme --example export-ios > apps/ios/Zeron/Theme/DesktopThemes.json
```

The Appearance screen selects the mode, a light theme, a dark theme, an accent
preset, and a surface treatment. The mode is system, light, or dark; the surface
treatment is theme default, frosted, or opaque. Frosted surfaces use
`ultraThinMaterial` and `glassEffect`, and fall back to opaque when Reduce
Transparency is on. Choices are device-local defaults under `appearance.*`.

Import a resolved theme family JSON through Appearance, Import desktop theme,
to add a theme. The file must be at most 16 MB and match the bundled shape:
non-empty id and name, at least one light or dark variant, every built-in color
role plus the seven accent roles, valid `#rrggbb` or `#rrggbbaa` colors, and no
id collisions with built-ins or installed themes. Imported families without
preset roles fall back to the desktop's accent arithmetic, byte-rounded mixes
with contrast correction. The test suite checks that this derivation reproduces
the exported desktop presets.

The phone's appearance is not synchronized from the host. Matching the host
means exporting the same resolved family on the host and importing it here.

## Versus the cloud path

The same app can also join the mesh. Sign in from Settings with Connect a cloud
account, and the phone becomes a peer device: workspace and session Loro docs
sync in both directions, and queue editing, attachments, and PR badges work
there. Direct pairing does less, one host at a time, with no account, no edge,
and all state on the host.

| | Cloud mesh | Companion |
| --- | --- | --- |
| Sign-in | WorkOS through the edge | None |
| Transport | Edge rooms plus the device relay | Tailscale to the host gateway |
| Scope | Every signed-in device | The paired computers only |
| Phone state | Loro mirrors of workspace and session docs | None; live RPC views |
| Offline host | Commands stay durable in the session doc and a reconnecting host drains them | Nothing is sent; the connection reports the error |
| Attachments, queue edits, PR badges | Supported | Not implemented |
| Theme | Bundled desktop catalog | Same catalog |

## Build, run, and test

Simulator validation on September 20, 2026 passed 21 companion and appearance
tests and both companion UI tests. The transport run included the real Rust
gateway over a fixture engine; it did not run a real agent or use a physical
iPhone. Captured screens: [light dashboard](screenshots/mobile-companion/companion-zeron-light.png),
[Catppuccin dashboard](screenshots/mobile-companion/companion-catppuccin-mocha.png),
[appearance settings](screenshots/mobile-companion/appearance-catppuccin-mocha.png),
and [transcript](screenshots/mobile-companion/companion-transcript-dark.png).

Xcode 27 with the iOS 27 simulator runtime; the project's deployment target is
iOS 26.0. `iPhone 18 Pro` is the installed simulator destination on this
machine. From `apps/ios`:

```sh
xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' build

xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' test
```

The companion subset:

```sh
xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' \
  -only-testing:ZeronTests/CompanionConnectionTests \
  -only-testing:ZeronTests/CompanionTransportTests \
  -only-testing:ZeronTests/AppearanceSettingsTests \
  -only-testing:ZeronTests/AppearanceImportTests \
  -only-testing:ZeronUITests/CompanionUITests test
```

- `CompanionConnectionTests` covers code round-trips, the accepted and rejected
  endpoint matrix, transcript frame application including UTF-8 byte lengths,
  and host catalog decoding. No fixture needed.
- `CompanionTransportTests` drives a fixture host: identity and catalog, a
  snapshot that outlives the 20-second deadline, wrong-key and changed-identity
  rejection, ambiguous mutation delivery that is never replayed, and the
  run/interrupt/respondInput wire shapes. Two cases cover reassembly of large
  replies: `testLargeUnicodeReplyReassemblesAcrossGatewayChunks` through the
  fake gateway on 28777, and `testRealRustGatewayChunksAndFreshReconnects`
  through the real gateway over three fresh connections. Each skips with
  `XCTSkip` when its fixture is not running.
- `AppearanceSettingsTests` covers the 19-family catalog, appearance and surface
  selection with persistence, accent derivation against the exported desktop
  roles, and import rejection for built-in ids. No fixture needed.
- `AppearanceImportTests` adds eight cases for the import path: install and
  persistence, derived accents for a family without presets, malformed preset
  cleanup, id collisions, repair of persisted customs, clearing an unreadable
  blob, repairing selections that no longer resolve, and replacing a family
  that a selection points into.
- `CompanionUITests` pairs, opens, and sends against the fixture and captures
  appearance screens. The tests pass `-appearance.*` launch arguments, and
  `-companion-settings` opens the settings sheet on launch.

The fixture host is a Node script with no real host behind it. It emulates the
gateway on `ws://127.0.0.1:28777` (`NOCHES_FIXTURE_PORT` overrides), answering
`EngineInfo`, `ListHarnesses`, `BigReply`, and the watch methods, recording
commands for `/commands`, and splitting replies into 32 KiB chunks with `end`
flags. It closes the socket on a client frame without `end: true`, and serves a
fixed profile: id `mobile-fixture`, key `a` repeated 64 times, device
`fixture-mac`. Run `npm install` in `edge/` first if its `node_modules` is
missing, since the script loads `ws` from there. It touches no real files,
hosts, or agents, and prints its own `noches-connect:` code. `BigReply`
answers with 15,000 moon emoji, 60 KB of UTF-8, so it spans several chunks.

```sh
node scripts/fixtures/noches-mobile-host.mjs
```

`--engine` switches the same script to a raw ndjson RPC upstream on
`ws://127.0.0.1:28778`, the host a real gateway expects. To put the Rust
gateway in front of it, build the binary, start the raw fixture, and write a
dummy credentials file for the test profile:

```sh
cargo build -p zeron-rpc --bin noches-connect
node scripts/fixtures/noches-mobile-host.mjs --engine &
FIXTURE=$!
RIG="$(mktemp -d /tmp/noches-gateway.XXXXXX)"
python3 - "$RIG/credentials.json" <<'PY'
import json, sys
profile = {"id": "mobile-fixture", "name": "Studio fixture",
           "endpoint": "ws://127.0.0.1:28779", "token": "a" * 64,
           "deviceId": "fixture-mac"}
json.dump({"clients": [profile]}, open(sys.argv[1], "w"))
PY
./target/debug/noches-connect serve --bind 127.0.0.1:28779 \
  --upstream ws://127.0.0.1:28778 --credentials "$RIG/credentials.json" &
GATEWAY=$!
```

The gateway binds loopback 28779 and proxies to the raw fixture on 28778, so
`testRealRustGatewayChunksAndFreshReconnects` exercises the real chunking path
end to end. When the run is over:

```sh
kill "$GATEWAY" "$FIXTURE"
rm -rf "$RIG"
```

The key in this rig is the throwaway fixture key, and the credentials file
lives only in the temp directory.

## Code map

- `apps/ios/Zeron/Companion/ConnectionProfile.swift`: profile, code parsing
  and validation, Keychain storage.
- `apps/ios/Zeron/Companion/DirectConnection.swift`: WebSocket framing, chunk
  reassembly, deadlines, heartbeats, and the never-replay failure path.
- `apps/ios/Zeron/Companion/CompanionModel.swift`: catalogs, transcript
  frames, sessions, and commands.
- `apps/ios/Zeron/Companion/CompanionView.swift`: companion home, pairing
  sheet, session view, and composer.
- `apps/ios/Zeron/Theme/AppearanceSettings.swift`: catalog, appearance and
  accent state, custom-family import.
- `crates/rpc/src/bin/noches-connect.rs`: gateway `serve`, `pair`, `revoke`,
  `import`, and `probe`.

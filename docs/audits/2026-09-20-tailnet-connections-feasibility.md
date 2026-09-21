# Noches connections over the tailnet

Tested on September 20, 2026, from the macOS checkout to `kidsseeghosts` over SSH.

**Conclusion: feasible.** A Mac client can already read the Linux Noches engine's
workspace, transcripts, and files through an SSH tunnel. No cloud sign-in was
needed. The Linux engine reported a local workspace. A supported Connections
feature needs host management and recovery; a combined local/remote sidebar also
needs multiple engine connections and explicit routing.

This investigation added a repeatable read probe and this report. It did not
implement connection UI, change either installation's configuration, restart
existing engines, or submit agent prompts.

## Actual host and transport

SSH to `kidsseeghosts` succeeded with batch authentication. Its Tailscale address
is `100.114.177.75`. Two separate installations are running:

| Installation | Linux executable | IPC port |
| --- | --- | --- |
| Upstream Zeron | `/home/kidsseeghosts/.zeron/app/0.2.59/zeron` | 27654 |
| Noches development build | `/home/kidsseeghosts/.local/share/noches-dev/builds/a1676c2c/zeron` | 27656 |

The development engine uses `/home/kidsseeghosts/.zeron-upstream` as its data
directory. Both a headed client and a headless engine were running for this
installation. Port 27656 is owned by the headless engine. These are observed
settings, not assumptions based on the upstream default.

The probe opened an ephemeral Mac localhost port, forwarded it to Linux
`127.0.0.1:27656`, and sent Noches RPC frames over WebSocket. It terminated its
SSH processes afterward. The engine's listener stayed on localhost; SSH supplied
authentication and transport encryption.

| Live check | Result |
| --- | --- |
| EngineInfo and EngineReady | Passed; local workspace |
| WatchDevices | Passed; 1 device |
| WatchChats | Passed; 4 chats |
| WatchSpaces | Passed; 1 space |
| ListRepos | Passed; empty repository list |
| WatchDocMessages | Existing transcript read successfully |
| ListWorkspaceDirectory | Existing chat checkout listed successfully |
| ReadWorkspaceFile | README text and content hash returned successfully |
| Two simultaneous WebSocket clients | Same engine identity; passed |
| 30 sequential EngineInfo calls | Final run: 5.95 ms median, 6.32 ms p95 |
| Terminate the temporary SSH connection | Both client sockets closed |
| Recreate tunnel, create client, resubscribe | Same identity and chat subscription restored |

The timings describe tiny RPC replies on the current network, not bulk transfer
speed or performance from another location. Reconnection was explicitly performed
by the probe; this does not establish automatic recovery in the desktop app.
Transcript and file contents are neither logged nor included in this report.

Reproduce with Node 22 or later and the existing SSH configuration:

```sh
node scripts/probe-remote-engine.mjs kidsseeghosts 27656
```

The script uses existing chats for read checks. An empty workspace skips the
transcript check. A chat without a valid checkout cannot pass the directory check.

## Automated verification

These commands ran successfully against the current Mac working tree:

```sh
cargo test -p zeron-rpc --lib --test device_room
cargo test -p zeron-engine --test device_routing --test workspace_sync --test relay_delivery --test local_profiles
cargo test -p zeron-ui --lib state::tests::
cargo test -p zeron-sync --lib
```

| Suite | Passed | Ignored |
| --- | ---: | ---: |
| RPC unit tests | 14 | 0 |
| Device-room integration | 12 | 1 |
| Engine device routing | 7 | 0 |
| Engine profile isolation | 6 | 0 |
| Relay command delivery | 1 | 0 |
| Workspace sync | 8 | 1 |
| UI state and engine attachment | 60 | 0 |
| Sync transport and storage | 53 | 0 |
| Total | 161 | 2 |

Coverage includes terminal streaming, workspace file reads and writes, command
execution on the intended engine, single consumption of queued commands,
workspace convergence, offline creation and viewer restart, relay failure and
recovery, authentication transitions, and UI attachment to an external daemon.
The cloud-dependent tests were ignored because they require an explicitly
configured live edge. Integration tests use local fixtures and mock agents;
they are not live Linux agent execution tests. Existing compiler warnings did
not prevent the selected suites from passing.

## Implementation findings

1. **The UI already supports an external engine.**
   `EngineHandle::bootstrap` in `crates/ui/src/state.rs` probes the configured
   localhost port and attaches using the same RPC contract as an embedded
   engine. `ZERON_IPC_PORT` is configurable. An SSH tunnel therefore fits the
   current attachment path without changing the wire protocol.

2. **Direct socket recovery needs implementation.**
   `connect_ws` in `crates/rpc/src/client.rs` dials once. Its transport task exits
   when the connection closes. `RemoteEngine` holds that client, while UI watch
   loops retry subscriptions on the same handle. A supported remote connection
   needs to recreate the client and subscriptions after SSH or network failure.
   The current relay LinkCache has separate recovery logic, but it specifically
   connects through device rooms on the edge.

3. **A remote-only connection must not silently embed a local engine.**
   Today a failed bootstrap probe falls back to embedding. That is useful for
   normal desktop startup but wrong for an explicitly selected SSH host. Add an
   explicit host mode that reports the remote host as unavailable and retries
   without changing where subsequent work will run.

4. **A combined sidebar is more than a tunnel.**
   `AppState` holds one `Option<EngineHandle>`. Cloud mode combines device rows
   through RegistryClient, syncs chat documents, and forwards device-addressed
   operations through LinkCache. Independent local profiles have none of those
   cloud links. Supporting them together requires per-host clients, aggregated
   lists, host-scoped selection/cache keys, and routing every operation to its
   owning engine. Existing file, terminal, and command handlers can be reused.

5. **Cloud sync and SSH control solve different storage requirements.**
   Current account sync still defaults to `https://edge.zeron.sh`; setting an
   edge URL is not a replacement for its registry, chat, and device-room APIs.
   SSH control can leave sessions and repositories on Linux and access them
   from the Mac. It does not replicate both local profiles for offline use or
   transfer a running agent and its Git state to another machine. Those would
   be separate features. The desktop's existing Bring my work flow imports
   local work into an account workspace, not into an SSH peer's local profile.

## Recommended scope

Start with a saved SSH host connection and an explicit host switcher. Reuse
OpenSSH configuration, connect to the installed engine on its configured port,
verify engine identity/capabilities, and show connection state. Include automatic
recovery and the remote-only startup behavior in that first version.

Then add simultaneous local and remote work in one sidebar. Keep each host's
engine authoritative for its own work and route reads and actions to that host.
This avoids requiring a new replication service merely to control Linux from
the Mac. Remote bootstrap or installation can follow once attachment works
reliably; this Linux machine already has the necessary engine running.

Before calling the feature ready, verify a real Mac UI session with Linux agent
execution, interactive approvals, terminal resize/replay, attachments/images,
preview URL reachability, sleep/wake, host restarts, mixed engine versions, and
host switching with pending work. Local browser/preview URLs may require
additional forwards. This audit exercised the live RPC path and automated UI
state tests, not a rendered Mac-to-Linux desktop session or those full workflows.

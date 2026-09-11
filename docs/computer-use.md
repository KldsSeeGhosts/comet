# Managed computer use

Noches owns computer use for Pi sessions on Linux. Pi exposes the
`noches_cua` tool; the engine authorizes requests and launches the installed
`cua-driver`. The driver still draws the session-colored agent cursor and
performs desktop inspection and input delivery.

```text
Pi tool adapter -> private Unix socket -> Noches engine -> cua-driver mcp
                                                              | (proxied)
                                                  cua-driver serve (per-run daemon)
```

The adapter is bundled at `crates/harness/src/pi/noches-cua.ts`. The engine
implementation is `crates/engine/src/computer_use.rs`.

## Permissions and ownership

- The first app inspection or control request asks through Noches' native
  question UI. One approval covers the engine host for the current session:
  inspecting and controlling apps, pointer and keyboard input, screenshots
  and clipboard access, including visible desktop control. It is not a
  per-app allowlist. A denial lasts until the turn ends; a fresh turn may
  ask again.
- One chat at a time can hold this engine's desktop lease. It covers the
  whole active turn, including pauses between tool calls, rather than just
  individual clicks. Other chats receive a busy error.
- Turn completion closes the driver session, reaps the driver and releases
  the lease. A parked Pi process keeps its bridge socket and its approval,
  but no desktop lease or driver. The next turn re-acquires both without
  asking again.
- On a real host the engine runs `cua-driver serve` behind the MCP child for
  the length of the turn. Metadata asked before approval starts a daemon
  without `--grant existing-profile`; that daemon never has the grant, and
  it is replaced by a granted daemon once approval exists. The daemon owns
  the agent cursor overlay runloop, so the synthetic
  cursor renders on screen instead of only updating in memory. Both
  processes are reaped before the lease is released.
- Stop, a disconnected tool caller, transport failure or timeout cancels
  outstanding work. The Linux driver process is killed and reaped before
  its lease is released. Already delivered input cannot be undone. Unknown
  outcomes are never replayed automatically.
- The engine stamps its own session label. Session lifecycle tools,
  configuration changes, recordings and unreviewed actions are not exposed.
  `help` and `describe` use MCP `tools/list`, not nonexistent driver tools.

Driver authorization stays in `standard` mode. A daemon started after
approval carries the narrow `--grant existing-profile`, which only admits
attaching DevTools to an existing logged-in Chromium-family profile after
the user approved computer use for the session; a pre-approval metadata
daemon never carries the grant and is replaced by a granted one when
approval arrives. Inherited `CUA_*` environment settings are removed
before launching the child, then standard mode, that grant and Wayland
support are set explicitly. Driver-level permission refusals remain errors;
a Noches grant does not bypass them.

The socket directory is private, mode 0700, and the socket is mode 0600.
Each run gets a distinct connection and identity. No computer-use endpoint
is exposed through the remote UI RPC API. A remote viewer's approval applies
to the named engine host, not the viewer's own desktop.

## Pi integration

The adapter disables the direct `cua` tool in Noches-managed Pi processes
and blocks calls to it if another extension reactivates it. Other extensions
and model providers remain enabled. Global Pi configuration is unchanged,
so standalone Pi keeps its existing CUA setup.

The adapter is loaded even when bridge startup fails. In that case managed
calls fail explicitly; the legacy tool does not silently take over. Failure
to install the adapter prevents that Pi process from starting.

Full MCP text, images and structured results reach the adapter. Structured
results are both retained in tool details and included in model-readable
text, so accessibility tokens and delivery facts survive. Combined text is
capped at 50KB or 2000 lines. Larger text is saved in a private temporary
file, with its path in the tool result. These local result files remain
available until removed or cleaned by the operating system. Treat them as
sensitive app content.

## Installation and limits

Install a compatible `cua-driver` on the engine host. Noches resolves an
explicit `CUA_DRIVER_PATH`, then PATH, then `~/.local/bin/cua-driver`.
An invalid explicit path fails rather than choosing another executable.
Noches does not download or update the driver automatically.

This first integration supports Linux and Pi. Other agent backends do not
yet connect to this service. macOS needs an app-owned driver lifecycle and
permission implementation before it can be enabled here.

This is application-level coordination, not a sandbox. Agents with shell
access and trusted extensions running as the same desktop user retain
those capabilities. Separate Noches engines and unrelated desktop tools do
not share the lease. Dev and production remain separate installations, but
both can still target the same physical desktop.

## Development verification

On `dev`, use only the dev feature. Bundled adapter and context-extension
files honor `ZERON_DATA_DIR`, otherwise `~/.zeron-dev` for a dev build.
No managed runtime files are written to production's `~/.zeron`.

```sh
cargo test -p zeron-engine -p zeron-harness --features dev
node --experimental-vm-modules --test crates/harness/tests/noches-cua.test.mjs
cargo clippy -p zeron-engine -p zeron-harness --all-targets --features dev -- -D warnings
./install.sh --dev
systemctl --user restart zeron-dev.service
```

The adapter tests require Node 22 with `stripTypeScriptTypes` support. Engine
tests use injected Python MCP fixtures, never a real driver or process-global
environment mutation. They check full results, permissions, lease release
between turns, interruption during blocked input, client disconnection,
frame limits and partial-client teardown.

For a manual smoke test, open a new Pi chat in Noches dev and ask it to use
`noches_cua` for `help`, `describe` and `health_report`. Then ask it to inspect
only the dev window and move its synthetic cursor. Approve the native prompt
only for that test. Verify the cursor disappears when the turn ends and
that another chat can request control. Do not approve production-window
input as part of a dev smoke test.

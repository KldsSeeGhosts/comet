# Noches local API

`zeron-local-api` transports HTTP requests over a private Unix socket. It does not implement layouts, agents, teams, coordination state, or workspace mutations. The app's UI thread owns those operations.

## Application wiring

The workspace includes this crate and the `noches` CLI. The UI owns the domain
bridge in `crates/ui/src/workspace/control.rs`. Cargo discovers
`apps/zeron/src/bin/noches.rs` as `noches`; build it on dev with
`cargo build -p zeron --bin noches --features dev`. Installation names it
`noches-dev`. The CLI name and discovery directory depend on the `dev` feature.

## UI bridge

```rust,ignore
let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
let api = zeron_local_api::ControlPlane::start(
    selected_variant_data_dir,
    "main".to_owned(),
    sender,
).await?;
let events = api.events();

// Drain receiver on the UI executor, not on the HTTP task's thread.
// Request { method: String, params: Value,
//           reply: oneshot::Sender<Result<Value, String>> }
// Dispatch method, then reply.send(actual_ui_result).
// Keep api alive for the application's lifetime.
```

`ControlPlane::manifest()` and `socket_path()` expose this instance's identity. `shutdown().await` stops the listener and waits for cleanup. Drop requests shutdown without blocking. A two-second shutdown deadline bounds open connections. `EventHub` is cloneable and `publish(topic, Value)` is safe from the UI thread.

The bridge must reject unsupported operations rather than acknowledge them as complete. A successful reply means the UI operation actually ran, or returns a real domain job identifier. The transport does not manufacture job or completion state. A dropped or timed-out HTTP reply does not undo an operation already dispatched to the UI. Check `request.reply.is_closed()` before starting cancellable work. A timeout is an uncertain outcome, so clients must not blindly retry mutations.

## HTTP contract

`GET /healthz` returns an unwrapped `Manifest`:

```json
{
  "protocol_version": 1,
  "instance_id": "f3ef05e7-7e48-4f79-9d95-5c8c9161b967",
  "instance_name": "main",
  "pid": 1234,
  "socket": {
    "path": "/home/user/.zeron-dev/local-api/main.sock",
    "device": 1,
    "inode": 2345,
    "uid": 1000
  }
}
```

`POST /api/v1/{domain}/{verb}` accepts a JSON object as the complete `params` value. The bridge receives `method = "{domain}.{verb}"`. Hyphens remain hyphens, for example `tab.split-view` and `coordination-state.set`. The body limit is axum's default 2 MiB. Domains and verbs must be 1-48 ASCII letters, digits, underscores, or hyphens.

Read verbs also accept GET: `get`, `list`, `views`, `read`, `status`, `should-stop`, `subscribe`, `watch`, and `inspect`. Use `?params=<URL-encoded JSON object>` to preserve JSON types. Without `params`, ordinary query values become strings. Do not mix the two forms. Other verbs require POST. The UI must still enforce each domain's read/write semantics.

Responses are `{"ok":true,"result":<UI value>}` or `{"ok":false,"error":{"code":"...","message":"..."}}`. HTTP errors include 400 for invalid params, 409 for a changed instance, 422 for a UI `Err(String)`, 503 for an unavailable bridge, and 504 after a 30-second UI reply deadline. `agent.wait` must use a bounded domain wait shorter than that deadline or return a job handle. Router-level 404/405 responses may be plain text.

Clients attach `x-noches-instance: <UUID>` to forwarded requests. The server rejects stale UUIDs before dispatch, including when a socket path has been reused. The header is optional for manual tools. Filesystem access is the authorization boundary, not an HTTP bearer token. Same-user processes may connect. Mutation authorization is separate: the UI requires a human Allow grant in the selected workspace, and launches require its separate orchestration grant. No API request can mint either grant.

## Subscriptions

Send the same request with `Accept: text/event-stream`. `Client::subscribe` uses POST; GET read subscriptions work too. The UI receives the ordinary method and params and must return:

```json
{"topics":["agent:stable-id"],"snapshot":{"version":42}}
```

Topics must be nonempty exact strings. The UI chooses topics after validating the target. It can publish with `api.events().publish("agent:stable-id", json_value)`. Do not allocate a persistent per-subscriber resource in the UI; this transport has no per-subscriber cleanup callback.

SSE begins with `event: ready` and the subscription reply as JSON data. Matching publications arrive as `event: message`, a numeric sequence in `id`, and `{"sequence":1,"topic":"...","data":...}`. A receiver attaches before UI dispatch, so publications during subscription setup are buffered. Snapshots and queued events can overlap; the UI must include domain versions for deduplication. No total ordering across distinct UI operations is implied.

The shared broadcast buffer holds 256 events, including unrelated topics. A lagging receiver gets `event: gap` with `{"missed":N}` and the stream closes. The CLI reports a gap or unexpected closure as failure. There is no replay or automatic reconnect. `Last-Event-ID` is rejected; subscribe again and read a fresh snapshot. Keepalive comments arrive every 15 seconds. Ctrl-C ends CLI subscriptions successfully.

## Identity and lifecycle

Each named instance uses `<data_dir>/local-api/<name>.sock`, `<name>.json`, and a permanent `<name>.lock`. The API directory must be owned by the current user with mode 0700. Sockets, manifests, and lock files use 0600. Symlinks at these paths are rejected. Existing API directory permissions are checked, not silently changed. Parent data-directory selection belongs to the app.

An exclusive flock serializes instances with the same name. A new instance never replaces an active listener. Reclaiming a socket requires all of the following:

* A valid private manifest matching the socket's path, owner, device, and inode.
* A failed health check and a Unix connect failure of exactly `ConnectionRefused`.
* `kill(pid, 0)` proving ESRCH. Live PIDs, reused PIDs, EPERM, timeouts, and ambiguous errors cause refusal.
* Unchanged manifest and socket identity after those checks.

An orphan socket without a manifest is left untouched. A missing socket with a live manifest PID is also refused. Cleanup only unlinks the instance's own recorded socket inode and matching UUID manifest, never replacements. Lock files stay in place to prevent split-lock races. Manifests are published atomically and never used as proof of health by themselves.

`Client::connect(path)` verifies the health identity against the filesystem socket. `discover_instances(data_dir)` lists manifests with an optional health error without deleting anything. `Client::discover(data_dir, socket_override, instance_name)` selects only a full manifest/health match and refuses ambiguous healthy instances. Explicit socket selection bypasses manifest discovery but still checks socket and health identity.

Default CLI discovery uses only `~/.zeron-dev` under the dev feature and only `~/.zeron` otherwise. It never falls back across variants. `--socket PATH` bypasses HOME and both default directories. `--instance NAME` disambiguates discovery. `instance list` shows stale records and their errors; `instance current` and `status` check the selected transport's health, not application domain status.

## CLI parameter mapping

All domain commands accept `--params '{...}'`. Explicit fields override keys in that object. Optional flags are omitted when absent so UI defaults remain authoritative. The CLI parses files and JSON but does not validate domain schemas.

* `layout views|list` forwards the params object. `layout compose --from-file FILE --dry-run --refresh-guard` maps to `composition`, `dry_run`, and `refresh_guard`. `layout save|apply|delete|run [NAME] [--from-file FILE] [--dry-run]` uses `name`, `composition`, and `dry_run`.
* `tab split|split-view [--to TARGET] [--direction left|right|up|down] [--ui chat|terminal|auto]` uses optional string fields `to`, `direction`, and `ui`. No direction or UI default is injected by the CLI.
* `agent send --to TARGET [MESSAGE] [--from-file FILE] [--queue]` uses string `to`, optional string `message`, and `queue: true` when requested. MESSAGE and `--from-file` are mutually exclusive. Message files are UTF-8 text.
* `agent wait --to TARGET [--idle] [--timeout SECONDS]` uses string `to`, `idle: true` when requested, and optional unsigned integer `timeout` in seconds. The UI owns waiting behavior and must respect the 30-second transport reply deadline.
* `agent read|subscribe|stop|interrupt|should-stop --to TARGET` uses the exact string `to`. Only `subscribe` requests SSE.
* `agents list`, `agents label --to TARGET LABEL`, and `agents group --to TARGET GROUP` use the named keys without resolving labels locally.
* `team run [--from-file FILE]` uses `spec`. `team report|status|cancel ID` uses `id`. `team list` forwards params. `team watch` requests SSE. Every operation requires an explicit `scope` with `workspace` and `worktree`, supplied through `--params`; run may take scope from its spec.
* `coordination-state get|watch KEY` uses `key`; watch requests SSE. `set KEY JSON [--if-version N]` uses `key`, parsed `value`, and optional numeric `if_version`. `delete KEY [--if-version N]` uses `key` and optional `if_version`. The UI implements compare-and-swap, including the meaning of version zero.
* `worktree|workspace|section VERB [--params JSON] [--subscribe] [--confirm]` forwards generic extension methods. `--confirm` forwards `confirm: true`.

Non-stream commands print the UI result as JSON. SSE prints newline-delimited JSON objects with `event`, `id`, and parsed `data`. Errors go to stderr with a nonzero exit code.

`layout run views|tabs|panes` accepts `--provider`, `--ui`, `--count`, `--label`,
`--prompt`, `--direction`, `--cwd`, and `--workspace`. A different positional name
loads a recipe. Inline `--from-file` compositions and named recipes determine
their own cells, so omit count/into. Planned runs require new empty cells and
assign fresh session IDs before a single guarded layout commit. A dry run
returns the proposed topology without creating chats or launching processes.
Existing session bindings survive inline compose plans. Named recipes replace
the topology; active CLI ownership still prevents unsafe removal.

`team report ID` also accepts `--label`, `--summary`, `--result-file`, and
`--report-capability`. The result file is a reference, never read by the CLI.
Supply the exact scope in `--params`.

## Consent and destructive operations

Human pane-menu actions grant API access and orchestration separately per
workspace. Grants and caller capabilities expire when the UI restarts. Revoking
API access also clears that workspace's session/report capabilities and pending
deletion. A plain `sessionId`, an `allow` parameter, or an API prompt cannot grant
permission.

`chat.new` and `layout.run` return `sessions` entries containing a `sessionId` and
random `sessionCapability`. `worktree.create` requires both fields plus the
workspace's orchestration grant and a recorded human input submission in that
same session. The UI captures a sealed proof at a real input callback; injected
API prompts and synthetic CUA dispatch cannot mint it. It refuses inline
`prompt` or `task` fields.
Team launch privately adds a role-specific `reportCapability` to each role's
prompt. `team.report` requires that capability and the exact team/scope/label.
Caller-supplied session IDs do not authenticate reports.

`workspace.delete` and `worktree.delete` require `confirm: true` (the CLI's
`--confirm`) to stage a request, then return `confirmationRequired: true`.
The UI holds the exact deletion request and requires the human confirmation
button to send it. Request fields cannot confirm it. Confirmation rechecks the
workspace grant and active worktree sessions; the engine atomically refuses
last-workspace deletion even when two requests race. `dryRun` or `dry_run`
previews deletions without requiring `confirm` or creating a pending
confirmation.

The UI opens one variant-specific orchestration database after acquiring its
API instance lock. Startup marks unfinished teams interrupted. Team launch,
report and cancellation serialize admission; cancellation is durable before
process interrupts, and a failed launch cancels and interrupts admitted roles.
If interruption fails, the API reports affected sessions. Coordination writes
and section edits require exact `if_version` values, including tombstones.

## Verification

After workspace wiring, run `cargo test -p zeron-local-api` and `cargo test -p zeron --bin noches --features dev`. The transport tests use real Unix sockets, not router-only mocks. The CLI tests verify argument routing without claiming that domain implementations exist.

API references: [axum Unix listeners](https://docs.rs/axum/0.8.9/axum/serve/trait.Listener.html), [reqwest Unix socket client](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html#method.unix_socket).

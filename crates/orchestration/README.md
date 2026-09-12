# Orchestration storage

`zeron-orchestration` stores team runs and coordination values in a dedicated SQLite database. It does not launch sessions, execute prompts, read result files, authenticate callers, or stop processes.

## Public API

All methods return `anyhow::Result<T>`. `Store` is cloneable, `Send + Sync`, and owns a connection behind a mutex. These methods block. Async callers must use `tokio::task::spawn_blocking`.

```rust,ignore
Store::open(path: impl AsRef<Path>) -> Result<Store>
store.recover_interrupted() -> Result<Vec<Team>>
store.team_create(spec: TeamSpec) -> Result<Team>
store.team_get(scope: &Scope, id: &str) -> Result<Option<Team>>
store.team_list(scope: &Scope) -> Result<Vec<Team>>
store.bind_role_session(scope: &Scope, id: &str, label: &str, session_id: &str) -> Result<Team>
store.team_report(scope: &Scope, id: &str, label: &str, session_id: &str, report: Report) -> Result<Team>
store.team_cancel(scope: &Scope, id: &str) -> Result<Team>
store.coordination_get(scope: &Scope, key: &str) -> Result<CoordinationEntry>
store.coordination_set(scope: &Scope, key: &str, if_version: i64, value: serde_json::Value) -> Result<CoordinationEntry>
store.coordination_delete(scope: &Scope, key: &str, if_version: i64) -> Result<CoordinationEntry>
store.subscribe(scope: &Scope) -> Result<Subscription>
```

DTO fields are public. DTOs implement Serde serialization and deserialization, except `Subscription`, which owns a Tokio receiver.

```rust,ignore
Scope { workspace: String, worktree: String }
RoleSpec { label: String, provider: String, prompt: String }
TeamSpec { scope: Scope, roles: Vec<RoleSpec> }
Report { summary: String, result_file: Option<String> }
Role {
    label: String, provider: String, prompt: String,
    session_id: Option<String>, report: Option<Report>,
}
Team { id: String, scope: Scope, status: TeamStatus, roles: Vec<Role> }
TeamStatus::{Running, Completed, Cancelled, Interrupted}
CoordinationEntry {
    scope: Scope, key: String, version: i64,
    value: Option<serde_json::Value>,
}
Event::{TeamChanged(Team), CoordinationChanged(CoordinationEntry)}
Snapshot { scope: Scope, teams: Vec<Team>, coordination: Vec<CoordinationEntry> }
Subscription { snapshot: Snapshot, events: tokio::sync::broadcast::Receiver<Event> }
```

Status strings use snake case. Events serialize as `{"kind":"team_changed","record":...}` or `{"kind":"coordination_changed","record":...}`. Coordination `value` is absent for tombstones, but present as JSON `null` for a stored null value. This distinction survives serialization round trips.

## Team lifecycle

Creation validates all 1-8 roles before writing anything. Labels must be unique within a team. Team IDs are UUIDs. New teams are `Running`, with unbound roles. The engine launches each role through its existing Run command, then binds the returned session ID. Bindings cannot change, and one session cannot fill multiple roles in the same team. Repeating the same binding while running is a no-op.

The engine must authenticate the reporting session and authorize its scope. The store then requires an exact match of workspace, worktree, team ID, role label, and bound session ID. Reports are immutable. Identical retries succeed while running or completed, without emitting another event. Conflicting retries fail. Only explicit reports from every role transition a team to `Completed`. Session exit alone does not complete a role.

Cancellation changes a running record to `Cancelled`. Repeated cancellation is a no-op. The engine separately decides how to stop sessions. Completed, cancelled, and interrupted teams cannot resume, bind new sessions, or accept new reports.

Opening a database does not alter run status. Call `recover_interrupted()` once at engine startup, before accepting requests or launching roles. It changes every running team in that database to `Interrupted` in one transaction, preserves existing bindings and reports, and leaves other terminal states untouched. Repeating recovery changes nothing. There is no automatic resume operation.

## Coordination and events

Scopes are opaque, exact string pairs, not canonicalized paths. The engine must supply stable workspace/worktree identities from trusted context. Every lookup, mutation, snapshot, and event subscription is scoped to both fields. Only explicit startup recovery spans scopes.

CAS requires the current version. A never-written key has version zero and no value. Every successful set or delete increments that key's version, including deleting an already absent key. Deletion retains a durable tombstone. Never discard tombstones or reset versions; stale writes would otherwise become valid again. Exhausted `i64` versions fail without changing data. Version conflicts return an error containing the expected and current versions; clients can reread before retrying.

`subscribe` captures a transactionally consistent snapshot and registers its receiver under the same mutex used for commits. Events publish synchronously after successful commit, in mutation order. Failed operations and idempotent team retries publish nothing. Snapshots include tombstones. Receivers buffer 256 events per scope. On `RecvError::Lagged`, discard that receiver and subscribe again for a fresh snapshot.

Use clones of one engine-owned `Store`. Separate `open` calls coordinate durable writes through SQLite transactions, but do not share the in-process event channels. External database writers cannot provide live events through this store. Lists and snapshots currently include every record in the requested scope.

## Limits and database checks

Limits count UTF-8 bytes, not characters. All string fields reject NUL; only report summaries may be empty or whitespace-only.

| Field | Maximum |
| --- | ---: |
| Workspace | 1,024 bytes |
| Worktree or result-file reference | 4,096 bytes |
| Team ID, role label, provider | 128 bytes |
| Session ID or coordination key | 256 bytes |
| Prompt | 65,536 bytes |
| Report summary | 16,384 bytes |
| Coordination value, serialized JSON | 65,536 bytes |
| Coordination JSON nesting depth | 32 |

Public constants are `MAX_ROLES`, `MAX_PROMPT_BYTES`, `MAX_SUMMARY_BYTES`, and `MAX_VALUE_BYTES`. Result-file references are opaque metadata, not proof that a file exists. Validate file access separately before opening one.

The store uses parameterized SQL, immediate write transactions, strict SQLite tables, a five-second busy timeout, and `synchronous=FULL`. It checks the application ID, schema version, and exact table/index/trigger definitions before accepting an existing database. Unknown or changed schemas fail rather than being migrated destructively. Team records and coordination values are validated when read. Request DTOs reject unknown fields.

## Application integration

The UI control bridge opens one variant-specific database after taking its
instance lock and runs startup recovery before dispatching requests. It checks
workspace consent and caller capabilities, validates providers and `TeamSpec`,
launches roles through existing Run commands, and binds their sessions. Blocking
store calls run on a background executor. Scoped watch events reach the Unix
socket API through the shared event hub.

The API exposes durable team status and reports; there is no dedicated team
status panel yet. Cancellation commits the durable record before interrupting
its sessions. A launch failure cancels its run and interrupts admitted roles.

## Checks

```sh
cargo test -p zeron-orchestration
cargo clippy -p zeron-orchestration --all-targets -- -D warnings
```

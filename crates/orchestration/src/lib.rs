//! Durable team records and worktree-scoped coordination state.
//!
//! All methods perform blocking SQLite I/O. Call them through `spawn_blocking`
//! from async code. Share clones of one store to receive all local events.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::broadcast;
use uuid::Uuid;

pub const MAX_ROLES: usize = 8;
pub const MAX_SUMMARY_BYTES: usize = 16 * 1024;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
const APP_ID: i64 = 0x5a4f5243;
const SCHEMA: &str = "
CREATE TABLE teams (
 workspace TEXT NOT NULL, worktree TEXT NOT NULL, id TEXT NOT NULL,
 data TEXT NOT NULL CHECK(json_valid(data)),
 PRIMARY KEY(workspace, worktree, id)
) STRICT;
CREATE TABLE coordination (
 workspace TEXT NOT NULL, worktree TEXT NOT NULL, key TEXT NOT NULL,
 version INTEGER NOT NULL CHECK(version > 0),
 value TEXT CHECK(value IS NULL OR json_valid(value)),
 PRIMARY KEY(workspace, worktree, key)
) STRICT;";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub workspace: String,
    pub worktree: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSpec {
    pub label: String,
    pub provider: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamSpec {
    pub scope: Scope,
    pub roles: Vec<RoleSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Running,
    Completed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub summary: String,
    pub result_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub label: String,
    pub provider: String,
    pub prompt: String,
    pub session_id: Option<String>,
    pub report: Option<Report>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Team {
    pub id: String,
    pub scope: Scope,
    pub status: TeamStatus,
    pub roles: Vec<Role>,
}

/// Version zero means this key has never existed. `None` with a nonzero version
/// is a durable tombstone, not a reset to zero. JSON null is `Some(Value::Null)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinationEntry {
    pub scope: Scope,
    pub key: String,
    pub version: i64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum Event {
    TeamChanged(Team),
    CoordinationChanged(CoordinationEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub scope: Scope,
    pub teams: Vec<Team>,
    /// Includes tombstones so clients retain CAS versions.
    pub coordination: Vec<CoordinationEntry>,
}

pub struct Subscription {
    pub snapshot: Snapshot,
    pub events: broadcast::Receiver<Event>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
    /// Directory containing the database file. Out-of-band role capability
    /// files live under `<dir>/runs/<run id>/roles/`.
    dir: PathBuf,
}

struct Inner {
    db: Connection,
    subscribers: HashMap<Scope, broadcast::Sender<Event>>,
}

impl Scope {
    fn validate(&self) -> Result<()> {
        field("workspace", &self.workspace, 1024, false)?;
        field("worktree", &self.worktree, 4096, false)
    }
}

impl TeamSpec {
    /// Validate a planned run without creating durable state or launching roles.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        validate_roles(&self.roles)
    }
}

fn field(name: &str, value: &str, max: usize, empty: bool) -> Result<()> {
    ensure!(value.len() <= max, "{name} exceeds {max} UTF-8 bytes");
    ensure!(empty || !value.trim().is_empty(), "{name} is empty");
    ensure!(!value.contains('\0'), "{name} contains NUL");
    Ok(())
}

fn validate_roles(roles: &[RoleSpec]) -> Result<()> {
    ensure!(
        (1..=MAX_ROLES).contains(&roles.len()),
        "team requires 1..=8 roles"
    );
    let mut labels = HashSet::new();
    for role in roles {
        field("label", &role.label, 128, false)?;
        // Labels become capability file names under runs/<id>/roles/.
        ensure!(
            !role.label.contains(['/', '\\']) && role.label != "." && role.label != "..",
            "role label must be a safe file name"
        );
        field("provider", &role.provider, 128, false)?;
        field("prompt", &role.prompt, MAX_PROMPT_BYTES, false)?;
        ensure!(labels.insert(&role.label), "duplicate role label");
    }
    Ok(())
}

fn validate_report(report: &Report) -> Result<()> {
    field("summary", &report.summary, MAX_SUMMARY_BYTES, true)?;
    if let Some(path) = &report.result_file {
        // This is an opaque reference. The store never reads or executes it.
        field("result_file", path, 4096, false)?;
    }
    Ok(())
}

fn validate_team(team: &Team) -> Result<()> {
    field("team id", &team.id, 128, false)?;
    team.scope.validate()?;
    let specs: Vec<_> = team
        .roles
        .iter()
        .map(|r| RoleSpec {
            label: r.label.clone(),
            provider: r.provider.clone(),
            prompt: r.prompt.clone(),
        })
        .collect();
    validate_roles(&specs)?;
    let mut sessions = HashSet::new();
    for role in &team.roles {
        if let Some(session) = &role.session_id {
            field("session id", session, 256, false)?;
            ensure!(sessions.insert(session), "duplicate role session");
        }
        if let Some(report) = &role.report {
            ensure!(role.session_id.is_some(), "unbound role has report");
            validate_report(report)?;
        }
    }
    let all_reported = team.roles.iter().all(|r| r.report.is_some());
    ensure!(
        (team.status == TeamStatus::Completed) == all_reported,
        "invalid team completion state"
    );
    Ok(())
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

fn schema_objects(db: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut stmt = db.prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type, name")?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

impl Store {
    /// Opens or creates a dedicated database. Opening does not recover runs.
    /// Unknown schema versions or foreign database schemas are rejected.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA trusted_schema = OFF; PRAGMA synchronous = FULL;",
        )?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let app_id: i64 = tx.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let objects = schema_objects(&tx)?;
        if version == 0 && app_id == 0 && objects.is_empty() {
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.pragma_update(None, "application_id", APP_ID)?;
        } else {
            ensure!(
                version == 1 && app_id == APP_ID,
                "unsupported orchestration database schema"
            );
            let reference = Connection::open_in_memory()?;
            reference.execute_batch(SCHEMA)?;
            ensure!(
                objects == schema_objects(&reference)?,
                "orchestration database schema mismatch"
            );
        }
        tx.commit()?;
        let dir = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                db,
                subscribers: HashMap::new(),
            })),
            dir,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("orchestration store lock poisoned"))
    }

    /// Directory holding the database file; empty for unnamed paths such as
    /// `:memory:`. Callers derive per-run capability file locations from it.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn team_create(&self, spec: TeamSpec) -> Result<Team> {
        // Validate the entire request before acquiring a write transaction.
        spec.validate()?;
        let team = Team {
            id: Uuid::new_v4().to_string(),
            scope: spec.scope,
            status: TeamStatus::Running,
            roles: spec
                .roles
                .into_iter()
                .map(|r| Role {
                    label: r.label,
                    provider: r.provider,
                    prompt: r.prompt,
                    session_id: None,
                    report: None,
                })
                .collect(),
        };
        let mut inner = self.lock()?;
        let tx = inner
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO teams(workspace,worktree,id,data) VALUES (?1,?2,?3,?4)",
            params![
                team.scope.workspace,
                team.scope.worktree,
                team.id,
                serde_json::to_string(&team)?
            ],
        )?;
        tx.commit()?;
        inner.publish(Event::TeamChanged(team.clone()));
        Ok(team)
    }

    pub fn team_get(&self, scope: &Scope, id: &str) -> Result<Option<Team>> {
        scope.validate()?;
        field("team id", id, 128, false)?;
        read_team(&self.lock()?.db, scope, id)
    }

    pub fn team_list(&self, scope: &Scope) -> Result<Vec<Team>> {
        scope.validate()?;
        list_teams(&self.lock()?.db, scope)
    }

    fn mutate_team(
        &self,
        scope: &Scope,
        id: &str,
        mutate: impl FnOnce(&mut Team) -> Result<()>,
    ) -> Result<Team> {
        scope.validate()?;
        field("team id", id, 128, false)?;
        let mut inner = self.lock()?;
        let tx = inner
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut team = read_team(&tx, scope, id)?.context("team not found in scope")?;
        let before = team.clone();
        mutate(&mut team)?;
        validate_team(&team)?;
        if team != before {
            save_team(&tx, &team)?;
        }
        tx.commit()?;
        if team != before {
            inner.publish(Event::TeamChanged(team.clone()));
        }
        Ok(team)
    }

    /// Bind once, after the caller launches the role with its existing Run path.
    pub fn bind_role_session(
        &self,
        scope: &Scope,
        id: &str,
        label: &str,
        session_id: &str,
    ) -> Result<Team> {
        field("label", label, 128, false)?;
        field("session id", session_id, 256, false)?;
        self.mutate_team(scope, id, |team| {
            ensure!(team.status == TeamStatus::Running, "team is terminal");
            ensure!(
                !team
                    .roles
                    .iter()
                    .any(|r| r.label != label && r.session_id.as_deref() == Some(session_id)),
                "session already bound to another role"
            );
            let role = team
                .roles
                .iter_mut()
                .find(|r| r.label == label)
                .context("role not found")?;
            match &role.session_id {
                Some(existing) => ensure!(
                    existing == session_id,
                    "role already bound to another session"
                ),
                None => role.session_id = Some(session_id.to_owned()),
            }
            Ok(())
        })
    }

    /// The caller must authenticate the session. All four identity fields are
    /// checked here; a role label or team id alone grants no reporting rights.
    pub fn team_report(
        &self,
        scope: &Scope,
        id: &str,
        label: &str,
        session_id: &str,
        report: Report,
    ) -> Result<Team> {
        field("label", label, 128, false)?;
        field("session id", session_id, 256, false)?;
        validate_report(&report)?;
        self.mutate_team(scope, id, |team| {
            ensure!(
                matches!(team.status, TeamStatus::Running | TeamStatus::Completed),
                "team is terminal"
            );
            let role = team
                .roles
                .iter_mut()
                .find(|r| r.label == label)
                .context("role not found")?;
            ensure!(
                role.session_id.as_deref() == Some(session_id),
                "role/session identity mismatch"
            );
            if let Some(existing) = &role.report {
                ensure!(
                    existing == &report,
                    "role already reported a different result"
                );
                return Ok(());
            }
            ensure!(team.status == TeamStatus::Running, "team is terminal");
            role.report = Some(report);
            if team.roles.iter().all(|r| r.report.is_some()) {
                team.status = TeamStatus::Completed;
            }
            Ok(())
        })
    }

    pub fn team_cancel(&self, scope: &Scope, id: &str) -> Result<Team> {
        self.mutate_team(scope, id, |team| {
            ensure!(
                matches!(team.status, TeamStatus::Running | TeamStatus::Cancelled),
                "team is terminal"
            );
            team.status = TeamStatus::Cancelled;
            Ok(())
        })
    }

    /// Call exactly at engine startup, before accepting work. Marks all running
    /// teams interrupted in one transaction. Never launches or resumes sessions.
    pub fn recover_interrupted(&self) -> Result<Vec<Team>> {
        let mut inner = self.lock()?;
        let tx = inner
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut changed = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT workspace,worktree,id,data FROM teams ORDER BY workspace,worktree,id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    Scope {
                        workspace: r.get(0)?,
                        worktree: r.get(1)?,
                    },
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (scope, id, data) = row?;
                // One undecodable row must not abort recovery for every other
                // team; a skipped row keeps its stored bytes untouched.
                let mut team = match decode_team(&scope, &id, &data) {
                    Ok(team) => team,
                    Err(error) => {
                        tracing::warn!(
                            row_id = %id,
                            error = %error,
                            "skipping undecodable orchestration team row during recovery"
                        );
                        continue;
                    }
                };
                if team.status == TeamStatus::Running {
                    team.status = TeamStatus::Interrupted;
                    changed.push(team);
                }
            }
        }
        for team in &changed {
            save_team(&tx, team)?;
        }
        tx.commit()?;
        for team in &changed {
            inner.publish(Event::TeamChanged(team.clone()));
        }
        Ok(changed)
    }

    pub fn coordination_get(&self, scope: &Scope, key: &str) -> Result<CoordinationEntry> {
        scope.validate()?;
        field("key", key, 256, false)?;
        read_entry(&self.lock()?.db, scope, key)
    }

    /// Requires the exact current version, including tombstones. Zero creates a
    /// never-seen key. A successful write always increments the version.
    pub fn coordination_set(
        &self,
        scope: &Scope,
        key: &str,
        if_version: i64,
        value: serde_json::Value,
    ) -> Result<CoordinationEntry> {
        validate_value(&value, 0)?;
        self.coordination_write(scope, key, if_version, Some(value))
    }

    /// Deleting an absent key also writes a tombstone and increments its version.
    pub fn coordination_delete(
        &self,
        scope: &Scope,
        key: &str,
        if_version: i64,
    ) -> Result<CoordinationEntry> {
        self.coordination_write(scope, key, if_version, None)
    }

    fn coordination_write(
        &self,
        scope: &Scope,
        key: &str,
        if_version: i64,
        value: Option<serde_json::Value>,
    ) -> Result<CoordinationEntry> {
        scope.validate()?;
        field("key", key, 256, false)?;
        ensure!(if_version >= 0, "if_version must be nonnegative");
        let encoded = value.as_ref().map(serde_json::to_string).transpose()?;
        ensure!(
            encoded.as_ref().is_none_or(|s| s.len() <= MAX_VALUE_BYTES),
            "value exceeds {MAX_VALUE_BYTES} JSON bytes"
        );
        let mut inner = self.lock()?;
        let tx = inner
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = read_entry(&tx, scope, key)?;
        ensure!(
            previous.version == if_version,
            "version conflict: expected {if_version}, current {}",
            previous.version
        );
        let version = previous
            .version
            .checked_add(1)
            .context("coordination version exhausted")?;
        let entry = CoordinationEntry {
            scope: scope.clone(),
            key: key.to_owned(),
            version,
            value,
        };
        tx.execute("INSERT INTO coordination(workspace,worktree,key,version,value) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(workspace,worktree,key) DO UPDATE SET version=excluded.version,value=excluded.value",
            params![scope.workspace, scope.worktree, key, version, encoded])?;
        tx.commit()?;
        inner.publish(Event::CoordinationChanged(entry.clone()));
        Ok(entry)
    }

    /// Captures the snapshot and subscribes under one mutex. On broadcast lag,
    /// discard the old receiver and call this again to obtain a fresh snapshot.
    pub fn subscribe(&self, scope: &Scope) -> Result<Subscription> {
        scope.validate()?;
        let mut inner = self.lock()?;
        let tx = inner.db.transaction()?;
        let teams = list_teams(&tx, scope)?;
        let coordination = list_entries(&tx, scope)?;
        tx.commit()?;
        inner
            .subscribers
            .retain(|_, sender| sender.receiver_count() > 0);
        let events = inner
            .subscribers
            .entry(scope.clone())
            .or_insert_with(|| broadcast::channel(256).0)
            .subscribe();
        Ok(Subscription {
            snapshot: Snapshot {
                scope: scope.clone(),
                teams,
                coordination,
            },
            events,
        })
    }
}

impl Inner {
    fn publish(&mut self, event: Event) {
        let scope = match &event {
            Event::TeamChanged(t) => &t.scope,
            Event::CoordinationChanged(e) => &e.scope,
        };
        if let Some(sender) = self.subscribers.get(scope) {
            // Synchronous send after commit, still serialized with subscriptions.
            let _ = sender.send(event);
        }
    }
}

fn decode_team(scope: &Scope, id: &str, data: &str) -> Result<Team> {
    ensure!(
        data.len() <= 8 * 1024 * 1024,
        "stored team exceeds size limit"
    );
    let team: Team = serde_json::from_str(data)?;
    ensure!(
        &team.scope == scope && team.id == id,
        "stored team identity mismatch"
    );
    validate_team(&team)?;
    Ok(team)
}

fn read_team(db: &Connection, scope: &Scope, id: &str) -> Result<Option<Team>> {
    let data: Option<String> = db
        .query_row(
            "SELECT data FROM teams WHERE workspace=?1 AND worktree=?2 AND id=?3",
            params![scope.workspace, scope.worktree, id],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|data| decode_team(scope, id, &data)).transpose()
}

fn list_teams(db: &Connection, scope: &Scope) -> Result<Vec<Team>> {
    let mut stmt =
        db.prepare("SELECT id,data FROM teams WHERE workspace=?1 AND worktree=?2 ORDER BY id")?;
    let rows = stmt.query_map(params![scope.workspace, scope.worktree], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, data) = row?;
        decode_team(scope, &id, &data)
    })
    .collect()
}

fn save_team(db: &Connection, team: &Team) -> Result<()> {
    db.execute(
        "UPDATE teams SET data=?4 WHERE workspace=?1 AND worktree=?2 AND id=?3",
        params![
            team.scope.workspace,
            team.scope.worktree,
            team.id,
            serde_json::to_string(team)?
        ],
    )?;
    Ok(())
}

fn validate_value(value: &serde_json::Value, depth: usize) -> Result<()> {
    ensure!(depth <= 32, "JSON nesting exceeds 32");
    match value {
        serde_json::Value::Array(a) => {
            for v in a {
                validate_value(v, depth + 1)?;
            }
        }
        serde_json::Value::Object(o) => {
            for v in o.values() {
                validate_value(v, depth + 1)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn decode_entry(
    scope: &Scope,
    key: &str,
    version: i64,
    data: Option<String>,
) -> Result<CoordinationEntry> {
    field("key", key, 256, false)?;
    ensure!(version > 0, "invalid stored version");
    let value = match data {
        Some(data) => {
            ensure!(
                data.len() <= MAX_VALUE_BYTES,
                "stored value exceeds size limit"
            );
            let value = serde_json::from_str(&data)?;
            validate_value(&value, 0)?;
            Some(value)
        }
        None => None,
    };
    Ok(CoordinationEntry {
        scope: scope.clone(),
        key: key.to_owned(),
        version,
        value,
    })
}

fn read_entry(db: &Connection, scope: &Scope, key: &str) -> Result<CoordinationEntry> {
    let row: Option<(i64, Option<String>)> = db
        .query_row(
            "SELECT version,value FROM coordination WHERE workspace=?1 AND worktree=?2 AND key=?3",
            params![scope.workspace, scope.worktree, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((version, value)) => decode_entry(scope, key, version, value),
        None => Ok(CoordinationEntry {
            scope: scope.clone(),
            key: key.to_owned(),
            version: 0,
            value: None,
        }),
    }
}

fn list_entries(db: &Connection, scope: &Scope) -> Result<Vec<CoordinationEntry>> {
    let mut stmt = db.prepare("SELECT key,version,value FROM coordination WHERE workspace=?1 AND worktree=?2 ORDER BY key")?;
    let rows = stmt.query_map(params![scope.workspace, scope.worktree], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
    })?;
    rows.map(|row| {
        let (key, version, value) = row?;
        decode_entry(scope, &key, version, value)
    })
    .collect()
}

#[cfg(test)]
mod tests;

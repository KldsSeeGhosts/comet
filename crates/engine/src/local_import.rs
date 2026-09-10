//! One-time local→synced profile import.
//!
//! A device that worked locally (PR #30's `profiles/local`) and then signs in
//! gets its chats, spaces, journals, and command-ledger claims carried into
//! the synced profile — through the same write paths every live mutation
//! uses, so nothing here invents a second persistence or sync mechanism:
//!
//!  - chat docs land via `save_snapshot_with_cursor(chat_id, bytes, 0, CHAT2_DOC_EPOCH)`,
//!    which is exactly the "born chat2" shape: cursor 0 makes the first room
//!    join push the doc's full update log from VV zero (doc_host first-contact
//!    push), and epoch 2 keeps `DocHost::open` off the s2 discard-and-adopt
//!    branch that would drop the transcript.
//!  - registry rows go through `WorkspaceHost::import_chat_row` /
//!    `import_space_row` (live upserts: persisted, pushed, watched).
//!  - attachments are NOT copied or rewritten. Transcripts embed absolute
//!    paths under the local profile's uploads root, so that root becomes a
//!    read-only jail root of the synced profile — the exact mechanism the
//!    legacy-uploads adoption already uses (`EngineProfile::claim_legacy_uploads_root`).
//!    The marker file re-arms the root on every later synced boot.
//!
//! Idempotence is structural: a chat/space whose row already exists in the
//! target is skipped, so re-running after a partial import (or after new
//! local work from a later signed-out stretch) imports only what's missing.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeron_doc::{
    MessagePart, MessageRole, REGISTRY_DOC_ID, RegistryDoc, SessionDoc, SessionMessageEntry,
};
use zeron_proto::{Chat, ChatConfig, HarnessId, SandboxLevel};
use zeron_sync::DocsStore;

use crate::EngineError;
use crate::chat2_host::CHAT2_DOC_EPOCH;
use crate::repos::home_dir;
use crate::run_journal::journal_paths;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;

/// Marker recording completed imports, at `{data_dir}/local-import.json`.
/// One entry per target (org, user): the same device may sign into several
/// accounts, and each gets its own one-time import.
const MARKER_FILE: &str = "local-import.json";

/// Serializes every marker read-modify-write in this process. Two runtimes in
/// one process (a swap mid-flight, tests) must not lose an account's entry to
/// a racing update; cross-process exclusion is the data-dir `InstanceLock`'s job.
fn marker_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Marker {
    #[serde(default)]
    imports: Vec<MarkerEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarkerEntry {
    org_id: String,
    user_id: String,
    imported_at_ms: i64,
    imported_chats: usize,
    imported_spaces: usize,
}

/// What the wizard needs to offer (or silently skip) the import step.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalImportStatus {
    /// Chats in the local profile with no row in this synced workspace yet.
    pub available_chats: usize,
    /// Spaces in the local profile with no row in this synced workspace yet.
    pub available_spaces: usize,
    /// A completed import for this (org, user) is already on record.
    pub imported_before: bool,
}

/// Per-item progress for the wizard's progress step.
/// NB: `rename_all` renames the *variants* (the `kind` tag); struct-variant
/// *fields* need `rename_all_fields` — without it the wire shape silently
/// ships snake_case counters (caught by `import_events_serialize_camel_case`).
#[derive(Debug, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum ImportEvent {
    /// Emitted once, before any copying.
    Start { chats: usize, spaces: usize },
    /// One imported chat (docs + journal + row).
    Chat {
        index: usize,
        total: usize,
        chat_id: String,
        title: Option<String>,
    },
    /// Terminal summary — also the RPC's final stream item.
    Summary {
        imported_chats: usize,
        imported_spaces: usize,
        skipped_chats: usize,
        skipped_spaces: usize,
        journals_copied: usize,
        ledger_rows_merged: usize,
        errors: Vec<String>,
    },
}

/// The import service a synced runtime carries (`EngineCore::assemble` builds
/// one when the profile is account-scoped and a local profile exists on disk).
#[derive(Clone)]
pub struct LocalImporter {
    inner: Arc<ImporterInner>,
}

struct ImporterInner {
    data_dir: PathBuf,
    device_id: String,
    org_id: String,
    user_id: String,
    target_store: Arc<DocsStore>,
    target_journals: PathBuf,
    workspace: WorkspaceHost,
    uploads: Uploads,
}

impl LocalImporter {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        data_dir: &Path,
        device_id: &str,
        org_id: &str,
        user_id: &str,
        target_store: Arc<DocsStore>,
        target_journals: PathBuf,
        workspace: WorkspaceHost,
        uploads: Uploads,
    ) -> Self {
        Self {
            inner: Arc::new(ImporterInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                org_id: org_id.to_string(),
                user_id: user_id.to_string(),
                target_store,
                target_journals,
                workspace,
                uploads,
            }),
        }
    }

    fn source_root(&self) -> PathBuf {
        self.inner.data_dir.join("profiles").join("local")
    }

    fn source_uploads(&self) -> PathBuf {
        self.source_root().join("uploads")
    }

    fn marker_path(&self) -> PathBuf {
        self.inner.data_dir.join(MARKER_FILE)
    }

    /// Load the marker under [`marker_lock`]. A file that exists but does not
    /// parse is moved aside to `local-import.json.corrupt` — its grants were
    /// already unreadable (boot's [`marker_grants_read_root`] parses the same
    /// file), and keeping the bytes as evidence beats silently clobbering them
    /// on the next write.
    fn load_marker_locked(&self) -> Marker {
        let path = self.marker_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => return Marker::default(),
        };
        match serde_json::from_str(&raw) {
            Ok(marker) => marker,
            Err(err) => {
                let aside = path.with_extension("json.corrupt");
                tracing::warn!(error = %err, aside = %aside.display(),
                    "local-import marker is corrupt; moving it aside");
                if let Err(err) = std::fs::rename(&path, &aside) {
                    tracing::warn!(error = %err, "corrupt marker aside failed");
                }
                Marker::default()
            }
        }
    }

    fn imported_before(&self) -> bool {
        let _guard = marker_lock();
        self.load_marker_locked()
            .imports
            .iter()
            .any(|e| e.org_id == self.inner.org_id && e.user_id == self.inner.user_id)
    }

    /// Record a completed import and re-arm the read-only uploads root for
    /// future boots (`EngineCore::assemble` consults [`marker_grants_read_root`]).
    ///
    /// The read-modify-write is serialized process-wide (cross-process is
    /// already excluded by the data-dir `InstanceLock`) and published
    /// atomically — write + fsync a sibling temp file, then rename over the
    /// marker — so a crash or short write can never truncate the file and
    /// erase other accounts' grants. Failures propagate to the caller and end
    /// up in the import summary.
    fn record_import(&self, chats: usize, spaces: usize) -> Result<(), EngineError> {
        let _guard = marker_lock();
        let mut marker = self.load_marker_locked();
        marker
            .imports
            .retain(|e| !(e.org_id == self.inner.org_id && e.user_id == self.inner.user_id));
        marker.imports.push(MarkerEntry {
            org_id: self.inner.org_id.clone(),
            user_id: self.inner.user_id.clone(),
            imported_at_ms: crate::now_ms(),
            imported_chats: chats,
            imported_spaces: spaces,
        });
        let bytes = serde_json::to_vec_pretty(&marker)
            .map_err(|err| EngineError::Other(format!("marker serialize: {err}")))?;
        let path = self.marker_path();
        let tmp = path.with_extension("json.tmp");
        let publish = || -> std::io::Result<()> {
            {
                use std::io::Write;
                let mut file = std::fs::File::create(&tmp)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            std::fs::rename(&tmp, &path)
        };
        publish().map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            EngineError::Other(format!("marker write: {err}"))
        })
    }

    /// Open the local profile's stores read-only-ish. `None` when the device
    /// never ran a local profile (nothing to import).
    fn open_source(&self) -> Result<Option<(DocsStore, RegistryDoc)>, EngineError> {
        let root = self.source_root();
        if !root.join("docs.sqlite3").is_file() {
            return Ok(None);
        }
        let store = DocsStore::open(&root)?;
        let Some(bytes) = store.load_snapshot(REGISTRY_DOC_ID)? else {
            return Ok(None); // store exists but no registry replica — nothing rowed
        };
        let registry = RegistryDoc::from_bytes(&bytes, &self.inner.device_id)?;
        Ok(Some((store, registry)))
    }

    /// What's importable right now (target-dedup applied).
    pub fn status(&self) -> Result<LocalImportStatus, EngineError> {
        let imported_before = self.imported_before();
        let Some((_, registry)) = self.open_source()? else {
            return Ok(LocalImportStatus {
                available_chats: 0,
                available_spaces: 0,
                imported_before,
            });
        };
        let mut available_chats = 0;
        for chat in registry.read_chats()? {
            if self.inner.workspace.chat(&chat.id)?.is_none() {
                available_chats += 1;
            }
        }
        let mut available_spaces = 0;
        for space in registry.read_spaces()? {
            if self.inner.workspace.space(&space.id)?.is_none() {
                available_spaces += 1;
            }
        }
        Ok(LocalImportStatus {
            available_chats,
            available_spaces,
            imported_before,
        })
    }

    /// Run the import, emitting [`ImportEvent`]s (the last is always
    /// `Summary`). Blocking (sqlite + fs) — callers run it off the async path.
    pub fn run(&self, mut emit: impl FnMut(ImportEvent)) -> Result<(), EngineError> {
        let Some((source_store, registry)) = self.open_source()? else {
            emit(ImportEvent::Start {
                chats: 0,
                spaces: 0,
            });
            emit(ImportEvent::Summary {
                imported_chats: 0,
                imported_spaces: 0,
                skipped_chats: 0,
                skipped_spaces: 0,
                journals_copied: 0,
                ledger_rows_merged: 0,
                errors: Vec::new(),
            });
            return Ok(());
        };

        let mut errors: Vec<String> = Vec::new();

        // Spaces first: chats reference `space_id`, and viewers resolve the
        // reference as soon as the chat row lands.
        let spaces = registry.read_spaces()?;
        let chats = registry.read_chats()?;
        let (total_chats, total_spaces) = (chats.len(), spaces.len());
        let pending_chats: Vec<_> = chats
            .into_iter()
            .filter(|chat| !matches!(self.inner.workspace.chat(&chat.id), Ok(Some(_))))
            .collect();
        let pending_spaces: Vec<_> = spaces
            .into_iter()
            .filter(|space| !matches!(self.inner.workspace.space(&space.id), Ok(Some(_))))
            .collect();
        let skipped_chats = total_chats - pending_chats.len();
        let skipped_spaces = total_spaces - pending_spaces.len();

        emit(ImportEvent::Start {
            chats: pending_chats.len(),
            spaces: pending_spaces.len(),
        });

        let mut imported_spaces = 0;
        for space in &pending_spaces {
            match self.inner.workspace.import_space_row(space) {
                Ok(()) => imported_spaces += 1,
                Err(err) => errors.push(format!("space {}: {err}", space.id)),
            }
        }

        let source_journals = self.source_root().join("journals");
        let total = pending_chats.len();
        let mut imported_chats = 0;
        let mut journals_copied = 0;
        for (index, chat) in pending_chats.iter().enumerate() {
            emit(ImportEvent::Chat {
                index,
                total,
                chat_id: chat.id.clone(),
                title: chat.title.clone(),
            });
            match self.import_chat(&source_store, chat, &source_journals) {
                Ok(journal) => {
                    imported_chats += 1;
                    if journal {
                        journals_copied += 1;
                    }
                }
                Err(err) => errors.push(format!("chat {}: {err}", chat.id)),
            }
        }

        // Merge the source's command ledger so imported pending commands can
        // never re-execute under this profile (mark-before-execute carries over).
        let ledger_rows_merged = source_store
            .processed_commands()
            .and_then(|rows| self.inner.target_store.import_processed_commands(&rows))
            .unwrap_or_else(|err| {
                errors.push(format!("command ledger: {err}"));
                0
            });

        // Transcripts embed absolute paths under the local uploads root; jail
        // it read-only now (and on future boots, via the marker). A marker
        // that fails to persist means imported attachments stop resolving
        // after a restart — that is a real failure, not a footnote.
        if self.source_uploads().is_dir() {
            self.inner
                .uploads
                .add_read_only_root(&self.source_uploads());
        }
        if let Err(err) = self.record_import(imported_chats, imported_spaces) {
            errors.push(format!("import marker: {err}"));
        }

        emit(ImportEvent::Summary {
            imported_chats,
            imported_spaces,
            skipped_chats,
            skipped_spaces,
            journals_copied,
            ledger_rows_merged,
            errors,
        });
        Ok(())
    }

    /// Copy one chat: doc snapshot (born-chat2 shape), journal files, row.
    /// Returns whether a journal file was copied.
    fn import_chat(
        &self,
        source_store: &DocsStore,
        chat: &zeron_proto::Chat,
        source_journals: &Path,
    ) -> Result<bool, EngineError> {
        // Doc bytes may be absent (a chat row created but never opened) — the
        // row alone is still worth carrying; a doc materializes on first open.
        if let Some(bytes) = source_store.load_snapshot(&chat.id)?
            && !self.inner.target_store.has_snapshot(&chat.id)?
        {
            self.inner.target_store.save_snapshot_with_cursor(
                &chat.id,
                &bytes,
                0,
                CHAT2_DOC_EPOCH,
            )?;
        }

        let mut copied = false;
        let (src_journal, src_resume) = journal_paths(source_journals, &chat.id);
        let (dst_journal, dst_resume) = journal_paths(&self.inner.target_journals, &chat.id);
        if src_journal.is_file() && !dst_journal.exists() {
            std::fs::create_dir_all(&self.inner.target_journals)?;
            std::fs::copy(&src_journal, &dst_journal)?;
            copied = true;
        }
        if src_resume.is_file() && !dst_resume.exists() {
            std::fs::create_dir_all(&self.inner.target_journals)?;
            std::fs::copy(&src_resume, &dst_resume)?;
        }

        // Row last: once it appears in watchers the chat is clickable, and by
        // then its doc + journal are already in place. Chat2 lineage is
        // explicit so `DocHost::open` never routes the import down the legacy
        // s2 adopt branch.
        let mut row = chat.clone();
        row.room_gen = Some(CHAT2_DOC_EPOCH);
        self.inner.workspace.import_chat_row(&row)?;
        Ok(copied)
    }

    /// Discover and import Pi sessions into the target workspace.
    pub fn import_pi_sessions(
        &self,
        custom_roots: Option<&[PathBuf]>,
    ) -> Result<PiImportSummary, EngineError> {
        import_pi_sessions(
            &self.inner.target_store,
            &self.inner.workspace,
            &self.inner.device_id,
            custom_roots,
        )
    }

    /// Import a single Pi session file into the target workspace.
    pub fn import_pi_session_file(&self, path: &Path) -> Result<Option<String>, EngineError> {
        import_pi_session(
            &self.inner.target_store,
            &self.inner.workspace,
            &self.inner.device_id,
            path,
        )
    }
}

// ── Pi session import ────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiImportSummary {
    pub imported_chats: usize,
    pub skipped_chats: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NativePiEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub kind: String,
    pub role: Option<String>,
    pub text: Option<String>,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ParsedPiSession {
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub model: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub entries: Vec<NativePiEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiImportedMessage {
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    pub timestamp_ms: i64,
}

fn expand_home(path: PathBuf) -> PathBuf {
    let home = home_dir();
    if path == Path::new("~") {
        return home;
    }
    path.strip_prefix("~")
        .ok()
        .map(|suffix| home.join(suffix))
        .unwrap_or(path)
}

fn file_modified_ms(path: &Path) -> i64 {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(crate::now_ms)
}

fn parse_timestamp_ms(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        if n > 100_000_000_000 {
            return Some(n);
        } else if n > 0 {
            return Some(n * 1000);
        }
    }
    if let Some(u) = value.as_u64() {
        if u > 100_000_000_000 {
            return i64::try_from(u).ok();
        } else if u > 0 {
            return i64::try_from(u * 1000).ok();
        }
    }
    if let Some(s) = value.as_str()
        && let Ok(dt) = DateTime::parse_from_rfc3339(s)
    {
        return Some(dt.timestamp_millis());
    }
    None
}

fn text_content(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn title_from_prompt(prompt: &str) -> String {
    let mut title = prompt
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    if title.chars().count() > 58 {
        title = format!("{}…", title.chars().take(57).collect::<String>());
    }
    title
}

/// Roots to scan for Pi JSONL sessions.
/// Checks `PI_CODING_AGENT_SESSION_DIR`, or `PI_CODING_AGENT_DIR/sessions`,
/// `~/.pi/agent/settings.json`'s `sessionDir`, and standard `~/.pi/agent/sessions`.
pub fn pi_session_roots() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR").filter(|v| !v.is_empty()) {
        return vec![expand_home(PathBuf::from(path))];
    }
    let home = home_dir();
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .map(expand_home)
        .unwrap_or_else(|| home.join(".pi").join("agent"));
    let mut roots = Vec::new();
    let settings = agent_dir.join("settings.json");
    if let Ok(bytes) = std::fs::read(&settings)
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(s) = value.get("sessionDir").and_then(Value::as_str)
    {
        let p = expand_home(PathBuf::from(s));
        if p.is_absolute() {
            roots.push(p);
        }
    }
    let standard = agent_dir.join("sessions");
    if !roots.contains(&standard) {
        roots.push(standard);
    }
    roots
}

/// Recursively find `*.jsonl` session files directly under `roots` or immediate subdirectories.
pub fn scan_pi_session_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                if seen.insert(path.clone()) {
                    files.push(path);
                }
            } else if path.is_dir()
                && let Ok(children) = std::fs::read_dir(&path)
            {
                for child in children.flatten() {
                    let child_path = child.path();
                    if child_path.is_file()
                        && child_path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                        && seen.insert(child_path.clone())
                    {
                        files.push(child_path);
                    }
                }
            }
        }
    }
    files
}

/// Scan for session files across default or custom roots.
pub fn scan_pi_sessions(custom_roots: Option<&[PathBuf]>) -> Vec<PathBuf> {
    let roots = match custom_roots {
        Some(r) => r.to_vec(),
        None => pi_session_roots(),
    };
    scan_pi_session_files(&roots)
}

/// Parse a Pi session `.jsonl` file, tolerating malformed/truncated tail lines.
pub fn parse_pi_session(path: &Path) -> Result<Option<ParsedPiSession>, EngineError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) => {
            return Err(EngineError::Other(format!(
                "open {}: {err}",
                path.display()
            )));
        }
    };
    let mut session_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut model = None;
    let mut created_at_ms = 0;
    let mut updated_at_ms = 0;
    let mut entries = Vec::new();

    let reader = BufReader::new(file);
    for line_res in reader.lines() {
        let Ok(line) = line_res else {
            // Tolerate malformed tail lines
            break;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // Tolerate malformed tail lines
            continue;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "title" {
            if let Some(t) = value
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                title = Some(t.to_owned());
            }
            if let Some(ts) = value.get("updatedAt").and_then(parse_timestamp_ms) {
                updated_at_ms = updated_at_ms.max(ts);
            }
            continue;
        }
        if kind == "session" {
            session_id = value
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            if let Some(ts) = value.get("timestamp").and_then(parse_timestamp_ms) {
                created_at_ms = ts;
                updated_at_ms = updated_at_ms.max(ts);
            }
            if title.is_none() {
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
            continue;
        }
        if matches!(kind, "session_info" | "title_change") {
            let key = if kind == "session_info" {
                "name"
            } else {
                "title"
            };
            if let Some(t) = value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                title = Some(t.to_owned());
            }
        }
        if kind == "model_change"
            && let Some(m) = value
                .get("modelId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            model = Some(m.to_owned());
        }

        let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };

        let entry_ts = value.get("timestamp").and_then(parse_timestamp_ms);
        let msg_ts = value
            .pointer("/message/timestamp")
            .and_then(parse_timestamp_ms);
        let timestamp_ms = msg_ts.or(entry_ts).unwrap_or(updated_at_ms);
        updated_at_ms = updated_at_ms.max(timestamp_ms);

        let parent_id = value
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let role = value
            .pointer("/message/role")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let text = value.pointer("/message/content").and_then(text_content);

        entries.push(NativePiEntry {
            id: id.to_owned(),
            parent_id,
            kind: kind.to_owned(),
            role,
            text,
            timestamp_ms,
        });
    }

    let Some(session_id) = session_id else {
        return Ok(None);
    };
    let cwd = cwd.unwrap_or_else(home_dir);
    let file_mod = file_modified_ms(path);
    let updated_at_ms = if updated_at_ms == 0 {
        file_mod
    } else {
        updated_at_ms
    };
    let created_at_ms = if created_at_ms == 0 {
        updated_at_ms
    } else {
        created_at_ms
    };

    Ok(Some(ParsedPiSession {
        path: path.to_path_buf(),
        session_id,
        cwd,
        title,
        model,
        created_at_ms,
        updated_at_ms: updated_at_ms.max(created_at_ms),
        entries,
    }))
}

/// Trace active branch from the leaf back to the root using `parentId`.
pub fn pi_active_chain(entries: &[NativePiEntry]) -> Vec<&NativePiEntry> {
    let by_id: HashMap<&str, &NativePiEntry> = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let Some(mut current) = entries.last() else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    while visited.insert(&current.id) {
        chain.push(current);
        let Some(parent) = current.parent_id.as_deref() else {
            break;
        };
        let Some(next) = by_id.get(parent).copied() else {
            break;
        };
        current = next;
    }
    chain.reverse();
    chain
}

/// Extract ordered user and assistant messages with text from the active chain,
/// skipping thinking and tool-only content.
pub fn extract_pi_messages(entries: &[NativePiEntry]) -> Vec<PiImportedMessage> {
    let chain = pi_active_chain(entries);
    let mut messages = Vec::new();
    for entry in chain {
        if entry.kind != "message" {
            continue;
        }
        let role = match entry.role.as_deref() {
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => continue,
        };
        let Some(text) = entry.text.as_deref() else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        messages.push(PiImportedMessage {
            id: entry.id.clone(),
            role,
            text: trimmed.to_string(),
            timestamp_ms: entry.timestamp_ms,
        });
    }
    messages
}

/// Discover and list parsed Pi sessions ordered newest-first deterministically.
pub fn list_pi_sessions(custom_roots: Option<&[PathBuf]>) -> Vec<ParsedPiSession> {
    let files = scan_pi_sessions(custom_roots);
    let mut parsed_list = Vec::new();
    for file in files {
        if let Ok(Some(parsed)) = parse_pi_session(&file) {
            parsed_list.push(parsed);
        }
    }
    parsed_list.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| a.path.cmp(&b.path))
    });
    parsed_list
}

/// Import a parsed Pi session into DocsStore and WorkspaceHost.
/// Sets `chat.harness_session_id` to the session file path (for native resume),
/// `chat.config.harness` to `HarnessId::Pi`, and populates `SessionDoc` with
/// ordered user and assistant messages.
pub fn import_parsed_pi_session(
    target_store: &DocsStore,
    workspace: &WorkspaceHost,
    device_id: &str,
    session: ParsedPiSession,
) -> Result<Option<String>, EngineError> {
    let messages = extract_pi_messages(&session.entries);
    if messages.is_empty() {
        return Ok(None);
    }

    let chat_id = format!("pi-{}", session.session_id);
    if workspace.chat(&chat_id)?.is_some() {
        return Ok(None);
    }

    let resume_path = session
        .path
        .canonicalize()
        .unwrap_or_else(|_| session.path.clone());
    let resume_id = resume_path.to_string_lossy().to_string();

    if let Ok(existing) = workspace.read_chats()
        && existing.iter().any(|c| {
            c.harness_session_id.as_deref() == Some(&resume_id)
                || c.harness_session_id.as_deref() == Some(&session.path.to_string_lossy())
        })
    {
        return Ok(None);
    }

    let title = session.title.clone().or_else(|| {
        messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| title_from_prompt(&m.text))
            .filter(|t| !t.is_empty())
    });

    let doc = SessionDoc::init(&chat_id)
        .map_err(|e| EngineError::Other(format!("SessionDoc::init: {e}")))?;
    for msg in &messages {
        doc.push_message(&SessionMessageEntry {
            id: msg.id.clone(),
            role: msg.role,
            parts: vec![MessagePart::Text {
                id: format!("{}-t", msg.id),
                text: msg.text.clone(),
            }],
            created_at: msg.timestamp_ms,
            device_id: device_id.to_string(),
            status: None,
            continuation_of: None,
        })
        .map_err(|e| EngineError::Other(format!("push_message: {e}")))?;
    }
    let bytes = doc
        .export_snapshot()
        .map_err(|e| EngineError::Other(format!("export_snapshot: {e}")))?;
    target_store.save_snapshot_with_cursor(&chat_id, &bytes, 0, CHAT2_DOC_EPOCH)?;

    let last_message = messages.last();
    let last_message_preview = last_message.map(|m| m.text.chars().take(120).collect::<String>());
    let last_message_at =
        last_message.and_then(|m| DateTime::<Utc>::from_timestamp_millis(m.timestamp_ms));
    let created_at =
        DateTime::<Utc>::from_timestamp_millis(session.created_at_ms).unwrap_or_else(Utc::now);

    let chat = Chat {
        id: chat_id.clone(),
        device_id: device_id.to_string(),
        title,
        archived: false,
        cwd: Some(session.cwd.to_string_lossy().to_string()),
        branch: None,
        checkout_id: None,
        source_context: None,
        config: Some(ChatConfig {
            harness: HarnessId::Pi,
            model: session.model.clone(),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        }),
        last_message_preview,
        last_message_at,
        created_at,
        harness_session_id: Some(resume_id),
        harness_session_cwd: Some(session.cwd.to_string_lossy().to_string()),
        space_id: None,
        last_seen_at: None,
        room_gen: Some(CHAT2_DOC_EPOCH),
    };
    workspace.import_chat_row(&chat)?;

    Ok(Some(chat_id))
}

/// Import a single Pi session file into the store and workspace.
pub fn import_pi_session(
    target_store: &DocsStore,
    workspace: &WorkspaceHost,
    device_id: &str,
    path: &Path,
) -> Result<Option<String>, EngineError> {
    let Some(parsed) = parse_pi_session(path)? else {
        return Ok(None);
    };
    import_parsed_pi_session(target_store, workspace, device_id, parsed)
}

/// Discover and import Pi sessions from roots in newest-first deterministic order.
pub fn import_pi_sessions(
    target_store: &DocsStore,
    workspace: &WorkspaceHost,
    device_id: &str,
    custom_roots: Option<&[PathBuf]>,
) -> Result<PiImportSummary, EngineError> {
    let parsed_list = list_pi_sessions(custom_roots);
    let mut imported_chats = 0;
    let mut skipped_chats = 0;
    let mut errors = Vec::new();

    for parsed in parsed_list {
        match import_parsed_pi_session(target_store, workspace, device_id, parsed) {
            Ok(Some(_)) => imported_chats += 1,
            Ok(None) => skipped_chats += 1,
            Err(e) => errors.push(e.to_string()),
        }
    }

    Ok(PiImportSummary {
        imported_chats,
        skipped_chats,
        errors,
    })
}

/// Whether a recorded import grants the synced profile `(org, user)` the local
/// profile's uploads root as a read-only jail root. `EngineCore::assemble`
/// calls this on every account-scoped boot.
pub fn marker_grants_read_root(data_dir: &Path, org_id: &str, user_id: &str) -> Option<PathBuf> {
    let marker: Marker = std::fs::read_to_string(data_dir.join(MARKER_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())?;
    let hit = marker
        .imports
        .iter()
        .any(|e| e.org_id == org_id && e.user_id == user_id);
    let uploads = data_dir.join("profiles").join("local").join("uploads");
    (hit && uploads.is_dir()).then_some(uploads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_events_serialize_camel_case() {
        let summary = serde_json::to_value(ImportEvent::Summary {
            imported_chats: 2,
            imported_spaces: 1,
            skipped_chats: 0,
            skipped_spaces: 0,
            journals_copied: 2,
            ledger_rows_merged: 3,
            errors: Vec::new(),
        })
        .expect("serialize");
        assert_eq!(summary["kind"], "summary");
        assert_eq!(
            summary["importedChats"], 2,
            "fields must be camelCase: {summary}"
        );
        let chat = serde_json::to_value(ImportEvent::Chat {
            index: 0,
            total: 2,
            chat_id: "c1".into(),
            title: None,
        })
        .expect("serialize");
        assert_eq!(chat["chatId"], "c1", "fields must be camelCase: {chat}");
    }

    #[test]
    fn pi_session_active_chain_and_content_filtering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");

        let lines = [
            r#"{"type":"session","version":3,"id":"sess-branch","timestamp":"2026-09-01T00:00:00Z","cwd":"/tmp/test-cwd"}"#,
            r#"{"type":"title","title":"Custom Pi Title","updatedAt":"2026-09-01T00:00:05Z"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-01T00:00:01Z","message":{"role":"user","content":"Initial prompt"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret thought"},{"type":"text","text":"First answer"}]}}"#,
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-09-01T00:00:03Z","message":{"role":"assistant","content":[{"type":"tool_call","name":"bash"}]}}"#,
            r#"{"type":"message","id":"u_abandoned","parentId":"a1","timestamp":"2026-09-01T00:00:04Z","message":{"role":"user","content":"abandoned branch prompt"}}"#,
            r#"{"type":"message","id":"u2","parentId":"t1","timestamp":"2026-09-01T00:00:06Z","message":{"role":"user","content":"Followup prompt"}}"#,
            r#"{"type":"message","id":"a2","parentId":"u2","timestamp":"2026-09-01T00:00:07Z","message":{"role":"assistant","content":"Final answer"}}"#,
            r#"{"type":"message","id":"truncated","#, // Malformed tail line
        ];
        std::fs::write(&path, lines.join("\n")).expect("write session");

        let parsed = parse_pi_session(&path)
            .expect("parse session")
            .expect("some session");
        assert_eq!(parsed.session_id, "sess-branch");
        assert_eq!(parsed.title.as_deref(), Some("Custom Pi Title"));

        let chain = pi_active_chain(&parsed.entries);
        let chain_ids: Vec<&str> = chain.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(chain_ids, vec!["u1", "a1", "t1", "u2", "a2"]);
        assert!(!chain_ids.contains(&"u_abandoned"));

        let messages = extract_pi_messages(&parsed.entries);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].id, "u1");
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].text, "Initial prompt");

        assert_eq!(messages[1].id, "a1");
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert_eq!(messages[1].text, "First answer");

        // t1 is skipped (tool-only, no text)
        assert_eq!(messages[2].id, "u2");
        assert_eq!(messages[2].role, MessageRole::User);
        assert_eq!(messages[2].text, "Followup prompt");

        assert_eq!(messages[3].id, "a2");
        assert_eq!(messages[3].role, MessageRole::Assistant);
        assert_eq!(messages[3].text, "Final answer");
    }

    #[test]
    fn pi_session_scan_and_newest_first_deterministic_ordering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).expect("create sub");

        let f1 = sub.join("sess1.jsonl");
        let f2 = dir.path().join("sess2.jsonl");

        std::fs::write(
            &f1,
            r#"{"type":"session","id":"sess-1","timestamp":"2026-09-01T00:00:00Z","cwd":"/tmp"}
{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-01T00:00:01Z","message":{"role":"user","content":"older"}}"#,
        )
        .expect("write f1");

        std::fs::write(
            &f2,
            r#"{"type":"session","id":"sess-2","timestamp":"2026-09-02T00:00:00Z","cwd":"/tmp"}
{"type":"message","id":"u2","parentId":null,"timestamp":"2026-09-02T00:00:01Z","message":{"role":"user","content":"newer"}}"#,
        )
        .expect("write f2");

        let files = scan_pi_sessions(Some(&[dir.path().to_path_buf()]));
        assert_eq!(files.len(), 2);

        let listed = list_pi_sessions(Some(&[dir.path().to_path_buf()]));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session_id, "sess-2");
        assert_eq!(listed[1].session_id, "sess-1");
    }

    #[tokio::test]
    async fn pi_session_import_idempotence_and_resume_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(DocsStore::open(dir.path()).expect("store"));
        let device_id = "device-test";
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: device_id.into(),
                device_name: "Test device".into(),
                platform: "linux".into(),
                org_id: "test-org".into(),
                user_id: "test-user".into(),
                edge: None,
            },
        )
        .expect("workspace host");

        let session_file = dir.path().join("session.jsonl");
        std::fs::write(
            &session_file,
            r#"{"type":"session","id":"01234567-89ab-cdef","timestamp":"2026-09-01T12:00:00Z","cwd":"/tmp/work"}
{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-01T12:00:01Z","message":{"role":"user","content":"How do I test this?"}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-01T12:00:05Z","message":{"role":"assistant","content":"Write tests."}}"#,
        )
        .expect("write session");

        let imported_id = import_pi_session(&store, &workspace, device_id, &session_file)
            .expect("import")
            .expect("chat_id");
        assert_eq!(imported_id, "pi-01234567-89ab-cdef");

        let chat = workspace
            .chat(&imported_id)
            .expect("read chat")
            .expect("chat exists");
        let canonical = session_file
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(chat.harness_session_id.as_deref(), Some(canonical.as_str()));
        assert_eq!(chat.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(chat.harness_session_cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(chat.title.as_deref(), Some("How do I test this?"));

        let config = chat.config.expect("config");
        assert_eq!(config.harness, HarnessId::Pi);

        // Verify doc snapshot was saved and has messages
        let bytes = store
            .load_snapshot(&imported_id)
            .expect("load snapshot")
            .expect("snapshot bytes");
        let raw = loro::LoroDoc::new();
        raw.import(&bytes).expect("import snapshot bytes");
        let doc = SessionDoc::from_doc(raw);
        let entries = doc.read_entries().expect("read entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].parts[0],
            MessagePart::Text {
                id: "u1-t".into(),
                text: "How do I test this?".into(),
            }
        );
        assert_eq!(
            entries[1].parts[0],
            MessagePart::Text {
                id: "a1-t".into(),
                text: "Write tests.".into(),
            }
        );

        // Re-importing must be idempotent (skip)
        let reimport =
            import_pi_session(&store, &workspace, device_id, &session_file).expect("reimport");
        assert_eq!(reimport, None);
    }

    /// The RPC surface (`ImportPiSessions`) calls [`LocalImporter::import_pi_sessions`];
    /// the first run imports the scanned sessions, and a second run must be a
    /// no-op that reports zero imported.
    #[tokio::test]
    async fn local_importer_import_pi_sessions_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(DocsStore::open(dir.path()).expect("store"));
        let device_id = "device-test";
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::workspace_host::WorkspaceHostConfig {
                device_id: device_id.into(),
                device_name: "Test device".into(),
                platform: "linux".into(),
                org_id: "test-org".into(),
                user_id: "test-user".into(),
                edge: None,
            },
        )
        .expect("workspace host");
        let importer = LocalImporter::new(
            dir.path(),
            device_id,
            "test-org",
            "test-user",
            store.clone(),
            dir.path().join("journals"),
            workspace.clone(),
            Uploads::new(dir.path()),
        );

        let root = dir.path().join("pi-sessions");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(
            root.join("session.jsonl"),
            r#"{"type":"session","id":"aaaabbbb-cccc-dddd","timestamp":"2026-09-01T12:00:00Z","cwd":"/tmp/work"}
{"type":"message","id":"u1","parentId":null,"timestamp":"2026-09-01T12:00:01Z","message":{"role":"user","content":"How do I test this?"}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-01T12:00:05Z","message":{"role":"assistant","content":"Write tests."}}"#,
        )
        .expect("write session");

        let summary = importer
            .import_pi_sessions(Some(std::slice::from_ref(&root)))
            .expect("import");
        assert_eq!(summary.imported_chats, 1);
        assert_eq!(summary.skipped_chats, 0);
        assert!(summary.errors.is_empty());
        assert!(
            workspace
                .chat("pi-aaaabbbb-cccc-dddd")
                .expect("chat")
                .is_some()
        );

        // Second invocation: nothing new to do — a no-op with zero imported.
        let rerun = importer.import_pi_sessions(Some(&[root])).expect("rerun");
        assert_eq!(rerun.imported_chats, 0);
        assert_eq!(rerun.skipped_chats, 1);
        assert!(rerun.errors.is_empty());
    }
}

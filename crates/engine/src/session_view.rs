//! Engine-owned Chat/CLI handoff. A failed hydration keeps the chat gated until retry.

mod hooks;
use hooks::{HookRuntime, NativeActivity};

use crate::terminals::{ProcessIdentity, boot_id};
use crate::{DocHost, EngineError, SessionsEngine, Terminals, WorkspaceHost};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_harness::provider_history::{
    NativeHistory, NativeMessage, ResumeCommand, prepare_resume,
};
use zeron_proto::{Chat, ChatConfig, TerminalSession, ToolCall};
use zeron_sync::DocsStore;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn failed(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub chat_id: String,
    pub owner: SessionOwner,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_activity: Option<NativeActivity>,
    pub terminal: Option<TerminalSession>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionOwner {
    Chat,
    Opening,
    Cli,
    Hydrating,
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HydrationOutcome {
    pub chat_id: String,
    pub imported_messages: usize,
    pub native_session_id: Option<String>,
    pub view: SessionView,
}

#[derive(Clone, Serialize, Deserialize)]
struct Handoff {
    command: ResumeCommand,
    config: ChatConfig,
    baseline: NativeHistory,
    terminal: Option<TerminalSession>,
    owner: SessionOwner,
    error: Option<String>,
    process: ProcessState,
    #[serde(skip)]
    recovered: bool,
}

#[derive(Clone, Serialize, Deserialize)]
enum ProcessState {
    BackendPending {
        boot: Option<String>,
    },
    NotSpawned,
    SpawnPending {
        boot: Option<String>,
    },
    Spawned {
        identity: Option<ProcessIdentity>,
        boot: Option<String>,
    },
    Dead,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    handoffs: HashMap<String, Handoff>,
}

#[derive(Default)]
struct Inner {
    gates: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    handoffs: Mutex<HashMap<String, Handoff>>,
    path: Option<PathBuf>,
    store: Option<Arc<DocsStore>>,
    persistence_error: Mutex<Option<String>>,
    hooks: std::sync::OnceLock<HookRuntime>,
}

#[derive(Clone)]
#[cfg_attr(test, derive(Default))]
pub struct SessionViews {
    inner: Arc<Inner>,
}

impl SessionViews {
    /// Called under the engine instance lock, before any stale-run recovery.
    /// The journal lives beside this profile's docs.sqlite3 store.
    pub(crate) fn load(path: &Path, store: Arc<DocsStore>) -> Result<Self, EngineError> {
        let mut handoffs = match std::fs::read(path) {
            Ok(bytes) => {
                let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| {
                    failed(format!(
                        "Cannot load handoff journal {}: {e}",
                        path.display()
                    ))
                })?;
                if journal.version != 1 {
                    return Err(failed("Unsupported handoff journal version"));
                }
                journal.handoffs
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        for h in handoffs.values_mut() {
            h.recovered = true;
            h.owner = SessionOwner::RecoveryRequired;
            h.error.get_or_insert_with(||
                "Engine restarted; close this CLI session to verify teardown and retry hydration".into()
            );
        }
        let views = Self {
            inner: Arc::new(Inner {
                handoffs: Mutex::new(handoffs),
                path: Some(path.into()),
                store: Some(store),
                ..Inner::default()
            }),
        };
        // Create and sync the empty journal as well. An unwritable profile must
        // fail assembly rather than accept a handoff it cannot recover.
        views.persist(&lock(&views.inner.handoffs))?;
        Ok(views)
    }

    fn persist(&self, handoffs: &HashMap<String, Handoff>) -> Result<(), EngineError> {
        let Some(path) = &self.inner.path else {
            return Ok(());
        };
        let result = (|| {
            let parent = path
                .parent()
                .ok_or_else(|| failed("Handoff journal has no parent"))?;
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            // NamedTempFile is private (0600); no runtime environment is serialized.
            serde_json::to_writer(
                &mut file,
                &Journal {
                    version: 1,
                    handoffs: handoffs.clone(),
                },
            )
            .map_err(|e| failed(e.to_string()))?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(path).map_err(|e| failed(e.to_string()))?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok::<_, EngineError>(())
        })();
        *lock(&self.inner.persistence_error) = result.as_ref().err().map(ToString::to_string);
        result.map_err(|e| {
            failed(format!(
                "Handoff journal save failed; Chat remains gated: {e}"
            ))
        })
    }

    fn put(&self, chat: &str, h: Handoff) -> Result<(), EngineError> {
        let mut handoffs = lock(&self.inner.handoffs);
        // Keep the latest process identity in memory even if the write fails.
        handoffs.insert(chat.into(), h);
        self.persist(&handoffs)
    }

    fn release(&self, chat: &str) -> Result<(), EngineError> {
        let mut handoffs = lock(&self.inner.handoffs);
        let mut next = handoffs.clone();
        next.remove(chat);
        // Never release a permit before removal is durable.
        self.persist(&next)?;
        *handoffs = next;
        Ok(())
    }

    fn gate(&self, chat: &str) -> Arc<AsyncMutex<()>> {
        lock(&self.inner.gates)
            .entry(chat.into())
            .or_default()
            .clone()
    }

    pub(crate) fn ensure_chat_owned(&self, chat: &str) -> Result<(), EngineError> {
        if let Some(error) = &*lock(&self.inner.persistence_error) {
            return Err(failed(format!(
                "Handoff journal is unavailable; retry session recovery: {error}"
            )));
        }
        if lock(&self.inner.handoffs).contains_key(chat) {
            return Err(failed(
                "Chat is owned by its CLI session; close and hydrate it before sending",
            ));
        }
        Ok(())
    }

    pub(crate) fn chat_permit(&self, chat: &str) -> Result<OwnedMutexGuard<()>, EngineError> {
        let guard = self
            .gate(chat)
            .try_lock_owned()
            .map_err(|_| failed("Session ownership is changing; retry after handoff"))?;
        self.ensure_chat_owned(chat)?;
        Ok(guard)
    }

    pub fn get(&self, chat_id: &str) -> SessionView {
        let handoffs = lock(&self.inner.handoffs);
        let h = handoffs.get(chat_id);
        SessionView {
            chat_id: chat_id.into(),
            owner: h.map_or(SessionOwner::Chat, |h| h.owner),
            native_activity: self.inner.hooks.get().and_then(|hooks| hooks.activity(chat_id)),
            terminal: h.filter(|h| !h.recovered).and_then(|h| h.terminal.clone()),
            error: lock(&self.inner.persistence_error)
                .clone()
                .or_else(|| h.and_then(|h| h.error.clone())),
        }
    }

    fn recovery(&self, chat: &str, error: &EngineError) -> Result<(), EngineError> {
        let mut handoffs = lock(&self.inner.handoffs);
        if let Some(h) = handoffs.get_mut(chat) {
            h.owner = SessionOwner::RecoveryRequired;
            h.error = Some(error.to_string());
        }
        self.persist(&handoffs)
    }

    /// The spawned transaction survives RPC cancellation or subscriber detach.
    pub async fn open(
        &self,
        sessions: SessionsEngine,
        workspace: WorkspaceHost,
        terminals: Terminals,
        chat_id: String,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalSession, EngineError> {
        let views = self.clone();
        tokio::spawn(async move {
            views
                .open_inner(&sessions, &workspace, &terminals, &chat_id, cols, rows)
                .await
        })
        .await
        .map_err(|e| failed(format!("Session handoff task failed: {e}")))?
    }

    async fn open_inner(
        &self,
        sessions: &SessionsEngine,
        workspace: &WorkspaceHost,
        terminals: &Terminals,
        chat_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalSession, EngineError> {
        let _gate = self.gate(chat_id).lock_owned().await;
        let chat = local_chat(workspace, chat_id)?;
        if let Some(h) = lock(&self.inner.handoffs).get(chat_id) {
            if let Some(error) = &h.error {
                return Err(failed(error.clone()));
            }
            if let Some(terminal) = &h.terminal {
                terminals.resize(&terminal.id, cols, rows)?;
                return Ok(terminal.clone());
            }
            return Err(failed(
                "Session handoff requires CloseSessionTerminal recovery",
            ));
        }
        self.ensure_chat_owned(chat_id)?;
        if sessions.turn_in_flight(chat_id) {
            return Err(failed(
                "Session is busy; wait for its current turn before opening CLI",
            ));
        }
        let config = chat
            .config
            .clone()
            .ok_or_else(|| failed("Chat has no provider configuration"))?;
        // Refuse unsupported lifecycle adapters before native-file probes,
        // interrupting Chat, or persisting a handoff that cannot return safely.
        hooks::managed_provider(config.harness)?;
        let fresh = chat.harness_session_id.is_none();
        let (native, cwd) = if fresh {
            if config.harness != zeron_proto::HarnessId::Pi
                || chat.harness_session_cwd.is_some()
                || chat.last_message_at.is_some()
                || chat.last_message_preview.is_some()
                || !sessions.chat_is_empty(chat_id)?
            {
                return Err(failed("Only an empty Pi chat can start a new native CLI session"));
            }
            let cwd = chat.cwd.as_deref()
                .map(crate::sessions::expand_home)
                .ok_or_else(|| failed("Choose a working directory before opening CLI"))?;
            (None, cwd)
        } else {
            let native = chat.harness_session_id.as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| failed("Chat has no native session to resume"))?;
            let cwd = chat.harness_session_cwd.as_deref()
                .ok_or_else(|| failed("Native session cwd is missing"))?;
            if chat.cwd.as_deref().map(crate::sessions::expand_home).is_some_and(|c| c != cwd) {
                return Err(failed("Chat cwd changed since its native session was created"));
            }
            (Some(native.to_owned()), cwd.to_owned())
        };
        let config_for_probe = config.clone();
        let new_session_dir = self.inner.path.as_ref().and_then(|path| path.parent())
            .map(|parent| parent.join("native-sessions"));
        let command = tokio::task::spawn_blocking(move || -> Result<ResumeCommand, EngineError> {
            if let Some(native) = native {
                return Ok(prepare_resume(&config_for_probe, &native, &cwd)?);
            }
            let dir = new_session_dir.ok_or_else(|| failed("Native session storage is unavailable"))?;
            let path = create_empty_pi_session(&dir, &cwd)?;
            match prepare_resume(&config_for_probe, &path.to_string_lossy(), &cwd) {
                Ok(command) => Ok(command),
                Err(error) => {
                    let _ = std::fs::remove_file(path);
                    Err(error.into())
                }
            }
        }).await.map_err(|e| failed(e.to_string()))??;
        let baseline = read_history(command.clone()).await?;
        // No state change or interrupt occurs until the native file and argv validate.
        // Recheck after the blocking probe, before interrupting anything.
        if sessions.turn_in_flight(chat_id) {
            return Err(failed("Session is busy; handoff refused"));
        }
        let mut h = Handoff {
            command: command.clone(),
            config,
            baseline,
            terminal: None,
            owner: SessionOwner::Opening,
            error: None,
            process: ProcessState::BackendPending { boot: boot_id() },
            recovered: false,
        };
        self.put(chat_id, h.clone())?;
        if fresh {
            sessions.remember_native_session(chat_id, &command.session_id, &command.cwd);
            workspace.flush();
        }
        if let Err(error) = sessions.interrupt(chat_id).await {
            self.recovery(chat_id, &error)?;
            return Err(error);
        }
        h.process = ProcessState::NotSpawned;
        self.put(chat_id, h.clone())?;
        // Capture final backend writes only after its event stream closes.
        h.baseline = match read_history(command.clone()).await.and_then(|history| {
            check_history_extension(&h.baseline, &history)?;
            Ok(history)
        }) {
            Ok(history) => history,
            Err(error) => {
                self.recovery(chat_id, &error)?;
                return Err(error);
            }
        };
        // A crash between spawn and recording the PID is ambiguous, never an
        // invitation to respawn or release Chat. A host reboot resolves it.
        h.process = ProcessState::SpawnPending { boot: boot_id() };
        self.put(chat_id, h.clone())?;
        let program = {
            if self.inner.hooks.get().is_none() {
                let hooks = HookRuntime::start()?;
                let _ = self.inner.hooks.set(hooks);
            }
            match self.inner.hooks.get().unwrap().prepare(chat_id, &command, &h.baseline.identity) {
                Ok(program) => program,
                Err(error) => {
                    h.process = ProcessState::NotSpawned;
                    self.put(chat_id, h)?;
                    self.recovery(chat_id, &error)?;
                    return Err(error);
                }
            }
        };
        let terminal = match terminals.open_argv(
            &command.cwd,
            cols,
            rows,
            &program,
            &command.args,
            &command.env,
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                if let Some(hooks) = self.inner.hooks.get() { hooks.revoke(chat_id); }
                h.process = ProcessState::NotSpawned;
                self.put(chat_id, h)?;
                self.recovery(chat_id, &error)?;
                return Err(error);
            }
        };
        h.process = ProcessState::Spawned {
            identity: terminals.process_identity(&terminal.id)?,
            boot: boot_id(),
        };
        h.terminal = Some(terminal.clone());
        h.owner = SessionOwner::Cli;
        if let Err(error) = self.put(chat_id, h) {
            // The durable SpawnPending record keeps restart fail-closed even
            // if termination or the subsequent recovery write also fails.
            let _ = terminals.terminate_and_wait(&terminal.id).await;
            return Err(error);
        }
        Ok(terminal)
    }

    pub async fn close(
        &self,
        sessions: SessionsEngine,
        workspace: WorkspaceHost,
        docs: DocHost,
        terminals: Terminals,
        chat_id: String,
    ) -> Result<HydrationOutcome, EngineError> {
        let views = self.clone();
        tokio::spawn(async move {
            views
                .close_inner(&sessions, &workspace, &docs, &terminals, &chat_id)
                .await
        })
        .await
        .map_err(|e| failed(format!("Session hydration task failed: {e}")))?
    }

    async fn close_inner(
        &self,
        sessions: &SessionsEngine,
        workspace: &WorkspaceHost,
        docs: &DocHost,
        terminals: &Terminals,
        chat_id: &str,
    ) -> Result<HydrationOutcome, EngineError> {
        let _gate = self.gate(chat_id).lock_owned().await;
        let chat = local_chat(workspace, chat_id)?;
        let handoff = lock(&self.inner.handoffs).get(chat_id).cloned();
        let Some(mut h) = handoff else {
            return Ok(HydrationOutcome {
                chat_id: chat_id.into(),
                imported_messages: 0,
                native_session_id: chat.harness_session_id,
                view: self.get(chat_id),
            });
        };
        let already_dead = matches!(&h.process, ProcessState::Dead | ProcessState::NotSpawned)
            || matches!(&h.process, ProcessState::Spawned { identity: Some(identity), .. } if identity.ensure_dead().is_ok());
        if matches!(h.owner, SessionOwner::Cli) && !h.recovered && !already_dead {
            if let Some(hooks) = self.inner.hooks.get() {
                hooks.freeze_idle(chat_id)?;
            } else {
                return Err(failed("Native session state is not verified"));
            }
        }
        h.owner = SessionOwner::Hydrating;
        self.put(chat_id, h.clone())?;
        let result = async {
            if let Some(terminal) = &h.terminal
                && terminals.contains(&terminal.id) {
                terminals.terminate_and_wait(&terminal.id).await?;
            } else {
                match &h.process {
                    ProcessState::NotSpawned | ProcessState::Dead => {},
                    ProcessState::BackendPending { .. } if !h.recovered => { sessions.interrupt(chat_id).await?; },
                    ProcessState::BackendPending { boot } | ProcessState::SpawnPending { boot }
                        | ProcessState::Spawned { identity: None, boot } => {
                        if boot.is_none() || boot_id().as_ref() == boot.as_ref() || boot_id().is_none() {
                            return Err(failed("Process teardown cannot be proved after interrupted handoff; keep Chat closed until a host reboot"));
                        }
                    },
                    ProcessState::Spawned { identity: Some(identity), .. } => identity.ensure_dead()?,
                }
            }
            // A failed import can now retry even after another engine restart.
            h.process = ProcessState::Dead;
            self.put(chat_id, h.clone())?;
            let history = read_history(h.command.clone()).await?;
            let entries = hydrate_entries_with_images(&h.baseline, &history, workspace.device_id(), &|block| {
                docs.store_native_image(field(block, "data")?, field(block, "mimeType")?)
            })?;
            let required_entries = entries.clone();
            let handle = docs.open(chat_id)?;
            let mut existing: HashSet<_> = handle.doc().read_entries()?.into_iter().map(|e| e.id).collect();
            let mut imported = 0;
            for entry in entries {
                if existing.insert(entry.id.clone()) {
                    handle.doc().push_message(&entry)?;
                    imported += 1;
                }
            }
            // Restore this native session's provider if configuration changed while in CLI.
            let mut config = h.config.clone();
            if let Some(model) = history.model { config.model = Some(model); }
            if history.reasoning.is_some() { config.reasoning = history.reasoning; }
            if !workspace.set_chat_config(chat_id, &config)? { return Err(failed("Chat disappeared during hydration")); }
            if !workspace.set_chat_cwd(chat_id, &h.command.cwd)? { return Err(failed("Chat disappeared during hydration")); }
            sessions.remember_native_session(chat_id, &h.command.session_id, &h.command.cwd);
            docs.flush_all();
            workspace.flush();
            self.verify_hydration_persisted(chat_id, workspace, &config, &h.command, &required_entries)?;
            Ok::<_, EngineError>(imported)
        }.await;
        match result {
            Ok(imported_messages) => {
                self.release(chat_id)?;
                if let Some(hooks) = self.inner.hooks.get() { hooks.revoke(chat_id); }
                if let Some(terminal) = &h.terminal
                    && terminals.contains(&terminal.id)
                {
                    terminals.forget_terminated(&terminal.id)?;
                }
                Ok(HydrationOutcome {
                    chat_id: chat_id.into(),
                    imported_messages,
                    native_session_id: Some(h.command.session_id),
                    view: self.get(chat_id),
                })
            }
            Err(error) => {
                self.recovery(chat_id, &error)?;
                Err(error)
            }
        }
    }

    // Existing flush APIs log errors. Read back the persisted documents before
    // removing the only durable recovery record, rather than trusting a log.
    fn verify_hydration_persisted(
        &self,
        chat_id: &str,
        workspace: &WorkspaceHost,
        config: &ChatConfig,
        command: &ResumeCommand,
        entries: &[SessionMessageEntry],
    ) -> Result<(), EngineError> {
        let Some(store) = &self.inner.store else {
            return Ok(());
        };
        let bytes = store
            .load_snapshot(chat_id)?
            .ok_or_else(|| failed("Hydrated transcript was not persisted"))?;
        let doc = loro::LoroDoc::new();
        doc.import(&bytes).map_err(|e| failed(e.to_string()))?;
        let saved = zeron_doc::SessionDoc::from_doc(doc).read_entries()?;
        if entries.iter().any(|entry| !saved.contains(entry)) {
            return Err(failed(
                "Hydrated transcript persistence is incomplete; retry close",
            ));
        }
        let bytes = store
            .load_snapshot(zeron_doc::REGISTRY_DOC_ID)?
            .ok_or_else(|| failed("Hydrated registry was not persisted"))?;
        let registry = zeron_doc::RegistryDoc::from_bytes(&bytes, workspace.device_id())?;
        let chat = registry
            .chat(chat_id)?
            .ok_or_else(|| failed("Hydrated chat was not persisted"))?;
        if chat.config.as_ref() != Some(config)
            || chat.cwd.as_deref() != Some(&command.cwd)
            || chat.harness_session_id.as_deref() != Some(&command.session_id)
            || chat.harness_session_cwd.as_deref() != Some(&command.cwd)
        {
            return Err(failed(
                "Hydrated session configuration was not persisted; retry close",
            ));
        }
        // DocsStore uses synchronous=NORMAL. A read-back proves visibility,
        // not power-loss durability. Complete a WAL checkpoint before deleting
        // the fsynced handoff record. This connection never changes doc rows.
        let root = self
            .inner
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .ok_or_else(|| failed("Handoff store path is missing"))?;
        let checkpoint = (|| -> Result<(i64, i64, i64), rusqlite::Error> {
            let conn = rusqlite::Connection::open_with_flags(
                root.join("docs.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            conn.pragma_update(None, "synchronous", "FULL")?;
            conn.query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
        })();
        let (busy, frames, saved) = checkpoint
            .map_err(|e| failed(format!("Hydration checkpoint failed; retry close: {e}")))?;
        if busy != 0 || frames != saved {
            return Err(failed(
                "Hydration checkpoint is busy or incomplete; retry close",
            ));
        }
        Ok(())
    }
}

/// Persist a valid Pi v3 header before its first PTY process starts. The file
/// remains in this engine profile so Chat can resume it after closing CLI.
fn create_empty_pi_session(dir: &Path, cwd: &str) -> Result<PathBuf, EngineError> {
    if !Path::new(cwd).is_absolute() || !Path::new(cwd).is_dir() {
        return Err(failed("Native session working directory is unavailable"));
    }
    std::fs::create_dir_all(dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{id}.jsonl"));
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut staged, &serde_json::json!({
        "type": "session", "version": 3, "id": id,
        "timestamp": chrono::Utc::now().to_rfc3339(), "cwd": cwd,
    })).map_err(|error| failed(error.to_string()))?;
    staged.write_all(b"\n")?;
    staged.as_file().sync_all()?;
    staged.persist_noclobber(&path).map_err(|error| failed(error.to_string()))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(path)
}

fn local_chat(workspace: &WorkspaceHost, id: &str) -> Result<Chat, EngineError> {
    let chat = workspace
        .chat(id)?
        .ok_or_else(|| failed("Chat not found"))?;
    if chat.device_id != workspace.device_id() {
        return Err(failed("Session handoff must run on the chat's host device"));
    }
    Ok(chat)
}

async fn read_history(command: ResumeCommand) -> Result<NativeHistory, EngineError> {
    tokio::task::spawn_blocking(move || command.read_history())
        .await
        .map_err(|e| failed(e.to_string()))?
        .map_err(Into::into)
}

/// Import only the native suffix recorded while CLI owned the chat. Existing Chat
/// IDs differ from native IDs; content matching would lose repeated user prompts.
fn check_history_extension(
    before: &NativeHistory,
    after: &NativeHistory,
) -> Result<(), EngineError> {
    if before.identity != after.identity || before.cwd != after.cwd {
        return Err(failed("Native session identity changed during CLI handoff"));
    }
    if !after.messages.starts_with(&before.messages) {
        return Err(failed(
            "Native history was rewritten or branched; automatic hydration refused",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn hydrate_entries(
    before: &NativeHistory,
    after: &NativeHistory,
    device: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    hydrate_entries_with_images(before, after, device, &|_| Err(failed("Image resolver is unavailable")))
}

fn hydrate_entries_with_images(
    before: &NativeHistory,
    after: &NativeHistory,
    device: &str,
    image: &impl Fn(&Value) -> Result<String, EngineError>,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    check_history_extension(before, after)?;
    let mut entries = Vec::new();
    let mut tools: HashMap<String, (usize, usize, ToolCall)> = HashMap::new();
    for message in &after.messages[before.messages.len()..] {
        let digest = Sha256::digest(format!("{}\0{}", after.identity, message.id).as_bytes());
        let id = format!("native-{digest:x}");
        let mut parts = Vec::new();
        let mut images = Vec::new();
        let blocks = match &message.content {
            Value::String(text) => vec![serde_json::json!({"type":"text", "text":text})],
            Value::Array(blocks) => blocks.clone(),
            _ => return Err(failed("Invalid native message content")),
        };
        for (i, block) in blocks.iter().enumerate() {
            let part_id = format!("{id}-{i}");
            match block["type"].as_str() {
                Some("text" | "input_text" | "output_text") => parts.push(MessagePart::Text {
                    id: part_id,
                    text: field(block, "text")?.into(),
                }),
                Some("thinking") => parts.push(MessagePart::Reasoning {
                    id: part_id,
                    text: field(block, "thinking")?.into(),
                }),
                Some("redacted_thinking") => {}
                Some("image") => images.push(image(block)?),
                Some("toolCall" | "tool_use") => {
                    let tool_id = field(block, "id")?.to_owned();
                    let name = field(block, "name")?;
                    let arguments = block.get("arguments").or_else(|| block.get("input"));
                    let call = if block["type"] == "toolCall" {
                        zeron_harness::provider_history::pi_tool_call(name, arguments.unwrap_or(&Value::Null))
                    } else {
                        ToolCall::Unknown { name: name.into(), input: arguments.cloned() }
                    };
                    tools.insert(tool_id, (entries.len(), parts.len(), call.clone()));
                    parts.push(MessagePart::Tool {
                        id: part_id,
                        call: zeron_doc::sanitize_tool_call(&call),
                        is_error: false,
                        resolved: false,
                        output: None,
                        diff: None,
                        output_ref: None,
                        output_bytes: None,
                        diff_ref: None,
                        diff_stats: None,
                        subagent_ref: None,
                        subagent_status: None,
                        subagent_tail: None,
                    });
                }
                Some("tool_result") => {
                    let tool = field(block, "tool_use_id")?;
                    let Some((entry, part, call)) = tools.get(tool) else {
                        return Err(failed(format!(
                            "Native tool result has no imported call: {tool}"
                        )));
                    };
                    let target: &mut SessionMessageEntry = entries
                        .get_mut(*entry)
                        .ok_or_else(|| failed("Native tool result precedes its call"))?;
                    if let MessagePart::Tool {
                        output,
                        resolved,
                        is_error,
                        diff,
                        ..
                    } = &mut target.parts[*part]
                    {
                        *output =
                            zeron_doc::summarize_tool_output(&content_text(&block["content"], image, &mut images)?);
                        *resolved = true;
                        *is_error = block["is_error"] == true;
                        if !*is_error {
                            *diff = zeron_harness::provider_history::pi_tool_diff(call, block);
                        }
                    }
                }
                kind => {
                    return Err(failed(format!(
                        "Native content {kind:?} cannot yet be hydrated"
                    )));
                }
            }
        }
        if !images.is_empty() {
            let refs = images.iter().map(|path| format!("- {path}")).collect::<Vec<_>>().join("\n");
            parts.push(MessagePart::Text {
                id: format!("{id}-images"),
                text: format!("\n\nAttached images (local files):\n{refs}"),
            });
        }
        if !parts.is_empty() {
            entries.push(SessionMessageEntry {
                id,
                role: if message.role == "user" {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                parts,
                created_at: timestamp(message)?,
                device_id: device.into(),
                status: Some(if message.aborted {
                    MessageStatus::Aborted
                } else {
                    MessageStatus::Complete
                }),
                continuation_of: None,
            });
        }
    }
    Ok(entries)
}

fn timestamp(message: &NativeMessage) -> Result<i64, EngineError> {
    if let Some(ms) = message.timestamp.as_i64() {
        return Ok(ms);
    }
    let date = message
        .timestamp
        .as_str()
        .ok_or_else(|| failed("Native message timestamp is missing"))?;
    chrono::DateTime::parse_from_rfc3339(date)
        .map(|d| d.timestamp_millis())
        .map_err(|e| failed(e.to_string()))
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, EngineError> {
    value[key]
        .as_str()
        .ok_or_else(|| failed(format!("Native content is missing {key}")))
}
fn content_text(
    value: &Value,
    image: &impl Fn(&Value) -> Result<String, EngineError>,
    images: &mut Vec<String>,
) -> Result<String, EngineError> {
    if let Some(s) = value.as_str() {
        return Ok(s.into());
    }
    value
        .as_array()
        .ok_or_else(|| failed("Native tool result is not text"))?
        .iter()
        .map(|b| {
            if b["type"] == "image" {
                let path = image(b)?;
                images.push(path.clone());
                return Ok(format!("Image: {path}"));
            }
            if b["type"] != "text" {
                return Err(failed("Unsupported native tool output content"));
            }
            Ok(field(b, "text")?.to_owned())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pi_session_has_a_durable_private_header() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let path = create_empty_pi_session(&dir.path().join("native-sessions"), cwd).unwrap();
        let mut command = fixture_handoff().command;
        command.history_path = path.clone();
        command.session_id = path.to_str().unwrap().into();
        command.cwd = cwd.into();
        let history = command.read_history().unwrap();
        assert_eq!(history.cwd, cwd);
        assert!(history.messages.is_empty());
        uuid::Uuid::parse_str(&history.identity).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with('\n'));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(create_empty_pi_session(dir.path(), "relative-directory").is_err());
    }

    #[tokio::test]
    async fn unsupported_lifecycle_refuses_before_native_probe_or_ownership_change() {
        use zeron_proto::HarnessId;
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_identity(
            &dir.path().join("engine"), Arc::new(crate::HarnessRegistry::new()),
            HarnessId::Pi, None, "org", "user",
        ).unwrap();
        let views = core.sessions.session_views();
        for harness in [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Cursor, HarnessId::Opencode] {
            let id = format!("unsupported-{harness:?}");
            let mut config = fixture_handoff().config;
            config.harness = harness;
            core.workspace.create_chat(&id, None, Some(&core.device_id),
                Some(config), Some(dir.path().to_str().unwrap().into())).unwrap();
            // A native identity exists, but opening must refuse before trying
            // to find its executable or history in the real user's profile.
            core.sessions.remember_native_session(&id, "ad66b593-8b4e-4308-82b6-8d4a597e202c", dir.path().to_str().unwrap());
            let before = core.workspace.chat(&id).unwrap().unwrap();
            let error = views.open(core.sessions.clone(), core.workspace.clone(), core.terminals.clone(), id.clone(), 80, 24)
                .await.unwrap_err();
            assert!(error.to_string().contains("CLI handoff is unavailable"), "{error}");
            if harness == HarnessId::ClaudeCode {
                assert!(error.to_string().contains("automatic continuations"), "{error}");
            }
            assert_eq!(core.workspace.chat(&id).unwrap().unwrap(), before);
            assert!(matches!(views.get(&id).owner, SessionOwner::Chat));
            assert!(lock(&views.inner.handoffs).is_empty());
            assert!(!core.terminals.any_open());
            assert!(core.sessions.chat_is_empty(&id).unwrap());
        }
        core.doc_host.shutdown_workers().await;
    }

    #[tokio::test]
    async fn empty_cli_rejects_existing_messages_and_commands_without_native_identity() {
        use zeron_doc::{SessionCommandEntry, SessionCommandPayload, SessionCommandStatus};
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_identity(
            &dir.path().join("engine"), Arc::new(crate::HarnessRegistry::new()),
            zeron_proto::HarnessId::Pi, None, "org", "user",
        ).unwrap();
        for id in ["messages", "commands"] {
            core.workspace.create_chat(id, None, Some(&core.device_id),
                Some(fixture_handoff().config), Some(dir.path().to_str().unwrap().into())).unwrap();
            assert!(core.sessions.chat_is_empty(id).unwrap());
            let doc = core.doc_host.open(id).unwrap();
            if id == "messages" {
                doc.doc().push_message(&SessionMessageEntry {
                    id: "prior".into(), role: MessageRole::User,
                    parts: vec![MessagePart::Text { id: "text".into(), text: "keep this history".into() }],
                    created_at: 1, device_id: core.device_id.clone(),
                    status: Some(MessageStatus::Complete), continuation_of: None,
                }).unwrap();
            } else {
                doc.doc().queue_command(&SessionCommandEntry {
                    id: "prior-command".into(), payload: SessionCommandPayload::Interrupt {},
                    issued_by: core.device_id.clone(), issued_at: 1,
                    based_on: None, expires_at: None, status: SessionCommandStatus::Applied,
                    resolution: None,
                }).unwrap();
            }
            assert!(!core.sessions.chat_is_empty(id).unwrap());
            let error = core.sessions.session_views().open(
                core.sessions.clone(), core.workspace.clone(), core.terminals.clone(), id.into(), 80, 24,
            ).await.unwrap_err();
            assert!(error.to_string().contains("Only an empty Pi chat"), "{error}");
            assert!(core.workspace.chat(id).unwrap().unwrap().harness_session_id.is_none());
            assert!(!core.terminals.any_open());
        }
        core.doc_host.shutdown_workers().await;
    }

    fn fixture_handoff() -> Handoff {
        use zeron_proto::{HarnessId, SandboxLevel};
        Handoff {
            command: ResumeCommand {
                program: "/bin/true".into(),
                args: vec![],
                env: vec![("API_KEY".into(), "must-not-be-written".into())],
                history_path: "/tmp/native.jsonl".into(),
                session_id: "/tmp/native.jsonl".into(),
                cwd: "/tmp".into(),
                harness: HarnessId::Pi,
            },
            config: ChatConfig {
                harness: HarnessId::Pi,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            },
            baseline: NativeHistory {
                identity: "native".into(),
                cwd: "/tmp".into(),
                model: None,
                reasoning: None,
                messages: vec![NativeMessage {
                    id: "one".into(),
                    role: "user".into(),
                    timestamp: Value::from(1),
                    content: Value::from("preserve me"),
                    aborted: false,
                }],
            },
            terminal: None,
            owner: SessionOwner::Opening,
            error: None,
            process: ProcessState::SpawnPending { boot: boot_id() },
            recovered: false,
        }
    }

    #[test]
    fn journal_restart_preserves_baseline_and_omits_environment() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let path = dir.path().join("handoffs.json");
        let views = SessionViews::load(&path, store.clone()).unwrap();
        views.put("chat", fixture_handoff()).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("API_KEY"));
        assert!(!bytes.contains("must-not-be-written"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(views);
        let restored = SessionViews::load(&path, store).unwrap();
        assert!(restored.chat_permit("chat").is_err());
        assert!(matches!(
            restored.get("chat").owner,
            SessionOwner::RecoveryRequired
        ));
        let handoffs = lock(&restored.inner.handoffs);
        let h = &handoffs["chat"];
        assert_eq!(h.baseline.messages, fixture_handoff().baseline.messages);
        assert!(h.command.env.is_empty());
        assert!(matches!(h.process, ProcessState::SpawnPending { .. }));
    }

    #[tokio::test]
    async fn engine_restart_hydrates_durable_suffix_without_respawning() {
        use zeron_proto::HarnessId;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("engine");
        let assemble = || {
            crate::EngineCore::assemble_with_identity(
                &root,
                Arc::new(crate::HarnessRegistry::new()),
                HarnessId::Pi,
                None,
                "org",
                "user",
            )
            .unwrap()
        };
        let core = assemble();
        let cwd = dir.path().to_string_lossy().into_owned();
        let path = dir.path().join("native.jsonl");
        let header = serde_json::json!({"type":"session", "version":3, "id":"native", "cwd":cwd});
        let first = serde_json::json!({"type":"message", "id":"one", "parentId":null,
            "message":{"role":"user", "content":"before", "timestamp":1}});
        let next = serde_json::json!({"type":"message", "id":"two", "parentId":"one",
            "message":{"role":"assistant", "content":"survives restart", "timestamp":2}});
        std::fs::write(&path, format!("{header}\n{first}\n")).unwrap();
        let mut h = fixture_handoff();
        h.command.cwd = cwd.clone();
        h.command.history_path = path.clone();
        h.command.session_id = path.to_string_lossy().into_owned();
        h.baseline = h.command.read_history().unwrap();
        h.process = ProcessState::Dead;
        core.workspace
            .create_chat(
                "chat",
                None,
                Some(&core.device_id),
                Some(h.config.clone()),
                Some(cwd),
            )
            .unwrap();
        core.sessions.session_views().put("chat", h).unwrap();
        std::fs::write(&path, format!("{header}\n{first}\n{next}\n")).unwrap();
        core.workspace.flush();
        core.doc_host.shutdown_workers().await;
        drop(core);

        let core = assemble();
        let views = core.sessions.session_views();
        assert!(matches!(
            views.get("chat").owner,
            SessionOwner::RecoveryRequired
        ));
        assert!(!core.terminals.any_open());
        let result = views
            .close(
                core.sessions.clone(),
                core.workspace.clone(),
                core.doc_host.clone(),
                core.terminals.clone(),
                "chat".into(),
            )
            .await
            .unwrap();
        assert_eq!(result.imported_messages, 1);
        assert!(views.chat_permit("chat").is_ok());
        core.doc_host.shutdown_workers().await;
        drop(views);
        drop(core);

        let core = assemble();
        assert!(core.sessions.session_views().chat_permit("chat").is_ok());
        let messages = core
            .doc_host
            .open("chat")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].parts[0], MessagePart::Text { text, .. } if text == "survives restart")
        );
        core.doc_host.shutdown_workers().await;
    }

    #[tokio::test]
    async fn recovered_spawn_window_cannot_release_chat_on_the_same_boot() {
        use zeron_proto::HarnessId;
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_identity(
            dir.path(),
            Arc::new(crate::HarnessRegistry::new()),
            HarnessId::Pi,
            None,
            "org",
            "user",
        )
        .unwrap();
        let h = fixture_handoff();
        core.workspace
            .create_chat(
                "chat",
                None,
                Some(&core.device_id),
                Some(h.config.clone()),
                Some(h.command.cwd.clone()),
            )
            .unwrap();
        let views = core.sessions.session_views();
        views.put("chat", h).unwrap();
        let restored = SessionViews::load(
            views.inner.path.as_ref().unwrap(),
            views.inner.store.clone().unwrap(),
        )
        .unwrap();
        let error = restored
            .close(
                core.sessions.clone(),
                core.workspace.clone(),
                core.doc_host.clone(),
                core.terminals.clone(),
                "chat".into(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("host reboot"), "{error}");
        assert!(restored.chat_permit("chat").is_err());
        assert!(!core.terminals.any_open());
        assert_eq!(
            lock(&restored.inner.handoffs)["chat"].baseline.messages,
            fixture_handoff().baseline.messages
        );
        core.doc_host.shutdown_workers().await;
    }

    #[test]
    fn failed_load_and_save_never_release_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let path = dir.path().join("handoffs.json");
        std::fs::write(&path, "{broken").unwrap();
        assert!(SessionViews::load(&path, store.clone()).is_err());
        std::fs::remove_file(&path).unwrap();
        let views = SessionViews::load(&path, store.clone()).unwrap();
        views.put("chat", fixture_handoff()).unwrap();
        let backup = dir.path().join("backup.json");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(views.release("chat").is_err());
        assert!(views.chat_permit("chat").is_err());
        assert!(views.chat_permit("another").is_err());
        assert!(views.put("second", fixture_handoff()).is_err());
        assert!(views.chat_permit("second").is_err());
        assert!(SessionViews::load(&path, store.clone()).is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        // The disk still has the last durable baseline, not a partial replacement.
        let restored = SessionViews::load(&path, store).unwrap();
        assert_eq!(
            lock(&restored.inner.handoffs)["chat"].baseline.messages,
            fixture_handoff().baseline.messages
        );
        views.release("chat").unwrap();
        assert!(views.chat_permit("chat").is_ok());
        assert!(views.chat_permit("second").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixture_cli_reattaches_and_failed_import_keeps_ownership_until_retry() {
        use zeron_proto::{HarnessId, RunRequest, SandboxLevel};
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_identity(
            &dir.path().join("engine"),
            Arc::new(crate::HarnessRegistry::new()),
            HarnessId::Pi,
            None,
            "org",
            "user",
        )
        .unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let config = ChatConfig {
            harness: HarnessId::Pi,
            model: Some("provider/model".into()),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        };
        core.workspace
            .create_chat(
                "handoff-chat",
                None,
                Some(&core.device_id),
                Some(config.clone()),
                Some(cwd.clone()),
            )
            .unwrap();
        let path = dir.path().join("session ; literal.jsonl");
        let header = serde_json::json!({"type":"session", "version":3,"id":"fixture", "cwd":cwd});
        let first = serde_json::json!({"type":"message","id":"a","parentId":null,
            "message":{"role":"user","content":"before", "timestamp":1}});
        let next = serde_json::json!({"type":"message","id":"b","parentId":"a",
            "message":{"role":"assistant","content":[{"type":"text","text":"from CLI"}], "timestamp":2,
                "provider":"provider", "model":"model"}});
        let before = format!("{header}\n{first}\n");
        let after = format!("{before}{next}\n");
        std::fs::write(&path, &before).unwrap();
        let command = ResumeCommand {
            program: "/bin/sh".into(),
            args: vec![],
            env: vec![],
            history_path: path.clone(),
            session_id: path.to_string_lossy().into_owned(),
            cwd: cwd.clone(),
            harness: HarnessId::Pi,
        };
        let baseline = command.read_history().unwrap();
        let terminal = core
            .terminals
            .open_argv(
                &cwd,
                80,
                24,
                "/bin/sh",
                &[
                    "-c".into(),
                    "printf '%s\n' \"$2\" >> \"$1\"; printf READY; read -r line".into(),
                    "fixture".into(),
                    path.to_string_lossy().into_owned(),
                    next.to_string(),
                ],
                &[],
            )
            .unwrap();
        let mut output = core.terminals.subscribe(&terminal.id, None).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            use base64::Engine;
            while let Some(event) = output.recv().await {
                if let zeron_proto::TerminalEvent::Data { data, .. } = event {
                    let text = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap();
                    if String::from_utf8_lossy(&text).contains("READY") {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        let views = core.sessions.session_views();
        views
            .put(
                "handoff-chat",
                Handoff {
                    command,
                    config,
                    baseline,
                    terminal: Some(terminal.clone()),
                    owner: SessionOwner::Cli,
                    error: None,
                    process: ProcessState::Spawned {
                        identity: core.terminals.process_identity(&terminal.id).unwrap(),
                        boot: boot_id(),
                    },
                    recovered: false,
                },
            )
            .unwrap();
        let reattached = views
            .open(
                core.sessions.clone(),
                core.workspace.clone(),
                core.terminals.clone(),
                "handoff-chat".into(),
                100,
                40,
            )
            .await
            .unwrap();
        assert_eq!(reattached.id, terminal.id);
        assert!(core.terminals.close(&terminal.id).is_err());
        #[cfg(target_os = "linux")]
        {
            let restored = SessionViews::load(
                views.inner.path.as_ref().unwrap(),
                views.inner.store.clone().unwrap(),
            )
            .unwrap();
            let empty_terminals = Terminals::new();
            let error = restored
                .close(
                    core.sessions.clone(),
                    core.workspace.clone(),
                    core.doc_host.clone(),
                    empty_terminals.clone(),
                    "handoff-chat".into(),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("still running"), "{error}");
            assert!(!empty_terminals.any_open());
            assert!(restored.chat_permit("handoff-chat").is_err());
        }
        let request = RunRequest {
            prompt: "must not run".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd,
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            resume: None,
            attachments: vec![],
            worktree: None,
        };
        assert!(
            core.sessions
                .dispatch("handoff-chat", HarnessId::Pi, request, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("owned")
        );
        // This shell fixture has no provider lifecycle. It must be refused
        // while live, then explicitly stopped before testing failed hydration.
        assert!(views.close(core.sessions.clone(), core.workspace.clone(), core.doc_host.clone(),
            core.terminals.clone(), "handoff-chat".into()).await.is_err());
        assert!(matches!(views.get("handoff-chat").owner, SessionOwner::Cli));
        core.terminals.terminate_and_wait(&terminal.id).await.unwrap();
        std::fs::write(&path, format!("{after}{{")).unwrap();
        let close = || {
            views.close(
                core.sessions.clone(),
                core.workspace.clone(),
                core.doc_host.clone(),
                core.terminals.clone(),
                "handoff-chat".into(),
            )
        };
        assert!(close().await.is_err());
        assert!(views.chat_permit("handoff-chat").is_err());
        assert!(matches!(
            views.get("handoff-chat").owner,
            SessionOwner::RecoveryRequired
        ));
        // Restart the ownership service after teardown but before hydration.
        // No PTY registry or runtime environment survives this reload.
        let views = SessionViews::load(
            views.inner.path.as_ref().unwrap(),
            views.inner.store.clone().unwrap(),
        )
        .unwrap();
        let terminals = Terminals::new();
        let close = || {
            views.close(
                core.sessions.clone(),
                core.workspace.clone(),
                core.doc_host.clone(),
                terminals.clone(),
                "handoff-chat".into(),
            )
        };
        assert!(views.chat_permit("handoff-chat").is_err());
        assert!(views.get("handoff-chat").terminal.is_none());
        assert!(close().await.is_err());
        std::fs::write(&path, after).unwrap();
        let outcome = close().await.unwrap();
        assert_eq!(outcome.imported_messages, 1);
        assert!(views.chat_permit("handoff-chat").is_ok());
        assert_eq!(close().await.unwrap().imported_messages, 0);
        let messages = core
            .doc_host
            .open("handoff-chat")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].parts[0], MessagePart::Text { text, .. } if text == "from CLI")
        );
        core.doc_host.shutdown_workers().await;
    }
    #[tokio::test]
    async fn ownership_excludes_dispatch_during_transition() {
        let views = SessionViews::default();
        let gate = views.gate("chat").lock_owned().await;
        assert!(views.chat_permit("chat").is_err());
        assert!(views.chat_permit("another").is_ok());
        drop(gate);
        assert!(views.chat_permit("chat").is_ok());
    }
    #[test]
    fn hydration_uses_native_suffix_not_text_deduplication() {
        let m = |id: &str| NativeMessage {
            id: id.into(),
            role: "user".into(),
            timestamp: Value::from(1),
            content: Value::from("again"),
            aborted: false,
        };
        let before = NativeHistory {
            identity: "session".into(),
            cwd: "/tmp".into(),
            model: None,
            reasoning: None,
            messages: vec![m("a")],
        };
        let mut after = before.clone();
        after.messages.push(m("b"));
        let entries = hydrate_entries(&before, &after, "device").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries, hydrate_entries(&before, &after, "device").unwrap());
        after.messages[0].id = "rewritten".into();
        assert!(hydrate_entries(&before, &after, "device").is_err());
    }

    #[test]
    fn pi_hydration_preserves_thinking_typed_tools_and_diffs() {
        use serde_json::json;
        use zeron_harness::provider_history::parse_history;
        let before = format!("{}\n", json!({"type":"session","version":3,"id":"pi-session","cwd":"/tmp"}));
        let mut after = before.clone();
        for record in [
            json!({"type":"message","id":"a","parentId":null,"message":{
                "role":"assistant","provider":"test","model":"model","timestamp":1,
                "content":[{"type":"thinking","thinking":"Check the file"},
                    {"type":"toolCall","id":"tool-1","name":"edit","arguments":{
                        "path":"/tmp/example.txt","oldText":"before","newText":"after"}}]}}),
            json!({"type":"message","id":"b","parentId":"a","message":{
                "role":"toolResult","toolCallId":"tool-1","timestamp":2,"isError":false,
                "content":[{"type":"text","text":"Updated example.txt"}],
                "details":{"diff":"-before\n+after"}}}),
            json!({"type":"message","id":"c","parentId":"b","message":{
                "role":"assistant","provider":"test","model":"model","timestamp":3,
                "content":[{"type":"text","text":"File updated."}]}}),
        ] {
            after.push_str(&format!("{record}\n"));
        }
        let before = parse_history(zeron_proto::HarnessId::Pi, &before).unwrap();
        let after = parse_history(zeron_proto::HarnessId::Pi, &after).unwrap();
        let entries = hydrate_entries(&before, &after, "device").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0].parts[0], MessagePart::Reasoning { text, .. } if text == "Check the file"));
        let MessagePart::Tool { call, resolved, output, diff, .. } = &entries[0].parts[1] else { panic!("tool missing") };
        assert!(matches!(call, ToolCall::EditFile { path, .. } if path == "/tmp/example.txt"));
        assert!(*resolved);
        assert_eq!(output.as_deref(), Some("Updated example.txt"));
        assert_eq!(diff.as_ref().unwrap().new_text, "-before\n+after");
        assert_eq!(diff.as_ref().unwrap().old_text.as_deref(), Some("before"));
        assert_eq!(entries, hydrate_entries(&before, &after, "device").unwrap());
    }

    #[tokio::test]
    async fn pi_images_are_durable_attachment_refs_and_retry_without_duplication() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble_with_identity(dir.path(),
            Arc::new(crate::HarnessRegistry::new()), zeron_proto::HarnessId::Pi, None, "org", "user").unwrap();
        let bytes = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/zeron.png")).unwrap();
        let data = STANDARD.encode(&bytes);
        let before = NativeHistory { identity: "pi-images".into(), cwd: dir.path().display().to_string(), model: None, reasoning: None, messages: vec![] };
        let mut after = before.clone();
        after.messages.push(NativeMessage { id: "user-image".into(), role: "user".into(), timestamp: json!(1), aborted: false,
            content: json!([{"type":"text","text":"Inspect this"}, {"type":"image","data":data,"mimeType":"image/png"}]) });
        let resolve = |block: &Value| core.doc_host.store_native_image(field(block, "data")?, field(block, "mimeType")?);
        let entries = hydrate_entries_with_images(&before, &after, "device", &resolve).unwrap();
        assert_eq!(entries, hydrate_entries_with_images(&before, &after, "device", &resolve).unwrap());
        let MessagePart::Text { text, .. } = &entries[0].parts[1] else { panic!("attachment refs missing") };
        let path = text.lines().find_map(|line| line.strip_prefix("- ")).unwrap();
        assert!(Path::new(path).starts_with(core.uploads.dir()));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(core.uploads.read_chunk(path, 0, &[]).unwrap().mime_type, "image/png");
        assert!(core.doc_host.store_native_image(&data, "image/svg+xml").is_err());
        assert!(core.doc_host.store_native_image("not base64", "image/png").is_err());
        assert_eq!(std::fs::read_dir(core.uploads.dir()).unwrap().count(), 1);
        core.doc_host.shutdown_workers().await;
    }
}

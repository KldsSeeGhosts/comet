use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Pi,
    Codex,
    OpenCode,
    Cursor,
    Kiro,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::OpenCode => "open_code",
            Self::Cursor => "cursor",
            Self::Kiro => "kiro",
        }
    }
}

/// Host-owned identity. Native session identifiers never replace `session_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub session_id: String,
    pub terminal_id: String,
    pub worktree_path: PathBuf,
    pub provider: Provider,
    /// Set when resuming a known native session. Mismatching native events fail closed.
    pub provider_session_id: Option<String>,
}

impl Binding {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.session_id)?;
        identifier(&self.terminal_id)?;
        if let Some(id) = &self.provider_session_id {
            identifier(id)?;
        }
        validate_absolute(&self.worktree_path)?;
        ensure!(self.worktree_path.is_dir(), "worktree is not a directory");
        ensure!(
            self.worktree_path.canonicalize()?.as_os_str() == self.worktree_path.as_os_str(),
            "worktree must be canonical"
        );
        Ok(())
    }
}

pub(crate) fn validate_absolute(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "path must be absolute");
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "path cannot contain traversal components"
    );
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path must be UTF-8"))?;
    ensure!(
        text.len() <= 4096 && !text.chars().any(char::is_control),
        "invalid path"
    );
    ensure!(
        !text.split('/').any(|part| part == "." || part == ".."),
        "path cannot contain traversal components"
    );
    Ok(())
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "invalid identifier"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEvent {
    SessionStarted,
    SessionEnded,
    PromptSubmitted,
    RunStarted,
    RunEnded,
    TurnStepCompleted,
    ResponseCompleted,
    TurnCompleted,
    SubagentStarted,
    SubagentStopped,
    PermissionRequested,
    PermissionResolved,
    ToolStarted,
    ToolFinished,
    Interrupted,
    Notification,
}

/// Known spellings only. Tool preflight is not evidence of a permission prompt.
/// Pi's `turn_end` and `agent_end` are not evidence that the agent has settled.
pub fn normalize(name: &str) -> Option<TurnEvent> {
    if name.len() > 128 || !name.is_ascii() {
        return None;
    }
    let key: String = name
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect();
    Some(match key.as_str() {
        "sessionstart" | "sessionstarted" | "agentspawn" => TurnEvent::SessionStarted,
        "sessionend" | "sessionshutdown" | "sessiondeleted" => TurnEvent::SessionEnded,
        "userpromptsubmit" | "beforesubmitprompt" | "beforeagentstart" | "promptsubmitted" => {
            TurnEvent::PromptSubmitted
        }
        "agentstart" | "runstarted" => TurnEvent::RunStarted,
        "agentend" | "runended" => TurnEvent::RunEnded,
        "turnend" | "turnstepcompleted" => TurnEvent::TurnStepCompleted,
        "afteragentresponse" | "responsecompleted" => TurnEvent::ResponseCompleted,
        "stop" | "turncomplete" | "turncompleted" | "agentturncomplete" | "agentsettled"
        | "sessionidle" => TurnEvent::TurnCompleted,
        "subagentstart" | "subagentstarted" | "backgroundtaskstarted" => TurnEvent::SubagentStarted,
        "subagentstop" | "subagentstopped" | "backgroundtaskcompleted" => {
            TurnEvent::SubagentStopped
        }
        "permissionrequest"
        | "permissionrequested"
        | "permissionasked"
        | "approvalrequested"
        | "uipromptstart" => TurnEvent::PermissionRequested,
        "permissionresolved" | "permissionreplied" | "uipromptend" => TurnEvent::PermissionResolved,
        "pretooluse" | "beforetoolcall" | "toolcall" | "toolexecutionstart"
        | "toolexecutebefore" => TurnEvent::ToolStarted,
        "posttooluse" | "posttoolusefailure" | "aftertoolcall" | "toolexecutionend"
        | "toolexecuteafter" => TurnEvent::ToolFinished,
        "interrupted" | "sessionerror" | "stopfailure" => TurnEvent::Interrupted,
        "notification" => TurnEvent::Notification,
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEnvelope {
    pub binding: Binding,
    /// A producer creates this once per event, then retains it through retries.
    pub event_id: String,
    pub event_name: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedEvent {
    pub binding: Binding,
    pub event_id: String,
    pub kind: TurnEvent,
    pub background_tasks: usize,
    /// An overflow or an unidentifiable start keeps completion suppressed until reconciliation.
    pub background_unknown: bool,
    pub demoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Accepted(AcceptedEvent),
    Duplicate,
    Ignored,
}

/// The server adapter constructs this from the actual connection, not request headers.
#[derive(Debug, Clone, Copy)]
pub enum Peer {
    Unix,
    Tcp(IpAddr),
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub sessions: usize,
    pub dedupe_per_session: usize,
    pub background_per_session: usize,
    pub body_bytes: usize,
    pub idle_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sessions: 256,
            dedupe_per_session: 1024,
            background_per_session: 256,
            body_bytes: 65_536,
            idle_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Intentionally not Debug or Serialize: callers must not log the bearer token.
pub struct Registration {
    pub binding: Binding,
    pub token: String,
}

struct Session {
    binding: Binding,
    seen: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
    background: HashSet<String>,
    unknown: bool,
    touched: Instant,
}

pub struct HookReceiver {
    limits: Limits,
    sessions: HashMap<[u8; 32], Session>,
}

impl Default for HookReceiver {
    fn default() -> Self {
        Self::new(Limits::default()).expect("default limits are valid")
    }
}

impl HookReceiver {
    pub fn new(limits: Limits) -> Result<Self> {
        ensure!(
            limits.sessions > 0
                && limits.dedupe_per_session > 0
                && limits.background_per_session > 0
                && limits.body_bytes > 0
                && !limits.idle_ttl.is_zero(),
            "limits must be positive"
        );
        Ok(Self {
            limits,
            sessions: HashMap::new(),
        })
    }

    pub fn register(&mut self, binding: Binding) -> Result<Registration> {
        binding.validate()?;
        self.prune();
        ensure!(
            !self
                .sessions
                .values()
                .any(|s| s.binding.session_id == binding.session_id),
            "session already registered; revoke before rebinding"
        );
        ensure!(
            self.sessions.len() < self.limits.sessions,
            "session capacity reached"
        );
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        self.sessions.insert(
            hash(token.as_bytes()),
            Session {
                binding: binding.clone(),
                seen: HashSet::new(),
                order: VecDeque::new(),
                background: HashSet::new(),
                unknown: false,
                touched: Instant::now(),
            },
        );
        Ok(Registration { binding, token })
    }

    pub fn revoke(&mut self, session_id: &str) {
        self.sessions
            .retain(|_, s| s.binding.session_id != session_id);
    }

    pub fn prune(&mut self) {
        self.sessions
            .retain(|_, s| s.touched.elapsed() < self.limits.idle_ttl);
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Host-only reconciliation, not a hook request. No synthetic completion is emitted.
    pub fn reconcile_background(&mut self, session_id: &str, task_ids: &[String]) -> Result<()> {
        ensure!(
            task_ids.len() <= self.limits.background_per_session,
            "too many background tasks"
        );
        for id in task_ids {
            identifier(id)?;
        }
        let session = self
            .sessions
            .values_mut()
            .find(|s| s.binding.session_id == session_id)
            .ok_or_else(|| anyhow::anyhow!("unknown session"))?;
        session.background = task_ids.iter().cloned().collect();
        session.unknown = false;
        Ok(())
    }

    /// Authenticate before parsing JSON. Reject non-loopback peers, mismatched bindings,
    /// oversized bodies, invalid native identities and paths without changing state.
    /// The caller must also cap the HTTP body while reading it, before allocation.
    pub fn handle(&mut self, peer: Peer, token: &str, body: &[u8]) -> Result<Outcome> {
        if let Peer::Tcp(ip) = peer {
            ensure!(ip.is_loopback(), "non-local hook peer");
        }
        ensure!(
            token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "unauthorized hook"
        );
        ensure!(body.len() <= self.limits.body_bytes, "hook body too large");
        self.prune();
        let session = self
            .sessions
            .get_mut(&hash(token.as_bytes()))
            .ok_or_else(|| anyhow::anyhow!("unauthorized hook"))?;
        let envelope: HookEnvelope = serde_json::from_slice(body)?;
        ensure!(
            envelope.binding == session.binding
                && envelope.binding.worktree_path.as_os_str()
                    == session.binding.worktree_path.as_os_str(),
            "hook binding mismatch"
        );
        identifier(&envelope.event_id)?;
        let Some(mut kind) = normalize(&envelope.event_name) else {
            return Ok(Outcome::Ignored);
        };
        validate_native(&session.binding, &envelope.payload)?;
        if kind == TurnEvent::Notification
            && envelope
                .payload
                .get("notification_type")
                .and_then(Value::as_str)
                == Some("permission_prompt")
        {
            kind = TurnEvent::PermissionRequested;
        }
        let task = ["agent_id", "subagent_id", "task_id"]
            .iter()
            .find_map(|key| envelope.payload.get(key).and_then(Value::as_str));
        if let Some(task) = task {
            identifier(task)?;
        }
        let id = hash(envelope.event_id.as_bytes());
        if session.seen.contains(&id) {
            return Ok(Outcome::Duplicate);
        }
        if session.order.len() == self.limits.dedupe_per_session
            && let Some(old) = session.order.pop_front()
        {
            session.seen.remove(&old);
        }
        session.seen.insert(id);
        session.order.push_back(id);
        session.touched = Instant::now();
        match kind {
            TurnEvent::SubagentStarted => {
                if let Some(task) = task {
                    if session.background.len() < self.limits.background_per_session
                        || session.background.contains(task)
                    {
                        session.background.insert(task.to_owned());
                    } else {
                        session.unknown = true;
                    }
                } else {
                    session.unknown = true;
                }
            }
            TurnEvent::SubagentStopped => {
                if let Some(task) = task {
                    session.background.remove(task);
                }
            }
            _ => {}
        }
        let demoted =
            kind == TurnEvent::TurnCompleted && (!session.background.is_empty() || session.unknown);
        if demoted {
            kind = TurnEvent::SubagentStopped;
        }
        Ok(Outcome::Accepted(AcceptedEvent {
            binding: session.binding.clone(),
            event_id: envelope.event_id,
            kind,
            background_tasks: session.background.len(),
            background_unknown: session.unknown,
            demoted,
        }))
    }
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn validate_native(binding: &Binding, payload: &Value) -> Result<()> {
    ensure!(
        payload.is_object() || payload.is_null(),
        "hook payload must be an object"
    );
    for key in ["cwd", "worktree_path"] {
        if let Some(value) = payload.get(key) {
            let path = Path::new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid native path"))?,
            );
            validate_absolute(path)?;
            // Exact root only: a subdirectory or lexical/symlink alias is not the binding.
            ensure!(
                path.as_os_str() == binding.worktree_path.as_os_str(),
                "native worktree mismatch"
            );
        }
    }
    if let Some(expected) = &binding.provider_session_id {
        let keys: &[&str] = match binding.provider {
            Provider::Codex => &["thread-id"],
            Provider::Cursor => &["conversation_id"],
            Provider::OpenCode => &["sessionID"],
            _ => &["session_id"],
        };
        for key in keys {
            if let Some(actual) = payload.get(*key) {
                ensure!(actual.as_str() == Some(expected), "native session mismatch");
            } else {
                bail!("native session identifier missing");
            }
        }
    }
    Ok(())
}

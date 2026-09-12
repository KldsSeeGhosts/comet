//! Read native session files without starting a second provider process.
//! Pi v2/v3 uses id/parentId; Claude uses uuid/parentUuid in project JSONL.

use crate::HarnessError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use zeron_proto::{ChatConfig, HarnessId, ReasoningLevel};

fn invalid(message: impl Into<String>) -> HarnessError {
    HarnessError::Protocol(message.into())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
    // Runtime-only. A recovered handoff reads history but never respawns a CLI.
    #[serde(skip)]
    pub env: Vec<(String, String)>,
    pub history_path: PathBuf,
    pub session_id: String,
    pub cwd: String,
    pub harness: HarnessId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeMessage {
    pub id: String,
    pub role: String,
    pub timestamp: Value,
    pub content: Value,
    pub aborted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeHistory {
    pub identity: String,
    pub cwd: String,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub messages: Vec<NativeMessage>,
}

/// Use the same typed tools and diffs for resumed Pi history and live RPC turns.
pub fn pi_tool_call(name: &str, arguments: &Value) -> zeron_proto::ToolCall {
    crate::pi::map_tool_call(name, arguments)
}

pub fn pi_tool_diff(call: &zeron_proto::ToolCall, result: &Value) -> Option<zeron_proto::ToolDiff> {
    crate::pi::tool_diff(Some(call), Some(result))
}

/// Validate the exact native session and supported options before interrupting Chat.
pub fn prepare_resume(
    config: &ChatConfig,
    session_id: &str,
    cwd: &str,
) -> Result<ResumeCommand, HarnessError> {
    if session_id.is_empty() || !Path::new(cwd).is_absolute() || !Path::new(cwd).is_dir() {
        return Err(invalid(
            "Native session identity or working directory is unavailable",
        ));
    }
    if !config.model_options.is_empty() {
        return Err(invalid(
            "CLI handoff does not yet support model-specific options",
        ));
    }
    let (program, history_path) = match config.harness {
        HarnessId::Pi => {
            let path = PathBuf::from(session_id);
            if !path.is_absolute() || path.extension().is_none_or(|s| s != "jsonl") {
                return Err(invalid(
                    "Pi handoff requires an absolute native session JSONL path",
                ));
            }
            let exe = crate::pi::resolve_pi_executable()
                .ok_or_else(|| HarnessError::NotInstalled("pi".into()))?;
            (exe, path)
        }
        HarnessId::ClaudeCode => {
            uuid::Uuid::parse_str(session_id)
                .map_err(|_| invalid("Claude handoff requires an exact session UUID"))?;
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| invalid("HOME is unavailable"))?;
            let root = std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"));
            let project: String = cwd
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let path = root
                .join("projects")
                .join(project)
                .join(format!("{session_id}.jsonl"));
            (resolve_claude(&home)?, path)
        }
        HarnessId::Codex => {
            return Err(invalid(
                "Codex CLI handoff is not supported yet: app-server rollout hydration is not implemented",
            ));
        }
        other => {
            return Err(invalid(format!(
                "{other:?} does not support native CLI handoff"
            )));
        }
    };
    if !program.is_file() {
        return Err(HarnessError::NotInstalled(program.display().to_string()));
    }
    let args = resume_args(config, session_id)?;
    let mut command = tokio::process::Command::new(&program);
    crate::compose_child_path(&mut command, &program);
    let mut env: Vec<_> = command
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    env.push(("PI_SKIP_VERSION_CHECK".into(), "1".into()));
    let resume = ResumeCommand {
        program: program.to_string_lossy().into_owned(),
        args,
        env,
        history_path,
        session_id: session_id.into(),
        cwd: cwd.into(),
        harness: config.harness,
    };
    resume.read_history()?;
    Ok(resume)
}

fn resolve_claude(home: &Path) -> Result<PathBuf, HarnessError> {
    if let Some(path) = std::env::var_os("CLAUDE_CODE_EXECUTABLE").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join("claude")));
    }
    if let Some(path) = crate::shell_env::login_shell_path() {
        candidates.extend(std::env::split_paths(path).map(|p| p.join("claude")));
    }
    candidates.extend([
        home.join(".claude/local/claude"),
        home.join(".local/bin/claude"),
    ]);
    candidates.extend(
        crate::node_version_manager_bins()
            .iter()
            .map(|p| p.join("claude")),
    );
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| HarnessError::NotInstalled("claude".into()))
}

fn resume_args(config: &ChatConfig, session: &str) -> Result<Vec<String>, HarnessError> {
    let mut args = match config.harness {
        HarnessId::Pi => vec!["--session".into(), session.into()],
        HarnessId::ClaudeCode => vec![format!("--resume={session}")],
        _ => return Err(invalid("Provider does not support native CLI handoff")),
    };
    if let Some(model) = &config.model {
        if model.is_empty() || model.starts_with('-') || model.contains('\0') {
            return Err(invalid("Invalid model for CLI handoff"));
        }
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(level) = config.reasoning {
        let spelling = serde_json::to_value(level).map_err(|e| invalid(e.to_string()))?;
        let level = spelling
            .as_str()
            .ok_or_else(|| invalid("Invalid reasoning level"))?;
        let supported = if config.harness == HarnessId::Pi {
            ["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&level)
        } else {
            ["low", "medium", "high", "xhigh", "max"].contains(&level)
        };
        if !supported {
            return Err(invalid(format!(
                "Reasoning level {level} cannot be preserved in this CLI"
            )));
        }
        args.extend([
            if config.harness == HarnessId::Pi {
                "--thinking"
            } else {
                "--effort"
            }
            .into(),
            level.into(),
        ]);
    }
    // Interactive CLIs retain their own approval prompts. Never introduce bypass flags.
    Ok(args)
}

impl ResumeCommand {
    pub fn read_history(&self) -> Result<NativeHistory, HarnessError> {
        use std::io::Read;
        const MAX: u64 = 128 * 1024 * 1024;
        let file = std::fs::File::open(&self.history_path)?;
        let mut bytes = Vec::new();
        file.take(MAX + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX {
            return Err(invalid("Native history exceeds 128 MiB"));
        }
        let text = String::from_utf8(bytes).map_err(|e| invalid(e.to_string()))?;
        let history = parse_history(self.harness, &text)?;
        if history.cwd != self.cwd {
            return Err(invalid("Native history cwd does not match the chat"));
        }
        if self.harness == HarnessId::ClaudeCode && history.identity != self.session_id {
            return Err(invalid(
                "Native history session UUID does not match the chat",
            ));
        }
        Ok(history)
    }
}

/// Strict parsing is intentional. A torn file must not silently erase a CLI turn.
pub fn parse_history(harness: HarnessId, text: &str) -> Result<NativeHistory, HarnessError> {
    let records: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l)
                .map_err(|e| invalid(format!("Native history line {}: {e}", i + 1)))
        })
        .collect::<Result<_, _>>()?;
    let pi = harness == HarnessId::Pi;
    if !pi && harness != HarnessId::ClaudeCode {
        return Err(invalid("Unsupported history provider"));
    }
    let (identity, cwd) = if pi {
        let header = records.first().ok_or_else(|| invalid("Empty Pi session"))?;
        if header["type"] != "session" || !matches!(header["version"].as_u64(), Some(2 | 3)) {
            return Err(invalid("Unsupported Pi session header; expected v2 or v3"));
        }
        (
            required(header, "id")?.to_owned(),
            required(header, "cwd")?.to_owned(),
        )
    } else {
        let record = records
            .iter()
            .find(|r| r["message"].is_object() && r["isSidechain"] != true)
            .ok_or_else(|| invalid("Claude session has no main conversation"))?;
        (
            required(record, "sessionId")?.to_owned(),
            required(record, "cwd")?.to_owned(),
        )
    };
    let (id_key, parent_key) = if pi {
        ("id", "parentId")
    } else {
        ("uuid", "parentUuid")
    };
    let mut index = HashMap::new();
    let mut leaf = None;
    for record in &records {
        if (pi && record["type"] == "session") || record["isSidechain"] == true {
            continue;
        }
        if let Some(id) = record[id_key].as_str() {
            if !pi && record.get("sessionId").is_some_and(|s| s != &identity) {
                return Err(invalid("Mixed Claude session identities"));
            }
            if index.insert(id, record).is_some() {
                return Err(invalid("Duplicate native entry ID"));
            }
            leaf = Some(id);
        }
    }
    let mut branch = Vec::new();
    let mut seen = HashSet::new();
    while let Some(id) = leaf {
        if !seen.insert(id) {
            return Err(invalid("Cycle in native history"));
        }
        let record = index
            .get(id)
            .ok_or_else(|| invalid("Native history parent is missing"))?;
        branch.push(*record);
        leaf = record[parent_key].as_str();
    }
    branch.reverse();
    let mut history = NativeHistory {
        identity,
        cwd,
        model: None,
        reasoning: None,
        messages: Vec::new(),
    };
    for record in branch {
        if pi && record["type"] == "model_change" {
            history.model = Some(format!(
                "{}/{}",
                required(record, "provider")?,
                required(record, "modelId")?
            ));
        }
        if pi && record["type"] == "thinking_level_change" {
            history.reasoning = Some(
                serde_json::from_value(record["thinkingLevel"].clone())
                    .map_err(|e| invalid(format!("Unsupported native thinking level: {e}")))?,
            );
        }
        let message = &record["message"];
        if !message.is_object() {
            continue;
        }
        let role = required(message, "role")?;
        if role == "assistant"
            && let Some(model) = message["model"].as_str()
        {
            history.model = Some(if pi {
                format!("{}/{}", required(message, "provider")?, model)
            } else {
                model.into()
            });
        }
        let content = if role == "toolResult" {
            let mut block = serde_json::json!({"type":"tool_result", "tool_use_id": required(message, "toolCallId")?,
                "content":message["content"], "is_error":message["isError"]});
            if let Some(details) = message.get("details") {
                block["details"] = details.clone();
            }
            Value::Array(vec![block])
        } else {
            message["content"].clone()
        };
        if !matches!(role, "user" | "assistant" | "toolResult") {
            continue;
        }
        if !content.is_string() && !content.is_array() {
            return Err(invalid("Native message content is missing"));
        }
        history.messages.push(NativeMessage {
            id: required(record, id_key)?.into(),
            role: role.into(),
            content,
            timestamp: message
                .get("timestamp")
                .cloned()
                .unwrap_or_else(|| record["timestamp"].clone()),
            aborted: matches!(message["stopReason"].as_str(), Some("aborted" | "error")),
        });
    }
    Ok(history)
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, HarnessError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("Native history is missing {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_arguments_are_literals() {
        let config = ChatConfig {
            harness: HarnessId::Pi,
            model: Some("provider/model;$(touch /tmp/nope)".into()),
            reasoning: None,
            model_options: Default::default(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        };
        let session = "/tmp/a b;$(touch nope).jsonl";
        let args = resume_args(&config, session).unwrap();
        assert_eq!(
            args,
            [
                "--session",
                session,
                "--model",
                "provider/model;$(touch /tmp/nope)"
            ]
        );
        assert!(!args.iter().any(|a| a == "-c" || a == "--continue"));
    }
    #[test]
    fn claude_resume_uses_the_session_uuid_and_keeps_model_and_effort() {
        let session = "ad66b593-8b4e-4308-82b6-8d4a597e202c";
        let config = ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-sonnet-4-5".into()),
            reasoning: Some(ReasoningLevel::High),
            model_options: Default::default(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        };
        let args = resume_args(&config, session).unwrap();
        assert_eq!(
            args,
            [
                "--resume=ad66b593-8b4e-4308-82b6-8d4a597e202c",
                "--model",
                "claude-sonnet-4-5",
                "--effort",
                "high"
            ]
        );
        assert!(!args.iter().any(|a| a == "-c" || a == "--continue" || a.contains("bypass")));
        // Claude cannot preserve Pi's off/minimal thinking levels.
        let mut off = config.clone();
        off.reasoning = Some(ReasoningLevel::Off);
        assert!(resume_args(&off, session).is_err());
        // The resumed history must belong to the resumed UUID.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{session}.jsonl"));
        std::fs::write(&path, format!(
            "{{\"type\":\"user\",\"uuid\":\"a\",\"parentUuid\":null,\"sessionId\":\"{session}\",\
             \"cwd\":\"/tmp\",\"timestamp\":1,\
             \"message\":{{\"role\":\"user\",\"content\":\"hello\",\"timestamp\":1}}}}\n"
        ))
        .unwrap();
        let command = ResumeCommand {
            program: "/bin/true".into(),
            args,
            env: vec![],
            history_path: path,
            session_id: session.into(),
            cwd: "/tmp".into(),
            harness: HarnessId::ClaudeCode,
        };
        let history = command.read_history().unwrap();
        assert_eq!(history.identity, session);
        assert_eq!(history.messages.len(), 1);
    }
    #[test]
    fn pi_walks_active_branch_and_rejects_torn_history() {
        let text = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"native\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
            "{\"type\":\"message\",\"id\":\"b\",\"parentId\":\"a\",\"message\":{\"role\":\"assistant\",\"content\":\"old branch\"}}\n",
            "{\"type\":\"message\",\"id\":\"c\",\"parentId\":\"a\",\"message\":{\"role\":\"assistant\",\"content\":\"new branch\"}}\n"
        );
        let h = parse_history(HarnessId::Pi, text).unwrap();
        assert_eq!(
            h.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert!(parse_history(HarnessId::Pi, &format!("{text}{{")).is_err());
    }
    #[test]
    fn claude_ignores_sidechains_and_keeps_tool_results() {
        let text = concat!(
            "{\"type\":\"user\",\"uuid\":\"a\",\"parentUuid\":null,\"sessionId\":\"s\",\"cwd\":\"/tmp\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
            "{\"type\":\"user\",\"uuid\":\"b\",\"parentUuid\":\"a\",\"sessionId\":\"s\",\"cwd\":\"/tmp\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t\",\"content\":\"done\"}]}}\n",
            "{\"uuid\":\"side\",\"isSidechain\":true,\"message\":{\"role\":\"assistant\",\"content\":\"hidden\"}}\n"
        );
        let h = parse_history(HarnessId::ClaudeCode, text).unwrap();
        assert_eq!(h.messages.len(), 2);
        assert_eq!(h.messages[1].content[0]["tool_use_id"], "t");
    }
}

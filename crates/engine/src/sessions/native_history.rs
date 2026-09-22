//! Read only the history of the provider session explicitly selected by the host.
//! A byte checkpoint separates already-rendered chat turns from native CLI turns.
use crate::EngineError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use zeron_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, fold_event_into_parts,
};
use zeron_proto::{AgentEvent, HarnessId, ToolCall};

const MAX_HISTORY: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Checkpoint {
    pub path: PathBuf,
    pub offset: usize,
    pub digest: String,
}

pub(super) fn error(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

fn read(path: &Path) -> Result<Vec<u8>, EngineError> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_HISTORY + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HISTORY {
        return Err(error("Provider history is too large to switch safely"));
    }
    if !bytes.ends_with(b"\n") {
        return Err(error("Provider history is still being written; try again"));
    }
    Ok(bytes)
}

fn records(bytes: &[u8]) -> Result<Vec<(usize, Value)>, EngineError> {
    let mut offset = 0;
    bytes
        .split_inclusive(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let start = offset;
            offset += line.len();
            serde_json::from_slice(line)
                .map(|value| (start, value))
                .map_err(|_| error("Provider history contains an unreadable record"))
        })
        .collect()
}

/// File names locate candidates; contents must prove both identity and cwd.
pub(super) fn checkpoint(
    harness: HarnessId,
    id: &str,
    cwd: &str,
) -> Result<Checkpoint, EngineError> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(error("The provider session has no valid resume ID"));
    }
    let home = crate::repos::home_dir();
    let root = match harness {
        HarnessId::Codex => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("sessions"),
        HarnessId::ClaudeCode => std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"))
            .join("projects"),
        _ => {
            return Err(error(
                "Native CLI switching is not supported by this provider",
            ));
        }
    };
    let mut pending = vec![(root, 0)];
    let mut found = Vec::new();
    let mut visited = 0;
    while let Some((dir, depth)) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            visited += 1;
            if visited > 50_000 {
                return Err(error("Provider history lookup exceeded its limit"));
            }
            let kind = entry.file_type()?;
            if kind.is_dir() && depth < 4 {
                pending.push((entry.path(), depth + 1));
            }
            if kind.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(&format!("{id}.jsonl"))
            {
                found.push(entry.path());
            }
        }
    }
    if found.len() != 1 {
        return Err(error(
            "The exact provider session history could not be located",
        ));
    }
    checkpoint_at(found.remove(0), harness, id, cwd)
}

pub(super) fn checkpoint_at(
    path: PathBuf,
    harness: HarnessId,
    id: &str,
    cwd: &str,
) -> Result<Checkpoint, EngineError> {
    let bytes = read(&path)?;
    validate(&records(&bytes)?, harness, id, cwd)?;
    Ok(Checkpoint {
        path,
        offset: bytes.len(),
        digest: format!("{:x}", Sha256::digest(&bytes)),
    })
}

fn validate(
    rows: &[(usize, Value)],
    harness: HarnessId,
    id: &str,
    cwd: &str,
) -> Result<(), EngineError> {
    let expected = fs::canonicalize(cwd)?;
    let valid = rows.iter().any(|(_, row)| {
        let (session, dir) = match harness {
            HarnessId::Codex if row["type"] == "session_meta" => (
                row["payload"]["id"].as_str(),
                row["payload"]["cwd"].as_str(),
            ),
            HarnessId::ClaudeCode => (row["sessionId"].as_str(), row["cwd"].as_str()),
            _ => (None, None),
        };
        session == Some(id)
            && dir.and_then(|dir| fs::canonicalize(dir).ok()).as_ref() == Some(&expected)
    });
    if !valid {
        return Err(error(
            "Provider history does not match this session and working directory",
        ));
    }
    Ok(())
}

impl Checkpoint {
    pub fn tail(&self) -> Result<Vec<(usize, Value)>, EngineError> {
        let bytes = read(&self.path)?;
        if bytes.len() < self.offset
            || format!("{:x}", Sha256::digest(&bytes[..self.offset])) != self.digest
        {
            return Err(error(
                "Provider history changed before the CLI checkpoint; automatic import is unavailable",
            ));
        }
        Ok(records(&bytes[self.offset..])?
            .into_iter()
            .map(|(offset, row)| (offset + self.offset, row))
            .collect())
    }
}

/// Codex's persisted lifecycle records, not terminal screen text, settle turns.
pub(super) fn codex_idle(rows: &[(usize, Value)]) -> Option<bool> {
    rows.iter().rev().find_map(|(_, row)| {
        if row["type"] != "event_msg" {
            return None;
        }
        match row["payload"]["type"].as_str()? {
            "task_started" | "user_message" => Some(false),
            "task_complete" | "turn_aborted" => Some(true),
            _ => None,
        }
    })
}

fn text_content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    value
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str().or_else(|| block["content"].as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Imports native records with deterministic IDs, making crash retries idempotent.
/// Tool results are folded into their original tool part, not echoed as user text.
pub(super) fn messages(
    harness: HarnessId,
    session: &str,
    device: &str,
    rows: &[(usize, Value)],
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    let mut result: Vec<SessionMessageEntry> = Vec::new();
    let mut claude_records = std::collections::HashSet::new();
    for (offset, row) in rows {
        let timestamp = row["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis())
            .unwrap_or(0);
        let (role, content) = match harness {
            HarnessId::ClaudeCode if !row["isSidechain"].as_bool().unwrap_or(false) => {
                if row["sessionId"].as_str().is_some_and(|id| id != session) {
                    return Err(error("CLI history changed session identity"));
                }
                if let Some(id) = row["uuid"].as_str()
                    && !claude_records.insert(id)
                {
                    continue;
                }
                match row["type"].as_str() {
                    Some("user") => (MessageRole::User, row["message"]["content"].clone()),
                    Some("assistant") => {
                        (MessageRole::Assistant, row["message"]["content"].clone())
                    }
                    _ => continue,
                }
            }
            HarnessId::Codex if row["type"] == "session_meta" => {
                if row["payload"]["id"].as_str() != Some(session) {
                    return Err(error("CLI history changed session identity"));
                }
                continue;
            }
            HarnessId::Codex
                if row["type"] == "event_msg" && row["payload"]["type"] == "user_message" =>
            {
                // response_item user messages also contain injected environment,
                // instructions, and repeated context. Only this event is a
                // confirmed user submission in Codex's persisted transcript.
                (MessageRole::User, row["payload"]["message"].clone())
            }
            HarnessId::Codex if row["type"] == "response_item" => {
                let item = &row["payload"];
                match item["type"].as_str() {
                    Some("message") => match item["role"].as_str() {
                        Some("user") => continue,
                        Some("assistant") => (MessageRole::Assistant, item["content"].clone()),
                        _ => continue,
                    },
                    Some("function_call" | "custom_tool_call") => (
                        MessageRole::Assistant,
                        serde_json::json!([{
                            "type":"tool_use", "id":item["call_id"], "name":item["name"],
                            "input": item.get("arguments").or_else(|| item.get("input"))
                        }]),
                    ),
                    Some("function_call_output" | "custom_tool_call_output") => (
                        MessageRole::Assistant,
                        serde_json::json!([{
                            "type":"tool_result", "tool_use_id":item["call_id"], "content":item["output"]
                        }]),
                    ),
                    _ => continue,
                }
            }
            _ => continue,
        };
        let mut parts = Vec::new();
        if let Some(text) = content.as_str() {
            parts.push(MessagePart::Text {
                id: format!("cli-{offset}-text"),
                text: text.to_owned(),
            });
        } else if let Some(blocks) = content.as_array() {
            for (index, block) in blocks.iter().enumerate() {
                match block["type"].as_str() {
                    Some("text" | "input_text" | "output_text") => {
                        if let Some(text) = block["text"].as_str() {
                            parts.push(MessagePart::Text {
                                id: format!("cli-{offset}-{index}"),
                                text: text.to_owned(),
                            });
                        }
                    }
                    Some("thinking") => {
                        if let Some(text) = block["thinking"].as_str() {
                            parts.push(MessagePart::Reasoning {
                                id: format!("cli-{offset}-{index}"),
                                text: text.to_owned(),
                            });
                        }
                    }
                    Some("tool_use") => fold_event_into_parts(
                        &mut parts,
                        &AgentEvent::ToolCall {
                            id: block["id"]
                                .as_str()
                                .ok_or_else(|| error("CLI tool call has no ID"))?
                                .into(),
                            call: ToolCall::Unknown {
                                name: block["name"].as_str().unwrap_or("CLI tool").into(),
                                input: block.get("input").cloned(),
                            },
                        },
                    ),
                    Some("tool_result") => {
                        let id = block["tool_use_id"]
                            .as_str()
                            .ok_or_else(|| error("CLI tool result has no ID"))?;
                        if let Some(entry) = result.iter_mut().rev().find(|entry| entry.parts.iter().any(|part| matches!(part, MessagePart::Tool { id: part_id, .. } if part_id == id))) {
                            fold_event_into_parts(&mut entry.parts, &AgentEvent::ToolResult { id: id.into(), is_error: block["is_error"].as_bool().unwrap_or(false), output: Some(text_content(&block["content"])), diff: None });
                        }
                    }
                    // Native attachments remain in provider history. Make their
                    // presence explicit instead of silently losing the record.
                    Some(kind) => parts.push(MessagePart::Text {
                        id: format!("cli-{offset}-{index}"),
                        text: format!("[CLI {kind}; retained in provider history]"),
                    }),
                    None => {
                        return Err(error("CLI message contains an unsupported content record"));
                    }
                }
            }
        }
        if !parts.is_empty() {
            result.push(SessionMessageEntry {
                id: format!("native:{session}:{offset}"),
                role,
                parts,
                created_at: timestamp,
                device_id: device.into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const ID: &str = "11111111-1111-4111-8111-111111111111";
    fn append(path: &Path, row: Value) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{row}").unwrap();
    }
    fn fixture() -> (tempfile::TempDir, Checkpoint) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":ID,"cwd":dir.path()}})
            ),
        )
        .unwrap();
        let checkpoint =
            checkpoint_at(path, HarnessId::Codex, ID, dir.path().to_str().unwrap()).unwrap();
        (dir, checkpoint)
    }

    #[test]
    fn checkpoint_rejects_foreign_cwd_id_torn_records_and_rewritten_prefix() {
        let (dir, checkpoint) = fixture();
        let other = tempfile::tempdir().unwrap();
        assert!(
            checkpoint_at(
                checkpoint.path.clone(),
                HarnessId::Codex,
                "wrong",
                dir.path().to_str().unwrap()
            )
            .is_err()
        );
        assert!(
            checkpoint_at(
                checkpoint.path.clone(),
                HarnessId::Codex,
                ID,
                other.path().to_str().unwrap()
            )
            .is_err()
        );
        let original = fs::read(&checkpoint.path).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&checkpoint.path)
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        assert!(checkpoint.tail().is_err());
        fs::write(&checkpoint.path, &original).unwrap();
        append(
            &checkpoint.path,
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        );
        assert_eq!(checkpoint.tail().unwrap().len(), 1);
        fs::write(&checkpoint.path, b"{}\n").unwrap();
        assert!(checkpoint.tail().is_err());
        let mut rewritten = original;
        rewritten[2] = b'x';
        fs::write(&checkpoint.path, rewritten).unwrap();
        assert!(checkpoint.tail().is_err());
    }

    #[test]
    fn codex_imports_only_new_user_turns_and_folds_tool_results() {
        let (_dir, checkpoint) = fixture();
        for row in [
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"injected instructions"}]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"real prompt"}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"tool-1","name":"shell","arguments":"pwd"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"tool-1","output":"/workspace"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        ] {
            append(&checkpoint.path, row);
        }
        let rows = checkpoint.tail().unwrap();
        assert_eq!(codex_idle(&rows), Some(true));
        assert_eq!(codex_idle(&rows[..2]), Some(false));
        let entries = messages(HarnessId::Codex, ID, "device", &rows).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, MessageRole::User);
        assert!(
            matches!(&entries[0].parts[0], MessagePart::Text { text, .. } if text == "real prompt")
        );
        assert!(matches!(
            &entries[1].parts[0],
            MessagePart::Tool {
                resolved: true,
                output: None,
                ..
            }
        ));
        // The existing transcript policy retains full outputs only in local
        // provider history. Import must preserve resolution without copying
        // tool payloads into the synchronized document.
        assert!(
            !serde_json::to_string(&entries)
                .unwrap()
                .contains("injected instructions")
        );
        let again = messages(HarnessId::Codex, ID, "device", &rows).unwrap();
        assert_eq!(
            serde_json::to_value(entries).unwrap(),
            serde_json::to_value(again).unwrap()
        );
    }

    #[test]
    fn claude_skips_sidechains_and_duplicate_records_and_rejects_changed_identity() {
        let rows = vec![
            (
                0,
                json!({"type":"user","sessionId":ID,"uuid":"u1","message":{"content":"hello"}}),
            ),
            (
                1,
                json!({"type":"user","sessionId":ID,"uuid":"u1","message":{"content":"hello"}}),
            ),
            (
                2,
                json!({"type":"assistant","sessionId":ID,"uuid":"a1","isSidechain":true,"message":{"content":"child"}}),
            ),
            (
                3,
                json!({"type":"assistant","sessionId":ID,"uuid":"a2","message":{"content":[{"type":"text","text":"reply"}]}}),
            ),
        ];
        let entries = messages(HarnessId::ClaudeCode, ID, "device", &rows).unwrap();
        assert_eq!(entries.len(), 2);
        let changed = vec![(
            4,
            json!({"type":"user","sessionId":"other","message":{"content":"oops"}}),
        )];
        assert!(messages(HarnessId::ClaudeCode, ID, "device", &changed).is_err());
    }
}

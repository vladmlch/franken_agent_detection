use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};

use super::ClaudeCodeConnector;
use super::generated_title;
use super::retained_content;
use crate::connectors::jsonl::JsonlLines;
use crate::connectors::{
    extract_invocations_from_content_blocks, flatten_content, parse_timestamp,
    unwrap_skill_invocations,
};
use crate::types::NormalizedMessage;

impl ClaudeCodeConnector {
    /// A prompt the user typed while the agent was mid-turn is recorded as
    /// `{"type":"attachment","attachment":{"type":"queued_command",
    /// "commandMode":"prompt","prompt":..,"origin":{"kind":"human"}}}` rather
    /// than as a `type:"user"` entry, so the user/assistant filter would drop
    /// it and the text would never be searchable (cass GH #500).
    ///
    /// Only prompt-mode entries typed by a person are normalized:
    /// `origin.kind == "human"`, or no `origin` at all (older Claude Code
    /// builds), which is kept but marked as unknown authorship. Relayed
    /// (`peer`) and any other explicit origin, meta entries, and
    /// machine-generated `task-notification` entries are skipped. The sibling
    /// `queue-operation` records are not indexed either: they repeat the text
    /// of every queued item, including ones later saved as normal user
    /// entries, so indexing them would double turns.
    pub(super) fn queued_command_message(raw: &Value) -> Option<NormalizedMessage> {
        let attachment = raw.get("attachment")?;
        if attachment.get("type").and_then(Value::as_str) != Some("queued_command")
            || attachment.get("commandMode").and_then(Value::as_str) != Some("prompt")
        {
            return None;
        }
        let is_meta = |v: &Value| v.get("isMeta").and_then(Value::as_bool) == Some(true);
        if is_meta(raw) || is_meta(attachment) {
            return None;
        }
        let authorship = match attachment.get("origin") {
            None | Some(Value::Null) => "unknown",
            Some(origin) if origin.get("kind").and_then(Value::as_str) == Some("human") => "human",
            Some(_) => return None,
        };

        let content = attachment
            .get("prompt")
            .map(flatten_content)
            .unwrap_or_default();
        if content.trim().is_empty() {
            return None;
        }

        let created = raw
            .get("timestamp")
            .and_then(parse_timestamp)
            .or_else(|| attachment.get("timestamp").and_then(parse_timestamp));

        let mut provenance = Map::new();
        provenance.insert(
            "command_mode".to_string(),
            Value::String("prompt".to_string()),
        );
        provenance.insert(
            "authorship".to_string(),
            Value::String(authorship.to_string()),
        );
        if let Some(source_uuid) = attachment
            .get("source_uuid")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            provenance.insert(
                "source_uuid".to_string(),
                Value::String(source_uuid.to_string()),
            );
        }

        let mut extra = raw.clone();
        if let Some(obj) = extra.as_object_mut() {
            let cass = obj
                .entry("cass".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !cass.is_object() {
                *cass = Value::Object(Map::new());
            }
            if let Some(cass) = cass.as_object_mut() {
                cass.insert("queued_command".to_string(), Value::Object(provenance));
            }
        }

        Some(NormalizedMessage {
            idx: 0,
            role: "user".to_string(),
            author: None,
            created_at: created,
            content,
            extra,
            invocations: Vec::new(),
            snippets: Vec::new(),
        })
    }
}

/// Widen a conversation's `[started_at, ended_at]` bounds to cover `created`.
fn widen_time_bounds(
    started_at: &mut Option<i64>,
    ended_at: &mut Option<i64>,
    created: Option<i64>,
) {
    if let Some(ts) = created {
        *started_at = Some(started_at.map_or(ts, |curr| curr.min(ts)));
        *ended_at = Some(ended_at.map_or(ts, |curr| curr.max(ts)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeFlavor {
    Claude,
    Qoder,
}

pub struct ParsedClaudeFile {
    pub messages: Vec<NormalizedMessage>,
    pub message_uuids: Vec<Option<String>>,
    pub records_by_uuid: Vec<(String, Option<String>)>,
    pub record_timestamps: HashMap<String, i64>,
    pub active_leaves: Vec<String>,
    pub title: Option<String>,
    pub json_title: Option<String>,
    pub workspace: Option<PathBuf>,
    pub model: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub metadata: Value,
}

#[allow(clippy::too_many_lines)]
pub fn parse_claude_records<R: BufRead>(
    reader: R,
    path: &Path,
    flavor: ClaudeFlavor,
) -> Result<Option<ParsedClaudeFile>> {
    let mut messages = Vec::new();
    let mut message_uuids = Vec::new();
    let mut records_by_uuid = Vec::new();
    let mut record_timestamps = HashMap::new();
    let mut active_leaves = Vec::new();
    let mut started_at: Option<i64> = None;
    let mut ended_at: Option<i64> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut workspace_dir_fallback: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut json_title: Option<String> = None;
    let mut message_model: Option<String> = None;
    let mut runtime_config_model: Option<String> = None;

    let is_subagent_transcript = path
        .components()
        .any(|c| c.as_os_str().to_str() == Some("subagents"));
    let mut parent_session_id: Option<String> = None;
    if is_subagent_transcript {
        let components: Vec<_> = path.components().collect();
        if let Some(pos) = components
            .iter()
            .position(|c| c.as_os_str().to_str() == Some("subagents"))
            && pos > 0
        {
            parent_session_id = components[pos - 1].as_os_str().to_str().map(String::from);
        }
    }

    for line_res in JsonlLines::new(reader) {
        let line = match line_res {
            Ok(line) => line,
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        if line.trim().is_empty() {
            continue;
        }
        // Strip a UTF-8 BOM so the first record (often
        // session_meta or the first prompt) is not silently lost.
        let line = line.trim_start_matches('\u{feff}');
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if workspace.is_none() {
            workspace = match flavor {
                ClaudeFlavor::Claude => val.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from),
                ClaudeFlavor::Qoder => val
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from),
            };
        }
        if session_id.is_none() {
            session_id = val
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        if git_branch.is_none() {
            git_branch = val
                .get("gitBranch")
                .and_then(|v| v.as_str())
                .map(String::from);
        }

        let record_uuid = val.get("uuid").and_then(Value::as_str).map(String::from);
        let parent_uuid = val
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(String::from);
        let created = val.get("timestamp").and_then(parse_timestamp);

        if let Some(ref uuid) = record_uuid {
            records_by_uuid.push((uuid.clone(), parent_uuid));
            if let Some(ts) = created {
                record_timestamps.insert(uuid.clone(), ts);
            }
        }

        let entry_type = val.get("type").and_then(|v| v.as_str());
        if flavor == ClaudeFlavor::Qoder {
            if entry_type == Some("active-leaf") {
                if let Some(leaf) = val
                    .get("leafUuid")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    active_leaves.push(leaf.to_string());
                }
                continue;
            }
            if entry_type == Some("workspace-directories") {
                if workspace_dir_fallback.is_none() {
                    workspace_dir_fallback = val
                        .get("directories")
                        .and_then(Value::as_array)
                        .and_then(|dirs| {
                            dirs.iter()
                                .filter_map(Value::as_str)
                                .map(str::trim)
                                .find(|s| !s.is_empty())
                        })
                        .map(PathBuf::from);
                }
                continue;
            }
            if entry_type == Some("runtime-config") {
                if let Some(model) = val
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    runtime_config_model = Some(model.to_string());
                }
                continue;
            }
        }
        // Claude's own generated title beats first-line
        // truncation; the entry is otherwise skipped as a
        // non-conversation sidecar type.
        if entry_type == Some("ai-title") {
            if let Some(title) = generated_title::generated_title(&val) {
                json_title = Some(title);
            }
            continue;
        }
        if entry_type == Some("attachment") {
            if let Some(message) = ClaudeCodeConnector::queued_command_message(&val) {
                widen_time_bounds(&mut started_at, &mut ended_at, message.created_at);
                messages.push(message);
                message_uuids.push(record_uuid);
            }
            continue;
        }
        let role_hint = val
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            .or_else(|| val.get("role").and_then(|v| v.as_str()));
        let is_user_assistant = matches!(entry_type, Some("user" | "assistant"))
            || (entry_type == Some("message") && matches!(role_hint, Some("user" | "assistant")));
        if !is_user_assistant {
            continue;
        }

        started_at = match (started_at, created) {
            (Some(curr), Some(ts)) => Some(curr.min(ts)),
            (None, Some(ts)) => Some(ts),
            (other, None) => other,
        };
        ended_at = match (ended_at, created) {
            (Some(curr), Some(ts)) => Some(curr.max(ts)),
            (None, Some(ts)) => Some(ts),
            (Some(curr), None) => Some(curr),
            (None, None) => None,
        };

        let role = role_hint.or(entry_type).unwrap_or("agent");
        let content_val = val
            .get("message")
            .and_then(|m| m.get("content"))
            .or_else(|| val.get("content"));
        let content_str = content_val
            .map(retained_content::flatten_claude_content)
            .unwrap_or_default();

        // Tool results ride in user entries as
        // content:[{type:"tool_result", tool_use_id, content}].
        // flatten_content ignores those blocks, so without this
        // pass every result entry is dropped wholesale
        // (~30-40% of real entries per transcript), leaving
        // invocations with call_ids but no outputs anywhere.
        if let Some(blocks) = content_val.and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let result_text = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => continue,
                };
                if result_text.trim().is_empty() {
                    continue;
                }
                let mut tool_extra = Map::new();
                tool_extra.insert(
                    "source".to_string(),
                    Value::String("tool_result".to_string()),
                );
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    tool_extra.insert("tool_use_id".to_string(), Value::String(id.to_string()));
                }
                // Failed tool calls are common and downstream
                // analytics split on them; pass the flag through
                // when Claude recorded one.
                if let Some(is_error) = block.get("is_error").and_then(Value::as_bool) {
                    tool_extra.insert("is_error".to_string(), Value::from(is_error));
                }
                messages.push(NormalizedMessage {
                    idx: 0,
                    role: "tool".to_string(),
                    author: None,
                    created_at: created,
                    content: result_text,
                    extra: Value::Object(tool_extra),
                    invocations: Vec::new(),
                    snippets: Vec::new(),
                });
                message_uuids.push(record_uuid.clone());
            }
        }

        if content_str.trim().is_empty() {
            continue;
        }

        let author = val
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
            .map(String::from);
        if message_model.is_none() {
            message_model = author
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(String::from);
        }
        let mut invocations =
            content_val.map_or_else(Vec::new, extract_invocations_from_content_blocks);
        unwrap_skill_invocations(&mut invocations);

        messages.push(NormalizedMessage {
            idx: 0,
            role: role.to_string(),
            author,
            created_at: created,
            content: content_str,
            extra: val,
            invocations,
            snippets: Vec::new(),
        });
        message_uuids.push(record_uuid);
    }

    if flavor == ClaudeFlavor::Qoder {
        if workspace.is_none() {
            workspace = workspace_dir_fallback;
        }
        if let Some(ref rt_model) = runtime_config_model {
            for message in &mut messages {
                if message.role == "assistant" && message.author.is_none() {
                    message.author = Some(rt_model.clone());
                }
            }
        }
    }
    let resolved_model = message_model.or_else(|| {
        (flavor == ClaudeFlavor::Qoder)
            .then_some(runtime_config_model)
            .flatten()
    });

    crate::types::reindex_messages(&mut messages);
    if messages.is_empty() {
        return Ok(None);
    }

    let title = json_title.clone().or_else(|| {
        messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| {
                m.content
                    .lines()
                    .next()
                    .unwrap_or(&m.content)
                    .chars()
                    .take(100)
                    .collect::<String>()
            })
            .or_else(|| {
                workspace
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
    });

    let metadata = serde_json::json!({
        "source": "claude_code",
        "sessionId": session_id,
        "cliSessionId": Value::Null,
        "gitBranch": git_branch,
        "permissionMode": Value::Null,
        "bodyAvailable": true,
        "sidechain": is_subagent_transcript,
        "parentSessionId": parent_session_id
    });

    Ok(Some(ParsedClaudeFile {
        messages,
        message_uuids,
        records_by_uuid,
        record_timestamps,
        active_leaves,
        title,
        json_title,
        workspace,
        model: resolved_model,
        started_at,
        ended_at,
        metadata,
    }))
}

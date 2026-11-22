use serde_json::Value;

use super::CodexConnector;
use super::discovery::RolloutFormat;
use super::segments;
use crate::connectors::utils::is_injected_context_message;
use crate::types::NormalizedMessage;

/// The thread's first real user prompt, or its first message when every prompt
/// is injected context.
pub(super) fn conversation_title(messages: &[NormalizedMessage]) -> Option<String> {
    messages
        .iter()
        .find(|m| m.role == "user" && !is_injected_context_message(&m.content))
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
            messages
                .first()
                .and_then(|m| m.content.lines().next())
                .map(|s| s.chars().take(100).collect())
        })
}

/// Subagent evidence lifted from a rollout's `session_meta` header, mirroring
/// the native classifier (`viewer/providers/codex/discovery/sessions.py`
/// `_classify_session_meta_payload`). A header always decides the verdict;
/// `parent_id` and the provenance fields are set only when the source proves
/// them. Forks (`forked_from_id` / `history_base`) are deliberately ignored.
#[derive(Clone, Debug, Default)]
pub(super) struct SubagentEvidence {
    pub(super) is_subagent: bool,
    pub(super) parent_id: Option<String>,
    pub(super) role: Option<String>,
    pub(super) nickname: Option<String>,
    pub(super) depth: Option<i64>,
    pub(super) subagent_type: Option<String>,
}

/// JSON truthiness matching the native `if src.get("subagent")` guard.
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn non_empty_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn is_ascii_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Extract a canonical lowercase 36-char UUID from `text`, matching native
/// `extract_uuid_from_string`.
fn extract_uuid_from_string(text: &str) -> Option<String> {
    text.as_bytes()
        .windows(36)
        .filter_map(|w| std::str::from_utf8(w).ok())
        .find(|s| is_ascii_uuid(s))
        .map(str::to_ascii_lowercase)
}

fn canonical_codex_parent_id(raw: &str) -> String {
    extract_uuid_from_string(raw).unwrap_or_else(|| raw.to_string())
}

pub(super) fn classify_session_meta(payload: &Value) -> SubagentEvidence {
    let mut ev = SubagentEvidence::default();
    let parent_top =
        non_empty_str(payload, "parent_thread_id").or_else(|| non_empty_str(payload, "parent_id"));
    if let Some(parent) = parent_top {
        ev.is_subagent = true;
        ev.parent_id = Some(canonical_codex_parent_id(parent));
    }
    if payload.get("thread_source").and_then(Value::as_str) == Some("subagent") {
        ev.is_subagent = true;
    }
    ev.role = non_empty_str(payload, "agent_role").map(str::to_string);
    ev.nickname = non_empty_str(payload, "agent_nickname").map(str::to_string);

    match payload.get("source") {
        Some(Value::String(s)) if s == "subagent" => ev.is_subagent = true,
        Some(src @ Value::Object(_)) => {
            let Some(sub) = src.get("subagent").filter(|v| json_truthy(v)) else {
                return ev;
            };
            ev.is_subagent = true;
            match sub {
                Value::Object(sub_obj) => {
                    if let Some(ts) = sub_obj.get("thread_spawn").and_then(Value::as_object) {
                        if ev.parent_id.is_none() {
                            ev.parent_id = ts
                                .get("parent_thread_id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    ts.get("parent_id")
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.is_empty())
                                })
                                .map(canonical_codex_parent_id);
                        }
                        ev.role = Some(
                            ts.get("agent_role")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .or_else(|| non_empty_str(payload, "agent_role"))
                                .unwrap_or("worker")
                                .to_string(),
                        );
                        ev.nickname = ts
                            .get("agent_nickname")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .or_else(|| non_empty_str(payload, "agent_nickname"))
                            .map(str::to_string);
                        ev.depth = Some(ts.get("depth").and_then(Value::as_i64).unwrap_or(1));
                    } else if let Some(other) = sub_obj.get("other") {
                        let other_name = match other {
                            Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        ev.role = Some(if other_name == "guardian" {
                            "Guardian / Auto-Review".to_string()
                        } else {
                            other_name.clone()
                        });
                        ev.nickname = Some(other_name.clone());
                        ev.subagent_type = Some(other_name);
                    }
                }
                Value::String(s) => {
                    ev.role = Some(s.clone());
                    ev.subagent_type = Some(s.clone());
                }
                _ => {}
            }
        }
        _ => {}
    }
    ev
}

/// A single-segment thread keeps today's metadata exactly. A merged thread also
/// records every segment path and the external id each segment used to own, so
/// a host can retire the per-segment rows those files produced on their own.
pub(super) fn thread_metadata(
    thread: &segments::Thread,
    format: RolloutFormat,
    subagent: Option<&SubagentEvidence>,
) -> Value {
    let mut metadata = serde_json::json!({
        "source": if format == RolloutFormat::Json { "rollout_json" } else { "rollout" }
    });
    if let Some(ev) = subagent
        && let Some(object) = metadata.as_object_mut()
    {
        object.insert("is_subagent".to_string(), Value::Bool(ev.is_subagent));
        if ev.is_subagent {
            if let Some(parent) = &ev.parent_id {
                object.insert("parent_id".to_string(), Value::String(parent.clone()));
            }
            if let Some(role) = &ev.role {
                object.insert("subagent_role".to_string(), Value::String(role.clone()));
            }
            if let Some(nickname) = &ev.nickname {
                object.insert(
                    "subagent_nickname".to_string(),
                    Value::String(nickname.clone()),
                );
            }
            if let Some(depth) = ev.depth {
                object.insert("subagent_depth".to_string(), serde_json::json!(depth));
            }
            if let Some(kind) = &ev.subagent_type {
                object.insert("subagent_type".to_string(), Value::String(kind.clone()));
            }
        }
    }
    if thread.segments.len() < 2 {
        return metadata;
    }
    let Some(name) = segments::SegmentName::of(&thread.segments[0].path) else {
        return metadata;
    };
    let entries: Vec<Value> = thread
        .segments
        .iter()
        .map(|segment| {
            let continuation =
                segments::SegmentName::of(&segment.path).is_some_and(|name| name.continuation);
            serde_json::json!({
                "path": segment.path,
                "external_id": CodexConnector::segment_external_id(
                    &segment.path,
                    &segment.sessions_dir,
                ),
                "kind": if continuation { "continuation" } else { "original" },
            })
        })
        .collect();
    if let Some(object) = metadata.as_object_mut() {
        object.insert("thread_id".to_string(), Value::String(name.thread_id));
        object.insert(
            "segment_count".to_string(),
            serde_json::json!(thread.segments.len()),
        );
        object.insert("segments".to_string(), Value::Array(entries));
    }
    metadata
}

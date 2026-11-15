//! Read-only parser for Devin IDE `acp-messages/<uuid>.db` SQLite stores.
//!
//! Each file stores one session's `meta(key, value)` and
//! `messages(position, kind, payload)` rows. Rows of `kind = 'subagent'` may
//! carry an `agentId` and inline `childMessages` array that map to separate
//! child [`NormalizedConversation`] entries linked back to `<stem>` via
//! `metadata.parent_id`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankensqlite::compat::RowExt;
use frankensqlite::params;
use serde_json::{Map, Value, json};

use super::super::sqlite_sync::{Connection, ConnectionExt, open_read_only_with_snapshot_fallback};
use super::super::utils::{file_modified_since, parse_timestamp, unwrap_skill_invocations};
use super::{assign_message_indices, bound_tool_arguments, first_user_line_title, sidecar_path};
use crate::types::{NormalizedConversation, NormalizedInvocation, NormalizedMessage};

const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct MetaInfo {
    title: Option<String>,
    model: Option<String>,
}

struct SubagentCandidate {
    position: i64,
    completed: bool,
    conv: NormalizedConversation,
}

/// Extract main and `kind = 'subagent'` conversations from one Devin IDE
/// `acp-messages/<uuid>.db` store.
///
/// Opens the database strictly read-only. Returns `Ok(vec![])` for empty
/// stores, stores missing the `messages` table, or stores older than
/// `since_ts`.
pub fn extract_acp_store(
    db_path: &Path,
    since_ts: Option<i64>,
) -> Result<Vec<NormalizedConversation>> {
    let Some(stem) = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return Ok(Vec::new());
    };

    if super::super::sqlite_sync::store_is_provably_empty(db_path) {
        return Ok(Vec::new());
    }

    let conn = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
        .with_context(|| format!("failed to open Devin ACP db: {}", db_path.display()))?;
    conn.execute("PRAGMA busy_timeout = 5000;")
        .context("failed to set busy_timeout on Devin ACP db")?;

    if !has_table(&conn, "messages")? {
        return Ok(Vec::new());
    }

    let meta_info = read_meta_info(&conn)?;
    let rows = read_message_rows(&conn)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut main_messages = Vec::new();
    let mut main_skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut main_workspace: Option<PathBuf> = None;
    let mut main_min_ts: Option<i64> = None;
    let mut main_max_ts: Option<i64> = None;
    let mut pos_0_is_user = false;
    let mut subagents: HashMap<String, SubagentCandidate> = HashMap::new();
    let mut overall_max_ts: Option<i64> = None;

    for (position, kind, payload_raw) in rows {
        let Ok(payload) = serde_json::from_str::<Value>(&payload_raw) else {
            continue;
        };
        if !payload.is_object() {
            continue;
        }

        if kind == "subagent" {
            let (summary_msg, candidate) = parse_subagent_row(
                position,
                &payload,
                &stem,
                db_path,
                meta_info.model.as_deref(),
                main_workspace.as_deref(),
                &mut overall_max_ts,
            );
            if let Some(msg) = summary_msg {
                update_ts_bounds(msg.created_at, &mut main_min_ts, &mut main_max_ts);
                main_messages.push(msg);
            }
            if let Some(cand) = candidate {
                let key = cand
                    .conv
                    .metadata
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if !key.is_empty() {
                    let replace = subagents
                        .get(&key)
                        .is_none_or(|prev| should_replace_subagent(prev, &cand));
                    if replace {
                        subagents.insert(key, cand);
                    }
                }
            }
            continue;
        }

        if let Some(msg) = parse_row_message(
            &kind,
            &payload,
            meta_info.model.as_deref(),
            &mut main_skipped,
            &mut main_workspace,
        ) {
            if position == 0 && kind == "user_message" {
                pos_0_is_user = true;
            }
            update_ts_bounds(msg.created_at, &mut main_min_ts, &mut main_max_ts);
            update_max_ts(msg.created_at, &mut overall_max_ts);
            main_messages.push(msg);
        }
    }

    if let Some(since) = since_ts {
        let recent = overall_max_ts.map_or_else(
            || {
                let wal = sidecar_path(db_path, "-wal");
                file_modified_since(db_path, Some(since))
                    || (wal.is_file() && file_modified_since(&wal, Some(since)))
            },
            |max_ts| max_ts >= since,
        );
        if !recent {
            return Ok(Vec::new());
        }
    }

    if main_messages.is_empty() {
        return Ok(Vec::new());
    }

    assign_message_indices(&mut main_messages);
    let first_user_title = first_user_line_title(&main_messages);
    let title = if pos_0_is_user {
        first_user_title
            .as_deref()
            .and_then(sanitize_title)
            .or_else(|| meta_info.title.clone())
    } else {
        meta_info
            .title
            .clone()
            .or_else(|| first_user_title.as_deref().and_then(sanitize_title))
    }
    .or_else(|| Some(stem.clone()));

    let mut metadata = Map::new();
    metadata.insert("source".into(), json!("acp"));
    metadata.insert("is_subagent".into(), json!(false));
    metadata.insert("store_layout".into(), json!("acp-messages"));
    if let Some(ref meta_title) = meta_info.title {
        metadata.insert("title".into(), Value::String(meta_title.clone()));
    }
    if let Some(ref model) = meta_info.model {
        metadata.insert("model".into(), Value::String(model.clone()));
    }
    if !main_skipped.is_empty() {
        metadata.insert("skipped_kinds".into(), json!(main_skipped));
    }

    let mut conversations = Vec::with_capacity(1 + subagents.len());
    conversations.push(NormalizedConversation {
        agent_slug: "devin".into(),
        external_id: Some(stem),
        title,
        workspace: main_workspace.clone(),
        source_path: db_path.to_path_buf(),
        started_at: main_min_ts,
        ended_at: main_max_ts,
        metadata: Value::Object(metadata),
        messages: main_messages,
    });

    let mut ordered_subs: Vec<SubagentCandidate> = subagents.into_values().collect();
    ordered_subs.sort_by_key(|c| c.position);
    for mut cand in ordered_subs {
        if cand.conv.workspace.is_none() {
            cand.conv.workspace.clone_from(&main_workspace);
        }
        conversations.push(cand.conv);
    }

    Ok(conversations)
}

fn should_replace_subagent(existing: &SubagentCandidate, candidate: &SubagentCandidate) -> bool {
    let existing_rank = (
        existing.conv.messages.len(),
        u8::from(existing.completed),
        existing.position,
    );
    let candidate_rank = (
        candidate.conv.messages.len(),
        u8::from(candidate.completed),
        candidate.position,
    );
    candidate_rank > existing_rank
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    let rows: Vec<i64> = conn
        .query_map_collect(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
            params![table],
            |row| row.get_typed(0),
        )
        .with_context(|| format!("failed to probe sqlite_master for {table}"))?;
    Ok(!rows.is_empty())
}

fn is_no_such_table(err: &impl std::fmt::Display) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("no such table")
}

fn read_meta_info(conn: &Connection) -> Result<MetaInfo> {
    if !has_table(conn, "meta")? {
        return Ok(MetaInfo::default());
    }
    let rows: Vec<(String, String)> = match conn.query_map_collect(
        "SELECT key, value FROM meta WHERE key = 'info'",
        params![],
        |row| {
            Ok((
                row.get_typed::<Option<String>>(0)?.unwrap_or_default(),
                row.get_typed::<Option<String>>(1)?.unwrap_or_default(),
            ))
        },
    ) {
        Ok(rows) => rows,
        Err(err) if is_no_such_table(&err) => return Ok(MetaInfo::default()),
        Err(err) => return Err(err).context("failed to read Devin ACP meta table"),
    };

    let mut info_out = MetaInfo::default();
    for (key, value) in rows {
        if key != "info" {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(&value) else {
            continue;
        };
        if !parsed.is_object() {
            continue;
        }
        if info_out.model.is_none() {
            info_out.model = extract_model_from_info(&parsed);
        }
        if info_out.title.is_none() {
            info_out.title = parsed
                .get("title")
                .and_then(Value::as_str)
                .and_then(sanitize_title);
        }
    }
    Ok(info_out)
}

fn read_message_rows(conn: &Connection) -> Result<Vec<(i64, String, String)>> {
    match conn.query_map_collect(
        "SELECT position, kind, payload FROM messages ORDER BY position ASC",
        params![],
        |row| {
            Ok((
                row.get_typed::<Option<i64>>(0)?.unwrap_or(0),
                row.get_typed::<Option<String>>(1)?.unwrap_or_default(),
                row.get_typed::<Option<String>>(2)?.unwrap_or_default(),
            ))
        },
    ) {
        Ok(rows) => Ok(rows),
        Err(err) if is_no_such_table(&err) => Ok(Vec::new()),
        Err(err) => Err(err).context("failed to query Devin ACP messages"),
    }
}

fn extract_model_from_info(info: &Value) -> Option<String> {
    let config_opts = info.get("configOptions")?;
    match config_opts {
        Value::Object(map) => map.iter().find_map(|(key, opt)| {
            key.eq_ignore_ascii_case("model")
                .then(|| current_value_string(opt))
                .flatten()
        }),
        Value::Array(items) => items.iter().find_map(|opt| {
            let is_model = ["id", "category", "name"].iter().any(|field| {
                opt.get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.eq_ignore_ascii_case("model"))
            });
            is_model.then(|| current_value_string(opt)).flatten()
        }),
        _ => None,
    }
}

fn current_value_string(opt: &Value) -> Option<String> {
    match opt.get("currentValue")? {
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn sanitize_title(raw: &str) -> Option<String> {
    let stripped = strip_project_context_xml(raw);
    let normalized = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn strip_project_context_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<project_context") {
        out.push_str(&rest[..start]);
        let after_start = &rest[start..];
        if let Some(end_tag) = after_start.find("</project_context>") {
            rest = &after_start[end_tag + "</project_context>".len()..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

fn extract_timestamp(payload: &Value) -> Option<i64> {
    if let Some(ts) = payload
        .get("_meta")
        .and_then(|m| m.get("cognition.ai/timestamp"))
        .and_then(parse_timestamp)
    {
        return Some(ts);
    }
    if let Some(ts) = payload
        .get("sourceEventTimestampMs")
        .and_then(parse_timestamp)
    {
        return Some(ts);
    }
    let content = payload.get("content")?;
    match content {
        Value::Array(chunks) => chunks.iter().find_map(|chunk| {
            chunk
                .get("_meta")
                .and_then(|m| m.get("cognition.ai/timestamp"))
                .and_then(parse_timestamp)
        }),
        Value::Object(obj) => obj
            .get("_meta")
            .and_then(|m| m.get("cognition.ai/timestamp"))
            .and_then(parse_timestamp)
            .or_else(|| obj.get("sourceEventTimestampMs").and_then(parse_timestamp)),
        _ => None,
    }
}

fn collect_text_parts(
    content: Option<&Value>,
    strip_context: bool,
    stream_deltas: bool,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut push_part = |raw: &str| {
        if stream_deltas {
            parts.push(raw.to_owned());
            return;
        }
        let cleaned = if strip_context {
            strip_project_context_xml(raw)
        } else {
            raw.to_owned()
        };
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_owned());
        }
    };

    match content? {
        Value::Array(chunks) => {
            for chunk in chunks {
                let Some(obj) = chunk.as_object() else {
                    continue;
                };
                match obj.get("content") {
                    Some(Value::Object(inner)) => {
                        if let Some(txt) = inner.get("text").and_then(Value::as_str) {
                            push_part(txt);
                        } else if inner.get("type").and_then(Value::as_str) == Some("image")
                            || inner.contains_key("data")
                        {
                            push_part("[image]");
                        }
                    }
                    Some(Value::String(txt)) if strip_context => push_part(txt),
                    _ => {
                        if strip_context && let Some(txt) = obj.get("text").and_then(Value::as_str)
                        {
                            push_part(txt);
                        }
                    }
                }
            }
        }
        Value::String(txt) => push_part(txt),
        _ => return None,
    }

    if parts.is_empty() {
        return None;
    }
    let joined = if stream_deltas {
        parts.concat()
    } else {
        parts.join("\n\n")
    };
    (!joined.trim().is_empty()).then_some(joined)
}

fn extract_tool_output_text(tool: &Value, payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    let source = tool
        .get("rawOutput")
        .or_else(|| tool.get("output"))
        .or_else(|| match tool.get("content") {
            Some(Value::String(_) | Value::Array(_)) => tool.get("content"),
            _ => None,
        })
        .or_else(|| payload.get("output"));

    match source {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                parts.push(s.clone());
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(txt) = item
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .or_else(|| item.get("text").and_then(Value::as_str))
                    .or_else(|| item.as_str())
                    && !txt.is_empty()
                {
                    parts.push(txt.to_owned());
                }
            }
        }
        _ => {}
    }

    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("\n");
    if joined.len() <= MAX_TOOL_OUTPUT_BYTES {
        return Some(joined);
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while end > 0 && !joined.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}... [truncated]", &joined[..end]))
}

fn normalize_tool_name(tool: &Value) -> String {
    let inference = tool
        .get("_meta")
        .and_then(|m| m.get("cognition.ai/inferenceToolName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(inf) = inference {
        return match inf {
            "exec" | "terminal" | "bash" | "kill_shell" | "get_output" => "exec".to_owned(),
            "read" | "mcp_read_resource" => "read".to_owned(),
            "grep" | "find_file_by_name" | "find_code_context" | "search" => "grep".to_owned(),
            other => other.to_owned(),
        };
    }

    if let Some(kind) = tool
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "tool_call")
    {
        return match kind {
            "execute" => "exec".to_owned(),
            "read" => "read".to_owned(),
            "search" => "grep".to_owned(),
            other => other.to_owned(),
        };
    }

    if tool
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|t| t.starts_with("Ran "))
    {
        return "exec".to_owned();
    }

    "tool_call".to_owned()
}

fn compact_tool_payload(payload: &Value, tool: &Value) -> Value {
    let mut compact_tool = Map::new();
    for key in [
        "toolCallId",
        "callId",
        "id",
        "title",
        "kind",
        "status",
        "_meta",
    ] {
        if let Some(val) = tool.get(key) {
            compact_tool.insert(key.to_owned(), val.clone());
        }
    }
    if let Some(raw_input) = tool
        .get("rawInput")
        .or_else(|| tool.get("input"))
        .or_else(|| tool.get("arguments"))
    {
        compact_tool.insert("rawInput".into(), bound_tool_arguments(raw_input.clone()));
    }
    if let Some(output_text) = extract_tool_output_text(tool, payload) {
        compact_tool.insert("rawOutput".into(), Value::String(output_text));
    }
    let mut root = Map::new();
    root.insert("kind".into(), json!("tool_call"));
    if let Some(ts) = payload.get("sourceEventTimestampMs") {
        root.insert("sourceEventTimestampMs".into(), ts.clone());
    }
    root.insert("content".into(), Value::Object(compact_tool));
    Value::Object(root)
}

fn compact_text_payload(kind: &str, text: &str) -> Value {
    json!({
        "kind": kind,
        "content": [{
            "content": {
                "type": "text",
                "text": text,
            }
        }],
    })
}

fn extract_plan_text(payload: &Value) -> Option<String> {
    let content = payload.get("content").unwrap_or(payload);
    let entries = content
        .get("entries")
        .or_else(|| payload.get("entries"))
        .or_else(|| content.get("plan").and_then(|p| p.get("entries")))
        .or_else(|| payload.get("plan").and_then(|p| p.get("entries")))
        .and_then(Value::as_array);

    if let Some(entries) = entries {
        let mut lines = Vec::new();
        for entry in entries {
            let Some(text) = entry
                .get("content")
                .or_else(|| entry.get("title"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let status = entry
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            lines.push(format!("- [{status}] {text}"));
        }
        if !lines.is_empty() {
            return Some(lines.join("\n"));
        }
    }

    content
        .get("markdown")
        .or_else(|| content.get("text"))
        .or_else(|| payload.get("markdown"))
        .or_else(|| payload.get("text"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_row_message(
    kind: &str,
    payload: &Value,
    model: Option<&str>,
    skipped_kinds: &mut BTreeMap<String, usize>,
    workspace: &mut Option<PathBuf>,
) -> Option<NormalizedMessage> {
    let created_at = extract_timestamp(payload);
    match kind {
        "user_message" => {
            let text = collect_text_parts(payload.get("content"), true, false)?;
            Some(NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at,
                extra: json!({
                    "kind": "user_message",
                    "payload": compact_text_payload("user_message", &text),
                }),
                content: text,
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
        }
        "agent_thought" => {
            let text = collect_text_parts(payload.get("content"), false, true)?;
            let mut extra = Map::new();
            extra.insert("kind".into(), json!("agent_thought"));
            extra.insert("thinking".into(), Value::String(text.clone()));
            extra.insert(
                "payload".into(),
                compact_text_payload("agent_thought", &text),
            );
            if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
                extra.insert("model".into(), Value::String(m.to_owned()));
            }
            Some(NormalizedMessage {
                idx: 0,
                role: "assistant".into(),
                author: model.map(str::to_owned),
                created_at,
                content: text,
                extra: Value::Object(extra),
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
        }
        "agent_message" => {
            let text = collect_text_parts(payload.get("content"), false, true)?;
            let mut extra = Map::new();
            extra.insert("kind".into(), json!("agent_message"));
            extra.insert(
                "payload".into(),
                compact_text_payload("agent_message", &text),
            );
            if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
                extra.insert("model".into(), Value::String(m.to_owned()));
            }
            Some(NormalizedMessage {
                idx: 0,
                role: "assistant".into(),
                author: model.map(str::to_owned),
                created_at,
                content: text,
                extra: Value::Object(extra),
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
        }
        "tool_call" => {
            let tool = payload
                .get("content")
                .filter(|v| v.is_object())
                .unwrap_or(payload);
            if workspace.is_none()
                && let Some(cwd) = tool
                    .get("_meta")
                    .and_then(|m| m.get("cognition.ai/cwd"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                *workspace = Some(PathBuf::from(cwd));
            }
            let name = normalize_tool_name(tool);
            let call_id = tool
                .get("toolCallId")
                .or_else(|| tool.get("callId"))
                .or_else(|| tool.get("id"))
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let raw_input = tool
                .get("rawInput")
                .or_else(|| tool.get("input"))
                .or_else(|| tool.get("arguments"))
                .or_else(|| payload.get("rawInput"))
                .cloned()
                .map(bound_tool_arguments);
            let mut invocations = vec![NormalizedInvocation {
                kind: "tool".into(),
                name: name.clone(),
                raw_name: tool.get("kind").and_then(Value::as_str).map(str::to_owned),
                call_id,
                arguments: raw_input,
            }];
            unwrap_skill_invocations(&mut invocations);

            let title = tool
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let output_text = extract_tool_output_text(tool, payload);
            let header = title.map_or_else(
                || format!("[tool_call: {name}]"),
                |t| format!("[tool_call: {name}] {t}"),
            );
            let content = match output_text {
                Some(ref out) if !out.trim().is_empty() => format!("{header}\n{out}"),
                _ => header,
            };
            let mut extra = Map::new();
            extra.insert("kind".into(), json!("tool_call"));
            extra.insert(
                "cass".into(),
                json!({ "tool_call_count": invocations.len() }),
            );
            extra.insert("payload".into(), compact_tool_payload(payload, tool));
            Some(NormalizedMessage {
                idx: 0,
                role: "assistant".into(),
                author: model.map(str::to_owned),
                created_at,
                content,
                extra: Value::Object(extra),
                snippets: Vec::new(),
                invocations,
            })
        }
        "plan" => {
            let text = extract_plan_text(payload)?;
            Some(NormalizedMessage {
                idx: 0,
                role: "assistant".into(),
                author: model.map(str::to_owned),
                created_at,
                content: text,
                extra: json!({ "kind": "plan" }),
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
        }
        other => {
            *skipped_kinds.entry(other.to_owned()).or_insert(0) += 1;
            None
        }
    }
}

fn parse_subagent_row(
    position: i64,
    payload: &Value,
    stem: &str,
    db_path: &Path,
    model: Option<&str>,
    fallback_workspace: Option<&Path>,
    overall_max_ts: &mut Option<i64>,
) -> (Option<NormalizedMessage>, Option<SubagentCandidate>) {
    let agent_id = payload
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .and_then(sanitize_title);
    let task = payload
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let profile = payload
        .get("profile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let success = payload.get("success").and_then(Value::as_bool);

    let summary_label = title.as_deref().or(task).unwrap_or("spawned");
    let agent_label = agent_id.unwrap_or("subagent");
    let mut desc_payload = Map::new();
    if let Some(aid) = agent_id {
        desc_payload.insert("agentId".into(), Value::String(aid.to_owned()));
    }
    if let Some(t) = title.as_deref() {
        desc_payload.insert("title".into(), Value::String(t.to_owned()));
    }
    if let Some(p) = profile {
        desc_payload.insert("profile".into(), Value::String(p.to_owned()));
    }
    if let Some(s) = status {
        desc_payload.insert("status".into(), Value::String(s.to_owned()));
    }
    let summary_msg = Some(NormalizedMessage {
        idx: 0,
        role: "assistant".into(),
        author: model.map(str::to_owned),
        created_at: extract_timestamp(payload),
        content: format!("Subagent {agent_label}: {summary_label}"),
        extra: json!({
            "kind": "subagent",
            "agent_id": agent_label,
            "payload": Value::Object(desc_payload),
        }),
        snippets: Vec::new(),
        invocations: Vec::new(),
    });

    let Some(agent_id) = agent_id else {
        return (summary_msg, None);
    };
    let Some(children) = payload.get("childMessages").and_then(Value::as_array) else {
        return (summary_msg, None);
    };

    let mut child_messages = Vec::new();
    let mut child_skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut child_workspace: Option<PathBuf> = None;
    let mut child_min_ts: Option<i64> = None;
    let mut child_max_ts: Option<i64> = None;

    for child in children {
        if !child.is_object() {
            continue;
        }
        let child_kind = child
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("agent_thought");
        if let Some(msg) = parse_row_message(
            child_kind,
            child,
            model,
            &mut child_skipped,
            &mut child_workspace,
        ) {
            update_ts_bounds(msg.created_at, &mut child_min_ts, &mut child_max_ts);
            update_max_ts(msg.created_at, overall_max_ts);
            child_messages.push(msg);
        }
    }

    if child_messages.is_empty() {
        return (summary_msg, None);
    }

    assign_message_indices(&mut child_messages);
    let conv_title = title
        .or_else(|| task.and_then(|t| t.lines().next()).and_then(sanitize_title))
        .or_else(|| Some(format!("Subagent {agent_id}")));

    let mut metadata = Map::new();
    metadata.insert("source".into(), json!("acp"));
    metadata.insert("is_subagent".into(), json!(true));
    metadata.insert("parent_id".into(), Value::String(stem.to_owned()));
    metadata.insert("agent_id".into(), Value::String(agent_id.to_owned()));
    metadata.insert("store_layout".into(), json!("acp-messages"));
    if let Some(p) = profile {
        metadata.insert("profile".into(), Value::String(p.to_owned()));
    }
    if let Some(s) = status {
        metadata.insert("status".into(), Value::String(s.to_owned()));
    }
    if let Some(ok) = success {
        metadata.insert("success".into(), Value::Bool(ok));
    }
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        metadata.insert("model".into(), Value::String(m.to_owned()));
    }
    if !child_skipped.is_empty() {
        metadata.insert("skipped_kinds".into(), json!(child_skipped));
    }

    let conv = NormalizedConversation {
        agent_slug: "devin".into(),
        external_id: Some(format!("{stem}:{agent_id}")),
        title: conv_title,
        workspace: child_workspace.or_else(|| fallback_workspace.map(Path::to_path_buf)),
        source_path: db_path.join(format!(":{agent_id}")),
        started_at: child_min_ts,
        ended_at: child_max_ts,
        metadata: Value::Object(metadata),
        messages: child_messages,
    };

    let completed =
        success == Some(true) || status.is_some_and(|s| s.eq_ignore_ascii_case("completed"));
    (
        summary_msg,
        Some(SubagentCandidate {
            position,
            completed,
            conv,
        }),
    )
}

fn update_ts_bounds(ts: Option<i64>, min_ts: &mut Option<i64>, max_ts: &mut Option<i64>) {
    if let Some(ts) = ts {
        *min_ts = Some(min_ts.map_or(ts, |cur| cur.min(ts)));
        *max_ts = Some(max_ts.map_or(ts, |cur| cur.max(ts)));
    }
}

fn update_max_ts(ts: Option<i64>, max_ts: &mut Option<i64>) {
    if let Some(ts) = ts {
        *max_ts = Some(max_ts.map_or(ts, |cur| cur.max(ts)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sanitize_title_strips_project_context_and_normalizes_whitespace() {
        assert_eq!(
            sanitize_title("<project_context>abc\ndef</project_context>\n  Fix   bug  "),
            Some("Fix bug".to_owned())
        );
        assert_eq!(
            sanitize_title("Prefix <project_context>unclosed"),
            Some("Prefix".to_owned())
        );
        assert_eq!(
            sanitize_title("<project_context>only</project_context>"),
            None
        );
    }

    #[test]
    fn extract_model_handles_array_and_object_config_options() {
        let arr = json!({
            "configOptions": [
                { "id": "mode", "currentValue": "agent" },
                { "id": "model", "category": "model", "currentValue": "claude-opus-4-1" },
            ]
        });
        assert_eq!(
            extract_model_from_info(&arr).as_deref(),
            Some("claude-opus-4-1")
        );

        let obj = json!({
            "configOptions": {
                "model": { "currentValue": "gpt-5.2" }
            }
        });
        assert_eq!(extract_model_from_info(&obj).as_deref(), Some("gpt-5.2"));
    }

    #[test]
    fn since_ts_uses_file_mtime_when_messages_lack_timestamps() {
        let dir = TempDir::new().expect("tempdir");
        let db = dir.path().join("aaaa-bbbb.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).expect("open db");
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE messages (position INTEGER PRIMARY KEY, kind TEXT, payload TEXT);
             INSERT INTO messages VALUES (0, 'user_message', '{\"kind\":\"user_message\",\"content\":\"hello\"}');",
        )
        .expect("seed db");
        drop(conn);

        let convs = extract_acp_store(&db, Some(0)).expect("extract with old since_ts");
        assert_eq!(convs.len(), 1);

        let far_future_ms = 4_102_444_800_000;
        let gated = extract_acp_store(&db, Some(far_future_ms)).expect("extract gated");
        assert!(gated.is_empty());
    }
}

//! Timestamp normalization, flat-file loading, and message-part assembly for `OpenCode`.

#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::if_not_else,
    clippy::manual_let_else,
    clippy::manual_string_new,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::nonminimal_bool,
    clippy::option_if_let_else,
    clippy::single_option_map,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps,
    clippy::unreadable_literal
)]

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankensqlite::{Row, SqliteValue};
use walkdir::WalkDir;

use super::models::{LoadedPart, MessageInfo, PartInfo, SessionInfo};
use super::unwrap_skill_invocations;
use super::utils::{file_modified_since, path_is_excluded, read_capped};
use crate::types::{NormalizedInvocation, NormalizedMessage};

pub(super) fn normalize_opencode_timestamp(ts: Option<i64>) -> Option<i64> {
    ts.map(|raw| {
        // OpenCode appears to store epoch timestamps in milliseconds (see fixtures),
        // but some sources may emit epoch seconds. We treat "plausible epoch seconds"
        // as seconds and otherwise assume milliseconds (including small synthetic test values).
        if (1_000_000_000..10_000_000_000).contains(&raw) {
            raw.saturating_mul(1000)
        } else {
            raw
        }
    })
}

pub(super) fn resolve_v1_model_id(
    model_id: Option<&str>,
    model: Option<&serde_json::Value>,
) -> Option<String> {
    model_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            model
                .and_then(|m| {
                    m.as_str()
                        .or_else(|| m.get("modelID").and_then(serde_json::Value::as_str))
                        .or_else(|| m.get("id").and_then(serde_json::Value::as_str))
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

pub(super) fn resolve_v1_provider_id(
    provider_id: Option<&str>,
    model: Option<&serde_json::Value>,
) -> Option<String> {
    provider_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            model
                .and_then(|m| {
                    m.get("providerID")
                        .or_else(|| m.get("provider"))
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

pub(super) fn build_v1_message_extra(
    message_id: &str,
    session_id: Option<&str>,
    model_id: Option<&str>,
    provider_id: Option<&str>,
    tokens: Option<&serde_json::Value>,
    tool_call_count: usize,
) -> serde_json::Value {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "message_id".to_string(),
        serde_json::Value::String(message_id.to_string()),
    );
    extra.insert(
        "session_id".to_string(),
        session_id.map_or(serde_json::Value::Null, |s| {
            serde_json::Value::String(s.to_string())
        }),
    );
    if let Some(model) = model_id.map(str::trim).filter(|s| !s.is_empty()) {
        extra.insert(
            "modelID".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    if let Some(provider) = provider_id.map(str::trim).filter(|s| !s.is_empty()) {
        extra.insert(
            "providerID".to_string(),
            serde_json::Value::String(provider.to_string()),
        );
    }
    if let Some(tokens) = tokens.filter(|v| v.is_object()) {
        extra.insert("tokens".to_string(), tokens.clone());
    }
    if tool_call_count > 0 {
        extra.insert(
            "cass".to_string(),
            serde_json::json!({ "tool_call_count": tool_call_count }),
        );
    }
    serde_json::Value::Object(extra)
}

/// The renderable failure text of a raw `state.error` record: the string
/// form, or the object's `message` string (F14).
pub(super) fn error_text_of(error: Option<&serde_json::Value>) -> Option<&str> {
    match error? {
        serde_json::Value::String(text) => Some(text.as_str()),
        serde_json::Value::Object(record) => {
            record.get("message").and_then(serde_json::Value::as_str)
        }
        _ => None,
    }
}

pub(super) fn optional_sqlite_value(row: &Row, index: usize) -> Option<SqliteValue> {
    row.get(index).and_then(|value| {
        if matches!(value, SqliteValue::Null) {
            None
        } else {
            Some(value.clone())
        }
    })
}

/// Normalize a raw SQLite value to epoch milliseconds.
///
/// Drizzle ORM can store timestamps as:
///  - TEXT: ISO 8601 strings like `"2024-01-15 14:30:00"` or `"2024-01-15T14:30:00"`
///  - INTEGER: epoch seconds (e.g. `1700000000`) or epoch milliseconds (e.g. `1700000000000`)
///
/// Returns `None` for NULL or unparseable values.
pub(super) fn normalize_sqlite_ts_value(val: &SqliteValue) -> Option<i64> {
    match val {
        SqliteValue::Integer(i) => normalize_opencode_timestamp(Some(*i)),
        SqliteValue::Float(f) => normalize_opencode_timestamp(Some(*f as i64)),
        SqliteValue::Text(s) => {
            // Try common SQLite/Drizzle datetime formats (space separator)
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
                Some(dt.and_utc().timestamp_millis())
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
            {
                Some(dt.and_utc().timestamp_millis())
            // ISO 8601 with T separator
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                Some(dt.and_utc().timestamp_millis())
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            {
                Some(dt.and_utc().timestamp_millis())
            // RFC 3339 with timezone (e.g. "2024-01-15T14:30:00Z")
            } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                Some(dt.timestamp_millis())
            } else {
                // Last resort: try parsing as integer string
                s.trim()
                    .parse::<i64>()
                    .ok()
                    .and_then(|i| normalize_opencode_timestamp(Some(i)))
            }
        }
        SqliteValue::Null | SqliteValue::Blob(_) => None,
    }
}

pub(super) fn session_has_updates(
    session_file: &Path,
    message_root: &Path,
    part_root: &Path,
    since_ts: Option<i64>,
) -> bool {
    if since_ts.is_none() {
        return true;
    }

    if file_modified_since(session_file, since_ts) {
        return true;
    }

    let session_id = session_file
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string);
    let Some(session_id) = session_id else {
        return true;
    };

    let session_msg_dir = message_root.join(&session_id);
    if !session_msg_dir.exists() {
        return false;
    }

    let mut message_ids = Vec::new();
    if let Ok(entries) = fs::read_dir(&session_msg_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().map(|ext| ext == "json").unwrap_or(false) {
                if file_modified_since(&path, since_ts) {
                    return true;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    message_ids.push(stem.to_string());
                }
            }
        }
    }

    for message_id in message_ids {
        let part_dir = part_root.join(&message_id);
        if !part_dir.exists() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&part_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if file_modified_since(&path, since_ts) {
                    return true;
                }
            }
        }
    }

    false
}

/// Parse a session JSON file
pub(super) fn parse_session_file(path: &Path) -> Result<SessionInfo> {
    let content = match read_capped(path) {
        Ok(Some(content)) => content,
        Ok(None) => {
            return Err(anyhow::Error::msg(format!(
                "session file {} exceeds the scan size cap",
                path.display()
            )));
        }
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("read session file {}", path.display()))
            );
        }
    };
    // Editors/tools sometimes leave a UTF-8 BOM on flat-file sessions;
    // serde_json rejects a leading U+FEFF and the whole session would be
    // silently skipped (gemini's replay strips it for the same reason).
    let content = content.trim_start_matches('\u{feff}');
    let session: SessionInfo = serde_json::from_str(content)
        .with_context(|| format!("parse session JSON {}", path.display()))?;
    Ok(session)
}

/// Load all messages for a session
pub(super) fn load_messages_with_exclusions(
    session_msg_dir: &Path,
    part_dir: &Path,
    excluded_paths: &[PathBuf],
) -> Result<Vec<NormalizedMessage>> {
    let mut pending: Vec<(Option<i64>, String, NormalizedMessage)> = Vec::new();

    // Find all message files for this session
    let msg_files: Vec<PathBuf> = WalkDir::new(session_msg_dir)
        .max_depth(1)
        .into_iter()
        .filter_entry(|e| !path_is_excluded(e.path(), excluded_paths))
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| !path_is_excluded(e.path(), excluded_paths))
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    for msg_file in msg_files {
        // Cap the read (message JSONs embed full tool outputs) and strip a
        // leading UTF-8 BOM, which serde_json rejects (same as session files).
        let content = match read_capped(&msg_file) {
            Ok(Some(c)) => c.trim_start_matches('\u{feff}').to_string(),
            Ok(None) => {
                tracing::debug!(
                    file = %msg_file.display(),
                    "opencode: message file exceeds the scan size cap; skipping"
                );
                continue;
            }
            Err(_) => continue,
        };

        let msg_info: MessageInfo = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Load parts for this specific message, keeping the raw part JSON
        // alongside the decoded fields (F14) so the whole record survives.
        let mut parts: Vec<LoadedPart> = Vec::new();
        let msg_part_dir = part_dir.join(&msg_info.id);

        if msg_part_dir.exists() && !path_is_excluded(&msg_part_dir, excluded_paths) {
            for entry in WalkDir::new(&msg_part_dir)
                .max_depth(1)
                .into_iter()
                .filter_entry(|e| !path_is_excluded(e.path(), excluded_paths))
                .flatten()
            {
                if !entry.file_type().is_file() || path_is_excluded(entry.path(), excluded_paths) {
                    continue;
                }
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false)
                    && let Some(content) = read_capped(path)
                        .ok()
                        .flatten()
                        .map(|c| c.trim_start_matches('\u{feff}').to_string())
                    && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&content)
                    && let Ok(info) = serde_json::from_value::<PartInfo>(raw.clone())
                {
                    parts.push(LoadedPart { info, raw });
                }
            }
        }
        sort_loaded_parts(&mut parts);

        // Assemble message content and invocations from parts
        let infos: Vec<PartInfo> = parts.iter().map(|part| part.info.clone()).collect();
        let invocations = extract_invocations_from_parts(&infos);
        let content_text = if parts.is_empty() {
            String::new()
        } else {
            assemble_content_from_parts(&infos)
        };
        // A tool part is identity-bearing even when it renders no text, so
        // its message survives with empty content (F13's codex rule).
        if content_text.trim().is_empty() && invocations.is_empty() && !has_tool_part(&parts) {
            continue;
        }

        // Determine role
        let role = msg_info
            .role
            .clone()
            .unwrap_or_else(|| "assistant".to_string());

        // Determine timestamp
        let created_at =
            normalize_opencode_timestamp(msg_info.time.as_ref().and_then(|t| t.created));

        // Author from model_id for assistant messages
        let resolved_model_id =
            resolve_v1_model_id(msg_info.model_id.as_deref(), msg_info.model.as_ref());
        let resolved_provider_id =
            resolve_v1_provider_id(msg_info.provider_id.as_deref(), msg_info.model.as_ref());
        let author = if role == "assistant" {
            resolved_model_id.clone()
        } else {
            Some("user".to_string())
        };

        let message_id = msg_info.id.clone();
        // F14: archive the raw parts in delivery order.
        let mut extra = build_v1_message_extra(
            &message_id,
            msg_info.session_id.as_deref(),
            resolved_model_id.as_deref(),
            resolved_provider_id.as_deref(),
            msg_info.tokens.as_ref(),
            invocations.len(),
        );
        if !parts.is_empty() {
            extra["parts"] =
                serde_json::Value::Array(parts.iter().map(|part| part.raw.clone()).collect());
        }
        pending.push((
            created_at,
            message_id.clone(),
            NormalizedMessage {
                idx: 0, // Will be assigned later
                role,
                author,
                created_at,
                content: content_text,
                extra,
                invocations,
                snippets: Vec::new(),
            },
        ));
    }

    // Sort by timestamp, then by message id to ensure deterministic ordering.
    pending.sort_by(|a, b| {
        let a_ts = a.0.unwrap_or(i64::MAX);
        let b_ts = b.0.unwrap_or(i64::MAX);
        a_ts.cmp(&b_ts).then_with(|| a.1.cmp(&b.1))
    });
    let mut messages: Vec<NormalizedMessage> = pending.into_iter().map(|(_, _, msg)| msg).collect();
    crate::types::reindex_messages(&mut messages);

    Ok(messages)
}

fn part_is_tool(info: &PartInfo) -> bool {
    info.part_type.as_deref() == Some("tool")
}

/// Whether any of a message's loaded parts is a tool part — a part that
/// carries identity and state even when it renders no text at all.
pub(super) fn has_tool_part(parts: &[LoadedPart]) -> bool {
    parts.iter().any(|part| part_is_tool(&part.info))
}

fn compare_part_infos(a: &PartInfo, b: &PartInfo) -> std::cmp::Ordering {
    let a_idx = a.index.unwrap_or(i64::MAX);
    let b_idx = b.index.unwrap_or(i64::MAX);
    a_idx
        .cmp(&b_idx)
        .then_with(|| {
            a.id.as_deref()
                .unwrap_or("")
                .cmp(b.id.as_deref().unwrap_or(""))
        })
        .then_with(|| {
            a.part_type
                .as_deref()
                .unwrap_or("")
                .cmp(b.part_type.as_deref().unwrap_or(""))
        })
        .then_with(|| {
            a.text
                .as_deref()
                .unwrap_or("")
                .cmp(b.text.as_deref().unwrap_or(""))
        })
}

/// Sort raw-archived parts into the same order the content renders in, so
/// `extra["parts"]` mirrors the delivered content piece for piece.
pub(super) fn sort_loaded_parts(parts: &mut [LoadedPart]) {
    parts.sort_by(|a, b| compare_part_infos(&a.info, &b.info));
}

/// Assemble message content from parts
pub(super) fn assemble_content_from_parts(parts: &[PartInfo]) -> String {
    let mut content_pieces: Vec<String> = Vec::new();

    for part in parts {
        match part.part_type.as_deref() {
            Some("text") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(text.clone());
                }
            }
            Some("tool") => {
                // Include tool output and error text if available. The error
                // text is source text a failed run carries in `state.error` —
                // it used to be dropped (F14).
                if let Some(state) = &part.state {
                    if let Some(output) = &state.output
                        && !output.trim().is_empty()
                    {
                        content_pieces.push(format!("[Tool Output]\n{}", output));
                    }
                    if let Some(error) = state.error.as_ref().and_then(|e| e.text())
                        && !error.trim().is_empty()
                    {
                        content_pieces.push(format!("[Tool Error]\n{}", error));
                    }
                }
            }
            Some("reasoning") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(format!("[Reasoning]\n{}", text));
                }
            }
            Some("patch") => {
                if let Some(text) = &part.text
                    && !text.trim().is_empty()
                {
                    content_pieces.push(format!("[Patch]\n{}", text));
                }
            }
            // Ignore step-start, step-finish, and other control parts
            _ => {}
        }
    }

    content_pieces.join("\n\n")
}

pub(super) fn extract_invocations_from_parts(parts: &[PartInfo]) -> Vec<NormalizedInvocation> {
    let mut invocations = Vec::new();
    for part in parts {
        if part.part_type.as_deref() != Some("tool") {
            continue;
        }
        let Some(name) = part
            .tool
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let call_id = part
            .call_id
            .as_deref()
            .or(part.id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let arguments = part
            .state
            .as_ref()
            .and_then(|state| state.input.clone())
            .filter(|v| !v.is_null());
        invocations.push(NormalizedInvocation {
            kind: "tool".to_string(),
            name: name.to_string(),
            raw_name: None,
            call_id,
            arguments,
        });
    }
    unwrap_skill_invocations(&mut invocations);
    invocations
}

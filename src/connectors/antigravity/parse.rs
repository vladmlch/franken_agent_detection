//! Transcript record normalization, prompt/model parsing, and workspace extraction.

use std::path::PathBuf;

use serde_json::{Map, Value};

use super::planner_calls;
use crate::connectors::parse_timestamp;
use crate::connectors::utils::unwrap_skill_invocations;
use crate::types::{NormalizedInvocation, NormalizedMessage};

/// Extract the inner text of every `<TAG>…</TAG>` block in `content`.
pub(super) fn extract_tagged_blocks(content: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(after[..end].trim().to_string());
        rest = &after[end + close.len()..];
    }
    out
}

/// Split a wrapped `USER_INPUT` payload into (request body, settings-change text,
/// metadata text). agy wraps the real prompt as `<USER_REQUEST>` and may append
/// `<USER_SETTINGS_CHANGE>` and `<ADDITIONAL_METADATA>`. A single record can
/// carry several `<USER_REQUEST>` blocks; they are joined. If no wrapper is
/// present we fall back to the raw content.
pub(super) fn extract_user_request(content: &str) -> (String, Option<String>, Option<String>) {
    let requests = extract_tagged_blocks(content, "USER_REQUEST");
    let settings = extract_tagged_blocks(content, "USER_SETTINGS_CHANGE")
        .into_iter()
        .next();
    let metadata = extract_tagged_blocks(content, "ADDITIONAL_METADATA")
        .into_iter()
        .next();
    let body = if requests.is_empty() {
        content.trim().to_string()
    } else {
        requests.join("\n\n")
    };
    (body, settings, metadata)
}

/// Pull the model name out of a `<USER_SETTINGS_CHANGE>` body such as
/// "The user changed setting `Model Selection` from None to Gemini 3.1 Pro
/// (High). No need to comment…" → `Some("Gemini 3.1 Pro (High)")`.
pub(super) fn model_from_settings(settings: &str) -> Option<String> {
    // agy phrases this as "... from <old> to <model>. ...". Anchor on " from "
    // and take the FIRST " to " AFTER it, so a " to " appearing earlier (e.g. in
    // a setting name, or a non-None previous value containing " to ") cannot
    // hijack the parse. Fall back to the first " to " when no " from " is present.
    let from_marker = " from ";
    let to_marker = " to ";
    let search_from = settings
        .find(from_marker)
        .map_or(0, |i| i + from_marker.len());
    let rel = settings[search_from..].find(to_marker)?;
    let start = search_from + rel + to_marker.len();
    let tail = &settings[start..];
    let end = tail.find(". ").unwrap_or(tail.len());
    let model = tail[..end].trim().trim_end_matches('.').trim();
    if model.is_empty() || model.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(model.to_string())
}

/// The first model recorded by any `<USER_SETTINGS_CHANGE>` in the transcript.
pub(super) fn detect_model(records: &[Value]) -> Option<String> {
    for rec in records {
        if rec.get("type").and_then(Value::as_str) != Some("USER_INPUT") {
            continue;
        }
        let content = rec.get("content").and_then(Value::as_str).unwrap_or("");
        if let Some(settings) = extract_tagged_blocks(content, "USER_SETTINGS_CHANGE")
            .into_iter()
            .next()
        {
            if let Some(model) = model_from_settings(&settings) {
                return Some(model);
            }
        }
    }
    None
}

/// `content` with every `<TAG>…</TAG>` span removed. Unterminated opens keep
/// their remainder verbatim.
fn strip_tagged_blocks(content: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        rest = &after[end + close.len()..];
    }
    out.push_str(rest);
    out
}

/// A single workspace path candidate: trimmed, unquoted, `file://` URIs
/// unwrapped, and required to be an absolute path — Unix `/...` (never the
/// bare root `/`) or a Windows drive `X:\...` / `X:/...` (never the bare
/// drive root). Anything inside a `.gemini` directory is the agent's own
/// store, never a user workspace.
fn workspace_path_candidate(raw: &str) -> Option<PathBuf> {
    let mut path = raw
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .trim();
    if let Some(rest) = path.strip_prefix("file://") {
        path = rest.trim();
        // A `file:///C:/...`-style URI leaves a leading slash on the drive.
        if let Some(rest) = path.strip_prefix('/') {
            let b = rest.as_bytes();
            if b.len() > 1 && b[0].is_ascii_alphabetic() && b[1] == b':' {
                path = rest;
            }
        }
    }
    let bytes = path.as_bytes();
    let is_unix_abs = bytes.first() == Some(&b'/') && bytes.len() > 1;
    let is_win_drive = bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    if !is_unix_abs && !is_win_drive {
        return None;
    }
    if path
        .replace('\\', "/")
        .split('/')
        .any(|seg| seg == ".gemini")
    {
        return None;
    }
    Some(PathBuf::from(path))
}

/// Parse one `<user_information>` body for the session's workspace. Two
/// host-stamped markers carry it: the `Active Workspaces:` section (its
/// first `- <path>` entry — the workspace folder list, the same notion
/// `conversations.workspace_id` groups by) and `Command Working Directory:
/// <path>` (the session cwd, the fallback when no list is stamped). Nothing
/// else in the block is interpreted.
pub(super) fn workspace_from_user_information(info: &str) -> Option<PathBuf> {
    let mut cwd = None;
    let mut first_workspace = None;
    let mut in_workspaces = false;
    for line in info.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_list_item = trimmed.starts_with('-');
        // Unwrap lightweight list/markdown dressing so `Active Workspaces:`,
        // `- Active Workspaces:` and `**Active Workspaces**:` all register.
        let entry = trimmed
            .trim_start_matches('-')
            .trim_start()
            .trim_matches('*')
            .trim();
        if let Some(rest) = entry.strip_prefix("Command Working Directory:") {
            if cwd.is_none() {
                cwd = workspace_path_candidate(rest);
            }
            in_workspaces = false;
        } else if entry == "Active Workspaces:" {
            in_workspaces = true;
        } else if in_workspaces && is_list_item {
            if first_workspace.is_none() {
                first_workspace = workspace_path_candidate(entry);
            }
        } else if !is_list_item {
            in_workspaces = false;
        }
    }
    first_workspace.or(cwd)
}

/// The conversation's workspace, read ONLY from host-stamped
/// `<user_information>` metadata: a `<USER_INPUT>`/`SYSTEM` record's
/// `content` or `prompt` field. For `USER_INPUT` records the `<USER_REQUEST>`
/// bodies are removed first — the request text is user-typed, so a
/// `<user_information>` example pasted into it is not evidence. Paths are
/// never inferred from tool-call arguments or message text: an unrelated
/// `read_file` must not mint a wrong `conversations.workspace_id`.
pub(super) fn extract_workspace_from_records(records: &[Value]) -> Option<PathBuf> {
    for rec in records {
        let is_user_input = rec.get("type").and_then(Value::as_str) == Some("USER_INPUT");
        let is_system = rec.get("source").and_then(Value::as_str) == Some("SYSTEM");
        if !is_user_input && !is_system {
            continue;
        }
        for key in ["content", "prompt"] {
            let Some(raw) = rec.get(key).and_then(Value::as_str) else {
                continue;
            };
            let stripped;
            let text = if is_user_input {
                stripped = strip_tagged_blocks(raw, "USER_REQUEST");
                &stripped
            } else {
                raw
            };
            for block in extract_tagged_blocks(text, "user_information") {
                if let Some(workspace) = workspace_from_user_information(&block) {
                    return Some(workspace);
                }
            }
        }
    }
    None
}

/// Parse `tool_calls` into normalized invocations. When the array is absent we
/// still synthesize one invocation named after the step `type` (e.g.
/// `VIEW_FILE` → `view_file`) so the tool remains searchable.
pub(super) fn parse_tool_calls(
    tool_calls: Option<&Value>,
    step_type: &str,
) -> Vec<NormalizedInvocation> {
    let synthesized_name = step_type.to_ascii_lowercase();
    let Some(arr) = tool_calls
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return vec![NormalizedInvocation {
            kind: "tool".to_string(),
            name: synthesized_name,
            raw_name: None,
            call_id: None,
            arguments: None,
        }];
    };

    let arr = arr
        .iter()
        .map(|tc| {
            let name = tc
                .get("name")
                .or_else(|| tc.get("tool_name"))
                .or_else(|| tc.pointer("/function/name"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map_or_else(|| synthesized_name.clone(), String::from);
            let call_id = tc
                .get("id")
                .or_else(|| tc.get("call_id"))
                .or_else(|| tc.get("tool_call_id"))
                .and_then(Value::as_str)
                .map(String::from);
            let arguments = tc
                .get("arguments")
                .or_else(|| tc.get("args"))
                .or_else(|| tc.get("input"))
                .or_else(|| tc.pointer("/function/arguments"))
                .cloned()
                .map(|raw| match raw {
                    // agy may store arguments as a JSON string; parse if possible.
                    Value::String(s) => {
                        serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
                    }
                    other => other,
                });
            NormalizedInvocation {
                kind: "tool".to_string(),
                name,
                raw_name: None,
                call_id,
                arguments,
            }
        })
        .collect::<Vec<_>>();
    let mut invocations = arr;
    unwrap_skill_invocations(&mut invocations);
    invocations
}

/// Source fields passed through into message extras verbatim (cass F10),
/// plus the full raw record archive (cass F14, owner direction 2026-11-08:
/// keep the raw source record rather than lossy normalization — the F12
/// precedent applied to the whole record).
///
/// The canonical mapping below extracts the text-bearing fields and used to
/// drop every other field on the floor; these four carry signals cass needs
/// to tell truncated sessions from clean ones and to verify tool exit/error:
///
/// - `truncated_fields` — the source-side truncation markers (which fields
///   were cut before persistence; the only signal separating truncated
///   sessions from clean ones),
/// - `status` — the record's lifecycle state (`DONE`/`RUNNING`/`ERROR`/
///   `INVALID`),
/// - `error` — the failure text (`ERROR_MESSAGE` and `GENERIC` records),
/// - `media` — attachment references (`{mime_type, uri}`).
///
/// They ride along under their source names, only when the source record
/// actually carries them (absent or `null` fields add no key).
///
/// F14 additionally archives the COMPLETE raw transcript record under
/// `agy_raw`, verbatim, on every emitted message — so cass can verify its
/// block inventory from the record the connector actually read instead of
/// guessing at what normalization dropped. The archive is unconditional
/// (even a record with no passthrough keys keeps its raw form); it is the
/// record that produced THIS message, cloned before any extraction.
fn insert_passthrough_extras(rec: &Value, extra: &mut Map<String, Value>) {
    for key in ["truncated_fields", "status", "error", "media"] {
        if let Some(value) = rec.get(key).filter(|v| !v.is_null()) {
            extra.insert(key.to_string(), value.clone());
        }
    }
    // cass F14: the archived raw source record, in full.
    extra.insert("agy_raw".to_string(), rec.clone());
}

/// Build a [`NormalizedMessage`] (idx assigned later by `reindex_messages`).
fn message(
    role: &str,
    author: Option<&str>,
    created_at: Option<i64>,
    content: String,
    extra: Map<String, Value>,
    invocations: Vec<NormalizedInvocation>,
) -> NormalizedMessage {
    NormalizedMessage {
        idx: 0,
        role: role.to_string(),
        author: author.map(String::from),
        created_at,
        content,
        extra: Value::Object(extra),
        invocations,
        snippets: Vec::new(),
    }
}

/// Map a single transcript record to a normalized message, or `None` for records
/// that carry no timeline content (e.g. `CONVERSATION_HISTORY` markers).
#[allow(clippy::too_many_lines)]
pub(super) fn record_to_message(rec: &Value) -> Option<NormalizedMessage> {
    let source = rec.get("source").and_then(Value::as_str).unwrap_or("");
    let step_type = rec.get("type").and_then(Value::as_str).unwrap_or("");
    let created = rec.get("created_at").and_then(parse_timestamp);
    let content = rec
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let thinking = rec
        .get("thinking")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let tool_calls = rec.get("tool_calls").filter(|v| v.is_array());

    match step_type {
        "USER_INPUT" => {
            let (body, settings, metadata) = extract_user_request(&content);
            // An attachment-only turn (empty <USER_REQUEST> with `media`) is
            // real user input in native — emit it with an empty body rather
            // than dropping the record whole.
            let has_media = rec
                .get("media")
                .and_then(Value::as_array)
                .is_some_and(|media| !media.is_empty());
            if body.trim().is_empty() && !has_media {
                return None;
            }
            let mut extra = Map::new();
            if let Some(s) = settings {
                extra.insert("settings_change".to_string(), Value::String(s));
            }
            if let Some(m) = metadata {
                extra.insert("additional_metadata".to_string(), Value::String(m));
            }
            insert_passthrough_extras(rec, &mut extra);
            Some(message("user", None, created, body, extra, Vec::new()))
        }
        "PLANNER_RESPONSE" => {
            // Local fix (cass review 2026-11-02, F3).
            let invocations = planner_calls::explicit_planner_invocations(rec.get("tool_calls"));
            if content.trim().is_empty() && thinking.is_none() && invocations.is_empty() {
                return None;
            }
            let body = if content.trim().is_empty() && thinking.is_none() {
                planner_calls::tool_call_body(&invocations)
            } else {
                content
            };
            let mut extra = Map::new();
            if let Some(t) = thinking {
                extra.insert("thinking".to_string(), Value::String(t));
            }
            if !invocations.is_empty() {
                extra.insert(
                    "cass".to_string(),
                    planner_calls::invocations_extra(&invocations),
                );
            }
            insert_passthrough_extras(rec, &mut extra);
            Some(message(
                "assistant",
                None,
                created,
                body,
                extra,
                invocations,
            ))
        }
        // cass F10 (item 5): a marker record — but when it carries content,
        // that text must reach the packet instead of vanishing with the
        // record. The content-less marker form still emits no message.
        "CONVERSATION_HISTORY" => {
            if content.trim().is_empty() {
                return None;
            }
            let mut extra = Map::new();
            extra.insert("agy_type".to_string(), Value::String(step_type.to_string()));
            if !source.is_empty() {
                extra.insert("agy_source".to_string(), Value::String(source.to_string()));
            }
            insert_passthrough_extras(rec, &mut extra);
            Some(message("system", None, created, content, extra, Vec::new()))
        }
        // cass F10 (item 2): ERROR_MESSAGE records with no `content` carry
        // the failure text in `error` and were dropped whole; emit that text
        // as the message content so the record stops vanishing. Content-
        // bearing records keep their content byte-for-byte (the envelope
        // shape they already had via the fall-through below).
        "ERROR_MESSAGE" => {
            let body = if content.trim().is_empty() {
                rec.get("error")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())?
                    .to_string()
            } else {
                content
            };
            let mut extra = Map::new();
            extra.insert("agy_type".to_string(), Value::String(step_type.to_string()));
            if !source.is_empty() {
                extra.insert("agy_source".to_string(), Value::String(source.to_string()));
            }
            insert_passthrough_extras(rec, &mut extra);
            Some(message("system", None, created, body, extra, Vec::new()))
        }
        "EPHEMERAL_MESSAGE" | "SYSTEM_MESSAGE" => {
            if content.trim().is_empty() {
                return None;
            }
            let mut extra = Map::new();
            extra.insert("agy_type".to_string(), Value::String(step_type.to_string()));
            let author = if step_type == "EPHEMERAL_MESSAGE" {
                "ephemeral"
            } else {
                "system"
            };
            insert_passthrough_extras(rec, &mut extra);
            Some(message(
                "system",
                Some(author),
                created,
                content,
                extra,
                Vec::new(),
            ))
        }
        _ => {
            // A MODEL step that is not a planner response is a tool action/result
            // (VIEW_FILE, EDIT_FILE, RUN_COMMAND, …, or an unknown future kind).
            if source == "MODEL" {
                // Local fix (cass review 2026-11-02, F3): a GENERIC record without
                // calls is the planner call's result, not a tool named "generic".
                let invocations = if tool_calls.is_none()
                    && planner_calls::RESULT_STEP_TYPES.contains(&step_type)
                {
                    Vec::new()
                } else {
                    parse_tool_calls(tool_calls, step_type)
                };
                // An error-only step (e.g. a failed GENERIC result with no
                // content, thinking or calls) lifts its `error` text as the
                // message content, the same way ERROR_MESSAGE does.
                let body =
                    if content.trim().is_empty() && thinking.is_none() && invocations.is_empty() {
                        rec.get("error")
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())?
                            .to_string()
                    } else {
                        content
                    };
                let mut extra = Map::new();
                extra.insert("agy_type".to_string(), Value::String(step_type.to_string()));
                if let Some(t) = thinking {
                    extra.insert("thinking".to_string(), Value::String(t));
                }
                if !invocations.is_empty() {
                    extra.insert(
                        "cass".to_string(),
                        serde_json::json!({ "tool_call_count": invocations.len() }),
                    );
                }
                let author = step_type.to_ascii_lowercase();
                insert_passthrough_extras(rec, &mut extra);
                Some(message(
                    "tool",
                    Some(author.as_str()),
                    created,
                    body,
                    extra,
                    invocations,
                ))
            } else {
                // Unknown non-MODEL kind: preserve as a system message rather than
                // dropping it. Never crash on an unrecognized schema.
                if content.trim().is_empty() {
                    return None;
                }
                let mut extra = Map::new();
                extra.insert("agy_type".to_string(), Value::String(step_type.to_string()));
                if !source.is_empty() {
                    extra.insert("agy_source".to_string(), Value::String(source.to_string()));
                }
                insert_passthrough_extras(rec, &mut extra);
                Some(message("system", None, created, content, extra, Vec::new()))
            }
        }
    }
}

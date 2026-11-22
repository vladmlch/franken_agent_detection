use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::CodexConnector;
use super::discovery::RolloutFormat;
use super::reader;
use super::subagent::{SubagentEvidence, classify_session_meta};
use super::token_usage_accumulation;
use super::user_prompts;
use crate::connectors::scan::ScanContext;
use crate::connectors::utils::read_capped;
use crate::connectors::{
    extract_invocations_from_content_blocks, flatten_content, parse_timestamp,
    unwrap_skill_invocations,
};
use crate::types::{NormalizedInvocation, NormalizedMessage};

impl CodexConnector {
    pub(super) fn is_token_usage_target_message(message: &NormalizedMessage) -> bool {
        // Attribute token_count usage to concrete assistant turns only.
        // This avoids attaching usage to synthetic reasoning helper
        // messages AND to the synthetic "[Tool: name]" placeholders pushed
        // for function_call items — a token_count following a tool call
        // belongs to the surrounding turn, not the placeholder.
        message.role == "assistant" && message.author.is_none() && message.invocations.is_empty()
    }

    pub(super) fn token_usage_from_payload(payload: &Value) -> Option<Value> {
        // Modern rollouts (verified against live 2026-01..2026-08 history)
        // nest per-turn usage at `info.last_token_usage`;
        // `info.total_token_usage` is CUMULATIVE across the session and
        // must never be attached per turn (downstream sums would double
        // count). Legacy shapes carried the fields directly on the payload.
        let usage_block = payload
            .pointer("/info/last_token_usage")
            .filter(|block| {
                block.get("input_tokens").is_some() || block.get("output_tokens").is_some()
            })
            .unwrap_or(payload);

        let input_tokens = usage_block.get("input_tokens").and_then(Value::as_i64);
        let output_tokens = usage_block
            .get("output_tokens")
            .and_then(Value::as_i64)
            .or_else(|| usage_block.get("tokens").and_then(Value::as_i64));

        if input_tokens.is_none() && output_tokens.is_none() {
            return None;
        }

        let mut usage = serde_json::Map::new();
        if let Some(input) = input_tokens {
            usage.insert("input_tokens".to_string(), Value::from(input));
        }
        if let Some(output) = output_tokens {
            usage.insert("output_tokens".to_string(), Value::from(output));
        }
        // Codex reports prompt-cache hits as `cached_input_tokens`, a
        // SUBSET of input_tokens — NOT additive like Anthropic's
        // cache_read/cache_creation split. Remapping it onto
        // `cache_read_tokens` would make downstream totals double-count
        // the cached portion, so the field is preserved verbatim instead
        // (unknown keys are ignored by total computation).
        if let Some(cache) = usage_block
            .get("cached_input_tokens")
            .and_then(Value::as_i64)
        {
            usage.insert("cached_input_tokens".to_string(), Value::from(cache));
        }
        usage.insert("data_source".to_string(), Value::String("api".to_string()));

        Some(Value::Object(usage))
    }

    pub(super) fn attach_token_usage_to_latest_assistant(
        messages: &mut [NormalizedMessage],
        token_usage: Value,
        source_path: &Path,
        line_number: usize,
    ) {
        if let Some(target) = messages
            .iter_mut()
            .rev()
            .find(|m| Self::is_token_usage_target_message(m))
        {
            if !target.extra.is_object() {
                target.extra = Value::Object(serde_json::Map::new());
            }

            if let Some(extra) = target.extra.as_object_mut() {
                let cass = extra
                    .entry("cass".to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));

                if !cass.is_object() {
                    *cass = Value::Object(serde_json::Map::new());
                }

                if let Some(cass_obj) = cass.as_object_mut() {
                    // Multiple token_count events for the same assistant turn:
                    // deterministic rule = last write wins.
                    cass_obj.insert("token_usage".to_string(), token_usage);
                }
            }
        } else {
            tracing::debug!(
                path = %source_path.display(),
                line_number,
                "codex token_count event had no preceding assistant message; skipping"
            );
        }
    }

    /// Local fix (cass review 2026-11-02, F6): like
    /// `attach_token_usage_to_latest_assistant`, but sums onto existing usage.
    pub(super) fn accumulate_token_usage_on_latest_assistant(
        messages: &mut [NormalizedMessage],
        token_usage: Value,
        source_path: &Path,
        line_number: usize,
    ) {
        let existing = messages
            .iter()
            .rev()
            .find(|m| Self::is_token_usage_target_message(m))
            .and_then(|m| m.extra.pointer("/cass/token_usage"))
            .cloned();
        let merged =
            token_usage_accumulation::accumulate_token_usage(existing.as_ref(), token_usage);
        Self::attach_token_usage_to_latest_assistant(messages, merged, source_path, line_number);
    }
}

pub(super) fn update_time_bounds(
    started_at: &mut Option<i64>,
    ended_at: &mut Option<i64>,
    ts: Option<i64>,
) {
    if let Some(ts) = ts {
        *started_at = Some(started_at.map_or(ts, |curr| curr.min(ts)));
        *ended_at = Some(ended_at.map_or(ts, |curr| curr.max(ts)));
    }
}

/// Parse the arguments of a modern Codex `response_item` tool call.
///
/// `function_call` payloads carry `arguments` as a JSON-encoded string (e.g.
/// `"{\"cmd\":\"ls\"}"`); `custom_tool_call` payloads (e.g. `apply_patch`) carry
/// freeform `input`. Returns the decoded JSON when the string parses, otherwise
/// the raw string, so downstream consumers never lose the original payload.
pub(super) fn parse_tool_call_arguments(payload: &Value) -> Option<Value> {
    let raw = payload.get("arguments").or_else(|| payload.get("input"))?;
    match raw {
        Value::String(s) if !s.is_empty() => {
            Some(serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone())))
        }
        other => Some(other.clone()),
    }
}

/// Extract the textual output of a modern Codex `response_item` tool result.
///
/// `output` is almost always a plain string, but the Responses API can also
/// nest it as `{"content":[{"text":...}]}`; handle both shapes and fall back to
/// flattening so structured results still surface as searchable text.
pub(super) fn tool_output_text(payload: &Value) -> String {
    let Some(output) = payload.get("output") else {
        return String::new();
    };
    if let Some(text) = output.as_str() {
        return text.to_string();
    }
    if let Some(content) = output.get("content") {
        let flattened = flatten_content(content);
        if !flattened.trim().is_empty() {
            return flattened;
        }
    }
    flatten_content(output)
}

/// One segment's records, parsed with its own prompt-dedup and token state:
/// the first keys on adjacent physical record positions, the second on a
/// per-file cumulative total, so neither can carry across a file boundary.
pub(super) struct ParsedSegment {
    pub(super) messages: Vec<NormalizedMessage>,
    pub(super) started_at: Option<i64>,
    pub(super) ended_at: Option<i64>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) subagent: Option<SubagentEvidence>,
}

#[allow(clippy::too_many_lines)]
pub(super) fn parse_rollout_segment(
    path: &Path,
    format: RolloutFormat,
    ctx: &ScanContext,
) -> Result<Option<ParsedSegment>> {
    let mut messages = Vec::new();
    let mut started_at = None;
    let mut ended_at = None;
    let mut session_cwd: Option<PathBuf> = None;
    let mut subagent: Option<SubagentEvidence> = None;
    // Pair individual records, not all occurrences of prompt text.
    // Capture stream/turn identity before the records normalize.
    let mut user_prompts = user_prompts::UserPrompts::default();
    let mut token_count_tracker = token_usage_accumulation::TokenCountTracker::default();

    if matches!(
        format,
        RolloutFormat::Jsonl | RolloutFormat::CompressedJsonl
    ) {
        let mut reader = reader::RolloutReader::open(
            path,
            format == RolloutFormat::CompressedJsonl,
            ctx.progress_tick.as_deref(),
        )?;

        // Accumulate privately until the complete opened snapshot is validated.
        while let Some((line_idx, val)) = reader.next_record()? {
            let entry_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let created = val.get("timestamp").and_then(parse_timestamp);

            match entry_type {
                "session_meta" => {
                    if let Some(payload) = val.get("payload").filter(|v| v.is_object()) {
                        session_cwd = payload
                            .get("cwd")
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from);
                        // First header in the segment wins, mirroring the
                        // native read of the rollout's leading records.
                        if subagent.is_none() {
                            subagent = Some(classify_session_meta(payload));
                        }
                    }
                    update_time_bounds(&mut started_at, &mut ended_at, created);
                }
                "response_item" => {
                    if let Some(payload) = val.get("payload") {
                        let payload_type = payload.get("type").and_then(|v| v.as_str());

                        match payload_type {
                            // Modern Codex encodes tool calls as
                            // `response_item` entries rather than
                            // `event_msg`/`tool_call`. `function_call`
                            // is a structured tool (e.g. `exec_command`);
                            // `custom_tool_call` is a freeform tool
                            // (e.g. `apply_patch`). Both lack a `content`
                            // field, so without explicit handling they
                            // flatten to empty and get dropped.
                            Some("function_call" | "custom_tool_call") => {
                                let tool_name = payload
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                let arguments = parse_tool_call_arguments(payload);
                                let call_id = payload
                                    .get("call_id")
                                    .or_else(|| payload.get("id"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from);

                                let content_text = format!("[Tool: {tool_name}]");
                                update_time_bounds(&mut started_at, &mut ended_at, created);
                                let mut invocations = vec![NormalizedInvocation {
                                    kind: "tool".to_string(),
                                    name: tool_name,
                                    raw_name: None,
                                    call_id,
                                    arguments,
                                }];
                                unwrap_skill_invocations(&mut invocations);
                                messages.push(NormalizedMessage {
                                    idx: 0,
                                    role: "assistant".to_string(),
                                    author: None,
                                    created_at: created,
                                    content: content_text,
                                    extra: val,
                                    invocations,
                                    snippets: Vec::new(),
                                });
                            }
                            // Tool results: `output` carries the captured
                            // stdout / patch summary. Emit as a
                            // first-class `tool` timeline entry; the
                            // linking `call_id` is preserved in `extra`.
                            // F13: a textless output WITH identity
                            // (`call_id`/`id` — image-only results whose
                            // `output` is a list of `image_url` parts) is
                            // local evidence too: emit it as a `tool`
                            // message with EMPTY content and the raw
                            // record as `extra`, so call/result linking
                            // can pair it — never a fabricated result
                            // text. Identity-less textless records stay
                            // dropped: there is nothing to link them to.
                            Some("function_call_output" | "custom_tool_call_output") => {
                                let output_text = tool_output_text(payload);
                                let has_identity = payload
                                    .get("call_id")
                                    .or_else(|| payload.get("id"))
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|id| !id.trim().is_empty());
                                if output_text.trim().is_empty() && !has_identity {
                                    continue;
                                }
                                update_time_bounds(&mut started_at, &mut ended_at, created);
                                messages.push(NormalizedMessage {
                                    idx: 0,
                                    role: "tool".to_string(),
                                    author: None,
                                    created_at: created,
                                    content: output_text,
                                    extra: val,
                                    invocations: Vec::new(),
                                    snippets: Vec::new(),
                                });
                            }
                            // Plain messages: assistant `output_text`,
                            // user/developer `input_text`, or legacy
                            // string content. Encrypted `reasoning`
                            // items have no plaintext content and are
                            // intentionally skipped here (plaintext
                            // reasoning arrives via `event_msg`).
                            _ => {
                                let role = payload
                                    .get("role")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("agent");

                                let content_str = payload
                                    .get("content")
                                    .map(flatten_content)
                                    .unwrap_or_default();

                                if content_str.trim().is_empty() {
                                    continue;
                                }

                                update_time_bounds(&mut started_at, &mut ended_at, created);
                                if role == "user" {
                                    user_prompts.observe(
                                        user_prompts::Stream::Response,
                                        line_idx,
                                        messages.len(),
                                        &content_str,
                                        created,
                                        payload.get("turn_id").and_then(Value::as_str),
                                    );
                                }
                                let mut invocations = payload
                                    .get("content")
                                    .map_or_else(Vec::new, extract_invocations_from_content_blocks);
                                unwrap_skill_invocations(&mut invocations);

                                messages.push(NormalizedMessage {
                                    idx: 0,
                                    role: role.to_string(),
                                    author: None,
                                    created_at: created,
                                    content: content_str,
                                    extra: val,
                                    invocations,
                                    snippets: Vec::new(),
                                });
                            }
                        }
                    }
                }
                "event_msg" => {
                    if let Some(payload) = val.get("payload") {
                        let event_type = payload.get("type").and_then(|v| v.as_str());

                        match event_type {
                            Some("user_message") => {
                                let text = payload
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if !text.is_empty() {
                                    update_time_bounds(&mut started_at, &mut ended_at, created);
                                    user_prompts.observe(
                                        user_prompts::Stream::Event,
                                        line_idx,
                                        messages.len(),
                                        text,
                                        created,
                                        payload.get("turn_id").and_then(Value::as_str),
                                    );
                                    messages.push(NormalizedMessage {
                                        idx: 0,
                                        role: "user".to_string(),
                                        author: None,
                                        created_at: created,
                                        content: text.to_string(),
                                        extra: val,
                                        invocations: Vec::new(),
                                        snippets: Vec::new(),
                                    });
                                }
                            }
                            Some("agent_reasoning") => {
                                let text =
                                    payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    update_time_bounds(&mut started_at, &mut ended_at, created);
                                    messages.push(NormalizedMessage {
                                        idx: 0,
                                        role: "assistant".to_string(),
                                        author: Some("reasoning".to_string()),
                                        created_at: created,
                                        content: text.to_string(),
                                        extra: val,
                                        invocations: Vec::new(),
                                        snippets: Vec::new(),
                                    });
                                }
                            }
                            Some("tool_call") => {
                                // Codex event_msg/tool_call events carry structured
                                // tool data that should produce invocations.
                                let tool_name = payload
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                let arguments = payload
                                    .get("input")
                                    .or_else(|| payload.get("arguments"))
                                    .cloned();
                                let call_id = payload
                                    .get("call_id")
                                    .or_else(|| payload.get("id"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from);

                                let content_text = format!("[Tool: {tool_name}]");
                                update_time_bounds(&mut started_at, &mut ended_at, created);
                                let mut invocations = vec![NormalizedInvocation {
                                    kind: "tool".to_string(),
                                    name: tool_name,
                                    raw_name: None,
                                    call_id,
                                    arguments,
                                }];
                                unwrap_skill_invocations(&mut invocations);
                                messages.push(NormalizedMessage {
                                    idx: 0,
                                    role: "assistant".to_string(),
                                    author: None,
                                    created_at: created,
                                    content: content_text,
                                    extra: val,
                                    invocations,
                                    snippets: Vec::new(),
                                });
                            }
                            Some("token_count") => {
                                if let Some(token_usage) =
                                    CodexConnector::token_usage_from_payload(payload)
                                {
                                    match token_count_tracker.classify(payload) {
                                        token_usage_accumulation::TokenCountKind::Repeat => {}
                                        token_usage_accumulation::TokenCountKind::NewRequest => {
                                            CodexConnector::accumulate_token_usage_on_latest_assistant(
                                                        &mut messages, token_usage, path, line_idx + 1,
                                                    );
                                        }
                                        token_usage_accumulation::TokenCountKind::Legacy => {
                                            CodexConnector::attach_token_usage_to_latest_assistant(
                                                &mut messages,
                                                token_usage,
                                                path,
                                                line_idx + 1,
                                            );
                                        }
                                    }
                                } else {
                                    tracing::debug!(
                                        path = %path.display(),
                                        line_number = line_idx + 1,
                                        "codex token_count event missing token fields; skipping"
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        user_prompts.finish(&mut messages);
    } else {
        // Legacy single-file rollouts can be huge; enforce the
        // project's 100MB scan cap (chatgpt policy).
        let content = match read_capped(path) {
            Ok(Some(content)) => content,
            Ok(None) => {
                tracing::warn!(
                    file = %path.display(),
                    "codex: legacy rollout exceeds the scan size cap; skipping"
                );
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "codex: unreadable legacy rollout");
                return Ok(None);
            }
        };
        ctx.tick();
        let content = content.trim_start_matches('\u{feff}');
        let val: Value = match serde_json::from_str(content) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        session_cwd = val
            .get("session")
            .and_then(|s| s.get("cwd"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        if let Some(items) = val.get("items").and_then(|v| v.as_array()) {
            for item in items {
                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("agent");
                let content_str = item.get("content").map(flatten_content).unwrap_or_default();

                if content_str.trim().is_empty() {
                    continue;
                }

                let created = item.get("timestamp").and_then(parse_timestamp);
                update_time_bounds(&mut started_at, &mut ended_at, created);

                let mut invocations = item
                    .get("content")
                    .map_or_else(Vec::new, extract_invocations_from_content_blocks);
                unwrap_skill_invocations(&mut invocations);

                messages.push(NormalizedMessage {
                    idx: 0,
                    role: role.to_string(),
                    author: None,
                    created_at: created,
                    content: content_str,
                    extra: item.clone(),
                    invocations,
                    snippets: Vec::new(),
                });
            }
        }
    }

    Ok(Some(ParsedSegment {
        messages,
        started_at,
        ended_at,
        cwd: session_cwd,
        subagent,
    }))
}

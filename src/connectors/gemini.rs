use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::json_stream::{
    JsonContainerKind, drive_seq_stream, peek_json_container, stream_json_array_after_peek,
};
use super::jsonl::JsonlLines;
use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{
    MAX_SCAN_FILE_BYTES, env_path_nonempty, excluded_scan_paths_from_env,
    is_injected_context_message, path_is_excluded, unwrap_skill_invocations,
};
use super::{
    Connector, file_modified_since, flatten_content, franken_detection_for_connector,
    parse_timestamp,
};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
};

/// Final JSONL history in insertion order, with one retained value per native ID.
/// Replacing a streamed message drops its previous payload immediately. The
/// separate order index also lets rewind and reorder move values without
/// cloning transcript bodies or keeping a slot for every historical update.
#[derive(Default)]
struct ReplayedMessages {
    ordered: BTreeMap<usize, Value>,
    by_id: HashMap<String, usize>,
    next_order: usize,
}

impl ReplayedMessages {
    fn upsert(&mut self, message: Value) {
        // Snapshot arrays contain message records, not patch events. In
        // particular, an ID on a patch must never replace the target body.
        if message.get("$patch").is_some() {
            return;
        }
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            if let Some(&order) = self.by_id.get(id) {
                self.ordered.insert(order, message);
                return;
            }
            self.by_id.insert(id.to_owned(), self.next_order);
        }
        // Historical exports can contain messages without IDs. Keep those
        // separate; neither their text nor their token counts identify a reply.
        self.ordered.insert(self.next_order, message);
        self.next_order += 1;
    }

    fn clear(&mut self) {
        self.ordered.clear();
        self.by_id.clear();
        self.next_order = 0;
    }

    fn remove(&mut self, id: &str) -> Option<Value> {
        self.by_id
            .remove(id)
            .and_then(|order| self.ordered.remove(&order))
    }

    fn rewind(&mut self, id: &str) {
        let Some(&order) = self.by_id.get(id) else {
            self.clear();
            return;
        };
        // Gemini removes the target itself as well as every later message.
        for message in self.ordered.split_off(&order).into_values() {
            if let Some(id) = message.get("id").and_then(Value::as_str) {
                self.by_id.remove(id);
            }
        }
    }

    fn patch_message(&mut self, mut patch: Map<String, Value>) {
        let Some(message) = patch
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.by_id.get(id))
            .and_then(|order| self.ordered.get_mut(order))
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        if let Some(content) = patch.remove("content") {
            message.insert("content".to_owned(), content);
        }
        if message.get("type").and_then(Value::as_str) != Some("gemini") {
            return;
        }
        let (Some(Value::Array(updates)), Some(Value::Array(calls))) =
            (patch.remove("toolCalls"), message.get_mut("toolCalls"))
        else {
            return;
        };
        for update in updates {
            let Value::Object(mut update) = update else {
                continue;
            };
            let Some(id) = update.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(call) = calls
                .iter_mut()
                .find(|call| call.get("id").and_then(Value::as_str) == Some(id))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            // Native patches only replace existing tool results. Names,
            // arguments, status and unrelated message fields stay intact.
            if let Some(result) = update.remove("result") {
                call.insert("result".to_owned(), result);
            }
        }
    }

    fn apply_patch(&mut self, mut patch: Map<String, Value>) {
        // A batch can also contain a single-message patch. Apply controls in
        // the same order as Gemini's loader: single, updates, remove, reorder.
        let updates = patch.remove("updates");
        let removals = patch.remove("removeIds");
        let ordering = patch.remove("orderIds");
        self.patch_message(patch);
        if let Some(Value::Array(updates)) = updates {
            for update in updates {
                if let Value::Object(update) = update {
                    self.patch_message(update);
                }
            }
        }
        if let Some(Value::Array(ids)) = removals {
            for id in ids.iter().filter_map(Value::as_str) {
                self.remove(id);
            }
        }
        if let Some(Value::Array(ids)) = ordering {
            // Listed surviving IDs move to the end; unlisted messages retain
            // their order. Removing first naturally ignores duplicate IDs.
            let moved: Vec<Value> = ids
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|id| self.remove(id))
                .collect();
            for message in moved {
                self.upsert(message);
            }
        }
    }

    fn into_values(self) -> Vec<Value> {
        self.ordered.into_values().collect()
    }
}

/// Extract actual workspace path from message content.
/// Gemini stores sessions by hash, but messages often contain the actual project path
/// in patterns like "# AGENTS.md instructions for /data/projects/foo" or file paths.
fn extract_workspace_from_content(messages: &[NormalizedMessage]) -> Option<PathBuf> {
    // Patterns to look for workspace paths in messages:
    // 1. AGENTS.md header: "# AGENTS.md instructions for /path/to/project"
    // 2. Working directory: "Working directory: /path/to/project"
    // 3. Common project paths: /data/projects/X

    // Limit to first 50 messages for performance
    for msg in messages.iter().take(50) {
        // Try AGENTS.md pattern first (most reliable)
        // Pattern: "AGENTS.md instructions for /path/to/project"
        if let Some(idx) = msg.content.find("AGENTS.md instructions for ") {
            let start = idx + "AGENTS.md instructions for ".len();
            if let Some(path) = extract_path_from_position(&msg.content, start) {
                return Some(path);
            }
        }

        // Try working directory pattern
        // Pattern: "Working directory: /path/to/project"
        if let Some(idx) = msg.content.find("Working directory:") {
            let start = idx + "Working directory:".len();
            if let Some(path) = extract_path_from_position(&msg.content, start) {
                return Some(path);
            }
        }
    }

    // Fallback: look for common project path patterns in first few messages
    for msg in messages.iter().take(5) {
        // Look for /data/projects/ paths
        if let Some(idx) = msg.content.find("/data/projects/")
            && let Some(path) = extract_path_from_position(&msg.content, idx)
        {
            return Some(path);
        }
    }

    None
}

/// Extract a path starting from the given position in the string.
/// Stops at whitespace, newlines, or common delimiters.
/// Returns the project directory (truncates at file extensions or deep paths).
fn extract_path_from_position(content: &str, start: usize) -> Option<PathBuf> {
    let rest = content.get(start..)?;
    let rest = rest.trim_start();

    // Handle quoted paths
    let (rest, delimiter) = rest.strip_prefix('"').map_or_else(
        || {
            rest.strip_prefix('\'')
                .map_or((rest, None), |stripped| (stripped, Some('\'')))
        },
        |stripped| (stripped, Some('"')),
    );

    // Find the end of the path
    let end = rest
        .find(|c: char| {
            delimiter.map_or_else(
                || {
                    c.is_whitespace()
                        || c == '>'
                        || c == '"'
                        || c == '\''
                        || c == ')'
                        || c == ']'
                        || c == ','
                },
                |d| c == d,
            )
        })
        .unwrap_or(rest.len());

    let path_str = rest.get(..end)?.trim_end_matches(['/', ':']);

    // Check for Unix absolute path OR Windows absolute path (C:\ or \\)
    let is_unix_abs = path_str.starts_with('/');
    let is_win_drive = path_str.len() >= 3
        && path_str
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && path_str.chars().nth(1) == Some(':')
        && (path_str.chars().nth(2) == Some('\\') || path_str.chars().nth(2) == Some('/'));
    let is_win_unc = path_str.starts_with("\\\\");

    if !is_unix_abs && !is_win_drive && !is_win_unc {
        return None;
    }

    if path_str.len() <= 3 {
        return None; // Too short to be a useful workspace path
    }

    // Normalize separators for cross-platform robustness (e.g. scanning Windows logs on Linux)
    // This ensures PathBuf::components() and parent() work correctly regardless of host OS.
    let clean_path_str = if is_win_drive || is_win_unc {
        path_str.replace('\\', "/")
    } else {
        path_str.to_string()
    };

    let path = PathBuf::from(clean_path_str);

    // If it looks like a file path (has extension), get the parent directory
    // Also if it's deeper than /data/projects/X or /home/user/projects/X, truncate
    let path = if path.extension().is_some() {
        path.parent()?.to_path_buf()
    } else {
        path
    };

    // For /data/projects/X/... paths, return just /data/projects/X
    if path.components().count() >= 4 {
        let path_str = path.to_string_lossy();
        if path_str.starts_with("/data/projects/") {
            // Return just /data/projects/project_name
            let parts: Vec<&str> = path_str.splitn(5, '/').collect();
            if parts.len() >= 4 {
                return Some(PathBuf::from(format!(
                    "/{}/{}/{}",
                    parts[1], parts[2], parts[3]
                )));
            }
        }
    }

    Some(path)
}

pub struct GeminiConnector;

impl Default for GeminiConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn root() -> PathBuf {
        if let Some(explicit) = env_path_nonempty("GEMINI_HOME") {
            return explicit;
        }
        dirs::home_dir().unwrap_or_default().join(".gemini/tmp")
    }

    fn is_session_file(path: &Path) -> bool {
        crate::is_gemini_session_file(path)
    }

    /// Find all legacy JSON and current event-sourced JSONL session files.
    /// Structure: `~/.gemini/tmp/<project>/chats/session-*.{json,jsonl}`.
    fn session_files(root: &Path) -> Vec<PathBuf> {
        if root.is_file() {
            return if Self::is_session_file(root) {
                vec![root.to_path_buf()]
            } else {
                Vec::new()
            };
        }

        let mut files = Vec::new();
        for entry in WalkDir::new(root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if Self::is_session_file(path) {
                files.push(path.to_path_buf());
            }
        }
        // Keep connector traversal deterministic across filesystems/runs.
        files.sort();
        files
    }

    fn looks_like_root(path: &Path) -> bool {
        Self::is_session_file(path)
            || path.join("chats").exists()
            || fs::read_dir(path)
                .is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| Self::is_session_file(&e.path()))))
            || fs::read_dir(path)
                .is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.path().join("chats").exists())))
    }

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if Self::looks_like_root(&ctx.data_dir) {
                vec![ScanRoot::local(ctx.data_dir.clone())]
            } else {
                vec![ScanRoot::local(Self::root())]
            }
        } else {
            let mut explicit = Vec::new();
            for scan_root in &ctx.scan_roots {
                if let Some(path) = scan_root.mirrored_store(&[".gemini", "tmp"])
                    && Self::looks_like_root(&path)
                {
                    explicit.push(scan_root.with_path(path));
                }
                let candidate = scan_root.path.join(".gemini/tmp");
                if Self::looks_like_root(&candidate) {
                    explicit.push(scan_root.with_path(candidate));
                }
                if Self::looks_like_root(&scan_root.path) {
                    explicit.push(scan_root.clone());
                }
                if scan_root.path.file_name().is_some_and(|n| n == ".gemini") {
                    let candidate = scan_root.path.join("tmp");
                    if Self::looks_like_root(&candidate) {
                        explicit.push(scan_root.with_path(candidate));
                    }
                }
            }
            if Self::looks_like_root(&ctx.data_dir) {
                explicit.push(ScanRoot::local(ctx.data_dir.clone()));
            }
            if ctx.data_dir.file_name().is_some_and(|n| n == ".gemini") {
                let candidate = ctx.data_dir.join("tmp");
                if Self::looks_like_root(&candidate) {
                    explicit.push(ScanRoot::local(candidate));
                }
            }
            explicit
        };

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut out = Vec::new();
        for root in Self::source_roots(ctx) {
            if !root.path.exists() || path_is_excluded(&root.path, &excluded_paths) {
                continue;
            }
            for file in Self::session_files(&root.path) {
                if path_is_excluded(&file, &excluded_paths)
                    || !file_modified_since(&file, ctx.since_ts)
                {
                    continue;
                }
                out.push(
                    DiscoveredSourceFile::new(
                        "gemini",
                        &root,
                        file,
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }
        Ok(out)
    }

    /// `$set.messages` replaces history; embedded header messages only upsert.
    fn apply_session_fields(
        session: &mut Map<String, Value>,
        messages: &mut ReplayedMessages,
        fields: Map<String, Value>,
        replace_messages: bool,
        file: &Path,
        line_number: usize,
    ) {
        for (key, value) in fields {
            if key == "messages" {
                match value {
                    Value::Array(snapshot) => {
                        if replace_messages {
                            messages.clear();
                        }
                        for message in snapshot {
                            messages.upsert(message);
                        }
                    }
                    _ => {
                        tracing::warn!(
                            path = %file.display(),
                            line = line_number,
                            "gemini: ignoring non-array messages snapshot"
                        );
                    }
                }
            } else {
                session.insert(key, value);
            }
        }
    }

    /// Replay the final Gemini CLI history, including replacements, patches
    /// and rewinds. See chatRecordingService.ts at gemini-cli 9b6e0265d16b.
    /// Malformed physical records are skipped; a real reader failure discards
    /// the whole source so an incomplete history cannot be marked complete.
    fn replay_jsonl_session(reader: impl BufRead, file: &Path) -> io::Result<Value> {
        let mut session = Map::new();
        let mut messages = ReplayedMessages::default();

        for (line_index, line_result) in JsonlLines::new(reader).enumerate() {
            let line_number = line_index + 1;
            let line = line_result?;
            let trimmed = line.trim().trim_start_matches('\u{feff}');
            if trimmed.is_empty() {
                continue;
            }

            let record = match serde_json::from_str::<Value>(trimmed) {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(
                        path = %file.display(),
                        line = line_number,
                        %error,
                        "gemini: skipping malformed JSONL line"
                    );
                    continue;
                }
            };
            let Value::Object(mut object) = record else {
                tracing::warn!(
                    path = %file.display(),
                    line = line_number,
                    "gemini: skipping non-object JSONL line"
                );
                continue;
            };

            // Event precedence follows Gemini's loader. A native message ID
            // identifies a whole replacement, not an additional content block.
            if let Some(id) = object.get("$rewindTo").and_then(Value::as_str) {
                messages.rewind(id);
            } else if object.get("$patch").is_some_and(Value::is_object) {
                if let Some(Value::Object(patch)) = object.remove("$patch") {
                    messages.apply_patch(patch);
                }
            } else if object.get("id").and_then(Value::as_str).is_some()
                && !object.contains_key("$patch")
            {
                messages.upsert(Value::Object(object));
            } else if object.get("$set").is_some_and(Value::is_object) {
                if let Some(Value::Object(fields)) = object.remove("$set") {
                    Self::apply_session_fields(
                        &mut session,
                        &mut messages,
                        fields,
                        true,
                        file,
                        line_number,
                    );
                }
            } else if (object.get("sessionId").and_then(Value::as_str).is_some()
                && object.get("projectHash").and_then(Value::as_str).is_some())
                || object.get("kind").and_then(Value::as_str) == Some("main")
            {
                Self::apply_session_fields(
                    &mut session,
                    &mut messages,
                    object,
                    false,
                    file,
                    line_number,
                );
            } else if object.get("type").and_then(Value::as_str).is_some()
                && !object.contains_key("$patch")
            {
                messages.upsert(Value::Object(object));
            }
        }

        session.insert("messages".to_string(), Value::Array(messages.into_values()));
        Ok(Value::Object(session))
    }

    fn parse_session_file(file: &Path) -> Option<ParsedGeminiSession> {
        // Legacy whole-file sessions can still carry large message bodies;
        // enforce the project's 100MB scan cap.
        if let Ok(metadata) = fs::metadata(file)
            && metadata.len() > MAX_SCAN_FILE_BYTES
        {
            tracing::warn!(
                path = %file.display(),
                "gemini: session exceeds the scan size cap; skipping"
            );
            return None;
        }
        let file_handle = match fs::File::open(file) {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(path = %file.display(), %error, "failed to open Gemini session");
                return None;
            }
        };
        let reader = std::io::BufReader::new(file_handle);
        let is_jsonl = file
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"));

        if is_jsonl {
            return match Self::replay_jsonl_session(reader, file) {
                Ok(value) => parsed_session_from_value(value),
                Err(error) => {
                    tracing::warn!(path = %file.display(), %error, "failed to read Gemini session");
                    None
                }
            };
        }

        match parse_json_session_reader(reader) {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(path = %file.display(), %error, "failed to parse Gemini session");
                None
            }
        }
    }

    fn normalize_message_role(message_type: &str) -> &str {
        match message_type {
            "model" | "gemini" => "assistant",
            "info" | "error" => "system",
            other => other,
        }
    }
}

/// Append only readable text, retaining its original whitespace and avoiding
/// a second serialized copy of provider objects or binary result payloads.
fn append_gemini_text(content: &mut String, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(text);
}

/// Gemini stores results as `PartListUnion`; textual `FunctionResponse` results
/// are `response.output` strings (`generateContentResponseUtilities.ts` at
/// gemini-cli 9b6e0265d16b). Never stringify arbitrary parts: they can contain
/// `inlineData`, `fileData`, or `__binary_injection__` alongside readable output.
fn append_gemini_tool_result(
    content: &mut String,
    result: &Value,
    tool_name: &str,
    has_result: &mut bool,
) {
    if let Some(parts) = result.as_array() {
        for part in parts {
            append_gemini_tool_result(content, part, tool_name, has_result);
        }
        return;
    }
    // Failed/cancelled calls use response.error; cancellation can retain
    // both output and error (scheduler/tool-executor.ts at the same revision).
    for (text, is_error) in [
        (
            result
                .as_str()
                .or_else(|| result.get("text").and_then(Value::as_str)),
            false,
        ),
        (
            result
                .pointer("/functionResponse/response/output")
                .and_then(Value::as_str),
            false,
        ),
        (
            result
                .pointer("/functionResponse/response/error")
                .and_then(Value::as_str),
            true,
        ),
    ] {
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            continue;
        };
        if *has_result {
            content.push('\n');
        } else {
            append_gemini_text(content, "[Tool result: ");
            content.push_str(tool_name);
            content.push_str("] ");
            *has_result = true;
        }
        if is_error {
            content.push_str("[Error] ");
        }
        content.push_str(text);
    }
}

/// One native Gemini message can carry only thoughts or tools. Keep its
/// readable parts and structured calls in one normalized message, so its
/// identity and any native usage metadata are not copied to extra rows.
fn normalize_gemini_message_content(
    raw: &Value,
    role: &str,
) -> (String, Vec<NormalizedInvocation>) {
    let plain = raw.get("content").map(flatten_content).unwrap_or_default();
    if role != "assistant" {
        return (plain, Vec::new());
    }

    let mut content = String::new();
    if let Some(thoughts) = raw.get("thoughts").and_then(Value::as_array) {
        for thought in thoughts {
            let subject = thought
                .get("subject")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty());
            let description = thought
                .get("description")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty());
            let Some(first) = subject.or(description) else {
                continue;
            };
            append_gemini_text(&mut content, "[Thinking] ");
            content.push_str(first);
            if subject.is_some()
                && let Some(description) = description
            {
                append_gemini_text(&mut content, description);
            }
        }
    }
    if content.is_empty() {
        // Preserve the text-only path's single allocation, even for a large
        // body. Only prepend into a new string when real thoughts need it.
        content = plain;
    } else {
        append_gemini_text(&mut content, &plain);
    }

    let mut invocations = Vec::new();
    if let Some(calls) = raw.get("toolCalls").and_then(Value::as_array) {
        for call in calls {
            let Some(name) = call
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
            else {
                continue;
            };
            append_gemini_text(&mut content, "[Tool: ");
            content.push_str(name);
            content.push(']');
            invocations.push(NormalizedInvocation {
                kind: "tool".to_owned(),
                name: name.to_owned(),
                raw_name: None,
                call_id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned),
                arguments: call.get("args").cloned(),
            });
            if let Some(result) = call.get("result") {
                let mut has_result = false;
                append_gemini_tool_result(&mut content, result, name, &mut has_result);
            }
        }
    }
    unwrap_skill_invocations(&mut invocations);
    (content, invocations)
}

struct ParsedGeminiSession {
    session_id: Option<String>,
    project_hash: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    messages: Vec<NormalizedMessage>,
}

fn merge_min_ts(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (None, Some(b)) => Some(b),
        (a, None) => a,
    }
}

fn merge_max_ts(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (None, Some(b)) => Some(b),
        (a, None) => a,
    }
}

fn normalize_gemini_message(
    item: Value,
    started_at: &mut Option<i64>,
    ended_at: &mut Option<i64>,
) -> Option<NormalizedMessage> {
    // Legacy Gemini used `model`; current Gemini CLI uses `gemini`.
    let msg_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("model");
    let role = GeminiConnector::normalize_message_role(msg_type);

    // Parse timestamp using shared utility
    let created = item.get("timestamp").and_then(parse_timestamp);

    // NOTE: Do NOT filter individual messages by timestamp here!
    // The file-level check in file_modified_since() is sufficient.
    // Filtering messages would cause older messages to be lost when
    // the file is re-indexed after new messages are added.
    *started_at = merge_min_ts(*started_at, created);
    *ended_at = merge_max_ts(*ended_at, created);

    let (content_str, invocations) = normalize_gemini_message_content(&item, role);

    // Empty text with no readable thoughts or named tools is not a message.
    if content_str.trim().is_empty() {
        return None;
    }

    Some(NormalizedMessage {
        idx: 0, // will be re-assigned after filtering
        role: role.to_string(),
        author: None,
        created_at: created,
        content: content_str,
        extra: item,
        invocations,
        snippets: Vec::new(),
    })
}

fn parsed_session_from_value(mut val: Value) -> Option<ParsedGeminiSession> {
    let session_id = val
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(String::from);
    let project_hash = val
        .get("projectHash")
        .and_then(|v| v.as_str())
        .map(String::from);
    let start_time = val.get("startTime").and_then(parse_timestamp);
    let last_updated = val.get("lastUpdated").and_then(parse_timestamp);

    let Value::Array(messages_arr) = val
        .as_object_mut()
        .and_then(|object| object.remove("messages"))?
    else {
        return None;
    };

    let mut messages = Vec::new();
    let mut started_at = start_time;
    let mut ended_at = last_updated;
    for item in messages_arr {
        if let Some(msg) = normalize_gemini_message(item, &mut started_at, &mut ended_at) {
            messages.push(msg);
        }
    }

    Some(ParsedGeminiSession {
        session_id,
        project_hash,
        started_at,
        ended_at,
        messages,
    })
}

#[derive(Default)]
struct GeminiStreamState {
    session_id: Option<String>,
    project_hash: Option<String>,
    start_time: Option<i64>,
    last_updated: Option<i64>,
    msg_started_at: Option<i64>,
    msg_ended_at: Option<i64>,
    messages: Vec<NormalizedMessage>,
    has_messages_array: bool,
}

struct GeminiMessagesFieldSeed<'a> {
    state: &'a mut GeminiStreamState,
    callback_error: &'a mut Option<anyhow::Error>,
}

impl<'de> DeserializeSeed<'de> for GeminiMessagesFieldSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MessagesVisitor<'a> {
            state: &'a mut GeminiStreamState,
            callback_error: &'a mut Option<anyhow::Error>,
        }

        impl<'de> Visitor<'de> for MessagesVisitor<'_> {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("Gemini messages field")
            }

            fn visit_seq<A>(self, seq: A) -> std::result::Result<(), A::Error>
            where
                A: SeqAccess<'de>,
            {
                self.state.has_messages_array = true;
                self.state.messages.clear();
                self.state.msg_started_at = None;
                self.state.msg_ended_at = None;
                let state = self.state;
                let mut push_item = |item: Value| -> Result<()> {
                    if let Some(msg) = normalize_gemini_message(
                        item,
                        &mut state.msg_started_at,
                        &mut state.msg_ended_at,
                    ) {
                        state.messages.push(msg);
                    }
                    Ok(())
                };
                drive_seq_stream(seq, &mut push_item, self.callback_error)
            }

            fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_bytes<E>(self, _: &[u8]) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_unit<E>(self) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_none<E>(self) -> std::result::Result<(), E> {
                self.state.has_messages_array = false;
                Ok(())
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<(), A::Error>
            where
                A: MapAccess<'de>,
            {
                self.state.has_messages_array = false;
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(())
            }
        }

        deserializer.deserialize_any(MessagesVisitor {
            state: self.state,
            callback_error: self.callback_error,
        })
    }
}

struct GeminiObjectSeed<'a> {
    state: &'a mut GeminiStreamState,
    callback_error: &'a mut Option<anyhow::Error>,
}

impl<'de> DeserializeSeed<'de> for GeminiObjectSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ObjectVisitor<'a> {
            state: &'a mut GeminiStreamState,
            callback_error: &'a mut Option<anyhow::Error>,
        }

        impl<'de> Visitor<'de> for ObjectVisitor<'_> {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Gemini JSON session object")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<(), M::Error>
            where
                M: MapAccess<'de>,
            {
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "sessionId" => {
                            let val = map.next_value::<Value>()?;
                            self.state.session_id = val.as_str().map(str::to_owned);
                        }
                        "projectHash" => {
                            let val = map.next_value::<Value>()?;
                            self.state.project_hash = val.as_str().map(str::to_owned);
                        }
                        "startTime" => {
                            let val = map.next_value::<Value>()?;
                            self.state.start_time = parse_timestamp(&val);
                        }
                        "lastUpdated" => {
                            let val = map.next_value::<Value>()?;
                            self.state.last_updated = parse_timestamp(&val);
                        }
                        "messages" => {
                            map.next_value_seed(GeminiMessagesFieldSeed {
                                state: self.state,
                                callback_error: self.callback_error,
                            })?;
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(())
            }
        }

        deserializer.deserialize_map(ObjectVisitor {
            state: self.state,
            callback_error: self.callback_error,
        })
    }
}

fn parse_json_session_reader(reader: impl BufRead) -> Result<Option<ParsedGeminiSession>> {
    let (kind, stream_reader) = peek_json_container(reader)?;
    match kind {
        JsonContainerKind::Array => {
            stream_json_array_after_peek::<_, IgnoredAny, _>(stream_reader, &mut |_| Ok(()))?;
            Ok(None)
        }
        JsonContainerKind::Object => {
            let mut deserializer = serde_json::Deserializer::from_reader(stream_reader);
            let mut state = GeminiStreamState::default();
            let mut callback_error = None;
            let result = GeminiObjectSeed {
                state: &mut state,
                callback_error: &mut callback_error,
            }
            .deserialize(&mut deserializer);
            if let Some(err) = callback_error {
                return Err(err);
            }
            result?;
            deserializer.end()?;

            if !state.has_messages_array {
                return Ok(None);
            }
            let started_at = merge_min_ts(state.start_time, state.msg_started_at);
            let ended_at = merge_max_ts(state.last_updated, state.msg_ended_at);
            Ok(Some(ParsedGeminiSession {
                session_id: state.session_id,
                project_hash: state.project_hash,
                started_at,
                ended_at,
                messages: state.messages,
            }))
        }
        JsonContainerKind::Empty | JsonContainerKind::Other(_) => {
            bail!("expected Gemini JSON session object or array")
        }
    }
}

#[allow(clippy::too_many_lines)]
fn scan_gemini_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_gemini_with_hooks(ctx, &mut SourceScanHooks::default(), on_conversation)
}

#[allow(clippy::too_many_lines)]
fn scan_gemini_with_hooks(
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let excluded_paths = excluded_scan_paths_from_env()?;
    let roots: Vec<ScanRoot> = GeminiConnector::source_roots(ctx);

    for root in roots {
        if !root.path.exists() || path_is_excluded(&root.path, &excluded_paths) {
            continue;
        }

        let files = GeminiConnector::session_files(&root.path);

        for file in files {
            // Skip files excluded or not modified since last scan (incremental indexing)
            if path_is_excluded(&file, &excluded_paths) || !file_modified_since(&file, ctx.since_ts)
            {
                continue;
            }
            // Pre-parse identity, mirrored from discover_sources() (FAD#22).
            let discovered = DiscoveredSourceFile::new(
                "gemini",
                &root,
                file.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            )
            .with_fs_metadata();
            if !hooks.should_scan(&discovered) {
                continue;
            }
            ctx.tick();

            let Some(parsed) = GeminiConnector::parse_session_file(&file) else {
                continue;
            };
            let ParsedGeminiSession {
                session_id,
                project_hash,
                started_at,
                ended_at,
                mut messages,
            } = parsed;

            // Re-assign sequential indices after filtering
            crate::types::reindex_messages(&mut messages);

            if messages.is_empty() {
                continue;
            }

            // Extract title from first user message
            let title = messages
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
                });

            // Try to extract actual workspace from message content first
            // Gemini stores by hash, but messages often contain the real project path
            let workspace = extract_workspace_from_content(&messages).or_else(|| {
                // Fallback to parent directory structure
                // Structure: ~/.gemini/tmp/<hash>/chats/session-*.json
                file.parent() // chats/
                    .and_then(|p| p.parent()) // <hash>/
                    .map(std::path::Path::to_path_buf)
            });

            let mut conv = NormalizedConversation {
                agent_slug: "gemini".into(),
                external_id: session_id
                    .or_else(|| file.file_stem().and_then(|s| s.to_str()).map(String::from)),
                title,
                workspace,
                source_path: file.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "source": "gemini",
                    "project_hash": project_hash
                }),
                messages,
            };
            root.apply_workspace_rewrite(&mut conv);
            on_conversation(conv)?;

            // Source complete: the session's conversation was delivered.
            // Withheld when the file changed while being parsed.
            if !discovered.fs_metadata_changed() {
                hooks.complete(&SourceCompletion {
                    source: discovered,
                    required_sidecars: Vec::new(),
                    conversations_emitted: 1,
                })?;
            }
        }
    }

    Ok(())
}

impl Connector for GeminiConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("gemini").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        scan_gemini_with_callback(ctx, &mut |conversation| {
            convs.push(conversation);
            Ok(())
        })?;
        Ok(convs)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Self::discover_sources(ctx)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        scan_gemini_with_callback(ctx, on_conversation)
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        scan_gemini_with_hooks(ctx, hooks, on_conversation)
    }
}

#[cfg(test)]
mod tests;

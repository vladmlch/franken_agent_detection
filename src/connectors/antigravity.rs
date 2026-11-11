//! Antigravity connector (the Antigravity IDE and the `agy` CLI).
//!
//! [Antigravity](https://antigravity.google) ships both an IDE and the `agy`
//! CLI, Google's successor to the Gemini CLI (`gmi`). Each keeps its own store
//! under `~/.gemini/` (a directory they share with — but keep separate from —
//! the legacy Gemini CLI, whose data lives under `~/.gemini/tmp/`):
//!
//! - `~/.gemini/antigravity/` — the Antigravity **IDE** workspace sessions.
//! - `~/.gemini/antigravity-cli/` — the `agy` **CLI** conversations.
//!
//! The two stores are structurally identical. By default the connector probes
//! BOTH and indexes every root that exists; `CASS_ANTIGRAVITY_DATA_ROOT`
//! replaces the pair with a single explicit base (tests / remote mirrors).
//! Inside a base:
//!
//! - `conversations/<uuid>.db` — one stock-SQLite database per conversation. The
//!   payload columns are an undocumented protobuf trajectory/steps model; this is
//!   the durable source of truth but expensive to read.
//! - `brain/<uuid>/.system_generated/logs/transcript.jsonl` — a **clean JSONL
//!   transcript** of the same conversation. This is the connector's primary read
//!   path: a simple, stable line-per-step JSON shape, no protobuf required. (A
//!   fuller `transcript_full.jsonl` sits beside it.)
//!
//! The `<uuid>` keying `conversations/<uuid>.db` and `brain/<uuid>/` is the same
//! id used by `agy --conversation <uuid>`.
//!
//! ## Provenance across the two stores
//!
//! Consumers key a conversation by `(agent, external_id)`, so the IDE and CLI
//! stores must never yield the same `external_id` for different
//! conversations. A conversation under the IDE base (`.gemini/antigravity`)
//! is emitted with `external_id = "ide/<uuid>"`; a conversation under the CLI
//! base (`.gemini/antigravity-cli`) — or under any base the connector cannot
//! classify (fixtures, relocated mirrors) — keeps the bare `<uuid>` so
//! existing archives stay stable. Every conversation additionally records
//! `metadata.layout` (`"ide"` / `"cli"`, when classified) and
//! `metadata.data_root` (the base directory it was read from).
//!
//! ## transcript.jsonl record shape
//!
//! Each line is one JSON object: `{ step_index, source, type, status,
//! created_at, content?, thinking?, tool_calls?, error?, media?,
//! truncated_fields? }`.
//!
//! - `step_index` (i64) — timeline order. Indices may have gaps; we sort by it.
//! - `source` — `USER_EXPLICIT` | `MODEL` | `SYSTEM`.
//! - `type` — the step kind. Known kinds and their normalized mapping:
//!   - `USER_INPUT` (`USER_EXPLICIT`) → `role: "user"`. The real prompt is wrapped
//!     in `<USER_REQUEST>…</USER_REQUEST>`; `<ADDITIONAL_METADATA>` (local time)
//!     and `<USER_SETTINGS_CHANGE>` (records the model) are appended. We extract
//!     the request body and keep the wrappers in `extra`.
//!   - `PLANNER_RESPONSE` (`MODEL`) → `role: "assistant"`. `content` is the visible
//!     reply; `thinking` is the model's reasoning, preserved in `extra.thinking`.
//!   - any other `MODEL` kind (`VIEW_FILE`, `EDIT_FILE`, `RUN_COMMAND`, …) → a
//!     `role: "tool"` step. `content` is the tool result; `tool_calls` (when
//!     present) are the structured calls. We synthesize a [`NormalizedInvocation`]
//!     named after the step so the tool is searchable even when `tool_calls` is
//!     absent. The raw `type` is preserved in `extra.agy_type`.
//!   - `EPHEMERAL_MESSAGE` / `SYSTEM_MESSAGE` (`SYSTEM`) → `role: "system"`
//!     (preserved faithfully; tagged in `extra` so consumers can down-rank the
//!     repetitive ephemeral reminders).
//!   - `ERROR_MESSAGE` (`SYSTEM`) → `role: "system"`. Records with `content` keep
//!     it; error-only records (no `content`) are emitted with the `error` text
//!     as their content instead of being dropped whole (cass F10).
//!   - `CONVERSATION_HISTORY` (`SYSTEM`) — a marker record. It emits no message
//!     when content-less, but when it carries `content` that text is emitted as
//!     a `role: "system"` message instead of vanishing with the record (cass F10).
//! - `created_at` — RFC3339 UTC (`2026-06-11T20:14:42Z`), read by [`parse_timestamp`].
//!
//! Unknown future `type`s are never dropped or crashed on: they are emitted as a
//! best-effort message keyed off `source`, with the raw `type` kept in
//! `extra.agy_type`. (See the agy version-compatibility guard work.)
//!
//! ## Source-field passthrough (cass F10)
//!
//! Every emitted message also carries, under their source names and only when
//! the record has them, the fields the canonical mapping above used to drop:
//! `truncated_fields` (the source-side truncation markers — the only signal
//! that distinguishes truncated sessions from clean ones), `status` (the
//! record's lifecycle state), `error` (the failure text) and `media`
//! (attachment references, `{mime_type, uri}`). They land in the message
//! `extra` without touching content, roles or order of anything already
//! emitted.
//!
//! ## Raw-record archive (cass F14)
//!
//! Every emitted message additionally carries the COMPLETE raw transcript
//! record — the exact line the connector read, with every field verbatim —
//! under `extra.agy_raw` (owner direction 2026-11-08: keep the raw source
//! record rather than lossy normalization, the F12 precedent applied to the
//! whole record). The canonical mapping above decides what the message IS;
//! the archive lets a consumer prove what the record HELD: cass verifies its
//! block inventory from `agy_raw` field by field (indexed text, lifted calls,
//! structural metadata) instead of treating every canonicalized record as
//! structurally unverifiable. The clone happens in
//! [`insert_passthrough_extras`], so all eight record arms archive the same
//! way; records the connector drops whole (empty-body `USER_INPUT` turns,
//! content-less markers) still emit no message and therefore no archive.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::jsonl::JsonlLines;
use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded,
    unwrap_skill_invocations,
};
use super::{Connector, file_modified_since, franken_detection_for_connector, parse_timestamp};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
    reindex_messages,
};

// Local fix (cass review 2026-11-02, F3): planner tool calls.
mod planner_calls;

/// Normalized agent slug emitted for every Antigravity conversation.
const AGENT_SLUG: &str = "antigravity";

/// Which Antigravity product wrote a store.
///
/// Classified from the on-disk location shape only — `.gemini/antigravity`
/// is the IDE, `.gemini/antigravity-cli` is the `agy` CLI — so the answer does
/// not depend on how the scan reached the base (default probe, env override,
/// or an explicit root). Any other base (fixtures, relocated mirrors) is
/// unclassified and treated like the CLI for identity purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AntigravityLayout {
    /// The Antigravity IDE (`~/.gemini/antigravity`).
    Ide,
    /// The `agy` CLI (`~/.gemini/antigravity-cli`).
    Cli,
}

impl AntigravityLayout {
    /// The base directory name under `~/.gemini/` for this layout.
    const fn base_dir_name(self) -> &'static str {
        match self {
            Self::Ide => "antigravity",
            Self::Cli => "antigravity-cli",
        }
    }

    /// The `metadata.layout` value for this layout.
    const fn metadata_value(self) -> &'static str {
        match self {
            Self::Ide => "ide",
            Self::Cli => "cli",
        }
    }

    /// Classify a base directory by its `.gemini/<name>` location shape.
    fn from_base(base: &Path) -> Option<Self> {
        let parent_is_dot_gemini = base
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".gemini");
        if !parent_is_dot_gemini {
            return None;
        }
        let name = base.file_name()?;
        [Self::Ide, Self::Cli]
            .into_iter()
            .find(|layout| name == layout.base_dir_name())
    }

    /// The `external_id` for a conversation `<uuid>` under this layout.
    ///
    /// IDE conversations are qualified (`ide/<uuid>`) so a uuid that appears
    /// under both stores cannot collide in `(agent, external_id)`-keyed
    /// consumers; the CLI keeps the bare `<uuid>` (the id `agy --conversation`
    /// resumes by) so archives indexed before the IDE store was probed stay
    /// stable.
    fn external_id(self, uuid: &str) -> String {
        match self {
            Self::Ide => format!("ide/{uuid}"),
            Self::Cli => uuid.to_string(),
        }
    }
}

/// Connector for the Antigravity (`agy`) coding agent.
pub struct AntigravityConnector;

impl Default for AntigravityConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Base directories holding `conversations/` and `brain/`.
    ///
    /// `CASS_ANTIGRAVITY_DATA_ROOT` (tests / remote mirrors) names a single
    /// explicit base and replaces the defaults. Otherwise both product stores
    /// are candidates — `~/.gemini/antigravity` (IDE) and
    /// `~/.gemini/antigravity-cli` (`agy` CLI) — and the scan indexes every one
    /// that exists (cass #454).
    fn base_roots() -> Vec<PathBuf> {
        if let Some(explicit) = env_path_nonempty("CASS_ANTIGRAVITY_DATA_ROOT") {
            return vec![explicit];
        }
        Self::default_base_roots(&dirs::home_dir().unwrap_or_default())
    }

    /// The default IDE + CLI bases under a home directory, IDE first.
    fn default_base_roots(home: &Path) -> Vec<PathBuf> {
        let dot_gemini = home.join(".gemini");
        vec![
            dot_gemini.join(AntigravityLayout::Ide.base_dir_name()),
            dot_gemini.join(AntigravityLayout::Cli.base_dir_name()),
        ]
    }

    /// The `<base>` directory (`brain/`'s parent) for a `brain/<uuid>`
    /// conversation directory.
    fn base_of_conversation_dir(conversation_dir: &Path) -> Option<&Path> {
        conversation_dir.parent()?.parent()
    }

    /// Transcript path for a `brain/<uuid>` conversation directory.
    fn transcript_path(conversation_dir: &Path) -> PathBuf {
        conversation_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl")
    }

    /// The fuller transcript variant, when agy wrote one.
    fn transcript_full_path(conversation_dir: &Path) -> PathBuf {
        conversation_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript_full.jsonl")
    }

    /// A conversation directory is recognized by the presence of its
    /// `transcript.jsonl` (the primary read path).
    fn is_conversation_dir(path: &Path) -> bool {
        Self::transcript_path(path).is_file()
    }

    /// The `<uuid>` for a conversation directory (its directory name).
    fn conversation_uuid(conversation_dir: &Path) -> Option<String> {
        conversation_dir
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
    }

    /// The sibling `conversations/<uuid>.db` for a `brain/<uuid>` directory, when
    /// the standard layout is intact (`<base>/brain/<uuid>` ↔
    /// `<base>/conversations/<uuid>.db`).
    fn sibling_db_path(conversation_dir: &Path) -> Option<PathBuf> {
        let uuid = Self::conversation_uuid(conversation_dir)?;
        let base = Self::base_of_conversation_dir(conversation_dir)?;
        let db = base.join("conversations").join(format!("{uuid}.db"));
        db.is_file().then_some(db)
    }

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots = if ctx.use_default_detection() {
            // Mirror the explicit-root acceptance: a default-detection
            // data_dir that itself looks like antigravity storage (a base
            // with a `brain/` tree, or the `brain/` dir itself) scopes the
            // scan to it, so fixture/mirror scans stay hermetic; otherwise
            // probe the system bases (IDE and CLI). Roots that do not exist
            // are skipped by the scan. `CASS_ANTIGRAVITY_DATA_ROOT` keeps
            // precedence so CI redirection is unaffected.
            let d = &ctx.data_dir;
            let env_override = env_path_nonempty("CASS_ANTIGRAVITY_DATA_ROOT").is_some();
            let looks_like_base =
                d.join("brain").is_dir() || d.file_name().is_some_and(|n| n == "brain");
            if !env_override && looks_like_base {
                vec![ScanRoot::local(d.clone())]
            } else {
                Self::base_roots()
                    .into_iter()
                    .map(ScanRoot::local)
                    .collect()
            }
        } else {
            let mut explicit = Vec::new();
            for root in &ctx.scan_roots {
                if let Some(mirrored) = root
                    .mirrored_store(&[".gemini", "antigravity", "brain"])
                    .or_else(|| root.mirrored_store(&[".gemini", "antigravity-cli", "brain"]))
                {
                    explicit.push(root.with_path(mirrored));
                } else {
                    explicit.push(root.clone());
                }
            }
            explicit
        };

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    /// Resolve every `brain/<uuid>` conversation directory reachable from a scan
    /// target. The target may be the conversation directory itself, the `brain/`
    /// directory, or a base directory (`antigravity` or `antigravity-cli`).
    fn conversation_dirs(scan_target: &Path) -> Vec<PathBuf> {
        if Self::is_conversation_dir(scan_target) {
            return vec![scan_target.to_path_buf()];
        }

        let mut out = Vec::new();
        // The transcript sits at <conv>/.system_generated/logs/transcript.jsonl,
        // so the conversation dir is three levels above the file. From the
        // base directory it is at depth 5; the cap is set higher so a scan
        // root one or two levels above (e.g. ~ or ~/.gemini, as remote mirrors
        // can produce) still finds it. The .system_generated/logs ancestry filter
        // below keeps correctness regardless of depth — the cap only bounds cost.
        for entry in WalkDir::new(scan_target)
            .min_depth(1)
            .max_depth(8)
            .into_iter()
            .flatten()
        {
            if !entry.file_type().is_file() || entry.file_name() != "transcript.jsonl" {
                continue;
            }
            let path = entry.path();
            // Require the .system_generated/logs/ ancestry so we don't pick up an
            // unrelated transcript.jsonl somewhere else.
            let conv_dir = path
                .parent() // logs/
                .filter(|logs| logs.file_name().is_some_and(|n| n == "logs"))
                .and_then(Path::parent) // .system_generated/
                .filter(|sg| sg.file_name().is_some_and(|n| n == ".system_generated"))
                .and_then(Path::parent); // <uuid>/
            if let Some(dir) = conv_dir {
                out.push(dir.to_path_buf());
            }
        }

        out.sort();
        out.dedup();
        out
    }

    /// True when the conversation's transcript is newer than `since_ts`.
    fn conversation_modified_since(conversation_dir: &Path, since_ts: Option<i64>) -> bool {
        if since_ts.is_none() {
            return true;
        }
        file_modified_since(&Self::transcript_path(conversation_dir), since_ts)
            || file_modified_since(&Self::transcript_full_path(conversation_dir), since_ts)
    }

    /// Read a transcript file into per-step JSON records, tolerating blank lines
    /// and individual malformed/non-UTF-8 lines (skipped + logged, never fatal),
    /// while discarding incomplete transcripts on mid-stream I/O errors.
    fn read_transcript_records(transcript: &Path) -> Vec<Value> {
        // Transcripts accumulate; enforce the project's 100MB scan cap.
        if let Ok(meta) = fs::metadata(transcript)
            && meta.len() > 100 * 1024 * 1024
        {
            tracing::warn!(
                transcript = %transcript.display(),
                size_bytes = meta.len(),
                "antigravity: transcript exceeds the scan size cap; skipping"
            );
            return Vec::new();
        }
        let Ok(file) = fs::File::open(transcript) else {
            return Vec::new();
        };
        Self::read_transcript_records_from(transcript, BufReader::new(file)).unwrap_or_default()
    }

    fn read_transcript_records_from(
        transcript: &Path,
        reader: impl BufRead,
    ) -> io::Result<Vec<Value>> {
        let mut records = Vec::new();
        for (lineno, line_res) in JsonlLines::new(reader).enumerate() {
            let line = match line_res {
                Ok(line) => line,
                Err(err) => {
                    tracing::warn!(
                        transcript = %transcript.display(),
                        line = lineno + 1,
                        error = %err,
                        "antigravity: aborting transcript after mid-stream read error"
                    );
                    return Err(err);
                }
            };
            let trimmed = line.trim_start_matches('\u{feff}').trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(value) => records.push(value),
                Err(err) => tracing::warn!(
                    transcript = %transcript.display(),
                    line = lineno + 1,
                    error = %err,
                    "antigravity: skipping malformed transcript line"
                ),
            }
        }
        Ok(records)
    }
}

/// Extract the inner text of every `<TAG>…</TAG>` block in `content`.
fn extract_tagged_blocks(content: &str, tag: &str) -> Vec<String> {
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
fn extract_user_request(content: &str) -> (String, Option<String>, Option<String>) {
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
fn model_from_settings(settings: &str) -> Option<String> {
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
fn detect_model(records: &[Value]) -> Option<String> {
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

/// Parse `tool_calls` into normalized invocations. When the array is absent we
/// still synthesize one invocation named after the step `type` (e.g.
/// `VIEW_FILE` → `view_file`) so the tool remains searchable.
fn parse_tool_calls(tool_calls: Option<&Value>, step_type: &str) -> Vec<NormalizedInvocation> {
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
fn record_to_message(rec: &Value) -> Option<NormalizedMessage> {
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
            if body.trim().is_empty() {
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
                if content.trim().is_empty() && thinking.is_none() && invocations.is_empty() {
                    return None;
                }
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
                    content,
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

fn parse_conversation(
    conversation_dir: &Path,
    _scan_root: &ScanRoot,
) -> Option<NormalizedConversation> {
    let transcript = AntigravityConnector::transcript_path(conversation_dir);
    let records = AntigravityConnector::read_transcript_records(&transcript);
    if records.is_empty() {
        return None;
    }

    // Sort by step_index (gaps are expected; ties keep input order via stable sort).
    let mut ordered: Vec<&Value> = records.iter().collect();
    ordered.sort_by_key(|r| {
        r.get("step_index")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });

    let base = AntigravityConnector::base_of_conversation_dir(conversation_dir);
    let layout = base.and_then(AntigravityLayout::from_base);
    let external_id = AntigravityConnector::conversation_uuid(conversation_dir)
        .map(|uuid| layout.unwrap_or(AntigravityLayout::Cli).external_id(&uuid));
    let model = detect_model(&records);

    let mut messages: Vec<NormalizedMessage> = Vec::new();
    let mut started_at: Option<i64> = None;
    let mut ended_at: Option<i64> = None;

    for rec in ordered {
        if let Some(ts) = rec.get("created_at").and_then(parse_timestamp) {
            started_at = Some(started_at.map_or(ts, |cur| cur.min(ts)));
            ended_at = Some(ended_at.map_or(ts, |cur| cur.max(ts)));
        }
        if let Some(msg) = record_to_message(rec) {
            messages.push(msg);
        }
    }

    if messages.is_empty() {
        // read_transcript_records already returns [] for an empty/un-run
        // conversation, so reaching here means every record was a content-less
        // marker — or the transcript schema drifted out from under us. Surface
        // it loudly rather than letting the conversation silently vanish.
        tracing::warn!(
            transcript = %transcript.display(),
            records = records.len(),
            "antigravity: transcript produced no messages (possible agy schema drift)"
        );
        return None;
    }

    reindex_messages(&mut messages);

    let title = messages
        .iter()
        .find(|m| m.role == "user")
        .or_else(|| messages.first())
        .and_then(|m| m.content.lines().find(|l| !l.trim().is_empty()))
        .map(|line| line.chars().take(100).collect::<String>());

    let mut metadata = Map::new();
    metadata.insert("source".to_string(), Value::String(AGENT_SLUG.to_string()));
    if let Some(model) = model {
        metadata.insert("model".to_string(), Value::String(model));
    }
    // Provenance: which store (IDE vs CLI) and which base directory this
    // conversation was read from, so identical uuids across stores stay
    // distinguishable downstream.
    if let Some(layout) = layout {
        metadata.insert(
            "layout".to_string(),
            Value::String(layout.metadata_value().to_string()),
        );
    }
    if let Some(base) = base {
        metadata.insert(
            "data_root".to_string(),
            Value::String(base.to_string_lossy().into_owned()),
        );
    }

    Some(NormalizedConversation {
        agent_slug: AGENT_SLUG.to_string(),
        external_id,
        title,
        workspace: None,
        source_path: transcript,
        started_at,
        ended_at,
        metadata: Value::Object(metadata),
        messages,
    })
}

fn scan_antigravity_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_antigravity_with_hooks(ctx, &mut SourceScanHooks::default(), on_conversation)
}

fn scan_antigravity_with_hooks(
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let excluded_paths = excluded_scan_paths_from_env()?;
    let roots = AntigravityConnector::source_roots(ctx);
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for root in roots {
        if !root.path.exists() || path_is_excluded(&root.path, &excluded_paths) {
            continue;
        }
        for conversation_dir in AntigravityConnector::conversation_dirs(&root.path) {
            let transcript = AntigravityConnector::transcript_path(&conversation_dir);
            if path_is_excluded(&conversation_dir, &excluded_paths)
                || path_is_excluded(&transcript, &excluded_paths)
            {
                continue;
            }
            if !seen.insert(dedupe_path_key(&conversation_dir)) {
                continue;
            }
            if !AntigravityConnector::conversation_modified_since(&conversation_dir, ctx.since_ts) {
                continue;
            }
            let discovered = DiscoveredSourceFile::new(
                AGENT_SLUG,
                &root,
                transcript,
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            )
            .with_fs_metadata();
            if !hooks.should_scan(&discovered) {
                continue;
            }
            ctx.tick();
            if let Some(mut conversation) = parse_conversation(&conversation_dir, &root) {
                root.apply_workspace_rewrite(&mut conversation);
                on_conversation(conversation).with_context(|| {
                    format!(
                        "emit antigravity conversation {}",
                        conversation_dir.display()
                    )
                })?;
                if !discovered.fs_metadata_changed() {
                    hooks.complete(&SourceCompletion {
                        source: discovered,
                        required_sidecars: Vec::new(),
                        conversations_emitted: 1,
                    })?;
                }
            }
        }
    }

    Ok(())
}

fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
    let excluded_paths = excluded_scan_paths_from_env()?;
    let roots = AntigravityConnector::source_roots(ctx);
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for root in roots {
        if !root.path.exists() || path_is_excluded(&root.path, &excluded_paths) {
            continue;
        }
        for conversation_dir in AntigravityConnector::conversation_dirs(&root.path) {
            if path_is_excluded(&conversation_dir, &excluded_paths) {
                continue;
            }
            if !AntigravityConnector::conversation_modified_since(&conversation_dir, ctx.since_ts) {
                continue;
            }

            // The transcript we actually parse — required for reconstruction.
            let transcript = AntigravityConnector::transcript_path(&conversation_dir);
            if transcript.is_file()
                && !path_is_excluded(&transcript, &excluded_paths)
                && seen.insert(dedupe_path_key(&transcript))
            {
                out.push(
                    DiscoveredSourceFile::new(
                        AGENT_SLUG,
                        &root,
                        transcript,
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }

            // The fuller transcript variant, if present (optional).
            let transcript_full = AntigravityConnector::transcript_full_path(&conversation_dir);
            if transcript_full.is_file()
                && !path_is_excluded(&transcript_full, &excluded_paths)
                && seen.insert(dedupe_path_key(&transcript_full))
            {
                out.push(
                    DiscoveredSourceFile::new(
                        AGENT_SLUG,
                        &root,
                        transcript_full,
                        DiscoveredSourceRole::PrimarySessionLog,
                        false,
                    )
                    .with_fs_metadata(),
                );
            }

            // The durable per-conversation SQLite database — the canonical source
            // of truth worth mirroring, though we read the transcript instead.
            if let Some(db) = AntigravityConnector::sibling_db_path(&conversation_dir) {
                if !path_is_excluded(&db, &excluded_paths) && seen.insert(dedupe_path_key(&db)) {
                    out.push(
                        DiscoveredSourceFile::new(
                            AGENT_SLUG,
                            &root,
                            db,
                            DiscoveredSourceRole::SqliteDatabase,
                            false,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// agy version-compatibility guard (bd-47kjh.2.5)
// ---------------------------------------------------------------------------

/// The `agy` versions whose transcript schema this reader has been validated against.
///
/// agy self-updates (`agy update`) and its on-disk formats are private and
/// undocumented, so bump this list whenever the reader is re-checked against a
/// newer agy. The connector itself never crashes on drift — unknown step types
/// are preserved (`extra.agy_type`) and a content-less transcript warns — but
/// this lets cass/ntm/CI proactively flag an untested agy so we re-validate.
pub const AGY_TESTED_VERSIONS: &[&str] = &["1.0.7"];

/// Best-effort `agy --version`, parsed to a bare dotted-numeric string (e.g.
/// `1.0.7`). Returns `None` if agy is absent or the output is unparseable.
#[must_use]
pub fn installed_agy_version() -> Option<String> {
    let output = std::process::Command::new("agy")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_agy_version(&text)
}

/// Pull the first dotted-numeric version token out of `agy --version` text such
/// as `agy version 1.0.7` or `v1.0.7`.
fn parse_agy_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|tok| tok.trim_start_matches('v'))
        .find(|t| {
            let parts: Vec<&str> = t.split('.').collect();
            parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(String::from)
}

/// Whether `version` is one this reader was validated against.
#[must_use]
pub fn agy_version_is_tested(version: &str) -> bool {
    let v = version.trim().trim_start_matches('v');
    AGY_TESTED_VERSIONS.contains(&v)
}

/// CI/diagnostic signal for agy version drift.
///
/// If agy is installed at a version this reader has NOT been validated against,
/// log a loud warning so the transcript reader gets re-checked after an
/// `agy update`. Returns the installed version (if any). Intended for a
/// startup/CI check — NOT the hot scan path.
pub fn warn_if_untested_agy_version() -> Option<String> {
    let version = installed_agy_version()?;
    if !agy_version_is_tested(&version) {
        tracing::warn!(
            installed = %version,
            tested = ?AGY_TESTED_VERSIONS,
            "antigravity: installed agy version is outside the tested-compatible set; \
             re-validate the transcript reader (bd-47kjh.2.5)"
        );
    }
    Some(version)
}

impl Connector for AntigravityConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector(AGENT_SLUG).unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        scan_antigravity_with_callback(ctx, &mut |conv| {
            convs.push(conv);
            Ok(())
        })?;
        Ok(convs)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        discover_sources(ctx)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        scan_antigravity_with_callback(ctx, on_conversation)
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
        scan_antigravity_with_hooks(ctx, hooks, on_conversation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::assert_discovery_covers_scan_sources;
    use std::fs;
    use tempfile::TempDir;

    const FIXTURE_UUID: &str = "f1e2d3c4-b5a6-4789-9abc-def012345678";

    /// The base directory (`brain/` + `conversations/`) of the checked-in
    /// fixture. It is not under a `.gemini/` parent, so it is deliberately
    /// unclassified (neither IDE nor CLI).
    fn fixture_base() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("antigravity")
    }

    fn fixture_conversation_dir() -> PathBuf {
        fixture_base().join("brain").join(FIXTURE_UUID)
    }

    fn fixture_ctx() -> ScanContext {
        let base = fixture_base();
        ScanContext::with_roots(base.clone(), vec![ScanRoot::local(base)], None)
    }

    fn only_conversation() -> NormalizedConversation {
        let convs = AntigravityConnector::new().scan(&fixture_ctx()).unwrap();
        assert_eq!(convs.len(), 1, "fixture is a single conversation");
        convs.into_iter().next().unwrap()
    }

    #[test]
    fn default_detection_scopes_to_antigravity_base_data_dir() {
        // The checked-in fixture base has the `brain/<uuid>` shape; default
        // detection with this base as data_dir must scan HERE, not the
        // machine's real ~/.gemini/antigravity-cli.
        let ctx = ScanContext::local_default(fixture_base(), None);
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(
            convs.len(),
            1,
            "default detection with an antigravity-base data_dir must scan that base"
        );
        assert_eq!(convs[0].external_id.as_deref(), Some(FIXTURE_UUID));
    }

    #[test]
    fn new_creates_connector() {
        let _ = AntigravityConnector::new();
        let _ = AntigravityConnector;
    }

    #[test]
    fn is_conversation_dir_requires_transcript() {
        let dir = TempDir::new().unwrap();
        assert!(!AntigravityConnector::is_conversation_dir(dir.path()));
        let logs = dir.path().join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("transcript.jsonl"), "{}\n").unwrap();
        assert!(AntigravityConnector::is_conversation_dir(dir.path()));
    }

    #[test]
    fn extract_tagged_blocks_handles_multiple_and_missing() {
        assert_eq!(
            extract_tagged_blocks("<A>one</A> mid <A>two</A>", "A"),
            vec!["one".to_string(), "two".to_string()]
        );
        assert!(extract_tagged_blocks("no tags here", "A").is_empty());
        // Unterminated open tag does not panic or loop forever.
        assert!(extract_tagged_blocks("<A>oops", "A").is_empty());
    }

    #[test]
    fn extract_user_request_unwraps_and_keeps_settings() {
        let raw = "<USER_REQUEST>\ndo the thing\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nchanged to Gemini 3.1 Pro (High). ok\n</USER_SETTINGS_CHANGE>";
        let (body, settings, metadata) = extract_user_request(raw);
        assert_eq!(body, "do the thing");
        assert!(settings.unwrap().contains("Gemini 3.1 Pro (High)"));
        assert_eq!(metadata.as_deref(), Some("time"));
    }

    #[test]
    fn extract_user_request_falls_back_to_raw_when_unwrapped() {
        let (body, settings, metadata) = extract_user_request("plain prompt with no tags");
        assert_eq!(body, "plain prompt with no tags");
        assert!(settings.is_none() && metadata.is_none());
    }

    #[test]
    fn model_from_settings_extracts_human_name() {
        assert_eq!(
            model_from_settings(
                "The user changed setting `Model Selection` from None to Gemini 3.1 Pro (High). No need to comment."
            )
            .as_deref(),
            Some("Gemini 3.1 Pro (High)")
        );
        assert_eq!(model_from_settings("changed from X to None. done"), None);
        // An earlier " to " (in a setting name / prior value) must NOT hijack the
        // parse: anchor on " from " then the next " to ".
        assert_eq!(
            model_from_settings(
                "changed setting `Auto to Stop` from None to Gemini 3.1 Pro (High). ok"
            )
            .as_deref(),
            Some("Gemini 3.1 Pro (High)")
        );
        // No " from " marker: fall back to the first " to ".
        assert_eq!(
            model_from_settings("set model to Gemini 3.1 Pro (High). thanks").as_deref(),
            Some("Gemini 3.1 Pro (High)")
        );
    }

    #[test]
    fn parse_tool_calls_synthesizes_name_from_type_when_absent() {
        let invs = parse_tool_calls(None, "VIEW_FILE");
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].name, "view_file");
        assert_eq!(invs[0].kind, "tool");
        assert!(invs[0].arguments.is_none());
    }

    #[test]
    fn parse_tool_calls_reads_structured_object_args() {
        let tc = serde_json::json!([
            {"name":"run_command","id":"c1","arguments":{"command":"echo hi"}}
        ]);
        let invs = parse_tool_calls(Some(&tc), "RUN_COMMAND");
        assert_eq!(invs[0].name, "run_command");
        assert_eq!(invs[0].call_id.as_deref(), Some("c1"));
        assert_eq!(invs[0].arguments.as_ref().unwrap()["command"], "echo hi");
    }

    #[test]
    fn parse_tool_calls_parses_stringified_json_args() {
        let tc = serde_json::json!([
            {"tool_name":"edit_file","id":"c2","arguments":"{\"path\":\"out.txt\"}"}
        ]);
        let invs = parse_tool_calls(Some(&tc), "EDIT_FILE");
        assert_eq!(invs[0].name, "edit_file");
        assert_eq!(invs[0].arguments.as_ref().unwrap()["path"], "out.txt");
    }

    #[test]
    fn scan_parses_fixture_metadata_and_roles() {
        let conv = only_conversation();
        assert_eq!(conv.agent_slug, "antigravity");
        assert_eq!(conv.external_id.as_deref(), Some(FIXTURE_UUID));
        assert_eq!(conv.metadata["model"], "Gemini 3.1 Pro (High)");
        assert_eq!(conv.metadata["source"], "antigravity");
        assert!(conv.title.as_deref().unwrap().starts_with("Read data.txt"));
        assert!(conv.started_at.unwrap() <= conv.ended_at.unwrap());
        assert_eq!(
            conv.source_path,
            AntigravityConnector::transcript_path(&fixture_conversation_dir())
        );

        // CONVERSATION_HISTORY is dropped; every other record becomes a message.
        assert_eq!(conv.messages.len(), 9);
        for (i, m) in conv.messages.iter().enumerate() {
            assert_eq!(m.idx, i64::try_from(i).unwrap());
        }
        assert!(conv.messages.iter().any(|m| m.role == "user"));
        assert!(conv.messages.iter().any(|m| m.role == "assistant"));
        assert!(conv.messages.iter().any(|m| m.role == "tool"));
        assert!(conv.messages.iter().any(|m| m.role == "system"));
    }

    #[test]
    fn user_message_unwraps_request_and_keeps_settings_extra() {
        let conv = only_conversation();
        let user = conv.messages.iter().find(|m| m.role == "user").unwrap();
        assert!(user.content.starts_with("Read data.txt"));
        assert!(!user.content.contains("USER_REQUEST"));
        assert!(
            user.extra["settings_change"]
                .as_str()
                .unwrap()
                .contains("Gemini 3.1 Pro (High)")
        );
        assert!(user.extra.get("additional_metadata").is_some());
    }

    #[test]
    fn planner_response_preserves_thinking() {
        let conv = only_conversation();
        let planner = conv
            .messages
            .iter()
            .find(|m| m.role == "assistant" && m.extra.get("thinking").is_some())
            .expect("a planner response with thinking");
        assert!(planner.content.contains("data.txt"));
        assert!(
            planner.extra["thinking"]
                .as_str()
                .unwrap()
                .contains("Planning the steps")
        );
    }

    // cass review 2026-11-02 F3: real producer shape.
    #[test]
    fn planner_tool_calls_only_record_is_kept_with_calls() {
        let rec = serde_json::json!({
            "step_index": 4, "source": "MODEL", "type": "PLANNER_RESPONSE",
            "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
            "tool_calls": [{"name": "view_file", "args": {"AbsolutePath": "/a.rs"}}]
        });
        let msg = record_to_message(&rec).expect("tool-call-only planner record is kept");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "[Tool: view_file]");
        assert_eq!(msg.invocations.len(), 1);
        assert_eq!(msg.invocations[0].name, "view_file");
        assert_eq!(
            msg.extra["cass"]["invocations"][0]["arguments"]["AbsolutePath"],
            "/a.rs"
        );
    }

    #[test]
    fn planner_text_thinking_and_calls_keep_all_three() {
        let rec = serde_json::json!({
            "source": "MODEL", "type": "PLANNER_RESPONSE", "created_at": "2026-06-11T20:14:43Z",
            "content": "reading it", "thinking": "plan",
            "tool_calls": [{"name": "view_file", "args": {}}]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.content, "reading it");
        assert_eq!(msg.extra["thinking"], "plan");
        assert_eq!(msg.invocations[0].name, "view_file");
    }

    #[test]
    fn generic_result_does_not_invent_a_generic_tool() {
        let rec = serde_json::json!({
            "source": "MODEL", "type": "GENERIC", "created_at": "2026-06-11T20:14:44Z",
            "content": "file contents"
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "tool");
        assert!(
            msg.invocations.is_empty(),
            "GENERIC is a result, not a tool call"
        );
    }

    #[test]
    fn empty_planner_without_calls_is_still_dropped() {
        let rec = serde_json::json!({"source": "MODEL", "type": "PLANNER_RESPONSE", "content": ""});
        assert!(record_to_message(&rec).is_none());
    }

    #[test]
    fn view_file_becomes_tool_message_with_synthesized_invocation() {
        let conv = only_conversation();
        let view = conv
            .messages
            .iter()
            .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("VIEW_FILE"))
            .expect("VIEW_FILE message");
        assert_eq!(view.role, "tool");
        assert!(view.content.contains("1234"));
        assert_eq!(view.invocations[0].name, "view_file");
        assert!(view.invocations[0].arguments.is_none());
    }

    #[test]
    fn run_command_invocation_carries_structured_args() {
        let conv = only_conversation();
        let run = conv
            .messages
            .iter()
            .find(|m| m.invocations.iter().any(|i| i.name == "run_command"))
            .expect("run_command message");
        assert_eq!(run.role, "tool");
        assert!(run.content.contains("HELLO_FROM_AGY"));
        let inv = &run.invocations[0];
        assert_eq!(inv.call_id.as_deref(), Some("call_run_0001"));
        assert_eq!(
            inv.arguments.as_ref().unwrap()["command"],
            "echo HELLO_FROM_AGY"
        );
    }

    #[test]
    fn edit_file_invocation_parses_stringified_args() {
        let conv = only_conversation();
        let edit = conv
            .messages
            .iter()
            .find(|m| m.invocations.iter().any(|i| i.name == "edit_file"))
            .expect("edit_file message");
        let inv = &edit.invocations[0];
        assert_eq!(inv.call_id.as_deref(), Some("call_edit_0002"));
        assert_eq!(inv.arguments.as_ref().unwrap()["path"], "out.txt");
        assert_eq!(inv.arguments.as_ref().unwrap()["content"], "DONE");
    }

    #[test]
    fn ephemeral_and_system_messages_preserved_as_system() {
        let conv = only_conversation();
        let ephemeral = conv
            .messages
            .iter()
            .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("EPHEMERAL_MESSAGE"))
            .expect("ephemeral message");
        assert_eq!(ephemeral.role, "system");
        assert_eq!(ephemeral.author.as_deref(), Some("ephemeral"));

        let system = conv
            .messages
            .iter()
            .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("SYSTEM_MESSAGE"))
            .expect("system message");
        assert_eq!(system.role, "system");
        assert_eq!(system.author.as_deref(), Some("system"));
    }

    #[test]
    fn unknown_model_step_type_is_preserved_not_dropped() {
        // BROWSER_PREVIEW is an unknown-to-us MODEL kind; the reader must never
        // crash or silently drop it (agy version-compat robustness).
        let conv = only_conversation();
        let unknown = conv
            .messages
            .iter()
            .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("BROWSER_PREVIEW"))
            .expect("unknown BROWSER_PREVIEW message preserved");
        assert_eq!(unknown.role, "tool");
        assert!(unknown.content.contains("preview"));
        assert_eq!(unknown.invocations[0].name, "browser_preview");
    }

    // ---------------------------------------------------------------------
    // cass F10: source passthrough. The canonical mapping below extracts the
    // text-bearing fields and used to drop four source signals cass needs
    // (truncated_fields markers, the lifecycle `status`, the `error` text,
    // `media` attachment refs) plus two whole record shapes (error-only
    // ERROR_MESSAGE, text-bearing CONVERSATION_HISTORY). All fixtures here
    // are synthetic — made-up text, real key/type structure only.
    // ---------------------------------------------------------------------

    #[test]
    fn truncated_fields_pass_through_to_extra() {
        // Real shape: `truncated_fields` is a list of the source field names
        // that were truncated before persistence (cass F10 item 1).
        let rec = serde_json::json!({
            "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
            "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
            "content": "<USER_REQUEST>\ninspect the widget rack\n</USER_REQUEST>",
            "truncated_fields": ["content"]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(
            msg.extra["truncated_fields"],
            serde_json::json!(["content"])
        );

        // PLANNER_RESPONSE markers (thinking/tool_calls truncations are the
        // real combinations) ride along without touching the content.
        let rec = serde_json::json!({
            "step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE",
            "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
            "content": "partial narration about the rack",
            "truncated_fields": ["thinking", "tool_calls"]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.content, "partial narration about the rack");
        assert_eq!(
            msg.extra["truncated_fields"],
            serde_json::json!(["thinking", "tool_calls"])
        );

        // No marker in the source → no key in extra.
        let rec = serde_json::json!({
            "source": "MODEL", "type": "GENERIC",
            "created_at": "2026-06-11T20:14:44Z", "content": "result text"
        });
        let msg = record_to_message(&rec).unwrap();
        assert!(msg.extra.get("truncated_fields").is_none());
    }

    #[test]
    fn error_only_error_message_is_emitted_with_error_text() {
        // Real shape: ERROR_MESSAGE records with no `content`, the failure
        // text only in `error` — dropped whole before cass F10 item 2.
        let rec = serde_json::json!({
            "step_index": 7, "source": "SYSTEM", "type": "ERROR_MESSAGE",
            "status": "DONE", "created_at": "2026-06-11T20:14:47Z",
            "error": "the widget pipeline collapsed"
        });
        let msg = record_to_message(&rec).expect("error-only ERROR_MESSAGE must not vanish");
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content, "the widget pipeline collapsed");
        assert_eq!(msg.extra["agy_type"], "ERROR_MESSAGE");
        assert_eq!(msg.extra["agy_source"], "SYSTEM");
        assert_eq!(msg.extra["error"], "the widget pipeline collapsed");

        // Content-bearing ERROR_MESSAGE keeps its content byte-for-byte and
        // now also exposes the `error` field (cass F10 item 3).
        let rec = serde_json::json!({
            "step_index": 8, "source": "SYSTEM", "type": "ERROR_MESSAGE",
            "status": "DONE", "created_at": "2026-06-11T20:14:48Z",
            "content": "the widget failed to load",
            "error": "widget_error_code_9"
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content, "the widget failed to load");
        assert_eq!(msg.extra["error"], "widget_error_code_9");

        // Neither content nor error → still no message (nothing to emit).
        let rec = serde_json::json!({
            "source": "SYSTEM", "type": "ERROR_MESSAGE",
            "status": "DONE", "created_at": "2026-06-11T20:14:49Z"
        });
        assert!(record_to_message(&rec).is_none());
    }

    // ---------------------------------------------------------------------
    // cass F14: archive the full raw source record. Every emitted message
    // also carries the complete raw transcript record under `extra.agy_raw`,
    // verbatim, alongside the canonical `agy_type` envelope — so cass can
    // PROVE its block inventory from the record itself instead of guessing
    // what normalization dropped. Synthetic fixtures; real key/type
    // structure only.
    // ---------------------------------------------------------------------

    #[test]
    fn record_to_message_archives_full_raw_source_record() {
        // USER_INPUT: the archived record keeps every source field verbatim
        // — wrappers, markers, media and token counts included.
        let rec = serde_json::json!({
            "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
            "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
            "content": "<USER_REQUEST>\ninspect the widget rack\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal noon\n</ADDITIONAL_METADATA>",
            "input_tokens": 11, "output_tokens": 7, "cache_read_tokens": 3,
            "media": [{"mime_type": "image/png", "uri": "file:///tmp/synthetic.png"}]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(
            msg.extra["agy_raw"], rec,
            "USER_INPUT record archived verbatim"
        );

        // PLANNER_RESPONSE with thinking and structured calls.
        let rec = serde_json::json!({
            "step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE",
            "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
            "content": "planning the rack inspection",
            "thinking": "considering the racks first",
            "tool_calls": [{"name": "view_file", "args": {"TargetFile": "/tmp/rack.rs"}}],
            "truncated_fields": ["tool_calls"]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(
            msg.extra["agy_raw"], rec,
            "PLANNER_RESPONSE record archived verbatim"
        );

        // MODEL-step envelope: the `agy_type` arm keeps the raw record too.
        let rec = serde_json::json!({
            "step_index": 2, "source": "MODEL", "type": "GENERIC",
            "status": "ERROR", "created_at": "2026-06-11T20:14:44Z",
            "content": "the rack probe returned nothing",
            "error": "synthetic probe failure",
            "exit_code": 2
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.extra["agy_type"], "GENERIC");
        assert_eq!(
            msg.extra["agy_raw"], rec,
            "GENERIC record archived verbatim"
        );

        // Error-only ERROR_MESSAGE: the synthesized content differs from any
        // source `content`, but the archived record is still the raw source.
        let rec = serde_json::json!({
            "step_index": 3, "source": "SYSTEM", "type": "ERROR_MESSAGE",
            "status": "DONE", "created_at": "2026-06-11T20:14:45Z",
            "error": "synthetic pipeline failure"
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.content, "synthetic pipeline failure");
        assert_eq!(
            msg.extra["agy_raw"], rec,
            "error-only record archived verbatim"
        );

        // SYSTEM_MESSAGE and text-bearing CONVERSATION_HISTORY too.
        let rec = serde_json::json!({
            "step_index": 4, "source": "SYSTEM", "type": "SYSTEM_MESSAGE",
            "status": "DONE", "created_at": "2026-06-11T20:14:46Z",
            "content": "synthetic system notice text"
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(
            msg.extra["agy_raw"], rec,
            "SYSTEM_MESSAGE record archived verbatim"
        );

        let rec = serde_json::json!({
            "step_index": 5, "source": "SYSTEM", "type": "CONVERSATION_HISTORY",
            "status": "DONE", "created_at": "2026-06-11T20:14:47Z",
            "content": "synthetic folded history note"
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(
            msg.extra["agy_raw"], rec,
            "CONVERSATION_HISTORY record archived verbatim"
        );
    }

    #[test]
    fn scan_archives_the_raw_record_for_every_emitted_message() {
        // End to end: every message the scan emits carries the raw record it
        // was built from, keyed to its source position (`step_index`).
        let dir = TempDir::new().unwrap();
        let logs = dir.path().join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            concat!(
                "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nclean the widget rack\\n</USER_REQUEST>\",\"media\":[{\"mime_type\":\"image/png\",\"uri\":\"file:///tmp/synthetic-a.png\"}]}\n",
                "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:43Z\",\"content\":\"planning the rack cleanup\",\"truncated_fields\":[\"thinking\"]}\n",
                "{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"GENERIC\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:44Z\",\"content\":\"the rack probe returned nothing\"}\n",
            ),
        )
        .unwrap();
        let ctx = ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![ScanRoot::local(dir.path().to_path_buf())],
            None,
        );
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        let msgs = &convs[0].messages;
        assert_eq!(msgs.len(), 3);
        for (i, m) in msgs.iter().enumerate() {
            let raw = m
                .extra
                .get("agy_raw")
                .expect("every emitted message archives its raw record");
            assert_eq!(raw["step_index"], i, "message {i} keyed to its record");
        }
        // The archived GENERIC record is the verbatim source line.
        assert_eq!(msgs[2].extra["agy_raw"]["type"], "GENERIC");
        assert_eq!(msgs[2].extra["agy_raw"]["status"], "DONE");
        assert_eq!(
            msgs[2].extra["agy_raw"]["content"],
            "the rack probe returned nothing"
        );
        // The archived USER_INPUT record keeps the wrapper text and media
        // the canonical mapping lifted out of the message content.
        assert_eq!(
            msgs[0].extra["agy_raw"]["content"],
            "<USER_REQUEST>\nclean the widget rack\n</USER_REQUEST>"
        );
        assert_eq!(
            msgs[0].extra["agy_raw"]["media"][0]["mime_type"],
            "image/png"
        );
    }

    #[test]
    fn generic_error_field_is_kept_in_extras() {
        // Real shape: GENERIC result records carry an `error` field next to
        // their text; FAD dropped the field from every record before F10.
        let rec = serde_json::json!({
            "step_index": 5, "source": "MODEL", "type": "GENERIC",
            "status": "ERROR", "created_at": "2026-06-11T20:14:45Z",
            "content": "the rack probe returned nothing",
            "error": "exit status 1 from the widget builder"
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.content, "the rack probe returned nothing");
        assert_eq!(msg.extra["error"], "exit status 1 from the widget builder");
        assert_eq!(msg.extra["status"], "ERROR");
    }

    #[test]
    fn media_references_are_kept_in_extras() {
        // Real shape: `media` is a list of {mime_type, uri} attachment refs.
        let rec = serde_json::json!({
            "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
            "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
            "content": "<USER_REQUEST>\nlook at the attached doodle\n</USER_REQUEST>",
            "media": [
                {"mime_type": "image/png", "uri": "file:///tmp/synthetic-doodle.png"},
                {"mime_type": "text/plain", "uri": "file:///tmp/synthetic-notes.txt"}
            ]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "look at the attached doodle");
        assert_eq!(
            msg.extra["media"],
            serde_json::json!([
                {"mime_type": "image/png", "uri": "file:///tmp/synthetic-doodle.png"},
                {"mime_type": "text/plain", "uri": "file:///tmp/synthetic-notes.txt"}
            ])
        );

        // GENERIC-carried media refs survive too (a real occurrence).
        let rec = serde_json::json!({
            "source": "MODEL", "type": "GENERIC",
            "created_at": "2026-06-11T20:14:46Z", "content": "rendered the attachment",
            "media": [{"mime_type": "image/png", "uri": "file:///tmp/synthetic-render.png"}]
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(msg.extra["media"][0]["mime_type"], "image/png");
        assert_eq!(
            msg.extra["media"][0]["uri"],
            "file:///tmp/synthetic-render.png"
        );
    }

    #[test]
    fn conversation_history_content_is_emitted() {
        // Real shape: CONVERSATION_HISTORY is a marker record, but one real
        // record carries text — that text must reach the packet (F10 item 5).
        let rec = serde_json::json!({
            "step_index": 1, "source": "SYSTEM", "type": "CONVERSATION_HISTORY",
            "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
            "content": "earlier turns were folded into this synthetic history note"
        });
        let msg = record_to_message(&rec).expect("CONVERSATION_HISTORY text must not vanish");
        assert_eq!(msg.role, "system");
        assert_eq!(
            msg.content,
            "earlier turns were folded into this synthetic history note"
        );
        assert_eq!(msg.extra["agy_type"], "CONVERSATION_HISTORY");

        // The marker form (no content) is still no message.
        let rec = serde_json::json!({
            "step_index": 2, "source": "SYSTEM", "type": "CONVERSATION_HISTORY",
            "status": "DONE", "created_at": "2026-06-11T20:14:43Z"
        });
        assert!(record_to_message(&rec).is_none());
    }

    #[test]
    fn lifecycle_status_passes_into_extras() {
        // Real shape: every record carries a lifecycle `status`
        // (DONE/RUNNING/ERROR/INVALID; F10 item 6).
        for status in ["DONE", "RUNNING", "ERROR", "INVALID"] {
            let rec = serde_json::json!({
                "source": "MODEL", "type": "VIEW_FILE",
                "created_at": "2026-06-11T20:14:44Z",
                "content": "file listing", "status": status
            });
            let msg = record_to_message(&rec).unwrap();
            assert_eq!(
                msg.extra["status"], status,
                "lifecycle status must ride along"
            );
        }

        // A record without `status` (defensive future shape) gets no key.
        let rec = serde_json::json!({
            "source": "MODEL", "type": "VIEW_FILE",
            "created_at": "2026-06-11T20:14:44Z", "content": "file listing"
        });
        let msg = record_to_message(&rec).unwrap();
        assert!(msg.extra.get("status").is_none());
    }

    #[test]
    fn scan_emits_f10_records_in_source_order_and_keeps_existing_ones() {
        // End to end: a synthetic transcript mixing already-emitted shapes
        // with the F10 shapes. Existing records keep their content/roles;
        // the new records land at their natural source positions.
        let dir = TempDir::new().unwrap();
        let logs = dir.path().join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            concat!(
                "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nclean the widget rack\\n</USER_REQUEST>\",\"media\":[{\"mime_type\":\"image/png\",\"uri\":\"file:///tmp/synthetic-a.png\"}]}\n",
                "{\"step_index\":1,\"source\":\"SYSTEM\",\"type\":\"CONVERSATION_HISTORY\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"synthetic folded history note\"}\n",
                "{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:43Z\",\"content\":\"planning the rack cleanup\",\"truncated_fields\":[\"thinking\"]}\n",
                "{\"step_index\":3,\"source\":\"MODEL\",\"type\":\"GENERIC\",\"status\":\"ERROR\",\"created_at\":\"2026-06-11T20:14:44Z\",\"content\":\"the rack probe returned nothing\",\"error\":\"synthetic probe failure\"}\n",
                "{\"step_index\":4,\"source\":\"SYSTEM\",\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:45Z\",\"error\":\"synthetic pipeline failure\"}\n",
            ),
        )
        .unwrap();
        let ctx = ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![ScanRoot::local(dir.path().to_path_buf())],
            None,
        );
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        let msgs = &convs[0].messages;
        assert_eq!(msgs.len(), 5);
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "system", "assistant", "tool", "system"]);

        // Already-emitted records keep their content untouched.
        assert_eq!(msgs[0].content, "clean the widget rack");
        assert_eq!(msgs[2].content, "planning the rack cleanup");
        assert_eq!(msgs[3].content, "the rack probe returned nothing");
        // The newly emitted records carry their source text.
        assert_eq!(msgs[1].content, "synthetic folded history note");
        assert_eq!(msgs[4].content, "synthetic pipeline failure");

        // Passthrough fields are visible per message, exactly as recorded.
        assert_eq!(msgs[0].extra["media"][0]["mime_type"], "image/png");
        assert_eq!(msgs[0].extra["status"], "DONE");
        assert_eq!(
            msgs[2].extra["truncated_fields"],
            serde_json::json!(["thinking"])
        );
        assert_eq!(msgs[3].extra["error"], "synthetic probe failure");
        assert_eq!(msgs[3].extra["status"], "ERROR");
        assert_eq!(msgs[4].extra["error"], "synthetic pipeline failure");
    }

    #[test]
    fn fixture_messages_carry_status_and_the_archived_raw_record() {
        // The checked-in fixture records carry `status: "DONE"` and no
        // error/media/truncated_fields, so after F10 every fixture message
        // gains the status key and after F14 also the archived raw record
        // — content/roles unchanged.
        let conv = only_conversation();
        for m in &conv.messages {
            assert_eq!(m.extra["status"], "DONE");
            assert!(m.extra.get("error").is_none());
            assert!(m.extra.get("media").is_none());
            assert!(m.extra.get("truncated_fields").is_none());
            let raw = m
                .extra
                .get("agy_raw")
                .expect("fixture messages archive their raw record");
            assert_eq!(raw["status"], "DONE");
            assert!(raw.get("type").is_some_and(|t| t.as_str().is_some()));
        }
    }

    #[test]
    fn discovery_lists_transcript_full_and_db() {
        let connector = AntigravityConnector::new();
        let discovered = connector.discover_source_files(&fixture_ctx()).unwrap();

        assert!(discovered.iter().all(|d| d.provider_slug == "antigravity"));
        assert!(
            discovered
                .iter()
                .any(|d| d.source_path.ends_with("transcript.jsonl")
                    && d.role == DiscoveredSourceRole::PrimarySessionLog
                    && d.required_for_reconstruction),
            "transcript.jsonl must be a required primary source"
        );
        assert!(
            discovered
                .iter()
                .any(|d| d.source_path.ends_with("transcript_full.jsonl")),
            "transcript_full.jsonl should be discovered"
        );
        assert!(
            discovered
                .iter()
                .any(|d| d.role == DiscoveredSourceRole::SqliteDatabase
                    && d.source_path.extension().is_some_and(|e| e == "db")),
            "the conversations/<uuid>.db should be discovered as the sqlite source"
        );
    }

    #[test]
    fn discovery_covers_scan_sources_for_fixture() {
        assert_discovery_covers_scan_sources(&AntigravityConnector::new(), &fixture_ctx());
    }

    #[test]
    fn scan_with_callback_matches_scan() {
        let connector = AntigravityConnector::new();
        let scanned = connector.scan(&fixture_ctx()).unwrap();
        let mut streamed = Vec::new();
        connector
            .scan_with_callback(&fixture_ctx(), &mut |c| {
                streamed.push(c);
                Ok(())
            })
            .unwrap();
        assert_eq!(streamed.len(), scanned.len());
        assert_eq!(streamed[0].messages.len(), scanned[0].messages.len());
    }

    #[test]
    fn scan_resolves_conversation_from_direct_dir() {
        let dir = fixture_conversation_dir();
        let ctx = ScanContext::with_roots(dir.clone(), vec![ScanRoot::local(dir)], None);
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn scan_returns_empty_for_missing_root() {
        let ctx = ScanContext::with_roots(
            PathBuf::from("/no/such/antigravity"),
            vec![ScanRoot::local(PathBuf::from("/no/such/antigravity"))],
            None,
        );
        assert!(AntigravityConnector::new().scan(&ctx).unwrap().is_empty());
    }

    #[test]
    fn since_ts_in_future_skips_conversation() {
        let base = fixture_base();
        let ctx =
            ScanContext::with_roots(base.clone(), vec![ScanRoot::local(base)], Some(i64::MAX));
        assert!(AntigravityConnector::new().scan(&ctx).unwrap().is_empty());
    }

    #[test]
    fn tested_versions_registry_and_version_check() {
        assert!(AGY_TESTED_VERSIONS.contains(&"1.0.7"));
        assert!(agy_version_is_tested("1.0.7"));
        assert!(agy_version_is_tested("v1.0.7"));
        assert!(!agy_version_is_tested("1.0.8"));
        assert!(!agy_version_is_tested("2.0.0"));
    }

    #[test]
    fn parse_agy_version_extracts_dotted_numeric_token() {
        assert_eq!(
            parse_agy_version("agy version 1.0.7").as_deref(),
            Some("1.0.7")
        );
        assert_eq!(parse_agy_version("v1.0.7\n").as_deref(), Some("1.0.7"));
        assert_eq!(
            parse_agy_version("Antigravity CLI 1.2.34 (build x)").as_deref(),
            Some("1.2.34")
        );
        assert_eq!(parse_agy_version("no version here").as_deref(), None);
    }

    #[test]
    fn transcript_with_only_markers_yields_no_conversation() {
        // A transcript whose only records carry no timeline content must not
        // panic and must not fabricate a conversation (it warns instead).
        let dir = TempDir::new().unwrap();
        let logs = dir.path().join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            "{\"step_index\":0,\"source\":\"SYSTEM\",\"type\":\"CONVERSATION_HISTORY\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\"}\n",
        )
        .unwrap();
        let ctx = ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![ScanRoot::local(dir.path().to_path_buf())],
            None,
        );
        assert!(AntigravityConnector::new().scan(&ctx).unwrap().is_empty());
    }

    #[test]
    fn malformed_transcript_lines_are_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        let logs = dir.path().join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        // A junk line between two valid records: junk is skipped, the rest parse.
        fs::write(
            logs.join("transcript.jsonl"),
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nhi there\\n</USER_REQUEST>\"}\nthis is not json\n{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:43Z\",\"content\":\"hello!\"}\n",
        )
        .unwrap();
        let ctx = ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![ScanRoot::local(dir.path().to_path_buf())],
            None,
        );
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].messages.len(),
            2,
            "valid records survive a junk line"
        );
    }

    #[test]
    fn ignores_legacy_gemini_cli_layout_under_shared_dot_gemini() {
        // Both the legacy Gemini CLI (~/.gemini/tmp/<hash>/chats/session-*.json)
        // and agy (~/.gemini/antigravity-cli/...) live under ~/.gemini. The agy
        // connector must resolve ONLY the antigravity-cli transcript, never the
        // gemini session files. Root at the shared `.gemini` parent to prove it.
        let tmp = TempDir::new().unwrap();
        let dot_gemini = tmp.path().join(".gemini");

        // Legacy gemini-cli session (must be ignored).
        let chats = dot_gemini.join("tmp").join("deadbeef").join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join("session-1.json"), "{\"messages\":[]}").unwrap();

        // agy conversation transcript (must be found).
        let logs = dot_gemini
            .join("antigravity-cli")
            .join("brain")
            .join("11111111-2222-3333-4444-555555555555")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nhi\\n</USER_REQUEST>\"}\n",
        )
        .unwrap();

        let ctx =
            ScanContext::with_roots(dot_gemini.clone(), vec![ScanRoot::local(dot_gemini)], None);
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1, "only the agy conversation should be found");
        assert_eq!(convs[0].agent_slug, "antigravity");
        assert_eq!(
            convs[0].external_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
    }
    // ---------------------------------------------------------------------
    // Dual-store layout (cass #454): IDE `~/.gemini/antigravity` + CLI
    // `~/.gemini/antigravity-cli`.
    // ---------------------------------------------------------------------

    const SHARED_UUID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    /// Write a minimal one-step transcript for `<base>/brain/<uuid>` whose user
    /// request is `marker`, returning the conversation directory.
    fn write_conversation(base: &Path, uuid: &str, marker: &str) -> PathBuf {
        let conv_dir = base.join("brain").join(uuid);
        let logs = conv_dir.join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(
            logs.join("transcript.jsonl"),
            format!(
                "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\n{marker}\\n</USER_REQUEST>\"}}\n"
            ),
        )
        .unwrap();
        conv_dir
    }

    #[test]
    fn default_base_roots_probe_ide_then_cli_under_dot_gemini() {
        let home = Path::new("/home/someone");
        assert_eq!(
            AntigravityConnector::default_base_roots(home),
            vec![
                PathBuf::from("/home/someone/.gemini/antigravity"),
                PathBuf::from("/home/someone/.gemini/antigravity-cli"),
            ]
        );
    }

    #[test]
    fn layout_is_classified_from_dot_gemini_location_shape_only() {
        assert_eq!(
            AntigravityLayout::from_base(Path::new("/h/.gemini/antigravity")),
            Some(AntigravityLayout::Ide)
        );
        assert_eq!(
            AntigravityLayout::from_base(Path::new("/h/.gemini/antigravity-cli")),
            Some(AntigravityLayout::Cli)
        );
        // A same-named directory outside `.gemini/` (fixture, relocated
        // mirror) is unclassified rather than guessed.
        assert_eq!(
            AntigravityLayout::from_base(Path::new("/mirror/antigravity")),
            None
        );
        assert_eq!(
            AntigravityLayout::from_base(Path::new("/h/.gemini/antigravity-ide")),
            None
        );
        assert_eq!(AntigravityLayout::from_base(&fixture_base()), None);
    }

    #[test]
    fn external_ids_qualify_ide_and_keep_cli_bare() {
        assert_eq!(AntigravityLayout::Ide.external_id("u-1"), "ide/u-1");
        assert_eq!(AntigravityLayout::Cli.external_id("u-1"), "u-1");
    }

    #[test]
    fn ide_and_cli_stores_are_both_scanned_and_same_uuid_never_collides() {
        let tmp = TempDir::new().unwrap();
        let dot_gemini = tmp.path().join(".gemini");
        let ide_base = dot_gemini.join("antigravity");
        let cli_base = dot_gemini.join("antigravity-cli");
        // The SAME uuid under both stores must surface as two distinct
        // conversations with different external ids.
        write_conversation(&ide_base, SHARED_UUID, "from the IDE");
        write_conversation(&cli_base, SHARED_UUID, "from the CLI");

        // The two default probe roots, exactly as `base_roots()` produces them.
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![
                ScanRoot::local(ide_base.clone()),
                ScanRoot::local(cli_base.clone()),
            ],
            None,
        );
        let mut convs = AntigravityConnector::new().scan(&ctx).unwrap();
        convs.sort_by(|a, b| a.external_id.cmp(&b.external_id));
        assert_eq!(convs.len(), 2, "one conversation per store");

        let cli = &convs[0];
        assert_eq!(cli.external_id.as_deref(), Some(SHARED_UUID));
        assert_eq!(cli.metadata["layout"], "cli");
        assert_eq!(
            cli.metadata["data_root"],
            cli_base.to_string_lossy().as_ref()
        );
        assert!(cli.source_path.starts_with(&cli_base));
        assert_eq!(cli.messages[0].content, "from the CLI");

        let ide = &convs[1];
        assert_eq!(
            ide.external_id.as_deref(),
            Some(format!("ide/{SHARED_UUID}").as_str())
        );
        assert_eq!(ide.metadata["layout"], "ide");
        assert_eq!(
            ide.metadata["data_root"],
            ide_base.to_string_lossy().as_ref()
        );
        assert!(ide.source_path.starts_with(&ide_base));
        assert_eq!(ide.messages[0].content, "from the IDE");

        // Discovery preserves per-root provenance for both stores.
        let discovered = AntigravityConnector::new()
            .discover_source_files(&ctx)
            .unwrap();
        let transcripts: Vec<&DiscoveredSourceFile> = discovered
            .iter()
            .filter(|d| d.required_for_reconstruction)
            .collect();
        assert_eq!(transcripts.len(), 2);
        assert!(
            transcripts
                .iter()
                .any(|d| d.scan_root == ide_base && d.source_path.starts_with(&ide_base))
        );
        assert!(
            transcripts
                .iter()
                .any(|d| d.scan_root == cli_base && d.source_path.starts_with(&cli_base))
        );
        assert_discovery_covers_scan_sources(&AntigravityConnector::new(), &ctx);
    }

    #[test]
    fn missing_ide_store_is_skipped_and_cli_store_still_indexed() {
        let tmp = TempDir::new().unwrap();
        let dot_gemini = tmp.path().join(".gemini");
        let ide_base = dot_gemini.join("antigravity"); // never created
        let cli_base = dot_gemini.join("antigravity-cli");
        write_conversation(&cli_base, SHARED_UUID, "cli only");

        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(ide_base), ScanRoot::local(cli_base)],
            None,
        );
        let convs = AntigravityConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].external_id.as_deref(), Some(SHARED_UUID));
        assert_eq!(convs[0].metadata["layout"], "cli");
    }

    #[test]
    fn scan_rooted_at_shared_dot_gemini_finds_both_stores_once() {
        // A remote mirror may root the scan one level up, at `.gemini` itself.
        // Both stores are found, each exactly once.
        let tmp = TempDir::new().unwrap();
        let dot_gemini = tmp.path().join(".gemini");
        write_conversation(
            &dot_gemini.join("antigravity"),
            "11111111-1111-4111-8111-111111111111",
            "ide",
        );
        write_conversation(
            &dot_gemini.join("antigravity-cli"),
            "22222222-2222-4222-8222-222222222222",
            "cli",
        );

        let ctx =
            ScanContext::with_roots(dot_gemini.clone(), vec![ScanRoot::local(dot_gemini)], None);
        let mut ids: Vec<String> = AntigravityConnector::new()
            .scan(&ctx)
            .unwrap()
            .into_iter()
            .filter_map(|c| c.external_id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "22222222-2222-4222-8222-222222222222".to_string(),
                "ide/11111111-1111-4111-8111-111111111111".to_string(),
            ]
        );
    }

    #[test]
    fn unclassified_fixture_base_keeps_bare_uuid_and_records_data_root() {
        let conv = only_conversation();
        assert_eq!(conv.external_id.as_deref(), Some(FIXTURE_UUID));
        assert!(
            conv.metadata.get("layout").is_none(),
            "a base outside .gemini/ must not be guessed as IDE or CLI"
        );
        assert_eq!(
            conv.metadata["data_root"],
            fixture_base().to_string_lossy().as_ref()
        );
    }

    #[test]
    fn antigravity_mid_read_io_error_discards_incomplete_transcript_and_skips_corrupt_utf8() {
        use std::io::{self, BufReader, Cursor, Read};

        struct FailingReader(io::ErrorKind);
        impl Read for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(self.0, "simulated mid-read failure"))
            }
        }

        let prefix = "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>complete prefix</USER_REQUEST>\"}\n";
        for kind in [
            io::ErrorKind::Other,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Interrupted,
        ] {
            let reader = BufReader::new(prefix.as_bytes().chain(FailingReader(kind)));
            assert!(
                AntigravityConnector::read_transcript_records_from(
                    Path::new("transcript.jsonl"),
                    reader
                )
                .is_err(),
                "mid-stream {kind:?} must discard incomplete Antigravity transcript"
            );
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            b"\xef\xbb\xbf{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>first valid</USER_REQUEST>\"}\n",
        );
        bytes.extend_from_slice(b"{\"step_index\":1,\"content\":\"\xffcorrupt\"}\n");
        bytes.extend_from_slice(
            b"{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"second valid\"}\n",
        );
        let records = AntigravityConnector::read_transcript_records_from(
            Path::new("transcript.jsonl"),
            Cursor::new(bytes),
        )
        .expect("corrupt UTF-8 line should be skipped");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["step_index"], 0);
        assert_eq!(records[1]["step_index"], 2);
    }
}

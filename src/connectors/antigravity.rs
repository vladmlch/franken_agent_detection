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
//! - `brain/<uuid>/.system_generated/logs/transcript*.jsonl` — **clean JSONL
//!   transcripts** of the same conversation: the connector's primary read
//!   path, a simple, stable line-per-step JSON shape, no protobuf required.
//!   When both `transcript_full.jsonl` (the fuller variant) and
//!   `transcript.jsonl` exist, the parse source is picked with the native
//!   `get_best_log_file` rule — an unconditional intra-directory priority
//!   (the fuller transcript always wins inside one logs/ directory; the
//!   native `(mtime, prio)` sort only ranks candidates across different
//!   `brain_dirs`); the first candidate that yields records wins, with no
//!   stitching across the pair.
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
//!     the request body and keep the wrappers in `extra`. An attachment-only
//!     turn (empty request body, non-empty `media`) still emits a `user`
//!     message, matching native.
//!   - `PLANNER_RESPONSE` (`MODEL`) → `role: "assistant"`. `content` is the visible
//!     reply; `thinking` is the model's reasoning, preserved in `extra.thinking`.
//!   - any other `MODEL` kind (`VIEW_FILE`, `EDIT_FILE`, `RUN_COMMAND`, …) → a
//!     `role: "tool"` step. `content` is the tool result; `tool_calls` (when
//!     present) are the structured calls. We synthesize a [`NormalizedInvocation`]
//!     named after the step so the tool is searchable even when `tool_calls` is
//!     absent. The raw `type` is preserved in `extra.agy_type`. An error-only
//!     step (no `content`, `thinking` or calls) lifts its `error` text as the
//!     message content instead of being dropped, like `ERROR_MESSAGE` does.
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
//! way; records the connector drops whole (empty-body `USER_INPUT` turns with
//! no attachments, content-less markers) still emit no message and therefore
//! no archive.
//!
//! ## Workspace
//!
//! `conversation.workspace` is read ONLY from host-stamped metadata: a
//! `<user_information>` block on a `USER_INPUT`/`SYSTEM` record's `content`
//! or `prompt`. The first `Active Workspaces:` entry wins; `Command Working
//! Directory:` is the fallback. Candidates must be absolute paths outside
//! `.gemini`. No path is ever inferred from tool calls or request text — a
//! `<user_information>` example pasted into `<USER_REQUEST>` is user-typed,
//! not evidence — and `ScanRoot::apply_workspace_rewrite` still rewrites the
//! result for remote mirrors.

use anyhow::Result;

use super::scan::{DiscoveredSourceFile, ScanContext, SourceScanHooks};
use super::{Connector, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};

use scan::{discover_sources, scan_antigravity_with_callback, scan_antigravity_with_hooks};

mod parse;
// Local fix (cass review 2026-11-02, F3): planner tool calls.
mod planner_calls;
mod scan;
mod subagent;

#[cfg(test)]
mod tests_core;
#[cfg(test)]
mod tests_transcripts_subagents;

/// Normalized agent slug emitted for every Antigravity conversation.
pub(super) const AGENT_SLUG: &str = "antigravity";

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

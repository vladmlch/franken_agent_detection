//! Cursor Agent JSONL transcript discovery and normalization (gh #306).

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::CursorConnector;
use crate::connectors::jsonl::JsonlLines;
use crate::connectors::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use crate::connectors::utils::{dedupe_path_key, env_path_nonempty, path_is_excluded, read_capped};
use crate::connectors::{
    extract_invocations_from_content_blocks, file_modified_since, flatten_content, parse_timestamp,
};
use crate::types::{NormalizedConversation, NormalizedMessage, reindex_messages};

/// Directory segment that brackets a session's transcript directory.
pub(super) const AGENT_TRANSCRIPTS_DIR: &str = "agent-transcripts";
/// Normalized agent slug, shared with the Composer path (same agent).
pub(super) const CURSOR_AGENT_SLUG: &str = "cursor";

/// Metadata marker of a merged Composer + transcript conversation (§6.3):
/// distinguishes merges from the transcript-only `"agent"` shape and the
/// Composer-only shape (marker absent).
pub(super) const CURSOR_MERGED_FORMAT: &str = "merged";
/// Merge-rules revision (§6.5 escape hatch): hosts that refuse to re-emit
/// unchanged conversations can bump this to force a rebuild.
pub(super) const CURSOR_MERGE_REVISION: i64 = 1;
/// Extra key carrying the raw transcript record on pair and error rows
/// (§6.2): `cursor_` namespacing keeps it out of the source bubble's keys.
pub(super) const CURSOR_TRANSCRIPT_RECORD_KEY: &str = "cursor_transcript_record";

impl CursorConnector {
    /// Default root holding `~/.cursor/projects/`. Overridable via
    /// `CASS_CURSOR_PROJECTS_ROOT` for tests / remote mirrors.
    fn agent_projects_root() -> Option<PathBuf> {
        if let Some(explicit) = env_path_nonempty("CASS_CURSOR_PROJECTS_ROOT") {
            return Some(explicit);
        }
        dirs::home_dir().map(|h| h.join(".cursor").join("projects"))
    }

    /// Roots under which to look for Agent transcripts. Mirrors the Composer
    /// root policy: a default-detection `data_dir` that itself looks like a
    /// Cursor base scopes the whole scan to that base (so fixture/mirror
    /// scans stay hermetic); otherwise default detection uses
    /// `~/.cursor/projects`, with `CASS_CURSOR_PROJECTS_ROOT` still taking
    /// precedence for CI; explicit scan roots (e.g. remote mirrors) are
    /// walked as-is.
    fn agent_scan_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if Self::looks_like_base(&ctx.data_dir)
                && env_path_nonempty("CASS_CURSOR_PROJECTS_ROOT").is_none()
            {
                vec![ScanRoot::local(ctx.data_dir.clone())]
            } else {
                Self::agent_projects_root()
                    .into_iter()
                    .map(ScanRoot::local)
                    .collect()
            }
        } else {
            ctx.scan_roots.clone()
        };
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    /// True only for a *primary* session transcript
    /// `<...>/agent-transcripts/<sid>/<sid>.jsonl`. Subagent transcripts (under a
    /// `subagents/` subtree, or any file whose stem differs from its parent dir
    /// name) are excluded.
    fn is_primary_agent_transcript(path: &Path) -> bool {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return false;
        }
        if path
            .components()
            .any(|c| c.as_os_str().to_str() == Some("subagents"))
        {
            return false;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        // `<sid>/<sid>.jsonl`: the file's stem equals its directory name.
        if parent.file_name().and_then(|s| s.to_str()) != Some(stem) {
            return false;
        }
        // Grandparent must be the `agent-transcripts` directory.
        parent
            .parent()
            .and_then(|gp| gp.file_name())
            .and_then(|s| s.to_str())
            == Some(AGENT_TRANSCRIPTS_DIR)
    }

    /// Resolve every primary Agent transcript reachable from a scan target.
    fn agent_transcript_files(scan_target: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in WalkDir::new(scan_target)
            .min_depth(1)
            .max_depth(8)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() && Self::is_primary_agent_transcript(entry.path()) {
                out.push(entry.path().to_path_buf());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Every primary Agent transcript reachable from the scan targets, in a
    /// fixed `(root, path)` order. The merge consumes this inventory in full
    /// (a session with a fresh half is merged regardless of the other
    /// half's mtime), while discovery and the transcript-only path consume
    /// its freshness-filtered subset. Deduped by filesystem identity so
    /// overlapping scan roots cannot double-read a file; a root or
    /// transcript the host excluded (CASS_EXCLUDE_PATHS) is never read.
    pub(super) fn agent_transcript_inventory(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
    ) -> Vec<(ScanRoot, PathBuf)> {
        let mut out = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for root in Self::agent_scan_roots(ctx) {
            if !root.path.exists() || path_is_excluded(&root.path, excluded_paths) {
                continue;
            }
            for transcript in Self::agent_transcript_files(&root.path) {
                if path_is_excluded(&transcript, excluded_paths) {
                    continue;
                }
                if !seen.insert(dedupe_path_key(&transcript)) {
                    continue;
                }
                out.push((root.clone(), transcript));
            }
        }
        out
    }

    /// Whether a transcript counts as fresh for an incremental scan: its own
    /// mtime or its `.workspace-trusted` sidecar's (a sidecar the host
    /// excluded does not count), with the usual 1s slack.
    pub(super) fn agent_transcript_is_fresh(
        transcript: &Path,
        since_ts: Option<i64>,
        excluded_paths: &[PathBuf],
    ) -> bool {
        let workspace_metadata_changed = Self::agent_workspace_metadata_path(transcript)
            .is_some_and(|path| {
                path.is_file()
                    && !path_is_excluded(&path, excluded_paths)
                    && file_modified_since(&path, since_ts)
            });
        file_modified_since(transcript, since_ts) || workspace_metadata_changed
    }

    /// The fresh `(root, transcript)` pairs that BOTH `scan()`'s
    /// transcript-only path and `discover_source_files()` consume, so
    /// discovery always covers every freshly scanned source (the connector
    /// conformance contract).
    fn agent_transcript_sources(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
    ) -> Vec<(ScanRoot, PathBuf)> {
        Self::agent_transcript_inventory(ctx, excluded_paths)
            .into_iter()
            .filter(|(_, transcript)| {
                Self::agent_transcript_is_fresh(transcript, ctx.since_ts, excluded_paths)
            })
            .collect()
    }

    /// The encoded `<project>` directory name for a transcript path.
    pub(super) fn agent_project_dir_name(transcript: &Path) -> Option<String> {
        // <project>/agent-transcripts/<sid>/<sid>.jsonl
        transcript
            .parent()? // <sid>/
            .parent()? // agent-transcripts/
            .parent()? // <project>/
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
    }

    pub(super) fn agent_workspace_metadata_path(transcript: &Path) -> Option<PathBuf> {
        Some(
            transcript
                .parent()?
                .parent()?
                .parent()?
                .join(".workspace-trusted"),
        )
    }

    /// Project slugs lose the distinction between separators and literal
    /// hyphens. Only the explicit workspacePath can establish attribution.
    /// Accept foreign absolute paths too: exported sources need not exist on
    /// the scanning host, and ScanRoot applies source mappings afterward.
    pub(super) fn agent_workspace_from_metadata(
        transcript: &Path,
        excluded_paths: &[PathBuf],
    ) -> Option<PathBuf> {
        let path = Self::agent_workspace_metadata_path(transcript)?;
        if path_is_excluded(&path, excluded_paths) {
            return None;
        }
        let text = read_capped(&path).ok()??;
        let metadata: Value = serde_json::from_str(&text).ok()?;
        let workspace = metadata.get("workspacePath")?.as_str()?;
        let bytes = workspace.as_bytes();
        let drive_absolute = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\');
        let unc_absolute = workspace.strip_prefix("\\\\").is_some_and(|rest| {
            let mut components = rest.split('\\');
            components.next().is_some_and(|part| !part.is_empty())
                && components.next().is_some_and(|part| !part.is_empty())
        });
        if workspace.chars().any(char::is_control)
            || !(workspace.starts_with('/') || drive_absolute || unc_absolute)
        {
            return None;
        }
        Some(PathBuf::from(workspace))
    }

    /// A file's mtime in epoch milliseconds, if it can be stated.
    pub(super) fn file_mtime_ms(path: &Path) -> Option<i64> {
        std::fs::metadata(path).ok().and_then(|meta| {
            meta.modified().ok().and_then(|m| {
                m.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_millis()).ok())
            })
        })
    }

    /// Read a JSONL transcript into per-line records, skipping blank and
    /// individually-malformed/non-UTF-8 lines (warned, never fatal) while
    /// discarding incomplete transcripts on mid-stream I/O errors.
    pub(super) fn read_agent_transcript_records(transcript: &Path) -> Vec<Value> {
        if let Ok(meta) = std::fs::metadata(transcript)
            && meta.len() > 100 * 1024 * 1024
        {
            tracing::warn!(
                transcript = %transcript.display(),
                "cursor: agent transcript exceeds the scan size cap; skipping"
            );
            return Vec::new();
        }
        let file = match std::fs::File::open(transcript) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    transcript = %transcript.display(),
                    error = %err,
                    "cursor: unreadable agent transcript"
                );
                return Vec::new();
            }
        };
        Self::read_agent_transcript_records_from(transcript, BufReader::new(file))
            .unwrap_or_default()
    }

    pub(super) fn read_agent_transcript_records_from(
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
                        "cursor: aborting agent transcript after mid-stream read error"
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
                    "cursor: skipping malformed agent-transcript line"
                ),
            }
        }
        Ok(records)
    }

    /// Map one transcript record to a normalized message, or `None` when it
    /// carries no usable content.
    pub(super) fn agent_record_to_message(rec: &Value) -> Option<NormalizedMessage> {
        let role_raw = rec.get("role").and_then(Value::as_str).unwrap_or("");
        if role_raw.trim().is_empty() {
            return None;
        }
        let role = Self::normalize_role(role_raw);

        // The documented shape nests content at `message.content`; tolerate a
        // top-level `content` too. Both array-of-blocks and plain-string forms
        // are handled by `flatten_content`.
        let content_val = rec
            .pointer("/message/content")
            .or_else(|| rec.get("content"));
        let (content, invocations) = content_val.map_or_else(
            || (String::new(), Vec::new()),
            |v| {
                (
                    flatten_content(v),
                    extract_invocations_from_content_blocks(v),
                )
            },
        );

        // No per-message timestamp in the documented shape; use one defensively
        // if a future/real record carries it.
        let created_at = rec
            .get("timestamp")
            .or_else(|| rec.get("createdAt"))
            .or_else(|| rec.get("created_at"))
            .and_then(parse_timestamp);

        if content.trim().is_empty() && invocations.is_empty() {
            return None;
        }

        // F11's contract (cass tests kept verbatim) pins transcript rows to
        // today's shape — empty extra — so master's `cass.tool_call_count`
        // marker is deliberately NOT inserted on this path (it survives on
        // the composer-bubble path, where master's test anchors it).
        Some(NormalizedMessage {
            idx: 0,
            role,
            author: None,
            created_at,
            content,
            extra: Value::Object(Map::new()),
            invocations,
            snippets: Vec::new(),
        })
    }

    /// Parse a single Agent transcript file into a normalized conversation.
    pub(super) fn parse_agent_transcript(
        transcript: &Path,
        root: &ScanRoot,
        excluded_paths: &[PathBuf],
    ) -> Option<NormalizedConversation> {
        let records = Self::read_agent_transcript_records(transcript);
        if records.is_empty() {
            return None;
        }

        let mut messages: Vec<NormalizedMessage> = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;
        for rec in &records {
            if let Some(msg) = Self::agent_record_to_message(rec) {
                if let Some(ts) = msg.created_at {
                    started_at = Some(started_at.map_or(ts, |cur| cur.min(ts)));
                    ended_at = Some(ended_at.map_or(ts, |cur| cur.max(ts)));
                }
                messages.push(msg);
            }
        }

        if messages.is_empty() {
            tracing::warn!(
                transcript = %transcript.display(),
                records = records.len(),
                "cursor: agent transcript produced no messages (possible schema drift)"
            );
            return None;
        }

        // The documented record shape has no timestamps; fall back to the file's
        // mtime so the conversation still has a sortable time.
        if started_at.is_none() {
            if let Some(ms) = Self::file_mtime_ms(transcript) {
                started_at = Some(ms);
                ended_at = Some(ms);
            }
        }

        reindex_messages(&mut messages);

        let external_id = transcript
            .file_stem()
            .and_then(|s| s.to_str())
            .map(String::from);
        let project = Self::agent_project_dir_name(transcript);
        let explicit_workspace = Self::agent_workspace_from_metadata(transcript, excluded_paths);
        let attribution = if explicit_workspace.is_some() {
            "workspace_trusted"
        } else {
            "unresolved"
        };

        let title = messages
            .iter()
            .find(|m| m.role == "user")
            .or_else(|| messages.first())
            .and_then(|m| m.content.lines().find(|l| !l.trim().is_empty()))
            .map(|line| line.chars().take(100).collect::<String>());

        let mut metadata = Map::new();
        metadata.insert(
            "source".to_string(),
            Value::String(CURSOR_AGENT_SLUG.to_string()),
        );
        metadata.insert(
            "cursor_format".to_string(),
            Value::String("agent".to_string()),
        );
        metadata.insert(
            "cursor_workspace_attribution".to_string(),
            Value::String(attribution.to_string()),
        );
        if let Some(p) = &project {
            metadata.insert("cursor_project_dir".to_string(), Value::String(p.clone()));
        }

        let mut conv = NormalizedConversation {
            agent_slug: CURSOR_AGENT_SLUG.to_string(),
            external_id,
            title,
            workspace: explicit_workspace,
            source_path: transcript.to_path_buf(),
            started_at,
            ended_at,
            metadata: Value::Object(metadata),
            messages,
        };
        root.apply_workspace_rewrite(&mut conv);
        Some(conv)
    }

    /// Agent-transcript files as discovered sources (mirrors the scan half's
    /// freshness gate over the same inventory: the F11 merge pass reads the
    /// fresh transcripts, merging each with its Composer half or emitting
    /// today's transcript-only shape).
    pub(super) fn discover_agent_sources(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
        out: &mut Vec<DiscoveredSourceFile>,
    ) {
        let mut seen_metadata = HashSet::new();
        for (root, transcript) in Self::agent_transcript_sources(ctx, excluded_paths) {
            if let Some(path) = Self::agent_workspace_metadata_path(&transcript)
                && path.is_file()
                && !path_is_excluded(&path, excluded_paths)
                && seen_metadata.insert(dedupe_path_key(&path))
            {
                out.push(
                    DiscoveredSourceFile::new(
                        CURSOR_AGENT_SLUG,
                        &root,
                        path,
                        DiscoveredSourceRole::MetadataSidecar,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
            out.push(
                DiscoveredSourceFile::new(
                    CURSOR_AGENT_SLUG,
                    &root,
                    transcript,
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata(),
            );
        }
    }
}

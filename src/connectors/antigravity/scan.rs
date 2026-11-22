//! Antigravity store layout discovery, transcript selection, and scan orchestration.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::parse::{detect_model, extract_workspace_from_records, record_to_message};
use super::subagent::{SubagentDescriptor, subagent_descriptor_index, subagent_verdict};
use super::{AGENT_SLUG, AntigravityConnector};
use crate::connectors::jsonl::JsonlLines;
use crate::connectors::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use crate::connectors::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded,
};
use crate::connectors::{file_modified_since, parse_timestamp};
use crate::types::{NormalizedConversation, NormalizedMessage, reindex_messages};

/// Which Antigravity product wrote a store.
///
/// Classified from the on-disk location shape only — `.gemini/antigravity`
/// is the IDE, `.gemini/antigravity-cli` is the `agy` CLI — so the answer does
/// not depend on how the scan reached the base (default probe, env override,
/// or an explicit root). Any other base (fixtures, relocated mirrors) is
/// unclassified and treated like the CLI for identity purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AntigravityLayout {
    /// The Antigravity IDE (`~/.gemini/antigravity`).
    Ide,
    /// The `agy` CLI (`~/.gemini/antigravity-cli`).
    Cli,
}

impl AntigravityLayout {
    /// The base directory name under `~/.gemini/` for this layout.
    pub(super) const fn base_dir_name(self) -> &'static str {
        match self {
            Self::Ide => "antigravity",
            Self::Cli => "antigravity-cli",
        }
    }

    /// The `metadata.layout` value for this layout.
    pub(super) const fn metadata_value(self) -> &'static str {
        match self {
            Self::Ide => "ide",
            Self::Cli => "cli",
        }
    }

    /// Classify a base directory by its `.gemini/<name>` location shape.
    pub(super) fn from_base(base: &Path) -> Option<Self> {
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
    pub(super) fn external_id(self, uuid: &str) -> String {
        match self {
            Self::Ide => format!("ide/{uuid}"),
            Self::Cli => uuid.to_string(),
        }
    }
}

impl AntigravityConnector {
    /// Base directories holding `conversations/` and `brain/`.
    ///
    /// `CASS_ANTIGRAVITY_DATA_ROOT` (tests / remote mirrors) names a single
    /// explicit base and replaces the defaults. Otherwise both product stores
    /// are candidates — `~/.gemini/antigravity` (IDE) and
    /// `~/.gemini/antigravity-cli` (`agy` CLI) — and the scan indexes every one
    /// that exists (cass #454).
    pub(super) fn base_roots() -> Vec<PathBuf> {
        if let Some(explicit) = env_path_nonempty("CASS_ANTIGRAVITY_DATA_ROOT") {
            return vec![explicit];
        }
        Self::default_base_roots(&dirs::home_dir().unwrap_or_default())
    }

    /// The default IDE + CLI bases under a home directory, IDE first.
    pub(super) fn default_base_roots(home: &Path) -> Vec<PathBuf> {
        let dot_gemini = home.join(".gemini");
        vec![
            dot_gemini.join(AntigravityLayout::Ide.base_dir_name()),
            dot_gemini.join(AntigravityLayout::Cli.base_dir_name()),
        ]
    }

    /// The `<base>` directory (`brain/`'s parent) for a `brain/<uuid>`
    /// conversation directory.
    pub(super) fn base_of_conversation_dir(conversation_dir: &Path) -> Option<&Path> {
        conversation_dir.parent()?.parent()
    }

    /// Transcript path for a `brain/<uuid>` conversation directory.
    pub(super) fn transcript_path(conversation_dir: &Path) -> PathBuf {
        conversation_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl")
    }

    /// The fuller transcript variant, when agy wrote one.
    pub(super) fn transcript_full_path(conversation_dir: &Path) -> PathBuf {
        conversation_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript_full.jsonl")
    }

    /// Transcript candidates for a `brain/<uuid>` conversation directory, in
    /// the native `get_best_log_file` selection order (`claude_viewer`
    /// `providers/antigravity/discovery.py`): within a single
    /// `.system_generated/logs/` directory the choice is an UNCONDITIONAL
    /// priority — `transcript_full.jsonl` (priority 3) whenever it exists
    /// and is non-empty, else `transcript.jsonl` (priority 2). The native
    /// `(mtime desc, priority desc)` sort only ranks candidates ACROSS
    /// different `brain_dirs`, never inside one logs/ directory, and must
    /// not be applied here: agy flushes `transcript.jsonl` microseconds
    /// after `transcript_full.jsonl` on every turn, so the short file
    /// routinely carries the newer nanosecond mtime while being the
    /// truncated variant. Returned paths are filtered to non-excluded
    /// regular files with `0 < len <= 100 MiB`, in strict priority order
    /// (`transcript_full.jsonl` first); the caller's
    /// first-candidate-that-yields-records rule supplies the fallback the
    /// native `elif` encodes.
    ///
    /// `overview.txt` (plain text, priority 1 in `claude_viewer`) is
    /// intentionally out of scope for FAD's JSONL transcript connector.
    pub(super) fn transcript_candidates(
        conversation_dir: &Path,
        excluded_paths: &[PathBuf],
    ) -> Vec<PathBuf> {
        /// The connector's transcript scan cap (mirrors `read_transcript_records`).
        const MAX_TRANSCRIPT_BYTES: u64 = 100 * 1024 * 1024;
        [
            Self::transcript_full_path(conversation_dir),
            Self::transcript_path(conversation_dir),
        ]
        .into_iter()
        .filter(|path| {
            !path_is_excluded(path, excluded_paths)
                && fs::metadata(path).is_ok_and(|meta| {
                    meta.is_file() && meta.len() > 0 && meta.len() <= MAX_TRANSCRIPT_BYTES
                })
        })
        .collect()
    }

    /// A conversation directory is recognized by the presence of EITHER
    /// transcript variant — the same pair native `get_best_log_file`
    /// considers.
    pub(super) fn is_conversation_dir(path: &Path) -> bool {
        Self::transcript_path(path).is_file() || Self::transcript_full_path(path).is_file()
    }

    /// The `<uuid>` for a conversation directory (its directory name).
    pub(super) fn conversation_uuid(conversation_dir: &Path) -> Option<String> {
        conversation_dir
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
    }

    /// The sibling `conversations/<uuid>.db` for a `brain/<uuid>` directory, when
    /// the standard layout is intact (`<base>/brain/<uuid>` ↔
    /// `<base>/conversations/<uuid>.db`).
    pub(super) fn sibling_db_path(conversation_dir: &Path) -> Option<PathBuf> {
        let uuid = Self::conversation_uuid(conversation_dir)?;
        let base = Self::base_of_conversation_dir(conversation_dir)?;
        let db = base.join("conversations").join(format!("{uuid}.db"));
        db.is_file().then_some(db)
    }

    pub(super) fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
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
    pub(super) fn conversation_dirs(scan_target: &Path) -> Vec<PathBuf> {
        if Self::is_conversation_dir(scan_target) {
            return vec![scan_target.to_path_buf()];
        }

        let mut out = Vec::new();
        // The transcript sits at <conv>/.system_generated/logs/transcript*.jsonl,
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
            let file_name = entry.file_name();
            if !entry.file_type().is_file()
                || (file_name != "transcript.jsonl" && file_name != "transcript_full.jsonl")
            {
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

    /// True when one of the conversation's non-excluded transcript variants
    /// is newer than `since_ts`. Excluded paths are filtered out so an
    /// excluded `transcript_full.jsonl`'s mtime cannot force rescan churn
    /// when only `transcript.jsonl` is admitted.
    pub(super) fn conversation_modified_since(
        conversation_dir: &Path,
        since_ts: Option<i64>,
        excluded_paths: &[PathBuf],
    ) -> bool {
        if since_ts.is_none() {
            return true;
        }
        [
            Self::transcript_path(conversation_dir),
            Self::transcript_full_path(conversation_dir),
        ]
        .iter()
        .any(|path| !path_is_excluded(path, excluded_paths) && file_modified_since(path, since_ts))
    }

    /// Read a transcript file into per-step JSON records, tolerating blank lines
    /// and individual malformed/non-UTF-8 lines (skipped + logged, never fatal),
    /// while discarding incomplete transcripts on mid-stream I/O errors.
    pub(super) fn read_transcript_records(transcript: &Path) -> Vec<Value> {
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

    pub(super) fn read_transcript_records_from(
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

/// Parse a conversation from ONE specific transcript file. Returns `None`
/// when the file yields no usable records; the caller owns candidate order
/// and fallback policy.
#[allow(clippy::too_many_lines)]
fn parse_conversation_from_transcript(
    transcript: &Path,
    conversation_dir: &Path,
    _scan_root: &ScanRoot,
    descriptors: &HashMap<String, SubagentDescriptor>,
    index_complete: bool,
) -> Option<NormalizedConversation> {
    let records = AntigravityConnector::read_transcript_records(transcript);
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
    let conversation_uuid = AntigravityConnector::conversation_uuid(conversation_dir);
    let external_id = conversation_uuid
        .as_ref()
        .map(|uuid| layout.unwrap_or(AntigravityLayout::Cli).external_id(uuid));
    let model = detect_model(&records);
    let workspace = extract_workspace_from_records(&records);

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

    // Subagent verdict: prompt reminder first (native order), then the
    // descriptor index; a complete index with neither signal proves main.
    let verdict = subagent_verdict(
        &records,
        conversation_uuid.as_deref(),
        descriptors,
        index_complete,
    );

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
    if let Some((is_subagent, parent, role, type_name)) = verdict {
        metadata.insert("is_subagent".to_string(), Value::Bool(is_subagent));
        if is_subagent {
            if let Some(parent) = parent {
                let qualified = layout
                    .unwrap_or(AntigravityLayout::Cli)
                    .external_id(&parent);
                metadata.insert("parent_id".to_string(), Value::String(qualified));
            }
            metadata.insert(
                "subagent_role".to_string(),
                Value::String(role.unwrap_or_else(|| "subagent".to_string())),
            );
            metadata.insert(
                "subagent_type".to_string(),
                Value::String(type_name.unwrap_or_else(|| "antigravity_subagent".to_string())),
            );
        }
    }

    Some(NormalizedConversation {
        agent_slug: AGENT_SLUG.to_string(),
        external_id,
        title,
        workspace,
        source_path: transcript.to_path_buf(),
        started_at,
        ended_at,
        metadata: Value::Object(metadata),
        messages,
    })
}

pub(super) fn scan_antigravity_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_antigravity_with_hooks(ctx, &mut SourceScanHooks::default(), on_conversation)
}

pub(super) fn scan_antigravity_with_hooks(
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let excluded_paths = excluded_scan_paths_from_env()?;
    scan_antigravity_with_exclusions(ctx, hooks, &excluded_paths, on_conversation)
}

pub(super) fn scan_antigravity_with_exclusions(
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    excluded_paths: &[PathBuf],
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let roots = AntigravityConnector::source_roots(ctx);
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for root in roots {
        if !root.path.exists() || path_is_excluded(&root.path, excluded_paths) {
            continue;
        }
        // Built over the whole root regardless of since_ts, so a stale parent's
        // descriptor still classifies a fresh child on an incremental scan.
        let (descriptors, index_complete) = subagent_descriptor_index(&root.path, excluded_paths);
        for conversation_dir in AntigravityConnector::conversation_dirs(&root.path) {
            if path_is_excluded(&conversation_dir, excluded_paths) {
                continue;
            }
            if !seen.insert(dedupe_path_key(&conversation_dir)) {
                continue;
            }
            if !AntigravityConnector::conversation_modified_since(
                &conversation_dir,
                ctx.since_ts,
                excluded_paths,
            ) {
                continue;
            }
            // Snapshot every usable transcript before parsing. The boundary
            // hooks are consulted on each candidate IN SELECTION ORDER only:
            // `should_scan` is stateful per source path (a CASS ledger
            // records whichever source it admits), so asking about a fallback
            // the parser never reads would corrupt the admission tracking.
            let discovered_candidates: Vec<DiscoveredSourceFile> =
                AntigravityConnector::transcript_candidates(&conversation_dir, excluded_paths)
                    .into_iter()
                    .map(|transcript| {
                        DiscoveredSourceFile::new(
                            AGENT_SLUG,
                            &root,
                            transcript,
                            DiscoveredSourceRole::PrimarySessionLog,
                            true,
                        )
                        .with_fs_metadata()
                    })
                    .collect();
            for (idx, candidate) in discovered_candidates.iter().enumerate() {
                // A `false` means this candidate already has a valid
                // completion (primary + sidecars unchanged — the recorded
                // conversation would be identical), or a host filter skips
                // the session; either way the conversation is done and no
                // fallback source may be consulted.
                if !hooks.should_scan(candidate) {
                    break;
                }
                let Some(mut conversation) = parse_conversation_from_transcript(
                    &candidate.source_path,
                    &conversation_dir,
                    &root,
                    &descriptors,
                    index_complete,
                ) else {
                    // Empty/torn read: admit the next candidate so its
                    // `should_scan` runs immediately before its read —
                    // the completed primary always lands on the file the
                    // parser actually read.
                    continue;
                };
                ctx.tick();
                root.apply_workspace_rewrite(&mut conversation);
                on_conversation(conversation).with_context(|| {
                    format!(
                        "emit antigravity conversation {}",
                        conversation_dir.display()
                    )
                })?;
                // The completion ledger keys on the transcript actually
                // read; every other candidate rides as an optional sidecar
                // so a change to any usable transcript invalidates it.
                // Declaring the un-parsed variant in `required_sidecars` is
                // backed by CASS's
                // `SourceDependencyPolicy::ObserveParentDirectory`: it
                // pre-captures BOTH candidates via `discover_source_files`
                // into `dependencies.files` before `should_scan` runs, so
                // the sidecar fingerprint exists at admission time.
                // Switching Antigravity to `SelfContained` in CASS would
                // instead require offering the sidecars before parsing.
                let sidecars: Vec<DiscoveredSourceFile> = discovered_candidates
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, d)| DiscoveredSourceFile {
                        required_for_reconstruction: false,
                        ..d.clone()
                    })
                    .collect();
                if !candidate.fs_metadata_changed()
                    && sidecars.iter().all(|d| !d.fs_metadata_changed())
                {
                    hooks.complete(&SourceCompletion {
                        source: candidate.clone(),
                        required_sidecars: sidecars,
                        conversations_emitted: 1,
                    })?;
                }
                break;
            }
        }
    }

    Ok(())
}

pub(super) fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
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
            if !AntigravityConnector::conversation_modified_since(
                &conversation_dir,
                ctx.since_ts,
                &excluded_paths,
            ) {
                continue;
            }

            // The transcripts the parser can use, in native selection order
            // (exclusions already applied inside `transcript_candidates`).
            // `required = !primary_taken` marks the PREFERRED candidate —
            // the first in the static priority order — as required for
            // reconstruction; a torn preferred candidate is not a
            // discovery-time concern because the parser falls back to the
            // next candidate at read time. The other candidate is an
            // optional fallback worth mirroring.
            let mut primary_taken = false;
            for transcript in
                AntigravityConnector::transcript_candidates(&conversation_dir, &excluded_paths)
            {
                if !seen.insert(dedupe_path_key(&transcript)) {
                    continue;
                }
                let required = !primary_taken;
                primary_taken = true;
                out.push(
                    DiscoveredSourceFile::new(
                        AGENT_SLUG,
                        &root,
                        transcript,
                        DiscoveredSourceRole::PrimarySessionLog,
                        required,
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

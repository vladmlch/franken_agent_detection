use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use super::CodexConnector;
use super::parse::{parse_rollout_segment, update_time_bounds};
use super::reader::{self, rollout_session_path};
use super::segments;
use super::subagent::{SubagentEvidence, conversation_title, thread_metadata};
use crate::connectors::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use crate::connectors::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded,
};
use crate::types::{NormalizedConversation, NormalizedMessage};

/// How a rollout file holds its records, from its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RolloutFormat {
    /// `rollout-*.jsonl`: one JSON record per line.
    Jsonl,
    /// `rollout-*.jsonl.zst`: the same lines, zstd-compressed. Codex's
    /// `local_thread_store_compression` feature compresses a finished session
    /// this way. Read only with the `codex-zstd` feature.
    CompressedJsonl,
    /// Legacy `rollout-*.json`: one JSON document.
    Json,
}

impl RolloutFormat {
    pub(super) fn of(path: &Path) -> Option<Self> {
        if reader::is_compressed_rollout(path) {
            return cfg!(feature = "codex-zstd").then_some(Self::CompressedJsonl);
        }
        let name = path.file_name()?.to_str()?;
        if !name.starts_with("rollout-") {
            return None;
        }
        let ext = path.extension()?.to_str()?;
        if ext.eq_ignore_ascii_case("jsonl") {
            Some(Self::Jsonl)
        } else if ext.eq_ignore_ascii_case("json") {
            Some(Self::Json)
        } else {
            None
        }
    }
}

pub(super) enum FileScanMetadata {
    Process,
    Skip,
}

/// An exact JSONL exclusion applies to the plain and compressed representation
/// of that file. Directory exclusions keep their ordinary prefix semantics.
/// Build the logical keys once per operation, before discovery or scan hooks.
pub(super) struct CodexScanExclusions {
    physical_paths: Vec<PathBuf>,
    jsonl_files: HashSet<PathBuf>,
}

impl CodexScanExclusions {
    fn from_env() -> Result<Self> {
        let physical_paths = excluded_scan_paths_from_env()?;
        let jsonl_files = physical_paths
            .iter()
            // An existing directory named `rollout-*.jsonl[.zst]` is still a
            // directory policy, not an exact rollout-file exclusion.
            .filter(|path| !path.is_dir())
            .filter_map(|path| Self::jsonl_path(path).map(Cow::into_owned))
            .collect();
        Ok(Self {
            physical_paths,
            jsonl_files,
        })
    }

    fn jsonl_path(path: &Path) -> Option<Cow<'_, Path>> {
        if reader::is_compressed_rollout(path) {
            // Preserve the actual JSONL suffix and stem spelling. The reader
            // accepts `.JSONL.ZST`, and paths can be case-sensitive.
            Some(Cow::Owned(path.with_extension("")))
        } else if RolloutFormat::of(path) == Some(RolloutFormat::Jsonl) {
            Some(Cow::Borrowed(path))
        } else {
            None
        }
    }

    fn excludes(&self, path: &Path) -> bool {
        if path_is_excluded(path, &self.physical_paths) {
            return true;
        }
        if self.jsonl_files.is_empty() {
            return false;
        }
        let Some(logical) = Self::jsonl_path(path) else {
            return false;
        };
        if self.jsonl_files.contains(logical.as_ref()) {
            return true;
        }
        // Resolve the source itself before removing its compression suffix:
        // an explicitly scoped leaf symlink may have a different rollout name
        // from its target. The policy reader already retains resolved entries.
        // With a configured policy, an unresolvable source is not known safe.
        fs::canonicalize(path).map_or(true, |resolved| {
            Self::jsonl_path(&resolved)
                .is_some_and(|logical| self.jsonl_files.contains(logical.as_ref()))
        })
    }
}

impl CodexConnector {
    fn is_under_codex_dir(path: &Path) -> bool {
        path.ancestors().any(|ancestor| {
            ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == ".codex")
        })
    }

    fn append_explicit_roots(roots: &mut Vec<PathBuf>, base: &Path) {
        if base.is_file() {
            roots.push(base.to_path_buf());
            return;
        }

        roots.push(base.to_path_buf());

        if !Self::is_under_codex_dir(base) {
            roots.push(base.join(".codex"));
        }
    }

    pub(super) fn home() -> PathBuf {
        if let Some(explicit) = env_path_nonempty("CODEX_HOME") {
            return explicit;
        }
        dirs::home_dir().unwrap_or_default().join(".codex")
    }

    pub(super) fn sessions_dir(home: &Path) -> PathBuf {
        let sessions = home.join("sessions");
        if sessions.exists() {
            sessions
        } else {
            home.to_path_buf()
        }
    }

    pub(super) fn is_rollout_file(path: &Path) -> bool {
        RolloutFormat::of(path).is_some()
    }

    fn sessions_dir_for_explicit_file(path: &Path) -> Option<PathBuf> {
        path.ancestors()
            .find(|ancestor| {
                ancestor.file_name().and_then(|name| name.to_str()) == Some("sessions")
            })
            .map(Path::to_path_buf)
    }

    pub(super) fn rollout_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let sessions = Self::sessions_dir(root);
        if !sessions.exists() {
            return out;
        }
        for entry in WalkDir::new(sessions).into_iter().flatten() {
            if entry.file_type().is_file() {
                match RolloutFormat::of(entry.path()) {
                    // Codex removes the plain rollout once the compressed one
                    // is in place. While both exist they hold one session,
                    // read from the plain file.
                    Some(RolloutFormat::CompressedJsonl)
                        if entry.path().with_extension("").is_file() => {}
                    Some(_) => out.push(entry.path().to_path_buf()),
                    None => {}
                }
            }
        }
        // Keep connector traversal deterministic across filesystems/runs.
        out.sort();
        out
    }

    /// Every rollout file the scan roots offer, in root then filename order,
    /// before exclusions and freshness. An explicit file naming a thread
    /// expands to that thread's segments: a continuation scoped on its own
    /// must still produce the whole conversation under the thread's identity.
    pub(super) fn rollout_candidates(ctx: &ScanContext) -> Vec<segments::Candidate> {
        let mut out = Vec::new();
        let mut seen_files: HashSet<PathBuf> = HashSet::new();
        let mut walked: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

        for root in Self::source_roots(ctx) {
            let explicit_file = root
                .path
                .is_file()
                .then_some(root.path.clone())
                .filter(|path| Self::is_rollout_file(path));
            let home = explicit_file
                .as_ref()
                .and_then(|path| path.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| root.path.clone());
            if !home.exists() {
                continue;
            }
            let sessions_dir = explicit_file
                .as_ref()
                .and_then(|path| Self::sessions_dir_for_explicit_file(path))
                .unwrap_or_else(|| Self::sessions_dir(&home));

            let thread = explicit_file
                .as_ref()
                .and_then(|path| segments::SegmentName::of(path));
            let siblings = thread.as_ref().map(|name| {
                walked
                    .entry(sessions_dir.clone())
                    .or_insert_with(|| Self::rollout_files(&sessions_dir))
                    .iter()
                    .filter(|path| {
                        segments::SegmentName::of(path)
                            .is_some_and(|other| other.thread_id == name.thread_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            });
            let mut files = siblings.unwrap_or_else(|| {
                explicit_file
                    .clone()
                    .map_or_else(|| Self::rollout_files(&home), |path| vec![path])
            });
            if thread.is_some() {
                // The walk drops a compressed file whose plain twin exists.
                // Offer the scoped spelling too and let segment ordering pick
                // the representation, so an explicit root is never ignored.
                files.extend(explicit_file);
            }

            for file in files {
                if seen_files.insert(dedupe_path_key(&file)) {
                    out.push(segments::Candidate {
                        root: root.clone(),
                        path: file,
                        sessions_dir: sessions_dir.clone(),
                    });
                }
            }
        }
        out
    }

    /// A thread is fresh when ANY of its segments is, so an incremental scan
    /// admitting only a new continuation still re-reads the whole thread.
    pub(super) fn thread_modified_since(thread: &segments::Thread, since_ts: Option<i64>) -> bool {
        thread.segments.iter().any(|segment| {
            matches!(
                Self::file_metadata_if_modified(&segment.path, since_ts),
                FileScanMetadata::Process
            )
        })
    }

    pub(super) fn segment_external_id(path: &Path, sessions_dir: &Path) -> Option<String> {
        path.strip_prefix(sessions_dir)
            .ok()
            .and_then(|rel| {
                rollout_session_path(rel)
                    .to_str()
                    .map(std::string::ToString::to_string)
            })
            .or_else(|| {
                path.file_name().and_then(|name| {
                    rollout_session_path(Path::new(name))
                        .to_str()
                        .map(std::string::ToString::to_string)
                })
            })
    }

    pub(super) fn file_metadata_if_modified(
        path: &Path,
        since_ts: Option<i64>,
    ) -> FileScanMetadata {
        let Ok(metadata) = fs::metadata(path) else {
            return FileScanMetadata::Process;
        };
        if Self::metadata_modified_since(&metadata, since_ts) {
            FileScanMetadata::Process
        } else {
            FileScanMetadata::Skip
        }
    }

    fn metadata_modified_since(metadata: &fs::Metadata, since_ts: Option<i64>) -> bool {
        since_ts.is_none_or(|ts| {
            let threshold = ts.saturating_sub(1_000);
            metadata.modified().map_or(true, |modified| {
                modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(true, |duration| {
                        i64::try_from(duration.as_millis()).unwrap_or(i64::MAX) >= threshold
                    })
            })
        })
    }

    pub(super) fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let is_codex_dir = ctx.data_dir.to_str().is_some_and(|s| {
            s.contains(".codex") || s.ends_with("/codex") || s.ends_with("\\codex")
        }) && ctx.data_dir.join("sessions").exists();

        let mut roots: Vec<ScanRoot> =
            if ctx.use_default_detection() {
                if is_codex_dir {
                    vec![ScanRoot::local(ctx.data_dir.clone())]
                } else {
                    vec![ScanRoot::local(Self::home())]
                }
            } else {
                let mut explicit = Vec::new();
                for scan_root in &ctx.scan_roots {
                    if let Some(mirrored) = scan_root.mirrored_store(&[".codex", "sessions"]) {
                        explicit.push(mirrored);
                    } else {
                        Self::append_explicit_roots(&mut explicit, &scan_root.path);
                    }
                }
                explicit
                    .into_iter()
                    .map(|path| {
                        if let Some(root) = ctx.scan_roots.iter().find(|root| {
                            path.starts_with(&root.path) || root.path.starts_with(&path)
                        }) {
                            root.with_path(path)
                        } else {
                            ScanRoot::local(path)
                        }
                    })
                    .collect()
            };

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    pub(super) fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = CodexScanExclusions::from_env()?;
        let mut out = Vec::new();

        for thread in segments::group(Self::rollout_candidates(ctx)) {
            // A thread is reconstructed from every segment, so one excluded
            // segment withdraws the whole thread instead of a fragment.
            if thread
                .segments
                .iter()
                .any(|segment| excluded_paths.excludes(&segment.path))
            {
                continue;
            }
            if !Self::thread_modified_since(&thread, ctx.since_ts) {
                continue;
            }
            for segment in &thread.segments {
                out.push(
                    DiscoveredSourceFile::new(
                        "codex",
                        &segment.root,
                        segment.path.clone(),
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }

        Ok(out)
    }
}

pub(super) fn scan_codex_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_codex_with_hooks(ctx, &mut SourceScanHooks::default(), on_conversation)
}

pub(super) struct MergedThread {
    messages: Vec<NormalizedMessage>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    session_cwd: Option<PathBuf>,
    subagent: Option<SubagentEvidence>,
}

fn merge_thread_segments(thread: &segments::Thread, ctx: &ScanContext) -> Result<MergedThread> {
    let mut messages = Vec::new();
    let mut started_at = None;
    let mut ended_at = None;
    let mut session_cwd: Option<PathBuf> = None;
    // The verdict comes from the earliest non-continuation segment carrying a
    // session_meta header (the original when present); a continuation header
    // cannot establish is_subagent=false because paginated continuations carry
    // history_base/forked_from_id rather than repeating spawn metadata.
    let mut subagent: Option<SubagentEvidence> = None;
    for segment in &thread.segments {
        let Some(segment_format) = RolloutFormat::of(&segment.path) else {
            continue;
        };
        let Some(parsed) = parse_rollout_segment(&segment.path, segment_format, ctx)? else {
            continue;
        };
        messages.extend(parsed.messages);
        update_time_bounds(&mut started_at, &mut ended_at, parsed.started_at);
        update_time_bounds(&mut started_at, &mut ended_at, parsed.ended_at);
        if session_cwd.is_none() {
            session_cwd = parsed.cwd;
        }
        let is_continuation =
            segments::SegmentName::of(&segment.path).is_some_and(|name| name.continuation);
        if subagent.is_none()
            && let Some(ev) = parsed.subagent
            && (!is_continuation || ev.is_subagent)
        {
            subagent = Some(ev);
        }
    }
    crate::types::reindex_messages(&mut messages);
    Ok(MergedThread {
        messages,
        started_at,
        ended_at,
        session_cwd,
        subagent,
    })
}

pub(super) fn scan_codex_with_hooks(
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let excluded_paths = CodexScanExclusions::from_env()?;

    for thread in segments::group(CodexConnector::rollout_candidates(ctx)) {
        // Excluded sources must not reach pre-parse hooks or completions, and
        // one excluded segment withdraws the whole thread: a fragment would
        // replace the complete conversation under the same identity.
        if thread
            .segments
            .iter()
            .any(|segment| excluded_paths.excludes(&segment.path))
        {
            continue;
        }
        if !CodexConnector::thread_modified_since(&thread, ctx.since_ts) {
            continue;
        }

        // Pre-parse identity for every segment, mirrored from
        // discover_sources() (FAD#22). Fingerprints are captured before parsing
        // so a segment written mid-scan withholds the completion.
        let discovered: Vec<DiscoveredSourceFile> = thread
            .segments
            .iter()
            .map(|segment| {
                DiscoveredSourceFile::new(
                    "codex",
                    &segment.root,
                    segment.path.clone(),
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata()
            })
            .collect();
        // The identity segment is offered LAST: a host binds its most recent
        // pre-parse observation to the completed source, and a continuation it
        // has never seen must admit the thread even when the original is
        // already reusable.
        let mut admit = false;
        for source in discovered.iter().skip(1).chain(discovered.first()) {
            admit |= hooks.should_scan(source);
        }
        if !admit {
            continue;
        }
        let Some((identity, sidecars)) = discovered.split_first() else {
            continue;
        };
        let identity_segment = &thread.segments[0];
        let Some(format) = RolloutFormat::of(&identity_segment.path) else {
            continue;
        };
        let external_id = CodexConnector::segment_external_id(
            &identity_segment.path,
            &identity_segment.sessions_dir,
        );

        let merged = merge_thread_segments(&thread, ctx)?;
        if merged.messages.is_empty() {
            continue;
        }

        let title = conversation_title(&merged.messages);

        let mut conv = NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id,
            title,
            workspace: merged.session_cwd,
            source_path: identity_segment.path.clone(),
            started_at: merged.started_at,
            ended_at: merged.ended_at,
            metadata: thread_metadata(&thread, format, merged.subagent.as_ref()),
            messages: merged.messages,
        };
        identity_segment.root.apply_workspace_rewrite(&mut conv);
        on_conversation(conv)?;

        // Source complete: the thread's conversation was delivered. Withheld
        // when any of its segments changed while being parsed.
        if !identity.fs_metadata_changed()
            && sidecars
                .iter()
                .all(|sidecar| !sidecar.fs_metadata_changed())
        {
            hooks.complete(&SourceCompletion {
                source: identity.clone(),
                required_sidecars: sidecars.to_vec(),
                conversations_emitted: 1,
            })?;
        }
    }

    Ok(())
}

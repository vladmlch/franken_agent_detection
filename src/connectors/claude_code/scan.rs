use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::ClaudeCodeConnector;
use super::parse::{ClaudeFlavor, parse_claude_records};
use super::retained_content;
use crate::connectors::scan::{
    DiscoveredSourceFile, ScanContext, ScanRoot, SourceCompletion, SourceScanHooks,
};
use crate::connectors::utils::{dedupe_path_key, excluded_scan_paths_from_env, path_is_excluded};
use crate::connectors::{
    extract_invocations_from_content_blocks, file_modified_since, parse_timestamp,
    unwrap_skill_invocations,
};
use crate::types::{NormalizedConversation, NormalizedMessage};

#[allow(clippy::too_many_lines)]
pub(super) fn scan_claude_with_callback(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_claude_with_callback_with_exclusions(
        ctx,
        on_conversation,
        &excluded_scan_paths_from_env()?,
        &mut SourceScanHooks::default(),
    )
}

pub(super) fn scan_claude_with_callback_with_exclusions(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    excluded_paths: &[PathBuf],
    hooks: &mut SourceScanHooks<'_>,
) -> Result<()> {
    scan_claude_with_readers(
        ctx,
        on_conversation,
        excluded_paths,
        hooks,
        &mut |path| fs::File::open(path).map(BufReader::new),
        &mut |path| fs::read_to_string(path),
    )
}

/// Retain one original error and a count, rather than buffering failures for
/// an entire corpus. Callback failures bypass this source-only recovery.
#[derive(Default)]
struct SourceReadFailures {
    first: Option<anyhow::Error>,
    count: usize,
}

impl SourceReadFailures {
    fn record(&mut self, path: &Path, operation: &str, error: io::Error) {
        tracing::warn!(path = %path.display(), %error, operation, "claude_code skipping unreadable source");
        self.count += 1;
        if self.first.is_none() {
            self.first =
                Some(anyhow::Error::new(error).context(format!("{operation} {}", path.display())));
        }
    }

    fn finish(self) -> Result<()> {
        match self.first {
            Some(error) => Err(error.context(format!(
                "Claude Code scan could not read {} source(s)",
                self.count
            ))),
            None => Ok(()),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn scan_claude_with_readers<R: BufRead>(
    ctx: &ScanContext,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    excluded_paths: &[PathBuf],
    hooks: &mut SourceScanHooks<'_>,
    open_jsonl: &mut impl FnMut(&Path) -> io::Result<R>,
    read_json: &mut impl FnMut(&Path) -> io::Result<String>,
) -> Result<()> {
    let roots: Vec<ScanRoot> = ClaudeCodeConnector::source_roots(ctx);
    // Overlapping explicit roots (or a symlinked CLAUDE_CONFIG_DIR aliasing
    // another root) reach the same transcript twice; dedupe on the
    // lossless path key across ALL roots, first occurrence wins.
    let mut seen_files: HashSet<PathBuf> = HashSet::new();

    let mut file_count = 0;
    let mut read_failures = SourceReadFailures::default();

    for root in roots {
        let explicit_file_root = root.path.is_file();
        let scan_target = root.path.clone();
        let external_id_root = if explicit_file_root {
            ClaudeCodeConnector::projects_root_for_explicit_file(&root.path)
                .or_else(|| root.path.parent().map(Path::to_path_buf))
        } else {
            Some(scan_target.clone())
        };

        if !scan_target.exists() {
            continue;
        }

        let session_paths = if explicit_file_root {
            vec![scan_target.clone()]
        } else {
            ClaudeCodeConnector::session_files(&scan_target)
        };

        'sources: for path in session_paths {
            if path_is_excluded(&path, excluded_paths) {
                tracing::debug!(
                    path = %path.display(),
                    "claude_code skipping excluded session file"
                );
                continue;
            }
            if !seen_files.insert(dedupe_path_key(&path)) {
                continue;
            }
            let ext = path.extension().and_then(|s| s.to_str());
            if !file_modified_since(&path, ctx.since_ts) {
                continue;
            }
            // Pre-parse source identity: constructed exactly like
            // discover_sources_with_exclusions(), with size/mtime observed
            // BEFORE the file is opened (FAD#22).
            let discovered = DiscoveredSourceFile::new(
                "claude_code",
                &root,
                path.clone(),
                ClaudeCodeConnector::discovered_source_role(&path),
                true,
            )
            .with_fs_metadata();
            if !hooks.should_scan(&discovered) {
                tracing::debug!(
                    path = %path.display(),
                    "claude_code host ledger skipped unchanged source"
                );
                continue;
            }
            ctx.tick();
            file_count += 1;
            if file_count <= 3 {
                tracing::debug!(path = %path.display(), "claude_code found file");
            }

            let mut messages = Vec::new();
            let mut started_at: Option<i64> = None;
            let mut ended_at: Option<i64> = None;
            let mut workspace: Option<PathBuf> = None;
            let mut session_id: Option<String> = None;
            let mut cli_session_id: Option<String> = None;
            let git_branch: Option<String> = None;
            let mut permission_mode: Option<String> = None;
            let mut source_kind = "claude_code";

            // Subagent transcripts live at
            // `<session>/subagents/agent-*.jsonl` and are separate
            // conversations; surface the relationship instead of leaving the
            // parent link implicit in the path.
            let is_subagent_transcript = path
                .components()
                .any(|c| c.as_os_str().to_str() == Some("subagents"));
            let mut parent_session_id: Option<String> = None;
            if is_subagent_transcript {
                let components: Vec<_> = path.components().collect();
                if let Some(pos) = components
                    .iter()
                    .position(|c| c.as_os_str().to_str() == Some("subagents"))
                    && pos > 0
                {
                    parent_session_id = components[pos - 1].as_os_str().to_str().map(String::from);
                }
            }

            if ext == Some("jsonl") {
                let reader = match open_jsonl(&path) {
                    Ok(reader) => reader,
                    Err(error) => {
                        read_failures.record(&path, "open", error);
                        continue;
                    }
                };

                let parsed = match parse_claude_records(reader, &path, ClaudeFlavor::Claude) {
                    Ok(Some(parsed)) => parsed,
                    Ok(None) => {
                        if file_count <= 3 {
                            tracing::debug!(path = %path.display(), "claude_code no messages extracted");
                        }
                        continue;
                    }
                    Err(error) => {
                        let io_err = error.downcast::<io::Error>()?;
                        read_failures.record(&path, "read", io_err);
                        // Never emit or certify an incompletely read source.
                        continue 'sources;
                    }
                };
                tracing::debug!(path = %path.display(), messages = parsed.messages.len(), "claude_code extracted messages");

                let mut conv = NormalizedConversation {
                    agent_slug: "claude_code".into(),
                    external_id: external_id_root
                        .as_deref()
                        .and_then(|base| path.strip_prefix(base).ok())
                        .and_then(|rel| rel.to_str())
                        .map(std::string::ToString::to_string)
                        .or_else(|| {
                            path.file_name()
                                .and_then(|s| s.to_str())
                                .map(std::string::ToString::to_string)
                        }),
                    title: parsed.title,
                    workspace: parsed.workspace,
                    source_path: path.clone(),
                    started_at: parsed.started_at,
                    ended_at: parsed.ended_at,
                    metadata: parsed.metadata,
                    messages: parsed.messages,
                };
                root.apply_workspace_rewrite(&mut conv);
                on_conversation(conv)?;

                if discovered.fs_metadata_changed() {
                    tracing::debug!(
                        path = %path.display(),
                        "claude_code source changed during parse; completion withheld"
                    );
                } else {
                    hooks.complete(&SourceCompletion {
                        source: discovered,
                        required_sidecars: Vec::new(),
                        conversations_emitted: 1,
                    })?;
                }
                continue;
            }
            if let Ok(metadata) = fs::metadata(&path)
                && metadata.len() > 100 * 1024 * 1024
            {
                tracing::debug!(
                    path = %path.display(),
                    size_bytes = metadata.len(),
                    "skipping large file (>100MB)"
                );
                continue;
            }

            let content_string = match read_json(&path) {
                Ok(content) => content,
                Err(error) => {
                    read_failures.record(&path, "read", error);
                    continue;
                }
            };
            let val: Value = match serde_json::from_str(&content_string) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "claude_code skipping malformed JSON");
                    continue;
                }
            };

            let json_title = ClaudeCodeConnector::non_empty_json_string(&val, "title");
            if workspace.is_none() {
                workspace =
                    ClaudeCodeConnector::non_empty_json_string(&val, "cwd").map(PathBuf::from);
            }
            if session_id.is_none() {
                session_id = ClaudeCodeConnector::non_empty_json_string(&val, "sessionId");
            }
            if cli_session_id.is_none() {
                cli_session_id = ClaudeCodeConnector::non_empty_json_string(&val, "cliSessionId");
            }
            if permission_mode.is_none() {
                permission_mode =
                    ClaudeCodeConnector::non_empty_json_string(&val, "permissionMode");
            }
            let sidecar_created_at = val.get("createdAt").and_then(parse_timestamp);
            let sidecar_last_activity_at = val.get("lastActivityAt").and_then(parse_timestamp);
            started_at = started_at
                .or(sidecar_created_at)
                .or(sidecar_last_activity_at);
            ended_at = ended_at.or(sidecar_last_activity_at).or(sidecar_created_at);

            if let Some(arr) = val.get("messages").and_then(|m| m.as_array()) {
                for item in arr {
                    let role = item
                        .get("role")
                        .or_else(|| item.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("agent");
                    let created = item
                        .get("timestamp")
                        .or_else(|| item.get("time"))
                        .and_then(parse_timestamp);

                    started_at = match (started_at, created) {
                        (Some(curr), Some(ts)) => Some(curr.min(ts)),
                        (None, Some(ts)) => Some(ts),
                        (other, None) => other,
                    };
                    ended_at = match (ended_at, created) {
                        (Some(curr), Some(ts)) => Some(curr.max(ts)),
                        (None, Some(ts)) => Some(ts),
                        (Some(curr), None) => Some(curr),
                        (None, None) => None,
                    };

                    let content_val = item.get("content").or_else(|| item.get("text"));
                    let content_str = content_val
                        .map(retained_content::flatten_claude_content)
                        .unwrap_or_default();

                    if content_str.trim().is_empty() {
                        continue;
                    }

                    let mut invocations =
                        content_val.map_or_else(Vec::new, extract_invocations_from_content_blocks);
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
            if messages.is_empty()
                && ClaudeCodeConnector::path_is_desktop_sidecar(&path)
                && let Some(message) = ClaudeCodeConnector::desktop_sidecar_metadata_message(&val)
            {
                source_kind = "claude_code_desktop_sidecar";
                messages.push(message);
            }
            crate::types::reindex_messages(&mut messages);

            if messages.is_empty() {
                if file_count <= 3 {
                    tracing::debug!(path = %path.display(), "claude_code no messages extracted");
                }
                continue;
            }
            tracing::debug!(path = %path.display(), messages = messages.len(), "claude_code extracted messages");

            let title = json_title.or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == "user")
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
                        workspace
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .and_then(|n| n.to_str())
                            .map(String::from)
                    })
            });

            let mut conv = NormalizedConversation {
                agent_slug: "claude_code".into(),
                external_id: if source_kind == "claude_code_desktop_sidecar" {
                    cli_session_id.clone().or_else(|| session_id.clone())
                } else {
                    external_id_root
                        .as_deref()
                        .and_then(|base| path.strip_prefix(base).ok())
                        .and_then(|rel| rel.to_str())
                        .map(std::string::ToString::to_string)
                        .or_else(|| {
                            path.file_name()
                                .and_then(|s| s.to_str())
                                .map(std::string::ToString::to_string)
                        })
                },
                title,
                workspace,
                source_path: path.clone(),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "source": source_kind,
                    "sessionId": session_id,
                    "cliSessionId": cli_session_id,
                    "gitBranch": git_branch,
                    "permissionMode": permission_mode,
                    "bodyAvailable": source_kind != "claude_code_desktop_sidecar",
                    "sidechain": is_subagent_transcript,
                    "parentSessionId": parent_session_id
                }),
                messages,
            };
            root.apply_workspace_rewrite(&mut conv);
            on_conversation(conv)?;

            // Source complete: the (single) conversation derived from this
            // transcript was delivered. Suppressed when the file changed
            // while it was being parsed — the host must re-observe it.
            if discovered.fs_metadata_changed() {
                tracing::debug!(
                    path = %path.display(),
                    "claude_code source changed during parse; completion withheld"
                );
            } else {
                hooks.complete(&SourceCompletion {
                    source: discovered,
                    required_sidecars: Vec::new(),
                    conversations_emitted: 1,
                })?;
            }
        }
    }

    read_failures.finish()
}

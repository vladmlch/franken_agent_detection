use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use super::claude_code::{ClaudeFlavor, ParsedClaudeFile, parse_claude_records};
use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded,
};
use super::{Connector, file_modified_since, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation, NormalizedMessage, reindex_messages};

/// Qoder CLI/IDE session ingestion (`~/.qoder/projects/<workspace-slug>/<uuid>.jsonl`).
pub struct QoderConnector;

impl Default for QoderConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl QoderConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub(crate) fn projects_root() -> PathBuf {
        Self::projects_root_resolved(
            env_path_nonempty("QODER_HOME").as_deref(),
            dirs::home_dir().as_deref(),
        )
    }

    pub(crate) fn projects_root_resolved(
        qoder_home: Option<&Path>,
        home_dir: Option<&Path>,
    ) -> PathBuf {
        if let Some(explicit) = qoder_home {
            return explicit.join("projects");
        }
        home_dir.map_or_else(
            || PathBuf::from(".qoder/projects"),
            |h| h.join(".qoder/projects"),
        )
    }

    fn path_has_subagents_component(path: &Path) -> bool {
        path.components()
            .any(|c| c.as_os_str().to_str() == Some("subagents"))
    }

    fn main_session_files(scan_target: &Path) -> Vec<PathBuf> {
        if scan_target.is_file() {
            let is_jsonl = scan_target.extension().and_then(|s| s.to_str()) == Some("jsonl");
            if is_jsonl && !Self::path_has_subagents_component(scan_target) {
                return vec![scan_target.to_path_buf()];
            }
            return Vec::new();
        }
        let mut files = Vec::new();
        for entry in WalkDir::new(scan_target).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if Self::path_has_subagents_component(path) {
                continue;
            }
            let is_hidden = path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if !is_hidden && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                files.push(path.to_path_buf());
            }
        }
        files.sort();
        files
    }

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let looks_like_root = |path: &Path| path.join("projects").is_dir();

        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                vec![ScanRoot::local(ctx.data_dir.join("projects"))]
            } else if ctx.allow_local_default_stores {
                vec![ScanRoot::local(Self::projects_root())]
            } else {
                Vec::new()
            }
        } else {
            ctx.scan_roots
                .iter()
                .map(|scan_root| {
                    scan_root
                        .mirrored_store(&[".qoder", "projects"])
                        .map_or_else(
                            || {
                                if scan_root.path.is_file() {
                                    scan_root.clone()
                                } else if scan_root.path.join("projects").is_dir() {
                                    scan_root.with_path(scan_root.path.join("projects"))
                                } else {
                                    scan_root.clone()
                                }
                            },
                            |mirrored| scan_root.with_path(mirrored),
                        )
                })
                .collect()
        };

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut seen_files = HashSet::new();
        let mut out = Vec::new();
        for root in Self::source_roots(ctx) {
            if !root.path.exists() {
                continue;
            }
            for path in Self::main_session_files(&root.path) {
                if path_is_excluded(&path, &excluded_paths) {
                    tracing::debug!(
                        path = %path.display(),
                        "qoder skipping excluded session source"
                    );
                    continue;
                }
                if !seen_files.insert(dedupe_path_key(&path)) {
                    continue;
                }
                if !file_modified_since(&path, ctx.since_ts) {
                    continue;
                }
                out.push(
                    DiscoveredSourceFile::new(
                        "qoder",
                        &root,
                        path,
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

/// Resolve the active branch from `active-leaf` records, latest timestamp, or
/// last record in file order, walking `parentUuid` back to the root with cycle
/// protection.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn active_branch(
    records: &[(String, Option<String>)],
    leaves: &[String],
) -> Vec<String> {
    active_branch_with_timestamps(records, &HashMap::new(), leaves)
}

pub(crate) fn active_branch_with_timestamps(
    records: &[(String, Option<String>)],
    timestamps: &HashMap<String, i64>,
    leaves: &[String],
) -> Vec<String> {
    if records.is_empty() {
        return Vec::new();
    }

    let mut parent_map: HashMap<&str, Option<&str>> = HashMap::with_capacity(records.len());
    for (uuid, parent) in records {
        parent_map.insert(uuid.as_str(), parent.as_deref());
    }

    let mut target_leaf: Option<&str> = leaves
        .iter()
        .rev()
        .map(String::as_str)
        .find(|leaf| parent_map.contains_key(leaf));

    if target_leaf.is_none() {
        let mut best: Option<(&str, i64)> = None;
        for (uuid, _) in records {
            if let Some(&ts) = timestamps.get(uuid.as_str())
                && best.is_none_or(|(_, best_ts)| ts > best_ts)
            {
                best = Some((uuid.as_str(), ts));
            }
        }
        target_leaf = best
            .map(|(uuid, _)| uuid)
            .or_else(|| records.last().map(|(uuid, _)| uuid.as_str()));
    }

    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut curr = target_leaf;
    while let Some(uid) = curr {
        if !visited.insert(uid) {
            break;
        }
        let Some(&parent) = parent_map.get(uid) else {
            break;
        };
        chain.push(uid.to_string());
        curr = parent;
    }

    chain.reverse();
    chain
}

fn filter_active_branch_messages(parsed: ParsedClaudeFile) -> Option<FilteredQoderSession> {
    let ParsedClaudeFile {
        mut messages,
        message_uuids,
        records_by_uuid,
        record_timestamps,
        active_leaves,
        json_title,
        workspace,
        model,
        ..
    } = parsed;

    if !records_by_uuid.is_empty() {
        let branch =
            active_branch_with_timestamps(&records_by_uuid, &record_timestamps, &active_leaves);
        let mut slots: Vec<Option<NormalizedMessage>> = messages.into_iter().map(Some).collect();
        let mut by_uuid: HashMap<&str, Vec<usize>> = HashMap::new();
        for (idx, msg_uuid) in message_uuids.iter().enumerate() {
            if let Some(uid) = msg_uuid.as_deref() {
                by_uuid.entry(uid).or_default().push(idx);
            }
        }
        let mut filtered = Vec::new();
        for uid in &branch {
            if let Some(indices) = by_uuid.get(uid.as_str()) {
                for &idx in indices {
                    if let Some(msg) = slots[idx].take() {
                        filtered.push(msg);
                    }
                }
            }
        }
        reindex_messages(&mut filtered);
        messages = filtered;
    }

    if messages.is_empty() {
        return None;
    }

    let mut started_at: Option<i64> = None;
    let mut ended_at: Option<i64> = None;
    for msg in &messages {
        if let Some(ts) = msg.created_at {
            started_at = Some(started_at.map_or(ts, |curr| curr.min(ts)));
            ended_at = Some(ended_at.map_or(ts, |curr| curr.max(ts)));
        }
    }

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

    Some(FilteredQoderSession {
        messages,
        title,
        workspace,
        model,
        started_at,
        ended_at,
    })
}

struct FilteredQoderSession {
    messages: Vec<NormalizedMessage>,
    title: Option<String>,
    workspace: Option<PathBuf>,
    model: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
}

#[derive(Default)]
struct SourceReadFailures {
    first: Option<anyhow::Error>,
    count: usize,
}

impl SourceReadFailures {
    fn record(&mut self, path: &Path, operation: &str, error: io::Error) {
        tracing::warn!(path = %path.display(), %error, operation, "qoder skipping unreadable source");
        self.count += 1;
        if self.first.is_none() {
            self.first =
                Some(anyhow::Error::new(error).context(format!("{operation} {}", path.display())));
        }
    }

    fn finish(self) -> Result<()> {
        match self.first {
            Some(error) => Err(error.context(format!(
                "Qoder scan could not read {} source(s)",
                self.count
            ))),
            None => Ok(()),
        }
    }
}

fn scan_qoder_with_callback(
    ctx: &ScanContext,
    excluded_paths: &[PathBuf],
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let roots = QoderConnector::source_roots(ctx);
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    let mut read_failures = SourceReadFailures::default();

    for root in roots {
        if !root.path.exists() {
            continue;
        }
        for path in QoderConnector::main_session_files(&root.path) {
            if path_is_excluded(&path, excluded_paths) {
                tracing::debug!(
                    path = %path.display(),
                    "qoder skipping excluded session file"
                );
                continue;
            }
            if !seen_files.insert(dedupe_path_key(&path)) {
                continue;
            }
            if !file_modified_since(&path, ctx.since_ts) {
                continue;
            }

            let discovered = DiscoveredSourceFile::new(
                "qoder",
                &root,
                path.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            )
            .with_fs_metadata();
            if !hooks.should_scan(&discovered) {
                tracing::debug!(
                    path = %path.display(),
                    "qoder host ledger skipped unchanged source"
                );
                continue;
            }
            ctx.tick();

            let file = match fs::File::open(&path) {
                Ok(file) => file,
                Err(error) => {
                    read_failures.record(&path, "open", error);
                    continue;
                }
            };
            let parsed =
                match parse_claude_records(BufReader::new(file), &path, ClaudeFlavor::Qoder) {
                    Ok(Some(parsed)) => parsed,
                    Ok(None) => continue,
                    Err(error) => {
                        let io_err = error.downcast::<io::Error>()?;
                        read_failures.record(&path, "read", io_err);
                        continue;
                    }
                };

            let Some(filtered) = filter_active_branch_messages(parsed) else {
                continue;
            };

            let external_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string);

            let mut conv = NormalizedConversation {
                agent_slug: "qoder".into(),
                external_id,
                title: filtered.title,
                workspace: filtered.workspace,
                source_path: path.clone(),
                started_at: filtered.started_at,
                ended_at: filtered.ended_at,
                metadata: serde_json::json!({
                    "source": "qoder_jsonl",
                    "is_subagent": false,
                    "model": filtered.model,
                }),
                messages: filtered.messages,
            };
            root.apply_workspace_rewrite(&mut conv);
            on_conversation(conv)?;

            if discovered.fs_metadata_changed() {
                tracing::debug!(
                    path = %path.display(),
                    "qoder source changed during parse; completion withheld"
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

impl Connector for QoderConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("qoder").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        self.scan_with_callback(ctx, &mut |conv| {
            convs.push(conv);
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
        scan_qoder_with_callback(
            ctx,
            &excluded_scan_paths_from_env()?,
            &mut SourceScanHooks::default(),
            on_conversation,
        )
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
        scan_qoder_with_callback(
            ctx,
            &excluded_scan_paths_from_env()?,
            hooks,
            on_conversation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn active_branch_on_fixture_keeps_leaf_and_drops_abandoned_sibling() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/qoder/projects/-tmp-demo/11111111-1111-4111-8111-111111111111.jsonl",
        );
        let file = fs::File::open(&fixture).unwrap();
        let parsed = parse_claude_records(BufReader::new(file), &fixture, ClaudeFlavor::Qoder)
            .unwrap()
            .unwrap();
        let branch = active_branch(&parsed.records_by_uuid, &parsed.active_leaves);
        assert_eq!(
            branch,
            vec!["u-root-1", "att-1", "a-kept-1", "u-kept-tool-1", "a-kept-2"]
        );
    }

    #[test]
    fn active_branch_without_leaf_falls_back_to_latest_timestamp() {
        let records = vec![
            ("root".to_string(), None),
            ("branch-a".to_string(), Some("root".to_string())),
            ("branch-b".to_string(), Some("root".to_string())),
        ];
        let mut timestamps = HashMap::new();
        timestamps.insert("root".to_string(), 1000);
        timestamps.insert("branch-a".to_string(), 3000);
        timestamps.insert("branch-b".to_string(), 2000);

        let branch = active_branch_with_timestamps(&records, &timestamps, &[]);
        assert_eq!(branch, vec!["root", "branch-a"]);
    }

    #[test]
    fn active_branch_terminates_on_parent_cycle() {
        let records = vec![
            ("n1".to_string(), Some("n2".to_string())),
            ("n2".to_string(), Some("n1".to_string())),
        ];
        let branch = active_branch(&records, &["n2".to_string()]);
        assert_eq!(branch, vec!["n1", "n2"]);
    }

    #[test]
    fn scan_fixture_emits_main_session_on_active_branch() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder");
        let dir = TempDir::new().unwrap();
        let ctx =
            ScanContext::with_roots(dir.path().to_path_buf(), vec![ScanRoot::local(root)], None);
        let convs = QoderConnector::new().scan(&ctx).unwrap();

        let main = convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some("11111111-1111-4111-8111-111111111111"))
            .expect("main fixture conversation");
        assert_eq!(main.agent_slug, "qoder");
        assert_eq!(main.metadata["source"], "qoder_jsonl");
        assert_eq!(main.metadata["is_subagent"], false);
        assert_eq!(main.metadata["model"], "qmodel-demo");
        assert_eq!(main.workspace, Some(PathBuf::from("/tmp/demo")));

        let contents: Vec<&str> = main.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents.len(), 4);
        assert!(contents[0].contains("Inspect the demo module"));
        assert!(contents[1].contains("Checking demo module."));
        assert_eq!(main.messages[2].role, "tool");
        assert!(contents[2].contains("Subagent finished checking demo"));
        assert!(contents[3].contains("All checks passed on kept branch."));
        assert!(
            !contents.iter().any(|c| c.contains("Abandoned")),
            "abandoned branch must not be emitted: {contents:?}"
        );
    }

    #[test]
    fn projects_root_resolved_prefers_qoder_home_then_home_then_relative() {
        let explicit = Path::new("/opt/qoder_home");
        let home = Path::new("/home/jane");
        assert_eq!(
            QoderConnector::projects_root_resolved(Some(explicit), Some(home)),
            PathBuf::from("/opt/qoder_home/projects")
        );
        assert_eq!(
            QoderConnector::projects_root_resolved(None, Some(home)),
            PathBuf::from("/home/jane/.qoder/projects")
        );
        assert_eq!(
            QoderConnector::projects_root_resolved(None, None),
            PathBuf::from(".qoder/projects")
        );
    }

    #[test]
    fn scan_without_local_default_stores_scans_nothing() {
        let dir = TempDir::new().unwrap();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None)
            .without_local_default_stores();
        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert!(convs.is_empty());
        let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
        assert!(discovered.is_empty());
    }
}

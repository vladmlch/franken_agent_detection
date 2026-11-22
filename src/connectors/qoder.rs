use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::claude_code::{ClaudeFlavor, parse_claude_records};
use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded,
};
use super::{Connector, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};

mod session;
#[cfg(test)]
mod tests;
mod title_db;

#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use session::{active_branch, active_branch_with_timestamps};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use title_db::{
    load_qoder_title_map_from_db, qoder_title_database_for_path_resolved,
    qoder_title_database_for_root_resolved,
};

use session::{
    SourceReadFailures, TaskSidecarIndex, extract_latest_custom_title,
    filter_active_branch_messages, read_qoder_session_sidecar, resolve_qoder_title,
    session_sidecar_path, source_modified_since, subagent_metadata_and_external_id,
};
use title_db::qoder_title_database_for_path;

/// Qoder CLI/IDE session ingestion (`~/.qoder/projects`, `~/.qoderwork/projects`,
/// `~/.qoder-cn/projects`, and `SharedClientCache/cli/projects`).
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

    #[cfg(test)]
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

    pub(crate) fn projects_roots() -> Vec<PathBuf> {
        Self::projects_roots_resolved(
            env_path_nonempty("QODER_PROJECTS_DIR").as_deref(),
            env_path_nonempty("QODER_HOME").as_deref(),
            dirs::home_dir().as_deref(),
            dirs::config_dir().as_deref(),
            env_path_nonempty("APPDATA").as_deref(),
            env_path_nonempty("XDG_CONFIG_HOME").as_deref(),
        )
    }

    pub(crate) fn projects_roots_resolved(
        qoder_projects_dir: Option<&Path>,
        qoder_home: Option<&Path>,
        home_dir: Option<&Path>,
        config_dir: Option<&Path>,
        appdata_dir: Option<&Path>,
        xdg_config_home: Option<&Path>,
    ) -> Vec<PathBuf> {
        if let Some(explicit_projects) = qoder_projects_dir {
            return vec![explicit_projects.to_path_buf()];
        }
        if let Some(explicit_home) = qoder_home {
            return vec![explicit_home.join("projects")];
        }

        let mut roots = Vec::new();
        let mut push_unique = |path: PathBuf| {
            if !roots.contains(&path) {
                roots.push(path);
            }
        };

        if let Some(home) = home_dir {
            push_unique(home.join(".qoder/projects"));
            push_unique(home.join(".qoderwork/projects"));
            push_unique(home.join(".qoder-cn/projects"));
            push_unique(
                home.join("Library/Application Support/Qoder/SharedClientCache/cli/projects"),
            );
            push_unique(home.join(".config/Qoder/SharedClientCache/cli/projects"));
            push_unique(home.join("AppData/Roaming/Qoder/SharedClientCache/cli/projects"));
        }
        if let Some(xdg) = xdg_config_home {
            push_unique(xdg.join("Qoder/SharedClientCache/cli/projects"));
        }
        if let Some(appdata) = appdata_dir {
            push_unique(appdata.join("Qoder/SharedClientCache/cli/projects"));
        }
        if let Some(cfg) = config_dir {
            push_unique(cfg.join("Qoder/SharedClientCache/cli/projects"));
        }

        if roots.is_empty() {
            roots.push(PathBuf::from(".qoder/projects"));
        }
        roots
    }

    fn is_valid_session_id(id: &str) -> bool {
        !id.is_empty()
            && !id.starts_with('.')
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }

    pub(super) fn jsonl_stem(path: &Path) -> Option<&str> {
        let name = path.file_name()?.to_str()?;
        if name.starts_with('.') {
            return None;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            return None;
        }
        path.file_stem()?.to_str()
    }

    fn is_main_session_file_name(path: &Path) -> bool {
        Self::jsonl_stem(path)
            .is_some_and(|stem| !stem.starts_with("agent-") && Self::is_valid_session_id(stem))
    }

    fn is_subagent_transcript_path(path: &Path) -> bool {
        let in_subagents_dir = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            == Some("subagents");
        let parent_ok = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .is_some_and(Self::is_valid_session_id);
        let name_ok = Self::jsonl_stem(path)
            .is_some_and(|stem| stem.starts_with("agent-") && Self::is_valid_session_id(stem));
        in_subagents_dir && parent_ok && name_ok
    }

    fn main_session_files(scan_target: &Path) -> Vec<PathBuf> {
        if scan_target.is_file() {
            if Self::is_main_session_file_name(scan_target)
                && !Self::is_subagent_transcript_path(scan_target)
            {
                return vec![scan_target.to_path_buf()];
            }
            return Vec::new();
        }

        let Ok(top_entries) = fs::read_dir(scan_target) else {
            return Vec::new();
        };

        let mut files = Vec::new();
        for entry in top_entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                if Self::is_main_session_file_name(&path) {
                    files.push(path);
                }
                continue;
            }
            let Ok(project_entries) = fs::read_dir(&path) else {
                continue;
            };
            for proj_entry in project_entries.flatten() {
                let child = proj_entry.path();
                if !child.is_dir() && Self::is_main_session_file_name(&child) {
                    files.push(child);
                }
            }
        }
        files.sort();
        files
    }

    fn subagent_session_files(scan_target: &Path) -> Vec<PathBuf> {
        if scan_target.is_file() {
            if Self::is_subagent_transcript_path(scan_target) {
                return vec![scan_target.to_path_buf()];
            }
            return Vec::new();
        }

        let Ok(top_entries) = fs::read_dir(scan_target) else {
            return Vec::new();
        };

        let mut files = Vec::new();
        for entry in top_entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }
            let Ok(project_entries) = fs::read_dir(&project_dir) else {
                continue;
            };
            for session_entry in project_entries.flatten() {
                let session_dir = session_entry.path();
                if !session_dir.is_dir() {
                    continue;
                }
                let Some(session_name) = session_dir.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !Self::is_valid_session_id(session_name) {
                    continue;
                }
                let subagents_dir = session_dir.join("subagents");
                let Ok(subagent_entries) = fs::read_dir(&subagents_dir) else {
                    continue;
                };
                for sub_entry in subagent_entries.flatten() {
                    let sub_path = sub_entry.path();
                    if !sub_path.is_dir() && Self::is_subagent_transcript_path(&sub_path) {
                        files.push(sub_path);
                    }
                }
            }
        }
        files.sort();
        files
    }

    fn all_session_files(scan_target: &Path) -> Vec<PathBuf> {
        let mut files = Self::main_session_files(scan_target);
        files.extend(Self::subagent_session_files(scan_target));
        files
    }

    fn resolve_mirrored_qoder_store(scan_root: &ScanRoot) -> Option<PathBuf> {
        scan_root
            .mirrored_store(&[".qoder", "projects"])
            .or_else(|| {
                let src = scan_root.mirrored_source_path.as_deref()?;
                if src.contains(".qoder-cn") {
                    scan_root.mirrored_store(&[".qoder-cn", "projects"])
                } else if src.contains(".qoderwork") {
                    scan_root.mirrored_store(&[".qoderwork", "projects"])
                } else {
                    None
                }
            })
    }

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let looks_like_root = |path: &Path| path.join("projects").is_dir();

        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                vec![ScanRoot::local(ctx.data_dir.join("projects"))]
            } else if ctx.allow_local_default_stores {
                Self::projects_roots()
                    .into_iter()
                    .map(ScanRoot::local)
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            ctx.scan_roots
                .iter()
                .map(|scan_root| {
                    Self::resolve_mirrored_qoder_store(scan_root).map_or_else(
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
            for path in Self::all_session_files(&root.path) {
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
                let is_subagent = Self::is_subagent_transcript_path(&path);
                let sidecar = (!is_subagent)
                    .then(|| session_sidecar_path(&path))
                    .flatten();
                if !source_modified_since(&path, sidecar.as_deref(), ctx.since_ts) {
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
                if let Some(sidecar_path) = sidecar
                    && sidecar_path.is_file()
                    && !path_is_excluded(&sidecar_path, &excluded_paths)
                {
                    out.push(
                        DiscoveredSourceFile::new(
                            "qoder",
                            &root,
                            sidecar_path,
                            DiscoveredSourceRole::MetadataSidecar,
                            false,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
        Ok(out)
    }
}

fn scan_qoder_with_callback(
    ctx: &ScanContext,
    excluded_paths: &[PathBuf],
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_qoder_with_title_resolver(
        ctx,
        excluded_paths,
        hooks,
        &qoder_title_database_for_path,
        on_conversation,
    )
}

#[allow(clippy::too_many_lines)]
fn scan_qoder_with_title_resolver(
    ctx: &ScanContext,
    excluded_paths: &[PathBuf],
    hooks: &mut SourceScanHooks<'_>,
    resolve_title_db: &dyn Fn(&Path) -> Option<PathBuf>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let roots = QoderConnector::source_roots(ctx);
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    let mut read_failures = SourceReadFailures::default();
    let mut task_cache: HashMap<PathBuf, TaskSidecarIndex> = HashMap::new();
    let mut title_db_cache: HashMap<PathBuf, HashMap<String, String>> = HashMap::new();

    for root in roots {
        if !root.path.exists() {
            continue;
        }
        for path in QoderConnector::all_session_files(&root.path) {
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
            let is_subagent = QoderConnector::is_subagent_transcript_path(&path);
            let sidecar = (!is_subagent)
                .then(|| session_sidecar_path(&path))
                .flatten();
            if !source_modified_since(&path, sidecar.as_deref(), ctx.since_ts) {
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
            let sidecar_discovered = sidecar
                .as_ref()
                .filter(|s| s.is_file() && !path_is_excluded(s, excluded_paths))
                .map(|sidecar_path| {
                    DiscoveredSourceFile::new(
                        "qoder",
                        &root,
                        sidecar_path.clone(),
                        DiscoveredSourceRole::MetadataSidecar,
                        false,
                    )
                    .with_fs_metadata()
                });
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
                match parse_claude_records(BufReader::new(&file), &path, ClaudeFlavor::Qoder) {
                    Ok(Some(parsed)) => parsed,
                    Ok(None) => continue,
                    Err(error) => {
                        let io_err = error.downcast::<io::Error>()?;
                        read_failures.record(&path, "read", io_err);
                        continue;
                    }
                };
            let custom_title = match extract_latest_custom_title(&file) {
                Ok(custom) => custom,
                Err(io_err) => {
                    read_failures.record(&path, "read", io_err);
                    continue;
                }
            };

            let Some(mut filtered) = filter_active_branch_messages(parsed) else {
                continue;
            };
            let transcript_title = custom_title.or_else(|| filtered.transcript_title.take());

            let (external_id, metadata, title) = if is_subagent {
                let (ext_id, meta) = subagent_metadata_and_external_id(
                    &path,
                    filtered.model.as_deref(),
                    &mut task_cache,
                );
                let title = resolve_qoder_title(
                    None,
                    None,
                    transcript_title.as_deref(),
                    &filtered.messages,
                    filtered.workspace.as_deref(),
                );
                (ext_id, meta, title)
            } else {
                let file_stem = QoderConnector::jsonl_stem(&path).map(str::to_string);
                let sidecar_meta = sidecar
                    .as_deref()
                    .filter(|s| s.is_file())
                    .map(read_qoder_session_sidecar)
                    .unwrap_or_default();
                if filtered.workspace.is_none() {
                    filtered.workspace = sidecar_meta.working_dir;
                }
                let db_title = if ctx.allow_local_default_stores && !root.origin.is_remote() {
                    file_stem.as_deref().and_then(|stem| {
                        let db_path = resolve_title_db(&path)?;
                        let map = title_db_cache
                            .entry(db_path.clone())
                            .or_insert_with(|| load_qoder_title_map_from_db(&db_path));
                        map.get(stem).cloned()
                    })
                } else {
                    None
                };
                let title = resolve_qoder_title(
                    db_title.as_deref(),
                    sidecar_meta.title.as_deref(),
                    transcript_title.as_deref(),
                    &filtered.messages,
                    filtered.workspace.as_deref(),
                );

                let mut metadata = serde_json::json!({
                    "source": "qoder_jsonl",
                    "is_subagent": false,
                    "model": filtered.model,
                });
                if let Some(obj) = metadata.as_object_mut() {
                    if let Some(fork_from) = sidecar_meta.fork_from {
                        obj.insert(
                            "fork_from".to_string(),
                            serde_json::Value::String(fork_from),
                        );
                    }
                    if let Some(parent_session_id) = sidecar_meta.parent_session_id {
                        obj.insert(
                            "parent_session_id".to_string(),
                            serde_json::Value::String(parent_session_id),
                        );
                    }
                }
                (file_stem, metadata, title)
            };

            let mut conv = NormalizedConversation {
                agent_slug: "qoder".into(),
                external_id,
                title,
                workspace: filtered.workspace,
                source_path: path.clone(),
                started_at: filtered.started_at,
                ended_at: filtered.ended_at,
                metadata,
                messages: filtered.messages,
            };
            root.apply_workspace_rewrite(&mut conv);
            on_conversation(conv)?;

            let changed = discovered.fs_metadata_changed()
                || sidecar_discovered
                    .as_ref()
                    .is_some_and(DiscoveredSourceFile::fs_metadata_changed);
            if changed {
                tracing::debug!(
                    path = %path.display(),
                    "qoder source changed during parse; completion withheld"
                );
            } else {
                hooks.complete(&SourceCompletion {
                    source: discovered,
                    required_sidecars: sidecar_discovered.into_iter().collect(),
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

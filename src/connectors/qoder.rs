use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, Seek};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::claude_code::{ClaudeFlavor, ParsedClaudeFile, parse_claude_records};
use super::jsonl::JsonlLines;
use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded, read_capped,
};
use super::{Connector, file_modified_since, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation, NormalizedMessage, reindex_messages};

const QODER_TITLE_DB_NAME: &str = "main.sqlite";
const QODER_TITLE_LAYOUTS: [(&str, &str); 2] = [
    (".qoder", "com.qoder.app.stable"),
    (".qoder-cn", "com.qodercn.app.stable"),
];

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

    fn jsonl_stem(path: &Path) -> Option<&str> {
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

fn session_sidecar_path(transcript_path: &Path) -> Option<PathBuf> {
    let stem = QoderConnector::jsonl_stem(transcript_path)?;
    Some(transcript_path.with_file_name(format!("{stem}-session.json")))
}

fn source_modified_since(path: &Path, sidecar: Option<&Path>, since_ts: Option<i64>) -> bool {
    file_modified_since(path, since_ts)
        || sidecar.is_some_and(|s| s.is_file() && file_modified_since(s, since_ts))
}

fn non_blank_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct QoderSessionSidecarRaw {
    title: Option<String>,
    parent_session_id: Option<String>,
    fork_from: Option<String>,
    working_dir: Option<String>,
}

#[derive(Default)]
struct QoderSessionSidecar {
    title: Option<String>,
    parent_session_id: Option<String>,
    fork_from: Option<String>,
    working_dir: Option<PathBuf>,
}

fn read_qoder_session_sidecar(sidecar_path: &Path) -> QoderSessionSidecar {
    let Ok(Some(contents)) = read_capped(sidecar_path) else {
        return QoderSessionSidecar::default();
    };
    let Ok(raw) = serde_json::from_str::<QoderSessionSidecarRaw>(&contents) else {
        return QoderSessionSidecar::default();
    };
    QoderSessionSidecar {
        title: non_blank_str(raw.title.as_deref()),
        parent_session_id: non_blank_str(raw.parent_session_id.as_deref()),
        fork_from: non_blank_str(raw.fork_from.as_deref()),
        working_dir: non_blank_str(raw.working_dir.as_deref()).map(PathBuf::from),
    }
}

fn qoder_title_database_candidates(
    home_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    let (Some(home), Some(config)) = (home_dir, config_dir) else {
        return Vec::new();
    };
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Vec::new();
    }
    let mut pairs = Vec::with_capacity(QODER_TITLE_LAYOUTS.len() * 2);
    for (dot_dir, app_id) in QODER_TITLE_LAYOUTS {
        let unresolved = home.join(dot_dir).join("projects");
        let store = config.join(app_id).join(QODER_TITLE_DB_NAME);
        if let Ok(resolved) = fs::canonicalize(&unresolved)
            && resolved != unresolved
        {
            pairs.push((resolved, store.clone()));
        }
        pairs.push((unresolved, store));
    }
    pairs
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn qoder_title_database_for_root_resolved(
    root: &Path,
    home_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    qoder_title_database_candidates(home_dir, config_dir)
        .into_iter()
        .find(|(alias, _)| alias == root)
        .map(|(_, store)| store)
}

pub(crate) fn qoder_title_database_for_path_resolved(
    path: &Path,
    home_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    qoder_title_database_candidates(home_dir, config_dir)
        .into_iter()
        .filter(|(alias, _)| path.starts_with(alias) && path != alias)
        .max_by_key(|(alias, _)| alias.as_os_str().len())
        .map(|(_, store)| store)
}

#[cfg_attr(
    not(any(
        feature = "cursor",
        feature = "opencode",
        feature = "devin",
        feature = "qoder-sqlite"
    )),
    allow(dead_code)
)]
fn qoder_title_database_for_path(path: &Path) -> Option<PathBuf> {
    qoder_title_database_for_path_resolved(
        path,
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
    )
}

#[cfg(any(
    feature = "cursor",
    feature = "opencode",
    feature = "devin",
    feature = "qoder-sqlite"
))]
pub(crate) fn load_qoder_title_map_from_db(db_path: &Path) -> HashMap<String, String> {
    use super::sqlite_sync::{
        ConnectionExt, open_read_only_with_snapshot_fallback, store_is_provably_empty,
    };
    use frankensqlite::compat::RowExt;

    if !fs::symlink_metadata(db_path).is_ok_and(|meta| meta.file_type().is_file())
        || store_is_provably_empty(db_path)
    {
        return HashMap::new();
    }

    let Ok(conn) = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref()) else {
        tracing::debug!(
            db = %db_path.display(),
            "qoder could not open Electron main.sqlite title store"
        );
        return HashMap::new();
    };
    for pragma in [
        "PRAGMA busy_timeout = 2000;",
        "PRAGMA query_only = ON;",
        "PRAGMA trusted_schema = OFF;",
    ] {
        if let Err(err) = conn.execute(pragma) {
            tracing::debug!(db = %db_path.display(), %err, "qoder title store pragma failed");
        }
    }

    let is_table = conn
        .query_row_map(
            "SELECT type FROM sqlite_master WHERE name = 'chat_sessions'",
            &[],
            |row| row.get_typed::<Option<String>>(0),
        )
        .ok()
        .flatten()
        .is_some_and(|kind| kind == "table");
    if !is_table {
        return HashMap::new();
    }

    let Ok(rows) =
        conn.query_map_collect("SELECT session_id, title FROM chat_sessions", &[], |row| {
            Ok((
                row.get_typed::<Option<String>>(0)?,
                row.get_typed::<Option<String>>(1)?,
            ))
        })
    else {
        tracing::debug!(
            db = %db_path.display(),
            "qoder could not query chat_sessions from main.sqlite"
        );
        return HashMap::new();
    };

    let mut titles = HashMap::new();
    for (session_id, title) in rows {
        let Some(session_id) = non_blank_str(session_id.as_deref()) else {
            continue;
        };
        let Some(title) = non_blank_str(title.as_deref()) else {
            continue;
        };
        titles.insert(session_id, title);
    }
    titles
}

#[cfg(not(any(
    feature = "cursor",
    feature = "opencode",
    feature = "devin",
    feature = "qoder-sqlite"
)))]
pub(crate) fn load_qoder_title_map_from_db(_db_path: &Path) -> HashMap<String, String> {
    HashMap::new()
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

    Some(FilteredQoderSession {
        messages,
        transcript_title: json_title,
        workspace,
        model,
        started_at,
        ended_at,
    })
}

fn extract_latest_custom_title(mut file: &fs::File) -> io::Result<Option<String>> {
    file.rewind()?;
    let mut latest: Option<String> = None;
    for line_res in JsonlLines::new(BufReader::new(file)) {
        let line = line_res?;
        if !line.contains("custom-title") {
            continue;
        }
        let trimmed = line.trim_start_matches('\u{feff}');
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if val.get("type").and_then(serde_json::Value::as_str) != Some("custom-title") {
            continue;
        }
        let candidate = ["customTitle", "title"].into_iter().find_map(|key| {
            val.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
        });
        if let Some(title) = candidate {
            latest = (!title.is_empty()).then(|| title.to_string());
        }
    }
    Ok(latest)
}

fn resolve_qoder_title(
    db_title: Option<&str>,
    sidecar_title: Option<&str>,
    transcript_title: Option<&str>,
    messages: &[NormalizedMessage],
    workspace: Option<&Path>,
) -> Option<String> {
    non_blank_str(db_title)
        .or_else(|| non_blank_str(sidecar_title))
        .or_else(|| non_blank_str(transcript_title))
        .or_else(|| {
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
                        .and_then(Path::file_name)
                        .and_then(|n| n.to_str())
                        .map(String::from)
                })
        })
}

struct FilteredQoderSession {
    messages: Vec<NormalizedMessage>,
    transcript_title: Option<String>,
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

#[derive(Clone, Default)]
struct TaskSidecar {
    agent_id: Option<String>,
    description: Option<String>,
    resolved_model: Option<String>,
}

#[derive(Default)]
struct TaskSidecarIndex {
    by_parent_tool_use_id: HashMap<String, TaskSidecar>,
    by_agent_id: HashMap<String, TaskSidecar>,
}

fn parse_json_file_value(path: &Path) -> Option<serde_json::Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn non_empty_json_field(val: &serde_json::Value, key: &str) -> Option<String> {
    val.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn load_task_sidecars(subagents_dir: &Path) -> TaskSidecarIndex {
    let mut index = TaskSidecarIndex::default();
    let Ok(entries) = fs::read_dir(subagents_dir) else {
        return index;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name_ok = path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with("task-"));
            let ext_ok = path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            path.is_file() && name_ok && ext_ok
        })
        .collect();
    paths.sort();

    for path in paths {
        let Some(val) = parse_json_file_value(&path) else {
            continue;
        };
        let sidecar = TaskSidecar {
            agent_id: non_empty_json_field(&val, "agentId"),
            description: non_empty_json_field(&val, "description"),
            resolved_model: non_empty_json_field(&val, "resolvedModel"),
        };
        if let Some(parent_tool_use_id) = non_empty_json_field(&val, "parentToolUseId") {
            index
                .by_parent_tool_use_id
                .entry(parent_tool_use_id)
                .or_insert_with(|| sidecar.clone());
        }
        if let Some(agent_id) = &sidecar.agent_id {
            index.by_agent_id.entry(agent_id.clone()).or_insert(sidecar);
        }
    }
    index
}

fn subagent_metadata_and_external_id(
    path: &Path,
    model: Option<&str>,
    task_cache: &mut HashMap<PathBuf, TaskSidecarIndex>,
) -> (Option<String>, serde_json::Value) {
    let subagents_dir = path.parent();
    let parent_id = subagents_dir
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let stem = path.file_stem().and_then(|s| s.to_str());
    let agent_id_from_stem = stem
        .and_then(|s| s.strip_prefix("agent-"))
        .filter(|s| !s.is_empty());

    let external_id = match (parent_id.as_deref(), stem) {
        (Some(pid), Some(st)) => Some(format!("{pid}/{st}")),
        (None, Some(st)) => Some(st.to_string()),
        _ => None,
    };

    let meta_val = match (subagents_dir, stem) {
        (Some(dir), Some(st)) => parse_json_file_value(&dir.join(format!("{st}.meta.json"))),
        _ => None,
    };
    let meta_agent_type = meta_val
        .as_ref()
        .and_then(|v| non_empty_json_field(v, "agentType"));
    let meta_description = meta_val
        .as_ref()
        .and_then(|v| non_empty_json_field(v, "description"));
    let meta_tool_use_id = meta_val
        .as_ref()
        .and_then(|v| non_empty_json_field(v, "toolUseId"));

    let task = subagents_dir.and_then(|dir| {
        let index = task_cache
            .entry(dir.to_path_buf())
            .or_insert_with(|| load_task_sidecars(dir));
        meta_tool_use_id
            .as_deref()
            .and_then(|id| index.by_parent_tool_use_id.get(id))
            .or_else(|| agent_id_from_stem.and_then(|id| index.by_agent_id.get(id)))
    });

    let effective_model = model
        .map(str::to_string)
        .or_else(|| task.and_then(|t| t.resolved_model.clone()));
    let mut map = serde_json::Map::new();
    map.insert(
        "source".to_string(),
        serde_json::Value::String("qoder_jsonl".to_string()),
    );
    map.insert("is_subagent".to_string(), serde_json::Value::Bool(true));
    map.insert(
        "model".to_string(),
        effective_model.map_or(serde_json::Value::Null, serde_json::Value::String),
    );
    if let Some(pid) = parent_id {
        map.insert("parent_id".to_string(), serde_json::Value::String(pid));
    }
    if let Some(agent_type) = meta_agent_type {
        map.insert(
            "agent_type".to_string(),
            serde_json::Value::String(agent_type),
        );
    }
    if let Some(description) = meta_description.or_else(|| task.and_then(|t| t.description.clone()))
    {
        map.insert(
            "description".to_string(),
            serde_json::Value::String(description),
        );
    }
    if let Some(tool_use_id) = meta_tool_use_id {
        map.insert(
            "tool_use_id".to_string(),
            serde_json::Value::String(tool_use_id),
        );
    }
    if let Some(agent_id) = task.and_then(|t| t.agent_id.clone()) {
        map.insert("agent_id".to_string(), serde_json::Value::String(agent_id));
    }

    (external_id, serde_json::Value::Object(map))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
    fn projects_roots_resolved_covers_all_four_families_and_qoder_projects_dir() {
        let explicit_projects = Path::new("/custom/qoder/projects");
        let explicit_home = Path::new("/opt/qoder_home");
        let home = Path::new("/home/jane");
        let appdata = Path::new("/win/Users/jane/AppData/Roaming");
        let xdg = Path::new("/home/jane/.xdg-config");

        assert_eq!(
            QoderConnector::projects_roots_resolved(
                Some(explicit_projects),
                Some(explicit_home),
                Some(home),
                None,
                Some(appdata),
                Some(xdg),
            ),
            vec![PathBuf::from("/custom/qoder/projects")]
        );

        assert_eq!(
            QoderConnector::projects_roots_resolved(
                None,
                Some(explicit_home),
                Some(home),
                None,
                Some(appdata),
                Some(xdg),
            ),
            vec![PathBuf::from("/opt/qoder_home/projects")]
        );

        let roots = QoderConnector::projects_roots_resolved(
            None,
            None,
            Some(home),
            None,
            Some(appdata),
            Some(xdg),
        );
        assert!(roots.contains(&home.join(".qoder/projects")));
        assert!(roots.contains(&home.join(".qoderwork/projects")));
        assert!(roots.contains(&home.join(".qoder-cn/projects")));
        assert!(roots.contains(
            &home.join("Library/Application Support/Qoder/SharedClientCache/cli/projects")
        ));
        assert!(roots.contains(&home.join(".config/Qoder/SharedClientCache/cli/projects")));
        assert!(roots.contains(&home.join("AppData/Roaming/Qoder/SharedClientCache/cli/projects")));
        assert!(roots.contains(&appdata.join("Qoder/SharedClientCache/cli/projects")));
        assert!(roots.contains(&xdg.join("Qoder/SharedClientCache/cli/projects")));
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

    #[test]
    fn scan_fixture_emits_subagent_with_sidecar_metadata() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder");
        let dir = TempDir::new().unwrap();
        let ctx =
            ScanContext::with_roots(dir.path().to_path_buf(), vec![ScanRoot::local(root)], None);
        let convs = QoderConnector::new().scan(&ctx).unwrap();

        let subagent = convs
            .iter()
            .find(|c| {
                c.external_id.as_deref()
                    == Some("11111111-1111-4111-8111-111111111111/agent-ageneral-purpose-0001")
            })
            .expect("subagent fixture conversation");
        assert_eq!(subagent.agent_slug, "qoder");
        assert_eq!(subagent.metadata["source"], "qoder_jsonl");
        assert_eq!(subagent.metadata["is_subagent"], true);
        assert_eq!(
            subagent.metadata["parent_id"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(subagent.metadata["agent_type"], "general-purpose");
        assert_eq!(subagent.metadata["description"], "Explore demo");
        assert_eq!(subagent.metadata["tool_use_id"], "call-subagent-1");
        assert_eq!(subagent.metadata["agent_id"], "ageneral-purpose-0001");
        assert_eq!(subagent.metadata["model"], "qmodel-demo");

        let contents: Vec<&str> = subagent
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents.len(), 2);
        assert!(contents[0].contains("Inspect demo module files"));
        assert!(contents[1].contains("Found 2 files in demo."));
        assert!(
            !contents.iter().any(|c| c.contains("Stale subagent branch")),
            "stale subagent branch must not be emitted: {contents:?}"
        );
    }

    #[test]
    fn subagent_with_corrupt_meta_still_emits_without_agent_type() {
        let dir = TempDir::new().unwrap();
        let subagents_dir = dir
            .path()
            .join("projects/-tmp-corrupt/22222222-2222-4222-8222-222222222222/subagents");
        fs::create_dir_all(&subagents_dir).unwrap();
        fs::write(
            subagents_dir.join("agent-acustom-0002.jsonl"),
            r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/tmp/corrupt","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Hello subagent"}}"#,
        )
        .unwrap();
        fs::write(
            subagents_dir.join("agent-acustom-0002.meta.json"),
            "{not valid json",
        )
        .unwrap();

        let ctx = ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![ScanRoot::local(dir.path().join("projects"))],
            None,
        );
        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        let subagent = &convs[0];
        assert_eq!(
            subagent.external_id.as_deref(),
            Some("22222222-2222-4222-8222-222222222222/agent-acustom-0002")
        );
        assert_eq!(subagent.metadata["is_subagent"], true);
        assert_eq!(
            subagent.metadata["parent_id"],
            "22222222-2222-4222-8222-222222222222"
        );
        assert!(subagent.metadata.get("agent_type").is_none());
    }

    #[test]
    fn subagent_with_missing_parent_session_still_emits() {
        let dir = TempDir::new().unwrap();
        let subagents_dir = dir
            .path()
            .join("projects/-tmp-orphan/33333333-3333-4333-8333-333333333333/subagents");
        fs::create_dir_all(&subagents_dir).unwrap();
        fs::write(
            subagents_dir.join("agent-aorphan-0003.jsonl"),
            r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/tmp/orphan","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Orphan subagent"}}"#,
        )
        .unwrap();

        let ctx = ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![ScanRoot::local(dir.path().join("projects"))],
            None,
        );
        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].metadata["is_subagent"], true);
        assert_eq!(
            convs[0].metadata["parent_id"],
            "33333333-3333-4333-8333-333333333333"
        );
    }

    #[test]
    fn main_session_files_never_returns_subagent_files_while_discover_includes_both() {
        let projects_root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder/projects");
        let main_files = QoderConnector::main_session_files(&projects_root);
        assert_eq!(main_files.len(), 1);
        assert!(
            main_files
                .iter()
                .all(|p| !QoderConnector::is_subagent_transcript_path(p)),
            "main_session_files must exclude subagents: {main_files:?}"
        );

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/qoder");
        let dir = TempDir::new().unwrap();
        let ctx =
            ScanContext::with_roots(dir.path().to_path_buf(), vec![ScanRoot::local(root)], None);
        let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
        assert_eq!(discovered.len(), 2);
        assert!(
            discovered
                .iter()
                .any(|d| QoderConnector::is_subagent_transcript_path(&d.source_path)),
            "discover_source_files should include subagent transcript"
        );
    }

    #[test]
    fn project_named_subagents_is_discovered_and_scanned_as_main_session() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects");
        let main_in_subagents_proj = projects
            .join("subagents")
            .join("11111111-1111-4111-8111-111111111111.jsonl");
        let stray_agent_in_proj = projects.join("-tmp-demo").join("agent-123.jsonl");
        let stray_nested = projects.join("-tmp-demo").join("notes").join("stray.jsonl");
        fs::create_dir_all(main_in_subagents_proj.parent().unwrap()).unwrap();
        fs::create_dir_all(stray_nested.parent().unwrap()).unwrap();
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/work/subagents","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Main session in subagents project"}}"#;
        fs::write(&main_in_subagents_proj, line).unwrap();
        fs::write(&stray_agent_in_proj, line).unwrap();
        fs::write(&stray_nested, line).unwrap();

        assert!(!QoderConnector::is_subagent_transcript_path(
            &main_in_subagents_proj
        ));
        let main_files = QoderConnector::main_session_files(&projects);
        assert_eq!(main_files, vec![main_in_subagents_proj.clone()]);

        let ctx = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(projects)],
            None,
        )
        .without_local_default_stores();
        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].external_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(convs[0].metadata["is_subagent"], false);
        assert_eq!(convs[0].source_path, main_in_subagents_proj);
    }

    #[test]
    fn flat_shared_client_cache_layout_with_dotted_stem_is_discovered_and_scanned() {
        let dir = TempDir::new().unwrap();
        let cli_projects = dir.path().join("Qoder/SharedClientCache/cli/projects");
        fs::create_dir_all(&cli_projects).unwrap();
        let stem = "task-10f849241e06451d9c9c.session.execution";
        let flat_path = cli_projects.join(format!("{stem}.jsonl"));
        let flat_agent = cli_projects.join("agent-stray.jsonl");
        fs::write(
            &flat_path,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/work/cli","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"SharedClientCache task"}}"#,
        )
        .unwrap();
        fs::write(
            &flat_agent,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Should skip"}}"#,
        )
        .unwrap();

        let ctx = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(cli_projects)],
            None,
        )
        .without_local_default_stores();
        let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].source_path, flat_path);

        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].external_id.as_deref(), Some(stem));
        assert_eq!(convs[0].metadata["is_subagent"], false);
        assert_eq!(convs[0].workspace, Some(PathBuf::from("/work/cli")));
    }

    #[test]
    fn companion_session_json_sidecar_backfills_workspace_metadata_and_boundaries() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects");
        let proj_dir = projects.join("-tmp-proj");
        fs::create_dir_all(&proj_dir).unwrap();
        let stem = "22222222-2222-4222-8222-222222222222";
        let transcript = proj_dir.join(format!("{stem}.jsonl"));
        let sidecar = proj_dir.join(format!("{stem}-session.json"));

        // Transcript has NO cwd and includes an ai-title that sidecar title must outrank.
        fs::write(
            &transcript,
            concat!(
                r#"{"type":"ai-title","aiTitle":"Generated title"}"#,
                "\n",
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"forked prompt"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            &sidecar,
            r#"{"title":"Forked work","parent_session_id":"00000000-0000-4000-8000-000000000000","fork_from":"11111111-1111-4111-8111-111111111111","working_dir":"/tmp/qoder-sidecar"}"#,
        )
        .unwrap();

        // Set transcript mtime old, sidecar mtime recent -> since_ts filtering must include it!
        let old_time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let new_time = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_modified(new_time)
            .unwrap();

        let since_ts_ms = i64::try_from(new_time.duration_since(UNIX_EPOCH).unwrap().as_millis())
            .unwrap()
            - 5_000;

        let ctx = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(projects)],
            Some(since_ts_ms),
        )
        .without_local_default_stores();

        let discovered = QoderConnector::new().discover_source_files(&ctx).unwrap();
        assert_eq!(discovered.len(), 2);
        assert!(
            discovered.iter().any(|d| d.source_path == transcript
                && d.role == DiscoveredSourceRole::PrimarySessionLog)
        );
        assert!(
            discovered.iter().any(
                |d| d.source_path == sidecar && d.role == DiscoveredSourceRole::MetadataSidecar
            )
        );

        let mut completions = Vec::new();
        let mut convs = Vec::new();
        {
            let mut on_complete = |c: &SourceCompletion| {
                completions.push(c.clone());
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut on_complete),
            };
            QoderConnector::new()
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conv| {
                    convs.push(conv);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.title.as_deref(), Some("Forked work"));
        assert_eq!(conv.workspace, Some(PathBuf::from("/tmp/qoder-sidecar")));
        assert_eq!(
            conv.metadata["fork_from"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(
            conv.metadata["parent_session_id"],
            "00000000-0000-4000-8000-000000000000"
        );

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].source.source_path, transcript);
        assert_eq!(completions[0].required_sidecars.len(), 1);
        assert_eq!(completions[0].required_sidecars[0].source_path, sidecar);

        // If neither transcript nor sidecar was modified after since_ts, nothing is scanned.
        let future_since_ms = i64::try_from(
            (SystemTime::now() + Duration::from_secs(3600))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let ctx_future = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(dir.path().join("projects"))],
            Some(future_since_ms),
        )
        .without_local_default_stores();
        assert!(
            QoderConnector::new()
                .discover_source_files(&ctx_future)
                .unwrap()
                .is_empty()
        );
        assert!(QoderConnector::new().scan(&ctx_future).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn companion_session_json_rejects_symlink_and_falls_back_gracefully() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects/proj");
        fs::create_dir_all(&projects).unwrap();
        let outside = dir.path().join("secret-sidecar.json");
        fs::write(&outside, r#"{"title":"Leaked","working_dir":"/secret"}"#).unwrap();

        let stem = "33333333-3333-4333-8333-333333333333";
        let transcript = projects.join(format!("{stem}.jsonl"));
        let sidecar_link = projects.join(format!("{stem}-session.json"));
        fs::write(
            &transcript,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Prompt fallback"}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, &sidecar_link).unwrap();

        let ctx = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(dir.path().join("projects"))],
            None,
        )
        .without_local_default_stores();
        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title.as_deref(), Some("Prompt fallback"));
        assert!(convs[0].workspace.is_none());
    }

    #[test]
    fn companion_session_json_does_not_overwrite_transcript_cwd_or_emit_blank_links() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects/proj");
        fs::create_dir_all(&projects).unwrap();
        let stem = "44444444-4444-4444-8444-444444444444";
        let transcript = projects.join(format!("{stem}.jsonl"));
        let sidecar = projects.join(format!("{stem}-session.json"));
        fs::write(
            &transcript,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/transcript/cwd","timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"Keep transcript cwd"}}"#,
        )
        .unwrap();
        fs::write(
            &sidecar,
            r#"{"title":"   ","parent_session_id":"  ","fork_from":"","working_dir":"/sidecar/cwd"}"#,
        )
        .unwrap();

        let ctx = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(dir.path().join("projects"))],
            None,
        )
        .without_local_default_stores();
        let convs = QoderConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].workspace, Some(PathBuf::from("/transcript/cwd")));
        assert_eq!(convs[0].title.as_deref(), Some("Keep transcript cwd"));
        assert!(convs[0].metadata.get("fork_from").is_none());
        assert!(convs[0].metadata.get("parent_session_id").is_none());
    }

    #[test]
    fn title_database_resolution_strictly_binds_home_default_roots_only() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join("home");
        let app_data = dir.path().join("appdata");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&app_data).unwrap();

        let intl_root = home.join(".qoder/projects");
        let cn_root = home.join(".qoder-cn/projects");
        assert_eq!(
            qoder_title_database_for_root_resolved(&intl_root, Some(&home), Some(&app_data)),
            Some(app_data.join("com.qoder.app.stable/main.sqlite"))
        );
        assert_eq!(
            qoder_title_database_for_root_resolved(&cn_root, Some(&home), Some(&app_data)),
            Some(app_data.join("com.qodercn.app.stable/main.sqlite"))
        );

        for non_bound in [
            home.join(".qoderwork/projects"),
            home.join("sessions/projects"),
            home.join("Library/Application Support/Qoder/SharedClientCache/cli/projects"),
            home.join(".qoder"),
            home.join("mirror-0001/.qoder/projects"),
            PathBuf::from(".qoder/projects"),
        ] {
            assert_eq!(
                qoder_title_database_for_root_resolved(&non_bound, Some(&home), Some(&app_data)),
                None,
                "only home-anchored .qoder and .qoder-cn may bind main.sqlite: {}",
                non_bound.display()
            );
        }

        // Transcript path resolution including symlinked default root.
        #[cfg(unix)]
        {
            let target = dir.path().join("moved-cn-projects");
            fs::create_dir_all(&target).unwrap();
            fs::create_dir_all(cn_root.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &cn_root).unwrap();
            let resolved_cn = fs::canonicalize(&cn_root).unwrap();
            assert_eq!(
                qoder_title_database_for_path_resolved(
                    &resolved_cn.join("proj/aaaa.jsonl"),
                    Some(&home),
                    Some(&app_data),
                ),
                Some(app_data.join("com.qodercn.app.stable/main.sqlite"))
            );
        }

        assert_eq!(
            qoder_title_database_for_path_resolved(
                &intl_root.join("-Users-u-demo/aaaa.jsonl"),
                Some(&home),
                Some(&app_data),
            ),
            Some(app_data.join("com.qoder.app.stable/main.sqlite"))
        );
        assert_eq!(
            qoder_title_database_for_path_resolved(
                &home.join(".qoderwork/projects/aaaa.jsonl"),
                Some(&home),
                Some(&app_data),
            ),
            None
        );
    }

    #[test]
    fn title_precedence_enforces_db_then_sidecar_then_transcript_then_prompt() {
        assert_eq!(
            resolve_qoder_title(
                Some("From database"),
                Some("From sidecar"),
                Some("Renamed in chat"),
                &[],
                None,
            )
            .as_deref(),
            Some("From database")
        );
        assert_eq!(
            resolve_qoder_title(
                Some("   "),
                Some("From sidecar"),
                Some("Renamed in chat"),
                &[],
                None,
            )
            .as_deref(),
            Some("From sidecar")
        );
        assert_eq!(
            resolve_qoder_title(
                None,
                Some("\u{0000}\u{0001}\u{0080}"),
                Some("Renamed in chat"),
                &[],
                None,
            )
            .as_deref(),
            Some("\u{0000}\u{0001}\u{0080}")
        );
        assert_eq!(
            resolve_qoder_title(None, Some("   "), Some("Renamed in chat"), &[], None).as_deref(),
            Some("Renamed in chat")
        );
    }

    #[cfg(any(
        feature = "cursor",
        feature = "opencode",
        feature = "devin",
        feature = "qoder-sqlite"
    ))]
    #[test]
    fn main_sqlite_title_store_precedence_and_hermeticity() {
        use crate::connectors::sqlite_sync::Connection;

        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("com.qoder.app.stable/main.sqlite");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let sid_db_wins = "11111111-1111-4111-8111-111111111111";
        let sid_blank_db = "22222222-2222-4222-8222-222222222222";
        let sid_missing_db = "33333333-3333-4333-8333-333333333333";
        let sid_ai_title = "44444444-4444-4444-8444-444444444444";
        let sid_prompt = "55555555-5555-4555-8555-555555555555";

        {
            let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
            conn.execute_batch(
                "CREATE TABLE chat_sessions (session_id TEXT PRIMARY KEY, title TEXT);",
            )
            .unwrap();
            conn.execute(&format!(
                "INSERT INTO chat_sessions (session_id, title) VALUES \
                 ('{sid_db_wins}', 'From database'), \
                 ('{sid_blank_db}', '   '), \
                 ('null-title', NULL);"
            ))
            .unwrap();
        }

        let loaded = load_qoder_title_map_from_db(&db_path);
        assert_eq!(
            loaded.get(sid_db_wins).map(String::as_str),
            Some("From database")
        );
        assert!(!loaded.contains_key(sid_blank_db));
        assert!(!loaded.contains_key("null-title"));

        // Corrupt DB and DB missing chat_sessions table return empty map gracefully.
        let corrupt_db = dir.path().join("corrupt.sqlite");
        fs::write(&corrupt_db, "not sqlite").unwrap();
        assert!(load_qoder_title_map_from_db(&corrupt_db).is_empty());

        let empty_schema_db = dir.path().join("other-table.sqlite");
        {
            let conn = Connection::open(empty_schema_db.to_string_lossy().as_ref()).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (id TEXT);")
                .unwrap();
        }
        assert!(load_qoder_title_map_from_db(&empty_schema_db).is_empty());

        #[cfg(unix)]
        {
            let linked_db = dir.path().join("linked-main.sqlite");
            std::os::unix::fs::symlink(&db_path, &linked_db).unwrap();
            assert!(load_qoder_title_map_from_db(&linked_db).is_empty());
        }

        // End-to-end scan through title resolver enforcing exact precedence:
        // main.sqlite > -session.json > custom-title > ai-title > first user prompt.
        let projects = dir.path().join("projects/proj");
        fs::create_dir_all(&projects).unwrap();

        fs::write(
            projects.join(format!("{sid_db_wins}.jsonl")),
            concat!(
                r#"{"type":"custom-title","customTitle":"Renamed in chat"}"#,
                "\n",
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 1"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            projects.join(format!("{sid_db_wins}-session.json")),
            r#"{"title":"From sidecar"}"#,
        )
        .unwrap();

        fs::write(
            projects.join(format!("{sid_blank_db}.jsonl")),
            concat!(
                r#"{"type":"custom-title","customTitle":"Renamed in chat"}"#,
                "\n",
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 2"}}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(
            projects.join(format!("{sid_blank_db}-session.json")),
            r#"{"title":"From sidecar"}"#,
        )
        .unwrap();

        fs::write(
            projects.join(format!("{sid_missing_db}.jsonl")),
            concat!(
                r#"{"type":"ai-title","aiTitle":"Generated AI title"}"#,
                "\n",
                r#"{"type":"custom-title","customTitle":"Renamed in chat"}"#,
                "\n",
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 3"}}"#,
                "\n",
            ),
        )
        .unwrap();

        fs::write(
            projects.join(format!("{sid_ai_title}.jsonl")),
            concat!(
                r#"{"type":"ai-title","aiTitle":"Generated AI title"}"#,
                "\n",
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 4"}}"#,
                "\n",
            ),
        )
        .unwrap();

        fs::write(
            projects.join(format!("{sid_prompt}.jsonl")),
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:00Z","message":{"role":"user","content":"First prompt 5"}}"#,
        )
        .unwrap();

        // With allow_local_default_stores = true and explicit resolver pointing at our temp main.sqlite:
        let ctx = ScanContext::with_roots(
            dir.path().join("state"),
            vec![ScanRoot::local(dir.path().join("projects"))],
            None,
        );
        let mut convs = Vec::new();
        let db_for_closure = db_path.clone();
        scan_qoder_with_title_resolver(
            &ctx,
            &[],
            &mut SourceScanHooks::default(),
            &move |_| Some(db_for_closure.clone()),
            &mut |c| {
                convs.push(c);
                Ok(())
            },
        )
        .unwrap();

        let title_for = |id: &str| {
            convs
                .iter()
                .find(|c| c.external_id.as_deref() == Some(id))
                .and_then(|c| c.title.as_deref())
                .map(str::to_string)
        };
        assert_eq!(title_for(sid_db_wins).as_deref(), Some("From database"));
        assert_eq!(title_for(sid_blank_db).as_deref(), Some("From sidecar"));
        assert_eq!(
            title_for(sid_missing_db).as_deref(),
            Some("Renamed in chat")
        );
        assert_eq!(
            title_for(sid_ai_title).as_deref(),
            Some("Generated AI title")
        );
        assert_eq!(title_for(sid_prompt).as_deref(), Some("First prompt 5"));

        // With allow_local_default_stores = false (hermetic): DB MUST NOT be consulted!
        let hermetic_ctx = ctx.without_local_default_stores();
        let mut hermetic_convs = Vec::new();
        let db_for_hermetic = db_path;
        scan_qoder_with_title_resolver(
            &hermetic_ctx,
            &[],
            &mut SourceScanHooks::default(),
            &move |_| Some(db_for_hermetic.clone()),
            &mut |c| {
                hermetic_convs.push(c);
                Ok(())
            },
        )
        .unwrap();
        let hermetic_db_wins = hermetic_convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some(sid_db_wins))
            .unwrap();
        assert_eq!(hermetic_db_wins.title.as_deref(), Some("From sidecar"));
    }
}

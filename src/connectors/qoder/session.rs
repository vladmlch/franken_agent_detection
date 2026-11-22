use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, Seek};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::QoderConnector;
use crate::connectors::claude_code::ParsedClaudeFile;
use crate::connectors::file_modified_since;
use crate::connectors::jsonl::JsonlLines;
use crate::connectors::utils::read_capped;
use crate::types::{NormalizedMessage, reindex_messages};

pub(super) fn session_sidecar_path(transcript_path: &Path) -> Option<PathBuf> {
    let stem = QoderConnector::jsonl_stem(transcript_path)?;
    Some(transcript_path.with_file_name(format!("{stem}-session.json")))
}

pub(super) fn source_modified_since(
    path: &Path,
    sidecar: Option<&Path>,
    since_ts: Option<i64>,
) -> bool {
    file_modified_since(path, since_ts)
        || sidecar.is_some_and(|s| s.is_file() && file_modified_since(s, since_ts))
}

pub(super) fn non_blank_str(value: Option<&str>) -> Option<String> {
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
pub(super) struct QoderSessionSidecar {
    pub(super) title: Option<String>,
    pub(super) parent_session_id: Option<String>,
    pub(super) fork_from: Option<String>,
    pub(super) working_dir: Option<PathBuf>,
}

pub(super) fn read_qoder_session_sidecar(sidecar_path: &Path) -> QoderSessionSidecar {
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

/// Resolve the active branch from `active-leaf` records, latest timestamp, or
/// last record in file order, walking `parentUuid` back to the root with cycle
/// protection.
#[cfg_attr(not(test), allow(dead_code))]
pub fn active_branch(records: &[(String, Option<String>)], leaves: &[String]) -> Vec<String> {
    active_branch_with_timestamps(records, &HashMap::new(), leaves)
}

pub fn active_branch_with_timestamps(
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

pub(super) fn filter_active_branch_messages(
    parsed: ParsedClaudeFile,
) -> Option<FilteredQoderSession> {
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

pub(super) fn extract_latest_custom_title(mut file: &fs::File) -> io::Result<Option<String>> {
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

pub(super) fn resolve_qoder_title(
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

pub(super) struct FilteredQoderSession {
    pub(super) messages: Vec<NormalizedMessage>,
    pub(super) transcript_title: Option<String>,
    pub(super) workspace: Option<PathBuf>,
    pub(super) model: Option<String>,
    pub(super) started_at: Option<i64>,
    pub(super) ended_at: Option<i64>,
}

#[derive(Default)]
pub(super) struct SourceReadFailures {
    first: Option<anyhow::Error>,
    count: usize,
}

impl SourceReadFailures {
    pub(super) fn record(&mut self, path: &Path, operation: &str, error: io::Error) {
        tracing::warn!(path = %path.display(), %error, operation, "qoder skipping unreadable source");
        self.count += 1;
        if self.first.is_none() {
            self.first =
                Some(anyhow::Error::new(error).context(format!("{operation} {}", path.display())));
        }
    }

    pub(super) fn finish(self) -> Result<()> {
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
pub(super) struct TaskSidecarIndex {
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

pub(super) fn subagent_metadata_and_external_id(
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

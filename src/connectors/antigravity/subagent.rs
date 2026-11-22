//! Subagent descriptor indexing and prompt-reminder verdict resolution.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use super::AntigravityConnector;
use crate::connectors::utils::path_is_excluded;

/// The proven verdict fields: (`is_subagent`, `parent_id`, `role`, `type_name`).
pub(super) type SubagentVerdict = (bool, Option<String>, Option<String>, Option<String>);

/// One subagent descriptor: the parent brain dir that names the child, plus
/// the role/type the descriptor JSON carries (native `_descriptor_role_type`).
#[derive(Clone, Debug)]
pub(super) struct SubagentDescriptor {
    parent: String,
    role: Option<String>,
    type_name: Option<String>,
}

/// Index `brain/<parent>/.system_generated/subagents/<child>.json` descriptors
/// under a scan target. The filename is the child session id; the parent dir
/// names the parent. Returns the index and whether it is COMPLETE: a single
/// conversation dir (or sub-path inside one), a walk I/O error, or an excluded
/// subtree sees incomplete sibling descriptors, so it cannot prove a main.
pub(super) fn subagent_descriptor_index(
    scan_target: &Path,
    excluded_paths: &[PathBuf],
) -> (HashMap<String, SubagentDescriptor>, bool) {
    if scan_target
        .ancestors()
        .any(AntigravityConnector::is_conversation_dir)
    {
        return (HashMap::new(), false);
    }
    let mut index = HashMap::new();
    let mut complete = true;
    let mut iter = WalkDir::new(scan_target)
        .min_depth(1)
        .max_depth(8)
        .sort_by_file_name()
        .into_iter();
    while let Some(entry_res) = iter.next() {
        let Ok(entry) = entry_res else {
            complete = false;
            continue;
        };
        let path = entry.path();
        if path_is_excluded(path, excluded_paths) {
            complete = false;
            if entry.file_type().is_dir() {
                iter.skip_current_dir();
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Require .../<parent>/.system_generated/subagents/<child>.json.
        let Some(sub_dir) = path.parent() else {
            continue;
        };
        if sub_dir.file_name().and_then(|n| n.to_str()) != Some("subagents") {
            continue;
        }
        let Some(sg) = sub_dir.parent() else {
            continue;
        };
        if sg.file_name().and_then(|n| n.to_str()) != Some(".system_generated") {
            continue;
        }
        let Some(parent_dir) = sg.parent() else {
            continue;
        };
        let Some(parent) = parent_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(child) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        let (role, type_name) = read_descriptor_role_type(path);
        index
            .entry(child.to_string())
            .or_insert_with(|| SubagentDescriptor {
                parent: parent.to_string(),
                role,
                type_name,
            });
    }
    (index, complete)
}

/// role/typeName from a descriptor JSON, preferring `subagentDescriptor` and
/// falling back to the top level (native `_descriptor_role_type`).
fn read_descriptor_role_type(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(content) = fs::read_to_string(path) else {
        return (None, None);
    };
    let Ok(val) = serde_json::from_str::<Value>(&content) else {
        return (None, None);
    };
    let Some(obj) = val.as_object() else {
        return (None, None);
    };
    let sd = obj.get("subagentDescriptor").and_then(Value::as_object);
    let field = |key: &str| -> Option<String> {
        sd.and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .or_else(|| obj.get(key).and_then(Value::as_str))
            .map(str::to_string)
    };
    (field("role"), field("typeName"))
}

/// Parse an immediate `\s*"<36 hex/dash chars>"` after `id:`.
fn parse_quoted_uuid(after_id_colon: &str) -> Option<String> {
    let rest = after_id_colon.trim_start().strip_prefix('"')?;
    let candidate: String = rest.chars().take(36).collect();
    (candidate.len() == 36
        && candidate
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-'))
    .then_some(candidate)
}

/// The first `id: "<36 hex/dash chars>"` in `scope` (native `id:\s*"`).
fn find_id_uuid(scope: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = scope[from..].find("id:") {
        from += rel + 3;
        if let Some(uuid) = parse_quoted_uuid(&scope[from..]) {
            return Some(uuid);
        }
    }
    None
}

/// Mirrors the native `PARENT_REMINDER_RE`: the parent UUID embedded in a
/// caller-agent, `<subagent_reminder>`, or `parent", id:` prompt reminder.
/// The leftmost matching reminder wins, as the native regex `search` does.
fn parent_reminder_uuid(text: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    let mut consider = |pos: usize, uuid: String| {
        if best.as_ref().is_none_or(|(b, _)| pos < *b) {
            best = Some((pos, uuid));
        }
    };
    // caller agent ( ... id: "<uuid>"  (the id must sit inside the parens)
    let mut search = 0;
    while let Some(rel) = text[search..].find("caller agent") {
        let pos = search + rel;
        search = pos + "caller agent".len();
        let rest = text[search..].trim_start();
        if let Some(after) = rest.strip_prefix('(') {
            let scope = after.find(')').map_or(after, |i| &after[..i]);
            if let Some(uuid) = find_id_uuid(scope) {
                consider(pos, uuid);
            }
        }
    }
    // <subagent_reminder> ... id: "<uuid>"
    let mut search = 0;
    while let Some(rel) = text[search..].find("<subagent_reminder>") {
        let pos = search + rel;
        search = pos + "<subagent_reminder>".len();
        if let Some(uuid) = find_id_uuid(&text[search..]) {
            consider(pos, uuid);
        }
    }
    // parent", id: "<uuid>" (anchored immediately after "parent\",", matching detect.py:16)
    let mut search = 0;
    while let Some(rel) = text[search..].find("parent\",") {
        let pos = search + rel;
        search = pos + "parent\",".len();
        let rest = text[search..].trim_start();
        if let Some(after_colon) = rest.strip_prefix("id:")
            && let Some(uuid) = parse_quoted_uuid(after_colon)
        {
            consider(pos, uuid);
        }
    }
    best.map(|(_, uuid)| uuid)
}

/// Resolve a conversation's subagent verdict: prompt reminder first (native
/// order), then the descriptor index. With a complete index and neither signal
/// the session is provably main (native closed-world semantics); an incomplete
/// index (a single-conversation scan) withholds the verdict instead of guessing.
pub(super) fn subagent_verdict(
    records: &[Value],
    conversation_uuid: Option<&str>,
    descriptors: &HashMap<String, SubagentDescriptor>,
    index_complete: bool,
) -> Option<SubagentVerdict> {
    let prompt_parent = records.iter().take(10).find_map(|rec| {
        ["content", "prompt"].iter().find_map(|key| {
            rec.get(key)
                .and_then(Value::as_str)
                .and_then(parent_reminder_uuid)
        })
    });
    let descriptor = conversation_uuid.and_then(|id| descriptors.get(id));
    match (prompt_parent, descriptor) {
        (Some(parent), d) => Some((
            true,
            Some(parent),
            d.and_then(|d| d.role.clone()),
            d.and_then(|d| d.type_name.clone()),
        )),
        (None, Some(d)) => Some((
            true,
            Some(d.parent.clone()),
            d.role.clone(),
            d.type_name.clone(),
        )),
        (None, None) if index_complete => Some((false, None, None, None)),
        (None, None) => None,
    }
}

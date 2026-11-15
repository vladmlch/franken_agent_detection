//! Pure forest helpers for Devin CLI `message_nodes` subagent discovery
//! and subtree extraction.

use std::collections::{HashMap, HashSet};

/// Prompt marker identifying the system root of a Devin CLI subagent tree.
pub(super) const SUBAGENT_MARKER: &str = "You are a subagent of Devin";

/// Minimal projection of a `message_nodes` row needed to discover subagent
/// roots and reconstruct their subtrees without SQLite.
pub(super) struct ForestNode {
    pub row_id: i64,
    pub node_id: i64,
    pub parent_node_id: Option<i64>,
    pub text: String,
}

fn children_of(nodes: &[ForestNode]) -> HashMap<i64, Vec<i64>> {
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for node in nodes {
        if let Some(parent) = node.parent_node_id {
            children.entry(parent).or_default().push(node.node_id);
        }
    }
    children
}

/// Return ascending `node_id`s of subagent roots matching the native rule:
/// parentless (`parent_node_id IS NULL`), containing [`SUBAGENT_MARKER`],
/// and having at least one child node.
pub(super) fn subagent_roots(nodes: &[ForestNode]) -> Vec<i64> {
    let children = children_of(nodes);
    let mut roots: Vec<i64> = nodes
        .iter()
        .filter(|n| n.parent_node_id.is_none())
        .filter(|n| n.text.contains(SUBAGENT_MARKER))
        .filter(|n| children.get(&n.node_id).is_some_and(|c| !c.is_empty()))
        .map(|n| n.node_id)
        .collect();
    roots.sort_unstable();
    roots.dedup();
    roots
}

/// Return all `node_id`s in the subtree rooted at `root`, ordered by
/// ascending `(row_id, node_id)`. Cycle-safe via visited tracking.
pub(super) fn subtree(nodes: &[ForestNode], root: i64) -> Vec<i64> {
    let children = children_of(nodes);
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(kids) = children.get(&id) {
            stack.extend(kids.iter().copied().filter(|k| !seen.contains(k)));
        }
    }
    let row_of: HashMap<i64, i64> = nodes.iter().map(|n| (n.node_id, n.row_id)).collect();
    let mut ordered: Vec<(i64, i64)> = seen
        .into_iter()
        .filter_map(|id| row_of.get(&id).map(|&row_id| (row_id, id)))
        .collect();
    ordered.sort_unstable();
    ordered.into_iter().map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(row_id: i64, node_id: i64, parent: Option<i64>, text: &str) -> ForestNode {
        ForestNode {
            row_id,
            node_id,
            parent_node_id: parent,
            text: text.to_owned(),
        }
    }

    #[test]
    fn roots_need_marker_no_parent_and_a_child() {
        let nodes = vec![
            n(1, 10, None, r#"{"role":"system","content":"main session"}"#),
            n(2, 11, Some(10), r#"{"role":"user","content":"hi"}"#),
            n(
                3,
                20,
                None,
                r#"{"role":"system","content":"You are a subagent of Devin. Task: x"}"#,
            ),
            n(4, 21, Some(20), r#"{"role":"user","content":"do x"}"#),
            // marker but childless: not a subagent session (native requires EXISTS child)
            n(
                5,
                30,
                None,
                r#"{"role":"system","content":"You are a subagent of Devin"}"#,
            ),
            // marker on a non-root node: not a root
            n(
                6,
                40,
                Some(11),
                r#"{"role":"system","content":"You are a subagent of Devin"}"#,
            ),
            n(7, 41, Some(40), r#"{"role":"user","content":"y"}"#),
        ];
        assert_eq!(subagent_roots(&nodes), vec![20]);
    }

    #[test]
    fn subtree_is_row_ordered_and_cycle_safe() {
        let nodes = vec![
            n(5, 20, None, "root"),
            n(9, 22, Some(21), "c2"),
            n(7, 21, Some(20), "c1"),
            n(8, 23, Some(20), "branch"),
            n(10, 24, Some(24), "self-loop"), // not reachable from 20
        ];
        assert_eq!(subtree(&nodes, 20), vec![20, 21, 23, 22]); // by row_id 5,7,8,9
    }
}

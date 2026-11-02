//! Local fix (cass review 2026-11-02, F4): one edges query plus one payload
//! query per 256 chain nodes, instead of one query per node. Same chain,
//! same order, same failure when a node disappears.

use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use frankensqlite::compat::{ParamValue, RowExt};

use super::{Connection, ConnectionExt, DevinChain, DevinNode, walk_main_chain};

const NODE_BATCH: usize = 256;

pub(super) fn load_main_chain_batched(
    conn: &Connection,
    session_id: &str,
    tip: i64,
) -> Result<DevinChain> {
    let edges: Vec<(i64, Option<i64>)> = conn
        .query_map_collect(
            "SELECT node_id, parent_node_id FROM message_nodes WHERE session_id = ?1",
            &[ParamValue::from(session_id)],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )
        .with_context(|| format!("failed to query Devin message_nodes for {session_id}"))?;
    let parents: HashMap<i64, Option<i64>> = edges.iter().copied().collect();
    let order = walk_main_chain(&parents, tip);
    let mut nodes = Vec::with_capacity(order.len());
    for ids in order.chunks(NODE_BATCH) {
        let placeholders = (0..ids.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT node_id, chat_message, created_at FROM message_nodes \
             WHERE session_id = ?1 AND node_id IN ({placeholders})"
        );
        let mut params = Vec::with_capacity(ids.len() + 1);
        params.push(ParamValue::from(session_id));
        params.extend(ids.iter().map(|id| ParamValue::from(*id)));
        let rows: Vec<(i64, DevinNode)> = conn
            .query_map_collect(&sql, &params, |row| {
                Ok((
                    row.get_typed(0)?,
                    DevinNode {
                        chat_message: row.get_typed::<Option<String>>(1)?.unwrap_or_default(),
                        created_at: row.get_typed(2)?,
                    },
                ))
            })
            .with_context(|| format!("failed to load Devin message nodes of {session_id}"))?;
        let mut by_id = HashMap::with_capacity(rows.len());
        for (id, node) in rows {
            ensure!(
                by_id.insert(id, node).is_none(),
                "duplicate Devin node id {id}"
            );
        }
        for id in ids {
            nodes.push(by_id.remove(id).with_context(|| {
                format!("failed to load Devin message node {id} of {session_id}")
            })?);
        }
    }
    Ok(DevinChain {
        off_chain_nodes: edges.len().saturating_sub(order.len()),
        nodes,
    })
}

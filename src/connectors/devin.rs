//! Devin CLI connector (Cognition).
//!
//! The Devin CLI keeps every local session in ONE shared SQLite store,
//! `~/.local/share/devin/cli/sessions.db` (cass #449):
//!
//! - `sessions` — `id`, `title`, `working_directory`, `model`, `agent_mode`,
//!   `created_at` / `last_activity_at` (epoch **seconds**), `main_chain_id`
//!   (nullable), `hidden` (retired sessions that `devin list` omits).
//! - `message_nodes` — a *forest*, not a list: `session_id`, `node_id`,
//!   `parent_node_id`, `chat_message` (JSON), `created_at`. Branches are
//!   retries and edits; the live conversation is the parent chain that ends
//!   at `sessions.main_chain_id`. Replaying every row would interleave
//!   abandoned turns with the real ones (surveys of real stores put ~87% of
//!   the table off-chain), so the scan projects the main chain only.
//!
//! `chat_message` shapes, by `role`:
//! - `system` — `{message_id, role, content, metadata}` (not indexed);
//! - `user` — same, plus optional `images` (inline base64) and
//!   `metadata.is_user_input`;
//! - `assistant` — plus `tool_calls[] {id, index, kind, name, arguments}`
//!   where `arguments` is a JSON **object**, and `thinking {signature, thinking}`;
//! - `tool` — plus `tool_call_id` and optional `images`.
//!
//! `content` is a plain string in every observed node; the OpenAI-style parts
//! array is accepted too. Image payloads never reach the indexed text — an
//! `[image]` marker stands in for each one.
//!
//! Read-only: the store is opened through the F9 helper's strict never-write
//! mode (`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_STRICT_READ_ONLY`, with a busy
//! timeout because the live CLI may hold it open), so no sidecar of the
//! user's store is ever created or modified. Devin's cloud sessions are not
//! on disk and are out of scope here.

#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::path::PathBuf;

use super::utils::excluded_scan_paths_from_env;
use super::{Connector, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};

#[cfg(feature = "devin")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "devin")]
use std::path::Path;

#[cfg(feature = "devin")]
use anyhow::{Context, Result};

#[cfg(feature = "devin")]
use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
#[cfg(feature = "devin")]
use super::sqlite_sync::{
    Connection, ConnectionExt, Row, RowExt, open_read_only_with_snapshot_fallback, params,
};
#[cfg(feature = "devin")]
use super::utils::{env_path_nonempty, path_is_excluded, unwrap_skill_invocations};
#[cfg(feature = "devin")]
use crate::types::{NormalizedInvocation, NormalizedMessage};

// Local fix (cass review 2026-11-02, F4).
#[cfg(feature = "devin")]
mod batched_chain;
// Local fix (cass review 2026-11-02, F5).
#[cfg(feature = "devin")]
mod acp_store;
#[cfg(feature = "devin")]
mod checked_scan;
#[cfg(feature = "devin")]
mod subagent_tree;

/// Override for the Devin data location. Accepts the `sessions.db` file, the
/// `cli` directory holding it, or the `devin` data root above that.
pub const DEVIN_DATA_ROOT_ENV: &str = "CASS_DEVIN_DATA_ROOT";

/// Upper bound on one `parent_node_id` walk. Nothing in the schema forbids a
/// cycle, and a real conversation is two orders of magnitude shorter.
#[cfg(feature = "devin")]
const MAX_CHAIN_DEPTH: usize = 50_000;

/// Serialized `tool_calls[].arguments` larger than this are replaced by a
/// size marker so one giant payload cannot dominate a message.
#[cfg(feature = "devin")]
const MAX_TOOL_ARGUMENTS_BYTES: usize = 32 * 1024;

/// The `-wal`/`-shm` sibling of a store file (`sessions.db` ->
/// `sessions.db-wal`). A live store's truth spans the whole bundle, and a
/// WAL-only commit never touches the primary file.
#[cfg(feature = "devin")]
fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db
        .file_name()
        .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
    name.push_str(suffix);
    db.with_file_name(name)
}

pub struct DevinConnector;

impl Default for DevinConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl DevinConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Resolve `sessions.db` from an override value that may name the file,
    /// its `cli` directory, or the `devin` data root.
    #[must_use]
    pub fn database_path_from_root(root: &std::path::Path) -> PathBuf {
        if root.extension().is_some_and(|ext| ext == "db") {
            return root.to_path_buf();
        }
        let direct = root.join("sessions.db");
        if direct.is_file() {
            return direct;
        }
        let nested = root.join("cli").join("sessions.db");
        if nested.is_file() {
            return nested;
        }
        direct
    }

    /// Default store: `~/.local/share/devin/cli/sessions.db` on every
    /// platform (the CLI does not use the macOS `Application Support` dir).
    #[cfg(feature = "devin")]
    fn default_database_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| {
            home.join(".local")
                .join("share")
                .join("devin")
                .join("cli")
                .join("sessions.db")
        })
    }

    /// Default Devin IDE `acp-messages` store directories across platforms.
    #[cfg(feature = "devin")]
    fn default_acp_messages_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(config_dir) = dirs::config_dir() {
            dirs.push(config_dir.join("Devin").join("User").join("acp-messages"));
        }
        if let Some(home) = dirs::home_dir() {
            for rel in [
                &[
                    "Library",
                    "Application Support",
                    "Devin",
                    "User",
                    "acp-messages",
                ][..],
                &[".config", "Devin", "User", "acp-messages"][..],
                &["AppData", "Roaming", "Devin", "User", "acp-messages"][..],
            ] {
                let path = rel.iter().fold(home.clone(), |p, seg| p.join(seg));
                if !dirs.contains(&path) {
                    dirs.push(path);
                }
            }
        }
        dirs
    }

    /// Whether `path` looks like a Devin IDE `acp-messages/<uuid>.db` store
    /// (excluding CLI `sessions.db`, viewer caches, and dotfiles).
    #[cfg(feature = "devin")]
    fn is_acp_db_file(path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        if path.extension().is_none_or(|ext| ext != "db") {
            return false;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        !name.starts_with('.') && name != "sessions.db" && !name.starts_with("viewer_")
    }

    #[cfg(feature = "devin")]
    fn push_acp_dir_dbs(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| Self::is_acp_db_file(p))
            .collect();
        found.sort();
        out.extend(found);
    }

    /// Resolve Devin IDE `acp-messages/*.db` stores from a root path that may
    /// name one `<uuid>.db`, an `acp-messages` dir, a `User` dir, or the
    /// parent `Devin` dir.
    #[cfg(feature = "devin")]
    fn collect_acp_dbs_from_root(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if root.is_file() {
            if Self::is_acp_db_file(root) {
                out.push(root.to_path_buf());
            }
            return out;
        }
        if !root.is_dir() {
            return out;
        }
        if root.file_name().and_then(|n| n.to_str()) == Some("acp-messages") {
            Self::push_acp_dir_dbs(root, &mut out);
        }
        let direct_acp = root.join("acp-messages");
        if direct_acp.is_dir() {
            Self::push_acp_dir_dbs(&direct_acp, &mut out);
        }
        let user_acp = root.join("User").join("acp-messages");
        if user_acp.is_dir() {
            Self::push_acp_dir_dbs(&user_acp, &mut out);
        }
        out
    }
}

#[cfg(feature = "devin")]
impl DevinConnector {
    /// Collect both CLI (`sessions.db`) and IDE (`acp-messages/*.db`) stores
    /// located under `root`.
    fn collect_stores_under_root(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let cli_db = Self::database_path_from_root(root);
        if cli_db.is_file() {
            found.push(cli_db);
        }
        found.extend(Self::collect_acp_dbs_from_root(root));
        found
    }

    /// Database candidates for a scan, in precedence order:
    /// a `.db` data_dir; default detection (stores under data_dir scope the
    /// scan, then the env override, then default CLI + IDE locations); or,
    /// with explicit roots, every root that resolves to CLI or IDE stores.
    fn database_candidates(ctx: &ScanContext) -> Vec<(ScanRoot, PathBuf)> {
        let mut candidates: Vec<(ScanRoot, PathBuf)> = Vec::new();
        if ctx.data_dir.extension().is_some_and(|ext| ext == "db") {
            candidates.push((ScanRoot::local(ctx.data_dir.clone()), ctx.data_dir.clone()));
        } else if ctx.use_default_detection() {
            let scoped = if ctx.data_dir.as_os_str().is_empty() {
                Vec::new()
            } else {
                Self::collect_stores_under_root(&ctx.data_dir)
            };
            if !scoped.is_empty() {
                for db in scoped {
                    candidates.push((ScanRoot::local(db.clone()), db));
                }
            } else if let Some(root) = env_path_nonempty(DEVIN_DATA_ROOT_ENV) {
                for db in Self::collect_stores_under_root(&root) {
                    candidates.push((ScanRoot::local(db.clone()), db));
                }
            } else {
                if let Some(db) = Self::default_database_path() {
                    candidates.push((ScanRoot::local(db.clone()), db));
                }
                for acp_dir in Self::default_acp_messages_dirs() {
                    for db in Self::collect_acp_dbs_from_root(&acp_dir) {
                        candidates.push((ScanRoot::local(db.clone()), db));
                    }
                }
            }
        } else {
            for scan_root in &ctx.scan_roots {
                if let Some(mirrored) =
                    scan_root.mirrored_store(&[".local", "share", "devin", "cli"])
                {
                    candidates.push((scan_root.clone(), mirrored.join("sessions.db")));
                }
                for ide_rel in [
                    &[
                        "Library",
                        "Application Support",
                        "Devin",
                        "User",
                        "acp-messages",
                    ][..],
                    &[".config", "Devin", "User", "acp-messages"][..],
                    &["AppData", "Roaming", "Devin", "User", "acp-messages"][..],
                ] {
                    if let Some(mirrored) = scan_root.mirrored_store(ide_rel) {
                        for db in Self::collect_acp_dbs_from_root(&mirrored) {
                            candidates.push((scan_root.clone(), db));
                        }
                    }
                }
                let db = Self::database_path_from_root(&scan_root.path);
                candidates.push((scan_root.clone(), db));
                for db in Self::collect_acp_dbs_from_root(&scan_root.path) {
                    candidates.push((scan_root.clone(), db));
                }
            }
        }
        let mut seen = HashSet::new();
        candidates.retain(|(_, db)| db.is_file() && seen.insert(db.clone()));
        candidates
    }

    fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        Ok(Self::database_candidates(ctx)
            .into_iter()
            .filter(|(root, db)| {
                !path_is_excluded(&root.path, &excluded_paths)
                    && !path_is_excluded(db, &excluded_paths)
            })
            .flat_map(|(root, db)| {
                let mut sources = vec![
                    DiscoveredSourceFile::new(
                        "devin",
                        &root,
                        db.clone(),
                        DiscoveredSourceRole::SqliteDatabase,
                        true,
                    )
                    .with_fs_metadata(),
                ];
                // A live store's truth spans the db and its -wal (+ -shm): a
                // WAL-only commit leaves the primary file's size and mtime
                // untouched, so the sidecars observed here are the reuse
                // evidence that still sees it — the same discovery contract
                // the shelley connector keeps for its SQLite stores.
                for (suffix, required_for_reconstruction) in [("-wal", true), ("-shm", false)] {
                    let sidecar = sidecar_path(&db, suffix);
                    if sidecar.is_file() {
                        sources.push(
                            DiscoveredSourceFile::new(
                                "devin",
                                &root,
                                sidecar,
                                DiscoveredSourceRole::MetadataSidecar,
                                required_for_reconstruction,
                            )
                            .with_fs_metadata(),
                        );
                    }
                }
                sources
            })
            .collect())
    }

    /// Dispatch a discovered SQLite store to either the CLI (`sessions.db`) or
    /// IDE (`acp-messages/<uuid>.db`) extractor.
    fn extract_store(db_path: &Path, since_ts: Option<i64>) -> Result<Vec<NormalizedConversation>> {
        if db_path.file_name().and_then(|n| n.to_str()) == Some("sessions.db") {
            return Self::extract_from_sqlite(db_path, since_ts);
        }
        if db_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("acp-messages")
        {
            return acp_store::extract_acp_store(db_path, since_ts);
        }
        let conn = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to open Devin db: {}", db_path.display()))?;
        conn.execute("PRAGMA busy_timeout = 5000;")
            .context("failed to set busy_timeout on Devin db")?;
        let tables: Vec<String> = conn
            .query_map_collect(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('sessions', 'messages')",
                params![],
                |row| row.get_typed(0),
            )
            .with_context(|| format!("failed to probe Devin schema: {}", db_path.display()))?;
        drop(conn);

        if tables.iter().any(|t| t == "sessions") {
            Self::extract_from_sqlite(db_path, since_ts)
        } else if tables.iter().any(|t| t == "messages") {
            acp_store::extract_acp_store(db_path, since_ts)
        } else {
            Ok(Vec::new())
        }
    }

    /// Read every visible session's main chain from one store.
    ///
    /// `since_ts` (epoch ms) skips sessions whose `last_activity_at` is
    /// older. Sessions without a live chain or without indexable messages
    /// are skipped.
    pub fn extract_from_sqlite(
        db_path: &Path,
        since_ts: Option<i64>,
    ) -> Result<Vec<NormalizedConversation>> {
        let conn = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to open Devin db: {}", db_path.display()))?;
        conn.execute("PRAGMA busy_timeout = 5000;")
            .context("failed to set busy_timeout on Devin db")?;

        let sessions = Self::load_sessions(&conn)?;
        let mut conversations = Vec::with_capacity(sessions.len());
        let mut seen_ids = HashSet::new();
        for session in sessions {
            if session.id.is_empty() || !seen_ids.insert(session.id.clone()) {
                continue;
            }
            let started_at = epoch_seconds_to_ms(session.created_at);
            let ended_at = epoch_seconds_to_ms(session.last_activity_at);
            if let Some(since) = since_ts
                && ended_at.or(started_at).unwrap_or(0) < since
            {
                continue;
            }
            let Some(main_chain_id) = session.main_chain_id else {
                continue;
            };
            let chain = batched_chain::load_main_chain_batched(&conn, &session.id, main_chain_id)?;
            let mut messages = Vec::new();
            for node in &chain.nodes {
                if let Some(message) = message_from_chat_message(
                    &node.chat_message,
                    epoch_seconds_to_ms(node.created_at),
                    session.model.as_deref(),
                ) {
                    messages.push(message);
                }
            }
            if messages.is_empty() {
                continue;
            }
            assign_message_indices(&mut messages);

            let title = session
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned)
                .or_else(|| first_user_line_title(&messages));
            let workspace = session
                .working_directory
                .as_deref()
                .map(str::trim)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from);
            let message_started_at = messages.iter().filter_map(|m| m.created_at).min();
            let message_ended_at = messages.iter().filter_map(|m| m.created_at).max();

            conversations.push(NormalizedConversation {
                agent_slug: "devin".into(),
                external_id: Some(session.id.clone()),
                title,
                workspace: workspace.clone(),
                source_path: db_path.join(urlencoding::encode(&session.id).as_ref()),
                started_at: started_at.or(message_started_at),
                ended_at: ended_at.or(message_ended_at),
                metadata: serde_json::json!({
                    "session_id": session.id,
                    "model": session.model,
                    "agent_mode": session.agent_mode,
                    "main_chain_id": main_chain_id,
                    "main_chain_nodes": chain.nodes.len(),
                    "off_chain_nodes": chain.off_chain_nodes,
                    "source": "sqlite",
                    "is_subagent": false,
                }),
                messages,
            });

            if Self::has_subagent_marker_root(&conn, &session.id)? {
                Self::extract_subagent_conversations(
                    &conn,
                    db_path,
                    &session,
                    workspace.as_deref(),
                    &mut conversations,
                )?;
            }
        }
        Ok(conversations)
    }

    fn has_subagent_marker_root(conn: &Connection, session_id: &str) -> Result<bool> {
        let rows: Vec<i64> = conn
            .query_map_collect(
                "SELECT 1 FROM message_nodes m \
                 WHERE m.session_id = ?1 AND m.parent_node_id IS NULL \
                   AND instr(m.chat_message, ?2) > 0 \
                 LIMIT 1",
                params![session_id, subagent_tree::SUBAGENT_MARKER],
                |row| row.get_typed(0),
            )
            .with_context(|| format!("failed to probe Devin subagent roots for {session_id}"))?;
        Ok(!rows.is_empty())
    }

    fn load_session_forest(
        conn: &Connection,
        session_id: &str,
    ) -> Result<(Vec<subagent_tree::ForestNode>, Vec<Option<i64>>)> {
        let map_row = |row: &Row<'_>| {
            Ok((
                subagent_tree::ForestNode {
                    row_id: row.get_typed(0)?,
                    node_id: row.get_typed(1)?,
                    parent_node_id: row.get_typed(2)?,
                    text: row.get_typed::<Option<String>>(3)?.unwrap_or_default(),
                },
                row.get_typed::<Option<i64>>(4)?,
            ))
        };
        let rows = match conn.query_map_collect(
            "SELECT row_id, node_id, parent_node_id, chat_message, created_at \
             FROM message_nodes WHERE session_id = ?1",
            params![session_id],
            map_row,
        ) {
            Ok(rows) => rows,
            Err(row_id_err) => conn
                .query_map_collect(
                    "SELECT node_id AS row_id, node_id, parent_node_id, chat_message, created_at \
                     FROM message_nodes WHERE session_id = ?1",
                    params![session_id],
                    map_row,
                )
                .with_context(|| {
                    format!(
                        "failed to load Devin message_nodes for {session_id} \
                         (row_id query failed: {row_id_err})"
                    )
                })?,
        };
        Ok(rows.into_iter().unzip())
    }

    fn extract_subagent_conversations(
        conn: &Connection,
        db_path: &Path,
        session: &DevinSession,
        workspace: Option<&Path>,
        conversations: &mut Vec<NormalizedConversation>,
    ) -> Result<()> {
        let (forest, created_ats) = Self::load_session_forest(conn, &session.id)?;
        let roots = subagent_tree::subagent_roots(&forest);
        if roots.is_empty() {
            return Ok(());
        }
        let index_by_id: HashMap<i64, usize> = forest
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.node_id, idx))
            .collect();
        let encoded_sid = urlencoding::encode(&session.id);

        for root in roots {
            let node_ids = subagent_tree::subtree(&forest, root);
            let mut messages = Vec::new();
            let mut min_ts: Option<i64> = None;
            let mut max_ts: Option<i64> = None;
            for node_id in node_ids {
                let Some(&idx) = index_by_id.get(&node_id) else {
                    continue;
                };
                let created_at = created_ats[idx];
                if let Some(ts) = created_at {
                    min_ts = Some(min_ts.map_or(ts, |cur| cur.min(ts)));
                    max_ts = Some(max_ts.map_or(ts, |cur| cur.max(ts)));
                }
                if let Some(msg) = message_from_chat_message(
                    &forest[idx].text,
                    epoch_seconds_to_ms(created_at),
                    session.model.as_deref(),
                ) {
                    messages.push(msg);
                }
            }
            if messages.is_empty() {
                continue;
            }
            assign_message_indices(&mut messages);
            let title = first_user_line_title(&messages);
            let external_id = format!("{}:agent-{root}", session.id);
            let source_path = db_path.join(format!("{encoded_sid}:agent-{root}"));
            conversations.push(NormalizedConversation {
                agent_slug: "devin".into(),
                external_id: Some(external_id),
                title,
                workspace: workspace.map(Path::to_path_buf),
                source_path,
                started_at: epoch_seconds_to_ms(min_ts),
                ended_at: epoch_seconds_to_ms(max_ts),
                metadata: serde_json::json!({
                    "session_id": session.id,
                    "subagent_root_node": root,
                    "source": "sqlite",
                    "is_subagent": true,
                    "parent_id": session.id,
                }),
                messages,
            });
        }
        Ok(())
    }

    fn load_sessions(conn: &Connection) -> Result<Vec<DevinSession>> {
        const COLUMNS: &str = "id, title, working_directory, model, agent_mode, \
                               created_at, last_activity_at, main_chain_id";
        let map_row = |row: &Row<'_>| {
            Ok(DevinSession {
                id: row.get_typed::<Option<String>>(0)?.unwrap_or_default(),
                title: row.get_typed(1)?,
                working_directory: row.get_typed(2)?,
                model: row.get_typed(3)?,
                agent_mode: row.get_typed(4)?,
                created_at: row.get_typed(5)?,
                last_activity_at: row.get_typed(6)?,
                main_chain_id: row.get_typed(7)?,
            })
        };
        // `hidden` marks sessions the CLI retired; they stay in the table but
        // never appear in `devin list`. Older stores may predate the column,
        // so fall back to an unfiltered read rather than failing the scan.
        match conn.query_map_collect(
            &format!("SELECT {COLUMNS} FROM sessions WHERE COALESCE(hidden, 0) = 0"),
            params![],
            map_row,
        ) {
            Ok(rows) => Ok(rows),
            Err(filtered_err) => conn
                .query_map_collect(
                    &format!("SELECT {COLUMNS} FROM sessions"),
                    params![],
                    map_row,
                )
                .with_context(|| {
                    format!("failed to query Devin sessions (hidden filter failed: {filtered_err})")
                }),
        }
    }

    /// Walk `parent_node_id` from the chain tip to its root and return the
    /// chain in reading (root-to-tip) order, plus how many rows of the
    /// session's forest were left out.
    #[cfg(test)]
    fn load_main_chain(conn: &Connection, session_id: &str, tip: i64) -> Result<DevinChain> {
        let edges: Vec<(i64, Option<i64>)> = conn
            .query_map_collect(
                "SELECT node_id, parent_node_id FROM message_nodes WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .with_context(|| format!("failed to query Devin message_nodes for {session_id}"))?;
        let parents: HashMap<i64, Option<i64>> = edges.iter().copied().collect();
        let order = walk_main_chain(&parents, tip);
        let mut nodes = Vec::with_capacity(order.len());
        for node_id in &order {
            let node = conn
                .query_row_map(
                    "SELECT chat_message, created_at FROM message_nodes \
                     WHERE session_id = ?1 AND node_id = ?2",
                    params![session_id, *node_id],
                    |row| {
                        Ok(DevinNode {
                            chat_message: row.get_typed::<Option<String>>(0)?.unwrap_or_default(),
                            created_at: row.get_typed(1)?,
                        })
                    },
                )
                .with_context(|| {
                    format!("failed to load Devin message node {node_id} of {session_id}")
                })?;
            nodes.push(node);
        }
        Ok(DevinChain {
            off_chain_nodes: edges.len().saturating_sub(order.len()),
            nodes,
        })
    }
}

#[cfg(feature = "devin")]
struct DevinSession {
    id: String,
    title: Option<String>,
    working_directory: Option<String>,
    model: Option<String>,
    agent_mode: Option<String>,
    created_at: Option<i64>,
    last_activity_at: Option<i64>,
    main_chain_id: Option<i64>,
}

#[cfg(feature = "devin")]
struct DevinNode {
    chat_message: String,
    created_at: Option<i64>,
}

#[cfg(feature = "devin")]
struct DevinChain {
    nodes: Vec<DevinNode>,
    off_chain_nodes: usize,
}

/// Root-to-tip node ids of the chain ending at `tip`. Stops at a missing
/// parent, a node not in the session, a repeated node (cycle), or the depth
/// bound — never spins.
#[cfg(feature = "devin")]
fn walk_main_chain(parents: &HashMap<i64, Option<i64>>, tip: i64) -> Vec<i64> {
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    let mut cursor = Some(tip);
    while let Some(node_id) = cursor {
        if order.len() >= MAX_CHAIN_DEPTH || !visited.insert(node_id) {
            break;
        }
        let Some(parent) = parents.get(&node_id) else {
            break;
        };
        order.push(node_id);
        cursor = *parent;
    }
    order.reverse();
    order
}

#[cfg(feature = "devin")]
fn epoch_seconds_to_ms(seconds: Option<i64>) -> Option<i64> {
    seconds
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
}

#[cfg(feature = "devin")]
fn assign_message_indices(messages: &mut [NormalizedMessage]) {
    for (idx, message) in messages.iter_mut().enumerate() {
        message.idx = i64::try_from(idx).unwrap_or(i64::MAX);
    }
}

#[cfg(feature = "devin")]
fn first_user_line_title(messages: &[NormalizedMessage]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.role == "user")
        .and_then(|message| message.content.lines().next())
        .map(|line| line.chars().take(100).collect())
}

/// `content` as text: a plain string, or an OpenAI-style parts array whose
/// `text` parts are joined and whose image parts become `[image]` markers.
#[cfg(feature = "devin")]
fn text_content(content: Option<&serde_json::Value>) -> Option<String> {
    match content? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let chunks: Vec<&str> = parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => part.get("text").and_then(|t| t.as_str()),
                    Some("image" | "image_url") => Some("[image]"),
                    _ => None,
                })
                .collect();
            (!chunks.is_empty()).then(|| chunks.join("\n"))
        }
        _ => None,
    }
}

/// `[image]` markers for the inline base64 `images` array, so a screenshot
/// payload never reaches the indexed text.
#[cfg(feature = "devin")]
fn image_markers(object: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let count = object.get("images").and_then(|v| v.as_array())?.len();
    (count > 0).then(|| vec!["[image]"; count].join("\n"))
}

#[cfg(feature = "devin")]
fn join_nonempty(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .flatten()
        .filter(|part| !part.trim().is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `message_nodes.chat_message` payload → at most one normalized message.
/// `system` nodes, contentless assistant nodes, and unparseable payloads
/// yield `None`. Visible for tests.
#[cfg(feature = "devin")]
pub(crate) fn message_from_chat_message(
    chat_message: &str,
    created_at: Option<i64>,
    model: Option<&str>,
) -> Option<NormalizedMessage> {
    let value: serde_json::Value = serde_json::from_str(chat_message).ok()?;
    let object = value.as_object()?;
    let role = object.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let content = text_content(object.get("content"));
    let message_id = object
        .get("message_id")
        .and_then(|id| id.as_str())
        .map(str::to_owned);
    let mut extra = serde_json::Map::new();
    if let Some(message_id) = &message_id {
        extra.insert(
            "message_id".into(),
            serde_json::Value::String(message_id.clone()),
        );
    }

    let message = match role {
        "user" => {
            let text = join_nonempty(&[image_markers(object), content]);
            if text.is_empty() {
                return None;
            }
            if let Some(is_user_input) = object
                .get("metadata")
                .and_then(|m| m.get("is_user_input"))
                .and_then(serde_json::Value::as_bool)
            {
                extra.insert(
                    "is_user_input".into(),
                    serde_json::Value::Bool(is_user_input),
                );
            }
            NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at,
                content: text,
                extra: serde_json::Value::Object(extra),
                snippets: Vec::new(),
                invocations: Vec::new(),
            }
        }
        "assistant" => {
            let mut invocations = Vec::new();
            let mut call_lines = Vec::new();
            for call in object
                .get("tool_calls")
                .and_then(|calls| calls.as_array())
                .into_iter()
                .flatten()
            {
                let name = call
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_owned();
                let arguments = call.get("arguments").cloned().map(bound_tool_arguments);
                call_lines.push(format!("[tool_call: {name}]"));
                invocations.push(NormalizedInvocation {
                    kind: "tool".into(),
                    name,
                    raw_name: None,
                    call_id: call.get("id").and_then(|id| id.as_str()).map(str::to_owned),
                    arguments,
                });
            }
            unwrap_skill_invocations(&mut invocations);
            let thinking = object
                .get("thinking")
                .and_then(|t| t.get("thinking"))
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty());
            if let Some(thinking) = thinking {
                extra.insert(
                    "thinking".into(),
                    serde_json::Value::String(thinking.to_owned()),
                );
            }
            if let Some(model_str) = model.filter(|s| !s.trim().is_empty()) {
                extra.insert(
                    "model".into(),
                    serde_json::Value::String(model_str.to_owned()),
                );
            }
            if !invocations.is_empty() {
                extra.insert(
                    "cass".into(),
                    serde_json::json!({ "tool_call_count": invocations.len() }),
                );
            }
            let text = join_nonempty(&[
                content,
                (!call_lines.is_empty()).then(|| call_lines.join("\n")),
            ]);
            if text.is_empty() {
                return None;
            }
            NormalizedMessage {
                idx: 0,
                role: "assistant".into(),
                author: model.map(str::to_owned),
                created_at,
                content: text,
                extra: serde_json::Value::Object(extra),
                snippets: Vec::new(),
                invocations,
            }
        }
        "tool" => {
            let text = join_nonempty(&[image_markers(object), content]);
            if text.is_empty() {
                return None;
            }
            if let Some(call_id) = object.get("tool_call_id").and_then(|id| id.as_str()) {
                extra.insert(
                    "tool_call_id".into(),
                    serde_json::Value::String(call_id.to_owned()),
                );
            }
            NormalizedMessage {
                idx: 0,
                role: "tool".into(),
                author: None,
                created_at,
                content: text,
                extra: serde_json::Value::Object(extra),
                snippets: Vec::new(),
                invocations: Vec::new(),
            }
        }
        _ => return None,
    };
    Some(message)
}

/// `tool_calls[].arguments` is a JSON object; keep it as-is unless its
/// serialized size is unreasonable, in which case a marker replaces it.
#[cfg(feature = "devin")]
fn bound_tool_arguments(arguments: serde_json::Value) -> serde_json::Value {
    let size = arguments.to_string().len();
    if size > MAX_TOOL_ARGUMENTS_BYTES {
        serde_json::Value::String(format!("[OMITTED large JSON payload bytes={size}]"))
    } else {
        arguments
    }
}

impl Connector for DevinConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("devin").unwrap_or_else(DetectionResult::not_found)
    }

    fn supports_streaming_scan(&self) -> bool {
        cfg!(feature = "devin")
    }

    fn supports_source_boundaries(&self) -> bool {
        cfg!(feature = "devin")
    }

    #[cfg(feature = "devin")]
    fn scan_with_callback(
        &self,
        ctx: &super::scan::ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.scan_with_source_boundaries(ctx, &mut SourceScanHooks::default(), on_conversation)
    }

    #[cfg(feature = "devin")]
    fn scan_with_source_boundaries(
        &self,
        ctx: &super::scan::ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let sources = Self::discover_sources(ctx)?;
        // Local fix (cass review 2026-11-02, F5): failures reach the host.
        let mut failures = checked_scan::ScanFailures::new();
        for source in sources {
            // Discovery lists the store's -wal/-shm sidecars so the host's
            // reuse evidence covers WAL-only commits; they are reconstruction
            // inputs, not scannable sources.
            if source.role != DiscoveredSourceRole::SqliteDatabase {
                continue;
            }
            if !hooks.should_scan(&source) {
                continue;
            }
            // Local fix (cass review 2026-11-07, F9): a store that is a valid
            // but empty database never reaches the extractor — there is
            // nothing to read, and a fresh install must not report a scan
            // error on every index run.
            if super::checked_scan::skip_provably_empty_store("Devin", &source.source_path) {
                continue;
            }
            ctx.tick();
            // A WAL-only commit (how the live CLI appends a turn) appends
            // frames to the -wal without touching the primary file, so the
            // -wal observed here — built exactly like discovery — is the
            // completion evidence that must invalidate a future skip.
            let wal = sidecar_path(&source.source_path, "-wal");
            let wal_discovered = wal.is_file().then(|| {
                let mut sidecar = source.clone();
                sidecar.source_path = wal;
                sidecar.role = DiscoveredSourceRole::MetadataSidecar;
                sidecar.required_for_reconstruction = true;
                sidecar.with_fs_metadata()
            });
            match Self::extract_store(&source.source_path, ctx.since_ts) {
                Ok(found) => {
                    tracing::debug!(
                        "devin sqlite: found {} sessions in {}",
                        found.len(),
                        source.source_path.display()
                    );
                    let mut conversations_emitted = 0usize;
                    for mut conv in found {
                        ctx.apply_workspace_rewrite_for_path(&source.scan_root, &mut conv);
                        on_conversation(conv)?;
                        conversations_emitted += 1;
                    }
                    let changed = source.fs_metadata_changed()
                        || wal_discovered
                            .as_ref()
                            .is_some_and(DiscoveredSourceFile::fs_metadata_changed);
                    if conversations_emitted > 0 && !changed {
                        hooks.complete(&SourceCompletion {
                            source,
                            required_sidecars: wal_discovered.into_iter().collect(),
                            conversations_emitted,
                        })?;
                    }
                }
                Err(error) => failures.record(&source.source_path, &error),
            }
        }
        failures.finish()
    }

    #[cfg(feature = "devin")]
    fn scan(&self, ctx: &super::scan::ScanContext) -> anyhow::Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conv| {
            conversations.push(conv);
            Ok(())
        })?;
        Ok(conversations)
    }

    /// Without the `devin` feature the connector is detection-only.
    #[cfg(not(feature = "devin"))]
    fn scan(&self, _ctx: &super::scan::ScanContext) -> anyhow::Result<Vec<NormalizedConversation>> {
        let _ = excluded_scan_paths_from_env()?;
        Ok(Vec::new())
    }

    #[cfg(feature = "devin")]
    fn discover_source_files(
        &self,
        ctx: &super::scan::ScanContext,
    ) -> anyhow::Result<Vec<DiscoveredSourceFile>> {
        Self::discover_sources(ctx)
    }

    #[cfg(not(feature = "devin"))]
    fn discover_source_files(
        &self,
        _ctx: &super::scan::ScanContext,
    ) -> anyhow::Result<Vec<super::scan::DiscoveredSourceFile>> {
        let _ = excluded_scan_paths_from_env()?;
        Ok(Vec::new())
    }
}

#[cfg(all(test, feature = "devin"))]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn open_test_connection(path: &Path) -> Connection {
        Connection::open(path.to_string_lossy().as_ref()).unwrap()
    }

    /// Schema as observed in CLI 3000.x stores (columns the connector reads
    /// plus a few it ignores).
    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                working_directory TEXT,
                backend_type TEXT,
                model TEXT,
                agent_mode TEXT,
                created_at INTEGER,
                last_activity_at INTEGER,
                main_chain_id INTEGER,
                hidden INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE message_nodes (
                row_id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                node_id INTEGER NOT NULL,
                parent_node_id INTEGER,
                chat_message TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                metadata TEXT,
                UNIQUE(session_id, node_id)
            );",
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_session(
        conn: &Connection,
        id: &str,
        title: &str,
        cwd: &str,
        created: i64,
        last: i64,
        main_chain_id: Option<i64>,
        hidden: i64,
    ) {
        conn.execute_compat(
            "INSERT INTO sessions (id, title, working_directory, backend_type, model, agent_mode, \
             created_at, last_activity_at, main_chain_id, hidden) \
             VALUES (?1, ?2, ?3, 'Windsurf', 'swe-1-7', 'bypass', ?4, ?5, ?6, ?7)",
            params![id, title, cwd, created, last, main_chain_id, hidden],
        )
        .unwrap();
    }

    fn insert_node(
        conn: &Connection,
        node_id: i64,
        session_id: &str,
        parent: Option<i64>,
        chat_message: &serde_json::Value,
        created: i64,
    ) {
        conn.execute_compat(
            "INSERT INTO message_nodes (node_id, session_id, parent_node_id, chat_message, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![node_id, session_id, parent, chat_message.to_string(), created],
        )
        .unwrap();
    }

    /// The reference fixture shape: system → user → assistant(tool_call) →
    /// tool → assistant, with an abandoned branch hanging off the user turn.
    fn seed_reference_store(db_path: &Path) {
        let conn = open_test_connection(db_path);
        create_schema(&conn);
        insert_session(
            &conn,
            "bald-ketch",
            "Fix the broken checks",
            "/tmp/as-agent-lab/devin/project",
            1_786_004_275,
            1_786_004_400,
            Some(5),
            0,
        );
        insert_node(
            &conn,
            1,
            "bald-ketch",
            None,
            &json!({"message_id": "m1", "role": "system", "content": "You are Devin.", "metadata": {}}),
            1_786_004_275,
        );
        insert_node(
            &conn,
            2,
            "bald-ketch",
            Some(1),
            &json!({"message_id": "m2", "role": "user", "content": "Fix the broken checks.",
                    "metadata": {"is_user_input": true}}),
            1_786_004_280,
        );
        insert_node(
            &conn,
            3,
            "bald-ketch",
            Some(2),
            &json!({"message_id": "m3", "role": "assistant", "content": "Reading the file.",
                    "thinking": {"signature": "sig", "thinking": "I should read it first."},
                    "tool_calls": [{"id": "call_1", "index": 0, "kind": "function",
                                    "name": "read_file", "arguments": {"path": "hello.py"}}],
                    "metadata": {"num_tokens": null}}),
            1_786_004_290,
        );
        insert_node(
            &conn,
            4,
            "bald-ketch",
            Some(3),
            &json!({"message_id": "m4", "role": "tool", "content": "print(\"hello\")",
                    "tool_call_id": "call_1", "metadata": {}}),
            1_786_004_300,
        );
        insert_node(
            &conn,
            5,
            "bald-ketch",
            Some(4),
            &json!({"message_id": "m5", "role": "assistant", "content": "The check passes now.",
                    "metadata": {}}),
            1_786_004_400,
        );
        // Abandoned branch (an edited/retried turn): must never be indexed.
        insert_node(
            &conn,
            6,
            "bald-ketch",
            Some(2),
            &json!({"message_id": "m6", "role": "assistant", "content": "ABANDONED BRANCH TEXT"}),
            1_786_004_285,
        );
        // A hidden (retired) session with a valid chain: excluded like `devin list`.
        insert_session(
            &conn,
            "hidden-one",
            "Old",
            "/tmp/old",
            1_700_000_000,
            1_700_000_100,
            Some(7),
            1,
        );
        insert_node(
            &conn,
            7,
            "hidden-one",
            None,
            &json!({"message_id": "h1", "role": "user", "content": "hidden prompt"}),
            1_700_000_000,
        );
        // A session with no live chain: nothing to index.
        insert_session(
            &conn,
            "no-chain",
            "Empty",
            "/tmp/e",
            1_700_000_000,
            1_700_000_100,
            None,
            0,
        );
        drop(conn);
    }

    #[test]
    fn extract_projects_the_main_chain_only() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        seed_reference_store(&db_path);

        let convs = DevinConnector::extract_from_sqlite(&db_path, None).unwrap();
        assert_eq!(convs.len(), 1, "hidden and chainless sessions are skipped");
        let conv = &convs[0];
        assert_eq!(conv.agent_slug, "devin");
        assert_eq!(conv.external_id.as_deref(), Some("bald-ketch"));
        assert_eq!(conv.title.as_deref(), Some("Fix the broken checks"));
        assert_eq!(
            conv.workspace.as_deref(),
            Some(Path::new("/tmp/as-agent-lab/devin/project"))
        );
        assert_eq!(conv.started_at, Some(1_786_004_275_000));
        assert_eq!(conv.ended_at, Some(1_786_004_400_000));
        assert_eq!(conv.source_path, db_path.join("bald-ketch"));
        assert_eq!(conv.metadata["model"], "swe-1-7");
        assert_eq!(conv.metadata["agent_mode"], "bypass");
        assert_eq!(conv.metadata["main_chain_nodes"], 5);
        assert_eq!(conv.metadata["off_chain_nodes"], 1);

        // system node dropped; four indexable turns in reading order.
        let roles: Vec<&str> = conv.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        let idxs: Vec<i64> = conv.messages.iter().map(|m| m.idx).collect();
        assert_eq!(idxs, [0, 1, 2, 3]);
        assert!(
            conv.messages
                .iter()
                .all(|m| !m.content.contains("ABANDONED")),
            "off-chain branch leaked into the transcript"
        );

        let user = &conv.messages[0];
        assert_eq!(user.content, "Fix the broken checks.");
        assert_eq!(user.created_at, Some(1_786_004_280_000));
        assert_eq!(user.extra["is_user_input"], true);
        assert_eq!(user.extra["message_id"], "m2");

        let assistant = &conv.messages[1];
        assert_eq!(assistant.author.as_deref(), Some("swe-1-7"));
        assert_eq!(
            assistant.content,
            "Reading the file.\n[tool_call: read_file]"
        );
        assert_eq!(assistant.extra["thinking"], "I should read it first.");
        assert_eq!(assistant.invocations.len(), 1);
        assert_eq!(assistant.invocations[0].kind, "tool");
        assert_eq!(assistant.invocations[0].name, "read_file");
        assert_eq!(assistant.invocations[0].call_id.as_deref(), Some("call_1"));
        assert_eq!(
            assistant.invocations[0].arguments,
            Some(json!({"path": "hello.py"}))
        );

        let tool = &conv.messages[2];
        assert_eq!(tool.content, "print(\"hello\")");
        assert_eq!(tool.extra["tool_call_id"], "call_1");
        assert_eq!(conv.messages[3].content, "The check passes now.");
    }

    // cass review 2026-11-02 F4: batched loader == reference loader.
    #[test]
    fn batched_main_chain_matches_reference_for_long_chains_with_branches() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        let conn = open_test_connection(&db_path);
        create_schema(&conn);
        insert_session(&conn, "long", "Long", "/tmp/p", 1, 2, Some(600), 0);
        for id in 1..=600_i64 {
            let parent = if id == 1 { None } else { Some(id - 1) };
            insert_node(
                &conn,
                id,
                "long",
                parent,
                &json!({"message_id": format!("m{id}"), "role": "user", "content": format!("turn {id}"), "metadata": {}}),
                1_786_000_000 + id,
            );
        }
        for id in 601..=800_i64 {
            insert_node(
                &conn,
                id,
                "long",
                Some(10),
                &json!({"message_id": format!("b{id}"), "role": "user", "content": "branch", "metadata": {}}),
                1_786_000_000 + id,
            );
        }
        let reference = DevinConnector::load_main_chain(&conn, "long", 600).unwrap();
        let batched = batched_chain::load_main_chain_batched(&conn, "long", 600).unwrap();
        assert_eq!(batched.off_chain_nodes, reference.off_chain_nodes);
        assert_eq!(batched.off_chain_nodes, 200);
        let pairs = |c: &DevinChain| {
            c.nodes
                .iter()
                .map(|n| (n.chat_message.clone(), n.created_at))
                .collect::<Vec<_>>()
        };
        assert_eq!(pairs(&batched), pairs(&reference));
        assert_eq!(batched.nodes.len(), 600);
    }

    #[test]
    fn extract_honors_since_ts_on_last_activity() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        seed_reference_store(&db_path);
        assert_eq!(
            DevinConnector::extract_from_sqlite(&db_path, Some(1_786_004_400_001))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            DevinConnector::extract_from_sqlite(&db_path, Some(1_786_004_400_000))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn extract_tolerates_a_store_without_the_hidden_column() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        let conn = open_test_connection(&db_path);
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, title TEXT, working_directory TEXT, model TEXT, \
             agent_mode TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER);
             CREATE TABLE message_nodes (node_id INTEGER, session_id TEXT, parent_node_id INTEGER, \
             chat_message TEXT, created_at INTEGER);",
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO sessions VALUES ('s', NULL, '', NULL, NULL, 10, 20, 1)",
            params![],
        )
        .unwrap();
        insert_node(
            &conn,
            1,
            "s",
            None,
            &json!({"role": "user", "content": "first line\nsecond line"}),
            10,
        );
        drop(conn);
        let convs = DevinConnector::extract_from_sqlite(&db_path, None).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].title.as_deref(),
            Some("first line"),
            "title falls back to the first user line"
        );
        assert!(
            convs[0].workspace.is_none(),
            "blank working_directory is no workspace"
        );
        assert_eq!(convs[0].messages[0].author, None);
    }

    #[test]
    fn walk_main_chain_stops_on_cycles_and_missing_parents() {
        let parents: HashMap<i64, Option<i64>> = HashMap::from([
            (1, None),
            (2, Some(1)),
            (3, Some(2)),
            (9, Some(9)),
            (10, Some(11)),
        ]);
        assert_eq!(walk_main_chain(&parents, 3), vec![1, 2, 3]);
        assert_eq!(
            walk_main_chain(&parents, 9),
            vec![9],
            "self-cycle terminates"
        );
        assert_eq!(
            walk_main_chain(&parents, 10),
            vec![10],
            "missing parent ends the chain"
        );
        assert!(
            walk_main_chain(&parents, 42).is_empty(),
            "unknown tip is no chain"
        );
        let cycle: HashMap<i64, Option<i64>> = HashMap::from([(1, Some(2)), (2, Some(1))]);
        assert_eq!(walk_main_chain(&cycle, 1), vec![2, 1]);
    }

    #[test]
    fn chat_message_parsing_covers_images_parts_and_empties() {
        let user_with_images = json!({"role": "user", "content": "look",
            "images": [{"width": 1, "height": 1, "base64_data": "AAAA"}, {"base64_data": "BBBB"}]});
        let message = message_from_chat_message(&user_with_images.to_string(), None, None).unwrap();
        assert_eq!(message.content, "[image]\n[image]\nlook");
        assert!(
            !message.content.contains("AAAA"),
            "base64 payload must not be indexed"
        );

        let parts = json!({"role": "assistant", "content": [
            {"type": "text", "text": "part one"},
            {"type": "image_url", "image_url": {"url": "data:..."}},
            {"type": "text", "text": "part two"}]});
        let message =
            message_from_chat_message(&parts.to_string(), Some(5_000), Some("m")).unwrap();
        assert_eq!(message.content, "part one\n[image]\npart two");
        assert_eq!(message.created_at, Some(5_000));

        let tool_with_image = json!({"role": "tool", "tool_call_id": "c", "content": "",
            "images": [{"base64_data": "CCCC"}]});
        let message = message_from_chat_message(&tool_with_image.to_string(), None, None).unwrap();
        assert_eq!(message.content, "[image]");

        assert!(
            message_from_chat_message(r#"{"role":"system","content":"x"}"#, None, None).is_none()
        );
        assert!(
            message_from_chat_message(r#"{"role":"assistant","content":""}"#, None, None).is_none()
        );
        assert!(message_from_chat_message(r#"{"role":"user"}"#, None, None).is_none());
        assert!(message_from_chat_message("not json", None, None).is_none());
        assert!(message_from_chat_message("[1,2]", None, None).is_none());

        let tool_call_only = json!({"role": "assistant", "content": null,
            "tool_calls": [{"id": "x", "name": "bash", "arguments": {"cmd": "ls"}}]});
        let message = message_from_chat_message(&tool_call_only.to_string(), None, None).unwrap();
        assert_eq!(message.content, "[tool_call: bash]");
        assert_eq!(message.invocations[0].arguments, Some(json!({"cmd": "ls"})));

        let huge = "x".repeat(MAX_TOOL_ARGUMENTS_BYTES + 1);
        let big_call = json!({"role": "assistant", "tool_calls": [{"name": "w", "arguments": {"blob": huge}}]});
        let message = message_from_chat_message(&big_call.to_string(), None, None).unwrap();
        let marker = message.invocations[0]
            .arguments
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap();
        assert!(
            marker.starts_with("[OMITTED large JSON payload bytes="),
            "{marker}"
        );
    }

    #[test]
    fn scan_and_discovery_resolve_the_store_from_a_scan_root() {
        let dir = TempDir::new().unwrap();
        let devin_root = dir.path().join("devin");
        let cli_dir = devin_root.join("cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        let db_path = cli_dir.join("sessions.db");
        seed_reference_store(&db_path);
        let connector = DevinConnector::new();

        // Explicit roots: the devin data root, the cli dir, and the db file
        // all resolve to the same store, deduplicated.
        let ctx = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![
                ScanRoot::local(devin_root.clone()),
                ScanRoot::local(cli_dir.clone()),
                ScanRoot::local(db_path.clone()),
                ScanRoot::local(dir.path().join("nothing-here")),
            ],
            None,
        );
        let convs = connector.scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        let sources = connector.discover_source_files(&ctx).unwrap();
        // The store's truth spans the db plus any -wal/-shm the engine left
        // behind, so discovery lists the whole bundle (the shelley contract):
        // the database first, then each present sidecar with its role.
        let wal_path = sidecar_path(&db_path, "-wal");
        let shm_path = sidecar_path(&db_path, "-shm");
        let expected = 1 + usize::from(wal_path.is_file()) + usize::from(shm_path.is_file());
        assert_eq!(sources.len(), expected);
        assert_eq!(sources[0].source_path, db_path);
        assert_eq!(sources[0].role, DiscoveredSourceRole::SqliteDatabase);
        assert_eq!(sources[0].scan_root, devin_root);
        if wal_path.is_file() {
            let wal = sources
                .iter()
                .find(|source| source.source_path == wal_path)
                .expect("present WAL sidecar must be discovered");
            assert_eq!(wal.role, DiscoveredSourceRole::MetadataSidecar);
            assert!(wal.required_for_reconstruction);
        }
        if shm_path.is_file() {
            let shm = sources
                .iter()
                .find(|source| source.source_path == shm_path)
                .expect("present SHM sidecar must be discovered");
            assert_eq!(shm.role, DiscoveredSourceRole::MetadataSidecar);
            assert!(!shm.required_for_reconstruction);
        }

        // Default detection with a store under data_dir scopes the scan to it.
        let scoped = ScanContext::local_default(devin_root.clone(), None);
        assert_eq!(connector.scan(&scoped).unwrap().len(), 1);
        let scoped_db = ScanContext::local_default(db_path.clone(), None);
        assert_eq!(connector.scan(&scoped_db).unwrap().len(), 1);

        // No store anywhere under the roots: empty, not an error.
        let empty = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![ScanRoot::local(dir.path().join("nothing-here"))],
            None,
        );
        assert!(connector.scan(&empty).unwrap().is_empty());
        assert!(connector.discover_source_files(&empty).unwrap().is_empty());
    }

    /// A WAL-only commit — how the live CLI appends a turn while holding the
    /// store open — leaves the primary file's size and mtime untouched, so a
    /// completion whose reuse evidence names only the primary authorizes the
    /// host's ledger to skip the store and the appended turn never reaches
    /// the index (found when cass moved to the merged 0.3.11 lineage; see
    /// cass agent_detection_completeness devin_ingestion). The completion
    /// must carry the -wal, exactly like the shelley connector's stores.
    #[test]
    fn source_boundary_completion_carries_the_live_wal() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        seed_reference_store(&db_path);
        // The live-CLI shape: a second connection holds the store in WAL
        // mode with committed-but-uncheckpointed frames.
        let writer = open_test_connection(&db_path);
        writer
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        insert_session(
            &writer,
            "live-turn",
            "Live turn",
            "/work/live",
            300,
            400,
            Some(9),
            0,
        );
        insert_node(
            &writer,
            9,
            "live-turn",
            None,
            &json!({"role": "user", "content": "appended while the CLI holds the store"}),
            350,
        );
        let wal_path = sidecar_path(&db_path, "-wal");
        assert!(wal_path.is_file(), "fixture must hold a live WAL");

        let connector = DevinConnector::new();
        let ctx = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![ScanRoot::local(db_path.clone())],
            None,
        );

        let mut completions: Vec<SourceCompletion> = Vec::new();
        let mut emitted = 0usize;
        {
            let mut on_complete = |completion: &SourceCompletion| {
                completions.push(completion.clone());
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut on_complete),
            };
            connector
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_conv| {
                    emitted += 1;
                    Ok(())
                })
                .unwrap();
        }

        // Both the seeded conversation and the WAL-only turn are readable
        // through the strict read-only open while the writer still holds it.
        assert_eq!(emitted, 2, "the WAL-only turn must be scanned");
        assert_eq!(completions.len(), 1, "one store => one completion");
        let completion = &completions[0];
        assert_eq!(completion.source.role, DiscoveredSourceRole::SqliteDatabase);
        assert_eq!(completion.source.source_path, db_path);
        assert_eq!(
            completion.required_sidecars.len(),
            1,
            "the -wal is the reuse evidence a WAL-only commit invalidates"
        );
        let wal = &completion.required_sidecars[0];
        assert_eq!(wal.source_path, wal_path);
        assert_eq!(wal.role, DiscoveredSourceRole::MetadataSidecar);
        assert!(wal.required_for_reconstruction);
        assert_eq!(wal.origin, completion.source.origin);
        assert_eq!(wal.scan_root, completion.source.scan_root);

        // Discovery lists the whole store bundle, so the host's pre-scan
        // dependency evidence covers the sidecars the completion names.
        let discovered = connector.discover_source_files(&ctx).unwrap();
        assert!(
            discovered
                .iter()
                .any(|source| source.role == DiscoveredSourceRole::SqliteDatabase
                    && source.source_path == db_path)
        );
        let discovered_wal = discovered
            .iter()
            .find(|source| source.source_path == wal_path)
            .expect("discovery must list the live -wal");
        assert_eq!(discovered_wal.role, DiscoveredSourceRole::MetadataSidecar);
        assert!(discovered_wal.required_for_reconstruction);

        drop(writer);
    }

    // cass review 2026-11-02 F5
    #[test]
    fn scan_reports_an_unreadable_database() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        std::fs::write(&db_path, b"this is not a sqlite database").unwrap();
        let ctx = crate::connectors::scan::ScanContext::with_roots(
            dir.path().to_path_buf(),
            vec![crate::connectors::scan::ScanRoot::local(db_path.clone())],
            None,
        );
        assert!(DevinConnector::new().scan(&ctx).is_err());
    }

    #[test]
    fn database_path_from_root_accepts_file_cli_dir_and_data_root() {
        let dir = TempDir::new().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join("sessions.db"), b"").unwrap();
        assert_eq!(
            DevinConnector::database_path_from_root(dir.path()),
            cli.join("sessions.db")
        );
        assert_eq!(
            DevinConnector::database_path_from_root(&cli),
            cli.join("sessions.db")
        );
        let file = dir.path().join("other.db");
        assert_eq!(DevinConnector::database_path_from_root(&file), file);
        let missing = dir.path().join("missing");
        assert_eq!(
            DevinConnector::database_path_from_root(&missing),
            missing.join("sessions.db")
        );
    }

    #[test]
    fn main_chain_session_is_proven_main() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("sessions.db");
        seed_reference_store(&db_path);
        let convs = DevinConnector::extract_from_sqlite(&db_path, None).unwrap();
        assert_eq!(convs.len(), 1);
        // The sessions-table main chain is provably a main session; the
        // subagent trees are a separate follow-up and never emitted here.
        assert_eq!(convs[0].metadata["is_subagent"], json!(false));
        assert!(convs[0].metadata.get("parent_id").is_none());
    }

    #[test]
    fn cli_subagent_subtrees_become_child_conversations() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("sessions.db");
        let conn = open_test_connection(&db);
        create_schema(&conn);
        insert_session(&conn, "parent-s", "Parent", "/w", 1_000, 2_000, Some(3), 0);
        insert_node(
            &conn,
            1,
            "parent-s",
            None,
            &json!({"role": "system", "content": "main"}),
            1_000,
        );
        insert_node(
            &conn,
            2,
            "parent-s",
            Some(1),
            &json!({"role": "user", "content": "spawn a helper"}),
            1_001,
        );
        insert_node(
            &conn,
            3,
            "parent-s",
            Some(2),
            &json!({"role": "assistant", "content": "spawned"}),
            1_002,
        );
        insert_node(
            &conn,
            10,
            "parent-s",
            None,
            &json!({"role": "system", "content": "You are a subagent of Devin. Investigate"}),
            1_003,
        );
        insert_node(
            &conn,
            11,
            "parent-s",
            Some(10),
            &json!({"role": "user", "content": "investigate the bug"}),
            1_004,
        );
        insert_node(
            &conn,
            12,
            "parent-s",
            Some(11),
            &json!({"role": "assistant", "content": "found it"}),
            1_005,
        );
        drop(conn);

        let convs = DevinConnector::extract_from_sqlite(&db, None).unwrap();
        let main = convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some("parent-s"))
            .unwrap();
        assert_eq!(main.metadata["is_subagent"], false);
        assert_eq!(main.messages.len(), 2);

        let sub = convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some("parent-s:agent-10"))
            .unwrap();
        assert_eq!(sub.metadata["is_subagent"], true);
        assert_eq!(sub.metadata["parent_id"], "parent-s");
        assert_eq!(sub.metadata["subagent_root_node"], 10);
        assert_eq!(sub.metadata["session_id"], "parent-s");
        assert_eq!(sub.metadata["source"], "sqlite");
        assert_eq!(sub.workspace.as_deref(), Some(Path::new("/w")));
        assert_eq!(sub.title.as_deref(), Some("investigate the bug"));
        assert_eq!(sub.started_at, Some(1_003_000));
        assert_eq!(sub.ended_at, Some(1_005_000));
        let texts: Vec<_> = sub.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, vec!["investigate the bug", "found it"]);
        assert_eq!(
            sub.messages.iter().map(|m| m.idx).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(sub.source_path.ends_with("parent-s:agent-10"));
        assert_eq!(convs.len(), 2);
    }

    #[test]
    fn cli_subagent_handles_legacy_schema_and_empty_or_gated_subtrees() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("sessions.db");
        let conn = open_test_connection(&db);
        // Legacy store without `row_id` or `hidden`.
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, title TEXT, working_directory TEXT, model TEXT, \
             agent_mode TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER);
             CREATE TABLE message_nodes (node_id INTEGER, session_id TEXT, parent_node_id INTEGER, \
             chat_message TEXT, created_at INTEGER);",
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO sessions VALUES ('s1', 'Main', '', NULL, NULL, 100, 200, 2)",
            params![],
        )
        .unwrap();
        insert_node(
            &conn,
            1,
            "s1",
            None,
            &json!({"role": "system", "content": "main"}),
            100,
        );
        insert_node(
            &conn,
            2,
            "s1",
            Some(1),
            &json!({"role": "user", "content": "hello"}),
            105,
        );
        // Valid subagent root without `row_id`.
        insert_node(
            &conn,
            10,
            "s1",
            None,
            &json!({"role": "system", "content": "You are a subagent of Devin"}),
            110,
        );
        insert_node(
            &conn,
            11,
            "s1",
            Some(10),
            &json!({"role": "user", "content": "legacy child"}),
            115,
        );
        // Childless marker root: ignored.
        insert_node(
            &conn,
            20,
            "s1",
            None,
            &json!({"role": "system", "content": "You are a subagent of Devin"}),
            120,
        );
        // Marker root with only a system child (no indexable messages): ignored.
        insert_node(
            &conn,
            30,
            "s1",
            None,
            &json!({"role": "system", "content": "You are a subagent of Devin"}),
            130,
        );
        insert_node(
            &conn,
            31,
            "s1",
            Some(30),
            &json!({"role": "system", "content": "still system"}),
            135,
        );
        drop(conn);

        let convs = DevinConnector::extract_from_sqlite(&db, None).unwrap();
        assert_eq!(convs.len(), 2);
        let sub = convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some("s1:agent-10"))
            .unwrap();
        assert_eq!(sub.messages.len(), 1);
        assert_eq!(sub.messages[0].content, "legacy child");
        assert!(sub.workspace.is_none());

        // `since_ts` newer than `last_activity_at` (200s -> 200_000ms) skips both main and subagents.
        let gated = DevinConnector::extract_from_sqlite(&db, Some(200_001)).unwrap();
        assert!(gated.is_empty());
    }
}

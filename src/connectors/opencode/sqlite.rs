//! `SQLite` (v1.2+ and 2.x) extraction and rendering for `OpenCode`.

#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::if_not_else,
    clippy::manual_let_else,
    clippy::manual_string_new,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::nonminimal_bool,
    clippy::option_if_let_else,
    clippy::single_option_map,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps,
    clippy::unreadable_literal
)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankensqlite::compat::{ParamValue, RowExt};
use frankensqlite::{Row, params};

use super::OpenCodeConnector;
use super::models::{
    LoadedPart, PartInfo, PendingSqliteMessage, SqliteMessageData, SqliteMessageRow,
    SqlitePartData, SqliteSession, SqliteV2MessageRow,
};
use super::parts::{
    assemble_content_from_parts, build_v1_message_extra, error_text_of,
    extract_invocations_from_parts, has_tool_part, normalize_opencode_timestamp,
    normalize_sqlite_ts_value, optional_sqlite_value, resolve_v1_model_id, resolve_v1_provider_id,
    sort_loaded_parts,
};
use crate::connectors::sqlite_sync::{Connection, ConnectionExt, open_read_only_with_snapshot_fallback};
use crate::connectors::unwrap_skill_invocations;
use crate::types::{NormalizedConversation, NormalizedInvocation, NormalizedMessage};

/// Max ids bound into a single `... IN (?, ?, …)` chunk. Kept well under
/// SQLite's default 999-parameter ceiling. (#372: incremental opencode scans
/// load only the changed sessions' messages/parts instead of the whole DB.)
const OPENCODE_SQL_IN_CHUNK: usize = 800;

/// Emit a scan liveness tick every N decoded rows during a large sqlite scan so
/// the host stall watchdog sees progress before the first conversation is
/// yielded (cass#373 Variant A). Frequent enough that a multi-minute full scan
/// never leaves a >120s silent gap; cheap enough to be noise otherwise.
const OPENCODE_SCAN_TICK_EVERY: usize = 2000;

/// Call `tick` (if present) once every `OPENCODE_SCAN_TICK_EVERY` rows.
#[inline]
fn opencode_scan_tick(tick: Option<&(dyn Fn() + Send + Sync)>, row_index: usize) {
    if row_index % OPENCODE_SCAN_TICK_EVERY == 0
        && let Some(tick) = tick
    {
        tick();
    }
}

impl OpenCodeConnector {
    /// Extract sessions from OpenCode's SQLite database (v1.2+).
    ///
    /// Schema: session(id, title, directory, project_id, time_created, time_updated),
    ///         message(id, session_id, data JSON), part(id, message_id, session_id, data JSON)
    pub(super) fn extract_from_sqlite(
        db_path: &Path,
        since_ts: Option<i64>,
        progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<Vec<NormalizedConversation>> {
        let conn = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to open OpenCode db: {}", db_path.display()))?;

        conn.execute("PRAGMA busy_timeout = 5000;")
            .with_context(|| "failed to set busy_timeout")?;

        // OpenCode 2.0.x keeps its sessions in `session_v2` and their transcripts in
        // `session_message`, next to the 1.x `session`/`message`/`part` tables; a
        // 1.x session migrated to 2.x keeps its id in both. Later 2.x builds keep
        // 2.x sessions in `session` itself, still with `session_message`
        // transcripts (cass GH #504). `session_v2` rows come first so a migrated
        // session keeps its 2.x metadata when `seen_ids` below drops the 1.x row.
        let has_session_v2 = sqlite_table_exists(&conn, "session_v2")?;
        let has_session_message = sqlite_table_exists(&conn, "session_message")?;
        let mut sessions: Vec<SqliteSession> = Vec::new();
        if has_session_v2 {
            sessions.extend(Self::load_sqlite_sessions(&conn, "session_v2")?);
        }
        // Without 2.x tables the 1.x `session` table is required, as before.
        if !has_session_v2 || sqlite_table_exists(&conn, "session")? {
            sessions.extend(Self::load_sqlite_sessions(&conn, "session")?);
        }
        let has_v1_messages = !has_session_message
            || (sqlite_table_exists(&conn, "message")? && sqlite_table_exists(&conn, "part")?);

        // #372: on an incremental scan, restrict the (expensive) message/part
        // decode to the sessions that could possibly clear the cutoff, instead
        // of decoding the whole DB and filtering afterwards. The predicate is
        // exactly the one the post-decode filter below applies via `ended_at`
        // (which prefers `session.time_updated`): keep a session iff its update
        // time is unknown — in which case the per-message time filter still
        // decides — or at/after the cutoff. Sessions whose known update time is
        // before the cutoff are dropped here without loading their messages or
        // parts, and the post-decode filter remains as a correctness backstop.
        let keep_session_ids: Option<HashSet<String>> = since_ts.map(|since| {
            sessions
                .iter()
                .filter(|session| {
                    session
                        .time_updated_raw
                        .as_ref()
                        .and_then(normalize_sqlite_ts_value)
                        // Same −1s mtime-granularity slack as
                        // file_modified_since, so a session touched within
                        // the slack window is not skipped on every
                        // subsequent incremental scan.
                        .is_none_or(|updated| updated >= since.saturating_sub(1_000))
                })
                .map(|session| session.id.clone())
                .collect()
        });

        let mut messages_by_session = if has_v1_messages {
            Self::load_sqlite_messages_by_session(&conn, keep_session_ids.as_ref(), progress_tick)?
        } else {
            HashMap::new()
        };
        let mut v2_messages_by_session = if has_session_message {
            Self::load_sqlite_v2_messages_by_session(
                &conn,
                keep_session_ids.as_ref(),
                progress_tick,
            )?
        } else {
            HashMap::new()
        };
        let mut convs = Vec::new();
        let mut seen_ids = HashSet::new();

        for (session_index, session) in sessions.into_iter().enumerate() {
            opencode_scan_tick(progress_tick, session_index);
            if !seen_ids.insert(session.id.clone()) {
                continue;
            }

            // A session with a 2.x transcript is read from it; the 1.x rows a
            // migration left behind for the same id are not indexed twice.
            let v1_messages = messages_by_session.remove(&session.id).unwrap_or_default();
            let (messages, schema) = match v2_messages_by_session.remove(&session.id) {
                Some(v2_messages) if !v2_messages.is_empty() => (v2_messages, "v2"),
                _ => (v1_messages, "v1"),
            };
            if messages.is_empty() {
                continue;
            }

            let msg_started_at = messages.iter().filter_map(|m| m.created_at).min();
            let msg_ended_at = messages.iter().filter_map(|m| m.created_at).max();

            let session_created_ms = session
                .time_created_raw
                .as_ref()
                .and_then(normalize_sqlite_ts_value);
            let session_updated_ms = session
                .time_updated_raw
                .as_ref()
                .and_then(normalize_sqlite_ts_value);

            let started_at = session_created_ms.or(msg_started_at);
            let ended_at = session_updated_ms.or(msg_ended_at).or(started_at);

            // Filter by since_ts in Rust (can't reliably filter in SQL when
            // timestamp column format is unknown). Sessions whose timestamps
            // are ALL unparseable must stay: pruning them on latest=0 would
            // drop them from every incremental scan forever (the flat-file
            // path guards this blind spot with mtimes; SQLite rows have no
            // such fallback). Only KNOWN-old sessions are pruned.
            if let Some(since) = since_ts
                && let Some(latest) = ended_at.or(started_at)
                && latest < since.saturating_sub(1_000)
            {
                continue;
            }

            let workspace = session.directory.map(PathBuf::from);
            let title = session.title.or_else(|| {
                messages
                    .first()
                    .and_then(|m| m.content.lines().next())
                    .map(|s| s.chars().take(100).collect())
            });

            convs.push(NormalizedConversation {
                agent_slug: "opencode".into(),
                external_id: Some(session.id.clone()),
                title,
                workspace,
                source_path: db_path.join(urlencoding::encode(&session.id).as_ref()),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "session_id": session.id,
                    "project_id": session.project_id,
                    "source": "sqlite",
                    "opencode_schema": schema,
                }),
                messages,
            });
        }

        Ok(convs)
    }

    /// Session rows from `table` (`session` or `session_v2`). Timestamps are read
    /// as raw SQLite values: Drizzle ORM may store them as ISO text
    /// (YYYY-MM-DD HH:MM:SS) or epoch integers depending on config, and we
    /// normalize in Rust rather than with strftime(), which breaks on integers.
    fn load_sqlite_sessions(conn: &Connection, table: &str) -> Result<Vec<SqliteSession>> {
        conn.query_map_collect(
            // `id IS NOT NULL`: SQLite PRIMARY KEYs do not imply NOT NULL, and
            // one NULL/BLOB id aborts the whole row collect — dropping every
            // conversation in the store.
            &format!(
                "SELECT id, title, directory, project_id, time_created, time_updated FROM {table} WHERE id IS NOT NULL"
            ),
            params![],
            |row| {
                Ok(SqliteSession {
                    id: row.get_typed(0)?,
                    title: row.get_typed(1)?,
                    directory: row.get_typed(2)?,
                    project_id: row.get_typed(3)?,
                    time_created_raw: optional_sqlite_value(row, 4),
                    time_updated_raw: optional_sqlite_value(row, 5),
                })
            },
        )
        .with_context(|| format!("failed to query OpenCode sessions from {table}"))
    }

    /// OpenCode 2.x transcripts from `session_message`, grouped by session and
    /// ordered by the per-session `seq` OpenCode itself pages by (cass GH #504).
    fn load_sqlite_v2_messages_by_session(
        conn: &Connection,
        keep_session_ids: Option<&HashSet<String>>,
        progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<HashMap<String, Vec<NormalizedMessage>>> {
        const SELECT: &str = "SELECT session_id, id, type, seq, data, time_created FROM session_message WHERE data IS NOT NULL AND session_id IS NOT NULL AND id IS NOT NULL AND type IS NOT NULL";
        let map_row = |row: &Row| -> Result<SqliteV2MessageRow, frankensqlite::FrankenError> {
            Ok(SqliteV2MessageRow {
                session_id: row.get_typed(0)?,
                id: row.get_typed(1)?,
                message_type: row.get_typed(2)?,
                seq: row.get_typed(3)?,
                data_json: row.get_typed(4)?,
                time_created_raw: optional_sqlite_value(row, 5),
            })
        };
        let rows: Vec<SqliteV2MessageRow> = match keep_session_ids {
            None => conn.query_map_collect(SELECT, params![], map_row)?,
            Some(ids) => {
                let id_list: Vec<&String> = ids.iter().collect();
                let mut rows = Vec::new();
                for chunk in id_list.chunks(OPENCODE_SQL_IN_CHUNK) {
                    let placeholders = vec!["?"; chunk.len()].join(",");
                    let sql = format!("{SELECT} AND session_id IN ({placeholders})");
                    let bind: Vec<ParamValue> = chunk
                        .iter()
                        .map(|id| ParamValue::from(id.as_str()))
                        .collect();
                    rows.extend(conn.query_map_collect(&sql, &bind, map_row)?);
                }
                rows
            }
        };

        let mut pending_by_session: HashMap<String, Vec<(Option<i64>, String, NormalizedMessage)>> =
            HashMap::new();
        for (row_index, row) in rows.into_iter().enumerate() {
            opencode_scan_tick(progress_tick, row_index);
            let data: serde_json::Value = match serde_json::from_str(&row.data_json) {
                Ok(data) => data,
                Err(e) => {
                    tracing::debug!(
                        "opencode sqlite: failed to parse session_message data for {}: {e}",
                        row.id
                    );
                    continue;
                }
            };
            let Some((role, author, content)) = v2_message_content(&row.message_type, &data) else {
                continue;
            };
            let invocations = v2_message_invocations(&row.message_type, &data);
            // F14: archive the raw content items so the host can verify the
            // part inventory (id, name, input, status, results, error) instead
            // of inferring it from the rendered text. Tool parts are
            // identity-bearing even when they render no text (file-only
            // results), so such a row survives with empty content — F13's
            // codex rule — instead of vanishing with its identity and state.
            let content_items = data
                .get("content")
                .and_then(serde_json::Value::as_array)
                .filter(|items| !items.is_empty())
                .cloned();
            let row_has_tool_part = content_items.as_deref().is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("tool")
                })
            });
            if content.trim().is_empty() && invocations.is_empty() && !row_has_tool_part {
                continue;
            }
            let col_ts = row
                .time_created_raw
                .as_ref()
                .and_then(normalize_sqlite_ts_value);
            let created_at = normalize_opencode_timestamp(
                data.pointer("/time/created")
                    .and_then(serde_json::Value::as_i64),
            )
            .or(col_ts);
            let tool_call_count = invocations.len();
            let mut extra = build_v2_message_extra(&row, &data, tool_call_count);
            if let Some(items) = content_items {
                extra["parts"] = serde_json::Value::Array(items);
            }
            let message = NormalizedMessage {
                idx: 0,
                role: role.to_string(),
                author,
                created_at,
                content,
                extra,
                invocations,
                snippets: Vec::new(),
            };
            pending_by_session
                .entry(row.session_id)
                .or_default()
                .push((row.seq, row.id, message));
        }

        let mut messages_by_session = HashMap::new();
        for (session_id, mut pending) in pending_by_session {
            pending.sort_by(|a, b| {
                a.0.unwrap_or(i64::MAX)
                    .cmp(&b.0.unwrap_or(i64::MAX))
                    .then_with(|| a.1.cmp(&b.1))
            });
            let mut messages: Vec<NormalizedMessage> =
                pending.into_iter().map(|(_, _, message)| message).collect();
            crate::types::reindex_messages(&mut messages);
            messages_by_session.insert(session_id, messages);
        }
        Ok(messages_by_session)
    }

    fn load_sqlite_messages_by_session(
        conn: &Connection,
        keep_session_ids: Option<&HashSet<String>>,
        progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<HashMap<String, Vec<NormalizedMessage>>> {
        // Load the message rows first (no ORDER BY — they are re-sorted per
        // session below), restricted to the kept sessions on an incremental
        // scan so old sessions' messages are never read or decoded (#372).
        let rows: Vec<SqliteMessageRow> = match keep_session_ids {
            None => conn.query_map_collect(
                // `data IS NOT NULL`: internal marker rows carry no payload
                // and would abort the whole collect (see the session query).
                "SELECT session_id, id, data, time_created FROM message WHERE data IS NOT NULL AND session_id IS NOT NULL AND id IS NOT NULL",
                params![],
                |row| {
                    Ok(SqliteMessageRow {
                        session_id: row.get_typed(0)?,
                        id: row.get_typed(1)?,
                        data_json: row.get_typed(2)?,
                        time_created_raw: optional_sqlite_value(row, 3),
                    })
                },
            )?,
            Some(ids) => {
                let id_list: Vec<&String> = ids.iter().collect();
                let mut rows: Vec<SqliteMessageRow> = Vec::new();
                for chunk in id_list.chunks(OPENCODE_SQL_IN_CHUNK) {
                    let placeholders = vec!["?"; chunk.len()].join(",");
                    let sql = format!(
                        "SELECT session_id, id, data, time_created FROM message WHERE data IS NOT NULL AND session_id IS NOT NULL AND id IS NOT NULL AND session_id IN ({placeholders})"
                    );
                    let bind: Vec<ParamValue> = chunk
                        .iter()
                        .map(|id| ParamValue::from(id.as_str()))
                        .collect();
                    rows.extend(conn.query_map_collect(&sql, &bind, |row| {
                        Ok(SqliteMessageRow {
                            session_id: row.get_typed(0)?,
                            id: row.get_typed(1)?,
                            data_json: row.get_typed(2)?,
                            time_created_raw: optional_sqlite_value(row, 3),
                        })
                    })?);
                }
                rows
            }
        };

        // Decode only the parts belonging to the messages we actually loaded.
        let kept_message_ids: Option<HashSet<String>> =
            keep_session_ids.map(|_| rows.iter().map(|row| row.id.clone()).collect());
        let mut parts_by_message =
            Self::load_sqlite_parts_by_message(conn, kept_message_ids.as_ref(), progress_tick)?;

        let mut pending_by_session: HashMap<String, Vec<PendingSqliteMessage>> = HashMap::new();

        for (row_index, row) in rows.into_iter().enumerate() {
            opencode_scan_tick(progress_tick, row_index);
            let msg_data: SqliteMessageData = match serde_json::from_str(&row.data_json) {
                Ok(d) => d,
                Err(e) => {
                    tracing::debug!(
                        "opencode sqlite: failed to parse message data for {}: {e}",
                        row.id
                    );
                    continue;
                }
            };

            let parts = parts_by_message.remove(&row.id).unwrap_or_default();
            let infos: Vec<PartInfo> = parts.iter().map(|part| part.info.clone()).collect();
            let invocations = extract_invocations_from_parts(&infos);
            let content_text = if !parts.is_empty() {
                assemble_content_from_parts(&infos)
            } else {
                String::new()
            };

            // F13's codex rule, applied to opencode tool parts: a tool part
            // is identity-bearing even when it renders no text (no output,
            // no error), so its message survives with empty content instead
            // of vanishing with the part's identity and state.
            if content_text.trim().is_empty() && invocations.is_empty() && !has_tool_part(&parts) {
                continue;
            }

            let role = msg_data.role.unwrap_or_else(|| "assistant".to_string());
            let col_ts = row
                .time_created_raw
                .as_ref()
                .and_then(normalize_sqlite_ts_value);
            let created_at =
                normalize_opencode_timestamp(msg_data.time.as_ref().and_then(|t| t.created))
                    .or(col_ts);

            let resolved_model_id =
                resolve_v1_model_id(msg_data.model_id.as_deref(), msg_data.model.as_ref());
            let resolved_provider_id =
                resolve_v1_provider_id(msg_data.provider_id.as_deref(), msg_data.model.as_ref());
            let author = if role == "assistant" {
                resolved_model_id.clone()
            } else {
                Some("user".to_string())
            };

            let message_id = row.id;
            let session_id = row.session_id;
            // F14: archive the raw parts in delivery order so the host can
            // verify the part inventory (identity, input, state) instead of
            // inferring it from the rendered text.
            let mut extra = build_v1_message_extra(
                &message_id,
                Some(&session_id),
                resolved_model_id.as_deref(),
                resolved_provider_id.as_deref(),
                msg_data.tokens.as_ref(),
                invocations.len(),
            );
            if !parts.is_empty() {
                extra["parts"] =
                    serde_json::Value::Array(parts.iter().map(|part| part.raw.clone()).collect());
            }
            pending_by_session
                .entry(session_id.clone())
                .or_default()
                .push(PendingSqliteMessage {
                    created_at,
                    message_id: message_id.clone(),
                    message: NormalizedMessage {
                        idx: 0,
                        role,
                        author,
                        created_at,
                        content: content_text,
                        extra,
                        invocations,
                        snippets: Vec::new(),
                    },
                });
        }

        let mut messages_by_session = HashMap::new();
        for (session_id, mut pending) in pending_by_session {
            pending.sort_by(|a, b| {
                let a_ts = a.created_at.unwrap_or(i64::MAX);
                let b_ts = b.created_at.unwrap_or(i64::MAX);
                a_ts.cmp(&b_ts)
                    .then_with(|| a.message_id.cmp(&b.message_id))
            });
            let mut messages: Vec<NormalizedMessage> =
                pending.into_iter().map(|pending| pending.message).collect();
            crate::types::reindex_messages(&mut messages);
            messages_by_session.insert(session_id, messages);
        }

        Ok(messages_by_session)
    }

    fn load_sqlite_parts_by_message(
        conn: &Connection,
        message_ids: Option<&HashSet<String>>,
        progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<HashMap<String, Vec<LoadedPart>>> {
        // No ORDER BY — parts are re-sorted per message below. On an incremental
        // scan only the kept messages' parts are read/decoded (#372: the `part`
        // table is the largest, so this is the dominant saving).
        let rows: Vec<(String, String)> = match message_ids {
            None => conn.query_map_collect(
                    "SELECT message_id, data FROM part WHERE data IS NOT NULL AND message_id IS NOT NULL",
                params![],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )?,
            Some(ids) => {
                let id_list: Vec<&String> = ids.iter().collect();
                let mut rows: Vec<(String, String)> = Vec::new();
                for chunk in id_list.chunks(OPENCODE_SQL_IN_CHUNK) {
                    let placeholders = vec!["?"; chunk.len()].join(",");
                    let sql = format!(
                        "SELECT message_id, data FROM part WHERE data IS NOT NULL AND message_id IS NOT NULL AND message_id IN ({placeholders})"
                    );
                    let bind: Vec<ParamValue> = chunk
                        .iter()
                        .map(|id| ParamValue::from(id.as_str()))
                        .collect();
                    rows.extend(conn.query_map_collect(&sql, &bind, |row| {
                        Ok((row.get_typed(0)?, row.get_typed(1)?))
                    })?);
                }
                rows
            }
        };

        let mut parts_by_message: HashMap<String, Vec<LoadedPart>> = HashMap::new();
        for (row_index, (message_id, row)) in rows.into_iter().enumerate() {
            opencode_scan_tick(progress_tick, row_index);
            // Keep the raw part JSON alongside the decoded fields (F14): the
            // tool state's identity (`state.id`), status, input, title and
            // metadata do not fit the decoded view but must survive verbatim.
            let raw = match serde_json::from_str::<serde_json::Value>(&row) {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::debug!("opencode sqlite: failed to parse part data: {e}");
                    continue;
                }
            };
            match serde_json::from_value::<SqlitePartData>(raw.clone()) {
                Ok(part_data) => {
                    parts_by_message
                        .entry(message_id)
                        .or_default()
                        .push(LoadedPart {
                            info: PartInfo {
                                id: part_data.id,
                                index: part_data.index,
                                message_id: None,
                                part_type: part_data.part_type,
                                text: part_data.text,
                                tool: part_data.tool,
                                call_id: part_data.call_id,
                                state: part_data.state,
                            },
                            raw,
                        });
                }
                Err(e) => {
                    tracing::debug!("opencode sqlite: failed to parse part data: {e}");
                }
            }
        }

        for parts in parts_by_message.values_mut() {
            sort_loaded_parts(parts);
        }

        Ok(parts_by_message)
    }
}

/// Whether `table` exists: `PRAGMA table_info` returns no rows for a missing table.
fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let columns = conn
        .query_map_collect(
            &format!("PRAGMA table_info(\"{table}\")"),
            params![],
            |_row| Ok(()),
        )
        .with_context(|| format!("failed to check for OpenCode table {table}"))?;
    Ok(!columns.is_empty())
}

/// Role, author and indexable text of one OpenCode 2.x `session_message` row,
/// rendered like the 1.x parts. `data` is the message without its `type` and
/// `id` (OpenCode's `SessionMessageData`). `None` for rows that carry no
/// transcript text: `idle`, the `agent-`/`model-`/`location-switched` selection
/// events, and compactions that did not complete.
pub(super) fn v2_message_content(
    message_type: &str,
    data: &serde_json::Value,
) -> Option<(&'static str, Option<String>, String)> {
    let field = |key: &str| {
        data.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    let model = || {
        data.pointer("/model/id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    match message_type {
        // `synthetic` is model-facing text added on the user's side of the turn;
        // 1.x kept the same text as synthetic text parts of the user message.
        "user" | "synthetic" => Some(("user", Some("user".to_string()), field("text").to_string())),
        "system" => Some(("system", None, field("text").to_string())),
        "skill" => {
            let text = field("text");
            let content = if text.trim().is_empty() {
                String::new()
            } else {
                format!("[Skill: {}]\n{text}", field("name"))
            };
            Some(("tool", None, content))
        }
        "shell" => {
            let command = field("command");
            let output = data
                .pointer("/output/output")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let content = match (command.trim().is_empty(), output.trim().is_empty()) {
                (true, true) => String::new(),
                (_, true) => format!("$ {command}"),
                _ => format!("$ {command}\n{output}"),
            };
            Some(("tool", None, content))
        }
        "assistant" => Some(("assistant", model(), assemble_v2_assistant_content(data))),
        "compaction"
            if data.get("status").and_then(serde_json::Value::as_str) == Some("completed") =>
        {
            let summary = field("summary");
            let content = if summary.trim().is_empty() {
                String::new()
            } else {
                format!("[Compaction Summary]\n{summary}")
            };
            Some(("assistant", model(), content))
        }
        _ => None,
    }
}

/// An OpenCode 2.x assistant message's `content` items rendered like the 1.x
/// parts: text as-is, reasoning as `[Reasoning]`, and a tool call's text
/// results as `[Tool Output]` (file results carry no text).
fn assemble_v2_assistant_content(data: &serde_json::Value) -> String {
    let mut pieces: Vec<String> = Vec::new();
    let items = data
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for item in items {
        let text = item
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("text") if !text.trim().is_empty() => pieces.push(text.to_string()),
            Some("reasoning") if !text.trim().is_empty() => {
                pieces.push(format!("[Reasoning]\n{text}"));
            }
            Some("tool") => {
                let output: Vec<&str> = item
                    .pointer("/state/content")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter(|result| {
                        result.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .filter_map(|result| result.get("text").and_then(serde_json::Value::as_str))
                    .collect();
                let output = output.join("\n");
                if !output.trim().is_empty() {
                    pieces.push(format!("[Tool Output]\n{output}"));
                }
                // A failed run's `state.error` is source text — render it
                // instead of dropping it (F14): a plain string, or an
                // object carrying the text under `message` (the measured
                // real shape is `{message, type}`).
                let error = error_text_of(item.pointer("/state/error")).unwrap_or("");
                if !error.trim().is_empty() {
                    pieces.push(format!("[Tool Error]\n{error}"));
                }
            }
            _ => {}
        }
    }
    pieces.join("\n\n")
}

pub(super) fn v2_message_invocations(
    message_type: &str,
    data: &serde_json::Value,
) -> Vec<NormalizedInvocation> {
    let mut invocations = Vec::new();
    match message_type {
        "assistant" => {
            if let Some(items) = data.get("content").and_then(serde_json::Value::as_array) {
                for item in items {
                    if item.get("type").and_then(serde_json::Value::as_str) != Some("tool") {
                        continue;
                    }
                    let Some(name) = item
                        .get("name")
                        .or_else(|| item.get("tool"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let call_id = item
                        .get("id")
                        .or_else(|| item.get("callID"))
                        .or_else(|| item.get("callId"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let arguments = item
                        .pointer("/state/input")
                        .or_else(|| item.get("input"))
                        .or_else(|| item.get("arguments"))
                        .filter(|v| !v.is_null())
                        .cloned();
                    invocations.push(NormalizedInvocation {
                        kind: "tool".to_string(),
                        name: name.to_string(),
                        raw_name: None,
                        call_id,
                        arguments,
                    });
                }
            }
        }
        "skill" => {
            if let Some(name) = data
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let call_id = data
                    .get("skill")
                    .or_else(|| data.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                invocations.push(NormalizedInvocation {
                    kind: "skill".to_string(),
                    name: name.to_string(),
                    raw_name: None,
                    call_id,
                    arguments: None,
                });
            }
        }
        "shell" => {
            if let Some(command) = data
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                invocations.push(NormalizedInvocation {
                    kind: "tool".to_string(),
                    name: "shell".to_string(),
                    raw_name: None,
                    call_id: None,
                    arguments: Some(serde_json::json!({ "command": command })),
                });
            }
        }
        _ => {}
    }
    unwrap_skill_invocations(&mut invocations);
    invocations
}

fn build_v2_message_extra(
    row: &SqliteV2MessageRow,
    data: &serde_json::Value,
    tool_call_count: usize,
) -> serde_json::Value {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "message_id".to_string(),
        serde_json::Value::String(row.id.clone()),
    );
    extra.insert(
        "session_id".to_string(),
        serde_json::Value::String(row.session_id.clone()),
    );
    extra.insert(
        "type".to_string(),
        serde_json::Value::String(row.message_type.clone()),
    );
    extra.insert(
        "seq".to_string(),
        row.seq
            .map_or(serde_json::Value::Null, serde_json::Value::from),
    );
    if let Some(model_id) = data
        .pointer("/model/id")
        .or_else(|| data.pointer("/model/modelID"))
        .or_else(|| data.get("modelID"))
        .or_else(|| data.get("model"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        extra.insert(
            "modelID".to_string(),
            serde_json::Value::String(model_id.to_string()),
        );
    }
    if let Some(provider_id) = data
        .pointer("/model/providerID")
        .or_else(|| data.pointer("/model/provider"))
        .or_else(|| data.get("providerID"))
        .or_else(|| data.get("provider"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        extra.insert(
            "providerID".to_string(),
            serde_json::Value::String(provider_id.to_string()),
        );
    }
    if let Some(tokens) = data
        .get("tokens")
        .or_else(|| data.get("usage"))
        .filter(|v| v.is_object())
    {
        extra.insert("tokens".to_string(), tokens.clone());
    }
    if tool_call_count > 0 {
        extra.insert(
            "cass".to_string(),
            serde_json::json!({ "tool_call_count": tool_call_count }),
        );
    }
    serde_json::Value::Object(extra)
}

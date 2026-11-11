//! Hermes Agent connector for SQLite session storage.
//!
//! [Hermes Agent](https://github.com/NousResearch/hermes-agent) is a
//! multi-platform coding agent (CLI, Telegram, Discord, Slack, WhatsApp,
//! Signal, Matrix, etc.) that stores sessions in SQLite.
//!
//! Database location: `$HERMES_HOME/state.db` (defaults to `~/.hermes/state.db`).
//!
//! Schema:
//!   `sessions`:  id (TEXT PK), source, model, title, parent_session_id,
//!                started_at (REAL), ended_at (REAL), end_reason,
//!                message_count, tool_call_count, input_tokens, output_tokens
//!   `messages`:  session_id (FK), role, content, tool_calls (JSON),
//!                tool_name, tool_call_id, reasoning, timestamp (REAL)

#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::missing_const_for_fn
)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankensqlite::compat::RowExt;
use frankensqlite::params;

use super::sqlite_sync::{Connection, ConnectionExt, open_read_only_with_snapshot_fallback};

use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{
    env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded, unwrap_skill_invocations,
};
use super::{Connector, franken_detection_for_connector};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
};

/// The `-wal`/`-shm` sibling of a store file (`state.db` ->
/// `state.db-wal`). A live store's truth spans the whole bundle, and a
/// WAL-only commit never touches the primary file.
fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db
        .file_name()
        .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
    name.push_str(suffix);
    db.with_file_name(name)
}

pub struct HermesConnector;

impl Default for HermesConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl HermesConnector {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Locate the Hermes SQLite database.
    ///
    /// Resolution order:
    /// 1. `HERMES_SQLITE_DB` env — explicit path to the database file
    /// 2. `HERMES_HOME` env — `$HERMES_HOME/state.db`
    /// 3. Default: `~/.hermes/state.db`
    fn sqlite_db_path() -> Option<PathBuf> {
        if let Some(p) = env_path_nonempty("HERMES_SQLITE_DB") {
            if p.is_file() {
                return Some(p);
            }
        }

        if let Ok(home) = dotenvy::var("HERMES_HOME") {
            let home = home.trim().to_string();
            if !home.is_empty() {
                let db = PathBuf::from(&home).join("state.db");
                if db.is_file() {
                    return Some(db);
                }
            }
        }

        if let Some(home) = dirs::home_dir() {
            let db = home.join(".hermes").join("state.db");
            if db.is_file() {
                return Some(db);
            }
        }

        None
    }

    fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut candidates: Vec<(ScanRoot, PathBuf)> = Vec::new();
        if ctx.data_dir.extension().is_some_and(|ext| ext == "db") {
            let root = ScanRoot::local(ctx.data_dir.clone());
            candidates.push((root, ctx.data_dir.clone()));
        } else if ctx.use_default_detection() {
            // Mirrors scan(): a state.db under data_dir scopes discovery to
            // it; only otherwise probe the system store.
            // HERMES_SQLITE_DB keeps precedence.
            let candidate = (!ctx.data_dir.as_os_str().is_empty())
                .then(|| ctx.data_dir.join("state.db"))
                .filter(|p| p.is_file());
            if let Some(db) = candidate.filter(|_| env_path_nonempty("HERMES_SQLITE_DB").is_none())
            {
                candidates.push((ScanRoot::local(db.clone()), db));
            } else if let Some(db) = Self::sqlite_db_path() {
                let root = ScanRoot::local(db.clone());
                candidates.push((root, db));
            }
        } else {
            if !ctx.data_dir.as_os_str().is_empty() {
                let root = ScanRoot::local(ctx.data_dir.clone());
                let db_path = root.path.join("state.db");
                candidates.push((root, db_path));
            }
            for scan_root in &ctx.scan_roots {
                if scan_root.path.extension().is_some_and(|ext| ext == "db") {
                    candidates.push((scan_root.clone(), scan_root.path.clone()));
                }
                if let Some(mirrored) = scan_root.mirrored_store(&[".hermes"]) {
                    candidates.push((scan_root.clone(), mirrored.join("state.db")));
                }
                candidates.push((scan_root.clone(), scan_root.path.join("state.db")));
                candidates.push((scan_root.clone(), scan_root.path.join(".hermes/state.db")));
            }
        }

        let mut seen = HashSet::new();
        Ok(candidates
            .into_iter()
            .filter(|(root, db_path)| {
                db_path.is_file()
                    && !path_is_excluded(&root.path, &excluded_paths)
                    && !path_is_excluded(db_path, &excluded_paths)
            })
            .filter(|(_, db_path)| seen.insert(db_path.clone()))
            .flat_map(|(root, db_path)| {
                let mut sources = vec![
                    DiscoveredSourceFile::new(
                        "hermes",
                        &root,
                        db_path.clone(),
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
                    let sidecar = sidecar_path(&db_path, suffix);
                    if sidecar.is_file() {
                        sources.push(
                            DiscoveredSourceFile::new(
                                "hermes",
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

    /// Extract sessions from the Hermes SQLite database.
    fn extract_from_sqlite(
        db_path: &Path,
        since_ts: Option<i64>,
    ) -> Result<Vec<NormalizedConversation>> {
        let conn = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to open Hermes db: {}", db_path.display()))?;

        conn.execute("PRAGMA busy_timeout = 5000;")
            .with_context(|| "failed to set busy_timeout")?;

        let sessions: Vec<HermesSession> = conn
            .query_map_collect(
                "SELECT id, source, model, title, parent_session_id, \
                        started_at, ended_at, end_reason, \
                        message_count, tool_call_count, input_tokens, output_tokens \
                 FROM sessions",
                params![],
                |row| {
                    Ok(HermesSession {
                        id: row.get_typed(0)?,
                        source: row.get_typed(1)?,
                        model: row.get_typed(2)?,
                        title: row.get_typed(3)?,
                        parent_session_id: row.get_typed(4)?,
                        started_at: row.get_typed(5)?,
                        ended_at: row.get_typed(6)?,
                        end_reason: row.get_typed(7)?,
                        message_count: row.get_typed(8)?,
                        tool_call_count: row.get_typed(9)?,
                        input_tokens: row.get_typed(10)?,
                        output_tokens: row.get_typed(11)?,
                    })
                },
            )
            .with_context(|| "failed to query Hermes sessions")?;

        let mut convs = Vec::new();
        let mut seen_ids = HashSet::new();

        for session in sessions {
            if !seen_ids.insert(session.id.clone()) {
                continue;
            }

            let started_ms = session.started_at.and_then(real_to_millis);
            let ended_ms = session.ended_at.and_then(real_to_millis);

            if let Some(since) = since_ts {
                let latest = ended_ms.or(started_ms).unwrap_or(0);
                if latest < since {
                    continue;
                }
            }

            let messages = Self::load_messages(&conn, &session.id, session.model.as_deref())?;
            if messages.is_empty() {
                continue;
            }

            let msg_started = messages.iter().filter_map(|m| m.created_at).min();
            let msg_ended = messages.iter().filter_map(|m| m.created_at).max();

            let started_at = started_ms.or(msg_started);
            let ended_at = ended_ms.or(msg_ended).or(started_at);

            // Infer workspace from tool calls (terminal workdir, file paths)
            let workspace = Self::infer_workspace(&messages);

            let title = session.title.or_else(|| {
                messages
                    .first()
                    .filter(|m| m.role == "user")
                    .and_then(|m| m.content.lines().next())
                    .map(|s| s.chars().take(100).collect())
            });

            convs.push(NormalizedConversation {
                agent_slug: "hermes".into(),
                external_id: Some(session.id.clone()),
                title,
                workspace,
                source_path: db_path.join(urlencoding::encode(&session.id).as_ref()),
                started_at,
                ended_at,
                metadata: serde_json::json!({
                    "session_id": session.id,
                    "source": session.source,
                    "model": session.model,
                    "parent_session_id": session.parent_session_id,
                    "end_reason": session.end_reason,
                    "message_count": session.message_count,
                    "tool_call_count": session.tool_call_count,
                    "input_tokens": session.input_tokens,
                    "output_tokens": session.output_tokens,
                }),
                messages,
            });
        }

        Ok(convs)
    }

    /// Load and normalize messages for a session.
    fn load_messages(
        conn: &Connection,
        session_id: &str,
        session_model: Option<&str>,
    ) -> Result<Vec<NormalizedMessage>> {
        let rows: Vec<HermesMessage> = conn.query_map_collect(
            "SELECT role, content, tool_calls, tool_name, tool_call_id, reasoning, timestamp \
             FROM messages WHERE session_id = ? ORDER BY timestamp ASC",
            params![session_id],
            |row| {
                Ok(HermesMessage {
                    role: row.get_typed(0)?,
                    content: row.get_typed(1)?,
                    tool_calls: row.get_typed(2)?,
                    tool_name: row.get_typed(3)?,
                    tool_call_id: row.get_typed(4)?,
                    reasoning: row.get_typed(5)?,
                    timestamp: row.get_typed(6)?,
                })
            },
        )?;

        let mut messages = Vec::new();

        for row in rows {
            let role = row.role.unwrap_or_else(|| "assistant".to_string());

            // Skip session_meta records — they contain metadata, not conversation content
            if role == "session_meta" {
                continue;
            }

            let content = row.content.unwrap_or_default();
            let reasoning = row.reasoning.unwrap_or_default();

            // Combine content and reasoning if both present
            let full_content = if reasoning.is_empty() {
                content.clone()
            } else if content.is_empty() {
                reasoning.clone()
            } else {
                format!("{content}\n\n[reasoning]\n{reasoning}")
            };

            if full_content.trim().is_empty() && row.tool_calls.is_none() {
                continue;
            }

            let created_at = row.timestamp.and_then(real_to_millis);
            let invocations = parse_tool_calls(row.tool_calls.as_deref());

            let author = if role == "assistant" {
                session_model
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            } else if role == "tool" {
                row.tool_name.clone()
            } else {
                Some(role.clone())
            };

            let mut extra = serde_json::json!({});
            if role == "assistant"
                && let Some(ref model) = author
            {
                extra["model"] = serde_json::json!(model);
            }
            if !invocations.is_empty() {
                extra["cass"] = serde_json::json!({ "tool_call_count": invocations.len() });
            }
            if let Some(ref name) = row.tool_name {
                extra["tool_name"] = serde_json::json!(name);
            }
            if let Some(ref call_id) = row.tool_call_id {
                extra["tool_call_id"] = serde_json::json!(call_id);
            }
            if !reasoning.is_empty() {
                extra["has_reasoning"] = serde_json::json!(true);
            }

            messages.push(NormalizedMessage {
                idx: 0,
                role,
                author,
                created_at,
                content: full_content,
                extra,
                invocations,
                snippets: Vec::new(),
            });
        }

        crate::types::reindex_messages(&mut messages);
        Ok(messages)
    }

    /// Infer workspace directory from tool call content.
    ///
    /// Hermes doesn't store workspace explicitly. We look for:
    /// - `terminal` tool calls with `workdir` parameter
    /// - `read_file` / `write_file` / `patch` tool calls with file paths
    fn infer_workspace(messages: &[NormalizedMessage]) -> Option<PathBuf> {
        for msg in messages {
            for inv in &msg.invocations {
                if let Some(args) = &inv.arguments {
                    // terminal tool: look for workdir
                    if inv.name == "terminal" {
                        if let Some(wd) = args.get("workdir").and_then(|v| v.as_str()) {
                            return Some(PathBuf::from(wd));
                        }
                    }
                    // file tools: extract directory from file path
                    if matches!(inv.name.as_str(), "read_file" | "write_file" | "patch") {
                        if let Some(path) = args
                            .get("path")
                            .or_else(|| args.get("file_path"))
                            .and_then(|v| v.as_str())
                        {
                            let p = Path::new(path);
                            if p.is_absolute() {
                                return p.parent().map(Path::to_path_buf);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

impl Connector for HermesConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("hermes").unwrap_or_else(DetectionResult::not_found)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(ctx, &mut SourceScanHooks::default(), on_conversation)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let sources = Self::discover_sources(ctx)?;
        // Local fix (cass review 2026-11-07, F9): a store that cannot be
        // read fails the scan instead of hiding behind a debug log (F5
        // semantics) — a BusyRecovery-class store is read through the
        // snapshot fallback in `open_read_only_with_snapshot_fallback`,
        // and every other open failure reaches the host; a valid but empty
        // store is "nothing to read" (the F9 empty-store rule).
        let mut failures = super::checked_scan::ScanFailures::new("Hermes");
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
            ctx.tick();
            if super::checked_scan::skip_provably_empty_store("Hermes", &source.source_path) {
                continue;
            }
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
            match Self::extract_from_sqlite(&source.source_path, ctx.since_ts) {
                Ok(db_convs) => {
                    tracing::debug!(
                        "hermes sqlite: found {} sessions in {}",
                        db_convs.len(),
                        source.source_path.display()
                    );
                    let mut conversations_emitted = 0usize;
                    for mut conv in db_convs {
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
                Err(e) => failures.record(&source.source_path, &e),
            }
        }
        failures.finish()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        self.scan_with_callback(ctx, &mut |conv| {
            convs.push(conv);
            Ok(())
        })?;
        Ok(convs)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Self::discover_sources(ctx)
    }
}

// ---------------------------------------------------------------------------
// Private types
// ---------------------------------------------------------------------------

struct HermesSession {
    id: String,
    source: Option<String>,
    model: Option<String>,
    title: Option<String>,
    parent_session_id: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    end_reason: Option<String>,
    message_count: Option<i64>,
    tool_call_count: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

struct HermesMessage {
    role: Option<String>,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    reasoning: Option<String>,
    timestamp: Option<f64>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a REAL unix timestamp (seconds, possibly fractional) to milliseconds.
fn real_to_millis(val: f64) -> Option<i64> {
    if val.is_nan() || val.is_infinite() || val <= 0.0 {
        return None;
    }
    // Hermes timestamps are REAL unix seconds (e.g. 1700000000.123)
    if (1_000_000_000.0..10_000_000_000.0).contains(&val) {
        Some((val * 1000.0) as i64)
    } else if val >= 10_000_000_000.0 {
        // Already milliseconds
        Some(val as i64)
    } else {
        None
    }
}

/// Parse the `tool_calls` JSON column into normalized invocations.
///
/// Hermes stores tool calls as a JSON array of `{name, arguments}` objects.
fn parse_tool_calls(json_str: Option<&str>) -> Vec<NormalizedInvocation> {
    let Some(json_str) = json_str else {
        return Vec::new();
    };
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) else {
        return Vec::new();
    };

    let mut invocations: Vec<_> = arr
        .into_iter()
        .filter_map(|obj| {
            let name = obj.get("name")?.as_str()?.to_string();
            let arguments = obj.get("arguments").cloned();
            Some(NormalizedInvocation {
                kind: "tool".into(),
                name,
                raw_name: None,
                call_id: obj.get("id").and_then(|v| v.as_str()).map(String::from),
                arguments,
            })
        })
        .collect();
    unwrap_skill_invocations(&mut invocations);
    invocations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_detection_scopes_sqlite_to_data_dir() {
        // A state.db under data_dir must scope the default-detection scan
        // to THIS store; the machine's real Hermes store must not leak
        // additional conversations into the result.
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT,
                model TEXT,
                title TEXT,
                parent_session_id TEXT,
                started_at REAL,
                ended_at REAL,
                end_reason TEXT,
                message_count INTEGER,
                tool_call_count INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER
            );
            CREATE TABLE messages (
                session_id TEXT,
                role TEXT,
                content TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                tool_call_id TEXT,
                reasoning TEXT,
                timestamp REAL
            );",
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO sessions (id, source, model, title, parent_session_id, started_at, ended_at, end_reason, message_count, tool_call_count, input_tokens, output_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                "sess-scope",
                "cli",
                "hermes-model",
                "Scoped session",
                Option::<String>::None,
                1_700_000_000.0_f64,
                1_700_000_100.0_f64,
                "completed",
                1_i64,
                0_i64,
                10_i64,
                5_i64
            ],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_name, tool_call_id, reasoning, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "sess-scope",
                "user",
                "Hello Hermes",
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                1_700_000_010.0_f64
            ],
        )
        .unwrap();
        drop(conn);

        let convs = HermesConnector
            .scan(&ScanContext::local_default(tmp.path().to_path_buf(), None))
            .unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].external_id.as_deref(), Some("sess-scope"));
        // Hermes derives per-session source paths as <db>/<external_id>.
        assert_eq!(convs[0].source_path, db_path.join("sess-scope"));
    }
    #[test]
    fn real_to_millis_converts_epoch_seconds() {
        assert_eq!(real_to_millis(1_700_000_000.0), Some(1_700_000_000_000));
        assert_eq!(real_to_millis(1_700_000_000.123), Some(1_700_000_000_123));
    }

    #[test]
    fn real_to_millis_passes_through_epoch_millis() {
        assert_eq!(real_to_millis(1_700_000_000_000.0), Some(1_700_000_000_000));
    }

    #[test]
    fn real_to_millis_rejects_invalid() {
        assert_eq!(real_to_millis(f64::NAN), None);
        assert_eq!(real_to_millis(f64::INFINITY), None);
        assert_eq!(real_to_millis(0.0), None);
        assert_eq!(real_to_millis(-1.0), None);
    }

    #[test]
    fn parse_tool_calls_extracts_invocations() {
        let json = r#"[{"name":"terminal","arguments":{"command":"ls","workdir":"/tmp"}},{"name":"read_file","arguments":{"path":"/etc/hosts"}}]"#;
        let invocations = parse_tool_calls(Some(json));
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].name, "terminal");
        assert_eq!(invocations[1].name, "read_file");
    }

    #[test]
    fn parse_tool_calls_handles_empty_and_null() {
        assert!(parse_tool_calls(None).is_empty());
        assert!(parse_tool_calls(Some("[]")).is_empty());
        assert!(parse_tool_calls(Some("not json")).is_empty());
    }

    #[test]
    fn infer_workspace_from_terminal_workdir() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "assistant".into(),
            author: None,
            created_at: None,
            content: String::new(),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
            invocations: vec![NormalizedInvocation {
                kind: "tool".into(),
                name: "terminal".into(),
                raw_name: None,
                call_id: None,
                arguments: Some(
                    serde_json::json!({"command": "ls", "workdir": "/home/user/project"}),
                ),
            }],
        }];
        assert_eq!(
            HermesConnector::infer_workspace(&messages),
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn infer_workspace_from_file_path() {
        let messages = vec![NormalizedMessage {
            idx: 0,
            role: "assistant".into(),
            author: None,
            created_at: None,
            content: String::new(),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
            invocations: vec![NormalizedInvocation {
                kind: "tool".into(),
                name: "write_file".into(),
                raw_name: None,
                call_id: None,
                arguments: Some(serde_json::json!({"path": "/home/user/project/src/main.rs"})),
            }],
        }];
        assert_eq!(
            HermesConnector::infer_workspace(&messages),
            Some(PathBuf::from("/home/user/project/src"))
        );
    }

    #[test]
    fn discover_source_files_includes_state_db_candidates() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let hermes_root = temp.path().join("hermes-home");
        let nested_root = temp.path().join("nested-home");
        std::fs::create_dir_all(&hermes_root).expect("hermes root");
        std::fs::create_dir_all(nested_root.join(".hermes")).expect("nested hermes root");
        let direct_db = hermes_root.join("state.db");
        let nested_db = nested_root.join(".hermes/state.db");
        std::fs::write(&direct_db, b"not a real sqlite db").expect("direct db");
        std::fs::write(&nested_db, b"not a real sqlite db").expect("nested db");

        let roots = vec![
            ScanRoot::local(hermes_root.clone()),
            ScanRoot::local(nested_root.clone()),
        ];
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), roots, None);
        let sources = HermesConnector::new()
            .discover_source_files(&ctx)
            .expect("source discovery");
        let mut paths = sources
            .iter()
            .map(|source| source.source_path.clone())
            .collect::<Vec<_>>();
        paths.sort();

        assert_eq!(paths, vec![direct_db, nested_db]);
        assert!(sources.iter().all(|source| {
            source.provider_slug == "hermes"
                && source.role == DiscoveredSourceRole::SqliteDatabase
                && source.required_for_reconstruction
        }));
    }

    fn create_hermes_schema(conn: &Connection) {
        conn.execute(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT,
                model TEXT,
                title TEXT,
                parent_session_id TEXT,
                started_at REAL,
                ended_at REAL,
                end_reason TEXT,
                message_count INTEGER,
                tool_call_count INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER
            );
            CREATE TABLE messages (
                session_id TEXT,
                role TEXT,
                content TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                tool_call_id TEXT,
                reasoning TEXT,
                timestamp REAL
            );",
        )
        .unwrap();
    }

    /// One committed session (one user turn) in the store's own schema.
    fn insert_hermes_turn(conn: &Connection, session_id: &str, text: &str, timestamp: f64) {
        conn.execute_compat(
            "INSERT INTO sessions (id, source, model, title, parent_session_id, started_at, ended_at, end_reason, message_count, tool_call_count, input_tokens, output_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                session_id,
                "cli",
                "hermes-model",
                "Hermes turn",
                Option::<String>::None,
                timestamp,
                timestamp,
                "completed",
                1_i64,
                0_i64,
                10_i64,
                5_i64
            ],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_name, tool_call_id, reasoning, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                "user",
                text,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                timestamp
            ],
        )
        .unwrap();
    }

    /// A planted store plus a live WAL writer holding one WAL-only commit
    /// (the live-CLI shape: the CLI appends a turn while holding the store
    /// with autocheckpoint off, so only the `-wal` changes).
    fn plant_store_with_live_wal(dir: &Path) -> (PathBuf, Connection) {
        let db_path = dir.join("state.db");
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        create_hermes_schema(&conn);
        insert_hermes_turn(
            &conn,
            "seed-turn",
            "committed before the CLI held the store",
            1_700_000_000.0_f64,
        );
        drop(conn);

        let writer = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        insert_hermes_turn(
            &writer,
            "live-turn",
            "appended while the CLI holds the store",
            1_700_000_100.0_f64,
        );
        assert!(
            sidecar_path(&db_path, "-wal").is_file(),
            "fixture must hold a live WAL"
        );
        (db_path, writer)
    }

    /// Discovery must list the whole live store bundle (the shelley
    /// contract): the database plus each present engine sidecar — a
    /// WAL-only commit is visible only through the `-wal` row.
    #[test]
    fn source_boundary_discovery_lists_the_store_bundle() {
        let dir = tempfile::TempDir::new().unwrap();
        let (db_path, writer) = plant_store_with_live_wal(dir.path());
        let wal_path = sidecar_path(&db_path, "-wal");
        let shm_path = sidecar_path(&db_path, "-shm");

        let connector = HermesConnector::new();
        let ctx = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![ScanRoot::local(db_path.clone())],
            None,
        );
        let sources = connector.discover_source_files(&ctx).unwrap();
        let expected = 1 + usize::from(wal_path.is_file()) + usize::from(shm_path.is_file());
        assert_eq!(
            sources.len(),
            expected,
            "discovery lists the database plus each present sidecar"
        );
        assert_eq!(sources[0].source_path, db_path);
        assert_eq!(sources[0].role, DiscoveredSourceRole::SqliteDatabase);
        let wal = sources
            .iter()
            .find(|source| source.source_path == wal_path)
            .expect("discovery must list the live -wal");
        assert_eq!(wal.role, DiscoveredSourceRole::MetadataSidecar);
        assert!(wal.required_for_reconstruction);
        assert_eq!(wal.origin, sources[0].origin);
        assert_eq!(wal.scan_root, sources[0].scan_root);
        if shm_path.is_file() {
            let shm = sources
                .iter()
                .find(|source| source.source_path == shm_path)
                .expect("present SHM sidecar must be discovered");
            assert_eq!(shm.role, DiscoveredSourceRole::MetadataSidecar);
            assert!(!shm.required_for_reconstruction);
        }

        // A clean close folds the WAL back into the database; whether the
        // engine also removes the sidecar files is its business (this
        // engine may leave them behind, as shelley's discovery test
        // documents) — discovery must follow whatever is on disk
        // (checkpoint/delete coverage).
        drop(writer);
        let sources = connector.discover_source_files(&ctx).unwrap();
        let expected = 1 + usize::from(wal_path.is_file()) + usize::from(shm_path.is_file());
        assert_eq!(
            sources.len(),
            expected,
            "discovery follows the store bundle's on-disk presence after close"
        );
        assert_eq!(sources[0].source_path, db_path);
        assert_eq!(sources[0].role, DiscoveredSourceRole::SqliteDatabase);
    }

    /// A WAL-only commit — how the live CLI appends a turn while holding
    /// the store open — leaves the primary file's size and mtime untouched,
    /// so a completion whose reuse evidence names only the primary
    /// authorizes the host's ledger to skip the store and the appended
    /// turn never reaches the index (the devin WAL bug class, found by the
    /// cass durable-source ledger). The completion must carry the `-wal`,
    /// exactly like the shelley connector's stores.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn source_boundary_completion_carries_the_live_wal() {
        let dir = tempfile::TempDir::new().unwrap();
        let (db_path, writer) = plant_store_with_live_wal(dir.path());
        let wal_path = sidecar_path(&db_path, "-wal");

        let connector = HermesConnector::new();
        let ctx = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![ScanRoot::local(db_path.clone())],
            None,
        );

        // Strict read-only: the boundary scan must never create or write
        // any sidecar of the store it reads.
        let db_before = std::fs::read(&db_path).unwrap();
        let wal_before = std::fs::read(&wal_path).unwrap();

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
        // through the strict read-only open while the writer still holds
        // the store.
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

        assert_eq!(
            std::fs::read(&db_path).unwrap(),
            db_before,
            "reader must not mutate the store"
        );
        assert_eq!(
            std::fs::read(&wal_path).unwrap(),
            wal_before,
            "reader must not mutate the WAL"
        );

        // Truncate coverage: the engine's own TRUNCATE checkpoint folds the
        // frames into the database and resets the `-wal` (to zero, or to a
        // header-only file) — the observed sidecar evidence must change
        // with it and keep matching the on-disk truth.
        writer
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let wal_len_after = std::fs::metadata(&wal_path).unwrap().len();
        assert_ne!(
            wal_len_after,
            wal_before.len() as u64,
            "a TRUNCATE checkpoint resets the wal"
        );
        let sources = connector.discover_source_files(&ctx).unwrap();
        let wal_row = sources
            .iter()
            .find(|source| source.source_path == wal_path)
            .expect("a reset wal is still present evidence");
        assert_eq!(
            wal_row.size_bytes,
            Some(wal_len_after),
            "the observed wal size must follow the on-disk wal"
        );

        // Delete coverage: a clean close folds the WAL into the database and
        // the `-wal` must drop out of the completion's reuse evidence.
        // This engine may leave a dead header-only file behind after close
        // (shelley's discovery test documents the same), so clear any
        // leftover sidecars: the store is fully checkpointed and closed,
        // which is the clean-close shape being modeled.
        drop(writer);
        let _ = std::fs::remove_file(&wal_path);
        let _ = std::fs::remove_file(sidecar_path(&db_path, "-shm"));
        let mut completions_after: Vec<SourceCompletion> = Vec::new();
        let mut emitted_after = 0usize;
        {
            let mut on_complete = |completion: &SourceCompletion| {
                completions_after.push(completion.clone());
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut on_complete),
            };
            connector
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_conv| {
                    emitted_after += 1;
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(emitted_after, 2, "the checkpointed turn is now in the db");
        assert_eq!(completions_after.len(), 1);
        assert!(
            completions_after[0].required_sidecars.is_empty(),
            "no wal on disk => no sidecar evidence"
        );
    }
}

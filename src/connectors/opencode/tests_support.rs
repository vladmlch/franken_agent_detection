//! Shared test fixtures and helpers for `OpenCode` tests.

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

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::super::sqlite_sync::{Connection, ConnectionExt, params};

pub(super) fn open_test_connection(path: &Path) -> Connection {
    Connection::open(path.to_string_lossy().as_ref()).unwrap()
}

pub(super) fn create_opencode_storage(dir: &TempDir) -> PathBuf {
    let storage = dir.path().join("opencode").join("storage");
    fs::create_dir_all(storage.join("session")).unwrap();
    fs::create_dir_all(storage.join("message")).unwrap();
    fs::create_dir_all(storage.join("part")).unwrap();
    storage
}

pub(super) fn write_session(storage: &Path, project_id: &str, session: &serde_json::Value) {
    let session_id = session["id"].as_str().unwrap();
    let session_dir = storage.join("session").join(project_id);
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join(format!("{session_id}.json")),
        session.to_string(),
    )
    .unwrap();
}

pub(super) fn write_message(storage: &Path, session_id: &str, message: &serde_json::Value) {
    let message_id = message["id"].as_str().unwrap();
    let message_dir = storage.join("message").join(session_id);
    fs::create_dir_all(&message_dir).unwrap();
    fs::write(
        message_dir.join(format!("{message_id}.json")),
        message.to_string(),
    )
    .unwrap();
}

pub(super) fn write_part(storage: &Path, message_id: &str, part: &serde_json::Value) {
    let part_id = part["id"].as_str().unwrap();
    let part_dir = storage.join("part").join(message_id);
    fs::create_dir_all(&part_dir).unwrap();
    fs::write(part_dir.join(format!("{part_id}.json")), part.to_string()).unwrap();
}

/// Create a test SQLite database with the OpenCode v1.2+ schema.
pub(super) fn create_test_sqlite_db(dir: &Path) -> PathBuf {
    let db_path = dir.join("opencode.db");
    let conn = open_test_connection(&db_path);

    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            title TEXT,
            directory TEXT,
            time_created TEXT DEFAULT CURRENT_TIMESTAMP,
            time_updated TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            data TEXT NOT NULL,
            time_created TEXT DEFAULT CURRENT_TIMESTAMP,
            time_updated TEXT DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            data TEXT NOT NULL,
            time_created TEXT DEFAULT CURRENT_TIMESTAMP,
            time_updated TEXT DEFAULT CURRENT_TIMESTAMP
        );",
    )
    .unwrap();

    db_path
}

/// Adds OpenCode 2.x tables (columns as in OpenCode v2.0.16's
/// packages/core/src/session/sql.ts) to a test db.
pub(super) fn add_opencode_v2_tables(db_path: &Path, with_session_v2: bool) {
    let conn = open_test_connection(db_path);
    if with_session_v2 {
        conn.execute_batch(
            "CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT,
                slug TEXT NOT NULL,
                directory TEXT NOT NULL,
                title TEXT,
                version TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                time_archived INTEGER
            );",
        )
        .unwrap();
    }
    conn.execute_batch(
        "CREATE TABLE session_message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            type TEXT NOT NULL,
            seq INTEGER NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );",
    )
    .unwrap();
}

pub(super) fn insert_v2_session(conn: &Connection, id: &str, title: &str, updated_ms: i64) {
    conn.execute_compat(
        "INSERT INTO session_v2 (id, project_id, slug, directory, title, version, time_created, time_updated)
         VALUES (?1, 'proj-2', ?2, '/work/v2', ?3, '2.0.16', ?4, ?5)",
        params![id, format!("slug-{id}"), title, updated_ms, updated_ms],
    )
    .unwrap();
}

pub(super) fn insert_v2_message(
    conn: &Connection,
    session_id: &str,
    seq: i64,
    kind: &str,
    data: &str,
) {
    conn.execute_compat(
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            format!("msg_{session_id}_{seq}"),
            session_id,
            kind,
            seq,
            1_700_000_000_000_i64 + seq * 1_000,
            1_700_000_000_000_i64 + seq * 1_000,
            data
        ],
    )
    .unwrap();
}

pub(super) fn insert_v1_text_message(
    conn: &Connection,
    session_id: &str,
    id: &str,
    role: &str,
    text: &str,
) {
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            id,
            session_id,
            format!(r#"{{"role":"{role}","time":{{"created":1600000000000}}}}"#),
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            format!("part-{id}"),
            id,
            session_id,
            serde_json::json!({"type": "text", "text": text}).to_string(),
        ],
    )
    .unwrap();
}

/// A scan context whose single scan root is remote, so
/// `allow_local_default_dbs` is false and this machine's real
/// `~/.local/share/opencode` store is never probed: the fixture store is
/// the only candidate.
pub(super) fn remote_scan_context(
    data_dir: &Path,
    root: &Path,
) -> crate::connectors::scan::ScanContext {
    let origin = crate::types::Origin::remote_with_host("testhost", "u@testhost");
    let scan_root = crate::connectors::scan::ScanRoot::remote(root.to_path_buf(), origin, None);
    crate::connectors::scan::ScanContext::with_roots(data_dir.to_path_buf(), vec![scan_root], None)
}

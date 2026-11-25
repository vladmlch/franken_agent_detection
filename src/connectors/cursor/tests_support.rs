//! Shared test helpers for Cursor connector unit tests.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

use super::CursorConnector;
use crate::connectors::sqlite_sync::{Connection, ConnectionExt, params};

/// Create a test SQLite database with the cursorDiskKV table
pub(super) fn create_test_db(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn
}

// -----------------------------------------------------------------
// Subagent verdict metadata (metadata.is_subagent / metadata.parent_id)
// -----------------------------------------------------------------

/// Build a synthetic state.vscdb with composerData rows and an optional
/// minimal composerHeaders(composerId, isSubagent) table.
/// Local synthetic store builder (this test module has no create_test_db).
pub(super) fn verdict_test_db(path: &Path) -> Connection {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn
}

pub(super) fn cursor_store(
    composers: &[(&str, &Value)],
    headers: Option<&[(&str, i64)]>,
) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("globalStorage").join("state.vscdb");
    let conn = verdict_test_db(&db_path);
    for (id, val) in composers {
        let key = format!("composerData:{id}");
        let body = val.to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![key.as_str(), body.as_str()],
        )
        .unwrap();
    }
    if let Some(rows) = headers {
        conn.execute(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)",
        )
        .unwrap();
        for (id, flag) in rows {
            conn.execute_compat(
                "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
                params![*id, *flag],
            )
            .unwrap();
        }
    }
    drop(conn);
    (dir, db_path)
}

pub(super) fn cursor_verdicts(db_path: &Path, since: Option<i64>) -> HashMap<String, Value> {
    let convs = CursorConnector::extract_from_db(db_path, since, &|| {}).unwrap();
    convs
        .into_iter()
        .filter_map(|c| c.external_id.map(|id| (id, c.metadata)))
        .collect()
}

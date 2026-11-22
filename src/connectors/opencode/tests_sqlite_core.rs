//! Unit tests for `OpenCode` `SQLite` v1 extraction, timestamp normalization, and discovery.

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
use std::path::PathBuf;

use frankensqlite::{SqliteValue, params};
use tempfile::TempDir;

use super::OpenCodeConnector;
use super::parts::normalize_sqlite_ts_value;
use super::tests_support::{create_test_sqlite_db, open_test_connection, remote_scan_context};
use crate::connectors::Connector;
use crate::connectors::scan::{ScanContext, ScanRoot};
use crate::connectors::sqlite_sync::ConnectionExt;

#[test]
fn sqlite_extract_simple_session() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params!["sess-1", "proj-1", "Test Session", "/home/user/project"],
    )
    .unwrap();

    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-1",
            "sess-1",
            r#"{"role":"user","time":{"created":1700000000000}}"#,
        ],
    )
    .unwrap();

    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-1",
            "msg-1",
            "sess-1",
            r#"{"type":"text","text":"Hello world"}"#,
        ],
    )
    .unwrap();

    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-2",
            "sess-1",
            r#"{"role":"assistant","time":{"created":1700000001000},"modelID":"claude-3"}"#,
        ],
    )
    .unwrap();

    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-2",
            "msg-2",
            "sess-1",
            r#"{"type":"text","text":"Hi there!"}"#,
        ],
    )
    .unwrap();

    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("sess-1"));
    assert_eq!(convs[0].title.as_deref(), Some("Test Session"));
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Hello world");
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert_eq!(convs[0].messages[1].content, "Hi there!");
    assert_eq!(convs[0].messages[1].author.as_deref(), Some("claude-3"));
}

#[test]
fn sqlite_extract_incremental_skips_old_sessions_without_dropping_new() {
    use std::collections::HashSet;
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    let insert = |sid: &str, updated_ms: Option<i64>, msg_created_ms: i64, text: &str| {
        match updated_ms {
            Some(u) => conn
                .execute_compat(
                    "INSERT INTO session (id, title, time_updated) VALUES (?1, ?2, ?3)",
                    params![sid, sid, u],
                )
                .unwrap(),
            None => conn
                .execute_compat(
                    "INSERT INTO session (id, title, time_updated) VALUES (?1, ?2, NULL)",
                    params![sid, sid],
                )
                .unwrap(),
        };
        conn.execute_compat(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            params![
                format!("{sid}-m"),
                sid,
                format!(r#"{{"role":"user","time":{{"created":{msg_created_ms}}}}}"#)
            ],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
            params![
                format!("{sid}-p"),
                format!("{sid}-m"),
                sid,
                format!(r#"{{"type":"text","text":"{text}"}}"#)
            ],
        )
        .unwrap();
    };

    let old_ms = 1_700_000_000_000_i64;
    let new_ms = 1_700_000_900_000_i64;
    let cutoff = 1_700_000_500_000_i64;

    insert("old", Some(old_ms), old_ms, "old content");
    insert("new", Some(new_ms), new_ms, "new content");
    // No session time -> the keep filter must fall through to the per-message
    // time filter (kept when recent, dropped when old).
    insert("null-recent", None, new_ms, "null recent content");
    insert("null-old", None, old_ms, "null old content");
    drop(conn);

    // Full scan sees every session.
    let full = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    let full_ids: HashSet<String> = full.iter().filter_map(|c| c.external_id.clone()).collect();
    assert_eq!(
        full_ids,
        ["old", "new", "null-recent", "null-old"]
            .iter()
            .copied()
            .map(str::to_string)
            .collect()
    );

    // Incremental at the cutoff: known-old sessions are skipped without
    // decoding their parts; the recent + unknown-time-recent survive.
    let inc = OpenCodeConnector::extract_from_sqlite(&db_path, Some(cutoff), None).unwrap();
    let inc_ids: HashSet<String> = inc.iter().filter_map(|c| c.external_id.clone()).collect();
    assert_eq!(
        inc_ids,
        ["new", "null-recent"]
            .iter()
            .copied()
            .map(str::to_string)
            .collect(),
        "incremental keeps updated>=cutoff and unknown-time+recent-msg, drops the rest"
    );

    // The kept session's content is byte-identical to the full-scan version:
    // the decode-skip must not corrupt or lose a kept session's messages.
    let new_full = full
        .iter()
        .find(|c| c.external_id.as_deref() == Some("new"))
        .unwrap();
    let new_inc = inc
        .iter()
        .find(|c| c.external_id.as_deref() == Some("new"))
        .unwrap();
    assert_eq!(new_inc.messages.len(), new_full.messages.len());
    assert_eq!(new_inc.messages[0].content, new_full.messages[0].content);
    assert_eq!(new_inc.messages[0].content, "new content");
}

#[test]
fn sqlite_extract_calls_progress_tick_during_scan() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["s", "s"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["m", "s", r#"{"role":"user"}"#],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params!["p", "m", "s", r#"{"type":"text","text":"hi"}"#],
    )
    .unwrap();
    drop(conn);

    let ticks = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ticks);
    let tick = move || {
        counter.fetch_add(1, Ordering::Relaxed);
    };
    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, Some(&tick)).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(
        ticks.load(Ordering::Relaxed) >= 1,
        "the scan progress tick must fire while decoding (cass#373 Variant A)"
    );
}

#[test]
fn sqlite_extract_empty_db() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn sqlite_extract_skips_empty_messages() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["sess-empty", "Empty Session"],
    )
    .unwrap();

    // Session with no messages should be skipped
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn sqlite_extract_with_tool_parts() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["sess-tools", "Tool Session"],
    )
    .unwrap();

    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["msg-t1", "sess-tools", r#"{"role":"assistant"}"#],
    )
    .unwrap();

    // Text part
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p1",
            "msg-t1",
            "sess-tools",
            r#"{"type":"text","text":"Let me check that."}"#,
        ],
    )
    .unwrap();

    // Tool part with output
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p2",
            "msg-t1",
            "sess-tools",
            r#"{"type":"tool","state":{"output":"file.rs: 42 lines"}}"#,
        ],
    )
    .unwrap();

    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Let me check that."));
    assert!(convs[0].messages[0].content.contains("[Tool Output]"));
    assert!(convs[0].messages[0].content.contains("file.rs: 42 lines"));
}

#[test]
fn sqlite_extract_deduplicates_sessions() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    // Two sessions with different IDs
    for (sid, title) in &[("sess-a", "Session A"), ("sess-b", "Session B")] {
        conn.execute_compat(
            "INSERT INTO session (id, title) VALUES (?1, ?2)",
            params![*sid, *title],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            params![format!("msg-{sid}"), *sid, r#"{"role":"user"}"#],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
            params![
                format!("p-{sid}"),
                format!("msg-{sid}"),
                *sid,
                r#"{"type":"text","text":"Hello"}"#,
            ],
        )
        .unwrap();
    }

    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 2);
}

#[test]
fn sqlite_extract_groups_bulk_scanned_messages_by_session() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    for session_id in ["sess-a", "sess-b"] {
        conn.execute_compat(
            "INSERT INTO session (id, title) VALUES (?1, ?2)",
            params![session_id, format!("Session {session_id}")],
        )
        .unwrap();
    }

    for (message_id, session_id, role, created_at) in [
        ("msg-a-late", "sess-a", "assistant", 30_i64),
        ("msg-b-only", "sess-b", "user", 20_i64),
        ("msg-a-early", "sess-a", "user", 10_i64),
    ] {
        conn.execute_compat(
            "INSERT INTO message (id, session_id, data, time_created)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                message_id,
                session_id,
                format!(r#"{{"role":"{role}"}}"#),
                created_at
            ],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO part (id, message_id, session_id, data, time_created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                format!("part-{message_id}"),
                message_id,
                session_id,
                format!(r#"{{"type":"text","text":"content for {message_id}"}}"#),
                created_at
            ],
        )
        .unwrap();
    }

    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 2);

    let sess_a = convs
        .iter()
        .find(|conv| conv.external_id.as_deref() == Some("sess-a"))
        .expect("sess-a conversation");
    assert_eq!(sess_a.messages.len(), 2);
    assert_eq!(sess_a.messages[0].idx, 0);
    assert_eq!(sess_a.messages[0].content, "content for msg-a-early");
    assert_eq!(sess_a.messages[1].idx, 1);
    assert_eq!(sess_a.messages[1].content, "content for msg-a-late");

    let sess_b = convs
        .iter()
        .find(|conv| conv.external_id.as_deref() == Some("sess-b"))
        .expect("sess-b conversation");
    assert_eq!(sess_b.messages.len(), 1);
    assert_eq!(sess_b.messages[0].idx, 0);
    assert_eq!(sess_b.messages[0].content, "content for msg-b-only");
}

/// Test that SQLite extraction handles integer timestamps (epoch seconds)
/// which Drizzle ORM may use instead of TEXT ISO 8601 strings.
#[test]
fn sqlite_extract_handles_integer_timestamps() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    let conn = open_test_connection(&db_path);

    // Create schema with INTEGER timestamp columns (Drizzle ORM integer mode)
    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            title TEXT,
            directory TEXT,
            time_created INTEGER,
            time_updated INTEGER
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            data TEXT NOT NULL,
            time_created INTEGER,
            time_updated INTEGER
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            data TEXT NOT NULL,
            time_created INTEGER,
            time_updated INTEGER
        );",
    )
    .unwrap();

    // Insert session with epoch second timestamps
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
        params!["sess-int", "proj-1", "Integer TS Session", 1700000000_i64, 1700000100_i64],
    ).unwrap();

    conn.execute_compat(
        "INSERT INTO message (id, session_id, data, time_created) VALUES (?1, ?2, ?3, ?4)",
        params!["msg-int", "sess-int", r#"{"role":"user"}"#, 1700000050_i64],
    )
    .unwrap();

    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-int",
            "msg-int",
            "sess-int",
            r#"{"type":"text","text":"Integer timestamps!"}"#,
        ],
    )
    .unwrap();

    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    // Epoch seconds should be normalized to milliseconds
    assert_eq!(convs[0].started_at, Some(1_700_000_000_000));
    assert_eq!(convs[0].ended_at, Some(1_700_000_100_000));
    assert!(convs[0].messages[0].content.contains("Integer timestamps!"));
}

#[test]
fn sqlite_extract_metadata_includes_source() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);

    conn.execute_compat(
        "INSERT INTO session (id, project_id, title) VALUES (?1, ?2, ?3)",
        params!["sess-meta", "proj-meta", "Meta Session"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["msg-meta", "sess-meta", r#"{"role":"user"}"#],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-meta",
            "msg-meta",
            "sess-meta",
            r#"{"type":"text","text":"Test"}"#,
        ],
    )
    .unwrap();

    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].metadata["source"], "sqlite");
    assert_eq!(convs[0].metadata["project_id"], "proj-meta");
}

// =====================================================
// normalize_sqlite_ts_value() Tests
// =====================================================

#[test]
fn normalize_sqlite_ts_value_integer_epoch_seconds() {
    let val = SqliteValue::Integer(1_700_000_000);
    assert_eq!(normalize_sqlite_ts_value(&val), Some(1_700_000_000_000));
}

#[test]
fn normalize_sqlite_ts_value_integer_epoch_millis() {
    let val = SqliteValue::Integer(1_700_000_000_000);
    // Already in ms range, should pass through
    assert_eq!(normalize_sqlite_ts_value(&val), Some(1_700_000_000_000));
}

#[test]
fn normalize_sqlite_ts_value_text_sqlite_format() {
    let val = SqliteValue::Text("2024-01-15 14:30:00".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    // Should parse to 2024-01-15T14:30:00 UTC epoch millis
    assert_eq!(result, 1_705_329_000_000);
}

#[test]
fn normalize_sqlite_ts_value_text_iso8601_t_separator() {
    let val = SqliteValue::Text("2024-01-15T14:30:00".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    assert_eq!(result, 1_705_329_000_000);
}

#[test]
fn normalize_sqlite_ts_value_text_fractional_seconds() {
    let val = SqliteValue::Text("2024-01-15 14:30:00.123".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    assert_eq!(result, 1_705_329_000_123);
}

#[test]
fn normalize_sqlite_ts_value_text_t_fractional() {
    let val = SqliteValue::Text("2024-01-15T14:30:00.456".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    assert_eq!(result, 1_705_329_000_456);
}

#[test]
fn normalize_sqlite_ts_value_text_rfc3339_z() {
    let val = SqliteValue::Text("2024-01-15T14:30:00Z".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    assert_eq!(result, 1_705_329_000_000);
}

#[test]
fn normalize_sqlite_ts_value_text_rfc3339_offset() {
    let val = SqliteValue::Text("2024-01-15T14:30:00+00:00".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    assert_eq!(result, 1_705_329_000_000);
}

#[test]
fn normalize_sqlite_ts_value_text_integer_string() {
    let val = SqliteValue::Text("1700000000".into());
    let result = normalize_sqlite_ts_value(&val).unwrap();
    assert_eq!(result, 1_700_000_000_000);
}

#[test]
fn normalize_sqlite_ts_value_null() {
    let val = SqliteValue::Null;
    assert_eq!(normalize_sqlite_ts_value(&val), None);
}

#[test]
fn normalize_sqlite_ts_value_unparseable_text() {
    let val = SqliteValue::Text("not a date".into());
    assert_eq!(normalize_sqlite_ts_value(&val), None);
}

#[test]
fn normalize_sqlite_ts_value_empty_text() {
    let val = SqliteValue::Text("".into());
    assert_eq!(normalize_sqlite_ts_value(&val), None);
}

#[test]
fn normalize_sqlite_ts_value_real() {
    let val = SqliteValue::Float(1_700_000_000.5);
    assert_eq!(normalize_sqlite_ts_value(&val), Some(1_700_000_000_000));
}

// =====================================================
// Regression: issue #174 — SQLite DB discovery
// =====================================================

/// Regression for issue #174: the candidate list must put XDG paths
/// reachable from `$HOME` ahead of `dirs::data_local_dir()` so macOS
/// users (where `data_local_dir()` resolves to `~/Library/Application
/// Support`) still have their canonical `~/.local/share/opencode/
/// opencode.db` found. The explicit override must always win.
#[test]
fn sqlite_db_candidates_from_orders_home_xdg_before_platform_dirs() {
    let home = PathBuf::from("/home/testuser");
    let xdg_data = PathBuf::from("/var/lib/xdg");
    let xdg_config = PathBuf::from("/etc/xdg");

    // No override → home-XDG paths must come first.
    let list = OpenCodeConnector::sqlite_db_candidates_from(
        None,
        Some(&home),
        Some(&xdg_data),
        Some(&xdg_config),
    );
    assert_eq!(list.len(), 4);
    assert_eq!(list[0], home.join(".local/share/opencode/opencode.db"));
    assert_eq!(list[1], home.join(".config/opencode/opencode.db"));
    assert_eq!(list[2], xdg_data.join("opencode/opencode.db"));
    assert_eq!(list[3], xdg_config.join("opencode/opencode.db"));

    // Explicit override always wins.
    let override_path = PathBuf::from("/custom/opencode.db");
    let list = OpenCodeConnector::sqlite_db_candidates_from(
        Some(override_path.clone()),
        Some(&home),
        Some(&xdg_data),
        Some(&xdg_config),
    );
    assert_eq!(list[0], override_path);
    assert_eq!(list.len(), 5);
}

/// Regression for issue #174: when two dirs helpers resolve to the
/// same path (common on macOS where config_dir == data_local_dir),
/// the candidate list must deduplicate while preserving priority.
#[test]
fn sqlite_db_candidates_from_deduplicates_overlapping_roots() {
    let home = PathBuf::from("/Users/testuser");
    // On macOS, both of these map to ~/Library/Application Support.
    let overlap = PathBuf::from("/Users/testuser/Library/Application Support");
    let list = OpenCodeConnector::sqlite_db_candidates_from(
        None,
        Some(&home),
        Some(&overlap),
        Some(&overlap),
    );
    // 2 home-XDG paths + 1 overlap path (deduplicated) = 3.
    assert_eq!(list.len(), 3, "list = {list:?}");
    assert_eq!(list[0], home.join(".local/share/opencode/opencode.db"));
    assert_eq!(list[1], home.join(".config/opencode/opencode.db"));
    assert_eq!(list[2], overlap.join("opencode/opencode.db"));
}

/// Regression for issue #174: when the caller passes an explicit
/// directory that contains `opencode.db`, the scanner must discover
/// it. This is the ctx.data_dir-as-parent path.
#[test]
fn scan_finds_sqlite_db_when_data_dir_is_db_parent() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params![
            "sess-parent",
            "proj-p",
            "Parent Session",
            "/home/user/parent",
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-parent",
            "sess-parent",
            r#"{"role":"user","time":{"created":1700000000000}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-parent",
            "msg-parent",
            "sess-parent",
            r#"{"type":"text","text":"Parent content"}"#,
        ],
    )
    .unwrap();

    // Pass the parent directory (not the .db file itself). The scan root
    // is remote so `allow_local_default_dbs` stays false: a local root
    // would drag this machine's real opencode store into the result.
    let connector = OpenCodeConnector::new();
    let ctx = remote_scan_context(dir.path(), dir.path());
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("sess-parent"));
}

/// Regression for issue #174: when the caller passes an explicit
/// scan root (use_default_detection() == false) that does NOT
/// contain opencode.db, the scanner must still find a DB via the
/// ctx.data_dir-as-parent candidate if one is present.
#[test]
fn scan_finds_sqlite_db_via_data_dir_even_with_explicit_scan_roots() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params![
            "sess-roots",
            "proj-roots",
            "Roots Session",
            "/home/user/roots",
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-roots",
            "sess-roots",
            r#"{"role":"user","time":{"created":1700000000000}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-roots",
            "msg-roots",
            "sess-roots",
            r#"{"type":"text","text":"Roots content"}"#,
        ],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = remote_scan_context(dir.path(), dir.path());
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "explicit scan_roots must still check ctx.data_dir for opencode.db"
    );
    assert_eq!(convs[0].external_id.as_deref(), Some("sess-roots"));
}

#[test]
fn scan_finds_sqlite_db_with_explicit_config_root() {
    let dir = TempDir::new().unwrap();
    let config_root = dir.path().join(".config");
    let opencode_dir = config_root.join("opencode");
    std::fs::create_dir_all(&opencode_dir).unwrap();

    let db_path = create_test_sqlite_db(&opencode_dir);
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params![
            "sess-config",
            "proj-config",
            "Config Session",
            "/home/user/config",
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-config",
            "sess-config",
            r#"{"role":"user","time":{"created":1700000000000}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-config",
            "msg-config",
            "sess-config",
            r#"{"type":"text","text":"Config content"}"#,
        ],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = remote_scan_context(dir.path(), &config_root);
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("sess-config"));
}

/// Regression: when `ctx.data_dir` points directly at an
/// `opencode.db` FILE (not the parent directory), the scanner must
/// use it as-is — without trying to join `opencode.db` onto it
/// again (which would produce a nonsense candidate like
/// `/path/to/opencode.db/opencode.db`).
#[test]
fn scan_accepts_data_dir_as_direct_db_file() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params![
            "sess-direct",
            "proj-direct",
            "Direct Session",
            "/home/user/direct",
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-direct",
            "sess-direct",
            r#"{"role":"user","time":{"created":1700000000000}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-direct",
            "msg-direct",
            "sess-direct",
            r#"{"type":"text","text":"Direct content"}"#,
        ],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    // Pass the db file itself as data_dir; the remote root keeps the
    // machine's real store out of the scan.
    let ctx = remote_scan_context(&db_path, dir.path());
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("sess-direct"));
}

/// Regression: a nonexistent `.db` path passed as `ctx.data_dir`
/// must not be silently treated as a directory (which would have
/// produced a bogus `/path/to/missing.db/opencode.db` candidate in
/// an earlier draft). The scanner simply finds nothing.
#[test]
fn scan_handles_nonexistent_db_path_in_data_dir() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("missing.db");
    // No file on disk at `missing`.

    let connector = OpenCodeConnector::new();
    let ctx = remote_scan_context(&missing, dir.path());
    let convs = connector.scan(&ctx).unwrap();
    assert!(
        convs.is_empty(),
        "nonexistent .db path should not produce sessions"
    );
}

/// Regression for cass#357: a remote-mirror scan root must never pull the
/// local machine's canonical home/XDG `opencode.db` into its scan. The
/// caller attributes everything returned by that invocation to the remote
/// source, so a home fallback double-indexes local sessions under the
/// remote `source_id`.
#[test]
fn remote_scan_root_does_not_include_local_default_dbs() {
    let dir = TempDir::new().unwrap();
    let mirror = dir.path().join("remotes/studio/mirror/opencode");
    fs::create_dir_all(&mirror).unwrap();
    let db_path = create_test_sqlite_db(&mirror);
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params![
            "sess-remote",
            "proj-r",
            "Remote Session",
            "/home/remote/project"
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-remote",
            "sess-remote",
            r#"{"role":"user","time":{"created":1700000000000}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-remote",
            "msg-remote",
            "sess-remote",
            r#"{"type":"text","text":"Remote content"}"#,
        ],
    )
    .unwrap();

    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::remote(
            mirror.clone(),
            crate::types::Origin::remote("studio"),
            None,
        )],
        None,
    );
    assert!(
        !OpenCodeConnector::allow_local_default_dbs(&ctx),
        "a remote scan root must disable the home/XDG DB fallback (cass#357)"
    );

    let connector = OpenCodeConnector::new();
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(
        convs
            .iter()
            .filter(|c| c.external_id.as_deref() == Some("sess-remote"))
            .count(),
        1,
        "the remote mirror's own DB must still be scanned"
    );
    for conv in &convs {
        assert!(
            conv.source_path.starts_with(&mirror),
            "remote-context scan leaked a session outside the mirror root (cass#357): {}",
            conv.source_path.display()
        );
    }
    // Discovery must stay in lockstep with scan (trait contract).
    let discovered = connector.discover_source_files(&ctx).unwrap();
    for file in &discovered {
        assert!(
            file.source_path.starts_with(&mirror),
            "remote-context discovery leaked a source outside the mirror root (cass#357): {}",
            file.source_path.display()
        );
    }
}

/// Companion to the cass#357 guard: local explicit roots (and empty
/// roots, i.e. default detection) must keep the issue #174 home/XDG DB
/// fallback, and any remote root in the mix disables it.
#[test]
fn local_scan_roots_keep_default_db_fallback() {
    let dir = TempDir::new().unwrap();
    let local_ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    assert!(OpenCodeConnector::allow_local_default_dbs(&local_ctx));

    let default_ctx = ScanContext::local_default(PathBuf::new(), None);
    assert!(OpenCodeConnector::allow_local_default_dbs(&default_ctx));

    let mixed_ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![
            ScanRoot::local(dir.path().to_path_buf()),
            ScanRoot::remote(
                dir.path().join("mirror"),
                crate::types::Origin::remote("laptop"),
                None,
            ),
        ],
        None,
    );
    assert!(
        !OpenCodeConnector::allow_local_default_dbs(&mixed_ctx),
        "any remote root disables the local-default fallback"
    );
}

/// Hermetic opt-out: a context built with
/// [`ScanContext::without_local_default_stores`] must never resolve
/// `$HOME`/XDG candidates — neither through the sqlite gate nor through
/// the default-detection `storage_root()` fallback. The conformance
/// harness relies on this so in-process tests can never read this
/// machine's real `~/.local/share/opencode` (or a planted probe store).
#[test]
fn hermetic_contexts_never_resolve_home_or_xdg_default_stores() {
    let dir = TempDir::new().unwrap();

    let local_ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    )
    .without_local_default_stores();
    assert!(!OpenCodeConnector::allow_local_default_dbs(&local_ctx));
    for root in OpenCodeConnector::sqlite_source_roots(&local_ctx) {
        assert!(
            root.path.starts_with(dir.path()),
            "hermetic local-roots context resolved a default store: {}",
            root.path.display()
        );
    }
    for root in OpenCodeConnector::storage_source_roots(&local_ctx) {
        assert!(
            root.path.starts_with(dir.path()),
            "hermetic local-roots context resolved a default storage root: {}",
            root.path.display()
        );
    }

    let default_ctx =
        ScanContext::local_default(PathBuf::new(), None).without_local_default_stores();
    assert!(!OpenCodeConnector::allow_local_default_dbs(&default_ctx));
    assert!(OpenCodeConnector::sqlite_source_roots(&default_ctx).is_empty());
    assert!(OpenCodeConnector::storage_source_roots(&default_ctx).is_empty());
}

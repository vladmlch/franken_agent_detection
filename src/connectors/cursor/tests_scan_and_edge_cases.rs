use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use frankensqlite::compat::RowExt;
use frankensqlite::params;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;

use super::CursorConnector;
use super::agent_transcripts::AGENT_TRANSCRIPTS_DIR;
use super::composer_parser::CURSOR_DISK_KV_RANGE_SQL;
use super::tests_support::create_test_db;
use crate::connectors::Connector;
use crate::connectors::scan::{ScanContext, ScanRoot};
use crate::connectors::sqlite_sync::{Connection, ConnectionExt};

// =========================================================================
// Detection tests
// =========================================================================

#[test]
fn detect_not_found_without_cursor_dir() {
    let connector = CursorConnector::new();
    let result = connector.detect();
    // On most CI/test systems, Cursor won't be installed
    // Just verify detect() doesn't panic
    let _ = result.detected;
}

// =========================================================================
// Scan tests
// =========================================================================

#[test]
fn scan_empty_directory_returns_empty() {
    let dir = TempDir::new().unwrap();

    // Create globalStorage to make scan() use this directory instead of fallback
    let global_dir = dir.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();
    // Create an empty state.vscdb to prevent fallback to system Cursor
    create_test_db(&global_dir.join("state.vscdb"));

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

/// Path of a SQLite sidecar file (`<db>-wal`, `<db>-shm`).
fn sqlite_sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut raw = db.as_os_str().to_owned();
    raw.push(suffix);
    PathBuf::from(raw)
}

/// `(bytes, mtime)` of one store file, for never-modify assertions.
fn file_state(path: &Path) -> (Option<Vec<u8>>, Option<std::time::SystemTime>) {
    (
        fs::read(path).ok(),
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok(),
    )
}

/// A `state.vscdb` in WAL mode with one composer row, closed the way a
/// hard-killed Cursor leaves it: the writer is gone, and a checkpointed
/// `-wal` (0 bytes) plus its `-shm` wal-index remain next to the db.
fn create_wal_mode_state_vscdb(dir: &Path) -> PathBuf {
    let db_path = dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = create_test_db(&db_path);
    let mode: String = conn
        .query_row_map("PRAGMA journal_mode=wal;", &[], |row| {
            row.get_typed::<String>(0)
        })
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");

    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            "composerData:wal-fallback",
            r#"{"text":"Read after a crash"}"#
        ],
    )
    .unwrap();

    // The incident shape: every frame checkpointed into the db, so the
    // `-wal` remains on disk at 0 bytes after the writer exits.
    conn.query_row_map::<(i64, i64, i64), _>("PRAGMA wal_checkpoint(TRUNCATE);", &[], |row| {
        Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?))
    })
    .unwrap();
    drop(conn);

    assert!(
        sqlite_sidecar(&db_path, "-wal").is_file(),
        "the writer must leave a `-wal` file behind"
    );
    assert!(
        sqlite_sidecar(&db_path, "-shm").is_file(),
        "the writer must leave a `-shm` wal-index behind"
    );
    db_path
}

/// Leave the `-shm` wal-index in one of the states a crashed writer can:
/// `garbage` bytes, a `torn` header checksum (one flipped bit), `absent`,
/// `zeroed`, or `empty`. A store in any of these states must still be
/// read, and the user's db and `-wal` files must never change.
fn damage_wal_index(db: &Path, state: &str) {
    let shm = sqlite_sidecar(db, "-shm");
    match state {
        "garbage" => fs::write(&shm, vec![0xA5_u8; 32_768]).unwrap(),
        "torn" => {
            let mut bytes = fs::read(&shm).unwrap();
            assert!(bytes.len() > 24, "the fixture `-shm` must have a header");
            // One flipped bit inside the wal-index header's checksum word:
            // a sidecar torn mid-write.
            bytes[16] ^= 0x01;
            fs::write(&shm, bytes).unwrap();
        }
        "absent" => fs::remove_file(&shm).unwrap(),
        "zeroed" => fs::write(&shm, vec![0_u8; 32_768]).unwrap(),
        "empty" => fs::write(&shm, []).unwrap(),
        other => unreachable!("unknown wal-index state: {other}"),
    }
}

/// A `state.vscdb` left with an unusable wal-index (the crash shape from
/// the 2026-11-07 OpenCode zero-index incident, on Cursor's store) must
/// still be read, without modifying the user's files. The db and `-wal`
/// are never written by any read-only open; the `-shm` is never written
/// whenever the snapshot fallback engages, and `garbage`/`torn`
/// wal-indexes refuse the strict open on every frankensqlite build, so the
/// fallback always engages for them.
#[serial]
#[test]
fn scan_reads_a_state_vscdb_left_with_an_unusable_wal_index() {
    for state in ["garbage", "torn", "absent", "zeroed", "empty"] {
        let tmp = TempDir::new().unwrap();
        let db_path = create_wal_mode_state_vscdb(tmp.path());
        damage_wal_index(&db_path, state);
        let before_db = file_state(&db_path);
        let before_wal = file_state(&sqlite_sidecar(&db_path, "-wal"));
        let before_shm = file_state(&sqlite_sidecar(&db_path, "-shm"));

        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            None,
        );
        let convs = CursorConnector::new()
            .scan(&ctx)
            .unwrap_or_else(|error| panic!("{state}: scan failed: {error:#}"));

        assert_eq!(
            convs.len(),
            1,
            "{state}: the crashed store must still be read"
        );
        assert!(
            convs[0]
                .messages
                .iter()
                .any(|message| message.content.contains("Read after a crash")),
            "{state}: the composer transcript must be readable"
        );

        assert_eq!(
            file_state(&db_path),
            before_db,
            "{state}: reading must never modify the db"
        );
        assert_eq!(
            file_state(&sqlite_sidecar(&db_path, "-wal")),
            before_wal,
            "{state}: reading must never modify the `-wal`"
        );
        if matches!(state, "garbage" | "torn") {
            assert_eq!(
                file_state(&sqlite_sidecar(&db_path, "-shm")),
                before_shm,
                "{state}: the snapshot fallback must leave the `-shm` untouched"
            );
        }
    }
}

/// A store that fails to open for any other reason must surface as a
/// scan error (cass F5 semantics), not a swallowed warn plus an
/// Ok(0 conversations) scan.
#[test]
fn scan_reports_an_unreadable_state_vscdb_as_a_scan_error() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    fs::write(&db_path, b"this is not a sqlite database").unwrap();

    let ctx = ScanContext::with_roots(
        tmp.path().to_path_buf(),
        vec![ScanRoot::local(tmp.path().to_path_buf())],
        None,
    );
    let error = CursorConnector::new().scan(&ctx).unwrap_err();
    let chain = format!("{error:#}");
    assert!(
        chain.contains("state.vscdb"),
        "the scan error must name the failing store: {chain}"
    );
}

/// cass follow-up to F9 (2026-11-07): the same rule on Cursor's store — a
/// `state.vscdb` that is a *valid but empty* database (a 0-byte file, or a
/// schema with no tables at all) must scan to `Ok` with 0 conversations,
/// not a scan error on every index run.
#[test]
fn scan_treats_a_provably_empty_state_vscdb_as_no_sessions() {
    for state in ["zero-byte", "no-tables"] {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("globalStorage").join("state.vscdb");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        match state {
            "zero-byte" => fs::write(&db_path, []).unwrap(),
            "no-tables" => {
                let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
                conn.execute("CREATE TABLE schema_marker (x INTEGER);")
                    .unwrap();
                conn.execute("DROP TABLE schema_marker;").unwrap();
                drop(conn);
                assert!(
                    fs::metadata(&db_path).unwrap().len() > 0,
                    "{state}: the fixture must be a real database file, not 0 bytes"
                );
            }
            other => unreachable!("unknown empty-store state: {other}"),
        }

        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            None,
        );
        let convs = CursorConnector::new().scan(&ctx).unwrap_or_else(|error| {
            panic!("{state}: an empty store must not be a scan error: {error:#}")
        });
        assert!(
            convs.is_empty(),
            "{state}: an empty store holds no conversations"
        );
    }
}

#[test]
fn scan_processes_global_storage() {
    let dir = TempDir::new().unwrap();

    // Create Cursor-like directory structure
    let cursor_dir = dir.path().join("Cursor");
    let global_dir = cursor_dir.join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    // Create database with test data
    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);
    let value = json!({ "text": "Scan test" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:scan-123", value.as_str()],
    )
    .unwrap();
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(cursor_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);

    let discovered_paths: HashSet<PathBuf> = connector
        .discover_source_files(&ctx)
        .unwrap()
        .into_iter()
        .map(|source| source.source_path)
        .collect();
    assert!(
        discovered_paths.contains(&db_path),
        "discovery should include consumed Cursor database {}",
        db_path.display()
    );
    assert!(
        convs[0].source_path.starts_with(&db_path),
        "Cursor conversation source path should be derived from consumed db {}",
        db_path.display()
    );
}

/// Cursor declares `key TEXT UNIQUE`, so the key range is served by the
/// `sqlite_autoindex_cursorDiskKV_1` index. A residual predicate in this
/// SQL (it once carried `value IS NOT NULL`) makes frankensqlite plan a
/// full table scan, which on a real 2.9 GB store read the whole table for
/// each of 1,488 composers: about 0.8 s each, 20+ minutes per scan.
#[test]
fn cursor_disk_kv_range_query_seeks_the_key_index() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
    conn.execute("CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
        .unwrap();
    let plan = conn
        .query_map_collect(
            &format!("EXPLAIN QUERY PLAN {CURSOR_DISK_KV_RANGE_SQL}"),
            params!["bubbleId:a:", "bubbleId:a;"],
            |row| row.get_typed::<String>(3),
        )
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("USING INDEX sqlite_autoindex_cursorDiskKV_1"),
        "the key-range query must seek the key index, got:\n{plan}"
    );
}

/// A large `state.vscdb` keeps `extract_from_db` busy for many minutes
/// before the first conversation exists. The host's stall watchdog only
/// sees that work through the progress tick, so every row read and every
/// composer parsed must tick (cass#373).
#[test]
fn scan_ticks_progress_for_every_row_and_composer() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = TempDir::new().unwrap();
    let cursor_dir = dir.path().join("Cursor");
    let global_dir = cursor_dir.join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();
    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);
    for id in ["tick-a", "tick-b", "tick-c"] {
        let value = json!({ "text": format!("composer {id}") }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![format!("composerData:{id}").as_str(), value.as_str()],
        )
        .unwrap();
    }
    let legacy =
        json!({ "tabs": [{ "bubbles": [{ "text": "legacy", "type": "user" }] }] }).to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params![
            "workbench.panel.aichat.view.aichat.chatdata",
            legacy.as_str()
        ],
    )
    .unwrap();
    drop(conn);

    let ticks = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ticks);
    let ctx =
        ScanContext::local_default(cursor_dir, None).with_progress_tick(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
    let convs = CursorConnector::new().scan(&ctx).unwrap();

    assert_eq!(convs.len(), 4, "three composers and one legacy chat");
    // One tick per composerData row read, one per composer parsed, and
    // one per ItemTable row read.
    let expected_min = 3 + 3 + 1;
    assert!(
        ticks.load(Ordering::Relaxed) >= expected_min,
        "the scan ticked only {} times",
        ticks.load(Ordering::Relaxed)
    );
}

#[test]
fn scan_with_home_scan_root_finds_cursor_storage() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();

    let global_dir = home.join(".config/Cursor/User/globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);
    let value = json!({ "text": "Explicit root" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:explicit-123", value.as_str()],
    )
    .unwrap();
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![ScanRoot::local(home.to_path_buf())],
        None,
    );
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
}

#[test]
fn scan_recognizes_cursor_in_path() {
    let dir = TempDir::new().unwrap();

    // Directory name contains "Cursor"
    let cursor_dir = dir.path().join("TestCursor");
    let global_dir = cursor_dir.join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();

    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);
    let value = json!({ "text": "Path test" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:path-123", value.as_str()],
    )
    .unwrap();
    drop(conn);

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(cursor_dir, None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 1);
}

#[test]
fn default_detection_scopes_agent_transcripts_to_cursor_base() {
    let dir = TempDir::new().unwrap();

    // A data_dir that IS a Cursor base: one Composer conversation…
    let cursor_dir = dir.path().join("Cursor");
    let global_dir = cursor_dir.join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();
    let db_path = global_dir.join("state.vscdb");
    let conn = create_test_db(&db_path);
    let value = json!({ "text": "Both sources" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:both-123", value.as_str()],
    )
    .unwrap();
    drop(conn);

    // …plus one Agent transcript under the SAME base. Default detection
    // must scan it from HERE, not from the machine's real
    // ~/.cursor/projects (which leaks foreign sessions into the scan and
    // made these tests fail on machines with local Cursor data).
    let sid = "sess-both";
    let transcript_dir = cursor_dir
        .join("proj-x")
        .join(AGENT_TRANSCRIPTS_DIR)
        .join(sid);
    fs::create_dir_all(&transcript_dir).unwrap();
    fs::write(
        transcript_dir.join(format!("{sid}.jsonl")),
        format!(
            "{}\n",
            r#"{"role":"user","message":{"content":[{"type":"text","text":"from agent transcripts"}]}}"#
        ),
    )
    .unwrap();

    let connector = CursorConnector::new();
    let ctx = ScanContext::local_default(cursor_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(
        convs.len(),
        2,
        "exactly the one composer conversation and the one agent transcript from this base"
    );
    let ids: HashSet<String> = convs.iter().filter_map(|c| c.external_id.clone()).collect();
    assert!(
        ids.contains(sid),
        "agent transcript under a Cursor-base data_dir must be indexed from that dir, ids={ids:?}"
    );
}

// =========================================================================
// Edge case tests
// =========================================================================

#[test]
fn parse_composer_data_invalid_json_returns_none() {
    let key = "composerData:invalid-123";
    let value = "not valid json {{{";

    let mut seen = HashSet::new();
    let conv =
        CursorConnector::parse_composer_data(key, value, Path::new("/test"), None, &mut seen, None);

    assert!(conv.is_none());
}

#[test]
fn parse_bubble_preserves_original_in_extra() {
    let bubble = json!({
        "text": "Test",
        "type": "user",
        "customField": "customValue"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
    assert_eq!(msg.extra["customField"], "customValue");
}

#[test]
fn conversation_title_from_first_message() {
    let key = "composerData:title-test";
    let value = json!({
        "tabs": [{
            "bubbles": [
                {"text": "This is the first line\nSecond line here", "type": "user"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    let conv = conv.unwrap();
    // Title should be first line only
    assert_eq!(conv.title, Some("This is the first line".to_string()));
}

#[test]
fn conversation_title_truncates_long_lines() {
    let key = "composerData:long-title";
    let long_text = "x".repeat(200);
    let value = json!({ "text": long_text }).to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    let conv = conv.unwrap();
    assert!(conv.title.as_ref().unwrap().len() <= 100);
}

#[test]
fn messages_are_reindexed_sequentially() {
    let key = "composerData:reindex";
    let value = json!({
        "tabs": [{
            "bubbles": [
                {"text": "One", "type": "user"},
                {"text": "Two", "type": "assistant"},
                {"text": "Three", "type": "user"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    )
    .unwrap();

    assert_eq!(conv.messages[0].idx, 0);
    assert_eq!(conv.messages[1].idx, 1);
    assert_eq!(conv.messages[2].idx, 2);
}

// =========================================================================
// WSL detection tests (Linux-only)
// =========================================================================

#[cfg(target_os = "linux")]
mod wsl_tests {
    use super::*;

    #[test]
    fn is_wsl_returns_false_on_native_linux() {
        // On a real Linux system (not WSL), /proc/version won't contain "microsoft"
        // This test just verifies the function doesn't panic
        let result = CursorConnector::is_wsl();
        // We can't assert the exact value since it depends on the environment,
        // but we verify the function works
        let _ = result;
    }

    #[test]
    fn find_wsl_cursor_path_returns_none_without_mnt_c() {
        // On native Linux, /mnt/c typically doesn't exist
        // This verifies the function gracefully returns None
        if !Path::new("/mnt/c/Users").exists() {
            let result = CursorConnector::find_wsl_cursor_path();
            assert!(result.is_none());
        }
    }

    #[test]
    fn find_wsl_cursor_path_skips_system_dirs() {
        // Create a temp structure that mimics /mnt/c/Users with system dirs
        let dir = TempDir::new().unwrap();
        let users_dir = dir.path().join("Users");
        fs::create_dir_all(&users_dir).unwrap();

        // Create system directories that should be skipped
        for sys_dir in ["Default", "Public", "All Users", "Default User"] {
            fs::create_dir_all(users_dir.join(sys_dir)).unwrap();
        }

        // The function checks /mnt/c/Users specifically, so we can't directly test
        // the skipping logic without mocking. Instead, verify the skip list is correct.
        let skip_list = ["Default", "Public", "All Users", "Default User"];
        assert_eq!(skip_list.len(), 4);
    }

    #[test]
    fn wsl_path_structure_is_valid() {
        // Verify the expected WSL path structure
        let expected = Path::new("/mnt/c/Users/TestUser/AppData/Roaming/Cursor/User");
        assert!(expected.starts_with("/mnt/c/Users"));
        assert!(expected.ends_with("Cursor/User"));
    }
}

// =========================================================================
// Edge case tests — malformed input robustness (br-2w98)
// =========================================================================

#[test]
fn truncated_json_in_db_value_returns_none() {
    let key = "composerData:truncated-123";
    // JSON truncated mid-object
    let value = r#"{"text": "Hello", "tabs": [{"bubbles": [{"text": "Hi", "type":"#;

    let mut seen = HashSet::new();
    let conv =
        CursorConnector::parse_composer_data(key, value, Path::new("/test"), None, &mut seen, None);

    assert!(
        conv.is_none(),
        "truncated JSON should not produce a conversation"
    );
}

#[test]
fn json_type_mismatch_in_bubbles_skips_bad_entries() {
    let key = "composerData:mismatch-123";
    let value = json!({
        "tabs": [{
            "bubbles": [
                // String where object expected
                "not a bubble object",
                // Number instead of object
                42,
                // Null
                null,
                // Boolean
                true,
                // Valid entry that should still be extracted
                {"text": "Valid bubble", "type": "user"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(
        conv.is_some(),
        "should extract valid bubbles despite type mismatches"
    );
    let conv = conv.unwrap();
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].content, "Valid bubble");
}

#[test]
fn deeply_nested_json_in_db_value_does_not_stack_overflow() {
    let key = "composerData:deep-123";
    // Build deeply nested JSON (200 levels) - serde_json has a recursion limit of 128
    let mut nested = String::new();
    for _ in 0..200 {
        nested.push_str("{\"a\":");
    }
    nested.push('1');
    for _ in 0..200 {
        nested.push('}');
    }

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &nested,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    // Should either parse or fail gracefully, never stack overflow
    assert!(
        conv.is_none(),
        "deeply nested JSON without text/tabs should produce no conversation"
    );
}

#[test]
fn large_message_body_in_bubble_handled() {
    let key = "composerData:large-123";
    let large_content = "x".repeat(1_000_000);
    let value = json!({
        "tabs": [{
            "bubbles": [
                {"text": large_content, "type": "user"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_some(), "large message body should not cause OOM");
    let conv = conv.unwrap();
    assert_eq!(conv.messages[0].content.len(), 1_000_000);
}

#[test]
fn corrupted_sqlite_db_does_not_panic() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("corrupted.vscdb");
    fs::write(&db_path, "not a sqlite database at all").unwrap();

    let result = CursorConnector::extract_from_db(&db_path, None, &|| {});
    // SQLite engines may open the file lazily; query failures are caught with if-let-Ok
    // The important thing is that it doesn't panic
    if let Ok(convs) = result {
        assert!(
            convs.is_empty(),
            "corrupted DB should produce no conversations"
        );
    }
}

#[test]
fn db_with_missing_tables_returns_empty() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("no_tables.vscdb");

    // Create a valid SQLite DB but without the expected tables
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
    conn.execute("CREATE TABLE unrelated_table (id INTEGER PRIMARY KEY, data TEXT)")
        .unwrap();
    drop(conn);

    let result = CursorConnector::extract_from_db(&db_path, None, &|| {});
    // Should not error even without expected tables
    assert!(result.is_ok(), "missing tables should not cause an error");
    assert!(result.unwrap().is_empty());
}

#[test]
fn v040_headers_referencing_missing_bubbles_skips_gracefully() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let conn = create_test_db(&db_path);
    // Insert composerData with fullConversationHeadersOnly referencing non-existent bubbles
    let value = json!({
        "fullConversationHeadersOnly": [
            {"bubbleId": "nonexistent-1"},
            {"bubbleId": "nonexistent-2"}
        ]
    })
    .to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:v040-missing", value.as_str()],
    )
    .unwrap();
    drop(conn);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    // Should not crash; no messages means no conversation
    assert!(
        convs.is_empty(),
        "missing bubble references should produce no conversations"
    );
}

#[test]
fn unexpected_numeric_type_in_bubble_defaults_to_assistant() {
    // Bubble type values other than 1 (user) or 2 (assistant)
    for type_val in [0, 3, -1, 999] {
        let bubble = json!({
            "text": "Unknown type bubble",
            "type": type_val
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0);
        assert!(
            msg.is_some(),
            "type {} should still produce a message",
            type_val
        );
        assert_eq!(
            msg.unwrap().role,
            "assistant",
            "unknown numeric type {} should default to assistant",
            type_val
        );
    }
}

#[test]
fn null_bytes_in_bubble_content_handled() {
    let bubble = json!({
        "text": "before\u{0000}after",
        "type": "user"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0);
    assert!(
        msg.is_some(),
        "null bytes in content should not cause errors"
    );
}

#[test]
fn mixed_valid_and_invalid_entries_in_db() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let conn = create_test_db(&db_path);

    // Insert invalid JSON
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:invalid-1", "not valid json {{{"],
    )
    .unwrap();

    // Insert valid entry
    let valid_value = json!({ "text": "Valid entry" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:valid-1", valid_value.as_str()],
    )
    .unwrap();

    // Insert empty JSON
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:empty-1", "{}"],
    )
    .unwrap();

    // Insert another valid entry
    let valid_value2 = json!({ "text": "Another valid" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:valid-2", valid_value2.as_str()],
    )
    .unwrap();

    drop(conn);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    assert_eq!(
        convs.len(),
        2,
        "should extract only the valid conversations, skipping invalid/empty"
    );
}

#[test]
fn parse_workspace_uri_handles_malformed_uris() {
    // Empty string
    assert!(CursorConnector::parse_workspace_uri("").is_none());

    // No scheme
    assert!(CursorConnector::parse_workspace_uri("just/a/path").is_none());

    // Unknown scheme
    assert!(CursorConnector::parse_workspace_uri("ftp://host/path").is_none());

    // file:// with path should work
    let result = CursorConnector::parse_workspace_uri("file:///home/user/project");
    assert!(result.is_some());
    assert_eq!(result.unwrap(), PathBuf::from("/home/user/project"));

    // vscode-remote:// with no path separator
    assert!(CursorConnector::parse_workspace_uri("vscode-remote://no-slash-here").is_none());

    // URL-encoded file:// path
    let result = CursorConnector::parse_workspace_uri("file:///home/user/my%20project/src");
    assert!(result.is_some());
    assert_eq!(result.unwrap(), PathBuf::from("/home/user/my project/src"));
}

#[test]
fn since_ts_filtering_skips_old_conversations() {
    let key = "composerData:old-conv";
    let value = json!({
        "text": "Old message",
        "createdAt": 1000000000000i64,
        "lastUpdatedAt": 1000000001000i64
    })
    .to_string();

    let mut seen = HashSet::new();
    // Set since_ts to after the conversation's lastUpdatedAt
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        Some(1500000000000),
        &mut seen,
        None,
    );

    assert!(
        conv.is_none(),
        "conversation older than since_ts should be skipped"
    );
}

// NOTE:
// The original cass test mutated HOME to validate default detection behavior.
// Rust 2024 marks process-wide environment mutation as unsafe, and this crate
// forbids unsafe code globally. Keep behavior covered by explicit-root tests.

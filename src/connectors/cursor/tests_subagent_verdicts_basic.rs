use std::collections::HashMap;
use std::fs;

use frankensqlite::params;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::CursorConnector;
use super::tests_support::{cursor_store, cursor_verdicts, verdict_test_db};
use crate::connectors::scan::{ScanContext, ScanRoot};
use crate::connectors::sqlite_sync::ConnectionExt;
use crate::connectors::{Connector, parse_timestamp};

#[test]
fn composer_listed_as_subagent_gets_verdict_and_parent() {
    let parent = "11111111-1111-4111-8111-111111111111";
    let child = "22222222-2222-4222-8222-222222222222";
    let parent_val = json!({
        "text": "parent prompt",
        "subagentComposerIds": [child],
        "createdAt": "2026-11-12T00:00:00Z",
        "lastUpdatedAt": "2026-11-12T00:00:00Z"
    });
    let child_val = json!({
        "text": "child prompt",
        "createdAt": "2026-11-12T00:00:00Z",
        "lastUpdatedAt": "2026-11-12T00:00:00Z"
    });
    // The child's own header explicitly says false, but the parent's
    // subagentComposerIds listing proves it a subagent: positive wins.
    let (_dir, db) = cursor_store(
        &[(parent, &parent_val), (child, &child_val)],
        Some(&[(parent, 0), (child, 0)]),
    );
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
    assert_eq!(v[parent]["is_subagent"], json!(false));
    assert!(v[parent].get("parent_id").is_none());
}

#[test]
fn own_header_explicit_false_is_main() {
    let id = "33333333-3333-4333-8333-333333333333";
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});
    let (_dir, db) = cursor_store(&[(id, &val)], Some(&[(id, 0)]));
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[id]["is_subagent"], json!(false));
    assert!(v[id].get("parent_id").is_none());
}

#[test]
fn composer_headers_is_subagent_flag_proves_subagent() {
    let id = "44444444-4444-4444-8444-444444444444";
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});
    let (_dir, db) = cursor_store(&[(id, &val)], Some(&[(id, 1)]));
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[id]["is_subagent"], json!(true));
    assert!(v[id].get("parent_id").is_none());
}

#[test]
fn incremental_scan_keeps_subagent_verdict_stable() {
    let parent = "55555555-5555-4555-8555-555555555555";
    let child = "66666666-6666-4666-8666-666666666666";
    let parent_val = json!({
        "text": "p",
        "subagentComposerIds": [child],
        "createdAt": "2026-11-12T00:00:00Z",
        "lastUpdatedAt": "2026-11-12T00:00:00Z"
    });
    let child_val = json!({
        "text": "c",
        "createdAt": "2026-11-12T01:00:00Z",
        "lastUpdatedAt": "2026-11-12T01:00:00Z"
    });
    let (_dir, db) = cursor_store(&[(parent, &parent_val), (child, &child_val)], None);
    let full = cursor_verdicts(&db, None);
    // A cutoff between the stale parent and the fresh child: only the
    // child is emitted, but the child->parent map still spans the whole
    // store, so the verdict must equal the full scan's.
    let since = parse_timestamp(&json!("2026-11-12T00:30:00Z")).unwrap();
    let incr = cursor_verdicts(&db, Some(since));
    assert!(incr.contains_key(child));
    assert!(!incr.contains_key(parent), "stale parent is not emitted");
    assert_eq!(incr[child]["is_subagent"], json!(true));
    assert_eq!(incr[child]["parent_id"], json!(parent));
    assert_eq!(incr[child]["is_subagent"], full[child]["is_subagent"]);
    assert_eq!(incr[child]["parent_id"], full[child]["parent_id"]);
}

#[test]
fn uninterpretable_composer_row_withholds_store_false() {
    // Round-9 N6 ruling: a captured composerData row whose payload FAD
    // cannot interpret is not an absent row — it might have held a
    // subagentComposerIds listing for ANY composer, so it withholds
    // every false this store would certify. Decodable neighbours keep
    // their evidence and their emission; a flagless composer stays
    // absent either way.
    let good = "77777777-7777-4777-8777-777777777777";
    let noflag = "77777777-7777-4777-8777-77777777aaab";
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [good, noflag] {
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    }
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:broken", "{not json"],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![good, 0i64],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert!(
        v[good].get("is_subagent").is_none(),
        "an uninterpretable captured row withholds every false in the \
         store — `good`'s own-header false is uncertain, not emitted"
    );
    assert!(
        v[noflag].get("is_subagent").is_none(),
        "a composer with no own-header flag must not guess false"
    );
}

#[test]
fn composer_headers_parent_composer_id_without_is_subagent_proves_subagent() {
    let parent = "88888888-8888-4888-8888-888888888881";
    let child = "88888888-8888-4888-8888-888888888882";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    let child_val = json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"});
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            child_val.to_string().as_str()
        ],
    )
    .unwrap();
    // Table has parentComposerId without isSubagent.
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, parentComposerId TEXT)",
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, parentComposerId) VALUES (?, ?)",
        params![child, parent],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
}

#[test]
fn own_is_subagent_true_falls_back_to_header_parent_id() {
    let parent = "88888888-8888-4888-8888-888888888883";
    let child = "88888888-8888-4888-8888-888888888884";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    let child_val = json!({
        "text": "child",
        "isSubagent": true,
        "createdAt": "2026-11-12T00:00:00Z"
    });
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            child_val.to_string().as_str()
        ],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT)",
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId) VALUES (?, ?, ?)",
        params![child, 1i64, parent],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
}

#[test]
fn composer_headers_value_json_proves_subagent_and_parent() {
    let parent = "88888888-8888-4888-8888-888888888885";
    let child = "88888888-8888-4888-8888-888888888886";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    let child_val = json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"});
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            child_val.to_string().as_str()
        ],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, value TEXT)",
    )
    .unwrap();
    let header_json = json!({
        "composerId": child,
        "isSubagent": true,
        "parentId": parent
    })
    .to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
        params![child, header_json.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
}

#[test]
fn item_table_composer_headers_proves_subagent_and_parent() {
    let parent = "88888888-8888-4888-8888-888888888887";
    let child = "88888888-8888-4888-8888-888888888888";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    let child_val = json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"});
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            child_val.to_string().as_str()
        ],
    )
    .unwrap();
    let item_headers = json!([{
        "composerId": child,
        "isSubagent": true,
        "parentComposerId": parent
    }])
    .to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", item_headers.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
}

#[test]
fn malformed_composer_headers_value_or_item_table_withholds_false() {
    let good = "88888888-8888-4888-8888-888888888889";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{good}").as_str(),
            val.to_string().as_str()
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", "{not valid json"],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert!(
        v[good].get("is_subagent").is_none(),
        "malformed composer.composerHeaders must withhold false verdict"
    );

    // Also verify malformed composerHeaders.value JSON withholds false.
    let dir2 = TempDir::new().unwrap();
    let db2 = dir2.path().join("state.vscdb");
    let conn2 = verdict_test_db(&db2);
    conn2
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{good}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    conn2
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn2
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
            params![good, "{not valid json"],
        )
        .unwrap();
    drop(conn2);
    let v2 = cursor_verdicts(&db2, None);
    assert!(
        v2[good].get("is_subagent").is_none(),
        "malformed composerHeaders.value must withhold false verdict"
    );
}

#[test]
fn cross_db_incremental_scan_keeps_subagent_verdict_identical_to_full() {
    let parent = "99999999-9999-4999-8999-999999999991";
    let child = "99999999-9999-4999-8999-999999999992";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_dir = base.join("globalStorage");
    let ws_dir = base.join("workspaceStorage").join("ws1");
    fs::create_dir_all(&global_dir).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    let global_db = global_dir.join("state.vscdb");
    let ws_db = ws_dir.join("state.vscdb");

    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let since = now_ms - 30_000;
    let stale_ms = now_ms - 120_000;

    let g_conn = verdict_test_db(&global_db);
    let parent_val = json!({
        "text": "parent",
        "subagentComposerIds": [child],
        "createdAt": stale_ms,
        "lastUpdatedAt": stale_ms
    });
    let child_val = json!({
        "text": "child",
        "createdAt": now_ms,
        "lastUpdatedAt": now_ms
    });
    g_conn
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{parent}").as_str(),
                parent_val.to_string().as_str()
            ],
        )
        .unwrap();
    g_conn
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{child}").as_str(),
                child_val.to_string().as_str()
            ],
        )
        .unwrap();
    drop(g_conn);

    // Workspace DB has a mirrored copy of child, modified more recently than globalStorage.
    let w_conn = verdict_test_db(&ws_db);
    w_conn
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{child}").as_str(),
                child_val.to_string().as_str()
            ],
        )
        .unwrap();
    drop(w_conn);

    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(&global_db)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_time))
        .unwrap();

    let full_ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let full = CursorConnector::new().scan(&full_ctx).unwrap();
    let full_child = full
        .iter()
        .find(|c| c.external_id.as_deref() == Some(child))
        .unwrap();

    let incr_ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        Some(since),
    );
    let incr = CursorConnector::new().scan(&incr_ctx).unwrap();
    let incr_child = incr
        .iter()
        .find(|c| c.external_id.as_deref() == Some(child))
        .unwrap();
    assert_eq!(incr_child.metadata["is_subagent"], json!(true));
    assert_eq!(incr_child.metadata["parent_id"], json!(parent));
    assert_eq!(
        incr_child.metadata["is_subagent"],
        full_child.metadata["is_subagent"]
    );
    assert_eq!(
        incr_child.metadata["parent_id"],
        full_child.metadata["parent_id"]
    );
}

#[test]
fn excluded_sibling_db_does_not_block_own_header_false() {
    let good = "99999999-9999-4999-8999-999999999993";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_dir = base.join("globalStorage");
    let ws_dir = base.join("workspaceStorage").join("ws1");
    fs::create_dir_all(&global_dir).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    let global_db = global_dir.join("state.vscdb");
    let ws_db = ws_dir.join("state.vscdb");
    drop(verdict_test_db(&global_db));
    let w_conn = verdict_test_db(&ws_db);
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});
    w_conn
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{good}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    w_conn
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    w_conn
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![good, 0i64],
        )
        .unwrap();
    drop(w_conn);

    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let mut convs = Vec::new();
    CursorConnector::scan_cursor_with_exclusions(&ctx, &[global_db], &mut |conv| {
        convs.push(conv);
        Ok(())
    })
    .unwrap();
    assert_eq!(convs.len(), 1);
    // Round-6: the excluded sibling no longer figures in the verdict; the
    // workspace store's own-header false is emitted on its own.
    assert_eq!(convs[0].metadata["is_subagent"], json!(false));
}

#[test]
fn non_object_composer_data_and_non_string_subagent_ids_withhold_false_and_parent_id_key_works() {
    let good = "99999999-9999-4999-8999-999999999994";
    let parent = "99999999-9999-4999-8999-999999999995";
    let child = "99999999-9999-4999-8999-999999999996";
    // 1) non-object JSON array in composerData
    let dir1 = TempDir::new().unwrap();
    let db1 = dir1.path().join("state.vscdb");
    let c1 = verdict_test_db(&db1);
    c1.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{good}").as_str(),
            json!({"text": "ok", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    c1.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:arr", "[]"],
    )
    .unwrap();
    drop(c1);
    assert!(
        cursor_verdicts(&db1, None)[good]
            .get("is_subagent")
            .is_none()
    );

    // 2) non-string item in subagentComposerIds + parentId fallback on child
    let dir2 = TempDir::new().unwrap();
    let db2 = dir2.path().join("state.vscdb");
    let c2 = verdict_test_db(&db2);
    c2.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{good}").as_str(),
            json!({"text": "ok", "subagentComposerIds": [123], "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    c2.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            json!({"text": "c", "parentId": parent, "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(c2);
    let v2 = cursor_verdicts(&db2, None);
    assert!(v2[good].get("is_subagent").is_none());
    assert_eq!(v2[child]["is_subagent"], json!(true));
    assert_eq!(v2[child]["parent_id"], json!(parent));
}

#[test]
fn lookup_composer_conversations_ticks_during_evidence_scan() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    let sid = "99999999-9999-4999-8999-999999999997";
    for i in 0..5 {
        let key = if i == 0 {
            format!("composerData:{sid}")
        } else {
            format!("composerData:other-{i}")
        };
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                key.as_str(),
                json!({"text": "hi", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    }
    drop(conn);
    let ticks = std::sync::atomic::AtomicUsize::new(0);
    let tick = || {
        ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
    let (found, failures) = CursorConnector::lookup_composer_conversations(
        &[(ScanRoot::local(dir.path().to_path_buf()), db)],
        &[sid.to_string()],
        &tick,
    );
    assert!(failures.is_empty());
    assert!(found.contains_key(sid));
    // 1 (open) + 1 (sid loop) + >= 5 (composerData evidence rows)
    assert!(
        ticks.load(std::sync::atomic::Ordering::Relaxed) >= 7,
        "expected per-row ticks during evidence scan, got {}",
        ticks.load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[test]
fn newer_partial_header_does_not_erase_proven_subagent_or_parent() {
    let parent = "aaaaaaaa-0000-4000-8000-000000000001";
    let child = "aaaaaaaa-0000-4000-8000-000000000002";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT, lastUpdatedAt INTEGER)",
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId, lastUpdatedAt) VALUES (?, ?, ?, ?)",
        params![child, 1i64, parent, 1_700_000_000_000i64],
    )
    .unwrap();
    // A newer legacy ItemTable header that omits isSubagent/parentComposerId
    // must not downgrade the proven subagent or erase its known parent.
    let item = json!([{"composerId": child, "lastUpdatedAt": 1_700_000_001_000i64}]).to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", item.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
}

#[test]
fn conflicting_header_flags_resolve_to_subagent_true() {
    let child = "aaaaaaaa-0000-4000-8000-000000000003";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![child, 0i64],
    )
    .unwrap();
    // Equal (absent) timestamps: an explicit true must win over an
    // explicit false, never the other way around.
    let item = json!([{"composerId": child, "isSubagent": true}]).to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", item.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
}

#[test]
fn cross_db_conflicting_header_flags_resolve_to_subagent_true() {
    let child = "aaaaaaaa-0000-4000-8000-000000000004";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_dir = base.join("globalStorage");
    let ws_dir = base.join("workspaceStorage").join("ws1");
    fs::create_dir_all(&global_dir).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    let global_db = global_dir.join("state.vscdb");
    let ws_db = ws_dir.join("state.vscdb");
    let child_val = json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"});
    let g = verdict_test_db(&global_db);
    g.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    g.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![child, 0i64],
    )
    .unwrap();
    drop(g);
    let w = verdict_test_db(&ws_db);
    w.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            child_val.to_string().as_str()
        ],
    )
    .unwrap();
    w.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    w.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![child, 1i64],
    )
    .unwrap();
    drop(w);
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let c = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(child))
        .unwrap();
    assert_eq!(c.metadata["is_subagent"], json!(true));
}

#[test]
fn header_json_string_is_subagent_is_truthy() {
    // composerHeaders.value JSON: native bool("true") is True.
    let a = "aaaaaaaa-0000-4000-8000-000000000005";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{a}").as_str(),
            json!({"text": "a", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let hj = json!({"composerId": a, "isSubagent": "true"}).to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
        params![a, hj.as_str()],
    )
    .unwrap();
    drop(conn);
    assert_eq!(cursor_verdicts(&db, None)[a]["is_subagent"], json!(true));

    // ItemTable: native bool("1") is True.
    let b = "aaaaaaaa-0000-4000-8000-000000000006";
    let dir2 = TempDir::new().unwrap();
    let db2 = dir2.path().join("state.vscdb");
    let conn2 = verdict_test_db(&db2);
    conn2
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{b}").as_str(),
                json!({"text": "b", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    let item = json!([{"composerId": b, "isSubagent": "1"}]).to_string();
    conn2
        .execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", item.as_str()],
        )
        .unwrap();
    drop(conn2);
    assert_eq!(cursor_verdicts(&db2, None)[b]["is_subagent"], json!(true));

    // Round-3 ruling: "false" decodes to an explicit false, so the
    // composer's own header certifies main (native bool("false") would say
    // true; FAD deviates deliberately — see decode_is_subagent).
    let c = "aaaaaaaa-0000-4000-8000-000000000007";
    let dir3 = TempDir::new().unwrap();
    let db3 = dir3.path().join("globalStorage").join("state.vscdb");
    let conn3 = verdict_test_db(&db3);
    conn3
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{c}").as_str(),
                json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    let item3 = json!([{"composerId": c, "isSubagent": "false"}]).to_string();
    conn3
        .execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", item3.as_str()],
        )
        .unwrap();
    drop(conn3);
    assert_eq!(cursor_verdicts(&db3, None)[c]["is_subagent"], json!(false));
}

#[test]
fn header_uninterpretable_values_withhold_false() {
    let good = "aaaaaaaa-0000-4000-8000-000000000008";
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});

    // A) non-string parentComposerId in composerHeaders.value JSON.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{good}").as_str(),
            val.to_string().as_str()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let hj = json!({"composerId": good, "parentComposerId": 123}).to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
        params![good, hj.as_str()],
    )
    .unwrap();
    drop(conn);
    assert!(
        cursor_verdicts(&db, None)[good]
            .get("is_subagent")
            .is_none(),
        "non-string parentComposerId must withhold false"
    );

    // B) non-string composerId in ItemTable.
    let dir2 = TempDir::new().unwrap();
    let db2 = dir2.path().join("state.vscdb");
    let conn2 = verdict_test_db(&db2);
    conn2
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{good}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    let item = json!([{"composerId": 123, "isSubagent": true}]).to_string();
    conn2
        .execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", item.as_str()],
        )
        .unwrap();
    drop(conn2);
    assert!(
        cursor_verdicts(&db2, None)[good]
            .get("is_subagent")
            .is_none(),
        "non-string composerId must withhold false"
    );

    // C) unknown isSubagent shape (array) in value JSON.
    let dir3 = TempDir::new().unwrap();
    let db3 = dir3.path().join("state.vscdb");
    let conn3 = verdict_test_db(&db3);
    conn3
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{good}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    conn3
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let hj3 = json!({"composerId": good, "isSubagent": [1]}).to_string();
    conn3
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
            params![good, hj3.as_str()],
        )
        .unwrap();
    drop(conn3);
    assert!(
        cursor_verdicts(&db3, None)[good]
            .get("is_subagent")
            .is_none(),
        "array isSubagent must withhold false"
    );

    // D) unknown isSubagent shape (object) in ItemTable.
    let dir4 = TempDir::new().unwrap();
    let db4 = dir4.path().join("state.vscdb");
    let conn4 = verdict_test_db(&db4);
    conn4
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{good}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    let item4 = json!([{"composerId": good, "isSubagent": {"x": 1}}]).to_string();
    conn4
        .execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", item4.as_str()],
        )
        .unwrap();
    drop(conn4);
    assert!(
        cursor_verdicts(&db4, None)[good]
            .get("is_subagent")
            .is_none(),
        "object isSubagent must withhold false"
    );
}

#[test]
fn remote_workspace_mirror_emits_own_header_false() {
    for (source, leaf) in [
        ("~/.config/Cursor/User/workspaceStorage/ws1", "ws1"),
        (
            "~/.config/Cursor/User/workspaceStorage",
            "workspaceStorage/ws1",
        ),
    ] {
        let tmp = TempDir::new().unwrap();
        let container = tmp.path().join("mirror_0123456789abcdef");
        let db = container.join(leaf).join("state.vscdb");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = verdict_test_db(&db);
        let main = "aaaaaaaa-0000-4000-8000-000000000009";
        let parent = "aaaaaaaa-0000-4000-8000-00000000000a";
        let child = "aaaaaaaa-0000-4000-8000-00000000000b";
        for (id, v) in [
            (
                main,
                json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"}),
            ),
            (
                parent,
                json!({"text": "p", "subagentComposerIds": [child], "createdAt": "2026-11-12T00:00:00Z"}),
            ),
            (
                child,
                json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"}),
            ),
        ] {
            conn.execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![
                    format!("composerData:{id}").as_str(),
                    v.to_string().as_str()
                ],
            )
            .unwrap();
        }
        // Round-6: a mirror's own-header false emits false like any store;
        // no global coverage is required.
        conn.execute(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)",
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![main, 0i64],
        )
        .unwrap();
        drop(conn);
        let root = ScanRoot::remote(
            container.clone(),
            crate::types::Origin::remote("laptop"),
            None,
        )
        .with_mirrored_source_path(source);
        let ctx = ScanContext::with_roots(tmp.path().join("state"), vec![root], None);
        let convs = CursorConnector::new().scan(&ctx).unwrap();
        let by_id: HashMap<String, Value> = convs
            .into_iter()
            .filter_map(|c| c.external_id.map(|id| (id, c.metadata)))
            .collect();
        assert_eq!(
            by_id[main]["is_subagent"],
            json!(false),
            "{source}: a mirror's own-header false emits false"
        );
        // Positive evidence inside the mirror still proves subagents.
        assert_eq!(by_id[child]["is_subagent"], json!(true), "{source}");
        assert_eq!(by_id[child]["parent_id"], json!(parent), "{source}");
    }
}

#[test]
fn remote_workspace_mirror_with_global_emits_own_header_false() {
    let tmp = TempDir::new().unwrap();
    let container = tmp.path().join("mirror_0123456789abcdef");
    let ws_db = container.join("ws1").join("state.vscdb");
    let global_db = container.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    let main = "aaaaaaaa-0000-4000-8000-00000000000c";
    let w = verdict_test_db(&ws_db);
    w.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{main}").as_str(),
            json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    w.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    w.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(w);
    let g = verdict_test_db(&global_db);
    g.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            "composerData:global-side",
            json!({"text": "g", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(g);
    let origin = crate::types::Origin::remote("laptop");
    let ws_root = ScanRoot::remote(container.clone(), origin.clone(), None)
        .with_mirrored_source_path("~/.config/Cursor/User/workspaceStorage/ws1");
    let g_root = ScanRoot::remote(container.clone(), origin, None)
        .with_mirrored_source_path("~/.config/Cursor/User/globalStorage");
    let ctx = ScanContext::with_roots(tmp.path().join("state"), vec![ws_root, g_root], None);
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let c = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(main))
        .unwrap();
    // Round-6 ruling: a mirrored store's own-header false emits false; the
    // global store's presence no longer enters the rule.
    assert_eq!(c.metadata["is_subagent"], json!(false));
}

#[test]
fn empty_remote_workspace_mirror_does_not_block_local_own_header_false() {
    let tmp = TempDir::new().unwrap();
    // A remote workspaceStorage mirror root whose leaf was never copied:
    // it contributes no database to the scan.
    let container = tmp.path().join("mirror_0123456789abcdef");
    fs::create_dir_all(&container).unwrap();
    let remote_root = ScanRoot::remote(container, crate::types::Origin::remote("laptop"), None)
        .with_mirrored_source_path("~/.config/Cursor/User/workspaceStorage/ws1");
    // An unrelated local store whose composer has an own-header false.
    let local_dir = tmp.path().join("local");
    let local_db = local_dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(local_db.parent().unwrap()).unwrap();
    let main = "aaaaaaaa-0000-4000-8000-00000000001a";
    let conn = verdict_test_db(&local_db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{main}").as_str(),
            json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(conn);
    let ctx = ScanContext::with_roots(
        tmp.path().join("state"),
        vec![remote_root, ScanRoot::local(local_dir)],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let c = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(main))
        .unwrap();
    assert_eq!(c.metadata["is_subagent"], json!(false));
}

#[test]
fn real_or_text_header_columns_do_not_abort_table_scan() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    // Untyped columns keep the exact storage class inserted (no affinity
    // conversion), so REAL/TEXT values reach the reader as-is.
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT, isSubagent, parentComposerId, lastUpdatedAt)",
    )
    .unwrap();
    let ids = [
        "aaaaaaaa-0000-4000-8000-000000000011",
        "aaaaaaaa-0000-4000-8000-000000000012",
        "aaaaaaaa-0000-4000-8000-000000000013",
        "aaaaaaaa-0000-4000-8000-000000000014",
    ];
    for id in ids {
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                json!({"text": "t", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    }
    let insert = "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId, lastUpdatedAt) VALUES (?, ?, ?, ?)";
    // INTEGER isSubagent + INTEGER lastUpdatedAt (the ordinary shape).
    conn.execute_compat(insert, params![ids[0], 1i64, "p1", 100i64])
        .unwrap();
    // REAL lastUpdatedAt.
    conn.execute_compat(insert, params![ids[1], 1i64, "p2", 200.5f64])
        .unwrap();
    // TEXT lastUpdatedAt (numeric).
    conn.execute_compat(insert, params![ids[2], 1i64, "p3", "300"])
        .unwrap();
    // TEXT isSubagent ("true" is a supported explicit encoding).
    conn.execute_compat(insert, params![ids[3], "true", "p4", 400i64])
        .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    for (id, parent) in ids.iter().copied().zip(["p1", "p2", "p3", "p4"]) {
        assert_eq!(v[id]["is_subagent"], json!(true), "{id}");
        assert_eq!(v[id]["parent_id"], json!(parent), "{id}");
    }
}

#[test]
fn unreadable_sibling_cell_does_not_block_own_header_false() {
    let child = "aaaaaaaa-0000-4000-8000-000000000017";
    let main = "aaaaaaaa-0000-4000-8000-000000000018";
    let parent = "aaaaaaaa-0000-4000-8000-000000000019";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [child, main] {
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                json!({"text": "t", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    }
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT, isSubagent, parentComposerId, lastUpdatedAt)",
    )
    .unwrap();
    let insert = "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId, lastUpdatedAt) VALUES (?, ?, ?, ?)";
    // Unparseable TEXT timestamp: the row is not fully interpretable, but
    // its readable isSubagent/parentComposerId still prove the subagent.
    conn.execute_compat(insert, params![child, 1i64, parent, "not-a-timestamp"])
        .unwrap();
    conn.execute_compat(insert, params![main, 0i64, None::<i64>, 100i64])
        .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(v[child]["is_subagent"], json!(true));
    assert_eq!(v[child]["parent_id"], json!(parent));
    assert_eq!(
        v[main]["is_subagent"],
        json!(false),
        "a readable own-header false is not blocked by a sibling's unreadable cell"
    );
}

use std::collections::HashMap;
use std::fs;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::CursorConnector;
use super::tests_support::{cursor_verdicts, verdict_test_db};
use crate::connectors::Connector;
use crate::connectors::scan::{ScanContext, ScanRoot};
use crate::connectors::sqlite_sync::{Connection, ConnectionExt, params};

#[test]
fn is_subagent_explicit_decoding_across_json_itemtable_and_sql() {
    // Supported true encodings in composerHeaders.value JSON.
    for (i, enc) in [
        json!(true),
        json!(1),
        json!("true"),
        json!("1"),
        json!(" TRUE "),
        json!("True"),
    ]
    .iter()
    .enumerate()
    {
        let id = format!("bbbbbbbb-0000-4000-8000-{i:012}");
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = verdict_test_db(&db);
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
        conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        let hj = json!({"composerId": id, "isSubagent": enc}).to_string();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
            params![id.as_str(), hj.as_str()],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            cursor_verdicts(&db, None)[&id]["is_subagent"],
            json!(true),
            "{enc}"
        );
    }

    // Supported false encodings in ItemTable: the composer's own header
    // certifies main.
    for (i, enc) in [
        json!(false),
        json!(0),
        json!("false"),
        json!("0"),
        json!(" FALSE "),
        json!("False"),
    ]
    .iter()
    .enumerate()
    {
        let id = format!("bbbbbbbb-0000-4000-8001-{i:012}");
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("globalStorage").join("state.vscdb");
        let conn = verdict_test_db(&db);
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
        let item = json!([{"composerId": id, "isSubagent": enc}]).to_string();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", item.as_str()],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            cursor_verdicts(&db, None)[&id]["is_subagent"],
            json!(false),
            "{enc}"
        );
    }

    // SQL columns: INTEGER 1/0 and supported TEXT spellings.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("globalStorage").join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT, isSubagent, lastUpdatedAt)")
        .unwrap();
    let insert =
        "INSERT INTO composerHeaders (composerId, isSubagent, lastUpdatedAt) VALUES (?, ?, ?)";
    let cases: [(&str, &str); 6] = [
        ("bbbbbbbb-0000-4000-8002-000000000001", "1"),
        ("bbbbbbbb-0000-4000-8002-000000000002", "0"),
        ("bbbbbbbb-0000-4000-8002-000000000003", "true"),
        ("bbbbbbbb-0000-4000-8002-000000000004", " FALSE "),
        ("bbbbbbbb-0000-4000-8002-000000000005", "1"),
        ("bbbbbbbb-0000-4000-8002-000000000006", "0"),
    ];
    for (id, _) in cases {
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
    conn.execute_compat(insert, params![cases[0].0, 1i64, 100i64])
        .unwrap();
    conn.execute_compat(insert, params![cases[1].0, 0i64, 100i64])
        .unwrap();
    conn.execute_compat(insert, params![cases[2].0, "true", 100i64])
        .unwrap();
    conn.execute_compat(insert, params![cases[3].0, " FALSE ", 100i64])
        .unwrap();
    conn.execute_compat(insert, params![cases[4].0, "1", 100i64])
        .unwrap();
    conn.execute_compat(insert, params![cases[5].0, "0", 100i64])
        .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    for ((id, _), expected) in cases
        .iter()
        .copied()
        .zip([true, false, true, false, true, false])
    {
        assert_eq!(v[id]["is_subagent"], json!(expected), "{id}");
    }

    // A composerHeaders row whose `value` JSON alone carries the explicit
    // false (no isSubagent column) is an own-header false too.
    let id = "bbbbbbbb-0000-4000-8002-000000000007";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("globalStorage").join("state.vscdb");
    let conn = verdict_test_db(&db);
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
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let hj = json!({"composerId": id, "isSubagent": false}).to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
        params![id, hj.as_str()],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        cursor_verdicts(&db, None)[id]["is_subagent"],
        json!(false),
        "value-JSON own-header false"
    );
}

#[test]
fn is_subagent_unknown_encodings_withhold_false() {
    let good = "bbbbbbbb-0000-4000-8003-000000000001";
    let val = json!({"text": "hello", "createdAt": "2026-11-12T00:00:00Z"});
    // Unknown JSON encodings on an identified header: the flagged composer
    // gets no verdict; others have no own header and also get none.
    for (i, enc) in [
        json!("yes"),
        json!("2"),
        json!(""),
        json!(2),
        json!(-1),
        json!(1.5),
        json!([true]),
        json!({"x": 1}),
    ]
    .iter()
    .enumerate()
    {
        let bad = format!("bbbbbbbb-0000-4000-8013-{i:012}");
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = verdict_test_db(&db);
        for id in [good, bad.as_str()] {
            conn.execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![
                    format!("composerData:{id}").as_str(),
                    val.to_string().as_str()
                ],
            )
            .unwrap();
        }
        let item = json!([{"composerId": bad, "isSubagent": enc}]).to_string();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", item.as_str()],
        )
        .unwrap();
        drop(conn);
        let v = cursor_verdicts(&db, None);
        assert!(v[&bad].get("is_subagent").is_none(), "{enc}");
        assert!(
            v[good].get("is_subagent").is_none(),
            "{enc}: good has no own header"
        );
    }

    // Unknown SQL TEXT / non-integer numeric encodings behave the same.
    for (i, enc) in ["yes", "", "2"].iter().enumerate() {
        let bad = format!("bbbbbbbb-0000-4000-8004-{i:012}");
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = verdict_test_db(&db);
        for id in [good, bad.as_str()] {
            conn.execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![
                    format!("composerData:{id}").as_str(),
                    val.to_string().as_str()
                ],
            )
            .unwrap();
        }
        conn.execute("CREATE TABLE composerHeaders (composerId TEXT, isSubagent)")
            .unwrap();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![bad.as_str(), *enc],
        )
        .unwrap();
        drop(conn);
        let v = cursor_verdicts(&db, None);
        assert!(v[&bad].get("is_subagent").is_none(), "SQL {enc}");
        assert!(
            v[good].get("is_subagent").is_none(),
            "SQL {enc}: good has no own header"
        );
    }

    // composerData own isSubagent only proves true (positive evidence); a
    // false there is not an own-header record, so it yields no verdict.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("globalStorage").join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{good}").as_str(),
            json!({"text": "t", "isSubagent": "false", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(conn);
    assert!(
        cursor_verdicts(&db, None)[good]
            .get("is_subagent")
            .is_none(),
        "composerData own false is not an own-header record"
    );
}

#[test]
fn real_is_subagent_column_is_not_a_supported_encoding() {
    let a = "bbbbbbbb-0000-4000-8005-000000000001";
    let b = "bbbbbbbb-0000-4000-8005-000000000002";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [a, b] {
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
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT, isSubagent)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![a, 1.0f64],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![b, 0.0f64],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert!(v[a].get("is_subagent").is_none(), "REAL 1.0 proves nothing");
    assert!(v[b].get("is_subagent").is_none(), "REAL 0.0 proves nothing");
}

#[test]
fn same_row_sql_and_value_json_conflict_keeps_positive() {
    // SQL true vs value-JSON false / empty / unknown: the positive survives.
    for (i, enc) in [json!(false), json!(""), json!("false"), json!("bogus")]
        .iter()
        .enumerate()
    {
        let id = format!("bbbbbbbb-0000-4000-8006-{i:012}");
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = verdict_test_db(&db);
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
        conn.execute(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, value TEXT)",
        )
        .unwrap();
        let hj = json!({"composerId": id, "isSubagent": enc}).to_string();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent, value) VALUES (?, ?, ?)",
            params![id.as_str(), 1i64, hj.as_str()],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            cursor_verdicts(&db, None)[&id]["is_subagent"],
            json!(true),
            "{enc}"
        );
    }

    // SQL false vs value-JSON true: the positive survives too.
    let id = "bbbbbbbb-0000-4000-8006-0000000000ff";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
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
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, value TEXT)",
    )
    .unwrap();
    let hj = json!({"composerId": id, "isSubagent": true}).to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, value) VALUES (?, ?, ?)",
        params![id, 0i64, hj.as_str()],
    )
    .unwrap();
    drop(conn);
    assert_eq!(cursor_verdicts(&db, None)[id]["is_subagent"], json!(true));
}

/// The value-JSON identity fallback is exercised honestly (round-8 M1):
/// when the SQL `composerId` is empty, the header flag lands only if the
/// value JSON supplies a usable string identity. A string identity
/// attributes the flag (true); a non-string identity attributes nothing,
/// so the emitted composer stays absent — next to a clean own-header
/// false that still emits.
#[test]
fn empty_sql_identity_uses_value_json_identity_for_attribution() {
    let rescued = "bbbbbbbb-0000-4000-8007-000000000001";
    let dropped = "bbbbbbbb-0000-4000-8007-000000000002";
    let main = "bbbbbbbb-0000-4000-8007-0000000000ff";
    for (identity, target, expected) in [
        (json!(rescued), rescued, json!(true)),
        (json!(123), dropped, Value::Null),
    ] {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("state.vscdb");
        let conn = verdict_test_db(&db);
        for id in [target, main] {
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
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, value TEXT)",
        )
        .unwrap();
        // Empty SQL key: the row's flag is attributable only through the
        // value JSON's identity.
        let hj = json!({"composerId": identity, "isSubagent": true}).to_string();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
            params!["", hj.as_str()],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![main, 0i64],
        )
        .unwrap();
        drop(conn);
        let v = cursor_verdicts(&db, None);
        if expected == Value::Null {
            assert!(
                v[target].get("is_subagent").is_none(),
                "a non-string value-JSON identity attributes nothing — \
                 the header flag cannot reach the emitted composer"
            );
        } else {
            assert_eq!(
                v[target]["is_subagent"], expected,
                "a string value-JSON identity attributes the flag to the composer"
            );
        }
        assert_eq!(
            v[main]["is_subagent"],
            json!(false),
            "a clean own-header false in the same store still emits"
        );
    }
}

#[test]
fn item_table_identity_less_header_leaves_verdict_absent() {
    let good = "bbbbbbbb-0000-4000-8008-000000000001";
    let val = json!({"text": "t", "createdAt": "2026-11-12T00:00:00Z"});
    for item in [
        json!([{"isSubagent": true, "parentComposerId": "P"}]),
        json!([{"name": "no identity"}]),
        json!([{"composerId": ""}]),
    ] {
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
        let body = item.to_string();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params!["composer.composerHeaders", body.as_str()],
        )
        .unwrap();
        drop(conn);
        assert!(
            cursor_verdicts(&db, None)[good]
                .get("is_subagent")
                .is_none(),
            "{item}: an unidentifiable header is not attributed to anyone"
        );
    }
}

#[test]
fn empty_sql_id_with_malformed_value_leaves_verdict_absent() {
    let good = "bbbbbbbb-0000-4000-8009-000000000001";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{good}").as_str(),
            json!({"text": "t", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT, value TEXT)")
        .unwrap();
    // NULL identity: the row cannot be attributed, but its malformed value
    // must still be validated instead of skipped.
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
        params![None::<&str>, "{not json"],
    )
    .unwrap();
    drop(conn);
    assert!(
        cursor_verdicts(&db, None)[good]
            .get("is_subagent")
            .is_none(),
        "malformed value on an empty-ID row attributes nothing"
    );
}

#[test]
fn header_evidence_survives_missing_cursor_disk_kv() {
    let parent = "bbbbbbbb-0000-4000-800b-000000000001";
    let child = "bbbbbbbb-0000-4000-800b-000000000002";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_dir = base.join("globalStorage");
    let ws_dir = base.join("workspaceStorage").join("ws1");
    fs::create_dir_all(&global_dir).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    let global_db = global_dir.join("state.vscdb");
    let ws_db = ws_dir.join("state.vscdb");
    // Global store: header proof only, no cursorDiskKV table at all.
    let g = Connection::open(global_db.to_string_lossy().as_ref()).unwrap();
    g.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT)",
    )
    .unwrap();
    g.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId) VALUES (?, ?, ?)",
        params![child, 1i64, parent],
    )
    .unwrap();
    drop(g);
    let w = verdict_test_db(&ws_db);
    w.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{child}").as_str(),
            json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(w);
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let mut convs = Vec::new();
    let res = CursorConnector::new().scan_with_callback(&ctx, &mut |conv| {
        convs.push(conv);
        Ok(())
    });
    // Extraction tolerates the missing-table query failure (warn only),
    // but the independently readable header proof must still reach the
    // workspace conversation through the cross-DB evidence.
    assert!(res.is_ok());
    let c = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(child))
        .expect("workspace child is emitted");
    assert_eq!(c.metadata["is_subagent"], json!(true));
    assert_eq!(c.metadata["parent_id"], json!(parent));
}

#[test]
fn empty_sql_identity_withholds_false_and_keeps_parent() {
    let main = "cccccccc-0000-4000-8000-000000000001";
    let c = "cccccccc-0000-4000-8000-000000000002";
    let parent = "cccccccc-0000-4000-8000-000000000003";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [main, c] {
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
        "CREATE TABLE composerHeaders (composerId TEXT, isSubagent INTEGER, parentComposerId TEXT, value TEXT)",
    )
    .unwrap();
    // NULL SQL identity, but a fully readable value JSON naming C.
    let hj = json!({"composerId": c, "isSubagent": true, "parentComposerId": parent}).to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId, value) VALUES (?, ?, ?, ?)",
        params![None::<&str>, 1i64, parent, hj.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(
        v[c]["is_subagent"],
        json!(true),
        "positive evidence must be kept"
    );
    assert_eq!(
        v[c]["parent_id"],
        json!(parent),
        "parent evidence must be kept"
    );
    assert!(
        v[main].get("is_subagent").is_none(),
        "main has no own-header flag and stays absent"
    );
}

#[test]
fn item_table_usable_composer_id_attributes_header() {
    let main = "cccccccc-0000-4000-8001-000000000001";
    let c = "cccccccc-0000-4000-8001-000000000002";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [main, c] {
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
    // composerId is usable; the non-string fallback id does not block attribution.
    let item = json!([{"composerId": c, "id": 123, "isSubagent": false}]).to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", item.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(
        v[c]["is_subagent"],
        json!(false),
        "a usable composerId attributes the own-header false"
    );
    assert!(
        v[main].get("is_subagent").is_none(),
        "main has no own header"
    );
}

#[test]
fn remote_mirror_emits_own_header_false_despite_foreign_global() {
    let tmp = TempDir::new().unwrap();
    let container = tmp.path().join("mirror_0123456789abcdef");
    let ws_db = container.join("ws1").join("state.vscdb");
    let global_db = container.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    let main = "cccccccc-0000-4000-8002-000000000001";
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
    drop(verdict_test_db(&global_db));
    let origin = crate::types::Origin::remote("laptop");
    // Same destination container, but the global root is a DIFFERENT remote
    // Cursor/User installation than the workspace root.
    let ws_root = ScanRoot::remote(container.clone(), origin.clone(), None)
        .with_mirrored_source_path("~/.config/Cursor/User/workspaceStorage/ws1");
    let g_root = ScanRoot::remote(container.clone(), origin, None)
        .with_mirrored_source_path("/opt/secondary/Cursor/User/globalStorage");
    let ctx = ScanContext::with_roots(tmp.path().join("state"), vec![ws_root, g_root], None);
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let c = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(main))
        .unwrap();
    // Round-6: global provenance is irrelevant; the mirror's own-header
    // false emits false on its own.
    assert_eq!(c.metadata["is_subagent"], json!(false));
}

#[test]
fn whole_cursor_remote_mirror_emits_own_header_false_and_keeps_positive() {
    let tmp = TempDir::new().unwrap();
    let container = tmp.path().join("mirror_0123456789abcdef");
    let ws_db = container.join("ws1").join("state.vscdb");
    let global_db = container
        .join("Cursor/User/globalStorage")
        .join("state.vscdb");
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    let main = "cccccccc-0000-4000-8003-000000000001";
    let parent = "cccccccc-0000-4000-8003-000000000002";
    let child = "cccccccc-0000-4000-8003-000000000003";
    let w = verdict_test_db(&ws_db);
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
        w.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                v.to_string().as_str()
            ],
        )
        .unwrap();
    }
    w.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    w.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(w);
    drop(verdict_test_db(&global_db));
    let origin = crate::types::Origin::remote("laptop");
    let ws_root = ScanRoot::remote(container.clone(), origin.clone(), None)
        .with_mirrored_source_path("~/.config/Cursor/User/workspaceStorage/ws1");
    // Application-home source (whole Cursor), supported by storage_roots.
    let home_root = ScanRoot::remote(container.clone(), origin, None)
        .with_mirrored_source_path("~/.config/Cursor");
    let ctx = ScanContext::with_roots(tmp.path().join("state"), vec![ws_root, home_root], None);
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let by_id: HashMap<String, Value> = convs
        .into_iter()
        .filter_map(|c| c.external_id.map(|id| (id, c.metadata)))
        .collect();
    // Round-6: remote stores emit an own-header false like any store.
    assert_eq!(by_id[main]["is_subagent"], json!(false));
    assert_eq!(by_id[child]["is_subagent"], json!(true));
    assert_eq!(by_id[child]["parent_id"], json!(parent));
}

#[test]
fn explicit_null_is_subagent_withholds_false() {
    let main = "cccccccc-0000-4000-8004-000000000001";
    let c = "cccccccc-0000-4000-8004-000000000002";
    let val = json!({"text": "t", "createdAt": "2026-11-12T00:00:00Z"});

    // ItemTable header with explicit null flag.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [main, c] {
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    }
    let item = json!([{"composerId": c, "isSubagent": null}]).to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", item.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert!(v[c].get("is_subagent").is_none(), "null flag must withhold");
    assert!(
        v[main].get("is_subagent").is_none(),
        "main has no own-header flag"
    );

    // composerData own evidence with explicit null flag.
    let dir2 = TempDir::new().unwrap();
    let db2 = dir2.path().join("state.vscdb");
    let conn2 = verdict_test_db(&db2);
    conn2
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{c}").as_str(),
                json!({"text": "t", "isSubagent": null, "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    drop(conn2);
    assert!(
        cursor_verdicts(&db2, None)[c].get("is_subagent").is_none(),
        "own null flag must withhold"
    );

    // composerHeaders.value JSON with explicit null flag.
    let dir3 = TempDir::new().unwrap();
    let db3 = dir3.path().join("state.vscdb");
    let conn3 = verdict_test_db(&db3);
    for id in [main, c] {
        conn3
            .execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![
                    format!("composerData:{id}").as_str(),
                    val.to_string().as_str()
                ],
            )
            .unwrap();
    }
    conn3
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let hj = json!({"composerId": c, "isSubagent": null}).to_string();
    conn3
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, value) VALUES (?, ?)",
            params![c, hj.as_str()],
        )
        .unwrap();
    drop(conn3);
    let v3 = cursor_verdicts(&db3, None);
    assert!(
        v3[c].get("is_subagent").is_none(),
        "value-JSON null flag must withhold"
    );
    assert!(
        v3[main].get("is_subagent").is_none(),
        "main has no own-header flag"
    );
}

#[test]
fn unread_workspace_stores_do_not_block_own_header_false() {
    let main = "cccccccc-0000-4000-8005-000000000001";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_db = base.join("globalStorage").join("state.vscdb");
    let ws_db = base
        .join("workspaceStorage")
        .join("ws1")
        .join("state.vscdb");
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    let g = verdict_test_db(&global_db);
    g.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{main}").as_str(),
            json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    g.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    g.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(g);
    drop(verdict_test_db(&ws_db));
    // Scan only the global store; the workspace sibling exists but is
    // unread. Round-6: store inventory no longer enters the verdict — the
    // global store's own-header false emits false on its own.
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(global_db.parent().unwrap().to_path_buf())],
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
fn local_full_installation_emits_own_header_false() {
    let main = "cccccccc-0000-4000-8006-000000000001";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_db = base.join("globalStorage").join("state.vscdb");
    let ws_db = base
        .join("workspaceStorage")
        .join("ws1")
        .join("state.vscdb");
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    let g = verdict_test_db(&global_db);
    g.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{main}").as_str(),
            json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    g.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    g.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(g);
    drop(verdict_test_db(&ws_db));
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let c = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(main))
        .unwrap();
    assert_eq!(c.metadata["is_subagent"], json!(false));
}

#[cfg(unix)]
#[test]
fn unreadable_workspace_dir_does_not_block_own_header_false() {
    use std::os::unix::fs::PermissionsExt;
    // Round-7 M1 migration: the enumeration-completeness gate this test
    // pinned is gone; the denied directory still blocks ws1's discovery
    // (root = base, so the workspace store IS on the scan path), but an
    // own-header false in the global store no longer needs it.
    let c = "dddddddd-0000-4000-8000-000000000001";
    let p = "dddddddd-0000-4000-8000-000000000002";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_db = base.join("globalStorage").join("state.vscdb");
    let ws_dir = base.join("workspaceStorage");
    let ws_db = ws_dir.join("ws1").join("state.vscdb");
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    let g = verdict_test_db(&global_db);
    g.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    g.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    g.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(g);
    let w = verdict_test_db(&ws_db);
    w.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{p}").as_str(),
            json!({"text": "p", "subagentComposerIds": [c], "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(w);
    // Deny enumeration of the workspace directory — it stays on the scan
    // path (the root covers the whole installation), so the ws store is
    // discovered-then-skipped every run.
    fs::set_permissions(&ws_dir, fs::Permissions::from_mode(0o000)).unwrap();
    // Only run the assertion when the OS enforces directory permissions
    // (e.g. non-root). Restore permissions before TempDir drops.
    let enforced = fs::read_dir(&ws_dir).is_err();
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    fs::set_permissions(&ws_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if enforced {
        let cc = convs
            .iter()
            .find(|x| x.external_id.as_deref() == Some(c))
            .unwrap();
        assert_eq!(
            cc.metadata["is_subagent"],
            json!(false),
            "an unreadable sibling workspace dir cannot withhold an own-header false"
        );
    }
}

#[test]
fn missing_global_store_does_not_block_own_header_false() {
    // Round-7 M1 migration: a missing global store used to gate false via
    // completeness; now only the composer's own header matters.
    let c = "dddddddd-0000-4000-8001-000000000001";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let ws_db = base
        .join("workspaceStorage")
        .join("ws1")
        .join("state.vscdb");
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    let w = verdict_test_db(&ws_db);
    w.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    w.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    w.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(w);
    // No globalStorage/state.vscdb exists at all.
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let cc = convs
        .iter()
        .find(|x| x.external_id.as_deref() == Some(c))
        .unwrap();
    assert_eq!(
        cc.metadata["is_subagent"],
        json!(false),
        "a missing global store cannot withhold an own-header false"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_global_store_does_not_block_own_header_false() {
    use std::os::unix::fs::PermissionsExt;
    // Round-7 M1 migration: an unreadable global store used to gate false
    // via completeness; now only the composer's own header matters.
    let c = "dddddddd-0000-4000-8002-000000000001";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let global_dir = base.join("globalStorage");
    let global_db = global_dir.join("state.vscdb");
    let ws_db = base
        .join("workspaceStorage")
        .join("ws1")
        .join("state.vscdb");
    fs::create_dir_all(global_dir.as_path()).unwrap();
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    drop(verdict_test_db(&global_db));
    let w = verdict_test_db(&ws_db);
    w.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    w.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    w.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(w);
    // Global exists but its directory is unreadable (metadata probe fails).
    fs::set_permissions(&global_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    fs::set_permissions(&global_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let cc = convs
        .iter()
        .find(|x| x.external_id.as_deref() == Some(c))
        .unwrap();
    assert_eq!(
        cc.metadata["is_subagent"],
        json!(false),
        "an unreadable global store cannot withhold an own-header false"
    );
}

#[cfg(unix)]
#[test]
fn standalone_alias_store_emits_own_header_false() {
    use std::os::unix::fs::symlink;
    // Round-7 M1 migration: the old test pinned installation-completeness —
    // a workspace DB alias whose installation's global was unread withheld
    // false. Inventory no longer enters the verdict: the alias is scanned
    // as a standalone store and its own-header false emits as-is. (The
    // unread parent's P->C listing in the sibling global store is the
    // documented B1 limit, not something the verdict consults.)
    let child = "dddddddd-0000-4000-8003-000000000001";
    let parent = "dddddddd-0000-4000-8003-000000000002";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let inst = base.join("U");
    let global_db = inst.join("globalStorage").join("state.vscdb");
    let ws_db = inst
        .join("workspaceStorage")
        .join("ws1")
        .join("state.vscdb");
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    fs::create_dir_all(ws_db.parent().unwrap()).unwrap();
    let gconn = verdict_test_db(&global_db);
    gconn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{parent}").as_str(),
            json!({"text": "p", "subagentComposerIds": [child], "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(gconn);
    let wconn = verdict_test_db(&ws_db);
    wconn
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{child}").as_str(),
                json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    wconn
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    wconn
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![child, 0i64],
        )
        .unwrap();
    drop(wconn);
    // A standalone-named alias pointing at one workspace DB of installation U.
    let alias_dir = base.join("alias");
    fs::create_dir_all(&alias_dir).unwrap();
    let alias = alias_dir.join("state.vscdb");
    symlink(&ws_db, &alias).unwrap();
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(alias.clone())],
        None,
    );
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    let conv = convs
        .iter()
        .find(|x| x.external_id.as_deref() == Some(child))
        .unwrap();
    assert_eq!(
        conv.metadata["is_subagent"],
        json!(false),
        "a standalone symlinked store emits its own-header false without \
         resolving the rest of its installation"
    );
}

#[test]
fn incomplete_remote_store_does_not_suppress_local_main_verdicts() {
    let m1 = "dddddddd-0000-4000-8004-000000000001";
    let m2 = "dddddddd-0000-4000-8004-000000000002";
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let local_global = base.join("local").join("globalStorage").join("state.vscdb");
    fs::create_dir_all(local_global.parent().unwrap()).unwrap();
    let g = verdict_test_db(&local_global);
    for id in [m1, m2] {
        g.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    }
    g.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    for id in [m1, m2] {
        g.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![id, 0i64],
        )
        .unwrap();
    }
    drop(g);
    // A remote mirror whose composerData is uninterpretable.
    let container = base.join("mirror");
    let remote_db = container.join("ws1").join("state.vscdb");
    fs::create_dir_all(remote_db.parent().unwrap()).unwrap();
    let r = verdict_test_db(&remote_db);
    r.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:broken", "{not json"],
    )
    .unwrap();
    drop(r);
    let local_root = ScanRoot::local(base.join("local"));
    let remote_root = ScanRoot::remote(container, crate::types::Origin::remote("laptop"), None)
        .with_mirrored_source_path("~/.config/Cursor/User/workspaceStorage/ws1");
    let ctx = ScanContext::with_roots(base.to_path_buf(), vec![local_root, remote_root], None);
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    for id in [m1, m2] {
        let cc = convs
            .iter()
            .find(|x| x.external_id.as_deref() == Some(id))
            .unwrap();
        assert_eq!(
            cc.metadata["is_subagent"],
            json!(false),
            "{id}: an unrelated uninterpretable remote store cannot suppress \
             the local store's own-header false"
        );
    }
}

use frankensqlite::compat::RowExt;
use frankensqlite::params;
use serde_json::json;
use tempfile::TempDir;

use super::CursorConnector;
use super::tests_support::{cursor_verdicts, verdict_test_db};
use crate::connectors::scan::ScanRoot;
use crate::connectors::sqlite_sync::{Connection, ConnectionExt};

// ------------------------------------------------------------------
// Round 7: emitted-row evidence merge (N6) / one read snapshot (N7)
// ------------------------------------------------------------------

/// Tick-driven writer injection for the round-7 interleaving tests. Every
/// tick at or after `trigger_at`, `write` is attempted until one attempt
/// commits; the counters report the number of write attempts and the tick
/// at which a commit landed (0 = blocked or never reached). The injected
/// commit always targets the store the connector is currently reading, so
/// a pre-snapshot commit is visible to every later statement while an
/// in-snapshot attempt is not.
struct TickInjection {
    tick: Box<dyn Fn()>,
    attempts: std::rc::Rc<std::cell::Cell<u64>>,
    committed: std::rc::Rc<std::cell::Cell<u64>>,
}

fn inject_on_tick(trigger_at: u64, write: impl Fn() -> bool + 'static) -> TickInjection {
    let attempts = std::rc::Rc::new(std::cell::Cell::new(0_u64));
    let committed = std::rc::Rc::new(std::cell::Cell::new(0_u64));
    let (count, a2, d2) = (
        std::cell::Cell::new(0_u64),
        attempts.clone(),
        committed.clone(),
    );
    let tick = move || {
        count.set(count.get() + 1);
        if count.get() >= trigger_at && d2.get() == 0 {
            a2.set(a2.get() + 1);
            if write() {
                d2.set(count.get());
            }
        }
    };
    TickInjection {
        tick: Box::new(tick),
        attempts,
        committed,
    }
}

/// N6 regression (extraction path): the cross-store evidence pass ran
/// before the writer committed P, C and the header rows — a stale cached
/// map must not let the fresh own-header false win. The emitting store's
/// own rows and headers are merged in first, so `child` is proven by P's
/// `subagentComposerIds` and `header_only` by the own header's
/// `parentComposerId` (a false-plus-parent header is a subagent with that
/// parent, never a main).
#[test]
fn emitted_rows_merge_with_stale_cached_evidence_extraction() {
    let parent = "eeeeeeee-0000-4000-8003-00000000000a";
    let child = "eeeeeeee-0000-4000-8003-00000000000b";
    let header_only = "eeeeeeee-0000-4000-8003-00000000000c";
    let header_parent = "eeeeeeee-0000-4000-8003-00000000000d";
    let other = "eeeeeeee-0000-4000-8003-00000000000e";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{other}").as_str(),
            json!({"text": "other", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(conn);
    let dbs = vec![(ScanRoot::local(dir.path().to_path_buf()), db.clone())];
    // The cached pass predates the commit — exactly what a scan's earlier
    // evidence pass returns for a store whose rows arrived afterwards.
    let mut cached = CursorConnector::collect_cross_db_evidence(&dbs, &|| {});
    assert!(
        !cached.child_to_parent.contains_key(child),
        "fixture: the cached pass must predate the commit"
    );
    // One atomic writer commit: text-bearing P and C, P's
    // subagentComposerIds listing, a second composer whose only proof is
    // its own header's parentComposerId, and both own headers as
    // isSubagent=0 + parentComposerId.
    let w = Connection::open(db.to_string_lossy().as_ref()).unwrap();
    w.execute("BEGIN").unwrap();
    for (id, val) in [
        (
            parent,
            json!({"text": "p", "subagentComposerIds": [child], "createdAt": "2026-11-12T00:00:00Z"}),
        ),
        (
            child,
            json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"}),
        ),
        (
            header_only,
            json!({"text": "h", "createdAt": "2026-11-12T00:00:00Z"}),
        ),
    ] {
        w.execute_compat(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    }
    w.execute(
        "CREATE TABLE IF NOT EXISTS composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT)",
    )
    .unwrap();
    for (id, par) in [(child, parent), (header_only, header_parent)] {
        w.execute_compat(
            "INSERT OR REPLACE INTO composerHeaders (composerId, isSubagent, parentComposerId) VALUES (?, ?, ?)",
            params![id, 0i64, par],
        )
        .unwrap();
    }
    w.execute("COMMIT").unwrap();
    drop(w);
    let convs =
        CursorConnector::extract_from_db_with_evidence(&db, None, &|| {}, Some(&mut cached))
            .unwrap();
    let c = convs
        .iter()
        .find(|cv| cv.external_id.as_deref() == Some(child))
        .unwrap();
    assert_eq!(
        c.metadata["is_subagent"],
        json!(true),
        "the committed P->C listing the cached pass missed must prove the subagent"
    );
    assert_eq!(c.metadata["parent_id"], json!(parent));
    let h = convs
        .iter()
        .find(|cv| cv.external_id.as_deref() == Some(header_only))
        .unwrap();
    assert_eq!(
        h.metadata["is_subagent"],
        json!(true),
        "an own header that says false but carries a parent id is a subagent"
    );
    assert_eq!(h.metadata["parent_id"], json!(header_parent));
}

/// N6 regression (targeted lookup): same stale-cached-evidence witness on
/// the lookup path — the sid row, local evidence and own headers are read
/// now, so the committed parent wins over the stale map.
#[test]
fn emitted_rows_merge_with_stale_cached_evidence_lookup() {
    let parent = "eeeeeeee-0000-4000-8004-00000000000a";
    let child = "eeeeeeee-0000-4000-8004-00000000000b";
    let other = "eeeeeeee-0000-4000-8004-00000000000e";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{other}").as_str(),
            json!({"text": "other", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    drop(conn);
    let dbs = vec![(ScanRoot::local(dir.path().to_path_buf()), db.clone())];
    let mut cached = CursorConnector::collect_cross_db_evidence(&dbs, &|| {});
    let w = Connection::open(db.to_string_lossy().as_ref()).unwrap();
    w.execute("BEGIN").unwrap();
    for (id, val) in [
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
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    }
    w.execute(
        "CREATE TABLE IF NOT EXISTS composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT)",
    )
    .unwrap();
    w.execute_compat(
        "INSERT OR REPLACE INTO composerHeaders (composerId, isSubagent, parentComposerId) VALUES (?, ?, ?)",
        params![child, 0i64, parent],
    )
    .unwrap();
    w.execute("COMMIT").unwrap();
    drop(w);
    let (found, failures) = CursorConnector::lookup_composer_conversations_with_evidence(
        &dbs,
        &[child.to_string()],
        &|| {},
        Some(&mut cached),
    );
    assert!(failures.is_empty());
    let resolved = found[child].as_ref().unwrap();
    assert_eq!(
        resolved.metadata["is_subagent"],
        json!(true),
        "the committed P->C listing the cached pass missed must prove the subagent"
    );
    assert_eq!(resolved.metadata["parent_id"], json!(parent));
}

/// N6-residual regression (round-8/9): a captured `composerData` row FAD
/// cannot interpret cannot void the store's evidence read — but it does
/// withhold false for the WHOLE store. C's own row carries
/// isSubagent+parentId — proven both by the resilient bulk scan and by
/// the fetched hit itself — so the own-header false loses. Sibling is
/// proven by a decodable listing row next to the bad one. The clean-false
/// `main` is withheld (round-9: the unreadable payload might have listed
/// it too), and the same undecodable composer emitted elsewhere is vetoed.
#[test]
fn lookup_keeps_decodable_evidence_and_hit_parents_past_a_bad_row() {
    let c = "eeeeeeee-0000-4000-8008-00000000000b";
    let p = "eeeeeeee-0000-4000-8008-00000000000a";
    let sibling = "eeeeeeee-0000-4000-8008-00000000000c";
    let p2 = "eeeeeeee-0000-4000-8008-00000000000d";
    let main = "eeeeeeee-0000-4000-8008-00000000000e";
    let broken = "!broken";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    // A BLOB value in a TEXT-affinity column — the old Option<String>
    // mapper aborted the entire evidence read on this cell.
    conn.execute("INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:!broken', X'00')")
        .unwrap();
    for (id, val) in [
        (
            c,
            json!({"text": "child", "parentId": p, "isSubagent": true, "createdAt": "2026-11-12T00:00:00Z"}),
        ),
        (
            sibling,
            json!({"text": "s", "createdAt": "2026-11-12T00:00:00Z"}),
        ),
        (
            p2,
            json!({"text": "p2", "subagentComposerIds": [sibling], "createdAt": "2026-11-12T00:00:00Z"}),
        ),
        (
            main,
            json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"}),
        ),
    ] {
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{id}").as_str(),
                val.to_string().as_str()
            ],
        )
        .unwrap();
    }
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    for id in [c, sibling, main] {
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![id, 0i64],
        )
        .unwrap();
    }
    drop(conn);
    // A second store where the same composer id resolves as text with a
    // clean own-header false — the veto must arrive via merged evidence.
    let dir2 = TempDir::new().unwrap();
    let db2 = dir2.path().join("state.vscdb");
    let conn2 = verdict_test_db(&db2);
    conn2
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{broken}").as_str(),
                json!({"text": "b", "createdAt": "2026-11-12T00:00:00Z"})
                    .to_string()
                    .as_str()
            ],
        )
        .unwrap();
    conn2
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn2
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![broken, 0i64],
        )
        .unwrap();
    drop(conn2);
    let dbs = vec![
        (ScanRoot::local(dir.path().to_path_buf()), db.clone()),
        (ScanRoot::local(dir2.path().to_path_buf()), db2),
    ];
    let mut cached = CursorConnector::collect_cross_db_evidence(&dbs, &|| {});
    let (found, failures) = CursorConnector::lookup_composer_conversations_with_evidence(
        &dbs,
        &[
            c.to_string(),
            sibling.to_string(),
            main.to_string(),
            broken.to_string(),
        ],
        &|| {},
        Some(&mut cached),
    );
    assert!(failures.is_empty());
    let resolved = found[c].as_ref().unwrap();
    assert_eq!(resolved.metadata["is_subagent"], json!(true));
    assert_eq!(
        resolved.metadata["parent_id"],
        json!(p),
        "the emitted hit's own parentId must survive the bulk read's bad sibling row"
    );
    let resolved = found[sibling].as_ref().unwrap();
    assert_eq!(resolved.metadata["is_subagent"], json!(true));
    assert_eq!(
        resolved.metadata["parent_id"],
        json!(p2),
        "the decodable listing beside the BLOB must keep its positive evidence"
    );
    assert!(
        found[main]
            .as_ref()
            .unwrap()
            .metadata
            .get("is_subagent")
            .is_none(),
        "round-9 N6: a captured row FAD cannot interpret withholds false \
         for the WHOLE store — its payload might have listed `main` too"
    );
    let broken_meta = &found[broken].as_ref().unwrap().metadata;
    assert!(
        broken_meta.get("is_subagent").is_none(),
        "the composer whose own row could not be decoded stays uncertain — \
         its undecodable flags might have held a parent"
    );
}

/// N6 residual (round-9, extraction): a composerData value stored as a
/// BLOB whose bytes are valid UTF-8 JSON decodes exactly like TEXT — the
/// native reader does the same (claude_viewer
/// viewer/providers/cursor/discovery.py: `v.decode("utf-8",
/// errors="ignore")` then `json.loads`). The BLOB parent's
/// `subagentComposerIds` must contribute P->C, so C's clean own-header
/// false loses. `main` is the contrast control: a DECODED blob
/// withholds nothing — its clean false still emits.
#[test]
fn blob_parent_listing_proves_subagent_past_own_header_false() {
    let c = "eeeeeeee-0000-4000-8009-00000000000b";
    let p = "eeeeeeee-0000-4000-8009-00000000000a";
    let main = "eeeeeeee-0000-4000-8009-00000000000e";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [c, main] {
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
    // P's row is bound as a BLOB carrying valid UTF-8 JSON — SQLite's
    // TEXT column affinity does not coerce it back.
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{p}").as_str(),
            json!({"text": "p", "subagentComposerIds": [c], "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .into_bytes()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    for id in [c, main] {
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![id, 0i64],
        )
        .unwrap();
    }
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert_eq!(
        v[c]["is_subagent"],
        json!(true),
        "the BLOB parent's listing must prove C — its payload is valid \
         UTF-8 JSON, exactly what the native reader decodes"
    );
    assert_eq!(v[c]["parent_id"], json!(p));
    assert_eq!(
        v[main]["is_subagent"],
        json!(false),
        "a blob that DID decode withholds nothing — the clean own-header \
         false control still emits"
    );
}

/// Same witness through the targeted-lookup path: the bulk evidence read
/// folds the BLOB parent's listing even though only C is fetched.
#[test]
fn lookup_blob_parent_listing_proves_subagent_past_own_header_false() {
    let c = "eeeeeeee-0000-4000-800a-00000000000b";
    let p = "eeeeeeee-0000-4000-800a-00000000000a";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            json!({"text": "child", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{p}").as_str(),
            json!({"text": "p", "subagentComposerIds": [c], "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .into_bytes()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(conn);
    let (found, failures) = CursorConnector::lookup_composer_conversations(
        &[(ScanRoot::local(dir.path().to_path_buf()), db)],
        &[c.to_string()],
        &|| {},
    );
    assert!(failures.is_empty());
    let resolved = found[c].as_ref().unwrap();
    assert_eq!(resolved.metadata["is_subagent"], json!(true));
    assert_eq!(resolved.metadata["parent_id"], json!(p));
}

/// N6 residual (round-9/10): a captured composerData row FAD cannot
/// interpret at all withholds false for the WHOLE store, not only its own
/// id — the unreadable payload might have listed any composer as a
/// subagent. Each undecodable storage form is pinned in its OWN store so
/// each must independently withhold: db1 carries a non-UTF-8 BLOB; db2
/// carries a raw INTEGER — its `value` column is declared without a type
/// so it has no affinity and 42 really stores as INTEGER (a TEXT-affinity
/// column would coerce it to "42" and never reach the undecodable arm —
/// round-10 M3). Rows that DO decode keep their positives.
#[test]
fn undecodable_composer_cells_withhold_store_false() {
    let main = "eeeeeeee-0000-4000-800c-00000000000e";
    let flagless = "eeeeeeee-0000-4000-800c-00000000000f";
    let parent = "eeeeeeee-0000-4000-800c-000000000010";
    let child = "eeeeeeee-0000-4000-800c-000000000011";
    let main_i = "eeeeeeee-0000-4000-800c-000000000012";
    // Store 1 — the non-UTF-8 BLOB cell.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for (id, val) in [
        (
            main,
            json!({"text": "m", "createdAt": "2026-11-12T00:00:00Z"}),
        ),
        (
            flagless,
            json!({"text": "f", "createdAt": "2026-11-12T00:00:00Z"}),
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
                val.to_string().as_str()
            ],
        )
        .unwrap();
    }
    conn.execute("INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:z_blob', X'80FF')")
        .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    for id in [main, child] {
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![id, 0i64],
        )
        .unwrap();
    }
    drop(conn);
    // Store 2 — the raw INTEGER cell on a no-affinity column.
    let dir_i = TempDir::new().unwrap();
    let db_i = dir_i.path().join("state.vscdb");
    let conn_i = verdict_test_db(&db_i);
    conn_i.execute("DROP TABLE cursorDiskKV").unwrap();
    // An untyped column carries no affinity: INTEGER stays INTEGER, TEXT
    // stays TEXT — SQLite's storage-class rule the verdict relies on.
    conn_i
        .execute("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value)")
        .unwrap();
    conn_i
        .execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                format!("composerData:{main_i}").as_str(),
                "{\"text\": \"mi\", \"createdAt\": \"2026-11-12T00:00:00Z\"}"
            ],
        )
        .unwrap();
    conn_i
        .execute("INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:z_int', 42)")
        .unwrap();
    // Independently assert the fixture reached the INTEGER storage class —
    // the assertion below means nothing if 42 silently stored as TEXT.
    let stored: String = conn_i
        .query_map_collect(
            "SELECT typeof(value) FROM cursorDiskKV WHERE key = 'composerData:z_int'",
            params![],
            |row| row.get_typed(0),
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        stored, "integer",
        "fixture: 42 must store as INTEGER on the no-affinity column"
    );
    conn_i
        .execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn_i
        .execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![main_i, 0i64],
        )
        .unwrap();
    drop(conn_i);
    let v = cursor_verdicts(&db, None);
    assert!(
        v[main].get("is_subagent").is_none(),
        "the non-UTF-8 BLOB withholds every false in its store — \
         `main`'s clean own-header false stays unemitted"
    );
    assert!(
        v[flagless].get("is_subagent").is_none(),
        "a composer with no own-header flag must not guess false"
    );
    assert_eq!(
        v[child]["is_subagent"],
        json!(true),
        "decodable rows keep their positive evidence past the bad cell"
    );
    assert_eq!(v[child]["parent_id"], json!(parent));
    assert!(
        !v.contains_key("z_blob"),
        "the undecodable BLOB is not emitted as a conversation"
    );
    let v_i = cursor_verdicts(&db_i, None);
    assert!(
        v_i[main_i].get("is_subagent").is_none(),
        "the raw INTEGER cell independently withholds every false in ITS \
         store — the BLOB case cannot mask it"
    );
    assert!(
        !v_i.contains_key("z_int"),
        "the undecodable INTEGER cell is not emitted as a conversation"
    );
}

/// N6 residual (round-10): store uncertainty is sticky for the whole
/// scan. The cross-store evidence pass reads store A while it still
/// holds a row FAD cannot interpret; the writer then replaces that row
/// with NULL before A's emitting reread. The reread sees a clean store —
/// but the earlier capture already marked A uncertain, so A's own-header
/// false is withheld anyway: the bad payload might have listed any
/// composer of A.
#[test]
fn captured_undecodable_row_keeps_store_uncertain_past_clean_reread() {
    let c = "aaaaaaa1-0000-4000-800d-00000000000c";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            "{\"text\": \"child\", \"createdAt\": \"2026-11-12T00:00:00Z\"}"
        ],
    )
    .unwrap();
    // The row the evidence pass will fail to interpret.
    conn.execute("INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:p', X'80FF')")
        .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(conn);
    let dir_b = TempDir::new().unwrap();
    let db_b = dir_b.path().join("state.vscdb");
    drop(verdict_test_db(&db_b));
    let dbs = vec![
        (ScanRoot::local(dir.path().to_path_buf()), db.clone()),
        (ScanRoot::local(dir_b.path().to_path_buf()), db_b),
    ];
    let mut cached = CursorConnector::collect_cross_db_evidence(&dbs, &|| {});
    // Between the evidence pass and the emitting reread, the writer
    // replaces the unreadable row with a NULL marker — the reread of A
    // now looks clean.
    let w = Connection::open(db.to_string_lossy().as_ref()).unwrap();
    w.execute("UPDATE cursorDiskKV SET value = NULL WHERE key = 'composerData:p'")
        .unwrap();
    drop(w);
    let convs =
        CursorConnector::extract_from_db_with_evidence(&db, None, &|| {}, Some(&mut cached))
            .unwrap();
    let resolved = convs
        .iter()
        .find(|cv| cv.external_id.as_deref() == Some(c))
        .expect("C's TEXT row still emits");
    assert!(
        resolved.metadata.get("is_subagent").is_none(),
        "the earlier capture of A's undecodable row stays authoritative \
         for the whole scan — a clean reread cannot lift it"
    );
}

/// The same stickiness on the targeted-lookup path (round-10 N6): the
/// evidence pass captured the undecodable row; the clean reread during
/// lookup must not permit C's own-header false.
#[test]
fn lookup_captured_undecodable_row_keeps_store_uncertain_past_clean_reread() {
    let c = "aaaaaaa2-0000-4000-800d-00000000000c";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            "{\"text\": \"child\", \"createdAt\": \"2026-11-12T00:00:00Z\"}"
        ],
    )
    .unwrap();
    conn.execute("INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:p', X'80FF')")
        .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(conn);
    let dir_b = TempDir::new().unwrap();
    let db_b = dir_b.path().join("state.vscdb");
    drop(verdict_test_db(&db_b));
    let dbs = vec![
        (ScanRoot::local(dir.path().to_path_buf()), db.clone()),
        (ScanRoot::local(dir_b.path().to_path_buf()), db_b),
    ];
    let mut cached = CursorConnector::collect_cross_db_evidence(&dbs, &|| {});
    let w = Connection::open(db.to_string_lossy().as_ref()).unwrap();
    w.execute("UPDATE cursorDiskKV SET value = NULL WHERE key = 'composerData:p'")
        .unwrap();
    drop(w);
    let (found, failures) = CursorConnector::lookup_composer_conversations_with_evidence(
        &dbs,
        &[c.to_string()],
        &|| {},
        Some(&mut cached),
    );
    assert!(failures.is_empty());
    let resolved = found[c].as_ref().unwrap();
    assert!(
        resolved.metadata.get("is_subagent").is_none(),
        "the earlier capture of A's undecodable row stays authoritative \
         for the whole scan — a clean reread cannot lift it"
    );
}

/// N10 (round-10): malformed UTF-8 inside a BLOB identifier must never
/// be lossy-decoded into an invented composer — `P\xff` cannot become
/// `P�` and shadow the parent the composer's own header names. Under
/// strict classification the whole cell is undecodable: C is not
/// emitted at all (its conversation cannot be decoded), and the store
/// stays uncertain, so `main`'s clean own-header false is withheld.
#[test]
fn non_utf8_blob_parent_id_is_undecodable_not_invented() {
    let c = "bbbbbbb1-0000-4000-800d-00000000000c";
    let p = "bbbbbbb1-0000-4000-800d-00000000000p";
    let main = "bbbbbbb1-0000-4000-800d-0000000000e0";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    // C's row is a BLOB whose bytes are invalid UTF-8 right inside the
    // parentComposerId identifier.
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            b"{\"text\":\"child\",\"parentComposerId\":\"P\xff\",\"createdAt\":\"2026-11-12T00:00:00Z\"}"
                .to_vec()
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{main}").as_str(),
            "{\"text\": \"m\", \"createdAt\": \"2026-11-12T00:00:00Z\"}"
        ],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT)",
    )
    .unwrap();
    // The known-header-parent contrast: C's own header already names
    // parent P — the corrupted identifier must never reach the output
    // as `P�`, nor replace the known parent.
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId) VALUES (?, ?, ?)",
        params![c, 1i64, p],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert!(
        !v.contains_key(c),
        "a non-UTF-8 BLOB is undecodable — no invented parent identifier \
         reaches the output"
    );
    assert!(
        v[main].get("is_subagent").is_none(),
        "the undecodable cell keeps the whole store uncertain for false"
    );
}

/// The same malformed-identifier rule on the targeted-lookup path
/// (round-10 N10): a non-UTF-8 BLOB hit never lossy-decodes into a
/// resolvable conversation, and its capture keeps the store uncertain,
/// so `main`'s own-header false is withheld.
#[test]
fn lookup_non_utf8_blob_parent_id_is_undecodable_not_invented() {
    let c = "bbbbbbb2-0000-4000-800d-00000000000c";
    let p = "bbbbbbb2-0000-4000-800d-00000000000p";
    let main = "bbbbbbb2-0000-4000-800d-0000000000e0";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            b"{\"text\":\"child\",\"parentComposerId\":\"P\xff\",\"createdAt\":\"2026-11-12T00:00:00Z\"}"
                .to_vec()
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{main}").as_str(),
            "{\"text\": \"m\", \"createdAt\": \"2026-11-12T00:00:00Z\"}"
        ],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER, parentComposerId TEXT)",
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, parentComposerId) VALUES (?, ?, ?)",
        params![c, 1i64, p],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![main, 0i64],
    )
    .unwrap();
    drop(conn);
    let (found, failures) = CursorConnector::lookup_composer_conversations(
        &[(ScanRoot::local(dir.path().to_path_buf()), db)],
        &[c.to_string(), main.to_string()],
        &|| {},
    );
    assert!(failures.is_empty());
    assert!(
        found.get(c).and_then(Option::as_ref).is_none(),
        "a non-UTF-8 BLOB hit is undecodable — no invented parent \
         identifier resolves to a conversation"
    );
    let resolved = found[main].as_ref().unwrap();
    assert!(
        resolved.metadata.get("is_subagent").is_none(),
        "the undecodable cell keeps the whole store uncertain for false"
    );
}

/// N10 (round-10): a BLOB `subagentComposerIds` listing whose bytes are
/// not valid UTF-8 cannot be lossy-recovered either — the corrupted
/// identifier it might have named keeps the whole store uncertain
/// instead of either inventing a parent link or being silently ignored.
#[test]
fn non_utf8_blob_listing_withholds_false() {
    let c = "ccccccc1-0000-4000-800d-00000000000c";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{c}").as_str(),
            "{\"text\": \"child\", \"createdAt\": \"2026-11-12T00:00:00Z\"}"
        ],
    )
    .unwrap();
    // The listing names a child id with a raw 0xFF byte inside — invalid
    // UTF-8, so the whole row is undecodable and its possible listing of
    // ANY composer (including C) stays unproven-but-not-disproven.
    conn.execute(
        "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:p', X'7B227375626167656E74436F6D706F736572496473223A5B2243FF225D7D')",
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    assert!(
        v[c].get("is_subagent").is_none(),
        "the unrecoverable listing withholds C's own-header false"
    );
    assert!(
        v[c].get("parent_id").is_none(),
        "and no invented parent link reaches the output"
    );
}

/// N9 regression (targeted lookup): false must come from the LATE
/// (post-row) own-header read only — an absent or flagless late flag may
/// not borrow a false that only the earlier read saw. The early header
/// read observes (C, false); the writer then commits C's composer row
/// and nulls C's isSubagent in one transaction. The late read observes
/// the NULL flag: no explicit false remains in the post-row state, so
/// the verdict is ABSENT — not the early false, not an inferred true.
#[test]
fn lookup_late_null_header_does_not_borrow_early_false() {
    let c = "eeeeeeee-0000-4000-8005-00000000000b";
    let other = "eeeeeeee-0000-4000-8005-00000000000e";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![
            format!("composerData:{other}").as_str(),
            json!({"text": "other", "createdAt": "2026-11-12T00:00:00Z"})
                .to_string()
                .as_str()
        ],
    )
    .unwrap();
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
        params![c, 0i64],
    )
    .unwrap();
    drop(conn);
    let db_str = db.to_string_lossy().to_string();
    // Fires on the sid-loop tick (new order) / inside the early
    // own-header read (old order): one atomic commit — C's composer row
    // appears and its own-header flag is nullified (the row stays).
    let inj = inject_on_tick(4, move || {
        let Ok(w) = Connection::open(&db_str) else {
            return false;
        };
        let ok = w.execute("BEGIN").is_ok()
            && w.execute_compat(
                "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![
                    format!("composerData:{c}").as_str(),
                    json!({"text": "c", "createdAt": "2026-11-12T00:00:00Z"})
                        .to_string()
                        .as_str()
                ],
            )
            .is_ok()
            && w.execute_compat(
                "UPDATE composerHeaders SET isSubagent = ? WHERE composerId = ?",
                params![None::<i64>, c],
            )
            .is_ok()
            && w.execute("COMMIT").is_ok();
        if !ok {
            let _ = w.execute("ROLLBACK");
        }
        ok
    });
    let (found, failures) = CursorConnector::lookup_composer_conversations(
        &[(ScanRoot::local(dir.path().to_path_buf()), db)],
        &[c.to_string()],
        inj.tick.as_ref(),
    );
    assert!(failures.is_empty());
    assert!(
        inj.attempts.get() > 0,
        "the interleaved write must have been attempted during the read"
    );
    assert!(
        inj.committed.get() > 0,
        "the concurrent writer's commit must SUCCEED during the scan — \
         lock-blocking a live store is not a passing mechanism"
    );
    let resolved = found
        .get(c)
        .and_then(|entry| entry.as_ref())
        .expect("C's composerData row committed before the point fetch");
    assert!(
        resolved.metadata.get("is_subagent").is_none(),
        "the late own-header read saw NULL — the false that only the \
         early read observed must not be borrowed (writer \
         committed_at_tick={})",
        inj.committed.get()
    );
}

/// N7/N8 regression (extraction): the own-header flags are read AFTER
/// the composerData rows, with no held transaction. A writer that flips
/// C's header to true during the row read must SUCCEED mid-scan — a held
/// read transaction would lock it out — and the header read that follows
/// must observe the committed true. (Cursor writes a subagent's header
/// with or before its rows; the commit here models the freshest header
/// state the ordering rule is designed to see.)
#[test]
fn extraction_reads_own_header_after_composer_rows() {
    let c = "eeeeeeee-0000-4000-8006-00000000000b";
    let other = "eeeeeeee-0000-4000-8006-00000000000e";
    let unrelated = "eeeeeeee-0000-4000-8006-00000000ffff";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [c, other] {
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
    conn.execute("CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent INTEGER)")
        .unwrap();
    for id in [c, unrelated] {
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![id, 0i64],
        )
        .unwrap();
    }
    drop(conn);
    let db_str = db.to_string_lossy().to_string();
    // Fires inside the composerData range read: one commit — C's header
    // flips to true. On the held-transaction implementation this attempt
    // is lock-blocked until after the verdict's snapshot; on the ordered
    // reads it lands before the own-header read and is seen.
    let inj = inject_on_tick(1, move || {
        let Ok(w) = Connection::open(&db_str) else {
            return false;
        };
        let ok = w.execute("BEGIN").is_ok()
            && w.execute_compat(
                "UPDATE composerHeaders SET isSubagent = ? WHERE composerId = ?",
                params![1i64, c],
            )
            .is_ok()
            && w.execute("COMMIT").is_ok();
        if !ok {
            let _ = w.execute("ROLLBACK");
        }
        ok
    });
    let convs = CursorConnector::extract_from_db(&db, None, inj.tick.as_ref()).unwrap();
    assert!(
        inj.attempts.get() > 0,
        "the interleaved write must have been attempted during the read"
    );
    assert!(
        inj.committed.get() > 0,
        "the concurrent writer's commit must SUCCEED during the scan — \
         lock-blocking a live store is not a passing mechanism"
    );
    let cv = convs
        .iter()
        .find(|cv| cv.external_id.as_deref() == Some(c))
        .expect("C's composerData row was committed before the scan");
    assert_eq!(
        cv.metadata["is_subagent"],
        json!(true),
        "the own-header read after the composerData read must see the \
         committed true (writer committed_at_tick={})",
        inj.committed.get()
    );
}

/// M2 (round-7): an uninterpretable `isSubagent` component vetoes an
/// otherwise cleanly decoded own-header false. Every absent case pairs a
/// proven SQL false with a null/unknown sibling component for the same
/// composer; clean-false controls in the same store still emit, so the
/// veto is pinned to the unknown component, not to the fixtures.
#[test]
fn uninterpretable_component_vetoes_own_header_false() {
    let v_null = "eeeeeeee-0000-4000-8007-000000000001";
    let v_bogus = "eeeeeeee-0000-4000-8007-000000000002";
    let v_item = "eeeeeeee-0000-4000-8007-000000000003";
    let ok_sql = "eeeeeeee-0000-4000-8007-000000000004";
    let ok_item = "eeeeeeee-0000-4000-8007-000000000005";
    let noflag = "eeeeeeee-0000-4000-8007-000000000006";
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("state.vscdb");
    let conn = verdict_test_db(&db);
    for id in [v_null, v_bogus, v_item, ok_sql, ok_item, noflag] {
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
        "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, isSubagent, value TEXT)",
    )
    .unwrap();
    // A proven SQL false paired with an uninterpretable value-JSON flag.
    for (id, enc) in [(v_null, json!(null)), (v_bogus, json!("bogus"))] {
        let hj = json!({"composerId": id, "isSubagent": enc}).to_string();
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent, value) VALUES (?, ?, ?)",
            params![id, 0i64, hj.as_str()],
        )
        .unwrap();
    }
    // Proven SQL false with no value-JSON flag: the ItemTable carries the
    // vetoing null for `v_item` and a confirming false for `ok_item`.
    for id in [v_item, ok_item] {
        conn.execute_compat(
            "INSERT INTO composerHeaders (composerId, isSubagent) VALUES (?, ?)",
            params![id, 0i64],
        )
        .unwrap();
    }
    // Clean-false control through the value JSON.
    let ok_sql_hj = json!({"composerId": ok_sql, "isSubagent": false}).to_string();
    conn.execute_compat(
        "INSERT INTO composerHeaders (composerId, isSubagent, value) VALUES (?, ?, ?)",
        params![ok_sql, 0i64, ok_sql_hj.as_str()],
    )
    .unwrap();
    let items = json!([
        {"composerId": v_item, "isSubagent": null},
        {"composerId": ok_item, "isSubagent": false}
    ])
    .to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params!["composer.composerHeaders", items.as_str()],
    )
    .unwrap();
    drop(conn);
    let v = cursor_verdicts(&db, None);
    for id in [v_null, v_bogus, v_item, noflag] {
        assert!(
            v[id].get("is_subagent").is_none(),
            "{id}: an unknown isSubagent component must veto the SQL false"
        );
    }
    for id in [ok_sql, ok_item] {
        assert_eq!(
            v[id]["is_subagent"],
            json!(false),
            "{id}: a clean own-header false still emits"
        );
    }
}

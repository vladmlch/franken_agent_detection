//! Devin IDE `acp-messages` store integration tests (Phase B).
#![cfg(feature = "devin")]

#[path = "fixtures/devin_acp/mod.rs"]
mod devin_acp;

use std::path::Path;

use franken_agent_detection::connectors::devin::DevinConnector;
use franken_agent_detection::connectors::scan::{
    DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion, SourceScanHooks,
};
use franken_agent_detection::connectors::sqlite_sync::{Connection, ConnectionExt};
use franken_agent_detection::{Connector, franken_detection_for_connector};
use frankensqlite::params;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn fixtures_create_valid_main_subagent_and_empty_stores() {
    let dir = TempDir::new().expect("tempdir");
    let main_db = dir.path().join("11111111-1111-4111-8111-111111111111.db");
    let sub_db = dir.path().join("22222222-2222-4222-8222-222222222222.db");
    let empty_no_msg = dir.path().join("33333333-3333-4333-8333-333333333333.db");
    let empty_zero = dir.path().join("44444444-4444-4444-8444-444444444444.db");

    devin_acp::build_main_session_db(&main_db);
    devin_acp::build_subagent_session_db(&sub_db);
    devin_acp::build_empty_store_db(&empty_no_msg, true, false);
    devin_acp::build_empty_store_db(&empty_zero, false, false);

    assert_eq!(devin_acp::count_tables(&main_db), 2);
    assert_eq!(devin_acp::count_tables(&sub_db), 2);
    assert_eq!(devin_acp::count_tables(&empty_no_msg), 1);
    assert_eq!(std::fs::metadata(&empty_zero).expect("zero meta").len(), 0);
}

#[test]
fn acp_main_session_extracts_messages_in_position_order() {
    let dir = TempDir::new().expect("tempdir");
    let stem = "11111111-1111-4111-8111-111111111111";
    let acp_dir = dir.path().join("acp-messages");
    let db_path = acp_dir.join(format!("{stem}.db"));
    devin_acp::build_main_session_db(&db_path);

    let ctx = ScanContext::with_roots(
        dir.path().join("cass-data"),
        vec![ScanRoot::local(acp_dir)],
        None,
    );
    let convs = DevinConnector::new().scan(&ctx).expect("scan acp main db");
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "devin");
    assert_eq!(conv.external_id.as_deref(), Some(stem));
    assert_eq!(conv.source_path, db_path);
    assert_eq!(conv.workspace.as_deref(), Some(Path::new("/work/repo")));
    assert_eq!(conv.metadata["source"], json!("acp"));
    assert_eq!(conv.metadata["is_subagent"], json!(false));
    assert_eq!(conv.metadata["store_layout"], json!("acp-messages"));
    assert_eq!(conv.metadata["model"], json!("claude-sonnet-4-6"));
    assert_eq!(conv.metadata["title"], json!("Inspect layout"));

    let contents: Vec<&str> = conv.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(conv.messages.len(), 4);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(contents[0], "Inspect crate layout");
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(contents[1], "Checking Cargo.toml");
    assert_eq!(conv.messages[2].invocations.len(), 1);
    assert_eq!(conv.messages[2].invocations[0].name, "read");
    assert_eq!(conv.messages[3].role, "assistant");
    assert_eq!(contents[3], "Layout verified.");
    assert_eq!(
        conv.messages.iter().map(|m| m.idx).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn acp_subagent_row_emits_child_conversation_with_parent_id() {
    let dir = TempDir::new().expect("tempdir");
    let stem = "22222222-2222-4222-8222-222222222222";
    let acp_dir = dir.path().join("User").join("acp-messages");
    let db_path = acp_dir.join(format!("{stem}.db"));
    devin_acp::build_subagent_session_db(&db_path);

    let ctx = ScanContext::with_roots(
        dir.path().join("cass-data"),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = DevinConnector::new().scan(&ctx).expect("scan acp subagent");
    assert_eq!(convs.len(), 2);

    let main = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(stem))
        .expect("main conversation");
    assert_eq!(main.metadata["is_subagent"], json!(false));
    assert_eq!(main.source_path, db_path);

    let sub_ext = format!("{stem}:agent-42");
    let sub = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(sub_ext.as_str()))
        .expect("subagent conversation");
    assert_eq!(sub.source_path, db_path.join(":agent-42"));
    assert_eq!(sub.metadata["source"], json!("acp"));
    assert_eq!(sub.metadata["is_subagent"], json!(true));
    assert_eq!(sub.metadata["parent_id"], json!(stem));
    assert_eq!(sub.metadata["agent_id"], json!("agent-42"));
    assert_eq!(sub.metadata["profile"], json!("explore"));
    assert_eq!(sub.metadata["status"], json!("completed"));
    assert_eq!(sub.title.as_deref(), Some("Module scanner"));
    let child_texts: Vec<&str> = sub.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        child_texts,
        vec!["Scanning module", "Module check complete."]
    );
}

#[test]
fn acp_empty_and_missing_messages_table_return_empty_without_error() {
    let dir = TempDir::new().expect("tempdir");
    let acp_dir = dir.path().join("acp-messages");
    devin_acp::build_empty_store_db(&acp_dir.join("zero.db"), false, false);
    devin_acp::build_empty_store_db(&acp_dir.join("no-messages.db"), true, false);
    devin_acp::build_empty_store_db(&acp_dir.join("empty-messages.db"), true, true);

    let ctx = ScanContext::with_roots(
        dir.path().join("cass-data"),
        vec![ScanRoot::local(acp_dir)],
        None,
    );
    let convs = DevinConnector::new()
        .scan(&ctx)
        .expect("empty stores are Ok");
    assert!(convs.is_empty());
}

#[test]
fn acp_non_list_child_messages_duplicate_agent_and_unseen_kind() {
    let dir = TempDir::new().expect("tempdir");
    let stem = "55555555-5555-4555-8555-555555555555";
    let acp_dir = dir.path().join("acp-messages");
    let db_path = acp_dir.join(format!("{stem}.db"));
    devin_acp::build_main_session_db(&db_path);

    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("reopen");
    devin_acp::insert_message_row(
        &conn,
        4,
        "subagent",
        &devin_acp::subagent_payload(
            "bad-list",
            "Bad child list",
            "completed",
            Some(true),
            json!("not-a-list"),
        ),
    );
    devin_acp::insert_message_row(
        &conn,
        5,
        "subagent",
        &devin_acp::subagent_payload(
            "dup-agent",
            "Populated run",
            "completed",
            Some(true),
            json!([devin_acp::agent_message_payload(
                "cm1",
                "t1",
                &["Real child output"],
                "2026-11-15T10:00:05.000Z"
            )]),
        ),
    );
    devin_acp::insert_message_row(
        &conn,
        6,
        "subagent",
        &devin_acp::subagent_payload("dup-agent", "Stale running", "running", None, json!([])),
    );
    devin_acp::insert_message_row(
        &conn,
        7,
        "future_unknown_kind",
        &json!({ "id": "u9", "kind": "future_unknown_kind" }),
    );
    drop(conn);

    let ctx = ScanContext::with_roots(
        dir.path().join("cass-data"),
        vec![ScanRoot::local(acp_dir.clone())],
        None,
    );
    let convs = DevinConnector::new().scan(&ctx).expect("scan edge cases");
    assert_eq!(convs.len(), 2, "main + single deduplicated dup-agent");
    let main = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(stem))
        .expect("main present");
    assert_eq!(
        main.metadata["skipped_kinds"]["future_unknown_kind"],
        json!(1)
    );
    assert!(
        convs
            .iter()
            .all(|c| c.external_id.as_deref()
                != Some("55555555-5555-4555-8555-555555555555:bad-list"))
    );
    let dup = convs
        .iter()
        .find(|c| {
            c.external_id.as_deref() == Some("55555555-5555-4555-8555-555555555555:dup-agent")
        })
        .expect("dup-agent retained");
    assert_eq!(dup.messages.len(), 1);
    assert_eq!(dup.messages[0].content, "Real child output");

    // `since_ts` newer than max timestamp skips the store.
    let gated_ctx = ScanContext::with_roots(
        dir.path().join("cass-data"),
        vec![ScanRoot::local(acp_dir)],
        Some(1_800_000_000_000),
    );
    let gated = DevinConnector::new()
        .scan(&gated_ctx)
        .expect("since_ts scan");
    assert!(gated.is_empty());
}

#[test]
fn acp_source_boundary_completion_carries_live_wal() {
    let dir = TempDir::new().expect("tempdir");
    let stem = "66666666-6666-4666-8666-666666666666";
    let acp_dir = dir.path().join("acp-messages");
    let db_path = acp_dir.join(format!("{stem}.db"));
    devin_acp::build_main_session_db(&db_path);

    let writer = Connection::open(db_path.to_string_lossy().as_ref()).expect("open wal writer");
    writer
        .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .expect("enable wal");
    devin_acp::insert_message_row(
        &writer,
        10,
        "agent_message",
        &devin_acp::agent_message_payload(
            "wal-a",
            "turn-2",
            &["Uncheckpointed WAL turn"],
            "2026-11-15T10:02:00.000Z",
        ),
    );
    let wal_path = db_path.with_file_name(format!("{stem}.db-wal"));
    assert!(wal_path.is_file(), "fixture must hold live -wal");

    let connector = DevinConnector::new();
    let ctx = ScanContext::with_roots(
        dir.path().join("cass-data"),
        vec![ScanRoot::local(acp_dir)],
        None,
    );
    let mut completions: Vec<SourceCompletion> = Vec::new();
    let mut emitted = Vec::new();
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
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conv| {
                emitted.push(conv);
                Ok(())
            })
            .expect("scan with source boundaries");
    }
    assert_eq!(emitted.len(), 1);
    assert!(
        emitted[0]
            .messages
            .iter()
            .any(|m| m.content == "Uncheckpointed WAL turn")
    );
    assert_eq!(completions.len(), 1);
    let wal_sidecar = completions[0]
        .required_sidecars
        .iter()
        .find(|s| s.source_path == wal_path)
        .expect("acp completion must carry -wal sidecar");
    assert_eq!(wal_sidecar.role, DiscoveredSourceRole::MetadataSidecar);
    assert!(wal_sidecar.required_for_reconstruction);

    let discovered = connector.discover_source_files(&ctx).expect("discover");
    assert!(discovered.iter().any(|s| s.source_path == db_path));
    assert!(
        discovered
            .iter()
            .any(|s| s.source_path == wal_path && s.required_for_reconstruction)
    );

    let _ = writer.execute_compat("SELECT 1", params![]);
    drop(writer);
}

const CHILD_HOME_ENV: &str = "FAD_DEVIN_ACP_CHILD_HOME";
const CHILD_STEM_ENV: &str = "FAD_DEVIN_ACP_CHILD_STEM";

#[test]
fn acp_default_home_detection_and_scan_child() {
    if std::env::var_os(CHILD_HOME_ENV).is_none() {
        return;
    }
    let stem = std::env::var(CHILD_STEM_ENV).expect("child stem");
    let det = franken_detection_for_connector("devin").expect("detect devin");
    assert!(
        det.detected,
        "devin detect() must find IDE acp-messages store"
    );
    let ctx = ScanContext::local_default(std::path::PathBuf::new(), None);
    let convs = DevinConnector::new().scan(&ctx).expect("default scan");
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(stem.as_str()));
}

#[test]
fn acp_default_home_detection_and_scan_find_ide_store() {
    let dir = TempDir::new().expect("tempdir");
    let stem = "77777777-7777-4777-8777-777777777777";
    #[cfg(target_os = "macos")]
    let acp_dir = dir
        .path()
        .join("Library")
        .join("Application Support")
        .join("Devin")
        .join("User")
        .join("acp-messages");
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let acp_dir = dir
        .path()
        .join(".config")
        .join("Devin")
        .join("User")
        .join("acp-messages");
    #[cfg(target_os = "windows")]
    let acp_dir = dir
        .path()
        .join("AppData")
        .join("Roaming")
        .join("Devin")
        .join("User")
        .join("acp-messages");

    devin_acp::build_main_session_db(&acp_dir.join(format!("{stem}.db")));

    let exe = std::env::current_exe().expect("current_exe");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "acp_default_home_detection_and_scan_child",
            "--nocapture",
        ])
        .env("HOME", dir.path())
        .env(CHILD_HOME_ENV, dir.path())
        .env(CHILD_STEM_ENV, stem)
        .env_remove("CASS_DEVIN_DATA_ROOT")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run isolated child test");
    assert!(
        out.status.success(),
        "isolated HOME detection/scan child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

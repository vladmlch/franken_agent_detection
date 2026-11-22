//! Unit tests for `OpenCode` 2.x schema, tool-part archives, and WAL-index resilience.

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

use frankensqlite::params;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;

use super::OpenCodeConnector;
use super::models::{PartInfo, ToolState, ToolStateError};
use super::parts::{assemble_content_from_parts, extract_invocations_from_parts};
use super::sqlite::{v2_message_content, v2_message_invocations};
use super::tests_support::{
    add_opencode_v2_tables, create_opencode_storage, create_test_sqlite_db, insert_v1_text_message,
    insert_v2_message, insert_v2_session, open_test_connection, remote_scan_context, write_message,
    write_part, write_session,
};
use crate::connectors::Connector;
use crate::connectors::sqlite_sync::ConnectionExt;

#[test]
fn sqlite_v2_session_only_in_session_v2_is_indexed_in_seq_order() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    insert_v2_session(&conn, "ses_v2only", "Created on 2.x", 1_700_000_100_000);
    // seq, not insertion order or timestamps, is the transcript order.
    insert_v2_message(
        &conn,
        "ses_v2only",
        2,
        "assistant",
        r#"{"agent":"build","model":{"id":"gpt-5","providerID":"openai"},"time":{"created":1700000002000},
            "content":[
              {"type":"reasoning","text":"think first"},
              {"type":"text","text":"the answer"},
              {"type":"tool","id":"call_1","name":"read","state":{"status":"completed","input":{"path":"a.rs"},
               "content":[{"type":"text","text":"tool result line"},{"type":"file","uri":"file:///a.png","mime":"image/png"}]},
               "time":{"created":1700000002000}}]}"#,
    );
    insert_v2_message(
        &conn,
        "ses_v2only",
        1,
        "user",
        r#"{"text":"find the v2only phrase","time":{"created":1700000001000}}"#,
    );
    insert_v2_message(
        &conn,
        "ses_v2only",
        0,
        "agent-switched",
        r#"{"agent":"build","time":{"created":1700000000000}}"#,
    );
    insert_v2_message(
        &conn,
        "ses_v2only",
        3,
        "idle",
        r#"{"outcome":"succeeded","time":{"created":1700000003000}}"#,
    );
    drop(conn);

    let extracted = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(extracted.len(), 1);
    let session = &extracted[0];
    assert_eq!(session.external_id.as_deref(), Some("ses_v2only"));
    assert_eq!(session.title.as_deref(), Some("Created on 2.x"));
    assert_eq!(session.workspace, Some(PathBuf::from("/work/v2")));
    assert_eq!(session.metadata["opencode_schema"], "v2");
    assert_eq!(
        session.messages.len(),
        2,
        "idle and agent-switched rows carry no text"
    );
    assert_eq!(session.messages[0].role, "user");
    assert_eq!(session.messages[0].content, "find the v2only phrase");
    assert_eq!(session.messages[0].created_at, Some(1_700_000_001_000));
    assert_eq!(session.messages[1].role, "assistant");
    assert_eq!(session.messages[1].author.as_deref(), Some("gpt-5"));
    assert_eq!(
        session.messages[1].content,
        "[Reasoning]\nthink first\n\nthe answer\n\n[Tool Output]\ntool result line"
    );
    assert_eq!(session.messages[1].extra["seq"], 2);
    assert_eq!(session.messages[1].extra["type"], "assistant");
    assert_eq!(session.messages[1].invocations.len(), 1);
    assert_eq!(session.messages[1].invocations[0].kind, "tool");
    assert_eq!(session.messages[1].invocations[0].name, "read");
    assert_eq!(
        session.messages[1].invocations[0].call_id.as_deref(),
        Some("call_1")
    );
    assert_eq!(
        session.messages[1].invocations[0].arguments,
        Some(json!({"path": "a.rs"}))
    );
    assert_eq!(
        (session.messages[0].idx, session.messages[1].idx),
        (0, 1),
        "messages are reindexed in seq order"
    );
}

#[test]
fn extract_invocations_from_v1_parts_and_v2_messages_unwraps_skills() {
    let parts: Vec<PartInfo> = vec![
        serde_json::from_value(json!({
            "id": "part-1",
            "type": "tool",
            "tool": "read",
            "callID": "call-read-1",
            "state": {
                "input": {"path": "src/lib.rs"},
                "output": "pub mod connectors;"
            }
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "id": "part-2",
            "type": "tool",
            "tool": "skill",
            "callID": "call-skill-1",
            "state": {
                "input": {"name": "commit"}
            }
        }))
        .unwrap(),
    ];
    let v1_invs = extract_invocations_from_parts(&parts);
    assert_eq!(v1_invs.len(), 2);
    assert_eq!(v1_invs[0].kind, "tool");
    assert_eq!(v1_invs[0].name, "read");
    assert_eq!(v1_invs[0].call_id.as_deref(), Some("call-read-1"));
    assert_eq!(v1_invs[0].arguments, Some(json!({"path": "src/lib.rs"})));
    assert_eq!(v1_invs[1].kind, "skill");
    assert_eq!(v1_invs[1].name, "commit");
    assert_eq!(v1_invs[1].raw_name.as_deref(), Some("skill"));

    let v2_skill = v2_message_invocations(
        "skill",
        &json!({"skill": "sk_1", "name": "review", "text": "Check each hunk."}),
    );
    assert_eq!(v2_skill.len(), 1);
    assert_eq!(v2_skill[0].kind, "skill");
    assert_eq!(v2_skill[0].name, "review");
    assert_eq!(v2_skill[0].call_id.as_deref(), Some("sk_1"));

    let v2_shell = v2_message_invocations(
        "shell",
        &json!({"command": "cargo test", "status": "exited"}),
    );
    assert_eq!(v2_shell.len(), 1);
    assert_eq!(v2_shell[0].kind, "tool");
    assert_eq!(v2_shell[0].name, "shell");
    assert_eq!(
        v2_shell[0].arguments,
        Some(json!({"command": "cargo test"}))
    );
}

#[test]
fn sqlite_v2_migrated_session_is_indexed_once_from_its_v2_transcript() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    // A 1.x session migrated to 2.x keeps the same id in both schemas.
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params!["ses_migrated", "proj-1", "Old 1.x title", "/work/v1"],
    )
    .unwrap();
    insert_v1_text_message(&conn, "ses_migrated", "msg-old", "user", "the v1 copy");
    insert_v2_session(&conn, "ses_migrated", "2.x title", 1_700_000_100_000);
    insert_v2_message(
        &conn,
        "ses_migrated",
        1,
        "user",
        r#"{"text":"the v2 copy"}"#,
    );
    // A session that only exists in 1.x is still read from 1.x.
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params!["ses_v1only", "proj-1", "Only 1.x", "/work/v1"],
    )
    .unwrap();
    insert_v1_text_message(&conn, "ses_v1only", "msg-v1", "user", "a 1.x prompt");
    drop(conn);

    let mut convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    convs.sort_by(|a, b| a.external_id.cmp(&b.external_id));
    assert_eq!(convs.len(), 2, "the migrated session is indexed once");
    let migrated = &convs[0];
    assert_eq!(migrated.external_id.as_deref(), Some("ses_migrated"));
    assert_eq!(migrated.title.as_deref(), Some("2.x title"));
    assert_eq!(migrated.metadata["opencode_schema"], "v2");
    let contents: Vec<&str> = migrated
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(contents, vec!["the v2 copy"]);
    let v1_only = &convs[1];
    assert_eq!(v1_only.external_id.as_deref(), Some("ses_v1only"));
    assert_eq!(v1_only.metadata["opencode_schema"], "v1");
    assert_eq!(v1_only.messages[0].content, "a 1.x prompt");
}

#[test]
fn sqlite_v2_rows_without_text_neither_create_messages_nor_hide_the_v1_transcript() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    // 2.x-only session whose rows are all non-transcript events.
    insert_v2_session(&conn, "ses_events", "Events only", 1_700_000_100_000);
    insert_v2_message(
        &conn,
        "ses_events",
        1,
        "model-switched",
        r#"{"model":{"id":"m","providerID":"p"}}"#,
    );
    insert_v2_message(
        &conn,
        "ses_events",
        2,
        "compaction",
        r#"{"status":"running","reason":"auto","summary":"partial","recent":""}"#,
    );
    insert_v2_message(
        &conn,
        "ses_events",
        3,
        "idle",
        r#"{"outcome":"interrupted"}"#,
    );
    insert_v2_message(
        &conn,
        "ses_events",
        4,
        "assistant",
        r#"{"content":[{"type":"text","text":"   "}]}"#,
    );
    // Migrated session whose 2.x rows carry no text: its 1.x transcript is kept.
    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params!["ses_fallback", "proj-1", "Fallback", "/work/v1"],
    )
    .unwrap();
    insert_v1_text_message(&conn, "ses_fallback", "msg-fb", "user", "kept from 1.x");
    insert_v2_session(&conn, "ses_fallback", "Fallback", 1_700_000_100_000);
    insert_v2_message(
        &conn,
        "ses_fallback",
        1,
        "idle",
        r#"{"outcome":"succeeded"}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1, "a session with no text rows is not indexed");
    assert_eq!(convs[0].external_id.as_deref(), Some("ses_fallback"));
    assert_eq!(convs[0].metadata["opencode_schema"], "v1");
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "kept from 1.x");
}

#[test]
fn sqlite_v2_malformed_row_is_skipped_without_dropping_the_session() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    insert_v2_session(&conn, "ses_bad_row", "One bad row", 1_700_000_100_000);
    insert_v2_message(&conn, "ses_bad_row", 1, "user", "{not json");
    insert_v2_message(
        &conn,
        "ses_bad_row",
        2,
        "user",
        r#"{"text":"still indexed"}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "still indexed");
}

#[test]
fn sqlite_v2_transcripts_are_read_for_session_rows_without_session_v2_or_1x_tables() {
    // Later 2.x builds keep 2.x sessions in `session` itself; a fresh store
    // need not have the 1.x `message`/`part` tables at all.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    let conn = open_test_connection(&db_path);
    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL
        );",
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
         VALUES ('ses_dev', 'proj-3', 'dev', '/work/dev', 'Dev layout', '2.1.0', 1700000000000, 1700000000000)",
        params![],
    )
    .unwrap();
    drop(conn);
    add_opencode_v2_tables(&db_path, false);
    let conn = open_test_connection(&db_path);
    insert_v2_message(
        &conn,
        "ses_dev",
        1,
        "user",
        r#"{"text":"dev layout prompt"}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("ses_dev"));
    assert_eq!(convs[0].messages[0].content, "dev layout prompt");
}

#[test]
fn sqlite_v2_incremental_scan_skips_old_v2_sessions() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    insert_v2_session(&conn, "ses_old", "Old", 1_600_000_000_000);
    insert_v2_message(&conn, "ses_old", 1, "user", r#"{"text":"old prompt"}"#);
    insert_v2_session(&conn, "ses_new", "New", 1_700_000_100_000);
    insert_v2_message(&conn, "ses_new", 1, "user", r#"{"text":"new prompt"}"#);
    drop(conn);

    let convs =
        OpenCodeConnector::extract_from_sqlite(&db_path, Some(1_700_000_000_000), None).unwrap();
    let ids: Vec<Option<&str>> = convs.iter().map(|c| c.external_id.as_deref()).collect();
    assert_eq!(ids, vec![Some("ses_new")]);
}

#[test]
fn v2_message_content_renders_every_text_bearing_type() {
    let render = |kind: &str, data: serde_json::Value| v2_message_content(kind, &data);
    assert_eq!(
        render(
            "shell",
            json!({"command": "cargo test", "status": "exited",
            "output": {"output": "ok. 3 passed", "cursor": 12, "size": 12, "truncated": false}})
        ),
        Some(("tool", None, "$ cargo test\nok. 3 passed".to_string()))
    );
    assert_eq!(
        render(
            "skill",
            json!({"skill": "sk_1", "name": "review", "text": "Check each hunk."})
        ),
        Some((
            "tool",
            None,
            "[Skill: review]\nCheck each hunk.".to_string()
        ))
    );
    assert_eq!(
        render("system", json!({"text": "Working directory changed."})),
        Some(("system", None, "Working directory changed.".to_string()))
    );
    assert_eq!(
        render("synthetic", json!({"text": "Attached file contents"})),
        Some((
            "user",
            Some("user".to_string()),
            "Attached file contents".to_string()
        ))
    );
    assert_eq!(
        render(
            "compaction",
            json!({"status": "completed", "reason": "auto",
            "model": {"id": "gpt-5", "providerID": "openai"}, "summary": "Earlier work", "recent": ""})
        ),
        Some((
            "assistant",
            Some("gpt-5".to_string()),
            "[Compaction Summary]\nEarlier work".to_string()
        ))
    );
    assert_eq!(
        render("compaction", json!({"status": "failed", "reason": "auto"})),
        None
    );
    assert_eq!(render("location-switched", json!({"location": {}})), None);
    assert_eq!(render("idle", json!({"outcome": "succeeded"})), None);
}

#[test]
fn sqlite_v1_and_v2_preserve_model_provider_tokens_and_tool_call_counts() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);

    // v1 session with assistant message carrying modelID, providerID, tokens, and a tool part
    conn.execute(
        "INSERT INTO session (id, title, directory, project_id, time_created, time_updated) VALUES ('ses_v1', 'V1 Session', '/workspace', 'proj', 1700000000000, 1700000010000)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, data, time_created, time_updated) VALUES ('msg_v1', 'ses_v1', '{\"role\":\"assistant\",\"modelID\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\",\"tokens\":{\"input\":400,\"output\":120,\"reasoning\":30,\"cache\":{\"read\":80,\"write\":20}}}', 1700000005000, 1700000005000)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, data, time_created, time_updated) VALUES ('prt_v1', 'msg_v1', 'ses_v1', '{\"type\":\"tool\",\"tool\":\"bash\",\"callID\":\"call_v1\",\"state\":{\"input\":{\"command\":\"cargo test\"},\"output\":\"ok\"}}', 1700000005000, 1700000005000)",
    )
    .unwrap();

    // v2 session with assistant message carrying model, tokens, and a tool item
    insert_v2_session(&conn, "ses_v2", "V2 Session", 1_700_000_020_000);
    insert_v2_message(
        &conn,
        "ses_v2",
        1,
        "assistant",
        r#"{"model":{"id":"gpt-5","providerID":"openai"},"tokens":{"input":250,"output":90,"reasoning":15,"cache":{"read":40,"write":10}},"content":[{"type":"text","text":"Running check"},{"type":"tool","id":"call_v2","name":"read","state":{"input":{"path":"src/lib.rs"},"content":[]}}]}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    let v1 = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some("ses_v1"))
        .expect("v1 session");
    let v1_msg = &v1.messages[0];
    assert_eq!(
        v1_msg
            .extra
            .get("modelID")
            .and_then(serde_json::Value::as_str),
        Some("claude-sonnet-4-5")
    );
    assert_eq!(
        v1_msg
            .extra
            .get("providerID")
            .and_then(serde_json::Value::as_str),
        Some("anthropic")
    );
    assert_eq!(
        v1_msg
            .extra
            .pointer("/tokens/input")
            .and_then(serde_json::Value::as_i64),
        Some(400)
    );
    assert_eq!(
        v1_msg
            .extra
            .pointer("/cass/tool_call_count")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );

    let v2 = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some("ses_v2"))
        .expect("v2 session");
    let v2_msg = &v2.messages[0];
    assert_eq!(
        v2_msg
            .extra
            .get("modelID")
            .and_then(serde_json::Value::as_str),
        Some("gpt-5")
    );
    assert_eq!(
        v2_msg
            .extra
            .get("providerID")
            .and_then(serde_json::Value::as_str),
        Some("openai")
    );
    assert_eq!(
        v2_msg
            .extra
            .pointer("/tokens/output")
            .and_then(serde_json::Value::as_i64),
        Some(90)
    );
    assert_eq!(
        v2_msg
            .extra
            .pointer("/cass/tool_call_count")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
}

// =====================================================
// Tool part identity/state kept in extras (cass F14)
// =====================================================

#[test]
fn sqlite_v2_tool_part_identity_and_state_archived_verbatim() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    insert_v2_session(&conn, "ses_tool", "Tool session", 1_700_000_100_000);
    insert_v2_message(&conn, "ses_tool", 0, "user", r#"{"text":"probe the repo"}"#);
    insert_v2_message(
        &conn,
        "ses_tool",
        1,
        "assistant",
        r#"{"agent":"build","model":{"id":"gpt-5","providerID":"openai"},"time":{"created":1700000002000},
            "content":[
              {"type":"reasoning","text":"plan first","native":{},"time":{}},
              {"type":"text","text":"running it now"},
              {"type":"tool","executed":false,"id":"prt_1","name":"shell","time":{"created":1,"ran":2,"completed":3},
               "state":{"status":"completed","input":{"command":"make check"},"metadata":{"duration":3},
                 "content":[{"type":"text","text":"ok. 3 passed"}]}},
              {"type":"tool","executed":false,"id":"prt_2","name":"edit","time":{"created":4,"ran":5,"completed":6},
               "state":{"status":"error","input":{"path":"b.rs","oldString":"x","newString":"y"},
                 "error":"exit 1: no such file"}}]}"#,
    );
    // The no-tool-parts shape (the ses_ebad… blocker): the content
    // items are still archived so the host can PROVE no tool part
    // exists instead of treating absence as unverifiable.
    insert_v2_message(
        &conn,
        "ses_tool",
        2,
        "assistant",
        r#"{"agent":"build","model":{"id":"gpt-5","providerID":"openai"},"time":{"created":1700000003000},
            "content":[
              {"type":"reasoning","text":"wrap up","native":{},"time":{}},
              {"type":"text","text":"all done"}]}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 3, "user + two assistant rows");
    let tool_row = &convs[0].messages[1];
    // Text results render inline; the error state's text renders too
    // (it is source text — dropping it left the text inventory a gap).
    assert_eq!(
        tool_row.content,
        "[Reasoning]\nplan first\n\nrunning it now\n\n[Tool Output]\nok. 3 passed\n\n[Tool Error]\nexit 1: no such file"
    );
    // The raw content items are archived verbatim in the extras.
    let parts = tool_row.extra["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0]["type"], "reasoning");
    assert_eq!(parts[2]["id"], "prt_1");
    assert_eq!(parts[2]["name"], "shell");
    assert_eq!(parts[2]["executed"], false);
    assert_eq!(parts[2]["state"]["status"], "completed");
    assert_eq!(parts[2]["state"]["input"]["command"], "make check");
    assert_eq!(parts[2]["state"]["content"][0]["text"], "ok. 3 passed");
    assert_eq!(parts[3]["id"], "prt_2");
    assert_eq!(parts[3]["state"]["status"], "error");
    assert_eq!(parts[3]["state"]["input"]["path"], "b.rs");
    assert_eq!(parts[3]["state"]["error"], "exit 1: no such file");
    let plain_row = &convs[0].messages[2];
    assert_eq!(plain_row.content, "[Reasoning]\nwrap up\n\nall done");
    let plain_parts = plain_row.extra["parts"].as_array().unwrap();
    assert_eq!(plain_parts.len(), 2, "no-tool rows archive their parts too");
    assert!(plain_parts.iter().all(|p| p["type"] != "tool"));
}

#[test]
fn sqlite_v2_textless_tool_part_row_is_kept() {
    // A tool part whose state holds no text at all (file-only results)
    // used to vanish with its whole row, dropping its identity and
    // state. The row is kept with empty content — F13's codex rule —
    // so the host can still prove the part existed.
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    insert_v2_session(&conn, "ses_img", "Image session", 1_700_000_100_000);
    insert_v2_message(&conn, "ses_img", 0, "user", r#"{"text":"draw a cat"}"#);
    insert_v2_message(
        &conn,
        "ses_img",
        1,
        "assistant",
        r#"{"agent":"build","model":{"id":"gpt-5","providerID":"openai"},"time":{"created":1700000002000},
            "content":[
              {"type":"tool","executed":false,"id":"prt_img","name":"generate","time":{},
               "state":{"status":"completed","input":{"prompt":"a cat"},
                 "content":[{"type":"file","uri":"file:///out.png","mime":"image/png"}]}}]}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        2,
        "the textless tool row must survive"
    );
    let row = &convs[0].messages[1];
    assert_eq!(row.content, "");
    assert_eq!(row.role, "assistant");
    let parts = row.extra["parts"].as_array().unwrap();
    assert_eq!(parts[0]["id"], "prt_img");
    assert_eq!(parts[0]["state"]["content"][0]["mime"], "image/png");
}

#[test]
fn sqlite_v2_error_object_message_renders_and_archives() {
    // The real failed-run shape: `state.error` is an OBJECT carrying the
    // failure text under `message` (measured shape: `{message, type}`,
    // status `error`), not a plain string. The render must deliver that
    // text and the archive must keep the object verbatim.
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    add_opencode_v2_tables(&db_path, true);
    let conn = open_test_connection(&db_path);
    insert_v2_session(&conn, "ses_err", "Error session", 1_700_000_100_000);
    insert_v2_message(&conn, "ses_err", 0, "user", r#"{"text":"run it"}"#);
    insert_v2_message(
        &conn,
        "ses_err",
        1,
        "assistant",
        r#"{"agent":"build","model":{"id":"gpt-5","providerID":"openai"},"time":{"created":1700000002000},
            "content":[
              {"type":"text","text":"trying now"},
              {"type":"tool","executed":false,"id":"prt_err","name":"shell","time":{},
               "state":{"status":"error","input":{"command":"make check"},
                 "error":{"message":"boom: denied","type":"tool_error"}}}]}"#,
    );
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    let row = &convs[0].messages[1];
    assert_eq!(row.content, "trying now\n\n[Tool Error]\nboom: denied");
    let parts = row.extra["parts"].as_array().unwrap();
    assert_eq!(parts[1]["id"], "prt_err");
    assert_eq!(parts[1]["state"]["status"], "error");
    assert_eq!(parts[1]["state"]["error"]["message"], "boom: denied");
    assert_eq!(parts[1]["state"]["error"]["type"], "tool_error");
}

#[test]
fn sqlite_legacy_tool_part_state_archived_verbatim() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["sess-lt", "Legacy tools"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["msg-lt", "sess-lt", r#"{"role":"assistant"}"#],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-lt-1",
            "msg-lt",
            "sess-lt",
            r#"{"id":"p-lt-1","messageID":"msg-lt","order":0,"type":"text","text":"checking"}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-lt-2",
            "msg-lt",
            "sess-lt",
            r#"{"id":"p-lt-2","messageID":"msg-lt","order":1,"type":"tool",
                "state":{"id":"toolu_01","status":"completed","input":{"command":"make"},
                "output":"done","title":"Bash","metadata":{"exit":0},"time":{"start":1,"end":2}}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-lt-3",
            "msg-lt",
            "sess-lt",
            r#"{"id":"p-lt-3","messageID":"msg-lt","order":2,"type":"tool",
                "state":{"id":"toolu_02","status":"error","input":{"path":"c.rs"},"error":"boom: denied"}}"#,
        ],
    )
    .unwrap();
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    let message = &convs[0].messages[0];
    assert_eq!(
        message.content,
        "checking\n\n[Tool Output]\ndone\n\n[Tool Error]\nboom: denied"
    );
    let parts = message.extra["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[1]["id"], "p-lt-2");
    assert_eq!(parts[1]["state"]["id"], "toolu_01");
    assert_eq!(parts[1]["state"]["status"], "completed");
    assert_eq!(parts[1]["state"]["input"]["command"], "make");
    assert_eq!(parts[1]["state"]["title"], "Bash");
    assert_eq!(parts[2]["state"]["error"], "boom: denied");
    assert_eq!(parts[2]["state"]["input"]["path"], "c.rs");
}

#[test]
fn sqlite_legacy_error_object_part_kept_and_rendered() {
    // Same failed-run shape on a 1.x part row: the error record is an
    // object with the text under `message`. The typed view must not drop
    // the whole part (identity) over the error's shape, and the text
    // must render.
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["sess-eo", "Error object"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["msg-eo", "sess-eo", r#"{"role":"assistant"}"#],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-eo-1",
            "msg-eo",
            "sess-eo",
            r#"{"id":"p-eo-1","messageID":"msg-eo","order":0,"type":"text","text":"checking"}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-eo-2",
            "msg-eo",
            "sess-eo",
            r#"{"id":"p-eo-2","messageID":"msg-eo","order":1,"type":"tool",
                "state":{"id":"toolu_e1","status":"error","input":{"command":"make"},
                "error":{"message":"boom: denied","type":"tool_error"},"title":"Bash"}}"#,
        ],
    )
    .unwrap();
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1, "the tool part must survive");
    let message = &convs[0].messages[0];
    assert_eq!(message.content, "checking\n\n[Tool Error]\nboom: denied");
    let parts = message.extra["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2, "both parts are archived");
    assert_eq!(parts[1]["state"]["id"], "toolu_e1");
    assert_eq!(parts[1]["state"]["error"]["message"], "boom: denied");
    assert_eq!(parts[1]["state"]["error"]["type"], "tool_error");
}

#[test]
fn sqlite_legacy_unreadable_error_shape_keeps_identity() {
    // An error record with no renderable text — an object without a
    // `message`, or a non-string non-object shape — keeps the part (and
    // its identity/state) alive; nothing is guessed or dropped.
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["sess-odd", "Odd errors"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["msg-odd", "sess-odd", r#"{"role":"assistant"}"#],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-od-1",
            "msg-odd",
            "sess-odd",
            r#"{"id":"p-od-1","messageID":"msg-odd","order":0,"type":"tool",
                "state":{"id":"toolu_o1","status":"error","input":{"command":"ls"},
                "error":{"type":"no_message"},"title":"Bash"}}"#,
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-od-2",
            "msg-odd",
            "sess-odd",
            r#"{"id":"p-od-2","messageID":"msg-odd","order":1,"type":"tool",
                "state":{"id":"toolu_o2","status":"error","input":{"command":"pwd"},
                "error":42,"title":"Bash"}}"#,
        ],
    )
    .unwrap();
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        1,
        "both identity-bearing tool parts must survive"
    );
    let message = &convs[0].messages[0];
    assert_eq!(message.content, "", "no text to render, row still kept");
    assert!(!message.content.contains("[Tool Error]"));
    let parts = message.extra["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["state"]["error"]["type"], "no_message");
    assert_eq!(parts[1]["state"]["error"], 42);
}

#[test]
fn sqlite_legacy_textless_tool_part_message_is_kept() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_sqlite_db(dir.path());
    let conn = open_test_connection(&db_path);
    conn.execute_compat(
        "INSERT INTO session (id, title) VALUES (?1, ?2)",
        params!["sess-lq", "Legacy quiet"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params!["msg-lq", "sess-lq", r#"{"role":"assistant"}"#],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "p-lq-1",
            "msg-lq",
            "sess-lq",
            r#"{"id":"p-lq-1","messageID":"msg-lq","order":0,"type":"tool",
                "state":{"id":"toolu_03","status":"completed","input":{"command":"ls"},"title":"Bash"}}"#,
        ],
    )
    .unwrap();
    drop(conn);

    let convs = OpenCodeConnector::extract_from_sqlite(&db_path, None, None).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        1,
        "the textless tool message survives"
    );
    let message = &convs[0].messages[0];
    assert_eq!(message.content, "");
    let parts = message.extra["parts"].as_array().unwrap();
    assert_eq!(parts[0]["state"]["id"], "toolu_03");
}

#[test]
fn file_layout_tool_part_state_archived_verbatim() {
    let dir = TempDir::new().unwrap();
    let storage = create_opencode_storage(&dir);
    let session = json!({"id": "sess-ft", "projectID": "proj-001"});
    write_session(&storage, "proj-001", &session);
    let message = json!({
        "id": "msg-ft", "role": "assistant", "sessionID": "sess-ft",
        "time": {"created": 1733000000}
    });
    write_message(&storage, "sess-ft", &message);
    write_part(
        &storage,
        "msg-ft",
        &json!({
            "id": "p-ft-1", "messageID": "msg-ft", "order": 0, "type": "text",
            "text": "checking files"
        }),
    );
    write_part(
        &storage,
        "msg-ft",
        &json!({
            "id": "p-ft-2", "messageID": "msg-ft", "order": 1, "type": "tool",
            "state": {"id": "toolu_04", "status": "completed",
                "input": {"path": "d.rs"}, "output": "42 lines", "title": "Read"}
        }),
    );
    // A textless tool part keeps its message alive too.
    write_message(
        &storage,
        "sess-ft",
        &json!({
            "id": "msg-ft-2", "role": "assistant", "sessionID": "sess-ft",
            "time": {"created": 1733000001}
        }),
    );
    write_part(
        &storage,
        "msg-ft-2",
        &json!({
            "id": "p-ft-3", "messageID": "msg-ft-2", "order": 0, "type": "tool",
            "state": {"id": "toolu_05", "status": "completed",
                "input": {"command": "pwd"}, "title": "Bash"}
        }),
    );

    let connector = OpenCodeConnector::new();
    let ctx = remote_scan_context(&storage, &storage);
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    let first = &convs[0].messages[0];
    assert_eq!(first.content, "checking files\n\n[Tool Output]\n42 lines");
    let parts = first.extra["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["state"]["id"], "toolu_04");
    assert_eq!(parts[1]["state"]["input"]["path"], "d.rs");
    let quiet = &convs[0].messages[1];
    assert_eq!(quiet.content, "");
    let quiet_parts = quiet.extra["parts"].as_array().unwrap();
    assert_eq!(quiet_parts[0]["state"]["id"], "toolu_05");
}

#[test]
fn assemble_content_from_parts_renders_tool_error() {
    let parts = vec![PartInfo {
        id: Some("p-err".into()),
        index: Some(0),
        message_id: None,
        part_type: Some("tool".into()),
        text: None,
        tool: None,
        call_id: None,
        state: Some(ToolState {
            input: None,
            output: Some("partial out".into()),
            error: Some(ToolStateError::Text("boom: denied".into())),
        }),
    }];
    let content = assemble_content_from_parts(&parts);
    assert_eq!(
        content,
        "[Tool Output]\npartial out\n\n[Tool Error]\nboom: denied"
    );
    // The object error shape renders its `message` text; a shape with
    // no renderable text renders nothing without losing the part.
    let parts = vec![PartInfo {
        id: Some("p-err-obj".into()),
        index: Some(0),
        message_id: None,
        part_type: Some("tool".into()),
        text: None,
        tool: None,
        call_id: None,
        state: Some(ToolState {
            input: None,
            output: None,
            error: Some(ToolStateError::Object {
                message: Some("denied again".into()),
            }),
        }),
    }];
    assert_eq!(
        assemble_content_from_parts(&parts),
        "[Tool Error]\ndenied again"
    );
    let parts = vec![PartInfo {
        id: Some("p-err-odd".into()),
        index: Some(0),
        message_id: None,
        part_type: Some("tool".into()),
        text: None,
        tool: None,
        call_id: None,
        state: Some(ToolState {
            input: None,
            output: None,
            error: Some(ToolStateError::Other(serde_json::json!({"type": "x"}))),
        }),
    }];
    assert_eq!(assemble_content_from_parts(&parts), "");
}

// =====================================================
// Unusable WAL-index fallback (cass F9)
// =====================================================

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

/// An OpenCode SQLite store in WAL mode with one committed session, closed
/// the way a hard-killed OpenCode leaves it: the writer is gone, and a
/// checkpointed `-wal` (0 bytes) plus its `-shm` wal-index remain next to
/// the db.
fn create_wal_mode_sqlite_db(dir: &Path) -> PathBuf {
    let db_path = dir.join("opencode.db");
    let conn = open_test_connection(&db_path);
    let mode: String = conn
        .query_row_map("PRAGMA journal_mode=wal;", &[], |row| {
            row.get_typed::<String>(0)
        })
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");

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

    conn.execute_compat(
        "INSERT INTO session (id, project_id, title, directory) VALUES (?1, ?2, ?3, ?4)",
        params!["sess-1", "proj-1", "Crashed Store", "/home/user/project"],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
        params![
            "msg-1",
            "sess-1",
            r#"{"role":"user","time":{"created":1700000000000}}"#
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
        params![
            "part-1",
            "msg-1",
            "sess-1",
            r#"{"type":"text","text":"Hello world"}"#
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

/// A store left with an unusable wal-index (the OpenCode crash shape from
/// the 2026-11-07 zero-index incident) must still be read, without
/// modifying the user's files. The db and `-wal` are never written by any
/// read-only open; the `-shm` is never written whenever the snapshot
/// fallback engages, and `garbage`/`torn` wal-indexes refuse the strict
/// open on every frankensqlite build, so the fallback always engages for
/// them.
#[serial]
#[test]
fn sqlite_scan_reads_a_store_left_with_an_unusable_wal_index() {
    for state in ["garbage", "torn", "absent", "zeroed", "empty"] {
        let dir = TempDir::new().unwrap();
        let db_path = create_wal_mode_sqlite_db(dir.path());
        damage_wal_index(&db_path, state);
        let before_db = file_state(&db_path);
        let before_wal = file_state(&sqlite_sidecar(&db_path, "-wal"));
        let before_shm = file_state(&sqlite_sidecar(&db_path, "-shm"));

        let ctx = remote_scan_context(dir.path(), dir.path());
        let convs = OpenCodeConnector::new()
            .scan(&ctx)
            .unwrap_or_else(|error| panic!("{state}: scan failed: {error:#}"));

        assert_eq!(
            convs.len(),
            1,
            "{state}: the crashed store must still be read"
        );
        assert_eq!(convs[0].external_id.as_deref(), Some("sess-1"), "{state}");
        assert_eq!(
            convs[0].messages[0].content, "Hello world",
            "{state}: the session transcript must be readable"
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
#[serial]
#[test]
fn sqlite_scan_reports_an_unreadable_store_as_a_scan_error() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("opencode.db"),
        b"this is not a sqlite database",
    )
    .unwrap();

    let ctx = remote_scan_context(dir.path(), dir.path());
    let error = OpenCodeConnector::new().scan(&ctx).unwrap_err();
    let chain = format!("{error:#}");
    assert!(
        chain.contains("opencode.db"),
        "the scan error must name the failing store: {chain}"
    );
}

/// cass follow-up to F9 (2026-11-07): a store that is a *valid but empty*
/// database is not a failure. SQLite's own semantics make a 0-byte file an
/// empty database, and a fresh install commonly creates the store file
/// before its first write; a readable schema with no tables at all holds
/// no sessions either way. Both must scan to `Ok` with 0 conversations
/// (logged at debug), not a scan error on every index run.
#[test]
fn sqlite_scan_treats_a_provably_empty_store_as_no_sessions() {
    for state in ["zero-byte", "no-tables"] {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("opencode.db");
        match state {
            // A fresh install before its first write: the store file
            // exists but holds nothing.
            "zero-byte" => fs::write(&db_path, []).unwrap(),
            // A real database file (valid header, nonzero size) whose
            // schema holds no tables at all.
            "no-tables" => {
                let conn = open_test_connection(&db_path);
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

        let ctx = remote_scan_context(dir.path(), dir.path());
        let convs = OpenCodeConnector::new().scan(&ctx).unwrap_or_else(|error| {
            panic!("{state}: an empty store must not be a scan error: {error:#}")
        });
        assert!(
            convs.is_empty(),
            "{state}: an empty store holds no conversations"
        );
    }
}

/// Schema drift is not emptiness: a database that has *some* tables but
/// none of OpenCode's is an app migration or corruption, and must still
/// surface as a scan error naming the store (F5 semantics, unchanged).
#[test]
fn sqlite_scan_reports_schema_drift_as_a_scan_error() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    let conn = open_test_connection(&db_path);
    conn.execute("CREATE TABLE unrelated_table (x INTEGER);")
        .unwrap();
    drop(conn);

    let ctx = remote_scan_context(dir.path(), dir.path());
    let error = OpenCodeConnector::new().scan(&ctx).unwrap_err();
    let chain = format!("{error:#}");
    assert!(
        chain.contains("opencode.db"),
        "schema drift must name the failing store: {chain}"
    );
}

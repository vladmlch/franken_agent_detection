//! Synthetic Devin IDE `acp-messages/<uuid>.db` fixture builders.
//!
//! All records are constructed purely from the observed structural schema in
//! `acp_shapes.md` with zero personal or production data.

use std::path::Path;

use franken_agent_detection::connectors::sqlite_sync::{Connection, ConnectionExt};
use frankensqlite::compat::RowExt;
use frankensqlite::params;
use serde_json::{Value, json};

fn open_rw(path: &Path) -> Connection {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture parent dir");
    }
    Connection::open(path.to_string_lossy().as_ref()).expect("open fixture sqlite db")
}

fn init_schema(conn: &Connection, include_messages: bool) {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);")
        .expect("create meta table");
    if include_messages {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (position INTEGER PRIMARY KEY, kind TEXT, payload TEXT);",
        )
        .expect("create messages table");
    }
}

fn insert_meta(conn: &Connection, title: Option<&str>, model: Option<&str>, message_count: usize) {
    let mut config_options = vec![json!({
        "id": "mode",
        "category": "mode",
        "currentValue": "agent",
    })];
    if let Some(model_name) = model {
        config_options.push(json!({
            "id": "model",
            "category": "model",
            "currentValue": model_name,
        }));
    }
    let mut info = serde_json::Map::new();
    info.insert("configOptions".into(), Value::Array(config_options));
    info.insert("availableCommands".into(), json!([]));
    if let Some(t) = title {
        info.insert("title".into(), Value::String(t.to_owned()));
    }
    let info_json = Value::Object(info).to_string();
    for (k, v) in [
        ("schema_version", "1".to_owned()),
        ("message_count", message_count.to_string()),
        ("truncated", "false".to_owned()),
        ("info", info_json),
    ] {
        conn.execute_compat(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![k, v],
        )
        .expect("insert meta row");
    }
}

pub fn insert_message_row(conn: &Connection, position: i64, kind: &str, payload: &Value) {
    conn.execute_compat(
        "INSERT INTO messages (position, kind, payload) VALUES (?1, ?2, ?3)",
        params![position, kind, payload.to_string()],
    )
    .expect("insert acp message row");
}

#[must_use]
pub fn user_message_payload(id: &str, text: &str, ts: &str) -> Value {
    json!({
        "id": id,
        "kind": "user_message",
        "content": [{
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text },
            "_meta": { "cognition.ai/timestamp": ts },
        }],
    })
}

#[must_use]
pub fn agent_thought_payload(id: &str, turn_id: &str, chunks: &[&str], ts: &str) -> Value {
    let content: Vec<Value> = chunks
        .iter()
        .map(|chunk| {
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": chunk },
                "_meta": { "cognition.ai/timestamp": ts },
            })
        })
        .collect();
    json!({
        "id": id,
        "kind": "agent_thought",
        "turnId": turn_id,
        "content": content,
    })
}

#[must_use]
pub fn agent_message_payload(id: &str, turn_id: &str, chunks: &[&str], ts: &str) -> Value {
    let content: Vec<Value> = chunks
        .iter()
        .map(|chunk| {
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": chunk },
                "_meta": { "cognition.ai/timestamp": ts },
            })
        })
        .collect();
    json!({
        "id": id,
        "kind": "agent_message",
        "turnId": turn_id,
        "content": content,
    })
}

#[must_use]
pub fn tool_call_payload(
    call_id: &str,
    tool_name: &str,
    kind: &str,
    title: &str,
    cwd: &str,
    ts_ms: i64,
    ts_iso: &str,
) -> Value {
    json!({
        "id": call_id,
        "kind": "tool_call",
        "sourceEventTimestampMs": ts_ms,
        "sourceEventIsStart": false,
        "warnings": [],
        "content": {
            "toolCallId": call_id,
            "title": title,
            "kind": kind,
            "rawInput": { "path": "README.md" },
            "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }],
            "_meta": {
                "cognition.ai/inferenceToolName": tool_name,
                "cognition.ai/cwd": cwd,
                "cognition.ai/timestamp": ts_iso,
            },
            "status": "completed",
        },
    })
}

#[must_use]
pub fn subagent_payload(
    agent_id: &str,
    title: &str,
    status: &str,
    success: Option<bool>,
    child_messages: Value,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), Value::String(format!("sub-{agent_id}")));
    obj.insert("kind".into(), Value::String("subagent".to_owned()));
    obj.insert("agentId".into(), Value::String(agent_id.to_owned()));
    obj.insert("title".into(), Value::String(title.to_owned()));
    obj.insert("task".into(), Value::String(format!("Task: {title}")));
    obj.insert("profile".into(), Value::String("explore".to_owned()));
    obj.insert("depth".into(), json!(1));
    obj.insert("isBackground".into(), json!(false));
    obj.insert("status".into(), Value::String(status.to_owned()));
    obj.insert("childMessages".into(), child_messages);
    if let Some(ok) = success {
        obj.insert("success".into(), Value::Bool(ok));
    }
    Value::Object(obj)
}

/// Fixture 1: standard main session with user, thought, `tool_call`, and assistant messages.
pub fn build_main_session_db(path: &Path) {
    let conn = open_rw(path);
    init_schema(&conn, true);
    let rows = vec![
        (
            0_i64,
            "user_message",
            user_message_payload(
                "u1",
                "<project_context>hidden</project_context>\nInspect crate layout",
                "2026-11-15T10:00:00.000Z",
            ),
        ),
        (
            1,
            "agent_thought",
            agent_thought_payload(
                "t1",
                "turn-1",
                &["Checking ", "Cargo.toml"],
                "2026-11-15T10:00:01.000Z",
            ),
        ),
        (
            2,
            "tool_call",
            tool_call_payload(
                "call-1",
                "read",
                "read",
                "Read Cargo.toml",
                "/work/repo",
                1_794_736_802_000,
                "2026-11-15T10:00:02.000Z",
            ),
        ),
        (
            3,
            "agent_message",
            agent_message_payload(
                "a1",
                "turn-1",
                &["Layout ", "verified."],
                "2026-11-15T10:00:03.000Z",
            ),
        ),
    ];
    insert_meta(
        &conn,
        Some("Inspect layout"),
        Some("claude-sonnet-4-6"),
        rows.len(),
    );
    for (pos, kind, payload) in &rows {
        insert_message_row(&conn, *pos, kind, payload);
    }
}

/// Fixture 2: session containing a `kind = 'subagent'` row with two `childMessages`.
pub fn build_subagent_session_db(path: &Path) {
    let conn = open_rw(path);
    init_schema(&conn, true);
    let child_msgs = json!([
        agent_thought_payload(
            "ct1",
            "sub-turn-1",
            &["Scanning module"],
            "2026-11-15T10:01:00.000Z",
        ),
        agent_message_payload(
            "ca1",
            "sub-turn-1",
            &["Module check complete."],
            "2026-11-15T10:01:05.000Z",
        ),
    ]);
    let rows = vec![
        (
            0_i64,
            "user_message",
            user_message_payload("u1", "Delegate module scan", "2026-11-15T10:00:50.000Z"),
        ),
        (
            1,
            "subagent",
            subagent_payload(
                "agent-42",
                "Module scanner",
                "completed",
                Some(true),
                child_msgs,
            ),
        ),
        (
            2,
            "agent_message",
            agent_message_payload(
                "a1",
                "turn-1",
                &["Subagent finished."],
                "2026-11-15T10:01:10.000Z",
            ),
        ),
    ];
    insert_meta(&conn, Some("Parent ACP session"), Some("gpt-5"), rows.len());
    for (pos, kind, payload) in &rows {
        insert_message_row(&conn, *pos, kind, payload);
    }
}

/// Fixture 3: empty store (0-byte file, missing `messages` table, or initialized empty table).
pub fn build_empty_store_db(path: &Path, include_schema: bool, include_messages_table: bool) {
    if !include_schema {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent dir");
        }
        std::fs::write(path, b"").expect("write 0-byte db");
        return;
    }
    let conn = open_rw(path);
    init_schema(&conn, include_messages_table);
    insert_meta(&conn, None, None, 0);
}

pub fn count_tables(path: &Path) -> usize {
    let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open fixture db");
    conn.query_row_map(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
        params![],
        |row| row.get_typed::<i64>(0),
    )
    .expect("count tables")
    .try_into()
    .unwrap_or(0)
}

//! Tests for the Composer + Agent-transcript fragment merge (design
//! 2026-11-08-cursor-fragment-design, §6.1–§6.5). All fixtures are
//! synthetic: a `state.vscdb`-shaped SQLite database plus JSONL transcripts
//! under one explicit scan root, with made-up text only. The same session
//! id on both sides is what triggers the merge; transcript-only and
//! composer-only shapes pin today's byte-for-byte behavior.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::CursorConnector;
use super::agent_transcripts::AGENT_TRANSCRIPTS_DIR;
use crate::connectors::Connector;
use crate::connectors::scan::{ScanContext, ScanRoot};
use crate::connectors::sqlite_sync::{Connection, ConnectionExt, params};
use crate::types::{NormalizedConversation, NormalizedMessage};

// ------------------------------------------------------------------
// Fixture helpers
// ------------------------------------------------------------------

/// Create a Cursor-shaped SQLite store (cursorDiskKV + ItemTable).
fn create_store(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn
}

/// Write one Composer session (v0.40 headers-only shape) into a store:
/// a `composerData:<sid>` row plus one `bubbleId:<sid>:<bubble>` row per
/// message.
fn write_composer_session(db_path: &Path, sid: &str, composer: &Value, bubbles: &[(&str, Value)]) {
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = create_store(db_path);
    let composer_key = format!("composerData:{sid}");
    conn.execute_compat(
        "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params![composer_key.as_str(), composer.to_string()],
    )
    .unwrap();
    for (bubble_id, bubble) in bubbles {
        let bubble_key = format!("bubbleId:{sid}:{bubble_id}");
        conn.execute_compat(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![bubble_key.as_str(), bubble.to_string()],
        )
        .unwrap();
    }
    drop(conn);
}

fn text_record(role: &str, text: &str) -> Value {
    json!({"role": role, "message": {"content": [{"type": "text", "text": text}]}})
}

fn tool_record(role: &str, text: &str, tool: &str, file_path: &str) -> Value {
    json!({"role": role, "message": {"content": [
        {"type": "text", "text": text},
        {"type": "tool_use", "name": tool, "input": {"file_path": file_path}},
    ]}})
}

fn tool_only_record(role: &str, tool: &str, file_path: &str) -> Value {
    json!({"role": role, "message": {"content": [
        {"type": "tool_use", "name": tool, "input": {"file_path": file_path}},
    ]}})
}

fn turn_ended(status: &str) -> Value {
    json!({"type": "turn_ended", "status": status})
}

fn error_turn_ended(error: &str) -> Value {
    json!({"type": "turn_ended", "status": "error", "error": error})
}

fn jsonl_text(records: &[Value]) -> String {
    let mut text = String::new();
    for record in records {
        text.push_str(&record.to_string());
        text.push('\n');
    }
    text
}

/// Write a primary Agent transcript `<root>/<project>/agent-transcripts/<sid>/<sid>.jsonl`.
fn write_transcript(root: &Path, project: &str, sid: &str, records: &[Value]) -> PathBuf {
    let dir = root.join(project).join(AGENT_TRANSCRIPTS_DIR).join(sid);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{sid}.jsonl"));
    fs::write(&file, jsonl_text(records)).unwrap();
    file
}

fn ctx_for(root: &Path) -> ScanContext {
    ScanContext::with_roots(
        root.to_path_buf(),
        vec![ScanRoot::local(root.to_path_buf())],
        None,
    )
}

fn ctx_since(root: &Path, since_ts: i64) -> ScanContext {
    ScanContext::with_roots(
        root.to_path_buf(),
        vec![ScanRoot::local(root.to_path_buf())],
        Some(since_ts),
    )
}

fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

fn pair_row(conv: &NormalizedConversation, i: usize) -> &NormalizedMessage {
    &conv.messages[i]
}

// ------------------------------------------------------------------
// The canonical fixture: one Composer session and a same-id transcript
// exercising every merge tier, plus insert / suppress / error-record
// shapes. Expected merged sequence (idx 0..8) is written out below and
// was computed by hand from the design rules.
// ------------------------------------------------------------------

fn canonical_composer() -> Value {
    json!({
        "name": "Login regression",
        "createdAt": 1700000000000i64,
        "lastUpdatedAt": 1700000008000i64,
        "unifiedMode": "agentPlus",
        "modelConfig": {"modelName": "test-model"},
        "fullConversationHeadersOnly": [
            {"bubbleId": "u1"}, {"bubbleId": "a1"}, {"bubbleId": "a2"},
            {"bubbleId": "u2"}, {"bubbleId": "a3"}, {"bubbleId": "u3"},
            {"bubbleId": "u4"},
        ],
    })
}

fn canonical_bubbles() -> Vec<(&'static str, Value)> {
    vec![
        (
            "u1",
            json!({"type": 1, "text": "Fix the login bug", "timestamp": 1700000001000i64, "workspaceProjectDir": "/work/demo"}),
        ),
        (
            "a1",
            json!({"type": 2, "text": "I will grep first", "timestamp": 1700000002000i64, "modelType": "gpt-x"}),
        ),
        (
            "a2",
            json!({"type": 2, "text": "Reading the file now", "timestamp": 1700000003000i64}),
        ),
        (
            "u2",
            json!({"type": 1, "text": "Summarize", "timestamp": 1700000004000i64}),
        ),
        (
            "a3",
            json!({"type": 2, "text": "Long composer answer with tail detail the transcript truncated", "timestamp": 1700000005000i64, "modelType": "gpt-x"}),
        ),
        (
            "u3",
            json!({"type": 1, "text": "Standalone question never in the transcript", "timestamp": 1700000006000i64}),
        ),
        (
            "u4",
            json!({"type": 1, "text": "the login", "timestamp": 1700000007000i64}),
        ),
    ]
}

fn canonical_records() -> Vec<Value> {
    vec![
        text_record("user", "Fix the login bug"),
        tool_record("assistant", "I will grep first", "Grep", "/src/login.rs"),
        turn_ended("success"),
        text_record(
            "assistant",
            "Reading the file now and the next fifty lines for context",
        ),
        error_turn_ended("Model call failed: rate limited"),
        text_record(
            "user",
            "Please Summarize: we wrapped the composer text inside a quoted context header",
        ),
        text_record("assistant", "Long composer answer with tail"),
        tool_only_record("assistant", "Read", "/src/auth.rs"),
        text_record(
            "user",
            "A final user nudge that exists only in the transcript",
        ),
    ]
}

fn canonical_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(&db, "alpha-1", &canonical_composer(), &canonical_bubbles());
    let transcript = write_transcript(root, "proj-a", "alpha-1", &canonical_records());
    (db, transcript)
}

#[test]
fn merged_conversation_pairs_all_tiers_donates_and_dense_indexes() {
    let tmp = TempDir::new().unwrap();
    let (db, transcript) = canonical_fixture(tmp.path());
    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "composer + same-id transcript must merge into one conversation"
    );
    let c = &convs[0];

    // Conversation-level fields: composer wins, transcript fills gaps.
    assert_eq!(c.agent_slug, "cursor");
    assert_eq!(c.external_id.as_deref(), Some("alpha-1"));
    assert_eq!(c.source_path, db.join("alpha-1"));
    assert_eq!(c.title.as_deref(), Some("Login regression"));
    assert_eq!(c.workspace.as_deref(), Some(Path::new("/work/demo")));
    assert_eq!(c.started_at, Some(1700000000000i64));
    assert!(c.ended_at.unwrap() >= 1700000008000i64);
    assert_eq!(c.metadata["source"], "cursor");
    assert_eq!(c.metadata["model"], "test-model");
    assert_eq!(c.metadata["unifiedMode"], "agentPlus");
    assert_eq!(c.metadata["cursor_format"], "merged");
    assert_eq!(c.metadata["cursor_merge_revision"], 1);
    assert_eq!(
        c.metadata["cursor_transcript_path"],
        json!(transcript.to_str().unwrap())
    );
    assert_eq!(c.metadata["cursor_project_dir"], "proj-a");
    assert_eq!(c.metadata["cursor_workspace_attribution"], "unresolved");

    // Dense idx over the whole merged sequence.
    assert_eq!(c.messages.len(), 9);
    for (i, msg) in c.messages.iter().enumerate() {
        assert_eq!(msg.idx, i as i64, "idx must be dense");
    }

    // idx 0 — t1 pair (u1 / "Fix the login bug"), composer donates timestamp.
    let m = pair_row(c, 0);
    assert_eq!(m.role, "user");
    assert_eq!(m.content, "Fix the login bug");
    assert_eq!(m.created_at, Some(1700000001000i64));
    assert_eq!(m.author, None);
    assert!(m.invocations.is_empty());
    assert_eq!(m.extra["text"], "Fix the login bug");
    assert_eq!(m.extra["type"], json!(1));
    assert_eq!(m.extra["workspaceProjectDir"], "/work/demo");
    assert_eq!(m.extra["cursor_transcript_record"]["role"], "user");

    // idx 1 — t1 pair with tools: transcript flatten wins, composer donates.
    let m = pair_row(c, 1);
    assert_eq!(m.role, "assistant");
    assert_eq!(m.content, "I will grep first\n[Tool: Grep - /src/login.rs]");
    assert_eq!(m.created_at, Some(1700000002000i64));
    assert_eq!(m.author.as_deref(), Some("gpt-x"));
    assert_eq!(m.invocations.len(), 1);
    assert_eq!(m.invocations[0].kind, "tool");
    assert_eq!(m.invocations[0].name, "Grep");
    assert_eq!(m.extra["cursor_transcript_record"]["role"], "assistant");

    // idx 2 — t2 pair: composer text is a prefix of the record text.
    let m = pair_row(c, 2);
    assert_eq!(m.role, "assistant");
    assert_eq!(
        m.content,
        "Reading the file now and the next fifty lines for context"
    );
    assert_eq!(m.created_at, Some(1700000003000i64));
    assert_eq!(m.author, None);

    // idx 3 — error turn_ended record becomes a synthetic assistant row at
    // its record position, extra carries only the namespaced record.
    let m = pair_row(c, 3);
    assert_eq!(m.role, "assistant");
    assert_eq!(m.content, "Model call failed: rate limited");
    assert_eq!(m.created_at, None);
    assert_eq!(m.author, None);
    assert!(m.invocations.is_empty());
    assert_eq!(m.extra.as_object().map(serde_json::Map::len), Some(1));
    assert_eq!(
        m.extra["cursor_transcript_record"]["error"],
        "Model call failed: rate limited"
    );
    assert_eq!(m.extra["cursor_transcript_record"]["type"], "turn_ended");

    // idx 4 — t4 pair: composer text contained in the record text.
    let m = pair_row(c, 4);
    assert_eq!(m.role, "user");
    assert_eq!(
        m.content,
        "Please Summarize: we wrapped the composer text inside a quoted context header"
    );
    assert_eq!(m.created_at, Some(1700000004000i64));

    // idx 5 — t5 pair: record contained in composer, so the composer
    // variant (the only side holding the truncated tail) wins content.
    let m = pair_row(c, 5);
    assert_eq!(m.role, "assistant");
    assert_eq!(
        m.content,
        "Long composer answer with tail detail the transcript truncated"
    );
    assert_eq!(m.created_at, Some(1700000005000i64));
    assert_eq!(m.author.as_deref(), Some("gpt-x"));
    assert!(m.invocations.is_empty());
    assert!(m.extra["cursor_transcript_record"].is_object());

    // idx 6 — unmatched composer message with no role-ok relation inserts
    // after the previous pair, keeping composer content and bubble extra
    // (and no transcript-record key: it is not a pair).
    let m = pair_row(c, 6);
    assert_eq!(m.role, "user");
    assert_eq!(m.content, "Standalone question never in the transcript");
    assert_eq!(m.created_at, Some(1700000006000i64));
    assert!(m.extra.get("cursor_transcript_record").is_none());
    assert_eq!(m.extra["type"], json!(1));

    // idx 7 / 8 — unpaired records stay as today's transcript rows:
    // flatten content, no timestamps, empty extra.
    let m = pair_row(c, 7);
    assert_eq!(m.role, "assistant");
    assert_eq!(m.content, "[Tool: Read - /src/auth.rs]");
    assert_eq!(m.created_at, None);
    assert_eq!(m.author, None);
    assert_eq!(m.extra, json!({}));
    assert_eq!(m.invocations.len(), 1);
    assert_eq!(m.invocations[0].name, "Read");
    let m = pair_row(c, 8);
    assert_eq!(m.role, "user");
    assert_eq!(
        m.content,
        "A final user nudge that exists only in the transcript"
    );
    assert_eq!(m.created_at, None);
    assert_eq!(m.extra, json!({}));

    // The suppressed u4 ("the login", contained in the paired user record)
    // must not appear anywhere.
    assert!(
        c.messages.iter().all(|msg| msg.content != "the login"),
        "composer message with a role-ok relation must be suppressed"
    );
}

#[test]
fn merge_is_deterministic_across_scans() {
    let tmp = TempDir::new().unwrap();
    canonical_fixture(tmp.path());
    let connector = CursorConnector::new();
    let first = connector.scan(&ctx_for(tmp.path())).unwrap();
    let second = connector.scan(&ctx_for(tmp.path())).unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap(),
        "same inputs must merge identically (idx included)"
    );
}

#[test]
fn merged_output_extends_by_append_only_tail() {
    let tmp = TempDir::new().unwrap();
    let (_, transcript) = canonical_fixture(tmp.path());
    let ctx = ctx_for(tmp.path());
    let first = CursorConnector::new().scan(&ctx).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].messages.len(), 9);

    // Incremental stability (§6.5): appending records to the transcript
    // must leave every already-emitted row byte-identical and only append
    // a tail.
    let mut grown = canonical_records();
    grown.push(text_record("user", "One more followup question"));
    grown.push(text_record("assistant", "One more answer line"));
    fs::write(&transcript, jsonl_text(&grown)).unwrap();

    let second = CursorConnector::new().scan(&ctx).unwrap();
    assert_eq!(second.len(), 1);
    let new_messages = &second[0].messages;
    assert_eq!(new_messages.len(), 11);
    for i in 0..9 {
        assert_eq!(
            serde_json::to_value(&new_messages[i]).unwrap(),
            serde_json::to_value(&first[0].messages[i]).unwrap(),
            "row {i} changed after a tail-only append"
        );
    }
    assert_eq!(new_messages[9].role, "user");
    assert_eq!(new_messages[9].content, "One more followup question");
    assert_eq!(new_messages[9].extra, json!({}));
    assert_eq!(new_messages[10].role, "assistant");
    assert_eq!(new_messages[10].content, "One more answer line");
    assert_eq!(new_messages[10].idx, 10);
}

#[test]
fn unmatched_head_composer_message_inserts_before_first_record() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "head-1",
        &json!({"name": "Head anchor", "fullConversationHeadersOnly": [
            {"bubbleId": "ux"}, {"bubbleId": "u1"},
        ]}),
        &[
            (
                "ux",
                json!({"type": 1, "text": "Where do I start", "timestamp": 1700000001000i64}),
            ),
            (
                "u1",
                json!({"type": 1, "text": "Fix the login bug", "timestamp": 1700000002000i64}),
            ),
        ],
    );
    write_transcript(
        root,
        "proj-a",
        "head-1",
        &[text_record("user", "Fix the login bug")],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    // No previous pair: the insert anchors at the head, before the record.
    assert_eq!(c.messages[0].content, "Where do I start");
    assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
    assert!(
        c.messages[0]
            .extra
            .get("cursor_transcript_record")
            .is_none()
    );
    assert_eq!(c.messages[1].content, "Fix the login bug");
    assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
    assert_eq!(
        c.messages[1].extra["cursor_transcript_record"]["role"],
        "user"
    );
}

#[test]
fn unmatched_composer_message_inserts_between_pairs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "mid-1",
        &json!({"name": "Mid anchor", "fullConversationHeadersOnly": [
            {"bubbleId": "u1"}, {"bubbleId": "um"}, {"bubbleId": "u2"},
        ]}),
        &[
            (
                "u1",
                json!({"type": 1, "text": "First question", "timestamp": 1700000001000i64}),
            ),
            (
                "um",
                json!({"type": 1, "text": "An interleaved remark not in the transcript", "timestamp": 1700000002000i64}),
            ),
            (
                "u2",
                json!({"type": 1, "text": "Second question", "timestamp": 1700000003000i64}),
            ),
        ],
    );
    write_transcript(
        root,
        "proj-a",
        "mid-1",
        &[
            text_record("user", "First question"),
            text_record("user", "Second question"),
        ],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[0].content, "First question");
    assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
    // Anchored directly after the record paired with the previous
    // composer message — i.e. between the two pairs.
    assert_eq!(
        c.messages[1].content,
        "An interleaved remark not in the transcript"
    );
    assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
    assert!(
        c.messages[1]
            .extra
            .get("cursor_transcript_record")
            .is_none()
    );
    assert_eq!(c.messages[2].content, "Second question");
    assert_eq!(c.messages[2].created_at, Some(1700000003000i64));
}

#[test]
fn unmatched_tail_composer_message_inserts_at_end() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "tail-1",
        &json!({"name": "Tail anchor", "fullConversationHeadersOnly": [
            {"bubbleId": "u1"}, {"bubbleId": "ut"},
        ]}),
        &[
            (
                "u1",
                json!({"type": 1, "text": "First question", "timestamp": 1700000001000i64}),
            ),
            (
                "ut",
                json!({"type": 1, "text": "A trailing remark absent from the transcript", "timestamp": 1700000002000i64}),
            ),
        ],
    );
    write_transcript(
        root,
        "proj-a",
        "tail-1",
        &[text_record("user", "First question")],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "First question");
    assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
    // Anchor past the last record: the insert lands after the walk.
    assert_eq!(
        c.messages[1].content,
        "A trailing remark absent from the transcript"
    );
    assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
    assert!(
        c.messages[1]
            .extra
            .get("cursor_transcript_record")
            .is_none()
    );
}

#[test]
fn out_of_order_duplicate_is_suppressed_not_inserted() {
    // u_bg's text equals record r0's text (t1), but r0 sits before the
    // forward cursor once r1 pairs u_q, so u_bg stays unmatched — and the
    // literal §6.2 suppress rule (relation with ANY role-ok record,
    // paired or not) drops it instead of inserting. r0 itself must never
    // be dropped: it survives as a transcript-only row.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "sup-1",
        &json!({"name": "Out of order", "fullConversationHeadersOnly": [
            {"bubbleId": "uq"}, {"bubbleId": "ubg"},
        ]}),
        &[
            (
                "uq",
                json!({"type": 1, "text": "the main question stands", "timestamp": 1700000001000i64}),
            ),
            (
                "ubg",
                json!({"type": 1, "text": "alpha beta background", "timestamp": 1700000002000i64}),
            ),
        ],
    );
    write_transcript(
        root,
        "proj-a",
        "sup-1",
        &[
            text_record("user", "alpha beta background"),
            text_record("user", "the main question stands"),
        ],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(
        c.messages.len(),
        2,
        "no third row for the suppressed composer message"
    );
    // r0 survives as a transcript-only row.
    assert_eq!(c.messages[0].content, "alpha beta background");
    assert_eq!(c.messages[0].extra, json!({}));
    assert_eq!(c.messages[0].created_at, None);
    // r1 pairs u_q; the composer side donates its timestamp.
    assert_eq!(c.messages[1].content, "the main question stands");
    assert_eq!(c.messages[1].created_at, Some(1700000001000i64));
    assert_eq!(
        c.messages[1].extra["cursor_transcript_record"]["role"],
        "user"
    );
}

#[test]
fn error_edge_records_stay_dropped_in_merged_conversations() {
    // Only a turn_ended record with a non-blank string `error` becomes a
    // synthetic row; blank and non-string errors stay dropped, and
    // success turn_ended records never yield rows.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "err-1",
        &json!({"name": "Error edges", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
        &[(
            "u1",
            json!({"type": 1, "text": "Question one", "timestamp": 1700000001000i64}),
        )],
    );
    write_transcript(
        root,
        "proj-a",
        "err-1",
        &[
            text_record("user", "Question one"),
            json!({"type": "turn_ended", "status": "error", "error": "   "}),
            json!({"type": "turn_ended", "status": "error", "error": 42}),
            turn_ended("success"),
        ],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.messages.len(), 1);
    assert_eq!(c.messages[0].content, "Question one");
    assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
}

#[test]
fn multi_file_transcript_sid_merges_once_with_all_files() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "eps-7",
        &json!({
            "name": "Epsilon session",
            "createdAt": 1700000000000i64,
            "fullConversationHeadersOnly": [{"bubbleId": "u1"}, {"bubbleId": "u2"}],
        }),
        &[
            (
                "u1",
                json!({"type": 1, "text": "Question alpha", "timestamp": 1700000001000i64}),
            ),
            (
                "u2",
                json!({"type": 1, "text": "Question beta", "timestamp": 1700000002000i64}),
            ),
        ],
    );
    let file_x = write_transcript(
        root,
        "proj-x",
        "eps-7",
        &[text_record("user", "Question alpha")],
    );
    let file_y = write_transcript(
        root,
        "proj-y",
        "eps-7",
        &[text_record("user", "Question beta")],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "all files of one sid merge into a single conversation"
    );
    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "Question alpha");
    assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
    assert_eq!(c.messages[1].content, "Question beta");
    assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
    assert_eq!(
        c.metadata["cursor_transcript_path"],
        json!([file_x.to_str().unwrap(), file_y.to_str().unwrap()])
    );
    assert_eq!(c.metadata["cursor_project_dir"], "proj-x");
}

// ------------------------------------------------------------------
// Freshness gate (§6.4)
// ------------------------------------------------------------------

#[test]
fn stale_db_with_fresh_transcript_merges_via_targeted_lookup() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Stale db first, then a clear sleep, then the transcripts: the db
    // stays stale while both transcripts come back fresh, deterministically.
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(&db, "alpha-1", &canonical_composer(), &canonical_bubbles());
    std::thread::sleep(std::time::Duration::from_millis(120));
    write_transcript(root, "proj-a", "alpha-1", &canonical_records());
    // A second, transcript-only session that is also fresh.
    write_transcript(
        root,
        "proj-b",
        "beta-8",
        &[text_record("user", "Beta eight question")],
    );
    let since = mtime_ms(&db) + 1001;
    let convs = CursorConnector::new()
        .scan(&ctx_since(root, since))
        .unwrap();
    assert_eq!(
        convs.len(),
        2,
        "fresh transcript merges with the stale composer row; other fresh sid stays separate"
    );
    let merged = &convs[0];
    assert_eq!(merged.external_id.as_deref(), Some("alpha-1"));
    assert_eq!(merged.metadata["cursor_format"], "merged");
    // Composer identity and donations prove the stale db was read.
    assert_eq!(merged.source_path, db.join("alpha-1"));
    assert_eq!(merged.title.as_deref(), Some("Login regression"));
    assert_eq!(merged.messages[0].created_at, Some(1700000001000i64));
    assert_eq!(merged.messages[5].author.as_deref(), Some("gpt-x"));
    let other = &convs[1];
    assert_eq!(other.external_id.as_deref(), Some("beta-8"));
    assert_eq!(other.metadata["cursor_format"], "agent");
    assert!(other.messages[0].created_at.is_none());
}

#[test]
fn fresh_db_with_stale_transcript_merges_and_skips_stale_only_sids() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Transcripts first (both will be stale), then the db (fresh): the db
    // must be written after a sleep so its mtime is clearly newer.
    write_transcript(
        root,
        "proj-c",
        "beta-7",
        &[text_record("user", "Beta seven question")],
    );
    let alpha_transcript = write_transcript(root, "proj-a", "alpha-1", &canonical_records());
    std::thread::sleep(std::time::Duration::from_millis(120));
    let db = root.join("globalStorage/state.vscdb");
    // The conversation itself must pass the incremental cutoff: give it a
    // lastUpdatedAt newer than the (stale) transcripts.
    let mut composer = canonical_composer();
    composer["lastUpdatedAt"] = json!(mtime_ms(&alpha_transcript) + 2000);
    write_composer_session(&db, "alpha-1", &composer, &canonical_bubbles());
    let since = mtime_ms(&alpha_transcript) + 1001;
    let convs = CursorConnector::new()
        .scan(&ctx_since(root, since))
        .unwrap();
    assert_eq!(
        convs.len(),
        1,
        "fresh db parses stale transcripts for its sids; stale transcript-only sids stay silent"
    );
    let merged = &convs[0];
    assert_eq!(merged.external_id.as_deref(), Some("alpha-1"));
    assert_eq!(merged.metadata["cursor_format"], "merged");
    // Transcript content made it in despite the stale mtime.
    assert_eq!(
        merged.messages[1].content,
        "I will grep first\n[Tool: Grep - /src/login.rs]"
    );
    assert_eq!(merged.messages[0].created_at, Some(1700000001000i64));
}

#[test]
fn message_less_composer_row_does_not_block_a_later_db_in_targeted_lookup() {
    // Cross-store resolution mirrors the full scan: a store whose row
    // parses to no conversation does not block a later store from
    // resolving the sid.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let global_db = root.join("globalStorage/state.vscdb");
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    {
        let conn = create_store(&global_db);
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:delta-4", r#"{"name": "Empty shell"}"#],
        )
        .unwrap();
        drop(conn);
    }
    let w2_db = root.join("workspaceStorage/w2/state.vscdb");
    write_composer_session(
        &w2_db,
        "delta-4",
        &json!({"name": "Delta four", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
        &[(
            "u1",
            json!({"type": 1, "text": "First pairing question", "timestamp": 1700000001000i64}),
        )],
    );
    std::thread::sleep(std::time::Duration::from_millis(120));
    write_transcript(
        root,
        "proj-d",
        "delta-4",
        &[text_record("user", "First pairing question")],
    );
    let since = mtime_ms(&global_db).max(mtime_ms(&w2_db)) + 1001;
    let convs = CursorConnector::new()
        .scan(&ctx_since(root, since))
        .unwrap();
    assert_eq!(convs.len(), 1);
    let merged = &convs[0];
    assert_eq!(merged.metadata["cursor_format"], "merged");
    assert_eq!(merged.source_path, w2_db.join("delta-4"));
    assert_eq!(merged.messages[0].created_at, Some(1700000001000i64));
}

#[test]
fn unopenable_stale_db_fails_scan_during_targeted_lookup() {
    // F9 semantics for the lookup too: a store that cannot be read fails
    // the scan instead of silently degrading to a transcript-only row —
    // the same store would fail a full scan.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let global_db = root.join("globalStorage/state.vscdb");
    fs::create_dir_all(global_db.parent().unwrap()).unwrap();
    fs::write(&global_db, b"this is not a sqlite database at all").unwrap();
    let w2_db = root.join("workspaceStorage/w2/state.vscdb");
    write_composer_session(
        &w2_db,
        "delta-5",
        &json!({"name": "Delta five", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
        &[(
            "u1",
            json!({"type": 1, "text": "Second pairing question", "timestamp": 1700000001000i64}),
        )],
    );
    std::thread::sleep(std::time::Duration::from_millis(120));
    write_transcript(
        root,
        "proj-d",
        "delta-5",
        &[text_record("user", "Second pairing question")],
    );
    let since = mtime_ms(&global_db).max(mtime_ms(&w2_db)) + 1001;
    let err = CursorConnector::new()
        .scan(&ctx_since(root, since))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("Cursor scan incomplete"),
        "lookup failure must fail the scan, got: {err:#}"
    );
}

// ------------------------------------------------------------------
// Unchanged shapes (regression pins)
// ------------------------------------------------------------------

#[test]
fn transcript_only_conversations_keep_todays_shape() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let beta2 = write_transcript(
        root,
        "proj-t",
        "beta-2",
        &[
            text_record("user", "Beta only question"),
            error_turn_ended("Beta model failed"),
        ],
    );
    write_transcript(
        root,
        "proj-p",
        "beta-3",
        &[text_record("user", "Beta three left")],
    );
    write_transcript(
        root,
        "proj-q",
        "beta-3",
        &[text_record("user", "Beta three right")],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 3);
    // Error records never yield rows on the transcript-only path.
    let b2 = convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some("beta-2"))
        .unwrap();
    assert_eq!(b2.source_path, beta2);
    assert_eq!(b2.metadata["cursor_format"], "agent");
    assert_eq!(b2.metadata["source"], "cursor");
    assert_eq!(b2.messages.len(), 1);
    assert_eq!(b2.messages[0].content, "Beta only question");
    assert_eq!(b2.messages[0].extra, json!({}));
    assert!(b2.messages[0].created_at.is_none());
    // Same sid in two projects stays two conversations.
    let b3s: Vec<_> = convs
        .iter()
        .filter(|c| c.external_id.as_deref() == Some("beta-3"))
        .collect();
    assert_eq!(b3s.len(), 2);
    assert_eq!(b3s[0].metadata["cursor_format"], "agent");
    assert_eq!(b3s[1].metadata["cursor_format"], "agent");
}

#[test]
fn composer_only_conversations_keep_todays_shape() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let db = root.join("globalStorage/state.vscdb");
    write_composer_session(
        &db,
        "gamma-3",
        &json!({"name": "Gamma only", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
        &[(
            "u1",
            json!({"type": 1, "text": "Composer only question", "timestamp": 1700000001000i64}),
        )],
    );
    let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.external_id.as_deref(), Some("gamma-3"));
    assert_eq!(c.source_path, db.join("gamma-3"));
    assert!(
        c.metadata.get("cursor_format").is_none(),
        "no merge markers without a transcript"
    );
    assert!(c.metadata.get("cursor_merge_revision").is_none());
    assert!(c.metadata.get("cursor_transcript_path").is_none());
    assert_eq!(c.messages.len(), 1);
    assert_eq!(c.messages[0].content, "Composer only question");
    assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
    assert!(
        c.messages[0]
            .extra
            .get("cursor_transcript_record")
            .is_none()
    );
}

#[test]
fn all_stale_sources_emit_nothing() {
    let tmp = TempDir::new().unwrap();
    canonical_fixture(tmp.path());
    let convs = CursorConnector::new()
        .scan(&ctx_since(tmp.path(), i64::MAX))
        .unwrap();
    assert!(convs.is_empty());
}

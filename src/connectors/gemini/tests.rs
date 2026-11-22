use super::*;
use crate::types::NormalizedMessage;
use serial_test::serial;
use tempfile::TempDir;

struct FailingReader(io::ErrorKind);

impl io::Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(self.0, "simulated Gemini source failure"))
    }
}

#[test]
fn jsonl_reader_failure_discards_an_otherwise_complete_history() {
    use std::io::{BufReader, Cursor, Read};

    let path = Path::new("session-read-failure.jsonl");
    let prefix = concat!(
        r#"{"sessionId":"session","projectHash":"project"}"#,
        "\n",
        r#"{"id":"reply","type":"gemini","content":"valid prefix"}"#,
        "\n",
    );
    let complete = GeminiConnector::replay_jsonl_session(Cursor::new(prefix), path).unwrap();
    assert_eq!(complete["messages"].as_array().unwrap().len(), 1);
    for kind in [
        io::ErrorKind::Other,
        io::ErrorKind::InvalidData,
        io::ErrorKind::Interrupted,
    ] {
        let reader = BufReader::new(prefix.as_bytes().chain(FailingReader(kind)));
        let error = GeminiConnector::replay_jsonl_session(reader, path).unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), "simulated Gemini source failure");
    }
}

#[test]
fn jsonl_preserves_distinct_idless_historical_messages() {
    let input = concat!(
        r#"{"kind":"main","sessionId":"session","projectHash":"project"}"#,
        "\n",
        r#"{"type":"user","content":"same text"}"#,
        "\n",
        r#"{"$set":{"messages":[{"type":"user","content":"same text"},{"type":"user","content":"same text"}]}}"#,
        "\n",
        r#"{"id":"reply","type":"gemini","content":"draft"}"#,
        "\n",
        r#"{"type":"user","content":"same text"}"#,
        "\n",
        r#"{"id":"reply","type":"gemini","content":"final"}"#,
    );
    let replay = GeminiConnector::replay_jsonl_session(
        std::io::Cursor::new(input),
        Path::new("session-legacy.jsonl"),
    )
    .unwrap();
    let messages = replay["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["content"], "same text");
    assert_eq!(messages[1]["content"], "same text");
    assert_eq!(messages[2]["id"], "reply");
    assert_eq!(messages[2]["content"], "final");
    assert_eq!(messages[3]["content"], "same text");
}

// ==================== Constructor Tests ====================

#[test]
fn new_creates_connector() {
    let connector = GeminiConnector::new();
    let _ = connector;
}

#[test]
fn default_creates_connector() {
    let connector = GeminiConnector;
    let _ = connector;
}

// ==================== extract_path_from_position Tests ====================

#[test]
fn extract_path_handles_unix_absolute() {
    let content = "Working directory: /home/user/project";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, Some(PathBuf::from("/home/user/project")));
}

#[test]
fn extract_path_handles_trailing_slash() {
    let content = "Working directory: /data/projects/foo/";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, Some(PathBuf::from("/data/projects/foo")));
}

#[test]
fn extract_path_handles_quoted_path_start() {
    let content = "Working directory: \"/data/projects/foo\"";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, Some(PathBuf::from("/data/projects/foo")));
}

#[test]
fn extract_path_handles_quoted_path_with_brackets() {
    let content = "Working directory: \"/data/projects/[foo]\"";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, Some(PathBuf::from("/data/projects/[foo]")));
}

#[test]
fn extract_path_handles_windows_drive() {
    let content = r"Working directory: C:\Users\User\Project";
    let path = extract_path_from_position(content, 19);
    // Windows paths are normalized to forward slashes for cross-platform robustness
    assert_eq!(path, Some(PathBuf::from("C:/Users/User/Project")));
}

#[test]
fn extract_path_handles_windows_forward_slashes() {
    let content = "Working directory: D:/Code/Rust";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, Some(PathBuf::from("D:/Code/Rust")));
}

#[test]
fn extract_path_handles_windows_unc() {
    let content = r"Working directory: \\Server\Share\Project";
    let path = extract_path_from_position(content, 19);
    // UNC paths are normalized to forward slashes for cross-platform robustness
    assert_eq!(path, Some(PathBuf::from("//Server/Share/Project")));
}

#[test]
fn extract_path_rejects_relative_paths() {
    let content = "Working directory: relative/path";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, None);
}

#[test]
fn extract_path_rejects_too_short_paths() {
    let content = "Working directory: /a";
    let path = extract_path_from_position(content, 19);
    assert_eq!(path, None);
}

#[test]
fn extract_path_stops_at_whitespace() {
    let content = "Path: /home/user/project more text";
    let path = extract_path_from_position(content, 6);
    assert_eq!(path, Some(PathBuf::from("/home/user/project")));
}

#[test]
fn extract_path_stops_at_newline() {
    let content = "Path: /home/user/project\nmore text";
    let path = extract_path_from_position(content, 6);
    assert_eq!(path, Some(PathBuf::from("/home/user/project")));
}

#[test]
fn extract_path_stops_at_quote() {
    let content = "Path: \"/home/user/project\" more";
    let path = extract_path_from_position(content, 7);
    assert_eq!(path, Some(PathBuf::from("/home/user/project")));
}

#[test]
fn extract_path_truncates_data_projects_paths() {
    let content = "File at /data/projects/myproject/src/lib.rs";
    let path = extract_path_from_position(content, 8);
    assert_eq!(path, Some(PathBuf::from("/data/projects/myproject")));
}

#[test]
fn extract_path_removes_file_extension_for_parent() {
    let content = "Editing /home/user/project/src/main.rs";
    let path = extract_path_from_position(content, 8);
    assert_eq!(path, Some(PathBuf::from("/home/user/project/src")));
}

// ==================== extract_workspace_from_content Tests ====================

#[test]
fn extract_workspace_from_agents_md_pattern() {
    let messages = vec![NormalizedMessage {
        idx: 0,
        role: "user".into(),
        author: None,
        created_at: None,
        content: "# AGENTS.md instructions for /data/projects/myapp\nHello".into(),
        extra: serde_json::Value::Null,
        snippets: vec![],
        invocations: vec![],
    }];
    let result = extract_workspace_from_content(&messages);
    assert_eq!(result, Some(PathBuf::from("/data/projects/myapp")));
}

#[test]
fn extract_workspace_from_working_directory_pattern() {
    let messages = vec![NormalizedMessage {
        idx: 0,
        role: "assistant".into(),
        author: None,
        created_at: None,
        content: "Working directory: /home/user/project\nLet me help.".into(),
        extra: serde_json::Value::Null,
        snippets: vec![],
        invocations: vec![],
    }];
    let result = extract_workspace_from_content(&messages);
    assert_eq!(result, Some(PathBuf::from("/home/user/project")));
}

#[test]
fn extract_workspace_fallback_to_data_projects() {
    let messages = vec![NormalizedMessage {
        idx: 0,
        role: "user".into(),
        author: None,
        created_at: None,
        content: "Check the file at /data/projects/foo/src/main.rs".into(),
        extra: serde_json::Value::Null,
        snippets: vec![],
        invocations: vec![],
    }];
    let result = extract_workspace_from_content(&messages);
    assert_eq!(result, Some(PathBuf::from("/data/projects/foo")));
}

#[test]
fn extract_workspace_returns_none_for_no_paths() {
    let messages = vec![NormalizedMessage {
        idx: 0,
        role: "user".into(),
        author: None,
        created_at: None,
        content: "Hello, how are you?".into(),
        extra: serde_json::Value::Null,
        snippets: vec![],
        invocations: vec![],
    }];
    let result = extract_workspace_from_content(&messages);
    assert_eq!(result, None);
}

#[test]
fn extract_workspace_prefers_agents_md_over_working_directory() {
    let messages = vec![NormalizedMessage {
        idx: 0,
        role: "user".into(),
        author: None,
        created_at: None,
        content: "Working directory: /tmp/wrong\n# AGENTS.md instructions for /data/projects/right"
            .into(),
        extra: serde_json::Value::Null,
        snippets: vec![],
        invocations: vec![],
    }];
    // AGENTS.md pattern should be found first
    let result = extract_workspace_from_content(&messages);
    assert_eq!(result, Some(PathBuf::from("/data/projects/right")));
}

// ==================== session_files Tests ====================

#[test]
fn session_files_finds_legacy_json_and_current_jsonl_in_chats() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("abcd1234");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    fs::write(chats_dir.join("session-1.json"), "{}").unwrap();
    fs::write(chats_dir.join("session-2.json"), "{}").unwrap();
    fs::write(chats_dir.join("session-3.JSONL"), "{}").unwrap();

    let files = GeminiConnector::session_files(dir.path());
    assert_eq!(files.len(), 3);
}

#[test]
fn session_files_ignores_non_session_files() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("abcd1234");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    fs::write(chats_dir.join("session-1.json"), "{}").unwrap();
    fs::write(chats_dir.join("other.json"), "{}").unwrap();
    fs::write(chats_dir.join("readme.txt"), "hello").unwrap();

    let files = GeminiConnector::session_files(dir.path());
    assert_eq!(files.len(), 1);
}

#[test]
fn session_files_ignores_session_json_outside_chats() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("abcd1234");
    fs::create_dir_all(&hash_dir).unwrap();
    // session-*.json not in chats/ directory
    fs::write(hash_dir.join("session-1.json"), "{}").unwrap();

    let files = GeminiConnector::session_files(dir.path());
    assert!(files.is_empty());
}

#[test]
fn session_files_finds_multiple_hash_directories() {
    let dir = TempDir::new().unwrap();

    let hash1_chats = dir.path().join("hash1").join("chats");
    fs::create_dir_all(&hash1_chats).unwrap();
    fs::write(hash1_chats.join("session-a.json"), "{}").unwrap();

    let hash2_chats = dir.path().join("hash2").join("chats");
    fs::create_dir_all(&hash2_chats).unwrap();
    fs::write(hash2_chats.join("session-b.json"), "{}").unwrap();

    let files = GeminiConnector::session_files(dir.path());
    assert_eq!(files.len(), 2);
}

#[test]
fn session_files_returns_sorted_order() {
    let dir = TempDir::new().unwrap();
    let chats_dir = dir.path().join("hash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    fs::write(chats_dir.join("session-z.json"), "{}").unwrap();
    fs::write(chats_dir.join("session-a.json"), "{}").unwrap();

    let files = GeminiConnector::session_files(dir.path());
    assert_eq!(files.len(), 2);

    let names: Vec<_> = files
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(names, vec!["session-a.json", "session-z.json"]);
}

#[test]
fn session_files_returns_empty_for_empty_dir() {
    let dir = TempDir::new().unwrap();
    let files = GeminiConnector::session_files(dir.path());
    assert!(files.is_empty());
}

#[test]
fn session_files_returns_empty_for_nonexistent_dir() {
    let files = GeminiConnector::session_files(Path::new("/nonexistent/path/xyz"));
    assert!(files.is_empty());
}

// ==================== scan Tests ====================

#[test]
fn scan_parses_gemini_session() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_test_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "test-session-123",
        "projectHash": "abc123",
        "startTime": "2025-01-15T10:00:00Z",
        "lastUpdated": "2025-01-15T10:05:00Z",
        "messages": [
            {
                "type": "user",
                "content": "Hello Gemini!",
                "timestamp": "2025-01-15T10:00:00Z"
            },
            {
                "type": "model",
                "content": "Hello! How can I help?",
                "timestamp": "2025-01-15T10:00:05Z"
            }
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "gemini");
    assert_eq!(conv.external_id.as_deref(), Some("test-session-123"));
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[0].content, "Hello Gemini!");
    assert_eq!(conv.messages[1].role, "assistant"); // model -> assistant
    assert_eq!(conv.messages[1].content, "Hello! How can I help?");
}

#[test]
fn scan_replays_event_sourced_jsonl_with_authoritative_reseeds() {
    let dir = TempDir::new().unwrap();
    let chats_dir = dir.path().join("gemini_project").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_jsonl = concat!(
        r#"{"kind":"main","sessionId":"jsonl-session","projectHash":"project-123","startTime":"2026-07-09T04:00:00Z","lastUpdated":"2026-07-09T04:00:01Z"}"#,
        "\n",
        r#"{"id":"discarded-before-seed","type":"user","timestamp":"2026-07-09T04:00:02Z","content":"superseded prefix"}"#,
        "\n",
        r#"{"$set":{"lastUpdated":"2026-07-09T04:00:03Z","messages":[{"id":"u1","type":"user","timestamp":"2026-07-09T04:00:00Z","content":[{"text":"authoritative question"}]},{"id":"a1","type":"gemini","timestamp":"2026-07-09T04:00:01Z","content":"first answer"}]}}"#,
        "\n",
        r#"{"id":"discarded-by-reseed","type":"user","timestamp":"2026-07-09T04:00:04Z","content":"intermediate append"}"#,
        "\n",
        r#"{"$set":{"lastUpdated":"2026-07-09T04:00:05Z","messages":[{"id":"u1","type":"user","timestamp":"2026-07-09T04:00:00Z","content":[{"text":"authoritative question"}]},{"id":"a1","type":"gemini","timestamp":"2026-07-09T04:00:01Z","content":"compacted answer"}]}}"#,
        "\n",
        r#"{"id":"i1","type":"info","timestamp":"2026-07-09T04:00:06Z","content":"model switched"}"#,
        "\n",
        r#"{"id":"a2","type":"gemini","timestamp":"2026-07-09T04:00:07Z","content":"answer after compaction"}"#,
        "\n",
        r#"{"id":"e1","type":"error","timestamp":"2026-07-09T04:00:08Z","content":"recoverable notice"}"#,
        "\n",
        r#"{"id":"truncated","type":"gemini","content":"unfinished"#,
    );
    let session_path = chats_dir.join("session-2026-07-09T04-00-jsonl.jsonl");
    fs::write(&session_path, session_jsonl).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let conversations = connector.scan(&ctx).unwrap();

    assert_eq!(conversations.len(), 1);
    let conversation = &conversations[0];
    assert_eq!(conversation.external_id.as_deref(), Some("jsonl-session"));
    assert_eq!(conversation.metadata["project_hash"], "project-123");
    assert_eq!(conversation.source_path, session_path);
    assert_eq!(conversation.messages.len(), 5);
    assert_eq!(conversation.messages[0].content, "authoritative question");
    assert_eq!(conversation.messages[1].content, "compacted answer");
    assert_eq!(conversation.messages[2].content, "model switched");
    assert_eq!(conversation.messages[3].content, "answer after compaction");
    assert_eq!(conversation.messages[4].content, "recoverable notice");
    assert_eq!(
        conversation
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "system", "assistant", "system"]
    );
    assert!(
        conversation
            .messages
            .iter()
            .all(|message| !message.content.contains("superseded"))
    );
    assert!(
        conversation
            .messages
            .iter()
            .all(|message| !message.content.contains("intermediate"))
    );
    assert_eq!(
        conversation
            .messages
            .iter()
            .map(|message| message.idx)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert!(conversation.started_at.is_some());
    assert!(conversation.ended_at > conversation.started_at);
    crate::connectors::assert_discovery_covers_scan_sources(&connector, &ctx);
}

#[test]
fn scan_with_callback_matches_scan_for_gemini_session() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "stream-test",
        "projectHash": "hash-123",
        "messages": [
            {
                "type": "user",
                "content": "Need help with /data/projects/demo",
                "timestamp": "2025-01-15T10:00:00Z"
            },
            {
                "type": "model",
                "content": "Working directory: /data/projects/demo",
                "timestamp": "2025-01-15T10:00:05Z"
            }
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let scanned = connector.scan(&ctx).unwrap();
    let mut streamed = Vec::new();
    connector
        .scan_with_callback(&ctx, &mut |conversation| {
            streamed.push(conversation);
            Ok(())
        })
        .unwrap();

    assert!(connector.supports_streaming_scan());
    assert_eq!(streamed.len(), scanned.len());
    assert_eq!(streamed[0].external_id, scanned[0].external_id);
    assert_eq!(streamed[0].workspace, scanned[0].workspace);
    assert_eq!(streamed[0].messages.len(), scanned[0].messages.len());
    assert_eq!(
        streamed[0].messages[0].content,
        scanned[0].messages[0].content
    );
    assert_eq!(
        streamed[0].messages[1].content,
        scanned[0].messages[1].content
    );
}

#[test]
fn scan_accepts_explicit_session_file_root() {
    let tmp = TempDir::new().unwrap();
    let chats_dir = tmp.path().join("hash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    let session_path = chats_dir.join("session-1.json");
    let session_json = r#"{
        "sessionId": "session-1",
        "messages": [
            {"type": "user", "content": "Hello Gemini!", "timestamp": "2026-01-15T08:41:26.974Z"},
            {"type": "model", "content": "Hello! How can I help?", "timestamp": "2026-01-15T08:41:30.000Z"}
        ]
    }"#;
    fs::write(&session_path, session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::with_roots(
        session_path.clone(),
        vec![crate::connectors::scan::ScanRoot::local(
            session_path.clone(),
        )],
        None,
    );

    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, session_path);
    assert_eq!(convs[0].messages.len(), 2);
}

#[test]
fn scan_with_gemini_root_scan_root() {
    let tmp = TempDir::new().unwrap();
    let gemini_root = tmp.path().join(".gemini");
    let chats_dir = gemini_root.join("tmp").join("hash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "session-root",
        "messages": [
            {"type": "user", "content": "Hello Gemini!", "timestamp": "2026-01-15T08:41:26.974Z"},
            {"type": "model", "content": "Hello! How can I help?", "timestamp": "2026-01-15T08:41:30.000Z"}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::with_roots(
        PathBuf::new(),
        vec![crate::connectors::scan::ScanRoot::local(
            gemini_root.clone(),
        )],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some("session-root"));
}

#[test]
fn scan_normalizes_model_role_to_assistant() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "session-1",
        "messages": [
            {"type": "model", "content": "I am the model"}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].role, "assistant");
}

#[test]
fn scan_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "session-1",
        "messages": [
            {"type": "user", "content": "First"},
            {"type": "model", "content": "Second"},
            {"type": "user", "content": "Third"}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].idx, 0);
    assert_eq!(convs[0].messages[1].idx, 1);
    assert_eq!(convs[0].messages[2].idx, 2);
}

#[test]
fn scan_extracts_title_from_first_user_message() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Title is extracted from first line of first user message
    let session_json = r#"{
        "sessionId": "session-1",
        "messages": [
            {"type": "model", "content": "Hello!"},
            {"type": "user", "content": "Help me with Rust"}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title.as_deref(), Some("Help me with Rust"));
}

#[test]
fn scan_title_skips_injected_context_user_records() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r##"{
        "sessionId": "session-ctx",
        "messages": [
            {"type": "user", "content": "# AGENTS.md instructions for /data/projects/demo\nBe helpful."},
            {"type": "user", "content": "Help me with Rust"},
            {"type": "model", "content": "Sure!"}
        ]
    }"##;
    fs::write(chats_dir.join("session-ctx.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title.as_deref(), Some("Help me with Rust"));
}

#[test]
fn scan_truncates_long_titles() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let long_content = "x".repeat(200);
    let session_json = format!(
        r#"{{"sessionId": "session-1", "messages": [{{"type": "user", "content": "{}"}}]}}"#,
        long_content
    );
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title.as_ref().map(String::len), Some(100));
}

#[test]
fn scan_sets_source_path() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "Hi"}]}"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].source_path.ends_with("session-1.json"));
    crate::connectors::assert_discovery_covers_scan_sources(&connector, &ctx);
}

#[test]
fn scan_extracts_project_hash_to_metadata() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "session-1",
        "projectHash": "myproject123",
        "messages": [{"type": "user", "content": "Hi"}]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(
        convs[0]
            .metadata
            .get("project_hash")
            .and_then(|v| v.as_str()),
        Some("myproject123")
    );
}

#[test]
fn scan_handles_empty_messages_array() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{"sessionId": "session-1", "messages": []}"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Empty messages should not create a conversation
    assert!(convs.is_empty());
}

#[test]
fn scan_skips_empty_content_messages() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "session-1",
        "messages": [
            {"type": "user", "content": "Hello"},
            {"type": "model", "content": "   "},
            {"type": "user", "content": ""}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Only the non-empty message should be included
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Hello");
}

#[test]
fn scan_handles_invalid_json() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    fs::write(chats_dir.join("session-1.json"), "not valid json").unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_handles_missing_messages_field() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{"sessionId": "session-1"}"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    // No messages field means skip this session
    assert!(convs.is_empty());
}

#[test]
fn scan_handles_empty_directory() {
    let dir = TempDir::new().unwrap();
    // Create a gemini-named subdir
    let gemini_dir = dir.path().join("gemini_test");
    fs::create_dir_all(&gemini_dir).unwrap();

    let connector = GeminiConnector::new();
    // Use explicit root to avoid fallback to real home
    let ctx = ScanContext::with_roots(
        gemini_dir.clone(),
        vec![crate::connectors::ScanRoot::local(gemini_dir)],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_uses_fallback_external_id_from_filename() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // No sessionId field
    let session_json = r#"{"messages": [{"type": "user", "content": "Hi"}]}"#;
    fs::write(chats_dir.join("session-fallback.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].external_id.as_deref(), Some("session-fallback"));
}

#[test]
fn scan_extracts_workspace_from_message_content() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Pattern: AGENTS.md instructions for /path
    let session_json = r##"{
        "sessionId": "session-1",
        "messages": [
            {"type": "user", "content": "# AGENTS.md instructions for /data/projects/myapp Hello there"}
        ]
    }"##;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(
        convs[0].workspace,
        Some(PathBuf::from("/data/projects/myapp"))
    );
}

#[test]
fn scan_falls_back_to_parent_dir_for_workspace() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("project_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // No workspace info in content
    let session_json = r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "Hi"}]}"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Falls back to hash directory
    assert!(
        convs[0]
            .workspace
            .as_ref()
            .unwrap()
            .ends_with("project_hash")
    );
}

#[test]
fn scan_parses_timestamps() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "session-1",
        "startTime": "2025-01-15T10:00:00Z",
        "lastUpdated": "2025-01-15T11:00:00Z",
        "messages": [
            {"type": "user", "content": "Hello", "timestamp": "2025-01-15T10:00:00Z"}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
    assert!(convs[0].messages[0].created_at.is_some());
}

#[test]
fn scan_handles_array_content() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Content as array of parts (Gemini multi-part format)
    let session_json = r#"{
        "sessionId": "session-1",
        "messages": [
            {"type": "user", "content": [{"text": "Hello "}, {"text": "World"}]}
        ]
    }"#;
    fs::write(chats_dir.join("session-1.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    // flatten_content should concatenate text parts
    assert!(convs[0].messages[0].content.contains("Hello"));
}

#[test]
fn scan_finds_multiple_sessions() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    fs::write(
        chats_dir.join("session-1.json"),
        r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "Hi 1"}]}"#,
    )
    .unwrap();
    fs::write(
        chats_dir.join("session-2.json"),
        r#"{"sessionId": "s2", "messages": [{"type": "user", "content": "Hi 2"}]}"#,
    )
    .unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
}

// Test disabled: requires `unsafe { std::env::set_var(...) }` which is
// incompatible with the crate-level `#![forbid(unsafe_code)]` lint.
// The logic is validated in the cass integration test suite instead.
#[test]
#[serial]
fn scan_respects_gemini_home_even_if_data_dir_contains_gemini_string() {
    // This test would need to mutate HOME env var which requires unsafe
    // in Rust 2024+. The behavior is validated by the cass e2e test suite.
    // Placeholder to preserve the test structure.
}

// ==================== detect Tests ====================

#[test]
fn detect_returns_not_found_for_missing_directory() {
    // Since detect uses the real root, we can only test the behavior indirectly
    // This test documents the expected behavior
    let connector = GeminiConnector::new();
    let result = connector.detect();
    // Result depends on whether ~/.gemini/tmp exists on the system
    // We just verify it returns a valid result
    let _ = result.detected;
}

// ==================== Edge case tests — malformed input robustness (br-3n1q) ====================

#[test]
fn empty_file_returns_no_conversations() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    fs::write(chats_dir.join("session-empty.json"), b"").unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "empty file should not cause errors");
    assert!(result.unwrap().is_empty());
}

#[test]
fn whitespace_only_file_returns_no_conversations() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    fs::write(chats_dir.join("session-ws.json"), "  \n\n  \t\n").unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "whitespace-only file should not cause errors"
    );
    assert!(result.unwrap().is_empty());
}

#[test]
fn truncated_json_file_handled() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // File truncated mid-JSON
    fs::write(
        chats_dir.join("session-trunc.json"),
        r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "He"#,
    )
    .unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "truncated JSON should not cause errors");
    assert!(result.unwrap().is_empty());
}

#[test]
fn invalid_utf8_file_skipped() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Write invalid UTF-8 bytes
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"{\"sessionId\": \"s1\", ");
    bytes.extend_from_slice(b"\xFF\xFE"); // Invalid UTF-8
    bytes.extend_from_slice(b"}");
    fs::write(chats_dir.join("session-badutf8.json"), &bytes).unwrap();

    // Also write a valid session
    fs::write(
        chats_dir.join("session-good.json"),
        r#"{"sessionId": "s2", "messages": [{"type": "user", "content": "Valid"}]}"#,
    )
    .unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "invalid UTF-8 file should be skipped gracefully"
    );
    let convs = result.unwrap();
    // The valid session should still be parsed
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Valid");
}

#[test]
fn bom_marker_at_file_start_handled() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // UTF-8 BOM followed by valid JSON
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\xEF\xBB\xBF"); // UTF-8 BOM
    bytes.extend_from_slice(
        br#"{"sessionId": "s1", "messages": [{"type": "user", "content": "BOM test"}]}"#,
    );
    fs::write(chats_dir.join("session-bom.json"), &bytes).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    // BOM prepended to JSON will cause serde_json parse error, which is handled
    assert!(result.is_ok(), "BOM marker should not cause errors");
    // The BOM causes parse failure, so no conversations (graceful degradation)
}

#[test]
fn json_type_mismatch_messages_not_array() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // messages is a string instead of array
    fs::write(
        chats_dir.join("session-badtype.json"),
        r#"{"sessionId": "s1", "messages": "not an array"}"#,
    )
    .unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "type mismatch should not cause errors");
    assert!(result.unwrap().is_empty());
}

#[test]
fn json_type_mismatch_messages_is_number() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    fs::write(
        chats_dir.join("session-nummsgs.json"),
        r#"{"sessionId": "s1", "messages": 42}"#,
    )
    .unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn deeply_nested_json_does_not_stack_overflow() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Build deeply nested JSON (serde_json recursion limit ~128)
    let mut nested = String::new();
    for _ in 0..200 {
        nested.push_str("{\"a\":");
    }
    nested.push('1');
    for _ in 0..200 {
        nested.push('}');
    }
    fs::write(chats_dir.join("session-deep.json"), &nested).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "deeply nested JSON should not cause stack overflow"
    );
    assert!(result.unwrap().is_empty());
}

#[test]
fn large_message_body_handled() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let large = "x".repeat(1_000_000);
    let session_json = format!(
        r#"{{"sessionId": "s1", "messages": [{{"type": "user", "content": "{}"}}]}}"#,
        large
    );
    fs::write(chats_dir.join("session-large.json"), &session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "large message body should not cause OOM");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content.len(), 1_000_000);
}

#[test]
fn null_bytes_in_content_handled() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json =
        r#"{"sessionId": "s1", "messages": [{"type": "user", "content": "before\u0000after"}]}"#;
    fs::write(chats_dir.join("session-null.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "null bytes in content should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert!(!convs[0].messages.is_empty());
}

// ==================== Gemini-specific edge cases (br-3n1q) ====================

#[test]
fn multi_turn_conversation_threading() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Multi-turn with alternating user/model messages
    let session_json = r#"{
        "sessionId": "multi-turn",
        "messages": [
            {"type": "user", "content": "First question", "timestamp": "2025-01-15T10:00:00Z"},
            {"type": "model", "content": "First answer", "timestamp": "2025-01-15T10:00:05Z"},
            {"type": "user", "content": "Follow up", "timestamp": "2025-01-15T10:01:00Z"},
            {"type": "model", "content": "More info", "timestamp": "2025-01-15T10:01:05Z"},
            {"type": "user", "content": "Third turn", "timestamp": "2025-01-15T10:02:00Z"},
            {"type": "model", "content": "Final answer", "timestamp": "2025-01-15T10:02:05Z"}
        ]
    }"#;
    fs::write(chats_dir.join("session-multi.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 6);
    // Verify alternating roles maintained
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert_eq!(convs[0].messages[2].role, "user");
    assert_eq!(convs[0].messages[3].role, "assistant");
    assert_eq!(convs[0].messages[4].role, "user");
    assert_eq!(convs[0].messages[5].role, "assistant");
    // Verify ended_at tracks latest timestamp
    assert!(convs[0].ended_at.unwrap() > convs[0].started_at.unwrap());
}

#[test]
fn safety_filter_blocked_content_handled() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Model response with empty content (safety filter blocked)
    let session_json = r#"{
        "sessionId": "safety",
        "messages": [
            {"type": "user", "content": "Some question"},
            {"type": "model", "content": ""},
            {"type": "model", "content": "   "},
            {"type": "model", "content": "Safe response after filter"}
        ]
    }"#;
    fs::write(chats_dir.join("session-safety.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    // Empty/whitespace-only messages should be filtered out
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].content, "Some question");
    assert_eq!(convs[0].messages[1].content, "Safe response after filter");
}

#[test]
fn grounding_metadata_in_extra_preserved() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Message with grounding/citation metadata
    let session_json = r#"{
        "sessionId": "grounding",
        "messages": [
            {
                "type": "model",
                "content": "According to the documentation...",
                "groundingMetadata": {
                    "citations": [
                        {"uri": "https://example.com/docs", "title": "API Docs"}
                    ]
                }
            }
        ]
    }"#;
    fs::write(chats_dir.join("session-grounding.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages[0].content,
        "According to the documentation..."
    );
    // Grounding metadata should be preserved in extra field
    assert!(
        convs[0].messages[0]
            .extra
            .get("groundingMetadata")
            .is_some(),
        "groundingMetadata should be preserved in extra"
    );
}

#[test]
fn multipart_content_with_image_refs() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    // Multi-part content with text and inline data (image references)
    let session_json = r#"{
        "sessionId": "multipart",
        "messages": [
            {
                "type": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {"type": "image_url", "url": "data:image/png;base64,iVBOR..."}
                ]
            },
            {
                "type": "model",
                "content": "I see a diagram showing..."
            }
        ]
    }"#;
    fs::write(chats_dir.join("session-multipart.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "multipart content with image refs should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert!(!convs[0].messages.is_empty());
    // The text part should be extracted
    assert!(
        convs[0].messages[0]
            .content
            .contains("What's in this image")
    );
}

#[test]
fn timestamp_edge_cases() {
    let dir = TempDir::new().unwrap();
    let hash_dir = dir.path().join("gemini_hash");
    let chats_dir = hash_dir.join("chats");
    fs::create_dir_all(&chats_dir).unwrap();

    let session_json = r#"{
        "sessionId": "ts-edge",
        "startTime": 1700000000000,
        "lastUpdated": "2025-12-01T10:00:00+05:30",
        "messages": [
            {"type": "user", "content": "Epoch millis start", "timestamp": 1700000000000},
            {"type": "user", "content": "ISO with offset", "timestamp": "2025-12-01T10:00:00+05:30"},
            {"type": "user", "content": "ISO with ms", "timestamp": "2025-12-01T10:00:00.123Z"},
            {"type": "user", "content": "No timestamp"},
            {"type": "user", "content": "Null ts", "timestamp": null}
        ]
    }"#;
    fs::write(chats_dir.join("session-tsedge.json"), session_json).unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "varied timestamp formats should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 5);
    // First three should have valid timestamps
    assert!(convs[0].messages[0].created_at.is_some());
    assert!(convs[0].messages[1].created_at.is_some());
    assert!(convs[0].messages[2].created_at.is_some());
    // Session-level timestamps should be set
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

#[test]
fn json_stream_handles_bom_whitespace_array_dispatch_and_truncated_mid_array() {
    use std::io::Cursor;

    // 1. Leading UTF-8 BOM + whitespace around a top-level `{...}` session object.
    let mut bom_session = Vec::from(b"\xEF\xBB\xBF \t\r\n".as_slice());
    bom_session.extend_from_slice(
        br#"{"sessionId":"bom-s1","projectHash":"ph","startTime":"2025-01-15T10:00:00Z","lastUpdated":"2025-01-15T10:05:00Z","messages":[{"type":"user","content":"BOM streamed"},{"type":"model","content":"   "},{"type":"gemini","content":"Reply"}]}"#,
    );
    let parsed = parse_json_session_reader(Cursor::new(&bom_session))
        .unwrap()
        .unwrap();
    assert_eq!(parsed.session_id.as_deref(), Some("bom-s1"));
    assert_eq!(parsed.project_hash.as_deref(), Some("ph"));
    assert_eq!(parsed.messages.len(), 2);
    assert_eq!(parsed.messages[0].content, "BOM streamed");
    assert_eq!(parsed.messages[1].role, "assistant");
    assert_eq!(parsed.messages[1].content, "Reply");

    // 2. Empty messages array inside `{...}` parses cleanly with 0 messages.
    let empty_msgs =
        parse_json_session_reader(Cursor::new(br#"{"sessionId":"empty","messages":[]}"#))
            .unwrap()
            .unwrap();
    assert!(empty_msgs.messages.is_empty());

    // 3. Top-level `[]` and valid `[...]` dispatch returns None without error.
    assert!(
        parse_json_session_reader(Cursor::new(b"[]"))
            .unwrap()
            .is_none()
    );
    assert!(
        parse_json_session_reader(Cursor::new(br#"[{"type":"user","content":"x"}]"#))
            .unwrap()
            .is_none()
    );

    // 4. Truncated mid-array JSON returns Err (never silently truncating).
    assert!(
        parse_json_session_reader(Cursor::new(
            br#"{"sessionId":"trunc","messages":[{"type":"user","content":"ok"},{"type":"model","content":"cut"#,
        ))
        .is_err()
    );
    assert!(parse_json_session_reader(Cursor::new(br#"[{"type":"user""#)).is_err());

    // 5. Connector scan with BOM succeeds and truncated mid-array never marks source complete.
    let dir = TempDir::new().unwrap();
    let chats_dir = dir.path().join("gemini_hash").join("chats");
    fs::create_dir_all(&chats_dir).unwrap();
    fs::write(chats_dir.join("session-bom.json"), &bom_session).unwrap();
    fs::write(
        chats_dir.join("session-trunc-mid-array.json"),
        br#"{"sessionId":"trunc","messages":[{"type":"user","content":"partial"},{"type":"model""#,
    )
    .unwrap();

    let connector = GeminiConnector::new();
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let mut completed = Vec::new();
    let mut on_complete = |c: &SourceCompletion| {
        completed.push(c.source.source_path.clone());
        Ok(())
    };
    let mut emitted = Vec::new();
    connector
        .scan_with_source_boundaries(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut on_complete),
            },
            &mut |conv| {
                emitted.push(conv);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].external_id.as_deref(), Some("bom-s1"));
    assert_eq!(completed.len(), 1);
    assert!(completed[0].ends_with("session-bom.json"));
}

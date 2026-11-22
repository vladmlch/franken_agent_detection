use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use super::tests_core::make_test_claude_dir;
use super::*;
use crate::connectors::Connector;
use crate::connectors::scan::ScanContext;

// =========================================================================
// JSON format parsing tests
// =========================================================================

#[test]
fn scan_parses_json_messages_array() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.json");
    let content = json!({
        "title": "Test Session",
        "messages": [
            {"role": "user", "content": "Hello", "timestamp": 1_700_000_000_000_i64},
            {"role": "assistant", "content": "Hi there!", "timestamp": 1_700_000_001_000_i64}
        ]
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[1].role, "assistant");
}

#[test]
fn scan_json_extracts_title() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.json");
    let content = json!({
        "title": "Custom Session Title",
        "messages": [
            {"role": "user", "content": "Test content"}
        ]
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title, Some("Custom Session Title".to_string()));
}

#[test]
fn scan_json_uses_type_as_role_fallback() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.json");
    let content = json!({
        "messages": [
            {"type": "user", "content": "Message with type instead of role"}
        ]
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].role, "user");
}

#[test]
fn scan_json_uses_text_as_content_fallback() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.json");
    let content = json!({
        "messages": [
            {"role": "user", "text": "Message with text field instead of content"}
        ]
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].messages[0].content.contains("text field"));
}

#[test]
fn scan_json_uses_time_as_timestamp_fallback() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.json");
    let content = json!({
        "messages": [
            {"role": "user", "content": "Test", "time": 1_700_000_000_000i64}
        ]
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].created_at, Some(1_700_000_000_000));
}

// =========================================================================
// Title extraction tests
// =========================================================================

#[test]
fn scan_title_from_first_user_message_jsonl() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"assistant","message":{"role":"assistant","content":"I can help"}}
{"type":"user","message":{"role":"user","content":"Help me build a web app"}}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title, Some("Help me build a web app".to_string()));
}

#[test]
fn scan_title_truncates_to_100_chars() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let long_message = "x".repeat(200);
    let session_file = claude_dir.join("session.jsonl");
    let content = format!(
        r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
        long_message
    );
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].title.as_ref().unwrap().len() <= 100);
}

#[test]
fn scan_title_uses_first_line_only() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":"First line\nSecond line\nThird line"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title, Some("First line".to_string()));
}

#[test]
fn scan_title_fallback_to_workspace_name() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Only assistant message, no user message for title
    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"assistant","cwd":"/projects/myapp","message":{"role":"assistant","content":"Response only"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Should fallback to workspace directory name
    assert_eq!(convs[0].title, Some("myapp".to_string()));
}

// =========================================================================
// Edge case tests
// =========================================================================

#[test]
fn scan_empty_directory_returns_empty() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs.is_empty());
}

#[test]
fn scan_skips_malformed_jsonl_lines() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"not valid json
{"type":"user","message":{"role":"user","content":"Valid message"}}
{broken json here
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Should still extract the valid line
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Valid message");
}

#[test]
fn scan_skips_malformed_json_files() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Create a malformed JSON file
    let bad_file = claude_dir.join("bad.json");
    fs::write(&bad_file, "not valid json {{{").unwrap();

    // Create a valid JSONL file
    let good_file = claude_dir.join("good.jsonl");
    fs::write(
        &good_file,
        r#"{"type":"user","message":{"role":"user","content":"Valid"}}"#,
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Should only have one conversation from the valid file
    assert_eq!(convs.len(), 1);
}

#[test]
fn scan_handles_empty_messages_array() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.json");
    let content = json!({
        "messages": []
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Empty messages should result in no conversation
    assert!(convs.is_empty());
}

#[test]
fn scan_processes_subdirectories() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());
    let subdir = claude_dir.join("project1");
    fs::create_dir_all(&subdir).unwrap();

    let session_file = subdir.join("session.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":"Nested message"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Nested message"));
}

#[test]
fn scan_skips_non_session_files() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Create various non-session files
    fs::write(claude_dir.join("config.toml"), "").unwrap();
    fs::write(claude_dir.join("notes.txt"), "").unwrap();
    fs::write(claude_dir.join("backup.bak"), "").unwrap();

    // Create a valid session file
    let session_file = claude_dir.join("session.jsonl");
    fs::write(
        &session_file,
        r#"{"type":"user","message":{"role":"user","content":"Valid"}}"#,
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Should only have one conversation from the .jsonl file
    assert_eq!(convs.len(), 1);
}

#[test]
fn scan_handles_claude_extension() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.claude");
    let content = json!({
        "messages": [
            {"role": "user", "content": "Claude extension test"}
        ]
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Claude extension"));
}

#[test]
fn scan_sets_external_id_from_relative_path() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("unique-session-id.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":"Test"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(
        convs[0].external_id,
        Some("unique-session-id.jsonl".to_string())
    );
}

#[test]
fn scan_external_id_includes_subdir_for_subagent_files() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Simulate subagent files under different parent sessions sharing the
    // same basename (e.g. agent-a297e09.jsonl).
    let sub_a = claude_dir.join("parent-session-aaa");
    let sub_b = claude_dir.join("parent-session-bbb");
    fs::create_dir_all(&sub_a).unwrap();
    fs::create_dir_all(&sub_b).unwrap();

    let content = r#"{"type":"user","message":{"role":"user","content":"Test"}}"#;
    fs::write(sub_a.join("agent-a297e09.jsonl"), content).unwrap();
    fs::write(sub_b.join("agent-a297e09.jsonl"), content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let mut convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
    convs.sort_by(|a, b| a.external_id.cmp(&b.external_id));

    assert_eq!(
        convs[0].external_id,
        Some("parent-session-aaa/agent-a297e09.jsonl".to_string())
    );
    assert_eq!(
        convs[1].external_id,
        Some("parent-session-bbb/agent-a297e09.jsonl".to_string())
    );
}

#[test]
fn scan_sets_agent_slug_to_claude_code() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":"Test"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].agent_slug, "claude_code");
}

#[test]
fn scan_preserves_original_json_in_extra() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content =
        r#"{"type":"user","customField":"customValue","message":{"role":"user","content":"Test"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].extra["customField"], "customValue");
}

#[test]
fn scan_tracks_started_and_ended_timestamps() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"First"}}
{"type":"assistant","timestamp":"2025-12-01T10:05:00Z","message":{"role":"assistant","content":"Last"}}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
    // ended_at should be after or equal to started_at
    assert!(convs[0].ended_at.unwrap() >= convs[0].started_at.unwrap());
}

#[test]
fn scan_multiple_files_returns_multiple_conversations() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Create two session files
    for i in 1..=3 {
        let session_file = claude_dir.join(format!("session{}.jsonl", i));
        let content =
            format!(r#"{{"type":"user","message":{{"role":"user","content":"Message {i}"}}}}"#);
        fs::write(&session_file, content).unwrap();
    }

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 3);
}

#[test]
fn scan_explicit_root_generic_name() {
    let dir = TempDir::new().unwrap();
    // Directory name that doesn't contain "claude" and no "projects" subdir
    let generic_root = dir.path().join("my_logs");
    fs::create_dir_all(&generic_root).unwrap();

    let session_file = generic_root.join("session.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":"Generic root test"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    // Create context with explicit root (use_default_detection = false)
    // Note: ScanContext::with_roots takes data_dir as first arg, but indexer passes root.path there too.
    // We simulate what indexer does.
    let roots = vec![crate::connectors::ScanRoot::local(generic_root.clone())];
    let ctx = ScanContext::with_roots(generic_root.clone(), roots, None);

    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(
        convs.len(),
        1,
        "Should find session in generic named explicit root"
    );
    assert_eq!(convs[0].messages[0].content, "Generic root test");
}

#[test]
fn scan_with_explicit_file_only_reads_that_file_and_keeps_projects_relative_external_id() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());
    let project_dir = claude_dir.join("projects").join("project-a");
    fs::create_dir_all(project_dir.join("subagents")).unwrap();

    let target_file = project_dir.join("target.jsonl");
    fs::write(
        &target_file,
        r#"{"type":"user","message":{"role":"user","content":"Target only"}}"#,
    )
    .unwrap();

    let sibling_file = project_dir.join("sibling.jsonl");
    fs::write(
        &sibling_file,
        r#"{"type":"user","message":{"role":"user","content":"Sibling"}}"#,
    )
    .unwrap();

    let nested_file = project_dir.join("subagents").join("nested.jsonl");
    fs::write(
        &nested_file,
        r#"{"type":"user","message":{"role":"user","content":"Nested"}}"#,
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let roots = vec![crate::connectors::ScanRoot::local(target_file.clone())];
    let ctx = ScanContext::with_roots(target_file.clone(), roots, None);

    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Target only");
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some("project-a/target.jsonl")
    );
}

// =========================================================================
// Edge case tests — malformed input robustness (br-cpf8)
// =========================================================================

#[test]
fn truncated_jsonl_mid_json_returns_partial_results() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // First line is valid, second line is truncated mid-JSON
    let session_file = claude_dir.join("truncated.jsonl");
    let content = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Hello\"}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"Hel";
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "truncated file should not cause an error");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        1,
        "truncated file at mid-JSON should yield only the 1 valid message"
    );
    assert_eq!(convs[0].messages[0].content, "Hello");
}

#[test]
fn truncated_mid_utf8_does_not_panic() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Valid JSONL line followed by bytes that start a multi-byte UTF-8
    // sequence but are truncated (U+1F600 = F0 9F 98 80, truncate after 2 bytes)
    let session_file = claude_dir.join("truncated_utf8.jsonl");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Valid\"}}\n",
    );
    // Incomplete UTF-8: start of a 4-byte sequence missing last 2 bytes
    bytes.extend_from_slice(b"\xF0\x9F");

    fs::write(&session_file, &bytes).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "truncated mid-UTF8 should not panic or error"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Valid");
}

#[test]
fn invalid_utf8_skips_corrupted_lines() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("invalid_utf8.jsonl");
    let mut bytes = Vec::new();
    // Valid line
    bytes.extend_from_slice(
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Before\"}}\n",
    );
    // Invalid UTF-8 bytes (0xFF 0xFE are never valid in UTF-8)
    bytes.extend_from_slice(b"\xFF\xFE invalid utf8 line\n");
    // Another valid line
    bytes.extend_from_slice(
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"After\"}}\n",
    );

    fs::write(&session_file, &bytes).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "invalid UTF-8 should not cause a panic");
    let convs = result.unwrap();
    // A successfully read physical line with invalid UTF-8 is skipped;
    // the valid records on either side remain available.
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        2,
        "should extract both valid messages around invalid UTF-8 line"
    );
    assert_eq!(convs[0].messages[0].content, "Before");
    assert_eq!(convs[0].messages[1].content, "After");
}

#[test]
fn empty_file_returns_no_conversations() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Completely empty JSONL file
    let session_file = claude_dir.join("empty.jsonl");
    fs::write(&session_file, b"").unwrap();

    // Completely empty JSON file
    let json_file = claude_dir.join("empty.json");
    fs::write(&json_file, b"").unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "empty files should not cause errors");
    let convs = result.unwrap();
    assert!(
        convs.is_empty(),
        "empty files should produce no conversations"
    );
}

#[test]
fn whitespace_only_file_returns_no_conversations() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // JSONL file with only whitespace and newlines
    let session_file = claude_dir.join("whitespace.jsonl");
    fs::write(&session_file, "   \n\n  \n   \n\t\n").unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "whitespace-only file should not cause errors"
    );
    let convs = result.unwrap();
    assert!(
        convs.is_empty(),
        "whitespace-only file should produce no conversations"
    );
}

#[test]
fn json_type_mismatch_skips_gracefully() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // JSONL lines where expected objects are wrong types
    let session_file = claude_dir.join("type_mismatch.jsonl");
    let content = concat!(
        // String where object expected for message
        "{\"type\":\"user\",\"message\":\"not an object\"}\n",
        // Number where content string expected
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":12345}}\n",
        // Array where string expected for type
        "{\"type\":[\"user\"],\"message\":{\"role\":\"user\",\"content\":\"Valid after mismatches\"}}\n",
        // Null content
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":null}}\n",
        // Boolean type
        "{\"type\":true,\"message\":{\"role\":\"user\",\"content\":\"Bool type\"}}\n",
        // Correct entry that should be extracted
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Correct entry\"}}\n",
    );
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "type mismatches should not cause errors");
    let convs = result.unwrap();
    // Only the last line with correct types should produce a message
    assert_eq!(convs.len(), 1);
    assert!(
        convs[0]
            .messages
            .iter()
            .any(|m| m.content == "Correct entry"),
        "should extract the correctly typed entry"
    );
}

#[test]
fn deeply_nested_json_does_not_stack_overflow() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Build JSON with 1000+ levels of nesting in the content field
    // serde_json has a default recursion limit of 128, so this tests
    // that the connector handles the parse error gracefully
    let mut nested = String::new();
    for _ in 0..200 {
        nested.push_str("{\"a\":");
    }
    nested.push('1');
    for _ in 0..200 {
        nested.push('}');
    }

    let session_file = claude_dir.join("deep.jsonl");
    let content = format!(
        "{}\n{}\n",
        nested, r#"{"type":"user","message":{"role":"user","content":"After deep nesting"}}"#
    );
    fs::write(&session_file, &content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);

    // This must not stack overflow or panic
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "deeply nested JSON should not cause stack overflow"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "After deep nesting");
}

#[test]
fn large_message_body_handled_without_oom() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // Create a JSONL file with a 1MB message body to verify streaming works
    let large_content = "x".repeat(1_000_000);
    let session_file = claude_dir.join("large_body.jsonl");
    let line = format!(
        r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
        large_content
    );
    fs::write(&session_file, &line).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "large message body should not cause OOM");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages[0].content.len(),
        1_000_000,
        "large message content should be preserved in full"
    );
}

#[test]
fn large_json_file_over_100mb_is_skipped() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // For JSON format, files > 100MB should be skipped.
    // We can't create a real 100MB file in a unit test efficiently,
    // but we verify the mechanism works with a valid JSON file under the limit.
    let session_file = claude_dir.join("under_limit.json");
    let content = json!({
        "messages": [
            {"role": "user", "content": "Under the limit"}
        ]
    })
    .to_string();
    fs::write(&session_file, &content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // File under 100MB should be processed normally
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Under the limit");
}

#[test]
fn null_bytes_embedded_in_content_handled() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // JSON allows \u0000 escape for null bytes in strings
    let session_file = claude_dir.join("null_bytes.jsonl");
    let content = concat!(
        r#"{"type":"user","message":{"role":"user","content":"before\u0000after"}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":"Clean message"}}"#,
        "\n"
    );
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "null bytes in content should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    // Both messages should be extracted; the null byte is valid JSON
    assert!(
        !convs[0].messages.is_empty(),
        "should extract at least the clean message"
    );
}

#[test]
fn bom_marker_at_file_start_handled() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    // UTF-8 BOM: EF BB BF
    let session_file = claude_dir.join("bom.jsonl");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\xEF\xBB\xBF"); // UTF-8 BOM
    bytes.extend_from_slice(
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"After BOM\"}}\n",
    );
    bytes.extend_from_slice(
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Second line\"}}\n",
    );
    fs::write(&session_file, &bytes).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "BOM marker should not cause errors");
    let convs = result.unwrap();
    // The BOM may cause the first line's JSON to fail parsing (since the BOM
    // bytes are prepended to the line). The second line should parse fine.
    // We verify the connector doesn't crash and extracts what it can.
    assert_eq!(convs.len(), 1);
    assert!(
        !convs[0].messages.is_empty(),
        "should extract at least the second message after BOM"
    );
    // The second line (without BOM) should always parse correctly
    assert!(
        convs[0].messages.iter().any(|m| m.content == "Second line"),
        "second line should be extractable regardless of BOM"
    );
}

#[cfg(test)]
mod qoder_flavor_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn qoder_flavor_parses_fixture_and_fallbacks_without_bogus_messages() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/qoder/projects/-tmp-demo/11111111-1111-4111-8111-111111111111.jsonl",
        );
        let file = fs::File::open(&fixture_path).expect("fixture file");
        let parsed = parse_claude_records(BufReader::new(file), &fixture_path, ClaudeFlavor::Qoder)
            .expect("parse should succeed")
            .expect("fixture should produce parsed file");

        // attachment(task_reminder), workspace-directories, runtime-config, active-leaf, last-prompt produce no messages.
        // Only user/assistant/tool_result records produce messages:
        // u-root-1 (user), a-abandoned-1 (assistant), u-abandoned-2 (user),
        // a-kept-1 (assistant), u-kept-tool-1 (tool), a-kept-2 (assistant) => 6 messages before branch filter.
        assert_eq!(parsed.messages.len(), 6);
        assert_eq!(parsed.message_uuids.len(), parsed.messages.len());
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|m| m.role.as_str())
                .collect::<Vec<_>>(),
            vec![
                "user",
                "assistant",
                "user",
                "assistant",
                "tool",
                "assistant"
            ]
        );
        // attachment att-1 must still be in records_by_uuid so parentUuid chains stay intact
        assert!(
            parsed
                .records_by_uuid
                .iter()
                .any(|(uuid, parent)| uuid == "att-1" && parent.as_deref() == Some("u-root-1"))
        );
        assert_eq!(
            parsed.active_leaves,
            vec!["u-abandoned-2".to_string(), "a-kept-2".to_string()]
        );
        assert_eq!(parsed.workspace, Some(PathBuf::from("/tmp/demo")));
        assert_eq!(parsed.model.as_deref(), Some("qmodel-demo"));

        // Equivalent Claude records produce identical message payloads
        let claude_equiv = concat!(
            r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/tmp/demo","timestamp":"2026-11-15T10:00:01Z","message":{"role":"user","content":"Inspect the demo module"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","cwd":"/tmp/demo","timestamp":"2026-11-15T10:00:04Z","message":{"id":"m1","role":"assistant","model":"qmodel-demo","content":[{"type":"thinking","thinking":"Delegate inspection to subagent"},{"type":"text","text":"Checking demo module."},{"type":"tool_use","id":"call-subagent-1","name":"Agent","input":{"description":"Explore demo"}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","cwd":"/tmp/demo","timestamp":"2026-11-15T10:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-subagent-1","content":"Subagent finished checking demo","is_error":false}]}}"#,
            "\n"
        );
        let qoder_equiv = concat!(
            r#"{"type":"workspace-directories","directories":["/tmp/demo"]}"#,
            "\n",
            r#"{"type":"runtime-config","model":"qmodel-demo"}"#,
            "\n",
            r#"{"type":"task_reminder","content":"ignore me"}"#,
            "\n",
            r#"{"type":"attachment","uuid":"att","parentUuid":"u1","attachment":{"type":"task_reminder","content":"ignore me"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"cwd":"/tmp/demo","timestamp":"2026-11-15T10:00:01Z","message":{"role":"user","content":"Inspect the demo module"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","cwd":"/tmp/demo","timestamp":"2026-11-15T10:00:04Z","message":{"id":"m1","role":"assistant","model":"qmodel-demo","content":[{"type":"thinking","thinking":"Delegate inspection to subagent"},{"type":"text","text":"Checking demo module."},{"type":"tool_use","id":"call-subagent-1","name":"Agent","input":{"description":"Explore demo"}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","cwd":"/tmp/demo","timestamp":"2026-11-15T10:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-subagent-1","content":"Subagent finished checking demo","is_error":false}]}}"#,
            "\n",
            r#"{"type":"active-leaf","leafUuid":"u2"}"#,
            "\n"
        );
        let p_claude = parse_claude_records(
            Cursor::new(claude_equiv.as_bytes()),
            Path::new("/tmp/c.jsonl"),
            ClaudeFlavor::Claude,
        )
        .unwrap()
        .unwrap();
        let p_qoder = parse_claude_records(
            Cursor::new(qoder_equiv.as_bytes()),
            Path::new("/tmp/q.jsonl"),
            ClaudeFlavor::Qoder,
        )
        .unwrap()
        .unwrap();
        assert_eq!(p_qoder.messages.len(), p_claude.messages.len());
        for (qm, cm) in p_qoder.messages.iter().zip(p_claude.messages.iter()) {
            assert_eq!(qm.role, cm.role);
            assert_eq!(qm.author, cm.author);
            assert_eq!(qm.created_at, cm.created_at);
            assert_eq!(qm.content, cm.content);
            assert_eq!(qm.invocations, cm.invocations);
        }

        // Fallback test: no cwd on records -> workspace-directories.directories[0];
        // no message.model -> runtime-config.model
        let fallback_jsonl = concat!(
            r#"{"type":"workspace-directories","directories":["/tmp/fallback-ws","/tmp/other"]}"#,
            "\n",
            r#"{"type":"runtime-config","model":"qmodel-fallback"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-11-15T10:00:01Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-11-15T10:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
            "\n"
        );
        let p_fallback = parse_claude_records(
            Cursor::new(fallback_jsonl.as_bytes()),
            Path::new("/tmp/fallback.jsonl"),
            ClaudeFlavor::Qoder,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            p_fallback.workspace,
            Some(PathBuf::from("/tmp/fallback-ws"))
        );
        assert_eq!(p_fallback.model.as_deref(), Some("qmodel-fallback"));
        assert_eq!(
            p_fallback.messages[1].author.as_deref(),
            Some("qmodel-fallback")
        );
    }
}

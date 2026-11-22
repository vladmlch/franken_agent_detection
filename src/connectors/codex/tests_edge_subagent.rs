use std::fs;
#[cfg(feature = "codex-zstd")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(feature = "codex-zstd")]
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::CodexConnector;
#[cfg(feature = "codex-zstd")]
use super::discovery::RolloutFormat;
use crate::connectors::Connector;
use crate::connectors::scan::ScanContext;
use crate::types::NormalizedConversation;

#[cfg(not(feature = "codex-zstd"))]
#[test]
fn compressed_rollouts_need_the_codex_zstd_feature() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("rollout-a.jsonl.zst"), b"x").unwrap();
    fs::write(sessions.join("rollout-b.jsonl"), b"x").unwrap();
    let files = CodexConnector::rollout_files(dir.path());
    assert_eq!(files, vec![sessions.join("rollout-b.jsonl")]);
}

/// GH cass#513: Codex's `local_thread_store_compression` replaces a
/// finished session's `rollout-*.jsonl` with `rollout-*.jsonl.zst`.
#[cfg(feature = "codex-zstd")]
mod compressed_rollouts {
    use super::*;

    const FIXTURE: &str = "sessions/2026/06/28/rollout-2026-06-28T10-00-00-modern-fixture.jsonl";

    /// A Codex home holding the fixture session, plain or compressed the
    /// way Codex compresses it: one frame that records the decoded length.
    fn codex_home(dir: &TempDir, compressed: bool) -> PathBuf {
        let home = dir.path().join(".codex");
        let plain = home.join(FIXTURE);
        fs::create_dir_all(plain.parent().unwrap()).unwrap();
        let text = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/codex")
                .join(FIXTURE),
        )
        .unwrap();
        if compressed {
            let packed = zstd::bulk::compress(&text, 3).unwrap();
            fs::write(plain.with_extension("jsonl.zst"), packed).unwrap();
        } else {
            fs::write(&plain, text).unwrap();
        }
        home
    }

    fn scan(home: PathBuf, since_ts: Option<i64>) -> Vec<Value> {
        CodexConnector::new()
            .scan(&ScanContext::local_default(home, since_ts))
            .unwrap()
            .into_iter()
            .map(|conversation| serde_json::to_value(conversation).unwrap())
            .collect()
    }

    #[test]
    fn a_compressed_rollout_scans_to_the_conversation_its_plain_form_gives() {
        let plain_dir = TempDir::new().unwrap();
        let packed_dir = TempDir::new().unwrap();
        let mut plain = scan(codex_home(&plain_dir, false), None);
        let mut packed = scan(codex_home(&packed_dir, true), None);
        assert_eq!(plain.len(), 1);
        assert_eq!(packed.len(), 1, "the compressed session is found");

        let plain_path = plain[0].as_object_mut().unwrap().remove("source_path");
        let packed_path = packed[0].as_object_mut().unwrap().remove("source_path");
        let format_of =
            |path: Option<Value>| RolloutFormat::of(Path::new(path.unwrap().as_str().unwrap()));
        assert_eq!(format_of(plain_path), Some(RolloutFormat::Jsonl));
        assert_eq!(format_of(packed_path), Some(RolloutFormat::CompressedJsonl));
        // Same id, messages, workspace, times and metadata: a session
        // indexed before Codex compressed it is the same session after.
        assert_eq!(packed, plain);
        assert_eq!(
            packed[0]["external_id"],
            "2026/06/28/rollout-2026-06-28T10-00-00-modern-fixture"
        );
        assert_eq!(packed[0]["messages"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn a_compressed_rollout_keeps_the_modification_time_filter() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir, true);
        let future = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 3_600_000;
        assert!(scan(home, Some(future)).is_empty());
    }

    #[test]
    fn discovery_finds_compressed_rollouts_and_prefers_a_plain_copy() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        for name in [
            "rollout-a.jsonl.zst",
            "rollout-b.JSONL.ZST",
            // Both exist for a moment while Codex compresses a session.
            "rollout-c.jsonl",
            "rollout-c.jsonl.zst",
            "other-d.jsonl.zst",
            "rollout-e.zst",
            "rollout-f.json.zst",
        ] {
            fs::write(sessions.join(name), b"x").unwrap();
        }
        let names = CodexConnector::rollout_files(dir.path())
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "rollout-a.jsonl.zst",
                "rollout-b.JSONL.ZST",
                "rollout-c.jsonl"
            ]
        );
    }

    #[test]
    fn a_cut_short_compressed_rollout_fails_the_scan_instead_of_shortening_it() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir, true);
        let packed = home.join(FIXTURE).with_extension("jsonl.zst");
        let bytes = fs::read(&packed).unwrap();
        fs::write(&packed, &bytes[..bytes.len() - 4]).unwrap();
        let error = CodexConnector::new()
            .scan(&ScanContext::local_default(home, None))
            .expect_err("a cut-short compressed rollout is not a complete session");
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::UnexpectedEof),
            "{error:#}"
        );
    }
}

#[test]
fn scan_skips_conversations_with_no_messages() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Only metadata, no actual messages
    let content = r#"{"type":"session_meta","payload":{"cwd":"/test"}}
{"type":"turn_context","payload":{}}
"#;
    fs::write(sessions.join("rollout-no-msgs.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Should be skipped because no actual messages
    assert_eq!(convs.len(), 0);
}

#[test]
fn scan_handles_array_content_in_response_item() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Content as array of text blocks (like Claude API format)
    let content = json!({
        "type": "response_item",
        "payload": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Part one."},
                {"type": "text", "text": " Part two."}
            ]
        }
    });
    fs::write(
        sessions.join("rollout-array.jsonl"),
        content.to_string() + "\n",
    )
    .unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    // flatten_content should combine the parts
    assert!(convs[0].messages[0].content.contains("Part one"));
}

#[test]
fn scan_uses_default_role_when_missing() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // No role specified in payload
    let content = r#"{"type":"response_item","payload":{"content":"No role specified"}}
"#;
    fs::write(sessions.join("rollout-no-role.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    // Default role should be "agent"
    assert_eq!(convs[0].messages[0].role, "agent");
}

#[test]
fn scan_stores_source_path() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
    let file_path = sessions.join("rollout-path.jsonl");
    fs::write(&file_path, content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].source_path, file_path);
}

// =====================================================
// Edge case tests — malformed input robustness (br-fiiv)
// =====================================================

#[test]
fn truncated_jsonl_mid_json_requires_retry() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // First line valid, second truncated mid-JSON
    let content = b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Valid\"}}\n{\"type\":\"response_item\",\"payload\":{\"role\":\"assistant\",\"con";
    fs::write(sessions.join("rollout-truncated.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    let error = result.expect_err("unfinished input requires retry");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::UnexpectedEof)
    );
}

#[test]
fn truncated_mid_utf8_requires_retry_without_panicking() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"OK\"}}\n",
    );
    // Incomplete 4-byte UTF-8 sequence (U+1F600 = F0 9F 98 80, only 2 bytes)
    bytes.extend_from_slice(b"\xF0\x9F");

    fs::write(sessions.join("rollout-utf8trunc.jsonl"), &bytes).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    let error = result.expect_err("incomplete UTF-8 requires retry");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::InvalidData)
    );
}

#[test]
fn invalid_utf8_requires_retry_instead_of_partial_success() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Before\"}}\n",
    );
    bytes.extend_from_slice(b"\xFF\xFE invalid utf8 line\n");
    bytes.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"After\"}}\n",
    );

    fs::write(sessions.join("rollout-badbytes.jsonl"), &bytes).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    let error = result.expect_err("invalid UTF-8 must not certify partial history");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::InvalidData)
    );
}

#[test]
fn empty_file_returns_no_conversations() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    fs::write(sessions.join("rollout-empty.jsonl"), b"").unwrap();
    fs::write(sessions.join("rollout-empty.json"), b"").unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
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
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    fs::write(sessions.join("rollout-ws.jsonl"), "  \n\n  \t\n").unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
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
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = concat!(
        // payload is a string instead of object
        "{\"type\":\"response_item\",\"payload\":\"not an object\"}\n",
        // type is a number
        "{\"type\":123,\"payload\":{\"role\":\"user\",\"content\":\"num type\"}}\n",
        // content is a number
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":99}}\n",
        // Correct entry
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Correct\"}}\n",
    );
    fs::write(sessions.join("rollout-types.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "type mismatches should not cause errors");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert!(
        convs[0].messages.iter().any(|m| m.content == "Correct"),
        "should extract the correctly typed entry"
    );
}

#[test]
fn deeply_nested_json_does_not_stack_overflow() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // serde_json has a recursion limit of 128; 200 levels will trigger parse error
    let mut nested = String::new();
    for _ in 0..200 {
        nested.push_str("{\"a\":");
    }
    nested.push('1');
    for _ in 0..200 {
        nested.push('}');
    }

    let content = format!(
        "{}\n{}\n",
        nested, r#"{"type":"response_item","payload":{"role":"user","content":"After nesting"}}"#
    );
    fs::write(sessions.join("rollout-deep.jsonl"), &content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "deeply nested JSON should not cause stack overflow"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content, "After nesting");
}

#[test]
fn large_message_body_handled_without_oom() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let large_content = "x".repeat(1_000_000);
    let line = format!(
        r#"{{"type":"response_item","payload":{{"role":"user","content":"{}"}}}}"#,
        large_content
    );
    fs::write(sessions.join("rollout-large.jsonl"), &line).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "large message body should not cause OOM");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages[0].content.len(), 1_000_000);
}

#[test]
fn null_bytes_embedded_in_content_handled() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = concat!(
        r#"{"type":"response_item","payload":{"role":"user","content":"before\u0000after"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"role":"user","content":"Clean"}}"#,
        "\n"
    );
    fs::write(sessions.join("rollout-null.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "null bytes in content should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert!(!convs[0].messages.is_empty());
}

#[test]
fn bom_marker_at_file_start_handled() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\xEF\xBB\xBF"); // UTF-8 BOM
    bytes.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"BOM line\"}}\n",
    );
    bytes.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Second\"}}\n",
    );
    fs::write(sessions.join("rollout-bom.jsonl"), &bytes).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "BOM marker should not cause errors");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert!(
        !convs[0].messages.is_empty(),
        "should extract at least the second line after BOM"
    );
    assert!(
        convs[0].messages.iter().any(|m| m.content == "Second"),
        "second line should parse correctly regardless of BOM"
    );
}

// =====================================================
// Codex-specific edge cases (br-fiiv)
// =====================================================

#[test]
fn missing_payload_field_skipped() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // response_item and event_msg without payload field
    let content = concat!(
        "{\"type\":\"response_item\",\"timestamp\":\"2025-12-01T10:00:00Z\"}\n",
        "{\"type\":\"event_msg\",\"timestamp\":\"2025-12-01T10:00:01Z\"}\n",
        "{\"type\":\"session_meta\",\"timestamp\":\"2025-12-01T10:00:02Z\"}\n",
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Has payload\"}}\n",
    );
    fs::write(sessions.join("rollout-nopayload.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok(), "missing payload should not cause errors");
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Has payload");
}

#[test]
fn timestamp_parsing_edge_cases() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = concat!(
        // ISO 8601 with milliseconds
        "{\"type\":\"response_item\",\"timestamp\":\"2025-12-01T10:00:00.123Z\",\"payload\":{\"role\":\"user\",\"content\":\"ms precision\"}}\n",
        // ISO 8601 with timezone offset
        "{\"type\":\"response_item\",\"timestamp\":\"2025-12-01T10:00:00+05:30\",\"payload\":{\"role\":\"user\",\"content\":\"tz offset\"}}\n",
        // Unix epoch milliseconds as number
        "{\"type\":\"response_item\",\"timestamp\":1700000000000,\"payload\":{\"role\":\"user\",\"content\":\"epoch millis\"}}\n",
        // No timestamp at all
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"no timestamp\"}}\n",
        // Null timestamp
        "{\"type\":\"response_item\",\"timestamp\":null,\"payload\":{\"role\":\"user\",\"content\":\"null ts\"}}\n",
    );
    fs::write(sessions.join("rollout-timestamps.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "varied timestamp formats should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        5,
        "all 5 messages should be extracted regardless of timestamp format"
    );
    // Messages with valid timestamps should have created_at set
    assert!(convs[0].messages[0].created_at.is_some());
    assert!(convs[0].messages[1].created_at.is_some());
    assert!(convs[0].messages[2].created_at.is_some());
}

#[test]
fn workspace_path_encoding_edge_cases() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Test various workspace path formats in session_meta
    let content = concat!(
        // Path with spaces
        "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/home/user/my project/src\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Spaces path\"}}\n",
    );
    fs::write(sessions.join("rollout-spaces.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].workspace,
        Some(PathBuf::from("/home/user/my project/src"))
    );

    // Unicode workspace path
    let content2 = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/home/\u{00FC}ser/projekt\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"Unicode path\"}}\n",
    );
    fs::write(sessions.join("rollout-unicode.jsonl"), content2).unwrap();

    let convs2 = connector.scan(&ctx).unwrap();
    assert!(
        !convs2.is_empty(),
        "unicode workspace paths should be handled"
    );
}

#[test]
fn event_msg_with_unknown_subtypes_skipped() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Various event_msg subtypes that should be gracefully skipped
    let content = concat!(
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"streaming_start\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"streaming_delta\",\"delta\":\"partial\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"streaming_end\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"tool_call\",\"name\":\"bash\",\"input\":{\"cmd\":\"ls\"}}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"tool_result\",\"output\":\"file.txt\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Real user input\"}}\n",
    );
    fs::write(sessions.join("rollout-events.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "unknown event subtypes should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    // user_message + tool_call events should produce messages
    assert_eq!(convs[0].messages.len(), 2);

    // tool_call event should produce an assistant message with invocation
    let tool_msg = &convs[0].messages[0];
    assert_eq!(tool_msg.role, "assistant");
    assert_eq!(tool_msg.invocations.len(), 1);
    assert_eq!(tool_msg.invocations[0].kind, "tool");
    assert_eq!(tool_msg.invocations[0].name, "bash");
    assert!(tool_msg.invocations[0].arguments.is_some());

    // user_message event should still produce a user message
    let user_msg = &convs[0].messages[1];
    assert_eq!(user_msg.content, "Real user input");
    assert_eq!(user_msg.role, "user");
}

#[test]
fn tool_call_format_variations() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // response_item with tool_use content blocks (like Claude API format)
    let content = json!({
        "type": "response_item",
        "payload": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Let me check that."},
                {"type": "tool_use", "name": "read_file", "input": {"path": "/etc/hosts"}}
            ]
        }
    })
    .to_string()
        + "\n"
        + &json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "bash", "input": {"command": "ls -la"}},
                    {"type": "text", "text": "Here are the results."}
                ]
            }
        })
        .to_string()
        + "\n";

    fs::write(sessions.join("rollout-tools.jsonl"), &content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(
        result.is_ok(),
        "tool call format variations should not cause errors"
    );
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    // flatten_content should handle tool_use blocks
    assert!(convs[0].messages[0].content.contains("Let me check"));
    assert!(
        convs[0].messages[1]
            .content
            .contains("Here are the results")
    );
}

// -----------------------------------------------------------------
// Subagent verdict metadata (metadata.is_subagent / metadata.parent_id)
// -----------------------------------------------------------------

/// Scan a synthetic `.codex/sessions` tree holding the given rollout files.
fn scan_codex_rollouts(files: &[(&str, &str)]) -> Vec<NormalizedConversation> {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    for (name, content) in files {
        fs::write(sessions.join(name), content).unwrap();
    }
    let ctx = ScanContext::local_default(codex_dir, None);
    CodexConnector::new().scan(&ctx).unwrap()
}

const SUB_PARENT: &str = "01a1781c-0000-4000-8000-000000000001";

fn user_record(text: &str) -> String {
    format!(
        "{{\"type\":\"response_item\",\"timestamp\":\"2026-11-12T00:00:01Z\",\"payload\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
    )
}

#[test]
fn subagent_parent_thread_id_sets_verdict_and_parent() {
    let meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"parent_thread_id\":\"{SUB_PARENT}\",\"thread_source\":\"subagent\",\"source\":{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"{SUB_PARENT}\",\"depth\":1,\"agent_nickname\":\"Descartes\",\"agent_role\":\"worker\"}}}}}}}}}}\n"
    );
    let content = format!("{meta}{}", user_record("do the thing"));
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-sub.jsonl", &content)]);
    assert_eq!(convs.len(), 1);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(true));
    assert_eq!(m["parent_id"], json!(SUB_PARENT));
    assert_eq!(m["subagent_nickname"], json!("Descartes"));
    assert_eq!(m["subagent_depth"], json!(1));
}

#[test]
fn subagent_parent_from_thread_spawn_when_top_level_absent() {
    let meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"thread_source\":\"subagent\",\"source\":{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"{SUB_PARENT}\"}}}}}}}}}}\n"
    );
    let content = format!("{meta}{}", user_record("x"));
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-spawn.jsonl", &content)]);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(true));
    assert_eq!(m["parent_id"], json!(SUB_PARENT));
}

#[test]
fn subagent_source_string_form_without_parent() {
    let meta = "{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{\"cwd\":\"/p\",\"source\":\"subagent\"}}\n";
    let content = format!("{meta}{}", user_record("x"));
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-src.jsonl", &content)]);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(true));
    assert!(m.get("parent_id").is_none());
}

#[test]
fn session_meta_without_subagent_evidence_is_main() {
    let content = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\"}}}}\n{}",
        user_record("hello")
    );
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-main.jsonl", &content)]);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(false));
    assert!(m.get("parent_id").is_none());
}

#[test]
fn rollout_without_session_meta_has_no_verdict() {
    let convs = scan_codex_rollouts(&[(
        "rollout-2026-11-12T00-00-00-nometa.jsonl",
        &user_record("hi"),
    )]);
    let m = &convs[0].metadata;
    assert!(m.get("is_subagent").is_none());
    assert!(m.get("parent_id").is_none());
}

#[test]
fn fork_is_not_a_subagent() {
    let content = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"forked_from_id\":\"{SUB_PARENT}\",\"history_base\":{{\"thread_id\":\"{SUB_PARENT}\"}}}}}}\n{}",
        user_record("forked")
    );
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-fork.jsonl", &content)]);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(false));
    assert!(m.get("parent_id").is_none());
}

#[test]
fn merged_thread_takes_verdict_from_original_segment() {
    let a = "01a1781c-0000-4000-8000-00000000000a";
    let b = "01a1781c-0000-4000-8000-00000000000b";
    let orig_meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"parent_thread_id\":\"{SUB_PARENT}\",\"thread_source\":\"subagent\"}}}}\n"
    );
    // The continuation header disagrees (claims main); the original wins.
    let cont_meta = "{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:01:00Z\",\"payload\":{\"cwd\":\"/p\"}}\n";
    let original = format!("{orig_meta}{}", user_record("first"));
    let continuation = format!("{cont_meta}{}", user_record("second"));
    let orig_name = format!("rollout-2026-11-12T00-00-00-{a}.jsonl");
    let cont_name = format!("rollout-2026-11-12T00-01-00-{a}_{b}.jsonl");
    let convs = scan_codex_rollouts(&[
        (orig_name.as_str(), original.as_str()),
        (cont_name.as_str(), continuation.as_str()),
    ]);
    assert_eq!(convs.len(), 1, "segments merge into one thread");
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(true));
    assert_eq!(m["parent_id"], json!(SUB_PARENT));
    assert_eq!(m["segment_count"], json!(2));
}

#[test]
fn merged_thread_original_main_overrides_continuation_subagent() {
    let a = "01a1781c-0000-4000-8000-00000000000c";
    let b = "01a1781c-0000-4000-8000-00000000000d";
    let orig_meta = "{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{\"cwd\":\"/p\"}}\n";
    let cont_meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:01:00Z\",\"payload\":{{\"cwd\":\"/p\",\"parent_thread_id\":\"{SUB_PARENT}\",\"thread_source\":\"subagent\"}}}}\n"
    );
    let original = format!("{orig_meta}{}", user_record("first"));
    let continuation = format!("{cont_meta}{}", user_record("second"));
    let orig_name = format!("rollout-2026-11-12T00-00-00-{a}.jsonl");
    let cont_name = format!("rollout-2026-11-12T00-01-00-{a}_{b}.jsonl");
    let convs = scan_codex_rollouts(&[
        (orig_name.as_str(), original.as_str()),
        (cont_name.as_str(), continuation.as_str()),
    ]);
    assert_eq!(convs.len(), 1);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(false));
    assert!(m.get("parent_id").is_none());
}

#[test]
fn orphaned_continuation_main_header_withholds_verdict() {
    let a = "01a1781c-0000-4000-8000-00000000000e";
    let b = "01a1781c-0000-4000-8000-00000000000f";
    // Paginated continuations carry history_base/forked_from_id rather than
    // repeating subagent spawn fields, so an orphaned continuation cannot prove main.
    let cont_meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:01:00Z\",\"payload\":{{\"cwd\":\"/p\",\"forked_from_id\":\"{a}\"}}}}\n"
    );
    let continuation = format!("{cont_meta}{}", user_record("orphaned continuation"));
    let cont_name = format!("rollout-2026-11-12T00-01-00-{a}_{b}.jsonl");
    let convs = scan_codex_rollouts(&[(cont_name.as_str(), continuation.as_str())]);
    assert_eq!(convs.len(), 1);
    assert!(
        convs[0].metadata.get("is_subagent").is_none(),
        "orphaned continuation without subagent fields must withhold verdict"
    );
}

#[test]
fn headerless_original_with_main_continuation_withholds_verdict() {
    let a = "01a1781c-0000-4000-8000-000000000010";
    let b = "01a1781c-0000-4000-8000-000000000011";
    let original = user_record("original without header");
    let cont_meta = "{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:01:00Z\",\"payload\":{\"cwd\":\"/p\"}}\n";
    let continuation = format!("{cont_meta}{}", user_record("continuation"));
    let orig_name = format!("rollout-2026-11-12T00-00-00-{a}.jsonl");
    let cont_name = format!("rollout-2026-11-12T00-01-00-{a}_{b}.jsonl");
    let convs = scan_codex_rollouts(&[
        (orig_name.as_str(), original.as_str()),
        (cont_name.as_str(), continuation.as_str()),
    ]);
    assert_eq!(convs.len(), 1);
    assert!(
        convs[0].metadata.get("is_subagent").is_none(),
        "headerless original must not inherit false from continuation"
    );
}

#[test]
fn empty_parent_thread_id_falls_back_to_parent_id_and_role_fields() {
    let meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"parent_thread_id\":\"\",\"parent_id\":\"{SUB_PARENT}\",\"agent_role\":\"explorer\",\"agent_nickname\":\"Euler\",\"source\":{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"\",\"parent_id\":\"{SUB_PARENT}\",\"agent_role\":\"\",\"agent_nickname\":\"\"}}}}}}}}}}\n"
    );
    let content = format!("{meta}{}", user_record("worker"));
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-empty.jsonl", &content)]);
    let m = &convs[0].metadata;
    assert_eq!(m["is_subagent"], json!(true));
    assert_eq!(m["parent_id"], json!(SUB_PARENT));
    assert_eq!(m["subagent_role"], json!("explorer"));
    assert_eq!(m["subagent_nickname"], json!("Euler"));
}

#[test]
fn non_object_session_meta_payload_is_ignored() {
    let null_then_sub = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":null}}\n{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"parent_thread_id\":\"{SUB_PARENT}\"}}}}\n{}",
        user_record("after null payload")
    );
    let convs =
        scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-nullsub.jsonl", &null_then_sub)]);
    assert_eq!(convs[0].metadata["is_subagent"], json!(true));
    assert_eq!(convs[0].metadata["parent_id"], json!(SUB_PARENT));

    let null_only = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":null}}\n{}",
        user_record("only null payload")
    );
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-nullonly.jsonl", &null_only)]);
    assert!(
        convs[0].metadata.get("is_subagent").is_none(),
        "non-object payload must not prove main"
    );
}

#[test]
fn parent_id_is_canonicalized_to_lowercase_uuid() {
    let upper = "01A1781C-0000-4000-8000-000000000001";
    let meta = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-11-12T00:00:00Z\",\"payload\":{{\"cwd\":\"/p\",\"parent_thread_id\":\"thread-{upper}\"}}}}\n"
    );
    let content = format!("{meta}{}", user_record("upper uuid"));
    let convs = scan_codex_rollouts(&[("rollout-2026-11-12T00-00-00-upper.jsonl", &content)]);
    assert_eq!(convs[0].metadata["is_subagent"], json!(true));
    assert_eq!(convs[0].metadata["parent_id"], json!(SUB_PARENT));
}

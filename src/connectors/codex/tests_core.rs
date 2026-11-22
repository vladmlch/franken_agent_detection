use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::CodexConnector;
use super::discovery::FileScanMetadata;
use crate::connectors::Connector;
use crate::connectors::scan::{DiscoveredSourceRole, ScanContext, ScanRoot};

// =====================================================
// Constructor Tests
// =====================================================

#[test]
fn new_creates_connector() {
    let connector = CodexConnector::new();
    // Just verify it doesn't panic - struct has no fields
    let _ = connector;
}

#[test]
fn default_creates_connector() {
    let connector = CodexConnector;
    let _ = connector;
}

// =====================================================
// home() Tests
// =====================================================

#[test]
fn home_returns_path_ending_with_codex() {
    // Note: We can't reliably test CODEX_HOME env var due to parallel test execution.
    // Testing that home() returns a valid path structure is sufficient.
    // The function uses CODEX_HOME if set, otherwise defaults to ~/.codex
    let home = CodexConnector::home();
    // Either the env var is set (ends with some path) or default (ends with .codex)
    let path_str = home.to_str().unwrap();
    let has_codex_dir = home
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case(".codex") || name.eq_ignore_ascii_case("codex")
        });
    assert!(
        has_codex_dir || path_str.to_ascii_lowercase().contains("codex"),
        "home() should return a path related to codex, got: {}",
        path_str
    );
}

// =====================================================
// rollout_files() Tests
// =====================================================

#[test]
fn rollout_files_finds_jsonl_files() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let rollout = sessions.join("rollout-abc123.jsonl");
    fs::write(&rollout, "{}").unwrap();

    let files = CodexConnector::rollout_files(dir.path());
    assert_eq!(files.len(), 1);
    assert!(files[0].to_str().unwrap().contains("rollout-abc123.jsonl"));
}

#[test]
fn rollout_files_finds_json_files() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let rollout = sessions.join("rollout-legacy.json");
    fs::write(&rollout, "{}").unwrap();

    let files = CodexConnector::rollout_files(dir.path());
    assert_eq!(files.len(), 1);
    assert!(files[0].to_str().unwrap().contains("rollout-legacy.json"));
}

#[test]
fn rollout_files_ignores_non_rollout_files() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Create various non-rollout files
    fs::write(sessions.join("config.json"), "{}").unwrap();
    fs::write(sessions.join("session.jsonl"), "{}").unwrap();
    fs::write(sessions.join("other.txt"), "test").unwrap();

    let files = CodexConnector::rollout_files(dir.path());
    assert_eq!(files.len(), 0);
}

#[test]
fn scan_with_explicit_home_root_finds_codex_sessions() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let sessions = home.join(".codex").join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Hello Codex"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Hi there!"}}
"#;
    let rollout = sessions.join("rollout-home.jsonl");
    fs::write(&rollout, content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(dir.path().join("cass"), vec![ScanRoot::local(home)], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].content, "Hello Codex");
    assert_eq!(convs[0].messages[1].content, "Hi there!");
}

#[test]
fn title_skips_injected_context_user_records() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let sessions = home.join(".codex").join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r##"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"# AGENTS.md instructions for /data/projects/demo\nBe helpful."}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Fix the flaky test"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"assistant","content":"On it."}}
"##;
    fs::write(sessions.join("rollout-title.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(dir.path().join("cass"), vec![ScanRoot::local(home)], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Fix the flaky test"));
}

#[test]
fn dual_stream_user_prompts_are_deduplicated() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let sessions = home.join(".codex").join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Dual-stream era: the same prompt arrives via BOTH streams.
    let content = r#"{"type":"event_msg","timestamp":"2025-12-01T10:00:00Z","payload":{"type":"user_message","message":"First read ALL of the AGENTS.md file"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"First read ALL of the AGENTS.md file"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"assistant","content":"Reading it now."}}
"#;
    fs::write(sessions.join("rollout-dual.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(dir.path().join("cass"), vec![ScanRoot::local(home)], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let user_texts: Vec<&str> = convs[0]
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        user_texts,
        vec!["First read ALL of the AGENTS.md file"],
        "the event_msg copy of a response_item user prompt must be suppressed"
    );
}

#[test]
fn token_count_reads_nested_last_token_usage() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let sessions = home.join(".codex").join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Real payload shape (sampled live 2026-06): usage nested under
    // info.last_token_usage, NOT flat on the payload.
    let usage_line = r#"{"timestamp":"2026-06-14T02:46:39.992Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":42187,"cached_input_tokens":5504,"output_tokens":733,"reasoning_output_tokens":516,"total_tokens":42920},"last_token_usage":{"input_tokens":42187,"cached_input_tokens":5504,"output_tokens":733,"reasoning_output_tokens":516,"total_tokens":42920}}}}"#;
    let content = format!(
        "{}\n{}\n{}\n{}\n",
        r#"{"type":"response_item","timestamp":"2026-06-14T02:45:00.000Z","payload":{"role":"assistant","content":"Working on it."}}"#,
        usage_line,
        r#"{"type":"response_item","timestamp":"2026-06-14T02:47:00.000Z","payload":{"role":"assistant","content":"Done."}}"#,
        // Local fix (cass review 2026-11-02, F6): a distinct request always
        // carries a larger cumulative `total_tokens`; an identical total is
        // a re-notification that the scan now skips.
        usage_line
            .replace("\"input_tokens\":42187", "\"input_tokens\":500")
            .replace("\"total_tokens\":42920", "\"total_tokens\":43420")
    );
    fs::write(sessions.join("rollout-tokens.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(dir.path().join("cass"), vec![ScanRoot::local(home)], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let with_usage: Vec<&Value> = convs[0]
        .messages
        .iter()
        .filter(|m| m.extra.pointer("/cass/token_usage/input_tokens").is_some())
        .map(|m| &m.extra)
        .collect();
    assert_eq!(with_usage.len(), 2, "each turn carries its own usage");
    let first = with_usage[0]
        .pointer("/cass/token_usage/input_tokens")
        .and_then(Value::as_i64);
    let second = with_usage[1]
        .pointer("/cass/token_usage/input_tokens")
        .and_then(Value::as_i64);
    assert_eq!(first, Some(42_187));
    assert_eq!(second, Some(500));
}

#[test]
fn rollout_files_finds_nested_rollouts() {
    let dir = TempDir::new().unwrap();
    let nested = dir
        .path()
        .join("sessions")
        .join("2025")
        .join("12")
        .join("17");
    fs::create_dir_all(&nested).unwrap();

    let rollout = nested.join("rollout-nested.jsonl");
    fs::write(&rollout, "{}").unwrap();

    let files = CodexConnector::rollout_files(dir.path());
    assert_eq!(files.len(), 1);
    assert!(files[0].to_str().unwrap().contains("rollout-nested.jsonl"));
}

#[test]
fn rollout_files_returns_sorted_order() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("rollout-z.jsonl"), "{}").unwrap();
    fs::write(sessions.join("rollout-a.jsonl"), "{}").unwrap();

    let files = CodexConnector::rollout_files(dir.path());
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
    assert_eq!(names, vec!["rollout-a.jsonl", "rollout-z.jsonl"]);
}

#[test]
fn rollout_files_returns_empty_when_no_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let files = CodexConnector::rollout_files(dir.path());
    assert_eq!(files.len(), 0);
}

// =====================================================
// scan() JSONL Format Tests
// =====================================================

#[test]
fn scan_parses_jsonl_response_item_messages() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Hello Codex"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Hello! How can I help?"}}
"#;
    fs::write(sessions.join("rollout-test.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Hello Codex");
    assert_eq!(convs[0].messages[1].role, "assistant");
}

#[test]
fn scan_parses_modern_response_item_output_text_and_tool_calls() {
    // Regression test for #13: modern Codex rollout files encode assistant
    // text as `output_text` content blocks and encode tool calls/results as
    // `response_item` payloads (`function_call`, `function_call_output`,
    // `custom_tool_call`, `custom_tool_call_output`). None of these were
    // captured before, silently dropping assistant output and tool activity.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("codex");

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(fixture.clone(), vec![ScanRoot::local(fixture)], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1, "fixture is a single rollout");
    let conv = &convs[0];
    assert_eq!(conv.workspace, Some(PathBuf::from("/tmp/demo-project")));

    // user, assistant output_text, function_call, function_call_output,
    // custom_tool_call, custom_tool_call_output = 6 messages. The encrypted
    // reasoning item carries no plaintext content and is skipped.
    assert_eq!(
        conv.messages.len(),
        6,
        "all modern shapes captured (encrypted reasoning skipped): {:#?}",
        conv.messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );

    // Assistant `output_text` is no longer dropped.
    let assistant = conv
        .messages
        .iter()
        .find(|m| {
            m.role == "assistant" && m.content == "I will inspect the files and then apply a patch."
        })
        .expect("assistant output_text message captured");
    assert!(assistant.invocations.is_empty());

    // `function_call` -> tool invocation with JSON-string arguments parsed.
    let exec = conv
        .messages
        .iter()
        .find(|m| m.invocations.iter().any(|i| i.name == "exec_command"))
        .expect("exec_command function_call captured");
    let exec_inv = &exec.invocations[0];
    assert_eq!(exec_inv.kind, "tool");
    assert_eq!(exec_inv.call_id.as_deref(), Some("call_1"));
    assert_eq!(
        exec_inv
            .arguments
            .as_ref()
            .and_then(|a| a.get("cmd"))
            .and_then(|v| v.as_str()),
        Some("ls"),
        "JSON-string arguments are decoded into structured JSON"
    );

    // `function_call_output` -> tool result, linkable via call_id in extra.
    let exec_out = conv
        .messages
        .iter()
        .find(|m| m.role == "tool" && m.content.contains("README.md"))
        .expect("function_call_output captured as tool result");
    assert_eq!(
        exec_out
            .extra
            .pointer("/payload/call_id")
            .and_then(|v| v.as_str()),
        Some("call_1"),
        "tool result remains linkable to its originating call"
    );

    // `custom_tool_call` (apply_patch) -> tool invocation; freeform input kept.
    let patch = conv
        .messages
        .iter()
        .find(|m| m.invocations.iter().any(|i| i.name == "apply_patch"))
        .expect("apply_patch custom_tool_call captured");
    let patch_inv = &patch.invocations[0];
    assert_eq!(patch_inv.call_id.as_deref(), Some("call_2"));
    assert!(
        patch_inv
            .arguments
            .as_ref()
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("Begin Patch")),
        "non-JSON tool input is retained as a raw string"
    );

    // `custom_tool_call_output` -> tool result message.
    assert!(
        conv.messages
            .iter()
            .any(|m| m.role == "tool" && m.content.contains("A hello.txt")),
        "custom_tool_call_output captured as tool result"
    );
}

#[test]
fn scan_with_callback_matches_scan_for_jsonl_rollout() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Hello Codex"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Hello! How can I help?"}}
"#;
    fs::write(sessions.join("rollout-stream.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let scanned = connector.scan(&ctx).unwrap();
    let mut streamed = Vec::new();
    connector
        .scan_with_callback(&ctx, &mut |conversation| {
            streamed.push(conversation);
            Ok(())
        })
        .unwrap();

    assert_eq!(streamed.len(), scanned.len());
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
fn discover_source_files_matches_scanned_rollout_sources() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Hello Codex"}}"#;
    let rollout = sessions.join("rollout-discovery.jsonl");
    fs::write(&rollout, content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir, None);
    let discovered = connector.discover_source_files(&ctx).unwrap();
    let scanned = connector.scan(&ctx).unwrap();

    assert_eq!(scanned.len(), 1);
    assert_eq!(
        discovered
            .iter()
            .map(|source| source.source_path.clone())
            .collect::<Vec<_>>(),
        vec![scanned[0].source_path.clone()]
    );
    assert_eq!(discovered[0].role, DiscoveredSourceRole::PrimarySessionLog);
    assert!(discovered[0].required_for_reconstruction);
    assert_eq!(discovered[0].provider_slug, "codex");
    assert_eq!(discovered[0].source_path, rollout);
}

#[test]
fn scan_with_explicit_rollout_file_only_reads_that_file_and_keeps_relative_external_id() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir
        .join("sessions")
        .join("2025")
        .join("12")
        .join("18");
    fs::create_dir_all(&sessions).unwrap();

    let first = sessions.join("rollout-one.jsonl");
    let second = sessions.join("rollout-two.jsonl");
    fs::write(
        &first,
        r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"first only"}}"#,
    )
    .unwrap();
    fs::write(
        &second,
        r#"{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"second only"}}"#,
    )
    .unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(first.clone(), vec![ScanRoot::local(first.clone())], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "first only");
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some("2025/12/18/rollout-one")
    );
    assert_eq!(convs[0].source_path, first);
}

#[test]
fn scan_emits_textless_image_tool_output_with_identity_as_empty_tool_message() {
    // F13: `function_call_output` records whose captured output carries
    // no text (image-only results: `output` is a list of `image_url`
    // parts) used to be dropped entirely, leaving their calls unpaired
    // downstream. The record is local evidence: emit it as a `tool`
    // message with EMPTY content and the raw record as `extra` — never
    // a fabricated result text — so call/result linking can pair it.
    // Records WITHOUT identity (no call_id/id) stay dropped: there is
    // nothing to link them to.
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = concat!(
        r#"{"type":"event_msg","timestamp":"2025-12-01T10:00:00Z","payload":{"type":"user_message","message":"inspect the made-up screenshot"}}"#,
        "\n",
        r#"{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"type":"function_call","call_id":"call_img","name":"view_image","arguments":"{\"path\":\"/made-up.png\"}","id":"resp-1"}}"#,
        "\n",
        r#"{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"function_call_output","call_id":"call_img","id":"resp-1","output":[{"type":"image_url","image_url":{"url":"data:image/png;base64,MADEUP"}}]}}"#,
        "\n",
        r#"{"type":"response_item","timestamp":"2025-12-01T10:00:03Z","payload":{"type":"function_call_output","output":""}}"#,
        "\n",
    );
    let rollout = sessions.join("rollout-images.jsonl");
    fs::write(&rollout, content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(rollout.clone(), vec![ScanRoot::local(rollout)], None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    // user prompt + tool call + the textless image result = 3 messages;
    // the identity-less empty output record stays dropped.
    assert_eq!(
        conv.messages.len(),
        3,
        "textless identity-bearing outputs emit; identity-less ones drop: {:#?}",
        conv.messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );
    let image_result = conv
        .messages
        .iter()
        .find(|m| m.role == "tool" && m.content.is_empty())
        .expect("image-only output emitted as a textless tool message");
    assert_eq!(
        image_result
            .extra
            .pointer("/payload/call_id")
            .and_then(|v| v.as_str()),
        Some("call_img"),
        "the raw record stays linkable via extra.payload.call_id"
    );
    assert_eq!(
        image_result
            .extra
            .pointer("/payload/type")
            .and_then(|v| v.as_str()),
        Some("function_call_output"),
        "the raw record is preserved verbatim as extra"
    );
}

#[test]
#[ignore = "release-mode performance harness; run explicitly for Codex scan wall-clock evidence"]
fn perf_scan_large_codex_fixture() {
    let file_count = std::env::var("FAD_CODEX_SCAN_BENCH_FILES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2_000);
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");

    for i in 0..file_count {
        let day_dir = sessions
            .join("2026")
            .join("05")
            .join(format!("{:02}", (i % 28) + 1));
        fs::create_dir_all(&day_dir).unwrap();
        let ts = 1_746_265_600_000_u64 + u64::try_from(i).unwrap();
        fs::write(
            day_dir.join(format!("rollout-{i:06}.jsonl")),
            format!(
                r#"{{"type":"event_msg","timestamp":{ts},"payload":{{"type":"user_message","message":"bench user {i}"}}}}
{{"type":"response_item","timestamp":{},"payload":{{"role":"assistant","content":"bench assistant {i}"}}}}
"#,
                ts + 1
            ),
        )
        .unwrap();
    }

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir, None);
    let start = Instant::now();
    let convs = connector.scan(&ctx).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(convs.len(), file_count);
    eprintln!(
        "fad_codex_scan_large_fixture files={} elapsed_ms={} ns_per_file={}",
        file_count,
        elapsed.as_millis(),
        elapsed.as_nanos() / u128::try_from(file_count).unwrap()
    );
}

#[test]
fn file_metadata_if_modified_preserves_scan_fallbacks() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("rollout-meta.jsonl");
    fs::write(&file, "{}").unwrap();

    assert!(matches!(
        CodexConnector::file_metadata_if_modified(&file, None),
        FileScanMetadata::Process
    ));
    assert!(
        matches!(
            CodexConnector::file_metadata_if_modified(&file, Some(i64::MAX)),
            FileScanMetadata::Skip
        ),
        "future since_ts should skip older files"
    );

    let missing = dir.path().join("missing.jsonl");
    assert!(matches!(
        CodexConnector::file_metadata_if_modified(&missing, Some(i64::MAX)),
        FileScanMetadata::Process
    ));
}

#[test]
fn scan_parses_event_msg_user_message() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"event_msg","timestamp":"2025-12-01T10:00:00Z","payload":{"type":"user_message","message":"User typed this"}}
"#;
    fs::write(sessions.join("rollout-user.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "User typed this");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

#[test]
fn scan_parses_event_msg_agent_reasoning() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"event_msg","timestamp":"2025-12-01T10:00:00Z","payload":{"type":"agent_reasoning","text":"Let me think about this..."}}
"#;
    fs::write(sessions.join("rollout-reasoning.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "assistant");
    assert_eq!(convs[0].messages[0].author, Some("reasoning".to_string()));
    assert_eq!(convs[0].messages[0].content, "Let me think about this...");
    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
}

#[test]
fn scan_extracts_workspace_from_session_meta() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"session_meta","timestamp":"2025-12-01T10:00:00Z","payload":{"cwd":"/home/user/project"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Test"}}
"#;
    fs::write(sessions.join("rollout-meta.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].workspace,
        Some(PathBuf::from("/home/user/project"))
    );
}

#[test]
fn scan_skips_empty_lines_in_jsonl() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Message 1"}}

{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Message 2"}}
"#;
    fs::write(sessions.join("rollout-empty-lines.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
}

#[test]
fn scan_skips_invalid_json_lines() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Valid"}}
not valid json at all
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"user","content":"Also valid"}}
"#;
    fs::write(sessions.join("rollout-invalid.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
}

#[test]
fn scan_skips_empty_content_messages() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Has content"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":""}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"assistant","content":"   "}}
"#;
    fs::write(sessions.join("rollout-empty-content.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    // Only the message with actual content should be included
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Has content");
}

#[test]
fn scan_skips_unknown_event_types() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Real message"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:01Z","payload":{"type":"token_count","tokens":100}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"turn_aborted"}}
{"type":"turn_context","timestamp":"2025-12-01T10:00:03Z","payload":{}}
"#;
    fs::write(sessions.join("rollout-unknown.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    // Only the response_item should be included
    assert_eq!(convs[0].messages.len(), 1);
}

#[test]
fn scan_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"First"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Second"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"user","content":"Third"}}
"#;
    fs::write(sessions.join("rollout-idx.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].idx, 0);
    assert_eq!(convs[0].messages[1].idx, 1);
    assert_eq!(convs[0].messages[2].idx, 2);
}

#[test]
fn scan_attaches_token_count_to_nearest_preceding_assistant() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"First answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"token_count","input_tokens":10,"output_tokens":20}}
{"type":"response_item","timestamp":"2025-12-01T10:00:03Z","payload":{"role":"assistant","content":"Second answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:04Z","payload":{"type":"token_count","input_tokens":30,"output_tokens":40}}
"#;
    fs::write(sessions.join("rollout-attach-nearest.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        3,
        "no synthetic token_count messages"
    );

    let first = &convs[0].messages[1];
    assert_eq!(first.content, "First answer");
    assert_eq!(
        first
            .extra
            .pointer("/cass/token_usage/input_tokens")
            .and_then(Value::as_i64),
        Some(10)
    );
    assert_eq!(
        first
            .extra
            .pointer("/cass/token_usage/output_tokens")
            .and_then(Value::as_i64),
        Some(20)
    );

    let second = &convs[0].messages[2];
    assert_eq!(second.content, "Second answer");
    assert_eq!(
        second
            .extra
            .pointer("/cass/token_usage/input_tokens")
            .and_then(Value::as_i64),
        Some(30)
    );
    assert_eq!(
        second
            .extra
            .pointer("/cass/token_usage/output_tokens")
            .and_then(Value::as_i64),
        Some(40)
    );
    assert_eq!(
        second
            .extra
            .pointer("/cass/token_usage/data_source")
            .and_then(|v| v.as_str()),
        Some("api")
    );
}

#[test]
fn scan_ignores_token_count_without_preceding_assistant() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:01Z","payload":{"type":"token_count","input_tokens":11,"output_tokens":22}}
{"type":"response_item","timestamp":"2025-12-01T10:00:02Z","payload":{"role":"assistant","content":"Answer later"}}
"#;
    fs::write(sessions.join("rollout-unmatched-token.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    assert!(
        convs[0].messages[1]
            .extra
            .pointer("/cass/token_usage")
            .is_none(),
        "token_count before first assistant must not attach to future message"
    );
}

#[test]
fn scan_multiple_token_count_for_one_assistant_prefers_last() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"token_count","input_tokens":5,"output_tokens":10}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:03Z","payload":{"type":"token_count","input_tokens":7,"output_tokens":14}}
"#;
    fs::write(sessions.join("rollout-token-last-wins.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2);
    let assistant = &convs[0].messages[1];
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/input_tokens")
            .and_then(Value::as_i64),
        Some(7)
    );
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/output_tokens")
            .and_then(Value::as_i64),
        Some(14)
    );
}

// cass review 2026-11-02 F6: tool-loop requests without assistant text in between.
#[test]
fn scan_sums_distinct_request_token_counts_and_skips_repeats() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"Question"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:01Z","payload":{"role":"assistant","content":"Answer"}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":5,"output_tokens":10},"total_token_usage":{"total_tokens":15}}}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:03Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":5,"output_tokens":10},"total_token_usage":{"total_tokens":15}}}}
{"type":"event_msg","timestamp":"2025-12-01T10:00:04Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":14},"total_token_usage":{"total_tokens":36}}}}
"#;
    fs::write(sessions.join("rollout-token-sum.jsonl"), content).unwrap();
    let convs = CodexConnector::new()
        .scan(&ScanContext::local_default(codex_dir.clone(), None))
        .unwrap();
    let usage = convs[0].messages[1]
        .extra
        .pointer("/cass/token_usage")
        .unwrap();
    assert_eq!(usage["input_tokens"].as_i64(), Some(12));
    assert_eq!(usage["output_tokens"].as_i64(), Some(24));
}

// =====================================================
// scan() Legacy JSON Format Tests
// =====================================================

#[test]
fn scan_parses_legacy_json_format() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = json!({
        "session": {"cwd": "/home/user/legacy"},
        "items": [
            {"role": "user", "content": "Legacy user message", "timestamp": "2025-12-01T10:00:00Z"},
            {"role": "assistant", "content": "Legacy assistant response", "timestamp": "2025-12-01T10:00:01Z"}
        ]
    });
    fs::write(sessions.join("rollout-legacy.json"), content.to_string()).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/home/user/legacy")));
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Legacy user message");
    assert_eq!(convs[0].messages[1].role, "assistant");
}

#[test]
fn scan_legacy_json_skips_empty_content() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = json!({
        "session": {},
        "items": [
            {"role": "user", "content": "Has content"},
            {"role": "assistant", "content": ""},
            {"role": "assistant", "content": "   "}
        ]
    });
    fs::write(
        sessions.join("rollout-empty-legacy.json"),
        content.to_string(),
    )
    .unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
}

#[test]
fn scan_legacy_json_handles_missing_items() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = json!({"session": {}});
    fs::write(sessions.join("rollout-no-items.json"), content.to_string()).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // No messages = conversation is skipped
    assert_eq!(convs.len(), 0);
}

#[test]
fn scan_skips_invalid_legacy_json() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    fs::write(sessions.join("rollout-bad.json"), "not valid json").unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 0);
}

// =====================================================
// Title Extraction Tests
// =====================================================

#[test]
fn scan_extracts_title_from_first_user_message() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","payload":{"role":"assistant","content":"I'm an assistant"}}
{"type":"response_item","payload":{"role":"user","content":"This should be the title"}}
"#;
    fs::write(sessions.join("rollout-title.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title, Some("This should be the title".to_string()));
}

#[test]
fn scan_truncates_long_titles() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let long_title = "x".repeat(200);
    let content = format!(
        r#"{{"type":"response_item","payload":{{"role":"user","content":"{}"}}}}"#,
        long_title
    );
    fs::write(sessions.join("rollout-long.jsonl"), content + "\n").unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title.as_ref().unwrap().len(), 100);
}

#[test]
fn scan_uses_first_line_for_multiline_title() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","payload":{"role":"user","content":"First line\nSecond line\nThird line"}}
"#;
    fs::write(sessions.join("rollout-multiline.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title, Some("First line".to_string()));
}

#[test]
fn scan_falls_back_to_first_message_for_title() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // No user messages, only assistant
    let content = r#"{"type":"response_item","payload":{"role":"assistant","content":"Assistant speaks first"}}
"#;
    fs::write(sessions.join("rollout-assistant-only.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].title, Some("Assistant speaks first".to_string()));
}

// =====================================================
// External ID Tests
// =====================================================

#[test]
fn scan_uses_relative_path_as_external_id() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir
        .join("sessions")
        .join("2025")
        .join("12")
        .join("17");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
    fs::write(sessions.join("rollout-nested-id.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // External ID should be the relative path from sessions dir
    assert!(convs[0].external_id.is_some());
    let ext_id = convs[0].external_id.as_ref().unwrap();
    assert!(ext_id.contains("2025") || ext_id.contains("rollout-nested-id"));
}

// =====================================================
// Metadata Tests
// =====================================================

#[test]
fn scan_sets_metadata_source_for_jsonl() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
    fs::write(sessions.join("rollout-meta-jsonl.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].metadata["source"], "rollout");
}

#[test]
fn scan_sets_metadata_source_for_json() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = json!({
        "session": {},
        "items": [{"role": "user", "content": "Test"}]
    });
    fs::write(sessions.join("rollout-meta-json.json"), content.to_string()).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].metadata["source"], "rollout_json");
}

// =====================================================
// Agent Slug Tests
// =====================================================

#[test]
fn scan_sets_agent_slug_to_codex() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","payload":{"role":"user","content":"Test"}}
"#;
    fs::write(sessions.join("rollout-slug.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].agent_slug, "codex");
}

// =====================================================
// Timestamp Tests
// =====================================================

#[test]
fn scan_parses_timestamps() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"First"}}
{"type":"response_item","timestamp":"2025-12-01T11:00:00Z","payload":{"role":"user","content":"Last"}}
"#;
    fs::write(sessions.join("rollout-ts.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].started_at.is_some());
    assert!(convs[0].ended_at.is_some());
    assert!(convs[0].messages[0].created_at.is_some());
}

#[test]
fn scan_tracks_timestamp_bounds_for_out_of_order_events() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type":"response_item","timestamp":"2025-12-01T11:00:00Z","payload":{"role":"assistant","content":"Second chronologically"}}
{"type":"response_item","timestamp":"2025-12-01T10:00:00Z","payload":{"role":"user","content":"First chronologically"}}
"#;
    fs::write(sessions.join("rollout-out-of-order.jsonl"), content).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    let expected_start = conv.messages.iter().filter_map(|m| m.created_at).min();
    let expected_end = conv.messages.iter().filter_map(|m| m.created_at).max();

    assert_eq!(conv.started_at, expected_start);
    assert_eq!(conv.ended_at, expected_end);
    assert!(conv.ended_at >= conv.started_at);
}

// =====================================================
// Edge Cases
// =====================================================

#[test]
fn scan_handles_empty_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    // No files in sessions directory

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 0);
}

#[test]
fn scan_handles_multiple_rollout_files() {
    let dir = TempDir::new().unwrap();
    let codex_dir = dir.path().join(".codex");
    let sessions = codex_dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let content1 = r#"{"type":"response_item","payload":{"role":"user","content":"Session 1"}}
"#;
    let content2 = r#"{"type":"response_item","payload":{"role":"user","content":"Session 2"}}
"#;
    fs::write(sessions.join("rollout-1.jsonl"), content1).unwrap();
    fs::write(sessions.join("rollout-2.jsonl"), content2).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext::local_default(codex_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 2);
}

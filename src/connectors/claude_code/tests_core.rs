use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::ClaudeCodeConnector;
use super::scan::scan_claude_with_callback_with_exclusions;
use crate::connectors::scan::{
    DiscoveredSourceFile, ScanContext, ScanRoot, SourceCompletion, SourceScanHooks,
};
use crate::connectors::{Connector, parse_timestamp};

/// Create a test-ready Claude directory structure.
/// Includes a `projects` marker subdir so `looks_like_root()` returns true
/// and the connector scans only the temp dir instead of the real ~/.claude/projects.
pub(super) fn make_test_claude_dir(base: &std::path::Path) -> PathBuf {
    let claude_dir = base.join(".claude");
    fs::create_dir_all(claude_dir.join("projects")).unwrap();
    claude_dir
}

// =========================================================================
// Constructor tests
// =========================================================================

#[test]
fn new_creates_connector() {
    let connector = ClaudeCodeConnector::new();
    let _ = connector;
}

#[test]
fn scan_marks_subagent_transcripts_with_parent_link() {
    let base = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(base.path());
    let session_dir = claude_dir
        .join("projects")
        .join("-data-projects-demo")
        .join("11111111-2222-3333-4444-555555555555");
    let subagents = session_dir
        .join("subagents")
        .join("66666666-7777-8888-9999-000000000000");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(
        subagents.join("agent-abc123.jsonl"),
        concat!(
            r#"{"type":"assistant","sessionId":"66666666-7777-8888-9999-000000000000","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"subagent work"}]}}"#,
            "\n",
        ),
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].metadata["sidechain"], true);
    assert_eq!(
        convs[0].metadata["parentSessionId"],
        "11111111-2222-3333-4444-555555555555"
    );
}

#[test]
fn scan_prefers_ai_title_over_first_line_truncation() {
    let base = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(base.path());
    let session_dir = claude_dir
        .join("projects")
        .join("-data-projects-demo")
        .join("aaaa-bbbb");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("cccc-dddd.jsonl"),
        concat!(
            r#"{"type":"ai-title","title":"Fix the flaky auth test"}"#,
            "\n",
            r#"{"type":"user","sessionId":"cccc-dddd","message":{"role":"user","content":[{"type":"text","text":"please look at the auth module and fix the flaky test"}]}}"#,
            "\n",
        ),
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Fix the flaky auth test"));
}

// cass review 2026-11-02 F1
#[test]
fn scan_prefers_real_ai_title_field() {
    let base = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(base.path());
    let session_dir = claude_dir
        .join("projects")
        .join("-data-projects-demo")
        .join("aaaa-bbbb");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("cccc-dddd.jsonl"),
        concat!(
            r#"{"type":"ai-title","aiTitle":"Fix the flaky auth test","sessionId":"cccc-dddd"}"#,
            "\n",
            r#"{"type":"user","sessionId":"cccc-dddd","message":{"role":"user","content":[{"type":"text","text":"please look at the auth module and fix the flaky test"}]}}"#,
            "\n",
        ),
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir, None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Fix the flaky auth test"));
}

// cass review 2026-11-02 F2
#[test]
fn scan_keeps_thinking_only_assistant_records() {
    let base = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(base.path());
    let session_dir = claude_dir.join("projects").join("-data-projects-demo");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("eeee-ffff.jsonl"),
        concat!(
            r#"{"type":"user","sessionId":"eeee-ffff","message":{"role":"user","content":"why?"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"eeee-ffff","requestId":"r1","message":{"id":"m1","role":"assistant","model":"claude-x","content":[{"type":"thinking","thinking":"migration order matters"}],"usage":{"input_tokens":10,"output_tokens":7}}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"eeee-ffff","requestId":"r1","message":{"id":"m1","role":"assistant","model":"claude-x","content":[{"type":"text","text":"answer"}],"usage":{"input_tokens":10,"output_tokens":7}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let convs = ClaudeCodeConnector::new()
        .scan(&ScanContext::local_default(claude_dir, None))
        .unwrap();
    assert_eq!(convs.len(), 1);
    let contents: Vec<&str> = convs[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        contents,
        ["why?", "[Thinking]\nmigration order matters", "answer"]
    );
    assert_eq!(convs[0].messages[1].extra["requestId"], "r1");
}

#[test]
fn default_creates_connector() {
    let connector = ClaudeCodeConnector;
    let _ = connector;
}

#[test]
fn projects_root_returns_claude_projects_path() {
    // Smoke test on the env-aware entry point. We can't reliably mutate
    // env vars in parallel tests (std::env::set_var is unsafe + forbid'd),
    // so this test only asserts the invariant that holds across all three
    // resolver branches: the path's last component is "projects". The
    // *precedence* logic is exercised by `projects_root_resolved_*` below
    // against explicit inputs, where we don't depend on process env at all.
    let root = ClaudeCodeConnector::projects_root();
    assert_eq!(
        root.file_name().and_then(|n| n.to_str()),
        Some("projects"),
        "projects_root() should always return a path ending in 'projects', got: {}",
        root.display()
    );
}

#[test]
fn projects_root_resolved_uses_explicit_override_first() {
    let explicit = std::path::Path::new("/opt/claude_explicit");
    let xdg = std::path::Path::new("/opt/xdg");
    let home = std::path::Path::new("/opt/home");
    let root = ClaudeCodeConnector::projects_root_resolved(Some(explicit), Some(xdg), Some(home));
    assert_eq!(root, PathBuf::from("/opt/claude_explicit/projects"));
}

#[test]
fn projects_root_resolved_falls_back_to_xdg_when_no_explicit_override() {
    let xdg = std::path::Path::new("/opt/xdg");
    let home = std::path::Path::new("/opt/home");
    let root = ClaudeCodeConnector::projects_root_resolved(None, Some(xdg), Some(home));
    // XDG layout is `${XDG_CONFIG_HOME}/claude-code/projects` (no leading
    // dot), distinct from the `.claude/projects` home-relative default —
    // matches what the Claude Code CLI itself does when XDG is set, and
    // what caam writes per-profile.
    assert_eq!(root, PathBuf::from("/opt/xdg/claude-code/projects"));
}

#[test]
fn projects_root_resolved_uses_home_when_no_overrides() {
    let home = std::path::Path::new("/home/jane");
    let root = ClaudeCodeConnector::projects_root_resolved(None, None, Some(home));
    assert_eq!(root, PathBuf::from("/home/jane/.claude/projects"));
}

#[test]
fn projects_root_resolved_falls_back_to_relative_when_home_missing() {
    // Sandboxed test environments occasionally have no HOME — historical
    // behavior was to return a relative `.claude/projects`, preserved here.
    let root = ClaudeCodeConnector::projects_root_resolved(None, None, None);
    assert_eq!(root, PathBuf::from(".claude/projects"));
}

#[test]
fn projects_root_resolved_treats_explicit_as_higher_priority_than_xdg() {
    // Regression guard: if the precedence is ever swapped (XDG before
    // CLAUDE_CONFIG_DIR), caam-style multi-account isolation that sets
    // both vars to *different* values would silently route to the wrong
    // profile. Pin the precedence here so a refactor can't quietly undo it.
    let explicit = std::path::Path::new("/opt/account_A");
    let xdg = std::path::Path::new("/opt/account_B/xdg_config");
    let root = ClaudeCodeConnector::projects_root_resolved(Some(explicit), Some(xdg), None);
    assert_eq!(root, PathBuf::from("/opt/account_A/projects"));
    // Verify it did NOT pick the XDG path.
    assert_ne!(
        root,
        PathBuf::from("/opt/account_B/xdg_config/claude-code/projects")
    );
}

#[test]
fn projects_root_candidates_keep_home_fallback_when_xdg_is_set() {
    let xdg = std::path::Path::new("/opt/xdg");
    let home = std::path::Path::new("/home/jane");
    let roots = ClaudeCodeConnector::projects_root_candidates_resolved(None, Some(xdg), Some(home));

    assert!(roots.contains(&PathBuf::from("/opt/xdg/claude-code/projects")));
    assert!(roots.contains(&PathBuf::from("/home/jane/.claude/projects")));
}

#[test]
fn projects_root_candidates_include_macos_desktop_sidecar_roots() {
    let home = std::path::Path::new("/Users/jane");
    let roots = ClaudeCodeConnector::projects_root_candidates_resolved(None, None, Some(home));

    assert!(roots.contains(&PathBuf::from(
        "/Users/jane/Library/Application Support/Claude/claude-code-sessions"
    )));
    assert!(roots.contains(&PathBuf::from(
        "/Users/jane/Library/Application Support/Claude/local-agent-mode-sessions"
    )));
}

#[test]
fn projects_root_candidates_explicit_override_does_not_scan_fallbacks() {
    let explicit = std::path::Path::new("/opt/claude_explicit");
    let xdg = std::path::Path::new("/opt/xdg");
    let home = std::path::Path::new("/home/jane");
    let roots = ClaudeCodeConnector::projects_root_candidates_resolved(
        Some(explicit),
        Some(xdg),
        Some(home),
    );

    assert_eq!(roots, vec![PathBuf::from("/opt/claude_explicit/projects")]);
}

#[test]
fn session_files_returns_sorted_order() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("b.jsonl"), "{}").unwrap();
    fs::write(dir.path().join("a.jsonl"), "{}").unwrap();
    fs::write(dir.path().join("ignore.txt"), "x").unwrap();

    let files = ClaudeCodeConnector::session_files(dir.path());
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
    assert_eq!(names, vec!["a.jsonl", "b.jsonl"]);
}

// =========================================================================
// Detection tests
// =========================================================================

#[test]
fn detect_not_found_without_projects_dir() {
    let connector = ClaudeCodeConnector::new();
    let result = connector.detect();
    // On most CI/test systems, .claude/projects won't exist
    // Just verify detect() doesn't panic
    let _ = result.detected;
}

// =========================================================================
// JSONL parsing tests
// =========================================================================

#[test]
fn scan_parses_jsonl_user_and_assistant_messages() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello Claude"}}
{"type":"assistant","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":"Hello! How can I help?"}}
{"type":"summary","timestamp":"2025-12-01T10:00:02Z","summary":"Test summary"}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);

    // Only user and assistant messages should be extracted (not summary)
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].role, "user");
    assert_eq!(convs[0].messages[0].content, "Hello Claude");
    assert_eq!(convs[0].messages[1].role, "assistant");
    assert!(convs[0].messages[1].content.contains("How can I help"));
}

#[test]
fn scan_with_callback_matches_scan_for_jsonl_session() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello Claude"}}
{"type":"assistant","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":"Hello! How can I help?"}}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
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
fn source_boundaries_complete_per_file_and_skip_on_resume() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());
    for name in ["a.jsonl", "b.jsonl"] {
        fs::write(
            claude_dir.join(name),
            concat!(
                r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello"}}"#,
                "\n",
            ),
        )
        .unwrap();
    }

    let connector = ClaudeCodeConnector::new();
    assert!(connector.supports_source_boundaries());
    let ctx = ScanContext::local_default(claude_dir.clone(), None);

    let mut completions: Vec<SourceCompletion> = Vec::new();
    let mut emitted = 0usize;
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
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_conv| {
                emitted += 1;
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(emitted, 2);
    assert_eq!(completions.len(), 2, "one completion per session file");
    for completion in &completions {
        assert_eq!(completion.conversations_emitted, 1);
        assert_eq!(completion.source.provider_slug, "claude_code");
        assert!(completion.source.size_bytes.is_some());
        assert!(completion.required_sidecars.is_empty());
    }

    // Identity matches discovery for every completed source.
    let discovered = connector.discover_source_files(&ctx).unwrap();
    for completion in &completions {
        let matching = discovered
            .iter()
            .find(|source| source.source_path == completion.source.source_path)
            .expect("completed source must be discoverable");
        assert_eq!(completion.source.size_bytes, matching.size_bytes);
        assert_eq!(completion.source.modified_at_ms, matching.modified_at_ms);
    }

    // Resume with a ledger built from the completions: nothing re-parsed.
    let ledger: Vec<SourceCompletion> = completions;
    let mut resumed = 0usize;
    {
        let mut should_scan = |source: &DiscoveredSourceFile| {
            !ledger.iter().any(|entry| {
                entry.source.source_path == source.source_path
                    && entry.source.size_bytes == source.size_bytes
                    && entry.source.modified_at_ms == source.modified_at_ms
            })
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut should_scan),
            on_source_complete: None,
        };
        connector
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_conv| {
                resumed += 1;
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(resumed, 0, "unchanged files must be skipped on resume");
}

#[test]
fn scan_skips_explicitly_excluded_session_path_without_skipping_siblings() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let active_session = claude_dir.join("active.jsonl");
    let stable_session = claude_dir.join("stable.jsonl");
    fs::write(
        &active_session,
        r#"{"type":"user","message":{"role":"user","content":"active in-progress content"}}"#,
    )
    .unwrap();
    fs::write(
        &stable_session,
        r#"{"type":"user","message":{"role":"user","content":"stable content"}}"#,
    )
    .unwrap();

    let ctx = ScanContext::local_default(claude_dir, None);
    let mut streamed = Vec::new();
    scan_claude_with_callback_with_exclusions(
        &ctx,
        &mut |conversation| {
            streamed.push(conversation);
            Ok(())
        },
        std::slice::from_ref(&active_session),
        &mut SourceScanHooks::default(),
    )
    .unwrap();

    assert_eq!(streamed.len(), 1);
    assert_eq!(streamed[0].source_path, stable_session);
    assert_eq!(streamed[0].messages[0].content, "stable content");
}

#[test]
fn discover_source_files_skips_explicitly_excluded_session_path() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let active_session = claude_dir.join("active.jsonl");
    let stable_session = claude_dir.join("stable.jsonl");
    fs::write(&active_session, "{}").unwrap();
    fs::write(&stable_session, "{}").unwrap();

    let ctx = ScanContext::local_default(claude_dir, None);
    let discovered = ClaudeCodeConnector::discover_sources_with_exclusions(
        &ctx,
        std::slice::from_ref(&active_session),
    );

    let source_paths: Vec<_> = discovered
        .into_iter()
        .map(|source| source.source_path)
        .collect();
    assert_eq!(source_paths, vec![stable_session]);
}

#[test]
fn large_sessions_keep_full_extras_preserving_reply_identity_and_usage() {
    use crate::connectors::token_extraction::extract_tokens_for_agent;
    use std::collections::HashSet;
    use std::io::{Read, Write};

    // The size at which the removed compaction layer used to kick in;
    // sessions at and above it are the regression guard for "extras
    // stay full".
    const LARGE_SESSION_BYTES: u64 = 32 * 1024 * 1024;

    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());
    let session_file = claude_dir.join("session.jsonl");
    let mut file = fs::File::create(&session_file).unwrap();
    // First two records are different content blocks of the same reply.
    // Both components of the identity matter; unidentified records must
    // remain separate rather than collapsing onto a shared missing key.
    let mut records = Vec::new();
    for (message_id, request_id, text) in [
        (Some("msg_1"), Some("req_1"), "first block"),
        (Some("msg_1"), Some("req_1"), "second block"),
        (Some("msg_1"), Some("req_2"), "different request"),
        (Some("msg_2"), Some("req_1"), "different message"),
        (None, None, "unidentified first"),
        (None, None, "unidentified second"),
    ] {
        let mut raw = json!({
            "type": "assistant",
            "message": {
                "role": "assistant", "model": "claude-opus-4-6",
                "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": 100, "output_tokens": 50,
                    "cache_read_input_tokens": 20, "cache_creation_input_tokens": 5}
            }
        });
        if let Some(id) = message_id {
            raw["message"]["id"] = json!(id);
        }
        if let Some(id) = request_id {
            raw["requestId"] = json!(id);
        }
        writeln!(file, "{raw}").unwrap();
        records.push(raw);
    }
    let record_bytes = file.metadata().unwrap().len();
    let ctx = ScanContext::local_default(claude_dir, None);
    let mut previous_size = record_bytes;
    for size in [
        LARGE_SESSION_BYTES - 1,
        LARGE_SESSION_BYTES,
        LARGE_SESSION_BYTES + 1,
    ] {
        // A blank whitespace line grows the source without introducing
        // extra messages or a giant duplicated message body in the test.
        std::io::copy(
            &mut std::io::repeat(b' ').take(size - previous_size),
            &mut file,
        )
        .unwrap();
        file.flush().unwrap();
        previous_size = size;
        assert_eq!(fs::metadata(&session_file).unwrap().len(), size);
        let conversations = ClaudeCodeConnector::new().scan(&ctx).unwrap();
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        assert_eq!(conversation.messages.len(), 6);
        assert_eq!(conversation.messages[0].content, "first block");
        assert_eq!(conversation.messages[1].content, "second block");
        let mut seen = HashSet::new();
        let mut total = 0;
        let mut undeduplicated_total = 0;
        for (message, raw) in conversation.messages.iter().zip(&records) {
            let extra = &message.extra;
            // Full extras at any size: every `extra` IS the raw source
            // record — envelope, content blocks and identity included.
            assert_eq!(extra, raw);
            assert!(extra.pointer("/message/content").is_some());
            let identity = extra
                .pointer("/message/id")
                .and_then(Value::as_str)
                .zip(extra.pointer("/requestId").and_then(Value::as_str));
            let usage = extract_tokens_for_agent(
                &conversation.agent_slug,
                extra,
                &message.content,
                &message.role,
            );
            assert_eq!(usage.input_tokens, Some(100));
            assert_eq!(usage.output_tokens, Some(50));
            assert_eq!(usage.cache_read_tokens, Some(20));
            assert_eq!(usage.cache_creation_tokens, Some(5));
            let tokens = usage.total_tokens().unwrap();
            undeduplicated_total += tokens;
            // Model a host consumer: extraction is per record; aggregation
            // dedupes only records carrying a complete reply identity.
            if identity.is_none_or(|key| seen.insert(key)) {
                total += tokens;
            }
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(undeduplicated_total, 1050);
        assert_eq!(total, 875);
    }
}

#[test]
fn scan_extracts_session_metadata() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","cwd":"/projects/myapp","sessionId":"sess-123","gitBranch":"main","message":{"role":"user","content":"Test message"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/projects/myapp")));
    assert_eq!(convs[0].metadata["sessionId"], "sess-123");
    assert_eq!(convs[0].metadata["gitBranch"], "main");
}

#[test]
fn scan_indexes_macos_desktop_sidecar_metadata_without_body() {
    let dir = TempDir::new().unwrap();
    let sidecar_root = dir
        .path()
        .join("Library")
        .join("Application Support")
        .join("Claude")
        .join("claude-code-sessions");
    let sidecar_path = sidecar_root
        .join("workspace-uuid")
        .join("session-uuid")
        .join("local_msg.json");
    fs::create_dir_all(sidecar_path.parent().unwrap()).unwrap();
    fs::write(
        &sidecar_path,
        json!({
            "sessionId": "local_msg",
            "cliSessionId": "cli-session-123",
            "cwd": "/Users/jane/project",
            "createdAt": 1_773_244_128_013_i64,
            "lastActivityAt": 1_773_278_849_911_i64,
            "model": "claude-opus-4-6",
            "title": "Prepare Endeavor Outliers 2025 demo case",
            "permissionMode": "plan"
        })
        .to_string(),
    )
    .unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(sidecar_root.clone())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.external_id.as_deref(), Some("cli-session-123"));
    assert_eq!(
        conv.title.as_deref(),
        Some("Prepare Endeavor Outliers 2025 demo case")
    );
    assert_eq!(conv.workspace, Some(PathBuf::from("/Users/jane/project")));
    assert_eq!(conv.source_path, sidecar_path);
    assert_eq!(conv.started_at, Some(1_773_244_128_013));
    assert_eq!(conv.ended_at, Some(1_773_278_849_911));
    assert_eq!(conv.metadata["source"], "claude_code_desktop_sidecar");
    assert_eq!(conv.metadata["cliSessionId"], "cli-session-123");
    assert_eq!(conv.metadata["bodyAvailable"], false);
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].role, "system");
    assert!(conv.messages[0].content.contains("Prepare Endeavor"));
    assert!(
        conv.messages[0]
            .content
            .contains("Workspace: /Users/jane/project")
    );
    assert!(
        conv.messages[0]
            .content
            .contains("CLI session id: cli-session-123")
    );
    assert!(
        conv.messages[0]
            .content
            .contains("culled the CLI JSONL body")
    );
}

#[test]
fn scan_extracts_model_as_author() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"assistant","message":{"role":"assistant","content":"Response","model":"claude-3-opus"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(
        convs[0].messages[0].author,
        Some("claude-3-opus".to_string())
    );
}

#[test]
fn scan_parses_iso8601_timestamp() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","timestamp":"2025-11-15T14:30:00.123Z","message":{"role":"user","content":"Test"}}"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert!(convs[0].messages[0].created_at.is_some());
    let ts = convs[0].messages[0].created_at.unwrap();
    // Should be around 2025-11-15 in milliseconds
    assert!(ts > 1_700_000_000_000);
}

#[test]
fn scan_handles_array_content() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "First part"},
                {"type": "text", "text": "Second part"}
            ]
        }
    })
    .to_string();
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages.len(), 1);
    assert!(convs[0].messages[0].content.contains("First part"));
    assert!(convs[0].messages[0].content.contains("Second part"));
}

#[test]
fn scan_skips_empty_content() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":""}}
{"type":"user","message":{"role":"user","content":"   "}}
{"type":"user","message":{"role":"user","content":"Valid message"}}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    // Only the valid message should be extracted
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].content, "Valid message");
}

/// cass GH #500: prompts typed while the agent is mid-turn are recorded as
/// `queued_command` attachments and must be indexed as user messages.
#[test]
fn scan_indexes_mid_turn_queued_command_prompts() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());
    let session_file = claude_dir.join("session.jsonl");
    let lines = [
        r#"{"type":"user","uuid":"u1","sessionId":"s","timestamp":"2026-09-24T10:00:00.000Z","message":{"role":"user","content":"please audit the zebratypedphrase module"}}"#,
        r#"{"type":"assistant","uuid":"a1","sessionId":"s","timestamp":"2026-09-24T10:00:05.000Z","message":{"role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"Starting on the audit now."}]}}"#,
        // queue-operation records repeat queued text and must stay out.
        r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-24T10:00:10.000Z","sessionId":"s","content":"also check the walrusqueuedphrase path"}"#,
        r#"{"type":"queue-operation","operation":"remove","timestamp":"2026-09-24T10:00:12.000Z","sessionId":"s","content":"also check the walrusqueuedphrase path"}"#,
        // Human-origin, string prompt.
        r#"{"type":"attachment","uuid":"q1","sessionId":"s","timestamp":"2026-09-24T10:00:12.000Z","attachment":{"type":"queued_command","prompt":"also check the walrusqueuedphrase path","source_uuid":"src1","commandMode":"prompt","origin":{"kind":"human"},"timestamp":"2026-09-24T10:00:12.000Z"}}"#,
        // Older builds: no origin field -> kept, unknown authorship.
        r#"{"type":"attachment","uuid":"q2","sessionId":"s","timestamp":"2026-09-24T10:00:13.000Z","attachment":{"type":"queued_command","prompt":"older era narwhallegacyphrase","commandMode":"prompt"}}"#,
        // List-shaped prompt.
        r#"{"type":"attachment","uuid":"q3","sessionId":"s","timestamp":"2026-09-24T10:00:14.000Z","attachment":{"type":"queued_command","prompt":[{"type":"text","text":"list shaped ocelotlistphrase prompt"}],"commandMode":"prompt","origin":{"kind":"human"}}}"#,
        // Skipped: relayed peer message, meta entry, task notification,
        // unrecognized explicit origin, empty prompt, other attachment.
        r#"{"type":"attachment","uuid":"x1","sessionId":"s","timestamp":"2026-09-24T10:00:15.000Z","attachment":{"type":"queued_command","prompt":"peer relayed ibexpeerphrase","commandMode":"prompt","isMeta":true,"origin":{"kind":"peer"}}}"#,
        r#"{"type":"attachment","uuid":"x2","sessionId":"s","timestamp":"2026-09-24T10:00:15.000Z","attachment":{"type":"queued_command","prompt":"meta yakmetaphrase","commandMode":"prompt","isMeta":true}}"#,
        r#"{"type":"attachment","uuid":"x3","sessionId":"s","timestamp":"2026-09-24T10:00:16.000Z","attachment":{"type":"queued_command","prompt":"<task-notification>lemurtaskphrase</task-notification>","commandMode":"task-notification"}}"#,
        r#"{"type":"attachment","uuid":"x4","sessionId":"s","timestamp":"2026-09-24T10:00:16.000Z","attachment":{"type":"queued_command","prompt":"odd okapiodd phrase","commandMode":"prompt","origin":{"kind":"something-new"}}}"#,
        r#"{"type":"attachment","uuid":"x5","sessionId":"s","timestamp":"2026-09-24T10:00:16.000Z","attachment":{"type":"queued_command","prompt":"   ","commandMode":"prompt","origin":{"kind":"human"}}}"#,
        r#"{"type":"attachment","uuid":"x6","sessionId":"s","timestamp":"2026-09-24T10:00:16.000Z","attachment":{"type":"total_tokens_reminder","prompt":"not a prompt"}}"#,
        r#"{"type":"assistant","uuid":"a2","sessionId":"s","timestamp":"2026-09-24T10:00:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Noted, I will also check that path."}]}}"#,
        // A late queued prompt extends the conversation's end bound.
        r#"{"type":"attachment","uuid":"q4","sessionId":"s","timestamp":"2026-09-24T10:00:30.000Z","attachment":{"type":"queued_command","prompt":"one more kiwilatephrase","commandMode":"prompt","origin":{"kind":"human"}}}"#,
    ];
    fs::write(&session_file, lines.join("\n")).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir, None);
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];

    let summary: Vec<(i64, &str, &str)> = conv
        .messages
        .iter()
        .map(|m| (m.idx, m.role.as_str(), m.content.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (0, "user", "please audit the zebratypedphrase module"),
            (1, "assistant", "Starting on the audit now."),
            (2, "user", "also check the walrusqueuedphrase path"),
            (3, "user", "older era narwhallegacyphrase"),
            (4, "user", "list shaped ocelotlistphrase prompt"),
            (5, "assistant", "Noted, I will also check that path."),
            (6, "user", "one more kiwilatephrase"),
        ]
    );

    let q1 = &conv.messages[2];
    assert_eq!(
        q1.created_at,
        parse_timestamp(&json!("2026-09-24T10:00:12.000Z"))
    );
    assert_eq!(q1.extra["cass"]["queued_command"]["authorship"], "human");
    assert_eq!(q1.extra["cass"]["queued_command"]["source_uuid"], "src1");
    assert_eq!(q1.extra["cass"]["queued_command"]["command_mode"], "prompt");
    // The raw record is carried verbatim.
    assert_eq!(q1.extra["attachment"]["origin"]["kind"], "human");

    let q2 = &conv.messages[3];
    assert_eq!(q2.extra["cass"]["queued_command"]["authorship"], "unknown");
    assert!(
        q2.extra["cass"]["queued_command"]
            .get("source_uuid")
            .is_none()
    );

    // The typed first message still names the conversation.
    assert_eq!(
        conv.title.as_deref(),
        Some("please audit the zebratypedphrase module")
    );
    assert_eq!(
        conv.started_at,
        parse_timestamp(&json!("2026-09-24T10:00:00.000Z"))
    );
    assert_eq!(
        conv.ended_at,
        parse_timestamp(&json!("2026-09-24T10:00:30.000Z"))
    );
}

#[test]
fn queued_command_extra_keeps_raw_record_and_provenance() {
    let raw = json!({
        "type": "attachment",
        "timestamp": "2026-09-24T10:00:12.000Z",
        "attachment": {
            "type": "queued_command",
            "prompt": "large session queued prompt",
            "source_uuid": "src9",
            "commandMode": "prompt",
            "origin": {"kind": "human"}
        }
    });
    let msg = ClaudeCodeConnector::queued_command_message(&raw).unwrap();
    assert_eq!(msg.role, "user");
    assert_eq!(msg.content, "large session queued prompt");
    // The raw attachment record survives verbatim — the queued-command
    // provenance rides inside it under `cass`, never replacing it.
    for (key, value) in raw.as_object().unwrap() {
        assert_eq!(msg.extra.get(key), Some(value));
    }
    assert_eq!(msg.extra["cass"]["queued_command"]["authorship"], "human");
    assert_eq!(msg.extra["cass"]["queued_command"]["source_uuid"], "src9");

    // Falls back to the nested timestamp when the outer one is missing.
    let nested_only = json!({
        "type": "attachment",
        "attachment": {
            "type": "queued_command",
            "prompt": "nested ts",
            "commandMode": "prompt",
            "timestamp": "2026-09-24T10:00:12.000Z"
        }
    });
    let msg = ClaudeCodeConnector::queued_command_message(&nested_only).unwrap();
    assert_eq!(
        msg.created_at,
        parse_timestamp(&json!("2026-09-24T10:00:12.000Z"))
    );
}

#[test]
fn scan_skips_non_user_assistant_types() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"summary","content":"Session summary"}
{"type":"file-history-snapshot","files":[]}
{"type":"user","message":{"role":"user","content":"User message"}}
{"type":"tool_result","result":"Some result"}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
}

#[test]
fn scan_reindexes_messages_sequentially() {
    let dir = TempDir::new().unwrap();
    let claude_dir = make_test_claude_dir(dir.path());

    let session_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"user","message":{"role":"user","content":"Message 1"}}
{"type":"assistant","message":{"role":"assistant","content":"Message 2"}}
{"type":"user","message":{"role":"user","content":"Message 3"}}
"#;
    fs::write(&session_file, content).unwrap();

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(claude_dir.clone(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs[0].messages[0].idx, 0);
    assert_eq!(convs[0].messages[1].idx, 1);
    assert_eq!(convs[0].messages[2].idx, 2);
}

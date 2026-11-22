//! Tests for the Cursor Agent transcript reader (gh #306). Fixtures use the
//! reporter's documented `{role, message:{content:[...]}}` shape plus
//! malformed / partial inputs to prove graceful degradation. All tests use
//! explicit scan roots (no HOME / env mutation), so the Composer path and
//! every other connector remain byte-for-byte unaffected.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::CursorConnector;
use super::agent_transcripts::AGENT_TRANSCRIPTS_DIR;
use crate::connectors::scan::{DiscoveredSourceRole, ScanContext, ScanRoot};
use crate::connectors::{Connector, assert_discovery_covers_scan_sources};

const USER_LINE: &str =
    r#"{"role":"user","message":{"content":[{"type":"text","text":"find the bug"}]}}"#;
const ASSISTANT_LINE: &str = r#"{"role":"assistant","message":{"content":[{"type":"text","text":"on it"},{"type":"tool_use","name":"Grep","input":{"pattern":"bug"}}]}}"#;

fn write_transcript(root: &Path, project: &str, sid: &str, lines: &[&str]) -> PathBuf {
    let dir = root.join(project).join(AGENT_TRANSCRIPTS_DIR).join(sid);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{sid}.jsonl"));
    fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();
    file
}

fn ctx_for(root: &Path) -> ScanContext {
    ScanContext::with_roots(
        root.to_path_buf(),
        vec![ScanRoot::local(root.to_path_buf())],
        None,
    )
}

fn write_workspace(transcript: &Path, workspace: &str) -> PathBuf {
    let path = CursorConnector::agent_workspace_metadata_path(transcript).unwrap();
    fs::write(
        &path,
        serde_json::json!({"workspacePath": workspace}).to_string(),
    )
    .unwrap();
    path
}

#[test]
fn scan_parses_documented_agent_shape() {
    let tmp = TempDir::new().unwrap();
    let transcript = write_transcript(
        tmp.path(),
        "Users-ibrahim-workspace-foo",
        "sess-1",
        &[USER_LINE, ASSISTANT_LINE],
    );
    write_workspace(&transcript, "/Users/ibrahim/workspace/foo");
    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.agent_slug, "cursor");
    assert_eq!(c.external_id.as_deref(), Some("sess-1"));
    assert_eq!(c.metadata["cursor_format"], "agent");
    assert_eq!(c.metadata["source"], "cursor");
    assert_eq!(
        c.metadata["cursor_workspace_attribution"],
        "workspace_trusted"
    );
    assert_eq!(
        c.metadata["cursor_project_dir"],
        "Users-ibrahim-workspace-foo"
    );
    assert_eq!(
        c.workspace.as_deref(),
        Some(Path::new("/Users/ibrahim/workspace/foo"))
    );
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("find the bug"));
    assert_eq!(c.messages[1].role, "assistant");
    assert!(c.messages[1].invocations.iter().any(|i| i.name == "Grep"));
    assert!(c.title.as_deref().unwrap().starts_with("find the bug"));
    // Indices are contiguous from 0.
    for (i, m) in c.messages.iter().enumerate() {
        assert_eq!(m.idx, i64::try_from(i).unwrap());
    }
}

#[test]
fn subagent_and_non_primary_transcripts_are_skipped() {
    let tmp = TempDir::new().unwrap();
    // Primary (kept).
    write_transcript(tmp.path(), "proj", "main-sid", &[USER_LINE]);
    // Subagent transcript under subagents/ (skipped).
    let sub = tmp
        .path()
        .join("proj")
        .join(AGENT_TRANSCRIPTS_DIR)
        .join("main-sid")
        .join("subagents")
        .join("sub-1");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("sub-1.jsonl"), format!("{USER_LINE}\n")).unwrap();
    // Non-primary sibling file (stem != dir name) (skipped).
    let dir = tmp
        .path()
        .join("proj")
        .join(AGENT_TRANSCRIPTS_DIR)
        .join("main-sid");
    fs::write(dir.join("notes.jsonl"), format!("{USER_LINE}\n")).unwrap();

    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert_eq!(convs.len(), 1, "only the primary transcript is indexed");
    assert_eq!(convs[0].external_id.as_deref(), Some("main-sid"));
}

#[test]
fn malformed_lines_are_skipped_not_fatal() {
    let tmp = TempDir::new().unwrap();
    write_transcript(
        tmp.path(),
        "proj",
        "s",
        &[USER_LINE, "this is not json", ASSISTANT_LINE],
    );
    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2, "valid records survive junk");
}

#[test]
fn empty_and_contentless_transcripts_yield_no_conversation() {
    let tmp = TempDir::new().unwrap();
    // Empty file.
    write_transcript(tmp.path(), "p1", "empty", &[]);
    // A role with empty content + an unrelated object.
    write_transcript(
        tmp.path(),
        "p2",
        "blank",
        &[
            r#"{"role":"user","message":{"content":[]}}"#,
            r#"{"foo":"bar"}"#,
        ],
    );
    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert!(convs.is_empty(), "no fabricated conversations");
}

#[test]
fn partial_records_degrade_gracefully() {
    let tmp = TempDir::new().unwrap();
    write_transcript(
        tmp.path(),
        "proj",
        "s",
        &[
            // Missing `message` wrapper; top-level content as a plain string.
            r#"{"role":"user","content":"plain string content"}"#,
            // Missing role -> skipped, never fatal.
            r#"{"message":{"content":[{"type":"text","text":"no role"}]}}"#,
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
        ],
    );
    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 2, "role-less record dropped");
    assert!(
        convs[0].messages[0]
            .content
            .contains("plain string content")
    );
}

#[test]
fn discovery_covers_agent_scan_sources() {
    let tmp = TempDir::new().unwrap();
    write_transcript(tmp.path(), "proj", "s", &[USER_LINE, ASSISTANT_LINE]);
    assert_discovery_covers_scan_sources(&CursorConnector::new(), &ctx_for(tmp.path()));

    let discovered = CursorConnector::new()
        .discover_source_files(&ctx_for(tmp.path()))
        .unwrap();
    assert!(
        discovered.iter().any(|d| d.provider_slug == "cursor"
            && d.source_path.extension().is_some_and(|e| e == "jsonl")
            && d.role == DiscoveredSourceRole::PrimarySessionLog
            && d.required_for_reconstruction),
        "the agent transcript jsonl must be a required primary source"
    );
}

#[test]
fn explicit_workspace_preserves_parent_and_leaf_hyphens_with_both_candidates_present() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("project-parent/my-app");
    let guessed = tmp.path().join("project/parent/my/app");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&guessed).unwrap();
    let project = workspace.to_str().unwrap().replace(['/', '\\', ':'], "-");
    assert_eq!(
        project,
        guessed.to_str().unwrap().replace(['/', '\\', ':'], "-")
    );
    let projects = tmp.path().join("cursor-projects");
    let transcript = write_transcript(&projects, &project, "stable-id", &[USER_LINE]);
    write_workspace(&transcript, workspace.to_str().unwrap());
    let convs = CursorConnector::new().scan(&ctx_for(&projects)).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace.as_ref(), Some(&workspace));
    assert_ne!(convs[0].workspace.as_ref(), Some(&guessed));
    assert_eq!(convs[0].external_id.as_deref(), Some("stable-id"));
    assert_eq!(convs[0].source_path, transcript);
    assert_eq!(convs[0].metadata["cursor_project_dir"], project);
}

#[test]
fn absent_malformed_and_invalid_workspace_metadata_never_guess() {
    let tmp = TempDir::new().unwrap();
    let transcript = write_transcript(
        tmp.path(),
        "home-example-projects-my-app",
        "s",
        &[USER_LINE],
    );
    let sidecar = CursorConnector::agent_workspace_metadata_path(&transcript).unwrap();
    let invalid = [
        None,
        Some("not json"),
        Some("{}"),
        Some(r#"{"workspacePath":null}"#),
        Some(r#"{"workspacePath":42}"#),
        Some(r#"{"workspacePath":[]}"#),
        Some(r#"{"workspacePath":""}"#),
        Some(r#"{"workspacePath":"relative/my-app"}"#),
        Some(r#"{"workspacePath":"file:///home/example/my-app"}"#),
        Some(r#"{"workspacePath":"C:relative"}"#),
        Some(r#"{"workspacePath":"/home/example/\u0000my-app"}"#),
    ];
    for input in invalid {
        if let Some(input) = input {
            fs::write(&sidecar, input).unwrap();
        }
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(convs.len(), 1, "{input:?}");
        let c = &convs[0];
        assert_eq!(c.workspace, None, "{input:?}");
        assert_eq!(c.metadata["cursor_workspace_attribution"], "unresolved");
        assert_eq!(
            c.metadata["cursor_project_dir"],
            "home-example-projects-my-app"
        );
        assert_eq!(c.external_id.as_deref(), Some("s"));
        assert_eq!(c.source_path, transcript);
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].content, "find the bug");
    }
}

#[test]
fn exported_workspace_metadata_preserves_attribution_and_source_bytes() {
    let source = TempDir::new().unwrap();
    let exported = TempDir::new().unwrap();
    let first = write_transcript(
        source.path(),
        "home-example-projects-my-app",
        "first",
        &[USER_LINE, ASSISTANT_LINE],
    );
    write_transcript(
        source.path(),
        "home-example-projects-my-app",
        "second",
        &[USER_LINE],
    );
    let sidecar = write_workspace(&first, "/home/example/projects/my-app");
    let connector = CursorConnector::new();
    let discovered = connector
        .discover_source_files(&ctx_for(source.path()))
        .unwrap();
    assert_eq!(discovered.len(), 3);
    let metadata: Vec<_> = discovered
        .iter()
        .filter(|s| s.role == DiscoveredSourceRole::MetadataSidecar)
        .collect();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].source_path, sidecar);
    assert!(metadata[0].required_for_reconstruction);
    let before: Vec<_> = discovered
        .iter()
        .map(|s| {
            (
                s.source_path.clone(),
                fs::read(&s.source_path).unwrap(),
                fs::metadata(&s.source_path).unwrap().modified().unwrap(),
            )
        })
        .collect();
    for source_file in &discovered {
        let destination = exported
            .path()
            .join(source_file.source_path.strip_prefix(source.path()).unwrap());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(&source_file.source_path, destination).unwrap();
    }
    let original = connector.scan(&ctx_for(source.path())).unwrap();
    let copied = connector.scan(&ctx_for(exported.path())).unwrap();
    assert_eq!(original.len(), 2);
    assert_eq!(copied.len(), original.len());
    for (a, b) in original.iter().zip(&copied) {
        assert_eq!(a.external_id, b.external_id);
        assert_eq!(a.workspace, b.workspace);
        assert_eq!(
            a.workspace.as_deref(),
            Some(Path::new("/home/example/projects/my-app"))
        );
        assert_eq!(a.metadata, b.metadata);
        assert_eq!(a.messages.len(), b.messages.len());
        for (a, b) in a.messages.iter().zip(&b.messages) {
            assert_eq!(a.content, b.content);
            assert_eq!(a.role, b.role);
        }
    }
    for (path, bytes, mtime) in before {
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), mtime);
    }
    let mapped = ScanContext::with_roots(
        exported.path().to_path_buf(),
        vec![
            ScanRoot::local(exported.path().to_path_buf())
                .with_rewrite("/home/example/projects", "/mapped/projects"),
        ],
        None,
    );
    let mapped = connector.scan(&mapped).unwrap();
    assert_eq!(mapped.len(), 2);
    assert!(
        mapped
            .iter()
            .all(|c| c.workspace.as_deref() == Some(Path::new("/mapped/projects/my-app")))
    );
    assert!(mapped.iter().all(|c| {
        c.metadata
            .pointer("/cass/workspace_original")
            .and_then(Value::as_str)
            == Some("/home/example/projects/my-app")
    }));
}

#[test]
fn absolute_foreign_workspace_paths_do_not_require_local_existence() {
    let tmp = TempDir::new().unwrap();
    let transcript = write_transcript(tmp.path(), "foreign-project", "s", &[USER_LINE]);
    for workspace in [
        "/remote/parent-name/my-app",
        r"C:\Users\example\my-app",
        r"\\server\share\my-app",
    ] {
        write_workspace(&transcript, workspace);
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].workspace.as_deref(), Some(Path::new(workspace)));
        assert_eq!(
            convs[0].metadata["cursor_workspace_attribution"],
            "workspace_trusted"
        );
    }
}

#[test]
fn metadata_change_selects_an_unchanged_transcript() {
    let tmp = TempDir::new().unwrap();
    let transcript = write_transcript(tmp.path(), "parent-my-app", "s", &[USER_LINE]);
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&transcript)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_time))
        .unwrap();
    let original_mtime = fs::metadata(&transcript).unwrap().modified().unwrap();
    let original_bytes = fs::read(&transcript).unwrap();
    let since = i64::try_from(
        (std::time::SystemTime::now() - std::time::Duration::from_secs(5))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let ctx = ScanContext::with_roots(
        tmp.path().to_path_buf(),
        vec![ScanRoot::local(tmp.path().to_path_buf())],
        Some(since),
    );
    assert!(CursorConnector::new().scan(&ctx).unwrap().is_empty());
    write_workspace(&transcript, "/parent/my-app");
    let convs = CursorConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].workspace.as_deref(),
        Some(Path::new("/parent/my-app"))
    );
    assert_discovery_covers_scan_sources(&CursorConnector::new(), &ctx);
    assert_eq!(fs::read(&transcript).unwrap(), original_bytes);
    assert_eq!(
        fs::metadata(&transcript).unwrap().modified().unwrap(),
        original_mtime
    );
}

#[test]
fn since_ts_in_future_skips_agent_transcript() {
    let tmp = TempDir::new().unwrap();
    write_transcript(tmp.path(), "proj", "s", &[USER_LINE]);
    let ctx = ScanContext::with_roots(
        tmp.path().to_path_buf(),
        vec![ScanRoot::local(tmp.path().to_path_buf())],
        Some(i64::MAX),
    );
    assert!(CursorConnector::new().scan(&ctx).unwrap().is_empty());
}

#[test]
fn no_agent_transcripts_leaves_scan_empty_and_safe() {
    // A root with neither a state.vscdb nor agent-transcripts yields nothing
    // and must not panic or fabricate anything.
    let tmp = TempDir::new().unwrap();
    let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn cursor_agent_transcript_mid_read_io_error_discards_incomplete_and_skips_corrupt_utf8() {
    use std::io::{self, BufReader, Cursor, Read};

    struct FailingReader(io::ErrorKind);
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "simulated mid-read failure"))
        }
    }

    let prefix = "{\"role\":\"user\",\"content\":\"complete prefix\"}\n";
    for kind in [
        io::ErrorKind::Other,
        io::ErrorKind::InvalidData,
        io::ErrorKind::Interrupted,
    ] {
        let reader = BufReader::new(prefix.as_bytes().chain(FailingReader(kind)));
        assert!(
            CursorConnector::read_agent_transcript_records_from(
                Path::new("transcript.jsonl"),
                reader
            )
            .is_err(),
            "mid-stream {kind:?} must discard incomplete Cursor agent transcript"
        );
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\xef\xbb\xbf{\"role\":\"user\",\"content\":\"first valid\"}\n");
    bytes.extend_from_slice(b"{\"role\":\"assistant\",\"content\":\"\xffcorrupt\"}\n");
    bytes.extend_from_slice(b"{\"role\":\"assistant\",\"content\":\"second valid\"}\n");
    let records = CursorConnector::read_agent_transcript_records_from(
        Path::new("transcript.jsonl"),
        Cursor::new(bytes),
    )
    .expect("corrupt UTF-8 line should be skipped");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["content"], "first valid");
    assert_eq!(records[1]["content"], "second valid");
}

#[test]
fn parse_bubble_extracts_tool_former_data_and_tool_calls() {
    let bubble = serde_json::json!({
        "type": 2,
        "modelType": "claude-sonnet-4-5",
        "toolFormerData": {
            "name": "read_file",
            "toolCallId": "call_tf_1",
            "params": "{\"path\":\"src/lib.rs\"}"
        },
        "toolCalls": [
            {
                "id": "call_tc_2",
                "name": "skill",
                "arguments": {"name": "commit"}
            }
        ]
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0).expect("tool-only bubble should parse");
    assert_eq!(msg.role, "assistant");
    assert_eq!(msg.content, "[Tool: read_file]\n[Tool: commit]");
    assert_eq!(msg.invocations.len(), 2);
    assert_eq!(msg.invocations[0].kind, "tool");
    assert_eq!(msg.invocations[0].name, "read_file");
    assert_eq!(msg.invocations[0].call_id.as_deref(), Some("call_tf_1"));
    assert_eq!(
        msg.invocations[0]
            .arguments
            .as_ref()
            .and_then(|a| a.get("path"))
            .and_then(Value::as_str),
        Some("src/lib.rs")
    );
    assert_eq!(msg.invocations[1].kind, "skill");
    assert_eq!(msg.invocations[1].name, "commit");
    assert_eq!(msg.invocations[1].raw_name.as_deref(), Some("skill"));
    assert_eq!(
        msg.extra
            .pointer("/cass/tool_call_count")
            .and_then(Value::as_u64),
        Some(2)
    );
}

//! GH520: an invalid retained basename must not become a contents-copy scan.
#![cfg(feature = "connectors")]

use std::fs;
use std::path::{Path, PathBuf};

use franken_agent_detection::{
    ClaudeCodeConnector, CodexConnector, Connector, Origin, QoderConnector, ScanContext, ScanRoot,
};

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn context(container: PathBuf, source: &str) -> ScanContext {
    let state = container.parent().unwrap().join("state");
    ScanContext::with_roots(
        state,
        vec![
            ScanRoot::remote(container, Origin::remote("laptop"), None)
                .with_mirrored_source_path(source),
        ],
        None,
    )
}

#[test]
fn application_named_codex_and_qoder_projects_are_not_expanded_again() {
    let cases: [(&str, &str, &str, Box<dyn Connector>); 2] = [
        (
            ".codex/sessions/.codex",
            "rollout-2026-01-05T10-00-00-s1.jsonl",
            concat!(
                "{\"timestamp\":\"2026-01-05T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-owned\"}}\n",
                "{\"timestamp\":\"2026-01-05T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"selected project\"}}\n"
            ),
            Box::new(CodexConnector::new()),
        ),
        (
            ".qoder/projects/.qoder",
            "11111111-1111-4111-8111-111111111111.jsonl",
            "{\"uuid\":\"u1\",\"parentUuid\":null,\"sessionId\":\"11111111-1111-4111-8111-111111111111\",\"timestamp\":\"2026-01-05T10:00:00Z\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"selected project\"}}\n",
            Box::new(QoderConnector::new()),
        ),
    ];
    for (store, filename, content, connector) in cases {
        for (slash, retained) in [(false, true), (true, true), (true, false)] {
            let temp = tempfile::tempdir().unwrap();
            let container = temp.path().join("opaque_hash");
            let payload = if retained {
                container.join(Path::new(store).file_name().unwrap())
            } else {
                container.clone()
            };
            let file = payload.join(filename);
            put(&file, content.as_bytes());
            let source = format!("~/{store}{}", if slash { "/" } else { "" });
            let ctx = context(container, &source);
            let sources = connector.discover_source_files(&ctx).unwrap();
            assert_eq!(sources.len(), 1, "{source}, retained={retained}");
            assert_eq!(sources[0].source_path, file);
            assert_eq!(sources[0].origin, Origin::remote("laptop"));
            let scanned = connector.scan(&ctx).unwrap();
            assert_eq!(scanned.len(), 1, "{source}, retained={retained}");
            assert_eq!(scanned[0].messages[0].content, "selected project");
            assert_eq!(scanned[0].source_path, file);
            let native = temp.path().join("native");
            put(&native.join(store).join(filename), content.as_bytes());
            let local = ScanContext::with_roots(
                temp.path().join("state"),
                vec![ScanRoot::local(native)],
                None,
            );
            let expected = connector.scan(&local).unwrap();
            assert_eq!(expected.len(), 1);
            assert_eq!(scanned[0].external_id, expected[0].external_id);
            assert_eq!(fs::read(&file).unwrap(), content.as_bytes());
        }
    }
}

fn assert_selected_scope(connector: &dyn Connector, ctx: &ScanContext, rejected: bool) {
    let expected = usize::from(!rejected);
    let sources = connector.discover_source_files(ctx).unwrap();
    assert_eq!(sources.len(), expected);
    let conversations = connector.scan(ctx).unwrap();
    assert_eq!(conversations.len(), expected);
    let mut streamed = Vec::new();
    connector
        .scan_with_callback(ctx, &mut |conversation| {
            streamed.push(conversation);
            Ok(())
        })
        .unwrap();
    assert_eq!(streamed.len(), expected);
    if !rejected {
        assert_eq!(streamed[0].external_id, conversations[0].external_id);
        assert_eq!(
            streamed[0].messages[0].content,
            conversations[0].messages[0].content
        );
    }
}

#[test]
fn selected_claude_non_directory_leaf_does_not_ingest_container_messages() {
    for invalid_leaf in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let container = temp.path().join("opaque_hash");
        let file = container.join("session.jsonl");
        let content = b"{\"type\":\"user\",\"timestamp\":\"2026-01-05T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"container transcript\"}}\n";
        put(&file, content);
        if invalid_leaf {
            fs::write(container.join("selected"), b"not a directory").unwrap();
        }
        let ctx = context(container, "~/.claude/projects/selected/");
        assert_selected_scope(&ClaudeCodeConnector::new(), &ctx, invalid_leaf);
        assert_eq!(fs::read(&file).unwrap(), content);
    }
}

#[cfg(unix)]
#[test]
fn dangling_selected_claude_leaf_does_not_ingest_container_messages() {
    let temp = tempfile::tempdir().unwrap();
    let container = temp.path().join("opaque_hash");
    let file = container.join("session.jsonl");
    let content = b"{\"type\":\"user\",\"timestamp\":\"2026-01-05T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"container transcript\"}}\n";
    put(&file, content);
    let leaf = container.join("selected");
    let target = temp.path().join("missing");
    std::os::unix::fs::symlink(&target, &leaf).unwrap();
    let ctx = context(container, "~/.claude/projects/selected/");
    assert_selected_scope(&ClaudeCodeConnector::new(), &ctx, true);
    assert_eq!(fs::read_link(&leaf).unwrap(), target);
    assert_eq!(fs::read(&file).unwrap(), content);
}

#[cfg(feature = "cursor")]
mod cursor {
    use super::*;
    use franken_agent_detection::CursorConnector;

    include!("support/cursor_workspace_fixture.rs");

    #[test]
    fn selected_cursor_non_directory_leaf_does_not_ingest_container_database() {
        for invalid_leaf in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let container = temp.path().join("opaque_hash");
            let file = container.join("state.vscdb");
            let bytes = cursor_fixture();
            put(&file, &bytes);
            if invalid_leaf {
                fs::write(container.join("selected"), b"not a directory").unwrap();
            }
            let ctx = context(
                container,
                "~/.config/Cursor/User/workspaceStorage/selected/",
            );
            assert_selected_scope(&CursorConnector::new(), &ctx, invalid_leaf);
            assert_eq!(fs::read(&file).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn dangling_selected_cursor_leaf_does_not_ingest_container_database() {
        let temp = tempfile::tempdir().unwrap();
        let container = temp.path().join("opaque_hash");
        let file = container.join("state.vscdb");
        let bytes = cursor_fixture();
        put(&file, &bytes);
        let leaf = container.join("selected");
        let target = temp.path().join("missing");
        std::os::unix::fs::symlink(&target, &leaf).unwrap();
        let ctx = context(
            container,
            "~/.config/Cursor/User/workspaceStorage/selected/",
        );
        assert_selected_scope(&CursorConnector::new(), &ctx, true);
        assert_eq!(fs::read_link(&leaf).unwrap(), target);
        assert_eq!(fs::read(&file).unwrap(), bytes);
    }
}

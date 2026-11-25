use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::json;
use tempfile::TempDir;

use super::CursorConnector;
use super::tests_support::create_test_db;
use crate::connectors::Connector;
use crate::connectors::scan::{ScanContext, ScanRoot};
use crate::connectors::sqlite_sync::{ConnectionExt, params};

#[test]
fn remote_mirrors_discover_and_scan_cursor_databases() {
    for (source, subtree) in [
        ("~/.config/Cursor/User", "User"),
        ("/Users/u/Library/Application Support/Cursor", "Cursor/User"),
        ("C:\\Users\\u\\AppData\\Roaming\\Cursor\\User", "User"),
    ] {
        let tmp = TempDir::new().unwrap();
        let wrapper = tmp.path().join("Cursor_User_0123456789abcdef");
        let db_path = wrapper.join(subtree).join("globalStorage/state.vscdb");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = create_test_db(&db_path);
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                "composerData:mirror-test",
                r#"{"text":"Remote conversation"}"#
            ],
        )
        .unwrap();
        drop(conn);
        let origin = crate::types::Origin::remote_with_host("laptop", "u@laptop");
        let root =
            ScanRoot::remote(wrapper, origin.clone(), None).with_mirrored_source_path(source);
        let ctx = ScanContext::with_roots(tmp.path().join("state"), vec![root.clone(), root], None);
        let connector = CursorConnector::new();
        let discovered = connector.discover_source_files(&ctx).unwrap();
        assert_eq!(discovered.len(), 1, "{source}");
        assert_eq!(discovered[0].source_path, db_path);
        assert_eq!(discovered[0].origin, origin);
        let conversations = connector.scan(&ctx).unwrap();
        assert_eq!(conversations.len(), 1, "{source}");
        assert_eq!(conversations[0].source_path, db_path.join("mirror-test"));
        assert!(
            conversations[0].messages[0]
                .content
                .contains("Remote conversation")
        );
    }
}

// =========================================================================
// Constructor tests
// =========================================================================

#[test]
fn new_creates_connector() {
    let connector = CursorConnector::new();
    let _ = connector;
}

#[test]
fn default_creates_connector() {
    let connector = CursorConnector;
    let _ = connector;
}

// =========================================================================
// find_db_files tests
// =========================================================================

#[test]
fn find_db_files_empty_for_nonexistent() {
    let dir = TempDir::new().unwrap();
    let dbs = CursorConnector::find_db_files(dir.path());
    assert!(dbs.is_empty());
}

#[test]
fn find_db_files_finds_global_storage() {
    let dir = TempDir::new().unwrap();
    let global_dir = dir.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();
    fs::write(global_dir.join("state.vscdb"), "").unwrap();

    let dbs = CursorConnector::find_db_files(dir.path());
    assert_eq!(dbs.len(), 1);
    assert!(dbs[0].ends_with("state.vscdb"));
}

#[test]
fn find_db_files_finds_workspace_storage() {
    let dir = TempDir::new().unwrap();
    let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

    let dbs = CursorConnector::find_db_files(dir.path());
    assert_eq!(dbs.len(), 1);
}

#[test]
fn find_db_files_finds_multiple() {
    let dir = TempDir::new().unwrap();

    // Create global storage
    let global_dir = dir.path().join("globalStorage");
    fs::create_dir_all(&global_dir).unwrap();
    fs::write(global_dir.join("state.vscdb"), "").unwrap();

    // Create multiple workspace storage dirs
    for i in 1..=3 {
        let ws_dir = dir.path().join("workspaceStorage").join(format!("ws{}", i));
        fs::create_dir_all(&ws_dir).unwrap();
        fs::write(ws_dir.join("state.vscdb"), "").unwrap();
    }

    let dbs = CursorConnector::find_db_files(dir.path());
    assert_eq!(dbs.len(), 4); // 1 global + 3 workspace
}

#[test]
fn find_db_files_returns_sorted_order() {
    let dir = TempDir::new().unwrap();

    let ws_z = dir.path().join("workspaceStorage").join("zeta");
    let ws_a = dir.path().join("workspaceStorage").join("alpha");
    fs::create_dir_all(&ws_z).unwrap();
    fs::create_dir_all(&ws_a).unwrap();
    fs::write(ws_z.join("state.vscdb"), "").unwrap();
    fs::write(ws_a.join("state.vscdb"), "").unwrap();

    let dbs = CursorConnector::find_db_files(dir.path());
    assert_eq!(dbs.len(), 2);

    let names: Vec<_> = dbs
        .iter()
        .map(|p| {
            p.parent()
                .and_then(|x| x.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(names, vec!["alpha", "zeta"]);
}

#[test]
fn find_db_files_ignores_workspace_noise_files() {
    let dir = TempDir::new().unwrap();
    let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
    fs::create_dir_all(&workspace_dir).unwrap();

    for i in 0..20 {
        fs::write(workspace_dir.join(format!("noise_{i}.json")), "{}").unwrap();
    }
    fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

    let dbs = CursorConnector::find_db_files(dir.path());
    assert_eq!(dbs.len(), 1);
    assert!(dbs[0].ends_with("state.vscdb"));
}

#[test]
fn find_db_files_supports_workspace_storage_root_db() {
    let dir = TempDir::new().unwrap();
    let workspace_storage = dir.path().join("workspaceStorage");
    fs::create_dir_all(&workspace_storage).unwrap();
    fs::write(workspace_storage.join("state.vscdb"), "").unwrap();

    let dbs = CursorConnector::find_db_files(dir.path());
    assert_eq!(dbs.len(), 1);
    assert!(dbs[0].ends_with("workspaceStorage/state.vscdb"));
}

#[test]
fn has_any_db_file_is_true_for_workspace_storage_db() {
    let dir = TempDir::new().unwrap();
    let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

    assert!(CursorConnector::has_any_db_file(dir.path()));
}

#[test]
fn has_any_db_file_is_false_for_workspace_noise_only() {
    let dir = TempDir::new().unwrap();
    let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(workspace_dir.join("note.txt"), "noise").unwrap();

    assert!(!CursorConnector::has_any_db_file(dir.path()));
}

// =========================================================================
// parse_bubble tests
// =========================================================================

#[test]
fn parse_bubble_with_text() {
    let bubble = json!({
        "text": "Hello from user",
        "type": "user"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0);
    assert!(msg.is_some());
    let msg = msg.unwrap();
    assert_eq!(msg.content, "Hello from user");
    assert_eq!(msg.role, "user");
}

#[test]
fn parse_bubble_with_content_field() {
    let bubble = json!({
        "content": "Response from assistant",
        "role": "assistant"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 1);
    assert!(msg.is_some());
    let msg = msg.unwrap();
    assert_eq!(msg.content, "Response from assistant");
    assert_eq!(msg.role, "assistant");
}

#[test]
fn parse_bubble_with_message_field() {
    let bubble = json!({
        "message": "Another message",
        "type": "ai"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0);
    assert!(msg.is_some());
    let msg = msg.unwrap();
    assert_eq!(msg.content, "Another message");
    assert_eq!(msg.role, "assistant"); // "ai" maps to assistant
}

#[test]
fn parse_bubble_role_normalization() {
    let test_cases = vec![
        ("user", "user"),
        ("human", "user"),
        ("assistant", "assistant"),
        ("ai", "assistant"),
        ("bot", "assistant"),
        ("custom", "custom"), // Unknown roles pass through
    ];

    for (input_role, expected_role) in test_cases {
        let bubble = json!({
            "text": "test",
            "type": input_role
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(
            msg.role, expected_role,
            "Failed for input role: {}",
            input_role
        );
    }
}

#[test]
fn parse_bubble_empty_content_returns_none() {
    let bubble = json!({
        "text": "",
        "type": "user"
    });

    assert!(CursorConnector::parse_bubble(&bubble, 0).is_none());
}

#[test]
fn parse_bubble_whitespace_only_returns_none() {
    let bubble = json!({
        "text": "   \n\t  ",
        "type": "user"
    });

    assert!(CursorConnector::parse_bubble(&bubble, 0).is_none());
}

#[test]
fn parse_bubble_extracts_timestamp() {
    let bubble = json!({
        "text": "Test",
        "type": "user",
        "timestamp": 1700000000000i64
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
    assert_eq!(msg.created_at, Some(1700000000000));
}

#[test]
fn parse_bubble_extracts_model() {
    let bubble = json!({
        "text": "Response",
        "type": "assistant",
        "model": "gpt-4"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
    assert_eq!(msg.author, Some("gpt-4".to_string()));
}

#[test]
fn parse_bubble_defaults_to_assistant() {
    let bubble = json!({
        "text": "No role specified"
    });

    let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
    assert_eq!(msg.role, "assistant");
}

// =========================================================================
// parse_composer_data tests
// =========================================================================

#[test]
fn parse_composer_data_with_tabs_and_bubbles() {
    let key = "composerData:abc-123";
    let value = json!({
        "createdAt": 1700000000000i64,
        "tabs": [{
            "bubbles": [
                {"text": "Hello", "type": "user"},
                {"text": "Hi there!", "type": "assistant"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_some());
    let conv = conv.unwrap();
    assert_eq!(conv.agent_slug, "cursor");
    assert_eq!(conv.external_id, Some("abc-123".to_string()));
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[1].role, "assistant");
}

#[test]
fn parse_composer_data_with_conversation_map() {
    let key = "composerData:def-456";
    let value = json!({
        "conversationMap": {
            "conv1": {
                "bubbles": [
                    {"text": "Question?", "type": "user"},
                    {"content": "Answer!", "role": "assistant"}
                ]
            }
        }
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_some());
    let conv = conv.unwrap();
    assert_eq!(conv.messages.len(), 2);
}

#[test]
fn parse_composer_data_with_text_only() {
    let key = "composerData:simple-123";
    let value = json!({
        "text": "Simple user input without bubbles",
        "createdAt": 1700000000000i64
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_some());
    let conv = conv.unwrap();
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("Simple user input"));
}

#[test]
fn parse_composer_data_with_rich_text() {
    let key = "composerData:rich-789";
    let value = json!({
        "richText": "Rich text content here"
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_some());
    let conv = conv.unwrap();
    assert!(conv.messages[0].content.contains("Rich text"));
}

#[test]
fn parse_composer_data_skips_duplicates() {
    let key = "composerData:dup-123";
    let value = json!({ "text": "Content" }).to_string();

    let mut seen = HashSet::new();
    let conv1 = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );
    let conv2 = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv1.is_some());
    assert!(conv2.is_none()); // Duplicate should return None
}

#[test]
fn parse_composer_data_returns_none_for_empty() {
    let key = "composerData:empty-123";
    let value = json!({}).to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_none());
}

#[test]
fn parse_composer_data_extracts_model_config() {
    let key = "composerData:model-123";
    let value = json!({
        "text": "Test",
        "modelConfig": {
            "modelName": "gpt-4-turbo"
        }
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_some());
    let conv = conv.unwrap();
    assert_eq!(conv.metadata["model"], "gpt-4-turbo");
}

#[test]
fn parse_composer_data_invalid_key_returns_none() {
    let key = "not-composer-data"; // Missing "composerData:" prefix
    let value = json!({ "text": "Content" }).to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_composer_data(
        key,
        &value,
        Path::new("/test"),
        None,
        &mut seen,
        None,
    );

    assert!(conv.is_none());
}

// =========================================================================
// parse_aichat_data tests
// =========================================================================

#[test]
fn parse_aichat_data_with_tabs() {
    let key = "aichat.chatdata";
    let value = json!({
        "tabs": [{
            "timestamp": 1700000000000i64,
            "bubbles": [
                {"text": "User question", "type": "user"},
                {"text": "AI response", "type": "ai"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

    assert!(conv.is_some());
    let conv = conv.unwrap();
    assert_eq!(conv.agent_slug, "cursor");
    assert!(conv.external_id.as_ref().unwrap().starts_with("aichat-"));
    assert_eq!(conv.messages.len(), 2);
}

#[test]
fn parse_aichat_data_tracks_min_max_out_of_order_timestamps() {
    let key = "aichat.bounds";
    let value = json!({
        "tabs": [
            {
                "timestamp": 1700000200,
                "bubbles": [
                    {"text": "later", "type": "ai", "timestamp": 1700000300}
                ]
            },
            {
                "timestamp": 1700000000,
                "bubbles": [
                    {"text": "earlier", "type": "user", "timestamp": 1700000100}
                ]
            }
        ]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen)
        .expect("aichat conversation should parse");

    // Bounds should be based on true min/max timestamps, not traversal order.
    assert_eq!(conv.started_at, Some(1_700_000_100_000));
    assert_eq!(conv.ended_at, Some(1_700_000_300_000));
}

#[test]
fn parse_aichat_data_parses_string_tab_timestamp() {
    let key = "aichat.string-ts";
    let value = json!({
        "tabs": [{
            "timestamp": "1700000400",
            "bubbles": [
                {"text": "no bubble timestamp", "type": "user"}
            ]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen)
        .expect("aichat conversation should parse");

    // String timestamps should be normalized to milliseconds.
    assert_eq!(conv.started_at, Some(1_700_000_400_000));
    assert_eq!(conv.ended_at, Some(1_700_000_400_000));
}

#[test]
fn parse_aichat_data_returns_none_for_empty() {
    let key = "aichat.empty";
    let value = json!({
        "tabs": []
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv = CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

    assert!(conv.is_none());
}

#[test]
fn parse_aichat_data_skips_duplicates() {
    let key = "aichat.dup";
    let value = json!({
        "tabs": [{
            "bubbles": [{"text": "Content", "type": "user"}]
        }]
    })
    .to_string();

    let mut seen = HashSet::new();
    let conv1 =
        CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);
    let conv2 =
        CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

    assert!(conv1.is_some());
    assert!(conv2.is_none());
}

// =========================================================================
// extract_from_db tests
// =========================================================================

#[test]
fn extract_from_db_with_composer_data() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let conn = create_test_db(&db_path);
    let value = json!({ "text": "Database test" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:db-test-123", value.as_str()],
    )
    .unwrap();
    drop(conn);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].messages[0].content.contains("Database test"));
}

#[test]
fn extract_from_db_with_aichat_data() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let conn = create_test_db(&db_path);
    let value = json!({
        "tabs": [{
            "bubbles": [{"text": "Aichat test", "type": "user"}]
        }]
    })
    .to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params![
            "workbench.panel.aichat.view.aichat.chatdata",
            value.as_str()
        ],
    )
    .unwrap();
    drop(conn);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    assert_eq!(convs.len(), 1);
}

#[test]
fn extract_from_db_handles_empty_db() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let _conn = create_test_db(&db_path);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn extract_from_db_fails_for_missing_file() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("nonexistent.vscdb");

    let result = CursorConnector::extract_from_db(&db_path, None, &|| {});
    assert!(result.is_err());
}

// Cursor inserts internal marker rows into `cursorDiskKV` with NULL `value`
// (e.g. composer metadata stubs). Without filtering, `row.get_typed::<String>(1)?`
// on NULL aborts the row mapper, the `if let Ok(rows) = ...` swallows the error,
// and the whole connector silently returns zero conversations. Regression test:
// a single NULL row must not mask the valid rows beside it. (PR #8)
#[test]
fn extract_from_db_skips_null_value_rows_in_cursor_disk_kv() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let conn = create_test_db(&db_path);
    // One valid composer entry.
    let value = json!({ "text": "Valid conversation" }).to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
        params!["composerData:valid-row", value.as_str()],
    )
    .unwrap();
    // One internal-marker entry with NULL value sitting in the same prefix range.
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?, NULL)",
        params!["composerData:null-marker"],
    )
    .unwrap();
    drop(conn);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "NULL-value row must not abort the row mapper for valid sibling rows"
    );
    assert!(convs[0].messages[0].content.contains("Valid conversation"));
}

#[test]
fn extract_from_db_skips_null_value_rows_in_item_table() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.vscdb");

    let conn = create_test_db(&db_path);
    let value = json!({
        "tabs": [{ "bubbles": [{"text": "Valid aichat", "type": "user"}] }]
    })
    .to_string();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
        params![
            "workbench.panel.aichat.view.aichat.chatdata",
            value.as_str()
        ],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO ItemTable (key, value) VALUES (?, NULL)",
        params!["workbench.panel.aichat.view.composer.null-marker"],
    )
    .unwrap();
    drop(conn);

    let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
    assert_eq!(convs.len(), 1);
}

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::redundant_closure_for_method_calls,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

use super::*;
use crate::connectors::scan::ScanRoot;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

// =========================================================================
// Constructor tests
// =========================================================================

#[test]
fn connector_with_key_stores_key() {
    let key_bytes = [42u8; KEY_SIZE];
    let connector = ChatGptConnector {
        encryption_key: Some(key_bytes),
    };
    assert!(connector.encryption_key.is_some());
    assert_eq!(connector.encryption_key.unwrap(), key_bytes);
}

/// `coding_agent_session_search-srzwm`: pin the SSH-style key-file
/// permission discipline. Mode 0644 / 0664 / any group-or-other
/// permission MUST be refused; 0600 (and stricter) MUST pass.
#[test]
#[cfg(unix)]
fn chatgpt_key_file_mode_is_safe_refuses_permissive_modes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let key_path = dir.path().join("chatgpt_key.bin");
    fs::write(&key_path, [0u8; KEY_SIZE]).unwrap();

    // 0600 — strict, must pass.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        ChatGptConnector::chatgpt_key_file_mode_is_safe(&key_path),
        "0600 mode must be accepted"
    );

    // 0644 — world-readable, must be refused.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        !ChatGptConnector::chatgpt_key_file_mode_is_safe(&key_path),
        "0644 mode (world-readable) MUST be refused — leaks AES-256 key to other users"
    );

    // 0660 — group-readable, must be refused.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o660)).unwrap();
    assert!(
        !ChatGptConnector::chatgpt_key_file_mode_is_safe(&key_path),
        "0660 mode (group-readable) MUST be refused"
    );

    // 0400 — read-only owner, must pass.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(
        ChatGptConnector::chatgpt_key_file_mode_is_safe(&key_path),
        "0400 mode must be accepted"
    );
}

/// `coding_agent_session_search-srzwm`: symlinks at the key file
/// path MUST be refused. A symlink to /dev/null or /etc/passwd
/// would silently load the wrong bytes as a "key" — bad UX (the
/// decrypt fails further downstream with a confusing error)
/// AND a TOCTOU surface (the symlink target can change between
/// stat and read).
#[test]
#[cfg(unix)]
fn chatgpt_key_file_mode_is_safe_refuses_symlinks() {
    let dir = TempDir::new().unwrap();
    let real_key = dir.path().join("real_key.bin");
    fs::write(&real_key, [0u8; KEY_SIZE]).unwrap();
    let link_path = dir.path().join("symlinked_key.bin");
    std::os::unix::fs::symlink(&real_key, &link_path).unwrap();
    assert!(
        !ChatGptConnector::chatgpt_key_file_mode_is_safe(&link_path),
        "symlink at the chatgpt key file path MUST be refused"
    );
}

#[test]
fn connector_without_key_stores_none() {
    let connector = ChatGptConnector {
        encryption_key: None,
    };
    assert!(connector.encryption_key.is_none());
}

// =========================================================================
// find_conversation_dirs tests
// =========================================================================

#[test]
fn find_conversation_dirs_empty_for_nonexistent() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().join("nonexistent");

    let dirs = ChatGptConnector::find_conversation_dirs(&base);
    assert!(dirs.is_empty());
}

#[test]
fn find_conversation_dirs_detects_v1_unencrypted() {
    let dir = TempDir::new().unwrap();
    let conv_dir = dir.path().join("conversations-abc123");
    fs::create_dir_all(&conv_dir).unwrap();

    let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

    assert_eq!(dirs.len(), 1);
    assert!(!dirs[0].1); // Not encrypted
}

#[test]
fn find_conversation_dirs_detects_v2_encrypted() {
    let dir = TempDir::new().unwrap();
    let conv_dir = dir.path().join("conversations-v2-abc123");
    fs::create_dir_all(&conv_dir).unwrap();

    let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].1); // Encrypted
}

#[test]
fn find_conversation_dirs_detects_v3_encrypted() {
    let dir = TempDir::new().unwrap();
    let conv_dir = dir.path().join("conversations-v3-abc123");
    fs::create_dir_all(&conv_dir).unwrap();

    let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].1); // Encrypted
}

#[test]
fn find_conversation_dirs_mixed_versions() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("conversations-old")).unwrap();
    fs::create_dir_all(dir.path().join("conversations-v2-new")).unwrap();
    fs::create_dir_all(dir.path().join("other-folder")).unwrap();

    let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());

    assert_eq!(dirs.len(), 2);
    // One encrypted, one not
    let encrypted_count = dirs.iter().filter(|(_, enc)| *enc).count();
    let unencrypted_count = dirs.iter().filter(|(_, enc)| !*enc).count();
    assert_eq!(encrypted_count, 1);
    assert_eq!(unencrypted_count, 1);
}

#[test]
fn find_conversation_dirs_returns_sorted_order() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("conversations-zed")).unwrap();
    fs::create_dir_all(dir.path().join("conversations-alpha")).unwrap();

    let dirs = ChatGptConnector::find_conversation_dirs(&dir.path().to_path_buf());
    assert_eq!(dirs.len(), 2);

    let names: Vec<_> = dirs
        .iter()
        .map(|(path, _)| {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(names, vec!["conversations-alpha", "conversations-zed"]);
}

#[test]
fn conversation_files_returns_sorted_order() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("z.json"), "{}").unwrap();
    fs::write(dir.path().join("a.json"), "{}").unwrap();
    fs::write(dir.path().join("ignore.txt"), "x").unwrap();

    let files = ChatGptConnector::conversation_files(dir.path());
    assert_eq!(files.len(), 2);

    let names: Vec<_> = files
        .iter()
        .map(|path| {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(names, vec!["a.json", "z.json"]);
}

// =========================================================================
// decrypt_file tests
// =========================================================================

#[test]
fn decrypt_file_fails_without_key() {
    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let data = vec![0u8; 100];

    let result = connector.decrypt_file(&data);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("No encryption key")
    );
}

#[test]
fn decrypt_file_fails_for_too_short_data() {
    let connector = ChatGptConnector {
        encryption_key: Some([0u8; KEY_SIZE]),
    };
    // Less than NONCE_SIZE + TAG_SIZE
    let data = vec![0u8; 10];

    let result = connector.decrypt_file(&data);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("too short"));
}

#[test]
fn decrypt_file_fails_with_wrong_key() {
    let connector = ChatGptConnector {
        encryption_key: Some([0u8; KEY_SIZE]),
    };
    // Valid-length but garbage data
    let data = vec![0u8; NONCE_SIZE + 50 + TAG_SIZE];

    let result = connector.decrypt_file(&data);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Decryption failed")
    );
}

// =========================================================================
// parse_conversation_file tests (mapping format)
// =========================================================================

#[test]
fn parse_mapping_format_conversation() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv1.json");

    let conv_json = json!({
        "id": "conv-123",
        "title": "Test Conversation",
        "mapping": {
            "node1": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hello, ChatGPT!"]},
                    "create_time": 1700000000.123
                }
            },
            "node2": {
                "parent": "node1",
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": ["Hello! How can I help you?"]},
                    "create_time": 1700000001.456,
                    "metadata": {"model_slug": "gpt-4"}
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    assert!(result.is_ok());
    let conv = result.unwrap().unwrap();

    assert_eq!(conv.agent_slug, "chatgpt");
    assert_eq!(conv.external_id, Some("conv-123".to_string()));
    assert_eq!(conv.title, Some("Test Conversation".to_string()));
    assert_eq!(conv.messages.len(), 2);

    // Check first message (user)
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[0].content, "Hello, ChatGPT!");

    // Check second message (assistant with model)
    assert_eq!(conv.messages[1].role, "assistant");
    assert!(conv.messages[1].content.contains("How can I help"));
    assert_eq!(conv.messages[1].author, Some("gpt-4".to_string()));
}

#[test]
fn parse_mapping_preserves_ended_at_with_missing_timestamp() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv_missing_ts.json");

    let conv_json = json!({
        "id": "conv-missing-ts",
        "mapping": {
            "node1": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["First message"]},
                    "create_time": 1700000000.0
                }
            },
            "node2": {
                "parent": "node1",
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": ["Second message without timestamp"]}
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].content, "First message");
    assert_eq!(conv.messages[1].content, "Second message without timestamp");
    assert_eq!(conv.started_at, Some(1_700_000_000_000));
    assert_eq!(conv.ended_at, Some(1_700_000_000_000));
}

#[test]
fn parse_skips_system_messages() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv.json");

    let conv_json = json!({
        "id": "conv-sys",
        "mapping": {
            "sys": {
                "parent": null,
                "message": {
                    "author": {"role": "system"},
                    "content": {"parts": ["You are a helpful assistant."]},
                    "create_time": 1700000000.0
                }
            },
            "user": {
                "parent": "sys",
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hi!"]},
                    "create_time": 1700000001.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    // System message should be skipped
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].role, "user");
}

#[test]
fn parse_skips_empty_content() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv.json");

    let conv_json = json!({
        "id": "conv-empty",
        "mapping": {
            "empty": {
                "parent": null,
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": [""]},
                    "create_time": 1700000000.0
                }
            },
            "whitespace": {
                "parent": "empty",
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["   \n\t  "]},
                    "create_time": 1700000001.0
                }
            },
            "valid": {
                "parent": "whitespace",
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Valid message"]},
                    "create_time": 1700000002.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    // Empty/whitespace messages should be skipped
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].content, "Valid message");
}

// =========================================================================
// parse_conversation_file tests (simple messages array format)
// =========================================================================

#[test]
fn parse_simple_messages_array_format() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv.json");

    let conv_json = json!({
        "id": "simple-conv",
        "title": "Simple Format",
        "messages": [
            {
                "role": "user",
                "content": "Question?",
                "timestamp": 1700000000000i64
            },
            {
                "role": "assistant",
                "content": "Answer!",
                "timestamp": 1700000001000i64
            }
        ]
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[0].content, "Question?");
    assert_eq!(conv.messages[1].role, "assistant");
    assert_eq!(conv.messages[1].content, "Answer!");
}

#[test]
fn parse_content_with_text_field() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv.json");

    let conv_json = json!({
        "id": "text-content",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"text": "Using text field instead of parts"},
                    "create_time": 1700000000.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages.len(), 1);
    assert!(conv.messages[0].content.contains("text field"));
}

// =========================================================================
// Edge case tests
// =========================================================================

#[test]
fn parse_returns_none_for_empty_conversation() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("empty.json");

    let conv_json = json!({
        "id": "empty-conv",
        "mapping": {}
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    assert!(result.is_ok());
    assert!(result.unwrap().is_none());
}

#[test]
fn parse_handles_missing_optional_fields() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("minimal.json");

    // Minimal conversation - no id, no title, no timestamps
    let conv_json = json!({
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Minimal message"]}
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages.len(), 1);
    // external_id falls back to filename stem
    assert!(conv.external_id.is_some());
    assert!(conv.title.is_none());
    assert!(conv.started_at.is_none());
}

#[test]
fn parse_extracts_id_from_conversation_id_field() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("conv.json");

    let conv_json = json!({
        "conversation_id": "alt-id-123",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Test"]}
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.external_id, Some("alt-id-123".to_string()));
}

#[test]
fn parse_fails_gracefully_for_invalid_json() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("invalid.json");

    fs::write(&conv_file, "not valid json {{{").unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    assert!(result.is_err());
}

#[test]
fn parse_multipart_content_joins_with_newlines() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("multipart.json");

    let conv_json = json!({
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Part 1", "Part 2", "Part 3"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages[0].content, "Part 1\nPart 2\nPart 3");
}

#[test]
fn parse_sets_metadata_correctly() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("meta.json");

    let conv_json = json!({
        "model": "gpt-4-turbo",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Test"]}
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.metadata["source"], "chatgpt_desktop");
    assert_eq!(conv.metadata["model"], "gpt-4-turbo");
    assert_eq!(conv.metadata["encrypted"], false);
}

#[test]
fn parse_encrypted_flag_in_metadata() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("enc.json");

    let conv_json = json!({
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Test"]}
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    // Parse as unencrypted first to test flag
    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);
    let conv = result.unwrap().unwrap();
    assert_eq!(conv.metadata["encrypted"], false);
}

// =========================================================================
// Timestamps and ordering tests
// =========================================================================

#[test]
fn parse_orders_messages_by_create_time() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("ordered.json");

    // Messages in reverse order in JSON
    let conv_json = json!({
        "mapping": {
            "late": {
                "message": {
                    "author": {"role": "assistant"},
                    "content": {"parts": ["Second"]},
                    "create_time": 1700000002.0
                }
            },
            "early": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["First"]},
                    "create_time": 1700000001.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    // Should be ordered by create_time
    assert_eq!(conv.messages[0].content, "First");
    assert_eq!(conv.messages[1].content, "Second");
}

#[test]
fn parse_converts_float_timestamp_to_millis() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("ts.json");

    let conv_json = json!({
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Test"]},
                    "create_time": 1700000000.5  // .5 seconds
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    // Should be converted to milliseconds
    assert_eq!(conv.messages[0].created_at, Some(1700000000500));
}

// =========================================================================
// Detection tests
// =========================================================================

#[test]
fn detect_not_found_without_app_dir() {
    let connector = ChatGptConnector {
        encryption_key: None,
    };

    let result = connector.detect();
    let _ = result.detected;
}

// =========================================================================
// Scan tests
// =========================================================================

#[test]
fn scan_empty_directory_returns_empty() {
    let dir = TempDir::new().unwrap();
    let connector = ChatGptConnector {
        encryption_key: None,
    };

    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn scan_skips_encrypted_without_key() {
    let dir = TempDir::new().unwrap();

    // Create encrypted directory structure
    let conv_dir = dir.path().join("conversations-v2-abc123");
    fs::create_dir_all(&conv_dir).unwrap();

    // Create a conversation file (will be skipped as encrypted)
    let conv_json = json!({
        "id": "test",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Test"]}
                }
            }
        }
    });
    fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };

    // Use a context that will make it look for conversations in our dir
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    // Encrypted dirs are skipped without key
    assert!(result.unwrap().is_empty());
}

#[test]
fn scan_processes_unencrypted_conversations() {
    let dir = TempDir::new().unwrap();

    // Create a directory structure that mimics com.openai.chat
    let openai_dir = dir.path().join("com.openai.chat");
    fs::create_dir_all(&openai_dir).unwrap();

    // Create unencrypted directory structure
    let conv_dir = openai_dir.join("conversations-uuid123");
    fs::create_dir_all(&conv_dir).unwrap();

    let conv_json = json!({
        "id": "test-conv",
        "title": "Test Title",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hello!"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });
    fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };

    // Pass the openai directory so scan recognizes it
    let ctx = ScanContext::local_default(openai_dir.clone(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, Some("Test Title".to_string()));
}

#[test]
fn scan_accepts_base_with_conversations_dir() {
    let dir = TempDir::new().unwrap();

    let conv_dir = dir.path().join("conversations-uuid123");
    fs::create_dir_all(&conv_dir).unwrap();

    let conv_json = json!({
        "id": "test-conv",
        "title": "Direct Base",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hello!"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });
    fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };

    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, Some("Direct Base".to_string()));
}

#[test]
fn scan_with_application_support_scan_root() {
    let dir = TempDir::new().unwrap();
    let app_support = dir.path().join("Library").join("Application Support");
    let openai_dir = app_support.join("com.openai.chat");
    let conv_dir = openai_dir.join("conversations-uuid123");
    fs::create_dir_all(&conv_dir).unwrap();

    let conv_json = json!({
        "id": "test-conv",
        "title": "App Support",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Hello!"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });
    fs::write(conv_dir.join("conv.json"), conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };

    let ctx = ScanContext::with_roots(PathBuf::new(), vec![ScanRoot::local(app_support)], None);
    let result = connector.scan(&ctx);

    assert!(result.is_ok());
    let convs = result.unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title, Some("App Support".to_string()));
}

// =========================================================================
// Edge case tests — malformed input robustness
// =========================================================================

#[test]
fn parse_truncated_json_returns_error() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("truncated.json");
    fs::write(
        &conv_file,
        r#"{"id": "trunc", "mapping": {"node1": {"message":"#,
    )
    .unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);
    assert!(result.is_err(), "truncated JSON should return an error");
}

#[test]
fn parse_type_mismatch_in_mapping_nodes_handled() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("mismatch.json");

    let conv_json = json!({
        "mapping": {
            // Node with string instead of object for message
            "bad1": {
                "message": "not an object"
            },
            // Node with null message
            "bad2": {
                "message": null
            },
            // Node without message key
            "bad3": {
                "parent": "bad2"
            },
            // Node with number instead of object
            "bad4": {
                "message": 42
            },
            // Valid node
            "good": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Valid"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    assert!(result.is_ok());
    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].content, "Valid");
}

#[test]
fn parse_deeply_nested_json_does_not_stack_overflow() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("deep.json");

    // Build deeply nested JSON (200 levels)
    let mut nested = String::new();
    for _ in 0..200 {
        nested.push_str("{\"a\":");
    }
    nested.push('1');
    for _ in 0..200 {
        nested.push('}');
    }
    fs::write(&conv_file, &nested).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    // Should not panic; may error due to recursion limit
    let result = connector.parse_conversation_file(&conv_file, None, false);
    if let Ok(conv) = result {
        assert!(
            conv.is_none(),
            "deep JSON without mapping should return None"
        );
    }
}

#[test]
fn parse_large_message_body_handled() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("large.json");

    let large_content = "x".repeat(1_000_000);
    let conv_json = json!({
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": [large_content]},
                    "create_time": 1700000000.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages[0].content.len(), 1_000_000);
}

#[test]
fn parse_null_bytes_in_content_handled() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("nullbytes.json");

    let conv_json = json!({
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["before\u{0000}after"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);
    assert!(result.is_ok(), "null bytes should not cause errors");
}

#[test]
fn parse_nonexistent_file_returns_error() {
    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(
        &PathBuf::from("/nonexistent/file.json"),
        None,
        false,
    );
    assert!(result.is_err());
}

#[test]
fn parse_mapping_nodes_without_content_parts_or_text_skipped() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("no_content.json");

    let conv_json = json!({
        "mapping": {
            // Message with content that has neither parts nor text
            "bad": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"type": "unknown"},
                    "create_time": 1700000000.0
                }
            },
            // Message with parts containing non-string values
            "bad2": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": [42, null, true]},
                    "create_time": 1700000001.0
                }
            },
            // Valid message
            "good": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Valid"]},
                    "create_time": 1700000002.0
                }
            }
        }
    });

    fs::write(&conv_file, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);

    let conv = result.unwrap().unwrap();
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].content, "Valid");
}

#[test]
fn parse_invalid_utf8_file_returns_error() {
    let dir = TempDir::new().unwrap();
    let conv_file = dir.path().join("bad_utf8.json");
    fs::write(&conv_file, b"\xFF\xFE invalid utf8 bytes").unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let result = connector.parse_conversation_file(&conv_file, None, false);
    assert!(result.is_err(), "invalid UTF-8 should return an error");
}

#[test]
fn scan_mixed_valid_and_invalid_conversation_files() {
    let dir = TempDir::new().unwrap();
    let conv_dir = dir.path().join("conversations-uuid123");
    fs::create_dir_all(&conv_dir).unwrap();

    // Invalid JSON file
    fs::write(conv_dir.join("bad.json"), "not json").unwrap();

    // Valid conversation
    let good_json = json!({
        "id": "good-conv",
        "mapping": {
            "node1": {
                "message": {
                    "author": {"role": "user"},
                    "content": {"parts": ["Valid conversation"]},
                    "create_time": 1700000000.0
                }
            }
        }
    });
    fs::write(conv_dir.join("good.json"), good_json.to_string()).unwrap();

    // Empty conversation
    let empty_json = json!({"id": "empty", "mapping": {}});
    fs::write(conv_dir.join("empty.json"), empty_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
    let convs = connector.scan(&ctx).unwrap();

    assert_eq!(convs.len(), 1, "should extract only the valid conversation");
    assert_eq!(convs[0].messages[0].content, "Valid conversation");
    crate::connectors::assert_discovery_covers_scan_sources(&connector, &ctx);
}

#[test]
fn parse_conversation_extracts_tool_calls_plugin_and_recipient_invocations() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("conv_tools.json");
    let conv_json = json!({
        "id": "conv-tools",
        "title": "Tool session",
        "mapping": {
            "n1": {
                "message": {
                    "id": "m1",
                    "author": {"role": "assistant"},
                    "recipient": "python",
                    "content": {"content_type": "code", "text": "print(6 * 7)"},
                    "create_time": 1700000001.0,
                    "metadata": {
                        "model_slug": "gpt-4o",
                        "invoked_plugin": {"namespace": "Wolfram", "plugin_id": "plug_1"},
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "skill",
                                    "arguments": "{\"name\":\"review\"}"
                                }
                            }
                        ]
                    }
                }
            }
        }
    });
    fs::write(&file_path, conv_json.to_string()).unwrap();

    let connector = ChatGptConnector {
        encryption_key: None,
    };
    let conv = connector
        .parse_conversation_file(&file_path, None, false)
        .unwrap()
        .expect("conversation parsed");
    assert_eq!(conv.messages.len(), 1);
    let msg = &conv.messages[0];
    assert_eq!(msg.invocations.len(), 3);
    assert_eq!(msg.invocations[0].kind, "skill");
    assert_eq!(msg.invocations[0].name, "review");
    assert_eq!(msg.invocations[0].raw_name.as_deref(), Some("skill"));
    assert_eq!(msg.invocations[1].name, "Wolfram");
    assert_eq!(msg.invocations[1].call_id.as_deref(), Some("plug_1"));
    assert_eq!(msg.invocations[2].name, "python");
    assert_eq!(
        msg.extra
            .pointer("/cass/tool_call_count")
            .and_then(|v| v.as_u64()),
        Some(3)
    );
}

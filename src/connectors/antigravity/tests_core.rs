use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

use super::parse::{
    extract_tagged_blocks, extract_user_request, model_from_settings, parse_tool_calls,
    record_to_message,
};
use super::scan::AntigravityLayout;
use super::{AGY_TESTED_VERSIONS, AntigravityConnector, agy_version_is_tested, parse_agy_version};
use crate::connectors::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use crate::connectors::{Connector, assert_discovery_covers_scan_sources};
use crate::types::NormalizedConversation;

const FIXTURE_UUID: &str = "f1e2d3c4-b5a6-4789-9abc-def012345678";

/// The base directory (`brain/` + `conversations/`) of the checked-in
/// fixture. It is not under a `.gemini/` parent, so it is deliberately
/// unclassified (neither IDE nor CLI).
fn fixture_base() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("antigravity")
}

fn fixture_conversation_dir() -> PathBuf {
    fixture_base().join("brain").join(FIXTURE_UUID)
}

fn fixture_ctx() -> ScanContext {
    let base = fixture_base();
    ScanContext::with_roots(base.clone(), vec![ScanRoot::local(base)], None)
}

fn only_conversation() -> NormalizedConversation {
    let convs = AntigravityConnector::new().scan(&fixture_ctx()).unwrap();
    assert_eq!(convs.len(), 1, "fixture is a single conversation");
    convs.into_iter().next().unwrap()
}

#[test]
fn default_detection_scopes_to_antigravity_base_data_dir() {
    // The checked-in fixture base has the `brain/<uuid>` shape; default
    // detection with this base as data_dir must scan HERE, not the
    // machine's real ~/.gemini/antigravity-cli.
    let ctx = ScanContext::local_default(fixture_base(), None);
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(
        convs.len(),
        1,
        "default detection with an antigravity-base data_dir must scan that base"
    );
    assert_eq!(convs[0].external_id.as_deref(), Some(FIXTURE_UUID));
}

#[test]
fn new_creates_connector() {
    let _ = AntigravityConnector::new();
    let _ = AntigravityConnector;
}

#[test]
fn is_conversation_dir_requires_transcript() {
    let dir = TempDir::new().unwrap();
    assert!(!AntigravityConnector::is_conversation_dir(dir.path()));
    let logs = dir.path().join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    // Either transcript variant marks the directory (native parity):
    // transcript_full.jsonl alone is enough, transcript.jsonl too.
    fs::write(logs.join("transcript_full.jsonl"), "{}\n").unwrap();
    assert!(AntigravityConnector::is_conversation_dir(dir.path()));
    fs::write(logs.join("transcript.jsonl"), "{}\n").unwrap();
    assert!(AntigravityConnector::is_conversation_dir(dir.path()));
}

#[test]
fn extract_tagged_blocks_handles_multiple_and_missing() {
    assert_eq!(
        extract_tagged_blocks("<A>one</A> mid <A>two</A>", "A"),
        vec!["one".to_string(), "two".to_string()]
    );
    assert!(extract_tagged_blocks("no tags here", "A").is_empty());
    // Unterminated open tag does not panic or loop forever.
    assert!(extract_tagged_blocks("<A>oops", "A").is_empty());
}

#[test]
fn extract_user_request_unwraps_and_keeps_settings() {
    let raw = "<USER_REQUEST>\ndo the thing\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nchanged to Gemini 3.1 Pro (High). ok\n</USER_SETTINGS_CHANGE>";
    let (body, settings, metadata) = extract_user_request(raw);
    assert_eq!(body, "do the thing");
    assert!(settings.unwrap().contains("Gemini 3.1 Pro (High)"));
    assert_eq!(metadata.as_deref(), Some("time"));
}

#[test]
fn extract_user_request_falls_back_to_raw_when_unwrapped() {
    let (body, settings, metadata) = extract_user_request("plain prompt with no tags");
    assert_eq!(body, "plain prompt with no tags");
    assert!(settings.is_none() && metadata.is_none());
}

#[test]
fn model_from_settings_extracts_human_name() {
    assert_eq!(
        model_from_settings(
            "The user changed setting `Model Selection` from None to Gemini 3.1 Pro (High). No need to comment."
        )
        .as_deref(),
        Some("Gemini 3.1 Pro (High)")
    );
    assert_eq!(model_from_settings("changed from X to None. done"), None);
    // An earlier " to " (in a setting name / prior value) must NOT hijack the
    // parse: anchor on " from " then the next " to ".
    assert_eq!(
        model_from_settings(
            "changed setting `Auto to Stop` from None to Gemini 3.1 Pro (High). ok"
        )
        .as_deref(),
        Some("Gemini 3.1 Pro (High)")
    );
    // No " from " marker: fall back to the first " to ".
    assert_eq!(
        model_from_settings("set model to Gemini 3.1 Pro (High). thanks").as_deref(),
        Some("Gemini 3.1 Pro (High)")
    );
}

#[test]
fn parse_tool_calls_synthesizes_name_from_type_when_absent() {
    let invs = parse_tool_calls(None, "VIEW_FILE");
    assert_eq!(invs.len(), 1);
    assert_eq!(invs[0].name, "view_file");
    assert_eq!(invs[0].kind, "tool");
    assert!(invs[0].arguments.is_none());
}

#[test]
fn parse_tool_calls_reads_structured_object_args() {
    let tc = serde_json::json!([
        {"name":"run_command","id":"c1","arguments":{"command":"echo hi"}}
    ]);
    let invs = parse_tool_calls(Some(&tc), "RUN_COMMAND");
    assert_eq!(invs[0].name, "run_command");
    assert_eq!(invs[0].call_id.as_deref(), Some("c1"));
    assert_eq!(invs[0].arguments.as_ref().unwrap()["command"], "echo hi");
}

#[test]
fn parse_tool_calls_parses_stringified_json_args() {
    let tc = serde_json::json!([
        {"tool_name":"edit_file","id":"c2","arguments":"{\"path\":\"out.txt\"}"}
    ]);
    let invs = parse_tool_calls(Some(&tc), "EDIT_FILE");
    assert_eq!(invs[0].name, "edit_file");
    assert_eq!(invs[0].arguments.as_ref().unwrap()["path"], "out.txt");
}

#[test]
fn scan_parses_fixture_metadata_and_roles() {
    let conv = only_conversation();
    assert_eq!(conv.agent_slug, "antigravity");
    assert_eq!(conv.external_id.as_deref(), Some(FIXTURE_UUID));
    assert_eq!(conv.metadata["model"], "Gemini 3.1 Pro (High)");
    assert_eq!(conv.metadata["source"], "antigravity");
    assert!(conv.title.as_deref().unwrap().starts_with("Read data.txt"));
    assert!(conv.started_at.unwrap() <= conv.ended_at.unwrap());
    // The fixture's transcript_full.jsonl is as fresh as transcript.jsonl,
    // so the native priority rule selects it as the parse source.
    assert_eq!(
        conv.source_path,
        AntigravityConnector::transcript_full_path(&fixture_conversation_dir())
    );

    // CONVERSATION_HISTORY is dropped; every other record becomes a message.
    assert_eq!(conv.messages.len(), 9);
    for (i, m) in conv.messages.iter().enumerate() {
        assert_eq!(m.idx, i64::try_from(i).unwrap());
    }
    assert!(conv.messages.iter().any(|m| m.role == "user"));
    assert!(conv.messages.iter().any(|m| m.role == "assistant"));
    assert!(conv.messages.iter().any(|m| m.role == "tool"));
    assert!(conv.messages.iter().any(|m| m.role == "system"));
}

#[test]
fn user_message_unwraps_request_and_keeps_settings_extra() {
    let conv = only_conversation();
    let user = conv.messages.iter().find(|m| m.role == "user").unwrap();
    assert!(user.content.starts_with("Read data.txt"));
    assert!(!user.content.contains("USER_REQUEST"));
    assert!(
        user.extra["settings_change"]
            .as_str()
            .unwrap()
            .contains("Gemini 3.1 Pro (High)")
    );
    assert!(user.extra.get("additional_metadata").is_some());
}

#[test]
fn planner_response_preserves_thinking() {
    let conv = only_conversation();
    let planner = conv
        .messages
        .iter()
        .find(|m| m.role == "assistant" && m.extra.get("thinking").is_some())
        .expect("a planner response with thinking");
    assert!(planner.content.contains("data.txt"));
    assert!(
        planner.extra["thinking"]
            .as_str()
            .unwrap()
            .contains("Planning the steps")
    );
}

// cass review 2026-11-02 F3: real producer shape.
#[test]
fn planner_tool_calls_only_record_is_kept_with_calls() {
    let rec = serde_json::json!({
        "step_index": 4, "source": "MODEL", "type": "PLANNER_RESPONSE",
        "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
        "tool_calls": [{"name": "view_file", "args": {"AbsolutePath": "/a.rs"}}]
    });
    let msg = record_to_message(&rec).expect("tool-call-only planner record is kept");
    assert_eq!(msg.role, "assistant");
    assert_eq!(msg.content, "[Tool: view_file]");
    assert_eq!(msg.invocations.len(), 1);
    assert_eq!(msg.invocations[0].name, "view_file");
    assert_eq!(
        msg.extra["cass"]["invocations"][0]["arguments"]["AbsolutePath"],
        "/a.rs"
    );
}

#[test]
fn planner_text_thinking_and_calls_keep_all_three() {
    let rec = serde_json::json!({
        "source": "MODEL", "type": "PLANNER_RESPONSE", "created_at": "2026-06-11T20:14:43Z",
        "content": "reading it", "thinking": "plan",
        "tool_calls": [{"name": "view_file", "args": {}}]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.content, "reading it");
    assert_eq!(msg.extra["thinking"], "plan");
    assert_eq!(msg.invocations[0].name, "view_file");
}

#[test]
fn generic_result_does_not_invent_a_generic_tool() {
    let rec = serde_json::json!({
        "source": "MODEL", "type": "GENERIC", "created_at": "2026-06-11T20:14:44Z",
        "content": "file contents"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "tool");
    assert!(
        msg.invocations.is_empty(),
        "GENERIC is a result, not a tool call"
    );
}

#[test]
fn empty_planner_without_calls_is_still_dropped() {
    let rec = serde_json::json!({"source": "MODEL", "type": "PLANNER_RESPONSE", "content": ""});
    assert!(record_to_message(&rec).is_none());
}

#[test]
fn view_file_becomes_tool_message_with_synthesized_invocation() {
    let conv = only_conversation();
    let view = conv
        .messages
        .iter()
        .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("VIEW_FILE"))
        .expect("VIEW_FILE message");
    assert_eq!(view.role, "tool");
    assert!(view.content.contains("1234"));
    assert_eq!(view.invocations[0].name, "view_file");
    assert!(view.invocations[0].arguments.is_none());
}

#[test]
fn run_command_invocation_carries_structured_args() {
    let conv = only_conversation();
    let run = conv
        .messages
        .iter()
        .find(|m| m.invocations.iter().any(|i| i.name == "run_command"))
        .expect("run_command message");
    assert_eq!(run.role, "tool");
    assert!(run.content.contains("HELLO_FROM_AGY"));
    let inv = &run.invocations[0];
    assert_eq!(inv.call_id.as_deref(), Some("call_run_0001"));
    assert_eq!(
        inv.arguments.as_ref().unwrap()["command"],
        "echo HELLO_FROM_AGY"
    );
}

#[test]
fn edit_file_invocation_parses_stringified_args() {
    let conv = only_conversation();
    let edit = conv
        .messages
        .iter()
        .find(|m| m.invocations.iter().any(|i| i.name == "edit_file"))
        .expect("edit_file message");
    let inv = &edit.invocations[0];
    assert_eq!(inv.call_id.as_deref(), Some("call_edit_0002"));
    assert_eq!(inv.arguments.as_ref().unwrap()["path"], "out.txt");
    assert_eq!(inv.arguments.as_ref().unwrap()["content"], "DONE");
}

#[test]
fn ephemeral_and_system_messages_preserved_as_system() {
    let conv = only_conversation();
    let ephemeral = conv
        .messages
        .iter()
        .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("EPHEMERAL_MESSAGE"))
        .expect("ephemeral message");
    assert_eq!(ephemeral.role, "system");
    assert_eq!(ephemeral.author.as_deref(), Some("ephemeral"));

    let system = conv
        .messages
        .iter()
        .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("SYSTEM_MESSAGE"))
        .expect("system message");
    assert_eq!(system.role, "system");
    assert_eq!(system.author.as_deref(), Some("system"));
}

#[test]
fn unknown_model_step_type_is_preserved_not_dropped() {
    // BROWSER_PREVIEW is an unknown-to-us MODEL kind; the reader must never
    // crash or silently drop it (agy version-compat robustness).
    let conv = only_conversation();
    let unknown = conv
        .messages
        .iter()
        .find(|m| m.extra.get("agy_type").and_then(Value::as_str) == Some("BROWSER_PREVIEW"))
        .expect("unknown BROWSER_PREVIEW message preserved");
    assert_eq!(unknown.role, "tool");
    assert!(unknown.content.contains("preview"));
    assert_eq!(unknown.invocations[0].name, "browser_preview");
}

// ---------------------------------------------------------------------
// cass F10: source passthrough. The canonical mapping below extracts the
// text-bearing fields and used to drop four source signals cass needs
// (truncated_fields markers, the lifecycle `status`, the `error` text,
// `media` attachment refs) plus two whole record shapes (error-only
// ERROR_MESSAGE, text-bearing CONVERSATION_HISTORY). All fixtures here
// are synthetic — made-up text, real key/type structure only.
// ---------------------------------------------------------------------

#[test]
fn truncated_fields_pass_through_to_extra() {
    // Real shape: `truncated_fields` is a list of the source field names
    // that were truncated before persistence (cass F10 item 1).
    let rec = serde_json::json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
        "content": "<USER_REQUEST>\ninspect the widget rack\n</USER_REQUEST>",
        "truncated_fields": ["content"]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "user");
    assert_eq!(
        msg.extra["truncated_fields"],
        serde_json::json!(["content"])
    );

    // PLANNER_RESPONSE markers (thinking/tool_calls truncations are the
    // real combinations) ride along without touching the content.
    let rec = serde_json::json!({
        "step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE",
        "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
        "content": "partial narration about the rack",
        "truncated_fields": ["thinking", "tool_calls"]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.content, "partial narration about the rack");
    assert_eq!(
        msg.extra["truncated_fields"],
        serde_json::json!(["thinking", "tool_calls"])
    );

    // No marker in the source → no key in extra.
    let rec = serde_json::json!({
        "source": "MODEL", "type": "GENERIC",
        "created_at": "2026-06-11T20:14:44Z", "content": "result text"
    });
    let msg = record_to_message(&rec).unwrap();
    assert!(msg.extra.get("truncated_fields").is_none());
}

#[test]
fn error_only_error_message_is_emitted_with_error_text() {
    // Real shape: ERROR_MESSAGE records with no `content`, the failure
    // text only in `error` — dropped whole before cass F10 item 2.
    let rec = serde_json::json!({
        "step_index": 7, "source": "SYSTEM", "type": "ERROR_MESSAGE",
        "status": "DONE", "created_at": "2026-06-11T20:14:47Z",
        "error": "the widget pipeline collapsed"
    });
    let msg = record_to_message(&rec).expect("error-only ERROR_MESSAGE must not vanish");
    assert_eq!(msg.role, "system");
    assert_eq!(msg.content, "the widget pipeline collapsed");
    assert_eq!(msg.extra["agy_type"], "ERROR_MESSAGE");
    assert_eq!(msg.extra["agy_source"], "SYSTEM");
    assert_eq!(msg.extra["error"], "the widget pipeline collapsed");

    // Content-bearing ERROR_MESSAGE keeps its content byte-for-byte and
    // now also exposes the `error` field (cass F10 item 3).
    let rec = serde_json::json!({
        "step_index": 8, "source": "SYSTEM", "type": "ERROR_MESSAGE",
        "status": "DONE", "created_at": "2026-06-11T20:14:48Z",
        "content": "the widget failed to load",
        "error": "widget_error_code_9"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "system");
    assert_eq!(msg.content, "the widget failed to load");
    assert_eq!(msg.extra["error"], "widget_error_code_9");

    // Neither content nor error → still no message (nothing to emit).
    let rec = serde_json::json!({
        "source": "SYSTEM", "type": "ERROR_MESSAGE",
        "status": "DONE", "created_at": "2026-06-11T20:14:49Z"
    });
    assert!(record_to_message(&rec).is_none());
}

// ---------------------------------------------------------------------
// cass F14: archive the full raw source record. Every emitted message
// also carries the complete raw transcript record under `extra.agy_raw`,
// verbatim, alongside the canonical `agy_type` envelope — so cass can
// PROVE its block inventory from the record itself instead of guessing
// what normalization dropped. Synthetic fixtures; real key/type
// structure only.
// ---------------------------------------------------------------------

#[test]
fn record_to_message_archives_full_raw_source_record() {
    // USER_INPUT: the archived record keeps every source field verbatim
    // — wrappers, markers, media and token counts included.
    let rec = serde_json::json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
        "content": "<USER_REQUEST>\ninspect the widget rack\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal noon\n</ADDITIONAL_METADATA>",
        "input_tokens": 11, "output_tokens": 7, "cache_read_tokens": 3,
        "media": [{"mime_type": "image/png", "uri": "file:///tmp/synthetic.png"}]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "user");
    assert_eq!(
        msg.extra["agy_raw"], rec,
        "USER_INPUT record archived verbatim"
    );

    // PLANNER_RESPONSE with thinking and structured calls.
    let rec = serde_json::json!({
        "step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE",
        "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
        "content": "planning the rack inspection",
        "thinking": "considering the racks first",
        "tool_calls": [{"name": "view_file", "args": {"TargetFile": "/tmp/rack.rs"}}],
        "truncated_fields": ["tool_calls"]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "assistant");
    assert_eq!(
        msg.extra["agy_raw"], rec,
        "PLANNER_RESPONSE record archived verbatim"
    );

    // MODEL-step envelope: the `agy_type` arm keeps the raw record too.
    let rec = serde_json::json!({
        "step_index": 2, "source": "MODEL", "type": "GENERIC",
        "status": "ERROR", "created_at": "2026-06-11T20:14:44Z",
        "content": "the rack probe returned nothing",
        "error": "synthetic probe failure",
        "exit_code": 2
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "tool");
    assert_eq!(msg.extra["agy_type"], "GENERIC");
    assert_eq!(
        msg.extra["agy_raw"], rec,
        "GENERIC record archived verbatim"
    );

    // Error-only ERROR_MESSAGE: the synthesized content differs from any
    // source `content`, but the archived record is still the raw source.
    let rec = serde_json::json!({
        "step_index": 3, "source": "SYSTEM", "type": "ERROR_MESSAGE",
        "status": "DONE", "created_at": "2026-06-11T20:14:45Z",
        "error": "synthetic pipeline failure"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.content, "synthetic pipeline failure");
    assert_eq!(
        msg.extra["agy_raw"], rec,
        "error-only record archived verbatim"
    );

    // SYSTEM_MESSAGE and text-bearing CONVERSATION_HISTORY too.
    let rec = serde_json::json!({
        "step_index": 4, "source": "SYSTEM", "type": "SYSTEM_MESSAGE",
        "status": "DONE", "created_at": "2026-06-11T20:14:46Z",
        "content": "synthetic system notice text"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(
        msg.extra["agy_raw"], rec,
        "SYSTEM_MESSAGE record archived verbatim"
    );

    let rec = serde_json::json!({
        "step_index": 5, "source": "SYSTEM", "type": "CONVERSATION_HISTORY",
        "status": "DONE", "created_at": "2026-06-11T20:14:47Z",
        "content": "synthetic folded history note"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(
        msg.extra["agy_raw"], rec,
        "CONVERSATION_HISTORY record archived verbatim"
    );
}

#[test]
fn scan_archives_the_raw_record_for_every_emitted_message() {
    // End to end: every message the scan emits carries the raw record it
    // was built from, keyed to its source position (`step_index`).
    let dir = TempDir::new().unwrap();
    let logs = dir.path().join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        concat!(
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nclean the widget rack\\n</USER_REQUEST>\",\"media\":[{\"mime_type\":\"image/png\",\"uri\":\"file:///tmp/synthetic-a.png\"}]}\n",
            "{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:43Z\",\"content\":\"planning the rack cleanup\",\"truncated_fields\":[\"thinking\"]}\n",
            "{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"GENERIC\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:44Z\",\"content\":\"the rack probe returned nothing\"}\n",
        ),
    )
    .unwrap();
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert_eq!(msgs.len(), 3);
    for (i, m) in msgs.iter().enumerate() {
        let raw = m
            .extra
            .get("agy_raw")
            .expect("every emitted message archives its raw record");
        assert_eq!(raw["step_index"], i, "message {i} keyed to its record");
    }
    // The archived GENERIC record is the verbatim source line.
    assert_eq!(msgs[2].extra["agy_raw"]["type"], "GENERIC");
    assert_eq!(msgs[2].extra["agy_raw"]["status"], "DONE");
    assert_eq!(
        msgs[2].extra["agy_raw"]["content"],
        "the rack probe returned nothing"
    );
    // The archived USER_INPUT record keeps the wrapper text and media
    // the canonical mapping lifted out of the message content.
    assert_eq!(
        msgs[0].extra["agy_raw"]["content"],
        "<USER_REQUEST>\nclean the widget rack\n</USER_REQUEST>"
    );
    assert_eq!(
        msgs[0].extra["agy_raw"]["media"][0]["mime_type"],
        "image/png"
    );
}

#[test]
fn generic_error_field_is_kept_in_extras() {
    // Real shape: GENERIC result records carry an `error` field next to
    // their text; FAD dropped the field from every record before F10.
    let rec = serde_json::json!({
        "step_index": 5, "source": "MODEL", "type": "GENERIC",
        "status": "ERROR", "created_at": "2026-06-11T20:14:45Z",
        "content": "the rack probe returned nothing",
        "error": "exit status 1 from the widget builder"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "tool");
    assert_eq!(msg.content, "the rack probe returned nothing");
    assert_eq!(msg.extra["error"], "exit status 1 from the widget builder");
    assert_eq!(msg.extra["status"], "ERROR");
}

#[test]
fn media_references_are_kept_in_extras() {
    // Real shape: `media` is a list of {mime_type, uri} attachment refs.
    let rec = serde_json::json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
        "content": "<USER_REQUEST>\nlook at the attached doodle\n</USER_REQUEST>",
        "media": [
            {"mime_type": "image/png", "uri": "file:///tmp/synthetic-doodle.png"},
            {"mime_type": "text/plain", "uri": "file:///tmp/synthetic-notes.txt"}
        ]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "user");
    assert_eq!(msg.content, "look at the attached doodle");
    assert_eq!(
        msg.extra["media"],
        serde_json::json!([
            {"mime_type": "image/png", "uri": "file:///tmp/synthetic-doodle.png"},
            {"mime_type": "text/plain", "uri": "file:///tmp/synthetic-notes.txt"}
        ])
    );

    // GENERIC-carried media refs survive too (a real occurrence).
    let rec = serde_json::json!({
        "source": "MODEL", "type": "GENERIC",
        "created_at": "2026-06-11T20:14:46Z", "content": "rendered the attachment",
        "media": [{"mime_type": "image/png", "uri": "file:///tmp/synthetic-render.png"}]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.extra["media"][0]["mime_type"], "image/png");
    assert_eq!(
        msg.extra["media"][0]["uri"],
        "file:///tmp/synthetic-render.png"
    );
}

#[test]
fn media_only_user_input_is_emitted_not_dropped() {
    // Native renders attachment turns whose <USER_REQUEST> body is empty;
    // the record must emit a user message carrying the media refs instead
    // of vanishing.
    let rec = serde_json::json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
        "content": "<USER_REQUEST>\n\n</USER_REQUEST>",
        "media": [{"mime_type": "image/png", "uri": "file:///tmp/synthetic-shot.png"}]
    });
    let msg = record_to_message(&rec).expect("media-only USER_INPUT must not vanish");
    assert_eq!(msg.role, "user");
    assert!(msg.content.is_empty());
    assert_eq!(
        msg.extra["media"],
        serde_json::json!([
            {"mime_type": "image/png", "uri": "file:///tmp/synthetic-shot.png"}
        ])
    );
    assert_eq!(msg.extra["agy_raw"], rec);

    // The wrappers that rode along on the attachment turn still reach extras.
    let rec = serde_json::json!({
        "step_index": 1, "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-06-11T20:14:43Z",
        "content": "<USER_REQUEST></USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal noon\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nchanged to Gemini 3.1 Pro (High). ok\n</USER_SETTINGS_CHANGE>",
        "media": [{"mime_type": "image/png", "uri": "file:///tmp/synthetic-shot-2.png"}]
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.role, "user");
    assert!(msg.content.is_empty());
    assert!(
        msg.extra["settings_change"]
            .as_str()
            .unwrap()
            .contains("Gemini 3.1 Pro (High)")
    );
    assert_eq!(msg.extra["additional_metadata"], "local noon");

    // A truly empty turn (no body, no media) is still dropped.
    let rec = serde_json::json!({
        "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-06-11T20:14:44Z",
        "content": "<USER_REQUEST>\n\n</USER_REQUEST>"
    });
    assert!(record_to_message(&rec).is_none());
}

#[test]
fn error_only_model_step_is_emitted_with_error_text() {
    // An error-only GENERIC step (no content, thinking or tool_calls)
    // carries its failure text in `error`; emit it like ERROR_MESSAGE
    // does instead of dropping the record.
    let rec = serde_json::json!({
        "step_index": 6, "source": "MODEL", "type": "GENERIC",
        "status": "ERROR", "created_at": "2026-06-11T20:14:46Z",
        "error": "the widget pipeline collapsed"
    });
    let msg = record_to_message(&rec).expect("error-only MODEL step must not vanish");
    assert_eq!(msg.role, "tool");
    assert_eq!(msg.author.as_deref(), Some("generic"));
    assert_eq!(msg.content, "the widget pipeline collapsed");
    assert_eq!(msg.extra["agy_type"], "GENERIC");
    assert_eq!(msg.extra["error"], "the widget pipeline collapsed");
    assert_eq!(msg.extra["agy_raw"], rec);
    assert!(msg.invocations.is_empty());

    // Whitespace-only error text still leaves nothing to emit.
    let rec = serde_json::json!({
        "source": "MODEL", "type": "GENERIC",
        "status": "ERROR", "created_at": "2026-06-11T20:14:47Z",
        "error": "   "
    });
    assert!(record_to_message(&rec).is_none());

    // Content keeps precedence over a same-record error field.
    let rec = serde_json::json!({
        "source": "MODEL", "type": "GENERIC",
        "status": "ERROR", "created_at": "2026-06-11T20:14:48Z",
        "content": "partial output before the failure",
        "error": "the widget pipeline collapsed"
    });
    let msg = record_to_message(&rec).unwrap();
    assert_eq!(msg.content, "partial output before the failure");
}

#[test]
fn conversation_history_content_is_emitted() {
    // Real shape: CONVERSATION_HISTORY is a marker record, but one real
    // record carries text — that text must reach the packet (F10 item 5).
    let rec = serde_json::json!({
        "step_index": 1, "source": "SYSTEM", "type": "CONVERSATION_HISTORY",
        "status": "DONE", "created_at": "2026-06-11T20:14:42Z",
        "content": "earlier turns were folded into this synthetic history note"
    });
    let msg = record_to_message(&rec).expect("CONVERSATION_HISTORY text must not vanish");
    assert_eq!(msg.role, "system");
    assert_eq!(
        msg.content,
        "earlier turns were folded into this synthetic history note"
    );
    assert_eq!(msg.extra["agy_type"], "CONVERSATION_HISTORY");

    // The marker form (no content) is still no message.
    let rec = serde_json::json!({
        "step_index": 2, "source": "SYSTEM", "type": "CONVERSATION_HISTORY",
        "status": "DONE", "created_at": "2026-06-11T20:14:43Z"
    });
    assert!(record_to_message(&rec).is_none());
}

#[test]
fn lifecycle_status_passes_into_extras() {
    // Real shape: every record carries a lifecycle `status`
    // (DONE/RUNNING/ERROR/INVALID; F10 item 6).
    for status in ["DONE", "RUNNING", "ERROR", "INVALID"] {
        let rec = serde_json::json!({
            "source": "MODEL", "type": "VIEW_FILE",
            "created_at": "2026-06-11T20:14:44Z",
            "content": "file listing", "status": status
        });
        let msg = record_to_message(&rec).unwrap();
        assert_eq!(
            msg.extra["status"], status,
            "lifecycle status must ride along"
        );
    }

    // A record without `status` (defensive future shape) gets no key.
    let rec = serde_json::json!({
        "source": "MODEL", "type": "VIEW_FILE",
        "created_at": "2026-06-11T20:14:44Z", "content": "file listing"
    });
    let msg = record_to_message(&rec).unwrap();
    assert!(msg.extra.get("status").is_none());
}

#[test]
fn scan_emits_f10_records_in_source_order_and_keeps_existing_ones() {
    // End to end: a synthetic transcript mixing already-emitted shapes
    // with the F10 shapes. Existing records keep their content/roles;
    // the new records land at their natural source positions.
    let dir = TempDir::new().unwrap();
    let logs = dir.path().join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        concat!(
            "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nclean the widget rack\\n</USER_REQUEST>\",\"media\":[{\"mime_type\":\"image/png\",\"uri\":\"file:///tmp/synthetic-a.png\"}]}\n",
            "{\"step_index\":1,\"source\":\"SYSTEM\",\"type\":\"CONVERSATION_HISTORY\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"synthetic folded history note\"}\n",
            "{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:43Z\",\"content\":\"planning the rack cleanup\",\"truncated_fields\":[\"thinking\"]}\n",
            "{\"step_index\":3,\"source\":\"MODEL\",\"type\":\"GENERIC\",\"status\":\"ERROR\",\"created_at\":\"2026-06-11T20:14:44Z\",\"content\":\"the rack probe returned nothing\",\"error\":\"synthetic probe failure\"}\n",
            "{\"step_index\":4,\"source\":\"SYSTEM\",\"type\":\"ERROR_MESSAGE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:45Z\",\"error\":\"synthetic pipeline failure\"}\n",
        ),
    )
    .unwrap();
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let msgs = &convs[0].messages;
    assert_eq!(msgs.len(), 5);
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "system", "assistant", "tool", "system"]);

    // Already-emitted records keep their content untouched.
    assert_eq!(msgs[0].content, "clean the widget rack");
    assert_eq!(msgs[2].content, "planning the rack cleanup");
    assert_eq!(msgs[3].content, "the rack probe returned nothing");
    // The newly emitted records carry their source text.
    assert_eq!(msgs[1].content, "synthetic folded history note");
    assert_eq!(msgs[4].content, "synthetic pipeline failure");

    // Passthrough fields are visible per message, exactly as recorded.
    assert_eq!(msgs[0].extra["media"][0]["mime_type"], "image/png");
    assert_eq!(msgs[0].extra["status"], "DONE");
    assert_eq!(
        msgs[2].extra["truncated_fields"],
        serde_json::json!(["thinking"])
    );
    assert_eq!(msgs[3].extra["error"], "synthetic probe failure");
    assert_eq!(msgs[3].extra["status"], "ERROR");
    assert_eq!(msgs[4].extra["error"], "synthetic pipeline failure");
}

#[test]
fn fixture_messages_carry_status_and_the_archived_raw_record() {
    // The checked-in fixture records carry `status: "DONE"` and no
    // error/media/truncated_fields, so after F10 every fixture message
    // gains the status key and after F14 also the archived raw record
    // — content/roles unchanged.
    let conv = only_conversation();
    for m in &conv.messages {
        assert_eq!(m.extra["status"], "DONE");
        assert!(m.extra.get("error").is_none());
        assert!(m.extra.get("media").is_none());
        assert!(m.extra.get("truncated_fields").is_none());
        let raw = m
            .extra
            .get("agy_raw")
            .expect("fixture messages archive their raw record");
        assert_eq!(raw["status"], "DONE");
        assert!(raw.get("type").is_some_and(|t| t.as_str().is_some()));
    }
}

#[test]
fn discovery_lists_transcript_full_and_db() {
    let connector = AntigravityConnector::new();
    let discovered = connector.discover_source_files(&fixture_ctx()).unwrap();

    assert!(discovered.iter().all(|d| d.provider_slug == "antigravity"));
    // The fixture's transcript_full.jsonl is the preferred transcript
    // (unconditional intra-directory priority), so it is the required
    // primary source; transcript.jsonl stays discovered as the optional
    // fallback.
    assert!(
        discovered
            .iter()
            .any(|d| d.source_path.ends_with("transcript_full.jsonl")
                && d.role == DiscoveredSourceRole::PrimarySessionLog
                && d.required_for_reconstruction),
        "transcript_full.jsonl must be the required primary source"
    );
    assert!(
        discovered
            .iter()
            .any(|d| d.source_path.ends_with("transcript.jsonl")),
        "transcript.jsonl should still be discovered as a fallback"
    );
    assert!(
        discovered
            .iter()
            .any(|d| d.role == DiscoveredSourceRole::SqliteDatabase
                && d.source_path.extension().is_some_and(|e| e == "db")),
        "the conversations/<uuid>.db should be discovered as the sqlite source"
    );
}

#[test]
fn discovery_covers_scan_sources_for_fixture() {
    assert_discovery_covers_scan_sources(&AntigravityConnector::new(), &fixture_ctx());
}

#[test]
fn scan_with_callback_matches_scan() {
    let connector = AntigravityConnector::new();
    let scanned = connector.scan(&fixture_ctx()).unwrap();
    let mut streamed = Vec::new();
    connector
        .scan_with_callback(&fixture_ctx(), &mut |c| {
            streamed.push(c);
            Ok(())
        })
        .unwrap();
    assert_eq!(streamed.len(), scanned.len());
    assert_eq!(streamed[0].messages.len(), scanned[0].messages.len());
}

#[test]
fn scan_resolves_conversation_from_direct_dir() {
    let dir = fixture_conversation_dir();
    let ctx = ScanContext::with_roots(dir.clone(), vec![ScanRoot::local(dir)], None);
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
}

#[test]
fn scan_returns_empty_for_missing_root() {
    let ctx = ScanContext::with_roots(
        PathBuf::from("/no/such/antigravity"),
        vec![ScanRoot::local(PathBuf::from("/no/such/antigravity"))],
        None,
    );
    assert!(AntigravityConnector::new().scan(&ctx).unwrap().is_empty());
}

#[test]
fn since_ts_in_future_skips_conversation() {
    let base = fixture_base();
    let ctx = ScanContext::with_roots(base.clone(), vec![ScanRoot::local(base)], Some(i64::MAX));
    assert!(AntigravityConnector::new().scan(&ctx).unwrap().is_empty());
}

#[test]
fn tested_versions_registry_and_version_check() {
    assert!(AGY_TESTED_VERSIONS.contains(&"1.0.7"));
    assert!(agy_version_is_tested("1.0.7"));
    assert!(agy_version_is_tested("v1.0.7"));
    assert!(!agy_version_is_tested("1.0.8"));
    assert!(!agy_version_is_tested("2.0.0"));
}

#[test]
fn parse_agy_version_extracts_dotted_numeric_token() {
    assert_eq!(
        parse_agy_version("agy version 1.0.7").as_deref(),
        Some("1.0.7")
    );
    assert_eq!(parse_agy_version("v1.0.7\n").as_deref(), Some("1.0.7"));
    assert_eq!(
        parse_agy_version("Antigravity CLI 1.2.34 (build x)").as_deref(),
        Some("1.2.34")
    );
    assert_eq!(parse_agy_version("no version here").as_deref(), None);
}

#[test]
fn transcript_with_only_markers_yields_no_conversation() {
    // A transcript whose only records carry no timeline content must not
    // panic and must not fabricate a conversation (it warns instead).
    let dir = TempDir::new().unwrap();
    let logs = dir.path().join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        "{\"step_index\":0,\"source\":\"SYSTEM\",\"type\":\"CONVERSATION_HISTORY\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\"}\n",
    )
    .unwrap();
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    assert!(AntigravityConnector::new().scan(&ctx).unwrap().is_empty());
}

#[test]
fn malformed_transcript_lines_are_skipped_not_fatal() {
    let dir = TempDir::new().unwrap();
    let logs = dir.path().join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    // A junk line between two valid records: junk is skipped, the rest parse.
    fs::write(
        logs.join("transcript.jsonl"),
        "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nhi there\\n</USER_REQUEST>\"}\nthis is not json\n{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:43Z\",\"content\":\"hello!\"}\n",
    )
    .unwrap();
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].messages.len(),
        2,
        "valid records survive a junk line"
    );
}

#[test]
fn ignores_legacy_gemini_cli_layout_under_shared_dot_gemini() {
    // Both the legacy Gemini CLI (~/.gemini/tmp/<hash>/chats/session-*.json)
    // and agy (~/.gemini/antigravity-cli/...) live under ~/.gemini. The agy
    // connector must resolve ONLY the antigravity-cli transcript, never the
    // gemini session files. Root at the shared `.gemini` parent to prove it.
    let tmp = TempDir::new().unwrap();
    let dot_gemini = tmp.path().join(".gemini");

    // Legacy gemini-cli session (must be ignored).
    let chats = dot_gemini.join("tmp").join("deadbeef").join("chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(chats.join("session-1.json"), "{\"messages\":[]}").unwrap();

    // agy conversation transcript (must be found).
    let logs = dot_gemini
        .join("antigravity-cli")
        .join("brain")
        .join("11111111-2222-3333-4444-555555555555")
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nhi\\n</USER_REQUEST>\"}\n",
    )
    .unwrap();

    let ctx = ScanContext::with_roots(dot_gemini.clone(), vec![ScanRoot::local(dot_gemini)], None);
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1, "only the agy conversation should be found");
    assert_eq!(convs[0].agent_slug, "antigravity");
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
}
// ---------------------------------------------------------------------
// Dual-store layout (cass #454): IDE `~/.gemini/antigravity` + CLI
// `~/.gemini/antigravity-cli`.
// ---------------------------------------------------------------------

const SHARED_UUID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

/// Write a minimal one-step transcript for `<base>/brain/<uuid>` whose user
/// request is `marker`, returning the conversation directory.
fn write_conversation(base: &Path, uuid: &str, marker: &str) -> PathBuf {
    let conv_dir = base.join("brain").join(uuid);
    let logs = conv_dir.join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        format!(
            "{{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\n{marker}\\n</USER_REQUEST>\"}}\n"
        ),
    )
    .unwrap();
    conv_dir
}

#[test]
fn default_base_roots_probe_ide_then_cli_under_dot_gemini() {
    let home = Path::new("/home/someone");
    assert_eq!(
        AntigravityConnector::default_base_roots(home),
        vec![
            PathBuf::from("/home/someone/.gemini/antigravity"),
            PathBuf::from("/home/someone/.gemini/antigravity-cli"),
        ]
    );
}

#[test]
fn layout_is_classified_from_dot_gemini_location_shape_only() {
    assert_eq!(
        AntigravityLayout::from_base(Path::new("/h/.gemini/antigravity")),
        Some(AntigravityLayout::Ide)
    );
    assert_eq!(
        AntigravityLayout::from_base(Path::new("/h/.gemini/antigravity-cli")),
        Some(AntigravityLayout::Cli)
    );
    // A same-named directory outside `.gemini/` (fixture, relocated
    // mirror) is unclassified rather than guessed.
    assert_eq!(
        AntigravityLayout::from_base(Path::new("/mirror/antigravity")),
        None
    );
    assert_eq!(
        AntigravityLayout::from_base(Path::new("/h/.gemini/antigravity-ide")),
        None
    );
    assert_eq!(AntigravityLayout::from_base(&fixture_base()), None);
}

#[test]
fn external_ids_qualify_ide_and_keep_cli_bare() {
    assert_eq!(AntigravityLayout::Ide.external_id("u-1"), "ide/u-1");
    assert_eq!(AntigravityLayout::Cli.external_id("u-1"), "u-1");
}

#[test]
fn ide_and_cli_stores_are_both_scanned_and_same_uuid_never_collides() {
    let tmp = TempDir::new().unwrap();
    let dot_gemini = tmp.path().join(".gemini");
    let ide_base = dot_gemini.join("antigravity");
    let cli_base = dot_gemini.join("antigravity-cli");
    // The SAME uuid under both stores must surface as two distinct
    // conversations with different external ids.
    write_conversation(&ide_base, SHARED_UUID, "from the IDE");
    write_conversation(&cli_base, SHARED_UUID, "from the CLI");

    // The two default probe roots, exactly as `base_roots()` produces them.
    let ctx = ScanContext::with_roots(
        tmp.path().to_path_buf(),
        vec![
            ScanRoot::local(ide_base.clone()),
            ScanRoot::local(cli_base.clone()),
        ],
        None,
    );
    let mut convs = AntigravityConnector::new().scan(&ctx).unwrap();
    convs.sort_by(|a, b| a.external_id.cmp(&b.external_id));
    assert_eq!(convs.len(), 2, "one conversation per store");

    let cli = &convs[0];
    assert_eq!(cli.external_id.as_deref(), Some(SHARED_UUID));
    assert_eq!(cli.metadata["layout"], "cli");
    assert_eq!(
        cli.metadata["data_root"],
        cli_base.to_string_lossy().as_ref()
    );
    assert!(cli.source_path.starts_with(&cli_base));
    assert_eq!(cli.messages[0].content, "from the CLI");

    let ide = &convs[1];
    assert_eq!(
        ide.external_id.as_deref(),
        Some(format!("ide/{SHARED_UUID}").as_str())
    );
    assert_eq!(ide.metadata["layout"], "ide");
    assert_eq!(
        ide.metadata["data_root"],
        ide_base.to_string_lossy().as_ref()
    );
    assert!(ide.source_path.starts_with(&ide_base));
    assert_eq!(ide.messages[0].content, "from the IDE");

    // Discovery preserves per-root provenance for both stores.
    let discovered = AntigravityConnector::new()
        .discover_source_files(&ctx)
        .unwrap();
    let transcripts: Vec<&DiscoveredSourceFile> = discovered
        .iter()
        .filter(|d| d.required_for_reconstruction)
        .collect();
    assert_eq!(transcripts.len(), 2);
    assert!(
        transcripts
            .iter()
            .any(|d| d.scan_root == ide_base && d.source_path.starts_with(&ide_base))
    );
    assert!(
        transcripts
            .iter()
            .any(|d| d.scan_root == cli_base && d.source_path.starts_with(&cli_base))
    );
    assert_discovery_covers_scan_sources(&AntigravityConnector::new(), &ctx);
}

#[test]
fn missing_ide_store_is_skipped_and_cli_store_still_indexed() {
    let tmp = TempDir::new().unwrap();
    let dot_gemini = tmp.path().join(".gemini");
    let ide_base = dot_gemini.join("antigravity"); // never created
    let cli_base = dot_gemini.join("antigravity-cli");
    write_conversation(&cli_base, SHARED_UUID, "cli only");

    let ctx = ScanContext::with_roots(
        tmp.path().to_path_buf(),
        vec![ScanRoot::local(ide_base), ScanRoot::local(cli_base)],
        None,
    );
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(SHARED_UUID));
    assert_eq!(convs[0].metadata["layout"], "cli");
}

#[test]
fn scan_rooted_at_shared_dot_gemini_finds_both_stores_once() {
    // A remote mirror may root the scan one level up, at `.gemini` itself.
    // Both stores are found, each exactly once.
    let tmp = TempDir::new().unwrap();
    let dot_gemini = tmp.path().join(".gemini");
    write_conversation(
        &dot_gemini.join("antigravity"),
        "11111111-1111-4111-8111-111111111111",
        "ide",
    );
    write_conversation(
        &dot_gemini.join("antigravity-cli"),
        "22222222-2222-4222-8222-222222222222",
        "cli",
    );

    let ctx = ScanContext::with_roots(dot_gemini.clone(), vec![ScanRoot::local(dot_gemini)], None);
    let mut ids: Vec<String> = AntigravityConnector::new()
        .scan(&ctx)
        .unwrap()
        .into_iter()
        .filter_map(|c| c.external_id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "22222222-2222-4222-8222-222222222222".to_string(),
            "ide/11111111-1111-4111-8111-111111111111".to_string(),
        ]
    );
}

#[test]
fn unclassified_fixture_base_keeps_bare_uuid_and_records_data_root() {
    let conv = only_conversation();
    assert_eq!(conv.external_id.as_deref(), Some(FIXTURE_UUID));
    assert!(
        conv.metadata.get("layout").is_none(),
        "a base outside .gemini/ must not be guessed as IDE or CLI"
    );
    assert_eq!(
        conv.metadata["data_root"],
        fixture_base().to_string_lossy().as_ref()
    );
}

#[test]
fn antigravity_mid_read_io_error_discards_incomplete_transcript_and_skips_corrupt_utf8() {
    use std::io::{self, BufReader, Cursor, Read};

    struct FailingReader(io::ErrorKind);
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "simulated mid-read failure"))
        }
    }

    let prefix = "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>complete prefix</USER_REQUEST>\"}\n";
    for kind in [
        io::ErrorKind::Other,
        io::ErrorKind::InvalidData,
        io::ErrorKind::Interrupted,
    ] {
        let reader = BufReader::new(prefix.as_bytes().chain(FailingReader(kind)));
        assert!(
            AntigravityConnector::read_transcript_records_from(
                Path::new("transcript.jsonl"),
                reader
            )
            .is_err(),
            "mid-stream {kind:?} must discard incomplete Antigravity transcript"
        );
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"\xef\xbb\xbf{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>first valid</USER_REQUEST>\"}\n",
    );
    bytes.extend_from_slice(b"{\"step_index\":1,\"content\":\"\xffcorrupt\"}\n");
    bytes.extend_from_slice(
        b"{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"second valid\"}\n",
    );
    let records = AntigravityConnector::read_transcript_records_from(
        Path::new("transcript.jsonl"),
        Cursor::new(bytes),
    )
    .expect("corrupt UTF-8 line should be skipped");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["step_index"], 0);
    assert_eq!(records[1]["step_index"], 2);
}

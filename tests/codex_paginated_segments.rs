//! Codex `history_mode=paginated` writes one thread across several rollout
//! files: the original `rollout-<ts>-<A>.jsonl` and its continuations
//! `rollout-<ts2>-<A>_<B>.jsonl`. `A` is the thread id; `B` only names the file.
//!
//! These fixtures pin the merged thread: one conversation per thread, segments
//! in filename-timestamp order, the original's identity, whole-thread
//! incremental rescans, and source boundaries that cover every segment.
//!
//! Read-only evidence behind the ordering and the no-dedupe rule (116 segments
//! of 40 real threads under `~/.codex/sessions`): no two segments of a thread
//! share a record, no message-bearing `call_id`/`id` repeats across segments,
//! filename order equals first-record order, and every segment starts at or
//! after the previous segment's last record.

#![cfg(feature = "connectors")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use franken_agent_detection::{
    CodexConnector, Connector, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    ScanContext, ScanRoot, SourceCompletion, SourceScanHooks,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const A: &str = "01a1d1b4-5529-7cd3-97db-d52326514bc2";
const B1: &str = "01a1d1ba-a4da-7b00-92b0-0368cfada0b2";
const B2: &str = "01a1d1c8-54dc-73d0-b2cd-4dd76e4c60ab";
const OTHER: &str = "01a1d1ff-1111-7222-8333-444444444444";

const TS0: &str = "2026-11-12T22-40-18";
const TS1: &str = "2026-11-12T22-47-12";
const TS2: &str = "2026-11-12T23-22-50";

const META0_MS: i64 = 1_794_519_618_000;
const META1_MS: i64 = 1_794_520_032_000;
const END1_MS: i64 = 1_794_520_080_000;
const END2_MS: i64 = 1_794_522_210_000;

fn original_name(ts: &str) -> String {
    format!("rollout-{ts}-{A}.jsonl")
}

fn continuation_name(ts: &str, rollout: &str) -> String {
    format!("rollout-{ts}-{A}_{rollout}.jsonl")
}

fn meta(ts: &str, cwd: &Path) -> Value {
    json!({
        "type": "session_meta",
        "timestamp": ts,
        "payload": {
            "id": A, "session_id": A, "timestamp": ts, "cwd": cwd,
            "history_mode": "paginated", "originator": "codex-tui",
            "source": "cli", "thread_source": "user"
        }
    })
}

fn user(ts: &str, text: &str) -> Value {
    json!({"type":"response_item","timestamp":ts,
           "payload":{"type":"message","role":"user","content":text}})
}

fn assistant(ts: &str, text: &str) -> Value {
    json!({"type":"response_item","timestamp":ts,
           "payload":{"type":"message","role":"assistant","content":text}})
}

fn tool_call(ts: &str, name: &str, call_id: &str) -> Value {
    json!({"type":"response_item","timestamp":ts,
           "payload":{"type":"function_call","name":name,"call_id":call_id,
                      "arguments":"{\"cmd\":\"ls\"}"}})
}

fn tool_output(ts: &str, call_id: &str, output: &str) -> Value {
    json!({"type":"response_item","timestamp":ts,
           "payload":{"type":"function_call_output","call_id":call_id,"output":output}})
}

fn reasoning(ts: &str, text: &str) -> Value {
    json!({"type":"event_msg","timestamp":ts,"payload":{"type":"agent_reasoning","text":text}})
}

fn token_count(ts: &str, total: i64, input: i64, output: i64) -> Value {
    json!({"type":"event_msg","timestamp":ts,
           "payload":{"type":"token_count","info":{
               "last_token_usage":{"input_tokens":input,"output_tokens":output},
               "total_token_usage":{"total_tokens":total}}}})
}

/// The three records every segment of the fixture thread carries, so a merged
/// conversation is recognizable by its text alone.
fn turn(cwd: &Path, meta_ts: &str, user_ts: &str, end_ts: &str, label: &str) -> Vec<Value> {
    vec![
        meta(meta_ts, cwd),
        user(user_ts, &format!("{label} question")),
        assistant(end_ts, &format!("{label} answer")),
    ]
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".codex").join("sessions")).unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn sessions(&self) -> PathBuf {
        self.root().join(".codex").join("sessions")
    }

    fn workspace(&self) -> PathBuf {
        let cwd = self.root().join("workspace");
        fs::create_dir_all(&cwd).unwrap();
        cwd
    }

    fn ctx(&self, since: Option<i64>) -> ScanContext {
        ctx_at(self.root(), since)
    }

    fn write(&self, rel: &str, records: &[Value]) -> PathBuf {
        write_records(&self.sessions().join(rel), records)
    }

    #[cfg(feature = "codex-zstd")]
    fn write_compressed(&self, rel: &str, records: &[Value]) -> PathBuf {
        let path = self.sessions().join(rel);
        let text = record_text(records);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, zstd::stream::encode_all(text.as_bytes(), 3).unwrap()).unwrap();
        path
    }

    /// The fixture thread: original + two continuations, written out of order
    /// so ordering cannot come from creation time or directory walk order.
    fn three_segments(&self) -> Vec<PathBuf> {
        let cwd = self.workspace();
        let second = self.write(
            &format!("2026/11/12/{}", continuation_name(TS2, B2)),
            &turn(
                &cwd,
                "2026-11-12T22:22:50Z",
                "2026-11-12T22:22:55Z",
                "2026-11-12T22:23:30Z",
                "third",
            ),
        );
        let original = self.write(
            &format!("2026/11/12/{}", original_name(TS0)),
            &turn(
                &cwd,
                "2026-11-12T21:40:18Z",
                "2026-11-12T21:40:20Z",
                "2026-11-12T21:41:00Z",
                "first",
            ),
        );
        let first = self.write(
            &format!("2026/11/12/{}", continuation_name(TS1, B1)),
            &turn(
                &cwd,
                "2026-11-12T21:47:12Z",
                "2026-11-12T21:47:20Z",
                "2026-11-12T21:48:00Z",
                "second",
            ),
        );
        vec![original, first, second]
    }
}

fn ctx_at(root: &Path, since: Option<i64>) -> ScanContext {
    ScanContext::with_roots(
        root.join("cass"),
        vec![ScanRoot::local(root.join(".codex"))],
        since,
    )
    .without_local_default_stores()
}

fn record_text(records: &[Value]) -> String {
    let mut text = String::new();
    for record in records {
        text.push_str(&record.to_string());
        text.push('\n');
    }
    text
}

fn write_records(path: &Path, records: &[Value]) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, record_text(records)).unwrap();
    path.to_path_buf()
}

fn set_mtime(path: &Path, ms: u64) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap();
}

fn scan(ctx: &ScanContext) -> Vec<NormalizedConversation> {
    CodexConnector::new().scan(ctx).unwrap()
}

fn contents(conv: &NormalizedConversation) -> Vec<&str> {
    conv.messages.iter().map(|m| m.content.as_str()).collect()
}

fn indices(conv: &NormalizedConversation) -> Vec<i64> {
    conv.messages.iter().map(|m| m.idx).collect()
}

/// Absolute temp paths are not reproducible, so the baseline pins every other
/// byte and the paths are asserted separately against the fixture layout.
fn baseline_value(convs: &[NormalizedConversation]) -> Value {
    let mut out = Vec::new();
    for conv in convs {
        let mut value = serde_json::to_value(conv).unwrap();
        for key in ["source_path", "workspace"] {
            if let Some(path) = value[key].as_str() {
                let name = Path::new(path).file_name().unwrap().to_str().unwrap();
                value[key] = json!(name);
            }
        }
        out.push(value);
    }
    Value::Array(out)
}

/// The single-file fixtures whose output must not move: a paginated thread
/// with no continuation, a non-UUID rollout name, and a legacy JSON document.
fn single_file_fixture() -> (Fixture, Vec<PathBuf>) {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let mut records = turn(
        &cwd,
        "2026-11-12T21:40:18Z",
        "2026-11-12T21:40:20Z",
        "2026-11-12T21:41:00Z",
        "first",
    );
    records.push(reasoning("2026-11-12T21:41:10Z", "thinking about ls"));
    records.push(tool_call("2026-11-12T21:41:20Z", "exec_command", "call-1"));
    records.push(tool_output("2026-11-12T21:41:30Z", "call-1", "file.txt"));
    records.push(token_count("2026-11-12T21:41:40Z", 900, 120, 45));
    let modern = fixture.write(&format!("2026/11/12/{}", original_name(TS0)), &records);
    let named = fixture.write(
        "rollout-legacy-name.jsonl",
        &[user("2026-11-12T20:00:00Z", "legacy name question")],
    );
    let legacy = fixture.write("rollout-legacy.json", &[]);
    fs::write(
        &legacy,
        json!({"session":{"cwd":cwd},"items":[
            {"role":"user","content":"legacy doc question","timestamp":"2026-11-12T19:00:00Z"}
        ]})
        .to_string(),
    )
    .unwrap();
    (fixture, vec![modern, named, legacy])
}

#[test]
#[ignore = "writes the pre-merge single-file baseline; run with --ignored and FAD_BASELINE_OUT"]
fn dump_single_file_baseline() {
    let (fixture, paths) = single_file_fixture();
    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), paths.len());
    let out = std::env::var("FAD_BASELINE_OUT").expect("set FAD_BASELINE_OUT");
    fs::write(
        out,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&baseline_value(&convs)).unwrap()
        ),
    )
    .unwrap();
}

#[test]
fn single_file_rollout_output_is_unchanged() {
    let (fixture, paths) = single_file_fixture();
    let convs = scan(&fixture.ctx(None));
    // Compare values, not text: `--all-features` enables serde_json's
    // `preserve_order`, which changes map key order and nothing else. The
    // byte-level pin is the `dump_single_file_baseline` diff in the report.
    let baseline = include_str!("fixtures/codex_paginated/single_file_baseline.json");
    let expected: Value = serde_json::from_str(baseline).unwrap();
    assert_eq!(
        baseline_value(&convs),
        expected,
        "a thread with no continuation keeps today's exact output"
    );
    assert_eq!(
        convs
            .iter()
            .map(|c| c.source_path.clone())
            .collect::<Vec<_>>(),
        paths,
        "source paths stay the rollout files themselves"
    );
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS0}-{A}").as_str())
    );
    assert_eq!(
        convs[0].workspace.as_deref(),
        Some(fixture.workspace().as_path())
    );
}

#[test]
fn original_and_one_continuation_merge_in_order() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let original = fixture.write(
        &format!("2026/11/12/{}", original_name(TS0)),
        &turn(
            &cwd,
            "2026-11-12T21:40:18Z",
            "2026-11-12T21:40:20Z",
            "2026-11-12T21:41:00Z",
            "first",
        ),
    );
    let continuation = fixture.write(
        &format!("2026/11/12/{}", continuation_name(TS1, B1)),
        &turn(
            &cwd,
            "2026-11-12T21:47:12Z",
            "2026-11-12T21:47:20Z",
            "2026-11-12T21:48:00Z",
            "second",
        ),
    );

    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 1, "one thread, one conversation: {convs:?}");
    let conv = &convs[0];
    assert_eq!(
        contents(conv),
        [
            "first question",
            "first answer",
            "second question",
            "second answer"
        ]
    );
    assert_eq!(indices(conv), [0, 1, 2, 3]);
    assert_eq!(conv.source_path, original);
    assert_eq!(
        conv.external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS0}-{A}").as_str()),
        "the original segment keeps its existing external id"
    );
    assert_eq!(conv.started_at, Some(META0_MS));
    assert_eq!(conv.ended_at, Some(END1_MS));
    assert_eq!(conv.workspace.as_deref(), Some(cwd.as_path()));
    assert_eq!(conv.title.as_deref(), Some("first question"));
    assert_eq!(conv.metadata["source"], json!("rollout"));
    assert_eq!(conv.metadata["thread_id"], json!(A));
    assert_eq!(
        conv.metadata["segments"],
        json!([
            {"path": original, "external_id": format!("2026/11/12/rollout-{TS0}-{A}"), "kind": "original"},
            {"path": continuation, "external_id": format!("2026/11/12/rollout-{TS1}-{A}_{B1}"), "kind": "continuation"}
        ])
    );
}

#[test]
fn original_and_two_continuations_merge_in_filename_order() {
    let fixture = Fixture::new();
    let paths = fixture.three_segments();
    // Creation order and mtimes both contradict the filename order.
    set_mtime(&paths[0], 3_000_000);
    set_mtime(&paths[1], 2_000_000);
    set_mtime(&paths[2], 1_000_000);

    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(
        contents(conv),
        [
            "first question",
            "first answer",
            "second question",
            "second answer",
            "third question",
            "third answer"
        ]
    );
    assert_eq!(indices(conv), [0, 1, 2, 3, 4, 5]);
    assert_eq!(conv.source_path, paths[0]);
    assert_eq!(conv.started_at, Some(META0_MS));
    assert_eq!(conv.ended_at, Some(END2_MS));
    assert_eq!(
        conv.metadata["segments"].as_array().unwrap().len(),
        3,
        "every segment path is recorded in order"
    );
    assert_eq!(
        conv.metadata["segments"][2]["path"],
        json!(paths[2]),
        "the last continuation is the newest filename timestamp"
    );
}

#[test]
fn continuation_only_incremental_scan_emits_the_whole_thread() {
    let fixture = Fixture::new();
    let paths = fixture.three_segments();
    set_mtime(&paths[0], 1_000_000);
    set_mtime(&paths[1], 1_500_000);
    set_mtime(&paths[2], 2_000_000);

    // Only the newest continuation is fresh; the earlier segments are stale.
    let convs = scan(&fixture.ctx(Some(2_000_000)));
    assert_eq!(convs.len(), 1, "the thread is still emitted: {convs:?}");
    let conv = &convs[0];
    assert_eq!(
        contents(conv),
        [
            "first question",
            "first answer",
            "second question",
            "second answer",
            "third question",
            "third answer"
        ],
        "a consumer replacing by external_id must not lose the earlier part"
    );
    assert_eq!(conv.source_path, paths[0]);
    assert_eq!(
        conv.external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS0}-{A}").as_str())
    );
    assert_eq!(conv.metadata["segments"].as_array().unwrap().len(), 3);

    // Discovery must list the stale siblings too, or a host cannot fingerprint
    // the whole thread.
    let discovered = CodexConnector::new()
        .discover_source_files(&fixture.ctx(Some(2_000_000)))
        .unwrap();
    let mut discovered_paths: Vec<_> = discovered.iter().map(|s| s.source_path.clone()).collect();
    discovered_paths.sort();
    let mut expected = paths;
    expected.sort();
    assert_eq!(discovered_paths, expected);

    // Nothing fresh at all: the thread is not re-emitted.
    assert!(scan(&fixture.ctx(Some(9_000_000))).is_empty());
}

#[test]
fn missing_original_still_emits_the_thread() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let first = fixture.write(
        &format!("2026/11/12/{}", continuation_name(TS1, B1)),
        &turn(
            &cwd,
            "2026-11-12T21:47:12Z",
            "2026-11-12T21:47:20Z",
            "2026-11-12T21:48:00Z",
            "second",
        ),
    );
    let second = fixture.write(
        &format!("2026/11/12/{}", continuation_name(TS2, B2)),
        &turn(
            &cwd,
            "2026-11-12T22:22:50Z",
            "2026-11-12T22:22:55Z",
            "2026-11-12T22:23:30Z",
            "third",
        ),
    );

    let convs = scan(&fixture.ctx(None));
    assert_eq!(
        convs.len(),
        1,
        "a thread without its original is not dropped"
    );
    let conv = &convs[0];
    assert_eq!(
        contents(conv),
        [
            "second question",
            "second answer",
            "third question",
            "third answer"
        ]
    );
    assert_eq!(conv.source_path, first);
    assert_eq!(
        conv.external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS1}-{A}_{B1}").as_str()),
        "the earliest surviving segment names the thread"
    );
    assert_eq!(conv.started_at, Some(META1_MS));
    assert_eq!(conv.ended_at, Some(END2_MS));
    assert_eq!(
        conv.metadata["segments"],
        json!([
            {"path": first, "external_id": format!("2026/11/12/rollout-{TS1}-{A}_{B1}"), "kind": "continuation"},
            {"path": second, "external_id": format!("2026/11/12/rollout-{TS2}-{A}_{B2}"), "kind": "continuation"}
        ])
    );
}

#[test]
fn cross_day_continuation_merges_into_the_originals_thread() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let original = fixture.write(
        &format!("2026/11/12/{}", original_name(TS0)),
        &turn(
            &cwd,
            "2026-11-12T21:40:18Z",
            "2026-11-12T21:40:20Z",
            "2026-11-12T21:41:00Z",
            "first",
        ),
    );
    let next_day = fixture.write(
        &format!(
            "2026/11/13/{}",
            continuation_name("2026-11-13T10-00-00", B1)
        ),
        &turn(
            &cwd,
            "2026-11-13T09:00:00Z",
            "2026-11-13T09:00:10Z",
            "2026-11-13T09:01:00Z",
            "next day",
        ),
    );

    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 1, "day directories do not split a thread");
    assert_eq!(
        contents(&convs[0]),
        [
            "first question",
            "first answer",
            "next day question",
            "next day answer"
        ]
    );
    assert_eq!(convs[0].source_path, original);
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS0}-{A}").as_str())
    );
    assert_eq!(convs[0].metadata["segments"][1]["path"], json!(next_day));
}

#[test]
fn distinct_threads_stay_separate_conversations() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    fixture.write(
        &format!("2026/11/12/{}", original_name(TS0)),
        &turn(
            &cwd,
            "2026-11-12T21:40:18Z",
            "2026-11-12T21:40:20Z",
            "2026-11-12T21:41:00Z",
            "first",
        ),
    );
    let other = fixture.write(
        &format!("2026/11/12/rollout-{TS1}-{OTHER}.jsonl"),
        &[
            meta("2026-11-12T21:47:12Z", &cwd),
            user("2026-11-12T21:47:20Z", "other thread question"),
        ],
    );

    let mut convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 2);
    convs.sort_by(|a, b| a.source_path.cmp(&b.source_path));
    assert_eq!(contents(&convs[0]), ["first question", "first answer"]);
    assert_eq!(contents(&convs[1]), ["other thread question"]);
    assert_eq!(convs[1].source_path, other);
    assert!(
        convs[1].metadata.get("segments").is_none(),
        "a single-file thread keeps today's metadata exactly"
    );
}

#[test]
fn source_boundaries_cover_every_segment_of_the_thread() {
    let fixture = Fixture::new();
    let paths = fixture.three_segments();
    let mut visited = Vec::new();
    let mut completions: Vec<SourceCompletion> = Vec::new();
    let mut conversations = Vec::new();
    {
        let mut should_scan = |source: &DiscoveredSourceFile| {
            visited.push(source.source_path.clone());
            true
        };
        let mut on_complete = |completion: &SourceCompletion| {
            completions.push(completion.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut should_scan),
            on_source_complete: Some(&mut on_complete),
        };
        CodexConnector::new()
            .scan_with_source_boundaries(&fixture.ctx(None), &mut hooks, &mut |conv| {
                conversations.push(conv);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(conversations.len(), 1);
    let mut expected = paths.clone();
    expected.sort();
    visited.sort();
    assert_eq!(
        visited, expected,
        "every segment reaches the pre-parse hook"
    );
    assert_eq!(completions.len(), 1);
    let completion = &completions[0];
    assert_eq!(completion.conversations_emitted, 1);
    assert_eq!(completion.source.source_path, paths[0]);
    assert_eq!(
        completion.source.role,
        DiscoveredSourceRole::PrimarySessionLog
    );
    assert!(completion.source.required_for_reconstruction);
    let mut sidecars: Vec<_> = completion
        .required_sidecars
        .iter()
        .map(|s| s.source_path.clone())
        .collect();
    sidecars.sort();
    assert_eq!(sidecars, [paths[1].clone(), paths[2].clone()]);
    assert!(
        completion
            .required_sidecars
            .iter()
            .all(|s| s.required_for_reconstruction
                && s.role == DiscoveredSourceRole::PrimarySessionLog),
        "a change to any segment must invalidate the thread's completion"
    );
}

#[test]
fn a_new_continuation_admits_the_thread_when_the_original_is_reusable() {
    let fixture = Fixture::new();
    let paths = fixture.three_segments();
    // A host ledger that already knows the original and the first continuation
    // but has never seen the newest one.
    let known = [paths[0].clone(), paths[1].clone()];
    let mut admitted = Vec::new();
    let mut completions = Vec::new();
    let mut conversations = Vec::new();
    {
        let mut should_scan = |source: &DiscoveredSourceFile| {
            let fresh = !known.contains(&source.source_path);
            admitted.push((source.source_path.clone(), fresh));
            fresh
        };
        let mut on_complete = |completion: &SourceCompletion| {
            completions.push(completion.source.source_path.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut should_scan),
            on_source_complete: Some(&mut on_complete),
        };
        CodexConnector::new()
            .scan_with_source_boundaries(&fixture.ctx(None), &mut hooks, &mut |conv| {
                conversations.push(conv);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(conversations.len(), 1, "the new segment rescans the thread");
    assert_eq!(contents(&conversations[0]).len(), 6);
    assert_eq!(conversations[0].source_path, paths[0]);
    assert_eq!(completions, vec![paths[0].clone()]);
    assert_eq!(
        admitted.last().unwrap(),
        &(paths[0].clone(), false),
        "the identity segment is the last admission so a host binds its \
         pre-parse observation to the completed source"
    );
}

#[test]
fn a_thread_whose_segments_are_all_reusable_is_not_emitted() {
    let fixture = Fixture::new();
    fixture.three_segments();
    let mut conversations = Vec::new();
    let mut completions = 0;
    {
        let mut should_scan = |_: &DiscoveredSourceFile| false;
        let mut on_complete = |_: &SourceCompletion| {
            completions += 1;
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut should_scan),
            on_source_complete: Some(&mut on_complete),
        };
        CodexConnector::new()
            .scan_with_source_boundaries(&fixture.ctx(None), &mut hooks, &mut |conv| {
                conversations.push(conv);
                Ok(())
            })
            .unwrap();
    }
    assert!(conversations.is_empty());
    assert_eq!(completions, 0);
}

#[test]
fn an_explicit_continuation_root_emits_the_whole_thread() {
    let fixture = Fixture::new();
    let paths = fixture.three_segments();
    let ctx = ScanContext::with_roots(
        fixture.root().join("cass"),
        vec![ScanRoot::local(paths[2].clone())],
        None,
    )
    .without_local_default_stores();
    let convs = scan(&ctx);
    assert_eq!(convs.len(), 1);
    assert_eq!(contents(&convs[0]).len(), 6);
    assert_eq!(convs[0].source_path, paths[0]);
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS0}-{A}").as_str()),
        "an explicitly scoped continuation keeps the thread's identity"
    );
}

#[test]
fn a_plain_and_compressed_twin_of_one_segment_is_read_once() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let records = turn(
        &cwd,
        "2026-11-12T21:40:18Z",
        "2026-11-12T21:40:20Z",
        "2026-11-12T21:41:00Z",
        "first",
    );
    let plain = fixture.write(&format!("2026/11/12/{}", original_name(TS0)), &records);
    #[cfg(feature = "codex-zstd")]
    let twin =
        fixture.write_compressed(&format!("2026/11/12/{}.zst", original_name(TS0)), &records);
    #[cfg(not(feature = "codex-zstd"))]
    let twin = plain.clone();

    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 1);
    assert_eq!(
        contents(&convs[0]),
        ["first question", "first answer"],
        "the twin must not duplicate the segment's records"
    );
    assert_eq!(convs[0].source_path, plain);
    let _ = twin;
}

#[cfg(feature = "codex-zstd")]
#[test]
fn compressed_segments_merge_like_plain_ones() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let original = fixture.write_compressed(
        &format!("2026/11/12/{}.zst", original_name(TS0)),
        &turn(
            &cwd,
            "2026-11-12T21:40:18Z",
            "2026-11-12T21:40:20Z",
            "2026-11-12T21:41:00Z",
            "first",
        ),
    );
    let continuation = fixture.write_compressed(
        &format!("2026/11/12/{}.zst", continuation_name(TS1, B1)),
        &turn(
            &cwd,
            "2026-11-12T21:47:12Z",
            "2026-11-12T21:47:20Z",
            "2026-11-12T21:48:00Z",
            "second",
        ),
    );

    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 1);
    assert_eq!(
        contents(&convs[0]),
        [
            "first question",
            "first answer",
            "second question",
            "second answer"
        ]
    );
    assert_eq!(convs[0].source_path, original);
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some(format!("2026/11/12/rollout-{TS0}-{A}").as_str()),
        "the compressed form names the same session"
    );
    assert_eq!(
        convs[0].metadata["segments"][1]["path"],
        json!(continuation)
    );
}

#[cfg(feature = "codex-zstd")]
#[test]
fn a_compressed_continuation_merges_with_a_plain_original() {
    let fixture = Fixture::new();
    let cwd = fixture.workspace();
    let original = fixture.write(
        &format!("2026/11/12/{}", original_name(TS0)),
        &turn(
            &cwd,
            "2026-11-12T21:40:18Z",
            "2026-11-12T21:40:20Z",
            "2026-11-12T21:41:00Z",
            "first",
        ),
    );
    fixture.write_compressed(
        &format!("2026/11/12/{}.zst", continuation_name(TS1, B1)),
        &turn(
            &cwd,
            "2026-11-12T21:47:12Z",
            "2026-11-12T21:47:20Z",
            "2026-11-12T21:48:00Z",
            "second",
        ),
    );

    let convs = scan(&fixture.ctx(None));
    assert_eq!(convs.len(), 1);
    assert_eq!(contents(&convs[0]).len(), 4);
    assert_eq!(convs[0].source_path, original);
}

#[test]
fn excluding_one_segment_excludes_the_whole_thread() {
    const CHILD_ROOT: &str = "FAD_CODEX_SEGMENT_EXCLUSION_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let sessions = root.join(".codex").join("sessions");
        let convs = scan(&ctx_at(&root, None));
        assert!(
            convs.is_empty(),
            "an excluded segment must not leave a partial thread: {convs:?}"
        );
        let discovered = CodexConnector::new()
            .discover_source_files(&ctx_at(&root, None))
            .unwrap();
        assert!(discovered.is_empty(), "discovery agrees with the scan");
        let _ = sessions;
        return;
    }

    let fixture = Fixture::new();
    let paths = fixture.three_segments();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "excluding_one_segment_excludes_the_whole_thread",
            "--nocapture",
        ])
        .env(CHILD_ROOT, fixture.root())
        .env("CASS_EXCLUDE_PATHS", paths[1].to_str().unwrap())
        .output()
        .expect("run the isolated exclusion child");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

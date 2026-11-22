use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

use super::AntigravityConnector;
use super::parse::workspace_from_user_information;
use super::scan::scan_antigravity_with_exclusions;
use crate::connectors::Connector;
use crate::connectors::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use crate::types::NormalizedConversation;

// -----------------------------------------------------------------
// transcript_full.jsonl selection (native `get_best_log_file` parity,
// claude_viewer providers/antigravity/discovery.py): within a single
// logs/ directory the fuller transcript wins UNCONDITIONALLY — the
// native (mtime, prio) sort only ranks candidates across different
// brain_dirs, never inside one directory (agy flushes transcript.jsonl
// microseconds after transcript_full.jsonl on every turn, so the short
// file routinely carries the newer nanosecond mtime while being the
// truncated variant). The short transcript takes over only when the
// full file cannot produce records.
// -----------------------------------------------------------------

/// Write `contents` as `brain/<uuid>/.system_generated/logs/<name>` inside
/// `base` and return the conversation directory.
fn write_named_transcript(base: &Path, uuid: &str, name: &str, contents: &str) -> PathBuf {
    let conv_dir = base.join("brain").join(uuid);
    let logs = conv_dir.join(".system_generated").join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join(name), contents).unwrap();
    conv_dir
}

fn set_file_mtime(path: &Path, secs_since_epoch: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs_since_epoch))
        .unwrap();
}

#[test]
fn transcript_full_is_preferred_on_equal_or_newer_mtime() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "10101010-1010-4101-8101-101010101010";
    let conv_dir = write_named_transcript(
        base,
        uuid,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nshort transcript marker\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(
        base,
        uuid,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nfull transcript marker\n</USER_REQUEST>")
        ),
    );
    let logs = conv_dir.join(".system_generated").join("logs");
    // Equal mtime (priority wins), then a strictly newer full file.
    for full_mtime in [1_000, 2_000] {
        set_file_mtime(&logs.join("transcript.jsonl"), 1_000);
        set_file_mtime(&logs.join("transcript_full.jsonl"), full_mtime);
        let convs = ag_scan(base);
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].external_id.as_deref(), Some(uuid));
        assert_eq!(
            convs[0].source_path,
            logs.join("transcript_full.jsonl"),
            "transcript_full must be the parse source at mtime {full_mtime}"
        );
        assert_eq!(convs[0].messages[0].content, "full transcript marker");
        // The archived record proves the full file was read — records
        // from the two transcripts are never stitched together.
        assert_eq!(
            convs[0].messages[0].extra["agy_raw"]["content"],
            "<USER_REQUEST>\nfull transcript marker\n</USER_REQUEST>"
        );
    }
}

#[test]
fn empty_transcript_full_falls_back_to_short() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "20202020-2020-4202-8202-202020202020";
    let conv_dir = write_named_transcript(base, uuid, "transcript_full.jsonl", "");
    write_named_transcript(
        base,
        uuid,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nshort marker\n</USER_REQUEST>")
        ),
    );
    let logs = conv_dir.join(".system_generated").join("logs");
    // Even with a newer mtime, a 0-byte full file produces no records.
    set_file_mtime(&logs.join("transcript_full.jsonl"), 2_000);
    set_file_mtime(&logs.join("transcript.jsonl"), 1_000);
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, logs.join("transcript.jsonl"));
    assert_eq!(convs[0].messages[0].content, "short marker");
}

#[test]
fn transcript_full_wins_over_fresher_short_within_same_dir() {
    // Native parity: inside one logs/ directory selection is an
    // unconditional priority, NOT an mtime race — the (mtime, prio)
    // sort only ranks candidates across different brain_dirs. A
    // transcript.jsonl flushed microseconds after transcript_full.jsonl
    // has the strictly newer mtime yet must still lose.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "30303030-3030-4303-8303-303030303030";
    let conv_dir = write_named_transcript(
        base,
        uuid,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nfresh short marker\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(
        base,
        uuid,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nolder full marker\n</USER_REQUEST>")
        ),
    );
    let logs = conv_dir.join(".system_generated").join("logs");
    // The short transcript is strictly fresher than the full one.
    set_file_mtime(&logs.join("transcript_full.jsonl"), 1_000);
    set_file_mtime(&logs.join("transcript.jsonl"), 2_000);
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].source_path,
        logs.join("transcript_full.jsonl"),
        "transcript_full wins unconditionally inside one logs/ directory"
    );
    assert_eq!(convs[0].messages[0].content, "older full marker");
}

#[test]
fn corrupt_or_oversized_transcript_full_falls_back_to_short() {
    // A full file that yields no records — unreadable junk lines, or a
    // size over the 100 MiB scan cap — is skipped for the next
    // candidate instead of losing the conversation.
    for label in ["corrupt", "oversized"] {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        let uuid = "40404040-4040-4404-8404-404040404040";
        let conv_dir = write_named_transcript(
            base,
            uuid,
            "transcript.jsonl",
            &format!(
                "{}\n",
                ag_user_step("<USER_REQUEST>\nshort marker\n</USER_REQUEST>")
            ),
        );
        let logs = conv_dir.join(".system_generated").join("logs");
        let full = logs.join("transcript_full.jsonl");
        if label == "oversized" {
            fs::File::create(&full)
                .unwrap()
                .set_len(101 * 1024 * 1024)
                .unwrap();
        } else {
            fs::write(&full, "this is not json\n{\"step_index\":0,\"unclosed\":").unwrap();
        }
        // The full file holds static priority and is tried first; it
        // must fail over to the short transcript.
        set_file_mtime(&full, 2_000);
        set_file_mtime(&logs.join("transcript.jsonl"), 1_000);
        let convs = ag_scan(base);
        assert_eq!(
            convs.len(),
            1,
            "{label}: the short transcript must still serve the conversation"
        );
        assert_eq!(
            convs[0].source_path,
            logs.join("transcript.jsonl"),
            "{label}"
        );
        assert_eq!(convs[0].messages[0].content, "short marker", "{label}");
    }
}

#[test]
fn transcript_full_only_directory_is_discovered_and_parsed() {
    // A brain/<uuid> holding ONLY transcript_full.jsonl is still a
    // conversation directory (native discovery parity).
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "50505050-5050-4505-8505-505050505050";
    let conv_dir = write_named_transcript(
        base,
        uuid,
        "transcript_full.jsonl",
        &format!(
            "{}\n{}\n",
            ag_user_step("<USER_REQUEST>\nfull-only marker\n</USER_REQUEST>"),
            ag_assistant_step()
        ),
    );
    assert!(
        AntigravityConnector::is_conversation_dir(&conv_dir),
        "transcript_full.jsonl alone must mark a conversation dir"
    );
    assert_eq!(
        AntigravityConnector::conversation_dirs(base),
        vec![conv_dir.clone()],
        "walk discovery must reach a full-only conversation dir"
    );
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(uuid));
    assert_eq!(
        convs[0].source_path,
        conv_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript_full.jsonl")
    );
    assert_eq!(convs[0].messages.len(), 2);
    assert_eq!(convs[0].messages[0].content, "full-only marker");

    // Discovery lists the file the parser actually reads as required.
    let discovered = AntigravityConnector::new()
        .discover_source_files(&ctx)
        .unwrap();
    assert!(
        discovered.iter().any(|d| {
            d.source_path.ends_with("transcript_full.jsonl")
                && d.role == DiscoveredSourceRole::PrimarySessionLog
                && d.required_for_reconstruction
        }),
        "a full-only transcript is the required primary source"
    );
}

#[test]
fn excluded_transcript_variant_is_never_read() {
    // `CASS_EXCLUDE_PATHS`-style exclusion must apply inside candidate
    // selection in both directions:
    // 1. transcript_full excluded → the short transcript still serves the
    //    conversation (it must not be skipped wholesale).
    // 2. transcript.jsonl excluded + a blank transcript_full → the
    //    excluded short file must never be read as the fallback.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid_a = "60606060-6060-4606-8606-606060606060";
    let conv_a = write_named_transcript(
        base,
        uuid_a,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nshort wins when full is excluded\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(
        base,
        uuid_a,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nexcluded full marker\n</USER_REQUEST>")
        ),
    );
    let excluded_full = conv_a.join(".system_generated/logs/transcript_full.jsonl");

    let uuid_b = "70707070-7070-4707-8707-707070707070";
    let conv_b = write_named_transcript(
        base,
        uuid_b,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nexcluded short marker\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(base, uuid_b, "transcript_full.jsonl", "\n\n");
    let excluded_short = conv_b.join(".system_generated/logs/transcript.jsonl");

    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let mut convs = Vec::new();
    scan_antigravity_with_exclusions(
        &ctx,
        &mut SourceScanHooks::default(),
        &[excluded_full, excluded_short],
        &mut |conv| {
            convs.push(conv);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(uuid_a));
    assert_eq!(
        convs[0].source_path,
        conv_a.join(".system_generated/logs/transcript.jsonl")
    );
    assert_eq!(
        convs[0].messages[0].content,
        "short wins when full is excluded"
    );
}

#[test]
fn excluded_transcript_variant_mtime_does_not_force_rescan() {
    // An excluded transcript_full.jsonl with a recent mtime must not
    // mark the conversation modified when the only admitted source
    // (transcript.jsonl) is older than since_ts — otherwise every
    // incremental scan re-reads a conversation whose usable source
    // never changed.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "b2b2b2b2-b2b2-4b2b-8b2b-b2b2b2b2b2b2";
    let conv_dir = write_named_transcript(
        base,
        uuid,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nold short marker\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(
        base,
        uuid,
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nnew full marker\n</USER_REQUEST>")
        ),
    );
    let logs = conv_dir.join(".system_generated").join("logs");
    let short = logs.join("transcript.jsonl");
    let full = logs.join("transcript_full.jsonl");
    set_file_mtime(&short, 1_000);
    set_file_mtime(&full, 9_999_999);
    // since_ts (epoch ms) between the two mtimes: only the excluded
    // full file would look "modified".
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        Some(5_000_000),
    );
    let scan = |excluded: &[PathBuf]| {
        let mut convs = Vec::new();
        scan_antigravity_with_exclusions(
            &ctx,
            &mut SourceScanHooks::default(),
            excluded,
            &mut |conv| {
                convs.push(conv);
                Ok(())
            },
        )
        .unwrap();
        convs
    };

    assert!(
        scan(std::slice::from_ref(&full)).is_empty(),
        "an excluded transcript's mtime must not mark the conversation modified"
    );
    // Sanity: without the exclusion the fresh full file legitimately
    // marks the conversation modified and the scan emits it.
    let convs = scan(&[]);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, logs.join("transcript_full.jsonl"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn transcript_fallback_records_the_actual_source_in_the_completion() {
    // transcript_full yields no records → the parser falls back to
    // transcript.jsonl; the SourceCompletion must name the file actually
    // read so a later change to it rescans.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "80808080-8080-4808-8808-808080808080";
    let conv_dir = write_named_transcript(
        base,
        uuid,
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nshort serves the ledger\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(base, uuid, "transcript_full.jsonl", "\n\n");
    let logs = conv_dir.join(".system_generated").join("logs");
    // transcript_full holds static priority: tried first, yields
    // nothing.
    set_file_mtime(&logs.join("transcript.jsonl"), 1_000);
    set_file_mtime(&logs.join("transcript_full.jsonl"), 2_000);
    let short = logs.join("transcript.jsonl");
    let full = logs.join("transcript_full.jsonl");
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let scan = |hooks: &mut SourceScanHooks<'_>| {
        let mut convs = Vec::new();
        scan_antigravity_with_exclusions(&ctx, hooks, &[], &mut |conv| {
            convs.push(conv);
            Ok(())
        })
        .unwrap();
        convs
    };

    let mut completions = Vec::new();
    let mut on_complete = |c: &SourceCompletion| {
        completions.push(c.clone());
        Ok(())
    };
    let convs = scan(&mut SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut on_complete),
    });
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, short);
    assert_eq!(completions.len(), 1);
    assert_eq!(
        completions[0].source.source_path, short,
        "the ledger must belong to the transcript actually read"
    );
    assert!(
        completions[0]
            .required_sidecars
            .iter()
            .any(|d| d.source_path == full),
        "the passed-over candidate rides as a sidecar so its changes \
         invalidate the ledger"
    );

    // Host-ledger simulation, keyed by the PRIMARY source path like
    // CASS `watch_source_ledger` (`source_ledger_key` → saved receipt):
    // `should_scan` may skip only the candidate whose OWN completed
    // entry has every recorded file (primary + sidecars) still
    // unchanged — a sidecar fingerprint under another primary never
    // skips it.
    let primary_key = completions[0].source.source_path.clone();
    let ledger_entry: Vec<(PathBuf, Option<u64>, Option<i64>)> =
        std::iter::once(&completions[0].source)
            .chain(completions[0].required_sidecars.iter())
            .map(|d| (d.source_path.clone(), d.size_bytes, d.modified_at_ms))
            .collect();
    let unchanged = |path: &Path, size: Option<u64>, mtime_ms: Option<i64>| {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        let now_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok());
        Some(meta.len()) == size && now_ms == mtime_ms
    };
    let mut should = |d: &DiscoveredSourceFile| {
        d.source_path != primary_key || !ledger_entry.iter().all(|(p, s, m)| unchanged(p, *s, *m))
    };

    // Unchanged: the torn full candidate is re-admitted but yields no
    // records, and the completed primary's files all match → no
    // conversation.
    assert!(
        scan(&mut SourceScanHooks {
            should_scan_source: Some(&mut should),
            on_source_complete: None,
        })
        .is_empty(),
        "unchanged candidates must be skipped"
    );

    // transcript.jsonl changes while transcript_full stays blank and
    // unchanged — the ledger must NOT suppress the rescan.
    fs::write(
        &short,
        format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nupdated short content\n</USER_REQUEST>")
        ),
    )
    .unwrap();
    set_file_mtime(&short, 3_000);
    let convs = scan(&mut SourceScanHooks {
        should_scan_source: Some(&mut should),
        on_source_complete: None,
    });
    assert_eq!(convs.len(), 1, "a changed fallback source must rescan");
    assert_eq!(convs[0].messages[0].content, "updated short content");
}

#[test]
#[allow(clippy::too_many_lines)]
fn ledger_state_machine_stays_aligned_with_the_parsed_source() {
    // `SourceScanHooks::should_scan` is STATEFUL per source path (CASS
    // `watch_source_ledger`): admitting a source records it as the
    // expected primary, and `on_source_complete` requires the completed
    // primary to be the last admitted one. The scan may therefore only
    // consult `should_scan` on a candidate it is about to read — asking
    // about a fallback it never parses corrupts the ledger.
    #[derive(Default)]
    struct SimLedger {
        // Keyed ONLY by the primary source path, like watch_source_ledger.
        entries: HashMap<PathBuf, Vec<(PathBuf, u64)>>,
        last_admitted: Option<PathBuf>,
        should_calls: Vec<PathBuf>,
        completions: Vec<PathBuf>,
    }
    impl SimLedger {
        fn should_scan(&mut self, d: &DiscoveredSourceFile) -> bool {
            self.should_calls.push(d.source_path.clone());
            if let Some(fps) = self.entries.get(&d.source_path)
                && fps
                    .iter()
                    .all(|(p, size)| fs::metadata(p).is_ok_and(|m| m.len() == *size))
            {
                return false;
            }
            self.last_admitted = Some(d.source_path.clone());
            true
        }
        fn complete(&mut self, c: &SourceCompletion) {
            assert_eq!(
                self.last_admitted.as_deref(),
                Some(c.source.source_path.as_path()),
                "completed primary must be the last admitted source"
            );
            self.completions.push(c.source.source_path.clone());
            let fps = std::iter::once(&c.source)
                .chain(c.required_sidecars.iter())
                .map(|d| (d.source_path.clone(), d.size_bytes.unwrap_or(0)))
                .collect();
            self.entries.insert(c.source.source_path.clone(), fps);
        }
    }
    let scan = |ctx: &ScanContext, ledger: &std::rc::Rc<std::cell::RefCell<SimLedger>>| {
        let l1 = ledger.clone();
        let mut should = move |d: &DiscoveredSourceFile| l1.borrow_mut().should_scan(d);
        let l2 = ledger.clone();
        let mut on_complete = move |c: &SourceCompletion| {
            l2.borrow_mut().complete(c);
            Ok(())
        };
        let mut convs = Vec::new();
        scan_antigravity_with_exclusions(
            ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut should),
                on_source_complete: Some(&mut on_complete),
            },
            &[],
            &mut |conv| {
                convs.push(conv);
                Ok(())
            },
        )
        .unwrap();
        convs
    };

    // Healthy pair: transcript_full is the parsed source.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let conv_dir = write_named_transcript(
        base,
        "90909090-9090-4909-8909-909090909090",
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nshort content\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(
        base,
        "90909090-9090-4909-8909-909090909090",
        "transcript_full.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nfull content\n</USER_REQUEST>")
        ),
    );
    let logs = conv_dir.join(".system_generated").join("logs");
    set_file_mtime(&logs.join("transcript.jsonl"), 1_000);
    set_file_mtime(&logs.join("transcript_full.jsonl"), 2_000);
    let short = logs.join("transcript.jsonl");
    let full = logs.join("transcript_full.jsonl");
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );

    let l = std::rc::Rc::new(std::cell::RefCell::new(SimLedger::default()));
    let convs = scan(&ctx, &l);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, full);
    {
        let b = l.borrow();
        assert_eq!(
            b.should_calls,
            vec![full.clone()],
            "only the top candidate is consulted on a fresh scan"
        );
        assert_eq!(b.completions, vec![full.clone()]);
        assert!(
            b.entries[&full].iter().any(|(p, _)| p == &short),
            "the unparsed fallback is recorded as full's sidecar"
        );
    }
    // Scan 2 unchanged: should_scan(full) hits the ledger → skip; the
    // fallback candidate is never consulted.
    let convs = scan(&ctx, &l);
    assert!(convs.is_empty());
    assert_eq!(
        l.borrow().should_calls,
        vec![full.clone(), full.clone()],
        "a ledger hit must end the pass before should_scan(short)"
    );

    // Blank transcript_full + valid short: parse falls back, the ledger
    // keys on `short`; rescan skips through short's entry.
    let dir2 = TempDir::new().unwrap();
    let base2 = dir2.path();
    let conv_dir2 = write_named_transcript(
        base2,
        "a1a1a1a1-a1a1-41a1-81a1-a1a1a1a1a1a1",
        "transcript.jsonl",
        &format!(
            "{}\n",
            ag_user_step("<USER_REQUEST>\nshort serves after blank full\n</USER_REQUEST>")
        ),
    );
    write_named_transcript(
        base2,
        "a1a1a1a1-a1a1-41a1-81a1-a1a1a1a1a1a1",
        "transcript_full.jsonl",
        "\n\n",
    );
    let logs2 = conv_dir2.join(".system_generated").join("logs");
    set_file_mtime(&logs2.join("transcript.jsonl"), 1_000);
    set_file_mtime(&logs2.join("transcript_full.jsonl"), 2_000);
    let short2 = logs2.join("transcript.jsonl");
    let full2 = logs2.join("transcript_full.jsonl");
    let ctx2 = ScanContext::with_roots(
        base2.to_path_buf(),
        vec![ScanRoot::local(base2.to_path_buf())],
        None,
    );

    let l2 = std::rc::Rc::new(std::cell::RefCell::new(SimLedger::default()));
    let convs = scan(&ctx2, &l2);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].source_path, short2);
    {
        let b = l2.borrow();
        assert_eq!(b.should_calls, vec![full2.clone(), short2.clone()]);
        assert_eq!(
            b.completions,
            vec![short2.clone()],
            "the completed primary is the fallback actually read"
        );
    }
    // Scan 2 unchanged: blank full is re-admitted but never completes;
    // short's ledger hit ends the pass.
    let convs = scan(&ctx2, &l2);
    assert!(convs.is_empty());
    assert_eq!(
        l2.borrow().should_calls,
        vec![full2.clone(), short2.clone(), full2.clone(), short2.clone()]
    );
}

// -----------------------------------------------------------------
// Subagent verdict metadata (metadata.is_subagent / metadata.parent_id)
// -----------------------------------------------------------------

fn ag_write_conversation(base: &Path, uuid: &str, steps: &[String]) {
    let logs = base
        .join("brain")
        .join(uuid)
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs).unwrap();
    let body = steps.join("\n") + "\n";
    fs::write(logs.join("transcript.jsonl"), body).unwrap();
}

fn ag_user_step(content: &str) -> String {
    serde_json::json!({
        "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
        "status": "DONE", "created_at": "2026-11-12T00:00:00Z", "content": content
    })
    .to_string()
}

fn ag_assistant_step() -> String {
    serde_json::json!({
        "step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE",
        "status": "DONE", "created_at": "2026-11-12T00:00:01Z", "content": "done"
    })
    .to_string()
}

fn ag_scan(base: &Path) -> Vec<NormalizedConversation> {
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    AntigravityConnector::new().scan(&ctx).unwrap()
}

fn ag_find<'a>(convs: &'a [NormalizedConversation], id: &str) -> &'a Value {
    &convs
        .iter()
        .find(|c| c.external_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("conversation {id} not emitted"))
        .metadata
}

#[test]
fn descriptor_names_subagent_with_parent() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let child = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    ag_write_conversation(
        base,
        parent,
        &[
            ag_user_step("<USER_REQUEST>\nparent task\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    ag_write_conversation(
        base,
        child,
        &[
            ag_user_step("<USER_REQUEST>\nchild task\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    let sub_dir = base
        .join("brain")
        .join(parent)
        .join(".system_generated")
        .join("subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(
        sub_dir.join(format!("{child}.json")),
        serde_json::json!({"subagentDescriptor": {"role": "researcher", "typeName": "research"}})
            .to_string(),
    )
    .unwrap();
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 2);
    let cm = ag_find(&convs, child);
    assert_eq!(cm["is_subagent"], serde_json::json!(true));
    assert_eq!(cm["parent_id"], serde_json::json!(parent));
    assert_eq!(cm["subagent_role"], serde_json::json!("researcher"));
    assert_eq!(cm["subagent_type"], serde_json::json!("research"));
    assert_eq!(
        ag_find(&convs, parent)["is_subagent"],
        serde_json::json!(false)
    );
}

#[test]
fn prompt_reminder_names_subagent_parent() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let parent = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let child = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    ag_write_conversation(
        base,
        parent,
        &[
            ag_user_step("<USER_REQUEST>\nparent\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    let reminder = format!(
        "caller agent (name: \"orchestrator\", id: \"{parent}\")\n<USER_REQUEST>\nchild task\n</USER_REQUEST>"
    );
    ag_write_conversation(base, child, &[ag_user_step(&reminder), ag_assistant_step()]);
    let convs = ag_scan(base);
    let cm = ag_find(&convs, child);
    assert_eq!(cm["is_subagent"], serde_json::json!(true));
    assert_eq!(cm["parent_id"], serde_json::json!(parent));
}

#[test]
fn conversation_without_evidence_is_main_when_index_complete() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    ag_write_conversation(
        base,
        uuid,
        &[
            ag_user_step("<USER_REQUEST>\nplain task\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].metadata["is_subagent"], serde_json::json!(false));
    assert!(convs[0].metadata.get("parent_id").is_none());
}

#[test]
fn single_conversation_scan_cannot_prove_main() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    ag_write_conversation(
        base,
        uuid,
        &[
            ag_user_step("<USER_REQUEST>\nplain task\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    // Scanning the conversation dir itself sees no sibling descriptors, so
    // the index is incomplete and a main verdict must not be guessed.
    let conv_dir = base.join("brain").join(uuid);
    let ctx = ScanContext::with_roots(conv_dir.clone(), vec![ScanRoot::local(conv_dir)], None);
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].metadata.get("is_subagent").is_none());
}

#[test]
fn excluded_parent_descriptor_is_not_read_and_withholds_false() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let parent = "11111111-1111-4111-8111-111111111111";
    let child = "22222222-2222-4222-8222-222222222222";
    ag_write_conversation(base, parent, &[ag_user_step("parent"), ag_assistant_step()]);
    ag_write_conversation(base, child, &[ag_user_step("child"), ag_assistant_step()]);
    let sub_dir = base
        .join("brain")
        .join(parent)
        .join(".system_generated")
        .join("subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(
        sub_dir.join(format!("{child}.json")),
        serde_json::json!({"subagentDescriptor": {"role": "worker", "typeName": "worker"}})
            .to_string(),
    )
    .unwrap();
    let excluded = vec![base.join("brain").join(parent)];
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf())],
        None,
    );
    let mut convs = Vec::new();
    scan_antigravity_with_exclusions(
        &ctx,
        &mut SourceScanHooks::default(),
        &excluded,
        &mut |conv| {
            convs.push(conv);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].external_id.as_deref(), Some(child));
    assert!(
        convs[0].metadata.get("is_subagent").is_none(),
        "excluded descriptor subtree must leave child verdict absent, not true or false: {:?}",
        convs[0].metadata
    );
}

#[cfg(unix)]
#[test]
fn unreadable_descriptor_dir_withholds_false_verdict() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let parent = "33333333-3333-4333-8333-333333333333";
    let child = "44444444-4444-4444-8444-444444444444";
    ag_write_conversation(base, parent, &[ag_user_step("parent"), ag_assistant_step()]);
    ag_write_conversation(base, child, &[ag_user_step("child"), ag_assistant_step()]);
    let sub_dir = base
        .join("brain")
        .join(parent)
        .join(".system_generated")
        .join("subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(sub_dir.join(format!("{child}.json")), "{}").unwrap();
    fs::set_permissions(&sub_dir, fs::Permissions::from_mode(0o000)).unwrap();

    // Only run the assertion when the OS enforces directory permissions
    // (e.g. non-root). Restore permissions before TempDir drops.
    let enforced = fs::read_dir(&sub_dir).is_err();
    let convs = ag_scan(base);
    fs::set_permissions(&sub_dir, fs::Permissions::from_mode(0o755)).unwrap();

    if enforced {
        let cm = ag_find(&convs, child);
        assert!(
            cm.get("is_subagent").is_none(),
            "unreadable descriptor directory must withhold false verdict: {cm:?}"
        );
    }
}

#[test]
fn sub_conversation_root_withholds_false_verdict() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "55555555-5555-4555-8555-555555555555";
    ag_write_conversation(base, uuid, &[ag_user_step("plain"), ag_assistant_step()]);
    let sub_root = base.join("brain").join(uuid).join(".system_generated");
    let ctx = ScanContext::with_roots(sub_root.clone(), vec![ScanRoot::local(sub_root)], None);
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(
        convs[0].metadata.get("is_subagent").is_none(),
        "sub-conversation root cannot see sibling descriptors"
    );
}

#[test]
fn parent_reminder_arm_is_anchored_like_native_regex() {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let false_positive_uuid = "66666666-6666-4666-8666-666666666666";
    let main_uuid = "77777777-7777-4777-8777-777777777777";
    let code_text =
        format!("example snippet: parent\", id: 1, other_id: \"x\", id: \"{false_positive_uuid}\"");
    ag_write_conversation(
        base,
        main_uuid,
        &[ag_user_step(&code_text), ag_assistant_step()],
    );

    let sub_uuid = "88888888-8888-4888-8888-888888888888";
    let valid_reminder = format!("parent\",  id: \"{main_uuid}\"\n<USER_REQUEST>\nhi");
    ag_write_conversation(
        base,
        sub_uuid,
        &[ag_user_step(&valid_reminder), ag_assistant_step()],
    );

    let convs = ag_scan(base);
    assert_eq!(
        ag_find(&convs, main_uuid)["is_subagent"],
        serde_json::json!(false)
    );
    assert_eq!(
        ag_find(&convs, sub_uuid)["is_subagent"],
        serde_json::json!(true)
    );
    assert_eq!(
        ag_find(&convs, sub_uuid)["parent_id"],
        serde_json::json!(main_uuid)
    );
}

#[test]
fn ide_layout_qualifies_parent_id_like_external_id() {
    let dir = TempDir::new().unwrap();
    let ide_base = dir.path().join(".gemini").join("antigravity");
    let parent = "99999999-9999-4999-8999-999999999999";
    let child = "aaaaaaaa-0000-4000-8000-aaaaaaaaaaaa";
    ag_write_conversation(
        &ide_base,
        parent,
        &[ag_user_step("parent"), ag_assistant_step()],
    );
    ag_write_conversation(
        &ide_base,
        child,
        &[ag_user_step("child"), ag_assistant_step()],
    );
    let sub_dir = ide_base
        .join("brain")
        .join(parent)
        .join(".system_generated")
        .join("subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(sub_dir.join(format!("{child}.json")), "{}").unwrap();

    let convs = ag_scan(&ide_base);
    let cm = ag_find(&convs, &format!("ide/{child}"));
    assert_eq!(cm["is_subagent"], serde_json::json!(true));
    assert_eq!(cm["parent_id"], serde_json::json!(format!("ide/{parent}")));
}

// -----------------------------------------------------------------
// Workspace extraction (<user_information>)
// -----------------------------------------------------------------

#[test]
fn workspace_extracts_from_user_information_block() {
    // The host stamps <user_information> as a sibling of <USER_REQUEST>
    // on the first user turn; both markers agree here.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "10101010-1010-4101-8101-101010101010";
    ag_write_conversation(
        base,
        uuid,
        &[
            ag_user_step(
                "<user_information>\nEnvironment context\nActive Workspaces:\n- /Users/alice/proj\nCommand Working Directory: /Users/alice/proj\n</user_information>\n<USER_REQUEST>\ndo the thing\n</USER_REQUEST>",
            ),
            ag_assistant_step(),
        ],
    );
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/Users/alice/proj")));
    // The wrapped request still normalizes to a clean user message.
    let user = convs[0].messages.iter().find(|m| m.role == "user").unwrap();
    assert_eq!(user.content, "do the thing");
}

#[test]
fn workspace_falls_back_to_command_working_directory() {
    // No Active Workspaces section: the stamped cwd alone is the
    // workspace. A SYSTEM record carries the block the same way.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "20202020-2020-4202-8202-202020202020";
    ag_write_conversation(
        base,
        uuid,
        &[
            serde_json::json!({
                "step_index": 0, "source": "SYSTEM", "type": "SYSTEM_MESSAGE",
                "status": "DONE", "created_at": "2026-11-12T00:00:00Z",
                "content": "<user_information>\nCommand Working Directory: /Users/carol/proj\n</user_information>"
            })
            .to_string(),
            ag_user_step("<USER_REQUEST>\nhi\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, Some(PathBuf::from("/Users/carol/proj")));
}

#[test]
fn workspace_absent_without_user_information() {
    // Conservative: request text and tool paths never mint a workspace.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "30303030-3030-4303-8303-303030303030";
    ag_write_conversation(
        base,
        uuid,
        &[
            ag_user_step("<USER_REQUEST>\nedit /Users/dave/proj/src/main.rs\n</USER_REQUEST>"),
            ag_assistant_step(),
        ],
    );
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, None);
}

#[test]
fn workspace_ignores_block_typed_inside_user_request() {
    // A <user_information> example pasted into the request body is
    // user-typed text, not host-stamped evidence — it must not mint a
    // workspace either.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "40404040-4040-4404-8404-404040404040";
    ag_write_conversation(
        base,
        uuid,
        &[
            ag_user_step(
                "<USER_REQUEST>\nexample: <user_information>\nActive Workspaces:\n- /Users/eve/fake\n</user_information>\n</USER_REQUEST>",
            ),
            ag_assistant_step(),
        ],
    );
    let convs = ag_scan(base);
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].workspace, None);
}

#[test]
fn workspace_rewrite_applies_to_extracted_workspace() {
    // The extracted workspace flows through
    // ScanRoot::apply_workspace_rewrite like every other connector's:
    // remote-mirror prefixes are rewritten and the original is kept in
    // metadata.cass.workspace_original.
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let uuid = "50505050-5050-4505-8505-505050505050";
    ag_write_conversation(
        base,
        uuid,
        &[
            ag_user_step(
                "<user_information>\nActive Workspaces:\n- /remote/ws/proj\n</user_information>\n<USER_REQUEST>\nhi\n</USER_REQUEST>",
            ),
            ag_assistant_step(),
        ],
    );
    let ctx = ScanContext::with_roots(
        base.to_path_buf(),
        vec![ScanRoot::local(base.to_path_buf()).with_rewrite("/remote/ws", "/local/mirror")],
        None,
    );
    let convs = AntigravityConnector::new().scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(
        convs[0].workspace,
        Some(PathBuf::from("/local/mirror/proj"))
    );
    assert_eq!(
        convs[0]
            .metadata
            .pointer("/cass/workspace_original")
            .and_then(Value::as_str),
        Some("/remote/ws/proj")
    );
}

#[test]
fn workspace_prefers_first_active_workspace_entry() {
    // The first listed workspace folder is the workspace identity; the
    // Command Working Directory is the fallback when no list is stamped.
    let info = "Active Workspaces:\n- /a/listed\n- /a/other\nCommand Working Directory: /a/cwd";
    assert_eq!(
        workspace_from_user_information(info),
        Some(PathBuf::from("/a/listed"))
    );
    // Non-absolute list entries are skipped, not picked.
    let info = "Active Workspaces:\n- rel/dir\n- /a/second\nCommand Working Directory: /a/cwd";
    assert_eq!(
        workspace_from_user_information(info),
        Some(PathBuf::from("/a/second"))
    );
}

#[test]
fn workspace_rejects_non_workspace_candidates() {
    for info in [
        // Bare filesystem roots are not workspaces.
        "Active Workspaces:\n- /\n",
        "Command Working Directory: /\n",
        "Command Working Directory: C:\\\n",
        // The agent's own ~/.gemini store is never a workspace.
        "Active Workspaces:\n- /Users/x/.gemini/antigravity\n",
        "Command Working Directory: /Users/x/.gemini/tmp\n",
        // Relative paths carry no provenance.
        "Command Working Directory: relative/path\n",
    ] {
        assert_eq!(workspace_from_user_information(info), None, "{info:?}");
    }
    // Windows drive paths are absolute too.
    assert_eq!(
        workspace_from_user_information("Command Working Directory: C:\\Users\\x\\proj"),
        Some(PathBuf::from("C:\\Users\\x\\proj"))
    );
    assert_eq!(
        workspace_from_user_information("Command Working Directory: D:/Users/x/proj"),
        Some(PathBuf::from("D:/Users/x/proj"))
    );
    // `file://` URIs (the Windsurf-style workspace stamp) unwrap to the
    // underlying absolute path, including the `/C:/...` drive form.
    assert_eq!(
        workspace_from_user_information("Active Workspaces:\n- file:///Users/alice/proj\n"),
        Some(PathBuf::from("/Users/alice/proj"))
    );
    assert_eq!(
        workspace_from_user_information("Command Working Directory: file:///C:/Users/x/proj"),
        Some(PathBuf::from("C:/Users/x/proj"))
    );
    assert_eq!(
        workspace_from_user_information("Active Workspaces:\n- file://relative/path\n"),
        None
    );
    // An empty section falls through to the cwd line.
    assert_eq!(
        workspace_from_user_information("Active Workspaces:\nCommand Working Directory: /a/cwd\n"),
        Some(PathBuf::from("/a/cwd"))
    );
}

//! Thread identity for Codex `history_mode=paginated` rollouts.
//!
//! One thread `<A>` is written as `rollout-<ts>-<A>.jsonl` plus continuations
//! `rollout-<ts2>-<A>_<B>.jsonl`; `<B>` names only the file. Read-only evidence
//! over 116 segments of 40 real threads: no record and no message-bearing id is
//! shared between two segments of a thread, filename-timestamp order equals
//! first-record order, and every segment starts at or after the previous
//! segment's last record. Segments therefore concatenate without deduplication.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::reader::{is_compressed_rollout, rollout_session_path};
use crate::connectors::scan::ScanRoot;

const UUID_LEN: usize = 36;

fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == UUID_LEN
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// The thread a rollout file belongs to, from its name alone. Names that do not
/// carry a thread UUID stay ungrouped, so legacy and hand-named rollouts keep
/// their one-file-one-conversation behavior.
pub(super) struct SegmentName {
    pub(super) thread_id: String,
    /// True for `...-<A>_<B>`: a continuation rather than the thread's original.
    pub(super) continuation: bool,
}

impl SegmentName {
    pub(super) fn of(path: &Path) -> Option<Self> {
        let stem = rollout_session_path(path);
        let name = stem.file_name()?.to_str()?;
        let rest = name.strip_prefix("rollout-")?;
        let (head, continuation) = match rest.rsplit_once('_') {
            Some((head, rollout_id)) if is_uuid(rollout_id) => (head, true),
            _ => (rest, false),
        };
        let split = head.len().checked_sub(UUID_LEN)?;
        if split == 0 || head.as_bytes()[split - 1] != b'-' {
            return None;
        }
        let thread_id = head.get(split..)?;
        if !is_uuid(thread_id) {
            return None;
        }
        Some(Self {
            thread_id: thread_id.to_string(),
            continuation,
        })
    }
}

/// One rollout file offered for grouping, with the provenance source boundaries
/// must preserve.
pub(super) struct Candidate {
    pub(super) root: ScanRoot,
    pub(super) path: PathBuf,
    pub(super) sessions_dir: PathBuf,
}

/// A thread's segments in merge order. `segments[0]` is the identity segment:
/// the original file, or the earliest surviving continuation when the original
/// is gone. It names the conversation and carries its completion.
pub(super) struct Thread {
    pub(super) segments: Vec<Candidate>,
}

/// Group candidates by thread, preserving first-appearance order so a
/// single-file archive keeps today's discovery and emission order.
pub(super) fn group(candidates: Vec<Candidate>) -> Vec<Thread> {
    let mut threads: Vec<Thread> = Vec::new();
    let mut slots: HashMap<(PathBuf, String), usize> = HashMap::new();
    for candidate in candidates {
        let slot = match SegmentName::of(&candidate.path) {
            Some(name) => {
                let key = (candidate.sessions_dir.clone(), name.thread_id);
                *slots.entry(key).or_insert_with(|| {
                    threads.push(Thread {
                        segments: Vec::new(),
                    });
                    threads.len() - 1
                })
            }
            None => {
                threads.push(Thread {
                    segments: Vec::new(),
                });
                threads.len() - 1
            }
        };
        threads[slot].segments.push(candidate);
    }
    for thread in &mut threads {
        order(&mut thread.segments);
    }
    threads
}

/// The rollout name without its directory or extension orders segments
/// chronologically: the timestamp prefix is fixed-width, and an original sorts
/// before a continuation carrying the same timestamp.
fn order(segments: &mut Vec<Candidate>) {
    if segments.len() < 2 {
        return;
    }
    segments.sort_by(|a, b| {
        rollout_session_path(&a.path)
            .cmp(&rollout_session_path(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut ordered: Vec<Candidate> = Vec::with_capacity(segments.len());
    for candidate in segments.drain(..) {
        if let Some(previous) = ordered.last() {
            let same_stem =
                rollout_session_path(&previous.path) == rollout_session_path(&candidate.path);
            let plain_twin =
                is_compressed_rollout(&previous.path) && !is_compressed_rollout(&candidate.path);
            let compressed_twin =
                !is_compressed_rollout(&previous.path) && is_compressed_rollout(&candidate.path);
            if same_stem && compressed_twin {
                continue;
            }
            if same_stem && plain_twin {
                ordered.pop();
            }
        }
        ordered.push(candidate);
    }
    *segments = ordered;
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "01a1d1b4-5529-7cd3-97db-d52326514bc2";
    const B: &str = "01a1d1ba-a4da-7b00-92b0-0368cfada0b2";

    #[test]
    fn parses_original_and_continuation_names() {
        let original = SegmentName::of(Path::new(&format!(
            "/s/2026/11/12/rollout-2026-11-12T22-40-18-{A}.jsonl"
        )))
        .unwrap();
        assert_eq!(original.thread_id, A);
        assert!(!original.continuation);

        let continuation = SegmentName::of(Path::new(&format!(
            "/s/rollout-2026-11-12T22-47-12-{A}_{B}.jsonl.zst"
        )))
        .unwrap();
        assert_eq!(continuation.thread_id, A);
        assert!(continuation.continuation);
    }

    #[test]
    fn leaves_names_without_a_thread_uuid_ungrouped() {
        for name in [
            "rollout-one.jsonl",
            "rollout-legacy.json",
            "rollout-2026-11-12T22-40-18-not-a-uuid.jsonl",
            "rollout-2026-11-12T22-40-18.jsonl",
            &format!("rollout-{A}.jsonl"),
            &format!("rollout-2026-11-12T22-40-18-{A}_short.jsonl"),
            "notes.jsonl",
        ] {
            assert!(
                SegmentName::of(Path::new(name)).is_none(),
                "must stay ungrouped: {name}"
            );
        }
    }

    fn candidate(path: &str) -> Candidate {
        Candidate {
            root: ScanRoot::local(PathBuf::from("/s")),
            path: PathBuf::from(path),
            sessions_dir: PathBuf::from("/s"),
        }
    }

    #[test]
    fn orders_by_name_and_keeps_the_original_first() {
        let threads = group(vec![
            candidate(&format!("/s/rollout-2026-11-13T09-00-00-{A}_{B}.jsonl")),
            candidate(&format!("/s/rollout-2026-11-12T22-47-12-{A}_{B}.jsonl")),
            candidate(&format!("/s/rollout-2026-11-12T22-40-18-{A}.jsonl")),
        ]);
        assert_eq!(threads.len(), 1);
        let paths: Vec<_> = threads[0]
            .segments
            .iter()
            .map(|s| s.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                format!("rollout-2026-11-12T22-40-18-{A}.jsonl"),
                format!("rollout-2026-11-12T22-47-12-{A}_{B}.jsonl"),
                format!("rollout-2026-11-13T09-00-00-{A}_{B}.jsonl"),
            ]
        );
    }

    #[test]
    fn keeps_distinct_threads_and_sessions_trees_apart() {
        let other = "01a1d1ff-1111-7222-8333-444444444444";
        let threads = group(vec![
            candidate(&format!("/s/rollout-2026-11-12T22-40-18-{A}.jsonl")),
            candidate(&format!("/s/rollout-2026-11-12T22-40-18-{other}.jsonl")),
            Candidate {
                sessions_dir: PathBuf::from("/elsewhere"),
                ..candidate(&format!("/elsewhere/rollout-2026-11-12T22-40-18-{A}.jsonl"))
            },
        ]);
        assert_eq!(threads.len(), 3);
    }

    #[test]
    fn collapses_a_plain_and_compressed_twin_to_the_plain_file() {
        let threads = group(vec![
            candidate(&format!("/s/rollout-2026-11-12T22-47-12-{A}_{B}.jsonl.zst")),
            candidate(&format!("/s/rollout-2026-11-12T22-40-18-{A}.jsonl")),
            candidate(&format!("/s/rollout-2026-11-12T22-40-18-{A}.jsonl.zst")),
        ]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].segments.len(), 2);
        assert!(!is_compressed_rollout(&threads[0].segments[0].path));
        assert_eq!(
            threads[0].segments[0].path,
            PathBuf::from(format!("/s/rollout-2026-11-12T22-40-18-{A}.jsonl"))
        );
        // A continuation that only exists compressed is still its own segment.
        assert!(is_compressed_rollout(&threads[0].segments[1].path));
    }
}

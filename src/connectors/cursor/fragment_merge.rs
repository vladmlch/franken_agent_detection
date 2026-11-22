//! Cursor Composer + Agent-transcript fragment merge and targeted lookup.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use frankensqlite::params;
use serde_json::{Map, Value};

use super::CursorConnector;
use super::agent_transcripts::{
    CURSOR_AGENT_SLUG, CURSOR_MERGE_REVISION, CURSOR_MERGED_FORMAT, CURSOR_TRANSCRIPT_RECORD_KEY,
};
use super::subagent_evidence::{ComposerDataCell, ComposerSubagentEvidence, ComposerSubagentFlag};
use crate::connectors::scan::ScanRoot;
use crate::connectors::sqlite_sync::{
    Connection, ConnectionExt, open_read_only_with_snapshot_fallback, store_is_provably_empty,
};
use crate::types::{NormalizedConversation, NormalizedMessage, reindex_messages};

// =====================================================================
// Cursor Agent transcripts (gh #306) + Composer fragment merge
// =====================================================================
//
// Cursor's newer "Agent" surface does NOT live in the Composer `state.vscdb`
// (`composerData:*`) path above. It writes clean per-session JSONL transcripts:
//
//   ~/.cursor/projects/<url-encoded-project>/agent-transcripts/<sid>/<sid>.jsonl
//
// Each line is one message in (essentially) Anthropic's wire shape:
//
//   {"role":"user","message":{"content":[{"type":"text","text":"..."}]}}
//   {"role":"assistant","message":{"content":[{"type":"text","text":"..."},
//                                  {"type":"tool_use","name":"Grep","input":{...}}]}}
//   {"type":"turn_ended","status":"...","error":"..."}   (session bookkeeping)
//
// This block scans those transcripts and MERGES each one with the Composer
// conversation carrying the same session id: the two sources are two
// fragments of one session. The Composer side (`state.vscdb`) holds user
// prompts and final answers with timestamps, model names and workspace
// dirs; the transcript holds the full assistant/tool/error record stream
// with no per-message timestamps. Design:
// .scratch/glm/2026-11-08-cursor-fragment-design §6.1–§6.5, whose rules in
// brief: the transcript record stream is the ordering skeleton; a
// forward-only, role-checked, one-to-one greedy matcher pairs Composer
// messages with records over whitespace-normalized text (tiers t1 equal >
// t2 prefix > t4 contained > t5 reverse-contained, floor MIN=1); a pair row
// keeps the record's role and content (the Composer variant wins only on
// t5, where the Composer side alone holds the truncated tail) and donates
// author/created_at/extra from the Composer bubble, adding a namespaced
// copy of the raw record for provenance; an unmatched Composer message
// inserts directly after the record paired with the previous Composer
// message — unless some role-ok liftable record anywhere holds a
// prefix/containment relation with it, in which case it is suppressed (the
// transcript is the more complete source and a same-text Composer row
// would be noise); a `turn_ended` record with a non-blank `error` string
// lifts as a synthetic assistant row at its record position; idx is
// re-indexed densely over the merged sequence. Sessions with no transcript
// keep today's Composer shape and sessions with no Composer row keep
// today's transcript shape, byte for byte. Freshness (§6.4): a session is
// emitted when EITHER side is fresh; a fresh transcript merges with its
// stale Composer row through a targeted `composerData:<sid>` lookup that
// probes the db files in the same sorted order a full scan uses.
//
// The shape is reporter-documented (gh #306) and unverifiable against real
// Cursor data here, so every step degrades gracefully: blank/malformed lines
// are skipped (never fatal), missing/unknown fields are tolerated, and a
// transcript yielding no messages is dropped with a warning rather than
// fabricating a conversation. The undocumented `agentKv:blob:*` binary KV
// entries are intentionally NOT decoded — guessing that private format would be
// the opposite of defensive; the JSONL transcripts are the clean documented
// source.

/// The relation tier between a Composer message and a transcript record
/// (§6.1), decided in priority order t1 > t2 > t4 > t5 with a floor of
/// MIN=1: any non-empty prefix/containment relation pairs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeTier {
    /// t1 — the normalized texts are equal; ties go to the transcript.
    Equal,
    /// t2 — the Composer text is a prefix of the record's text.
    ComposerPrefix,
    /// t4 — the Composer text is contained in the record's text.
    ComposerContained,
    /// t5 — the record's text is contained in the Composer text: only the
    /// Composer side holds the truncated tail, so it wins the row content.
    RecordContained,
}

/// One liftable transcript record: its normalized message, its
/// whitespace-normalized text-only matching key, and the raw record for
/// provenance.
struct LiftedRecord {
    message: NormalizedMessage,
    text_only_norm: String,
    record: Value,
}

/// A transcript record positioned in the merge stream (§6.2): the stream is
/// the ordering skeleton of every merged conversation.
enum TranscriptStreamEntry {
    /// A liftable record, by index into the merge's `lifts` vector.
    Lift(usize),
    /// A `turn_ended` record with a non-blank `error` string: lifted as a
    /// synthetic assistant row at its record position.
    Error { error: String, record: Value },
    /// Everything else (success `turn_ended`, unliftable or blank records):
    /// dropped, exactly like today's transcript-only parse.
    Skip,
}

impl CursorConnector {
    // ------------------------------------------------------------------
    // Composer + Agent-transcript merge (design §6.1–§6.5)
    // ------------------------------------------------------------------

    /// The session id a primary transcript file carries (its file stem) —
    /// the join key against Composer `composerData:<sid>` rows.
    pub(super) fn transcript_sid(transcript: &Path) -> Option<String> {
        transcript
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(String::from)
    }

    /// Matching key: collapse whitespace runs to single spaces and trim,
    /// case preserved (§6.1).
    fn whitespace_norm(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The text-only portion of a transcript record's content value — the
    /// matching key on the record side (§6.1). A plain string passes
    /// through; a block array contributes only text blocks (`type` absent
    /// or `text`/`input_text`/`output_text`), joined with `\n`. Tool
    /// renderings stay out of the key: the Composer side never carries
    /// them, so a `[Tool: ...]` segment would break every
    /// prefix/containment comparison.
    fn text_only_portion(content: &Value) -> String {
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        let Some(blocks) = content.as_array() else {
            return String::new();
        };
        let mut parts: Vec<&str> = Vec::new();
        for block in blocks {
            let block_type = block.get("type").and_then(Value::as_str);
            let is_text = matches!(
                block_type,
                None | Some("text" | "input_text" | "output_text")
            );
            if is_text && let Some(text) = block.get("text").and_then(Value::as_str) {
                parts.push(text);
            }
        }
        parts.join("\n")
    }

    /// The relation tier between a normalized Composer text and a normalized
    /// record text (§6.1). `None` means the two do not relate. Both texts
    /// empty cannot occur here — Composer messages are trim-nonempty by
    /// construction and an empty record key never lifts — so unlike the
    /// probe reference this returns `None` rather than t1 for that pair.
    fn pair_tier(composer_norm: &str, record_norm: &str) -> Option<MergeTier> {
        if composer_norm.is_empty() || record_norm.is_empty() {
            return None;
        }
        if composer_norm == record_norm {
            return Some(MergeTier::Equal);
        }
        if record_norm.starts_with(composer_norm) {
            return Some(MergeTier::ComposerPrefix);
        }
        if record_norm.contains(composer_norm) {
            return Some(MergeTier::ComposerContained);
        }
        if composer_norm.contains(record_norm) {
            return Some(MergeTier::RecordContained);
        }
        None
    }

    /// The non-blank string `error` of a `turn_ended` record, if it carries
    /// one — the error-record classifier (§6.2). Success records, blank
    /// errors and non-string errors all return `None` and stay dropped.
    fn turn_ended_error(rec: &Value) -> Option<&str> {
        if rec.get("type").and_then(Value::as_str) != Some("turn_ended") {
            return None;
        }
        rec.get("error")
            .and_then(Value::as_str)
            .filter(|error| !error.trim().is_empty())
    }

    /// Pair-row extra: the Composer bubble object merged with a namespaced
    /// copy of the raw transcript record (§6.2). A non-object bubble extra
    /// (not produced by `parse_bubble` today) degrades to the record alone
    /// rather than losing provenance.
    fn merged_row_extra(bubble: &Value, record: &Value) -> Value {
        let mut map = bubble.as_object().cloned().unwrap_or_default();
        map.insert(CURSOR_TRANSCRIPT_RECORD_KEY.to_string(), record.clone());
        Value::Object(map)
    }

    /// Merged-conversation metadata (§6.3): the Composer keys (source,
    /// model, unifiedMode), the transcript attribution/project-dir
    /// fallbacks, the merge markers, and the transcript-file provenance
    /// (one file → string, several → array, in stream order).
    fn merged_conversation_metadata(
        composer_metadata: &Value,
        transcript_attribution: &str,
        transcript_project_dir: Option<&str>,
        files: &[(ScanRoot, PathBuf)],
    ) -> Value {
        let mut map = composer_metadata.as_object().cloned().unwrap_or_else(|| {
            // Not produced today; keep the source marker if it is ever lost.
            let mut fallback = Map::new();
            fallback.insert(
                "source".to_string(),
                Value::String(CURSOR_AGENT_SLUG.to_string()),
            );
            fallback
        });
        map.insert(
            "cursor_workspace_attribution".to_string(),
            Value::String(transcript_attribution.to_string()),
        );
        if let Some(project) = transcript_project_dir {
            map.insert(
                "cursor_project_dir".to_string(),
                Value::String(project.to_string()),
            );
        }
        map.insert(
            "cursor_format".to_string(),
            Value::String(CURSOR_MERGED_FORMAT.to_string()),
        );
        map.insert(
            "cursor_merge_revision".to_string(),
            Value::from(CURSOR_MERGE_REVISION),
        );
        let provenance = if files.len() == 1 {
            Value::String(files[0].1.to_string_lossy().to_string())
        } else {
            Value::Array(
                files
                    .iter()
                    .map(|(_, path)| Value::String(path.to_string_lossy().to_string()))
                    .collect(),
            )
        };
        map.insert("cursor_transcript_path".to_string(), provenance);
        Value::Object(map)
    }

    /// Merge one Composer conversation with every transcript file of the
    /// same session id (design §6.1–§6.3). The transcript record stream is
    /// the ordering skeleton: pair rows keep the record's role and content
    /// (the Composer variant wins only on t5) and donate
    /// author/created_at/extra from the Composer bubble plus a namespaced
    /// copy of the raw record; unmatched Composer messages insert directly
    /// after the record paired with the previous Composer message (head if
    /// none, tail after the walk) unless the suppress rule drops them;
    /// `turn_ended` error records lift as synthetic assistant rows;
    /// unpaired records keep today's transcript row shape; idx is
    /// re-indexed densely. Deterministic for fixed inputs.
    pub(super) fn merge_composer_transcripts(
        composer: NormalizedConversation,
        files: &[(ScanRoot, PathBuf)],
        excluded_paths: &[PathBuf],
        tick: &dyn Fn(),
    ) -> NormalizedConversation {
        // Call sites only merge sids that have at least one transcript file;
        // keep the Composer conversation whole if that is ever violated.
        let Some((first_root, first_transcript)) = files.first() else {
            return composer;
        };

        // Record stream plus per-file time bounds, mirroring what
        // parse_agent_transcript would compute for each file.
        let mut stream: Vec<TranscriptStreamEntry> = Vec::new();
        let mut lifts: Vec<LiftedRecord> = Vec::new();
        let mut lift_stream_pos: Vec<usize> = Vec::new();
        let mut transcript_started: Option<i64> = None;
        let mut transcript_ended: Option<i64> = None;
        for (_root, transcript) in files {
            tick();
            let mut file_started: Option<i64> = None;
            let mut file_ended: Option<i64> = None;
            for rec in &Self::read_agent_transcript_records(transcript) {
                if let Some(error) = Self::turn_ended_error(rec) {
                    stream.push(TranscriptStreamEntry::Error {
                        error: error.to_string(),
                        record: rec.clone(),
                    });
                    continue;
                }
                if let Some(message) = Self::agent_record_to_message(rec) {
                    if let Some(ts) = message.created_at {
                        file_started = Some(file_started.map_or(ts, |cur| cur.min(ts)));
                        file_ended = Some(file_ended.map_or(ts, |cur| cur.max(ts)));
                    }
                    let content_val = rec
                        .pointer("/message/content")
                        .or_else(|| rec.get("content"));
                    let text_only = content_val.map_or_else(String::new, Self::text_only_portion);
                    lift_stream_pos.push(stream.len());
                    lifts.push(LiftedRecord {
                        text_only_norm: Self::whitespace_norm(&text_only),
                        message,
                        record: rec.clone(),
                    });
                    stream.push(TranscriptStreamEntry::Lift(lifts.len() - 1));
                } else {
                    stream.push(TranscriptStreamEntry::Skip);
                }
            }
            // Same mtime fallback parse_agent_transcript applies per file.
            if file_started.is_none() {
                if let Some(ms) = Self::file_mtime_ms(transcript) {
                    file_started = Some(ms);
                    file_ended = Some(ms);
                }
            }
            if let Some(start) = file_started {
                transcript_started = Some(transcript_started.map_or(start, |cur| cur.min(start)));
            }
            if let Some(end) = file_ended {
                transcript_ended = Some(transcript_ended.map_or(end, |cur| cur.max(end)));
            }
        }

        // Transcript-side conversation fallbacks, taken from the first file
        // in stream order (deterministic) and mirroring
        // parse_agent_transcript for that file.
        let explicit_workspace =
            Self::agent_workspace_from_metadata(first_transcript, excluded_paths);
        let transcript_attribution = if explicit_workspace.is_some() {
            "workspace_trusted"
        } else {
            "unresolved"
        };
        let transcript_workspace = explicit_workspace.map(|workspace| {
            PathBuf::from(
                first_root.rewrite_workspace(&workspace.to_string_lossy(), Some(CURSOR_AGENT_SLUG)),
            )
        });
        let transcript_project_dir = Self::agent_project_dir_name(first_transcript);
        // Title fallback over the lifted messages in stream order — the same
        // rule parse_agent_transcript applies (first user line, else the
        // first message's). A Composer title is effectively always Some, so
        // this rarely fires, but the fallback must match the transcript
        // shape when it does.
        let mut first_user: Option<&NormalizedMessage> = None;
        let mut first_any: Option<&NormalizedMessage> = None;
        for lift in &lifts {
            if first_any.is_none() {
                first_any = Some(&lift.message);
            }
            if lift.message.role == "user" && first_user.is_none() {
                first_user = Some(&lift.message);
            }
        }
        let transcript_title = first_user
            .or(first_any)
            .and_then(|message| message.content.lines().find(|line| !line.trim().is_empty()))
            .map(|line| line.chars().take(100).collect::<String>());

        // Forward-only, role-checked, one-to-one greedy matcher over the
        // whitespace-normalized texts (§6.1): the Composer cursor never
        // moves backward, each record is used at most once, and the first
        // positional relation wins (tier order only labels the relation).
        let composer_norms: Vec<String> = composer
            .messages
            .iter()
            .map(|message| Self::whitespace_norm(&message.content))
            .collect();
        let n_composer = composer.messages.len();
        let n_lifts = lifts.len();
        let mut used = vec![false; n_lifts];
        let mut pair_lift: Vec<Option<usize>> = vec![None; n_composer];
        let mut pair_tier_of: Vec<Option<MergeTier>> = vec![None; n_composer];
        let mut cursor = 0;
        for ci in 0..n_composer {
            let mut j = cursor;
            while j < n_lifts {
                if used[j] || lifts[j].message.role != composer.messages[ci].role {
                    j += 1;
                    continue;
                }
                if let Some(tier) = Self::pair_tier(&composer_norms[ci], &lifts[j].text_only_norm) {
                    used[j] = true;
                    pair_lift[ci] = Some(j);
                    pair_tier_of[ci] = Some(tier);
                    cursor = j + 1;
                    break;
                }
                j += 1;
            }
        }

        // Conservative suppress rule (§6.2): an unmatched Composer message
        // with a role-ok prefix/containment relation to ANY liftable
        // record — paired or not, at any position — is suppressed rather
        // than inserted.
        let suppressed: Vec<bool> = (0..n_composer)
            .map(|ci| {
                pair_lift[ci].is_none()
                    && (0..n_lifts).any(|j| {
                        lifts[j].message.role == composer.messages[ci].role
                            && Self::pair_tier(&composer_norms[ci], &lifts[j].text_only_norm)
                                .is_some()
                    })
            })
            .collect();

        // Insert anchoring: directly after the record paired with the
        // previous Composer message, at the head before the first pair, and
        // past the last record for tail messages.
        let mut last_pair_pos: Option<usize> = None;
        let mut insert_at: Vec<Option<usize>> = vec![None; n_composer];
        for ci in 0..n_composer {
            if let Some(j) = pair_lift[ci] {
                last_pair_pos = Some(lift_stream_pos[j]);
            } else if !suppressed[ci] {
                insert_at[ci] = Some(last_pair_pos.map_or(0, |pos| pos + 1));
            } else {
                tracing::debug!(
                    external_id = ?composer.external_id,
                    "cursor: composer message suppressed (role-ok transcript relation)"
                );
            }
        }
        // Anchor → Composer indexes in Composer order (several messages can
        // share an anchor). Read by position only; never iterated, so the
        // HashMap cannot affect determinism.
        let mut insert_groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for (ci, anchor) in insert_at.iter().enumerate() {
            if let Some(anchor) = anchor {
                insert_groups.entry(*anchor).or_default().push(ci);
            }
        }
        let mut composer_of_lift: Vec<Option<usize>> = vec![None; n_lifts];
        for (ci, j) in pair_lift.iter().enumerate() {
            if let Some(j) = j {
                composer_of_lift[*j] = Some(ci);
            }
        }

        // Emit the merged sequence, then dense re-index (§6.2).
        let mut messages: Vec<NormalizedMessage> = Vec::new();
        for (pos, entry) in stream.iter().enumerate() {
            if let Some(inserts) = insert_groups.get(&pos) {
                for &ci in inserts {
                    // Unmatched, unsuppressed Composer message: today's
                    // Composer row, anchored here.
                    messages.push(composer.messages[ci].clone());
                }
            }
            match entry {
                TranscriptStreamEntry::Lift(j) => {
                    let lift = &lifts[*j];
                    if let Some(ci) = composer_of_lift[*j] {
                        let composer_msg = &composer.messages[ci];
                        // Pair row: the record's role and — except on t5,
                        // where only the Composer side holds the truncated
                        // tail — its content, with author, timestamp and
                        // bubble extra donated from the Composer side plus
                        // the raw record for provenance.
                        messages.push(NormalizedMessage {
                            idx: 0,
                            role: lift.message.role.clone(),
                            author: composer_msg.author.clone(),
                            created_at: composer_msg.created_at,
                            content: if pair_tier_of[ci] == Some(MergeTier::RecordContained) {
                                composer_msg.content.clone()
                            } else {
                                lift.message.content.clone()
                            },
                            extra: Self::merged_row_extra(&composer_msg.extra, &lift.record),
                            snippets: Vec::new(),
                            invocations: lift.message.invocations.clone(),
                        });
                    } else {
                        // Unpaired record: today's transcript row, unchanged.
                        messages.push(lift.message.clone());
                    }
                }
                TranscriptStreamEntry::Error { error, record } => {
                    // Synthetic assistant row at the record's position (§6.2).
                    messages.push(NormalizedMessage {
                        idx: 0,
                        role: "assistant".to_string(),
                        author: None,
                        created_at: None,
                        content: error.clone(),
                        extra: {
                            let mut map = Map::new();
                            map.insert(CURSOR_TRANSCRIPT_RECORD_KEY.to_string(), record.clone());
                            Value::Object(map)
                        },
                        snippets: Vec::new(),
                        invocations: Vec::new(),
                    });
                }
                TranscriptStreamEntry::Skip => {}
            }
        }
        // Tail-anchored inserts land after the walk.
        if let Some(inserts) = insert_groups.get(&stream.len()) {
            for &ci in inserts {
                messages.push(composer.messages[ci].clone());
            }
        }
        reindex_messages(&mut messages);

        NormalizedConversation {
            agent_slug: composer.agent_slug,
            external_id: composer.external_id,
            title: composer.title.or(transcript_title),
            workspace: composer.workspace.or(transcript_workspace),
            source_path: composer.source_path,
            // Conversation-level Composer fields win; the transcript bounds
            // fill the gaps, and ended_at takes whichever side ends later.
            started_at: composer.started_at.or(transcript_started),
            ended_at: composer.ended_at.max(transcript_ended),
            metadata: Self::merged_conversation_metadata(
                &composer.metadata,
                transcript_attribution,
                transcript_project_dir.as_deref(),
                files,
            ),
            messages,
        }
    }

    /// The `composerData:<sid>` value cell of one store, if the row exists: a
    /// primary-key point query (F8: never a value-predicated scan). Shares the
    /// range scan's storage-class semantics (`composer_data_cell`): TEXT and
    /// valid-UTF-8 BLOB bytes carry the composer JSON, NULL counts as absent,
    /// and a non-UTF-8 BLOB is undecodable — never lossy-decoded (round-10
    /// N10), so malformed bytes cannot invent an identifier on this path.
    fn composer_cell_for_sid(conn: &Connection, sid: &str) -> Result<Option<ComposerDataCell>> {
        let key = format!("composerData:{sid}");
        let rows = conn.query_map_collect(
            "SELECT value FROM cursorDiskKV WHERE key = ?",
            params![key.as_str()],
            |row| Ok(Self::composer_data_cell(row, 0)),
        )?;
        Ok(rows.into_iter().next())
    }

    /// Resolve the Composer conversations for `sids` from db files an
    /// incremental scan would otherwise not read (§6.4): a fresh transcript
    /// needs its stale Composer half. Each store is probed in the same
    /// sorted order a full scan reads them, and a store resolves a sid only
    /// when its row parses to a conversation — the same winner the full
    /// scan's cross-store first-emitted dedupe would pick. A store that
    /// cannot be opened fails the scan (F5/F9 semantics: identical to a
    /// full scan over the same files); per-sid query failures warn and move
    /// on, mirroring the full scan's composerData-query tolerance. The
    /// conversation-level since cutoff is bypassed — the session is emitted
    /// anyway because its transcript half is fresh. A resolved conversation
    /// gets the workspace rewrite of the scan root its store belongs to,
    /// exactly as a full scan would apply it.
    #[cfg(test)]
    pub(super) fn lookup_composer_conversations(
        db_paths: &[(ScanRoot, PathBuf)],
        sids: &[String],
        tick: &dyn Fn(),
    ) -> (HashMap<String, Option<NormalizedConversation>>, Vec<String>) {
        Self::lookup_composer_conversations_with_evidence(db_paths, sids, tick, None)
    }

    pub(super) fn lookup_composer_conversations_with_evidence(
        db_paths: &[(ScanRoot, PathBuf)],
        sids: &[String],
        tick: &dyn Fn(),
        mut external_evidence: Option<&mut ComposerSubagentEvidence>,
    ) -> (HashMap<String, Option<NormalizedConversation>>, Vec<String>) {
        let mut found: HashMap<String, Option<NormalizedConversation>> = HashMap::new();
        let mut failures: Vec<String> = Vec::new();
        let mut pending: Vec<String> = sids.to_vec();
        for (root, db_path) in db_paths {
            if pending.is_empty() {
                break;
            }
            if store_is_provably_empty(db_path) {
                continue;
            }
            tick();
            let conn =
                match open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref()) {
                    Ok(conn) => conn,
                    Err(error) => {
                        failures.push(format!(
                            "{}: failed to open Cursor db: {error:#}",
                            db_path.display()
                        ));
                        continue;
                    }
                };
            if let Err(error) = conn.execute("PRAGMA busy_timeout = 5000;") {
                failures.push(format!(
                    "{}: failed to set busy_timeout: {error:#}",
                    db_path.display()
                ));
                continue;
            }
            // Round-8 N7/N8 ruling: no explicit read transaction is held on
            // this live store — the snapshot guarantee comes from read ORDER.
            // Positive/parent evidence and the per-sid composer rows are read
            // first (each statement its own short implicit read); each
            // composer's own-header flags are read LAST, after every composer
            // row was seen. Cursor commits a subagent's header together with
            // or before its rows, so the header observed here is the one that
            // belongs to those rows (or a newer, more-true one) — never an
            // obsolete false for a subagent whose rows were emitted.
            // Contradicting evidence committed after the row reads stays
            // unobserved: the accepted B1 limit.
            let mut local_evidence =
                Self::composer_subagent_evidence_from_conn(&conn, db_path, tick);
            let mut hits: Vec<(String, String)> = Vec::new();
            let mut still_pending: Vec<String> = Vec::new();
            for sid in &pending {
                tick();
                match Self::composer_cell_for_sid(&conn, sid) {
                    Ok(Some(ComposerDataCell::Text(value))) => {
                        hits.push((sid.clone(), value));
                    }
                    // The row was captured but cannot be interpreted at all:
                    // like a bad cell in the bulk read, its payload might have
                    // listed any composer, so it marks this store uncertain
                    // for the rest of the scan (round-10 N6/N10).
                    Ok(Some(ComposerDataCell::Undecodable)) => {
                        Self::merge_flag(
                            &mut local_evidence.own,
                            sid.clone(),
                            ComposerSubagentFlag {
                                flag_unknown: true,
                                ..ComposerSubagentFlag::default()
                            },
                        );
                        local_evidence.uncertain_stores.insert(db_path.clone());
                        still_pending.push(sid.clone());
                    }
                    Ok(Some(ComposerDataCell::Null) | None) => {
                        still_pending.push(sid.clone());
                    }
                    Err(error) => {
                        tracing::warn!(
                            db = %db_path.display(),
                            session = %sid,
                            error = %error,
                            "cursor: targeted composerData lookup failed; a stale Composer row may be missed"
                        );
                        still_pending.push(sid.clone());
                    }
                }
            }
            // Round-8 N6 residual: the JSON evidence of every successfully
            // fetched hit is folded in independently of the bulk read's
            // success — the exact row being emitted is itself positive/parent
            // evidence.
            for (sid, value) in &hits {
                Self::fold_composer_value(&mut local_evidence, sid, value, db_path);
            }
            // The own-header flags are read AGAIN after the composer rows
            // (the evidence build read them earlier). The late flags merge
            // into the evidence map so positives/parents from EITHER read
            // prove a subagent — a header that turned true between the reads
            // is still seen — but the false predicate below consults ONLY
            // this post-row map (round-9 N9): an absent or flagless late
            // header must not borrow a false that only the early read saw.
            let late_headers = Self::read_composer_headers(&conn, tick);
            for (id, flag) in &late_headers {
                Self::merge_flag(&mut local_evidence.headers, id.clone(), flag.clone());
            }
            // Round-7 N6: merge this store's current positive/parent evidence
            // into the earlier cross-store evidence before resolving verdicts —
            // the cached pass may predate the very rows being emitted. The
            // merge extends the shared map in place (round-10 O5): everything
            // captured here — positives, parents and this store's uncertainty —
            // stays visible to every later store probed this scan.
            let evidence = if let Some(ev) = external_evidence.as_deref_mut() {
                Self::extend_subagent_evidence(ev, &local_evidence);
                &*ev
            } else {
                &local_evidence
            };
            // False comes from the LATE (post-row) own-header map only
            // (round-6/8/9 rulings): `local_evidence.headers` additionally
            // carries the earlier read's flags, which still feed positives.
            let own_headers = &late_headers;
            for (sid, value) in hits {
                let key = format!("composerData:{sid}");
                let parsed = Self::parse_composer_data(
                    &key,
                    &value,
                    db_path,
                    None,
                    &mut HashSet::new(),
                    Some(&conn),
                );
                if let Some(mut composer_conv) = parsed {
                    let own_header_false =
                        Self::own_header_certifies_false(&sid, own_headers, evidence, db_path);
                    Self::apply_subagent_verdict(
                        &mut composer_conv,
                        &sid,
                        evidence,
                        own_header_false,
                    );
                    root.apply_workspace_rewrite(&mut composer_conv);
                    found.insert(sid, Some(composer_conv));
                } else {
                    // Parses to nothing here; a later store may
                    // still resolve it, exactly like the full
                    // scan's cross-store dedupe.
                    still_pending.push(sid);
                }
            }
            pending = still_pending;
        }
        (found, failures)
    }
}

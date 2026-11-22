//! Connector for Cursor IDE chat history.
//!
//! Cursor stores chat history in SQLite databases (state.vscdb) within:
//! - macOS: ~/Library/Application Support/Cursor/User/globalStorage/
//! - macOS workspaces: ~/Library/Application Support/Cursor/User/workspaceStorage/{id}/
//! - Linux: ~/.config/Cursor/User/globalStorage/
//! - Windows: %APPDATA%/Cursor/User/globalStorage/
//!
//! Chat data is stored in the `cursorDiskKV` table with keys like:
//! - `composerData:{uuid}` - Composer/chat session data (JSON)
//!
//! And in the `ItemTable` with keys like:
//! - `workbench.panel.aichat.view.aichat.chatdata` - Legacy chat data

#![allow(
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    clippy::match_same_arms,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::option_if_let_else,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

mod agent_transcripts;
mod composer_parser;
mod fragment_merge;
mod roots;
mod storage_roots;
mod subagent_evidence;

#[cfg(test)]
mod tests_agent_transcripts;
#[cfg(test)]
mod tests_composer_parsing;
#[cfg(test)]
mod tests_fragment_merge;
#[cfg(test)]
mod tests_scan_and_edge_cases;
#[cfg(test)]
mod tests_subagent_concurrency;
#[cfg(test)]
mod tests_subagent_verdicts_basic;
#[cfg(test)]
mod tests_subagent_verdicts_matrix;
#[cfg(test)]
mod tests_support;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;

// `ScanRoot` is also imported by `storage_roots` via `use super::ScanRoot;`.
use super::scan::{DiscoveredSourceFile, ScanContext, ScanRoot};
use super::utils::{excluded_scan_paths_from_env, path_is_excluded};
use super::{Connector, file_modified_since, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};
use subagent_evidence::ComposerSubagentEvidence;

pub struct CursorConnector;

impl Default for CursorConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorConnector {
    pub fn new() -> Self {
        Self
    }
    #[allow(clippy::unused_self)]
    fn scan_cursor_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        Self::scan_cursor_with_exclusions(ctx, &excluded_paths, on_conversation)
    }

    pub(super) fn scan_cursor_with_exclusions(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let tick = || ctx.tick();
        // The same composer id can be mirrored across DBs (globalStorage vs
        // workspaceStorage, explicit overlapping roots, backups). extract_from_db
        // only dedupes within one store, so span a session-id set across ALL
        // stores here — first occurrence wins (candidate order is
        // explicit/data_dir before defaults). Conversations with no
        // external_id pass through: dropping them would trade a duplicate bug
        // for a loss bug. Mirrors the opencode connector's spanning dedupe.
        let mut seen_db_session_ids: HashSet<String> = HashSet::new();

        // Transcript inventory and its fresh subset. The merge (§6.4) always
        // reads both halves of a session: the inventory feeds the merges
        // regardless of mtime, the fresh subset decides what is emitted.
        let inventory = Self::agent_transcript_inventory(ctx, excluded_paths);
        let fresh_transcripts: Vec<(ScanRoot, PathBuf)> = inventory
            .iter()
            .filter(|(_, transcript)| {
                Self::agent_transcript_is_fresh(transcript, ctx.since_ts, excluded_paths)
            })
            .cloned()
            .collect();
        let mut files_by_sid: HashMap<String, Vec<(ScanRoot, PathBuf)>> = HashMap::new();
        for (root, transcript) in &inventory {
            if let Some(sid) = Self::transcript_sid(transcript) {
                files_by_sid
                    .entry(sid)
                    .or_default()
                    .push((root.clone(), transcript.clone()));
            }
        }
        let mut merged_sids: HashSet<String> = HashSet::new();
        let mut failures = super::checked_scan::ScanFailures::new("Cursor");

        // Composer (`state.vscdb` -> `composerData:*`).
        //
        // Every db file is collected in the fixed order a full scan reads
        // them — the targeted lookup below must probe exactly this order —
        // of which only the fresh ones are extracted now (§6.4).
        //
        // Local fix (cass review 2026-11-07, F9): a store that cannot be
        // read fails the scan instead of hiding behind a warn (F5
        // semantics) — a BusyRecovery-class store is read through the
        // snapshot fallback in `open_read_only_with_snapshot_fallback`,
        // provably-empty stores are skipped, and every other open failure
        // reaches the host.
        let mut all_db_paths: Vec<(ScanRoot, PathBuf)> = Vec::new();
        let roots = Self::source_roots(ctx);

        for root in &roots {
            if !root.path.exists() {
                continue;
            }
            if path_is_excluded(&root.path, excluded_paths) {
                continue;
            }

            for db_path in Self::find_db_files(&root.path) {
                if path_is_excluded(&db_path, excluded_paths) {
                    continue;
                }
                all_db_paths.push((root.clone(), db_path));
            }
        }

        // Cross-DB evidence is only needed to resolve positive parent/flag
        // evidence across multiple stores (round-6: false no longer uses it).
        let needs_cross_db_evidence = all_db_paths.len() > 1;
        let mut cross_db_evidence: Option<ComposerSubagentEvidence> = None;

        for (root, db_path) in &all_db_paths {
            // Skip files not modified since last scan
            if !file_modified_since(db_path, ctx.since_ts) {
                continue;
            }
            if super::checked_scan::skip_provably_empty_store("Cursor", db_path) {
                continue;
            }
            ctx.tick();
            if needs_cross_db_evidence && cross_db_evidence.is_none() {
                cross_db_evidence = Some(Self::collect_cross_db_evidence(&all_db_paths, &tick));
            }

            match Self::extract_from_db_with_evidence(
                db_path,
                ctx.since_ts,
                &tick,
                cross_db_evidence.as_mut(),
            ) {
                Ok(convs) => {
                    let mut kept = 0;
                    for mut conv in convs {
                        match conv.external_id.as_ref() {
                            Some(id) if !seen_db_session_ids.insert(id.clone()) => {
                                continue;
                            }
                            _ => {}
                        }
                        root.apply_workspace_rewrite(&mut conv);
                        kept += 1;
                        // Pass 1 — every fresh-store Composer conversation:
                        // a sid with any transcript merges (the db side is
                        // fresh, so the transcript is parsed regardless of
                        // its own mtime); composer-only sids keep today's
                        // shape.
                        let sid = conv.external_id.clone();
                        let files = sid.as_ref().and_then(|sid| files_by_sid.get(sid).cloned());
                        if let (Some(sid), Some(files)) = (sid, files) {
                            merged_sids.insert(sid);
                            on_conversation(Self::merge_composer_transcripts(
                                conv,
                                &files,
                                excluded_paths,
                                &tick,
                            ))?;
                        } else {
                            on_conversation(conv)?;
                        }
                    }
                    tracing::debug!(
                        path = %db_path.display(),
                        count = kept,
                        "cursor extracted conversations"
                    );
                }
                Err(e) => {
                    failures.record(db_path, &e);
                }
            }
        }

        // Pass 2 — fresh transcripts pass 1 did not already merge. A
        // targeted composerData:<sid> lookup over the db files resolves the
        // stale Composer half (§6.4); sids with no resolvable half keep
        // today's per-file transcript-only output, byte for byte.
        let mut need_lookup: Vec<String> = Vec::new();
        let mut need_lookup_seen: HashSet<String> = HashSet::new();
        for sid in fresh_transcripts
            .iter()
            .filter_map(|(_, transcript)| Self::transcript_sid(transcript))
        {
            if !merged_sids.contains(&sid) && need_lookup_seen.insert(sid.clone()) {
                need_lookup.push(sid);
            }
        }
        if !need_lookup.is_empty() && needs_cross_db_evidence && cross_db_evidence.is_none() {
            cross_db_evidence = Some(Self::collect_cross_db_evidence(&all_db_paths, &tick));
        }
        let (looked_up, lookup_failures) = Self::lookup_composer_conversations_with_evidence(
            &all_db_paths,
            &need_lookup,
            &tick,
            cross_db_evidence.as_mut(),
        );
        for (root, transcript) in fresh_transcripts {
            ctx.tick();
            let sid = Self::transcript_sid(&transcript);
            if let Some(sid) = &sid
                && merged_sids.contains(sid)
            {
                continue;
            }
            if let Some(sid) = &sid
                && let Some(Some(composer)) = looked_up.get(sid)
            {
                merged_sids.insert(sid.clone());
                let files = files_by_sid.get(sid).cloned().unwrap_or_default();
                on_conversation(Self::merge_composer_transcripts(
                    composer.clone(),
                    &files,
                    excluded_paths,
                    &tick,
                ))?;
                continue;
            }
            // Transcript-only: today's per-file shape, byte for byte.
            if let Some(conv) = Self::parse_agent_transcript(&transcript, &root, excluded_paths) {
                on_conversation(conv)?;
            }
        }
        // F5/F9 semantics for the targeted lookup too: a store it could not
        // read fails the scan instead of quietly degrading merged sessions
        // to transcript-only rows. Extraction failures ride the same
        // accumulator so one error names every unreadable store.
        for failure in lookup_failures {
            failures.push(failure);
        }
        failures.finish()
    }
}

impl Connector for CursorConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("cursor").unwrap_or_else(DetectionResult::not_found)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_cursor_with_callback(ctx, on_conversation)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut all_convs = Vec::new();
        self.scan_with_callback(ctx, &mut |conv| {
            all_convs.push(conv);
            Ok(())
        })?;
        Ok(all_convs)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Self::discover_sources(ctx)
    }
}

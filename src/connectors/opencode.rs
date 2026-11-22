//! `OpenCode` connector for JSON file-based and `SQLite` storage.
//!
//! **v1.2+ (SQLite):** Data is stored in `~/.local/share/opencode/opencode.db`
//! with tables: session, message, part. The `message.data` and `part.data` columns
//! contain JSON blobs.
//!
//! **Pre-v1.2 (JSON):** Data at `~/.local/share/opencode/storage/` using files:
//!   - session/{projectID}/{sessionID}.json  - Session metadata
//!   - message/{sessionID}/{messageID}.json  - Message metadata
//!   - part/{messageID}/{partID}.json        - Actual message content
//!
//! **Tool part identity and state (cass F14):** every message that has parts
//! (v2 `content` items, 1.x `part` rows/files) archives the raw part JSONs in
//! `extra["parts"]` in delivery order, so the host can prove the part
//! inventory verbatim. A failed run's `state.error` text renders inline as
//! `[Tool Error]` (it is source text) — a plain string or an object carrying
//! the text under `message` (the measured real shape is `{message, type}`) —
//! and a tool part that renders no text still keeps its message alive with
//! empty content rather than dropping the part's identity and state with it.

#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::if_not_else,
    clippy::manual_let_else,
    clippy::manual_string_new,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::nonminimal_bool,
    clippy::option_if_let_else,
    clippy::single_option_map,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps,
    clippy::unreadable_literal
)]

mod discovery;
mod models;
mod parts;
mod sqlite;

#[cfg(test)]
mod tests_sqlite_core;
#[cfg(test)]
mod tests_sqlite_v2_and_wal;
#[cfg(test)]
mod tests_storage_and_edge;
#[cfg(test)]
mod tests_support;

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use walkdir::WalkDir;

use super::scan::{DiscoveredSourceFile, ScanContext};
use super::utils::{dedupe_path_key, excluded_scan_paths_from_env, path_is_excluded};
use super::{Connector, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};
use discovery::looks_like_opencode_storage;
use parts::{
    load_messages_with_exclusions, normalize_opencode_timestamp, parse_session_file,
    session_has_updates,
};

pub struct OpenCodeConnector;

impl Default for OpenCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeConnector {
    pub fn new() -> Self {
        Self
    }
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("opencode").unwrap_or_else(DetectionResult::not_found)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        self.scan_with_callback(ctx, &mut |conv| {
            convs.push(conv);
            Ok(())
        })?;
        Ok(convs)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut scanned_dbs: HashSet<PathBuf> = HashSet::new();

        // --- Phase 1: Try SQLite database(s) (v1.2+) ---
        // Collect candidate database paths in priority order:
        //   1. If ctx.data_dir looks like a path to `opencode.db` itself
        //      (has a `.db` extension), use it as-is.
        //   2. Otherwise, if ctx.data_dir is non-empty, treat it as a
        //      directory and check for `<data_dir>/opencode.db`.
        //   3. Always add the built-in default search list. This ensures
        //      we find the canonical XDG location even when explicit scan
        //      roots or a stale detection path were passed in (see issue #174).
        //
        // Non-existence of any candidate is filtered at iteration time
        // (`if !db.exists() { continue; }` below), so we do not gate the
        // `extension == "db"` branch on `.exists()` — otherwise a
        // user-supplied .db path that doesn't exist yet would silently
        // fall through to the "join opencode.db" branch and produce a
        // nonsense `/path/to/file.db/opencode.db` candidate.
        let mut db_candidates: Vec<PathBuf> = Vec::new();
        if ctx.data_dir.extension().is_some_and(|ext| ext == "db") {
            db_candidates.push(ctx.data_dir.clone());
        } else if !ctx.data_dir.as_os_str().is_empty() {
            db_candidates.push(ctx.data_dir.join("opencode.db"));
        }

        if !ctx.use_default_detection() {
            for scan_root in &ctx.scan_roots {
                if path_is_excluded(&scan_root.path, &excluded_paths) {
                    continue;
                }
                for store in [
                    &[".local", "share", "opencode"][..],
                    &[".config", "opencode"][..],
                    &["Library", "Application Support", "opencode"][..],
                    &["AppData", "Roaming", "opencode"][..],
                ] {
                    if let Some(mirrored) = scan_root.mirrored_store(store) {
                        db_candidates.push(mirrored.join("opencode.db"));
                    }
                }
                Self::append_explicit_db_candidates(&mut db_candidates, &scan_root.path);
            }
        }

        if Self::allow_local_default_dbs(ctx) {
            db_candidates.extend(Self::sqlite_db_candidates());
        }

        // Deduplicate while preserving priority order.
        {
            let mut seen = HashSet::new();
            db_candidates
                .retain(|p| !path_is_excluded(p, &excluded_paths) && seen.insert(p.clone()));
        }

        // Session ids seen across ALL db candidates: stale migrated copies
        // (e.g. ~/.config/opencode/opencode.db left behind after a move to
        // ~/.local/share) share session ids with the live store, and
        // emitting both double-indexes every conversation. First-wins by
        // candidate priority order (explicit/data_dir before defaults).
        let mut seen_db_session_ids: HashSet<String> = HashSet::new();
        // Local fix (cass review 2026-11-07, F9): a store that cannot be
        // read fails the scan instead of hiding behind a warn (F5
        // semantics) — a BusyRecovery-class store is read through the
        // snapshot fallback in `open_read_only_with_snapshot_fallback`,
        // and every other open failure reaches the host; a valid but empty
        // store is "nothing to read" (the F9 empty-store rule).
        let mut failures = super::checked_scan::ScanFailures::new("OpenCode");

        for db in db_candidates {
            if !db.is_file() || path_is_excluded(&db, &excluded_paths) {
                continue;
            }
            // Canonicalize if possible so two routes to the same file are
            // still deduplicated (e.g. via symlink or `./`-prefixed path).
            let canonical = std::fs::canonicalize(&db).unwrap_or_else(|_| db.clone());
            if path_is_excluded(&canonical, &excluded_paths) || !scanned_dbs.insert(canonical) {
                continue;
            }
            if super::checked_scan::skip_provably_empty_store("OpenCode", &db) {
                continue;
            }
            match Self::extract_from_sqlite(&db, ctx.since_ts, ctx.progress_tick.as_deref()) {
                Ok(sqlite_convs) => {
                    let fresh_count = sqlite_convs.len();
                    for mut conv in sqlite_convs {
                        ctx.apply_workspace_rewrite_for_path(&db, &mut conv);
                        if conv
                            .external_id
                            .as_ref()
                            .is_none_or(|id| seen_db_session_ids.insert(id.clone()))
                        {
                            on_conversation(conv)?;
                        }
                    }
                    tracing::debug!(
                        "opencode sqlite: found {fresh_count} sessions in {}",
                        db.display()
                    );
                }
                Err(e) => {
                    // The whole chain: the outer context alone ("failed to
                    // query OpenCode sessions from session") hid the engine
                    // error behind exports failing under a live writer (cass
                    // GH #519).
                    failures.record(&db, &e);
                }
            }
        }

        // Collect seen IDs from SQLite results to avoid duplicates with JSON
        let mut seen_ids: HashSet<String> = seen_db_session_ids;

        // --- Phase 2: Fall back to JSON file storage (pre-v1.2) ---
        let mut storage_roots: Vec<PathBuf> = Vec::new();
        if ctx.use_default_detection() {
            if ctx.data_dir.exists() && looks_like_opencode_storage(&ctx.data_dir) {
                storage_roots.push(ctx.data_dir.clone());
            } else if ctx.allow_local_default_stores {
                if let Some(root) = Self::storage_root() {
                    storage_roots.push(root);
                }
            }
        } else {
            if ctx.data_dir.exists()
                && !path_is_excluded(&ctx.data_dir, &excluded_paths)
                && looks_like_opencode_storage(&ctx.data_dir)
            {
                storage_roots.push(ctx.data_dir.clone());
            }
            for scan_root in &ctx.scan_roots {
                if path_is_excluded(&scan_root.path, &excluded_paths) {
                    continue;
                }
                let mut candidates = vec![scan_root.path.clone()];
                for store in [
                    &[".local", "share", "opencode", "storage"][..],
                    &[".config", "opencode", "storage"][..],
                    &["Library", "Application Support", "opencode", "storage"][..],
                    &["AppData", "Roaming", "opencode", "storage"][..],
                ] {
                    if let Some(mirrored) = scan_root.mirrored_store(store) {
                        candidates.push(mirrored);
                    }
                }
                Self::append_explicit_storage_candidates(&mut candidates, &scan_root.path);
                for candidate in candidates {
                    if candidate.exists()
                        && !path_is_excluded(&candidate, &excluded_paths)
                        && looks_like_opencode_storage(&candidate)
                    {
                        storage_roots.push(candidate);
                    }
                }
            }
        }

        storage_roots.retain(|root| !path_is_excluded(root, &excluded_paths));
        if storage_roots.is_empty() {
            // No JSONL phase left to run: the F9 sqlite failures still fail
            // the scan (a healthy run without failures stays Ok).
            return failures.finish();
        }

        storage_roots.sort();
        storage_roots.dedup();

        let mut seen_session_files: HashSet<PathBuf> = HashSet::new();

        for storage_root in storage_roots {
            let session_dir = storage_root.join("session");
            let message_dir = storage_root.join("message");
            let part_dir = storage_root.join("part");

            if !session_dir.exists() || path_is_excluded(&session_dir, &excluded_paths) {
                continue;
            }

            // Collect all session files
            let session_files: Vec<PathBuf> = WalkDir::new(&session_dir)
                .into_iter()
                .filter_entry(|e| !path_is_excluded(e.path(), &excluded_paths))
                .flatten()
                .filter(|e| e.file_type().is_file())
                .filter(|e| !path_is_excluded(e.path(), &excluded_paths))
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|ext| ext == "json")
                        .unwrap_or(false)
                })
                .map(|e| e.path().to_path_buf())
                .collect();

            for session_file in session_files {
                if !seen_session_files.insert(dedupe_path_key(&session_file)) {
                    continue;
                }
                if !session_has_updates(&session_file, &message_dir, &part_dir, ctx.since_ts) {
                    continue;
                }
                ctx.tick();

                // Parse session
                let session = match parse_session_file(&session_file) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(
                            "opencode: failed to parse session {}: {e}",
                            session_file.display()
                        );
                        continue;
                    }
                };

                // Deduplicate by session ID
                if !seen_ids.insert(session.id.clone()) {
                    continue;
                }

                // Load messages for this session
                let session_msg_dir = message_dir.join(&session.id);
                let messages = if session_msg_dir.exists()
                    && !path_is_excluded(&session_msg_dir, &excluded_paths)
                {
                    load_messages_with_exclusions(&session_msg_dir, &part_dir, &excluded_paths)?
                } else {
                    Vec::new()
                };

                if messages.is_empty() {
                    continue;
                }

                // Build normalized conversation
                let msg_started_at = messages.iter().filter_map(|m| m.created_at).min();
                let msg_ended_at = messages.iter().filter_map(|m| m.created_at).max();

                let started_at = session
                    .time
                    .as_ref()
                    .and_then(|t| normalize_opencode_timestamp(t.created))
                    .or(msg_started_at);
                let ended_at = session
                    .time
                    .as_ref()
                    .and_then(|t| normalize_opencode_timestamp(t.updated))
                    .or(msg_ended_at)
                    .or(started_at);

                let workspace = session.directory.map(PathBuf::from);
                let title = session.title.or_else(|| {
                    messages
                        .first()
                        .and_then(|m| m.content.lines().next())
                        .map(|s| s.chars().take(100).collect())
                });

                let mut conv = NormalizedConversation {
                    agent_slug: "opencode".into(),
                    external_id: Some(session.id.clone()),
                    title,
                    workspace,
                    source_path: session_file.clone(),
                    started_at,
                    ended_at,
                    metadata: serde_json::json!({
                        "session_id": session.id,
                        "project_id": session.project_id,
                    }),
                    messages,
                };
                ctx.apply_workspace_rewrite_for_path(&session_file, &mut conv);
                on_conversation(conv)?;
            }
        }

        failures.finish()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Self::discover_sources(ctx)
    }
}

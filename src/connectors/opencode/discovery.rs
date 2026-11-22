//! Storage root discovery and source enumeration for `OpenCode`.

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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use super::OpenCodeConnector;
use super::parts::session_has_updates;
use crate::connectors::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use crate::connectors::utils::{
    dedupe_path_key, env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded,
};

impl OpenCodeConnector {
    /// Get the OpenCode storage directory.
    /// OpenCode stores sessions in ~/.local/share/opencode/storage/
    pub(super) fn storage_root() -> Option<PathBuf> {
        // Check for env override first (useful for testing)
        if let Some(p) = env_path_nonempty("OPENCODE_STORAGE_ROOT") {
            if p.is_dir() {
                return Some(p);
            }
        }

        // Primary location: XDG data directory (Linux/macOS)
        if let Some(data) = dirs::data_local_dir() {
            let storage_dir = data.join("opencode/storage");
            if storage_dir.is_dir() {
                return Some(storage_dir);
            }
        }

        // XDG config path — on macOS dirs::data_local_dir() returns
        // ~/Library/Application Support which misses XDG-style installs
        // that place data under ~/.config/opencode/ (#146).
        if let Some(config) = dirs::config_dir() {
            let storage_dir = config.join("opencode/storage");
            if storage_dir.is_dir() {
                return Some(storage_dir);
            }
        }

        // Fallback: ~/.local/share/opencode/storage
        if let Some(home) = dirs::home_dir() {
            let storage_dir = home.join(".local/share/opencode/storage");
            if storage_dir.is_dir() {
                return Some(storage_dir);
            }
            // Also check ~/.config/opencode/storage for XDG-style installs
            let xdg_storage = home.join(".config/opencode/storage");
            if xdg_storage.is_dir() {
                return Some(xdg_storage);
            }
        }

        None
    }

    /// Home/XDG DB discovery is only valid when every supplied scan root
    /// refers to this machine. A remote-mirror root must never pull the
    /// local canonical `opencode.db` into its scan — the caller attributes
    /// everything returned by that invocation to the remote source, so the
    /// local sessions would be double-indexed under the remote `source_id`
    /// (cass#357). Vacuously true when `scan_roots` is empty, preserving
    /// default detection and the issue #174 local-explicit-root behavior.
    /// A context that opted out via
    /// [`ScanContext::without_local_default_stores`] never resolves
    /// home/XDG stores at all — hermetic harnesses rely on this so no
    /// in-process test can read the developer's real `~/.local/share/opencode`.
    pub(super) fn allow_local_default_dbs(ctx: &ScanContext) -> bool {
        ctx.allow_local_default_stores && ctx.scan_roots.iter().all(|root| root.origin.is_local())
    }

    /// All known locations where OpenCode may store its SQLite database,
    /// in priority order. Exposed so that scan paths can fall back through
    /// them even when the caller provided an explicit (non-matching)
    /// `data_dir`.
    pub(super) fn sqlite_db_candidates() -> Vec<PathBuf> {
        Self::sqlite_db_candidates_from(
            env_path_nonempty("OPENCODE_SQLITE_DB"),
            dirs::home_dir().as_deref(),
            dirs::data_local_dir().as_deref(),
            dirs::config_dir().as_deref(),
        )
    }

    /// Pure, env-free variant of [`Self::sqlite_db_candidates`] for tests.
    /// Every environment-dependent input is passed in explicitly so the
    /// resulting list is fully deterministic.
    pub(super) fn sqlite_db_candidates_from(
        explicit_override: Option<PathBuf>,
        home: Option<&Path>,
        xdg_data: Option<&Path>,
        xdg_config: Option<&Path>,
    ) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();

        // 1. Explicit override for tests / custom installs.
        if let Some(path) = explicit_override {
            out.push(path);
        }

        // 2. XDG data dir via $HOME. OpenCode uses this on every platform,
        //    including macOS. This MUST be tried before
        //    dirs::data_local_dir() because on macOS the latter resolves
        //    to ~/Library/Application Support which OpenCode does not use.
        if let Some(home) = home {
            out.push(home.join(".local/share/opencode/opencode.db"));
            out.push(home.join(".config/opencode/opencode.db"));
        }

        // 3. Platform-native data/config dirs (for users who have moved
        //    OpenCode into non-XDG locations).
        if let Some(data) = xdg_data {
            out.push(data.join("opencode/opencode.db"));
        }
        if let Some(config) = xdg_config {
            out.push(config.join("opencode/opencode.db"));
        }

        // Deduplicate preserving order — multiple dirs helpers may resolve
        // to the same path on a given platform (e.g. macOS config_dir ==
        // data_local_dir).
        let mut seen = HashSet::new();
        out.retain(|p| seen.insert(p.clone()));
        out
    }

    fn is_local_share_root(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "share")
            && path
                .parent()
                .is_some_and(|p| p.file_name().is_some_and(|name| name == ".local"))
    }

    fn is_appdata_roaming(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "Roaming")
            && path
                .parent()
                .is_some_and(|p| p.file_name().is_some_and(|name| name == "AppData"))
    }

    pub(super) fn append_explicit_db_candidates(out: &mut Vec<PathBuf>, base: &Path) {
        let file_name = base.file_name().and_then(|n| n.to_str());
        let is_config = file_name.is_some_and(|n| n == ".config");
        let is_local = file_name.is_some_and(|n| n == ".local");
        let is_share = Self::is_local_share_root(base);
        let is_app_support = file_name.is_some_and(|n| n == "Application Support");
        let is_roaming = Self::is_appdata_roaming(base);
        let is_opencode = file_name.is_some_and(|n| n == "opencode");

        if base.extension().is_some_and(|ext| ext == "db") {
            out.push(base.to_path_buf());
        } else {
            out.push(base.join("opencode.db"));
        }

        if is_opencode {
            out.push(base.join("opencode.db"));
        }

        // Treat base as an XDG-style root for opencode data.
        out.push(base.join("opencode/opencode.db"));

        if is_local {
            out.push(base.join("share/opencode/opencode.db"));
        }

        if !(is_config || is_local || is_share || is_app_support || is_roaming || is_opencode) {
            out.push(base.join(".local/share/opencode/opencode.db"));
            out.push(base.join(".config/opencode/opencode.db"));
            out.push(base.join("Library/Application Support/opencode/opencode.db"));
            out.push(base.join("AppData/Roaming/opencode/opencode.db"));
        }
    }

    pub(super) fn append_explicit_storage_candidates(out: &mut Vec<PathBuf>, base: &Path) {
        let file_name = base.file_name().and_then(|n| n.to_str());
        let is_config = file_name.is_some_and(|n| n == ".config");
        let is_local = file_name.is_some_and(|n| n == ".local");
        let is_share = Self::is_local_share_root(base);
        let is_app_support = file_name.is_some_and(|n| n == "Application Support");
        let is_roaming = Self::is_appdata_roaming(base);
        let is_opencode = file_name.is_some_and(|n| n == "opencode");

        out.push(base.join("opencode/storage"));

        if is_opencode {
            out.push(base.join("storage"));
        }

        if is_local {
            out.push(base.join("share/opencode/storage"));
        }

        if !(is_config || is_local || is_share || is_app_support || is_roaming || is_opencode) {
            out.push(base.join(".local/share/opencode/storage"));
            out.push(base.join(".config/opencode/storage"));
            out.push(base.join("Library/Application Support/opencode/storage"));
            out.push(base.join("AppData/Roaming/opencode/storage"));
        }
    }

    pub(super) fn sqlite_source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut db_candidates: Vec<ScanRoot> = Vec::new();
        if ctx.data_dir.extension().is_some_and(|ext| ext == "db") {
            db_candidates.push(ScanRoot::local(ctx.data_dir.clone()));
        } else if !ctx.data_dir.as_os_str().is_empty() {
            db_candidates.push(ScanRoot::local(ctx.data_dir.join("opencode.db")));
        }

        if !ctx.use_default_detection() {
            for scan_root in &ctx.scan_roots {
                let mut candidates = Vec::new();
                for store in [
                    &[".local", "share", "opencode"][..],
                    &[".config", "opencode"][..],
                    &["Library", "Application Support", "opencode"][..],
                    &["AppData", "Roaming", "opencode"][..],
                ] {
                    if let Some(mirrored) = scan_root.mirrored_store(store) {
                        candidates.push(mirrored.join("opencode.db"));
                    }
                }
                Self::append_explicit_db_candidates(&mut candidates, &scan_root.path);
                db_candidates.extend(candidates.into_iter().map(|path| scan_root.with_path(path)));
            }
        }

        if Self::allow_local_default_dbs(ctx) {
            db_candidates.extend(
                Self::sqlite_db_candidates()
                    .into_iter()
                    .map(ScanRoot::local),
            );
        }

        let mut seen = HashSet::new();
        db_candidates.retain(|root| seen.insert(root.path.clone()));
        db_candidates
    }

    pub(super) fn storage_source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut storage_roots: Vec<ScanRoot> = Vec::new();
        if ctx.use_default_detection() {
            if ctx.data_dir.exists() && looks_like_opencode_storage(&ctx.data_dir) {
                storage_roots.push(ScanRoot::local(ctx.data_dir.clone()));
            } else if ctx.allow_local_default_stores {
                if let Some(root) = Self::storage_root() {
                    storage_roots.push(ScanRoot::local(root));
                }
            }
        } else {
            if ctx.data_dir.exists() && looks_like_opencode_storage(&ctx.data_dir) {
                storage_roots.push(ScanRoot::local(ctx.data_dir.clone()));
            }
            for scan_root in &ctx.scan_roots {
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
                    if candidate.exists() && looks_like_opencode_storage(&candidate) {
                        storage_roots.push(scan_root.with_path(candidate));
                    }
                }
            }
        }

        storage_roots.sort_by(|a, b| a.path.cmp(&b.path));
        storage_roots.dedup_by(|a, b| a.path == b.path);
        storage_roots
    }

    pub(super) fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut out = Vec::new();
        Self::discover_sqlite_sources(ctx, &excluded_paths, &mut out);
        Self::discover_legacy_storage_sources(ctx, &excluded_paths, &mut out);
        Ok(out)
    }

    fn discover_sqlite_sources(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
        out: &mut Vec<DiscoveredSourceFile>,
    ) {
        let mut seen = HashSet::new();

        for root in Self::sqlite_source_roots(ctx) {
            if path_is_excluded(&root.path, excluded_paths) {
                continue;
            }
            if !root.path.is_file() {
                continue;
            }
            let canonical = std::fs::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone());
            if path_is_excluded(&canonical, excluded_paths) {
                continue;
            }
            if !seen.insert(canonical) {
                continue;
            }
            out.push(
                DiscoveredSourceFile::new(
                    "opencode",
                    &root,
                    root.path.clone(),
                    DiscoveredSourceRole::SqliteDatabase,
                    true,
                )
                .with_fs_metadata(),
            );
        }
    }

    fn discover_legacy_storage_sources(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
        out: &mut Vec<DiscoveredSourceFile>,
    ) {
        let mut seen_session_files = HashSet::new();
        for root in Self::storage_source_roots(ctx) {
            if path_is_excluded(&root.path, excluded_paths) {
                continue;
            }
            let session_dir = root.path.join("session");
            let message_dir = root.path.join("message");
            let part_dir = root.path.join("part");
            if !session_dir.exists() || path_is_excluded(&session_dir, excluded_paths) {
                continue;
            }

            let session_files: Vec<PathBuf> = WalkDir::new(&session_dir)
                .into_iter()
                .filter_entry(|entry| !path_is_excluded(entry.path(), excluded_paths))
                .flatten()
                .filter(|entry| entry.file_type().is_file())
                .filter(|entry| !path_is_excluded(entry.path(), excluded_paths))
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .map(|entry| entry.path().to_path_buf())
                .collect();

            for session_file in session_files {
                if !seen_session_files.insert(dedupe_path_key(&session_file)) {
                    continue;
                }
                if !session_has_updates(&session_file, &message_dir, &part_dir, ctx.since_ts) {
                    continue;
                }
                out.push(
                    DiscoveredSourceFile::new(
                        "opencode",
                        &root,
                        session_file.clone(),
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                );

                Self::discover_legacy_session_sidecars(
                    &root,
                    &session_file,
                    &message_dir,
                    &part_dir,
                    excluded_paths,
                    out,
                );
            }
        }
    }

    fn discover_legacy_session_sidecars(
        root: &ScanRoot,
        session_file: &Path,
        message_dir: &Path,
        part_dir: &Path,
        excluded_paths: &[PathBuf],
        out: &mut Vec<DiscoveredSourceFile>,
    ) {
        let Some(session_id) = session_file.file_stem().and_then(|name| name.to_str()) else {
            return;
        };
        let session_msg_dir = message_dir.join(session_id);
        if !session_msg_dir.exists() || path_is_excluded(&session_msg_dir, excluded_paths) {
            return;
        }

        for entry in WalkDir::new(&session_msg_dir)
            .into_iter()
            .filter_entry(|entry| !path_is_excluded(entry.path(), excluded_paths))
            .flatten()
        {
            if !entry.file_type().is_file()
                || path_is_excluded(entry.path(), excluded_paths)
                || !entry.path().extension().is_some_and(|ext| ext == "json")
            {
                continue;
            }

            let message_file = entry.path().to_path_buf();
            out.push(
                DiscoveredSourceFile::new(
                    "opencode",
                    root,
                    message_file.clone(),
                    DiscoveredSourceRole::MetadataSidecar,
                    true,
                )
                .with_fs_metadata(),
            );
            Self::discover_legacy_message_parts(root, &message_file, part_dir, excluded_paths, out);
        }
    }

    fn discover_legacy_message_parts(
        root: &ScanRoot,
        message_file: &Path,
        part_dir: &Path,
        excluded_paths: &[PathBuf],
        out: &mut Vec<DiscoveredSourceFile>,
    ) {
        let Some(message_id) = message_file.file_stem().and_then(|name| name.to_str()) else {
            return;
        };
        let message_part_dir = part_dir.join(message_id);
        if path_is_excluded(&message_part_dir, excluded_paths) {
            return;
        }
        for part_entry in WalkDir::new(&message_part_dir)
            .into_iter()
            .filter_entry(|entry| !path_is_excluded(entry.path(), excluded_paths))
            .flatten()
        {
            if !part_entry.file_type().is_file()
                || path_is_excluded(part_entry.path(), excluded_paths)
                || part_entry
                    .path()
                    .extension()
                    .is_none_or(|ext| ext != "json")
            {
                continue;
            }
            out.push(
                DiscoveredSourceFile::new(
                    "opencode",
                    root,
                    part_entry.path().to_path_buf(),
                    DiscoveredSourceRole::MetadataSidecar,
                    true,
                )
                .with_fs_metadata(),
            );
        }
    }
}

/// Check if a directory looks like OpenCode storage
pub(super) fn looks_like_opencode_storage(path: &std::path::Path) -> bool {
    // Check for characteristic subdirectories.
    // We require 'session' and 'message' to be present to confirm this is an OpenCode storage root.
    // relying on the path name containing "opencode" is too loose and causes shadowing
    // if the CASS data directory has "opencode" in its name.
    path.join("session").exists() && path.join("message").exists()
}

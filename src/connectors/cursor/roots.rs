//! Cursor storage root discovery and WSL path probing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::CursorConnector;
use super::storage_roots;
use crate::connectors::file_modified_since;
use crate::connectors::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use crate::connectors::utils::{dedupe_path_key, excluded_scan_paths_from_env, path_is_excluded};

impl CursorConnector {
    /// Compute the exclusive upper bound for prefix range scans.
    ///
    /// For prefix `abc:`, this returns `abc;`, so
    /// `key >= prefix AND key < upper` matches all keys that start with `prefix`.
    pub(super) fn prefix_upper_bound(prefix: &str) -> String {
        let mut upper = prefix.to_string();
        if let Some(last) = upper.pop() {
            if let Some(next_char) = std::char::from_u32(last as u32 + 1) {
                upper.push(next_char);
            } else {
                // Fallback for char overflow: keep the last char and append max scalar.
                upper.push(last);
                upper.push('\u{10FFFF}');
            }
        }
        upper
    }

    /// Get the base Cursor application support directory
    pub fn app_support_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/Cursor/User"))
        }
        #[cfg(target_os = "linux")]
        {
            // Check if we're in WSL and should look at Windows Cursor paths first
            if Self::is_wsl()
                && let Some(wsl_path) = Self::find_wsl_cursor_path()
            {
                return Some(wsl_path);
            }
            // Fall back to Linux native path
            dirs::home_dir().map(|h| h.join(".config/Cursor/User"))
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_dir().map(|d| d.join("Cursor/User"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None
        }
    }

    pub(super) fn looks_like_base(path: &Path) -> bool {
        path.join("globalStorage").exists()
            || path.join("workspaceStorage").exists()
            || path
                .file_name()
                .is_some_and(|n| n == "globalStorage" || n == "workspaceStorage")
            || (path.is_file() && path.file_name().is_some_and(|n| n == "state.vscdb"))
    }

    fn append_explicit_roots(roots: &mut Vec<PathBuf>, base: &Path) {
        if Self::looks_like_base(base) {
            roots.push(base.to_path_buf());
        }

        let candidates = [
            base.join(".config/Cursor/User"),
            base.join("Library/Application Support/Cursor/User"),
            base.join("AppData/Roaming/Cursor/User"),
            base.join("Cursor/User"),
        ];

        for candidate in candidates {
            if Self::looks_like_base(&candidate) {
                roots.push(candidate);
            }
        }
    }

    /// Check if running inside Windows Subsystem for Linux
    #[cfg(target_os = "linux")]
    pub(super) fn is_wsl() -> bool {
        std::fs::read_to_string("/proc/version")
            .map(|v| v.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }

    /// Find Cursor installation path via WSL mount points
    /// Probes /mnt/c/Users/*/AppData/Roaming/Cursor/User
    #[cfg(target_os = "linux")]
    pub(super) fn find_wsl_cursor_path() -> Option<PathBuf> {
        let mnt_c_users = Path::new("/mnt/c/Users");
        if !mnt_c_users.exists() {
            return None;
        }

        // Optimization: Try to get the Windows username directly to avoid scanning all users
        if let Ok(output) = std::process::Command::new("cmd.exe")
            .args(["/c", "echo %USERNAME%"])
            .output()
        {
            if output.status.success() {
                let username = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let is_safe = !username.is_empty()
                    && !username.contains(['/', '\\'])
                    && !username.contains(':');
                if is_safe {
                    let user_path = mnt_c_users.join(&username);
                    let cursor_path = user_path.join("AppData/Roaming/Cursor/User");
                    if cursor_path.exists() {
                        return Some(cursor_path);
                    }
                }
            }
        }

        // Fallback: Scan directories if username detection failed
        // Limit depth and skip system directories to avoid hangs
        for entry in std::fs::read_dir(mnt_c_users).ok()?.flatten() {
            // Skip system directories
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            if name_str == "Default"
                || name_str == "Public"
                || name_str == "All Users"
                || name_str == "Default User"
                || name_str.starts_with('.')
            {
                continue;
            }

            let cursor_path = entry.path().join("AppData/Roaming/Cursor/User");
            // Check existence without traversing deeper
            if cursor_path.join("globalStorage").exists() {
                tracing::debug!(
                    path = %cursor_path.display(),
                    "Found Windows Cursor installation via WSL"
                );
                return Some(cursor_path);
            }
        }
        None
    }

    /// Find all state.vscdb files in Cursor storage
    pub(super) fn find_db_files(base: &Path) -> Vec<PathBuf> {
        let mut dbs = Vec::new();

        // 1. Check if base itself is a DB (explicit file scan)
        if base.is_file() && base.file_name().is_some_and(|n| n == "state.vscdb") {
            dbs.push(base.to_path_buf());
            return dbs;
        }

        // 2. Check if base contains state.vscdb directly (e.g. pointing at globalStorage)
        let direct_db = base.join("state.vscdb");
        if direct_db.exists() {
            dbs.push(direct_db);
        }

        // 3. Check standard layout: globalStorage
        let global_db = base.join("globalStorage/state.vscdb");
        if global_db.exists() {
            dbs.push(global_db);
        }

        // 4. Check standard layout: workspaceStorage/{workspace-id}/state.vscdb
        // We only need this specific file pattern, so avoid recursive walking
        // over all workspace files (which can be large and noisy).
        let workspace_storage = if base
            .file_name()
            .is_some_and(|name| name == "workspaceStorage")
        {
            base.to_path_buf()
        } else {
            base.join("workspaceStorage")
        };
        dbs.extend(storage_roots::workspace_db_files(&workspace_storage));

        // Keep connector traversal deterministic across filesystems/runs.
        dbs.sort();
        dbs.dedup();
        dbs
    }

    pub(super) fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots: Vec<ScanRoot> = Vec::new();
        if ctx.use_default_detection() {
            if Self::looks_like_base(&ctx.data_dir) {
                roots.push(ScanRoot::local(ctx.data_dir.clone()));
            } else if let Some(default_base) = Self::app_support_dir() {
                roots.push(ScanRoot::local(default_base));
            }
        } else {
            for scan_root in &ctx.scan_roots {
                // Some(empty) is a recognized but rejected/missing mirror.
                // Do not retry its unsafe shape through local-layout probes.
                let candidates = storage_roots::mirrored_db_files(scan_root).unwrap_or_else(|| {
                    let mut candidates = Vec::new();
                    Self::append_explicit_roots(&mut candidates, &scan_root.path);
                    candidates
                });
                roots.extend(candidates.into_iter().map(|path| scan_root.with_path(path)));
            }
        }

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    pub(super) fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut out = Vec::new();
        let mut seen_files = HashSet::new();
        for root in Self::source_roots(ctx) {
            if path_is_excluded(&root.path, &excluded_paths) {
                continue;
            }
            for db_path in Self::find_db_files(&root.path) {
                if path_is_excluded(&db_path, &excluded_paths) {
                    continue;
                }
                if !seen_files.insert(dedupe_path_key(&db_path)) {
                    continue;
                }
                if !file_modified_since(&db_path, ctx.since_ts) {
                    continue;
                }
                out.push(
                    DiscoveredSourceFile::new(
                        "cursor",
                        &root,
                        db_path,
                        DiscoveredSourceRole::SqliteDatabase,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }
        // Cursor Agent transcripts (gh #306) — mirror scan_agent_transcripts so
        // discovery covers every scanned source (conformance contract).
        Self::discover_agent_sources(ctx, &excluded_paths, &mut out);
        Ok(out)
    }

    /// Fast existence probe for detect(): return true on the first matching
    /// Cursor DB path instead of collecting all DB files.
    #[allow(dead_code)]
    pub(super) fn has_any_db_file(base: &Path) -> bool {
        if base.is_file() && base.file_name().is_some_and(|n| n == "state.vscdb") {
            return true;
        }

        if base.join("state.vscdb").is_file() {
            return true;
        }

        if base.join("globalStorage/state.vscdb").is_file() {
            return true;
        }

        let workspace_storage = base.join("workspaceStorage");
        if !workspace_storage.exists() {
            return false;
        }

        if workspace_storage.join("state.vscdb").is_file() {
            return true;
        }

        if let Ok(entries) = std::fs::read_dir(&workspace_storage) {
            for entry in entries.flatten() {
                if entry.path().join("state.vscdb").is_file() {
                    return true;
                }
            }
        }

        false
    }
}

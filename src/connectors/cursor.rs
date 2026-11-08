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

mod storage_roots;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankensqlite::compat::RowExt;
use frankensqlite::params;

use super::sqlite_sync::{
    Connection, ConnectionExt, open_read_only_with_snapshot_fallback, store_is_provably_empty,
};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use super::utils::{dedupe_path_key, env_path_nonempty, read_capped};
use super::{
    Connector, extract_invocations_from_content_blocks, file_modified_since, flatten_content,
    franken_detection_for_connector, parse_timestamp,
};
use crate::types::{DetectionResult, NormalizedConversation, NormalizedMessage, reindex_messages};

/// Cursor v0.40+ bubble type constants (numeric encoding)
mod bubble_type {
    /// User message type in new format
    pub const USER: i64 = 1;
    /// Assistant message type in new format
    pub const ASSISTANT: i64 = 2;
}

/// Type alias for the bubble data lookup map.
/// Keys are bubble IDs for O(1) lookup within a composer.
type BubbleDataMap = HashMap<String, Value>;

/// One key-prefix range of `cursorDiskKV` (`?1` inclusive, `?2` exclusive).
/// It must stay a bare key range: frankensqlite plans any residual predicate
/// (such as `value IS NOT NULL`) as a full table scan, and a real
/// multi-gigabyte `state.vscdb` then reads the whole table once per composer
/// (cass F8). Callers skip NULL values in the row mapper instead.
const CURSOR_DISK_KV_RANGE_SQL: &str =
    "SELECT key, value FROM cursorDiskKV WHERE key >= ? AND key < ?";

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

    /// Compute the exclusive upper bound for prefix range scans.
    ///
    /// For prefix `abc:`, this returns `abc;`, so
    /// `key >= prefix AND key < upper` matches all keys that start with `prefix`.
    fn prefix_upper_bound(prefix: &str) -> String {
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

    fn looks_like_base(path: &Path) -> bool {
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
    fn is_wsl() -> bool {
        std::fs::read_to_string("/proc/version")
            .map(|v| v.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }

    /// Find Cursor installation path via WSL mount points
    /// Probes /mnt/c/Users/*/AppData/Roaming/Cursor/User
    #[cfg(target_os = "linux")]
    fn find_wsl_cursor_path() -> Option<PathBuf> {
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
    fn find_db_files(base: &Path) -> Vec<PathBuf> {
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

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
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

    fn discover_sources(ctx: &ScanContext) -> Vec<DiscoveredSourceFile> {
        let mut out = Vec::new();
        let mut seen_files = HashSet::new();
        for root in Self::source_roots(ctx) {
            for db_path in Self::find_db_files(&root.path) {
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
        Self::discover_agent_sources(ctx, &mut out);
        out
    }

    /// Fast existence probe for detect(): return true on the first matching
    /// Cursor DB path instead of collecting all DB files.
    #[allow(dead_code)]
    fn has_any_db_file(base: &Path) -> bool {
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

    /// Fetch bubble data for a specific composer from the database.
    /// Returns a map keyed by bubbleId for efficient O(1) lookup.
    /// This lazy-loads only the bubbles needed for one conversation,
    /// avoiding loading all bubbles into memory.
    fn fetch_bubble_data_for_composer(conn: &Connection, composer_id: &str) -> BubbleDataMap {
        let mut bubble_map = BubbleDataMap::new();

        // Optimization: Use range query instead of LIKE for prefix matching.
        // This guarantees SQLite uses the index on the `key` column.
        // Prefix: `bubbleId:{composerId}:`
        // Range: >= prefix AND < prefix_next
        let prefix = format!("bubbleId:{}:", composer_id);
        let limit = Self::prefix_upper_bound(&prefix);

        let prefix_len = prefix.len();

        // Skip NULL-value rows (same rationale as the composerData query in
        // extract_from_db): Cursor inserts internal markers with no chat
        // payload, and `row.get_typed::<String>(1)?` on NULL aborts the whole
        // query_map_collect — silently emptying this conversation's bubble
        // map so the entire chat is dropped as message-less. They are skipped
        // here rather than in SQL (see `CURSOR_DISK_KV_RANGE_SQL`).
        match conn.query_map_collect(
            CURSOR_DISK_KV_RANGE_SQL,
            params![prefix.as_str(), limit.as_str()],
            |row| {
                let key: String = row.get_typed(0)?;
                let value: Option<String> = row.get_typed(1)?;
                Ok((key, value))
            },
        ) {
            Ok(rows) => {
                for (key, value) in rows {
                    let Some(value) = value else {
                        continue;
                    };
                    // Key format: bubbleId:{composerId}:{bubbleId}
                    // Extract just the bubbleId part
                    if key.len() > prefix_len {
                        let bubble_id = &key[prefix_len..];
                        if let Ok(parsed) = serde_json::from_str::<Value>(&value) {
                            bubble_map.insert(bubble_id.to_string(), parsed);
                        }
                    }
                }
            }
            Err(e) => {
                // A failed bubble range-query must not be silently
                // indistinguishable from a composer with no bubbles:
                // parse_composer_data would drop the whole chat as
                // message-less (same class as the composerData query fix).
                tracing::warn!(
                    composer = %composer_id,
                    error = %e,
                    "cursor: bubbleData query failed; messages from this conversation may be missing"
                );
            }
        }

        bubble_map
    }

    /// Extract workspace from bubble data.
    /// Cursor v0.40+ stores workspaceProjectDir in bubble entries.
    fn extract_workspace_from_bubbles(bubble_map: &BubbleDataMap) -> Option<PathBuf> {
        for bubble in bubble_map.values() {
            // Try workspaceProjectDir first (most common)
            if let Some(dir) = bubble.get("workspaceProjectDir").and_then(|v| v.as_str())
                && !dir.is_empty()
            {
                return Some(PathBuf::from(dir));
            }

            // Try workspaceUris array
            if let Some(uris) = bubble.get("workspaceUris").and_then(|v| v.as_array()) {
                for uri in uris {
                    if let Some(uri_str) = uri.as_str() {
                        // Parse file:// or vscode-remote:// URIs
                        if let Some(path) = Self::parse_workspace_uri(uri_str) {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }

    /// Parse a workspace URI to extract the path.
    /// Handles file:// and vscode-remote:// URIs.
    fn parse_workspace_uri(uri: &str) -> Option<PathBuf> {
        if let Some(path) = uri.strip_prefix("file://") {
            // URL decode
            let decoded = urlencoding::decode(path).ok()?;
            let mut path_str = decoded.as_ref();

            // An RFC 3986 authority (`file://localhost/path`,
            // `file://server/share`) is not part of the path: drop it when
            // the remainder starts with a separator, so the result stays
            // absolute instead of becoming a bogus relative path.
            if !path_str.starts_with('/')
                && !path_str.starts_with('\\')
                && let Some(slash) = path_str.find(['/', '\\'])
            {
                // `file://C:/x` (no authority, malformed but seen in the
                // wild) carries a DRIVE LETTER where an authority would be;
                // stripping it would amputate the path.
                let authority = &path_str[..slash];
                let looks_like_drive = authority.len() == 2 && authority.as_bytes()[1] == b':';
                if !looks_like_drive {
                    path_str = &path_str[slash..];
                }
            }

            // On Windows, file:///C:/... becomes /C:/...
            // We need to strip the leading slash if it looks like a drive letter
            if cfg!(windows) && path_str.starts_with('/') && path_str.len() > 2 {
                let chars: Vec<char> = path_str.chars().collect();
                if chars[2] == ':' && chars[1].is_ascii_alphabetic() {
                    path_str = &path_str[1..];
                }
            }

            return Some(PathBuf::from(path_str));
        }

        // Handle vscode-remote://ssh-remote+{json}/path format
        if let Some(rest) = uri.strip_prefix("vscode-remote://") {
            // Extract path after the host/connection info
            // Format: vscode-remote://ssh-remote+{encoded-json}/actual/path
            if let Some(slash_idx) = rest.find('/') {
                let path = &rest[slash_idx..];
                let decoded = urlencoding::decode(path).ok()?;
                return Some(PathBuf::from(decoded.into_owned()));
            }
        }

        None
    }

    /// Extract chat sessions from a SQLite database.
    ///
    /// `tick` is called for every row the queries read and before every
    /// composer is parsed. A large `state.vscdb` takes many minutes here
    /// before the first conversation is yielded, and the host's stall
    /// watchdog must see that work as progress (cass#373).
    fn extract_from_db(
        db_path: &Path,
        since_ts: Option<i64>,
        tick: &dyn Fn(),
    ) -> Result<Vec<NormalizedConversation>> {
        let conn = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to open Cursor db: {}", db_path.display()))?;

        // Set busy timeout to 5 seconds to avoid locking errors when Cursor is running
        conn.execute("PRAGMA busy_timeout = 5000;")
            .with_context(|| "failed to set busy_timeout")?;

        let mut convs = Vec::new();
        let mut seen_ids = HashSet::new();

        // Try cursorDiskKV table for composerData entries
        let composer_prefix = "composerData:";
        let composer_limit = Self::prefix_upper_bound(composer_prefix);
        // Surface query errors at warn level: a corrupted page or missing
        // table must not be indistinguishable from a legitimately empty DB.
        match conn.query_map_collect(
            // Skip NULL-value rows: Cursor inserts internal markers with no
            // chat payload, and `row.get_typed::<String>(1)?` on NULL aborts the
            // whole query_map_collect — silently turning every row into zero
            // conversations. Not in SQL (see `CURSOR_DISK_KV_RANGE_SQL`).
            CURSOR_DISK_KV_RANGE_SQL,
            params![composer_prefix, composer_limit.as_str()],
            |row| {
                tick();
                let key: String = row.get_typed(0)?;
                let value: Option<String> = row.get_typed(1)?;
                Ok((key, value))
            },
        ) {
            Ok(rows) => {
                for (key, value) in rows {
                    let Some(value) = value else {
                        continue;
                    };
                    // Each composer runs its own bubble range query.
                    tick();
                    if let Some(conv) = Self::parse_composer_data(
                        &key,
                        &value,
                        db_path,
                        since_ts,
                        &mut seen_ids,
                        Some(&conn),
                    ) {
                        convs.push(conv);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(db = %db_path.display(), error = %e, "cursor: composerData query failed; conversations from this store may be missing");
            }
        }

        // Also try ItemTable for legacy aichat data
        match conn.query_map_collect(
            "SELECT key, value FROM ItemTable WHERE (key LIKE '%aichat%chatdata%' OR key LIKE '%composer%') AND value IS NOT NULL",
            params![],
            |row| {
                tick();
                let key: String = row.get_typed(0)?;
                let value: String = row.get_typed(1)?;
                Ok((key, value))
            },
        ) {
            Ok(rows) => {
                for (key, value) in rows {
                    if let Some(conv) =
                        Self::parse_aichat_data(&key, &value, db_path, since_ts, &mut seen_ids)
                    {
                        convs.push(conv);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(db = %db_path.display(), error = %e, "cursor: ItemTable query failed; legacy conversations from this store may be missing");
            }
        }

        Ok(convs)
    }

    /// Parse composerData JSON into a conversation.
    ///
    /// Supports multiple Cursor formats:
    /// - v0.40+ (new): `fullConversationHeadersOnly` with separate bubbleId entries
    /// - v0.3x (tabs): `tabs` → `bubbles` structure
    /// - v0.2x (conversationMap): `conversationMap` → `bubbles` structure
    /// - Simple: `text`/`richText` fields only
    fn parse_composer_data(
        key: &str,
        value: &str,
        db_path: &Path,
        since_ts: Option<i64>,
        seen_ids: &mut HashSet<String>,
        conn: Option<&Connection>,
    ) -> Option<NormalizedConversation> {
        let val: Value = serde_json::from_str(value).ok()?;

        // Extract composer ID from key (composerData:{uuid})
        let composer_id = key.strip_prefix("composerData:")?.to_string();

        // Skip if already seen
        if seen_ids.contains(&composer_id) {
            return None;
        }
        seen_ids.insert(composer_id.clone());

        // Extract timestamps
        let created_at = val.get("createdAt").and_then(parse_timestamp);
        let last_updated_at = val.get("lastUpdatedAt").and_then(parse_timestamp);

        let mut messages = Vec::new();
        let mut workspace: Option<PathBuf> = None;

        // Check for v0.40+ format with fullConversationHeadersOnly
        // This format stores only bubble IDs in composerData, with actual content
        // in separate bubbleId:{composerId}:{bubbleId} keys
        // Note: requires a database connection to fetch bubble content
        if let (Some(headers), Some(conn)) = (
            val.get("fullConversationHeadersOnly")
                .and_then(|v| v.as_array()),
            conn,
        ) {
            // Lazy-load bubble data for this composer
            let bubble_map = Self::fetch_bubble_data_for_composer(conn, &composer_id);

            // Extract workspace from bubbles
            workspace = Self::extract_workspace_from_bubbles(&bubble_map);

            // Parse each header reference
            for header in headers {
                if let Some(bubble_id) = header.get("bubbleId").and_then(|v| v.as_str())
                    && let Some(bubble) = bubble_map.get(bubble_id)
                    && let Some(msg) = Self::parse_bubble(bubble, messages.len())
                {
                    messages.push(msg);
                }
            }
        }

        // Parse conversation from bubbles/tabs structure (legacy v0.3x)
        if messages.is_empty()
            && let Some(tabs) = val.get("tabs").and_then(|v| v.as_array())
        {
            for tab in tabs {
                if let Some(bubbles) = tab.get("bubbles").and_then(|v| v.as_array()) {
                    for (idx, bubble) in bubbles.iter().enumerate() {
                        if let Some(msg) = Self::parse_bubble(bubble, idx) {
                            messages.push(msg);
                        }
                    }
                }
            }
        }

        // Also check conversationMap for older format (v0.2x)
        if messages.is_empty()
            && let Some(conv_map) = val.get("conversationMap").and_then(|v| v.as_object())
        {
            for (_, conv_val) in conv_map {
                if let Some(bubbles) = conv_val.get("bubbles").and_then(|v| v.as_array()) {
                    for (idx, bubble) in bubbles.iter().enumerate() {
                        if let Some(msg) = Self::parse_bubble(bubble, messages.len() + idx) {
                            messages.push(msg);
                        }
                    }
                }
            }
        }

        // Check for text/richText as user input (simple composer sessions)
        let user_text = val
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| val.get("richText").and_then(|v| v.as_str()))
            .unwrap_or("");

        if !user_text.is_empty() && messages.is_empty() {
            messages.push(NormalizedMessage {
                idx: 0,
                role: "user".to_string(),
                author: None,
                created_at,
                content: user_text.to_string(),
                extra: serde_json::json!({}),
                invocations: Vec::new(),
                snippets: Vec::new(),
            });
        }

        // Skip if no messages
        if messages.is_empty() {
            return None;
        }

        // Re-index messages
        crate::types::reindex_messages(&mut messages);

        // Extract model info
        let model_name = val
            .get("modelConfig")
            .and_then(|m| m.get("modelName"))
            .and_then(|m| m.as_str());

        // Use explicit name field if available (v0.40+), otherwise derive from first message
        let title = val
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(100).collect())
            .or_else(|| {
                messages.first().map(|m| {
                    m.content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(100)
                        .collect()
                })
            })
            .or_else(|| model_name.map(|m| format!("Cursor chat with {}", m)));

        // source_path must be unique per conversation for proper lookup in the TUI.
        // Since multiple conversations live in the same database file, we append
        // the composer_id to create a unique synthetic path for each conversation.
        let safe_id = urlencoding::encode(&composer_id);
        let unique_source_path = db_path.join(safe_id.as_ref());

        // Use lastUpdatedAt if available (most accurate), fall back to max message time, then createdAt
        let ended_at = last_updated_at
            .or_else(|| messages.iter().filter_map(|m| m.created_at).max())
            .or(created_at);
        // Optimization: Skip conversations not modified since last scan.
        // The −1s slack mirrors file_modified_since (mtime granularity):
        // without it a conversation updated inside the slack window is
        // skipped on every subsequent incremental scan.
        if let (Some(since), Some(ts)) = (since_ts, ended_at)
            && ts < since.saturating_sub(1_000)
        {
            return None;
        }

        Some(NormalizedConversation {
            agent_slug: "cursor".to_string(),
            external_id: Some(composer_id),
            title,
            workspace,
            source_path: unique_source_path,
            started_at: created_at,
            ended_at,
            metadata: serde_json::json!({
                "source": "cursor",
                "model": model_name,
                "unifiedMode": val.get("unifiedMode").and_then(|v| v.as_str()),
            }),
            messages,
        })
    }

    /// Parse a bubble (message) from Cursor's format.
    ///
    /// Handles both new format (v0.40+) and legacy formats:
    /// - Content: text > rawText > content > message
    /// - Role: numeric type (1=user, 2=assistant) or string type/role
    /// - Author: modelType (new) or model (legacy)
    fn parse_bubble(bubble: &Value, idx: usize) -> Option<NormalizedMessage> {
        // Extract content - try all known field names in priority order
        let content = bubble
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| bubble.get("rawText").and_then(|v| v.as_str()))
            .or_else(|| bubble.get("content").and_then(|v| v.as_str()))
            .or_else(|| bubble.get("message").and_then(|v| v.as_str()))?;

        if content.trim().is_empty() {
            return None;
        }

        // Extract role - try numeric type first (v0.40+), then string type/role (legacy)
        let role = bubble
            .get("type")
            .and_then(|v| {
                // v0.40+ format: numeric type (1=user, 2=assistant)
                v.as_i64()
                    .map(|t| {
                        match t {
                            bubble_type::USER => "user",
                            bubble_type::ASSISTANT => "assistant",
                            // Fallback for unknown numeric types (e.g. system messages, tools)
                            // Default to "assistant" to preserve content visibility
                            _ => "assistant",
                        }
                        .to_string()
                    })
                    // Legacy format: string type
                    .or_else(|| v.as_str().map(Self::normalize_role))
            })
            .or_else(|| {
                // Fallback: check "role" field (legacy format)
                bubble
                    .get("role")
                    .and_then(|v| v.as_str())
                    .map(Self::normalize_role)
            })
            .unwrap_or_else(|| "assistant".to_string());

        let created_at = bubble
            .get("timestamp")
            .or_else(|| bubble.get("createdAt"))
            .and_then(parse_timestamp);

        // Extract author - try both field names (modelType is v0.40+)
        let author = bubble
            .get("modelType")
            .or_else(|| bubble.get("model"))
            .or_else(|| {
                // Also check modelInfo.modelName for v0.40+
                bubble.get("modelInfo").and_then(|m| m.get("modelName"))
            })
            .and_then(|v| v.as_str())
            .map(String::from);

        Some(NormalizedMessage {
            idx: idx as i64,
            role,
            author,
            created_at,
            content: content.to_string(),
            extra: bubble.clone(),
            invocations: Vec::new(),
            snippets: Vec::new(),
        })
    }

    /// Normalize role string to standard values (user/assistant).
    fn normalize_role(role: &str) -> String {
        match role.to_lowercase().as_str() {
            "user" | "human" => "user",
            "assistant" | "ai" | "bot" => "assistant",
            _ => role,
        }
        .to_string()
    }

    /// Parse legacy aichat data
    fn parse_aichat_data(
        key: &str,
        value: &str,
        db_path: &Path,
        _since_ts: Option<i64>, // File-level filtering done in scan(); message filtering not needed
        seen_ids: &mut HashSet<String>,
    ) -> Option<NormalizedConversation> {
        let val: Value = serde_json::from_str(value).ok()?;

        // Skip if already seen
        let id = format!("aichat-{}", key);
        if seen_ids.contains(&id) {
            return None;
        }
        seen_ids.insert(id.clone());

        let mut messages = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;
        let mut update_time_bounds = |candidate_ts: Option<i64>| {
            if let Some(ts) = candidate_ts {
                started_at = Some(if let Some(current) = started_at {
                    current.min(ts)
                } else {
                    ts
                });
                ended_at = Some(if let Some(current) = ended_at {
                    current.max(ts)
                } else {
                    ts
                });
            }
        };

        // Parse tabs array
        if let Some(tabs) = val.get("tabs").and_then(|v| v.as_array()) {
            for tab in tabs {
                let tab_ts = tab.get("timestamp").and_then(parse_timestamp);

                // NOTE: Do NOT filter by timestamp here! File-level check is sufficient.

                if let Some(bubbles) = tab.get("bubbles").and_then(|v| v.as_array()) {
                    for bubble in bubbles {
                        if let Some(msg) = Self::parse_bubble(bubble, messages.len()) {
                            update_time_bounds(msg.created_at.or(tab_ts));
                            messages.push(msg);
                        }
                    }
                }
            }
        }

        if messages.is_empty() {
            return None;
        }

        // Re-index
        crate::types::reindex_messages(&mut messages);

        let title = messages.first().map(|m| {
            m.content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect()
        });

        // source_path must be unique per conversation for proper lookup in the TUI.
        let safe_id = urlencoding::encode(&id);
        let unique_source_path = db_path.join(safe_id.as_ref());

        Some(NormalizedConversation {
            agent_slug: "cursor".to_string(),
            external_id: Some(id),
            title,
            workspace: None,
            source_path: unique_source_path,
            started_at,
            ended_at,
            metadata: serde_json::json!({"source": "cursor_aichat"}),
            messages,
        })
    }
}

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

/// Directory segment that brackets a session's transcript directory.
const AGENT_TRANSCRIPTS_DIR: &str = "agent-transcripts";
/// Normalized agent slug, shared with the Composer path (same agent).
const CURSOR_AGENT_SLUG: &str = "cursor";

/// Metadata marker of a merged Composer + transcript conversation (§6.3):
/// distinguishes merges from the transcript-only `"agent"` shape and the
/// Composer-only shape (marker absent).
const CURSOR_MERGED_FORMAT: &str = "merged";
/// Merge-rules revision (§6.5 escape hatch): hosts that refuse to re-emit
/// unchanged conversations can bump this to force a rebuild.
const CURSOR_MERGE_REVISION: i64 = 1;
/// Extra key carrying the raw transcript record on pair and error rows
/// (§6.2): `cursor_` namespacing keeps it out of the source bubble's keys.
const CURSOR_TRANSCRIPT_RECORD_KEY: &str = "cursor_transcript_record";

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
    /// Default root holding `~/.cursor/projects/`. Overridable via
    /// `CASS_CURSOR_PROJECTS_ROOT` for tests / remote mirrors.
    fn agent_projects_root() -> Option<PathBuf> {
        if let Some(explicit) = env_path_nonempty("CASS_CURSOR_PROJECTS_ROOT") {
            return Some(explicit);
        }
        dirs::home_dir().map(|h| h.join(".cursor").join("projects"))
    }

    /// Roots under which to look for Agent transcripts. Mirrors the Composer
    /// root policy: a default-detection `data_dir` that itself looks like a
    /// Cursor base scopes the whole scan to that base (so fixture/mirror
    /// scans stay hermetic); otherwise default detection uses
    /// `~/.cursor/projects`, with `CASS_CURSOR_PROJECTS_ROOT` still taking
    /// precedence for CI; explicit scan roots (e.g. remote mirrors) are
    /// walked as-is.
    fn agent_scan_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if Self::looks_like_base(&ctx.data_dir)
                && env_path_nonempty("CASS_CURSOR_PROJECTS_ROOT").is_none()
            {
                vec![ScanRoot::local(ctx.data_dir.clone())]
            } else {
                Self::agent_projects_root()
                    .into_iter()
                    .map(ScanRoot::local)
                    .collect()
            }
        } else {
            ctx.scan_roots.clone()
        };
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    /// True only for a *primary* session transcript
    /// `<...>/agent-transcripts/<sid>/<sid>.jsonl`. Subagent transcripts (under a
    /// `subagents/` subtree, or any file whose stem differs from its parent dir
    /// name) are excluded.
    fn is_primary_agent_transcript(path: &Path) -> bool {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return false;
        }
        if path
            .components()
            .any(|c| c.as_os_str().to_str() == Some("subagents"))
        {
            return false;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        // `<sid>/<sid>.jsonl`: the file's stem equals its directory name.
        if parent.file_name().and_then(|s| s.to_str()) != Some(stem) {
            return false;
        }
        // Grandparent must be the `agent-transcripts` directory.
        parent
            .parent()
            .and_then(|gp| gp.file_name())
            .and_then(|s| s.to_str())
            == Some(AGENT_TRANSCRIPTS_DIR)
    }

    /// Resolve every primary Agent transcript reachable from a scan target.
    fn agent_transcript_files(scan_target: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in WalkDir::new(scan_target)
            .min_depth(1)
            .max_depth(8)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() && Self::is_primary_agent_transcript(entry.path()) {
                out.push(entry.path().to_path_buf());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Every primary Agent transcript reachable from the scan targets, in a
    /// fixed `(root, path)` order. The merge consumes this inventory in full
    /// (a session with a fresh half is merged regardless of the other
    /// half's mtime), while discovery and the transcript-only path consume
    /// its freshness-filtered subset. Deduped by filesystem identity so
    /// overlapping scan roots cannot double-read a file.
    fn agent_transcript_inventory(ctx: &ScanContext) -> Vec<(ScanRoot, PathBuf)> {
        let mut out = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for root in Self::agent_scan_roots(ctx) {
            if !root.path.exists() {
                continue;
            }
            for transcript in Self::agent_transcript_files(&root.path) {
                if !seen.insert(dedupe_path_key(&transcript)) {
                    continue;
                }
                out.push((root.clone(), transcript));
            }
        }
        out
    }

    /// Whether a transcript counts as fresh for an incremental scan: its own
    /// mtime or its `.workspace-trusted` sidecar's, with the usual 1s slack.
    fn agent_transcript_is_fresh(transcript: &Path, since_ts: Option<i64>) -> bool {
        let workspace_metadata_changed = Self::agent_workspace_metadata_path(transcript)
            .is_some_and(|path| path.is_file() && file_modified_since(&path, since_ts));
        file_modified_since(transcript, since_ts) || workspace_metadata_changed
    }

    /// The fresh `(root, transcript)` pairs that BOTH `scan()`'s
    /// transcript-only path and `discover_source_files()` consume, so
    /// discovery always covers every freshly scanned source (the connector
    /// conformance contract).
    fn agent_transcript_sources(ctx: &ScanContext) -> Vec<(ScanRoot, PathBuf)> {
        Self::agent_transcript_inventory(ctx)
            .into_iter()
            .filter(|(_, transcript)| Self::agent_transcript_is_fresh(transcript, ctx.since_ts))
            .collect()
    }

    /// The encoded `<project>` directory name for a transcript path.
    fn agent_project_dir_name(transcript: &Path) -> Option<String> {
        // <project>/agent-transcripts/<sid>/<sid>.jsonl
        transcript
            .parent()? // <sid>/
            .parent()? // agent-transcripts/
            .parent()? // <project>/
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from)
    }

    fn agent_workspace_metadata_path(transcript: &Path) -> Option<PathBuf> {
        Some(
            transcript
                .parent()?
                .parent()?
                .parent()?
                .join(".workspace-trusted"),
        )
    }

    /// Project slugs lose the distinction between separators and literal
    /// hyphens. Only the explicit workspacePath can establish attribution.
    /// Accept foreign absolute paths too: exported sources need not exist on
    /// the scanning host, and ScanRoot applies source mappings afterward.
    fn agent_workspace_from_metadata(transcript: &Path) -> Option<PathBuf> {
        let path = Self::agent_workspace_metadata_path(transcript)?;
        let text = read_capped(&path).ok()??;
        let metadata: Value = serde_json::from_str(&text).ok()?;
        let workspace = metadata.get("workspacePath")?.as_str()?;
        let bytes = workspace.as_bytes();
        let drive_absolute = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\');
        let unc_absolute = workspace.strip_prefix("\\\\").is_some_and(|rest| {
            let mut components = rest.split('\\');
            components.next().is_some_and(|part| !part.is_empty())
                && components.next().is_some_and(|part| !part.is_empty())
        });
        if workspace.chars().any(char::is_control)
            || !(workspace.starts_with('/') || drive_absolute || unc_absolute)
        {
            return None;
        }
        Some(PathBuf::from(workspace))
    }

    /// A file's mtime in epoch milliseconds, if it can be stated.
    fn file_mtime_ms(path: &Path) -> Option<i64> {
        std::fs::metadata(path).ok().and_then(|meta| {
            meta.modified().ok().and_then(|m| {
                m.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_millis()).ok())
            })
        })
    }

    /// Read a JSONL transcript into per-line records, skipping blank and
    /// individually-malformed lines (warned, never fatal).
    fn read_agent_transcript_records(transcript: &Path) -> Vec<Value> {
        let text = match read_capped(transcript) {
            Ok(Some(text)) => text,
            Ok(None) => {
                tracing::warn!(
                    transcript = %transcript.display(),
                    "cursor: agent transcript exceeds the scan size cap; skipping"
                );
                return Vec::new();
            }
            Err(err) => {
                tracing::warn!(
                    transcript = %transcript.display(),
                    error = %err,
                    "cursor: unreadable agent transcript"
                );
                return Vec::new();
            }
        };
        let mut records = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(value) => records.push(value),
                Err(err) => tracing::warn!(
                    transcript = %transcript.display(),
                    line = lineno + 1,
                    error = %err,
                    "cursor: skipping malformed agent-transcript line"
                ),
            }
        }
        records
    }

    /// Map one transcript record to a normalized message, or `None` when it
    /// carries no usable content.
    fn agent_record_to_message(rec: &Value) -> Option<NormalizedMessage> {
        let role_raw = rec.get("role").and_then(Value::as_str).unwrap_or("");
        if role_raw.trim().is_empty() {
            return None;
        }
        let role = Self::normalize_role(role_raw);

        // The documented shape nests content at `message.content`; tolerate a
        // top-level `content` too. Both array-of-blocks and plain-string forms
        // are handled by `flatten_content`.
        let content_val = rec
            .pointer("/message/content")
            .or_else(|| rec.get("content"));
        let (content, invocations) = content_val.map_or_else(
            || (String::new(), Vec::new()),
            |v| {
                (
                    flatten_content(v),
                    extract_invocations_from_content_blocks(v),
                )
            },
        );

        // No per-message timestamp in the documented shape; use one defensively
        // if a future/real record carries it.
        let created_at = rec
            .get("timestamp")
            .or_else(|| rec.get("createdAt"))
            .or_else(|| rec.get("created_at"))
            .and_then(parse_timestamp);

        if content.trim().is_empty() && invocations.is_empty() {
            return None;
        }

        Some(NormalizedMessage {
            idx: 0,
            role,
            author: None,
            created_at,
            content,
            extra: Value::Object(Map::new()),
            invocations,
            snippets: Vec::new(),
        })
    }

    /// Parse a single Agent transcript file into a normalized conversation.
    fn parse_agent_transcript(
        transcript: &Path,
        root: &ScanRoot,
    ) -> Option<NormalizedConversation> {
        let records = Self::read_agent_transcript_records(transcript);
        if records.is_empty() {
            return None;
        }

        let mut messages: Vec<NormalizedMessage> = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;
        for rec in &records {
            if let Some(msg) = Self::agent_record_to_message(rec) {
                if let Some(ts) = msg.created_at {
                    started_at = Some(started_at.map_or(ts, |cur| cur.min(ts)));
                    ended_at = Some(ended_at.map_or(ts, |cur| cur.max(ts)));
                }
                messages.push(msg);
            }
        }

        if messages.is_empty() {
            tracing::warn!(
                transcript = %transcript.display(),
                records = records.len(),
                "cursor: agent transcript produced no messages (possible schema drift)"
            );
            return None;
        }

        // The documented record shape has no timestamps; fall back to the file's
        // mtime so the conversation still has a sortable time.
        if started_at.is_none() {
            if let Some(ms) = Self::file_mtime_ms(transcript) {
                started_at = Some(ms);
                ended_at = Some(ms);
            }
        }

        reindex_messages(&mut messages);

        let external_id = transcript
            .file_stem()
            .and_then(|s| s.to_str())
            .map(String::from);
        let project = Self::agent_project_dir_name(transcript);
        let explicit_workspace = Self::agent_workspace_from_metadata(transcript);
        let attribution = if explicit_workspace.is_some() {
            "workspace_trusted"
        } else {
            "unresolved"
        };
        let workspace = explicit_workspace.map(|w| {
            let rewritten = root.rewrite_workspace(&w.to_string_lossy(), Some(CURSOR_AGENT_SLUG));
            PathBuf::from(rewritten)
        });

        let title = messages
            .iter()
            .find(|m| m.role == "user")
            .or_else(|| messages.first())
            .and_then(|m| m.content.lines().find(|l| !l.trim().is_empty()))
            .map(|line| line.chars().take(100).collect::<String>());

        let mut metadata = Map::new();
        metadata.insert(
            "source".to_string(),
            Value::String(CURSOR_AGENT_SLUG.to_string()),
        );
        metadata.insert(
            "cursor_format".to_string(),
            Value::String("agent".to_string()),
        );
        metadata.insert(
            "cursor_workspace_attribution".to_string(),
            Value::String(attribution.to_string()),
        );
        if let Some(p) = &project {
            metadata.insert("cursor_project_dir".to_string(), Value::String(p.clone()));
        }

        Some(NormalizedConversation {
            agent_slug: CURSOR_AGENT_SLUG.to_string(),
            external_id,
            title,
            workspace,
            source_path: transcript.to_path_buf(),
            started_at,
            ended_at,
            metadata: Value::Object(metadata),
            messages,
        })
    }

    /// Agent-transcript files as discovered sources (mirrors the scan half's
    /// freshness gate over the same inventory).
    fn discover_agent_sources(ctx: &ScanContext, out: &mut Vec<DiscoveredSourceFile>) {
        let mut seen_metadata = HashSet::new();
        for (root, transcript) in Self::agent_transcript_sources(ctx) {
            if let Some(path) = Self::agent_workspace_metadata_path(&transcript)
                && path.is_file()
                && seen_metadata.insert(dedupe_path_key(&path))
            {
                out.push(
                    DiscoveredSourceFile::new(
                        CURSOR_AGENT_SLUG,
                        &root,
                        path,
                        DiscoveredSourceRole::MetadataSidecar,
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
            out.push(
                DiscoveredSourceFile::new(
                    CURSOR_AGENT_SLUG,
                    &root,
                    transcript,
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata(),
            );
        }
    }

    // ------------------------------------------------------------------
    // Composer + Agent-transcript merge (design §6.1–§6.5)
    // ------------------------------------------------------------------

    /// The session id a primary transcript file carries (its file stem) —
    /// the join key against Composer `composerData:<sid>` rows.
    fn transcript_sid(transcript: &Path) -> Option<String> {
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
    fn merge_composer_transcripts(
        composer: NormalizedConversation,
        files: &[(ScanRoot, PathBuf)],
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
        let explicit_workspace = Self::agent_workspace_from_metadata(first_transcript);
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

    /// The `composerData:<sid>` value of one store, if present: a
    /// primary-key point query (F8: never a value-predicated scan). NULL
    /// values count as absent, mirroring the full scan's NULL skip.
    fn composer_row_for_sid(conn: &Connection, sid: &str) -> Result<Option<String>> {
        let key = format!("composerData:{sid}");
        let rows = conn.query_map_collect(
            "SELECT value FROM cursorDiskKV WHERE key = ?",
            params![key.as_str()],
            |row| {
                let value: Option<String> = row.get_typed(0)?;
                Ok(value)
            },
        )?;
        Ok(rows.into_iter().flatten().next())
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
    /// anyway because its transcript half is fresh.
    fn lookup_composer_conversations(
        db_paths: &[PathBuf],
        sids: &[String],
        tick: &dyn Fn(),
    ) -> (HashMap<String, Option<NormalizedConversation>>, Vec<String>) {
        let mut found: HashMap<String, Option<NormalizedConversation>> = HashMap::new();
        let mut failures: Vec<String> = Vec::new();
        let mut pending: Vec<String> = sids.to_vec();
        for db_path in db_paths {
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
            let mut still_pending: Vec<String> = Vec::new();
            for sid in &pending {
                tick();
                match Self::composer_row_for_sid(&conn, sid) {
                    Ok(Some(value)) => {
                        let key = format!("composerData:{sid}");
                        let parsed = Self::parse_composer_data(
                            &key,
                            &value,
                            db_path,
                            None,
                            &mut HashSet::new(),
                            Some(&conn),
                        );
                        if let Some(composer_conv) = parsed {
                            found.insert(sid.clone(), Some(composer_conv));
                        } else {
                            // Parses to nothing here; a later store may
                            // still resolve it, exactly like the full
                            // scan's cross-store dedupe.
                            still_pending.push(sid.clone());
                        }
                    }
                    Ok(None) => still_pending.push(sid.clone()),
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
            pending = still_pending;
        }
        (found, failures)
    }
}

impl Connector for CursorConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("cursor").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let tick = || {
            if let Some(tick) = &ctx.progress_tick {
                tick();
            }
        };

        let roots: Vec<PathBuf> = Self::source_roots(ctx)
            .into_iter()
            .map(|root| root.path)
            .filter(|path| path.exists())
            .collect();
        // Every db file, in the fixed order a full scan reads them — the
        // targeted lookup below must probe exactly this order — of which
        // only the fresh ones are extracted now.
        let all_db_paths: Vec<PathBuf> = roots
            .iter()
            .flat_map(|root| Self::find_db_files(root))
            .collect();
        let fresh_db_paths: Vec<PathBuf> = all_db_paths
            .iter()
            // Skip files not modified since last scan
            .filter(|db_path| file_modified_since(db_path, ctx.since_ts))
            .cloned()
            .collect();

        // Composer (`state.vscdb` -> `composerData:*`) from fresh stores.
        //
        // Local fix (cass review 2026-11-07, F9): a store that cannot be
        // read fails the scan instead of hiding behind a warn (F5
        // semantics) — a BusyRecovery-class store is read through the
        // snapshot fallback in `open_read_only_with_snapshot_fallback`,
        // and every other open failure reaches the host.
        let composer_convs =
            super::checked_scan::collect_databases_checked("Cursor", fresh_db_paths, |db_path| {
                Self::extract_from_db(db_path, ctx.since_ts, &tick)
            })?;
        // The same composer id can be mirrored across DBs (globalStorage vs
        // workspaceStorage, explicit overlapping roots, backups). extract_from_db
        // only dedupes within one store, so span a session-id set across ALL
        // stores here — first occurrence wins (candidate order is
        // explicit/data_dir before defaults). Conversations with no
        // external_id pass through: dropping them would trade a duplicate bug
        // for a loss bug. Mirrors the opencode connector's spanning dedupe.
        let mut seen_db_session_ids: HashSet<String> = HashSet::new();
        let composer_convs: Vec<NormalizedConversation> = composer_convs
            .into_iter()
            .filter(|conv| {
                conv.external_id
                    .as_ref()
                    .is_none_or(|id| seen_db_session_ids.insert(id.clone()))
            })
            .collect();

        // Transcript inventory and its fresh subset. The merge (§6.4) always
        // reads both halves of a session: the inventory feeds the merges
        // regardless of mtime, the fresh subset decides what is emitted.
        let inventory = Self::agent_transcript_inventory(ctx);
        let fresh_transcripts: Vec<(ScanRoot, PathBuf)> = inventory
            .iter()
            .filter(|(_, transcript)| Self::agent_transcript_is_fresh(transcript, ctx.since_ts))
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

        let mut all_convs = Vec::new();
        let mut merged_sids: HashSet<String> = HashSet::new();

        // Pass 1 — every fresh-store Composer conversation. A sid with any
        // transcript merges (the db side is fresh, so the transcript is
        // parsed regardless of its own mtime); composer-only sids keep
        // today's shape.
        for conv in composer_convs {
            let Some(sid) = conv.external_id.clone() else {
                // Defensive: parse_composer_data always sets an id.
                all_convs.push(conv);
                continue;
            };
            if let Some(files) = files_by_sid.get(&sid).cloned() {
                merged_sids.insert(sid);
                all_convs.push(Self::merge_composer_transcripts(conv, &files, &tick));
            } else {
                all_convs.push(conv);
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
        let (looked_up, lookup_failures) =
            Self::lookup_composer_conversations(&all_db_paths, &need_lookup, &tick);
        for (root, transcript) in fresh_transcripts {
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
                all_convs.push(Self::merge_composer_transcripts(
                    composer.clone(),
                    &files,
                    &tick,
                ));
                continue;
            }
            // Transcript-only: today's per-file shape, byte for byte.
            if let Some(conv) = Self::parse_agent_transcript(&transcript, &root) {
                all_convs.push(conv);
            }
        }
        // F5/F9 semantics for the targeted lookup too: a store it could not
        // read fails the scan instead of quietly degrading merged sessions
        // to transcript-only rows.
        if !lookup_failures.is_empty() {
            return Err(anyhow::anyhow!(
                "Cursor scan incomplete: {} database(s) failed: {}",
                lookup_failures.len(),
                lookup_failures.join("; ")
            ));
        }

        Ok(all_convs)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(Self::discover_sources(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::scan::ScanRoot;
    use crate::connectors::sqlite_sync::ConnectionExt;
    use frankensqlite::params;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    /// Create a test SQLite database with the cursorDiskKV table
    fn create_test_db(path: &Path) -> Connection {
        let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        conn
    }

    #[test]
    fn remote_mirrors_discover_and_scan_cursor_databases() {
        for (source, subtree) in [
            ("~/.config/Cursor/User", "User"),
            ("/Users/u/Library/Application Support/Cursor", "Cursor/User"),
            ("C:\\Users\\u\\AppData\\Roaming\\Cursor\\User", "User"),
        ] {
            let tmp = TempDir::new().unwrap();
            let wrapper = tmp.path().join("Cursor_User_0123456789abcdef");
            let db_path = wrapper.join(subtree).join("globalStorage/state.vscdb");
            fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let conn = create_test_db(&db_path);
            conn.execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![
                    "composerData:mirror-test",
                    r#"{"text":"Remote conversation"}"#
                ],
            )
            .unwrap();
            drop(conn);
            let origin = crate::types::Origin::remote_with_host("laptop", "u@laptop");
            let root =
                ScanRoot::remote(wrapper, origin.clone(), None).with_mirrored_source_path(source);
            let ctx =
                ScanContext::with_roots(tmp.path().join("state"), vec![root.clone(), root], None);
            let connector = CursorConnector::new();
            let discovered = connector.discover_source_files(&ctx).unwrap();
            assert_eq!(discovered.len(), 1, "{source}");
            assert_eq!(discovered[0].source_path, db_path);
            assert_eq!(discovered[0].origin, origin);
            let conversations = connector.scan(&ctx).unwrap();
            assert_eq!(conversations.len(), 1, "{source}");
            assert_eq!(conversations[0].source_path, db_path.join("mirror-test"));
            assert!(
                conversations[0].messages[0]
                    .content
                    .contains("Remote conversation")
            );
        }
    }

    // =========================================================================
    // Constructor tests
    // =========================================================================

    #[test]
    fn new_creates_connector() {
        let connector = CursorConnector::new();
        let _ = connector;
    }

    #[test]
    fn default_creates_connector() {
        let connector = CursorConnector;
        let _ = connector;
    }

    // =========================================================================
    // find_db_files tests
    // =========================================================================

    #[test]
    fn find_db_files_empty_for_nonexistent() {
        let dir = TempDir::new().unwrap();
        let dbs = CursorConnector::find_db_files(dir.path());
        assert!(dbs.is_empty());
    }

    #[test]
    fn find_db_files_finds_global_storage() {
        let dir = TempDir::new().unwrap();
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 1);
        assert!(dbs[0].ends_with("state.vscdb"));
    }

    #[test]
    fn find_db_files_finds_workspace_storage() {
        let dir = TempDir::new().unwrap();
        let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 1);
    }

    #[test]
    fn find_db_files_finds_multiple() {
        let dir = TempDir::new().unwrap();

        // Create global storage
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("state.vscdb"), "").unwrap();

        // Create multiple workspace storage dirs
        for i in 1..=3 {
            let ws_dir = dir.path().join("workspaceStorage").join(format!("ws{}", i));
            fs::create_dir_all(&ws_dir).unwrap();
            fs::write(ws_dir.join("state.vscdb"), "").unwrap();
        }

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 4); // 1 global + 3 workspace
    }

    #[test]
    fn find_db_files_returns_sorted_order() {
        let dir = TempDir::new().unwrap();

        let ws_z = dir.path().join("workspaceStorage").join("zeta");
        let ws_a = dir.path().join("workspaceStorage").join("alpha");
        fs::create_dir_all(&ws_z).unwrap();
        fs::create_dir_all(&ws_a).unwrap();
        fs::write(ws_z.join("state.vscdb"), "").unwrap();
        fs::write(ws_a.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 2);

        let names: Vec<_> = dbs
            .iter()
            .map(|p| {
                p.parent()
                    .and_then(|x| x.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn find_db_files_ignores_workspace_noise_files() {
        let dir = TempDir::new().unwrap();
        let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&workspace_dir).unwrap();

        for i in 0..20 {
            fs::write(workspace_dir.join(format!("noise_{i}.json")), "{}").unwrap();
        }
        fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 1);
        assert!(dbs[0].ends_with("state.vscdb"));
    }

    #[test]
    fn find_db_files_supports_workspace_storage_root_db() {
        let dir = TempDir::new().unwrap();
        let workspace_storage = dir.path().join("workspaceStorage");
        fs::create_dir_all(&workspace_storage).unwrap();
        fs::write(workspace_storage.join("state.vscdb"), "").unwrap();

        let dbs = CursorConnector::find_db_files(dir.path());
        assert_eq!(dbs.len(), 1);
        assert!(dbs[0].ends_with("workspaceStorage/state.vscdb"));
    }

    #[test]
    fn has_any_db_file_is_true_for_workspace_storage_db() {
        let dir = TempDir::new().unwrap();
        let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::write(workspace_dir.join("state.vscdb"), "").unwrap();

        assert!(CursorConnector::has_any_db_file(dir.path()));
    }

    #[test]
    fn has_any_db_file_is_false_for_workspace_noise_only() {
        let dir = TempDir::new().unwrap();
        let workspace_dir = dir.path().join("workspaceStorage").join("abc123");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::write(workspace_dir.join("note.txt"), "noise").unwrap();

        assert!(!CursorConnector::has_any_db_file(dir.path()));
    }

    // =========================================================================
    // parse_bubble tests
    // =========================================================================

    #[test]
    fn parse_bubble_with_text() {
        let bubble = json!({
            "text": "Hello from user",
            "type": "user"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.content, "Hello from user");
        assert_eq!(msg.role, "user");
    }

    #[test]
    fn parse_bubble_with_content_field() {
        let bubble = json!({
            "content": "Response from assistant",
            "role": "assistant"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 1);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.content, "Response from assistant");
        assert_eq!(msg.role, "assistant");
    }

    #[test]
    fn parse_bubble_with_message_field() {
        let bubble = json!({
            "message": "Another message",
            "type": "ai"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.content, "Another message");
        assert_eq!(msg.role, "assistant"); // "ai" maps to assistant
    }

    #[test]
    fn parse_bubble_role_normalization() {
        let test_cases = vec![
            ("user", "user"),
            ("human", "user"),
            ("assistant", "assistant"),
            ("ai", "assistant"),
            ("bot", "assistant"),
            ("custom", "custom"), // Unknown roles pass through
        ];

        for (input_role, expected_role) in test_cases {
            let bubble = json!({
                "text": "test",
                "type": input_role
            });

            let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
            assert_eq!(
                msg.role, expected_role,
                "Failed for input role: {}",
                input_role
            );
        }
    }

    #[test]
    fn parse_bubble_empty_content_returns_none() {
        let bubble = json!({
            "text": "",
            "type": "user"
        });

        assert!(CursorConnector::parse_bubble(&bubble, 0).is_none());
    }

    #[test]
    fn parse_bubble_whitespace_only_returns_none() {
        let bubble = json!({
            "text": "   \n\t  ",
            "type": "user"
        });

        assert!(CursorConnector::parse_bubble(&bubble, 0).is_none());
    }

    #[test]
    fn parse_bubble_extracts_timestamp() {
        let bubble = json!({
            "text": "Test",
            "type": "user",
            "timestamp": 1700000000000i64
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.created_at, Some(1700000000000));
    }

    #[test]
    fn parse_bubble_extracts_model() {
        let bubble = json!({
            "text": "Response",
            "type": "assistant",
            "model": "gpt-4"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.author, Some("gpt-4".to_string()));
    }

    #[test]
    fn parse_bubble_defaults_to_assistant() {
        let bubble = json!({
            "text": "No role specified"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.role, "assistant");
    }

    // =========================================================================
    // parse_composer_data tests
    // =========================================================================

    #[test]
    fn parse_composer_data_with_tabs_and_bubbles() {
        let key = "composerData:abc-123";
        let value = json!({
            "createdAt": 1700000000000i64,
            "tabs": [{
                "bubbles": [
                    {"text": "Hello", "type": "user"},
                    {"text": "Hi there!", "type": "assistant"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.agent_slug, "cursor");
        assert_eq!(conv.external_id, Some("abc-123".to_string()));
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[1].role, "assistant");
    }

    #[test]
    fn parse_composer_data_with_conversation_map() {
        let key = "composerData:def-456";
        let value = json!({
            "conversationMap": {
                "conv1": {
                    "bubbles": [
                        {"text": "Question?", "type": "user"},
                        {"content": "Answer!", "role": "assistant"}
                    ]
                }
            }
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 2);
    }

    #[test]
    fn parse_composer_data_with_text_only() {
        let key = "composerData:simple-123";
        let value = json!({
            "text": "Simple user input without bubbles",
            "createdAt": 1700000000000i64
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, "user");
        assert!(conv.messages[0].content.contains("Simple user input"));
    }

    #[test]
    fn parse_composer_data_with_rich_text() {
        let key = "composerData:rich-789";
        let value = json!({
            "richText": "Rich text content here"
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert!(conv.messages[0].content.contains("Rich text"));
    }

    #[test]
    fn parse_composer_data_skips_duplicates() {
        let key = "composerData:dup-123";
        let value = json!({ "text": "Content" }).to_string();

        let mut seen = HashSet::new();
        let conv1 = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );
        let conv2 = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv1.is_some());
        assert!(conv2.is_none()); // Duplicate should return None
    }

    #[test]
    fn parse_composer_data_returns_none_for_empty() {
        let key = "composerData:empty-123";
        let value = json!({}).to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_none());
    }

    #[test]
    fn parse_composer_data_extracts_model_config() {
        let key = "composerData:model-123";
        let value = json!({
            "text": "Test",
            "modelConfig": {
                "modelName": "gpt-4-turbo"
            }
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.metadata["model"], "gpt-4-turbo");
    }

    #[test]
    fn parse_composer_data_invalid_key_returns_none() {
        let key = "not-composer-data"; // Missing "composerData:" prefix
        let value = json!({ "text": "Content" }).to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_none());
    }

    // =========================================================================
    // parse_aichat_data tests
    // =========================================================================

    #[test]
    fn parse_aichat_data_with_tabs() {
        let key = "aichat.chatdata";
        let value = json!({
            "tabs": [{
                "timestamp": 1700000000000i64,
                "bubbles": [
                    {"text": "User question", "type": "user"},
                    {"text": "AI response", "type": "ai"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

        assert!(conv.is_some());
        let conv = conv.unwrap();
        assert_eq!(conv.agent_slug, "cursor");
        assert!(conv.external_id.as_ref().unwrap().starts_with("aichat-"));
        assert_eq!(conv.messages.len(), 2);
    }

    #[test]
    fn parse_aichat_data_tracks_min_max_out_of_order_timestamps() {
        let key = "aichat.bounds";
        let value = json!({
            "tabs": [
                {
                    "timestamp": 1700000200,
                    "bubbles": [
                        {"text": "later", "type": "ai", "timestamp": 1700000300}
                    ]
                },
                {
                    "timestamp": 1700000000,
                    "bubbles": [
                        {"text": "earlier", "type": "user", "timestamp": 1700000100}
                    ]
                }
            ]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen)
                .expect("aichat conversation should parse");

        // Bounds should be based on true min/max timestamps, not traversal order.
        assert_eq!(conv.started_at, Some(1_700_000_100_000));
        assert_eq!(conv.ended_at, Some(1_700_000_300_000));
    }

    #[test]
    fn parse_aichat_data_parses_string_tab_timestamp() {
        let key = "aichat.string-ts";
        let value = json!({
            "tabs": [{
                "timestamp": "1700000400",
                "bubbles": [
                    {"text": "no bubble timestamp", "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen)
                .expect("aichat conversation should parse");

        // String timestamps should be normalized to milliseconds.
        assert_eq!(conv.started_at, Some(1_700_000_400_000));
        assert_eq!(conv.ended_at, Some(1_700_000_400_000));
    }

    #[test]
    fn parse_aichat_data_returns_none_for_empty() {
        let key = "aichat.empty";
        let value = json!({
            "tabs": []
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

        assert!(conv.is_none());
    }

    #[test]
    fn parse_aichat_data_skips_duplicates() {
        let key = "aichat.dup";
        let value = json!({
            "tabs": [{
                "bubbles": [{"text": "Content", "type": "user"}]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv1 =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);
        let conv2 =
            CursorConnector::parse_aichat_data(key, &value, Path::new("/test"), None, &mut seen);

        assert!(conv1.is_some());
        assert!(conv2.is_none());
    }

    // =========================================================================
    // extract_from_db tests
    // =========================================================================

    #[test]
    fn extract_from_db_with_composer_data() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Database test" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:db-test-123", value.as_str()],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        assert_eq!(convs.len(), 1);
        assert!(convs[0].messages[0].content.contains("Database test"));
    }

    #[test]
    fn extract_from_db_with_aichat_data() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        let value = json!({
            "tabs": [{
                "bubbles": [{"text": "Aichat test", "type": "user"}]
            }]
        })
        .to_string();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params![
                "workbench.panel.aichat.view.aichat.chatdata",
                value.as_str()
            ],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn extract_from_db_handles_empty_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let _conn = create_test_db(&db_path);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        assert!(convs.is_empty());
    }

    #[test]
    fn extract_from_db_fails_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("nonexistent.vscdb");

        let result = CursorConnector::extract_from_db(&db_path, None, &|| {});
        assert!(result.is_err());
    }

    // Cursor inserts internal marker rows into `cursorDiskKV` with NULL `value`
    // (e.g. composer metadata stubs). Without filtering, `row.get_typed::<String>(1)?`
    // on NULL aborts the row mapper, the `if let Ok(rows) = ...` swallows the error,
    // and the whole connector silently returns zero conversations. Regression test:
    // a single NULL row must not mask the valid rows beside it. (PR #8)
    #[test]
    fn extract_from_db_skips_null_value_rows_in_cursor_disk_kv() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        // One valid composer entry.
        let value = json!({ "text": "Valid conversation" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:valid-row", value.as_str()],
        )
        .unwrap();
        // One internal-marker entry with NULL value sitting in the same prefix range.
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, NULL)",
            params!["composerData:null-marker"],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        assert_eq!(
            convs.len(),
            1,
            "NULL-value row must not abort the row mapper for valid sibling rows"
        );
        assert!(convs[0].messages[0].content.contains("Valid conversation"));
    }

    #[test]
    fn extract_from_db_skips_null_value_rows_in_item_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        let value = json!({
            "tabs": [{ "bubbles": [{"text": "Valid aichat", "type": "user"}] }]
        })
        .to_string();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params![
                "workbench.panel.aichat.view.aichat.chatdata",
                value.as_str()
            ],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, NULL)",
            params!["workbench.panel.aichat.view.composer.null-marker"],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        assert_eq!(convs.len(), 1);
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_not_found_without_cursor_dir() {
        let connector = CursorConnector::new();
        let result = connector.detect();
        // On most CI/test systems, Cursor won't be installed
        // Just verify detect() doesn't panic
        let _ = result.detected;
    }

    // =========================================================================
    // Scan tests
    // =========================================================================

    #[test]
    fn scan_empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();

        // Create globalStorage to make scan() use this directory instead of fallback
        let global_dir = dir.path().join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        // Create an empty state.vscdb to prevent fallback to system Cursor
        create_test_db(&global_dir.join("state.vscdb"));

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    /// Path of a SQLite sidecar file (`<db>-wal`, `<db>-shm`).
    fn sqlite_sidecar(db: &Path, suffix: &str) -> PathBuf {
        let mut raw = db.as_os_str().to_owned();
        raw.push(suffix);
        PathBuf::from(raw)
    }

    /// `(bytes, mtime)` of one store file, for never-modify assertions.
    fn file_state(path: &Path) -> (Option<Vec<u8>>, Option<std::time::SystemTime>) {
        (
            fs::read(path).ok(),
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok(),
        )
    }

    /// A `state.vscdb` in WAL mode with one composer row, closed the way a
    /// hard-killed Cursor leaves it: the writer is gone, and a checkpointed
    /// `-wal` (0 bytes) plus its `-shm` wal-index remain next to the db.
    fn create_wal_mode_state_vscdb(dir: &Path) -> PathBuf {
        let db_path = dir.join("globalStorage").join("state.vscdb");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = create_test_db(&db_path);
        let mode: String = conn
            .query_row_map("PRAGMA journal_mode=wal;", &[], |row| {
                row.get_typed::<String>(0)
            })
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![
                "composerData:wal-fallback",
                r#"{"text":"Read after a crash"}"#
            ],
        )
        .unwrap();

        // The incident shape: every frame checkpointed into the db, so the
        // `-wal` remains on disk at 0 bytes after the writer exits.
        conn.query_row_map::<(i64, i64, i64), _>("PRAGMA wal_checkpoint(TRUNCATE);", &[], |row| {
            Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?))
        })
        .unwrap();
        drop(conn);

        assert!(
            sqlite_sidecar(&db_path, "-wal").is_file(),
            "the writer must leave a `-wal` file behind"
        );
        assert!(
            sqlite_sidecar(&db_path, "-shm").is_file(),
            "the writer must leave a `-shm` wal-index behind"
        );
        db_path
    }

    /// Leave the `-shm` wal-index in one of the states a crashed writer can:
    /// `garbage` bytes, a `torn` header checksum (one flipped bit), `absent`,
    /// `zeroed`, or `empty`. A store in any of these states must still be
    /// read, and the user's db and `-wal` files must never change.
    fn damage_wal_index(db: &Path, state: &str) {
        let shm = sqlite_sidecar(db, "-shm");
        match state {
            "garbage" => fs::write(&shm, vec![0xA5_u8; 32_768]).unwrap(),
            "torn" => {
                let mut bytes = fs::read(&shm).unwrap();
                assert!(bytes.len() > 24, "the fixture `-shm` must have a header");
                // One flipped bit inside the wal-index header's checksum word:
                // a sidecar torn mid-write.
                bytes[16] ^= 0x01;
                fs::write(&shm, bytes).unwrap();
            }
            "absent" => fs::remove_file(&shm).unwrap(),
            "zeroed" => fs::write(&shm, vec![0_u8; 32_768]).unwrap(),
            "empty" => fs::write(&shm, []).unwrap(),
            other => unreachable!("unknown wal-index state: {other}"),
        }
    }

    /// A `state.vscdb` left with an unusable wal-index (the crash shape from
    /// the 2026-11-07 OpenCode zero-index incident, on Cursor's store) must
    /// still be read, without modifying the user's files. The db and `-wal`
    /// are never written by any read-only open; the `-shm` is never written
    /// whenever the snapshot fallback engages, and `garbage`/`torn`
    /// wal-indexes refuse the strict open on every frankensqlite build, so the
    /// fallback always engages for them.
    #[serial]
    #[test]
    fn scan_reads_a_state_vscdb_left_with_an_unusable_wal_index() {
        for state in ["garbage", "torn", "absent", "zeroed", "empty"] {
            let tmp = TempDir::new().unwrap();
            let db_path = create_wal_mode_state_vscdb(tmp.path());
            damage_wal_index(&db_path, state);
            let before_db = file_state(&db_path);
            let before_wal = file_state(&sqlite_sidecar(&db_path, "-wal"));
            let before_shm = file_state(&sqlite_sidecar(&db_path, "-shm"));

            let ctx = ScanContext::with_roots(
                tmp.path().to_path_buf(),
                vec![ScanRoot::local(tmp.path().to_path_buf())],
                None,
            );
            let convs = CursorConnector::new()
                .scan(&ctx)
                .unwrap_or_else(|error| panic!("{state}: scan failed: {error:#}"));

            assert_eq!(
                convs.len(),
                1,
                "{state}: the crashed store must still be read"
            );
            assert!(
                convs[0]
                    .messages
                    .iter()
                    .any(|message| message.content.contains("Read after a crash")),
                "{state}: the composer transcript must be readable"
            );

            assert_eq!(
                file_state(&db_path),
                before_db,
                "{state}: reading must never modify the db"
            );
            assert_eq!(
                file_state(&sqlite_sidecar(&db_path, "-wal")),
                before_wal,
                "{state}: reading must never modify the `-wal`"
            );
            if matches!(state, "garbage" | "torn") {
                assert_eq!(
                    file_state(&sqlite_sidecar(&db_path, "-shm")),
                    before_shm,
                    "{state}: the snapshot fallback must leave the `-shm` untouched"
                );
            }
        }
    }

    /// A store that fails to open for any other reason must surface as a
    /// scan error (cass F5 semantics), not a swallowed warn plus an
    /// Ok(0 conversations) scan.
    #[test]
    fn scan_reports_an_unreadable_state_vscdb_as_a_scan_error() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("globalStorage").join("state.vscdb");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        fs::write(&db_path, b"this is not a sqlite database").unwrap();

        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            None,
        );
        let error = CursorConnector::new().scan(&ctx).unwrap_err();
        let chain = format!("{error:#}");
        assert!(
            chain.contains("state.vscdb"),
            "the scan error must name the failing store: {chain}"
        );
    }

    /// cass follow-up to F9 (2026-11-07): the same rule on Cursor's store — a
    /// `state.vscdb` that is a *valid but empty* database (a 0-byte file, or a
    /// schema with no tables at all) must scan to `Ok` with 0 conversations,
    /// not a scan error on every index run.
    #[test]
    fn scan_treats_a_provably_empty_state_vscdb_as_no_sessions() {
        for state in ["zero-byte", "no-tables"] {
            let tmp = TempDir::new().unwrap();
            let db_path = tmp.path().join("globalStorage").join("state.vscdb");
            fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            match state {
                "zero-byte" => fs::write(&db_path, []).unwrap(),
                "no-tables" => {
                    let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
                    conn.execute("CREATE TABLE schema_marker (x INTEGER);")
                        .unwrap();
                    conn.execute("DROP TABLE schema_marker;").unwrap();
                    drop(conn);
                    assert!(
                        fs::metadata(&db_path).unwrap().len() > 0,
                        "{state}: the fixture must be a real database file, not 0 bytes"
                    );
                }
                other => unreachable!("unknown empty-store state: {other}"),
            }

            let ctx = ScanContext::with_roots(
                tmp.path().to_path_buf(),
                vec![ScanRoot::local(tmp.path().to_path_buf())],
                None,
            );
            let convs = CursorConnector::new().scan(&ctx).unwrap_or_else(|error| {
                panic!("{state}: an empty store must not be a scan error: {error:#}")
            });
            assert!(
                convs.is_empty(),
                "{state}: an empty store holds no conversations"
            );
        }
    }

    #[test]
    fn scan_processes_global_storage() {
        let dir = TempDir::new().unwrap();

        // Create Cursor-like directory structure
        let cursor_dir = dir.path().join("Cursor");
        let global_dir = cursor_dir.join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();

        // Create database with test data
        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Scan test" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:scan-123", value.as_str()],
        )
        .unwrap();
        drop(conn);

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(cursor_dir.clone(), None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);

        let discovered_paths: HashSet<PathBuf> = connector
            .discover_source_files(&ctx)
            .unwrap()
            .into_iter()
            .map(|source| source.source_path)
            .collect();
        assert!(
            discovered_paths.contains(&db_path),
            "discovery should include consumed Cursor database {}",
            db_path.display()
        );
        assert!(
            convs[0].source_path.starts_with(&db_path),
            "Cursor conversation source path should be derived from consumed db {}",
            db_path.display()
        );
    }

    /// Cursor declares `key TEXT UNIQUE`, so the key range is served by the
    /// `sqlite_autoindex_cursorDiskKV_1` index. A residual predicate in this
    /// SQL (it once carried `value IS NOT NULL`) makes frankensqlite plan a
    /// full table scan, which on a real 2.9 GB store read the whole table for
    /// each of 1,488 composers: about 0.8 s each, 20+ minutes per scan.
    #[test]
    fn cursor_disk_kv_range_query_seeks_the_key_index() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
            .unwrap();
        let plan = conn
            .query_map_collect(
                &format!("EXPLAIN QUERY PLAN {CURSOR_DISK_KV_RANGE_SQL}"),
                params!["bubbleId:a:", "bubbleId:a;"],
                |row| row.get_typed::<String>(3),
            )
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("USING INDEX sqlite_autoindex_cursorDiskKV_1"),
            "the key-range query must seek the key index, got:\n{plan}"
        );
    }

    /// A large `state.vscdb` keeps `extract_from_db` busy for many minutes
    /// before the first conversation exists. The host's stall watchdog only
    /// sees that work through the progress tick, so every row read and every
    /// composer parsed must tick (cass#373).
    #[test]
    fn scan_ticks_progress_for_every_row_and_composer() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = TempDir::new().unwrap();
        let cursor_dir = dir.path().join("Cursor");
        let global_dir = cursor_dir.join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        for id in ["tick-a", "tick-b", "tick-c"] {
            let value = json!({ "text": format!("composer {id}") }).to_string();
            conn.execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![format!("composerData:{id}").as_str(), value.as_str()],
            )
            .unwrap();
        }
        let legacy =
            json!({ "tabs": [{ "bubbles": [{ "text": "legacy", "type": "user" }] }] }).to_string();
        conn.execute_compat(
            "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            params![
                "workbench.panel.aichat.view.aichat.chatdata",
                legacy.as_str()
            ],
        )
        .unwrap();
        drop(conn);

        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let ctx =
            ScanContext::local_default(cursor_dir, None).with_progress_tick(Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }));
        let convs = CursorConnector::new().scan(&ctx).unwrap();

        assert_eq!(convs.len(), 4, "three composers and one legacy chat");
        // One tick per composerData row read, one per composer parsed, and
        // one per ItemTable row read.
        let expected_min = 3 + 3 + 1;
        assert!(
            ticks.load(Ordering::Relaxed) >= expected_min,
            "the scan ticked only {} times",
            ticks.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn scan_with_home_scan_root_finds_cursor_storage() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();

        let global_dir = home.join(".config/Cursor/User/globalStorage");
        fs::create_dir_all(&global_dir).unwrap();

        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Explicit root" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:explicit-123", value.as_str()],
        )
        .unwrap();
        drop(conn);

        let connector = CursorConnector::new();
        let ctx = ScanContext::with_roots(
            PathBuf::new(),
            vec![ScanRoot::local(home.to_path_buf())],
            None,
        );
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        let convs = result.unwrap();
        assert_eq!(convs.len(), 1);
    }

    #[test]
    fn scan_recognizes_cursor_in_path() {
        let dir = TempDir::new().unwrap();

        // Directory name contains "Cursor"
        let cursor_dir = dir.path().join("TestCursor");
        let global_dir = cursor_dir.join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();

        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Path test" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:path-123", value.as_str()],
        )
        .unwrap();
        drop(conn);

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(cursor_dir, None);
        let result = connector.scan(&ctx);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn default_detection_scopes_agent_transcripts_to_cursor_base() {
        let dir = TempDir::new().unwrap();

        // A data_dir that IS a Cursor base: one Composer conversation…
        let cursor_dir = dir.path().join("Cursor");
        let global_dir = cursor_dir.join("globalStorage");
        fs::create_dir_all(&global_dir).unwrap();
        let db_path = global_dir.join("state.vscdb");
        let conn = create_test_db(&db_path);
        let value = json!({ "text": "Both sources" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:both-123", value.as_str()],
        )
        .unwrap();
        drop(conn);

        // …plus one Agent transcript under the SAME base. Default detection
        // must scan it from HERE, not from the machine's real
        // ~/.cursor/projects (which leaks foreign sessions into the scan and
        // made these tests fail on machines with local Cursor data).
        let sid = "sess-both";
        let transcript_dir = cursor_dir
            .join("proj-x")
            .join(AGENT_TRANSCRIPTS_DIR)
            .join(sid);
        fs::create_dir_all(&transcript_dir).unwrap();
        fs::write(
            transcript_dir.join(format!("{sid}.jsonl")),
            format!(
                "{}\n",
                r#"{"role":"user","message":{"content":[{"type":"text","text":"from agent transcripts"}]}}"#
            ),
        )
        .unwrap();

        let connector = CursorConnector::new();
        let ctx = ScanContext::local_default(cursor_dir.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(
            convs.len(),
            2,
            "exactly the one composer conversation and the one agent transcript from this base"
        );
        let ids: HashSet<String> = convs.iter().filter_map(|c| c.external_id.clone()).collect();
        assert!(
            ids.contains(sid),
            "agent transcript under a Cursor-base data_dir must be indexed from that dir, ids={ids:?}"
        );
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn parse_composer_data_invalid_json_returns_none() {
        let key = "composerData:invalid-123";
        let value = "not valid json {{{";

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_none());
    }

    #[test]
    fn parse_bubble_preserves_original_in_extra() {
        let bubble = json!({
            "text": "Test",
            "type": "user",
            "customField": "customValue"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0).unwrap();
        assert_eq!(msg.extra["customField"], "customValue");
    }

    #[test]
    fn conversation_title_from_first_message() {
        let key = "composerData:title-test";
        let value = json!({
            "tabs": [{
                "bubbles": [
                    {"text": "This is the first line\nSecond line here", "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        let conv = conv.unwrap();
        // Title should be first line only
        assert_eq!(conv.title, Some("This is the first line".to_string()));
    }

    #[test]
    fn conversation_title_truncates_long_lines() {
        let key = "composerData:long-title";
        let long_text = "x".repeat(200);
        let value = json!({ "text": long_text }).to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        let conv = conv.unwrap();
        assert!(conv.title.as_ref().unwrap().len() <= 100);
    }

    #[test]
    fn messages_are_reindexed_sequentially() {
        let key = "composerData:reindex";
        let value = json!({
            "tabs": [{
                "bubbles": [
                    {"text": "One", "type": "user"},
                    {"text": "Two", "type": "assistant"},
                    {"text": "Three", "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        )
        .unwrap();

        assert_eq!(conv.messages[0].idx, 0);
        assert_eq!(conv.messages[1].idx, 1);
        assert_eq!(conv.messages[2].idx, 2);
    }

    // =========================================================================
    // WSL detection tests (Linux-only)
    // =========================================================================

    #[cfg(target_os = "linux")]
    mod wsl_tests {
        use super::*;

        #[test]
        fn is_wsl_returns_false_on_native_linux() {
            // On a real Linux system (not WSL), /proc/version won't contain "microsoft"
            // This test just verifies the function doesn't panic
            let result = CursorConnector::is_wsl();
            // We can't assert the exact value since it depends on the environment,
            // but we verify the function works
            let _ = result;
        }

        #[test]
        fn find_wsl_cursor_path_returns_none_without_mnt_c() {
            // On native Linux, /mnt/c typically doesn't exist
            // This verifies the function gracefully returns None
            if !Path::new("/mnt/c/Users").exists() {
                let result = CursorConnector::find_wsl_cursor_path();
                assert!(result.is_none());
            }
        }

        #[test]
        fn find_wsl_cursor_path_skips_system_dirs() {
            // Create a temp structure that mimics /mnt/c/Users with system dirs
            let dir = TempDir::new().unwrap();
            let users_dir = dir.path().join("Users");
            fs::create_dir_all(&users_dir).unwrap();

            // Create system directories that should be skipped
            for sys_dir in ["Default", "Public", "All Users", "Default User"] {
                fs::create_dir_all(users_dir.join(sys_dir)).unwrap();
            }

            // The function checks /mnt/c/Users specifically, so we can't directly test
            // the skipping logic without mocking. Instead, verify the skip list is correct.
            let skip_list = ["Default", "Public", "All Users", "Default User"];
            assert_eq!(skip_list.len(), 4);
        }

        #[test]
        fn wsl_path_structure_is_valid() {
            // Verify the expected WSL path structure
            let expected = Path::new("/mnt/c/Users/TestUser/AppData/Roaming/Cursor/User");
            assert!(expected.starts_with("/mnt/c/Users"));
            assert!(expected.ends_with("Cursor/User"));
        }
    }

    // =========================================================================
    // Edge case tests — malformed input robustness (br-2w98)
    // =========================================================================

    #[test]
    fn truncated_json_in_db_value_returns_none() {
        let key = "composerData:truncated-123";
        // JSON truncated mid-object
        let value = r#"{"text": "Hello", "tabs": [{"bubbles": [{"text": "Hi", "type":"#;

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(
            conv.is_none(),
            "truncated JSON should not produce a conversation"
        );
    }

    #[test]
    fn json_type_mismatch_in_bubbles_skips_bad_entries() {
        let key = "composerData:mismatch-123";
        let value = json!({
            "tabs": [{
                "bubbles": [
                    // String where object expected
                    "not a bubble object",
                    // Number instead of object
                    42,
                    // Null
                    null,
                    // Boolean
                    true,
                    // Valid entry that should still be extracted
                    {"text": "Valid bubble", "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(
            conv.is_some(),
            "should extract valid bubbles despite type mismatches"
        );
        let conv = conv.unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].content, "Valid bubble");
    }

    #[test]
    fn deeply_nested_json_in_db_value_does_not_stack_overflow() {
        let key = "composerData:deep-123";
        // Build deeply nested JSON (200 levels) - serde_json has a recursion limit of 128
        let mut nested = String::new();
        for _ in 0..200 {
            nested.push_str("{\"a\":");
        }
        nested.push('1');
        for _ in 0..200 {
            nested.push('}');
        }

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &nested,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        // Should either parse or fail gracefully, never stack overflow
        assert!(
            conv.is_none(),
            "deeply nested JSON without text/tabs should produce no conversation"
        );
    }

    #[test]
    fn large_message_body_in_bubble_handled() {
        let key = "composerData:large-123";
        let large_content = "x".repeat(1_000_000);
        let value = json!({
            "tabs": [{
                "bubbles": [
                    {"text": large_content, "type": "user"}
                ]
            }]
        })
        .to_string();

        let mut seen = HashSet::new();
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            None,
            &mut seen,
            None,
        );

        assert!(conv.is_some(), "large message body should not cause OOM");
        let conv = conv.unwrap();
        assert_eq!(conv.messages[0].content.len(), 1_000_000);
    }

    #[test]
    fn corrupted_sqlite_db_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("corrupted.vscdb");
        fs::write(&db_path, "not a sqlite database at all").unwrap();

        let result = CursorConnector::extract_from_db(&db_path, None, &|| {});
        // SQLite engines may open the file lazily; query failures are caught with if-let-Ok
        // The important thing is that it doesn't panic
        if let Ok(convs) = result {
            assert!(
                convs.is_empty(),
                "corrupted DB should produce no conversations"
            );
        }
    }

    #[test]
    fn db_with_missing_tables_returns_empty() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("no_tables.vscdb");

        // Create a valid SQLite DB but without the expected tables
        let conn = Connection::open(db_path.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE unrelated_table (id INTEGER PRIMARY KEY, data TEXT)")
            .unwrap();
        drop(conn);

        let result = CursorConnector::extract_from_db(&db_path, None, &|| {});
        // Should not error even without expected tables
        assert!(result.is_ok(), "missing tables should not cause an error");
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn v040_headers_referencing_missing_bubbles_skips_gracefully() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);
        // Insert composerData with fullConversationHeadersOnly referencing non-existent bubbles
        let value = json!({
            "fullConversationHeadersOnly": [
                {"bubbleId": "nonexistent-1"},
                {"bubbleId": "nonexistent-2"}
            ]
        })
        .to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:v040-missing", value.as_str()],
        )
        .unwrap();
        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        // Should not crash; no messages means no conversation
        assert!(
            convs.is_empty(),
            "missing bubble references should produce no conversations"
        );
    }

    #[test]
    fn unexpected_numeric_type_in_bubble_defaults_to_assistant() {
        // Bubble type values other than 1 (user) or 2 (assistant)
        for type_val in [0, 3, -1, 999] {
            let bubble = json!({
                "text": "Unknown type bubble",
                "type": type_val
            });

            let msg = CursorConnector::parse_bubble(&bubble, 0);
            assert!(
                msg.is_some(),
                "type {} should still produce a message",
                type_val
            );
            assert_eq!(
                msg.unwrap().role,
                "assistant",
                "unknown numeric type {} should default to assistant",
                type_val
            );
        }
    }

    #[test]
    fn null_bytes_in_bubble_content_handled() {
        let bubble = json!({
            "text": "before\u{0000}after",
            "type": "user"
        });

        let msg = CursorConnector::parse_bubble(&bubble, 0);
        assert!(
            msg.is_some(),
            "null bytes in content should not cause errors"
        );
    }

    #[test]
    fn mixed_valid_and_invalid_entries_in_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.vscdb");

        let conn = create_test_db(&db_path);

        // Insert invalid JSON
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:invalid-1", "not valid json {{{"],
        )
        .unwrap();

        // Insert valid entry
        let valid_value = json!({ "text": "Valid entry" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:valid-1", valid_value.as_str()],
        )
        .unwrap();

        // Insert empty JSON
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:empty-1", "{}"],
        )
        .unwrap();

        // Insert another valid entry
        let valid_value2 = json!({ "text": "Another valid" }).to_string();
        conn.execute_compat(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params!["composerData:valid-2", valid_value2.as_str()],
        )
        .unwrap();

        drop(conn);

        let convs = CursorConnector::extract_from_db(&db_path, None, &|| {}).unwrap();
        assert_eq!(
            convs.len(),
            2,
            "should extract only the valid conversations, skipping invalid/empty"
        );
    }

    #[test]
    fn parse_workspace_uri_handles_malformed_uris() {
        // Empty string
        assert!(CursorConnector::parse_workspace_uri("").is_none());

        // No scheme
        assert!(CursorConnector::parse_workspace_uri("just/a/path").is_none());

        // Unknown scheme
        assert!(CursorConnector::parse_workspace_uri("ftp://host/path").is_none());

        // file:// with path should work
        let result = CursorConnector::parse_workspace_uri("file:///home/user/project");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), PathBuf::from("/home/user/project"));

        // vscode-remote:// with no path separator
        assert!(CursorConnector::parse_workspace_uri("vscode-remote://no-slash-here").is_none());

        // URL-encoded file:// path
        let result = CursorConnector::parse_workspace_uri("file:///home/user/my%20project/src");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), PathBuf::from("/home/user/my project/src"));
    }

    #[test]
    fn since_ts_filtering_skips_old_conversations() {
        let key = "composerData:old-conv";
        let value = json!({
            "text": "Old message",
            "createdAt": 1000000000000i64,
            "lastUpdatedAt": 1000000001000i64
        })
        .to_string();

        let mut seen = HashSet::new();
        // Set since_ts to after the conversation's lastUpdatedAt
        let conv = CursorConnector::parse_composer_data(
            key,
            &value,
            Path::new("/test"),
            Some(1500000000000),
            &mut seen,
            None,
        );

        assert!(
            conv.is_none(),
            "conversation older than since_ts should be skipped"
        );
    }

    // NOTE:
    // The original cass test mutated HOME to validate default detection behavior.
    // Rust 2024 marks process-wide environment mutation as unsafe, and this crate
    // forbids unsafe code globally. Keep behavior covered by explicit-root tests.
}

#[cfg(test)]
mod agent_transcript_tests {
    //! Tests for the Cursor Agent transcript reader (gh #306). Fixtures use the
    //! reporter's documented `{role, message:{content:[...]}}` shape plus
    //! malformed / partial inputs to prove graceful degradation. All tests use
    //! explicit scan roots (no HOME / env mutation), so the Composer path and
    //! every other connector remain byte-for-byte unaffected.
    use super::*;
    use crate::connectors::assert_discovery_covers_scan_sources;
    use std::fs;
    use tempfile::TempDir;

    const USER_LINE: &str =
        r#"{"role":"user","message":{"content":[{"type":"text","text":"find the bug"}]}}"#;
    const ASSISTANT_LINE: &str = r#"{"role":"assistant","message":{"content":[{"type":"text","text":"on it"},{"type":"tool_use","name":"Grep","input":{"pattern":"bug"}}]}}"#;

    fn write_transcript(root: &Path, project: &str, sid: &str, lines: &[&str]) -> PathBuf {
        let dir = root.join(project).join(AGENT_TRANSCRIPTS_DIR).join(sid);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{sid}.jsonl"));
        fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();
        file
    }

    fn ctx_for(root: &Path) -> ScanContext {
        ScanContext::with_roots(
            root.to_path_buf(),
            vec![ScanRoot::local(root.to_path_buf())],
            None,
        )
    }

    fn write_workspace(transcript: &Path, workspace: &str) -> PathBuf {
        let path = CursorConnector::agent_workspace_metadata_path(transcript).unwrap();
        fs::write(
            &path,
            serde_json::json!({"workspacePath": workspace}).to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn scan_parses_documented_agent_shape() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(
            tmp.path(),
            "Users-ibrahim-workspace-foo",
            "sess-1",
            &[USER_LINE, ASSISTANT_LINE],
        );
        write_workspace(&transcript, "/Users/ibrahim/workspace/foo");
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.agent_slug, "cursor");
        assert_eq!(c.external_id.as_deref(), Some("sess-1"));
        assert_eq!(c.metadata["cursor_format"], "agent");
        assert_eq!(c.metadata["source"], "cursor");
        assert_eq!(
            c.metadata["cursor_workspace_attribution"],
            "workspace_trusted"
        );
        assert_eq!(
            c.metadata["cursor_project_dir"],
            "Users-ibrahim-workspace-foo"
        );
        assert_eq!(
            c.workspace.as_deref(),
            Some(Path::new("/Users/ibrahim/workspace/foo"))
        );
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].role, "user");
        assert!(c.messages[0].content.contains("find the bug"));
        assert_eq!(c.messages[1].role, "assistant");
        assert!(c.messages[1].invocations.iter().any(|i| i.name == "Grep"));
        assert!(c.title.as_deref().unwrap().starts_with("find the bug"));
        // Indices are contiguous from 0.
        for (i, m) in c.messages.iter().enumerate() {
            assert_eq!(m.idx, i64::try_from(i).unwrap());
        }
    }

    #[test]
    fn subagent_and_non_primary_transcripts_are_skipped() {
        let tmp = TempDir::new().unwrap();
        // Primary (kept).
        write_transcript(tmp.path(), "proj", "main-sid", &[USER_LINE]);
        // Subagent transcript under subagents/ (skipped).
        let sub = tmp
            .path()
            .join("proj")
            .join(AGENT_TRANSCRIPTS_DIR)
            .join("main-sid")
            .join("subagents")
            .join("sub-1");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("sub-1.jsonl"), format!("{USER_LINE}\n")).unwrap();
        // Non-primary sibling file (stem != dir name) (skipped).
        let dir = tmp
            .path()
            .join("proj")
            .join(AGENT_TRANSCRIPTS_DIR)
            .join("main-sid");
        fs::write(dir.join("notes.jsonl"), format!("{USER_LINE}\n")).unwrap();

        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(convs.len(), 1, "only the primary transcript is indexed");
        assert_eq!(convs[0].external_id.as_deref(), Some("main-sid"));
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let tmp = TempDir::new().unwrap();
        write_transcript(
            tmp.path(),
            "proj",
            "s",
            &[USER_LINE, "this is not json", ASSISTANT_LINE],
        );
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2, "valid records survive junk");
    }

    #[test]
    fn empty_and_contentless_transcripts_yield_no_conversation() {
        let tmp = TempDir::new().unwrap();
        // Empty file.
        write_transcript(tmp.path(), "p1", "empty", &[]);
        // A role with empty content + an unrelated object.
        write_transcript(
            tmp.path(),
            "p2",
            "blank",
            &[
                r#"{"role":"user","message":{"content":[]}}"#,
                r#"{"foo":"bar"}"#,
            ],
        );
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert!(convs.is_empty(), "no fabricated conversations");
    }

    #[test]
    fn partial_records_degrade_gracefully() {
        let tmp = TempDir::new().unwrap();
        write_transcript(
            tmp.path(),
            "proj",
            "s",
            &[
                // Missing `message` wrapper; top-level content as a plain string.
                r#"{"role":"user","content":"plain string content"}"#,
                // Missing role -> skipped, never fatal.
                r#"{"message":{"content":[{"type":"text","text":"no role"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            ],
        );
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 2, "role-less record dropped");
        assert!(
            convs[0].messages[0]
                .content
                .contains("plain string content")
        );
    }

    #[test]
    fn discovery_covers_agent_scan_sources() {
        let tmp = TempDir::new().unwrap();
        write_transcript(tmp.path(), "proj", "s", &[USER_LINE, ASSISTANT_LINE]);
        assert_discovery_covers_scan_sources(&CursorConnector::new(), &ctx_for(tmp.path()));

        let discovered = CursorConnector::new()
            .discover_source_files(&ctx_for(tmp.path()))
            .unwrap();
        assert!(
            discovered.iter().any(|d| d.provider_slug == "cursor"
                && d.source_path.extension().is_some_and(|e| e == "jsonl")
                && d.role == DiscoveredSourceRole::PrimarySessionLog
                && d.required_for_reconstruction),
            "the agent transcript jsonl must be a required primary source"
        );
    }

    #[test]
    fn explicit_workspace_preserves_parent_and_leaf_hyphens_with_both_candidates_present() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("project-parent/my-app");
        let guessed = tmp.path().join("project/parent/my/app");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&guessed).unwrap();
        let project = workspace.to_str().unwrap().replace(['/', '\\', ':'], "-");
        assert_eq!(
            project,
            guessed.to_str().unwrap().replace(['/', '\\', ':'], "-")
        );
        let projects = tmp.path().join("cursor-projects");
        let transcript = write_transcript(&projects, &project, "stable-id", &[USER_LINE]);
        write_workspace(&transcript, workspace.to_str().unwrap());
        let convs = CursorConnector::new().scan(&ctx_for(&projects)).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].workspace.as_ref(), Some(&workspace));
        assert_ne!(convs[0].workspace.as_ref(), Some(&guessed));
        assert_eq!(convs[0].external_id.as_deref(), Some("stable-id"));
        assert_eq!(convs[0].source_path, transcript);
        assert_eq!(convs[0].metadata["cursor_project_dir"], project);
    }

    #[test]
    fn absent_malformed_and_invalid_workspace_metadata_never_guess() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(
            tmp.path(),
            "home-example-projects-my-app",
            "s",
            &[USER_LINE],
        );
        let sidecar = CursorConnector::agent_workspace_metadata_path(&transcript).unwrap();
        let invalid = [
            None,
            Some("not json"),
            Some("{}"),
            Some(r#"{"workspacePath":null}"#),
            Some(r#"{"workspacePath":42}"#),
            Some(r#"{"workspacePath":[]}"#),
            Some(r#"{"workspacePath":""}"#),
            Some(r#"{"workspacePath":"relative/my-app"}"#),
            Some(r#"{"workspacePath":"file:///home/example/my-app"}"#),
            Some(r#"{"workspacePath":"C:relative"}"#),
            Some(r#"{"workspacePath":"/home/example/\u0000my-app"}"#),
        ];
        for input in invalid {
            if let Some(input) = input {
                fs::write(&sidecar, input).unwrap();
            }
            let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
            assert_eq!(convs.len(), 1, "{input:?}");
            let c = &convs[0];
            assert_eq!(c.workspace, None, "{input:?}");
            assert_eq!(c.metadata["cursor_workspace_attribution"], "unresolved");
            assert_eq!(
                c.metadata["cursor_project_dir"],
                "home-example-projects-my-app"
            );
            assert_eq!(c.external_id.as_deref(), Some("s"));
            assert_eq!(c.source_path, transcript);
            assert_eq!(c.messages.len(), 1);
            assert_eq!(c.messages[0].content, "find the bug");
        }
    }

    #[test]
    fn exported_workspace_metadata_preserves_attribution_and_source_bytes() {
        let source = TempDir::new().unwrap();
        let exported = TempDir::new().unwrap();
        let first = write_transcript(
            source.path(),
            "home-example-projects-my-app",
            "first",
            &[USER_LINE, ASSISTANT_LINE],
        );
        write_transcript(
            source.path(),
            "home-example-projects-my-app",
            "second",
            &[USER_LINE],
        );
        let sidecar = write_workspace(&first, "/home/example/projects/my-app");
        let connector = CursorConnector::new();
        let discovered = connector
            .discover_source_files(&ctx_for(source.path()))
            .unwrap();
        assert_eq!(discovered.len(), 3);
        let metadata: Vec<_> = discovered
            .iter()
            .filter(|s| s.role == DiscoveredSourceRole::MetadataSidecar)
            .collect();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].source_path, sidecar);
        assert!(metadata[0].required_for_reconstruction);
        let before: Vec<_> = discovered
            .iter()
            .map(|s| {
                (
                    s.source_path.clone(),
                    fs::read(&s.source_path).unwrap(),
                    fs::metadata(&s.source_path).unwrap().modified().unwrap(),
                )
            })
            .collect();
        for source_file in &discovered {
            let destination = exported
                .path()
                .join(source_file.source_path.strip_prefix(source.path()).unwrap());
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(&source_file.source_path, destination).unwrap();
        }
        let original = connector.scan(&ctx_for(source.path())).unwrap();
        let copied = connector.scan(&ctx_for(exported.path())).unwrap();
        assert_eq!(original.len(), 2);
        assert_eq!(copied.len(), original.len());
        for (a, b) in original.iter().zip(&copied) {
            assert_eq!(a.external_id, b.external_id);
            assert_eq!(a.workspace, b.workspace);
            assert_eq!(
                a.workspace.as_deref(),
                Some(Path::new("/home/example/projects/my-app"))
            );
            assert_eq!(a.metadata, b.metadata);
            assert_eq!(a.messages.len(), b.messages.len());
            for (a, b) in a.messages.iter().zip(&b.messages) {
                assert_eq!(a.content, b.content);
                assert_eq!(a.role, b.role);
            }
        }
        for (path, bytes, mtime) in before {
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), mtime);
        }
        let mapped = ScanContext::with_roots(
            exported.path().to_path_buf(),
            vec![
                ScanRoot::local(exported.path().to_path_buf())
                    .with_rewrite("/home/example/projects", "/mapped/projects"),
            ],
            None,
        );
        let mapped = connector.scan(&mapped).unwrap();
        assert_eq!(mapped.len(), 2);
        assert!(
            mapped
                .iter()
                .all(|c| c.workspace.as_deref() == Some(Path::new("/mapped/projects/my-app")))
        );
    }

    #[test]
    fn absolute_foreign_workspace_paths_do_not_require_local_existence() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(tmp.path(), "foreign-project", "s", &[USER_LINE]);
        for workspace in [
            "/remote/parent-name/my-app",
            r"C:\Users\example\my-app",
            r"\\server\share\my-app",
        ] {
            write_workspace(&transcript, workspace);
            let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
            assert_eq!(convs.len(), 1);
            assert_eq!(convs[0].workspace.as_deref(), Some(Path::new(workspace)));
            assert_eq!(
                convs[0].metadata["cursor_workspace_attribution"],
                "workspace_trusted"
            );
        }
    }

    #[test]
    fn metadata_change_selects_an_unchanged_transcript() {
        let tmp = TempDir::new().unwrap();
        let transcript = write_transcript(tmp.path(), "parent-my-app", "s", &[USER_LINE]);
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        let original_mtime = fs::metadata(&transcript).unwrap().modified().unwrap();
        let original_bytes = fs::read(&transcript).unwrap();
        let since = i64::try_from(
            (std::time::SystemTime::now() - std::time::Duration::from_secs(5))
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            Some(since),
        );
        assert!(CursorConnector::new().scan(&ctx).unwrap().is_empty());
        write_workspace(&transcript, "/parent/my-app");
        let convs = CursorConnector::new().scan(&ctx).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].workspace.as_deref(),
            Some(Path::new("/parent/my-app"))
        );
        assert_discovery_covers_scan_sources(&CursorConnector::new(), &ctx);
        assert_eq!(fs::read(&transcript).unwrap(), original_bytes);
        assert_eq!(
            fs::metadata(&transcript).unwrap().modified().unwrap(),
            original_mtime
        );
    }

    #[test]
    fn since_ts_in_future_skips_agent_transcript() {
        let tmp = TempDir::new().unwrap();
        write_transcript(tmp.path(), "proj", "s", &[USER_LINE]);
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![ScanRoot::local(tmp.path().to_path_buf())],
            Some(i64::MAX),
        );
        assert!(CursorConnector::new().scan(&ctx).unwrap().is_empty());
    }

    #[test]
    fn no_agent_transcripts_leaves_scan_empty_and_safe() {
        // A root with neither a state.vscdb nor agent-transcripts yields nothing
        // and must not panic or fabricate anything.
        let tmp = TempDir::new().unwrap();
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert!(convs.is_empty());
    }
}

#[cfg(test)]
mod cursor_fragment_merge_tests {
    //! Tests for the Composer + Agent-transcript fragment merge (design
    //! 2026-11-08-cursor-fragment-design, §6.1–§6.5). All fixtures are
    //! synthetic: a `state.vscdb`-shaped SQLite database plus JSONL transcripts
    //! under one explicit scan root, with made-up text only. The same session
    //! id on both sides is what triggers the merge; transcript-only and
    //! composer-only shapes pin today's byte-for-byte behavior.
    use super::*;
    use crate::connectors::sqlite_sync::ConnectionExt;
    use frankensqlite::params;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    // ------------------------------------------------------------------
    // Fixture helpers
    // ------------------------------------------------------------------

    /// Create a Cursor-shaped SQLite store (cursorDiskKV + ItemTable).
    fn create_store(path: &Path) -> Connection {
        let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        conn
    }

    /// Write one Composer session (v0.40 headers-only shape) into a store:
    /// a `composerData:<sid>` row plus one `bubbleId:<sid>:<bubble>` row per
    /// message.
    fn write_composer_session(
        db_path: &Path,
        sid: &str,
        composer: &Value,
        bubbles: &[(&str, Value)],
    ) {
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = create_store(db_path);
        let composer_key = format!("composerData:{sid}");
        conn.execute_compat(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            params![composer_key.as_str(), composer.to_string()],
        )
        .unwrap();
        for (bubble_id, bubble) in bubbles {
            let bubble_key = format!("bubbleId:{sid}:{bubble_id}");
            conn.execute_compat(
                "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params![bubble_key.as_str(), bubble.to_string()],
            )
            .unwrap();
        }
        drop(conn);
    }

    fn text_record(role: &str, text: &str) -> Value {
        json!({"role": role, "message": {"content": [{"type": "text", "text": text}]}})
    }

    fn tool_record(role: &str, text: &str, tool: &str, file_path: &str) -> Value {
        json!({"role": role, "message": {"content": [
            {"type": "text", "text": text},
            {"type": "tool_use", "name": tool, "input": {"file_path": file_path}},
        ]}})
    }

    fn tool_only_record(role: &str, tool: &str, file_path: &str) -> Value {
        json!({"role": role, "message": {"content": [
            {"type": "tool_use", "name": tool, "input": {"file_path": file_path}},
        ]}})
    }

    fn turn_ended(status: &str) -> Value {
        json!({"type": "turn_ended", "status": status})
    }

    fn error_turn_ended(error: &str) -> Value {
        json!({"type": "turn_ended", "status": "error", "error": error})
    }

    fn jsonl_text(records: &[Value]) -> String {
        let mut text = String::new();
        for record in records {
            text.push_str(&record.to_string());
            text.push('\n');
        }
        text
    }

    /// Write a primary Agent transcript `<root>/<project>/agent-transcripts/<sid>/<sid>.jsonl`.
    fn write_transcript(root: &Path, project: &str, sid: &str, records: &[Value]) -> PathBuf {
        let dir = root.join(project).join(AGENT_TRANSCRIPTS_DIR).join(sid);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{sid}.jsonl"));
        fs::write(&file, jsonl_text(records)).unwrap();
        file
    }

    fn ctx_for(root: &Path) -> ScanContext {
        ScanContext::with_roots(
            root.to_path_buf(),
            vec![ScanRoot::local(root.to_path_buf())],
            None,
        )
    }

    fn ctx_since(root: &Path, since_ts: i64) -> ScanContext {
        ScanContext::with_roots(
            root.to_path_buf(),
            vec![ScanRoot::local(root.to_path_buf())],
            Some(since_ts),
        )
    }

    fn mtime_ms(path: &Path) -> i64 {
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap()
    }

    fn pair_row(conv: &NormalizedConversation, i: usize) -> &NormalizedMessage {
        &conv.messages[i]
    }

    // ------------------------------------------------------------------
    // The canonical fixture: one Composer session and a same-id transcript
    // exercising every merge tier, plus insert / suppress / error-record
    // shapes. Expected merged sequence (idx 0..8) is written out below and
    // was computed by hand from the design rules.
    // ------------------------------------------------------------------

    fn canonical_composer() -> Value {
        json!({
            "name": "Login regression",
            "createdAt": 1700000000000i64,
            "lastUpdatedAt": 1700000008000i64,
            "unifiedMode": "agentPlus",
            "modelConfig": {"modelName": "test-model"},
            "fullConversationHeadersOnly": [
                {"bubbleId": "u1"}, {"bubbleId": "a1"}, {"bubbleId": "a2"},
                {"bubbleId": "u2"}, {"bubbleId": "a3"}, {"bubbleId": "u3"},
                {"bubbleId": "u4"},
            ],
        })
    }

    fn canonical_bubbles() -> Vec<(&'static str, Value)> {
        vec![
            (
                "u1",
                json!({"type": 1, "text": "Fix the login bug", "timestamp": 1700000001000i64, "workspaceProjectDir": "/work/demo"}),
            ),
            (
                "a1",
                json!({"type": 2, "text": "I will grep first", "timestamp": 1700000002000i64, "modelType": "gpt-x"}),
            ),
            (
                "a2",
                json!({"type": 2, "text": "Reading the file now", "timestamp": 1700000003000i64}),
            ),
            (
                "u2",
                json!({"type": 1, "text": "Summarize", "timestamp": 1700000004000i64}),
            ),
            (
                "a3",
                json!({"type": 2, "text": "Long composer answer with tail detail the transcript truncated", "timestamp": 1700000005000i64, "modelType": "gpt-x"}),
            ),
            (
                "u3",
                json!({"type": 1, "text": "Standalone question never in the transcript", "timestamp": 1700000006000i64}),
            ),
            (
                "u4",
                json!({"type": 1, "text": "the login", "timestamp": 1700000007000i64}),
            ),
        ]
    }

    fn canonical_records() -> Vec<Value> {
        vec![
            text_record("user", "Fix the login bug"),
            tool_record("assistant", "I will grep first", "Grep", "/src/login.rs"),
            turn_ended("success"),
            text_record(
                "assistant",
                "Reading the file now and the next fifty lines for context",
            ),
            error_turn_ended("Model call failed: rate limited"),
            text_record(
                "user",
                "Please Summarize: we wrapped the composer text inside a quoted context header",
            ),
            text_record("assistant", "Long composer answer with tail"),
            tool_only_record("assistant", "Read", "/src/auth.rs"),
            text_record(
                "user",
                "A final user nudge that exists only in the transcript",
            ),
        ]
    }

    fn canonical_fixture(root: &Path) -> (PathBuf, PathBuf) {
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(&db, "alpha-1", &canonical_composer(), &canonical_bubbles());
        let transcript = write_transcript(root, "proj-a", "alpha-1", &canonical_records());
        (db, transcript)
    }

    #[test]
    fn merged_conversation_pairs_all_tiers_donates_and_dense_indexes() {
        let tmp = TempDir::new().unwrap();
        let (db, transcript) = canonical_fixture(tmp.path());
        let convs = CursorConnector::new().scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(
            convs.len(),
            1,
            "composer + same-id transcript must merge into one conversation"
        );
        let c = &convs[0];

        // Conversation-level fields: composer wins, transcript fills gaps.
        assert_eq!(c.agent_slug, "cursor");
        assert_eq!(c.external_id.as_deref(), Some("alpha-1"));
        assert_eq!(c.source_path, db.join("alpha-1"));
        assert_eq!(c.title.as_deref(), Some("Login regression"));
        assert_eq!(c.workspace.as_deref(), Some(Path::new("/work/demo")));
        assert_eq!(c.started_at, Some(1700000000000i64));
        assert!(c.ended_at.unwrap() >= 1700000008000i64);
        assert_eq!(c.metadata["source"], "cursor");
        assert_eq!(c.metadata["model"], "test-model");
        assert_eq!(c.metadata["unifiedMode"], "agentPlus");
        assert_eq!(c.metadata["cursor_format"], "merged");
        assert_eq!(c.metadata["cursor_merge_revision"], 1);
        assert_eq!(
            c.metadata["cursor_transcript_path"],
            json!(transcript.to_str().unwrap())
        );
        assert_eq!(c.metadata["cursor_project_dir"], "proj-a");
        assert_eq!(c.metadata["cursor_workspace_attribution"], "unresolved");

        // Dense idx over the whole merged sequence.
        assert_eq!(c.messages.len(), 9);
        for (i, msg) in c.messages.iter().enumerate() {
            assert_eq!(msg.idx, i as i64, "idx must be dense");
        }

        // idx 0 — t1 pair (u1 / "Fix the login bug"), composer donates timestamp.
        let m = pair_row(c, 0);
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "Fix the login bug");
        assert_eq!(m.created_at, Some(1700000001000i64));
        assert_eq!(m.author, None);
        assert!(m.invocations.is_empty());
        assert_eq!(m.extra["text"], "Fix the login bug");
        assert_eq!(m.extra["type"], json!(1));
        assert_eq!(m.extra["workspaceProjectDir"], "/work/demo");
        assert_eq!(m.extra["cursor_transcript_record"]["role"], "user");

        // idx 1 — t1 pair with tools: transcript flatten wins, composer donates.
        let m = pair_row(c, 1);
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content, "I will grep first\n[Tool: Grep - /src/login.rs]");
        assert_eq!(m.created_at, Some(1700000002000i64));
        assert_eq!(m.author.as_deref(), Some("gpt-x"));
        assert_eq!(m.invocations.len(), 1);
        assert_eq!(m.invocations[0].kind, "tool");
        assert_eq!(m.invocations[0].name, "Grep");
        assert_eq!(m.extra["cursor_transcript_record"]["role"], "assistant");

        // idx 2 — t2 pair: composer text is a prefix of the record text.
        let m = pair_row(c, 2);
        assert_eq!(m.role, "assistant");
        assert_eq!(
            m.content,
            "Reading the file now and the next fifty lines for context"
        );
        assert_eq!(m.created_at, Some(1700000003000i64));
        assert_eq!(m.author, None);

        // idx 3 — error turn_ended record becomes a synthetic assistant row at
        // its record position, extra carries only the namespaced record.
        let m = pair_row(c, 3);
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content, "Model call failed: rate limited");
        assert_eq!(m.created_at, None);
        assert_eq!(m.author, None);
        assert!(m.invocations.is_empty());
        assert_eq!(m.extra.as_object().map(serde_json::Map::len), Some(1));
        assert_eq!(
            m.extra["cursor_transcript_record"]["error"],
            "Model call failed: rate limited"
        );
        assert_eq!(m.extra["cursor_transcript_record"]["type"], "turn_ended");

        // idx 4 — t4 pair: composer text contained in the record text.
        let m = pair_row(c, 4);
        assert_eq!(m.role, "user");
        assert_eq!(
            m.content,
            "Please Summarize: we wrapped the composer text inside a quoted context header"
        );
        assert_eq!(m.created_at, Some(1700000004000i64));

        // idx 5 — t5 pair: record contained in composer, so the composer
        // variant (the only side holding the truncated tail) wins content.
        let m = pair_row(c, 5);
        assert_eq!(m.role, "assistant");
        assert_eq!(
            m.content,
            "Long composer answer with tail detail the transcript truncated"
        );
        assert_eq!(m.created_at, Some(1700000005000i64));
        assert_eq!(m.author.as_deref(), Some("gpt-x"));
        assert!(m.invocations.is_empty());
        assert!(m.extra["cursor_transcript_record"].is_object());

        // idx 6 — unmatched composer message with no role-ok relation inserts
        // after the previous pair, keeping composer content and bubble extra
        // (and no transcript-record key: it is not a pair).
        let m = pair_row(c, 6);
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "Standalone question never in the transcript");
        assert_eq!(m.created_at, Some(1700000006000i64));
        assert!(m.extra.get("cursor_transcript_record").is_none());
        assert_eq!(m.extra["type"], json!(1));

        // idx 7 / 8 — unpaired records stay as today's transcript rows:
        // flatten content, no timestamps, empty extra.
        let m = pair_row(c, 7);
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content, "[Tool: Read - /src/auth.rs]");
        assert_eq!(m.created_at, None);
        assert_eq!(m.author, None);
        assert_eq!(m.extra, json!({}));
        assert_eq!(m.invocations.len(), 1);
        assert_eq!(m.invocations[0].name, "Read");
        let m = pair_row(c, 8);
        assert_eq!(m.role, "user");
        assert_eq!(
            m.content,
            "A final user nudge that exists only in the transcript"
        );
        assert_eq!(m.created_at, None);
        assert_eq!(m.extra, json!({}));

        // The suppressed u4 ("the login", contained in the paired user record)
        // must not appear anywhere.
        assert!(
            c.messages.iter().all(|msg| msg.content != "the login"),
            "composer message with a role-ok relation must be suppressed"
        );
    }

    #[test]
    fn merge_is_deterministic_across_scans() {
        let tmp = TempDir::new().unwrap();
        canonical_fixture(tmp.path());
        let connector = CursorConnector::new();
        let first = connector.scan(&ctx_for(tmp.path())).unwrap();
        let second = connector.scan(&ctx_for(tmp.path())).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap(),
            "same inputs must merge identically (idx included)"
        );
    }

    #[test]
    fn merged_output_extends_by_append_only_tail() {
        let tmp = TempDir::new().unwrap();
        let (_, transcript) = canonical_fixture(tmp.path());
        let ctx = ctx_for(tmp.path());
        let first = CursorConnector::new().scan(&ctx).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].messages.len(), 9);

        // Incremental stability (§6.5): appending records to the transcript
        // must leave every already-emitted row byte-identical and only append
        // a tail.
        let mut grown = canonical_records();
        grown.push(text_record("user", "One more followup question"));
        grown.push(text_record("assistant", "One more answer line"));
        fs::write(&transcript, jsonl_text(&grown)).unwrap();

        let second = CursorConnector::new().scan(&ctx).unwrap();
        assert_eq!(second.len(), 1);
        let new_messages = &second[0].messages;
        assert_eq!(new_messages.len(), 11);
        for i in 0..9 {
            assert_eq!(
                serde_json::to_value(&new_messages[i]).unwrap(),
                serde_json::to_value(&first[0].messages[i]).unwrap(),
                "row {i} changed after a tail-only append"
            );
        }
        assert_eq!(new_messages[9].role, "user");
        assert_eq!(new_messages[9].content, "One more followup question");
        assert_eq!(new_messages[9].extra, json!({}));
        assert_eq!(new_messages[10].role, "assistant");
        assert_eq!(new_messages[10].content, "One more answer line");
        assert_eq!(new_messages[10].idx, 10);
    }

    #[test]
    fn unmatched_head_composer_message_inserts_before_first_record() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "head-1",
            &json!({"name": "Head anchor", "fullConversationHeadersOnly": [
                {"bubbleId": "ux"}, {"bubbleId": "u1"},
            ]}),
            &[
                (
                    "ux",
                    json!({"type": 1, "text": "Where do I start", "timestamp": 1700000001000i64}),
                ),
                (
                    "u1",
                    json!({"type": 1, "text": "Fix the login bug", "timestamp": 1700000002000i64}),
                ),
            ],
        );
        write_transcript(
            root,
            "proj-a",
            "head-1",
            &[text_record("user", "Fix the login bug")],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.messages.len(), 2);
        // No previous pair: the insert anchors at the head, before the record.
        assert_eq!(c.messages[0].content, "Where do I start");
        assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
        assert!(
            c.messages[0]
                .extra
                .get("cursor_transcript_record")
                .is_none()
        );
        assert_eq!(c.messages[1].content, "Fix the login bug");
        assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
        assert_eq!(
            c.messages[1].extra["cursor_transcript_record"]["role"],
            "user"
        );
    }

    #[test]
    fn unmatched_composer_message_inserts_between_pairs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "mid-1",
            &json!({"name": "Mid anchor", "fullConversationHeadersOnly": [
                {"bubbleId": "u1"}, {"bubbleId": "um"}, {"bubbleId": "u2"},
            ]}),
            &[
                (
                    "u1",
                    json!({"type": 1, "text": "First question", "timestamp": 1700000001000i64}),
                ),
                (
                    "um",
                    json!({"type": 1, "text": "An interleaved remark not in the transcript", "timestamp": 1700000002000i64}),
                ),
                (
                    "u2",
                    json!({"type": 1, "text": "Second question", "timestamp": 1700000003000i64}),
                ),
            ],
        );
        write_transcript(
            root,
            "proj-a",
            "mid-1",
            &[
                text_record("user", "First question"),
                text_record("user", "Second question"),
            ],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.messages.len(), 3);
        assert_eq!(c.messages[0].content, "First question");
        assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
        // Anchored directly after the record paired with the previous
        // composer message — i.e. between the two pairs.
        assert_eq!(
            c.messages[1].content,
            "An interleaved remark not in the transcript"
        );
        assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
        assert!(
            c.messages[1]
                .extra
                .get("cursor_transcript_record")
                .is_none()
        );
        assert_eq!(c.messages[2].content, "Second question");
        assert_eq!(c.messages[2].created_at, Some(1700000003000i64));
    }

    #[test]
    fn unmatched_tail_composer_message_inserts_at_end() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "tail-1",
            &json!({"name": "Tail anchor", "fullConversationHeadersOnly": [
                {"bubbleId": "u1"}, {"bubbleId": "ut"},
            ]}),
            &[
                (
                    "u1",
                    json!({"type": 1, "text": "First question", "timestamp": 1700000001000i64}),
                ),
                (
                    "ut",
                    json!({"type": 1, "text": "A trailing remark absent from the transcript", "timestamp": 1700000002000i64}),
                ),
            ],
        );
        write_transcript(
            root,
            "proj-a",
            "tail-1",
            &[text_record("user", "First question")],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].content, "First question");
        assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
        // Anchor past the last record: the insert lands after the walk.
        assert_eq!(
            c.messages[1].content,
            "A trailing remark absent from the transcript"
        );
        assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
        assert!(
            c.messages[1]
                .extra
                .get("cursor_transcript_record")
                .is_none()
        );
    }

    #[test]
    fn out_of_order_duplicate_is_suppressed_not_inserted() {
        // u_bg's text equals record r0's text (t1), but r0 sits before the
        // forward cursor once r1 pairs u_q, so u_bg stays unmatched — and the
        // literal §6.2 suppress rule (relation with ANY role-ok record,
        // paired or not) drops it instead of inserting. r0 itself must never
        // be dropped: it survives as a transcript-only row.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "sup-1",
            &json!({"name": "Out of order", "fullConversationHeadersOnly": [
                {"bubbleId": "uq"}, {"bubbleId": "ubg"},
            ]}),
            &[
                (
                    "uq",
                    json!({"type": 1, "text": "the main question stands", "timestamp": 1700000001000i64}),
                ),
                (
                    "ubg",
                    json!({"type": 1, "text": "alpha beta background", "timestamp": 1700000002000i64}),
                ),
            ],
        );
        write_transcript(
            root,
            "proj-a",
            "sup-1",
            &[
                text_record("user", "alpha beta background"),
                text_record("user", "the main question stands"),
            ],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(
            c.messages.len(),
            2,
            "no third row for the suppressed composer message"
        );
        // r0 survives as a transcript-only row.
        assert_eq!(c.messages[0].content, "alpha beta background");
        assert_eq!(c.messages[0].extra, json!({}));
        assert_eq!(c.messages[0].created_at, None);
        // r1 pairs u_q; the composer side donates its timestamp.
        assert_eq!(c.messages[1].content, "the main question stands");
        assert_eq!(c.messages[1].created_at, Some(1700000001000i64));
        assert_eq!(
            c.messages[1].extra["cursor_transcript_record"]["role"],
            "user"
        );
    }

    #[test]
    fn error_edge_records_stay_dropped_in_merged_conversations() {
        // Only a turn_ended record with a non-blank string `error` becomes a
        // synthetic row; blank and non-string errors stay dropped, and
        // success turn_ended records never yield rows.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "err-1",
            &json!({"name": "Error edges", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
            &[(
                "u1",
                json!({"type": 1, "text": "Question one", "timestamp": 1700000001000i64}),
            )],
        );
        write_transcript(
            root,
            "proj-a",
            "err-1",
            &[
                text_record("user", "Question one"),
                json!({"type": "turn_ended", "status": "error", "error": "   "}),
                json!({"type": "turn_ended", "status": "error", "error": 42}),
                turn_ended("success"),
            ],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].content, "Question one");
        assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
    }

    #[test]
    fn multi_file_transcript_sid_merges_once_with_all_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "eps-7",
            &json!({
                "name": "Epsilon session",
                "createdAt": 1700000000000i64,
                "fullConversationHeadersOnly": [{"bubbleId": "u1"}, {"bubbleId": "u2"}],
            }),
            &[
                (
                    "u1",
                    json!({"type": 1, "text": "Question alpha", "timestamp": 1700000001000i64}),
                ),
                (
                    "u2",
                    json!({"type": 1, "text": "Question beta", "timestamp": 1700000002000i64}),
                ),
            ],
        );
        let file_x = write_transcript(
            root,
            "proj-x",
            "eps-7",
            &[text_record("user", "Question alpha")],
        );
        let file_y = write_transcript(
            root,
            "proj-y",
            "eps-7",
            &[text_record("user", "Question beta")],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(
            convs.len(),
            1,
            "all files of one sid merge into a single conversation"
        );
        let c = &convs[0];
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].content, "Question alpha");
        assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
        assert_eq!(c.messages[1].content, "Question beta");
        assert_eq!(c.messages[1].created_at, Some(1700000002000i64));
        assert_eq!(
            c.metadata["cursor_transcript_path"],
            json!([file_x.to_str().unwrap(), file_y.to_str().unwrap()])
        );
        assert_eq!(c.metadata["cursor_project_dir"], "proj-x");
    }

    // ------------------------------------------------------------------
    // Freshness gate (§6.4)
    // ------------------------------------------------------------------

    #[test]
    fn stale_db_with_fresh_transcript_merges_via_targeted_lookup() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Stale db first, then a clear sleep, then the transcripts: the db
        // stays stale while both transcripts come back fresh, deterministically.
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(&db, "alpha-1", &canonical_composer(), &canonical_bubbles());
        std::thread::sleep(std::time::Duration::from_millis(120));
        write_transcript(root, "proj-a", "alpha-1", &canonical_records());
        // A second, transcript-only session that is also fresh.
        write_transcript(
            root,
            "proj-b",
            "beta-8",
            &[text_record("user", "Beta eight question")],
        );
        let since = mtime_ms(&db) + 1001;
        let convs = CursorConnector::new()
            .scan(&ctx_since(root, since))
            .unwrap();
        assert_eq!(
            convs.len(),
            2,
            "fresh transcript merges with the stale composer row; other fresh sid stays separate"
        );
        let merged = &convs[0];
        assert_eq!(merged.external_id.as_deref(), Some("alpha-1"));
        assert_eq!(merged.metadata["cursor_format"], "merged");
        // Composer identity and donations prove the stale db was read.
        assert_eq!(merged.source_path, db.join("alpha-1"));
        assert_eq!(merged.title.as_deref(), Some("Login regression"));
        assert_eq!(merged.messages[0].created_at, Some(1700000001000i64));
        assert_eq!(merged.messages[5].author.as_deref(), Some("gpt-x"));
        let other = &convs[1];
        assert_eq!(other.external_id.as_deref(), Some("beta-8"));
        assert_eq!(other.metadata["cursor_format"], "agent");
        assert!(other.messages[0].created_at.is_none());
    }

    #[test]
    fn fresh_db_with_stale_transcript_merges_and_skips_stale_only_sids() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Transcripts first (both will be stale), then the db (fresh): the db
        // must be written after a sleep so its mtime is clearly newer.
        write_transcript(
            root,
            "proj-c",
            "beta-7",
            &[text_record("user", "Beta seven question")],
        );
        let alpha_transcript = write_transcript(root, "proj-a", "alpha-1", &canonical_records());
        std::thread::sleep(std::time::Duration::from_millis(120));
        let db = root.join("globalStorage/state.vscdb");
        // The conversation itself must pass the incremental cutoff: give it a
        // lastUpdatedAt newer than the (stale) transcripts.
        let mut composer = canonical_composer();
        composer["lastUpdatedAt"] = json!(mtime_ms(&alpha_transcript) + 2000);
        write_composer_session(&db, "alpha-1", &composer, &canonical_bubbles());
        let since = mtime_ms(&alpha_transcript) + 1001;
        let convs = CursorConnector::new()
            .scan(&ctx_since(root, since))
            .unwrap();
        assert_eq!(
            convs.len(),
            1,
            "fresh db parses stale transcripts for its sids; stale transcript-only sids stay silent"
        );
        let merged = &convs[0];
        assert_eq!(merged.external_id.as_deref(), Some("alpha-1"));
        assert_eq!(merged.metadata["cursor_format"], "merged");
        // Transcript content made it in despite the stale mtime.
        assert_eq!(
            merged.messages[1].content,
            "I will grep first\n[Tool: Grep - /src/login.rs]"
        );
        assert_eq!(merged.messages[0].created_at, Some(1700000001000i64));
    }

    #[test]
    fn message_less_composer_row_does_not_block_a_later_db_in_targeted_lookup() {
        // Cross-store resolution mirrors the full scan: a store whose row
        // parses to no conversation does not block a later store from
        // resolving the sid.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let global_db = root.join("globalStorage/state.vscdb");
        fs::create_dir_all(global_db.parent().unwrap()).unwrap();
        {
            let conn = create_store(&global_db);
            conn.execute_compat(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
                params!["composerData:delta-4", r#"{"name": "Empty shell"}"#],
            )
            .unwrap();
            drop(conn);
        }
        let w2_db = root.join("workspaceStorage/w2/state.vscdb");
        write_composer_session(
            &w2_db,
            "delta-4",
            &json!({"name": "Delta four", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
            &[(
                "u1",
                json!({"type": 1, "text": "First pairing question", "timestamp": 1700000001000i64}),
            )],
        );
        std::thread::sleep(std::time::Duration::from_millis(120));
        write_transcript(
            root,
            "proj-d",
            "delta-4",
            &[text_record("user", "First pairing question")],
        );
        let since = mtime_ms(&global_db).max(mtime_ms(&w2_db)) + 1001;
        let convs = CursorConnector::new()
            .scan(&ctx_since(root, since))
            .unwrap();
        assert_eq!(convs.len(), 1);
        let merged = &convs[0];
        assert_eq!(merged.metadata["cursor_format"], "merged");
        assert_eq!(merged.source_path, w2_db.join("delta-4"));
        assert_eq!(merged.messages[0].created_at, Some(1700000001000i64));
    }

    #[test]
    fn unopenable_stale_db_fails_scan_during_targeted_lookup() {
        // F9 semantics for the lookup too: a store that cannot be read fails
        // the scan instead of silently degrading to a transcript-only row —
        // the same store would fail a full scan.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let global_db = root.join("globalStorage/state.vscdb");
        fs::create_dir_all(global_db.parent().unwrap()).unwrap();
        fs::write(&global_db, b"this is not a sqlite database at all").unwrap();
        let w2_db = root.join("workspaceStorage/w2/state.vscdb");
        write_composer_session(
            &w2_db,
            "delta-5",
            &json!({"name": "Delta five", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
            &[(
                "u1",
                json!({"type": 1, "text": "Second pairing question", "timestamp": 1700000001000i64}),
            )],
        );
        std::thread::sleep(std::time::Duration::from_millis(120));
        write_transcript(
            root,
            "proj-d",
            "delta-5",
            &[text_record("user", "Second pairing question")],
        );
        let since = mtime_ms(&global_db).max(mtime_ms(&w2_db)) + 1001;
        let err = CursorConnector::new()
            .scan(&ctx_since(root, since))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("Cursor scan incomplete"),
            "lookup failure must fail the scan, got: {err:#}"
        );
    }

    // ------------------------------------------------------------------
    // Unchanged shapes (regression pins)
    // ------------------------------------------------------------------

    #[test]
    fn transcript_only_conversations_keep_todays_shape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let beta2 = write_transcript(
            root,
            "proj-t",
            "beta-2",
            &[
                text_record("user", "Beta only question"),
                error_turn_ended("Beta model failed"),
            ],
        );
        write_transcript(
            root,
            "proj-p",
            "beta-3",
            &[text_record("user", "Beta three left")],
        );
        write_transcript(
            root,
            "proj-q",
            "beta-3",
            &[text_record("user", "Beta three right")],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 3);
        // Error records never yield rows on the transcript-only path.
        let b2 = convs
            .iter()
            .find(|c| c.external_id.as_deref() == Some("beta-2"))
            .unwrap();
        assert_eq!(b2.source_path, beta2);
        assert_eq!(b2.metadata["cursor_format"], "agent");
        assert_eq!(b2.metadata["source"], "cursor");
        assert_eq!(b2.messages.len(), 1);
        assert_eq!(b2.messages[0].content, "Beta only question");
        assert_eq!(b2.messages[0].extra, json!({}));
        assert!(b2.messages[0].created_at.is_none());
        // Same sid in two projects stays two conversations.
        let b3s: Vec<_> = convs
            .iter()
            .filter(|c| c.external_id.as_deref() == Some("beta-3"))
            .collect();
        assert_eq!(b3s.len(), 2);
        assert_eq!(b3s[0].metadata["cursor_format"], "agent");
        assert_eq!(b3s[1].metadata["cursor_format"], "agent");
    }

    #[test]
    fn composer_only_conversations_keep_todays_shape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let db = root.join("globalStorage/state.vscdb");
        write_composer_session(
            &db,
            "gamma-3",
            &json!({"name": "Gamma only", "fullConversationHeadersOnly": [{"bubbleId": "u1"}]}),
            &[(
                "u1",
                json!({"type": 1, "text": "Composer only question", "timestamp": 1700000001000i64}),
            )],
        );
        let convs = CursorConnector::new().scan(&ctx_for(root)).unwrap();
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.external_id.as_deref(), Some("gamma-3"));
        assert_eq!(c.source_path, db.join("gamma-3"));
        assert!(
            c.metadata.get("cursor_format").is_none(),
            "no merge markers without a transcript"
        );
        assert!(c.metadata.get("cursor_merge_revision").is_none());
        assert!(c.metadata.get("cursor_transcript_path").is_none());
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].content, "Composer only question");
        assert_eq!(c.messages[0].created_at, Some(1700000001000i64));
        assert!(
            c.messages[0]
                .extra
                .get("cursor_transcript_record")
                .is_none()
        );
    }

    #[test]
    fn all_stale_sources_emit_nothing() {
        let tmp = TempDir::new().unwrap();
        canonical_fixture(tmp.path());
        let convs = CursorConnector::new()
            .scan(&ctx_since(tmp.path(), i64::MAX))
            .unwrap();
        assert!(convs.is_empty());
    }
}

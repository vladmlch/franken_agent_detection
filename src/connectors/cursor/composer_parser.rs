//! Cursor composerData, bubble, and legacy aichat database parsing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankensqlite::compat::RowExt;
use frankensqlite::params;
use serde_json::Value;

use super::CursorConnector;
use super::subagent_evidence::{ComposerDataCell, ComposerSubagentEvidence};
use crate::connectors::sqlite_sync::{
    Connection, ConnectionExt, open_read_only_with_snapshot_fallback,
};
use crate::connectors::utils::unwrap_skill_invocations;
use crate::connectors::{
    extract_invocations_from_content_blocks, flatten_content, parse_timestamp,
};
use crate::types::{NormalizedConversation, NormalizedInvocation, NormalizedMessage};

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
pub(super) const CURSOR_DISK_KV_RANGE_SQL: &str =
    "SELECT key, value FROM cursorDiskKV WHERE key >= ? AND key < ?";

impl CursorConnector {
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
    pub(super) fn parse_workspace_uri(uri: &str) -> Option<PathBuf> {
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
    #[cfg(test)]
    pub(super) fn extract_from_db(
        db_path: &Path,
        since_ts: Option<i64>,
        tick: &dyn Fn(),
    ) -> Result<Vec<NormalizedConversation>> {
        Self::extract_from_db_with_evidence(db_path, since_ts, tick, None)
    }

    pub(super) fn extract_from_db_with_evidence(
        db_path: &Path,
        since_ts: Option<i64>,
        tick: &dyn Fn(),
        external_evidence: Option<&mut ComposerSubagentEvidence>,
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
        //
        // Round-8 N7/N8 ruling: this store is deliberately NOT read inside an
        // explicit transaction — a held deferred read transaction pins a
        // SHARED lock (and on WAL stores the reader horizon) on the user's
        // LIVE store for the whole JSON-decode interval, so a writer's commit
        // can exhaust its busy timeout waiting on the scan. The snapshot
        // guarantee comes from read ORDER instead: the composer rows and all
        // positive/parent evidence are captured first (each statement its own
        // short implicit read), and each composer's own-header flags are read
        // LAST — Cursor commits a subagent's header together with or before
        // its rows, so a header observed after the rows can never be an
        // obsolete false for a subagent whose rows were seen, and a header
        // that turned true later is seen as true. Contradicting evidence
        // committed between the row read and the header read stays
        // unobserved — the accepted B1 limit, no different from a store this
        // scan did not read. Value cells are captured per storage class:
        // TEXT and UTF-8-decodable BLOB carry composer JSON, while a cell FAD
        // cannot interpret marks the store uncertain for false rather than
        // aborting the range (round-8/9 N6).
        match conn.query_map_collect(
            CURSOR_DISK_KV_RANGE_SQL,
            params![composer_prefix, composer_limit.as_str()],
            |row| {
                tick();
                let key: String = row.get_typed(0)?;
                Ok((key, Self::composer_data_cell(row, 1)))
            },
        ) {
            Ok(rows) => {
                // The header records are read inside the evidence build,
                // strictly after the composerData capture above.
                let local_evidence = Self::composer_subagent_evidence(&rows, &conn, db_path, tick);
                // Round-7 N6: the evidence of the exact rows and headers being
                // emitted is merged into any earlier cross-store evidence
                // before resolving verdicts — the earlier pass may have run
                // before these rows were committed, and the current data can
                // still prove a subagent it missed (an own header that says
                // isSubagent=false but carries a parent id is a subagent with
                // that parent, never a main). The merge extends the shared map
                // in place (round-10 O5): positives/parents and store
                // uncertainty captured here stay visible to every later store
                // and to the targeted-lookup pass of this scan.
                let evidence = if let Some(ev) = external_evidence {
                    Self::extend_subagent_evidence(ev, &local_evidence);
                    &*ev
                } else {
                    &local_evidence
                };
                // Own-header records for the false verdict: `local_evidence`
                // .headers is exactly the post-row (late) header map — the
                // only read of this store's headers ran after the row fold
                // above (round-6/8/9 rulings). Cross-store headers merge into
                // `evidence` for positives but never certify a false here.
                let own_headers = &local_evidence.headers;
                for (key, cell) in &rows {
                    let ComposerDataCell::Text(value) = cell else {
                        continue;
                    };
                    // Each composer runs its own bubble range query.
                    tick();
                    if let Some(mut conv) = Self::parse_composer_data(
                        key,
                        value,
                        db_path,
                        since_ts,
                        &mut seen_ids,
                        Some(&conn),
                    ) {
                        if let Some(id) = conv.external_id.clone() {
                            let own_header_false = Self::own_header_certifies_false(
                                &id,
                                own_headers,
                                evidence,
                                db_path,
                            );
                            Self::apply_subagent_verdict(
                                &mut conv,
                                &id,
                                evidence,
                                own_header_false,
                            );
                        }
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
    pub(super) fn parse_composer_data(
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

    fn extract_bubble_invocations(bubble: &Value) -> Vec<NormalizedInvocation> {
        let parse_json_or_clone = |v: &Value| -> Value {
            v.as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| v.clone())
        };
        let mut invocations = Vec::new();

        // 1. Cursor v0.40+ toolFormerData object: {"name": "...", "toolCallId": "...", "rawArgs": "...", "params": "..."}
        if let Some(tf) = bubble.get("toolFormerData").and_then(Value::as_object) {
            if let Some(name) = tf
                .get("name")
                .or_else(|| tf.get("toolName"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let call_id = tf
                    .get("toolCallId")
                    .or_else(|| tf.get("id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let arguments = tf
                    .get("params")
                    .or_else(|| tf.get("rawArgs"))
                    .or_else(|| tf.get("args"))
                    .or_else(|| tf.get("input"))
                    .filter(|v| !v.is_null())
                    .map(parse_json_or_clone);
                invocations.push(NormalizedInvocation {
                    kind: "tool".to_string(),
                    name: name.to_string(),
                    raw_name: None,
                    call_id,
                    arguments,
                });
            }
        }

        // 2. toolCalls / tool_calls array on the bubble
        for key in ["toolCalls", "tool_calls"] {
            if let Some(arr) = bubble.get(key).and_then(Value::as_array) {
                for item in arr {
                    let func = item.get("function");
                    let Some(name) = item
                        .get("name")
                        .or_else(|| item.get("toolName"))
                        .or_else(|| func.and_then(|f| f.get("name")))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let call_id = item
                        .get("id")
                        .or_else(|| item.get("toolCallId"))
                        .or_else(|| item.get("call_id"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    let arguments = item
                        .get("arguments")
                        .or_else(|| item.get("input"))
                        .or_else(|| item.get("params"))
                        .or_else(|| func.and_then(|f| f.get("arguments")))
                        .filter(|v| !v.is_null())
                        .map(parse_json_or_clone);
                    invocations.push(NormalizedInvocation {
                        kind: "tool".to_string(),
                        name: name.to_string(),
                        raw_name: None,
                        call_id,
                        arguments,
                    });
                }
            }
        }

        // 3. Array-typed content blocks
        if let Some(content_blocks) = bubble.get("content").filter(|v| v.is_array()) {
            invocations.extend(extract_invocations_from_content_blocks(content_blocks));
        }

        unwrap_skill_invocations(&mut invocations);
        invocations
    }

    /// Parse a bubble (message) from Cursor's format.
    ///
    /// Handles both new format (v0.40+) and legacy formats:
    /// - Content: text > rawText > content > message
    /// - Role: numeric type (1=user, 2=assistant) or string type/role
    /// - Author: modelType (new) or model (legacy)
    pub(super) fn parse_bubble(bubble: &Value, idx: usize) -> Option<NormalizedMessage> {
        let invocations = Self::extract_bubble_invocations(bubble);

        // Extract content - try all known field names in priority order
        let raw_content = bubble
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| bubble.get("rawText").and_then(|v| v.as_str()))
            .or_else(|| bubble.get("content").and_then(|v| v.as_str()))
            .or_else(|| bubble.get("message").and_then(|v| v.as_str()))
            .map(str::to_string)
            .or_else(|| {
                bubble
                    .get("content")
                    .filter(|v| v.is_array())
                    .map(flatten_content)
            })
            .unwrap_or_default();

        let content = if raw_content.trim().is_empty() {
            if invocations.is_empty() {
                return None;
            }
            invocations
                .iter()
                .map(|inv| format!("[Tool: {}]", inv.name))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            raw_content
        };

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

        let mut extra = bubble.clone();
        if !invocations.is_empty()
            && let Some(obj) = extra.as_object_mut()
        {
            obj.insert(
                "cass".to_string(),
                serde_json::json!({ "tool_call_count": invocations.len() }),
            );
        }

        Some(NormalizedMessage {
            idx: idx as i64,
            role,
            author,
            created_at,
            content,
            extra,
            invocations,
            snippets: Vec::new(),
        })
    }

    /// Normalize role string to standard values (user/assistant).
    pub(super) fn normalize_role(role: &str) -> String {
        match role.to_lowercase().as_str() {
            "user" | "human" => "user",
            "assistant" | "ai" | "bot" => "assistant",
            _ => role,
        }
        .to_string()
    }

    /// Parse legacy aichat data
    pub(super) fn parse_aichat_data(
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

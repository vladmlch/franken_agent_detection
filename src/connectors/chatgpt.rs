//! Connector for ChatGPT desktop app conversation history.
//!
//! ChatGPT stores conversations in:
//! - macOS: ~/Library/Application Support/com.openai.chat/
//!
//! ## Conversation storage versions:
//! - v1 (legacy): Plain JSON files in `conversations-{uuid}/` (unencrypted)
//! - v2/v3: Encrypted files in `conversations-v2-{uuid}/` or `conversations-v3-{uuid}/`
//!
//! ## Encryption Details (v2/v3):
//! ChatGPT desktop encrypts conversations using AES-256-GCM with a key stored in the
//! macOS Keychain under access group `2DC432GLL2.com.openai.shared`.
//!
//! **Important**: The encryption key is protected by Apple's Keychain Access Groups
//! mechanism, which requires the accessing app to be signed with OpenAI's Team ID
//! (2DC432GLL2). This means third-party apps cannot directly access the key.
//!
//! ## Decryption Options:
//! To decrypt v2/v3 conversations, you can:
//! 1. Set the `CHATGPT_ENCRYPTION_KEY` environment variable to the base64-encoded key
//! 2. Create a key file at `~/.config/cass/chatgpt_key.bin` containing the raw 32-byte key
//!
//! The key can potentially be extracted by:
//! - Using Keychain Access.app to export the key (requires user authorization)
//! - Running a helper tool signed with appropriate entitlements
//!
//! ## File Format:
//! Encrypted files appear to use AES-256-GCM with:
//! - 12-byte nonce at the start
//! - Encrypted JSON data
//! - 16-byte authentication tag at the end

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::redundant_closure_for_method_calls,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal
)]

use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result};
use base64::prelude::*;
use serde_json::Value;
use walkdir::WalkDir;

use super::scan::{
    DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks,
};
use super::utils::{excluded_scan_paths_from_env, path_is_excluded, unwrap_skill_invocations};
use super::{
    Connector, extract_invocations_from_content_blocks, file_modified_since,
    franken_detection_for_connector, parse_timestamp,
};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
};

/// Nonce size for AES-GCM (12 bytes)
const NONCE_SIZE: usize = 12;
/// Authentication tag size for AES-GCM (16 bytes)
const TAG_SIZE: usize = 16;
/// AES-256 key size (32 bytes)
const KEY_SIZE: usize = 32;

pub struct ChatGptConnector {
    /// Optional encryption key for v2/v3 conversations
    encryption_key: Option<[u8; KEY_SIZE]>,
}

impl Default for ChatGptConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptConnector {
    pub fn new() -> Self {
        let encryption_key = Self::load_encryption_key();
        if encryption_key.is_some() {
            tracing::info!(
                "chatgpt encryption key loaded, encrypted conversations will be decrypted"
            );
        }
        Self { encryption_key }
    }

    /// Construct with an explicitly supplied AES-256 key, bypassing the
    /// `CHATGPT_ENCRYPTION_KEY` / key-file discovery entirely.
    ///
    /// This is the env-free injection seam (cass qu81y): hosts and tests
    /// that already hold key material pass it directly instead of mutating
    /// process-global environment, which is unsound under parallel test
    /// scheduling and leaks state on panic.
    #[must_use]
    pub fn with_encryption_key(key: [u8; KEY_SIZE]) -> Self {
        Self {
            encryption_key: Some(key),
        }
    }

    /// Load encryption key from environment variable or key file
    fn load_encryption_key() -> Option<[u8; KEY_SIZE]> {
        // Try environment variable first (base64-encoded)
        if let Ok(key_b64) = dotenvy::var("CHATGPT_ENCRYPTION_KEY") {
            if let Ok(key_bytes) = BASE64_STANDARD.decode(key_b64.trim()) {
                if key_bytes.len() == KEY_SIZE {
                    let mut key = [0u8; KEY_SIZE];
                    key.copy_from_slice(&key_bytes);
                    tracing::debug!(
                        "chatgpt encryption key loaded from CHATGPT_ENCRYPTION_KEY env var"
                    );
                    return Some(key);
                }
                tracing::warn!(
                    "CHATGPT_ENCRYPTION_KEY has wrong length: {} (expected {})",
                    key_bytes.len(),
                    KEY_SIZE
                );
            } else {
                tracing::warn!("CHATGPT_ENCRYPTION_KEY is not valid base64");
            }
        }

        // Try key file
        let key_file_paths = [
            dirs::config_dir().map(|p| p.join("cass/chatgpt_key.bin")),
            dirs::home_dir().map(|p| p.join(".config/cass/chatgpt_key.bin")),
            dirs::home_dir().map(|p| p.join(".cass/chatgpt_key.bin")),
        ];

        for path_opt in key_file_paths.iter().flatten() {
            if path_opt.exists() {
                // [coding_agent_session_search-srzwm] Refuse symlinks
                // and refuse mode > 0600. The AES-256 key in this file
                // decrypts the operator's ChatGPT conversation history;
                // an OS-shared dev box where another user has read
                // access to ~/.config/cass/ would expose every
                // ChatGPT conversation. SSH refuses to use a private
                // key with mode > 0600 for this exact reason — apply
                // the same discipline here.
                if !Self::chatgpt_key_file_mode_is_safe(path_opt) {
                    continue;
                }
                match fs::read(path_opt) {
                    Ok(key_bytes) if key_bytes.len() == KEY_SIZE => {
                        let mut key = [0u8; KEY_SIZE];
                        key.copy_from_slice(&key_bytes);
                        tracing::debug!(path = %path_opt.display(), "chatgpt encryption key loaded from file");
                        return Some(key);
                    }
                    Ok(key_bytes) => {
                        tracing::warn!(
                            path = %path_opt.display(),
                            "chatgpt key file has wrong size: {} (expected {})",
                            key_bytes.len(),
                            KEY_SIZE
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_opt.display(),
                            error = %e,
                            "failed to read chatgpt key file"
                        );
                    }
                }
            }
        }

        None
    }

    /// `coding_agent_session_search-srzwm`: gate the chatgpt key file
    /// by the same permission rules SSH applies to private keys.
    /// Refuses symlinks (a symlink to /etc/passwd or /dev/null would
    /// silently load garbage as a "key"), and refuses any mode where
    /// group/other have read/write/exec bits set (mode & 0o077 != 0).
    /// Returns false on Windows / non-Unix where mode bits are not
    /// meaningful in the same way; the caller will then proceed to
    /// fs::read on those platforms (cass desktop encryption is
    /// macOS-only anyway, so this is mostly defensive).
    #[cfg(unix)]
    fn chatgpt_key_file_mode_is_safe(path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let symlink_meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to stat chatgpt key file"
                );
                return false;
            }
        };
        if symlink_meta.file_type().is_symlink() {
            tracing::warn!(
                path = %path.display(),
                "refusing to load chatgpt key file: path is a symlink — \
                 the key file MUST be a regular file owned by the operator"
            );
            return false;
        }
        let mode = symlink_meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:#o}"),
                "refusing to load chatgpt key file: mode {mode:#o} grants \
                 group/other access. Set permissions to 0600 (chmod 600 {})",
                path.display()
            );
            return false;
        }
        true
    }

    #[cfg(not(unix))]
    fn chatgpt_key_file_mode_is_safe(_path: &Path) -> bool {
        // Windows: mode bits don't carry the same meaning. Fall through
        // to the read; ChatGPT desktop encryption is macOS-only
        // currently, so this branch is largely unreachable.
        true
    }

    /// Get the ChatGPT app support directory
    pub fn app_support_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir().map(|h| h.join("Library/Application Support/com.openai.chat"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            // ChatGPT desktop is currently macOS only
            None
        }
    }

    /// Find conversation directories (both encrypted and unencrypted)
    fn find_conversation_dirs(base: &PathBuf) -> Vec<(PathBuf, bool)> {
        let mut dirs = Vec::new();

        if !base.exists() {
            return dirs;
        }

        // 1. Check if base itself is a conversation dir
        if base.is_dir()
            && let Some(name) = base.file_name().and_then(|n| n.to_str())
            && name.starts_with("conversations-")
        {
            let is_encrypted = name.contains("-v2-") || name.contains("-v3-");
            dirs.push((base.clone(), is_encrypted));
            return dirs;
        }

        // 2. Check children (standard behavior)
        for entry in fs::read_dir(base).into_iter().flatten().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Check for conversation directories
            if name.starts_with("conversations-") {
                // v1 (unencrypted) or v2/v3 (encrypted)
                let is_encrypted = name.contains("-v2-") || name.contains("-v3-");
                dirs.push((path, is_encrypted));
            }
        }

        // Keep connector traversal deterministic across filesystems/runs.
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        dirs
    }

    fn looks_like_base(path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }

        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("conversations-"))
        {
            return true;
        }

        fs::read_dir(path)
            .map(|entries| {
                entries.flatten().any(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        name.starts_with("conversations-") && entry.path().is_dir()
                    })
                })
            })
            .unwrap_or(false)
    }

    fn append_explicit_roots(roots: &mut Vec<PathBuf>, base: &Path) {
        if Self::looks_like_base(base) {
            roots.push(base.to_path_buf());
        }

        let candidates = [
            base.join("com.openai.chat"),
            base.join("Library/Application Support/com.openai.chat"),
            base.join("AppData/Roaming/com.openai.chat"),
        ];

        for candidate in candidates {
            if Self::looks_like_base(&candidate) {
                roots.push(candidate);
            }
        }
    }

    fn conversation_files(dir_path: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in WalkDir::new(dir_path).max_depth(1).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str());
            if ext == Some("json") || ext == Some("data") {
                files.push(path.to_path_buf());
            }
        }
        // Keep connector traversal deterministic across filesystems/runs.
        files.sort();
        files
    }

    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if Self::looks_like_base(&ctx.data_dir) {
                vec![ScanRoot::local(ctx.data_dir.clone())]
            } else if let Some(default_base) = Self::app_support_dir() {
                vec![ScanRoot::local(default_base)]
            } else {
                Vec::new()
            }
        } else {
            let mut explicit = Vec::new();
            for root in &ctx.scan_roots {
                let mut candidates = Vec::new();
                Self::append_explicit_roots(&mut candidates, &root.path);
                explicit.extend(candidates.into_iter().map(|path| root.with_path(path)));
            }
            explicit
        };

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    fn discover_sources(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let mut out = Vec::new();
        for root in Self::source_roots(ctx) {
            if !root.path.exists() || path_is_excluded(&root.path, &excluded_paths) {
                continue;
            }
            let conv_dirs = Self::find_conversation_dirs(&root.path);
            for (dir_path, is_encrypted) in conv_dirs {
                if path_is_excluded(&dir_path, &excluded_paths) {
                    continue;
                }
                if is_encrypted && self.encryption_key.is_none() {
                    continue;
                }
                for path in Self::conversation_files(&dir_path) {
                    if path_is_excluded(&path, &excluded_paths)
                        || !file_modified_since(&path, ctx.since_ts)
                    {
                        continue;
                    }
                    out.push(
                        DiscoveredSourceFile::new(
                            "chatgpt",
                            &root,
                            path,
                            DiscoveredSourceRole::PrimarySessionLog,
                            true,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
        Ok(out)
    }

    /// Decrypt an encrypted conversation file
    fn decrypt_file(&self, data: &[u8]) -> Result<Vec<u8>> {
        let key = self.encryption_key.ok_or_else(|| {
            anyhow::anyhow!(
                "No encryption key available. Set CHATGPT_ENCRYPTION_KEY env var or create key file."
            )
        })?;

        if data.len() < NONCE_SIZE + TAG_SIZE {
            anyhow::bail!("Encrypted data too short: {} bytes", data.len());
        }

        // Extract nonce from the beginning. (`Nonce::from_slice` is
        // deprecated in aes-gcm 0.11; TryFrom keeps the length check
        // explicit.)
        let nonce = Nonce::try_from(&data[..NONCE_SIZE])
            .map_err(|_| anyhow::anyhow!("invalid nonce length in encrypted file"))?;

        // The rest is ciphertext + tag
        let ciphertext = &data[NONCE_SIZE..];

        // Create cipher and decrypt
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| anyhow::anyhow!("Failed to create cipher: {}", e))?;

        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

        Ok(plaintext)
    }

    /// Parse a conversation file (JSON or encrypted data format)
    fn parse_conversation_file(
        &self,
        path: &PathBuf,
        _since_ts: Option<i64>,
        is_encrypted: bool,
    ) -> Result<Option<NormalizedConversation>> {
        // Safety check: Don't read files larger than 100MB to avoid OOM
        const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
        if let Ok(metadata) = fs::metadata(path)
            && metadata.len() > MAX_FILE_BYTES
        {
            tracing::warn!(
                path = %path.display(),
                size_bytes = metadata.len(),
                "skipping large file (>100MB)"
            );
            return Ok(None);
        }

        let content_bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        // Post-read backstop: when fs::metadata errored above (ancestor
        // perms, TOCTOU unlink), the pre-read guard was skipped — enforce
        // the cap on the buffer we actually hold so a metadata failure can
        // never turn into an unbounded read + decrypt double-buffer.
        if content_bytes.len() as u64 > MAX_FILE_BYTES {
            tracing::warn!(
                path = %path.display(),
                size_bytes = content_bytes.len(),
                "skipping large file (>100MB, post-read check)"
            );
            return Ok(None);
        }

        // Decrypt if necessary
        let content = if is_encrypted {
            let decrypted = self.decrypt_file(&content_bytes)?;
            String::from_utf8(decrypted).with_context(|| {
                format!(
                    "decrypted content is not valid UTF-8 from {}",
                    path.display()
                )
            })?
        } else {
            String::from_utf8(content_bytes)
                .with_context(|| format!("content is not valid UTF-8 from {}", path.display()))?
        };

        let val: Value = serde_json::from_str(&content)
            .with_context(|| format!("parse JSON from {}", path.display()))?;

        let mut messages = Vec::new();
        let mut started_at: Option<i64> = None;
        let mut ended_at: Option<i64> = None;

        // Extract conversation ID
        let conv_id = val
            .get("id")
            .or_else(|| val.get("conversation_id"))
            .and_then(|v| v.as_str())
            .or_else(|| path.file_stem().and_then(|s| s.to_str()))
            .map(String::from);

        // Extract title
        let title = val.get("title").and_then(|v| v.as_str()).map(String::from);

        // Parse messages from mapping structure (ChatGPT format)
        if let Some(mapping) = val.get("mapping").and_then(|v| v.as_object()) {
            // Collect messages with their parent info for ordering
            let mut msg_nodes: Vec<(Option<String>, String, &Value)> = Vec::new();

            for (node_id, node) in mapping {
                if let Some(msg) = node.get("message") {
                    let parent = node
                        .get("parent")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    msg_nodes.push((parent, node_id.clone(), msg));
                }
            }

            // Simple ordering: sort by create_time if available
            msg_nodes.sort_by(|a, b| {
                let ts_a = a.2.get("create_time").and_then(|v| v.as_f64());
                let ts_b = b.2.get("create_time").and_then(|v| v.as_f64());
                match (ts_a, ts_b) {
                    (Some(a_ts), Some(b_ts)) => a_ts
                        .partial_cmp(&b_ts)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.cmp(&b.1)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.1.cmp(&b.1),
                }
            });

            for (_, _, msg) in msg_nodes {
                // Get role
                let role = msg
                    .get("author")
                    .and_then(|a| a.get("role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");

                // Skip system messages
                if role == "system" {
                    continue;
                }

                // Get content
                let content_val = msg.get("content");
                let raw_content_str = content_val
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                    .map_or_else(
                        || {
                            content_val
                                .and_then(|c| c.get("text"))
                                .and_then(|t| t.as_str())
                                .map(str::to_string)
                                .unwrap_or_default()
                        },
                        |parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        },
                    );

                let invocations = Self::extract_message_invocations(msg, role, &raw_content_str);
                let content_str = if raw_content_str.trim().is_empty() {
                    if invocations.is_empty() {
                        continue;
                    }
                    invocations
                        .iter()
                        .map(|inv| format!("[Tool: {}]", inv.name))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    raw_content_str
                };

                // Get timestamp (ChatGPT uses float seconds)
                let created_at = msg
                    .get("create_time")
                    .and_then(|v| v.as_f64())
                    .map(|ts| (ts * 1000.0).round() as i64);

                // NOTE: Do NOT filter individual messages by timestamp here!
                // The file-level check in file_modified_since() is sufficient.
                // Filtering messages would cause older messages to be lost when
                // the file is re-indexed after new messages are added.

                started_at = match (started_at, created_at) {
                    (Some(curr), Some(ts)) => Some(curr.min(ts)),
                    (None, Some(ts)) => Some(ts),
                    (other, None) => other,
                };
                ended_at = match (ended_at, created_at) {
                    (Some(curr), Some(ts)) => Some(curr.max(ts)),
                    (None, Some(ts)) => Some(ts),
                    (Some(curr), None) => Some(curr),
                    (None, None) => None,
                };

                // Get model info
                let model = msg
                    .get("metadata")
                    .and_then(|m| m.get("model_slug"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let mut extra = msg.clone();
                if !invocations.is_empty()
                    && let Some(obj) = extra.as_object_mut()
                {
                    obj.insert(
                        "cass".to_string(),
                        serde_json::json!({ "tool_call_count": invocations.len() }),
                    );
                }

                messages.push(NormalizedMessage {
                    idx: messages.len() as i64,
                    role: role.to_string(),
                    author: model,
                    created_at,
                    content: content_str,
                    extra,
                    invocations,
                    snippets: Vec::new(),
                });
            }
        }

        // Also try simple messages array format
        if messages.is_empty()
            && let Some(msgs) = val.get("messages").and_then(|v| v.as_array())
        {
            for item in msgs {
                let role = item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");

                if role == "system" {
                    continue;
                }

                let raw_content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let invocations = Self::extract_message_invocations(item, role, raw_content);
                let content = if raw_content.trim().is_empty() {
                    if invocations.is_empty() {
                        continue;
                    }
                    invocations
                        .iter()
                        .map(|inv| format!("[Tool: {}]", inv.name))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    raw_content.to_string()
                };

                let created_at = item
                    .get("timestamp")
                    .or_else(|| item.get("create_time"))
                    .and_then(parse_timestamp);

                // NOTE: Do NOT filter individual messages by timestamp here!
                // File-level check is sufficient for incremental indexing.

                started_at = match (started_at, created_at) {
                    (Some(curr), Some(ts)) => Some(curr.min(ts)),
                    (None, Some(ts)) => Some(ts),
                    (other, None) => other,
                };
                ended_at = match (ended_at, created_at) {
                    (Some(curr), Some(ts)) => Some(curr.max(ts)),
                    (None, Some(ts)) => Some(ts),
                    (Some(curr), None) => Some(curr),
                    (None, None) => None,
                };

                let mut extra = item.clone();
                if !invocations.is_empty()
                    && let Some(obj) = extra.as_object_mut()
                {
                    obj.insert(
                        "cass".to_string(),
                        serde_json::json!({ "tool_call_count": invocations.len() }),
                    );
                }

                messages.push(NormalizedMessage {
                    idx: messages.len() as i64,
                    role: role.to_string(),
                    author: None,
                    created_at,
                    content,
                    extra,
                    invocations,
                    snippets: Vec::new(),
                });
            }
        }

        if messages.is_empty() {
            return Ok(None);
        }

        Ok(Some(NormalizedConversation {
            agent_slug: "chatgpt".to_string(),
            external_id: conv_id,
            title,
            workspace: None, // ChatGPT doesn't have workspace concept
            source_path: path.clone(),
            started_at,
            ended_at,
            metadata: serde_json::json!({
                "source": if is_encrypted { "chatgpt_desktop_encrypted" } else { "chatgpt_desktop" },
                "model": val.get("model").and_then(|v| v.as_str()),
                "encrypted": is_encrypted,
            }),
            messages,
        }))
    }

    fn extract_message_invocations(
        msg: &Value,
        role: &str,
        content_text: &str,
    ) -> Vec<NormalizedInvocation> {
        let parse_json_or_clone = |v: &Value| -> Value {
            v.as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| v.clone())
        };
        let mut invocations = Vec::new();

        // 1. OpenAI-style tool_calls array (top-level or under metadata)
        for ptr in ["/tool_calls", "/metadata/tool_calls"] {
            if let Some(arr) = msg.pointer(ptr).and_then(Value::as_array) {
                for item in arr {
                    let func = item.get("function");
                    let Some(name) = item
                        .get("name")
                        .or_else(|| func.and_then(|f| f.get("name")))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let call_id = item
                        .get("id")
                        .or_else(|| item.get("call_id"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    let arguments = item
                        .get("arguments")
                        .or_else(|| item.get("input"))
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

        // 2. ChatGPT invoked_plugin metadata
        if let Some(plugin) = msg
            .pointer("/metadata/invoked_plugin")
            .and_then(Value::as_object)
            && let Some(namespace) = plugin
                .get("namespace")
                .or_else(|| plugin.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            let call_id = plugin
                .get("plugin_id")
                .or_else(|| plugin.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            invocations.push(NormalizedInvocation {
                kind: "tool".to_string(),
                name: namespace.to_string(),
                raw_name: None,
                call_id,
                arguments: None,
            });
        }

        // 3. ChatGPT assistant messages directed at a tool recipient (e.g. "python", "browser", "dalle.text2im", "web.run", "bio")
        if role == "assistant"
            && let Some(recipient) = msg
                .get("recipient")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|r| !r.is_empty() && *r != "all")
        {
            let arguments = if content_text.trim().is_empty() {
                None
            } else {
                Some(
                    serde_json::from_str::<Value>(content_text.trim())
                        .unwrap_or_else(|_| serde_json::json!({ "input": content_text })),
                )
            };
            invocations.push(NormalizedInvocation {
                kind: "tool".to_string(),
                name: recipient.to_string(),
                raw_name: None,
                call_id: msg
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                arguments,
            });
        }

        // 4. Content block arrays (tool_use)
        if let Some(blocks) = msg.get("content").filter(|v| v.is_array()) {
            invocations.extend(extract_invocations_from_content_blocks(blocks));
        }

        unwrap_skill_invocations(&mut invocations);
        invocations
    }
}

impl Connector for ChatGptConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("chatgpt").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut all_convs = Vec::new();
        self.scan_with_callback(ctx, &mut |conv| {
            all_convs.push(conv);
            Ok(())
        })?;
        Ok(all_convs)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(ctx, &mut SourceScanHooks::default(), on_conversation)
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let roots = Self::source_roots(ctx);

        for root in roots {
            if !root.path.exists() || path_is_excluded(&root.path, &excluded_paths) {
                continue;
            }

            let conv_dirs = Self::find_conversation_dirs(&root.path);

            for (dir_path, is_encrypted) in conv_dirs {
                if path_is_excluded(&dir_path, &excluded_paths) {
                    continue;
                }
                // Skip encrypted directories if we don't have a key
                if is_encrypted && self.encryption_key.is_none() {
                    tracing::debug!(
                        path = %dir_path.display(),
                        "chatgpt skipping encrypted directory (no decryption key)"
                    );
                    continue;
                }

                // Walk through conversation files
                for path in Self::conversation_files(&dir_path) {
                    if path_is_excluded(&path, &excluded_paths)
                        || !file_modified_since(&path, ctx.since_ts)
                    {
                        continue;
                    }

                    let discovered = DiscoveredSourceFile::new(
                        "chatgpt",
                        &root,
                        path.clone(),
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata();
                    if !hooks.should_scan(&discovered) {
                        continue;
                    }
                    ctx.tick();

                    match self.parse_conversation_file(&path, ctx.since_ts, is_encrypted) {
                        Ok(Some(mut conv)) => {
                            root.apply_workspace_rewrite(&mut conv);
                            tracing::debug!(
                                path = %path.display(),
                                messages = conv.messages.len(),
                                encrypted = is_encrypted,
                                "chatgpt extracted conversation"
                            );
                            on_conversation(conv)?;
                            if !discovered.fs_metadata_changed() {
                                hooks.complete(&SourceCompletion {
                                    source: discovered,
                                    required_sidecars: Vec::new(),
                                    conversations_emitted: 1,
                                })?;
                            }
                        }
                        Ok(None) => {
                            tracing::debug!(
                                path = %path.display(),
                                "chatgpt no messages in conversation"
                            );
                        }
                        Err(e) => {
                            if is_encrypted {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "chatgpt failed to decrypt/parse conversation (key might be wrong)"
                                );
                            } else {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "chatgpt failed to parse conversation"
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.discover_sources(ctx)
    }
}

#[cfg(test)]
mod tests;

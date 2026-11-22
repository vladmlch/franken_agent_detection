//! Local coding-agent detection and session-history connectors.
//!
//! Two layers, both synchronous, local and read-only:
//!
//! - **Detection** (always available): [`detect_installed_agents`] runs
//!   filesystem probes for known coding agents and returns one stable,
//!   JSON-serializable report.
//! - **Connectors** (`connectors` feature; SQLite- and crypto-backed stores
//!   behind their own features, all of them via `all-connectors`): each
//!   connector reads one agent's session history into the normalized
//!   conversation model, with source discovery, provenance, streaming and
//!   resumable per-source ingestion. See `connectors::Connector` and
//!   `get_connector_factories`.
//!
//! ## Types
//!
//! The [`types`] module contains normalized types for representing agent conversations:
//! - [`DetectionResult`](types::DetectionResult) — always available
//! - [`NormalizedConversation`], [`NormalizedMessage`], [`NormalizedSnippet`]
//!   — available with the `connectors` feature

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::uninlined_format_args, clippy::redundant_clone))]

#[cfg(feature = "connectors")]
pub mod connectors;
pub mod types;

// Re-export core types at crate root for convenience.
pub use types::DetectionResult;
#[cfg(feature = "connectors")]
pub use types::{
    // Scan & provenance types
    LOCAL_SOURCE_ID,
    NormalizedConversation,
    NormalizedInvocation,
    NormalizedMessage,
    NormalizedSnippet,
    Origin,
    PathMapping,
    Platform,
    SourceKind,
    reindex_messages,
};
// Re-export connector infrastructure at crate root.
#[cfg(feature = "chatgpt")]
pub use connectors::chatgpt::ChatGptConnector;
#[cfg(feature = "cursor")]
pub use connectors::cursor::CursorConnector;
#[cfg(feature = "opencode")]
pub use connectors::opencode::OpenCodeConnector;
#[cfg(feature = "connectors")]
pub use connectors::token_extraction::{
    ExtractedTokenUsage, ModelInfo, TokenDataSource, extract_devin_tokens,
    extract_web_search_requests,
};
#[cfg(feature = "connectors")]
pub use connectors::{
    Connector, DiscoveredSourceFile, DiscoveredSourceRole, PathTrie, ScanContext, ScanProgressTick,
    ScanRoot, SourceCompletion, SourceScanHooks, WorkspaceCache, antigravity::AntigravityConnector,
    claude_code::ClaudeCodeConnector, codex::CodexConnector, devin::DevinConnector,
    estimate_tokens_from_content, extract_claude_code_tokens, extract_codex_tokens,
    extract_invocations_from_content_blocks, extract_tokens_for_agent, file_modified_since,
    flatten_content, franken_detection_for_connector, gemini::GeminiConnector,
    get_connector_factories, normalize_model, parse_timestamp, percent_decode_utf8,
    qoder::QoderConnector, qwen::QwenConnector, token_extraction, unwrap_skill_invocations,
};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct AgentDetectOptions {
    /// Restrict detection to specific connector slugs (e.g. `["codex", "gemini"]`).
    ///
    /// When `None`, all known connectors are evaluated.
    pub only_connectors: Option<Vec<String>>,

    /// When false, omit entries that were not detected.
    pub include_undetected: bool,

    /// Optional per-connector root overrides for deterministic detection (tests/fixtures).
    pub root_overrides: Vec<AgentDetectRootOverride>,
}

#[derive(Debug, Clone)]
pub struct AgentDetectRootOverride {
    pub slug: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionSummary {
    pub detected_count: usize,
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionEntry {
    /// Stable connector/agent identifier (e.g. `codex`, `claude`, `gemini`).
    pub slug: String,
    pub detected: bool,
    pub evidence: Vec<String>,
    pub root_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledAgentDetectionReport {
    pub format_version: u32,
    pub generated_at: String,
    pub installed_agents: Vec<InstalledAgentDetectionEntry>,
    pub summary: InstalledAgentDetectionSummary,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentDetectError {
    #[error("agent detection is disabled (compile with feature `agent-detect`)")]
    FeatureDisabled,

    #[error("unknown connector(s): {connectors:?}")]
    UnknownConnectors { connectors: Vec<String> },
}

const KNOWN_CONNECTORS: &[&str] = &[
    "antigravity",
    "chatgpt",
    "claude",
    "codex",
    "cursor",
    "devin",
    "gemini",
    "opencode",
    "qoder",
    "qwen",
];

/// Resolve any known connector slug, alias, or emitted conversation `agent_slug`
/// to its canonical entry in `KNOWN_CONNECTORS`.
#[must_use]
pub fn canonical_connector_slug(slug: &str) -> Option<&'static str> {
    let trimmed = slug.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "antigravity" | "agy" | "antigravity-cli" | "antigravity-ide" => Some("antigravity"),
        "chatgpt" | "chat-gpt" | "chatgpt-desktop" => Some("chatgpt"),
        // The connector emits `agent_slug = "claude_code"`; the registry's
        // public slug stays `claude`. Both must resolve.
        "claude" | "claude-code" | "claude_code" => Some("claude"),
        "codex" | "codex-cli" => Some("codex"),
        "cursor" => Some("cursor"),
        "devin" | "devin-cli" => Some("devin"),
        "gemini" | "gemini-cli" => Some("gemini"),
        "opencode" | "open-code" => Some("opencode"),
        "qoder" | "qoder-cli" | "qoder_cli" | "qoder-cn" | "qoder_cn" | "qoderwork" => {
            Some("qoder")
        }
        "qwen" | "qwen-code" | "qwen-cli" => Some("qwen"),
        _ => None,
    }
}

fn normalize_slug(raw: &str) -> Option<String> {
    let slug = raw.trim().to_ascii_lowercase();
    if slug.is_empty() { None } else { Some(slug) }
}

fn canonical_or_normalized_slug(raw: &str) -> Option<String> {
    let normalized = normalize_slug(raw)?;
    Some(canonical_connector_slug(&normalized).map_or(normalized, std::string::ToString::to_string))
}

/// Expand a leading `~` in a configured path value.
///
/// Only a bare `~` or a `~/rest` (`~\rest` on Windows) prefix is expanded, the
/// way agents that document tilde support in their own env overrides do it;
/// `~user` has no portable meaning here and is preserved verbatim. When the
/// home directory is unknown the raw value survives rather than being silently
/// rooted at the process working directory.
pub(crate) fn expand_leading_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(home) = home {
        if trimmed == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = trimmed
            .strip_prefix("~/")
            .or_else(|| trimmed.strip_prefix("~\\"))
        {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

fn home_join(parts: &[&str]) -> Option<PathBuf> {
    let mut path = dirs::home_dir()?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

fn cwd_join(parts: &[&str]) -> Option<PathBuf> {
    let mut path = std::env::current_dir().ok()?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

/// Qwen's runtime store follows its own environment precedence, including
/// tilde expansion and paths relative to the process working directory.
/// Keep this shared by detection and scanning so relocated stores agree.
pub(crate) fn qwen_runtime_root_from_env() -> Option<PathBuf> {
    let raw = ["QWEN_RUNTIME_DIR", "QWEN_HOME"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))?;
    let path = if raw == "~" {
        dirs::home_dir()?
    } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        let mut home = dirs::home_dir()?;
        for segment in rest.split(['/', '\\']).filter(|part| !part.is_empty()) {
            home.push(segment);
        }
        home
    } else {
        PathBuf::from(raw)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    // Storage resolves its runtime base lexically before filesystem lookup.
    // Canonicalizing instead would follow symlinks and require every prefix
    // to exist, potentially selecting a different store for `link/../root`.
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

#[allow(clippy::too_many_lines)]
fn env_override_roots(slug: &str) -> Option<Vec<PathBuf>> {
    let read = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string());
    let read_path = |key: &str| read(key).filter(|v| !v.is_empty()).map(PathBuf::from);

    match slug {
        "antigravity" => {
            let root = read("CASS_ANTIGRAVITY_DATA_ROOT")?;
            if root.is_empty() {
                return None;
            }
            Some(vec![PathBuf::from(root)])
        }
        "claude" => claude_env_override_roots(read("CLAUDE_CONFIG_DIR").as_deref()),
        "devin" => {
            // Mirrors DevinConnector's CASS_DEVIN_DATA_ROOT resolution
            // (CLI `sessions.db`, IDE `acp-messages`, or parent `devin` / `Devin`
            // data roots) so a relocated store is detected where it is scanned.
            let root = read("CASS_DEVIN_DATA_ROOT")?;
            if root.is_empty() {
                return None;
            }
            let root = PathBuf::from(root);
            if root.extension().is_some_and(|ext| ext == "db") {
                return Some(vec![root]);
            }
            Some(vec![
                root.join("sessions.db"),
                root.join("cli").join("sessions.db"),
                root.join("acp-messages"),
                root.join("User").join("acp-messages"),
                root,
            ])
        }
        "codex" => {
            let root = read("CODEX_HOME")?;
            if root.is_empty() {
                return None;
            }
            Some(vec![PathBuf::from(root).join("sessions")])
        }
        "cursor" => {
            let projects = read_path("CASS_CURSOR_PROJECTS_ROOT")?;
            let mut roots: Vec<_> = projects.is_dir().then_some(projects).into_iter().collect();
            // The projects override replaces Agent transcripts, but Composer
            // remains an independent store. Do not fall back to ~/.cursor
            // when the configured transcript directory is missing.
            let agent_home = home_join(&[".cursor"]);
            for root in default_probe_roots(slug) {
                if Some(&root) != agent_home.as_ref() && !roots.contains(&root) {
                    roots.push(root);
                }
            }
            Some(roots)
        }
        "gemini" => {
            let root = read_path("GEMINI_HOME")?;
            // GEMINI_HOME directly replaces the session root, even if it is
            // missing. A file root must satisfy the scanner's path admission.
            let admitted = root.is_dir() || (root.is_file() && is_gemini_session_file(&root));
            Some(admitted.then_some(root).into_iter().collect())
        }
        "qoder" => qoder_env_override_roots(
            read("QODER_PROJECTS_DIR").as_deref(),
            read("QODER_HOME").as_deref(),
            dirs::home_dir().as_deref(),
        ),
        "qwen" => {
            let root = qwen_runtime_root_from_env()?;
            // The runtime override replaces the default store even when it
            // is absent. Native histories live in projects; tmp is legacy.
            Some(
                [root.join("projects"), root.join("tmp"), root]
                    .into_iter()
                    .filter(|path| path.is_dir())
                    .collect(),
            )
        }
        "opencode" => {
            // The legacy storage root and SQLite databases are independent.
            // SQLite overrides add a candidate; they never hide default DBs.
            let mut roots: Vec<_> = [
                read_path("OPENCODE_SQLITE_DB").filter(|path| path.is_file()),
                read_path("OPENCODE_STORAGE_ROOT").filter(|path| path.is_dir()),
            ]
            .into_iter()
            .flatten()
            .collect();
            if roots.is_empty() {
                return None;
            }
            for root in default_probe_roots(slug) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            Some(roots)
        }
        _ => None,
    }
}

/// Path admission shared by Gemini detection and session scanning.
fn is_gemini_session_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("session-"))
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonl")
            })
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("chats")
}

fn qoder_env_override_roots(
    qoder_projects_dir: Option<&str>,
    qoder_home: Option<&str>,
    home: Option<&Path>,
) -> Option<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let push_unique = |out: &mut Vec<PathBuf>, path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(dir) = qoder_projects_dir.map(str::trim).filter(|s| !s.is_empty()) {
        push_unique(&mut roots, expand_leading_tilde(dir, home));
    }
    if let Some(root) = qoder_home.map(str::trim).filter(|s| !s.is_empty()) {
        let resolved = expand_leading_tilde(root, home);
        push_unique(&mut roots, resolved.join("projects"));
        push_unique(&mut roots, resolved);
    }
    (!roots.is_empty()).then_some(roots)
}

/// Claude Code detection roots when `CLAUDE_CONFIG_DIR` is set (cass #448).
///
/// Mirrors `ClaudeCodeConnector::projects_root_resolved`: an explicit
/// `CLAUDE_CONFIG_DIR` REPLACES the default roots, and the connector scans
/// `$CLAUDE_CONFIG_DIR/projects`. Both the projects dir and the config dir
/// itself are probed so a freshly redirected profile with no sessions yet is
/// still reported as installed (the scan then simply finds nothing).
fn claude_env_override_roots(claude_config_dir: Option<&str>) -> Option<Vec<PathBuf>> {
    let root = claude_config_dir?.trim();
    if root.is_empty() {
        return None;
    }
    let root = PathBuf::from(root);
    Some(vec![root.join("projects"), root])
}

/// Claude Code default detection roots (cass #448).
///
/// `XDG_CONFIG_HOME` is additive, not a replacement: the connector's resolver
/// scans `$XDG_CONFIG_HOME/claude-code/projects` when the variable is set, and
/// `~/.claude` users who export `XDG_CONFIG_HOME` globally (common on Linux)
/// must stay detected too.
fn claude_default_probe_roots(
    xdg_config_home: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = xdg_config_home {
        out.push(xdg.join("claude-code"));
    }
    let Some(home) = home else {
        return out;
    };
    out.push(home.join(".claude"));
    out.push(home.join(".config").join("claude"));
    let claude_support = home
        .join("Library")
        .join("Application Support")
        .join("Claude");
    out.push(claude_support.join("claude-code-sessions"));
    out.push(claude_support.join("local-agent-mode-sessions"));
    let claude_linux = home.join(".config").join("Claude");
    out.push(claude_linux.join("claude-code-sessions"));
    out.push(claude_linux.join("local-agent-mode-sessions"));
    let claude_win_msix = home
        .join("AppData")
        .join("Local")
        .join("Packages")
        .join("Claude_pzs8sxrjxfjjc")
        .join("LocalCache")
        .join("Roaming")
        .join("Claude");
    out.push(claude_win_msix.join("claude-code-sessions"));
    out.push(claude_win_msix.join("local-agent-mode-sessions"));
    let claude_win_roaming = home.join("AppData").join("Roaming").join("Claude");
    out.push(claude_win_roaming.join("claude-code-sessions"));
    out.push(claude_win_roaming.join("local-agent-mode-sessions"));
    out
}

#[allow(clippy::too_many_lines)]
fn default_probe_roots(slug: &str) -> Vec<PathBuf> {
    fn maybe_push(out: &mut Vec<PathBuf>, parts: &[&str]) {
        if let Some(path) = home_join(parts) {
            out.push(path);
        }
    }

    let mut out = Vec::new();

    match slug {
        "chatgpt" => {
            maybe_push(
                &mut out,
                &["Library", "Application Support", "com.openai.chat"],
            );
        }
        "claude" => {
            let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty());
            out.extend(claude_default_probe_roots(
                xdg_config_home.as_deref(),
                dirs::home_dir().as_deref(),
            ));
            for env_base in [
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|p| {
                        p.join("Packages")
                            .join("Claude_pzs8sxrjxfjjc")
                            .join("LocalCache")
                            .join("Roaming")
                            .join("Claude")
                    }),
                std::env::var_os("APPDATA")
                    .map(PathBuf::from)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|p| p.join("Claude")),
            ]
            .into_iter()
            .flatten()
            {
                for leaf in ["claude-code-sessions", "local-agent-mode-sessions"] {
                    let candidate = env_base.join(leaf);
                    if !out.contains(&candidate) {
                        out.push(candidate);
                    }
                }
            }
        }
        "codex" => {
            maybe_push(&mut out, &[".codex", "sessions"]);
        }
        "cursor" => {
            maybe_push(&mut out, &[".cursor"]);
            maybe_push(&mut out, &[".config", "Cursor"]);
            maybe_push(&mut out, &[".config", "Cursor", "User"]);
            maybe_push(
                &mut out,
                &["Library", "Application Support", "Cursor", "User"],
            );
            maybe_push(&mut out, &["AppData", "Roaming", "Cursor", "User"]);
        }
        "devin" => {
            maybe_push(&mut out, &[".local", "bin", "devin"]);
            maybe_push(&mut out, &[".local", "share", "devin"]);
            maybe_push(&mut out, &[".local", "share", "devin", "cli"]);
            maybe_push(&mut out, &[".config", "Devin", "User", "acp-messages"]);
            maybe_push(&mut out, &[".config", "Devin", "User"]);
            maybe_push(
                &mut out,
                &[
                    "Library",
                    "Application Support",
                    "Devin",
                    "User",
                    "acp-messages",
                ],
            );
            maybe_push(
                &mut out,
                &["Library", "Application Support", "Devin", "User"],
            );
            maybe_push(
                &mut out,
                &["AppData", "Roaming", "Devin", "User", "acp-messages"],
            );
            maybe_push(&mut out, &["AppData", "Roaming", "Devin", "User"]);
        }
        "antigravity" => {
            // Two products, two stores (cass #454): the Antigravity IDE
            // (`~/.gemini/antigravity`) and the `agy` CLI
            // (`~/.gemini/antigravity-cli`). The connector scans both.
            maybe_push(&mut out, &[".gemini", "antigravity", "conversations"]);
            maybe_push(&mut out, &[".gemini", "antigravity", "brain"]);
            maybe_push(&mut out, &[".gemini", "antigravity"]);
            maybe_push(&mut out, &[".gemini", "antigravity-cli", "conversations"]);
            maybe_push(&mut out, &[".gemini", "antigravity-cli", "brain"]);
            maybe_push(&mut out, &[".gemini", "antigravity-cli"]);
        }
        "gemini" => {
            maybe_push(&mut out, &[".gemini"]);
            maybe_push(&mut out, &[".config", "gemini"]);
        }
        "opencode" => {
            // The canonical v1.2+ SQLite database. Probed first so diagnostic
            // output surfaces the data file (not the sibling config directory)
            // whenever it exists. See cass issue #188 — `~/.config/opencode/`
            // is the config dir and does NOT hold session data.
            maybe_push(&mut out, &[".local", "share", "opencode", "opencode.db"]);
            maybe_push(&mut out, &[".local", "share", "opencode"]);
            maybe_push(&mut out, &[".config", "opencode", "opencode.db"]);
            maybe_push(&mut out, &[".config", "opencode"]);
        }
        "qoder" => {
            maybe_push(&mut out, &[".qoder", "projects"]);
            maybe_push(&mut out, &[".qoderwork", "projects"]);
            maybe_push(&mut out, &[".qoder-cn", "projects"]);
            maybe_push(
                &mut out,
                &[
                    "Library",
                    "Application Support",
                    "Qoder",
                    "SharedClientCache",
                    "cli",
                    "projects",
                ],
            );
            maybe_push(
                &mut out,
                &[".config", "Qoder", "SharedClientCache", "cli", "projects"],
            );
            maybe_push(
                &mut out,
                &[
                    "AppData",
                    "Roaming",
                    "Qoder",
                    "SharedClientCache",
                    "cli",
                    "projects",
                ],
            );
            if let Some(appdata) = std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
            {
                let candidate = appdata
                    .join("Qoder")
                    .join("SharedClientCache")
                    .join("cli")
                    .join("projects");
                if !out.contains(&candidate) {
                    out.push(candidate);
                }
            }
            maybe_push(&mut out, &[".qoder"]);
        }
        "qwen" => {
            maybe_push(&mut out, &[".qwen", "projects"]);
            maybe_push(&mut out, &[".qwen", "tmp"]);
            maybe_push(&mut out, &[".qwen"]);
        }
        _ => {}
    }

    out
}

fn detect_roots(
    slug: &'static str,
    roots: &[PathBuf],
    source_label: &str,
) -> InstalledAgentDetectionEntry {
    let mut detected = false;
    let mut evidence: Vec<String> = Vec::new();
    let mut root_paths: Vec<String> = Vec::new();

    if roots.is_empty() {
        evidence.push("no probe roots available".to_string());
    }

    for root in roots {
        let root_str = root.display().to_string();
        if root.exists() {
            detected = true;
            root_paths.push(root_str.clone());
            evidence.push(format!("{source_label} root exists: {root_str}"));
        } else {
            evidence.push(format!("{source_label} root missing: {root_str}"));
        }
    }

    // Preserve probe order — probes are already arranged by priority
    // (see default_probe_roots), so the first existing root is the most
    // preferred display path. A lexicographic sort would cause config
    // directories to shadow data directories — e.g. `.config/opencode`
    // would mask `.local/share/opencode/opencode.db` (cass issue #188).
    InstalledAgentDetectionEntry {
        slug: slug.to_string(),
        detected,
        evidence,
        root_paths,
    }
}

fn entry_from_detect(slug: &'static str) -> InstalledAgentDetectionEntry {
    if let Some(override_roots) = env_override_roots(slug) {
        return detect_roots(slug, &override_roots, "env");
    }
    let roots = default_probe_roots(slug);
    detect_roots(slug, &roots, "default")
}

fn entry_from_override(slug: &'static str, roots: &[PathBuf]) -> InstalledAgentDetectionEntry {
    detect_roots(slug, roots, "override")
}

fn build_overrides_map(overrides: &[AgentDetectRootOverride]) -> HashMap<String, Vec<PathBuf>> {
    let mut out: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for override_root in overrides {
        let Some(slug) = canonical_or_normalized_slug(&override_root.slug) else {
            continue;
        };
        out.entry(slug)
            .or_default()
            .push(override_root.root.clone());
    }
    out
}

fn validate_known_connectors(
    available: &HashSet<&'static str>,
    only: Option<&HashSet<String>>,
    overrides: &HashMap<String, Vec<PathBuf>>,
) -> Result<(), AgentDetectError> {
    let mut unknown: Vec<String> = Vec::new();
    if let Some(only) = only {
        unknown.extend(
            only.iter()
                .filter(|slug| !available.contains(slug.as_str()))
                .cloned(),
        );
    }
    unknown.extend(
        overrides
            .keys()
            .filter(|slug| !available.contains(slug.as_str()))
            .cloned(),
    );
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    unknown.dedup();
    Err(AgentDetectError::UnknownConnectors {
        connectors: unknown,
    })
}

/// Returns default probe paths for all known connectors using tilde-relative paths.
///
/// These paths use `~/` prefix instead of resolved home directories, making them
/// suitable for SSH probe scripts where the remote home directory is unknown.
/// Each entry is `(slug, paths)` where `paths` are bash-friendly strings like
/// `~/.claude/projects`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn default_probe_paths_tilde() -> Vec<(&'static str, Vec<String>)> {
    fn tilde(parts: &[&str]) -> String {
        let mut path = String::from("~/");
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                path.push('/');
            }
            path.push_str(part);
        }
        path
    }

    KNOWN_CONNECTORS
        .iter()
        .map(|&slug| {
            let paths: Vec<String> = match slug {
                "chatgpt" => vec![tilde(&[
                    "Library",
                    "Application Support",
                    "com.openai.chat",
                ])],
                "claude" => vec![
                    tilde(&[".claude", "projects"]),
                    tilde(&[".claude"]),
                    tilde(&[".config", "claude"]),
                    tilde(&[
                        "Library",
                        "Application Support",
                        "Claude",
                        "claude-code-sessions",
                    ]),
                    tilde(&[
                        "Library",
                        "Application Support",
                        "Claude",
                        "local-agent-mode-sessions",
                    ]),
                    tilde(&[".config", "Claude", "claude-code-sessions"]),
                    tilde(&[".config", "Claude", "local-agent-mode-sessions"]),
                    tilde(&[
                        "AppData",
                        "Local",
                        "Packages",
                        "Claude_pzs8sxrjxfjjc",
                        "LocalCache",
                        "Roaming",
                        "Claude",
                        "claude-code-sessions",
                    ]),
                    tilde(&[
                        "AppData",
                        "Local",
                        "Packages",
                        "Claude_pzs8sxrjxfjjc",
                        "LocalCache",
                        "Roaming",
                        "Claude",
                        "local-agent-mode-sessions",
                    ]),
                    tilde(&["AppData", "Roaming", "Claude", "claude-code-sessions"]),
                    tilde(&["AppData", "Roaming", "Claude", "local-agent-mode-sessions"]),
                ],
                "codex" => vec![tilde(&[".codex", "sessions"])],
                "cursor" => vec![
                    tilde(&[".cursor"]),
                    tilde(&[".config", "Cursor"]),
                    tilde(&[".config", "Cursor", "User"]),
                    tilde(&["Library", "Application Support", "Cursor", "User"]),
                    tilde(&["AppData", "Roaming", "Cursor", "User"]),
                ],
                "devin" => vec![
                    tilde(&[".local", "bin", "devin"]),
                    tilde(&[".local", "share", "devin"]),
                    tilde(&[".local", "share", "devin", "cli"]),
                    tilde(&[".config", "Devin", "User", "acp-messages"]),
                    tilde(&[".config", "Devin", "User"]),
                    tilde(&[
                        "Library",
                        "Application Support",
                        "Devin",
                        "User",
                        "acp-messages",
                    ]),
                    tilde(&["Library", "Application Support", "Devin", "User"]),
                    tilde(&["AppData", "Roaming", "Devin", "User", "acp-messages"]),
                    tilde(&["AppData", "Roaming", "Devin", "User"]),
                ],
                "antigravity" => vec![
                    tilde(&[".gemini", "antigravity", "conversations"]),
                    tilde(&[".gemini", "antigravity", "brain"]),
                    tilde(&[".gemini", "antigravity"]),
                    tilde(&[".gemini", "antigravity-cli", "conversations"]),
                    tilde(&[".gemini", "antigravity-cli", "brain"]),
                    tilde(&[".gemini", "antigravity-cli"]),
                ],
                "gemini" => vec![
                    tilde(&[".gemini", "tmp"]),
                    tilde(&[".gemini"]),
                    tilde(&[".config", "gemini"]),
                ],
                "opencode" => vec![
                    // Direct path to the v1.2+ SQLite database — probed first
                    // so display/diagnostics surface the data file (not the
                    // sibling config dir). See cass issue #188.
                    tilde(&[".local", "share", "opencode", "opencode.db"]),
                    tilde(&[".local", "share", "opencode"]),
                    tilde(&[".config", "opencode", "opencode.db"]),
                    tilde(&[".config", "opencode"]),
                ],
                "qoder" => vec![
                    tilde(&[".qoder", "projects"]),
                    tilde(&[".qoderwork", "projects"]),
                    tilde(&[".qoder-cn", "projects"]),
                    tilde(&[
                        "Library",
                        "Application Support",
                        "Qoder",
                        "SharedClientCache",
                        "cli",
                        "projects",
                    ]),
                    tilde(&[".config", "Qoder", "SharedClientCache", "cli", "projects"]),
                    tilde(&[
                        "AppData",
                        "Roaming",
                        "Qoder",
                        "SharedClientCache",
                        "cli",
                        "projects",
                    ]),
                    tilde(&[".qoder"]),
                ],
                "qwen" => vec![
                    tilde(&[".qwen", "projects"]),
                    tilde(&[".qwen", "tmp"]),
                    tilde(&[".qwen"]),
                ],
                _ => vec![],
            };
            (slug, paths)
        })
        .collect()
}

/// Detect installed/available coding agents by running local filesystem probes.
///
/// This returns a stable JSON shape (via `serde`) intended for CLI/resource consumption.
///
/// # Errors
/// Returns [`AgentDetectError::UnknownConnectors`] when `only_connectors`
/// includes unknown slugs.
#[allow(clippy::missing_const_for_fn)]
pub fn detect_installed_agents(
    opts: &AgentDetectOptions,
) -> Result<InstalledAgentDetectionReport, AgentDetectError> {
    let available: HashSet<&'static str> = KNOWN_CONNECTORS.iter().copied().collect();
    let overrides = build_overrides_map(&opts.root_overrides);

    let only: Option<HashSet<String>> = opts.only_connectors.as_ref().map(|slugs| {
        slugs
            .iter()
            .filter_map(|slug| canonical_or_normalized_slug(slug))
            .collect()
    });

    validate_known_connectors(&available, only.as_ref(), &overrides)?;

    let mut all_entries: Vec<InstalledAgentDetectionEntry> = KNOWN_CONNECTORS
        .iter()
        .copied()
        .filter(|slug| only.as_ref().is_none_or(|set| set.contains(*slug)))
        .map(|slug| {
            overrides.get(slug).map_or_else(
                || entry_from_detect(slug),
                |roots| entry_from_override(slug, roots),
            )
        })
        .collect();

    all_entries.sort_by(|a, b| a.slug.cmp(&b.slug));

    let detected_count = all_entries.iter().filter(|entry| entry.detected).count();
    let total_count = all_entries.len();

    Ok(InstalledAgentDetectionReport {
        format_version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        installed_agents: if opts.include_undetected {
            all_entries
        } else {
            all_entries
                .into_iter()
                .filter(|entry| entry.detected)
                .collect()
        },
        summary: InstalledAgentDetectionSummary {
            detected_count,
            total_count,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // cass #448: the Claude Code detection probe must agree with the
    // connector's root resolver (CLAUDE_CONFIG_DIR, then XDG_CONFIG_HOME,
    // then ~/.claude), otherwise a redirected install with no legacy
    // ~/.claude is "not detected" and never scanned.
    #[test]
    fn claude_env_override_replaces_defaults_with_the_redirected_profile() {
        let roots = claude_env_override_roots(Some("/srv/tenant-a")).expect("override");
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/srv/tenant-a/projects"),
                PathBuf::from("/srv/tenant-a")
            ]
        );
        assert_eq!(
            claude_env_override_roots(Some("  ~/.claude-work  ")).expect("trimmed"),
            vec![
                PathBuf::from("~/.claude-work/projects"),
                PathBuf::from("~/.claude-work")
            ]
        );
    }

    #[test]
    fn claude_env_override_ignores_unset_or_blank_config_dir() {
        assert!(claude_env_override_roots(None).is_none());
        assert!(claude_env_override_roots(Some("")).is_none());
        assert!(claude_env_override_roots(Some("   ")).is_none());
    }

    #[test]
    fn claude_default_probe_roots_add_xdg_claude_code_before_home_defaults() {
        let home = PathBuf::from("/home/u");
        let xdg = PathBuf::from("/home/u/xdg");
        let roots = claude_default_probe_roots(Some(&xdg), Some(&home));
        assert_eq!(roots[0], PathBuf::from("/home/u/xdg/claude-code"));
        assert_eq!(roots[1], PathBuf::from("/home/u/.claude"));
        assert!(roots.contains(&PathBuf::from("/home/u/.config/claude")));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/Library/Application Support/Claude/claude-code-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/Library/Application Support/Claude/local-agent-mode-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/.config/Claude/claude-code-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/.config/Claude/local-agent-mode-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/local-agent-mode-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/AppData/Roaming/Claude/claude-code-sessions"
        )));
        assert!(roots.contains(&PathBuf::from(
            "/home/u/AppData/Roaming/Claude/local-agent-mode-sessions"
        )));
        // XDG is additive: the legacy home roots survive a globally exported
        // XDG_CONFIG_HOME.
        let without_xdg = claude_default_probe_roots(None, Some(&home));
        assert_eq!(&roots[1..], &without_xdg[..]);
        assert_eq!(without_xdg[0], PathBuf::from("/home/u/.claude"));
    }

    #[test]
    fn qoder_slugs_env_overrides_and_probe_roots_cover_all_variants() {
        for alias in [
            "qoder",
            "qoder-cli",
            "qoder_cli",
            "qoder-cn",
            "qoder_cn",
            "qoderwork",
        ] {
            assert_eq!(canonical_connector_slug(alias), Some("qoder"), "{alias}");
        }

        let home = PathBuf::from("/home/u");
        assert_eq!(
            qoder_env_override_roots(Some("~/.qoder-custom/projects"), None, Some(&home)),
            Some(vec![PathBuf::from("/home/u/.qoder-custom/projects")])
        );
        assert_eq!(
            qoder_env_override_roots(None, Some("/opt/qoder"), Some(&home)),
            Some(vec![
                PathBuf::from("/opt/qoder/projects"),
                PathBuf::from("/opt/qoder")
            ])
        );
        assert_eq!(
            qoder_env_override_roots(Some("/direct/projects"), Some("/opt/qoder"), Some(&home)),
            Some(vec![
                PathBuf::from("/direct/projects"),
                PathBuf::from("/opt/qoder/projects"),
                PathBuf::from("/opt/qoder"),
            ])
        );
        assert!(qoder_env_override_roots(Some("   "), Some(""), Some(&home)).is_none());

        let probe_roots = default_probe_roots("qoder");
        if let Some(real_home) = dirs::home_dir() {
            assert!(probe_roots.contains(&real_home.join(".qoderwork/projects")));
            assert!(probe_roots.contains(&real_home.join(".qoder-cn/projects")));
            assert!(probe_roots.contains(
                &real_home.join("Library/Application Support/Qoder/SharedClientCache/cli/projects")
            ));
            assert!(
                probe_roots
                    .contains(&real_home.join(".config/Qoder/SharedClientCache/cli/projects"))
            );
            assert!(
                probe_roots.contains(
                    &real_home.join("AppData/Roaming/Qoder/SharedClientCache/cli/projects")
                )
            );
        }
    }

    #[test]
    fn claude_default_probe_roots_without_home_keep_only_xdg() {
        let xdg = PathBuf::from("/x");
        assert_eq!(
            claude_default_probe_roots(Some(&xdg), None),
            vec![PathBuf::from("/x/claude-code")]
        );
        assert!(claude_default_probe_roots(None, None).is_empty());
    }

    #[test]
    fn claude_detection_reports_the_redirected_profile_root() {
        // Exercise the same `detect_roots` path `entry_from_detect` takes for
        // an env override, against a real directory, without touching the
        // process environment.
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = tmp.path().join("claude-cfg");
        std::fs::create_dir_all(profile.join("projects")).expect("projects dir");
        let roots = claude_env_override_roots(profile.to_str()).expect("override");
        let entry = detect_roots("claude", &roots, "env");
        assert!(entry.detected);
        assert_eq!(
            entry.root_paths,
            vec![
                profile.join("projects").display().to_string(),
                profile.display().to_string()
            ]
        );

        let xdg = tmp.path().join("xdg");
        std::fs::create_dir_all(xdg.join("claude-code").join("projects")).expect("xdg dir");
        let missing_home = tmp.path().join("no-such-home");
        let roots = claude_default_probe_roots(Some(&xdg), Some(&missing_home));
        let entry = detect_roots("claude", &roots, "default");
        assert!(entry.detected);
        assert_eq!(
            entry.root_paths,
            vec![xdg.join("claude-code").display().to_string()]
        );
    }

    #[test]
    fn detect_installed_agents_can_be_scoped_to_specific_connectors() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let codex_root = tmp.path().join("codex-home").join("sessions");
        std::fs::create_dir_all(&codex_root).expect("create codex sessions");

        let gemini_root = tmp.path().join("gemini-home").join("tmp");
        std::fs::create_dir_all(&gemini_root).expect("create gemini root");

        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string(), "gemini".to_string()]),
            include_undetected: true,
            root_overrides: vec![
                AgentDetectRootOverride {
                    slug: "codex".to_string(),
                    root: codex_root,
                },
                AgentDetectRootOverride {
                    slug: "gemini".to_string(),
                    root: gemini_root.clone(),
                },
            ],
        })
        .expect("detect");

        assert_eq!(report.format_version, 1);
        assert!(!report.generated_at.is_empty());
        assert_eq!(report.summary.total_count, 2);
        assert_eq!(report.summary.detected_count, 2);

        let slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .map(|entry| entry.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["codex", "gemini"]);

        let codex = report
            .installed_agents
            .iter()
            .find(|entry| entry.slug == "codex")
            .expect("codex entry");
        assert!(codex.detected);
        assert!(
            codex
                .root_paths
                .iter()
                .any(|path| path.ends_with("/sessions"))
        );

        let gemini = report
            .installed_agents
            .iter()
            .find(|entry| entry.slug == "gemini")
            .expect("gemini entry");
        assert!(gemini.detected);
        assert_eq!(gemini.root_paths, vec![gemini_root.display().to_string()]);
    }

    #[test]
    fn unknown_connectors_are_rejected() {
        let err = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["not-a-real-connector".to_string()]),
            include_undetected: true,
            root_overrides: vec![],
        })
        .expect_err("should error");

        let err_msg = err.to_string();
        assert!(
            matches!(
                err,
                AgentDetectError::UnknownConnectors { connectors }
                    if connectors == vec!["not-a-real-connector".to_string()]
            ),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn unknown_overrides_are_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string()]),
            include_undetected: true,
            root_overrides: vec![AgentDetectRootOverride {
                slug: "definitely-unknown".to_string(),
                root: tmp.path().join("does-not-matter"),
            }],
        })
        .expect_err("should error");

        let err_msg = err.to_string();
        assert!(
            matches!(
                err,
                AgentDetectError::UnknownConnectors { connectors }
                    if connectors == vec!["definitely-unknown".to_string()]
            ),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn cass_connectors_and_aliases_detect_via_overrides() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let chatgpt_root = tmp.path().join("chatgpt-root");
        std::fs::create_dir_all(&chatgpt_root).expect("mkdir chatgpt");

        let antigravity_root = tmp.path().join("antigravity-root");
        std::fs::create_dir_all(&antigravity_root).expect("mkdir antigravity");

        let qoder_root = tmp.path().join("qoder-root");
        std::fs::create_dir_all(&qoder_root).expect("mkdir qoder");

        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec![
                "agy".to_string(),
                "chatgpt".to_string(),
                "qoder-cli".to_string(),
            ]),
            include_undetected: true,
            root_overrides: vec![
                AgentDetectRootOverride {
                    slug: "antigravity-cli".to_string(),
                    root: antigravity_root,
                },
                AgentDetectRootOverride {
                    slug: "chatgpt-desktop".to_string(),
                    root: chatgpt_root,
                },
                AgentDetectRootOverride {
                    slug: "qoderwork".to_string(),
                    root: qoder_root,
                },
            ],
        })
        .expect("detect");

        assert_eq!(report.summary.total_count, 3);
        assert_eq!(report.summary.detected_count, 3);

        let slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .map(|entry| entry.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["antigravity", "chatgpt", "qoder"]);
    }

    #[test]
    fn default_probe_paths_tilde_covers_expected_roots() {
        let mut by_slug: std::collections::HashMap<&'static str, Vec<String>> =
            std::collections::HashMap::new();
        for (slug, paths) in default_probe_paths_tilde() {
            by_slug.insert(slug, paths);
        }

        let opencode = by_slug.get("opencode").expect("opencode paths");
        assert!(opencode.contains(&"~/.local/share/opencode".to_string()));
        assert!(opencode.contains(&"~/.config/opencode".to_string()));

        let cursor = by_slug.get("cursor").expect("cursor paths");
        assert!(cursor.contains(&"~/.config/Cursor".to_string()));
        assert!(cursor.contains(&"~/.config/Cursor/User".to_string()));
        assert!(cursor.contains(&"~/Library/Application Support/Cursor/User".to_string()));
        assert!(cursor.contains(&"~/AppData/Roaming/Cursor/User".to_string()));

        let qoder = by_slug.get("qoder").expect("qoder paths");
        assert!(qoder.contains(&"~/.qoder/projects".to_string()));
        assert!(qoder.contains(&"~/.qoderwork/projects".to_string()));
        assert!(qoder.contains(&"~/.qoder-cn/projects".to_string()));
        assert!(qoder.contains(
            &"~/Library/Application Support/Qoder/SharedClientCache/cli/projects".to_string()
        ));
        assert!(qoder.contains(&"~/.config/Qoder/SharedClientCache/cli/projects".to_string()));
        assert!(
            qoder.contains(&"~/AppData/Roaming/Qoder/SharedClientCache/cli/projects".to_string())
        );

        let claude = by_slug.get("claude").expect("claude paths");
        assert!(claude.contains(&"~/.config/Claude/claude-code-sessions".to_string()));
        assert!(claude.contains(&"~/.config/Claude/local-agent-mode-sessions".to_string()));
        assert!(claude.contains(
            &"~/AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code-sessions".to_string()
        ));
        assert!(claude.contains(
            &"~/AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/local-agent-mode-sessions".to_string()
        ));
        assert!(claude.contains(&"~/AppData/Roaming/Claude/claude-code-sessions".to_string()));
        assert!(claude.contains(&"~/AppData/Roaming/Claude/local-agent-mode-sessions".to_string()));
    }

    /// Registry invariants: the five registration points
    /// (`KNOWN_CONNECTORS`, `canonical_connector_slug`,
    /// `default_probe_roots`, `default_probe_paths_tilde`, and the factory
    /// list) must agree exactly. Range-editing one table has twice silently
    /// dropped a sibling row; this pins every table to the slug set mechanically.
    /// Every `agent_slug` a connector writes into its conversations must be
    /// accepted by detection, so a consumer can go from a conversation back
    /// to `detect_installed_agents` (bead 8np: `claude_code` vs `claude`).
    #[test]
    fn emitted_conversation_slugs_resolve_in_the_registry() {
        for (emitted, canonical) in [
            ("claude_code", "claude"),
            ("codex-cli", "codex"),
            ("qoder-cli", "qoder"),
            ("qoderwork", "qoder"),
            ("agy", "antigravity"),
        ] {
            assert_eq!(
                canonical_connector_slug(emitted),
                Some(canonical),
                "{emitted}"
            );
        }
        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["claude_code".into()]),
            include_undetected: true,
            root_overrides: Vec::new(),
        })
        .expect("emitted slug resolves");
        assert_eq!(report.installed_agents[0].slug, "claude");
    }

    #[test]
    fn registry_tables_agree_exactly() {
        let known: HashSet<&str> = KNOWN_CONNECTORS.iter().copied().collect();
        assert!(!known.is_empty());

        // 1. Every known connector resolves to itself (canonical self-arm).
        for slug in KNOWN_CONNECTORS {
            assert_eq!(
                canonical_connector_slug(slug),
                Some(*slug),
                "connector {slug} must be its own canonical slug"
            );
        }

        // 2. Every known connector has default probe roots.
        for slug in KNOWN_CONNECTORS {
            assert!(
                !default_probe_roots(slug).is_empty(),
                "connector {slug} must have at least one default probe root"
            );
        }

        // 3. The tilde table covers exactly the known set — no missing rows,
        // no orphaned rows for retired slugs.
        let tilde_slugs: Vec<&str> = default_probe_paths_tilde()
            .into_iter()
            .map(|(slug, _)| slug)
            .collect();
        let tilde_set: HashSet<&str> = tilde_slugs.iter().copied().collect();
        assert_eq!(
            tilde_set,
            known,
            "tilde table slugs must equal KNOWN_CONNECTORS; missing={:?} extra={:?}",
            known.difference(&tilde_set).collect::<Vec<_>>(),
            tilde_set.difference(&known).collect::<Vec<_>>(),
        );
        assert_eq!(
            tilde_slugs.len(),
            tilde_set.len(),
            "tilde table must not repeat a connector"
        );
        for (slug, paths) in default_probe_paths_tilde() {
            assert!(
                !paths.is_empty(),
                "connector {slug} has empty tilde paths"
            );
        }

        // 4. The compiled factory registry covers every scanning connector.
        #[cfg(feature = "connectors")]
        {
            let feature_gated: HashMap<&str, bool> = HashMap::from([
                ("chatgpt", cfg!(feature = "chatgpt")),
                ("cursor", cfg!(feature = "cursor")),
                ("opencode", cfg!(feature = "opencode")),
            ]);
            let mut factory_slugs: HashSet<&str> = HashSet::new();
            for (raw, _) in get_connector_factories() {
                let canonical = canonical_connector_slug(raw).unwrap_or_else(|| {
                    // ubs:ignore[rust.ownership.panic-macro] — Fail this registry invariant test if a factory has no canonical public connector name.
                    panic!("factory slug {raw} does not canonicalise into KNOWN_CONNECTORS")
                });
                assert!(
                    factory_slugs.insert(canonical),
                    "connector {canonical} is registered twice in the factory list (via {raw})"
                );
            }
            for slug in KNOWN_CONNECTORS {
                match feature_gated.get(slug) {
                    Some(false) => assert!(
                        !factory_slugs.contains(slug),
                        "connector {slug} registered a factory although its cargo feature is off"
                    ),
                    Some(true) | None => assert!(
                        factory_slugs.contains(slug),
                        "connector {slug} needs a factory registration, a feature gate entry, or a detection-only listing"
                    ),
                }
            }
        }
    }
}

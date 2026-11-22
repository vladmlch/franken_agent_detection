use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::scan::{DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot};
use crate::connectors::utils::{env_path_nonempty, excluded_scan_paths_from_env, path_is_excluded};
use crate::connectors::{file_modified_since, parse_timestamp};
use crate::types::NormalizedMessage;

impl super::ClaudeCodeConnector {
    pub(super) fn projects_root() -> PathBuf {
        Self::projects_root_resolved(
            env_path_nonempty("CLAUDE_CONFIG_DIR").as_deref(),
            env_path_nonempty("XDG_CONFIG_HOME").as_deref(),
            dirs::home_dir().as_deref(),
        )
    }

    fn projects_root_candidates() -> Vec<PathBuf> {
        let primary = Self::projects_root();
        let mut roots = Self::projects_root_candidates_resolved(
            env_path_nonempty("CLAUDE_CONFIG_DIR").as_deref(),
            env_path_nonempty("XDG_CONFIG_HOME").as_deref(),
            dirs::home_dir().as_deref(),
        );
        if !roots.contains(&primary) {
            roots.push(primary);
        }
        roots
    }

    fn desktop_session_roots_resolved(home_dir: Option<&Path>) -> Vec<PathBuf> {
        let Some(home) = home_dir else {
            return Vec::new();
        };
        let claude_support = home
            .join("Library")
            .join("Application Support")
            .join("Claude");
        vec![
            claude_support.join("claude-code-sessions"),
            claude_support.join("local-agent-mode-sessions"),
        ]
    }

    /// Pure resolver for [`Self::projects_root`] — split out so the precedence
    /// chain can be unit-tested without manipulating process env vars
    /// (`std::env::set_var` is `unsafe` and not safe across parallel tests).
    ///
    /// Honors the env-var redirects Claude Code itself documents, in the same
    /// precedence order:
    ///   1. `CLAUDE_CONFIG_DIR` — explicit override.
    ///   2. `XDG_CONFIG_HOME`   — XDG fallback.
    ///   3. `${HOME}/.claude/projects` — default.
    ///
    /// Without this, the connector silently ignores caam-isolated profiles and
    /// any user with `XDG_CONFIG_HOME` set.
    pub(super) fn projects_root_resolved(
        claude_config_dir: Option<&Path>,
        xdg_config_home: Option<&Path>,
        home_dir: Option<&Path>,
    ) -> PathBuf {
        if let Some(explicit) = claude_config_dir {
            return explicit.join("projects");
        }
        if let Some(xdg) = xdg_config_home {
            return xdg.join("claude-code").join("projects");
        }
        // Match the historical behavior: when even `dirs::home_dir()` returns
        // None we fall back to a relative `.claude/projects` path. That keeps
        // strict-mode test environments (sandboxes with no HOME) unchanged.
        home_dir.map_or_else(
            || PathBuf::from(".claude/projects"),
            |h| h.join(".claude/projects"),
        )
    }

    pub(super) fn projects_root_candidates_resolved(
        claude_config_dir: Option<&Path>,
        xdg_config_home: Option<&Path>,
        home_dir: Option<&Path>,
    ) -> Vec<PathBuf> {
        if let Some(explicit) = claude_config_dir {
            return vec![explicit.join("projects")];
        }

        let mut roots = Vec::new();
        if let Some(xdg) = xdg_config_home {
            roots.push(xdg.join("claude-code").join("projects"));
        }
        roots.push(home_dir.map_or_else(
            || PathBuf::from(".claude/projects"),
            |home| home.join(".claude/projects"),
        ));
        roots.extend(Self::desktop_session_roots_resolved(home_dir));
        roots.sort();
        roots.dedup();
        roots
    }

    pub(super) fn session_files(scan_target: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in WalkDir::new(scan_target).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry.path().extension().and_then(|s| s.to_str());
            if ext == Some("jsonl") || ext == Some("json") || ext == Some("claude") {
                files.push(entry.path().to_path_buf());
            }
        }
        // Keep connector traversal deterministic across filesystems/runs.
        files.sort();
        files
    }

    pub(super) fn projects_root_for_explicit_file(path: &Path) -> Option<PathBuf> {
        path.ancestors()
            .filter(|ancestor| ancestor.is_dir())
            .find(|ancestor| {
                ancestor.file_name().and_then(|name| name.to_str()) == Some("projects")
            })
            .map(Path::to_path_buf)
    }

    pub(super) fn path_is_desktop_sidecar(path: &Path) -> bool {
        path.components().any(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some("claude-code-sessions" | "local-agent-mode-sessions")
            )
        })
    }

    pub(super) fn discovered_source_role(path: &Path) -> DiscoveredSourceRole {
        if Self::path_is_desktop_sidecar(path) {
            DiscoveredSourceRole::MetadataSidecar
        } else {
            DiscoveredSourceRole::PrimarySessionLog
        }
    }

    pub(super) fn non_empty_json_string(value: &Value, key: &str) -> Option<String> {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    pub(super) fn desktop_sidecar_metadata_message(raw: &Value) -> Option<NormalizedMessage> {
        let title = Self::non_empty_json_string(raw, "title");
        let cwd = Self::non_empty_json_string(raw, "cwd");
        let model = Self::non_empty_json_string(raw, "model");
        let cli_session_id = Self::non_empty_json_string(raw, "cliSessionId");
        let session_id = Self::non_empty_json_string(raw, "sessionId");
        let permission_mode = Self::non_empty_json_string(raw, "permissionMode");
        if title.is_none()
            && cwd.is_none()
            && model.is_none()
            && cli_session_id.is_none()
            && session_id.is_none()
        {
            return None;
        }

        let mut lines = vec!["Claude Code Desktop session metadata".to_string()];
        if let Some(title) = &title {
            lines.push(format!("Title: {title}"));
        }
        if let Some(cwd) = &cwd {
            lines.push(format!("Workspace: {cwd}"));
        }
        if let Some(model) = &model {
            lines.push(format!("Model: {model}"));
        }
        if let Some(permission_mode) = &permission_mode {
            lines.push(format!("Permission mode: {permission_mode}"));
        }
        if let Some(cli_session_id) = &cli_session_id {
            lines.push(format!("CLI session id: {cli_session_id}"));
        } else if let Some(session_id) = &session_id {
            lines.push(format!("Session id: {session_id}"));
        }
        lines.push(
            "Conversation body unavailable in this Desktop sidecar; Claude Code may have culled the CLI JSONL body."
                .to_string(),
        );

        let created_at = raw
            .get("lastActivityAt")
            .or_else(|| raw.get("createdAt"))
            .and_then(parse_timestamp);
        Some(NormalizedMessage {
            idx: 0,
            role: "system".to_string(),
            author: Some("claude_code_desktop".to_string()),
            created_at,
            content: lines.join("\n"),
            extra: serde_json::json!({
                "cass": {
                    "source": "claude_code_desktop_sidecar",
                    "body_available": false,
                    "body_cull_note": "Claude Code may auto-cull CLI JSONL bodies while Desktop sidecars retain title/workspace metadata."
                },
                "raw": raw
            }),
            invocations: Vec::new(),
            snippets: Vec::new(),
        })
    }

    pub(super) fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let looks_like_root = |path: &PathBuf| path.join("projects").exists();

        let mut roots: Vec<ScanRoot> = if ctx.use_default_detection() {
            if looks_like_root(&ctx.data_dir) {
                vec![ScanRoot::local(ctx.data_dir.clone())]
            } else {
                Self::projects_root_candidates()
                    .into_iter()
                    .map(ScanRoot::local)
                    .collect()
            }
        } else {
            ctx.scan_roots
                .iter()
                .map(|scan_root| {
                    scan_root
                        .mirrored_store(&[".claude", "projects"])
                        .map_or_else(|| scan_root.clone(), |path| scan_root.with_path(path))
                })
                .collect()
        };

        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    pub(super) fn discover_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(Self::discover_sources_with_exclusions(
            ctx,
            &excluded_scan_paths_from_env()?,
        ))
    }

    pub(super) fn discover_sources_with_exclusions(
        ctx: &ScanContext,
        excluded_paths: &[PathBuf],
    ) -> Vec<DiscoveredSourceFile> {
        let mut out = Vec::new();
        for root in Self::source_roots(ctx) {
            let scan_target = root.path.clone();
            if !scan_target.exists() {
                continue;
            }
            let session_paths = if scan_target.is_file() {
                vec![scan_target]
            } else {
                Self::session_files(&scan_target)
            };
            for path in session_paths {
                if path_is_excluded(&path, excluded_paths) {
                    tracing::debug!(
                        path = %path.display(),
                        "claude_code skipping excluded session source"
                    );
                    continue;
                }
                if !file_modified_since(&path, ctx.since_ts) {
                    continue;
                }
                out.push(
                    DiscoveredSourceFile::new(
                        "claude_code",
                        &root,
                        path.clone(),
                        Self::discovered_source_role(&path),
                        true,
                    )
                    .with_fs_metadata(),
                );
            }
        }
        out
    }
}

mod discovery;
mod parse;
mod reader;
mod segments;
mod subagent;
#[cfg(test)]
mod tests_core;
#[cfg(test)]
mod tests_edge_subagent;
mod token_usage_accumulation;
mod user_prompts;

pub use reader::{
    codex_rollout_byte_budget, is_compressed_rollout, rollout_session_path,
    set_codex_rollout_byte_budget,
};
#[cfg(feature = "codex-zstd")]
pub use reader::{compressed_rollout_declared_len, decompressed_rollout};

use anyhow::Result;

use discovery::{scan_codex_with_callback, scan_codex_with_hooks};

use super::scan::{DiscoveredSourceFile, ScanContext, SourceScanHooks};
use super::{Connector, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};

pub struct CodexConnector;

impl Default for CodexConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Connector for CodexConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("codex").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        scan_codex_with_callback(ctx, &mut |conv| {
            convs.push(conv);
            Ok(())
        })?;
        Ok(convs)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Self::discover_sources(ctx)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        scan_codex_with_callback(ctx, on_conversation)
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
        scan_codex_with_hooks(ctx, hooks, on_conversation)
    }
}

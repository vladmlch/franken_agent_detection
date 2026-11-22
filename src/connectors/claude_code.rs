use anyhow::Result;

use super::scan::{DiscoveredSourceFile, ScanContext, SourceScanHooks};
use super::utils::excluded_scan_paths_from_env;
use super::{Connector, flatten_content, franken_detection_for_connector};
use crate::types::{DetectionResult, NormalizedConversation};

#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::io::{self, BufRead, BufReader};
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
use super::scan::{ScanRoot, SourceCompletion};

mod discovery;
mod generated_title;
mod parse;
#[cfg(test)]
mod read_failures;
mod retained_content;
mod scan;
#[cfg(test)]
mod tests_core;
#[cfg(test)]
mod tests_json_edge;

pub(crate) use parse::{ClaudeFlavor, ParsedClaudeFile, parse_claude_records};
#[cfg(test)]
use scan::scan_claude_with_readers;
use scan::{scan_claude_with_callback, scan_claude_with_callback_with_exclusions};

/// Claude Code session ingestion.
///
/// Streaming scans recover healthy sources
/// after source I/O failures, then return the first failure with a count.
/// Collecting scans remain all-or-error; callback errors stop immediately.
pub struct ClaudeCodeConnector;

impl Default for ClaudeCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Connector for ClaudeCodeConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("claude_code").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        scan_claude_with_callback(ctx, &mut |conv| {
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
        scan_claude_with_callback(ctx, on_conversation)
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
        scan_claude_with_callback_with_exclusions(
            ctx,
            on_conversation,
            &excluded_scan_paths_from_env()?,
            hooks,
        )
    }
}

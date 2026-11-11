//! Local fix (cass review 2026-11-02, F5): a Devin DB that cannot be read
//! must surface as a scan error so the host keeps the watermark and retries,
//! instead of a debug log plus a successful (incomplete) scan.

use std::path::Path;

use anyhow::{Result, anyhow};

/// Accumulates failed database reads during a streaming scan so healthy
/// sessions are still emitted while the scan finishes with an error naming
/// every unreadable database.
pub(super) struct ScanFailures {
    failures: Vec<String>,
}

impl ScanFailures {
    pub(super) const fn new() -> Self {
        Self {
            failures: Vec::new(),
        }
    }

    pub(super) fn record(&mut self, path: &Path, error: &anyhow::Error) {
        tracing::warn!("devin sqlite: failed to read {}: {error:#}", path.display());
        self.failures.push(format!("{}: {error:#}", path.display()));
    }

    pub(super) fn finish(self) -> Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "Devin scan incomplete: {} database(s) failed: {}",
                self.failures.len(),
                self.failures.join("; ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn no_failures_passes_and_any_failure_fails_the_scan() {
        assert!(ScanFailures::new().finish().is_ok());

        let mut failures = ScanFailures::new();
        failures.record(Path::new("broken"), &anyhow!("node 7 vanished"));
        failures.record(Path::new("gone"), &anyhow!("permission denied"));
        let err = failures.finish().unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("2 database(s) failed"));
        assert!(rendered.contains("broken: node 7 vanished"));
        assert!(rendered.contains("gone: permission denied"));
    }
}

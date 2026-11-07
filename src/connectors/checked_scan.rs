//! Local fix (cass review 2026-11-02, F5 — generalized by F9): a store that
//! cannot be read must surface as a scan error so the host keeps the
//! watermark and retries, instead of a log line plus a successful
//! (incomplete) scan. Devin's F5 module (`devin/checked_scan.rs`) is the
//! original; this generalizes it for the remaining store-style SQLite
//! connectors (`OpenCode`, `Cursor`, `Crush`, `Hermes`, `Goose`), keeping the
//! exact F5 error contract per connector.

use std::path::Path;

use anyhow::{Result, anyhow};

use crate::types::NormalizedConversation;

/// Read every candidate database, keep the healthy results, and return
/// `Err` naming each failed path so the host keeps the watermark and
/// retries. `connector` labels the log and error messages (`OpenCode`,
/// `Cursor`, `Devin`, ...).
pub fn collect_databases_checked<P: AsRef<Path>>(
    connector: &str,
    paths: impl IntoIterator<Item = P>,
    mut load: impl FnMut(&Path) -> Result<Vec<NormalizedConversation>>,
) -> Result<Vec<NormalizedConversation>> {
    let mut conversations = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        let path = path.as_ref();
        match load(path) {
            Ok(found) => conversations.extend(found),
            Err(error) => {
                tracing::warn!(
                    "{connector} sqlite: failed to read {}: {error:#}",
                    path.display()
                );
                failures.push(format!("{}: {error:#}", path.display()));
            }
        }
    }
    if failures.is_empty() {
        Ok(conversations)
    } else {
        Err(anyhow!(
            "{connector} scan incomplete: {} database(s) failed: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn healthy_databases_pass_and_any_failure_fails_the_scan() {
        let ok = collect_databases_checked("OpenCode", ["a", "b"], |_| Ok(Vec::new()));
        assert!(ok.unwrap().is_empty());

        let mut visited = Vec::new();
        let err = collect_databases_checked("Cursor", ["a", "broken", "c"], |path| {
            visited.push(path.display().to_string());
            if path == Path::new("broken") {
                Err(anyhow!("node 7 vanished"))
            } else {
                Ok(Vec::new())
            }
        })
        .unwrap_err();
        assert_eq!(
            visited,
            ["a", "broken", "c"],
            "all databases are still read"
        );
        assert_eq!(
            err.to_string(),
            "Cursor scan incomplete: 1 database(s) failed: broken: node 7 vanished"
        );
    }
}

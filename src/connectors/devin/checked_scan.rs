//! Local fix (cass review 2026-11-02, F5): a Devin DB that cannot be read
//! must surface as a scan error so the host keeps the watermark and retries,
//! instead of a debug log plus a successful (incomplete) scan.

use std::path::Path;

use anyhow::{Result, anyhow};

use crate::types::NormalizedConversation;

pub(super) fn collect_databases_checked<P: AsRef<Path>>(
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
                tracing::warn!("devin sqlite: failed to read {}: {error:#}", path.display());
                failures.push(format!("{}: {error:#}", path.display()));
            }
        }
    }
    if failures.is_empty() {
        Ok(conversations)
    } else {
        Err(anyhow!(
            "Devin scan incomplete: {} database(s) failed: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_databases_pass_and_any_failure_fails_the_scan() {
        let ok = collect_databases_checked(["a", "b"], |_| Ok(Vec::new()));
        assert!(ok.unwrap().is_empty());

        let mut visited = Vec::new();
        let err = collect_databases_checked(["a", "broken", "c"], |path| {
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
        assert!(format!("{err:#}").contains("broken: node 7 vanished"));
    }
}

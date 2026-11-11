//! Local fix (cass review 2026-11-02, F5 — generalized by F9): a store that
//! cannot be read must surface as a scan error so the host keeps the
//! watermark and retries, instead of a log line plus a successful
//! (incomplete) scan. Devin's F5 module (`devin/checked_scan.rs`) is the
//! original; this generalizes it for the remaining store-style SQLite
//! connectors (`OpenCode`, `Cursor`, `Crush`, `Hermes`, `Goose`), keeping the
//! exact F5 error contract per connector.
//!
//! The one deliberate non-error (cass follow-up to F9, 2026-11-07): a store
//! that is a *valid but empty* database — a 0-byte main file with no `-wal`
//! content, or a schema with no tables at all — is a fresh install, not a
//! failure: it yields no sessions with `Ok`, logged at debug.

use std::path::Path;

use anyhow::{Result, anyhow};

use super::sqlite_sync;
use crate::types::NormalizedConversation;

/// The F9 empty-store rule in streaming form: whether `path` is a valid but
/// empty store that scans to `Ok` with no sessions (one debug line) instead
/// of reaching the connector's read — the same skip
/// [`collect_databases_checked`] applies before invoking `load`.
pub fn skip_provably_empty_store(connector: &str, path: &Path) -> bool {
    if sqlite_sync::store_is_provably_empty(path) {
        tracing::debug!(
            "{connector} sqlite: {} is an empty database; nothing to read",
            path.display()
        );
        true
    } else {
        false
    }
}

/// Streaming sibling of [`collect_databases_checked`] with the same F5
/// contract, for the store connectors whose `scan_with_source_boundaries`
/// walks discovered sources (master's source-boundary scan): healthy
/// sources still emit and completion hooks still fire while failures
/// accumulate, and the scan finishes with the same error naming every
/// unreadable store so the host keeps the watermark and retries. Devin's
/// `devin/checked_scan.rs` accumulator (the F5 original, kept by master)
/// has the identical contract with the connector name fixed to "Devin".
pub struct ScanFailures {
    connector: &'static str,
    failures: Vec<String>,
}

impl ScanFailures {
    pub const fn new(connector: &'static str) -> Self {
        Self {
            connector,
            failures: Vec::new(),
        }
    }

    pub fn record(&mut self, path: &Path, error: &anyhow::Error) {
        tracing::warn!(
            "{} sqlite: failed to read {}: {error:#}",
            self.connector,
            path.display()
        );
        self.failures.push(format!("{}: {error:#}", path.display()));
    }

    /// Fold an already-formatted failure message (e.g. the Cursor targeted
    /// lookup's open failures) into the same end-of-scan error.
    pub fn push(&mut self, message: String) {
        tracing::warn!("{} sqlite: {message}", self.connector);
        self.failures.push(message);
    }

    pub fn finish(self) -> Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "{} scan incomplete: {} database(s) failed: {}",
                self.connector,
                self.failures.len(),
                self.failures.join("; ")
            ))
        }
    }
}

/// Read every candidate database, keep the healthy results, and return
/// `Err` naming each failed path so the host keeps the watermark and
/// retries. `connector` labels the log and error messages (`OpenCode`,
/// `Cursor`, `Devin`, ...).
///
/// A store that is a valid but empty database never reaches `load`: there
/// is nothing to read, and a fresh install must not report a scan error on
/// every index run.
///
/// Retained from cass's collect-style scans even though master's streaming
/// architecture replaced every production caller (the streaming siblings
/// above carry the same F5/F9 contract, and `skip_provably_empty_store`
/// applies the same skip): the collect form is the reference implementation
/// of the shared error contract, and its tests below pin the full
/// empty-store matrix (0-byte, no-tables, schema drift, non-database) in
/// one place. Silence `dead_code` for the non-test build accordingly.
#[allow(dead_code)]
pub fn collect_databases_checked<P: AsRef<Path>>(
    connector: &str,
    paths: impl IntoIterator<Item = P>,
    mut load: impl FnMut(&Path) -> Result<Vec<NormalizedConversation>>,
) -> Result<Vec<NormalizedConversation>> {
    let mut conversations = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        let path = path.as_ref();
        // cass follow-up to F9 (2026-11-07): a provably empty store is a
        // fresh install, not a failure — 0 sessions, one debug line, and
        // the connector is never invoked for it.
        if sqlite_sync::store_is_provably_empty(path) {
            tracing::debug!(
                "{connector} sqlite: {} is an empty database; nothing to read",
                path.display()
            );
            continue;
        }
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
    use tempfile::TempDir;

    #[test]
    fn streaming_failures_pass_when_empty_and_fail_naming_every_store() {
        assert!(ScanFailures::new("Crush").finish().is_ok());

        let mut failures = ScanFailures::new("Crush");
        failures.record(Path::new("broken"), &anyhow!("node 7 vanished"));
        failures.record(Path::new("gone"), &anyhow!("permission denied"));
        let err = failures.finish().unwrap_err();
        let rendered = format!("{err:#}");
        assert_eq!(
            rendered,
            "Crush scan incomplete: 2 database(s) failed: broken: node 7 vanished; gone: permission denied"
        );
    }

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

    /// cass follow-up to F9 (2026-11-07): a store that is a *valid but empty*
    /// database — a 0-byte main file with no `-wal` content (SQLite's own
    /// semantics: an empty database; a fresh install commonly creates the
    /// store file before its first write) or a schema with no tables at all —
    /// is "nothing to read", so the connector is never even invoked for it.
    /// Schema drift (some tables, none of the connector's) and a non-database
    /// file still surface as scan errors naming each store.
    #[test]
    fn provably_empty_stores_are_skipped_without_calling_the_connector() {
        let dir = TempDir::new().unwrap();

        // A fresh install before its first write: the store file exists but
        // holds nothing.
        let zero_byte = dir.path().join("zero.db");
        std::fs::write(&zero_byte, []).unwrap();

        // A real database file whose schema holds no tables at all.
        let no_tables = dir.path().join("no_tables.db");
        {
            let conn = crate::connectors::sqlite_sync::Connection::open(
                no_tables.to_string_lossy().as_ref(),
            )
            .unwrap();
            conn.execute("CREATE TABLE schema_marker (x INTEGER);")
                .unwrap();
            conn.execute("DROP TABLE schema_marker;").unwrap();
            drop(conn);
            assert!(
                std::fs::metadata(&no_tables).unwrap().len() > 0,
                "the fixture must be a real database file, not 0 bytes"
            );
        }

        // Schema drift: some tables, none of this connector's.
        let drift = dir.path().join("drift.db");
        {
            let conn =
                crate::connectors::sqlite_sync::Connection::open(drift.to_string_lossy().as_ref())
                    .unwrap();
            conn.execute("CREATE TABLE unrelated_table (x INTEGER);")
                .unwrap();
            drop(conn);
        }

        // Not a database at all.
        let garbage = dir.path().join("garbage.db");
        std::fs::write(&garbage, b"this is not a sqlite database").unwrap();

        let mut visited = Vec::new();
        let error = collect_databases_checked(
            "OpenCode",
            [&zero_byte, &no_tables, &drift, &garbage],
            |path| {
                visited.push(path.display().to_string());
                Err(anyhow!("connector read failed"))
            },
        )
        .unwrap_err();
        assert_eq!(
            visited,
            [drift.display().to_string(), garbage.display().to_string(),],
            "only the non-empty stores reach the connector"
        );
        let chain = format!("{error:#}");
        assert!(
            chain.contains("drift.db"),
            "schema drift must surface as a scan error: {chain}"
        );
        assert!(
            chain.contains("garbage.db"),
            "a non-database file must surface as a scan error: {chain}"
        );

        // With only the provably empty stores the scan is `Ok` with nothing
        // read: the connector is never invoked.
        let empty = collect_databases_checked("OpenCode", [&zero_byte, &no_tables], |path| {
            panic!("the connector must not be called for {}", path.display())
        })
        .unwrap();
        assert!(empty.is_empty());
    }
}

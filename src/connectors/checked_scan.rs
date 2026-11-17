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

    /// Verify that a symlink at `path` resolves to an accessible target via
    /// `std::fs::metadata`.
    ///
    /// Returns `true` when `metadata(path)` succeeds. Returns `false` and
    /// records a failure in `self` when resolving the target fails (e.g.,
    /// `NotFound` for a dangling symlink or `PermissionDenied` for an
    /// unreadable target).
    #[allow(dead_code)]
    pub fn verify_symlink_target(&mut self, path: &Path) -> bool {
        match std::fs::metadata(path) {
            Ok(_) => true,
            Err(error) => {
                let reason = match error.kind() {
                    std::io::ErrorKind::NotFound => "dangling symlink",
                    std::io::ErrorKind::PermissionDenied => "unreadable symlink target",
                    _ => "broken symlink target",
                };
                self.record(path, &anyhow!("{reason}: {error}"));
                false
            }
        }
    }

    /// Inspect a single [`walkdir::WalkDir`] iterator item, returning
    /// `Some(entry)` when healthy or recording a scan failure and returning
    /// `None` when the walk hit an I/O error or encountered a broken/dangling
    /// or unreadable symlink.
    ///
    /// Replaces `WalkDir::into_iter().flatten()`, which silently drops walk
    /// errors and leaves unfollowed broken symlink subtrees unreported as zero
    /// sessions.
    #[allow(dead_code)]
    pub fn walk_dir_entry_or_record_failure(
        &mut self,
        item: std::result::Result<walkdir::DirEntry, walkdir::Error>,
        fallback_path: &Path,
    ) -> Option<walkdir::DirEntry> {
        match item {
            Ok(entry) => {
                if entry.path_is_symlink() && !self.verify_symlink_target(entry.path()) {
                    return None;
                }
                Some(entry)
            }
            Err(error) => {
                let path = error
                    .path()
                    .map_or_else(|| fallback_path.to_path_buf(), Path::to_path_buf);
                self.record(&path, &anyhow::Error::new(error));
                None
            }
        }
    }

    /// Check whether a direct candidate path is a regular file, recording a
    /// scan failure when `path` is a dangling symlink (`metadata` fails with
    /// `NotFound` while `symlink_metadata` succeeds) or fails to stat for any
    /// reason other than plain absence.
    #[allow(dead_code)]
    pub fn check_candidate_file_or_record_failure(&mut self, path: &Path) -> bool {
        match std::fs::metadata(path) {
            Ok(meta) => meta.is_file(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(path) {
                    Ok(_) => {
                        self.record(path, &anyhow!("dangling symlink: {error}"));
                    }
                    Err(lstat_error) if lstat_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(lstat_error) => {
                        self.record(
                            path,
                            &anyhow!("failed to inspect symlink metadata: {lstat_error}"),
                        );
                    }
                }
                false
            }
            Err(error) => {
                self.record(path, &anyhow!("failed to stat candidate path: {error}"));
                false
            }
        }
    }
}

/// Free-function helper delegating to
/// [`ScanFailures::walk_dir_entry_or_record_failure`].
#[allow(dead_code)]
pub fn walk_dir_entry_or_record_failure(
    failures: &mut ScanFailures,
    item: std::result::Result<walkdir::DirEntry, walkdir::Error>,
    fallback_path: &Path,
) -> Option<walkdir::DirEntry> {
    failures.walk_dir_entry_or_record_failure(item, fallback_path)
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

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_during_walk_is_recorded_as_scan_failure() {
        use std::os::unix::fs::symlink;
        use walkdir::WalkDir;

        let dir = TempDir::new().unwrap();
        let healthy = dir.path().join("healthy.jsonl");
        std::fs::write(&healthy, b"{}\n").unwrap();

        let broken_dir = dir.path().join("broken_subtree");
        symlink(dir.path().join("does_not_exist"), &broken_dir).unwrap();

        // Both follow_links(false) and follow_links(true) must record the
        // broken subtree rather than silently swallowing it.
        for follow in [false, true] {
            let mut failures = ScanFailures::new("TestConnector");
            let mut visited = Vec::new();
            for item in WalkDir::new(dir.path()).follow_links(follow) {
                if let Some(entry) =
                    walk_dir_entry_or_record_failure(&mut failures, item, dir.path())
                    && entry.file_type().is_file()
                {
                    visited.push(entry.path().to_path_buf());
                }
            }
            assert_eq!(visited, vec![healthy.clone()]);
            let err = failures.finish().unwrap_err();
            let rendered = format!("{err:#}");
            assert!(
                rendered.contains("broken_subtree"),
                "follow={follow}: expected broken_subtree in failure: {rendered}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_and_symlink_target_are_recorded_as_scan_failures() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        use walkdir::WalkDir;

        struct RestorePermissions<'a>(&'a std::path::Path);
        impl Drop for RestorePermissions<'_> {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
            }
        }

        let dir = TempDir::new().unwrap();
        let healthy = dir.path().join("ok.jsonl");
        std::fs::write(&healthy, b"{}\n").unwrap();

        let locked = dir.path().join("locked_dir");
        std::fs::create_dir(&locked).unwrap();
        let inner_target = locked.join("secret");
        std::fs::create_dir(&inner_target).unwrap();
        let link_into_locked = dir.path().join("link_into_locked");
        symlink(&inner_target, &link_into_locked).unwrap();

        // Also include a healthy symlink that must not trigger a failure.
        let healthy_dir = dir.path().join("real_dir");
        std::fs::create_dir(&healthy_dir).unwrap();
        let healthy_link = dir.path().join("healthy_link");
        symlink(&healthy_dir, &healthy_link).unwrap();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = RestorePermissions(&locked);

        // Only run permission assertions if the current user cannot bypass 0o000.
        if std::fs::read_dir(&locked).is_err() {
            let mut failures = ScanFailures::new("TestConnector");
            let mut visited = Vec::new();
            for item in WalkDir::new(dir.path()) {
                if let Some(entry) = failures.walk_dir_entry_or_record_failure(item, dir.path())
                    && entry.file_type().is_file()
                {
                    visited.push(entry.path().to_path_buf());
                }
            }
            assert_eq!(visited, vec![healthy]);
            let err = failures.finish().unwrap_err();
            let rendered = format!("{err:#}");
            assert!(
                rendered.contains("locked_dir"),
                "expected locked_dir in failure: {rendered}"
            );
            assert!(
                rendered.contains("link_into_locked"),
                "expected link_into_locked in failure: {rendered}"
            );
            assert!(
                !rendered.contains("healthy_link"),
                "healthy_link must not be reported as failed: {rendered}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_candidate_file_distinguishes_absent_from_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("absent.db");
        let regular = dir.path().join("present.db");
        std::fs::write(&regular, b"x").unwrap();
        let dangling = dir.path().join("dangling.db");
        symlink(dir.path().join("missing_target.db"), &dangling).unwrap();

        let mut failures = ScanFailures::new("OpenCode");
        assert!(!failures.check_candidate_file_or_record_failure(&missing));
        assert!(failures.check_candidate_file_or_record_failure(&regular));
        assert!(
            ScanFailures::new("OpenCode").finish().is_ok(),
            "sanity check"
        );
        // Before probing dangling, failures must still be empty.
        assert!(!failures.check_candidate_file_or_record_failure(&dangling));
        let err = failures.finish().unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("dangling.db"),
            "dangling symlink candidate must be recorded: {rendered}"
        );
    }
}

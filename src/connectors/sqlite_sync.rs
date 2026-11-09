//! Synchronous bridge over the async `FrankenSQLite` 0.2 engine API.
//!
//! fsqlite 0.2 made every engine entry point `async` with `!Send` futures
//! (the engine is `Rc<RefCell<..>>` internally). The connectors are fully
//! synchronous, so this module drives each fsqlite future to completion on
//! the calling thread with a private current-thread `asupersync` runtime.
//!
//! Every future is created, polled, and dropped entirely within one bridge
//! call, so the engine's `Rc<RefCell<..>>` state never crosses a thread
//! boundary. `Runtime::block_on` has no `Send` bound and saves/restores the
//! ambient runtime handle, so nested use from inside a consumer's own
//! `block_on` is safe (proven by the sqlmodel-frankensqlite
//! `nested_block_on_*` probes for the same bridge pattern).
//!
//! The runtime is kept in a thread-local slot and *taken out* while a future
//! is being driven: a reentrant bridge call (e.g. from a row-mapping closure)
//! finds the slot empty and builds a fresh runtime instead of re-entering
//! `block_on` on the same runtime instance.

use std::cell::RefCell;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use asupersync::runtime::{Runtime, RuntimeBuilder};
use frankensqlite::FrankenError;
use frankensqlite::Row;
use frankensqlite::compat::ConnectionExt as AsyncConnectionExt;
use frankensqlite::compat::{OpenFlags, ParamValue};

thread_local! {
    static DRIVER: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Monotonic suffix for private snapshot-directory names (F9). No external
/// crates: the name only has to be unique among this process's snapshot
/// dirs; `std::fs::create_dir` refuses an existing name.
static SNAPSHOT_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Drive a `!Send` fsqlite future to completion on the calling thread.
fn drive<T>(future: impl Future<Output = T>) -> Result<T, FrankenError> {
    let runtime = match DRIVER.with(|slot| slot.borrow_mut().take()) {
        Some(runtime) => runtime,
        None => RuntimeBuilder::current_thread().build().map_err(|error| {
            FrankenError::Internal(format!("failed to build SQLite connector runtime: {error}"))
        })?,
    };
    let output = runtime.block_on(future);
    DRIVER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(runtime);
        }
    });
    Ok(output)
}

/// Shut down this thread's cached connector runtime, waiting up to 30 s for
/// its worker threads. Returns `false` if they did not stop in time.
///
/// Call it on a thread that used a SQLite connector before that thread, or
/// the process, ends. On Windows the main thread's thread-local destructors
/// run during process exit, after the OS has already terminated every other
/// thread. A runtime still cached then cannot join its blocking-pool threads,
/// and the panic that follows aborts the process (exit 0xC0000409) after its
/// work succeeded (cass GH #519). Drop the thread's connections first: one
/// still open keeps the runtime busy, and shutdown then waits out the
/// timeout. A later bridge call builds a new runtime.
#[must_use]
pub fn shutdown_driver() -> bool {
    DRIVER
        .with(|slot| slot.borrow_mut().take())
        .is_none_or(|runtime| runtime.shutdown_timeout(std::time::Duration::from_secs(30)))
}

/// RAII owner of the private directory holding a store snapshot copy (F9).
///
/// Dropping the guard removes the directory, best effort. It is held in
/// [`Connection::snapshot`] as the last field, so the engine connection is
/// closed before the copy's files are removed.
struct SnapshotDir {
    dir: PathBuf,
}

impl Drop for SnapshotDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            tracing::warn!(
                dir = %self.dir.display(),
                %error,
                "could not remove the private SQLite snapshot copy"
            );
        }
    }
}

/// Path of a SQLite sidecar file (`<db>-wal`, `<db>-shm`).
fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let mut raw = db.as_os_str().to_owned();
    raw.push(suffix);
    PathBuf::from(raw)
}

/// Size + mtime of one store file, to detect an app writing while we copy.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StableStamp {
    len: u64,
    mtime: SystemTime,
}

/// [`StableStamp`] of `path`, or `None` when the file does not exist.
fn stable_stamp(path: &Path) -> Result<Option<StableStamp>, FrankenError> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(StableStamp {
            len: meta.len(),
            mtime: meta.modified()?,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Create a fresh private directory for one store snapshot copy.
fn fresh_snapshot_dir() -> Result<SnapshotDir, FrankenError> {
    let base = std::env::temp_dir();
    for _ in 0..8 {
        let sequence = SNAPSHOT_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "fad-sqlite-snapshot-{}-{}",
            std::process::id(),
            sequence
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(SnapshotDir { dir }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(FrankenError::Internal(
        "could not create a private SQLite snapshot directory".to_owned(),
    ))
}

/// Copy the main db file and its `-wal` (never its `-shm`) into a fresh
/// private directory, then open the copy with WAL-index recovery allowed —
/// on our copy rebuilding the wal-index is fine, unlike on the user's
/// store.
///
/// The copy is taken under a size+mtime stability stamp of the db and
/// `-wal` recorded before and re-checked after the copy, so a store the
/// app is actively writing is retried once and then refused with a clear
/// error instead of being read half-written.
fn open_snapshot_copy(db_path: &Path) -> Result<Connection, FrankenError> {
    let file_name = db_path.file_name().ok_or_else(|| {
        FrankenError::Internal(format!(
            "cannot snapshot a non-file SQLite path: {}",
            db_path.display()
        ))
    })?;
    let wal_path = sidecar_path(db_path, "-wal");
    for attempt in 0..2 {
        let before_db = stable_stamp(db_path)?;
        let before_wal = stable_stamp(&wal_path)?;
        let Some(before_db) = before_db else {
            return Err(FrankenError::Internal(format!(
                "the store vanished before it could be snapshotted: {}",
                db_path.display()
            )));
        };
        let snapshot = fresh_snapshot_dir()?;
        let copy_db = snapshot.dir.join(file_name);
        std::fs::copy(db_path, &copy_db)?;
        if wal_path.is_file() {
            std::fs::copy(&wal_path, sidecar_path(&copy_db, "-wal"))?;
        }
        let stable = stable_stamp(db_path)?.is_some_and(|after| after == before_db)
            && stable_stamp(&wal_path)? == before_wal;
        if !stable {
            // The app wrote while we copied: discard this copy and retry once.
            drop(snapshot);
            if attempt == 1 {
                return Err(FrankenError::Internal(format!(
                    "the store changed while copying it for a read-only snapshot ({}); \
                     retry the scan when the app is idle",
                    db_path.display()
                )));
            }
            continue;
        }
        let inner = drive(
            frankensqlite::Connection::open_schema_only_with_wal_index_recovery(
                copy_db.to_string_lossy().into_owned(),
            ),
        )??;
        return Ok(Connection {
            inner,
            snapshot: Some(snapshot),
        });
    }
    unreachable!("the snapshot retry loop returns from both of its arms");
}

/// Open an external app store strictly read-only, the way every FAD SQLite
/// connector does, and fall back to a private snapshot copy when the
/// store's WAL-index is unusable (cass F9).
///
/// The open uses the fork's never-write mode
/// (`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_STRICT_READ_ONLY`): the engine may
/// not create or modify any sidecar of the store, so it never performs
/// WAL-index recovery. When the `-shm` wal-index is absent, zeroed, empty,
/// torn, or garbage (typical after the app crashed) and recovery would be
/// needed to read — a `-wal` with committed frames, or any damaged `-shm`
/// beside one — the open fails with the typed
/// [`FrankenError::BusyRecovery`]. Registry fsqlite 0.4.13 instead rebuilds
/// a never-initialized `-shm` in place on a plain read-only open (GH#528),
/// which would touch the user's files; the strict bit keeps that from ever
/// happening. One read stays available without any write: a header-less
/// (0-byte) `-wal` with no `-shm` reads the main file directly, exactly
/// like stock SQLite (GH#515), so a cleanly checkpointed store needs no
/// snapshot. The user's files are a personal archive and are never
/// modified, not even the `-shm` sidecar. When the open is refused, the db
/// and its `-wal` are copied into a private temporary directory (never the
/// `-shm`), the copy is opened with WAL-index recovery allowed (it rebuilds
/// only the copy's own `-shm`), and the copy is removed when the connection
/// drops.
///
/// Any other open error — and a fallback that itself fails — is returned
/// so the caller's scan surfaces it as an error instead of hiding it.
pub fn open_read_only_with_snapshot_fallback(path: &str) -> Result<Connection, FrankenError> {
    match open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_STRICT_READ_ONLY,
    ) {
        Ok(connection) => Ok(connection),
        Err(FrankenError::BusyRecovery) => {
            let db_path = Path::new(path);
            // The unusable-WAL-index class requires a `-wal` (or at least a
            // `-shm`) next to the db; without sidecars this BusyRecovery is
            // not that class, and there is nothing to snapshot.
            if !sidecar_path(db_path, "-wal").is_file() && !sidecar_path(db_path, "-shm").is_file()
            {
                return Err(FrankenError::BusyRecovery);
            }
            tracing::warn!(
                store = path,
                "SQLite store has an unusable WAL-index for a read-only open; \
                 reading a private snapshot copy (the store's files are never modified)"
            );
            open_snapshot_copy(db_path)
        }
        Err(error) => Err(error),
    }
}

/// The SQLite database file magic: bytes 0..16 of a database header
/// (`SQLite format 3\0`). A file without it is not a database, however an
/// engine build might otherwise treat it.
const SQLITE_HEADER_MAGIC: [u8; 16] = *b"SQLite format 3\0";

/// cass follow-up to F9 (2026-11-07): whether a store database is
/// *provably empty* —
///
/// * a 0-byte main file with no `-wal` content: SQLite's own semantics make
///   a 0-byte file an empty database, and a fresh install commonly creates
///   the store file before its first write, or
/// * a database file (header magic checked, so a large all-zeros file never
///   passes) that opens read-only and whose schema holds no tables at all.
///
/// Both hold no sessions for any connector, so a scan reports them as
/// "nothing to read" instead of a scan error on every index run.
///
/// Returns `false` for everything uncertain, so the caller falls through to
/// its own read and surfaces its error exactly as before: garbage bytes and
/// wrong header magic (including a large all-zeros file), a non-empty file
/// too small for a header, a schema with *some* tables (schema drift, e.g.
/// an app migration — the connector's own read must name it), and any open
/// failure. A store whose wal-index is unusable (`BusyRecovery`) is
/// deliberately not classified here: proving its emptiness would cost a
/// snapshot copy of the store, so it falls through to the connector's
/// [`open_read_only_with_snapshot_fallback`] read instead.
#[must_use]
pub fn store_is_provably_empty(db_path: &Path) -> bool {
    use std::io::Read as _;

    // Case 1: a 0-byte main file. Any bytes at all in the `-wal` could hold
    // committed frames, so only an absent or 0-byte `-wal` proves emptiness.
    let main_is_zero_bytes =
        std::fs::metadata(db_path).is_ok_and(|meta| meta.is_file() && meta.len() == 0);
    if main_is_zero_bytes {
        let wal_has_content =
            std::fs::metadata(sidecar_path(db_path, "-wal")).is_ok_and(|meta| meta.len() > 0);
        return !wal_has_content;
    }

    // Case 2: a real database file whose schema holds no tables at all.
    let mut magic = [0_u8; 16];
    let header_is_sqlite = std::fs::File::open(db_path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == SQLITE_HEADER_MAGIC;
    if !header_is_sqlite {
        return false;
    }
    // A strict read-only open only (see above for why no snapshot fallback),
    // so even this probe can never write a sidecar of the user's store.
    let Ok(connection) = open_with_flags(
        db_path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_STRICT_READ_ONLY,
    ) else {
        return false;
    };
    connection
        .query_map_collect(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' LIMIT 1",
            &[],
            |_row| Ok(()),
        )
        .is_ok_and(|tables| tables.is_empty())
}

/// Synchronous wrapper over [`frankensqlite::Connection`].
///
/// Exposes the subset of the engine surface the connectors use, with the
/// pre-0.2 blocking signatures.
pub struct Connection {
    inner: frankensqlite::Connection,
    /// Private snapshot copy backing this connection (F9), removed on drop.
    /// Declared after `inner` so the engine connection closes first.
    snapshot: Option<SnapshotDir>,
}

impl Connection {
    /// Open (or create) a database at `path`.
    pub fn open(path: &str) -> Result<Self, FrankenError> {
        Ok(Self {
            inner: drive(frankensqlite::Connection::open(path))??,
            snapshot: None,
        })
    }

    /// Whether this connection reads a private snapshot copy of the store
    /// (the unusable-WAL-index fallback) rather than the store itself.
    #[must_use]
    pub const fn used_snapshot_fallback(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The private directory holding the snapshot copy this connection
    /// reads, when the fallback was used. It is removed when the
    /// connection drops.
    #[must_use]
    pub fn snapshot_dir(&self) -> Option<&Path> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.dir.as_path())
    }

    /// Execute a single SQL statement, returning the affected row count.
    pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
        drive(self.inner.execute(sql))?
    }

    /// Execute a string of semicolon-separated SQL statements.
    pub fn execute_batch(&self, sql: &str) -> Result<(), FrankenError> {
        drive(self.inner.execute_batch(sql))?
    }

    /// Run `f` inside one deferred read transaction so every query it issues
    /// observes a single coherent committed snapshot, then end the
    /// transaction promptly (a held read transaction blocks the writer's WAL
    /// checkpoint).
    ///
    /// On success the transaction is committed (a no-op for reads); if `f`
    /// or the commit fails, the transaction is rolled back best-effort.
    /// If `f` unwinds, rollback is attempted before resuming its original panic.
    pub fn read_transaction<T, E>(&self, f: impl FnOnce(&Self) -> Result<T, E>) -> Result<T, E>
    where
        E: From<FrankenError>,
    {
        self.execute("BEGIN DEFERRED;").map_err(E::from)?;
        // The callback is never resumed after an unwind. Keep its existing
        // unconstrained signature while releasing this transaction before the
        // caller can catch the panic and reuse the connection.
        let result = match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(result) => result,
            Err(payload) => {
                // A cleanup panic must not replace the callback's payload.
                let _ = catch_unwind(AssertUnwindSafe(|| self.execute("ROLLBACK;")));
                resume_unwind(payload);
            }
        };
        match result {
            Ok(value) => {
                if let Err(err) = self.execute("COMMIT;") {
                    let _ = self.execute("ROLLBACK;");
                    return Err(E::from(err));
                }
                Ok(value)
            }
            Err(err) => {
                let _ = self.execute("ROLLBACK;");
                Err(err)
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // fsqlite 0.2 no longer checkpoints on drop, and read-only opens are
        // mutation-free (GH#294): they do not recover an unpublished WAL
        // sidecar. Closing in place on drop restores the pre-0.2 observable
        // contract that writes made through a dropped connection are visible
        // to a later read-only open.
        if let Err(error) = drive(self.inner.close_best_effort_in_place()) {
            tracing::warn!(%error, "could not drive SQLite connector close");
        }
    }
}

/// Open a database with rusqlite-style open flags (synchronous form of
/// [`frankensqlite::compat::open_with_flags`]).
///
/// Strict engine semantics: no snapshot fallback. External app stores are
/// opened through [`open_read_only_with_snapshot_fallback`] instead.
pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection, FrankenError> {
    Ok(Connection {
        inner: drive(frankensqlite::compat::open_with_flags(path, flags))??,
        snapshot: None,
    })
}

/// Synchronous form of [`frankensqlite::compat::ConnectionExt`].
pub trait ConnectionExt {
    /// Execute a query that returns exactly one row, mapping it with `f`.
    fn query_row_map<T, F>(
        &self,
        sql: &str,
        params: &[ParamValue],
        f: F,
    ) -> Result<T, FrankenError>
    where
        F: FnOnce(&Row) -> Result<T, FrankenError>;

    /// Execute a query and collect all rows into a `Vec<T>` via mapping closure.
    fn query_map_collect<T, F>(
        &self,
        sql: &str,
        params: &[ParamValue],
        f: F,
    ) -> Result<Vec<T>, FrankenError>
    where
        F: FnMut(&Row) -> Result<T, FrankenError>;

    /// Execute a SQL statement with `ParamValue` parameters.
    fn execute_compat(&self, sql: &str, params: &[ParamValue]) -> Result<usize, FrankenError>;
}

impl ConnectionExt for Connection {
    fn query_row_map<T, F>(&self, sql: &str, params: &[ParamValue], f: F) -> Result<T, FrankenError>
    where
        F: FnOnce(&Row) -> Result<T, FrankenError>,
    {
        drive(AsyncConnectionExt::query_row_map(
            &self.inner,
            sql,
            params,
            f,
        ))?
    }

    fn query_map_collect<T, F>(
        &self,
        sql: &str,
        params: &[ParamValue],
        f: F,
    ) -> Result<Vec<T>, FrankenError>
    where
        F: FnMut(&Row) -> Result<T, FrankenError>,
    {
        drive(AsyncConnectionExt::query_map_collect(
            &self.inner,
            sql,
            params,
            f,
        ))?
    }

    fn execute_compat(&self, sql: &str, params: &[ParamValue]) -> Result<usize, FrankenError> {
        drive(AsyncConnectionExt::execute_compat(&self.inner, sql, params))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{Cx, cx::CapMask};
    use frankensqlite::compat::RowExt;
    use serial_test::serial;

    fn driver_cached() -> bool {
        DRIVER.with(|slot| slot.borrow().is_some())
    }

    /// Snapshot copies this process has under the system temp dir.
    fn leaked_snapshot_dirs() -> Vec<PathBuf> {
        std::fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("fad-sqlite-snapshot-")
                    })
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Path of a SQLite sidecar file (`<db>-wal`, `<db>-shm`).
    fn sidecar(db: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        let mut raw = db.as_os_str().to_owned();
        raw.push(suffix);
        std::path::PathBuf::from(raw)
    }

    /// A WAL-mode database whose writer exited after a TRUNCATE checkpoint,
    /// leaving a `-wal` and a `-shm` behind.
    fn wal_mode_store(dir: &std::path::Path) -> std::path::PathBuf {
        let db = dir.join("store.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        let mode: String = conn
            .query_row_map("PRAGMA journal_mode=wal;", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (7);")
            .unwrap();
        conn.query_row_map::<(i64, i64, i64), _>("PRAGMA wal_checkpoint(TRUNCATE);", &[], |row| {
            Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?))
        })
        .unwrap();
        drop(conn);
        assert!(sidecar(&db, "-wal").is_file());
        assert!(sidecar(&db, "-shm").is_file());
        db
    }

    /// Leave the `-shm` wal-index in one of the states a crashed writer can:
    /// `garbage` bytes, a `torn` header checksum (one flipped bit), `absent`,
    /// `zeroed`, or `empty`.
    fn damage_shm(db: &std::path::Path, state: &str) {
        let shm = sidecar(db, "-shm");
        match state {
            "garbage" => std::fs::write(&shm, vec![0xA5_u8; 32_768]).unwrap(),
            "torn" => {
                let mut bytes = std::fs::read(&shm).unwrap();
                assert!(bytes.len() > 24, "the fixture `-shm` must have a header");
                bytes[16] ^= 0x01;
                std::fs::write(&shm, bytes).unwrap();
            }
            "absent" => std::fs::remove_file(&shm).unwrap(),
            "zeroed" => std::fs::write(&shm, vec![0_u8; 32_768]).unwrap(),
            "empty" => std::fs::write(&shm, []).unwrap(),
            other => unreachable!("unknown state: {other}"),
        }
    }

    /// `(bytes, mtime)` of the db / `-wal` / `-shm` trio, for never-modify
    /// assertions.
    fn trio_state(db: &std::path::Path) -> Vec<(Option<Vec<u8>>, Option<std::time::SystemTime>)> {
        ["", "-wal", "-shm"]
            .iter()
            .map(|suffix| {
                let path = sidecar(db, suffix);
                (
                    std::fs::read(&path).ok(),
                    std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok(),
                )
            })
            .collect()
    }

    /// The names of every entry in the store's directory, for the
    /// no-file-created assertion: a refused or strict open must not leave
    /// any new sidecar (`-shm`, `-journal`, the MVCC companion, ...) behind.
    fn directory_names(db: &std::path::Path) -> Vec<String> {
        let dir = db.parent().expect("the store has a parent directory");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the store's directory")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// A strict read-only open never performs WAL-index recovery and never
    /// writes any sidecar: with the `-shm` wal-index absent, garbage, torn,
    /// zeroed, or empty, the open fails with the typed `BusyRecovery` and
    /// the db / `-wal` / `-shm` bytes and mtimes are untouched, with no
    /// file created next to the store. This is the premise the snapshot
    /// fallback (cass F9) exists for. Registry fsqlite 0.4.13 rebuilds the
    /// zeroed and empty classes in place on a *plain* read-only open
    /// (GH#528); the strict bit refuses them instead, so the original files
    /// survive every class byte- and mtime-identically. (`absent` refuses
    /// too: this fixture's checkpoint leaves a valid 32-byte WAL header, and
    /// a strict open never creates the missing `-shm`. The one no-write
    /// read — a 0-byte header-less `-wal` with no `-shm`, GH#515 — is
    /// covered by the engine's own keepers.)
    #[test]
    fn strict_read_only_open_refuses_an_unusable_wal_index() {
        for state in ["garbage", "torn", "absent", "zeroed", "empty"] {
            let dir = tempfile::TempDir::new().unwrap();
            let db = wal_mode_store(dir.path());
            damage_shm(&db, state);
            let before_trio = trio_state(&db);
            let before_names = directory_names(&db);

            let Err(error) = open_with_flags(
                db.to_string_lossy().as_ref(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_STRICT_READ_ONLY,
            ) else {
                panic!("{state}: the strict open must refuse an unusable wal-index");
            };
            assert!(
                matches!(error, FrankenError::BusyRecovery),
                "{state}: expected the typed BusyRecovery, got {error:?}"
            );
            assert_eq!(
                trio_state(&db),
                before_trio,
                "{state}: a refused strict open must leave the db, `-wal`, and `-shm` \
                 byte- and mtime-identical"
            );
            assert_eq!(
                directory_names(&db),
                before_names,
                "{state}: a refused strict open must not create any file"
            );
            if state == "absent" {
                assert!(
                    !sidecar(&db, "-shm").exists(),
                    "{state}: a refused strict open must not create the missing `-shm`"
                );
            }
        }
    }

    /// The snapshot fallback (cass F9) reads a store left with an unusable
    /// wal-index through a private copy. The contract, for every damaged
    /// state and on every frankensqlite build:
    ///   * the store's rows are readable in every state;
    ///   * the fallback engages exactly when the strict read-only open
    ///     refuses the store (the fork's strict bit refuses every class
    ///     this fixture can produce — `absent` included, since its
    ///     checkpoint leaves a valid 32-byte WAL header that a strict open
    ///     never builds an `-shm` for; engine builds without the strict bit
    ///     may instead accept a never-initialized class and rebuild it in
    ///     place, which is exactly what the fallback protects against);
    ///   * when it engages, the original db / `-wal` / `-shm` bytes and
    ///     mtimes never change, no new file appears next to the original,
    ///     the copy exists while the connection lives, and it is removed
    ///     when the connection drops.
    #[serial]
    #[test]
    fn snapshot_fallback_reads_the_store_and_cleans_up_after_itself() {
        for state in ["garbage", "torn", "absent", "zeroed", "empty"] {
            let dir = tempfile::TempDir::new().unwrap();
            let db = wal_mode_store(dir.path());
            damage_shm(&db, state);

            // Whether this engine's strict open refuses the damaged store.
            // The probe uses the strict bit, so on the fork every class
            // that would need a WAL-index write is refused; an engine
            // without it may accept a class and rebuild the
            // never-initialized `-shm` in place, so capture the originals
            // only afterwards.
            let strict = open_with_flags(
                db.to_string_lossy().as_ref(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_STRICT_READ_ONLY,
            );
            let strict_refused = strict.is_err();
            drop(strict.ok());
            let before = trio_state(&db);
            let before_names = directory_names(&db);

            let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
                .unwrap_or_else(|error| panic!("{state}: fallback failed: {error:?}"));
            assert_eq!(
                conn.used_snapshot_fallback(),
                strict_refused,
                "{state}: the fallback must engage exactly when the strict open refuses"
            );
            let value: i64 = conn
                .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
                .unwrap_or_else(|error| panic!("{state}: query failed: {error:?}"));
            assert_eq!(value, 7, "{state}: the store's rows must be readable");

            if strict_refused {
                let snapshot_dir = conn
                    .snapshot_dir()
                    .unwrap_or_else(|| panic!("{state}: a snapshot copy must exist"))
                    .to_path_buf();
                assert!(snapshot_dir.is_dir(), "{state}");
                assert!(
                    snapshot_dir.join("store.db").is_file(),
                    "{state}: the db copy must exist"
                );

                drop(conn);
                assert!(
                    !snapshot_dir.exists(),
                    "{state}: the snapshot copy must be removed when the connection drops"
                );
                assert_eq!(
                    trio_state(&db),
                    before,
                    "{state}: the original db, `-wal`, and `-shm` must never be modified"
                );
                assert_eq!(
                    directory_names(&db),
                    before_names,
                    "{state}: no new file may appear next to the original store"
                );
                if state == "absent" {
                    assert!(
                        !sidecar(&db, "-shm").exists(),
                        "{state}: no `-shm` may be created next to the original store"
                    );
                }
            } else {
                assert!(
                    conn.snapshot_dir().is_none(),
                    "{state}: no snapshot copy is taken when the strict open succeeds"
                );
                drop(conn);
                // Nothing is ever written by a strict read-only open.
                assert_eq!(
                    trio_state(&db),
                    before,
                    "{state}: the db, `-wal`, and `-shm` must never be modified"
                );
                assert_eq!(
                    directory_names(&db),
                    before_names,
                    "{state}: no new file may appear next to the original store"
                );
            }
        }
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no snapshot copy may outlive its connection"
        );
    }

    /// A non-WAL-index open failure is passed through unchanged: a file that
    /// is not a database surfaces as the engine's own error, and no
    /// snapshot copy is taken.
    #[serial]
    #[test]
    fn snapshot_fallback_passes_other_open_failures_through() {
        let dir = tempfile::TempDir::new().unwrap();
        let not_a_db = dir.path().join("not-a-db");
        std::fs::write(&not_a_db, b"this is not a sqlite database").unwrap();

        let conn = open_read_only_with_snapshot_fallback(not_a_db.to_string_lossy().as_ref());
        assert!(
            conn.is_err(),
            "a file that is not a database must surface as an open error"
        );
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no snapshot copy may be taken for a non-WAL-index failure"
        );
    }

    /// cass GH #519: a runtime left in the thread-local slot is dropped by
    /// the process-exit destructors on Windows and aborts the process.
    /// `shutdown_driver` must empty the slot, and the bridge must keep
    /// working after it.
    #[test]
    fn shutdown_driver_releases_the_cached_runtime_and_the_bridge_still_works() {
        std::thread::spawn(|| {
            assert!(shutdown_driver(), "nothing cached yet");
            assert!(!driver_cached());
            let connection = Connection::open(":memory:").unwrap();
            connection
                .execute_batch("CREATE TABLE t (v INTEGER)")
                .unwrap();
            drop(connection);
            assert!(driver_cached(), "a bridge call caches its runtime");
            let started = std::time::Instant::now();
            assert!(shutdown_driver());
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "an idle runtime shuts down promptly"
            );
            assert!(!driver_cached(), "shutdown must release the runtime");
            let connection = Connection::open(":memory:").unwrap();
            connection
                .execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
            assert!(driver_cached(), "a later call builds a new runtime");
            let count: i64 = connection
                .query_row_map("SELECT count(*) FROM t", &[], |row| row.get_typed(0))
                .unwrap();
            assert_eq!(count, 1);
            drop(connection);
            assert!(shutdown_driver());
            assert!(!driver_cached());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn read_transaction_rolls_back_callback_and_row_mapper_panics() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            for restricted in [false, true] {
                let restriction = restricted.then(|| Cx::push_restriction(CapMask::none()));
                let caller = Cx::current().expect("caller context");
                for panic_in_mapper in [false, true] {
                    let connection = Connection::open(":memory:").unwrap();
                    connection
                        .execute_batch(
                            "CREATE TABLE values_seen (value INTEGER); \
                             INSERT INTO values_seen VALUES (1);",
                        )
                        .unwrap();
                    let panic = catch_unwind(AssertUnwindSafe(|| {
                        connection.read_transaction::<(), FrankenError>(|connection| {
                            connection.execute("INSERT INTO values_seen VALUES (2)")?;
                            if panic_in_mapper {
                                return connection.query_row_map("SELECT 1", &[], |_| {
                                    panic!("read-transaction callback panic"); // ubs:ignore[rust.ownership.panic-macro] — Inject mapper unwind to prove rollback and caller-context restoration.
                                });
                            }
                            panic!("read-transaction callback panic"); // ubs:ignore[rust.ownership.panic-macro] — catch_unwind verifies this original callback payload survives.
                        })
                    }))
                    .expect_err("callback must keep unwinding");
                    assert_eq!(
                        panic.downcast_ref::<&str>(),
                        Some(&"read-transaction callback panic")
                    );
                    let restored = Cx::current().expect("restored caller context");
                    assert_eq!(restored.task_id(), caller.task_id());
                    assert_eq!(restored.region_id(), caller.region_id());
                    assert_eq!(restored.capabilities(), caller.capabilities());

                    // The same connection must admit a new transaction, and
                    // work before the callback panic must not be committed.
                    let values: Vec<i64> = connection
                        .read_transaction(|connection| {
                            connection.query_map_collect(
                                "SELECT value FROM values_seen ORDER BY value",
                                &[],
                                |row| row.get_typed(0),
                            )
                        })
                        .unwrap();
                    assert_eq!(values, vec![1]);
                    drop(connection);
                    let restored = Cx::current().expect("caller context after close");
                    assert_eq!(restored.task_id(), caller.task_id());
                    assert_eq!(restored.capabilities(), caller.capabilities());
                }
                drop(restriction);
            }
        });
    }

    #[test]
    fn read_transaction_preserves_success_and_returned_error_behavior() {
        let connection = Connection::open(":memory:").unwrap();
        connection
            .execute("CREATE TABLE values_seen (value INTEGER)")
            .unwrap();
        let value = connection
            .read_transaction(|connection| {
                connection.execute("INSERT INTO values_seen VALUES (1)")?;
                Ok::<_, FrankenError>(42)
            })
            .unwrap();
        assert_eq!(value, 42);

        let error = connection
            .read_transaction::<(), FrankenError>(|connection| {
                connection.execute("INSERT INTO values_seen VALUES (2)")?;
                Err(FrankenError::Internal("callback error".to_owned()))
            })
            .unwrap_err();
        assert!(matches!(error, FrankenError::Internal(message) if message == "callback error"));
        let values: Vec<i64> = connection
            .read_transaction(|connection| {
                connection.query_map_collect(
                    "SELECT value FROM values_seen ORDER BY value",
                    &[],
                    |row| row.get_typed(0),
                )
            })
            .unwrap();
        assert_eq!(values, vec![1]);
    }

    #[test]
    fn read_transaction_preserves_panic_when_rollback_returns_an_error() {
        let connection = Connection::open(":memory:").unwrap();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            connection.read_transaction::<(), FrankenError>(|connection| {
                // End the transaction first so the unwind cleanup encounters
                // a real "no transaction is active" rollback error.
                connection.execute("ROLLBACK;")?;
                assert!(connection.execute("ROLLBACK;").is_err());
                panic!("original callback panic"); // ubs:ignore[rust.ownership.panic-macro] — This original unwind must survive a second rollback failure.
            })
        }))
        .expect_err("rollback error must not replace the callback panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"original callback panic")
        );
        let value: i64 = connection
            .read_transaction(|connection| {
                connection.query_row_map("SELECT 42", &[], |row| row.get_typed(0))
            })
            .unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn read_transaction_failed_begin_keeps_existing_transaction() {
        let connection = Connection::open(":memory:").unwrap();
        connection
            .execute_batch(
                "CREATE TABLE values_seen (value INTEGER); \
                 BEGIN DEFERRED; INSERT INTO values_seen VALUES (1);",
            )
            .unwrap();
        let called = std::cell::Cell::new(false);
        let result = connection.read_transaction::<(), FrankenError>(|_| {
            called.set(true);
            Ok(())
        });
        assert!(result.is_err(), "nested BEGIN must fail");
        assert!(!called.get(), "failed BEGIN must not call the callback");
        connection.execute("COMMIT;").unwrap();
        let value: i64 = connection
            .query_row_map("SELECT value FROM values_seen", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 1, "the pre-existing transaction must remain owned");
    }

    #[test]
    fn nested_sql_bridge_preserves_caller_context() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let parent = Cx::current().expect("outer runtime context");
            for restricted in [false, true] {
                let restriction = restricted.then(|| Cx::push_restriction(CapMask::none()));
                let caller = Cx::current().expect("caller context");
                let assert_caller = || {
                    let current = Cx::current().expect("restored caller context");
                    assert_eq!(current.task_id(), caller.task_id());
                    assert_eq!(current.region_id(), caller.region_id());
                    assert_eq!(current.capabilities(), caller.capabilities());
                };

                let connection = Connection::open(":memory:").unwrap();
                assert_caller();
                connection
                    .execute_batch("CREATE TABLE values_seen (value INTEGER); INSERT INTO values_seen VALUES (40);")
                    .unwrap();
                assert_caller();
                let value = connection
                    .query_row_map("SELECT value FROM values_seen", &[], |row| {
                        // Row mapping runs inside drive(). A nested bridge must
                        // use a separate runtime, then restore the mapping task.
                        let mapping = Cx::current().expect("mapping context");
                        let nested = Connection::open(":memory:")?;
                        let extra: i64 = nested.query_row_map("SELECT 2", &[], |row| {
                            row.get_typed(0)
                        })?;
                        drop(nested);
                        let restored = Cx::current().expect("restored mapping context");
                        assert_eq!(restored.task_id(), mapping.task_id());
                        assert_eq!(restored.capabilities(), mapping.capabilities());
                        Ok(row.get_typed::<i64>(0)? + extra)
                    })
                    .unwrap();
                assert_eq!(value, 42);
                assert_caller();
                drop(connection);
                assert_caller();
                drop(restriction);
                let restored = Cx::current().expect("restored outer runtime context");
                assert_eq!(restored.task_id(), parent.task_id());
                assert_eq!(restored.capabilities(), parent.capabilities());
            }
        });
    }

    // cass follow-up to F9 (2026-11-07): the provably-empty classifier.

    /// A real database file whose schema holds no tables at all.
    fn zero_table_store(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let db = dir.join(name);
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE schema_marker (x INTEGER);")
            .unwrap();
        conn.execute("DROP TABLE schema_marker;").unwrap();
        drop(conn);
        assert!(
            std::fs::metadata(&db).unwrap().len() > 0,
            "the fixture must be a real database file, not 0 bytes"
        );
        db
    }

    #[test]
    fn a_zero_byte_store_with_no_wal_content_is_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        std::fs::write(&db, []).unwrap();
        assert!(store_is_provably_empty(&db));

        // A 0-byte `-wal` still proves emptiness.
        std::fs::write(sidecar(&db, "-wal"), []).unwrap();
        assert!(store_is_provably_empty(&db));
    }

    #[test]
    fn a_zero_byte_store_with_wal_content_is_not_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        std::fs::write(&db, []).unwrap();
        std::fs::write(sidecar(&db, "-wal"), b"frames").unwrap();
        assert!(!store_is_provably_empty(&db));
    }

    #[test]
    fn a_database_with_no_tables_is_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = zero_table_store(dir.path(), "store.db");
        assert!(store_is_provably_empty(&db));
    }

    #[test]
    fn a_wal_mode_database_with_no_tables_is_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = zero_table_store(dir.path(), "store.db");
        {
            let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
            let mode: String = conn
                .query_row_map("PRAGMA journal_mode=wal;", &[], |row| row.get_typed(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            conn.query_row_map::<(i64, i64, i64), _>(
                "PRAGMA wal_checkpoint(TRUNCATE);",
                &[],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
            )
            .unwrap();
        }
        assert!(sidecar(&db, "-wal").is_file());
        assert!(store_is_provably_empty(&db));
    }

    #[test]
    fn a_database_with_only_an_unrelated_table_is_not_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute("CREATE TABLE unrelated_table (x INTEGER);")
            .unwrap();
        drop(conn);
        assert!(!store_is_provably_empty(&db));
    }

    #[test]
    fn garbage_bytes_are_not_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        std::fs::write(&db, b"this is not a sqlite database").unwrap();
        assert!(!store_is_provably_empty(&db));
    }

    #[test]
    fn an_all_zeros_file_is_not_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        std::fs::write(&db, vec![0_u8; 32_768]).unwrap();
        assert!(
            !store_is_provably_empty(&db),
            "wrong header magic is never emptiness, whatever the size"
        );
    }

    #[test]
    fn a_nonempty_file_too_small_for_a_header_is_not_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        std::fs::write(&db, b"tiny").unwrap();
        assert!(!store_is_provably_empty(&db));
    }

    #[test]
    fn a_missing_file_is_not_provably_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("absent.db");
        assert!(!store_is_provably_empty(&db));
    }
}

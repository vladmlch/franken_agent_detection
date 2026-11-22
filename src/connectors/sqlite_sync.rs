//! Synchronous `rusqlite` facade shared by every SQLite-backed connector.
//!
//! The connectors are fully synchronous, so this module wraps
//! [`rusqlite::Connection`] directly and owns the zero-mutation open policy
//! for external app stores: a store is opened strictly read-only in place
//! whenever its on-disk state provably allows it
//! ([`open_external_strict_in_place`]), and a private snapshot copy inside
//! the system temp dir is used when it does not
//! ([`open_read_only_with_snapshot_fallback`], cass F9). The user's files
//! are a personal archive: the original db and its `-wal`, `-shm`, and
//! `-journal` sidecars are never created, modified, or deleted — not even
//! their mtimes.
//!
//! Two empirically verified C SQLite traps drive the in-place open modes:
//!
//! * **Trap 1:** if a SQLite file's 100-byte header has `header[18] == 2`
//!   or `header[19] == 2` (WAL file-format version) and the `-shm` is
//!   absent — e.g. a cleanly closed WAL database whose last connection
//!   checkpointed and deleted both sidecars — then *any* plain
//!   `SQLITE_OPEN_READ_ONLY` open (even `mode=ro`) makes C SQLite **create
//!   `<db>-wal` and `<db>-shm` on disk**. A WAL-headered store without a
//!   valid `-shm` therefore never reaches the engine on the original path;
//!   it is read through the snapshot copy instead.
//! * **Trap 2:** `mode=ro&readonly_shm=1` still requires the existing
//!   `-shm` wal-index and `-wal` header to be sane before it can serve a
//!   read without writing. Both are validated byte-for-byte up front (a
//!   crashed app can leave them absent, zeroed, empty, torn, or garbage);
//!   anything else is refused with [`SqliteError::BusyRecovery`], which the
//!   fallback turns into a private snapshot copy.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

pub use rusqlite::OpenFlags;
use rusqlite::types::ToSqlOutput;
pub use rusqlite::types::{FromSql, Value as SqliteValue, ValueRef as SqliteValueRef};

/// The engine's row type, re-exported so row-mapping closures in the
/// connectors name one `Row` no matter which facade produced it.
pub type Row<'stmt> = rusqlite::Row<'stmt>;

/// The one error type the SQLite connectors surface.
///
/// `BusyRecovery` is the typed signal that a store's WAL-index cannot serve
/// a strictly read-only open without recovery writes — the marker
/// [`open_read_only_with_snapshot_fallback`] turns into a snapshot copy.
/// `Internal` carries the refusal/policy errors the facade itself produces
/// (over-budget snapshot, vanished store, ...).
#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    /// Any error the bundled SQLite engine itself produced.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// A filesystem error while probing or copying store files.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The store's WAL-index (or an active rollback journal) cannot serve a
    /// strictly read-only open without recovery writes.
    #[error("database WAL-index requires recovery for read-only open")]
    BusyRecovery,
    /// A policy refusal produced by this facade (snapshot budget, ...).
    #[error("{0}")]
    Internal(String),
}

/// Compatibility alias: downstream connectors were written against
/// `frankensqlite::FrankenError`; the facade keeps that name working.
pub type FrankenError = SqliteError;

/// One query parameter in engine-typed form (compat name kept from the
/// `frankensqlite::compat` surface the connectors were written against).
#[derive(Debug, Clone)]
pub struct ParamValue(pub SqliteValue);

impl rusqlite::ToSql for ParamValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl From<i64> for ParamValue {
    fn from(value: i64) -> Self {
        Self(SqliteValue::Integer(value))
    }
}

impl From<i32> for ParamValue {
    fn from(value: i32) -> Self {
        Self(SqliteValue::Integer(i64::from(value)))
    }
}

impl From<u32> for ParamValue {
    fn from(value: u32) -> Self {
        Self(SqliteValue::Integer(i64::from(value)))
    }
}

impl From<usize> for ParamValue {
    fn from(value: usize) -> Self {
        Self(SqliteValue::Integer(
            i64::try_from(value).unwrap_or(i64::MAX),
        ))
    }
}

impl From<f64> for ParamValue {
    fn from(value: f64) -> Self {
        Self(SqliteValue::Real(value))
    }
}

impl From<bool> for ParamValue {
    fn from(value: bool) -> Self {
        Self(SqliteValue::Integer(i64::from(value)))
    }
}

impl From<&str> for ParamValue {
    fn from(value: &str) -> Self {
        Self(SqliteValue::Text(value.to_owned()))
    }
}

impl From<String> for ParamValue {
    fn from(value: String) -> Self {
        Self(SqliteValue::Text(value))
    }
}

impl From<&String> for ParamValue {
    fn from(value: &String) -> Self {
        Self(SqliteValue::Text(value.clone()))
    }
}

impl From<&[u8]> for ParamValue {
    fn from(value: &[u8]) -> Self {
        Self(SqliteValue::Blob(value.to_vec()))
    }
}

impl From<Vec<u8>> for ParamValue {
    fn from(value: Vec<u8>) -> Self {
        Self(SqliteValue::Blob(value))
    }
}

impl<T> From<Option<T>> for ParamValue
where
    Self: From<T>,
{
    fn from(value: Option<T>) -> Self {
        value.map_or(Self(SqliteValue::Null), Self::from)
    }
}

/// Positional parameters for the [`ConnectionExt`] methods: heterogeneous
/// arguments collected into a `&[ParamValue]` slice (`params![]` is the
/// empty slice).
#[macro_export]
macro_rules! fad_sqlite_params {
    () => {{
        const EMPTY: &[$crate::connectors::sqlite_sync::ParamValue] = &[];
        EMPTY
    }};
    ($($val:expr),+ $(,)?) => {
        &[$($crate::connectors::sqlite_sync::ParamValue::from($val)),+]
    };
}

pub use fad_sqlite_params as params;

/// Ergonomic row accessors matching the names the connectors already use.
pub trait RowExt {
    /// Column `idx` decoded into `T`.
    fn get_typed<T: FromSql>(&self, idx: usize) -> Result<T, SqliteError>;
    /// Column `idx` as a raw engine value.
    fn get_value(&self, idx: usize) -> Result<SqliteValue, SqliteError>;
}

impl RowExt for rusqlite::Row<'_> {
    fn get_typed<T: FromSql>(&self, idx: usize) -> Result<T, SqliteError> {
        self.get(idx).map_err(SqliteError::from)
    }

    fn get_value(&self, idx: usize) -> Result<SqliteValue, SqliteError> {
        self.get::<usize, SqliteValue>(idx)
            .map_err(SqliteError::from)
    }
}

/// Monotonic suffix for private snapshot-directory names (F9). No external
/// crates: the name only has to be unique among this process's snapshot
/// dirs; `std::fs::create_dir` refuses an existing name.
static SNAPSHOT_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The largest db + `-wal` total the snapshot-copy fallback will copy into
/// the temp dir (F18): 2 GiB.
///
/// A crashed app can leave a huge store with an unusable WAL-index (the
/// owner's Devin CLI store is a 27.6 GB db + ~98 MB `-wal`); snapshotting
/// such a store on every scan would copy tens of GiB into the system temp
/// dir, so an over-cap store is refused with a scan error naming the store,
/// its size, and this limit instead. There is deliberately no environment
/// knob: the fallback must stay bounded for every store it meets, and an
/// over-cap store reads normally again on the app's next start, which
/// rebuilds its WAL-index.
const SNAPSHOT_FALLBACK_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Compatibility no-op kept from the async-engine bridge: there is no
/// connector runtime to release any more, so shutdown always succeeds.
#[must_use]
pub const fn shutdown_driver() -> bool {
    true
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

/// Path of a SQLite sidecar file (`<db>-wal`, `<db>-shm`, `<db>-journal`).
fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let mut raw = db.as_os_str().to_owned();
    raw.push(suffix);
    PathBuf::from(raw)
}

/// Whether the sidecar exists at all (any filesystem object).
fn sidecar_exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Size + mtime of one store file, to detect an app writing while we copy.
#[derive(Clone, Copy, PartialEq, Eq)]
struct StableStamp {
    len: u64,
    mtime: SystemTime,
}

/// [`StableStamp`] of `path`, or `None` when the file does not exist.
fn stable_stamp(path: &Path) -> Result<Option<StableStamp>, SqliteError> {
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
fn fresh_snapshot_dir() -> Result<SnapshotDir, SqliteError> {
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
    Err(SqliteError::Internal(
        "could not create a private SQLite snapshot directory".to_owned(),
    ))
}

/// What one snapshot copy may consume (F18): the cap on the db + `-wal`
/// total, and the free space the temp dir reported when the fallback
/// engaged.
///
/// Production fills both from the real filesystem — `max_bytes` is
/// [`SNAPSHOT_FALLBACK_MAX_BYTES`], `free_bytes` is the temp dir's
/// available space — so the bounds hold with no configuration. Tests
/// inject synthetic values instead: this is the seam that lets the
/// refusal paths run against a few-KiB store rather than a 2 GiB
/// fixture.
struct SnapshotBudget {
    max_bytes: u64,
    free_bytes: u64,
}

/// Bytes an unprivileged process can still allocate in the system temp dir
/// (statvfs `f_bavail`), for the snapshot-copy free-space check. The temp
/// dir is where [`fresh_snapshot_dir`] puts every copy, so this is the
/// space a copy would consume.
fn temp_dir_available_bytes() -> Result<u64, SqliteError> {
    fs4::available_space(std::env::temp_dir()).map_err(|error| {
        SqliteError::Internal(format!(
            "could not check the free space in the temp dir before snapshotting a SQLite \
             store: {error}"
        ))
    })
}

/// Whether `<db>-journal` is a live rollback journal: a regular file with
/// content whose first byte is nonzero. A zeroed first byte is SQLite's own
/// mark for an invalidated journal. Unreadable journals count as inactive
/// here — the strict open re-checks and reports the read error itself.
fn journal_sidecar_is_active(db_path: &Path) -> bool {
    use std::io::Read as _;
    let journal_path = sidecar_path(db_path, "-journal");
    let Ok(meta) = std::fs::metadata(&journal_path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    let mut first = [0_u8; 1];
    std::fs::File::open(&journal_path)
        .and_then(|mut file| file.read_exact(&mut first))
        .is_ok_and(|()| first[0] != 0)
}

/// `std::fs::copy` preserves the source's permission bits, so a store file
/// archived read-only (0444) would hand the snapshot copy a read-only db,
/// `-wal`, or `-journal` — and C SQLite then refuses the WAL-index rebuild
/// and journal-rollback writes the copy exists to allow. The copy lives in
/// a private temp dir, so the read-only flag is cleared on the copies
/// only; the original store files are never chmod'ed.
fn clear_copy_readonly(dst: &Path) -> Result<(), SqliteError> {
    let mut permissions = std::fs::metadata(dst)?.permissions();
    if !permissions.readonly() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions = std::fs::Permissions::from_mode(permissions.mode() | 0o600);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    std::fs::set_permissions(dst, permissions)?;
    Ok(())
}

/// Copy the main db file plus its `-wal` and `-journal` (never its `-shm`)
/// into a fresh private directory, then open the copy with WAL-index and
/// rollback-journal recovery allowed — on our copy rebuilding the wal-index
/// or rolling back a hot journal is fine, unlike on the user's store.
///
/// The copy is taken under a size+mtime stability stamp of the db, `-wal`,
/// and `-journal` recorded before and re-checked after the copy, so a store
/// the app is actively writing is retried once and then refused with a
/// clear error instead of being read half-written.
///
/// `budget` bounds the copy (F18) before any file is copied: a store whose
/// db + `-wal` total exceeds `budget.max_bytes`, or whose copy would leave
/// the temp dir with less than the copy's own size free, is refused with a
/// scan error and no copy at all. The user's files are never touched either
/// way.
fn open_snapshot_copy(db_path: &Path, budget: &SnapshotBudget) -> Result<Connection, SqliteError> {
    let file_name = db_path.file_name().ok_or_else(|| {
        SqliteError::Internal(format!(
            "cannot snapshot a non-file SQLite path: {}",
            db_path.display()
        ))
    })?;
    let wal_path = sidecar_path(db_path, "-wal");
    let journal_path = sidecar_path(db_path, "-journal");
    for attempt in 0..2 {
        let before_db = stable_stamp(db_path)?;
        let before_wal = stable_stamp(&wal_path)?;
        let before_journal = stable_stamp(&journal_path)?;
        let Some(before_db) = before_db else {
            return Err(SqliteError::Internal(format!(
                "the store vanished before it could be snapshotted: {}",
                db_path.display()
            )));
        };
        // F18: bound the copy before any file is copied. A crashed app can
        // leave a huge store with an unusable WAL-index (the owner's Devin
        // CLI store is a 27.6 GB db + ~98 MB `-wal`); copying it on every
        // scan would fill the system temp dir, so an over-cap store is
        // refused instead. The refusal names the store, its size, and the
        // limit, and notes that the app's next start rebuilds the
        // WAL-index — the same scan-error path every other unreadable
        // store takes, so the scan reports it and keeps indexing the rest.
        let wal_bytes = before_wal.map_or(0, |wal| wal.len);
        let total_bytes = before_db.len + wal_bytes;
        if total_bytes > budget.max_bytes {
            let db_bytes = before_db.len;
            let max_bytes = budget.max_bytes;
            return Err(SqliteError::Internal(format!(
                "the SQLite store {} is too large to read through a private snapshot copy \
                 ({db_bytes} bytes db + {wal_bytes} bytes write-ahead log = {total_bytes} \
                 bytes, over the {max_bytes}-byte snapshot limit): no copy is made and the \
                 store's files are never modified; the app rebuilds the store's WAL-index \
                 on its next start, so a later scan will read it",
                db_path.display()
            )));
        }
        // Under the cap, still refuse a copy that would leave the temp dir
        // with less than the copy's own size free, so one scan cannot fill
        // the disk up to the last byte. The boundary is exact: free space
        // equal to twice the copy leaves exactly the copy's own size, which
        // is not less, so it still copies.
        let free_bytes = budget.free_bytes;
        if free_bytes < total_bytes.saturating_mul(2) {
            return Err(SqliteError::Internal(format!(
                "too little free space in the temp dir to copy the SQLite store {} for a \
                 read-only snapshot ({total_bytes} bytes to copy, {free_bytes} bytes \
                 available: the copy would leave less than the copy's own size free): no \
                 copy is made and the store's files are never modified; the app rebuilds \
                 the store's WAL-index on its next start, so a later scan will read it",
                db_path.display()
            )));
        }
        let snapshot = fresh_snapshot_dir()?;
        let copy_db = snapshot.dir.join(file_name);
        std::fs::copy(db_path, &copy_db)?;
        clear_copy_readonly(&copy_db)?;
        if wal_path.is_file() {
            let copy_wal = sidecar_path(&copy_db, "-wal");
            std::fs::copy(&wal_path, &copy_wal)?;
            clear_copy_readonly(&copy_wal)?;
        }
        if journal_path.is_file() {
            let copy_journal = sidecar_path(&copy_db, "-journal");
            std::fs::copy(&journal_path, &copy_journal)?;
            clear_copy_readonly(&copy_journal)?;
        }
        let stable = stable_stamp(db_path)?.is_some_and(|after| after == before_db)
            && stable_stamp(&wal_path)? == before_wal
            && stable_stamp(&journal_path)? == before_journal;
        if !stable {
            // The app wrote while we copied: discard this copy and retry once.
            drop(snapshot);
            if attempt == 1 {
                return Err(SqliteError::Internal(format!(
                    "the store changed while copying it for a read-only snapshot ({}); \
                     retry the scan when the app is idle",
                    db_path.display()
                )));
            }
            continue;
        }
        // The copy lives in a private directory, so the engine may create
        // its own `-shm`, rebuild the wal-index, or roll back the journal
        // there without ever touching the store's files.
        let inner = rusqlite::Connection::open(&copy_db)?;
        inner.execute_batch("PRAGMA query_only = 1;")?;
        return Ok(Connection {
            inner,
            snapshot: Some(snapshot),
        });
    }
    unreachable!("the snapshot retry loop returns from both of its arms");
}

/// The SQLite database file magic: bytes 0..16 of a database header
/// (`SQLite format 3\0`). A file without it is not a database, however an
/// engine build might otherwise treat it.
const SQLITE_HEADER_MAGIC: [u8; 16] = *b"SQLite format 3\0";

/// Length of the standard SQLite database file header in bytes.
const SQLITE_HEADER_PROBE_SIZE: usize = 100;

/// Length of the standard SQLite WAL header in bytes.
///
/// A `-wal` file at or below this length holds no transaction frames
/// (read-only clients commonly create a 0-byte or 32-byte frameless `-wal`).
const SQLITE_WAL_HEADER_SIZE: usize = 32;

/// Documented SQLite WAL header magic values (little-endian / big-endian
/// checksum byte order) and format version `3007000`.
const SQLITE_WAL_MAGIC_LE: u32 = 0x377f_0682;
const SQLITE_WAL_MAGIC_BE: u32 = 0x377f_0683;
const SQLITE_WAL_FORMAT_VERSION: u32 = 3_007_000;

/// Length of the SQLite wal-index header in bytes.
///
/// The `-shm` file starts with two 48-byte copies of the header, so the
/// file must hold at least those 96 bytes; real wal-indexes additionally
/// carry at least one checkpoint-block region, and a file under the
/// documented minimum of 136 bytes can never be a usable wal-index.
const SQLITE_SHM_MIN_SIZE: u64 = 136;
const SQLITE_SHM_HEADER_PAIR: usize = 96;

/// Whether the `-wal` file's header is internally sane. A 0-byte `-wal` is
/// valid (a cleanly checkpointed store); anything shorter than the 32-byte
/// header, or a header whose magic, format version, or page size is wrong,
/// means the wal-index cannot be trusted and the store must not be opened
/// in place.
#[cfg(unix)]
fn wal_file_is_valid(wal_path: &Path) -> Result<bool, SqliteError> {
    use std::io::Read as _;

    let meta = match std::fs::metadata(wal_path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !meta.is_file() {
        return Ok(false);
    }
    let len = meta.len();
    if len == 0 {
        return Ok(true);
    }
    if len < SQLITE_WAL_HEADER_SIZE as u64 {
        return Ok(false);
    }
    let mut header = [0_u8; SQLITE_WAL_HEADER_SIZE];
    let mut file = match std::fs::File::open(wal_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    file.read_exact(&mut header)?;
    let magic = read_be_u32(&header, 0);
    if magic != SQLITE_WAL_MAGIC_LE && magic != SQLITE_WAL_MAGIC_BE {
        return Ok(false);
    }
    if read_be_u32(&header, 4) != SQLITE_WAL_FORMAT_VERSION {
        return Ok(false);
    }
    let page_size = read_be_u32(&header, 8);
    Ok(page_size == 65_536 || (512..65_536).contains(&page_size) && page_size.is_power_of_two())
}

/// The SQLite wal-index header checksum: five pairs of native-endian
/// `u32`s over the header's first 40 bytes.
#[cfg(unix)]
fn wal_index_checksum(header_40: &[u8]) -> (u32, u32) {
    let mut s0 = 0_u32;
    let mut s1 = 0_u32;
    for pair in header_40.chunks_exact(8) {
        let x0 = u32::from_ne_bytes([pair[0], pair[1], pair[2], pair[3]]);
        let x1 = u32::from_ne_bytes([pair[4], pair[5], pair[6], pair[7]]);
        s0 = s0.wrapping_add(x0).wrapping_add(s1);
        s1 = s1.wrapping_add(x1).wrapping_add(s0);
    }
    (s0, s1)
}

/// Whether the `-shm` file is a coherent wal-index: a regular file at
/// least [`SQLITE_SHM_MIN_SIZE`] bytes, whose two 48-byte header copies
/// agree and whose first header carries format version `3007000`, the
/// `isInit` marker, a plausible `bigEndCksum` flag, a valid `szPage`, and a
/// matching header checksum.
#[cfg(unix)]
fn shm_file_is_valid(shm_path: &Path) -> Result<bool, SqliteError> {
    use std::io::Read as _;

    let meta = match std::fs::metadata(shm_path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !meta.is_file() || meta.len() < SQLITE_SHM_MIN_SIZE {
        return Ok(false);
    }
    let mut buf = [0_u8; SQLITE_SHM_HEADER_PAIR];
    let mut file = std::fs::File::open(shm_path)?;
    file.read_exact(&mut buf)?;
    let (h0, h1) = buf.split_at(48);
    if h0 != h1 {
        return Ok(false);
    }
    if u32::from_ne_bytes([h0[0], h0[1], h0[2], h0[3]]) != SQLITE_WAL_FORMAT_VERSION {
        return Ok(false);
    }
    if h0[12] != 1 || h0[13] > 1 {
        return Ok(false);
    }
    let sz_page = u16::from_ne_bytes([h0[14], h0[15]]);
    if !(sz_page == 1 || (sz_page >= 512 && sz_page.is_power_of_two())) {
        return Ok(false);
    }
    let (s0, s1) = wal_index_checksum(&h0[..40]);
    Ok(s0 == u32::from_ne_bytes([h0[40], h0[41], h0[42], h0[43]])
        && s1 == u32::from_ne_bytes([h0[44], h0[45], h0[46], h0[47]]))
}

/// Build a `file:` URI for `db_path` in pure `std` (no `urlencoding`
/// crate): iterate the raw path bytes so non-UTF-8 names survive,
/// collapse consecutive `/` into one, keep `/` itself and every other
/// printable ASCII byte unescaped, and percent-encode `%`, `?`, `#`,
/// every byte `<= 0x20`, and every non-ASCII byte `>= 0x80` —
/// `sqlite3ParseUri` decodes each `%XX` back into the exact raw byte.
#[cfg(unix)]
fn read_only_uri(db_path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let absolute = std::fs::canonicalize(db_path)
        .unwrap_or_else(|_| std::path::absolute(db_path).unwrap_or_else(|_| db_path.to_path_buf()));
    let mut encoded = String::with_capacity(absolute.as_os_str().len() + 16);
    let mut prev_was_slash = false;
    for &byte in absolute.as_os_str().as_bytes() {
        if byte == b'/' {
            if !prev_was_slash {
                encoded.push('/');
            }
            prev_was_slash = true;
            continue;
        }
        prev_was_slash = false;
        if byte <= b' ' || matches!(byte, b'%' | b'?' | b'#') || byte >= 0x80 {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        } else {
            encoded.push(char::from(byte));
        }
    }
    format!("file:{encoded}?mode=ro&readonly_shm=1")
}

/// Whether an open or first-read failure is the WAL-index class a snapshot
/// copy resolves (busy, cannot-open, or read-only), as opposed to the
/// caller's own problem (SQL errors, missing file, ...).
#[cfg(unix)]
const fn wal_index_open_failure(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::ReadOnly,
                ..
            },
            ..
        )
    )
}

/// Mode 2 of [`open_external_strict_in_place`]: a WAL store whose `-wal`
/// and `-shm` both exist and validate is opened in place through a
/// `mode=ro&readonly_shm=1` URI, which makes C SQLite mmap the wal-index
/// read-only and take its read marks through locks instead of writing them
/// — zero bytes and zero mtime changes on all three files.
#[cfg(unix)]
fn open_wal_in_place(
    db_path: &Path,
    wal_path: &Path,
    shm_path: &Path,
) -> Result<Connection, SqliteError> {
    if !wal_file_is_valid(wal_path)? || !shm_file_is_valid(shm_path)? {
        return Err(SqliteError::BusyRecovery);
    }
    let uri = read_only_uri(db_path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let inner = match rusqlite::Connection::open_with_flags(uri.as_str(), flags) {
        Ok(inner) => inner,
        Err(error) if wal_index_open_failure(&error) => {
            return Err(SqliteError::BusyRecovery);
        }
        Err(error) => return Err(error.into()),
    };
    // The schema read forces C SQLite to actually attach the wal-index now,
    // so a store that only fails on first access still reports
    // `BusyRecovery` here instead of mid-scan.
    match inner.query_row("PRAGMA schema_version;", [], |row| row.get::<usize, i64>(0)) {
        Ok(_) => Ok(Connection {
            inner,
            snapshot: None,
        }),
        Err(error) if wal_index_open_failure(&error) => {
            drop(inner);
            Err(SqliteError::BusyRecovery)
        }
        Err(error) => Err(error.into()),
    }
}

/// Open an external app store strictly read-only *in place*: not one byte,
/// mtime, or sidecar of the original db, `-wal`, `-shm`, or `-journal` may
/// be created or modified.
///
/// Open modes, after the 100-byte header probe proves a real database and
/// the `-journal` check refuses a live rollback journal
/// (`BusyRecovery`):
///
/// * **Mode 1 — pure rollback db** (non-WAL header, no `-wal`, no `-shm`):
///   a plain `READ_ONLY | NO_MUTEX` open on the original path; a
///   rollback-mode store creates no sidecar on a read-only open.
/// * **Mode 2 — Unix WAL store with `-wal` + `-shm`** present: both are
///   validated byte-for-byte (WAL header magic/version/page-size; wal-index
///   header copies, version, `isInit`, `szPage`, checksum), then the store
///   opens through a `file:...?mode=ro&readonly_shm=1` URI.
/// * **Every other WAL/sidecar shape** — a WAL-headered db with missing or
///   invalid `-shm`, a `-wal` without `-shm`, a `-shm` without `-wal`, or
///   any of it on a non-Unix platform — is refused with
///   [`SqliteError::BusyRecovery`] *without* an open on the original path,
///   because trap 1 shows C SQLite would create the missing sidecars there.
///
/// Missing, non-regular, sub-header, or non-SQLite files get the io/engine
/// error back (never `BusyRecovery`): they must not be snapshotted.
pub fn open_external_strict_in_place(db_path: &Path) -> Result<Connection, SqliteError> {
    use std::io::Read as _;

    let meta = std::fs::metadata(db_path)?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("SQLite path is not a regular file: {}", db_path.display()),
        )
        .into());
    }
    if meta.len() < SQLITE_HEADER_PROBE_SIZE as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "SQLite path is too small for a database header: {}",
                db_path.display()
            ),
        )
        .into());
    }
    let mut header = [0_u8; SQLITE_HEADER_PROBE_SIZE];
    std::fs::File::open(db_path)?.read_exact(&mut header)?;
    if header[..16] != SQLITE_HEADER_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "file does not carry SQLite header magic: {}",
                db_path.display()
            ),
        )
        .into());
    }

    // A live rollback journal means the store needs recovery a read-only
    // open must not perform: the snapshot copy will roll it back privately.
    let journal_path = sidecar_path(db_path, "-journal");
    if std::fs::metadata(&journal_path).is_ok_and(|meta| meta.is_file() && meta.len() > 0) {
        let mut first = [0_u8; 1];
        match std::fs::File::open(&journal_path).and_then(|mut f| f.read_exact(&mut first)) {
            Ok(()) if first[0] != 0 => return Err(SqliteError::BusyRecovery),
            Ok(()) => {}
            Err(error) => return Err(error.into()),
        }
    }

    let is_wal_header = header[18] == 2 || header[19] == 2;
    let wal_path = sidecar_path(db_path, "-wal");
    let shm_path = sidecar_path(db_path, "-shm");
    let wal_exists = sidecar_exists(&wal_path);
    let shm_exists = sidecar_exists(&shm_path);

    // Mode 1 — pure rollback-mode store: no WAL marker anywhere, so a
    // read-only open cannot create `-wal` / `-shm`.
    if !is_wal_header && !wal_exists && !shm_exists {
        let inner = rusqlite::Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        return Ok(Connection {
            inner,
            snapshot: None,
        });
    }

    #[cfg(unix)]
    if wal_exists && shm_exists {
        return open_wal_in_place(db_path, &wal_path, &shm_path);
    }
    #[cfg(not(unix))]
    let _ = (wal_exists, shm_exists);

    // Trap 1 (and every ambiguous sidecar shape): never let C SQLite near
    // the original path; the caller's snapshot fallback reads it safely.
    Err(SqliteError::BusyRecovery)
}

/// Strict read-only open with snapshot-copy fallback (cass F9).
///
/// The in-place open ([`open_external_strict_in_place`]) never lets the
/// engine create, modify, or delete any byte or sidecar of the user's
/// store: pure rollback-mode stores open read-only directly, and on Unix a
/// WAL store whose `-wal` and `-shm` both validate opens through a
/// `mode=ro&readonly_shm=1` URI. Everything else — an absent, zeroed,
/// empty, torn, or garbage wal-index; a `-wal` without `-shm`; a WAL-header
/// db whose last connection checkpointed the sidecars away (trap 1); an
/// active rollback journal — is refused with [`SqliteError::BusyRecovery`].
///
/// When refused, the db, its `-wal`, and its `-journal` are copied into a
/// private temporary directory (never the `-shm`), the copy is opened
/// read-write so SQLite recovers the WAL/journal inside the private dir
/// (it rebuilds only the copy's own sidecars), `PRAGMA query_only = 1` is
/// set, and the copy is removed when the connection drops.
///
/// The copy is bounded (F18): a store whose db + `-wal` total exceeds
/// [`SNAPSHOT_FALLBACK_MAX_BYTES`] (2 GiB, no environment knob), or whose
/// copy would leave the temp dir with less than the copy's own size free,
/// is refused before any copy with a scan error naming the store, its
/// size, and the bound — the same error path any other unreadable store
/// takes, so the scan reports it and keeps indexing the other stores. A
/// refused store reads normally again on the app's next start, which
/// rebuilds its WAL-index.
///
/// Any other open error — and a fallback that itself fails — is returned
/// so the caller's scan surfaces it as an error instead of hiding it.
pub fn open_read_only_with_snapshot_fallback(path: &str) -> Result<Connection, SqliteError> {
    let db_path = Path::new(path);
    match open_external_strict_in_place(db_path) {
        Ok(connection) => Ok(connection),
        Err(SqliteError::BusyRecovery) => {
            tracing::warn!(
                store = path,
                "SQLite store has an unusable WAL-index or an active rollback journal for \
                 a read-only open; reading a private snapshot copy (the store's files are \
                 never modified)"
            );
            let budget = SnapshotBudget {
                max_bytes: SNAPSHOT_FALLBACK_MAX_BYTES,
                free_bytes: temp_dir_available_bytes()?,
            };
            open_snapshot_copy(db_path, &budget)
        }
        Err(error) => Err(error),
    }
}

/// cass follow-up to F9 (2026-11-07): whether a store database is
/// *provably empty* —
///
/// * a 0-byte main file with no `-wal` content and no live `-journal`:
///   SQLite's own semantics make a 0-byte file an empty database, and a
///   fresh install commonly creates the store file before its first write,
///   or
/// * a database file that opens strictly read-only in place (header magic
///   and sidecar state checked, so a large all-zeros file or a store
///   needing recovery never passes) and whose schema holds no tables at
///   all.
///
/// Both hold no sessions for any connector, so a scan reports them as
/// "nothing to read" instead of a scan error on every index run.
///
/// Returns `false` for everything uncertain, so the caller falls through to
/// its own read and surfaces its error exactly as before: garbage bytes and
/// wrong header magic (including a large all-zeros file), a non-empty file
/// too small for a header, a schema with *some* tables (schema drift, e.g.
/// an app migration — the connector's own read must name it), an active
/// rollback journal, and any open failure. A store whose wal-index is
/// unusable (`BusyRecovery`) is deliberately not classified here: proving
/// its emptiness would cost a snapshot copy of the store, so it falls
/// through to the connector's [`open_read_only_with_snapshot_fallback`]
/// read instead.
#[must_use]
pub fn store_is_provably_empty(db_path: &Path) -> bool {
    // Case 1: a 0-byte main file. Any bytes at all in the `-wal` could hold
    // committed frames, so only an absent or 0-byte `-wal` proves emptiness,
    // and a live rollback journal could still roll writes back in.
    let main_is_zero_bytes =
        std::fs::metadata(db_path).is_ok_and(|meta| meta.is_file() && meta.len() == 0);
    if main_is_zero_bytes {
        let wal_has_content =
            std::fs::metadata(sidecar_path(db_path, "-wal")).is_ok_and(|meta| meta.len() > 0);
        return !wal_has_content && !journal_sidecar_is_active(db_path);
    }

    // Case 2: a real database file whose schema holds no tables at all.
    // A strict in-place open only (see above for why no snapshot fallback),
    // so even this probe can never write a sidecar of the user's store.
    let Ok(connection) = open_external_strict_in_place(db_path) else {
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

/// Default maximum re-verification interval (5 minutes).
///
/// Guards against same-second write-checkpoint-truncate cycles that preserve
/// header markers in [`SqliteContainerHeaderState`].
pub const SQLITE_CONTAINER_REVERIFY_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

/// Zero-open SQLite + WAL header state for bounded freshness and change
/// detection without opening a SQLite connection or touching `-shm`.
///
/// Captures:
/// - Main DB file metadata (`db_size`, whole-second `db_mtime_sec`, Unix
///   `db_ino` without `st_dev` for device-id-free portability) and, when
///   `db_size >= 100`, the verified 100-byte SQLite header fields
///   (`change_counter` at bytes `24..28`, `page_count` at `28..32`, and
///   `schema_cookie` at `40..44`).
/// - Sibling `-wal` metadata and 32-byte WAL header fields (`wal_size`,
///   `wal_mtime_sec`, `wal_ckpt_seq` at `12..16`, `wal_salt_1` at `16..20`,
///   `wal_salt_2` at `20..24`) when `-wal` exists and `wal_size > 32`.
///   Absent, 0-byte, and 32-byte frameless `-wal` files set `wal_present =
///   false` and ignore `-wal` mtime.
/// - Sibling `-shm` is never stated or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SqliteContainerHeaderState {
    pub db_size: u64,
    pub db_mtime_sec: i64,
    /// Unix inode (`st_ino`), or `0` on non-Unix platforms. Intentionally
    /// excludes `st_dev` to preserve device-id-free portability across mounts.
    pub db_ino: u64,
    /// Whether `db_size >= 100` and the `"SQLite format 3\0"` header magic
    /// was verified.
    pub db_header_valid: bool,
    pub change_counter: u32,
    pub page_count: u32,
    pub schema_cookie: u32,
    /// `true` only when sibling `<db_path>-wal` exists and `wal_size > 32`.
    pub wal_present: bool,
    pub wal_size: u64,
    pub wal_mtime_sec: i64,
    pub wal_ckpt_seq: u32,
    pub wal_salt_1: u32,
    pub wal_salt_2: u32,
}

impl SqliteContainerHeaderState {
    /// Returns `true` if any material container marker changed relative to
    /// `prev`.
    ///
    /// When `wal_present == false` in both states (absent, 0-byte, or 32-byte
    /// frameless `-wal`), all WAL fields are ignored.
    #[must_use]
    pub const fn has_material_change_since(&self, prev: &Self) -> bool {
        if self.db_size != prev.db_size
            || self.db_mtime_sec != prev.db_mtime_sec
            || self.db_ino != prev.db_ino
            || self.db_header_valid != prev.db_header_valid
            || self.change_counter != prev.change_counter
            || self.page_count != prev.page_count
            || self.schema_cookie != prev.schema_cookie
            || self.wal_present != prev.wal_present
        {
            return true;
        }
        if self.wal_present {
            self.wal_size != prev.wal_size
                || self.wal_mtime_sec != prev.wal_mtime_sec
                || self.wal_ckpt_seq != prev.wal_ckpt_seq
                || self.wal_salt_1 != prev.wal_salt_1
                || self.wal_salt_2 != prev.wal_salt_2
        } else {
            false
        }
    }
}

/// Returns `true` when `elapsed` since the last full verification has reached
/// or exceeded `max_interval` (same-second checkpoint/truncate edge cases).
#[must_use]
pub const fn needs_periodic_reverify(
    elapsed: std::time::Duration,
    max_interval: std::time::Duration,
) -> bool {
    elapsed.as_nanos() >= max_interval.as_nanos()
}

const fn read_be_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn system_time_to_unix_sec(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(error) => i64::try_from(error.duration().as_secs())
            .unwrap_or(i64::MAX)
            .saturating_neg(),
    }
}

#[cfg(unix)]
fn file_inode(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.ino()
}

#[cfg(not(unix))]
const fn file_inode(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[derive(Clone, Copy)]
struct WalHeaderProbe {
    size: u64,
    mtime_sec: i64,
    ckpt_seq: u32,
    salt_1: u32,
    salt_2: u32,
}

fn probe_wal_header(wal_path: &Path) -> std::io::Result<Option<WalHeaderProbe>> {
    use std::io::Read as _;

    let meta = match std::fs::metadata(wal_path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "SQLite WAL sidecar is not a regular file: {}",
                wal_path.display()
            ),
        ));
    }
    if meta.len() <= SQLITE_WAL_HEADER_SIZE as u64 {
        // Absent, 0-byte, or 32-byte frameless WAL header (created by read-only
        // clients without any committed frames): treat as absent and ignore mtime.
        return Ok(None);
    }

    let mut wal_file = match std::fs::File::open(wal_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let opened_meta = wal_file.metadata()?;
    if !opened_meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "SQLite WAL sidecar is not a regular file: {}",
                wal_path.display()
            ),
        ));
    }
    let wal_size = opened_meta.len();
    if wal_size <= SQLITE_WAL_HEADER_SIZE as u64 {
        return Ok(None);
    }
    let wal_mtime_sec = system_time_to_unix_sec(opened_meta.modified()?);

    let mut header = [0_u8; SQLITE_WAL_HEADER_SIZE];
    wal_file.read_exact(&mut header)?;
    let magic = read_be_u32(&header, 0);
    if magic != SQLITE_WAL_MAGIC_LE && magic != SQLITE_WAL_MAGIC_BE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid SQLite WAL header magic: {}", wal_path.display()),
        ));
    }
    let version = read_be_u32(&header, 4);
    // Accept the documented SQLite WAL format version (3007000) as well as 0
    // for minimal synthetic test headers, while failing closed on unknown
    // format versions (e.g. 3008000).
    if version != 0 && version != SQLITE_WAL_FORMAT_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unsupported SQLite WAL format version {version}: {}",
                wal_path.display()
            ),
        ));
    }

    Ok(Some(WalHeaderProbe {
        size: wal_size,
        mtime_sec: wal_mtime_sec,
        ckpt_seq: read_be_u32(&header, 12),
        salt_1: read_be_u32(&header, 16),
        salt_2: read_be_u32(&header, 20),
    }))
}

/// Probe a SQLite container's file metadata and raw DB + WAL headers without
/// opening a SQLite connection or stating/touching `-shm`.
///
/// - Returns `Ok(None)` if `db_path` does not exist (`NotFound`).
/// - Returns `Ok(Some(state))` for 0-byte/sub-header databases
///   (`db_header_valid = false`) and valid SQLite databases (`db_size >= 100`
///   with `"SQLite format 3\0"` magic).
/// - Ignores sibling `<db_path>-wal` when absent or `wal_size <= 32` (32-byte
///   frameless WAL header created by read-only clients) and ignores `-wal`
///   mtime in that case.
/// - Never stats or opens `<db_path>-shm`.
pub fn stat_sqlite_container_header(
    db_path: &Path,
) -> std::io::Result<Option<SqliteContainerHeaderState>> {
    use std::io::Read as _;

    let mut file = match std::fs::File::open(db_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("SQLite path is not a regular file: {}", db_path.display()),
        ));
    }

    let db_size = meta.len();
    let db_mtime_sec = system_time_to_unix_sec(meta.modified()?);
    let db_ino = file_inode(&meta);

    let (db_header_valid, change_counter, page_count, schema_cookie) =
        if db_size >= SQLITE_HEADER_PROBE_SIZE as u64 {
            let mut header = [0_u8; SQLITE_HEADER_PROBE_SIZE];
            file.read_exact(&mut header)?;
            if header[..16] != SQLITE_HEADER_MAGIC {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "file does not carry SQLite header magic: {}",
                        db_path.display()
                    ),
                ));
            }
            (
                true,
                read_be_u32(&header, 24),
                read_be_u32(&header, 28),
                read_be_u32(&header, 40),
            )
        } else {
            (false, 0, 0, 0)
        };

    let wal_path = sidecar_path(db_path, "-wal");
    let wal_probe = probe_wal_header(&wal_path)?;

    Ok(Some(SqliteContainerHeaderState {
        db_size,
        db_mtime_sec,
        db_ino,
        db_header_valid,
        change_counter,
        page_count,
        schema_cookie,
        wal_present: wal_probe.is_some(),
        wal_size: wal_probe.map_or(0, |wal| wal.size),
        wal_mtime_sec: wal_probe.map_or(0, |wal| wal.mtime_sec),
        wal_ckpt_seq: wal_probe.map_or(0, |wal| wal.ckpt_seq),
        wal_salt_1: wal_probe.map_or(0, |wal| wal.salt_1),
        wal_salt_2: wal_probe.map_or(0, |wal| wal.salt_2),
    }))
}

/// Synchronous wrapper over [`rusqlite::Connection`].
///
/// Exposes the subset of the engine surface the connectors use, keeping the
/// blocking signatures the connectors were written against.
pub struct Connection {
    inner: rusqlite::Connection,
    /// Private snapshot copy backing this connection (F9), removed on drop.
    /// Declared after `inner` so the engine connection closes first.
    snapshot: Option<SnapshotDir>,
}

impl Connection {
    /// Open (or create) a database at `path`.
    pub fn open(path: &str) -> Result<Self, SqliteError> {
        Ok(Self {
            inner: rusqlite::Connection::open(path)?,
            snapshot: None,
        })
    }

    /// The wrapped engine connection, for engine-specific calls the facade
    /// does not re-expose.
    #[must_use]
    pub const fn raw(&self) -> &rusqlite::Connection {
        &self.inner
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

    /// `SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE`: when `disable` is true, dropping
    /// this connection will not run a final checkpoint and will not delete
    /// the store's `-wal` / `-shm` — test fixtures use it to leave a
    /// WAL-mode store's sidecars on disk.
    pub fn set_no_ckpt_on_close(&self, disable: bool) -> Result<(), SqliteError> {
        self.inner
            .set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                disable,
            )
            .map(|_| ())
            .map_err(SqliteError::Sqlite)
    }

    /// Execute a single SQL statement, returning the affected row count.
    ///
    /// Statements that produce rows (`PRAGMA busy_timeout = 2000;` and
    /// friends) are stepped to completion and report `0` changed rows —
    /// `rusqlite::Statement::execute` would refuse them outright, while the
    /// connectors use `execute` for pragma tuning calls.
    pub fn execute(&self, sql: &str) -> Result<usize, SqliteError> {
        let mut statement = self.inner.prepare(sql)?;
        if statement.column_count() == 0 {
            statement.execute([]).map_err(SqliteError::Sqlite)
        } else {
            let mut rows = statement.query([])?;
            while rows.next()?.is_some() {}
            Ok(0)
        }
    }

    /// Execute a string of semicolon-separated SQL statements.
    pub fn execute_batch(&self, sql: &str) -> Result<(), SqliteError> {
        self.inner.execute_batch(sql).map_err(SqliteError::Sqlite)
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
        E: From<SqliteError>,
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

/// Open a database with rusqlite-style open flags.
///
/// Strict semantics: no snapshot fallback. External app stores are opened
/// through [`open_read_only_with_snapshot_fallback`] instead.
pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection, SqliteError> {
    Ok(Connection {
        inner: rusqlite::Connection::open_with_flags(path, flags)?,
        snapshot: None,
    })
}

/// Ergonomic query helpers matching the names the connectors already use.
pub trait ConnectionExt {
    /// Execute a query that returns exactly one row, mapping it with `f`.
    fn query_row_map<T, F>(&self, sql: &str, params: &[ParamValue], f: F) -> Result<T, SqliteError>
    where
        F: FnOnce(&Row<'_>) -> Result<T, SqliteError>;

    /// Execute a query and collect all rows into a `Vec<T>` via mapping closure.
    fn query_map_collect<T, F>(
        &self,
        sql: &str,
        params: &[ParamValue],
        f: F,
    ) -> Result<Vec<T>, SqliteError>
    where
        F: FnMut(&Row<'_>) -> Result<T, SqliteError>;

    /// Execute a query and visit every row with `f`, streaming with no
    /// intermediate `Vec` allocation.
    fn query_for_each<F>(&self, sql: &str, params: &[ParamValue], f: F) -> Result<(), SqliteError>
    where
        F: FnMut(&Row<'_>) -> Result<(), SqliteError>;

    /// Execute a SQL statement with `ParamValue` parameters.
    fn execute_compat(&self, sql: &str, params: &[ParamValue]) -> Result<usize, SqliteError>;
}

impl ConnectionExt for Connection {
    fn query_row_map<T, F>(&self, sql: &str, params: &[ParamValue], f: F) -> Result<T, SqliteError>
    where
        F: FnOnce(&Row<'_>) -> Result<T, SqliteError>,
    {
        let mut statement = self.inner.prepare(sql)?;
        statement.query_row(rusqlite::params_from_iter(params.iter()), |row| Ok(f(row)))?
    }

    fn query_map_collect<T, F>(
        &self,
        sql: &str,
        params: &[ParamValue],
        mut f: F,
    ) -> Result<Vec<T>, SqliteError>
    where
        F: FnMut(&Row<'_>) -> Result<T, SqliteError>,
    {
        let mut statement = self.inner.prepare(sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(f(row)?);
        }
        Ok(out)
    }

    fn query_for_each<F>(
        &self,
        sql: &str,
        params: &[ParamValue],
        mut f: F,
    ) -> Result<(), SqliteError>
    where
        F: FnMut(&Row<'_>) -> Result<(), SqliteError>,
    {
        let mut statement = self.inner.prepare(sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            f(row)?;
        }
        Ok(())
    }

    fn execute_compat(&self, sql: &str, params: &[ParamValue]) -> Result<usize, SqliteError> {
        self.inner
            .execute(sql, rusqlite::params_from_iter(params.iter()))
            .map_err(SqliteError::Sqlite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    /// Path of a SQLite sidecar file (`<db>-wal`, `<db>-shm`, `-journal`).
    fn sidecar(db: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        let mut raw = db.as_os_str().to_owned();
        raw.push(suffix);
        std::path::PathBuf::from(raw)
    }

    /// A WAL-mode database whose writer exited after a TRUNCATE checkpoint,
    /// leaving a `-wal` and a `-shm` behind.
    ///
    /// C SQLite checkpoints and *deletes* `-wal` and `-shm` when closing the
    /// last writable connection, so `no_ckpt_on_close` is set before the
    /// drop to keep both sidecars on disk.
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
        conn.set_no_ckpt_on_close(true).unwrap();
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

    /// `(bytes, mtime)` of the db / `-wal` / `-shm` / `-journal` quad, for
    /// never-modify assertions.
    fn quad_state(db: &std::path::Path) -> Vec<(Option<Vec<u8>>, Option<std::time::SystemTime>)> {
        ["", "-wal", "-shm", "-journal"]
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
    /// any new sidecar (`-shm`, `-journal`, ...) behind.
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

    /// A strict in-place open never performs WAL-index recovery and never
    /// writes any sidecar: with the `-shm` wal-index garbage, torn, absent,
    /// zeroed, or empty, the open fails with the typed `BusyRecovery` and
    /// the db / `-wal` / `-shm` bytes and mtimes are untouched, with no
    /// file created next to the store. This is the premise the snapshot
    /// fallback (cass F9) exists for: C SQLite would otherwise rebuild the
    /// missing or damaged wal-index in place — or, for `absent`, create
    /// both sidecars outright (trap 1).
    #[test]
    fn strict_read_only_open_refuses_an_unusable_wal_index() {
        for state in ["garbage", "torn", "absent", "zeroed", "empty"] {
            let dir = tempfile::TempDir::new().unwrap();
            let db = wal_mode_store(dir.path());
            damage_shm(&db, state);
            let before_quad = quad_state(&db);
            let before_names = directory_names(&db);

            let Err(error) = open_external_strict_in_place(&db) else {
                panic!("{state}: the strict open must refuse an unusable wal-index");
            };
            assert!(
                matches!(error, SqliteError::BusyRecovery),
                "{state}: expected the typed BusyRecovery, got {error:?}"
            );
            assert_eq!(
                quad_state(&db),
                before_quad,
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
    /// state:
    ///   * the store's rows are readable in every state;
    ///   * the fallback engages exactly when the strict in-place open
    ///     refuses the store;
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

            // Whether this store's damaged wal-index makes the strict
            // in-place open refuse it.
            let strict_refused = open_external_strict_in_place(&db).is_err();
            let before = quad_state(&db);
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
                    quad_state(&db),
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
                    quad_state(&db),
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
    /// is not a database surfaces as an io-level error, and no snapshot copy
    /// is taken.
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

    /// A live rollback journal on a plain (non-WAL) store engages the
    /// snapshot copy: the in-place open must never touch it, and the
    /// private copy rolls back or ignores the journal inside the temp dir.
    #[serial]
    #[test]
    fn snapshot_fallback_handles_an_active_rollback_journal() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        {
            let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=delete; CREATE TABLE t (v INTEGER); \
                 INSERT INTO t VALUES (7);",
            )
            .unwrap();
        }
        std::fs::write(sidecar(&db, "-journal"), b"\xd9live journal bytes").unwrap();
        let before = quad_state(&db);
        let before_names = directory_names(&db);
        assert!(
            !store_is_provably_empty(&db),
            "an active rollback journal is never provable emptiness"
        );

        let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
            .unwrap_or_else(|error| panic!("fallback failed: {error:?}"));
        assert!(
            conn.used_snapshot_fallback(),
            "a live `-journal` must engage the snapshot copy"
        );
        let value: i64 = conn
            .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 7);
        drop(conn);
        assert_eq!(
            quad_state(&db),
            before,
            "the original db and `-journal` must never be modified"
        );
        assert_eq!(
            directory_names(&db),
            before_names,
            "no new file may appear next to the original store"
        );
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no snapshot copy may outlive its connection"
        );
    }

    /// `std::fs::copy` preserves the source's permission bits, so a store
    /// archived read-only (0444) would hand the snapshot copy a read-only
    /// db and `-wal`, and C SQLite would then refuse the recovery writes
    /// the copy exists to allow. The copies get their read-only flag
    /// cleared — the originals are never chmod'ed — the store still reads
    /// through the fallback, and the original files stay byte- and
    /// mtime-identical.
    #[cfg(unix)]
    #[serial]
    #[test]
    fn snapshot_copy_of_a_read_only_store_recovers_and_leaves_it_untouched() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::TempDir::new().unwrap();
        let db = wal_mode_store(dir.path());
        damage_shm(&db, "garbage");
        // Archive every store file read-only, the way an external store
        // that must never be written would arrive.
        for suffix in ["", "-wal", "-shm"] {
            let path = sidecar(&db, suffix);
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o444);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        let before = quad_state(&db);

        let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
            .unwrap_or_else(|error| panic!("fallback failed: {error:?}"));
        assert!(
            conn.used_snapshot_fallback(),
            "a damaged wal-index must engage the snapshot copy"
        );
        let value: i64 = conn
            .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 7, "the read-only store's rows must be readable");
        let snapshot_dir = conn
            .snapshot_dir()
            .expect("a snapshot copy must exist")
            .to_path_buf();
        // The copies must be writable even though the sources were 0444.
        for name in ["store.db", "store.db-wal"] {
            let mode = std::fs::metadata(snapshot_dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(
                mode & 0o222,
                0,
                "{name}: the private copy must be writable (mode {mode:o})"
            );
        }
        drop(conn);
        assert!(
            !snapshot_dir.exists(),
            "the snapshot copy must be removed when the connection drops"
        );
        assert_eq!(
            quad_state(&db),
            before,
            "the original 0444 files must stay byte- and mtime-identical"
        );
        for suffix in ["", "-wal", "-shm"] {
            let path = sidecar(&db, suffix);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o444,
                "the original store files must keep their read-only mode"
            );
        }
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no snapshot copy may outlive its connection"
        );
    }

    /// Trap 1: a cleanly closed WAL database keeps `header[18..20] == (2, 2)`
    /// but has no `-wal` / `-shm` on disk, and any plain read-only open on
    /// the original path would *create* both. The facade must route it
    /// through the snapshot copy instead, leaving zero sidecars behind.
    #[serial]
    #[test]
    fn cleanly_closed_wal_store_falls_back_without_creating_sidecars() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        {
            let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=wal; CREATE TABLE t (v INTEGER); \
                 INSERT INTO t VALUES (7);",
            )
            .unwrap();
            // No `set_no_ckpt_on_close`: a clean close checkpoints and
            // deletes `-wal` and `-shm`.
        }
        assert!(
            !sidecar(&db, "-wal").exists(),
            "a clean WAL close removes the `-wal`"
        );
        assert!(
            !sidecar(&db, "-shm").exists(),
            "a clean WAL close removes the `-shm`"
        );
        let header = std::fs::read(&db).unwrap();
        assert_eq!(
            &header[18..20],
            &[2, 2],
            "the db header must still carry the WAL file-format version"
        );
        let before = quad_state(&db);
        let before_names = directory_names(&db);

        let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
            .unwrap_or_else(|error| panic!("fallback failed: {error:?}"));
        assert!(
            conn.used_snapshot_fallback(),
            "a cleanly closed WAL store must read through the snapshot copy"
        );
        let value: i64 = conn
            .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 7);
        drop(conn);
        assert_eq!(quad_state(&db), before, "the store must never be modified");
        assert_eq!(directory_names(&db), before_names, "no sidecars created");
        assert!(
            !sidecar(&db, "-wal").exists() && !sidecar(&db, "-shm").exists(),
            "zero `-wal`/`-shm` may be created next to the original file"
        );
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no snapshot copy may outlive its connection"
        );
    }

    /// On Unix, a WAL store whose `-wal` and `-shm` are both intact opens in
    /// place through `mode=ro&readonly_shm=1`: no snapshot copy, no byte or
    /// mtime change on any of the three files, no new file created.
    #[cfg(unix)]
    #[serial]
    #[test]
    fn strict_in_place_open_reads_a_valid_wal_store_without_touching_anything() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = wal_mode_store(dir.path());
        let before = quad_state(&db);
        let before_names = directory_names(&db);

        let conn = open_external_strict_in_place(&db)
            .unwrap_or_else(|error| panic!("the in-place open must succeed: {error:?}"));
        assert!(
            !conn.used_snapshot_fallback(),
            "a valid WAL store reads in place, not through a snapshot"
        );
        let value: i64 = conn
            .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 7);
        drop(conn);
        assert_eq!(
            quad_state(&db),
            before,
            "readonly_shm=1 must leave db, `-wal`, and `-shm` byte- and mtime-identical"
        );
        assert_eq!(
            directory_names(&db),
            before_names,
            "the in-place open must not create any file"
        );
    }

    /// The `file:` URI used for the in-place open escapes `%`, `?`, `#`,
    /// spaces, and non-ASCII bytes so stores living under such directory
    /// names — Cyrillic and other Unicode names included — still open.
    #[cfg(unix)]
    #[test]
    fn strict_in_place_open_handles_uri_special_characters_in_the_path() {
        let dir = tempfile::TempDir::new().unwrap();
        for name in [
            "sp ace",
            "per%cent",
            "ques?tion",
            "ha#sh",
            "mix ed%?#.dbdir",
            "кириллица",
            "ストア",
        ] {
            let odd = dir.path().join(name);
            std::fs::create_dir(&odd).unwrap();
            let db = wal_mode_store(&odd);
            let before = quad_state(&db);

            let conn = open_external_strict_in_place(&db)
                .unwrap_or_else(|error| panic!("{name}: in-place open failed: {error:?}"));
            let value: i64 = conn
                .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
                .unwrap_or_else(|error| panic!("{name}: query failed: {error:?}"));
            assert_eq!(value, 7, "{name}");
            drop(conn);
            assert_eq!(quad_state(&db), before, "{name}: files untouched");
        }
    }

    // cass F18 (2026-11-09): the snapshot-copy budget — size cap and
    // free-space floor. `open_snapshot_copy` takes its budget as a
    // parameter: production fills it from the real filesystem, tests
    // inject synthetic values, so these refusal paths run against a
    // few-KiB store instead of a 2 GiB fixture.

    /// The db + `-wal` total of a fixture, summed exactly as the fallback
    /// sums it for the budget checks.
    fn store_total_bytes(db: &std::path::Path) -> u64 {
        let wal_bytes = std::fs::metadata(sidecar(db, "-wal")).map_or(0, |meta| meta.len());
        std::fs::metadata(db).expect("the fixture db exists").len() + wal_bytes
    }

    /// A store whose db + `-wal` total exceeds the budget's cap is refused
    /// with the typed scan error naming the store, its size, and the cap,
    /// and no temp copy is created. Production caps at
    /// `SNAPSHOT_FALLBACK_MAX_BYTES`; this test injects a cap one byte
    /// below the fixture's total so the fixture stays a few KiB. The
    /// fixture keeps its crashed-app state (an unusable wal-index), which
    /// is the only state that ever reaches the fallback, and the store's
    /// files stay untouched.
    #[serial]
    #[test]
    fn snapshot_fallback_refuses_a_store_over_the_size_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = wal_mode_store(dir.path());
        damage_shm(&db, "garbage");
        let before_quad = quad_state(&db);
        let before_names = directory_names(&db);
        let db_bytes = std::fs::metadata(&db).unwrap().len();
        let wal_bytes = std::fs::metadata(sidecar(&db, "-wal")).map_or(0, |meta| meta.len());
        let total_bytes = store_total_bytes(&db);
        let cap = total_bytes - 1;
        let budget = SnapshotBudget {
            max_bytes: cap,
            free_bytes: u64::MAX,
        };

        let Err(error) = open_snapshot_copy(&db, &budget) else {
            panic!("an over-cap store must be refused instead of copied");
        };
        let SqliteError::Internal(message) = error else {
            panic!("expected the typed Internal refusal, got {error:?}");
        };
        assert!(
            message.contains(&db.display().to_string()),
            "the refusal must name the store: {message}"
        );
        assert!(
            message.contains(&format!("{db_bytes} bytes db")),
            "the refusal must name the db size: {message}"
        );
        assert!(
            message.contains(&format!("{wal_bytes} bytes write-ahead log")),
            "the refusal must name the `-wal` size: {message}"
        );
        assert!(
            message.contains(&format!("= {total_bytes} bytes")),
            "the refusal must name the total size: {message}"
        );
        assert!(
            message.contains(&format!("over the {cap}-byte")),
            "the refusal must name the cap: {message}"
        );
        assert!(
            message.contains("a later scan will read it"),
            "the refusal must say the app's next start makes the store readable: {message}"
        );
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no temp copy may be created for an over-cap store"
        );
        assert_eq!(
            quad_state(&db),
            before_quad,
            "a refused over-cap store must never be modified"
        );
        assert_eq!(
            directory_names(&db),
            before_names,
            "a refused over-cap store must not gain any file"
        );
    }

    /// A store under the cap with ample free space still falls back and
    /// reads through the snapshot copy — the F9 behavior preserved under
    /// the F18 budget — and the copy is removed when the connection drops.
    #[serial]
    #[test]
    fn snapshot_fallback_reads_a_store_under_the_size_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = wal_mode_store(dir.path());
        damage_shm(&db, "garbage");
        let budget = SnapshotBudget {
            max_bytes: SNAPSHOT_FALLBACK_MAX_BYTES,
            free_bytes: u64::MAX,
        };

        let conn = open_snapshot_copy(&db, &budget)
            .unwrap_or_else(|error| panic!("an under-cap store must fall back: {error:?}"));
        assert!(
            conn.used_snapshot_fallback(),
            "an under-cap store reads through the snapshot copy"
        );
        let value: i64 = conn
            .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 7, "the store's rows must be readable");
        let snapshot_dir = conn
            .snapshot_dir()
            .expect("a copy must exist")
            .to_path_buf();
        drop(conn);
        assert!(
            !snapshot_dir.exists(),
            "the snapshot copy must be removed when the connection drops"
        );
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no snapshot copy may outlive its connection"
        );
    }

    /// A store whose copy would leave the temp dir with less than the
    /// copy's own size free is refused with the typed scan error saying so,
    /// and no temp copy is created. The boundary is exact: `free_bytes`
    /// equal to twice the copy still leaves the copy's own size free, so it
    /// proceeds and reads; one byte less refuses.
    #[serial]
    #[test]
    fn snapshot_fallback_refuses_a_store_with_too_little_free_space() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = wal_mode_store(dir.path());
        damage_shm(&db, "garbage");
        let before_quad = quad_state(&db);
        let before_names = directory_names(&db);
        let total_bytes = store_total_bytes(&db);

        // The exact boundary still copies: exactly the copy's own size
        // would remain free, which is not less than the copy's own size.
        let boundary = SnapshotBudget {
            max_bytes: SNAPSHOT_FALLBACK_MAX_BYTES,
            free_bytes: total_bytes.saturating_mul(2),
        };
        let conn = open_snapshot_copy(&db, &boundary)
            .unwrap_or_else(|error| panic!("the boundary must still copy: {error:?}"));
        let value: i64 = conn
            .query_row_map("SELECT v FROM t", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(value, 7);
        drop(conn);
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "the boundary copy must be cleaned up before the refusal case"
        );

        // One byte less: the copy would leave less than the copy's own size
        // free, so it is refused before any copy is made.
        let free_bytes = total_bytes.saturating_mul(2) - 1;
        let budget = SnapshotBudget {
            max_bytes: SNAPSHOT_FALLBACK_MAX_BYTES,
            free_bytes,
        };
        let Err(error) = open_snapshot_copy(&db, &budget) else {
            panic!("a low-free-space store must be refused instead of copied");
        };
        let SqliteError::Internal(message) = error else {
            panic!("expected the typed Internal refusal, got {error:?}");
        };
        assert!(
            message.contains(&db.display().to_string()),
            "the refusal must name the store: {message}"
        );
        assert!(
            message.contains(&format!("{total_bytes} bytes to copy")),
            "the refusal must name the copy size: {message}"
        );
        assert!(
            message.contains(&format!("{free_bytes} bytes available")),
            "the refusal must name the free space: {message}"
        );
        assert!(
            message.contains("less than the copy"),
            "the refusal must say the copy would leave too little free: {message}"
        );
        assert!(
            message.contains("a later scan will read it"),
            "the refusal must say the app's next start makes the store readable: {message}"
        );
        assert!(
            leaked_snapshot_dirs().is_empty(),
            "no temp copy may be created for a low-free-space store"
        );
        assert_eq!(
            quad_state(&db),
            before_quad,
            "a refused low-free-space store must never be modified"
        );
        assert_eq!(
            directory_names(&db),
            before_names,
            "a refused low-free-space store must not gain any file"
        );
    }

    /// `shutdown_driver` is a compatibility no-op now that no connector
    /// runtime exists: it must keep succeeding, and the facade keeps working
    /// before and after it.
    #[test]
    fn shutdown_driver_is_a_no_op_and_the_facade_still_works() {
        std::thread::spawn(|| {
            assert!(shutdown_driver());
            let connection = Connection::open(":memory:").unwrap();
            connection
                .execute_batch("CREATE TABLE t (v INTEGER)")
                .unwrap();
            drop(connection);
            assert!(shutdown_driver());
            let connection = Connection::open(":memory:").unwrap();
            connection
                .execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
            let count: i64 = connection
                .query_row_map("SELECT count(*) FROM t", &[], |row| row.get_typed(0))
                .unwrap();
            assert_eq!(count, 1);
            drop(connection);
            assert!(shutdown_driver());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn read_transaction_rolls_back_callback_and_row_mapper_panics() {
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
                            panic!("read-transaction callback panic"); // ubs:ignore[rust.ownership.panic-macro] — Inject mapper unwind to prove rollback releases the transaction.
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
        }
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

    /// `params!` builds `&[ParamValue]` slices usable directly with the
    /// `ConnectionExt` `&[ParamValue]` signatures — bare `params![...]`,
    /// `params![]`, and a `let`-bound binding all work.
    #[test]
    fn params_macro_builds_param_slices() {
        let a = params![1_i64, "x", 2.5_f64, true, Option::<i64>::None];
        assert_eq!(a.len(), 5);
        assert!(matches!(a[0].0, SqliteValue::Integer(1)));
        assert!(matches!(a[1].0, SqliteValue::Text(ref s) if s == "x"));
        assert!(matches!(a[2].0, SqliteValue::Real(_)));
        assert!(matches!(a[3].0, SqliteValue::Integer(1)));
        assert!(matches!(a[4].0, SqliteValue::Null));
        let empty = params![];
        assert_eq!(empty.len(), 0);
        let bound: &[ParamValue] = params![9_i64, "z"];
        assert_eq!(bound.len(), 2);

        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER, s TEXT);")
            .unwrap();
        conn.execute_compat("INSERT INTO t VALUES (?, ?)", params![7_i64, "hi"])
            .unwrap();
        let v: i64 = conn
            .query_row_map("SELECT v FROM t", params![], |r| r.get_typed(0))
            .unwrap();
        assert_eq!(v, 7);
    }

    /// `query_for_each` visits every row without collecting a `Vec`.
    #[test]
    fn query_for_each_streams_rows_and_propagates_callback_errors() {
        let connection = Connection::open(":memory:").unwrap();
        connection
            .execute_batch(
                "CREATE TABLE values_seen (value INTEGER); \
                 INSERT INTO values_seen VALUES (1); \
                 INSERT INTO values_seen VALUES (2); \
                 INSERT INTO values_seen VALUES (3);",
            )
            .unwrap();
        let mut seen = Vec::new();
        connection
            .query_for_each("SELECT value FROM values_seen ORDER BY value", &[], |row| {
                seen.push(row.get_typed::<i64>(0)?);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, vec![1, 2, 3]);

        let error = connection
            .query_for_each("SELECT value FROM values_seen ORDER BY value", &[], |row| {
                let value: i64 = row.get_typed(0)?;
                assert_eq!(value, 1);
                Err(FrankenError::Internal("stop".to_owned()))
            })
            .unwrap_err();
        assert!(matches!(error, FrankenError::Internal(message) if message == "stop"));
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

        // An invalidated `-journal` (first byte zero) still proves emptiness.
        std::fs::write(sidecar(&db, "-journal"), [0_u8; 64]).unwrap();
        assert!(store_is_provably_empty(&db));

        // A live `-journal` defeats the proof.
        std::fs::write(sidecar(&db, "-journal"), b"\xd9live journal").unwrap();
        assert!(!store_is_provably_empty(&db));
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
            // A WAL commit is what makes SQLite record `szPage` in the
            // wal-index header; without one it stays 0 and the strict
            // in-place open rightfully refuses the store.
            conn.execute_batch("CREATE TABLE wal_probe (x INTEGER); DROP TABLE wal_probe;")
                .unwrap();
            conn.query_row_map::<(i64, i64, i64), _>(
                "PRAGMA wal_checkpoint(TRUNCATE);",
                &[],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
            )
            .unwrap();
            // Keep `-wal` and `-shm` on disk across the drop.
            conn.set_no_ckpt_on_close(true).unwrap();
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

    // Zero-open SQLite + WAL header state probe tests.

    fn set_whole_sec_mtime(path: &std::path::Path, unix_sec: u64, subsec_nanos: u32) {
        let time = SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(unix_sec)
            + std::time::Duration::from_nanos(u64::from(subsec_nanos));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open file to set mtime");
        file.set_modified(time).expect("set file mtime");
    }

    fn make_wal_bytes(
        magic: u32,
        version: u32,
        ckpt_seq: u32,
        salt_1: u32,
        salt_2: u32,
        total_len: usize,
    ) -> Vec<u8> {
        let mut bytes = vec![0_u8; total_len];
        bytes[0..4].copy_from_slice(&magic.to_be_bytes());
        bytes[4..8].copy_from_slice(&version.to_be_bytes());
        bytes[8..12].copy_from_slice(&4096_u32.to_be_bytes());
        bytes[12..16].copy_from_slice(&ckpt_seq.to_be_bytes());
        bytes[16..20].copy_from_slice(&salt_1.to_be_bytes());
        bytes[20..24].copy_from_slice(&salt_2.to_be_bytes());
        bytes
    }

    #[test]
    fn container_header_probe_returns_none_for_missing_and_state_for_zero_byte_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("missing.db");
        assert_eq!(stat_sqlite_container_header(&missing).unwrap(), None);

        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, []).unwrap();
        let first = stat_sqlite_container_header(&empty)
            .unwrap()
            .expect("0-byte db should return Some(state)");
        assert_eq!(first.db_size, 0);
        assert!(!first.db_header_valid);
        assert_eq!(first.change_counter, 0);
        assert_eq!(first.page_count, 0);
        assert_eq!(first.schema_cookie, 0);
        assert!(!first.wal_present);

        let second = stat_sqlite_container_header(&empty).unwrap().unwrap();
        assert!(!second.has_material_change_since(&first));
    }

    #[test]
    fn container_header_probe_rejects_non_sqlite_file_at_header_size() {
        let dir = tempfile::TempDir::new().unwrap();
        let bogus = dir.path().join("bogus.db");
        std::fs::write(&bogus, vec![b'X'; 128]).unwrap();
        let err = stat_sqlite_container_header(&bogus).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn container_header_probe_extracts_real_sqlite_header_fields_and_ignores_subsecond_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("sessions.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=delete; CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT); INSERT INTO t VALUES (1, 'hi');",
        )
        .unwrap();
        drop(conn);

        set_whole_sec_mtime(&db, 1_700_000_000, 100_000_000);
        let first = stat_sqlite_container_header(&db)
            .unwrap()
            .expect("real sqlite db should probe");
        assert!(first.db_header_valid);
        assert!(first.db_size >= 100);
        assert_eq!(first.db_mtime_sec, 1_700_000_000);
        #[cfg(unix)]
        assert_ne!(first.db_ino, 0);
        assert!(first.change_counter >= 1);
        assert!(first.page_count >= 1);
        assert!(first.schema_cookie >= 1);
        assert!(!first.wal_present);

        // Sub-second mtime drift within the same whole second is ignored.
        set_whole_sec_mtime(&db, 1_700_000_000, 900_000_000);
        let same_sec = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(!same_sec.has_material_change_since(&first));

        // Crossing a whole-second boundary registers.
        set_whole_sec_mtime(&db, 1_700_000_001, 0);
        let next_sec = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(next_sec.has_material_change_since(&first));
    }

    #[test]
    fn container_header_probe_ignores_zero_and_32_byte_frameless_wal_and_all_shm_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=delete; CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);",
        )
        .unwrap();
        drop(conn);

        let base = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(!base.wal_present);

        // Read-only / checkpointed clients can create a 0-byte `-wal` and `-shm`.
        let wal = sidecar(&db, "-wal");
        let shm = sidecar(&db, "-shm");
        std::fs::write(&wal, []).unwrap();
        std::fs::write(&shm, vec![0xAA_u8; 32_768]).unwrap();
        set_whole_sec_mtime(&wal, 1_700_000_100, 0);
        let with_zero_wal = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(!with_zero_wal.wal_present);
        assert!(!with_zero_wal.has_material_change_since(&base));

        // A 32-byte frameless `-wal` header + touched mtime + modified `-shm`
        // must STILL keep wal_present == false and report no material change.
        let frameless = make_wal_bytes(SQLITE_WAL_MAGIC_LE, SQLITE_WAL_FORMAT_VERSION, 1, 2, 3, 32);
        std::fs::write(&wal, frameless).unwrap();
        set_whole_sec_mtime(&wal, 1_700_000_200, 0);
        std::fs::write(&shm, vec![0xBB_u8; 32_768]).unwrap();
        let with_frameless_wal = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(!with_frameless_wal.wal_present);
        assert_eq!(with_frameless_wal.wal_mtime_sec, 0);
        assert!(!with_frameless_wal.has_material_change_since(&base));
        assert!(!with_frameless_wal.has_material_change_since(&with_zero_wal));
    }

    #[test]
    fn container_header_probe_detects_framed_wal_salt_and_ckpt_seq_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);

        let wal = sidecar(&db, "-wal");
        let wal_one = make_wal_bytes(
            SQLITE_WAL_MAGIC_LE,
            SQLITE_WAL_FORMAT_VERSION,
            4,
            100,
            200,
            128,
        );
        std::fs::write(&wal, wal_one).unwrap();
        set_whole_sec_mtime(&wal, 1_700_000_300, 0);

        let first = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(first.wal_present);
        assert_eq!(first.wal_size, 128);
        assert_eq!(first.wal_mtime_sec, 1_700_000_300);
        assert_eq!(first.wal_ckpt_seq, 4);
        assert_eq!(first.wal_salt_1, 100);
        assert_eq!(first.wal_salt_2, 200);

        // Same size and exact same mtime_sec, but ckpt_seq changed -> detected.
        let wal_ckpt = make_wal_bytes(
            SQLITE_WAL_MAGIC_BE,
            SQLITE_WAL_FORMAT_VERSION,
            5,
            100,
            200,
            128,
        );
        std::fs::write(&wal, wal_ckpt).unwrap();
        set_whole_sec_mtime(&wal, 1_700_000_300, 0);
        let second = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(second.has_material_change_since(&first));

        // Same size and exact same mtime_sec, ckpt_seq same as second, salt changed -> detected.
        let wal_salt = make_wal_bytes(
            SQLITE_WAL_MAGIC_BE,
            SQLITE_WAL_FORMAT_VERSION,
            5,
            101,
            200,
            128,
        );
        std::fs::write(&wal, wal_salt).unwrap();
        set_whole_sec_mtime(&wal, 1_700_000_300, 0);
        let third = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert!(third.has_material_change_since(&second));

        // Invalid WAL magic fails closed.
        let bad_magic = make_wal_bytes(0xdead_beef, SQLITE_WAL_FORMAT_VERSION, 5, 101, 200, 128);
        std::fs::write(&wal, bad_magic).unwrap();
        assert_eq!(
            stat_sqlite_container_header(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        // Unknown WAL version fails closed.
        let bad_version = make_wal_bytes(SQLITE_WAL_MAGIC_LE, 3_008_000, 5, 101, 200, 128);
        std::fs::write(&wal, bad_version).unwrap();
        assert_eq!(
            stat_sqlite_container_header(&db).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn container_header_probe_detects_change_counter_increment_with_identical_size_and_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=delete; CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);",
        )
        .unwrap();
        drop(conn);

        set_whole_sec_mtime(&db, 1_700_000_400, 0);
        let before = stat_sqlite_container_header(&db).unwrap().unwrap();

        // Update a row in-place (page count and file size stay equal) and force
        // the exact same mtime_sec; change_counter still detects the write.
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute("PRAGMA journal_mode=delete;").unwrap();
        conn.execute("UPDATE t SET v = 2;").unwrap();
        drop(conn);
        set_whole_sec_mtime(&db, 1_700_000_400, 0);

        let after = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert_eq!(after.db_size, before.db_size);
        assert_eq!(after.db_mtime_sec, before.db_mtime_sec);
        assert_ne!(after.change_counter, before.change_counter);
        assert!(after.has_material_change_since(&before));
    }

    #[cfg(unix)]
    #[test]
    fn container_header_probe_detects_inode_replacement_with_identical_bytes_and_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("store.db");
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);

        set_whole_sec_mtime(&db, 1_700_000_500, 0);
        let before = stat_sqlite_container_header(&db).unwrap().unwrap();

        let bytes = std::fs::read(&db).unwrap();
        let replacement = dir.path().join("store.db.tmp");
        std::fs::write(&replacement, bytes).unwrap();
        std::fs::rename(&replacement, &db).unwrap();
        set_whole_sec_mtime(&db, 1_700_000_500, 0);

        let after = stat_sqlite_container_header(&db).unwrap().unwrap();
        assert_ne!(after.db_ino, before.db_ino);
        assert_eq!(after.db_size, before.db_size);
        assert_eq!(after.db_mtime_sec, before.db_mtime_sec);
        assert_eq!(after.change_counter, before.change_counter);
        assert!(after.has_material_change_since(&before));
    }

    #[test]
    fn periodic_reverify_respects_interval_boundary() {
        use std::time::Duration;

        let max_interval = SQLITE_CONTAINER_REVERIFY_INTERVAL;
        assert!(!needs_periodic_reverify(
            max_interval.checked_sub(Duration::from_nanos(1)).unwrap(),
            max_interval
        ));
        assert!(needs_periodic_reverify(max_interval, max_interval));
        assert!(needs_periodic_reverify(
            max_interval.checked_add(Duration::from_secs(1)).unwrap(),
            max_interval
        ));
    }
}

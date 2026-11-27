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
        conn.query_row_map::<(i64, i64, i64), _>("PRAGMA wal_checkpoint(TRUNCATE);", &[], |row| {
            Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?))
        })
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

#[cfg(unix)]
#[serial]
#[test]
fn truncated_32_byte_wal_with_nonzero_mx_frame_falls_back_cleanly() {
    let protocol_err = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(15), None);
    assert!(
        wal_index_open_failure(&protocol_err),
        "SQLITE_PROTOCOL (FileLockingProtocolFailed) must map to BusyRecovery fallback"
    );

    let dir = tempfile::TempDir::new().unwrap();
    let db = wal_mode_store(dir.path());
    {
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute("INSERT INTO t VALUES (42);").unwrap();
        conn.set_no_ckpt_on_close(true).unwrap();
    }
    let wal = sidecar(&db, "-wal");
    let wal_bytes = std::fs::read(&wal).unwrap();
    assert!(
        wal_bytes.len() > SQLITE_WAL_HEADER_SIZE,
        "uncheckpointed WAL must contain at least one frame"
    );
    // Truncate `-wal` to its 32-byte header while `-shm` still reports mxFrame > 0.
    std::fs::write(&wal, &wal_bytes[..SQLITE_WAL_HEADER_SIZE]).unwrap();
    let before = quad_state(&db);
    let before_names = directory_names(&db);

    let Err(error) = open_external_strict_in_place(&db) else {
        panic!("truncated 32-byte -wal with mxFrame > 0 must refuse in-place open");
    };
    assert!(matches!(error, SqliteError::BusyRecovery));

    let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
        .unwrap_or_else(|error| panic!("snapshot fallback failed: {error:?}"));
    assert!(conn.used_snapshot_fallback());
    let values: Vec<i64> = conn
        .query_map_collect("SELECT v FROM t ORDER BY v", &[], |row| row.get_typed(0))
        .unwrap();
    assert_eq!(values, vec![7]);
    drop(conn);

    assert_eq!(quad_state(&db), before);
    assert_eq!(directory_names(&db), before_names);
    assert!(leaked_snapshot_dirs().is_empty());
}

#[cfg(unix)]
#[serial]
#[test]
fn stale_salt_in_shm_falls_back_to_snapshot_and_reads_committed_wal_rows() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = wal_mode_store(dir.path());
    let shm = sidecar(&db, "-shm");
    let stale_shm = std::fs::read(&shm).unwrap();

    // Start a fresh WAL generation with a new salt and commit row 42 into `-wal`
    // without checkpointing, then restore the earlier valid `-shm` (stale aSalt,
    // mxFrame == 0) so `readonly_shm=1` would otherwise silently hide row 42.
    {
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        conn.execute("INSERT INTO t VALUES (42);").unwrap();
        conn.set_no_ckpt_on_close(true).unwrap();
    }
    let wal_bytes = std::fs::read(sidecar(&db, "-wal")).unwrap();
    assert!(wal_bytes.len() > SQLITE_WAL_HEADER_SIZE);
    assert_ne!(
        &wal_bytes[16..24],
        &stale_shm[32..40],
        "new WAL generation must carry a fresh aSalt"
    );
    std::fs::write(&shm, &stale_shm).unwrap();
    let before = quad_state(&db);
    let before_names = directory_names(&db);

    let Err(error) = open_external_strict_in_place(&db) else {
        panic!("stale aSalt in -shm must refuse in-place open");
    };
    assert!(matches!(error, SqliteError::BusyRecovery));

    let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
        .unwrap_or_else(|error| panic!("snapshot fallback failed: {error:?}"));
    assert!(conn.used_snapshot_fallback());
    let values: Vec<i64> = conn
        .query_map_collect("SELECT v FROM t ORDER BY v", &[], |row| row.get_typed(0))
        .unwrap();
    assert_eq!(
        values,
        vec![7, 42],
        "snapshot fallback must recover committed WAL rows instead of hiding them"
    );
    drop(conn);

    assert_eq!(quad_state(&db), before);
    assert_eq!(directory_names(&db), before_names);
    assert!(leaked_snapshot_dirs().is_empty());
}

#[test]
fn cleanly_closed_wal_database_with_no_tables_is_provably_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = zero_table_store(dir.path(), "store.db");
    {
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        let mode: String = conn
            .query_row_map("PRAGMA journal_mode=wal;", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }
    assert!(!sidecar(&db, "-wal").exists());
    assert!(!sidecar(&db, "-shm").exists());
    assert!(store_is_provably_empty(&db));
    assert!(!sidecar(&db, "-wal").exists());
    assert!(!sidecar(&db, "-shm").exists());
}

#[test]
fn wal_database_with_table_only_in_wal_and_damaged_shm_is_not_provably_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = zero_table_store(dir.path(), "store.db");
    {
        let conn = Connection::open(db.to_string_lossy().as_ref()).unwrap();
        let mode: String = conn
            .query_row_map("PRAGMA journal_mode=wal;", &[], |row| row.get_typed(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        conn.query_row_map::<(i64, i64, i64), _>("PRAGMA wal_checkpoint(TRUNCATE);", &[], |row| {
            Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?))
        })
        .unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        conn.set_no_ckpt_on_close(true).unwrap();
    }
    damage_shm(&db, "garbage");
    assert!(!store_is_provably_empty(&db));
}

#[cfg(unix)]
#[test]
fn same_process_live_wal_writer_falls_back_without_mutating_shm_read_marks() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("store.db");
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE t (v INTEGER);
             INSERT INTO t VALUES (1);
             SELECT COUNT(*) FROM t;",
        )
        .unwrap();
    // Advance WAL past aReadMark without running a read on `writer` so a
    // shared-pShmNode reader would bump `-shm` aReadMark[1] (offset 104).
    writer.execute("INSERT INTO t VALUES (2);", []).unwrap();
    let before = quad_state(&db);
    let before_names = directory_names(&db);

    let Err(err) = open_external_strict_in_place(&db) else {
        panic!("same-process live WAL writer must refuse in-place open");
    };
    assert!(matches!(err, SqliteError::BusyRecovery), "unexpected: {err:?}");
    assert_eq!(quad_state(&db), before, "strict refusal must not touch shm");

    let conn = open_read_only_with_snapshot_fallback(db.to_string_lossy().as_ref())
        .unwrap_or_else(|error| panic!("snapshot fallback failed: {error:?}"));
    assert!(conn.used_snapshot_fallback());
    let mut values: Vec<i64> = conn
        .query_map_collect("SELECT v FROM t ORDER BY v", &[], |row| row.get_typed(0))
        .unwrap();
    values.sort_unstable();
    assert_eq!(values, vec![1, 2]);
    drop(conn);
    assert_eq!(quad_state(&db), before, "reader must preserve db/wal/shm bytes");
    assert_eq!(directory_names(&db), before_names);
    drop(writer);
}

# Upgrade Log

## 2026-11-25 — migrate SQLite layer from `frankensqlite`/`asupersync` to `rusqlite` `=0.40.2`

Replaced `frankensqlite` (`fsqlite`) and `asupersync` with synchronous `rusqlite`
(superseding the deferred `fsqlite` bump below):

- Added `rusqlite = { version = "=0.40.2", features = ["bundled", "modern_sqlite"], optional = true }`
  under `_sqlite-sync = ["connectors", "dep:rusqlite", "dep:fs4"]` in `Cargo.toml`,
  wired into the 4 surviving SQLite connector features: `cursor`, `opencode`, `devin`,
  and `qoder-sqlite`.
- Removed `frankensqlite`, `asupersync`, `[profile.dev.package."*"]` `opt-level = 1`,
  and the 20 `[patch.crates-io]` `fsqlite*` redirects from `Cargo.toml`, and gated
  `connectors::sqlite_sync` behind `#[cfg(feature = "_sqlite-sync")]`.
- Split `src/connectors/sqlite_sync.rs` into `src/connectors/sqlite_sync.rs` (1,561
  lines) and `src/connectors/sqlite_sync/tests.rs` (1,479 lines) to satisfy the
  `<= 2,000` LOC per-file policy, retaining `FrankenError`, `ParamValue`, and
  `shutdown_driver()` compatibility wrappers.
- Added the `-wal`/`-shm` header and salt/frame-bound coherence guard
  (`wal_and_shm_are_coherent`) and `SQLITE_PROTOCOL` (`FileLockingProtocolFailed`)
  fallback to `BusyRecovery` in `sqlite_sync` so `mode=ro&readonly_shm=1` opens
  never mutate or fail on stale/corrupt WAL sidecars and fall back to the bounded
  private snapshot copy.

## 2026-06-17 — third-party dependency bumps

Updated compatible third-party dependencies (Cargo.lock only; semver ranges in
Cargo.toml already permit these):

- chrono 0.4.44 → 0.4.45 (patch)
- serde_json 1.0.149 → 1.0.150 (patch)
- serial_test 3.4.0 → 3.5.0 (minor, dev-dep)

Deferred:

- fsqlite 0.1.3 → 0.1.10 deferred — coordinated franken release (handled by parent).

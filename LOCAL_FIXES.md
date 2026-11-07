# cass-local-fixes

Base: 6b337ba434b72c65c6c5a748b7ec47885f840ac0 (FAD 0.3.10, pinned by CASS).
Each commit on this branch is one additive fix; see CASS
docs/superpowers/plans/2026-11-02-review-fixes-05-fad-fork.md.

| Task | Commit | Drop when upstream… |
|---|---|---|
| F1 | (this commit) | reads aiTitle for ai-title records |
| F2 | (this commit) | keeps thinking blocks as searchable content |
| F3 | (this commit) | keeps planner tool_calls; GENERIC results are not tools |
| F4 | (this commit) | loads main-chain payloads in batches of 256 |
| F5 | (this commit) | an unreadable database fails the scan instead of hiding |
| F6 | (this commit) | sums distinct-request token_count events, skips repeats |
| F7 | (this commit) | ticks progress per row and composer inside Cursor's extract_from_db |
| F8 | (this commit) | Cursor key-range queries seek the key index (no residual predicate), or frankensqlite plans a range plus a residual predicate as an index seek |
| F9 | (this commit) | reads external SQLite stores through a private snapshot copy when the wal-index is unusable (never writes the user's db/`-wal`/`-shm`), and every FAD SQLite open site plus OpenCode/Cursor/Crush/Hermes/Goose scans surface open failures as scan errors (F5 semantics), except a store that is a valid but empty database (0-byte file with no `-wal` content, or a schema with no tables), which scans to Ok with no sessions instead of a scan error |

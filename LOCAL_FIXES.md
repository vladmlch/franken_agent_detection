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

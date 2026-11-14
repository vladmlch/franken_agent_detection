# Cursor subagent verdicts: rule, accepted limits, open findings

Status: 2026-11-14, branch `feat/subagent-verdicts`. The owner stopped the
fix loop after round 10 ("запиши находки по курсор в отдельный документ и
иди дальше"). This file records the rule the connector implements, the
limits accepted on purpose, and the findings that are still open, so the
next change starts from here instead of from the review threads.

## What the connector emits

`NormalizedConversation.metadata` carries two optional keys for every
Cursor composer:

- `is_subagent` — a JSON bool, present only when the source proves it.
- `parent_id` — the parent composer id, present only for a subagent with a
  known parent.

## The rule

This section states the intended rule. The code meets it except where an
entry under "Open findings" says otherwise.

1. **True wins.** A composer is a subagent (`true`, with its parent when
   known) when a read source proves it: its own `composerHeaders` row
   (column `isSubagent`, or the row's `value` JSON), its `ItemTable`
   `composer.composerHeaders` entry, a `parentComposerId`/`parentId` on its
   own header or composer data, or another composer's
   `subagentComposerIds` list. No later read and no other source can
   downgrade a proven subagent or erase a known parent.
2. **False only from the composer's own explicit flag.** `false` is emitted
   only when the composer's own header, read after its rows, explicitly
   says not-subagent and the evidence collected so far does not contradict
   it.
3. **Everything else is absent.** No flag, an uninterpretable flag, or an
   uncertain store gives no verdict.

Flag decoding is explicit, not Python truthiness: `true`, `1`, `"true"`,
`"1"` mean subagent; `false`, `0`, `"false"`, `"0"` mean not-subagent
(trimmed, case-insensitive). Any other value proves nothing and vetoes
`false`.

Reads take no explicit transaction on the user's live store. Composer data
and evidence are read before the emitting store's header read, which is the
only read that can certify `false`. This is an ordering rule that relies on
the accepted assumption about Cursor's write order (below). It is not an
atomic snapshot of the scan: later reads of other stores, or later lookup
passes, are not part of the same snapshot.

## Accepted limits

- **Parents in stores the scan did not read.** A parent's
  `subagentComposerIds` can live in a store this scan did not read, for
  example a partial mirror or a workspace the scan was not pointed at. Then
  the child's own explicit `false` stands. `false` means "Cursor's own
  header says not-subagent, and nothing read in this scan contradicts it."
  The owner's ruling B1 (round 7) accepts this limit, and it is documented
  at `subagent_verdict`.
- **Cursor's write order.** Reading the headers last assumes Cursor writes a
  subagent's header together with or before its rows. Neither the code nor
  the native viewer proves this. No review found a wrong-`false` witness
  that depends on it.
- **Evidence committed after it was read.** Even in a store the scan did
  read, a parent listing or a true header committed after that read is not
  seen, and an explicit own-header `false` read earlier stands. The code
  comment at `subagent_verdict` states this; it is the same accepted limit
  as unread stores.
- **Fractional flags.** `1.0`/`0.0` (SQL REAL, JSON floats) prove nothing,
  so the verdict stays absent where native would decide.
- **Native deviations, on purpose.**
  - Native uses `bool(value)`, so the string `"false"` counts as a subagent
    there; FAD decodes it as not-subagent.
  - Native lets a newer header overwrite an older one. FAD never lets a
    later read downgrade a proven subagent.
  - Native recovers malformed UTF-8 in `composerData` with
    `errors="ignore"`. FAD rejects a non-UTF-8 cell as undecodable (round
    10), so such a composer may not be emitted at all even when its header
    is readable.

## Owner-data check

The round-6 rule was checked on a copy of the owner's Cursor
`state.vscdb`. The count was not re-run on the final tree (`674e02c`), so
it does not show that the edge cases below are handled:

| Verdict | Composers (FAD rule) | Native classifier |
|---|---|---|
| Not-subagent (`false`) | 443 | 445 main |
| Subagent (`true`) | 1026 | 1026 |
| No verdict (absent) | 19 | — |
| **Total** | **1488** | **1471** |

## Designs that were tried and dropped

- **Rounds 1–5: completeness gating.** `false` was allowed only when every
  store was read completely, then only for a fully read local installation
  with a proven inventory. Reviews kept finding wrong-`false` witnesses
  that path checks cannot see: hard links, bind mounts, symlinked members
  attributed to another installation, and the ordinary race where Cursor
  commits a parent and a child between the evidence read and the
  extraction read. Round 6 deleted the gate.
- **Round 7: one read transaction per store.** It fixed the race but held a
  SHARED lock and pinned the WAL reader horizon on the user's live store
  while Rust decoded all the JSON. Round 8 replaced it with the read order
  above.

## Open findings

These findings come from the final review of `674e02c`, which is
`.scratch/qoder/2026-11-14-fad-cursor-rereview10/review.md` in the cass
checkout; read it for the full witness of each. They are open because the
owner stopped the fix loop after round 10.

Already fixed in `674e02c`:

- **N10:** strict UTF-8 for `composerData` BLOBs.
- **M3:** the INTEGER-cell fixture now reaches the `Undecodable` arm.

Every open finding below is Important.

- **N6/O5 — A single-store scan forgets evidence between extraction and
  lookup.** With exactly one store, `external_evidence` is `None`, so the
  uncertainty and parents captured in pass 1 die when extraction returns.
  Pass 2, the lookup, triggered by a fresh transcript, can then certify
  `false` after a writer cleaned the bad row. Fix: allocate one evidence
  accumulator for the whole scan even for a single store, and pass it
  through both passes.
- **O6 — Later evidence cannot revise an emitted `false`.** If store A
  emits C=false and store B, read later in the same scan, proves C=true, the
  duplicate from B is dropped. The same happens to results already
  collected by lookup. Fix: hold `false` candidates until every planned read
  has finished, then resolve them against the final accumulator before
  publishing.
- **O7 — Header JSON still decodes malformed bytes lossily.** Header
  `value` BLOBs and `ItemTable` JSON turn invalid bytes into `�`. That can
  invent a parent id `P�` that replaces the correctly read SQL parent `P`.
  Raw TEXT headers use unchecked `as_str()`. Fix: decode header and
  identity cells as strict UTF-8, the same way as `composerData`.
- **O8 — Accumulated header uncertainty does not veto `false`.** An
  unsupported flag (for example `"bogus"`) in one store's header for C is
  recorded as `flag_unknown`. A clean late `false` from another store still
  certifies C. Fix: also veto `false` when the accumulated header evidence
  for that composer has an unknown component.
- **O9 — An unreadable own-header `value` is treated as absent.** A SQL
  header `(C, 0, "{not valid json")` emits `false`, although the unreadable
  component could have held a positive. The test
  `malformed_composer_headers_value_or_item_table_withholds_false` does not
  catch this: it has no other false header. Fix: mark a present but
  undecodable or non-object value as unknown.
- **O10 — Explicit `null` in composerData `isSubagent` is treated as a
  missing field.** Serde maps both to `None`, so `{"isSubagent":null}` plus
  a clean `false` header emits `false`. In header JSON, by contrast, `null`
  is unknown. Fix: keep presence apart from value and treat explicit
  `null` as unknown.
- **O11 — Uncertainty is keyed by literal path, not by physical store.**
  Two symlinked paths to one database keep separate uncertainty. A bad row
  seen through path A does not stop a `false` emitted through path B. Fix:
  key uncertainty by canonical path, and by file identity for hard links;
  or document literal-path identity as the contract.

The review also noted that, before this update, "The rule" overstated the
code and called the read order a snapshot guarantee. Those sections now
carry the corrected wording.

## History

Verdict work started at `a9f35e0` (base `18e2ffa`).

| Round | Commit | Executor | Review |
|---|---|---|---|
| 1 | `a9f35e0..252f40d` (all four connectors) | codex qwen4 | agy carbon-high |
| 2 | `9b78e19` (fixes after review, codex/antigravity/cursor) | agy hydrogen-high | agy carbon-high |
| 3 | `f7ad7d1` | codex qwen4 high | Sonus, 270 credits |
| 4 | `42f3ebc` | codex qwen4 high | Sonus, 249 credits |
| 5 | `a84b7ca` | codex qwen4 high | Sonus, 249 credits |
| 6 | `ec9e9e6` | codex qwen4 high | Sonus, 257 credits |
| 7 | `e44fc69` (own-header rule) | devin swe-2-max | Sonus, 363 credits |
| 8 | `e11637e` (one snapshot per store) | devin swe-2-max | Sonus, 337 credits |
| 9 | `e538681` (read order) | devin swe-2-max | Sonus, 316 credits |
| 10 | `4d43562` (post-row false, BLOB JSON) | devin swe-2-max | Sonus, 250 credits |
| 11 | `674e02c` (sticky store uncertainty, strict BLOBs) | devin swe-2-max | Sonus, 272 credits — final; findings recorded above |

Briefs, reports and reviews live outside this repository, in the cass
checkout's `.scratch/` tree:

- `.scratch/codex/2026-11-14-fad-cursor-verdict-fix2/`: rounds 3–6, briefs
  and report;
- `.scratch/devin/2026-11-14-fad-cursor-round6/`: rounds 7–11, follow-ups
  and report;
- `.scratch/qoder/2026-11-14-fad-cursor-rereview2..N/`: the Sonus reviews.

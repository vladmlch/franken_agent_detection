//! Subagent evidence gathering and verdict resolution for Cursor composers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::CursorConnector;
use super::composer_parser::CURSOR_DISK_KV_RANGE_SQL;
use crate::connectors::parse_timestamp;
use crate::connectors::scan::ScanRoot;
use crate::connectors::sqlite_sync::{
    Connection, ConnectionExt, Row, RowExt, SqliteError, SqliteValueRef,
    open_read_only_with_snapshot_fallback, params, store_is_provably_empty,
};
use crate::types::NormalizedConversation;

/// Per-composer subagent flag/parent from a header row or the composer's own
/// JSON (`isSubagent` / `parentComposerId` / `parentId`).
#[derive(Clone, Debug, Default)]
pub(super) struct ComposerSubagentFlag {
    pub(super) is_subagent: Option<bool>,
    pub(super) parent_id: Option<String>,
    pub(super) last_updated_at: Option<i64>,
    /// True when some header isSubagent component for this composer could not be
    /// interpreted (round-6). A false verdict requires a cleanly decoded own
    /// header, so an unknown component withholds it.
    pub(super) flag_unknown: bool,
}

/// Lightweight partial deserialization of `composerData:*` JSON for subagent
/// evidence, avoiding full DOM tree allocation for `tabs` / `bubbles`.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ComposerEvidenceJson {
    #[serde(default)]
    subagent_composer_ids: Option<Value>,
    #[serde(default)]
    parent_composer_id: Option<Value>,
    #[serde(default)]
    parent_id: Option<Value>,
    /// Own `isSubagent` only proves positive subagent evidence (round-6: a
    /// composerData flag is not an own-header record and never emits false, so
    /// the absent/null distinction no longer matters).
    #[serde(default)]
    is_subagent: Option<Value>,
}

/// Subagent evidence gathered for one or more Cursor stores before composers parse.
#[derive(Clone, Default)]
pub(super) struct ComposerSubagentEvidence {
    /// child composerId -> parent composerId, from `subagentComposerIds`.
    pub(super) child_to_parent: HashMap<String, String>,
    /// composerId -> its own JSON `isSubagent`/`parentComposerId`/`parentId`.
    pub(super) own: HashMap<String, ComposerSubagentFlag>,
    /// composerId -> `composerHeaders` / `ItemTable` flag/parent.
    pub(super) headers: HashMap<String, ComposerSubagentFlag>,
    /// Identity (`db_path`) of every store whose captured `composerData` rows
    /// contained at least one row FAD could not interpret at all (round-8/9/10
    /// N6 ruling): the unreadable payload might have held a
    /// `subagentComposerIds` listing for ANY composer, so it withholds every
    /// false certified FROM THAT STORE — but no other store's. The set is
    /// merged by `extend_subagent_evidence`, so the uncertainty is sticky for
    /// the whole scan: once ANY read of store S captured an undecodable row,
    /// no composer of S gets false in this scan even when a later reread of S
    /// is clean. Decodable rows' positive/parent evidence is always kept.
    pub(super) uncertain_stores: HashSet<PathBuf>,
}

/// One `cursorDiskKV` value cell under a `composerData:*` key. The column is
/// declared TEXT but SQLite types per value: a stored TEXT carries the
/// composer JSON, NULL is an internal marker, and a BLOB whose bytes are
/// UTF-8 JSON is decoded exactly like TEXT — the native reference does the
/// same (`v.decode("utf-8", errors="ignore")` then `json.loads`,
/// claude_viewer viewer/providers/cursor/discovery.py). Any other storage
/// class (INTEGER, REAL) is evidence this connector cannot decode. Reading
/// the raw `SqliteValueRef` instead of `Option<String>` keeps one such cell
/// from aborting the whole range query (round-8 N6 residual); a genuinely
/// undecodable row marks the store uncertain for false, not just its own id.
pub(super) enum ComposerDataCell {
    Null,
    Text(String),
    Undecodable,
}

/// One `composerHeaders` SQL cell. SQLite columns are dynamically typed, so a
/// column declared INTEGER can still hold REAL/TEXT/BLOB; the reader interprets
/// each storage class instead of aborting the table scan on a type mismatch.
enum HeaderCell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    /// The column could not be read at all.
    Unreadable,
}

/// One `composerHeaders` row, read tolerantly. `sub_unknown` means the
/// `isSubagent` cell held a shape FAD cannot decode: an own-header false is
/// withheld for that composer, while the row's readable evidence still counts
/// (a proven subagent stays one).
struct ComposerHeaderRow {
    id: String,
    is_subagent: Option<bool>,
    parent_id: Option<String>,
    value: Option<String>,
    last_updated_at: Option<i64>,
    sub_unknown: bool,
}

impl CursorConnector {
    /// Explicit `isSubagent` decoding (round-3 orchestrator ruling, replacing
    /// the round-2 Python-truthiness rule). Proven true: JSON `true`, integer
    /// `1`, and the strings `"true"`/`"1"` (case-insensitive, trimmed). Proven
    /// false: JSON `false`, integer `0`, and the strings `"false"`/`"0"`.
    /// Anything else — other strings (including `""`), other numbers (floats,
    /// `2`, `-1`), objects, arrays — proves nothing: `None` tells the caller the
    /// flag is unknown rather than guess a verdict.
    ///
    /// Deviation from native: models.py:48 uses Python `bool(...)`, which makes
    /// every non-empty string (even `"false"`/`"0"`) truthy. FAD decodes the
    /// supported encodings semantically instead, because an unsupported
    /// positive is as wrong as an unsupported negative, and the monotonic
    /// merge would make an invented positive permanent.
    pub(super) fn decode_is_subagent(value: &Value) -> Option<bool> {
        match value {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => match n.as_i64() {
                Some(1) => Some(true),
                Some(0) => Some(false),
                _ => None,
            },
            Value::String(s) => Self::decode_is_subagent_str(s),
            _ => None,
        }
    }

    /// The string part of `decode_is_subagent`, shared with SQL TEXT cells:
    /// trimmed, case-insensitive `"true"`/`"1"` and `"false"`/`"0"` only.
    fn decode_is_subagent_str(s: &str) -> Option<bool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    /// `isSubagent` from a header JSON object with explicit decoding. A present
    /// value FAD cannot interpret — including an explicit `null` — marks the
    /// flag unknown so an own-header false is withheld (round-6: a present null
    /// is not an absent field).
    fn header_bool_field(obj: &Map<String, Value>, unknown: &mut bool) -> Option<bool> {
        match obj.get("isSubagent") {
            None => None,
            Some(Value::Null) => {
                *unknown = true;
                None
            }
            Some(v) => {
                let parsed = Self::decode_is_subagent(v);
                if parsed.is_none() {
                    *unknown = true;
                }
                parsed
            }
        }
    }

    /// A non-empty string id from a header JSON object. A present non-string or
    /// empty id is an unknown shape (native stringifies it; FAD declines to
    /// guess): it simply does not attribute the field.
    fn header_id_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
        match obj.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    /// Merge one composer's flag/parent evidence. Positive subagent evidence is
    /// monotonic: once any source proves `is_subagent = true`, no other source
    /// (including a newer or partial header) may downgrade it to false/absent,
    /// and a known `parent_id` is never erased by a source that lacks one. A
    /// true/false conflict resolves to true. Native overwrites headers by
    /// `lastUpdatedAt` (discovery.py:198-201, 326-330); FAD deviates deliberately
    /// because a wrong `false` is worse than a stale positive. Timestamps only
    /// break ties between two known parents.
    pub(super) fn merge_flag(
        map: &mut HashMap<String, ComposerSubagentFlag>,
        id: String,
        incoming: ComposerSubagentFlag,
    ) {
        match map.get_mut(&id) {
            None => {
                map.insert(id, incoming);
            }
            Some(existing) => {
                let inc_ts = incoming.last_updated_at.unwrap_or(0);
                let cur_ts = existing.last_updated_at.unwrap_or(0);
                existing.is_subagent = match (existing.is_subagent, incoming.is_subagent) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (a, b) => a.or(b),
                };
                existing.flag_unknown |= incoming.flag_unknown;
                if incoming.parent_id.is_some() && (existing.parent_id.is_none() || inc_ts > cur_ts)
                {
                    existing.parent_id = incoming.parent_id;
                }
                existing.last_updated_at =
                    match (existing.last_updated_at, incoming.last_updated_at) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
            }
        }
    }

    fn text_or_bytes_column(
        row: &Row<'_>,
        idx: usize,
    ) -> std::result::Result<Option<String>, SqliteError> {
        // TEXT and BLOB cells both decode through the same lossy UTF-8 view
        // the typed `Option<String>`/`Option<Vec<u8>>` pair produced; any
        // other storage class (or an unreadable column) is an error.
        match row.get_ref(idx) {
            Ok(SqliteValueRef::Null) => Ok(None),
            Ok(SqliteValueRef::Text(bytes) | SqliteValueRef::Blob(bytes)) => {
                Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
            }
            _ => Err(SqliteError::Internal(format!(
                "column {idx} is not TEXT, BLOB, or NULL"
            ))),
        }
    }

    /// Native `_safe_int` truncates `int(float(val))` toward zero.
    #[allow(clippy::cast_possible_truncation)]
    fn safe_int_from_f64(v: f64) -> i64 {
        v as i64
    }

    fn header_cell(row: &Row<'_>, idx: usize) -> HeaderCell {
        match row.get_ref(idx) {
            Ok(SqliteValueRef::Null) => HeaderCell::Null,
            Ok(SqliteValueRef::Integer(i)) => HeaderCell::Int(i),
            Ok(SqliteValueRef::Real(f)) => HeaderCell::Real(f),
            Ok(SqliteValueRef::Text(bytes)) => {
                HeaderCell::Text(String::from_utf8_lossy(bytes).into_owned())
            }
            Ok(SqliteValueRef::Blob(bytes)) => HeaderCell::Blob(bytes.to_vec()),
            Err(_) => HeaderCell::Unreadable,
        }
    }

    /// Read one `composerHeaders` row without aborting on REAL/TEXT/BLOB cells
    /// (discovery.py:20-27 parses `lastUpdatedAt` via `_safe_int`). An
    /// undecodable `isSubagent` cell sets `sub_unknown` so the composer does not
    /// get an own-header false, while the row's readable evidence is kept.
    fn read_composer_header_row(
        row: &Row<'_>,
        sub_idx: Option<usize>,
        parent_idx: Option<usize>,
        value_idx: Option<usize>,
        updated_idx: Option<usize>,
    ) -> ComposerHeaderRow {
        let mut out = ComposerHeaderRow {
            id: String::new(),
            is_subagent: None,
            parent_id: None,
            value: None,
            last_updated_at: None,
            sub_unknown: false,
        };
        match Self::header_cell(row, 0) {
            HeaderCell::Text(s) => out.id = s,
            HeaderCell::Null => {}
            _ => {}
        }
        if let Some(i) = sub_idx {
            match Self::header_cell(row, i) {
                HeaderCell::Null => {}
                HeaderCell::Int(v) => {
                    out.is_subagent = Self::decode_is_subagent(&Value::from(v));
                    if out.is_subagent.is_none() {
                        out.sub_unknown = true;
                    }
                }
                // Supported TEXT spellings only; REAL/BLOB and other integers
                // are not proven encodings (round-3 ruling).
                HeaderCell::Text(s) => {
                    out.is_subagent = Self::decode_is_subagent_str(&s);
                    if out.is_subagent.is_none() {
                        out.sub_unknown = true;
                    }
                }
                _ => out.sub_unknown = true,
            }
        }
        if let Some(i) = parent_idx {
            match Self::header_cell(row, i) {
                HeaderCell::Null => {}
                HeaderCell::Text(s) if !s.is_empty() => out.parent_id = Some(s),
                HeaderCell::Text(_) => {}
                _ => {}
            }
        }
        if let Some(i) = value_idx {
            match Self::header_cell(row, i) {
                HeaderCell::Null => {}
                HeaderCell::Text(s) => out.value = Some(s),
                HeaderCell::Blob(b) => out.value = Some(String::from_utf8_lossy(&b).into_owned()),
                _ => {}
            }
        }
        if let Some(i) = updated_idx {
            match Self::header_cell(row, i) {
                HeaderCell::Null => {}
                HeaderCell::Int(v) => out.last_updated_at = Some(v),
                HeaderCell::Real(v) if v.is_finite() => {
                    out.last_updated_at = Some(Self::safe_int_from_f64(v));
                }
                HeaderCell::Text(s) => {
                    if let Ok(v) = s.parse::<i64>() {
                        out.last_updated_at = Some(v);
                    } else if let Some(v) = s.parse::<f64>().ok().filter(|f| f.is_finite()) {
                        out.last_updated_at = Some(Self::safe_int_from_f64(v));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn sqlite_table_exists(conn: &Connection, table_name: &str) -> Result<bool, ()> {
        conn.query_map_collect(
            "SELECT name FROM sqlite_master WHERE type='table' AND name = ?",
            params![table_name],
            |row| row.get_typed::<String>(0),
        )
        .map(|rows| !rows.is_empty())
        .map_err(|_| ())
    }

    /// Interpret one `cursorDiskKV` value cell at column `idx` in place: TEXT
    /// and valid-UTF-8 BLOB bytes carry the composer JSON, NULL is an
    /// internal marker, everything else is undecodable. Decoding is STRICT
    /// (round-10 N10): a BLOB that is not valid UTF-8 is undecodable — no
    /// lossy/ignore-errors recovery, because replacement characters inside
    /// an identifier would invent composers (e.g. `P�`) that can shadow a
    /// real parent or listing. Raw (non-UTF-8) TEXT gets the same verdict.
    pub(super) fn composer_data_cell(row: &Row<'_>, idx: usize) -> ComposerDataCell {
        match row.get_ref(idx) {
            Ok(SqliteValueRef::Text(bytes) | SqliteValueRef::Blob(bytes)) => {
                match std::str::from_utf8(bytes) {
                    Ok(t) => ComposerDataCell::Text(t.to_string()),
                    Err(_) => ComposerDataCell::Undecodable,
                }
            }
            Ok(SqliteValueRef::Null) => ComposerDataCell::Null,
            _ => ComposerDataCell::Undecodable,
        }
    }

    /// Fold one composer row's own JSON into the evidence map: its
    /// `subagentComposerIds` listing and its own `isSubagent`/parent fields.
    /// Shared by the bulk row scan and by the targeted lookup's exact per-sid
    /// hits — a fetched hit's own JSON proves subagent/parent even when the
    /// bulk evidence read failed (round-8 N6 residual). A row whose payload
    /// cannot be interpreted marks the composer's own flag unknown AND
    /// records `store` in `uncertain_stores` — the unreadable payload might
    /// have listed any composer as a subagent, so it withholds every false
    /// certified from that store (round-9/10 N6). A payload that is
    /// verifiably empty is skipped like the native reader (`if not v`).
    pub(super) fn fold_composer_value(
        ev: &mut ComposerSubagentEvidence,
        id: &str,
        value: &str,
        store: &Path,
    ) {
        if value.trim().is_empty() {
            return;
        }
        let mark_unknown = |ev: &mut ComposerSubagentEvidence| {
            Self::merge_flag(
                &mut ev.own,
                id.to_string(),
                ComposerSubagentFlag {
                    flag_unknown: true,
                    ..ComposerSubagentFlag::default()
                },
            );
        };
        if !value.trim_start().starts_with('{') {
            mark_unknown(ev);
            ev.uncertain_stores.insert(store.to_path_buf());
            return;
        }
        let Ok(parsed) = serde_json::from_str::<ComposerEvidenceJson>(value) else {
            mark_unknown(ev);
            ev.uncertain_stores.insert(store.to_path_buf());
            return;
        };
        match &parsed.subagent_composer_ids {
            Some(Value::Array(arr)) => {
                for cid in arr.iter().filter_map(Value::as_str) {
                    if !cid.is_empty() {
                        ev.child_to_parent.insert(cid.to_string(), id.to_string());
                    }
                }
            }
            Some(Value::Null) | None => {}
            // A listing FAD cannot read could have named any child.
            Some(_) => {
                ev.uncertain_stores.insert(store.to_path_buf());
            }
        }
        let mut flag_unknown = false;
        let mut parse_parent = |val: &Option<Value>| -> Option<String> {
            match val {
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(Value::Null) | None => None,
                Some(_) => {
                    flag_unknown = true;
                    None
                }
            }
        };
        let parent_id =
            parse_parent(&parsed.parent_composer_id).or_else(|| parse_parent(&parsed.parent_id));
        let is_subagent = match &parsed.is_subagent {
            Some(v) => {
                if let Some(b) = Self::decode_is_subagent(v) {
                    Some(b)
                } else {
                    // A present-but-uninterpretable isSubagent — including an
                    // explicit null — is not an absent field (same ruling as
                    // header_bool_field).
                    flag_unknown = true;
                    None
                }
            }
            None => None,
        };
        if parent_id.is_some() || is_subagent.is_some() || flag_unknown {
            Self::merge_flag(
                &mut ev.own,
                id.to_string(),
                ComposerSubagentFlag {
                    is_subagent,
                    parent_id,
                    last_updated_at: None,
                    flag_unknown,
                },
            );
        }
    }

    /// Fold the positive/parent evidence of captured composerData `rows` into
    /// `ev`. Every row that decodes keeps its positive/parent evidence
    /// (round-8: one bad cell must not void the whole range read). A cell
    /// whose storage class FAD cannot interpret at all (INTEGER/REAL) marks
    /// its own composer unknown AND the whole evidence set uncertain — its
    /// payload might have listed any composer as a subagent (round-9 N6).
    fn fold_composer_row_evidence(
        rows: &[(String, ComposerDataCell)],
        ev: &mut ComposerSubagentEvidence,
        store: &Path,
    ) {
        for (key, cell) in rows {
            let Some(id) = key.strip_prefix("composerData:") else {
                continue;
            };
            match cell {
                ComposerDataCell::Null => {} // a NULL marker carries no children
                ComposerDataCell::Undecodable => {
                    Self::merge_flag(
                        &mut ev.own,
                        id.to_string(),
                        ComposerSubagentFlag {
                            flag_unknown: true,
                            ..ComposerSubagentFlag::default()
                        },
                    );
                    ev.uncertain_stores.insert(store.to_path_buf());
                }
                ComposerDataCell::Text(value) => {
                    Self::fold_composer_value(ev, id, value, store);
                }
            }
        }
    }

    /// Build the child->parent map from the composerData rows already read,
    /// plus the composerHeaders / ItemTable flags. `rows` is the full key-range
    /// result, so the map spans the whole store even on an incremental scan:
    /// `since_ts` filters which conversations are emitted, not which rows are read here.
    pub(super) fn composer_subagent_evidence(
        rows: &[(String, ComposerDataCell)],
        conn: &Connection,
        store: &Path,
        tick: &dyn Fn(),
    ) -> ComposerSubagentEvidence {
        let mut ev = ComposerSubagentEvidence::default();
        Self::fold_composer_row_evidence(rows, &mut ev, store);
        // Header records are deliberately read AFTER the composer rows
        // (round-8 N7/N8 ordering ruling — see the emit paths).
        ev.headers = Self::read_composer_headers(conn, tick);
        ev
    }

    /// The same evidence, reading the composerData key range itself: the
    /// targeted lookup path opens a store without a prior full read.
    /// `store` identifies this store for `uncertain_stores` — uncertainty is
    /// sticky per store for the whole scan (round-10 N6).
    pub(super) fn composer_subagent_evidence_from_conn(
        conn: &Connection,
        store: &Path,
        tick: &dyn Fn(),
    ) -> ComposerSubagentEvidence {
        let prefix = "composerData:";
        let limit = Self::prefix_upper_bound(prefix);
        // The raw SqliteValueRef capture keeps a single non-TEXT cell (e.g. a
        // BLOB under composerData) from voiding the whole range read —
        // every row that decodes keeps its positive/parent evidence.
        let rows = conn.query_map_collect(
            CURSOR_DISK_KV_RANGE_SQL,
            params![prefix, limit.as_str()],
            |row| {
                tick();
                let key: String = row.get_typed(0)?;
                Ok((key, Self::composer_data_cell(row, 1)))
            },
        );
        if let Ok(rows) = rows {
            Self::composer_subagent_evidence(&rows, conn, store, tick)
        } else {
            // The composerData read failed, but header evidence is an
            // independent source: read it instead of discarding it (O1).
            let headers = Self::read_composer_headers(conn, tick);
            ComposerSubagentEvidence {
                headers,
                ..ComposerSubagentEvidence::default()
            }
        }
    }

    /// Read `composerHeaders` (SQL columns `isSubagent`/`parentComposerId` plus
    /// `value` JSON) and legacy `ItemTable` `'composer.composerHeaders'`,
    /// probing tables/columns so older schemas stay readable. Missing or
    /// unreadable sources simply contribute no evidence (absent, not a verdict);
    /// an isSubagent component FAD cannot decode lands in `flag_unknown`.
    pub(super) fn read_composer_headers(
        conn: &Connection,
        tick: &dyn Fn(),
    ) -> HashMap<String, ComposerSubagentFlag> {
        let mut headers = HashMap::new();

        match Self::sqlite_table_exists(conn, "composerHeaders") {
            Err(()) => return HashMap::new(),
            Ok(false) => {}
            Ok(true) => {
                let Ok(cols) = conn.query_map_collect(
                    "PRAGMA table_info(composerHeaders)",
                    params![],
                    |row| row.get_typed::<String>(1),
                ) else {
                    return HashMap::new();
                };
                let has_id = cols.iter().any(|c| c == "composerId");
                let has_sub = cols.iter().any(|c| c == "isSubagent");
                let has_parent = cols.iter().any(|c| c == "parentComposerId");
                let has_value = cols.iter().any(|c| c == "value");
                let has_updated = cols.iter().any(|c| c == "lastUpdatedAt");

                if has_id && (has_sub || has_parent || has_value) {
                    let mut select_cols = vec!["composerId"];
                    let sub_idx = has_sub.then(|| {
                        let i = select_cols.len();
                        select_cols.push("isSubagent");
                        i
                    });
                    let parent_idx = has_parent.then(|| {
                        let i = select_cols.len();
                        select_cols.push("parentComposerId");
                        i
                    });
                    let value_idx = has_value.then(|| {
                        let i = select_cols.len();
                        select_cols.push("value");
                        i
                    });
                    let updated_idx = has_updated.then(|| {
                        let i = select_cols.len();
                        select_cols.push("lastUpdatedAt");
                        i
                    });
                    let sql = format!("SELECT {} FROM composerHeaders", select_cols.join(", "));
                    let rows = conn.query_map_collect(&sql, params![], |row| {
                        tick();
                        Ok(Self::read_composer_header_row(
                            row,
                            sub_idx,
                            parent_idx,
                            value_idx,
                            updated_idx,
                        ))
                    });
                    if let Ok(rows) = rows {
                        for hr in rows {
                            let mut flag = ComposerSubagentFlag {
                                is_subagent: hr.is_subagent,
                                parent_id: hr.parent_id.clone(),
                                last_updated_at: hr.last_updated_at,
                                flag_unknown: hr.sub_unknown,
                            };
                            // Identity recoverable from the value JSON, used to keep
                            // positive/parent evidence when the SQL identity is empty.
                            let mut val_id: Option<String> = None;
                            if let Some(raw) = hr.value.filter(|s| !s.trim().is_empty()) {
                                if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw)
                                {
                                    let val_sub =
                                        Self::header_bool_field(&obj, &mut flag.flag_unknown);
                                    let val_parent =
                                        Self::header_id_field(&obj, "parentComposerId");
                                    let val_parent_fallback =
                                        Self::header_id_field(&obj, "parentId");
                                    let json_composer_id =
                                        Self::header_id_field(&obj, "composerId");
                                    let json_id = Self::header_id_field(&obj, "id");
                                    val_id = json_composer_id.or(json_id);
                                    let val_updated =
                                        obj.get("lastUpdatedAt").and_then(parse_timestamp);
                                    // Same-row positive-dominates merge: a decoded
                                    // false/unknown in the value JSON must not erase
                                    // the SQL column's proven true (N2).
                                    flag.is_subagent = match (val_sub, hr.is_subagent) {
                                        (Some(true), _) | (_, Some(true)) => Some(true),
                                        (a, b) => a.or(b),
                                    };
                                    flag.parent_id =
                                        val_parent.or(hr.parent_id).or(val_parent_fallback);
                                    flag.last_updated_at = match (hr.last_updated_at, val_updated) {
                                        (Some(a), Some(b)) => Some(a.max(b)),
                                        (a, b) => a.or(b),
                                    };
                                }
                            }
                            let id = if hr.id.is_empty() {
                                val_id
                            } else {
                                Some(hr.id)
                            };
                            if let Some(id) = id {
                                Self::merge_flag(&mut headers, id, flag);
                            }
                        }
                    }
                }
            }
        }

        // Legacy fallback: ItemTable key 'composer.composerHeaders'.
        if Self::sqlite_table_exists(conn, "ItemTable") == Ok(true) {
            if let Ok(rows) = conn.query_map_collect(
                "SELECT value FROM ItemTable WHERE key = 'composer.composerHeaders' LIMIT 1",
                params![],
                |row| {
                    tick();
                    Self::text_or_bytes_column(row, 0)
                },
            ) {
                if let Some(Some(raw)) = rows.into_iter().next()
                    && !raw.trim().is_empty()
                {
                    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) {
                        for item in items {
                            let Some(obj) = item.as_object() else {
                                continue;
                            };
                            let cid = Self::header_id_field(obj, "composerId");
                            let iid = Self::header_id_field(obj, "id");
                            let Some(id) = cid.or(iid) else {
                                continue;
                            };
                            let mut flag = ComposerSubagentFlag {
                                is_subagent: None,
                                parent_id: Self::header_id_field(obj, "parentComposerId")
                                    .or_else(|| Self::header_id_field(obj, "parentId")),
                                last_updated_at: obj.get("lastUpdatedAt").and_then(parse_timestamp),
                                flag_unknown: false,
                            };
                            flag.is_subagent = Self::header_bool_field(obj, &mut flag.flag_unknown);
                            Self::merge_flag(&mut headers, id, flag);
                        }
                    }
                }
            }
        }

        headers
    }

    /// Aggregate subagent evidence across all discovered Cursor DBs so an
    /// incremental scan of a fresh `workspaceStorage` DB still sees parent/header
    /// evidence in a stale `globalStorage` DB.
    pub(super) fn collect_cross_db_evidence(
        db_paths: &[(ScanRoot, PathBuf)],
        tick: &dyn Fn(),
    ) -> ComposerSubagentEvidence {
        let mut merged = ComposerSubagentEvidence::default();
        for (_, db_path) in db_paths {
            if store_is_provably_empty(db_path) {
                continue;
            }
            tick();
            let Ok(conn) =
                open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref())
            else {
                continue;
            };
            if conn.execute("PRAGMA busy_timeout = 5000;").is_err() {
                continue;
            }
            let db_ev = Self::composer_subagent_evidence_from_conn(&conn, db_path, tick);
            Self::extend_subagent_evidence(&mut merged, &db_ev);
        }
        merged
    }

    /// Fold `extra` into `merged`, keeping the positive-dominance invariants of
    /// [`Self::merge_flag`]. Round-7 N6: the positive/parent evidence of the
    /// exact composer rows and headers being emitted is combined with any
    /// earlier (cached cross-store) evidence before resolving a verdict — a
    /// cached pass that ran before those rows were committed must not hide
    /// their parent links or true flags.
    pub(super) fn extend_subagent_evidence(
        merged: &mut ComposerSubagentEvidence,
        extra: &ComposerSubagentEvidence,
    ) {
        for (child, parent) in &extra.child_to_parent {
            merged.child_to_parent.insert(child.clone(), parent.clone());
        }
        for (id, flag) in &extra.own {
            Self::merge_flag(&mut merged.own, id.clone(), flag.clone());
        }
        for (id, flag) in &extra.headers {
            Self::merge_flag(&mut merged.headers, id.clone(), flag.clone());
        }
        // `uncertain_stores` unions by store identity (round-10 N6): the
        // uncertainty stays scoped to the store that captured the unreadable
        // row — an uninterpretable remote store cannot suppress another
        // store's own-header false — but it is sticky for the whole scan, so
        // a later clean reread of the same store cannot lift it.
        merged
            .uncertain_stores
            .extend(extra.uncertain_stores.iter().cloned());
    }

    /// Resolve one composer's verdict. Positive evidence (a parent link or an
    /// explicit true from any source) wins and yields true (+parent). Otherwise
    /// false is returned only when the composer's OWN header record, read after
    /// the composer's rows on this scan, cleanly decodes to an explicit false
    /// (own_header_false, round-6 ruling). Everything else is absent (None).
    ///
    /// Round-7 B1 ruling: a false verdict means "Cursor's own header for this
    /// composer explicitly says not-subagent, and no contradicting evidence was
    /// observed in this scan". A parent listing in a store this scan did not
    /// read — or committed between the scan's positive-evidence read and its
    /// own-header read (round-8 ordering rule) — can still exist: that is the
    /// accepted, documented limit of the rule, not something inventory or
    /// completeness gating could soundly fix.
    fn subagent_verdict(
        composer_id: &str,
        ev: &ComposerSubagentEvidence,
        own_header_false: bool,
    ) -> Option<(bool, Option<String>)> {
        if let Some(parent) = ev.child_to_parent.get(composer_id) {
            return Some((true, Some(parent.clone())));
        }
        let own = ev.own.get(composer_id);
        let header = ev.headers.get(composer_id);
        if let Some(parent) = own
            .and_then(|o| o.parent_id.as_ref())
            .or_else(|| header.and_then(|h| h.parent_id.as_ref()))
        {
            return Some((true, Some(parent.clone())));
        }
        if own.is_some_and(|o| o.is_subagent == Some(true))
            || header.is_some_and(|h| h.is_subagent == Some(true))
        {
            return Some((true, None));
        }
        if own_header_false {
            return Some((false, None));
        }
        None
    }

    /// False-eligibility for one emitted composer. `own_headers` must be the
    /// map read AFTER this store's composer rows (round-9 N9): a false that
    /// only an earlier read saw cannot be borrowed when the post-row header
    /// is absent or flagless. The late flag must decode cleanly to false and
    /// no observed component of this composer's own evidence may be
    /// uninterpretable. `store` is the EMITTING store's identity (round-10
    /// N6): if ANY read of it this scan captured a composerData row FAD could
    /// not interpret, that payload might have listed any composer, so it
    /// withholds every false certified from THAT store for the whole scan —
    /// a later clean reread cannot lift the earlier uncertainty — while
    /// another store's uncertainty cannot suppress this store's falses.
    pub(super) fn own_header_certifies_false(
        composer_id: &str,
        own_headers: &HashMap<String, ComposerSubagentFlag>,
        ev: &ComposerSubagentEvidence,
        store: &Path,
    ) -> bool {
        own_headers
            .get(composer_id)
            .is_some_and(|f| f.is_subagent == Some(false) && !f.flag_unknown)
            && !ev.uncertain_stores.contains(store)
            && !ev.own.get(composer_id).is_some_and(|f| f.flag_unknown)
    }

    /// Lift the verdict into a parsed composer conversation's metadata.
    pub(super) fn apply_subagent_verdict(
        conv: &mut NormalizedConversation,
        composer_id: &str,
        ev: &ComposerSubagentEvidence,
        own_header_false: bool,
    ) {
        let Some((is_subagent, parent)) = Self::subagent_verdict(composer_id, ev, own_header_false)
        else {
            return;
        };
        if !conv.metadata.is_object() {
            conv.metadata = Value::Object(Map::new());
        }
        if let Some(obj) = conv.metadata.as_object_mut() {
            obj.insert("is_subagent".to_string(), Value::Bool(is_subagent));
            if let Some(parent) = parent {
                obj.insert("parent_id".to_string(), Value::String(parent));
            }
        }
    }
}

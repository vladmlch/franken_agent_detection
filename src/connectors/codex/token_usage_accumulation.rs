//! Local fix (cass review 2026-11-02, F6). Modern rollouts emit one
//! `token_count` per model request with `info.last_token_usage` (per request)
//! and `info.total_token_usage` (cumulative). Tool loops produce several
//! requests without an assistant text message in between, and last-write-wins
//! kept only the final request. A repeated event (same cumulative total) is a
//! re-notification and must not be added again. Legacy flat events keep
//! last-write-wins.

use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TokenCountKind {
    Legacy,
    NewRequest,
    Repeat,
}

#[derive(Default)]
pub(super) struct TokenCountTracker {
    last_total: Option<i64>,
}

impl TokenCountTracker {
    pub(super) fn classify(&mut self, payload: &Value) -> TokenCountKind {
        let Some(total) = payload
            .pointer("/info/total_token_usage/total_tokens")
            .and_then(Value::as_i64)
        else {
            return TokenCountKind::Legacy;
        };
        if self.last_total == Some(total) {
            TokenCountKind::Repeat
        } else {
            self.last_total = Some(total);
            TokenCountKind::NewRequest
        }
    }
}

/// Field-wise sum of integer counters; non-integer fields take the incoming value.
pub(super) fn accumulate_token_usage(existing: Option<&Value>, incoming: Value) -> Value {
    let (Some(Value::Object(existing)), Value::Object(incoming_map)) = (existing, &incoming) else {
        return incoming;
    };
    let mut merged: Map<String, Value> = existing.clone();
    for (key, value) in incoming_map {
        let summed = match (merged.get(key).and_then(Value::as_i64), value.as_i64()) {
            (Some(a), Some(b)) => Value::from(a.saturating_add(b)),
            _ => value.clone(),
        };
        merged.insert(key.clone(), summed);
    }
    Value::Object(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_by_cumulative_total() {
        let mut tracker = TokenCountTracker::default();
        let ev = |total: i64| json!({"type":"token_count","info":{"total_token_usage":{"total_tokens": total}}});
        assert_eq!(tracker.classify(&ev(100)), TokenCountKind::NewRequest);
        assert_eq!(tracker.classify(&ev(100)), TokenCountKind::Repeat);
        assert_eq!(tracker.classify(&ev(180)), TokenCountKind::NewRequest);
        assert_eq!(
            tracker.classify(&json!({"type":"token_count","input_tokens":5})),
            TokenCountKind::Legacy
        );
    }

    #[test]
    fn sums_integer_counters_only() {
        let merged = accumulate_token_usage(
            Some(&json!({"input_tokens": 5, "output_tokens": 10, "model": "a"})),
            json!({"input_tokens": 7, "output_tokens": 14, "model": "b"}),
        );
        assert_eq!(
            merged,
            json!({"input_tokens": 12, "output_tokens": 24, "model": "b"})
        );
        assert_eq!(
            accumulate_token_usage(None, json!({"x": 1})),
            json!({"x": 1})
        );
    }
}

//! Local fix (cass review 2026-11-02, F1): Claude Code writes generated
//! titles as `{"type":"ai-title","aiTitle":"…"}`. Older/synthetic records
//! used `title`; keep that as a fallback.

use serde_json::Value;

pub(super) fn generated_title(raw: &Value) -> Option<String> {
    if raw.get("type").and_then(Value::as_str) != Some("ai-title") {
        return None;
    }
    ["aiTitle", "title"].into_iter().find_map(|key| {
        raw.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ai_title_wins_and_legacy_title_still_works() {
        assert_eq!(
            generated_title(&json!({"type":"ai-title","aiTitle":" A ","title":"B"})).as_deref(),
            Some("A")
        );
        assert_eq!(
            generated_title(&json!({"type":"ai-title","title":"B"})).as_deref(),
            Some("B")
        );
        assert_eq!(
            generated_title(&json!({"type":"ai-title","aiTitle":" ","title":"B"})).as_deref(),
            Some("B")
        );
        assert_eq!(generated_title(&json!({"type":"user","aiTitle":"A"})), None);
        assert_eq!(
            generated_title(&json!({"type":"ai-title","aiTitle":5})),
            None
        );
    }
}

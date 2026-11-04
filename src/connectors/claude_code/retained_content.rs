//! Local fix (cass review 2026-11-02, F2): Claude assistant records carry
//! `{"type":"thinking","thinking":"…"}` blocks that the shared projection
//! ignores; thinking-only records were dropped before normalization.
//! `redacted_thinking` stays out (ciphertext, not searchable text).

use serde_json::Value;

pub(super) fn flatten_claude_content(content: &Value) -> String {
    let Some(blocks) = content.as_array() else {
        return super::flatten_content(content);
    };
    blocks
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("thinking") {
                block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| format!("[Thinking]\n{text}"))
            } else {
                crate::connectors::utils::projection_content_part(block)
                    .filter(|text| !text.is_empty())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thinking_only_mixed_and_redacted() {
        assert_eq!(
            flatten_claude_content(&json!([{"type":"thinking","thinking":"check parent"}])),
            "[Thinking]\ncheck parent"
        );
        assert_eq!(
            flatten_claude_content(&json!([
                {"type":"thinking","thinking":"check parent"},
                {"type":"text","text":"answer"}
            ])),
            "[Thinking]\ncheck parent\nanswer"
        );
        assert_eq!(
            flatten_claude_content(&json!([{"type":"redacted_thinking","data":"enc"}])),
            ""
        );
        assert_eq!(flatten_claude_content(&json!("plain")), "plain");
    }

    #[test]
    fn non_thinking_blocks_match_the_shared_projection() {
        let content = json!([
            {"type":"text","text":"a"},
            {"type":"tool_use","name":"Read","input":{"description":"look"}},
            {"type":"tool_result","content":"ignored here"}
        ]);
        assert_eq!(
            flatten_claude_content(&content),
            crate::connectors::utils::flatten_content(&content)
        );
    }
}

//! Local fix (cass review 2026-11-02, F3).
//!
//! Real Antigravity transcripts put the model's tool calls on
//! `PLANNER_RESPONSE` records as `tool_calls: [{"name", "args"}]`, often
//! with no text or thinking. The following `GENERIC` record is that call's
//! result, not a tool named "generic". CASS keeps `extra` but not
//! `NormalizedMessage.invocations`, so the calls are also stored under
//! `extra.cass.invocations`.

use serde_json::{Value, json};

use crate::types::NormalizedInvocation;

/// MODEL step types that are results/prose, never a tool named after the type.
pub(super) const RESULT_STEP_TYPES: &[&str] = &["GENERIC"];

pub(super) fn explicit_planner_invocations(
    tool_calls: Option<&Value>,
) -> Vec<NormalizedInvocation> {
    let Some(items) = tool_calls.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|call| {
            let name = [
                call.get("name"),
                call.get("tool_name"),
                call.pointer("/function/name"),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .find(|name| !name.is_empty())?;
            let call_id = [
                call.get("id"),
                call.get("call_id"),
                call.get("tool_call_id"),
            ]
            .into_iter()
            .flatten()
            .find_map(Value::as_str)
            .map(str::to_owned);
            let arguments = [
                call.get("args"),
                call.get("arguments"),
                call.get("input"),
                call.pointer("/function/arguments"),
            ]
            .into_iter()
            .flatten()
            .next()
            .cloned();
            Some(NormalizedInvocation {
                kind: "tool".to_string(),
                name: name.to_string(),
                raw_name: None,
                call_id,
                arguments,
            })
        })
        .collect()
}

pub(super) fn tool_call_body(invocations: &[NormalizedInvocation]) -> String {
    invocations
        .iter()
        .map(|inv| format!("[Tool: {}]", inv.name))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn invocations_extra(invocations: &[NormalizedInvocation]) -> Value {
    json!({ "invocations": invocations })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_planner_shape_keeps_name_and_args() {
        let calls = json!([
            {"name": "view_file", "args": {"AbsolutePath": "/a.rs"}},
            {"args": {}},
            {"name": "  ", "args": {}}
        ]);
        let invocations = explicit_planner_invocations(Some(&calls));
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].name, "view_file");
        assert_eq!(
            invocations[0].arguments.as_ref().unwrap()["AbsolutePath"],
            "/a.rs"
        );
        assert_eq!(tool_call_body(&invocations), "[Tool: view_file]");
        assert_eq!(
            invocations_extra(&invocations)["invocations"][0]["name"],
            "view_file"
        );
    }

    #[test]
    fn absent_or_malformed_tool_calls_give_nothing() {
        assert!(explicit_planner_invocations(None).is_empty());
        assert!(explicit_planner_invocations(Some(&json!({"name": "x"}))).is_empty());
    }
}

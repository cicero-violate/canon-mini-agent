use crate::invalid_action::{auto_fill_message_fields, build_invalid_action_feedback};
use serde_json::json;

#[test]
fn allows_missing_observation_field() {
    let action = json!({
        "action": "run_command",
        "cmd": "echo hi",
        "rationale": "test rationale"
    });

    let result = build_invalid_action_feedback(Some(&action), "", "executor");

    assert!(
        !result.contains("missing field: observation"),
        "observation should be optional but was reported missing"
    );
}

#[test]
fn allows_empty_observation_field() {
    let action = json!({
        "action": "run_command",
        "cmd": "echo hi",
        "observation": "",
        "rationale": "test rationale"
    });

    let result = build_invalid_action_feedback(Some(&action), "", "executor");

    assert!(
        !result.contains("missing field: observation"),
        "empty observation should be allowed"
    );
}

#[test]
fn observation_type_still_validated_if_present() {
    let action = json!({
        "action": "run_command",
        "cmd": "echo hi",
        "observation": 123,
        "rationale": "test rationale"
    });

    let result = build_invalid_action_feedback(Some(&action), "", "executor");

    assert!(
        result.contains("field type mismatch: observation"),
        "observation type should still be validated"
    );
}

#[test]
fn planner_diagnostics_plan_requires_source_validation_evidence() {
    let action = json!({
        "action": "plan",
        "op": "set_plan_status",
        "status": "in_progress",
        "observation": "Diagnostics report says a stale violation is active.",
        "rationale": "Reprioritize work from diagnostics.",
        "predicted_next_actions": [
            {"action": "read_file", "intent": "Inspect the relevant source file."},
            {"action": "cargo_test", "intent": "Verify the guard after updating plan behavior."}
        ]
    });

    let result = build_invalid_action_feedback(Some(&action), "", "planner");

    assert!(
        result.contains("must cite current source validation"),
        "planner diagnostics-derived plan actions without source validation should be rejected"
    );
}

#[test]
fn planner_diagnostics_plan_allows_cited_source_validation_evidence() {
    let action = json!({
        "action": "plan",
        "op": "set_plan_status",
        "status": "in_progress",
        "observation": "Diagnostics claim rechecked with read_file on src/tools.rs and verified against current-cycle source evidence.",
        "rationale": "Use verified source evidence before acting on diagnostics.",
        "predicted_next_actions": [
            {"action": "cargo_test", "intent": "Verify the planner guard remains green."},
            {"action": "plan", "intent": "Continue with source-validated planning work."}
        ]
    });

    let result = build_invalid_action_feedback(Some(&action), "", "planner");

    assert!(
        !result.contains("must cite current source validation"),
        "planner diagnostics-derived plan actions with cited source validation should be allowed"
    );
}

#[test]
fn auto_fill_reroutes_diagnostics_self_addressed_message_to_planner() {
    let mut action = json!({
        "action": "message",
        "from": "diagnostics",
        "to": "diagnostics",
        "type": "blocker",
        "status": "blocked",
        "payload": {
            "summary": "transport still blocked",
            "blocker": "transport timeout",
            "evidence": "chromium timeout",
            "required_action": "repair routing"
        }
    });

    let changed = auto_fill_message_fields(&mut action, "diagnostics");

    assert!(changed, "self-routed diagnostics message should be corrected");
    assert_eq!(action.get("to").and_then(|v| v.as_str()), Some("planner"));
}

#[test]
fn auto_fill_preserves_allowed_solo_self_complete_message() {
    let mut action = json!({
        "action": "message",
        "from": "solo",
        "to": "solo",
        "type": "result",
        "status": "complete",
        "payload": {
            "summary": "done"
        }
    });

    let changed = auto_fill_message_fields(&mut action, "solo");

    assert!(changed, "solo completion should still receive missing field autofill");
    assert_eq!(action.get("to").and_then(|v| v.as_str()), Some("solo"));
}

use serde_json::json;
use crate::invalid_action::build_invalid_action_feedback;

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


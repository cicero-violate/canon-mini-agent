use serde_json::json;

use crate::invalid_action::{
    auto_fill_message_fields, build_invalid_action_feedback, default_message_route,
    expected_message_format,
};

#[test]
fn default_message_route_executor() {
    let (from, to, msg_type, status) = default_message_route("executor");
    assert_eq!(from, "executor");
    assert_eq!(to, "verifier");
    assert_eq!(msg_type, "handoff");
    assert_eq!(status, "complete");
}

#[test]
fn auto_fill_message_fields_populates_defaults_and_expected_format() {
    let mut action = json!({
        "action": "message"
    });
    let changed = auto_fill_message_fields(&mut action, "executor");
    assert!(changed);
    let obj = action.as_object().unwrap();
    assert_eq!(obj.get("from").and_then(|v| v.as_str()), Some("executor"));
    assert_eq!(obj.get("to").and_then(|v| v.as_str()), Some("verifier"));
    assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("handoff"));
    assert_eq!(obj.get("status").and_then(|v| v.as_str()), Some("complete"));
    let payload = obj.get("payload").and_then(|v| v.as_object()).unwrap();
    assert_eq!(
        payload.get("summary").and_then(|v| v.as_str()),
        Some("auto-filled message fields")
    );
    let expected = expected_message_format("executor", "verifier", "handoff", "complete");
    assert_eq!(
        payload.get("expected_format").and_then(|v| v.as_str()),
        Some(expected.as_str())
    );
    assert!(obj.get("observation").is_some());
    assert!(obj.get("rationale").is_some());
}

#[test]
fn build_invalid_action_feedback_includes_missing_fields() {
    let action = json!({
        "action": "run_command",
        "observation": "",
        "rationale": "",
        "cmd": ""
    });
    let feedback = build_invalid_action_feedback(Some(&action), "bad action", "executor");
    assert!(feedback.contains("missing field: observation"));
    assert!(feedback.contains("missing field: rationale"));
    assert!(feedback.contains("missing field: cmd"));
}

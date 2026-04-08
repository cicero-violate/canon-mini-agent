use serde_json::json;

use crate::invalid_action::{
    auto_fill_message_fields, build_invalid_action_feedback, default_message_route,
    expected_message_format,
};
use crate::tools::{patch_scope_error, patch_scope_error_with_mode};

#[test]
fn default_message_route_executor() {
    let (from, to, msg_type, status) = default_message_route("executor");
    assert_eq!(from, "executor");
    assert_eq!(to, "verifier");
    assert_eq!(msg_type, "handoff");
    assert_eq!(status, "complete");
}

#[test]
fn default_message_route_solo() {
    let (from, to, msg_type, status) = default_message_route("solo");
    assert_eq!(from, "solo");
    assert_eq!(to, "solo");
    assert_eq!(msg_type, "result");
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
fn auto_fill_message_fields_populates_blocker_payload_defaults_when_blocked() {
    let mut action = json!({
        "action": "message",
        "type": "blocker",
        "status": "blocked",
        "payload": {}
    });
    let changed = auto_fill_message_fields(&mut action, "executor");
    assert!(changed);
    let obj = action.as_object().unwrap();
    assert_eq!(obj.get("from").and_then(|v| v.as_str()), Some("executor"));
    assert_eq!(obj.get("to").and_then(|v| v.as_str()), Some("verifier"));
    assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("blocker"));
    assert_eq!(obj.get("status").and_then(|v| v.as_str()), Some("blocked"));
    let payload = obj.get("payload").and_then(|v| v.as_object()).unwrap();
    assert_eq!(
        payload.get("summary").and_then(|v| v.as_str()),
        Some("auto-filled message fields")
    );
    assert_eq!(
        payload.get("blocker").and_then(|v| v.as_str()),
        Some("auto-filled blocker details")
    );
    assert_eq!(
        payload.get("evidence").and_then(|v| v.as_str()),
        Some("auto-filled blocker evidence")
    );
    assert_eq!(
        payload.get("required_action").and_then(|v| v.as_str()),
        Some("auto-filled required action")
    );
    let expected = expected_message_format("executor", "verifier", "blocker", "blocked");
    assert_eq!(
        payload.get("expected_format").and_then(|v| v.as_str()),
        Some(expected.as_str())
    );
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

#[test]
fn build_invalid_action_feedback_requires_message_observation() {
    let action = json!({
        "action": "message",
        "from": "executor",
        "to": "planner",
        "type": "result",
        "status": "complete",
        "rationale": "handoff result",
        "payload": {
            "summary": "done"
        }
    });
    let feedback = build_invalid_action_feedback(Some(&action), "bad action", "executor");
    assert!(feedback.contains("missing field: observation"));
}

#[test]
fn scope_guard_executor_blocks_invariants_and_objectives_in_normal_mode() {
    let invariants_patch = "\
*** Begin Patch
*** Update File: INVARIANTS.json
@@
-{}
+{}
*** End Patch";
    let objectives_patch = "\
*** Begin Patch
*** Update File: PLANS/OBJECTIVES.json
@@
-{}
+{}
*** End Patch";
    assert!(patch_scope_error("executor", invariants_patch).is_some());
    assert!(patch_scope_error("executor", objectives_patch).is_some());
}

#[test]
fn scope_guard_verifier_allows_only_plan_and_violations() {
    let plan_patch = "\
*** Begin Patch
*** Update File: PLAN.json
@@
-{}
+{}
*** End Patch";
    let violations_patch = "\
*** Begin Patch
*** Update File: VIOLATIONS.json
@@
-{}
+{}
*** End Patch";
    let spec_patch = "\
*** Begin Patch
*** Update File: SPEC.md
@@
-old
+new
*** End Patch";
    assert!(patch_scope_error("verifier", plan_patch).is_none());
    assert!(patch_scope_error("verifier", violations_patch).is_none());
    assert!(patch_scope_error("verifier", spec_patch).is_some());
}

#[test]
fn scope_guard_planner_blocks_source_files() {
    let src_patch = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** End Patch";
    assert!(patch_scope_error("planner", src_patch).is_some());
}

#[test]
fn scope_guard_executor_self_mod_allows_spec_and_src_only() {
    let spec_patch = "\
*** Begin Patch
*** Update File: SPEC.md
@@
-old
+new
*** End Patch";
    let src_patch = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** End Patch";
    let plan_patch = "\
*** Begin Patch
*** Update File: PLAN.json
@@
-{}
+{}
*** End Patch";
    assert!(patch_scope_error_with_mode("executor", spec_patch, true).is_none());
    assert!(patch_scope_error_with_mode("executor", src_patch, true).is_none());
    assert!(patch_scope_error_with_mode("executor", plan_patch, true).is_some());
}

#[test]
fn scope_guard_planner_blocks_source_files_in_self_mod_mode() {
    let src_patch = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** End Patch";
    let test_patch = "\
*** Begin Patch
*** Update File: tests/invalid_action_harness.rs
@@
-old
+new
*** End Patch";
    assert!(patch_scope_error_with_mode("planner", src_patch, true).is_some());
    assert!(patch_scope_error_with_mode("planner", test_patch, true).is_some());
}

#[test]
fn scope_guard_solo_allows_full_workspace_patch_surface() {
    let src_patch = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** End Patch";
    let plan_patch = "\
*** Begin Patch
*** Update File: PLAN.json
@@
-{}
+{}
*** End Patch";
    let violations_patch = "\
*** Begin Patch
*** Update File: VIOLATIONS.json
@@
-{}
+{}
*** End Patch";
    let diagnostics_patch = "\
*** Begin Patch
*** Update File: DIAGNOSTICS.json
@@
-{}
+{}
*** End Patch";
    assert!(patch_scope_error("solo", src_patch).is_none());
    assert!(patch_scope_error("solo", plan_patch).is_none());
    assert!(patch_scope_error("solo", violations_patch).is_none());
    assert!(patch_scope_error("solo", diagnostics_patch).is_none());
}

#[test]
fn scope_guard_executor_blocks_plan_and_diagnostics() {
    let plan_patch = "\
*** Begin Patch
*** Update File: PLAN.json
@@
-{}
+{}
*** End Patch";
    let diagnostics_patch = "\
*** Begin Patch
*** Update File: DIAGNOSTICS.json
@@
-{}
+{}
*** End Patch";
    assert!(patch_scope_error("executor", plan_patch).is_some());
    assert!(patch_scope_error("executor", diagnostics_patch).is_some());
}

#[test]
fn scope_guard_executor_blocks_source_files_in_normal_mode() {
    let src_patch = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** End Patch";
    let test_patch = "\
*** Begin Patch
*** Update File: tests/invalid_action_harness.rs
@@
-old
+new
*** End Patch";
    assert!(patch_scope_error_with_mode("executor", src_patch, false).is_some());
    assert!(patch_scope_error_with_mode("executor", test_patch, false).is_some());
}

#[test]
fn scope_guard_diagnostics_allows_only_diagnostics_files() {
    let diagnostics_patch = "\
*** Begin Patch
*** Update File: DIAGNOSTICS.json
@@
-{}
+{}
*** End Patch";
    let plan_patch = "\
*** Begin Patch
*** Update File: PLAN.json
@@
-{}
+{}
*** End Patch";
    let src_patch = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** End Patch";
    assert!(patch_scope_error("diagnostics", diagnostics_patch).is_none());
    assert!(patch_scope_error("diagnostics", plan_patch).is_some());
    assert!(patch_scope_error("diagnostics", src_patch).is_some());
}

#[test]
fn scope_guard_planner_allows_plan_and_lane_only() {
    let plan_patch = "\
*** Begin Patch
*** Update File: PLAN.json
@@
-{}
+{}
*** End Patch";
    let lane_patch = "\
*** Begin Patch
*** Update File: PLANS/executor-1.json
@@
-{}
+{}
*** End Patch";
    let violations_patch = "\
*** Begin Patch
*** Update File: VIOLATIONS.json
@@
-{}
+{}
*** End Patch";
    assert!(patch_scope_error("planner", plan_patch).is_none());
    assert!(patch_scope_error("planner", lane_patch).is_none());
    assert!(patch_scope_error("planner", violations_patch).is_some());
}

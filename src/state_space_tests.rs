use crate::state_space::{
    decide_bootstrap_phase, decide_resume_phase, extract_progress_path_from_result, CargoTestGate,
};

#[test]
fn extract_progress_path_detects_output_log() {
    let sample = "detached cargo test pid=123 output_log=/tmp/abc.log timeout_secs=1200";
    let path = extract_progress_path_from_result(sample).expect("missing output log");
    assert_eq!(path, "/tmp/abc.log");
}

#[test]
fn cargo_test_gate_requires_tail_then_clears() {
    let mut gate = CargoTestGate::new();
    gate.note_result("cargo_test", "note: cargo test detached\nprogress_path: /tmp/run.log");
    assert_eq!(gate.pending_tail_path(), Some("/tmp/run.log"));
    let msg = gate
        .message_blocker_if_needed("message", "/workspace/ai_sandbox/canon")
        .expect("expected blocker message");
    assert!(msg.contains("tail -n 200 /tmp/run.log"));
    gate.note_action("run_command", Some("tail -n 200 /tmp/run.log"));
    assert_eq!(gate.pending_tail_path(), None);
}

#[test]
fn resume_verifier_without_items_routes_planner() {
    let decision = decide_resume_phase("verifier", false, false, false);
    assert_eq!(decision.scheduled_phase, Some("planner".to_string()));
    assert!(decision.planner_pending);
}

#[test]
fn resume_planner_sets_pending() {
    let decision = decide_resume_phase("planner", true, false, false);
    assert_eq!(decision.scheduled_phase, Some("planner".to_string()));
    assert!(decision.planner_pending);
}

#[test]
fn resume_diagnostics_sets_pending() {
    let decision = decide_resume_phase("diagnostics", true, false, false);
    assert_eq!(decision.scheduled_phase, Some("diagnostics".to_string()));
    assert!(decision.diagnostics_pending);
}

#[test]
fn bootstrap_phase_from_start_role() {
    assert_eq!(decide_bootstrap_phase("planner"), Some("planner".to_string()));
    assert_eq!(
        decide_bootstrap_phase("diagnostics"),
        Some("diagnostics".to_string())
    );
    assert_eq!(decide_bootstrap_phase("executor"), Some("executor".to_string()));
    assert_eq!(decide_bootstrap_phase("unknown"), None);
}

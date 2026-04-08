use crate::state_space::{
    decide_bootstrap_phase, decide_resume_phase, extract_progress_path_from_result, CargoTestGate,
};
use crate::state_space::{
    decide_wake_flags, executor_step_limit_exceeded, scheduled_phase_resume_done, WakeFlagInput,
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
        .message_blocker_if_needed("message", "/workspace")
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
fn resume_solo_preserves_phase() {
    let decision = decide_resume_phase("solo", false, false, false);
    assert_eq!(decision.scheduled_phase, Some("solo".to_string()));
    assert!(!decision.planner_pending);
    assert!(!decision.diagnostics_pending);
}

#[test]
fn bootstrap_phase_from_start_role() {
    assert_eq!(decide_bootstrap_phase("planner"), Some("planner".to_string()));
    assert_eq!(
        decide_bootstrap_phase("diagnostics"),
        Some("diagnostics".to_string())
    );
    assert_eq!(decide_bootstrap_phase("executor"), Some("executor".to_string()));
    assert_eq!(decide_bootstrap_phase("solo"), Some("solo".to_string()));
    assert_eq!(decide_bootstrap_phase("unknown"), None);
}

#[test]
fn wake_flags_selects_newest_non_blocked() {
    let flags = vec![
        WakeFlagInput { role: "planner", modified_ms: 10 },
        WakeFlagInput { role: "executor", modified_ms: 20 },
    ];
    let decision = decide_wake_flags(false, &flags);
    assert_eq!(decision.scheduled_phase, Some("executor".to_string()));
    assert!(decision.executor_wake);
}

#[test]
fn wake_flags_blocks_planner_when_active_blocker() {
    let flags = vec![
        WakeFlagInput { role: "planner", modified_ms: 30 },
        WakeFlagInput { role: "executor", modified_ms: 20 },
    ];
    let decision = decide_wake_flags(true, &flags);
    assert_eq!(decision.scheduled_phase, Some("executor".to_string()));
    assert!(decision.executor_wake);
}

#[test]
fn scheduled_phase_resume_done_all_cases() {
    assert!(scheduled_phase_resume_done("planner", false, false, 0, true, false, false));
    assert!(scheduled_phase_resume_done("verifier", false, false, 0, true, false, false));
    assert!(scheduled_phase_resume_done("diagnostics", false, false, 0, true, false, false));
    assert!(scheduled_phase_resume_done("executor", false, false, 0, true, false, false));
    assert!(scheduled_phase_resume_done("solo", false, false, 0, true, false, false));

    assert!(!scheduled_phase_resume_done("planner", true, false, 0, true, false, false));
    assert!(!scheduled_phase_resume_done("verifier", false, false, 1, false, false, false));
    assert!(!scheduled_phase_resume_done("diagnostics", false, true, 0, true, false, false));
    assert!(!scheduled_phase_resume_done("executor", false, false, 0, true, true, true));
}

#[test]
fn executor_step_limit_boundary() {
    assert!(!executor_step_limit_exceeded(9, 10));
    assert!(executor_step_limit_exceeded(10, 10));
    assert!(executor_step_limit_exceeded(11, 10));
}

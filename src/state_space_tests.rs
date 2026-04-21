use crate::state_space::{
    allow_diagnostics_run, allow_named_phase_run, check_completion_endpoint, check_completion_tab,
    decide_active_blocker, decide_phase_gates, decide_post_diagnostics, decide_wake_signals,
    executor_step_limit_exceeded, is_verifier_specific_blocker,
    scheduled_phase_resume_done, should_force_blocker, verifier_blocker_phase_override,
    ActiveBlockerDecision, CompletionEndpointCheck, CompletionTabCheck, PhaseGates,
    SemanticControlState, WakeSignalInput,
};
use crate::state_space::{
    decide_bootstrap_phase, decide_resume_phase, extract_progress_path_from_result, CargoTestGate,
};
use crate::system_state::SystemState;

#[test]
fn extract_progress_path_detects_output_log() {
    let sample = "output_log: /tmp/abc.log\nsummary: test result: ok. 1 passed; 0 failed";
    let path = extract_progress_path_from_result(sample).expect("missing output log");
    assert_eq!(path, "/tmp/abc.log");
}

#[test]
fn cargo_test_gate_does_not_block_messages() {
    let mut gate = CargoTestGate::new();
    gate.note_result(
        "cargo_test",
        "output_log: /tmp/run.log\nsummary: (no test result yet)",
    );
    assert!(gate
        .message_blocker_if_needed("message", "/workspace")
        .is_none());
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
fn resume_verifier_with_items_preserves_verifier_phase() {
    let decision = decide_resume_phase("verifier", true, false, false);
    assert_eq!(decision.scheduled_phase, Some("verifier".to_string()));
    assert!(!decision.planner_pending);
    assert!(!decision.diagnostics_pending);
}

#[test]
fn resume_unknown_phase_is_passthrough() {
    let decision = decide_resume_phase("executor", false, true, true);
    assert_eq!(decision.scheduled_phase, Some("executor".to_string()));
    assert!(decision.planner_pending);
    assert!(decision.diagnostics_pending);
}

#[test]
fn resume_phase_covers_all_documented_branches() {
    let planner = decide_resume_phase("planner", true, false, false);
    assert_eq!(planner.scheduled_phase, Some("planner".to_string()));
    assert!(planner.planner_pending);
    assert!(!planner.diagnostics_pending);

    let diagnostics = decide_resume_phase("diagnostics", true, false, false);
    assert_eq!(diagnostics.scheduled_phase, Some("diagnostics".to_string()));
    assert!(!diagnostics.planner_pending);
    assert!(diagnostics.diagnostics_pending);

    let verifier_without_items = decide_resume_phase("verifier", false, false, false);
    assert_eq!(
        verifier_without_items.scheduled_phase,
        Some("planner".to_string())
    );
    assert!(verifier_without_items.planner_pending);
    assert!(!verifier_without_items.diagnostics_pending);

    let verifier_with_items = decide_resume_phase("verifier", true, false, false);
    assert_eq!(
        verifier_with_items.scheduled_phase,
        Some("verifier".to_string())
    );
    assert!(!verifier_with_items.planner_pending);
    assert!(!verifier_with_items.diagnostics_pending);

    let executor = decide_resume_phase("executor", false, true, true);
    assert_eq!(executor.scheduled_phase, Some("executor".to_string()));
    assert!(executor.planner_pending);
    assert!(executor.diagnostics_pending);

    let solo = decide_resume_phase("solo", false, false, false);
    assert_eq!(solo.scheduled_phase, Some("solo".to_string()));
    assert!(!solo.planner_pending);
    assert!(!solo.diagnostics_pending);
}

#[test]
fn bootstrap_phase_from_start_role() {
    assert_eq!(
        decide_bootstrap_phase("planner"),
        Some("planner".to_string())
    );
    assert_eq!(
        decide_bootstrap_phase("diagnostics"),
        Some("diagnostics".to_string())
    );
    assert_eq!(
        decide_bootstrap_phase("executor"),
        Some("executor".to_string())
    );
    assert_eq!(decide_bootstrap_phase("solo"), Some("solo".to_string()));
    assert_eq!(decide_bootstrap_phase("unknown"), None);
}

#[test]
fn wake_signals_selects_newest_non_blocked() {
    let flags = vec![
        WakeSignalInput {
            role: "planner",
            modified_ms: 10,
        },
        WakeSignalInput {
            role: "executor",
            modified_ms: 20,
        },
    ];
    let decision = decide_wake_signals(false, &flags);
    assert_eq!(decision.scheduled_phase, Some("executor".to_string()));
    assert!(decision.executor_wake);
}

#[test]
fn wake_signals_blocks_planner_when_active_blocker() {
    let flags = vec![
        WakeSignalInput {
            role: "planner",
            modified_ms: 30,
        },
        WakeSignalInput {
            role: "executor",
            modified_ms: 20,
        },
    ];
    let decision = decide_wake_signals(true, &flags);
    assert_eq!(decision.scheduled_phase, Some("executor".to_string()));
    assert!(decision.executor_wake);
}

#[test]
fn wake_signals_returns_none_when_no_flags_exist() {
    let decision = decide_wake_signals(false, &[]);
    assert_eq!(decision.scheduled_phase, None);
    assert!(!decision.planner_pending);
    assert!(!decision.diagnostics_pending);
    assert!(!decision.executor_wake);
}

#[test]
fn wake_signals_sets_planner_pending_when_planner_is_newest() {
    let flags = vec![
        WakeSignalInput {
            role: "executor",
            modified_ms: 10,
        },
        WakeSignalInput {
            role: "planner",
            modified_ms: 20,
        },
    ];
    let decision = decide_wake_signals(false, &flags);
    assert_eq!(decision.scheduled_phase, Some("planner".to_string()));
    assert!(decision.planner_pending);
    assert!(!decision.diagnostics_pending);
    assert!(!decision.executor_wake);
}

#[test]
fn wake_signals_sets_diagnostics_pending_when_diagnostics_is_newest() {
    let flags = vec![
        WakeSignalInput {
            role: "planner",
            modified_ms: 10,
        },
        WakeSignalInput {
            role: "diagnostics",
            modified_ms: 30,
        },
    ];
    let decision = decide_wake_signals(false, &flags);
    assert_eq!(decision.scheduled_phase, Some("diagnostics".to_string()));
    assert!(!decision.planner_pending);
    assert!(decision.diagnostics_pending);
    assert!(!decision.executor_wake);
}

#[test]
fn wake_signals_ignores_blocked_planner_and_keeps_next_newest_role() {
    let flags = vec![
        WakeSignalInput {
            role: "planner",
            modified_ms: 50,
        },
        WakeSignalInput {
            role: "diagnostics",
            modified_ms: 40,
        },
        WakeSignalInput {
            role: "executor",
            modified_ms: 30,
        },
    ];
    let decision = decide_wake_signals(true, &flags);
    assert_eq!(decision.scheduled_phase, Some("diagnostics".to_string()));
    assert!(!decision.planner_pending);
    assert!(decision.diagnostics_pending);
    assert!(!decision.executor_wake);
}

#[test]
fn wake_signals_covers_blocker_filtering_and_newest_role_selection() {
    let blocked_to_none = vec![WakeSignalInput {
        role: "planner",
        modified_ms: 50,
    }];
    let blocked_decision = decide_wake_signals(true, &blocked_to_none);
    assert_eq!(blocked_decision.scheduled_phase, None);
    assert!(!blocked_decision.planner_pending);
    assert!(!blocked_decision.diagnostics_pending);
    assert!(!blocked_decision.executor_wake);

    let planner_newest = vec![
        WakeSignalInput {
            role: "executor",
            modified_ms: 10,
        },
        WakeSignalInput {
            role: "planner",
            modified_ms: 20,
        },
    ];
    let planner_decision = decide_wake_signals(false, &planner_newest);
    assert_eq!(
        planner_decision.scheduled_phase,
        Some("planner".to_string())
    );
    assert!(planner_decision.planner_pending);
    assert!(!planner_decision.diagnostics_pending);
    assert!(!planner_decision.executor_wake);

    let diagnostics_newest = vec![
        WakeSignalInput {
            role: "planner",
            modified_ms: 10,
        },
        WakeSignalInput {
            role: "diagnostics",
            modified_ms: 30,
        },
    ];
    let diagnostics_decision = decide_wake_signals(false, &diagnostics_newest);
    assert_eq!(
        diagnostics_decision.scheduled_phase,
        Some("diagnostics".to_string())
    );
    assert!(!diagnostics_decision.planner_pending);
    assert!(diagnostics_decision.diagnostics_pending);
    assert!(!diagnostics_decision.executor_wake);

    let executor_newest = vec![
        WakeSignalInput {
            role: "planner",
            modified_ms: 10,
        },
        WakeSignalInput {
            role: "executor",
            modified_ms: 20,
        },
    ];
    let executor_decision = decide_wake_signals(false, &executor_newest);
    assert_eq!(
        executor_decision.scheduled_phase,
        Some("executor".to_string())
    );
    assert!(!executor_decision.planner_pending);
    assert!(!executor_decision.diagnostics_pending);
    assert!(executor_decision.executor_wake);
}

#[test]
fn scheduled_phase_resume_done_all_cases() {
    assert!(scheduled_phase_resume_done(
        "planner", false, false, 0, true, false, false
    ));
    assert!(scheduled_phase_resume_done(
        "verifier", false, false, 0, true, false, false
    ));
    assert!(scheduled_phase_resume_done(
        "diagnostics",
        false,
        false,
        0,
        true,
        false,
        false
    ));
    assert!(scheduled_phase_resume_done(
        "executor", false, false, 0, true, false, false
    ));
    assert!(scheduled_phase_resume_done(
        "solo", false, false, 0, true, false, false
    ));

    assert!(!scheduled_phase_resume_done(
        "planner", true, false, 0, true, false, false
    ));
    assert!(!scheduled_phase_resume_done(
        "verifier", false, false, 1, false, false, false
    ));
    assert!(!scheduled_phase_resume_done(
        "diagnostics",
        false,
        true,
        0,
        true,
        false,
        false
    ));
    assert!(!scheduled_phase_resume_done(
        "executor", false, false, 0, true, true, true
    ));
}

#[test]
fn executor_step_limit_boundary() {
    assert!(!executor_step_limit_exceeded(0, 10));
    assert!(executor_step_limit_exceeded(0, 0));
    assert!(!executor_step_limit_exceeded(9, 10));
    assert!(executor_step_limit_exceeded(10, 10));
    assert!(executor_step_limit_exceeded(11, 10));
}

#[test]
fn executor_submit_timeout_boundary() {
    let timed_out = |started_ms: u64, now_ms: u64, timeout_ms: u64| {
        now_ms.saturating_sub(started_ms) >= timeout_ms
    };

    assert!(!timed_out(100, 149, 50));
    assert!(timed_out(100, 150, 50));
    assert!(timed_out(100, 151, 50));
}

#[test]
fn completion_endpoint_reports_mismatch_only_for_wrong_endpoint() {
    assert_eq!(
        check_completion_endpoint("planner", Some("executor")),
        CompletionEndpointCheck::Mismatch
    );
    assert_eq!(
        check_completion_endpoint("planner", Some("planner")),
        CompletionEndpointCheck::Ok
    );
    assert_eq!(
        check_completion_endpoint("planner", None),
        CompletionEndpointCheck::Ok
    );
}

#[test]
fn completion_tab_reports_none_match_and_mismatch() {
    assert_eq!(check_completion_tab(None, 7), CompletionTabCheck::NoneSet);
    assert_eq!(check_completion_tab(Some(7), 7), CompletionTabCheck::Ok);
    assert_eq!(
        check_completion_tab(Some(8), 7),
        CompletionTabCheck::Mismatch
    );
}

#[test]
fn active_blocker_clears_planner_ownership_when_needed() {
    assert_eq!(
        decide_active_blocker(true, true, Some("planner")),
        ActiveBlockerDecision {
            planner_pending: false,
            scheduled_phase: None,
        }
    );
    assert_eq!(
        decide_active_blocker(true, false, Some("diagnostics")),
        ActiveBlockerDecision {
            planner_pending: false,
            scheduled_phase: Some("diagnostics".to_string()),
        }
    );
    assert_eq!(
        decide_active_blocker(false, true, Some("planner")),
        ActiveBlockerDecision {
            planner_pending: true,
            scheduled_phase: Some("planner".to_string()),
        }
    );
}

#[test]
fn planner_verifier_and_diagnostics_gate_helpers_follow_schedule() {
    assert!(allow_named_phase_run(None, "planner"));
    assert!(allow_named_phase_run(Some("planner"), "planner"));
    assert!(!allow_named_phase_run(Some("verifier"), "planner"));

    assert!(allow_named_phase_run(None, "verifier"));
    assert!(allow_named_phase_run(Some("verifier"), "verifier"));
    assert!(!allow_named_phase_run(Some("planner"), "verifier"));

    assert!(allow_diagnostics_run(None, false));
    assert!(allow_diagnostics_run(Some("diagnostics"), false));
    assert!(!allow_diagnostics_run(Some("planner"), false));
    assert!(!allow_diagnostics_run(Some("diagnostics"), true));
}

#[test]
fn decide_phase_gates_combines_pending_and_schedule_rules() {
    assert_eq!(
        decide_phase_gates(true, true, true, false, None),
        PhaseGates {
            planner: true,
            executor: true,
            verifier: true,
            diagnostics: true,
            solo: false,
        }
    );
    assert_eq!(
        decide_phase_gates(true, true, true, true, Some("planner")),
        PhaseGates {
            planner: true,
            executor: false,
            verifier: false,
            diagnostics: false,
            solo: false,
        }
    );
    assert_eq!(
        decide_phase_gates(false, false, false, false, Some("solo")),
        PhaseGates {
            planner: false,
            executor: true,
            verifier: false,
            diagnostics: false,
            solo: true,
        }
    );
}

#[test]
fn semantic_control_state_projects_phase_gates_from_system_state() {
    let mut state = SystemState::new(&[0], 1);
    state.scheduled_phase = Some("planner".to_string());
    state.planner_pending = true;
    state.diagnostics_pending = true;
    let semantic = SemanticControlState {
        scheduled_phase: state.scheduled_phase.clone(),
        planner_pending: state.planner_pending,
        diagnostics_pending: state.diagnostics_pending,
        verifier_queued: true,
        verifier_in_flight: true,
        active_blocker_to_verifier: state.active_blocker_to_verifier,
    };
    assert_eq!(
        semantic.phase_gates(),
        PhaseGates {
            planner: true,
            executor: false,
            verifier: false,
            diagnostics: false,
            solo: false,
        }
    );
}

#[test]
fn semantic_control_state_projects_blocker_suppression_and_resume_done() {
    let mut state = SystemState::new(&[0], 1);
    state.scheduled_phase = Some("planner".to_string());
    state.planner_pending = true;
    state.active_blocker_to_verifier = true;
    let semantic = SemanticControlState {
        scheduled_phase: state.scheduled_phase.clone(),
        planner_pending: state.planner_pending,
        diagnostics_pending: state.diagnostics_pending,
        verifier_queued: false,
        verifier_in_flight: false,
        active_blocker_to_verifier: state.active_blocker_to_verifier,
    };
    assert_eq!(
        semantic.active_blocker_decision(),
        ActiveBlockerDecision {
            planner_pending: false,
            scheduled_phase: None,
        }
    );

    let mut diagnostics_state = SystemState::new(&[0], 1);
    diagnostics_state.scheduled_phase = Some("diagnostics".to_string());
    diagnostics_state.diagnostics_pending = false;
    let semantic = SemanticControlState {
        scheduled_phase: diagnostics_state.scheduled_phase.clone(),
        planner_pending: diagnostics_state.planner_pending,
        diagnostics_pending: diagnostics_state.diagnostics_pending,
        verifier_queued: false,
        verifier_in_flight: false,
        active_blocker_to_verifier: diagnostics_state.active_blocker_to_verifier,
    };
    assert!(semantic.scheduled_phase_done(false, false));
}

#[test]
fn semantic_control_state_projects_wake_signals_and_resume_hydration() {
    let state = SystemState::new(&[0], 1);
    let mut state = state;
    state.active_blocker_to_verifier = true;
    let semantic = SemanticControlState {
        scheduled_phase: state.scheduled_phase.clone(),
        planner_pending: state.planner_pending,
        diagnostics_pending: state.diagnostics_pending,
        verifier_queued: false,
        verifier_in_flight: false,
        active_blocker_to_verifier: state.active_blocker_to_verifier,
    };
    let wake = decide_wake_signals(
        semantic.active_blocker_to_verifier,
        &[
            WakeSignalInput {
                role: "planner",
                modified_ms: 30,
            },
            WakeSignalInput {
                role: "diagnostics",
                modified_ms: 20,
            },
        ],
    );
    assert_eq!(wake.scheduled_phase.as_deref(), Some("diagnostics"));
    assert!(!wake.planner_pending);
    assert!(wake.diagnostics_pending);

    let resumed = semantic.with_resumed_checkpoint_phase("verifier", false);
    assert_eq!(resumed.scheduled_phase.as_deref(), Some("planner"));
    assert!(resumed.planner_pending);
    assert!(!resumed.diagnostics_pending);
}

#[test]
fn blocker_threshold_and_verifier_specific_detection_are_stable() {
    assert!(!should_force_blocker(2));
    assert!(should_force_blocker(3));
    assert!(is_verifier_specific_blocker(
        "Verifier failed schema check",
        "verifier must retry"
    ));
    assert!(!is_verifier_specific_blocker(
        "planner blocked",
        "rewrite plan"
    ));
}

#[test]
fn verifier_blocker_override_routes_non_verifier_blockers_to_planner() {
    assert_eq!(verifier_blocker_phase_override(true), None);
    assert_eq!(verifier_blocker_phase_override(false), Some("planner"));
}

#[test]
fn post_diagnostics_retriggers_planner_when_inputs_changed() {
    assert!(!decide_post_diagnostics(false, false));
    assert!(decide_post_diagnostics(true, false));
    assert!(decide_post_diagnostics(false, true));
}

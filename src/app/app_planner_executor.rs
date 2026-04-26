use super::*;

pub(super) async fn run_planner_phase(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    planner_bootstrapped: &mut bool,
    cargo_test_failures: &str,
) -> bool {
    let semantic_control_snapshot_hash_path = ctx.workspace.join("agent_state/.snapshot_hash");
    {
        let mut state = writer.state().as_kv_map();
        let blockers = crate::blockers::load_blockers(ctx.workspace);
        let now_ms = crate::logging::now_ms();
        let planner_blocker_escalated_count = crate::blockers::count_class_recent(
            &blockers,
            "planner",
            &crate::error_class::ErrorClass::BlockerEscalated,
            now_ms,
            5 * 60 * 1000,
        );
        // Only inject invariant trigger when entering escalation, not while already blocked.
        if planner_blocker_escalated_count >= 3 && !writer.state().planner_pending {
            state.insert("actor_kind".to_string(), "planner".to_string());
            state.insert("error_class".to_string(), "blocker_escalated".to_string());
        }
        if let Err(reason) =
            crate::invariants::evaluate_invariant_gate("planner", &state, ctx.workspace)
        {
            eprintln!("[invariant_gate] planner G_p (BLOCKED): {reason}");
            crate::blockers::record_action_failure_with_writer(
                ctx.workspace,
                Some(writer),
                "orchestrator",
                "planner_dispatch",
                &reason,
                None,
            );
            let record = serde_json::json!({
                "kind": "invariant_gate",
                "phase": "planner",
                "gate": "G_p",
                "proposed_role": "planner",
                "blocked": true,
                "reason": reason,
                "ts_ms": crate::logging::now_ms(),
            });
            let _ = crate::logging::append_action_log_record(&record);
            writer.record_violation("planner", &reason);
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            return false;
        }
    }

    let mut last_executor_diff = writer.state().last_executor_diff.clone();
    let inputs: PlannerInputs = load_planner_inputs(
        ctx.lanes,
        ctx.workspace,
        &writer.state().verifier_summary,
        &writer.state().last_plan_text.clone(),
        &mut last_executor_diff,
        cargo_test_failures.to_string(),
        ctx.master_plan_path,
        &semantic_control_snapshot_hash_path,
    );
    writer.apply(ControlEvent::LastExecutorDiffSet {
        text: last_executor_diff,
    });
    let restart_resume = peek_post_restart_result("planner");
    // Discard a stale restart-resume when the plan has no ready tasks.
    // The resume banner only covers the previous turn's action; if the executor
    // already consumed those tasks, the planner needs the full cycle prompt so
    // it can add new tasks or close the objective. Using the short banner would
    // cause the planner to send a `message` handoff to an executor that has
    // nothing to do, re-entering the plan-gap/executor-lane mutual-block deadlock.
    let restart_resume = if restart_resume.is_some() {
        let ready = crate::prompt_inputs::read_ready_tasks(ctx.workspace, 1);
        if ready == "(no ready tasks)" {
            let _ = take_post_restart_result("planner");
            None
        } else {
            restart_resume
        }
    } else {
        restart_resume
    };
    let mut planner_prompt = if let Some(resume) = restart_resume.as_ref() {
        let prompt = restart_resume_banner("planner", resume);
        let _ = take_post_restart_result("planner");
        prompt
    } else {
        planner_cycle_prompt(
            &inputs.summary_text,
            &inputs.objectives_text,
            &inputs.lessons_text,
            &inputs.enforced_invariants_text,
            &inputs.semantic_control_text,
            &inputs.plan_diff_text,
            &inputs.executor_diff_text,
            &inputs.cargo_test_failures,
        )
    };
    inject_inbound_message(&mut planner_prompt, writer, "planner");
    trace_orchestrator_forwarded("orchestrator", "planner", "planner", None, None, None, None);
    let planner_system = system_instructions(AgentPromptKind::Planner);
    let send_system_prompt = if restart_resume.is_some() {
        false
    } else {
        !*planner_bootstrapped
    };
    let result = run_agent(
        "planner",
        "planner",
        &planner_system,
        planner_prompt,
        ctx.planner_ep,
        ctx.bridge,
        ctx.workspace,
        ctx.tabs_planner,
        Some(writer),
        None,
        false,
        false,
        send_system_prompt,
        0,
    )
    .await;
    *planner_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!(
                "[orchestrate] planner ok bytes={}",
                result.summary_text().len()
            );
            let lessons_text = crate::prompt_inputs::read_lessons_or_empty(ctx.workspace);
            append_orchestration_trace(
                "learning_loop_cycle_audit",
                json!({
                    "phase": "planner",
                    "lessons_present": !lessons_text.trim().is_empty(),
                    "lessons_bytes": lessons_text.len(),
                    "objectives_path": OBJECTIVES_FILE,
                    "plan_path": MASTER_PLAN_FILE,
                }),
            );
            if writer.state().last_plan_text != inputs.plan_text {
                writer.apply(ControlEvent::LastPlanTextSet {
                    text: inputs.plan_text,
                });
            }

            // Block executor dispatch only when the planner explicitly signals blocked.
            // All other completions (plan ok, read results, objectives results, ready handoff)
            // fall through to the ready_tasks_exist check below, which acts as the second gate.
            if !planner_completion_allows_executor_dispatch(&result) {
                writer.apply(ControlEvent::PlannerPendingSet { pending: true });
                apply_scheduled_phase_if_changed(writer, Some("planner"));
                return true;
            }

            // Semantic preflight: demote ready tasks that reference symbols not
            // found in the workspace graph.
            crate::plan_preflight::preflight_ready_tasks(ctx.workspace);

            let lane_ids: Vec<usize> = ctx.lanes.iter().map(|l| l.index).collect();
            let ready_tasks_exist =
                crate::prompt_inputs::read_ready_tasks(ctx.workspace, 1) != "(no ready tasks)";
            let mut executor_handoff_queued = false;
            for lane_id in lane_ids {
                let lane_plan_already_empty = writer
                    .state()
                    .lanes
                    .get(&lane_id)
                    .map(|lane| lane.plan_text.is_empty())
                    .unwrap_or(true);
                if !lane_plan_already_empty {
                    writer.apply(ControlEvent::LanePlanTextSet {
                        lane_id,
                        text: String::new(),
                    });
                }
                let in_progress = {
                    let s = writer.state();
                    let ls = s.lanes.get(&lane_id);
                    ls.map(|l| l.in_progress_by.is_some()).unwrap_or(false)
                };
                if !in_progress && ready_tasks_exist {
                    writer.apply(ControlEvent::LanePendingSet {
                        lane_id,
                        pending: true,
                    });
                    executor_handoff_queued = true;
                }
            }
            writer.apply(ControlEvent::PlannerPendingSet { pending: false });
            if executor_handoff_queued {
                apply_scheduled_phase_if_changed(writer, Some("executor"));
            }
            true
        }
        Err(err) => {
            eprintln!("[orchestrate] planner error: {err:#}");
            log_error_event(
                "planner",
                "orchestrate",
                None,
                &format!("planner error: {err:#}"),
                Some(json!({ "stage": "planner_cycle" })),
            );
            false
        }
    }
}

pub(super) fn requeue_lane_after_submit_recovery(
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
) {
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    apply_lane_pending_if_changed(writer, lane_id, true);
    rt.executor_submit_inflight.remove(&lane_id);
    rt.timed_out_executor_submits.remove(&lane_id);
    writer.apply(ControlEvent::LaneSubmitInFlightSet {
        lane_id,
        in_flight: false,
    });
}

pub(super) fn register_submitted_executor_turn(
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
    submitted_turn: SubmittedExecutorTurn,
) {
    writer.apply(ControlEvent::LaneSubmitInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::ExecutorTurnRegistered {
        tab_id,
        turn_id,
        lane_id,
        lane_label: submitted_turn.lane_label.clone(),
        actor: submitted_turn.actor.clone(),
        endpoint_id: submitted_turn.endpoint_id.clone(),
    });
    rt.submitted_turns.insert((tab_id, turn_id), submitted_turn);
}

pub(super) fn executor_submit_timeout_message(ctx: &OrchestratorContext<'_>, lane_id: usize, command_id: &str) -> String {
    format!(
        "pending submit timeout: lane={} command_id={}",
        ctx.lanes[lane_id].label, command_id
    )
}

pub(super) fn executor_submit_timeout_trace(ctx: &OrchestratorContext<'_>, lane_id: usize, command_id: &str) -> Value {
    json!({
        "lane_name": ctx.lanes[lane_id].label,
        "command_id": command_id,
    })
}

pub(super) fn executor_submit_timeout_details(ctx: &OrchestratorContext<'_>, lane_id: usize, command_id: &str) -> Value {
    json!({
        "stage": "executor_submit_timeout",
        "lane": ctx.lanes[lane_id].label,
        "command_id": command_id,
    })
}

pub(super) fn log_timed_out_executor_submit(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    pending: PendingSubmitState,
) {
    let timeout_message = executor_submit_timeout_message(ctx, lane_id, &pending.command_id);
    eprintln!(
        "[orchestrate] pending submit timeout: lane={} command_id={}",
        ctx.lanes[lane_id].label, pending.command_id
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &timeout_message,
        Some(executor_submit_timeout_details(
            ctx,
            lane_id,
            &pending.command_id,
        )),
    );
    append_orchestration_trace(
        "executor_submit_timeout",
        executor_submit_timeout_trace(ctx, lane_id, &pending.command_id),
    );
    crate::blockers::record_action_failure_with_writer(
        ctx.workspace.as_path(),
        None,
        "executor",
        "executor_submit_timeout",
        &format!(
            "executor submit timed out: lane={} command_id={}",
            ctx.lanes[lane_id].label, pending.command_id
        ),
        None,
    );
}

pub(super) fn recover_timed_out_executor_submit_lane(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
) {
    if let Some(pending) = rt.executor_submit_inflight.remove(&lane_id) {
        rt.timed_out_executor_submits.insert(lane_id, pending.clone());
        log_timed_out_executor_submit(ctx, lane_id, pending);
    }
    writer.apply(ControlEvent::LaneSubmitInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    apply_lane_pending_if_changed(writer, lane_id, true);
}

pub(super) fn sweep_timed_out_executor_submits(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    pending_submit_timeout_ms: u64,
) {
    if rt.executor_submit_inflight.is_empty() {
        return;
    }
    let timed_out: Vec<usize> = rt
        .executor_submit_inflight
        .iter()
        .filter_map(|(lane_id, pending)| {
            (now.saturating_sub(pending.started_ms) >= pending_submit_timeout_ms)
                .then_some(*lane_id)
        })
        .collect();
    for lane_id in timed_out {
        recover_timed_out_executor_submit_lane(ctx, writer, rt, lane_id);
    }
}

pub(super) fn timed_out_submitted_turns(
    _writer: &CanonicalWriter,
    rt: &RuntimeState,
    now: u64,
    submitted_turn_timeout_ms: u64,
) -> Vec<(u32, u64, usize)> {
    let mut timed_out = Vec::new();
    for (&(tab_id, turn_id), submitted) in rt.submitted_turns.iter() {
        if now.saturating_sub(submitted.started_ms) >= submitted_turn_timeout_ms {
            timed_out.push((tab_id, turn_id, submitted.lane));
        }
    }
    timed_out
}

pub(super) fn log_timed_out_submitted_turn(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
    command_id: &str,
) {
    eprintln!(
        "[orchestrate] submitted turn timeout: lane={} tab_id={} turn_id={} command_id={}",
        ctx.lanes[lane_id].label, tab_id, turn_id, command_id
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &format!(
            "submitted turn timed out waiting for completion: lane={} tab_id={} turn_id={} command_id={}",
            ctx.lanes[lane_id].label, tab_id, turn_id, command_id
        ),
        Some(json!({
            "stage": "executor_completion_timeout",
            "lane": ctx.lanes[lane_id].label,
            "tab_id": tab_id,
            "turn_id": turn_id,
            "command_id": command_id,
        })),
    );
    append_orchestration_trace(
        "executor_completion_timeout",
        json!({
            "lane_name": ctx.lanes[lane_id].label,
            "tab_id": tab_id,
            "turn_id": turn_id,
            "command_id": command_id,
        }),
    );
    let reason = submitted_turn_timeout_reason(ctx, lane_id, tab_id, turn_id, command_id);
    crate::blockers::record_action_failure_with_writer(
        ctx.workspace.as_path(),
        None,
        "executor",
        "executor_completion_timeout",
        &reason,
        None,
    );
}

pub(super) fn submitted_turn_timeout_reason(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
    command_id: &str,
) -> String {
    format!(
        "executor completion timed out: lane={} tab_id={} turn_id={} command_id={}",
        ctx.lanes[lane_id].label, tab_id, turn_id, command_id
    )
}

pub(super) fn recover_timed_out_submitted_turn(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    tab_id: u32,
    turn_id: u64,
    lane_id: usize,
) {
    let Some(submitted) = rt.submitted_turns.remove(&(tab_id, turn_id)) else {
        return;
    };
    let prompt_inflight_before = writer
        .state()
        .lane_prompt_in_flight
        .get(&lane_id)
        .copied()
        .unwrap_or(false);
    let lane_in_progress_before = writer
        .state()
        .lanes
        .get(&lane_id)
        .and_then(|lane| lane.in_progress_by.clone());
    let lane_pending_before = writer
        .state()
        .lanes
        .get(&lane_id)
        .map(|lane| lane.pending)
        .unwrap_or(false);
    let reason = submitted_turn_timeout_reason(ctx, lane_id, tab_id, turn_id, &submitted.command_id);
    log_timed_out_submitted_turn(ctx, lane_id, tab_id, turn_id, &submitted.command_id);
    let recovery_decision =
        submitted_turn_timeout_recovery_decision(ctx.workspace.as_path(), &reason);
    let retry_recovery = match recovery_decision.as_ref() {
        Some(decision) => {
            if let Some(attempt_count) = recovery_budget_exhaustion(ctx.workspace.as_path(), decision) {
                record_recovery_suppressed(writer, decision, attempt_count);
                false
            } else {
                record_recovery_triggered(writer, decision);
                true
            }
        }
        None => true,
    };
    writer.apply(ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id });
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    if retry_recovery {
        apply_lane_pending_if_changed(writer, lane_id, true);
    } else {
        apply_scheduled_phase_if_changed(writer, Some("planner"));
        if !writer.state().planner_pending {
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        }
    }
    if let Some(decision) = recovery_decision.as_ref().filter(|_| retry_recovery) {
        let prompt_inflight_after = writer
            .state()
            .lane_prompt_in_flight
            .get(&lane_id)
            .copied()
            .unwrap_or(false);
        let lane_idle_after = writer
            .state()
            .lanes
            .get(&lane_id)
            .map(|lane| lane.in_progress_by.is_none())
            .unwrap_or(false);
        let lane_in_progress_after = writer
            .state()
            .lanes
            .get(&lane_id)
            .and_then(|lane| lane.in_progress_by.clone());
        let lane_pending_after = writer
            .state()
            .lanes
            .get(&lane_id)
            .map(|lane| lane.pending)
            .unwrap_or(false);
        let state_delta_count = usize::from(prompt_inflight_before != prompt_inflight_after)
            + usize::from(lane_in_progress_before != lane_in_progress_after)
            + usize::from(lane_pending_before != lane_pending_after);
        let state_delta_seen = state_delta_count > 0;
        let final_state_repaired = !prompt_inflight_after && lane_idle_after && lane_pending_after;
        let success = state_delta_seen && final_state_repaired;
        record_recovery_outcome(
            writer,
            decision,
            success,
            if success { 0 } else { decision.support_count },
            state_delta_seen,
            state_delta_count,
        );
    }
}

pub(super) fn sweep_timed_out_submitted_turns(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    submitted_turn_timeout_ms: u64,
) {
    if rt.submitted_turns.is_empty() {
        return;
    }
    let timed_out = timed_out_submitted_turns(writer, rt, now, submitted_turn_timeout_ms);
    for (tab_id, turn_id, lane_id) in timed_out {
        recover_timed_out_submitted_turn(ctx, writer, rt, tab_id, turn_id, lane_id);
    }
}

/// Intent: validation_gate
/// Resource: route_gate_state
/// Inputs: &mut std::collections::HashMap<std::string::String, std::string::String>, usize, usize, &str, &str, &str
/// Outputs: ()
/// Effects: inserts route-gate state markers when count reaches threshold
/// Forbidden: mutation outside the provided state map
/// Invariants: actor_kind and error marker are inserted only when count >= threshold
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn apply_route_gate_signal(
    state: &mut std::collections::HashMap<String, String>,
    count: usize,
    threshold: usize,
    actor_kind: &str,
    error_key: &str,
    error_value: &str,
) {
    if count >= threshold {
        state.insert("actor_kind".to_string(), actor_kind.to_string());
        state.insert(error_key.to_string(), error_value.to_string());
    }
}

pub(super) fn apply_executor_route_gate_signal(
    state: &mut std::collections::HashMap<String, String>,
    blockers: &crate::blockers::BlockersFile,
    now_ms: u64,
    actor_kind: &str,
    error_class: &crate::error_class::ErrorClass,
    threshold: usize,
    error_key: &str,
    error_value: &str,
) {
    let count = crate::blockers::count_class_recent(
        blockers,
        actor_kind,
        error_class,
        now_ms,
        5 * 60 * 1000,
    );
    apply_route_gate_signal(state, count, threshold, actor_kind, error_key, error_value);
}

pub(super) fn apply_executor_route_gate_error_class_signals(
    state: &mut std::collections::HashMap<String, String>,
    blockers: &crate::blockers::BlockersFile,
    now_ms: u64,
) {
    apply_executor_route_gate_signal(state, blockers, now_ms, "solo", &crate::error_class::ErrorClass::InvalidSchema, 3, "error_class", "invalid_schema");
    apply_executor_route_gate_signal(state, blockers, now_ms, "solo", &crate::error_class::ErrorClass::VerificationFailed, 1, "error_class", "verification_failed");
    apply_executor_route_gate_signal(state, blockers, now_ms, "executor", &crate::error_class::ErrorClass::InvalidSchema, 3, "error_class", "invalid_schema");
    apply_executor_route_gate_signal(state, blockers, now_ms, "executor", &crate::error_class::ErrorClass::UnauthorizedPlanOp, 1, "error_class", "unauthorized_plan_op");
    // Transport timeouts are health signals, not semantic dispatch predicates.
    // Blocking executor route on historical llm_timeout support deadlocks the
    // system after transient endpoint failures; retry/backoff owns recovery.
    apply_executor_route_gate_signal(state, blockers, now_ms, "executor", &crate::error_class::ErrorClass::StepLimitExceeded, 1, "error", "step_limit_exceeded");
    apply_executor_route_gate_signal(state, blockers, now_ms, "executor", &crate::error_class::ErrorClass::VerificationFailed, 1, "error_class", "verification_failed");
}

pub(super) fn apply_executor_route_gate_role_signals(
    state: &mut std::collections::HashMap<String, String>,
    blockers: &crate::blockers::BlockersFile,
    now_ms: u64,
) {
    apply_executor_route_gate_signal(state, blockers, now_ms, "diagnostics", &crate::error_class::ErrorClass::BlockerEscalated, 3, "error_class", "blocker_escalated");
    apply_executor_route_gate_signal(state, blockers, now_ms, "diagnostics", &crate::error_class::ErrorClass::InvalidSchema, 3, "error_class", "invalid_schema");
    apply_executor_route_gate_signal(state, blockers, now_ms, "verifier", &crate::error_class::ErrorClass::VerificationFailed, 1, "actor_kind", "solo");
    apply_executor_route_gate_signal(state, blockers, now_ms, "verifier", &crate::error_class::ErrorClass::VerificationFailed, 1, "error_class", "verification_failed");
}

pub(super) fn executor_route_gate_state(
    ready_count: &str,
    blockers: &crate::blockers::BlockersFile,
    now_ms: u64,
) -> std::collections::HashMap<String, String> {
    let mut state = std::collections::HashMap::new();
    state.insert("ready_tasks".to_string(), ready_count.to_string());

    apply_executor_route_gate_error_class_signals(&mut state, &blockers, now_ms);
    let orchestrator_invalid_route_count = crate::blockers::count_class_recent(
        &blockers,
        "orchestrator",
        &crate::error_class::ErrorClass::InvalidRoute,
        now_ms,
        60 * 1000,
    );
    if orchestrator_invalid_route_count >= 3 {
        state.insert("actor_kind".to_string(), "orchestrator".to_string());
        state.insert("error_class".to_string(), "invalid_route".to_string());
    }
    apply_executor_route_gate_role_signals(&mut state, &blockers, now_ms);
    state
}

pub(super) fn evaluate_executor_route_gates(writer: &mut CanonicalWriter, ready_count: &str) -> bool {
    let ws = std::path::PathBuf::from(workspace());
    let blockers = crate::blockers::load_blockers(&ws);
    let now_ms = crate::logging::now_ms();
    let mut state = executor_route_gate_state(ready_count, &blockers, now_ms);

    if let Err(reason) = crate::invariants::evaluate_invariant_gate("route", &state, &ws) {
        apply_route_gate_block(writer, &ws, &reason);
        return false;
    }

    if !evaluate_executor_missing_target_gate(writer, &ws, &blockers, &state, now_ms) {
        return false;
    }

    apply_any_missing_target_route_signal(&mut state, &blockers, now_ms);

    apply_orchestrator_livelock_signal(&mut state, &blockers, now_ms);

    if let Err(reason) = crate::invariants::evaluate_invariant_gate("executor", &state, &ws) {
        apply_route_gate_block(writer, &ws, &reason);
        return false;
    }

    true
}

pub(super) fn apply_any_missing_target_route_signal(
    state: &mut std::collections::HashMap<String, String>,
    blockers: &crate::blockers::BlockersFile,
    now_ms: u64,
) {
    let missing_target_count = blockers
        .blockers
        .iter()
        .filter(|b| {
            now_ms.saturating_sub(b.ts_ms) <= 5 * 60 * 1000
                && matches!(b.error_class, crate::error_class::ErrorClass::MissingTarget)
        })
        .count();
    if missing_target_count >= 1 {
        state.insert("actor_kind".to_string(), "any".to_string());
        state.insert("error".to_string(), "missing_target".to_string());
    }
}

pub(super) fn evaluate_executor_missing_target_gate(
    writer: &mut CanonicalWriter,
    ws: &std::path::Path,
    blockers: &crate::blockers::BlockersFile,
    state: &std::collections::HashMap<String, String>,
    now_ms: u64,
) -> bool {
    let count = crate::blockers::count_class_recent(
        blockers,
        "executor",
        &crate::error_class::ErrorClass::MissingTarget,
        now_ms,
        5 * 60 * 1000,
    );
    if count < 1 {
        return true;
    }
    let mut executor_state = state.clone();
    executor_state.insert("actor_kind".to_string(), "executor".to_string());
    executor_state.insert("error".to_string(), "missing_target".to_string());
    if let Err(reason) = crate::invariants::evaluate_invariant_gate("executor", &executor_state, ws) {
        apply_route_gate_block(writer, ws, &reason);
        return false;
    }
    true
}

pub(super) fn apply_orchestrator_livelock_signal(
    state: &mut std::collections::HashMap<String, String>,
    blockers: &crate::blockers::BlockersFile,
    now_ms: u64,
) {
    let count = crate::blockers::count_class_recent(
        blockers,
        "orchestrator",
        &crate::error_class::ErrorClass::LivelockDetected,
        now_ms,
        5 * 60 * 1000,
    );
    if count >= 1 {
        state.insert("actor_kind".to_string(), "orchestrator".to_string());
        state.insert("error_class".to_string(), "livelock_detected".to_string());
    }
}

/// Intent: route_gate
/// Resource: error
/// Inputs: &str
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn route_gate_block_record(reason: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "invariant_gate",
        "phase": "route",
        "gate": "G_r",
        "proposed_role": "executor",
        "blocked": true,
        "reason": reason,
        "ts_ms": crate::logging::now_ms(),
    })
}

/// Intent: route_gate
/// Resource: route_dispatch
/// Inputs: &mut canonical_writer::CanonicalWriter, &std::path::Path, &str
/// Outputs: ()
/// Effects: fs_write, logging, state_write
/// Forbidden: uses_network, spawns_process
/// Invariants: route_gate_blocks_dispatch_and_marks_planner_pending
/// Failure: infallible
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn apply_route_gate_block(writer: &mut CanonicalWriter, ws: &std::path::Path, reason: &str) {
    eprintln!("[invariant_gate] route G_r (BLOCKED): {reason}");
    writer.record_violation("executor", reason);
    let recovery_decision = route_gate_recovery_decision(ws, reason);
    let recovery_budget_exhaustion = recovery_decision
        .as_ref()
        .and_then(|decision| route_gate_recovery_budget_exhaustion(ws, decision));
    let recovery_count = recovery_decision.as_ref().map(|decision| decision.support_count);
    let mut blocker_message = route_gate_blocker_message(reason);
    annotate_route_gate_recovery_payload(&mut blocker_message, recovery_count);
    if persist_planner_blocker_message(writer, &blocker_message) {
        let record = route_gate_block_record(reason);
        crate::blockers::record_action_failure_with_writer(
            ws,
            None,
            "orchestrator",
            "route_dispatch",
            reason,
            None,
        );
        let _ = crate::logging::append_action_log_record(&record);
    }
    if let Some(decision) = recovery_decision {
        if let Some(attempt_count) = recovery_budget_exhaustion {
            record_recovery_suppressed(writer, &decision, attempt_count);
            apply_suppressed_route_gate_terminal_cleanup(writer, &decision);
        } else {
            apply_recovery_decision(writer, &decision);
        }
    }
    if !writer.state().planner_pending {
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
    }
}

const ROUTE_GATE_RECOVERY_TLOG_RECORDS: usize = 128;

pub(super) fn route_gate_recovery_decision(
    ws: &std::path::Path,
    reason: &str,
) -> Option<crate::recovery::RecoveryDecision> {
    let class = crate::recovery::classify_route_gate_reason(reason)?;
    let class_key = class.as_key();
    let count = recent_same_route_gate_violation_count(ws, reason)
        .max(recent_route_gate_violation_class_count(ws, class_key));
    crate::recovery::decision_for_failure(
        class,
        reason,
        count,
        &crate::recovery::RecoveryConfig::default(),
    )
}

pub(super) fn submitted_turn_timeout_recovery_decision(
    ws: &std::path::Path,
    reason: &str,
) -> Option<crate::recovery::RecoveryDecision> {
    let blockers = crate::blockers::load_blockers(ws);
    let now_ms = crate::logging::now_ms();
    let support_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::LlmTimeout,
        now_ms,
        5 * 60 * 1000,
    )
    .max(1);
    crate::recovery::decision_for_failure(
        crate::error_class::ErrorClass::LlmTimeout,
        reason,
        support_count,
        &crate::recovery::RecoveryConfig::default(),
    )
}

pub(super) fn route_gate_recovery_budget_exhaustion(
    ws: &std::path::Path,
    decision: &crate::recovery::RecoveryDecision,
) -> Option<usize> {
    recovery_budget_exhaustion(ws, decision)
}

pub(super) fn recovery_budget_exhaustion(
    ws: &std::path::Path,
    decision: &crate::recovery::RecoveryDecision,
) -> Option<usize> {
    let attempt_count = recent_same_recovery_attempt_count(ws, decision.class.as_key());
    if decision.max_attempts > 0 && attempt_count >= decision.max_attempts {
        Some(attempt_count)
    } else {
        None
    }
}

pub(super) fn recent_same_route_gate_violation_count(
    ws: &std::path::Path,
    reason: &str,
) -> usize {
    let tlog_path = ws.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::read_recent_records(&tlog_path, ROUTE_GATE_RECOVERY_TLOG_RECORDS)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| match &record.event {
            crate::events::Event::Effect {
                event:
                    crate::events::EffectEvent::InvariantViolation {
                        proposed_role,
                        reason: recorded_reason,
                    },
            } => proposed_role.as_str() == "executor" && recorded_reason.as_str() == reason,
            _ => false,
        })
        .count()
}

pub(super) fn recent_route_gate_violation_class_count(
    ws: &std::path::Path,
    class: &str,
) -> usize {
    let tlog_path = ws.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::read_recent_records(&tlog_path, ROUTE_GATE_RECOVERY_TLOG_RECORDS)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| match &record.event {
            crate::events::Event::Effect {
                event:
                    crate::events::EffectEvent::InvariantViolation {
                        proposed_role,
                        reason,
                    },
            } => {
                proposed_role.as_str() == "executor"
                    && crate::recovery::classify_route_gate_reason(reason)
                        .map(|detected| detected.as_key() == class)
                        .unwrap_or(false)
            }
            _ => false,
        })
        .count()
}

pub(super) fn recent_same_recovery_attempt_count(
    ws: &std::path::Path,
    class: &str,
) -> usize {
    let tlog_path = ws.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::read_recent_records(&tlog_path, ROUTE_GATE_RECOVERY_TLOG_RECORDS)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| match &record.event {
            crate::events::Event::Effect {
                event:
                    crate::events::EffectEvent::RecoveryTriggered {
                        class: recorded_class,
                        ..
                    },
            } => recorded_class.as_str() == class,
            _ => false,
        })
        .count()
}

pub(super) fn annotate_route_gate_recovery_payload(
    message: &mut serde_json::Value,
    recovery_count: Option<usize>,
) {
    let Some(count) = recovery_count else {
        return;
    };
    let Some(payload) = message.get_mut("payload").and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let evidence = payload
        .get("evidence")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    payload.insert(
        "evidence".to_string(),
        serde_json::Value::String(format!(
            "{evidence}\nrecovery_trigger=tlog.repeated_invariant_violation recent_same_reason_count={count}"
        )),
    );
    payload.insert(
        "recovery".to_string(),
        serde_json::json!({
            "source": "tlog.ndjson",
            "trigger": "repeated_invariant_violation",
            "recent_same_reason_count": count,
            "action": "clear_executor_wake_and_pending_lanes_then_route_planner"
        }),
    );
}

pub(super) fn apply_recovery_decision(
    writer: &mut CanonicalWriter,
    decision: &crate::recovery::RecoveryDecision,
) {
    record_recovery_triggered(writer, decision);
    if decision.policy != crate::recovery::RecoveryPolicy::ClearExecutorAndWakePlanner {
        record_recovery_outcome(writer, decision, false, decision.support_count, false, 0);
        return;
    }
    let pending_lanes_before = claimable_executor_pending_lane_count(writer);
    let executor_wake_before = writer.state().wake_signals_pending.contains_key("executor");
    let scheduled_phase_before = writer.state().scheduled_phase.clone();
    let planner_pending_before = writer.state().planner_pending;
    let lane_ids = writer
        .state()
        .lanes
        .iter()
        .filter_map(|(lane_id, lane)| {
            if lane.pending && lane.in_progress_by.is_none() {
                Some(*lane_id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for lane_id in &lane_ids {
        writer.apply(ControlEvent::LanePendingSet {
            lane_id: *lane_id,
            pending: false,
        });
    }
    if let Some((_, signature)) = writer.state().wake_signals_pending.get("executor").cloned() {
        writer.apply(ControlEvent::WakeSignalConsumed {
            role: "executor".to_string(),
            signature,
        });
    }
    apply_scheduled_phase_if_changed(writer, Some("planner"));
    if !writer.state().planner_pending {
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
    }
    let pending_lanes_after = claimable_executor_pending_lane_count(writer);
    let executor_wake_after = writer.state().wake_signals_pending.contains_key("executor");
    let routed_to_planner = writer.state().scheduled_phase.as_deref() == Some("planner");
    let planner_pending = writer.state().planner_pending;
    let state_delta_count = usize::from(pending_lanes_after < pending_lanes_before)
        + usize::from(executor_wake_before && !executor_wake_after)
        + usize::from(scheduled_phase_before.as_deref() != Some("planner") && routed_to_planner)
        + usize::from(!planner_pending_before && planner_pending);
    let state_delta_seen = state_delta_count > 0;
    let final_state_repaired =
        pending_lanes_after == 0 && !executor_wake_after && routed_to_planner && planner_pending;
    let success = state_delta_seen && final_state_repaired;
    let failure_count_after = if success { 0 } else { decision.support_count };
    record_recovery_outcome(
        writer,
        decision,
        success,
        failure_count_after,
        state_delta_seen,
        state_delta_count,
    );
    eprintln!(
        "[route_recovery] class={} policy={} repeated={} cleared_lanes={:?} rerouted=planner",
        decision.class.as_key(),
        decision.policy.as_key(),
        decision.support_count,
        lane_ids
    );
}


fn apply_suppressed_route_gate_terminal_cleanup(
    writer: &mut CanonicalWriter,
    decision: &crate::recovery::RecoveryDecision,
) {
    if decision.policy != crate::recovery::RecoveryPolicy::ClearExecutorAndWakePlanner {
        return;
    }

    let pending_lanes_before = claimable_executor_pending_lane_count(writer);
    let executor_wake_before = writer.state().wake_signals_pending.contains_key("executor");
    let scheduled_phase_before = writer.state().scheduled_phase.clone();
    let planner_pending_before = writer.state().planner_pending;
    let lane_ids = writer
        .state()
        .lanes
        .iter()
        .filter_map(|(lane_id, lane)| {
            if lane.pending && lane.in_progress_by.is_none() {
                Some(*lane_id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for lane_id in &lane_ids {
        writer.apply(ControlEvent::LanePendingSet {
            lane_id: *lane_id,
            pending: false,
        });
    }
    if let Some((_, signature)) = writer.state().wake_signals_pending.get("executor").cloned() {
        writer.apply(ControlEvent::WakeSignalConsumed {
            role: "executor".to_string(),
            signature,
        });
    }
    apply_scheduled_phase_if_changed(writer, Some("planner"));
    if !writer.state().planner_pending {
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
    }

    let pending_lanes_after = claimable_executor_pending_lane_count(writer);
    let executor_wake_after = writer.state().wake_signals_pending.contains_key("executor");
    let routed_to_planner = writer.state().scheduled_phase.as_deref() == Some("planner");
    let planner_pending = writer.state().planner_pending;
    let state_delta_count = usize::from(pending_lanes_after < pending_lanes_before)
        + usize::from(executor_wake_before && !executor_wake_after)
        + usize::from(scheduled_phase_before.as_deref() != Some("planner") && routed_to_planner)
        + usize::from(!planner_pending_before && planner_pending);
    let state_delta_seen = state_delta_count > 0;
    let final_state_repaired =
        pending_lanes_after == 0 && !executor_wake_after && routed_to_planner && planner_pending;
    let success = state_delta_seen && final_state_repaired;
    record_recovery_outcome(
        writer,
        decision,
        success,
        if success { 0 } else { decision.support_count },
        state_delta_seen,
        state_delta_count,
    );
    eprintln!(
        "[route_recovery] suppressed_terminal_cleanup class={} policy={} attempts_exhausted cleared_lanes={:?} rerouted=planner success={}",
        decision.class.as_key(),
        decision.policy.as_key(),
        lane_ids,
        success
    );
}

fn record_recovery_triggered(
    writer: &mut CanonicalWriter,
    decision: &crate::recovery::RecoveryDecision,
) {
    writer.record_effect(crate::events::EffectEvent::RecoveryTriggered {
        generated_at_ms: crate::logging::now_ms(),
        class: decision.class.as_key().to_string(),
        policy: decision.policy.as_key().to_string(),
        reason: decision.reason.clone(),
        support_count: decision.support_count,
        threshold: decision.threshold,
        window_ms: decision.window_ms,
    });
}

fn record_recovery_suppressed(
    writer: &mut CanonicalWriter,
    decision: &crate::recovery::RecoveryDecision,
    attempt_count: usize,
) {
    writer.record_effect(crate::events::EffectEvent::RecoverySuppressed {
        generated_at_ms: crate::logging::now_ms(),
        class: decision.class.as_key().to_string(),
        policy: decision.policy.as_key().to_string(),
        reason: decision.reason.clone(),
        suppression_reason: format!(
            "retry_budget_exhausted attempt_count={} max_attempts={}",
            attempt_count, decision.max_attempts
        ),
    });
    eprintln!(
        "[route_recovery] suppressed class={} policy={} attempts={} max_attempts={}",
        decision.class.as_key(),
        decision.policy.as_key(),
        attempt_count,
        decision.max_attempts
    );
}

fn claimable_executor_pending_lane_count(writer: &CanonicalWriter) -> usize {
    writer
        .state()
        .lanes
        .values()
        .filter(|lane| lane.pending && lane.in_progress_by.is_none())
        .count()
}

fn record_recovery_outcome(
    writer: &mut CanonicalWriter,
    decision: &crate::recovery::RecoveryDecision,
    success: bool,
    failure_count_after: usize,
    progress_event_seen: bool,
    eval_window_events: usize,
) {
    writer.record_effect(crate::events::EffectEvent::RecoveryOutcomeRecorded {
        generated_at_ms: crate::logging::now_ms(),
        class: decision.class.as_key().to_string(),
        policy: decision.policy.as_key().to_string(),
        success,
        failure_count_before: decision.support_count,
        failure_count_after,
        progress_event_seen,
        eval_window_events,
    });
}

/// Intent: validation_gate
fn preflight_executor_dispatch(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &RuntimeState,
) -> Option<bool> {
    let ws = std::path::PathBuf::from(workspace());
    let ready_tasks_text = crate::prompt_inputs::read_ready_tasks(&ws, 1);
    let ready_count = if ready_tasks_text == "(no ready tasks)" {
        "0"
    } else {
        "1+"
    };
    if ready_count == "0" {
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        apply_scheduled_phase_if_changed(writer, Some("planner"));
        return Some(true);
    }
    if !evaluate_executor_route_gates(writer, ready_count) {
        return Some(false);
    }

    let lanes_seeded = ctx.lanes.iter().any(|lane| {
        let lane_state = writer.state().lanes.get(&lane.index);
        lane_state.map(|state| state.pending).unwrap_or(false)
            || lane_state
                .and_then(|state| state.in_progress_by.as_ref())
                .is_some()
            || writer.state().lane_in_flight(lane.index)
            || writer.state().lane_submit_active(lane.index)
    });
    let runtime_busy = !rt.executor_submit_inflight.is_empty()
        || !rt.submitted_turns.is_empty()
        || rt
            .deferred_completions
            .values()
            .any(|queue| !queue.is_empty());
    if !lanes_seeded && !runtime_busy {
        eprintln!(
            "[orchestrate] executor bootstrap: ready tasks exist but no lane work is seeded; waking planner"
        );
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        apply_scheduled_phase_if_changed(writer, Some("planner"));
        return Some(true);
    }

    None
}

/// Intent: transport_effect
fn dispatch_executor_submits(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
) -> bool {
    if let Some(dispatch_result) = executor_dispatch_gate_result(ctx, writer, rt) {
        return dispatch_result;
    }

    dispatch_ready_executor_lanes(ctx, writer, rt, now, submit_joinset)
}

fn executor_dispatch_gate_result(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
) -> Option<bool> {
    let semantic_control = SemanticControlState::new(
        writer.state().scheduled_phase.clone(),
        writer.state().planner_pending,
        writer.state().diagnostics_pending,
        writer.state().active_blocker_to_verifier,
    );
    if semantic_control.executor_dispatch_blocked() {
        return Some(false);
    }

    // No-ready-tasks guard: if PLAN.json has no ready tasks, skip executor dispatch
    // entirely and wake the planner instead.  This eliminates the idle turn where
    // the executor discovers no work and sends an empty handoff message back.
    // Route gate G_r: check enforced invariants before dispatching the executor.
    // Currently observational — violations are logged but do not hard-block.
    // Once invariants accumulate enough support, this will become a hard gate.
    // Clean-start guard: after agent_state reset, the executor can see ready
    // PLAN tasks but still have zero lane work seeded because lane.pending is
    // only populated by the planner bootstrap path. In that state, starting at
    // executor would silently idle forever while the browser backend stays live.
    if let Some(preflight_result) = preflight_executor_dispatch(ctx, writer, rt) {
        return Some(preflight_result);
    }

    None
}

/// Intent: transport_effect
fn dispatch_ready_executor_lanes(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
) -> bool {
    let mut cycle_progress = false;

    for lane in ctx.lanes {
        let lane_ready_for_submit = !writer.state().lane_submit_active(lane.index)
            && writer.state().lane_next_submit_ms(lane.index) <= now;
        if !lane_ready_for_submit {
            continue;
        }

        if let Some(job) = claim_executor_submit(writer, lane) {
            queue_executor_lane_submit(
                ctx,
                writer,
                rt,
                submit_joinset,
                lane.index,
                job,
                lane.endpoint.clone(),
                lane.tabs.clone(),
            );
            cycle_progress = true;
        }
    }

    cycle_progress
}

fn queue_executor_lane_submit(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
    lane_index: usize,
    job: PendingExecutorSubmit,
    endpoint: LlmEndpoint,
    tabs: TabManagerHandle,
) {
    writer.apply(ControlEvent::PhaseSet {
        phase: "executor".to_string(),
        lane: Some(lane_index),
    });
    let bridge = ctx.bridge.clone();
    let command_id = make_command_id(&job.executor_role, "executor", 1);
    let response_timeout_secs = response_timeout_for_role(&job.executor_role);
    rt.timed_out_executor_submits.remove(&lane_index);
    rt.executor_submit_inflight.insert(
        lane_index,
        PendingSubmitState {
            job: job.clone(),
            started_ms: now_ms(),
            command_id: command_id.clone(),
            endpoint_id: endpoint.id.clone(),
            tabs: tabs.clone(),
        },
    );
    writer.apply(ControlEvent::LaneSubmitInFlightSet {
        lane_id: lane_index,
        in_flight: true,
    });
    submit_joinset.spawn(async move {
        let result = submit_executor_turn(
            &job,
            &endpoint,
            &bridge,
            &tabs,
            true,
            &command_id,
            response_timeout_secs,
        )
        .await;
        (lane_index, job, result)
    });
}

fn run_executor_phase(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    pending_submit_timeout_ms: u64,
    submitted_turn_timeout_ms: u64,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
) -> bool {
    let mut cycle_progress = false;
    sweep_timed_out_executor_submits(ctx, writer, rt, now, pending_submit_timeout_ms);
    sweep_timed_out_submitted_turns(ctx, writer, rt, now, submitted_turn_timeout_ms);
    if dispatch_executor_submits(ctx, writer, rt, now, submit_joinset) {
        cycle_progress = true;
    }

    while let Some(joined) = submit_joinset.try_join_next() {
        match joined {
            Ok((lane_id, job, result)) => {
                if handle_executor_submit_join_result(
                    ctx,
                    writer,
                    rt,
                    lane_id,
                    job,
                    result,
                    pending_submit_timeout_ms,
                ) {
                    cycle_progress = true;
                }
            }
            Err(err) => {
                eprintln!("[orchestrate] submit join error: {err:#}");
                log_error_event(
                    "orchestrate",
                    "orchestrate",
                    None,
                    &format!("submit join error: {err:#}"),
                    Some(json!({ "stage": "submit_join" })),
                );
            }
        }
    }

    cycle_progress
}

fn handle_executor_submit_join_result(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
    job: PendingExecutorSubmit,
    result: Result<String>,
    pending_submit_timeout_ms: u64,
) -> bool {
    match result {
        Ok(exec_result) => handle_executor_submit_ack_result(
            ctx,
            writer,
            rt,
            lane_id,
            job,
            exec_result,
            pending_submit_timeout_ms,
        ),
        Err(err) => {
            eprintln!(
                "[orchestrate] {} submit error (preserving lane ownership): {err:#}",
                job.executor_name
            );
            log_error_event(
                "executor",
                "orchestrate",
                None,
                &format!(
                    "{} submit error (preserving lane ownership): {err:#}",
                    job.executor_name
                ),
                Some(json!({ "stage": "executor_submit", "lane": job.executor_name })),
            );
            // Recovery: clear stuck ownership and requeue lane
            requeue_lane_after_submit_recovery(writer, rt, job.lane_index);
            false
        }
    }
}

fn log_missing_submit_ack(job: &PendingExecutorSubmit, exec_result: &str) {
    eprintln!(
        "[orchestrate] {} missing submit_ack (preserving lane ownership): {exec_result}",
        job.executor_name
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &format!(
            "{} missing submit_ack (preserving lane ownership): {exec_result}",
            job.executor_name
        ),
        Some(json!({
            "stage": "executor_submit_ack_missing",
            "lane": job.executor_name,
        })),
    );
}

fn handle_missing_submit_ack(
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    job: &PendingExecutorSubmit,
    exec_result: &str,
) -> bool {
    log_missing_submit_ack(job, exec_result);
    crate::blockers::record_action_failure_with_writer(
        std::path::Path::new(&workspace()),
        None,
        "executor",
        "uncanonicalized_recovery",
        &format!(
            "recovery path without canonical event: missing submit_ack forced lane requeue lane={} output={}",
            job.executor_name,
            truncate(exec_result, MAX_SNIPPET)
        ),
        None,
    );
    // Recovery: clear stuck ownership and requeue lane
    requeue_lane_after_submit_recovery(writer, rt, job.lane_index);
    false
}

fn log_late_submit_ack(ctx: &OrchestratorContext<'_>, lane_id: usize, tab_id: u32, turn_id: u64) {
    let lane_label = &ctx.lanes[lane_id].label;
    log_submit_ack_event(
        format!(
            "submit ack without pending submit (late ack — registering turn): lane={} tab_id={} turn_id={}",
            lane_label,
            tab_id,
            turn_id
        ),
        json!({
            "stage": "executor_submit_ack_late",
            "lane": lane_label,
            "tab_id": tab_id,
            "turn_id": turn_id,
        }),
    );
}

fn register_late_submit_ack(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
    pending: PendingSubmitState,
    tab_id: u32,
    turn_id: u64,
    command_id: Option<String>,
) -> bool {
    if !submit_ack_matches_active_tab(ctx, writer, lane_id, tab_id) {
        return false;
    }
    log_late_submit_ack(ctx, lane_id, tab_id, turn_id);
    crate::blockers::record_action_failure_with_writer(
        ctx.workspace.as_path(),
        None,
        "executor",
        "uncanonicalized_recovery",
        &format!(
            "recovery path without canonical event: late submit_ack reconstructed turn lane={} tab_id={} turn_id={}",
            ctx.lanes[lane_id].label, tab_id, turn_id
        ),
        None,
    );
    register_submitted_executor_turn(
        writer,
        rt,
        lane_id,
        tab_id,
        turn_id,
        build_submitted_executor_turn(writer, &pending.job, &pending, tab_id, command_id),
    );
    true
}

fn log_late_submit_ack_command_mismatch(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    expected_command_id: &str,
    observed_command_id: Option<&str>,
) {
    let lane_label = &ctx.lanes[lane_id].label;
    let observed = observed_command_id.unwrap_or("<missing>");
    let message = format!(
        "late submit ack command mismatch: lane={} expected_command_id={} observed_command_id={} (ignoring stale ack)",
        lane_label, expected_command_id, observed
    );
    log_submit_ack_event(
        message.clone(),
        json!({
            "stage": "executor_submit_ack_command_mismatch",
            "lane": lane_label,
            "expected_command_id": expected_command_id,
            "observed_command_id": observed,
        }),
    );
    crate::blockers::record_action_failure_with_writer(
        ctx.workspace.as_path(),
        None,
        "executor",
        "runtime_control_bypass",
        &format!("runtime-only control influence: {message}"),
        None,
    );
}

fn take_matching_timed_out_submit(
    ctx: &OrchestratorContext<'_>,
    rt: &mut RuntimeState,
    lane_id: usize,
    command_id: Option<&str>,
) -> Option<PendingSubmitState> {
    let pending = rt.timed_out_executor_submits.remove(&lane_id)?;
    if command_id == Some(pending.command_id.as_str()) {
        return Some(pending);
    }

    log_late_submit_ack_command_mismatch(ctx, lane_id, &pending.command_id, command_id);
    rt.timed_out_executor_submits.insert(lane_id, pending);
    None
}

fn log_submit_ack_tab_mismatch(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    active_tab: u32,
    tab_id: u32,
) {
    let lane_label = &ctx.lanes[lane_id].label;
    log_submit_ack_event(
        format!(
            "submit ack tab mismatch: lane={} active_tab={} ack_tab={} (rebinding active tab)",
            lane_label, active_tab, tab_id
        ),
        json!({
            "stage": "executor_submit_ack_tab_mismatch",
            "lane": lane_label,
            "active_tab": active_tab,
            "ack_tab": tab_id,
        }),
    );
}

fn log_submit_ack_event(message: String, payload: serde_json::Value) {
    eprintln!("[orchestrate] {message}");
    log_error_event("executor", "orchestrate", None, &message, Some(payload));
}

fn submitted_executor_steps_used(writer: &CanonicalWriter, lane_id: usize) -> usize {
    writer.state().lane_steps_used_count(lane_id)
}

/// Intent: pure_transform
fn build_submitted_executor_turn(
    writer: &CanonicalWriter,
    job: &PendingExecutorSubmit,
    pending: &PendingSubmitState,
    tab_id: u32,
    command_id: Option<String>,
) -> SubmittedExecutorTurn {
    SubmittedExecutorTurn {
        tab_id,
        lane: job.lane_index,
        lane_label: job.label.clone(),
        command_id: command_id.unwrap_or_else(|| pending.command_id.clone()),
        started_ms: pending.started_ms,
        actor: job.executor_role.clone(),
        endpoint_id: pending.endpoint_id.clone(),
        tabs: pending.tabs.clone(),
        steps_used: submitted_executor_steps_used(writer, job.lane_index),
    }
}

/// Intent: transport_effect
fn submit_ack_matches_active_tab(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    lane_id: usize,
    tab_id: u32,
) -> bool {
    let Some(active_tab) = writer.state().lane_active_tab_id(lane_id) else {
        return true;
    };
    if active_tab == tab_id {
        return true;
    }

    log_submit_ack_tab_mismatch(ctx, lane_id, active_tab, tab_id);
    writer.apply(ControlEvent::ExecutorSubmitAckTabRebound {
        lane_id,
        from_tab_id: active_tab,
        to_tab_id: tab_id,
    });
    true
}

fn handle_executor_submit_ack_result(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
    job: PendingExecutorSubmit,
    exec_result: String,
    pending_submit_timeout_ms: u64,
) -> bool {
    let Some((tab_id, turn_id, command_id)) = parse_submit_ack(&exec_result) else {
        return handle_missing_submit_ack(writer, rt, &job, &exec_result);
    };

    if !submit_ack_matches_active_tab(ctx, writer, lane_id, tab_id) {
        return false;
    }

    let Some(pending) = rt.executor_submit_inflight.remove(&lane_id) else {
        // The timeout path already removed executor_submit_inflight for
        // this lane, but the submit actually succeeded.  Register the turn
        // so the completion can still be routed back to the LLM.
        let Some(pending) = take_matching_timed_out_submit(
            ctx,
            rt,
            lane_id,
            command_id.as_deref(),
        ) else {
            return false;
        };
        return register_late_submit_ack(
            ctx, writer, rt, lane_id, pending, tab_id, turn_id, command_id,
        );
    };
    let _ = pending_submit_timeout_ms;

    register_submitted_executor_turn(
        writer,
        rt,
        lane_id,
        tab_id,
        turn_id,
        build_submitted_executor_turn(writer, &job, &pending, tab_id, command_id),
    );
    true
}

async fn process_completed_turns(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    let completed_turns = ctx.bridge.take_completed_turns().await;
    for item in completed_turns {
        cycle_progress |= process_completed_turn_item(
            ctx,
            writer,
            rt,
            continuation_joinset,
            verifier_pending_results,
            item,
        );
    }
    cycle_progress
}

fn process_completed_turn_item(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    item: Value,
) -> bool {
    append_orchestration_trace("llm_message_received", item.clone());
    let Some((tab_id, turn_id, exec_result, completed_endpoint_id)) = parse_completed_turn(&item)
    else {
        return false;
    };
    let Some(submitted) = resolve_completed_turn_submission(
        ctx,
        writer,
        rt,
        tab_id,
        turn_id,
        &exec_result,
        completed_endpoint_id.as_deref(),
    ) else {
        return false;
    };
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id: submitted.lane,
        in_flight: false,
    });
    handle_executor_completion(
        submitted,
        tab_id,
        turn_id,
        exec_result,
        writer,
        rt,
        ctx.lanes,
        ctx.bridge,
        ctx.workspace,
        continuation_joinset,
        verifier_pending_results,
    )
}

fn resolve_completed_turn_submission(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    tab_id: u32,
    turn_id: u64,
    exec_result: &str,
    completed_endpoint_id: Option<&str>,
) -> Option<SubmittedExecutorTurn> {
    if let Some(submitted) = rt.submitted_turns.remove(&(tab_id, turn_id)) {
        return validate_registered_submitted_turn(
            writer,
            tab_id,
            turn_id,
            submitted,
            completed_endpoint_id,
        );
    }
    recover_completed_turn_submission(
        ctx,
        writer,
        rt,
        tab_id,
        turn_id,
        exec_result,
        completed_endpoint_id,
    )
}

/// Intent: validation_gate
fn validate_registered_submitted_turn(
    writer: &mut CanonicalWriter,
    tab_id: u32,
    turn_id: u64,
    submitted: SubmittedExecutorTurn,
    completed_endpoint_id: Option<&str>,
) -> Option<SubmittedExecutorTurn> {
    writer.apply(ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id });
    if check_completion_endpoint(&submitted.endpoint_id, completed_endpoint_id)
        == CompletionEndpointCheck::Mismatch
    {
        append_orchestration_trace(
            "executor_completion_endpoint_mismatch",
            json!({
                "tab_id": tab_id,
                "turn_id": turn_id,
                "expected_endpoint_id": submitted.endpoint_id,
                "completed_endpoint_id": completed_endpoint_id,
            }),
        );
        return None;
    }
    Some(submitted)
}

fn recover_completed_turn_submission(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    tab_id: u32,
    turn_id: u64,
    exec_result: &str,
    completed_endpoint_id: Option<&str>,
) -> Option<SubmittedExecutorTurn> {
    let lane_id = match writer.state().tab_id_to_lane.get(&tab_id).copied() {
        Some(lane_id) => lane_id,
        None => {
            trace_unmatched_executor_completion(tab_id, turn_id, exec_result);
            return None;
        }
    };
    if check_completion_endpoint(&ctx.lanes[lane_id].endpoint.id, completed_endpoint_id)
        == CompletionEndpointCheck::Mismatch
    {
        append_orchestration_trace(
            "executor_completion_endpoint_mismatch",
            json!({
                "lane_name": ctx.lanes[lane_id].label,
                "tab_id": tab_id,
                "turn_id": turn_id,
                "expected_endpoint_id": ctx.lanes[lane_id].endpoint.id,
                "completed_endpoint_id": completed_endpoint_id,
            }),
        );
        return None;
    }
    let active_tab = writer.state().lane_active_tab_id(lane_id);
    if check_completion_tab(active_tab, tab_id) == CompletionTabCheck::Mismatch {
        append_orchestration_trace(
            "executor_completion_tab_mismatch",
            json!({
                "lane_name": ctx.lanes[lane_id].label,
                "active_tab": active_tab,
                "tab_id": tab_id,
                "turn_id": turn_id,
            }),
        );
        return None;
    }
    let pending = match rt.executor_submit_inflight.remove(&lane_id) {
        Some(pending) => pending,
        None => {
            trace_unmatched_executor_completion(tab_id, turn_id, exec_result);
            return None;
        }
    };
    Some(build_recovered_submitted_turn(
        ctx, writer, tab_id, turn_id, lane_id, pending,
    ))
}

fn trace_unmatched_executor_completion(tab_id: u32, turn_id: u64, exec_result: &str) {
    append_orchestration_trace(
        "executor_completion_unmatched",
        json!({
            "tab_id": tab_id,
            "turn_id": turn_id,
            "text": truncate(exec_result, MAX_SNIPPET),
        }),
    );
}

/// Intent: pure_transform
fn build_recovered_submitted_turn(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    tab_id: u32,
    turn_id: u64,
    lane_id: usize,
    pending: PendingSubmitState,
) -> SubmittedExecutorTurn {
    writer.apply(ControlEvent::ExecutorCompletionRecovered {
        tab_id,
        turn_id,
        lane_id,
        lane_label: ctx.lanes[lane_id].label.clone(),
        actor: pending.job.executor_role.clone(),
        endpoint_id: pending.endpoint_id.clone(),
    });
    let steps_used = writer.state().lane_steps_used_count(lane_id);
    SubmittedExecutorTurn {
        tab_id,
        lane: lane_id,
        lane_label: ctx.lanes[lane_id].label.clone(),
        command_id: pending.command_id,
        started_ms: pending.started_ms,
        actor: pending.job.executor_role,
        endpoint_id: pending.endpoint_id,
        tabs: pending.tabs,
        steps_used,
    }
}

fn drain_continuations(
    writer: &mut CanonicalWriter,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    while let Some(joined) = continuation_joinset.try_join_next() {
        cycle_progress |= handle_joined_continuation(
            writer,
            verifier_pending_results,
            joined,
        );
    }
    cycle_progress
}

fn handle_joined_continuation(
    writer: &mut CanonicalWriter,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    joined: std::result::Result<ContinuationJoinOutput, tokio::task::JoinError>,
) -> bool {
    match joined {
        Ok((submitted, turn_id, result, mut effect_rx)) => {
            record_continuation_effects(writer, &mut effect_rx);
            handle_completed_continuation(
                writer,
                verifier_pending_results,
                submitted,
                turn_id,
                result,
            )
        }
        Err(err) => {
            log_continuation_join_error(&err);
            false
        }
    }
}

fn record_continuation_effects(
    writer: &mut CanonicalWriter,
    effect_rx: &mut UnboundedReceiver<EffectEvent>,
) {
    while let Ok(effect) = effect_rx.try_recv() {
        writer.record_effect(effect);
    }
}

fn log_continuation_join_error(err: &tokio::task::JoinError) {
    eprintln!("[orchestrate] continuation join error: {err:#}");
    log_error_event(
        "orchestrate",
        "orchestrate",
        None,
        &format!("continuation join error: {err:#}"),
        Some(json!({ "stage": "continuation_join" })),
    );
}

fn handle_completed_continuation(
    writer: &mut CanonicalWriter,
    _verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    submitted: SubmittedExecutorTurn,
    turn_id: u64,
    result: Result<AgentCompletion>,
) -> bool {
    match result {
        Ok(completion) => {
            writer.apply(ControlEvent::LanePromptInFlightSet {
                lane_id: submitted.lane,
                in_flight: false,
            });
            match completion {
                AgentCompletion::MessageAction { action, summary } => {
                    finalize_executor_message_completion(writer, submitted.lane);
                    apply_control_from_executor_action_result(
                        writer,
                        submitted.lane,
                        &action,
                        &summary,
                    );
                }
                AgentCompletion::Summary(final_exec_result) => {
                    finalize_executor_summary_without_verifier(
                        writer,
                        &submitted,
                        turn_id,
                        &final_exec_result,
                    );
                }
            }
        }
        Err(err) => {
            let err_text = format!("{err:#}");
            recover_failed_continuation(writer, &submitted, &err_text);
        }
    }
    true
}

fn finalize_executor_summary_without_verifier(
    writer: &mut CanonicalWriter,
    submitted: &SubmittedExecutorTurn,
    turn_id: u64,
    final_exec_result: &str,
) {
    let summary = truncate(final_exec_result, 800).replace('\n', " ");
    let action = json!({
        "action": "message",
        "from": "executor",
        "to": "planner",
        "type": "handoff",
        "status": "complete",
        "task_id": submitted.command_id,
        "observation": format!(
            "executor completed turn {} on lane {} in two-role mode; verifier phase is inlined into planner",
            turn_id,
            submitted.lane_label
        ),
        "rationale": "Two-role runtime routes executor completion summaries directly to planner for integrated planning/verification/diagnostics.",
        "predicted_next_actions": [
            { "action": "plan", "intent": "update the task graph based on executor completion evidence" },
            { "action": "message", "intent": "handoff the next bounded task to executor" }
        ],
        "payload": {
            "summary": format!("Executor completion (lane={} turn={}): {}", submitted.lane_label, turn_id, summary),
            "executor_result": truncate(final_exec_result, 4000),
        }
    });
    finalize_executor_message_completion(writer, submitted.lane);
    apply_control_from_executor_action_result(writer, submitted.lane, &action, final_exec_result);
}

fn recover_failed_continuation(
    writer: &mut CanonicalWriter,
    submitted: &SubmittedExecutorTurn,
    err_text: &str,
) {
    eprintln!(
        "[orchestrate] executor continuation error: lane={} err={}",
        submitted.lane_label, err_text
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &format!(
            "executor continuation error: lane={} err={}",
            submitted.lane_label, err_text
        ),
        Some(json!({ "stage": "executor_continuation", "lane": submitted.lane_label })),
    );
    let lane_id = submitted.lane;
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    writer.apply(ControlEvent::LanePendingSet {
        lane_id,
        pending: true,
    });
}

fn drain_deferred_completions(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    for lane_id in 0..ctx.lanes.len() {
        if writer.state().lane_in_flight(lane_id) {
            continue;
        }
        cycle_progress |= drain_lane_deferred_completions(
            ctx,
            writer,
            rt,
            continuation_joinset,
            verifier_pending_results,
            lane_id,
        );
    }
    cycle_progress
}

fn drain_lane_deferred_completions(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    lane_id: usize,
) -> bool {
    let mut cycle_progress = false;
    while let Some(deferred) = rt
        .deferred_completions
        .get_mut(&lane_id)
        .and_then(|queue| queue.pop_front())
    {
        if handle_executor_completion(
            deferred.submitted,
            deferred.tab_id,
            deferred.turn_id,
            deferred.exec_result,
            writer,
            rt,
            ctx.lanes,
            ctx.bridge,
            ctx.workspace,
            continuation_joinset,
            verifier_pending_results,
        ) {
            cycle_progress = true;
        }
        if writer.state().lane_in_flight(lane_id) {
            break;
        }
    }
    cycle_progress
}

fn canonical_role_label(role: &str) -> &'static str {
    if role.starts_with("executor") {
        "executor"
    } else if role == "solo" {
        "solo"
    } else if role == "verifier" {
        "verifier"
    } else if role == "diagnostics" {
        "planner"
    } else if role == "planner" || role == "mini_planner" {
        "planner"
    } else {
        "executor"
    }
}

fn blocker_target_role(role: &str) -> &'static str {
    if role == "planner" || role == "mini_planner" {
        "executor"
    } else {
        "planner"
    }
}

fn blocker_escalation_prompt(role: &str, last_error: &str, task_context: &str) -> String {
    let from = canonical_role_label(role);
    let to = blocker_target_role(role);
    format!(
        "Repeated failures detected. You cannot proceed without external action.\nReturn exactly one action that reports a blocker using this schema:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"{from}\",\n  \"to\": \"{to}\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Summarize the blocked state based on evidence.\",\n  \"rationale\": \"Explain why you cannot proceed.\",\n  \"predicted_next_actions\": [\n    {{\n      \"action\": \"read_file\",\n      \"intent\": \"Reinspect the blocking artifact after the required external fix lands.\"\n    }},\n    {{\n      \"action\": \"message\",\n      \"intent\": \"Report completion or a narrower blocker once the external fix is available.\"\n    }}\n  ],\n  \"payload\": {{\n    \"summary\": \"Short blocker summary.\",\n    \"blocker\": \"Root cause that prevents progress.\",\n    \"evidence\": \"{evidence}\",\n    \"required_action\": \"What must be fixed to continue.\",\n    \"severity\": \"error\"\n  }}\n}}\n```\nTask context:\n{context}\nReturn exactly one action.",
        evidence = truncate(last_error, MAX_SNIPPET),
        context = truncate(task_context, MAX_SNIPPET),
    )
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::option::Option<(u32, u64, std::string::String, std::option::Option<std::string::String>)>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_completed_turn(value: &Value) -> Option<(u32, u64, String, Option<String>)> {
    let tab_id = value.get("tab_id").and_then(|x| x.as_u64())? as u32;
    let turn_id = value.get("turn_id").and_then(|x| x.as_u64())?;
    let text = value.get("text").and_then(|x| x.as_str())?.to_string();
    let endpoint_id = value
        .get("endpoint_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    Some((tab_id, turn_id, text, endpoint_id))
}

fn handle_executor_completion(
    mut submitted: SubmittedExecutorTurn,
    tab_id: u32,
    turn_id: u64,
    exec_result: String,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lanes: &[LaneConfig],
    bridge: &WsBridge,
    workspace: &PathBuf,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
    _verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    submitted.steps_used = submitted.steps_used.saturating_add(1);
    writer.apply(ControlEvent::LaneStepsUsedSet {
        lane_id: submitted.lane,
        steps: submitted.steps_used,
    });
    let lane_cfg = &lanes[submitted.lane];
    let lane_name = lane_cfg.label.as_str();
    if maybe_defer_executor_completion(
        writer,
        rt,
        &submitted,
        turn_id,
        tab_id,
        &exec_result,
        lane_name,
    ) {
        return false;
    }

    record_executor_completion_observation(&submitted, lane_name, turn_id, tab_id, &exec_result);
    let mut submitted = submitted;
    maybe_rebind_executor_completion_tab(
        workspace,
        writer,
        &mut submitted,
        tab_id,
        turn_id,
        lane_name,
    );
    if handle_executor_completion_message_action(
        writer,
        &submitted,
        lane_cfg,
        &exec_result,
    ) {
        return true;
    }
    handle_executor_completion_tool_continuation(
        writer,
        &submitted,
        lane_cfg,
        lane_name,
        tab_id,
        turn_id,
        &exec_result,
        bridge,
        workspace,
        continuation_joinset,
    )
}

fn handle_executor_completion_tool_continuation(
    writer: &mut CanonicalWriter,
    submitted: &SubmittedExecutorTurn,
    lane_cfg: &LaneConfig,
    lane_name: &str,
    tab_id: u32,
    turn_id: u64,
    exec_result: &str,
    bridge: &WsBridge,
    workspace: &PathBuf,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
) -> bool {
    eprintln!(
        "[orchestrate] executor turn requires tool execution: lane={} turn_id={}",
        lane_name, turn_id
    );
    append_orchestration_trace(
        "executor_completion_requires_tool",
        json!({
            "lane_name": lane_name,
            "tab_id": tab_id,
            "turn_id": turn_id,
            "endpoint_id": lane_cfg.endpoint.id,
        }),
    );
    spawn_executor_completion_continuation(
        writer,
        submitted,
        lane_cfg,
        tab_id,
        turn_id,
        exec_result,
        bridge,
        workspace,
        continuation_joinset,
    );
    true
}

fn record_executor_completion_observation(
    submitted: &SubmittedExecutorTurn,
    lane_name: &str,
    turn_id: u64,
    tab_id: u32,
    exec_result: &str,
) {
    trace_executor_completion_processed(lane_name, turn_id, tab_id);
    if let Err(e) = append_executor_completion_log(submitted, 1, turn_id, tab_id, exec_result) {
        log_executor_completion_observation_error(submitted, lane_name, turn_id, tab_id, &e);
    }
}

fn trace_executor_completion_processed(lane_name: &str, turn_id: u64, tab_id: u32) {
    append_orchestration_trace(
        "llm_message_processed",
        json!({
            "tab_id": tab_id,
            "turn_id": turn_id,
            "lane_name": lane_name,
        }),
    );
}

fn log_executor_completion_observation_error(
    submitted: &SubmittedExecutorTurn,
    lane_name: &str,
    turn_id: u64,
    tab_id: u32,
    error: &impl std::fmt::Display,
) {
    eprintln!("[orchestrate] executor_completion_log_error: {error}");
    log_error_event(
        "orchestrate",
        "executor_completion_log",
        Some(1),
        &format!(
            "executor completion log append failed for lane={} turn_id={} tab_id={}: {error}",
            lane_name, turn_id, tab_id
        ),
        Some(json!({
            "lane_name": lane_name,
            "turn_id": turn_id,
            "tab_id": tab_id,
            "endpoint_id": submitted.endpoint_id,
            "command_id": submitted.command_id,
        })),
    );
}

fn log_startup_stage_error(stage_label: &str, err: &impl std::fmt::Display) {
    eprintln!("[canon-mini-agent] {stage_label} failed: {err:#}");
    log_error_event(
        "orchestrate",
        "startup",
        None,
        &format!("{stage_label} failed: {err:#}"),
        Some(json!({ "stage": "startup" })),
    );
}

fn maybe_defer_executor_completion(
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    submitted: &SubmittedExecutorTurn,
    turn_id: u64,
    tab_id: u32,
    exec_result: &str,
    lane_name: &str,
) -> bool {
    if !writer.state().lane_in_flight(submitted.lane) {
        return false;
    }
    rt.deferred_completions
        .entry(submitted.lane)
        .or_default()
        .push_back(DeferredExecutorCompletion {
            submitted: submitted.clone(),
            turn_id,
            tab_id,
            exec_result: exec_result.to_string(),
        });
    append_orchestration_trace(
        "executor_completion_deferred",
        json!({
            "lane_name": lane_name,
            "tab_id": tab_id,
            "turn_id": turn_id,
        }),
    );
    true
}

fn maybe_rebind_executor_completion_tab(
    workspace: &Path,
    writer: &mut CanonicalWriter,
    submitted: &mut SubmittedExecutorTurn,
    tab_id: u32,
    turn_id: u64,
    lane_name: &str,
) {
    if submitted.tab_id == tab_id {
        return;
    }
    record_executor_completion_tab_rebind(
        workspace,
        writer,
        submitted,
        tab_id,
        turn_id,
        lane_name,
    );
}

fn record_executor_completion_tab_rebind(
    workspace: &Path,
    writer: &mut CanonicalWriter,
    submitted: &mut SubmittedExecutorTurn,
    tab_id: u32,
    turn_id: u64,
    lane_name: &str,
) {
    eprintln!(
        "[orchestrate] completed turn tab rebound: turn_id={} expected_tab={} actual_tab={}",
        turn_id, submitted.tab_id, tab_id
    );
    append_orchestration_trace(
        "executor_completion_tab_rebound",
        json!({
            "lane_name": lane_name,
            "turn_id": turn_id,
            "expected_tab": submitted.tab_id,
            "actual_tab": tab_id,
        }),
    );
    crate::blockers::record_action_failure_with_writer(
        workspace,
        None,
        "executor",
        "runtime_control_bypass",
        &format!(
            "runtime-only control influence: executor completion rebound changed lane={} turn_id={} tab from {} to {}",
            lane_name, turn_id, submitted.tab_id, tab_id
        ),
        None,
    );
    let lane_id = submitted.lane;
    writer.apply(ControlEvent::ExecutorCompletionTabRebound {
        lane_id,
        from_tab_id: submitted.tab_id,
        to_tab_id: tab_id,
    });
    submitted.tab_id = tab_id;
}

fn spawn_executor_completion_continuation(
    writer: &mut CanonicalWriter,
    submitted: &SubmittedExecutorTurn,
    lane_cfg: &LaneConfig,
    tab_id: u32,
    turn_id: u64,
    exec_result: &str,
    bridge: &WsBridge,
    workspace: &PathBuf,
    continuation_joinset: &mut tokio::task::JoinSet<ContinuationJoinOutput>,
) {
    let executor_endpoint = lane_cfg.endpoint.clone();
    let bridge = bridge.clone();
    let workspace = workspace.clone();
    let exec_result = exec_result.to_string();
    let submitted_clone = submitted.clone();
    let tabs = submitted.tabs.clone();
    let (effect_tx, effect_rx) = mpsc::unbounded_channel::<EffectEvent>();
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id: submitted.lane,
        in_flight: true,
    });
    continuation_joinset.spawn(async move {
        let result = continue_executor_completion(
            &submitted_clone,
            tab_id,
            &exec_result,
            turn_id,
            &executor_endpoint,
            &bridge,
            &workspace,
            &tabs,
            Some(effect_tx),
        )
        .await;
        (submitted_clone, turn_id, result, effect_rx)
    });
}

fn handle_executor_completion_message_action(
    writer: &mut CanonicalWriter,
    submitted: &SubmittedExecutorTurn,
    lane_cfg: &LaneConfig,
    exec_result: &str,
) -> bool {
    let Ok(mut actions) = parse_actions(exec_result) else {
        return false;
    };
    if actions
        .first()
        .and_then(|a| a.get("action"))
        .and_then(|v| v.as_str())
        != Some("message")
    {
        return false;
    }

    writer.apply(ControlEvent::LaneStepsUsedSet {
        lane_id: submitted.lane,
        steps: 0,
    });
    let Some(action) = actions.pop() else {
        return false;
    };

    log_action_result(
        Some(writer),
        &submitted.actor,
        &lane_cfg.endpoint,
        "executor",
        1,
        &submitted.command_id,
        &action,
        true,
        exec_result,
    );
    finalize_executor_message_completion(writer, submitted.lane);
    apply_control_from_executor_action_result(writer, submitted.lane, &action, exec_result);
    true
}

fn finalize_executor_message_completion(writer: &mut CanonicalWriter, lane_id: usize) {
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LaneStepsUsedSet { lane_id, steps: 0 });
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    writer.apply(ControlEvent::LanePendingSet {
        lane_id,
        pending: false,
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutorActionResultClass {
    ReadyHandoff,
    BlockedHandoff,
    CompleteHandoff,
    ToolFailPlanning,
    ToolFailRecoverable,
    ToolOk,
}

fn exec_result_has_cargo_check_failure(exec_result: &str) -> bool {
    let text = exec_result.to_ascii_lowercase();
    text.contains("cargo check failed")
        || text.contains("error: could not compile")
        || (text.contains("cargo check") && text.contains("error["))
}

fn classify_executor_action_result_class(
    action: &Value,
    exec_result: &str,
) -> ExecutorActionResultClass {
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if status == "blocked" {
        return ExecutorActionResultClass::BlockedHandoff;
    }
    let action_kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if action_kind == "message" {
        if exec_result_has_cargo_check_failure(exec_result) {
            return ExecutorActionResultClass::ToolFailPlanning;
        }
        if status == "ready" {
            return ExecutorActionResultClass::ReadyHandoff;
        }
        if status == "complete" {
            return ExecutorActionResultClass::CompleteHandoff;
        }
        return ExecutorActionResultClass::ToolOk;
    }

    let class = crate::error_class::classify_result(action_kind, exec_result, false);
    use crate::error_class::ErrorClass;
    match class {
        ErrorClass::MissingTarget
        | ErrorClass::InvalidSchema
        | ErrorClass::PlanPreflightFailed
        | ErrorClass::CompileError
        | ErrorClass::VerificationFailed
        | ErrorClass::UnauthorizedPlanOp
        | ErrorClass::InvalidRoute => ExecutorActionResultClass::ToolFailPlanning,
        ErrorClass::LlmTimeout | ErrorClass::ReactionOnly => {
            ExecutorActionResultClass::ToolFailRecoverable
        }
        _ => ExecutorActionResultClass::ToolOk,
    }
}

fn synthesize_executor_blocker_handoff(action: &Value, exec_result: &str) -> Value {
    let action_kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    json!({
        "action": "message",
        "from": "executor",
        "to": "planner",
        "type": "blocker",
        "status": "blocked",
        "observation": "Executor produced a non-recoverable result class; planner repair is required before the next execute phase.",
        "rationale": "Control transitions must be derived from canonical result classes, and planning-class failures are routed to planner.",
        "payload": {
            "summary": format!("Executor {} failed and needs planner repair", action_kind),
            "blocker": "Executor action result mapped to planning failure class",
            "error_class": crate::error_class::classify_result(action_kind, exec_result, false).as_key(),
            "evidence": truncate(exec_result, 2000)
        }
    })
}

fn apply_control_from_executor_action_result(
    writer: &mut CanonicalWriter,
    lane_id: usize,
    action: &Value,
    exec_result: &str,
) {
    match classify_executor_action_result_class(action, exec_result) {
        ExecutorActionResultClass::BlockedHandoff => {
            persist_planner_message(writer, action);
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            writer.apply(ControlEvent::ScheduledPhaseSet { phase: None });
        }
        ExecutorActionResultClass::ToolFailPlanning => {
            let blocker = synthesize_executor_blocker_handoff(action, exec_result);
            persist_planner_message(writer, &blocker);
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            writer.apply(ControlEvent::ScheduledPhaseSet { phase: None });
        }
        ExecutorActionResultClass::ToolFailRecoverable => {
            writer.apply(ControlEvent::LanePendingSet {
                lane_id,
                pending: true,
            });
        }
        ExecutorActionResultClass::ReadyHandoff
        | ExecutorActionResultClass::CompleteHandoff
        | ExecutorActionResultClass::ToolOk => {
            persist_executor_completion_message(writer, action);
        }
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &mut canonical_writer::CanonicalWriter, &serde_json::Value
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_executor_completion_message(writer: &mut CanonicalWriter, action: &Value) {
    let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let action_text = serde_json::to_string_pretty(action).unwrap_or_default();
    let from_role = action
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("executor");
    let effective_to = normalize_executor_completion_target(to_role);
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if effective_to.eq_ignore_ascii_case("planner") {
        // Single operational-agent mode:
        // non-blocked executor->planner handoffs are phase transitions, not
        // message-passing dependencies. Keep blocked escalations canonical.
        if status == "blocked" {
            persist_planner_message(writer, action);
        } else {
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            writer.apply(ControlEvent::ScheduledPhaseSet { phase: None });
            return;
        }
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        return;
    }

    let to_key = effective_to
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    persist_non_planner_inbound_message(writer, from_role, &to_key, &action_text);
}

fn canonical_objectives_text(workspace: &Path) -> String {
    crate::objectives::load_canonical_objectives_json(workspace).unwrap_or_else(|| {
        let path = crate::objectives::resolve_objectives_path(workspace);
        read_text_or_empty(path)
    })
}

fn objective_status_normalized(objective: &crate::objectives::Objective) -> Option<String> {
    if !objective.status.trim().is_empty() {
        return Some(objective.status.trim().to_ascii_lowercase());
    }
    crate::objectives::extract_status(&objective.description).map(|s| s.to_ascii_lowercase())
}

fn objective_requires_plan_work(objective: &crate::objectives::Objective) -> bool {
    let status = objective_status_normalized(objective);
    if let Some(status) = status.as_deref() {
        if matches!(status, "deferred" | "blocked") {
            return false;
        }
    }
    !crate::objectives::is_completed(objective)
}

fn has_actionable_objectives(objectives_text: &str) -> bool {
    let Ok(file) = serde_json::from_str::<crate::objectives::ObjectivesFile>(objectives_text)
    else {
        return false;
    };
    file.objectives.iter().any(objective_requires_plan_work)
}

fn should_reject_solo_self_complete(
    action: &Value,
    objectives_text: &str,
    plan_text: &str,
) -> bool {
    let is_complete_message = action.get("action").and_then(|v| v.as_str()) == Some("message")
        && action
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("complete"));
    if !is_complete_message {
        return false;
    }
    has_actionable_objectives(objectives_text)
        && !crate::orchestrator_seam::plan_has_incomplete_tasks(plan_text)
}

fn action_retry_fingerprint(action: &Value) -> String {
    let mut action = action.clone();
    if let Some(obj) = action.as_object_mut() {
        for key in [
            "command_id",
            "observation",
            "rationale",
            "question",
            "predicted_next_actions",
        ] {
            obj.remove(key);
        }
    }
    serde_json::to_string(&action).unwrap_or_default()
}

fn semantic_action_fingerprint(action: &Value) -> String {
    let mut action = action.clone();
    if let Some(obj) = action.as_object_mut() {
        for key in [
            "command_id",
            "observation",
            "rationale",
            "question",
            "predicted_next_actions",
        ] {
            obj.remove(key);
        }
        if obj.get("action").and_then(|v| v.as_str()) == Some("message") {
            if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
                payload.remove("summary");
                payload.remove("evidence");
            }
        }
    }
    serde_json::to_string(&action).unwrap_or_default()
}

fn verifier_confirmed_with_plan_text(reason: &str, plan_text: &str) -> bool {
    if crate::orchestrator_seam::plan_has_incomplete_tasks(plan_text) {
        return false;
    }
    if let Ok(v) = serde_json::from_str::<Value>(reason) {
        if let Some(verified) = v.get("verified").and_then(|x| x.as_bool()) {
            return verified;
        }
    }
    false
}

fn verifier_confirmed(reason: &str) -> bool {
    let plan_text =
        crate::prompt_inputs::read_text_or_empty(Path::new(workspace()).join(MASTER_PLAN_FILE));
    verifier_confirmed_with_plan_text(reason, &plan_text)
}

fn claim_next_lane(writer: &mut CanonicalWriter, lane: &LaneConfig) -> Option<(usize, String)> {
    let lane_id = lane.index;
    let (pending, in_progress, latest_result) = {
        let s = writer.state();
        let ls = s.lanes.get(&lane_id);
        (
            ls.map(|l| l.pending).unwrap_or(false),
            ls.and_then(|l| l.in_progress_by.as_deref()).is_some(),
            ls.map(|l| l.latest_verifier_result.clone())
                .unwrap_or_default(),
        )
    };
    if pending && !in_progress {
        writer.apply(ControlEvent::LanePendingSet {
            lane_id,
            pending: false,
        });
        writer.apply(ControlEvent::LaneInProgressSet {
            lane_id,
            actor: Some(lane.label.clone()),
        });
        return Some((lane_id, latest_result));
    }
    None
}

fn claim_executor_submit(
    writer: &mut CanonicalWriter,
    lane: &LaneConfig,
) -> Option<PendingExecutorSubmit> {
    let (lane_id, latest_verify_result) = claim_next_lane(writer, lane)?;
    let executor_display = format!("executor {}", lane.label);
    let executor_role = format!("executor[{}]", lane.label);
    Some(PendingExecutorSubmit {
        executor_name: "executor".to_string(),
        executor_display,
        lane_index: lane_id,
        label: lane.label.clone(),
        latest_verify_result,
        executor_role,
    })
}

/// Intent: transport_effect
/// Resource: error
/// Inputs: &app::PendingExecutorSubmit, &llm_runtime::config::LlmEndpoint, &llm_runtime::ws_server::WsBridge, &std::sync::Arc<tokio::sync::Mutex<llm_runtime::tab_management::TabSlotTable>>, bool, &str, u64
/// Outputs: {async fn body of app::submit_executor_turn()}
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
async fn submit_executor_turn(
    job: &PendingExecutorSubmit,
    endpoint: &LlmEndpoint,
    bridge: &WsBridge,
    tabs: &TabManagerHandle,
    send_system_prompt: bool,
    command_id: &str,
    response_timeout_secs: u64,
) -> Result<String> {
    let ready_tasks =
        crate::prompt_inputs::read_ready_tasks(&std::path::PathBuf::from(workspace()), 10);
    let restart_resume = peek_post_restart_result(&job.executor_role);
    let mut exec_prompt = if let Some(resume) = restart_resume.as_ref() {
        let prompt = restart_resume_banner(&job.executor_role, resume);
        let _ = take_post_restart_result(&job.executor_role);
        prompt
    } else {
        executor_cycle_prompt(
            job.executor_display.as_str(),
            job.label.as_str(),
            &job.latest_verify_result,
            &ready_tasks,
        )
    };
    if let Some(inbound) = take_external_user_message_without_writer("executor") {
        append_external_user_message_to_prompt(&mut exec_prompt, &inbound);
    } else if let Some(inbound) = take_inbound_message_without_writer("executor") {
        append_inbound_to_prompt(&mut exec_prompt, &inbound, "executor");
    }
    let executor_system = system_instructions(AgentPromptKind::Executor);
    let role_schema = if should_send_system_prompt(send_system_prompt, endpoint.stateful, 0) {
        executor_system
    } else {
        String::new()
    };
    let prompt = exec_prompt;
    eprintln!(
        "[{}] step=1 prompt_bytes={}",
        job.executor_role,
        prompt.len()
    );
    crate::logging::record_prompt_overflow(
        &std::path::PathBuf::from(workspace()),
        &job.executor_role,
        prompt.len(),
    );
    log_message_event(
        &job.executor_role,
        endpoint,
        "executor",
        1,
        command_id,
        "llm_request",
        json!({
            "submit_only": true,
            "prompt_bytes": prompt.len(),
            "role_schema_bytes": role_schema.len(),
            "prompt": truncate(&prompt, MAX_SNIPPET),
        }),
    );
    trace_message_forwarded(
        &job.executor_role,
        "executor",
        1,
        &endpoint.id,
        true,
        prompt.len(),
    );
    let raw = llm_worker_send_request_timeout(
        bridge,
        &endpoint.id,
        &endpoint.url,
        endpoint.stateful,
        &prompt,
        &role_schema,
        None,
        None,
        false,
        true,
        &job.executor_role,
        tabs,
        endpoint.max_tabs,
        true,
        Some(response_timeout_secs),
    )
    .await?;
    trace_message_received(
        &job.executor_role,
        "executor",
        1,
        &endpoint.id,
        true,
        raw.len(),
    );
    log_message_event(
        &job.executor_role,
        endpoint,
        "executor",
        1,
        command_id,
        "llm_response",
        json!({
            "submit_only": true,
            "response_bytes": raw.len(),
            "raw": truncate(&raw, MAX_SNIPPET),
        }),
    );
    if let Ok(mut ack) = serde_json::from_str::<Value>(&raw) {
        if ack.get("submit_ack").and_then(|v| v.as_bool()) == Some(true) {
            ack["command_id"] = Value::String(command_id.to_string());
            eprintln!("[{}] step=1 submit_ack={}", job.executor_role, raw);
            log_message_event(
                &job.executor_role,
                endpoint,
                "executor",
                1,
                command_id,
                "llm_submit_ack",
                ack.clone(),
            );
            append_orchestration_trace(
                "llm_message_processed",
                json!({
                    "role": job.executor_role,
                    "prompt_kind": "executor",
                    "step": 1,
                    "endpoint_id": endpoint.id,
                    "submit_ack": ack,
                }),
            );
        }
    }
    Ok(raw)
}

// ── Main ───────────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Resolve target workspace early so all subsystems see the same value.
    let workspace_override = args
        .windows(2)
        .find(|w| w[0] == "--workspace")
        .map(|w| w[1].clone());
    if let Some(ref path) = workspace_override {
        let p = std::path::Path::new(path);
        if !p.is_absolute() {
            bail!("--workspace must be an absolute path, got: {path}");
        }
        set_workspace(path.clone());
        eprintln!("[canon-mini-agent] workspace={path} (--workspace)");
    } else {
        let default_workspace = env!("CARGO_MANIFEST_DIR").to_string();
        set_workspace(default_workspace.clone());
        eprintln!(
            "[canon-mini-agent] workspace={} (default self)",
            default_workspace
        );
    }

    // Resolve agent state directory (canon-mini-agent's own runtime state).
    let state_dir_override = args
        .windows(2)
        .find(|w| w[0] == "--state-dir")
        .map(|w| w[1].clone());
    if let Some(ref path) = state_dir_override {
        let p = std::path::Path::new(path);
        if !p.is_absolute() {
            bail!("--state-dir must be an absolute path, got: {path}");
        }
        set_agent_state_dir(path.clone());
        eprintln!("[canon-mini-agent] state_dir={path} (--state-dir)");
    } else {
        eprintln!(
            "[canon-mini-agent] state_dir={} (default)",
            DEFAULT_AGENT_STATE_DIR
        );
    }

    let orchestrate = args.iter().any(|a| a == "--orchestrate");
    let start_role = args
        .windows(2)
        .find(|w| w[0] == "--start")
        .map(|w| w[1].as_str())
        .unwrap_or("executor");
    if !matches!(
        start_role,
        "executor" | "verifier" | "planner" | "solo"
    ) {
        bail!("invalid --start value: {start_role} (expected executor|verifier|planner|solo)");
    }
    let role_arg = args
        .windows(2)
        .find(|w| w[0] == "--role")
        .map(|w| w[1].as_str());
    let role_flags = ["--verifier", "--planner"];
    let has_role_flag = args.iter().any(|a| role_flags.contains(&a.as_str()));
    if role_arg.is_some() && has_role_flag {
        bail!("--role cannot be combined with --planner or --verifier");
    }
    if role_arg.is_some() && orchestrate {
        bail!("--role cannot be combined with --orchestrate");
    }

    let mut is_verifier = !orchestrate && args.iter().any(|a| a == "--verifier");
    let mut is_planner = !orchestrate && args.iter().any(|a| a == "--planner");
    let is_diagnostics = false;

    if let Some(role) = role_arg {
        match role {
            "executor" => {}
            "planner" => is_planner = true,
            "verifier" => is_verifier = true,
            _ => bail!("invalid --role value: {role} (expected executor|planner|verifier)"),
        }
    }
    let (ws_port, ws_port_explicit) = choose_ws_port(&args)?;
    if ws_port_explicit {
        eprintln!("[canon-mini-agent] ws_port={} (explicit)", ws_port);
    } else {
        eprintln!(
            "[canon-mini-agent] ws_port={} (auto-selected from {:?})",
            ws_port, WS_PORT_CANDIDATES
        );
    }

    let workspace = PathBuf::from(workspace());
    let spec_path = workspace.join(SPEC_FILE);
    let master_plan_path = workspace.join(MASTER_PLAN_FILE);
    let instance_id = find_flag_arg(&args, "--instance").map(str::to_string);
    let path_prefix = instance_id.clone().unwrap_or_else(|| "default".to_string());
    init_log_paths(&path_prefix);
    let planner_projection_rel = planner_projection_file_for_instance(&path_prefix);
    let planner_projection_path = workspace.join(&planner_projection_rel);
    let _ = PLANNER_PROJECTION_FILE_PATH.set(planner_projection_rel.clone());
    let legacy_diagnostics_rel = format!("PLANS/{}/diagnostics-{}.json", path_prefix, path_prefix);
    if let Err(err) = crate::logging::migrate_projection_if_present(
        &workspace,
        &legacy_diagnostics_rel,
        &planner_projection_rel,
        &planner_projection_rel,
        "baseline_planner_projection_legacy_migration",
    ) {
        eprintln!("[canon-mini-agent] planner projection migration failed: {err:#}");
    }
    if let Err(err) = ensure_objectives_and_invariants_json(&workspace) {
        log_startup_stage_error("objectives/invariants conversion", &err);
    }
    if let Err(err) = ensure_workspace_artifact_baseline(&workspace, &planner_projection_path) {
        log_startup_stage_error("workspace artifact bootstrap", &err);
    }
    if let Err(err) = crate::issues::sweep_stale_issues(&workspace) {
        log_startup_stage_error("issue staleness sweep", &err);
    }

    let shutdown = init_shutdown_signal();
    let shutdown_task = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown_task.flag.store(true, Ordering::SeqCst);
            shutdown_task.notify.notify_waiters();
        }
    });

    let endpoints = build_endpoints();
    let mut executor_endpoints: Vec<LlmEndpoint> = endpoints
        .iter()
        .filter(|e| e.role.as_deref() == Some("executor"))
        .cloned()
        .collect();
    executor_endpoints.sort_by(|a, b| a.id.cmp(&b.id));
    let lanes: Vec<LaneConfig> = executor_endpoints
        .into_iter()
        .enumerate()
        .map(|(index, ep)| LaneConfig {
            index,
            plan_file: lane_plan_file_for_instance(&path_prefix, &ep.id),
            label: ep.id.clone(),
            endpoint: ep,
            tabs: llm_worker_new_tabs(),
        })
        .collect();
    if lanes.is_empty() {
        bail!("no executor endpoints with role = \"executor\" found in constants");
    }
    let plans_dir = workspace.join("agent_state").join(&path_prefix);
    let _ = std::fs::create_dir_all(&plans_dir);
    if !planner_projection_path.exists() {
        let _ = std::fs::write(&planner_projection_path, "");
    }
    let _ = ensure_workspace_artifact_baseline(&workspace, &planner_projection_path);
    for lane in &lanes {
        let plan_path = workspace.join(&lane.plan_file);
        if plan_path.exists() {
            continue;
        }
        let legacy_json = workspace.join(format!("PLANS/executor-{}.json", lane.endpoint.id));
        let legacy_md = workspace.join(format!("PLANS/executor-{}.md", lane.endpoint.id));
        let contents = std::fs::read_to_string(&legacy_json)
            .or_else(|_| std::fs::read_to_string(&legacy_md))
            .unwrap_or_default();
        let contents = if contents.trim().is_empty() {
            "{}".to_string()
        } else {
            contents
        };
        if let Some(parent) = plan_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&plan_path, contents);
    }

    let ws_addr: std::net::SocketAddr = format!("127.0.0.1:{ws_port}").parse()?;
    let bridge = ws_server::spawn(
        ws_addr,
        DEFAULT_RESPONSE_TIMEOUT_SECS,
        DEFAULT_LLM_RETRY_COUNT,
        DEFAULT_LLM_RETRY_DELAY_SECS,
        Arc::<OnceLock<()>>::new(OnceLock::new()),
    );
    eprintln!("[canon-mini-agent] backend ready (chromium ws://127.0.0.1:{ws_port})");

    let tabs = llm_worker_new_tabs();

    if orchestrate {
        const SERVICE_POLL_MS: u64 = 500;
        const DEFAULT_PENDING_SUBMIT_TIMEOUT_MS: u64 = 120_000;
        const DEFAULT_SUBMITTED_TURN_TIMEOUT_MS: u64 = 240_000;
        let pending_submit_timeout_ms = std::env::var("CANON_PENDING_SUBMIT_TIMEOUT_MS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_PENDING_SUBMIT_TIMEOUT_MS);
        let submitted_turn_timeout_ms = std::env::var("CANON_SUBMITTED_TURN_TIMEOUT_MS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_SUBMITTED_TURN_TIMEOUT_MS);

        let solo_mode = start_role == "solo";
        let _ = std::fs::write(
            orchestrator_mode_flag_path(),
            if solo_mode {
                "single\n"
            } else {
                "orchestrate\n"
            },
        );

        eprintln!("[orchestrate] start_role={start_role}");

        let planner_ep = find_endpoint(&endpoints, "mini_planner")?.clone();
        let tabs_planner = llm_worker_new_tabs();
        let tabs_verify = llm_worker_new_tabs();

        // ── Canonical-writer initialisation ─────────────────────────────
        let lane_indices: Vec<usize> = lanes.iter().map(|l| l.index).collect();
        let initial_system_state = SystemState::new(&lane_indices, lanes.len());
        let tlog_path = PathBuf::from(crate::constants::agent_state_dir()).join("tlog.ndjson");
        let system_state = match Tlog::replay(&tlog_path, initial_system_state.clone()) {
            Ok(replayed) => replayed,
            Err(err) => {
                eprintln!(
                    "[orchestrate] failed to replay {}: {err:#}; using fresh initial state",
                    tlog_path.display()
                );
                initial_system_state.clone()
            }
        };
        let tlog = Tlog::open(&tlog_path);
        let mut writer = CanonicalWriter::try_new(system_state, tlog, workspace.clone())?;
        writer.apply(ControlEvent::OrchestratorModeSet {
            mode: if solo_mode {
                "single".to_string()
            } else {
                "orchestrate".to_string()
            },
        });
        if writer.state().objectives_json.trim().is_empty() {
            let (seed_path, seed_contents) = crate::objectives::load_bootstrap_objectives_seed(&workspace);
            writer.apply(ControlEvent::ObjectivesInitialized {
                source_path: seed_path.to_string_lossy().to_string(),
                hash: crate::logging::stable_hash_hex(&seed_contents),
                contents: seed_contents.clone(),
            });
            let _ = crate::objectives::reconcile_objectives_projection(
                &workspace,
                &seed_contents,
                "objectives_bootstrap_projection",
            );
        } else {
            let _ = crate::objectives::reconcile_objectives_projection(
                &workspace,
                &writer.state().objectives_json,
                "objectives_replay_projection_reconcile",
            );
        }
        let _ = crate::issues::reconcile_issues_projection(
            &workspace,
            "issues_replay_projection_reconcile",
        );
        let _ = crate::lessons::reconcile_lessons_projection(
            &workspace,
            "lessons_replay_projection_reconcile",
        );
        let _ = crate::invariants::reconcile_enforced_invariants_projection(
            &workspace,
            "enforced_invariants_replay_projection_reconcile",
        );
        if writer.tlog_seq() == 0 {
            writer.try_apply(ControlEvent::PlannerPendingSet { pending: true })?;
        }
        let mut rt = new_runtime_state(&lanes);

        let mut resume_verifier_items: Vec<ResumeVerifierItem> = Vec::new();
        let _solo_bootstrapped = false;
        let checkpoint = load_checkpoint(&workspace);
        let checkpoint_loaded = checkpoint.is_some();
        if let Some(checkpoint) = checkpoint {
            eprintln!(
                "[orchestrate] resume checkpoint loaded: phase={} lane={:?} age_ms={}",
                checkpoint.phase,
                checkpoint.phase_lane,
                now_ms().saturating_sub(checkpoint.created_ms)
            );
            resume_verifier_items = checkpoint.verifier_pending_results;
            let state = writer.state().clone();
            let mut resume_decision = decide_resume_phase(
                &state.phase,
                !resume_verifier_items.is_empty(),
                state.planner_pending,
                state.diagnostics_pending,
            );
            if !runtime_role_enabled(resume_decision.scheduled_phase.as_deref().unwrap_or("")) {
                resume_decision.scheduled_phase = None;
            }
            resume_decision.diagnostics_pending = false;
            if writer.state().scheduled_phase != resume_decision.scheduled_phase {
                writer.apply(ControlEvent::ScheduledPhaseSet {
                    phase: resume_decision.scheduled_phase.clone(),
                });
            }
            if writer.state().planner_pending != resume_decision.planner_pending {
                writer.apply(ControlEvent::PlannerPendingSet {
                    pending: resume_decision.planner_pending,
                });
            }
            if writer.state().diagnostics_pending != resume_decision.diagnostics_pending {
                writer.apply(ControlEvent::DiagnosticsPendingSet {
                    pending: resume_decision.diagnostics_pending,
                });
            }
            if let Err(err) = writer.try_record_effect(crate::events::EffectEvent::CheckpointLoaded {
                phase: state.phase.clone(),
            }) {
                eprintln!(
                    "[orchestrate] non-fatal: failed to record checkpoint_loaded effect: {err:#}"
                );
                log_error_event(
                    "orchestrate",
                    "checkpoint_loaded_effect_append",
                    None,
                    &format!(
                        "non-fatal checkpoint_loaded effect append failure (continuing): {err:#}"
                    ),
                    Some(json!({
                        "phase": state.phase.clone(),
                    })),
                );
            }
            // On resume, DO NOT clear executor_submit_inflight.
            // Clearing inflight state while preserving active tabs and submitted_turns
            // causes valid submit_ack events to lose their pending context, triggering
            // "submit ack without pending submit" and orphaning executor state.
            // We preserve executor_submit_inflight so late acks remain reconcilable.
            // Keep submitted_turns, tab_id_to_lane, and lane_active_tab intact
            // so late completions and active tabs can still be reconciled.
            rt.deferred_completions.clear();
            for lane in &lanes {
                writer.apply(ControlEvent::LanePromptInFlightSet {
                    lane_id: lane.index,
                    in_flight: false,
                });
                // Only clear the submit guard for lanes that have no live inflight entry.
                // If executor_submit_inflight still holds an entry for this lane, the
                // submit_joinset task is still running and will deliver an ack — clearing
                // the guard now would allow a second submit to be spawned while the first
                // is in flight, causing a double-submit and the "submit ack without pending
                // submit" error when the first ack arrives after the inflight map entry has
                // been overwritten by the second submit.
                if !rt.executor_submit_inflight.contains_key(&lane.index) {
                    writer.apply(ControlEvent::LaneSubmitInFlightSet {
                        lane_id: lane.index,
                        in_flight: false,
                    });
                }
            }
        }
        // Reset lanes that lost ownership (no active tab) — runs whether or not a checkpoint
        // loaded. Tlog replay can produce the same orphaned in_progress state when the process
        // was killed between tab release and turn registration (prompt_in_flight=false), which
        // the stale-lane cleanup below does not cover.
        {
            let lane_ids: Vec<usize> = writer.state().lanes.keys().copied().collect();
            for lane_id in lane_ids {
                let (in_progress, has_active_tab) = {
                    let s = writer.state();
                    let in_prog = s
                        .lanes
                        .get(&lane_id)
                        .and_then(|l| l.in_progress_by.as_ref())
                        .is_some();
                    let has_tab = s.lane_active_tab.contains_key(&lane_id);
                    (in_prog, has_tab)
                };
                if in_progress && !has_active_tab {
                    crate::blockers::record_action_failure_with_writer(
                        workspace.as_path(),
                        None,
                        "orchestrate",
                        "checkpoint_runtime_divergence",
                        &format!(
                            "checkpoint/runtime divergence: lane {} resumed in progress without an active tab and was requeued",
                            lanes[lane_id].label
                        ),
                        None,
                    );
                    writer.apply(ControlEvent::LaneInProgressSet {
                        lane_id,
                        actor: None,
                    });
                    writer.apply(ControlEvent::LanePendingSet {
                        lane_id,
                        pending: true,
                    });
                }
            }
        }
        // When the checkpoint was discarded (seq mismatch) with an existing tlog, the normal
        // resume path (decide_resume_phase) was skipped. If planner_pending is still false from
        // tlog replay the agent has no trigger to start working and will idle-poll forever.
        // Force the planner so work is always re-seeded after a discarded checkpoint.
        if !checkpoint_loaded && writer.tlog_seq() > 0 && !writer.state().planner_pending {
            eprintln!("[orchestrate] checkpoint discarded — seeding planner to avoid idle livelock");
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        }
        // Runtime/state reconciliation: after replay (especially when checkpoint resume is
        // discarded), canonical in-flight flags may survive without matching runtime objects.
        // Clear those stale flags so executor dispatch can progress instead of livelocking on
        // "all lanes busy".
        {
            let lane_ids: Vec<usize> = writer.state().lanes.keys().copied().collect();
            for lane_id in lane_ids {
                let submit_flag = writer.state().lane_submit_active(lane_id);
                let prompt_flag = writer.state().lane_in_flight(lane_id);
                let has_runtime_submit = rt.executor_submit_inflight.contains_key(&lane_id);
                let has_runtime_turn = rt.submitted_turns.values().any(|t| t.lane == lane_id);
                let has_state_turn = writer
                    .state()
                    .submitted_turn_ids
                    .values()
                    .any(|record| record.lane_id == lane_id);

                if submit_flag && !has_runtime_submit {
                    eprintln!(
                        "[orchestrate] stale-flag recovery: lane {} submit_in_flight had no runtime submit; clearing",
                        lane_id
                    );
                    writer.apply(ControlEvent::LaneSubmitInFlightSet {
                        lane_id,
                        in_flight: false,
                    });
                }
                if prompt_flag && !has_runtime_turn && !has_state_turn {
                    eprintln!(
                        "[orchestrate] stale-flag recovery: lane {} prompt_in_flight had no submitted turn; clearing",
                        lane_id
                    );
                    writer.apply(ControlEvent::LanePromptInFlightSet {
                        lane_id,
                        in_flight: false,
                    });
                }
            }
        }
        // When checkpoint resume is discarded (seq mismatch), runtime joinsets are empty and we
        // cannot safely continue replayed submitted_turn_ids from a prior process. Purge them so
        // stale-lane recovery can requeue lanes instead of staying phantom-busy forever.
        if !checkpoint_loaded && !writer.state().submitted_turn_ids.is_empty() {
            let stale_keys: Vec<String> = writer.state().submitted_turn_ids.keys().cloned().collect();
            eprintln!(
                "[orchestrate] checkpoint discarded — dropping {} stale submitted turn ids",
                stale_keys.len()
            );
            for key in stale_keys {
                let Some((tab_str, turn_str)) = key.split_once(':') else {
                    continue;
                };
                let Ok(tab_id) = tab_str.parse::<u32>() else {
                    continue;
                };
                let Ok(turn_id) = turn_str.parse::<u64>() else {
                    continue;
                };
                writer.apply(ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id });
            }
        }
        // Stale-lane cleanup: runs regardless of whether a checkpoint loaded.
        // After tlog replay, a lane may be marked in_progress + prompt_in_flight
        // but have no corresponding submitted_turn_ids — this happens when the
        // process was killed mid-step before the turn was registered or completed.
        // Those lanes are phantom-busy and must be reset so the wake signal can
        // route work to them.
        {
            let lane_ids: Vec<usize> = writer.state().lanes.keys().copied().collect();
            for lane_id in &lane_ids {
                let (in_progress, prompt_in_flight, submit_in_flight) = {
                    let s = writer.state();
                    let in_prog = s
                        .lanes
                        .get(lane_id)
                        .and_then(|l| l.in_progress_by.as_ref())
                        .is_some();
                    let in_flight = s.lane_in_flight(*lane_id);
                    let submit_flight = s.lane_submit_active(*lane_id);
                    (in_prog, in_flight, submit_flight)
                };
                let has_submitted_turn = writer
                    .state()
                    .submitted_turn_ids
                    .values()
                    .any(|r| r.lane_id == *lane_id);
                if in_progress && prompt_in_flight && !has_submitted_turn {
                    eprintln!(
                        "[orchestrate] stale-lane recovery: lane {} was in_progress+prompt_in_flight with no submitted turn; requeuing",
                        lane_id
                    );
                    writer.apply(ControlEvent::LanePromptInFlightSet {
                        lane_id: *lane_id,
                        in_flight: false,
                    });
                    writer.apply(ControlEvent::LaneInProgressSet {
                        lane_id: *lane_id,
                        actor: None,
                    });
                    writer.apply(ControlEvent::LanePendingSet {
                        lane_id: *lane_id,
                        pending: true,
                    });
                }
                // Additional stale-lane recovery: lane is marked in_progress but
                // has neither prompt-submit inflight flags nor submitted turns.
                // This state is not claimable by claim_next_lane() and can lock
                // executor phase forever after checkpoint resume.
                if in_progress && !prompt_in_flight && !submit_in_flight && !has_submitted_turn {
                    eprintln!(
                        "[orchestrate] stale-lane recovery: lane {} was in_progress with no inflight work; requeuing",
                        lane_id
                    );
                    writer.apply(ControlEvent::LaneInProgressSet {
                        lane_id: *lane_id,
                        actor: None,
                    });
                    writer.apply(ControlEvent::LanePendingSet {
                        lane_id: *lane_id,
                        pending: true,
                    });
                }
            }
        }
        // Resume hardening: if we loaded a checkpoint into executor phase but no
        // executor work is runnable after cleanup, force planner reseed.
        if checkpoint_loaded {
            let executor_phase = writer.state().phase == "executor";
            let submitted_turns_present = !writer.state().submitted_turn_ids.is_empty();
            let any_lane_runnable = lanes.iter().any(|lane| {
                writer
                    .state()
                    .lanes
                    .get(&lane.index)
                    .map(|ls| ls.pending && ls.in_progress_by.is_none())
                    .unwrap_or(false)
            });
            if executor_phase && !submitted_turns_present && !any_lane_runnable {
                eprintln!(
                    "[orchestrate] resume checkpoint had executor phase but no runnable lane work; reseeding planner"
                );
                writer.apply(ControlEvent::PhaseSet {
                    phase: "planner".to_string(),
                    lane: None,
                });
                writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            }
        }

        let mut planner_bootstrapped = false;
        let _diagnostics_bootstrapped = false;
        let _verifier_bootstrapped = false;
        let mut submit_joinset: tokio::task::JoinSet<(
            usize,
            PendingExecutorSubmit,
            Result<String>,
        )> = tokio::task::JoinSet::new();
        let mut continuation_joinset: tokio::task::JoinSet<ContinuationJoinOutput> =
            tokio::task::JoinSet::new();
        let mut verifier_joinset: tokio::task::JoinSet<(usize, String)> =
            tokio::task::JoinSet::new();
        let mut verifier_pending_results: VecDeque<(SubmittedExecutorTurn, u64, String)> =
            VecDeque::new();

        if !resume_verifier_items.is_empty() {
            eprintln!(
                "[orchestrate] resuming {} verifier items from checkpoint",
                resume_verifier_items.len()
            );
            for item in resume_verifier_items.drain(..) {
                if let Some(lane) = lanes.get(item.lane_id) {
                    let submitted = SubmittedExecutorTurn {
                        tab_id: 0,
                        lane: item.lane_id,
                        lane_label: lane.label.clone(),
                        command_id: "resume".to_string(),
                        started_ms: now_ms(),
                        actor: "executor".to_string(),
                        endpoint_id: lane.endpoint.id.clone(),
                        tabs: tabs_verify.clone(),
                        steps_used: 0,
                    };
                    verifier_pending_results.push_back((submitted, 0, item.final_exec_result));
                }
            }
        } else if let Some(item) = recover_verifier_item_from_executor_post_restart(&lanes) {
            eprintln!(
                "[orchestrate] recovered verifier item from executor post-restart result: lane={}",
                item.lane_label
            );
            if let Some(lane) = lanes.get(item.lane_id) {
                let submitted = SubmittedExecutorTurn {
                    tab_id: 0,
                    lane: item.lane_id,
                    lane_label: lane.label.clone(),
                    command_id: "resume".to_string(),
                    started_ms: now_ms(),
                    actor: "executor".to_string(),
                    endpoint_id: lane.endpoint.id.clone(),
                    tabs: tabs_verify.clone(),
                    steps_used: 0,
                };
                verifier_pending_results.push_back((submitted, 0, item.final_exec_result));
            }
        }

        eprintln!("[orchestrate] pipeline started: planner -> background executors -> verifier/planner-projection -> planner");

        const STALL_CYCLE_THRESHOLD: u32 = 5;
        const EXECUTOR_STALL_PROBE_MS: u64 = 5_000;
        const EXECUTOR_STALL_STALE_MS: u64 = 45_000;
        const EXECUTOR_STALL_RECOVERY_COOLDOWN_MS: u64 = 30_000;
        const IDLE_PULSE_COOLDOWN_MS: u64 = 5_000;
        const INLINE_UNIFIED_AGENT: bool = true;
        let mut stall_count: u32 = 0;
        let mut last_executor_stall_probe_ms: u64 = 0;
        let mut last_executor_stall_recovery_ms: u64 = 0;
        let mut last_idle_pulse_ts_ms: u64 = 0;
        let mut last_idle_boundary_hash: Option<u64> = None;
        loop {
            let _ = std::fs::remove_file(cycle_idle_marker_path());
            let mut cycle_progress = false;
            let objectives_hash_before =
                crate::logging::stable_hash_hex(&writer.state().objectives_json);
            let plan_mtime_before = file_modified_ms(&master_plan_path);
            let control_surfaces = [
                "system_state",
                "active_blocker",
                "verifier_pending_results",
                "verifier_joinset",
            ];
            if shutdown.flag.load(Ordering::SeqCst) {
                eprintln!("[orchestrate] shutdown requested; saving checkpoint");
                if let Err(err) =
                    save_checkpoint(&workspace, &mut writer, &lanes, &verifier_pending_results)
                {
                    eprintln!("[orchestrate] checkpoint save failed: {err:#}");
                    log_error_event(
                        "orchestrate",
                        "checkpoint",
                        None,
                        &format!("checkpoint save failed: {err:#}"),
                        None,
                    );
                }
                return Ok(());
            }

            apply_wake_signals(&mut writer);

            if writer.state().scheduled_phase.is_none() && writer.state().phase == "bootstrap" {
                if let Some(phase) = decide_bootstrap_phase(start_role) {
                    let phase = if runtime_role_enabled(&phase) {
                        phase
                    } else {
                        "planner".to_string()
                    };
                    eprintln!(
                        "[orchestrate] bootstrap_start_role: role={} scheduled_phase=None",
                        phase
                    );
                    if phase == "planner" {
                        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
                    }
                    let mut bootstrap_phase = phase.clone();
                    if phase == "executor" {
                        let ready_tasks_text =
                            crate::prompt_inputs::read_ready_tasks(workspace.as_path(), 1);
                        let ready_tasks_exist = ready_tasks_text != "(no ready tasks)";
                        let lanes_seeded = lanes.iter().any(|lane| {
                            let lane_state = writer.state().lanes.get(&lane.index);
                            lane_state.map(|state| state.pending).unwrap_or(false)
                                || lane_state
                                    .and_then(|state| state.in_progress_by.as_ref())
                                    .is_some()
                                || writer.state().lane_in_flight(lane.index)
                                || writer.state().lane_submit_active(lane.index)
                        });
                        let has_any_wake_signal = !writer.state().wake_signals_pending.is_empty();
                        if ready_tasks_exist && !lanes_seeded {
                            eprintln!(
                                "[orchestrate] bootstrap executor rerouted to planner: ready tasks exist but no lane work is seeded"
                            );
                            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
                            bootstrap_phase = "planner".to_string();
                        } else if !has_any_wake_signal && !lanes_seeded {
                            eprintln!(
                                "[orchestrate] bootstrap executor rerouted to planner: no pending wake signals and no seeded lanes"
                            );
                            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
                            bootstrap_phase = "planner".to_string();
                        }
                    }

                    writer.apply(ControlEvent::PhaseSet {
                        phase: bootstrap_phase,
                        lane: None,
                    });
                }
            }

            apply_diagnostics_pending_if_changed(&mut writer, false);
            let scheduled_phase = writer.state().scheduled_phase.clone();
            if scheduled_phase
                .as_deref()
                .is_some_and(|phase| !runtime_role_enabled(phase))
            {
                apply_scheduled_phase_if_changed(&mut writer, None);
            }

            let active_blocker = writer.state().active_blocker_to_verifier;
            if INLINE_UNIFIED_AGENT {
                // Single operational loop with internal plan->execute phases.
                apply_planner_pending_if_changed(&mut writer, true);
                apply_scheduled_phase_if_changed(&mut writer, None);
            } else {
                let blocker_decision = crate::state_space::ActiveBlockerDecision {
                    planner_pending: writer.state().planner_pending,
                    scheduled_phase: sanitize_phase_for_runtime(
                        writer.state().scheduled_phase.as_deref(),
                    ),
                };
                let planner_suppression_changes_state = blocker_decision.planner_pending
                    != writer.state().planner_pending
                    || blocker_decision.scheduled_phase.as_deref()
                        != writer.state().scheduled_phase.as_deref();
                if active_blocker
                    && planner_suppression_changes_state
                    && (writer.state().planner_pending
                        || writer.state().scheduled_phase.as_deref() == Some("planner"))
                {
                    eprintln!("[orchestrate] planner paused: active blocker to verifier");
                    crate::blockers::record_action_failure_with_writer(
                        workspace.as_path(),
                        Some(&mut writer),
                        "orchestrate",
                        "runtime_control_bypass",
                        "runtime-only control influence: semantic verifier-blocker state suppressed planner dispatch",
                        None,
                    );
                }
                apply_planner_pending_if_changed(&mut writer, blocker_decision.planner_pending);
                apply_scheduled_phase_if_changed(
                    &mut writer,
                    blocker_decision.scheduled_phase.as_deref(),
                );
            }

            let state_hash_before = cycle_control_hash(&ControlConvergenceSnapshot {
                state: writer.state(),
                active_blocker,
                verifier_pending: !verifier_pending_results.is_empty(),
                verifier_running: !verifier_joinset.is_empty(),
            });

            let semantic_control = SemanticControlState::new(
                writer.state().scheduled_phase.clone(),
                writer.state().planner_pending,
                writer.state().diagnostics_pending,
                writer.state().active_blocker_to_verifier,
            )
            .with_verifier_activity(
                !verifier_pending_results.is_empty(),
                !verifier_joinset.is_empty(),
            );
            let mut phase_gates = semantic_control.phase_gates();
            phase_gates.verifier = false;
            phase_gates.diagnostics = false;
            phase_gates.solo = false;

            let orchestrator_ctx = OrchestratorContext {
                lanes: &lanes,
                workspace: &workspace,
                bridge: &bridge,
                tabs_planner: &tabs_planner,
                planner_ep: &planner_ep,
                master_plan_path: &master_plan_path,
            };
            let cargo_test_failures = load_cargo_test_failures(&workspace);

            let runtime_executor_busy = !rt.submitted_turns.is_empty()
                || !rt.executor_submit_inflight.is_empty()
                || !submit_joinset.is_empty()
                || !continuation_joinset.is_empty();
            let run_planner_inline = if INLINE_UNIFIED_AGENT {
                !runtime_executor_busy
            } else {
                phase_gates.planner
            };
            if run_planner_inline {
                writer.apply(ControlEvent::PhaseSet {
                    phase: "planner".to_string(),
                    lane: None,
                });
                if run_planner_phase(
                    &orchestrator_ctx,
                    &mut writer,
                    &mut planner_bootstrapped,
                    &cargo_test_failures,
                )
                .await
                {
                    cycle_progress = true;
                }
            }

            let now = now_ms();
            let run_executor_inline = if INLINE_UNIFIED_AGENT {
                !active_blocker
            } else {
                phase_gates.executor
            };
            if run_executor_inline {
                if run_executor_phase(
                    &orchestrator_ctx,
                    &mut writer,
                    &mut rt,
                    now,
                    pending_submit_timeout_ms,
                    submitted_turn_timeout_ms,
                    &mut submit_joinset,
                ) {
                    cycle_progress = true;
                }
            }

            if process_completed_turns(
                &orchestrator_ctx,
                &mut writer,
                &mut rt,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            )
            .await
            {
                cycle_progress = true;
            }

            if drain_continuations(
                &mut writer,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            ) {
                cycle_progress = true;
            }

            if drain_deferred_completions(
                &orchestrator_ctx,
                &mut writer,
                &mut rt,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            ) {
                cycle_progress = true;
            }

            if !verifier_pending_results.is_empty() || !verifier_joinset.is_empty() {
                verifier_pending_results.clear();
                while verifier_joinset.try_join_next().is_some() {}
            }

            let semantic_control = SemanticControlState::new(
                writer.state().scheduled_phase.clone(),
                writer.state().planner_pending,
                writer.state().diagnostics_pending,
                writer.state().active_blocker_to_verifier,
            )
            .with_verifier_activity(
                !verifier_pending_results.is_empty(),
                !verifier_joinset.is_empty(),
            );
            let _ = semantic_control;

            if writer.state().scheduled_phase.is_some() {
                // Phase completion for executor must be based on aggregate lane activity,
                // not only `phase_lane` (which can legitimately be None between submits).
                // Using phase_lane-only creates None<->executor schedule thrash while
                // wake signals are pending and lanes are still busy.
                let executor_lane_pending = writer.state().lanes.values().any(|lane| lane.pending);
                let executor_in_progress = writer.state().lanes.values().any(|lane| {
                    lane.in_progress_by.is_some()
                }) || writer.state().lane_submit_in_flight.values().any(|&v| v)
                    || writer.state().lane_prompt_in_flight.values().any(|&v| v)
                    || !rt.executor_submit_inflight.is_empty()
                    || !rt.submitted_turns.is_empty()
                    || !submit_joinset.is_empty()
                    || !continuation_joinset.is_empty();
                let semantic_control = SemanticControlState::new(
                    writer.state().scheduled_phase.clone(),
                    writer.state().planner_pending,
                    writer.state().diagnostics_pending,
                    writer.state().active_blocker_to_verifier,
                )
                .with_verifier_activity(
                    !verifier_pending_results.is_empty(),
                    !verifier_joinset.is_empty(),
                );
                if semantic_control
                    .scheduled_phase_done(executor_lane_pending, executor_in_progress)
                {
                    apply_scheduled_phase_if_changed(&mut writer, None);
                }
            }

            // Event-driven executor deadlock recovery:
            // if canonical state says executor is still busy but runtime has no live
            // in-flight submit/turn objects, confirm via tlog progress signals and
            // force a planner handoff after a cooldown.
            let now = now_ms();
            let executor_flagged_busy = writer.state().phase == "executor"
                && (writer.state().lane_submit_in_flight.values().any(|&v| v)
                    || writer.state().lane_prompt_in_flight.values().any(|&v| v)
                    || writer
                        .state()
                        .lanes
                        .values()
                        .any(|lane| lane.in_progress_by.is_some()));
            let runtime_executor_busy = !rt.submitted_turns.is_empty()
                || !rt.executor_submit_inflight.is_empty()
                || !submit_joinset.is_empty()
                || !continuation_joinset.is_empty();
            if executor_flagged_busy
                && !runtime_executor_busy
                && now.saturating_sub(last_executor_stall_probe_ms) >= EXECUTOR_STALL_PROBE_MS
            {
                last_executor_stall_probe_ms = now;
                let signals = read_executor_progress_signals(workspace.as_path(), now);
                let progress_stale = signals
                    .last_progress_ts_ms
                    .map(|ts| now.saturating_sub(ts) > EXECUTOR_STALL_STALE_MS)
                    .unwrap_or(true);
                let divergence_hot = signals.checkpoint_divergence_blockers_recent >= 2;
                let recovery_cooldown_done = now.saturating_sub(last_executor_stall_recovery_ms)
                    >= EXECUTOR_STALL_RECOVERY_COOLDOWN_MS;

                if progress_stale && divergence_hot && recovery_cooldown_done {
                    crate::blockers::record_action_failure_with_writer(
                        workspace.as_path(),
                        Some(&mut writer),
                        "orchestrate",
                        "executor_stall_recovery",
                        &format!(
                            "executor deadlock recovered: stale executor progress with busy lane flags (last_progress_seq={:?}, divergence_blockers_recent={})",
                            signals.last_progress_seq,
                            signals.checkpoint_divergence_blockers_recent
                        ),
                        None,
                    );
                    for lane in &lanes {
                        let lane_busy = writer.state().lane_submit_active(lane.index)
                            || writer.state().lane_in_flight(lane.index)
                            || writer
                                .state()
                                .lanes
                                .get(&lane.index)
                                .and_then(|s| s.in_progress_by.as_ref())
                                .is_some();
                        if lane_busy {
                            writer.apply(ControlEvent::LaneSubmitInFlightSet {
                                lane_id: lane.index,
                                in_flight: false,
                            });
                            writer.apply(ControlEvent::LanePromptInFlightSet {
                                lane_id: lane.index,
                                in_flight: false,
                            });
                            writer.apply(ControlEvent::LaneInProgressSet {
                                lane_id: lane.index,
                                actor: None,
                            });
                            writer.apply(ControlEvent::LanePendingSet {
                                lane_id: lane.index,
                                pending: true,
                            });
                            writer.apply(ControlEvent::LaneNextSubmitAtSet {
                                lane_id: lane.index,
                                ms: 0,
                            });
                        }
                    }
                    apply_scheduled_phase_if_changed(&mut writer, None);
                    writer.apply(ControlEvent::PhaseSet {
                        phase: "planner".to_string(),
                        lane: None,
                    });
                    writer.apply(ControlEvent::PlannerPendingSet { pending: true });
                    last_executor_stall_recovery_ms = now;
                    cycle_progress = true;
                }
            }

            let objectives_hash_after =
                crate::logging::stable_hash_hex(&writer.state().objectives_json);
            let plan_mtime_after = file_modified_ms(&master_plan_path);
            let objective_review_required = plan_mtime_before != plan_mtime_after;
            let objectives_updated = objectives_hash_before != objectives_hash_after;
            let objectives_text = writer.state().objectives_json.clone();
            let plan_text = read_text_or_empty(&master_plan_path);
            let plan_content_changed = plan_text != writer.state().last_plan_text;
            if objective_review_required && !objectives_updated && plan_content_changed {
                append_orchestration_trace(
                    "objective_evolution_enforcement_signal",
                    json!({
                        "required_action": "objective_review_or_update_required",
                        "reason": "plan_changed_without_objective_update",
                        "plan_changed": plan_content_changed,
                        "objectives_updated": objectives_updated,
                        "objectives_path": OBJECTIVES_FILE,
                        "plan_path": MASTER_PLAN_FILE,
                    }),
                );
                /* trace-only: semantic control state owns planner follow-up */
            }

            let has_objective_work = has_actionable_objectives(&objectives_text);
            let has_plan_work = plan_has_incomplete_tasks(&plan_text);
            if has_objective_work && !has_plan_work {
                append_orchestration_trace(
                    "objective_plan_enforcement_signal",
                    json!({
                        "required_action": "plan_task_required_for_actionable_objective",
                        "reason": "objectives_require_work_but_plan_has_no_pending_tasks",
                        "objectives_path": OBJECTIVES_FILE,
                        "plan_path": MASTER_PLAN_FILE,
                    }),
                );
                writer.apply(ControlEvent::PlannerObjectivePlanGapQueued);
                cycle_progress = true;
            }

            // Convergence guard: detect cycles where work was dispatched but the
            // semantic control snapshot did not change. Consecutive stalls indicate
            // a livelock.
            //
            // Skip the stall increment when executor turns are still in flight: the
            // browser tab has accepted a submission (submitted_turns) or a submit is
            // being negotiated (executor_submit_inflight / lane_submit_in_flight).
            // In those cases the semantic control state may remain stable until the
            // result arrives; counting the cycle as a stall would be a false positive.
            let executor_inflight = !rt.submitted_turns.is_empty()
                || !rt.executor_submit_inflight.is_empty()
                || writer.state().lane_submit_in_flight.values().any(|&v| v);
            let active_blocker_after = writer.state().active_blocker_to_verifier;
            let state_hash_after = cycle_control_hash(&ControlConvergenceSnapshot {
                state: writer.state(),
                active_blocker: active_blocker_after,
                verifier_pending: !verifier_pending_results.is_empty(),
                verifier_running: !verifier_joinset.is_empty(),
            });
            if cycle_progress && state_hash_before == state_hash_after && !executor_inflight {
                stall_count += 1;
                eprintln!(
                    "[orchestrate] convergence: no net state change (stall {}/{})",
                    stall_count, STALL_CYCLE_THRESHOLD
                );
                if stall_count >= STALL_CYCLE_THRESHOLD {
                    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
                    write_livelock_report(
                        agent_state_dir,
                        stall_count,
                        &control_surfaces,
                        writer.state().planner_pending,
                        writer.state().diagnostics_pending,
                    );
                    apply_planner_pending_if_changed(&mut writer, false);
                    apply_diagnostics_pending_if_changed(&mut writer, false);
                    stall_count = 0;
                    cycle_progress = false;
                }
            } else {
                stall_count = 0;
            }

            if !cycle_progress {
                let idle_boundary_hash = state_hash_after;
                let idle_now = now_ms();
                let boundary_changed = last_idle_boundary_hash != Some(idle_boundary_hash);
                let cooldown_elapsed =
                    idle_now.saturating_sub(last_idle_pulse_ts_ms) >= IDLE_PULSE_COOLDOWN_MS;
                if boundary_changed || cooldown_elapsed {
                    writer.apply(ControlEvent::OrchestratorIdlePulse { ts_ms: idle_now });
                    let _ = std::fs::write(cycle_idle_marker_path(), "idle\n");
                    last_idle_pulse_ts_ms = idle_now;
                    last_idle_boundary_hash = Some(idle_boundary_hash);
                }
                tokio::time::sleep(std::time::Duration::from_millis(SERVICE_POLL_MS)).await;
            }
        }
    } else {
        // Single-role mode
        let _ = std::fs::write(orchestrator_mode_flag_path(), "single\n");
        let single_role_ctx = SingleRoleContext {
            workspace: &workspace,
            spec_path: &spec_path,
            master_plan_path: &master_plan_path,
        };
        let (inputs, endpoint) = load_single_role_setup(
            &single_role_ctx,
            &endpoints,
            is_verifier,
            is_diagnostics,
            is_planner,
        )?;
        let instructions = system_instructions(inputs.prompt_kind);
        eprintln!(
            "[canon-mini-agent] role={} input loaded ({} bytes)",
            inputs.role,
            inputs.primary_input.len()
        );
        eprintln!(
            "[canon-mini-agent] endpoint id={} url={}",
            endpoint.id,
            endpoint.pick_url(0)
        );

        let lane_indices: Vec<usize> = lanes.iter().map(|l| l.index).collect();
        let initial_system_state = SystemState::new(&lane_indices, lanes.len());
        let tlog_path = PathBuf::from(crate::constants::agent_state_dir()).join("tlog.ndjson");
        let system_state = Tlog::replay(&tlog_path, initial_system_state.clone())
            .unwrap_or(initial_system_state);
        let mut writer = CanonicalWriter::new(system_state, Tlog::open(&tlog_path), workspace.clone());
        writer.apply(ControlEvent::OrchestratorModeSet {
            mode: "single".to_string(),
        });

        let cargo_test_failures = load_cargo_test_failures(&workspace);
        let initial_prompt =
            build_single_role_prompt(&single_role_ctx, &inputs, &cargo_test_failures)?;

        let submit_only = inputs.role == "executor";
        let reason = run_agent(
            inputs.role.as_str(),
            canonical_role_label(inputs.role.as_str()),
            &instructions,
            initial_prompt,
            if inputs.role == "executor" {
                &lanes[0].endpoint
            } else {
                &endpoint
            },
            &bridge,
            &workspace,
            &tabs,
            Some(&mut writer),
            None,
            submit_only,
            inputs.role == "executor",
            true,
            0,
        )
        .await?;
        writer.apply(ControlEvent::OrchestratorIdlePulse { ts_ms: now_ms() });
        let _ = std::fs::write(cycle_idle_marker_path(), "idle\n");
        println!("message: {}", reason.into_summary());
        Ok(())
    }
}

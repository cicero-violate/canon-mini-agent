//
use anyhow::{anyhow, bail, Context, Result};
use canon_llm::{
    config::LlmEndpoint,
    endpoint_worker::{
        llm_worker_new_tabs, llm_worker_send_request_timeout, llm_worker_send_request_with_req_id_timeout,
    },
    tab_management::TabManagerHandle,
    ws_server,
    ws_server::WsBridge,
};
use serde_json::{json, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};
use tokio::sync::Notify;

use crate::engine::process_action_and_execute;
use crate::logging::{
    append_action_log_record, append_orchestration_trace, compact_log_record, init_log_paths,
    log_action_result, log_message_event, make_command_id, now_ms,
};
use crate::prompts::{
    action_observation, action_rationale, action_result_prompt, diagnostics_cycle_prompt,
    diagnostics_python_reads_event_logs, executor_cycle_prompt, is_explicit_idle_action,
    normalize_action, parse_actions, planner_cycle_prompt, single_role_solo_prompt, system_instructions,
    truncate, validate_action, verifier_cycle_prompt, AgentPromptKind,
};
use crate::invalid_action::{
    auto_fill_message_fields, build_invalid_action_feedback, corrective_invalid_action_prompt,
    default_message_route, expected_message_format,
};
use crate::state_space::{
    allow_diagnostics_run, allow_verifier_run, block_executor_dispatch, check_completion_endpoint,
    check_completion_tab, decide_active_blocker, decide_bootstrap_phase, decide_phase_gates,
    decide_post_diagnostics, decide_resume_phase, decide_wake_flags, executor_step_limit_exceeded,
    executor_submit_timed_out, is_verifier_specific_blocker, scheduled_phase_resume_done,
    should_force_blocker, verifier_blocker_phase_override, CargoTestGate, CompletionEndpointCheck,
    CompletionTabCheck, WakeFlagInput,
};
use crate::constants::{
    DEFAULT_AGENT_STATE_DIR, DEFAULT_RESPONSE_TIMEOUT_SECS,
    DIAGNOSTICS_FILE_PATH, ENDPOINT_SPECS, EXECUTOR_STEP_LIMIT, INVARIANTS_FILE, MASTER_PLAN_FILE,
    MAX_SNIPPET, MAX_STEPS, OBJECTIVES_FILE, ROLE_TIMEOUT_SECS, SPEC_FILE, VIOLATIONS_FILE,
    WS_PORT_CANDIDATES,
    set_agent_state_dir, set_workspace, workspace,
};
use crate::md_convert::ensure_objectives_and_invariants_json;
use crate::prompt_inputs::{
    build_single_role_prompt, lane_summary_text, load_planner_inputs, load_single_role_inputs,
    load_verifier_prompt_inputs, read_required_text, read_text_or_empty, LaneConfig,
    OrchestratorContext, PlannerInputs, SingleRoleContext, SingleRoleInputs, VerifierPromptInputs,
};

/// Extract a string field from a JSON object, returning `""` on missing/non-string.
fn jstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn ws_port_arg(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|w| w[0] == "--port")
        .map(|w| w[1].as_str())
}

fn instance_arg(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|w| w[0] == "--instance")
        .map(|w| w[1].as_str())
}

fn ws_port_is_available(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn choose_ws_port(args: &[String]) -> Result<(u16, bool)> {
    if let Some(raw) = ws_port_arg(args) {
        let port = raw
            .parse::<u16>()
            .with_context(|| format!("invalid --port value: {raw}"))?;
        return Ok((port, true));
    }

    for &port in WS_PORT_CANDIDATES {
        if ws_port_is_available(port) {
            return Ok((port, false));
        }
    }

    bail!(
        "no free ws port available in {:?}; pass --port explicitly or extend WS_PORT_CANDIDATES",
        WS_PORT_CANDIDATES
    );
}

fn role_key(role: &str) -> &str {
    if role.starts_with("executor") {
        "executor"
    } else if role == "solo" {
        "solo"
    } else {
        role
    }
}

fn response_timeout_for_role(role: &str) -> u64 {
    ROLE_TIMEOUT_SECS
        .iter()
        .find(|(key, _)| *key == role_key(role))
        .map(|(_, val)| *val)
        .unwrap_or(DEFAULT_RESPONSE_TIMEOUT_SECS)
}

fn summarize_cargo_test_failures(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let mut out = serde_json::Map::new();
    for key in [
        "error_locations",
        "failed_tests",
        "stalled_tests",
        "failure_block",
        "rerun_hint",
    ] {
        if let Some(v) = value.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(out).to_string()
}

fn load_cargo_test_failures(workspace: &Path) -> String {
    let path = workspace.join("cargo_test_failures.json");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    summarize_cargo_test_failures(&raw)
}

fn load_single_role_setup(
    ctx: &SingleRoleContext<'_>,
    endpoints: &[LlmEndpoint],
    is_verifier: bool,
    is_diagnostics: bool,
    is_planner: bool,
) -> Result<(SingleRoleInputs, LlmEndpoint)> {
    let inputs = load_single_role_inputs(ctx, is_verifier, is_diagnostics, is_planner)?;
    let endpoint = find_endpoint(endpoints, inputs.role.as_str())?.clone();
    Ok((inputs, endpoint))
}

fn trace_message_forwarded(
    role: &str,
    prompt_kind: &str,
    step: usize,
    endpoint_id: &str,
    submit_only: bool,
    prompt_bytes: usize,
) {
    append_orchestration_trace(
        "llm_message_forwarded",
        json!({
            "role": role,
            "prompt_kind": prompt_kind,
            "step": step,
            "endpoint_id": endpoint_id,
            "submit_only": submit_only,
            "prompt_bytes": prompt_bytes,
        }),
    );
}

fn trace_message_received(
    role: &str,
    prompt_kind: &str,
    step: usize,
    endpoint_id: &str,
    submit_only: bool,
    response_bytes: usize,
) {
    append_orchestration_trace(
        "llm_message_received",
        json!({
            "role": role,
            "prompt_kind": prompt_kind,
            "step": step,
            "endpoint_id": endpoint_id,
            "submit_only": submit_only,
            "response_bytes": response_bytes,
        }),
    );
}

fn trace_orchestrator_forwarded(
    from: &str,
    to: &str,
    phase: &str,
    lane_name: Option<&str>,
    lane_plan_file: Option<&str>,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
) {
    let mut payload = serde_json::Map::new();
    payload.insert("from".to_string(), Value::String(from.to_string()));
    payload.insert("to".to_string(), Value::String(to.to_string()));
    payload.insert("phase".to_string(), Value::String(phase.to_string()));
    if let Some(lane_name) = lane_name {
        payload.insert("lane_name".to_string(), Value::String(lane_name.to_string()));
    }
    if let Some(lane_plan_file) = lane_plan_file {
        payload.insert(
            "lane_plan_file".to_string(),
            Value::String(lane_plan_file.to_string()),
        );
    }
    if let Some(tab_id) = tab_id {
        payload.insert("tab_id".to_string(), Value::Number(tab_id.into()));
    }
    if let Some(turn_id) = turn_id {
        payload.insert("turn_id".to_string(), Value::Number(turn_id.into()));
    }
    append_orchestration_trace("llm_message_forwarded", Value::Object(payload));
}

struct BlockerFields {
    blocker_text: String,
    required_action: String,
    evidence: String,
    blocker_display: String,
    severity: String,
}

fn normalize_blocker_fields(payload: &Value) -> BlockerFields {
    let blocker_text = jstr(payload, "blocker").to_string();
    let required_action = jstr(payload, "required_action").to_string();
    let evidence = jstr(payload, "evidence").to_string();
    let severity_raw = jstr(payload, "severity");
    let severity = if severity_raw.is_empty() {
        "error".to_string()
    } else {
        severity_raw.to_string()
    };
    let blocker_display = if blocker_text.is_empty() {
        "upstream blocker".to_string()
    } else {
        blocker_text.clone()
    };
    BlockerFields {
        blocker_text,
        required_action,
        evidence,
        blocker_display,
        severity,
    }
}

fn build_blocker_payload(
    summary: &str,
    blocker: &str,
    evidence: &str,
    required_action: &str,
    severity: &str,
) -> Value {
    json!({
        "summary": summary,
        "blocker": blocker,
        "evidence": evidence,
        "required_action": required_action,
        "severity": severity,
    })
}

fn build_verifier_blocker_ack(fields: &BlockerFields) -> Value {
    json!({
        "action": "message",
        "from": "verifier",
        "to": "planner",
        "type": "blocker",
        "status": "blocked",
        "observation": "Inbound blocker received; verifier yielding without further work until resolved.",
        "rationale": "Blocker is not verifier-specific; pausing verification avoids unnecessary work.",
        "payload": build_blocker_payload(
            "Verifier paused due to upstream blocker.",
            &fields.blocker_display,
            &fields.evidence,
            &fields.required_action,
            &fields.severity,
        )
    })
}

async fn run_planner_phase(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    verifier_summary: &[String],
    planner_bootstrapped: &mut bool,
    cargo_test_failures: &str,
) -> bool {
    let inputs: PlannerInputs = load_planner_inputs(
        ctx.lanes,
        ctx.workspace,
        verifier_summary,
        &dispatch_state.last_plan_text,
        &mut dispatch_state.last_executor_diff,
        cargo_test_failures.to_string(),
        ctx.violations_path,
        ctx.diagnostics_path,
        ctx.master_plan_path,
    );
    let mut planner_prompt = planner_cycle_prompt(
        &inputs.summary_text,
        &inputs.objectives_text,
        &inputs.invariants_text,
        &inputs.violations_text,
        &inputs.diagnostics_text,
        &inputs.plan_diff_text,
        &inputs.executor_diff_text,
        &inputs.cargo_test_failures,
    );
    inject_inbound_message(&mut planner_prompt, "planner");
    trace_orchestrator_forwarded("orchestrator", "planner", "planner", None, None, None, None);
    let planner_system = system_instructions(AgentPromptKind::Planner);
    let result = run_agent(
        "planner",
        "planner",
        &planner_system,
        planner_prompt,
        ctx.planner_ep,
        ctx.bridge,
        ctx.workspace,
        ctx.tabs_planner,
        false,
        false,
        !*planner_bootstrapped,
        0,
    )
    .await;
    *planner_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!("[orchestrate] planner ok bytes={}", result.len());
            dispatch_state.last_plan_text = inputs.plan_text;
            for lane in ctx.lanes {
                let lane_state = dispatch_lane_mut(dispatch_state, lane.index);
                lane_state.plan_text.clear();
                if lane_state.in_progress_by.is_none()
                    && !verifier_confirmed(&lane_state.latest_verifier_result)
                {
                    lane_state.pending = true;
                }
            }
            dispatch_state.planner_pending = false;
            true
        }
        Err(err) => {
            eprintln!("[orchestrate] planner error: {err:#}");
            false
        }
    }
}

async fn run_solo_phase(
    ctx: &OrchestratorContext<'_>,
    solo_bootstrapped: &mut bool,
    cargo_test_failures: &str,
) -> bool {
    let spec = match read_required_text(ctx.workspace.join(SPEC_FILE), SPEC_FILE) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("[orchestrate] solo error: {err:#}");
            return false;
        }
    };
    let master_plan = read_text_or_empty(ctx.master_plan_path);
    let agent_root = crate::constants::agent_state_dir().trim_end_matches("/agent_state");
    let agent_objectives = Path::new(agent_root).join(OBJECTIVES_FILE);
    let objectives = if agent_objectives.exists() {
        read_text_or_empty(agent_objectives)
    } else {
        read_text_or_empty(ctx.workspace.join(OBJECTIVES_FILE))
    };
    let invariants = read_text_or_empty(ctx.workspace.join(INVARIANTS_FILE));
    let violations = read_text_or_empty(ctx.violations_path);
    let diagnostics = read_text_or_empty(ctx.diagnostics_path);
    let mut prompt = single_role_solo_prompt(
        &spec,
        &master_plan,
        &objectives,
        &invariants,
        &violations,
        &diagnostics,
        cargo_test_failures,
    );
    inject_inbound_message(&mut prompt, "solo");
    trace_orchestrator_forwarded("orchestrator", "solo", "solo", None, None, None, None);
    let solo_system = system_instructions(AgentPromptKind::Solo);
    let result = run_agent(
        "solo",
        "solo",
        &solo_system,
        prompt,
        ctx.solo_ep,
        ctx.bridge,
        ctx.workspace,
        ctx.tabs_solo,
        false,
        true,
        !*solo_bootstrapped,
        0,
    )
    .await;
    *solo_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!("[orchestrate] solo ok bytes={}", result.len());
            true
        }
        Err(err) => {
            eprintln!("[orchestrate] solo error: {err:#}");
            false
        }
    }
}

async fn run_diagnostics_phase(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    verifier_summary: &[String],
    diagnostics_bootstrapped: &mut bool,
    verifier_changed: bool,
    cargo_test_failures: &str,
) -> bool {
    let summary_text = lane_summary_text(ctx.lanes, verifier_summary);
    let mut prompt = diagnostics_cycle_prompt(&summary_text, cargo_test_failures);
    inject_inbound_message(&mut prompt, "diagnostics");
    trace_orchestrator_forwarded("verifier", "diagnostics", "diagnostics", None, None, None, None);
    let diagnostics_system = system_instructions(AgentPromptKind::Diagnostics);
    let result = run_agent(
        "diagnostics",
        "diagnostics",
        &diagnostics_system,
        prompt,
        ctx.diagnostics_ep,
        ctx.bridge,
        ctx.workspace,
        ctx.tabs_diagnostics,
        false,
        false,
        !*diagnostics_bootstrapped,
        0,
    )
    .await;
    *diagnostics_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!("[orchestrate] diagnostics ok bytes={}", result.len());
            let new_diagnostics_text = read_text_or_empty(ctx.diagnostics_path);
            let diagnostics_changed = dispatch_state.diagnostics_text != new_diagnostics_text;
            dispatch_state.diagnostics_text = new_diagnostics_text;
            dispatch_state.diagnostics_pending = false;
            dispatch_state.planner_pending =
                decide_post_diagnostics(diagnostics_changed, verifier_changed);
            true
        }
        Err(err) => {
            eprintln!("[orchestrate] diagnostics error: {err:#}");
            false
        }
    }
}

async fn run_verifier_phase(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    verifier_summary: &mut [String],
    verifier_joinset: &mut tokio::task::JoinSet<(usize, String)>,
    verifier_bootstrapped: &mut bool,
    scheduled_phase: &mut Option<String>,
    current_phase: &mut String,
    current_phase_lane: &mut Option<usize>,
    cargo_test_failures: &str,
) -> (bool, bool) {
    let mut cycle_progress = false;
    let mut verifier_changed = false;
    while let Some((submitted, turn_id, final_exec_result)) = verifier_pending_results.pop_front() {
        if !allow_verifier_run(scheduled_phase.as_deref()) {
            verifier_pending_results.push_front((submitted, turn_id, final_exec_result));
            break;
        }
        *current_phase = "verifier".to_string();
        *current_phase_lane = Some(submitted.lane);
        let lane_plan_file = ctx.lanes[submitted.lane].plan_file.clone();
        let prompt_inputs: VerifierPromptInputs = load_verifier_prompt_inputs(
            ctx.lanes,
            ctx.workspace,
            verifier_summary,
            &mut dispatch_state.last_executor_diff,
            cargo_test_failures.to_string(),
        );
        let mut verifier_prompt = verifier_cycle_prompt(
            submitted.lane_label.as_str(),
            &final_exec_result,
            &prompt_inputs.executor_diff_text,
            &prompt_inputs.cargo_test_failures,
        );
        if let Some(inbound) = take_inbound_message("verifier") {
            if let Some((_, to, payload)) = try_parse_blocker(&inbound) {
                let fields = normalize_blocker_fields(&payload);
                let verifier_specific =
                    is_verifier_specific_blocker(&fields.blocker_text, &fields.required_action);
                if to.eq_ignore_ascii_case("verifier")
                    && verifier_blocker_phase_override(verifier_specific).is_some()
                {
                    let ack = build_verifier_blocker_ack(&fields);
                    persist_planner_message(&ack);
                    verifier_pending_results.push_front((submitted, turn_id, final_exec_result));
                    let override_phase = verifier_blocker_phase_override(verifier_specific).unwrap();
                    *scheduled_phase = Some(override_phase.to_string());
                    continue;
                }
            }
            append_inbound_to_prompt(&mut verifier_prompt, &inbound);
        } else if let Some(inbound) = extract_message_action(&final_exec_result) {
            append_inbound_to_prompt(&mut verifier_prompt, &inbound);
        }
        trace_orchestrator_forwarded(
            &format!("executor:{}", submitted.lane_label),
            "verifier",
            "verifier",
            Some(submitted.lane_label.as_str()),
            Some(lane_plan_file.as_str()),
            Some(submitted.tab_id),
            Some(turn_id),
        );
        let verifier_system = system_instructions(AgentPromptKind::Verifier);
        let verifier_ep = ctx.verifier_ep.clone();
        let bridge = ctx.bridge.clone();
        let workspace = ctx.workspace.to_path_buf();
        let send_system = !*verifier_bootstrapped;
        *verifier_bootstrapped = true;
        let tabs_verify = ctx.tabs_verify.clone();
        verifier_joinset.spawn(async move {
            let verify_result = match run_agent(
                "verifier",
                "verifier",
                &verifier_system,
                verifier_prompt,
                &verifier_ep,
                &bridge,
                &workspace,
                &tabs_verify,
                false,
                false,
                send_system,
                0,
            )
            .await
            {
                Ok(result) => result,
                Err(err) => format!(
                    "{{\"verified\":false,\"summary\":\"verifier error: {}\"}}",
                    err.to_string().replace('"', "'")
                ),
            };
            (submitted.lane, verify_result)
        });
        cycle_progress = true;
    }

    while let Some(joined) = verifier_joinset.try_join_next() {
        match joined {
            Ok((lane_id, verify_result)) => {
                if verify_result.trim().eq_ignore_ascii_case("shutdown requested") {
                    eprintln!(
                        "[orchestrate] verifier shutdown marker received; preserving previous verifier result"
                    );
                    cycle_progress = true;
                    continue;
                }
                let lane = dispatch_lane_mut(dispatch_state, lane_id);
                let changed = lane.latest_verifier_result != verify_result;
                lane.latest_verifier_result = verify_result.clone();
                lane.in_progress_by = None;
                lane.pending = !verifier_confirmed(&verify_result);
                if changed {
                    verifier_summary[lane_id] = verify_result;
                    verifier_changed = true;
                }
                cycle_progress = true;
            }
            Err(err) => {
                eprintln!("[orchestrate] verifier join error: {err:#}");
            }
        }
    }

    (cycle_progress, verifier_changed)
}

fn run_executor_phase(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    now: u64,
    pending_submit_timeout_ms: u64,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
    scheduled_phase: Option<&str>,
    current_phase: &mut String,
    current_phase_lane: &mut Option<usize>,
) -> bool {
    let mut cycle_progress = false;
    if !dispatch_state.executor_submit_inflight.is_empty() {
        let mut timed_out = Vec::new();
        for (lane_id, pending) in dispatch_state.executor_submit_inflight.iter() {
            if executor_submit_timed_out(pending.started_ms, now, pending_submit_timeout_ms) {
                timed_out.push(*lane_id);
            }
        }
        for lane_id in timed_out {
            if let Some(pending) = dispatch_state.executor_submit_inflight.remove(&lane_id) {
                eprintln!(
                    "[orchestrate] pending submit timeout: lane={} command_id={}",
                    ctx.lanes[lane_id].label,
                    pending.command_id
                );
                append_orchestration_trace(
                    "executor_submit_timeout",
                    json!({
                        "lane_name": ctx.lanes[lane_id].label,
                        "command_id": pending.command_id,
                    }),
                );
            }
            dispatch_state.lane_submit_in_flight.insert(lane_id, false);
            let lane = dispatch_lane_mut(dispatch_state, lane_id);
            lane.in_progress_by = None;
            lane.pending = true;
        }
    }

    if !block_executor_dispatch(scheduled_phase) {
        for lane in ctx.lanes {
            if dispatch_state.lane_submit_active(lane.index)
                || dispatch_state.lane_next_submit_ms(lane.index) > now
            {
                continue;
            }
            if let Some(job) = claim_executor_submit(dispatch_state, lane) {
                *current_phase = "executor".to_string();
                *current_phase_lane = Some(lane.index);
                let lane_index = lane.index;
                let endpoint = lane.endpoint.clone();
                let bridge = ctx.bridge.clone();
                let tabs = lane.tabs.clone();
                let command_id = make_command_id(&job.executor_role, "executor", 1);
                let response_timeout_secs = response_timeout_for_role(&job.executor_role);
                dispatch_state.executor_submit_inflight.insert(
                    lane_index,
                    PendingSubmitState {
                        job: job.clone(),
                        started_ms: now_ms(),
                        command_id: command_id.clone(),
                        endpoint_id: endpoint.id.clone(),
                        tabs: tabs.clone(),
                    },
                );
                dispatch_state.lane_submit_in_flight.insert(lane_index, true);
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
        }
    }

    while let Some(joined) = submit_joinset.try_join_next() {
        match joined {
            Ok((lane_id, job, result)) => {
                match result {
                    Ok(exec_result) => {
                        if let Some((tab_id, turn_id, command_id)) = parse_submit_ack(&exec_result) {
                            let Some(pending) = dispatch_state.executor_submit_inflight.remove(&lane_id) else {
                                eprintln!(
                                    "[orchestrate] submit ack without pending submit: lane={} tab_id={} turn_id={}",
                                    ctx.lanes[lane_id].label,
                                    tab_id,
                                    turn_id
                                );
                                continue;
                            };
                            if executor_submit_timed_out(
                                pending.started_ms,
                                now_ms(),
                                pending_submit_timeout_ms,
                            ) {
                                eprintln!(
                                    "[orchestrate] submit ack arrived after timeout: lane={} tab_id={} turn_id={}",
                                    ctx.lanes[lane_id].label,
                                    tab_id,
                                    turn_id
                                );
                                dispatch_state.lane_submit_in_flight.insert(lane_id, false);
                                dispatch_state.lane_prompt_in_flight.insert(lane_id, false);
                                continue;
                            }
                            if let Some(active_tab) = dispatch_state.lane_active_tab(lane_id) {
                                if active_tab != tab_id {
                                    eprintln!(
                                        "[orchestrate] submit ack tab mismatch: lane={} active_tab={} ack_tab={} (overwriting active tab)",
                                        ctx.lanes[lane_id].label,
                                        active_tab,
                                        tab_id
                                    );
                                }
                            }
                            dispatch_state.lane_active_tab.insert(lane_id, tab_id);
                            dispatch_state
                                .tab_id_to_lane
                                .entry(tab_id)
                                .or_insert(lane_id);
                            dispatch_state.submitted_turns.insert(
                                (tab_id, turn_id),
                                SubmittedExecutorTurn {
                                    tab_id,
                                    lane: job.lane_index,
                                    lane_label: job.label.clone(),
                                    command_id: command_id.unwrap_or_else(|| pending.command_id.clone()),
                                    actor: job.executor_role.clone(),
                                    endpoint_id: pending.endpoint_id.clone(),
                                    tabs: pending.tabs.clone(),
                                    steps_used: dispatch_state.lane_steps_used(job.lane_index),
                                },
                            );
                            dispatch_state.lane_next_submit_at_ms.insert(lane_id, now_ms());
                            dispatch_state.lane_submit_in_flight.insert(lane_id, false);
                            cycle_progress = true;
                        } else {
                            eprintln!("[orchestrate] {} missing submit_ack (preserving lane ownership): {exec_result}", job.executor_name);
                            let lane = dispatch_lane_mut(dispatch_state, job.lane_index);
                            // Recovery: clear stuck ownership and requeue lane
                            lane.in_progress_by = None;
                            lane.pending = true;
                            dispatch_state.executor_submit_inflight.remove(&job.lane_index);
                            dispatch_state.lane_submit_in_flight.insert(job.lane_index, false);
                        }
                    }
                    Err(err) => {
                        eprintln!("[orchestrate] {} submit error (preserving lane ownership): {err:#}", job.executor_name);
                        let lane = dispatch_lane_mut(dispatch_state, job.lane_index);
                        // Recovery: clear stuck ownership and requeue lane
                        lane.in_progress_by = None;
                        lane.pending = true;
                        dispatch_state.executor_submit_inflight.remove(&job.lane_index);
                        dispatch_state.lane_submit_in_flight.insert(job.lane_index, false);
                    }
                }
            }
            Err(err) => {
                eprintln!("[orchestrate] submit join error: {err:#}");
            }
        }
    }

    cycle_progress
}

async fn process_completed_turns(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    continuation_joinset: &mut tokio::task::JoinSet<(SubmittedExecutorTurn, u64, Result<String>)>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    let completed_turns = ctx.bridge.take_completed_turns().await;
    for item in completed_turns {
        append_orchestration_trace("llm_message_received", item.clone());
        let Some((tab_id, turn_id, exec_result, completed_endpoint_id)) = parse_completed_turn(&item) else {
            continue;
        };
        let submitted = if let Some(submitted) =
            dispatch_state.submitted_turns.remove(&(tab_id, turn_id))
        {
            if check_completion_endpoint(&submitted.endpoint_id, completed_endpoint_id.as_deref())
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
                continue;
            }
            submitted
        } else {
            let lane_id = dispatch_state.tab_id_to_lane.get(&tab_id).copied();
            let Some(lane_id) = lane_id else {
                append_orchestration_trace(
                    "executor_completion_unmatched",
                    json!({
                        "tab_id": tab_id,
                        "turn_id": turn_id,
                        "text": truncate(&exec_result, MAX_SNIPPET),
                    }),
                );
                continue;
            };
            if check_completion_endpoint(&ctx.lanes[lane_id].endpoint.id, completed_endpoint_id.as_deref())
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
                continue;
            }
            match check_completion_tab(
                dispatch_state.lane_active_tab(lane_id),
                tab_id,
            ) {
                CompletionTabCheck::Mismatch => {
                    append_orchestration_trace(
                        "executor_completion_tab_mismatch",
                        json!({
                            "lane_name": ctx.lanes[lane_id].label,
                            "active_tab": dispatch_state.lane_active_tab(lane_id),
                            "tab_id": tab_id,
                            "turn_id": turn_id,
                        }),
                    );
                    continue;
                }
                CompletionTabCheck::NoneSet => {
                    dispatch_state.lane_active_tab.insert(lane_id, tab_id);
                }
                CompletionTabCheck::Ok => {}
            }
            let Some(pending) = dispatch_state.executor_submit_inflight.remove(&lane_id) else {
                append_orchestration_trace(
                    "executor_completion_unmatched",
                    json!({
                        "tab_id": tab_id,
                        "turn_id": turn_id,
                        "text": truncate(&exec_result, MAX_SNIPPET),
                    }),
                );
                continue;
            };
            dispatch_state.lane_submit_in_flight.insert(lane_id, false);
            dispatch_state.lane_next_submit_at_ms.insert(lane_id, now_ms());
            SubmittedExecutorTurn {
                tab_id,
                lane: lane_id,
                lane_label: ctx.lanes[lane_id].label.clone(),
                command_id: pending.command_id,
                actor: pending.job.executor_role,
                endpoint_id: pending.endpoint_id,
                tabs: pending.tabs,
                steps_used: dispatch_state.lane_steps_used(lane_id),
            }
        };
        dispatch_state.lane_prompt_in_flight.insert(submitted.lane, false);
        if handle_executor_completion(
            submitted,
            tab_id,
            turn_id,
            exec_result,
            dispatch_state,
            ctx.lanes,
            ctx.bridge,
            ctx.workspace,
            continuation_joinset,
            verifier_pending_results,
        ) {
            cycle_progress = true;
        }
    }
    cycle_progress
}

fn drain_continuations(
    dispatch_state: &mut DispatchState,
    continuation_joinset: &mut tokio::task::JoinSet<(SubmittedExecutorTurn, u64, Result<String>)>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    while let Some(joined) = continuation_joinset.try_join_next() {
        match joined {
            Ok((submitted, turn_id, result)) => match result {
                Ok(final_exec_result) => {
                    dispatch_state.lane_prompt_in_flight.insert(submitted.lane, false);
                    // Continuations only return once the executor has reached completion,
                    // and the returned value is the completion summary (not the raw action JSON).
                    verifier_pending_results.push_back((submitted, turn_id, final_exec_result));
                    cycle_progress = true;
                }
                Err(err) => {
                    eprintln!(
                        "[orchestrate] executor continuation error: lane={} err={err:#}",
                        submitted.lane_label
                    );
                    dispatch_state.lane_prompt_in_flight.insert(submitted.lane, false);
                    let lane = dispatch_lane_mut(dispatch_state, submitted.lane);
                    lane.in_progress_by = None;
                    lane.pending = true;
                    cycle_progress = true;
                }
            },
            Err(err) => {
                eprintln!("[orchestrate] continuation join error: {err:#}");
            }
        }
    }
    cycle_progress
}

fn drain_deferred_completions(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    continuation_joinset: &mut tokio::task::JoinSet<(SubmittedExecutorTurn, u64, Result<String>)>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    for lane_id in 0..ctx.lanes.len() {
        if dispatch_state.lane_in_flight(lane_id) {
            continue;
        }
        while let Some(deferred) = dispatch_state
            .deferred_completions
            .get_mut(&lane_id)
            .and_then(|queue| queue.pop_front())
        {
            if handle_executor_completion(
                deferred.submitted,
                deferred.tab_id,
                deferred.turn_id,
                deferred.exec_result,
                dispatch_state,
                ctx.lanes,
                ctx.bridge,
                ctx.workspace,
                continuation_joinset,
                verifier_pending_results,
            ) {
                cycle_progress = true;
            }
            if dispatch_state.lane_in_flight(lane_id) {
                break;
            }
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
        "diagnostics"
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
        "Repeated failures detected. You cannot proceed without external action.\nReturn exactly one action that reports a blocker using this schema:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"{from}\",\n  \"to\": \"{to}\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Summarize the blocked state based on evidence.\",\n  \"rationale\": \"Explain why you cannot proceed.\",\n  \"payload\": {{\n    \"summary\": \"Short blocker summary.\",\n    \"blocker\": \"Root cause that prevents progress.\",\n    \"evidence\": \"{evidence}\",\n    \"required_action\": \"What must be fixed to continue.\",\n    \"severity\": \"error\"\n  }}\n}}\n```\nTask context:\n{context}\nReturn exactly one action.",
        evidence = truncate(last_error, MAX_SNIPPET),
        context = truncate(task_context, MAX_SNIPPET),
    )
}

#[derive(Clone)]
struct ShutdownSignal {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

static SHUTDOWN_SIGNAL: OnceLock<ShutdownSignal> = OnceLock::new();

fn init_shutdown_signal() -> ShutdownSignal {
    SHUTDOWN_SIGNAL
        .get_or_init(|| ShutdownSignal {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        })
        .clone()
}

fn shutdown_signal() -> Option<ShutdownSignal> {
    SHUTDOWN_SIGNAL.get().cloned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CheckpointLane {
    lane_id: usize,
    lane_label: String,
    plan_text: String,
    pending: bool,
    in_progress_by: Option<String>,
    latest_verifier_result: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResumeVerifierItem {
    lane_id: usize,
    lane_label: String,
    lane_plan_file: String,
    final_exec_result: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrchestratorCheckpoint {
    #[serde(default)]
    workspace: String,
    created_ms: u64,
    phase: String,
    phase_lane: Option<usize>,
    planner_pending: bool,
    diagnostics_pending: bool,
    diagnostics_text: String,
    last_plan_text: String,
    last_executor_diff: String,
    lanes: Vec<CheckpointLane>,
    verifier_summary: Vec<String>,
    verifier_pending_results: Vec<ResumeVerifierItem>,
}

fn checkpoint_path(_workspace: &Path) -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir()).join("mini_agent_checkpoint.json")
}

fn cycle_idle_marker_path() -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir()).join("orchestrator_cycle_idle.flag")
}

fn save_checkpoint(
    workspace: &Path,
    phase: &str,
    phase_lane: Option<usize>,
    dispatch_state: &DispatchState,
    lanes: &[LaneConfig],
    verifier_summary: &[String],
    verifier_pending_results: &VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> Result<()> {
    let mut lane_snapshots = Vec::new();
    for lane in lanes {
        if let Some(state) = dispatch_state.lanes.get(&lane.index) {
            lane_snapshots.push(CheckpointLane {
                lane_id: lane.index,
                lane_label: lane.label.clone(),
                plan_text: state.plan_text.clone(),
                pending: state.pending,
                in_progress_by: state.in_progress_by.clone(),
                latest_verifier_result: state.latest_verifier_result.clone(),
            });
        }
    }
    let mut resume_items = Vec::new();
    for (submitted, _turn_id, final_exec_result) in verifier_pending_results.iter() {
        resume_items.push(ResumeVerifierItem {
            lane_id: submitted.lane,
            lane_label: submitted.lane_label.clone(),
            lane_plan_file: lanes
                .get(submitted.lane)
                .map(|lane| lane.plan_file.clone())
                .unwrap_or_default(),
            final_exec_result: final_exec_result.clone(),
        });
    }
    let checkpoint = OrchestratorCheckpoint {
        workspace: workspace.to_string_lossy().into_owned(),
        created_ms: now_ms(),
        phase: phase.to_string(),
        phase_lane,
        planner_pending: dispatch_state.planner_pending,
        diagnostics_pending: dispatch_state.diagnostics_pending,
        diagnostics_text: dispatch_state.diagnostics_text.clone(),
        last_plan_text: dispatch_state.last_plan_text.clone(),
        last_executor_diff: dispatch_state.last_executor_diff.clone(),
        lanes: lane_snapshots,
        verifier_summary: verifier_summary.to_vec(),
        verifier_pending_results: resume_items,
    };
    let path = checkpoint_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, serde_json::to_string_pretty(&checkpoint)?)?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

fn load_checkpoint(workspace: &Path) -> Option<OrchestratorCheckpoint> {
    let path = checkpoint_path(workspace);
    let raw = std::fs::read_to_string(path).ok()?;
    let cp: OrchestratorCheckpoint = serde_json::from_str(&raw).ok()?;
    if cp.workspace.is_empty() || cp.workspace != workspace.to_string_lossy().as_ref() {
        eprintln!(
            "[orchestrate] checkpoint workspace mismatch (stored={} current={}) — discarding",
            cp.workspace,
            workspace.display()
        );
        return None;
    }
    Some(cp)
}

fn looks_like_diff(raw: &str) -> bool {
    raw.contains("diff --git")
        || (raw.contains("--- ") && raw.contains("+++ "))
        || raw.contains("@@ ")
        || raw.contains("@@ -")
}

fn guardrail_action_from_raw(raw: &str, role: &str) -> Option<Value> {
    if raw.contains("assistant reaction-only terminal frame:") {
        let path = if role == "diagnostics" {
            "state/event_log/event.tlog.d"
        } else {
            "canon-utils"
        };
        return Some(json!({
            "action": "list_dir",
            "observation": "Received reaction-only response; forcing a concrete discovery action.",
            "rationale": "Reaction-only responses are invalid; gather fresh evidence instead.",
            "path": path
        }));
    }
    if looks_like_diff(raw) {
        let (from, to, msg_type, status) = default_message_route(role);
        return Some(json!({
            "action": "message",
            "from": from,
            "to": to,
            "type": msg_type,
            "status": status,
            "observation": "Model responded with diff-only text; wrapping as message payload.",
            "rationale": "Diff output must be wrapped in a valid message action.",
            "payload": {
                "summary": "diff-only output captured",
                "diff_excerpt": truncate(raw, 1500),
                "expected_format": expected_message_format(from, to, msg_type, status)
            }
        }));
    }
    None
}

fn apply_error_result(
    role: &str,
    task_context: &str,
    error_streak: &mut usize,
    last_error: &mut Option<String>,
    last_result: &mut Option<String>,
    err_text: &str,
    default_result: String,
) {
    *error_streak = error_streak.saturating_add(1);
    *last_error = Some(err_text.to_string());
    *last_result = Some(default_result);
    if should_force_blocker(*error_streak) {
        *last_result = Some(blocker_escalation_prompt(
            role,
            last_error.as_deref().unwrap_or(err_text),
            task_context,
        ));
    }
}

struct InvalidActionFeedback {
    err_text: String,
    feedback: String,
}

fn parse_action_from_raw(
    role: &str,
    endpoint: &LlmEndpoint,
    prompt_kind: &str,
    step: usize,
    exchange_id: &str,
    raw: &str,
    allow_guardrail: bool,
    allow_auto_fill_message: bool,
    trace_context: Option<(&str, u32)>,
) -> Result<Value, InvalidActionFeedback> {
    let log = |event: &str, data: Value| {
        log_message_event(role, endpoint, prompt_kind, step, exchange_id, event, data);
    };
    let trace = |status: &str| {
        if let Some((lane_label, tab_id)) = trace_context {
            append_orchestration_trace(
                "executor_tool_result_forwarded",
                json!({ "lane_name": lane_label, "tab_id": tab_id, "turn_id": exchange_id, "status": status }),
            );
        }
    };

    let actions = match parse_actions(raw) {
        Ok(a) => a,
        Err(e) => {
            if allow_guardrail {
                if let Some(guard_action) = guardrail_action_from_raw(raw, role) {
                    log("llm_guardrail_action", json!({
                        "error": e.to_string(), "raw": truncate(raw, MAX_SNIPPET), "action": guard_action,
                    }));
                    return Ok(guard_action);
                }
            }
            eprintln!("[{role}] step={} parse_error: {e}", step);
            log("llm_parse_error", json!({ "error": e.to_string(), "raw": truncate(raw, MAX_SNIPPET) }));
            trace("parse_error");
            return Err(InvalidActionFeedback {
                err_text: e.to_string(),
                feedback: build_invalid_action_feedback(None, &e.to_string(), role),
            });
        }
    };

    if actions.len() != 1 {
        let msg = format!("Got {} actions — emit exactly one action per turn.", actions.len());
        eprintln!("[{role}] step={} {msg}", step);
        log("llm_invalid_action_count", json!({ "action_count": actions.len(), "raw": truncate(raw, MAX_SNIPPET) }));
        trace("invalid_action_count");
        return Err(InvalidActionFeedback {
            err_text: msg.clone(),
            feedback: build_invalid_action_feedback(None, &msg, role),
        });
    }

    let mut action = actions[0].clone();
    let raw_action = action.clone();
    if let Err(e) = normalize_action(&mut action) {
        log("llm_invalid_action", json!({
            "stage": "normalize_action", "error": e.to_string(), "raw": truncate(raw, MAX_SNIPPET),
        }));
        return Err(InvalidActionFeedback {
            err_text: e.to_string(),
            feedback: build_invalid_action_feedback(Some(&raw_action), &e.to_string(), role),
        });
    }

    if allow_auto_fill_message {
        auto_fill_message_fields(&mut action, role);
    }

    if let Err(e) = validate_action(&action) {
        log("llm_invalid_action", json!({
            "stage": "validate_action", "error": e.to_string(),
            "raw": truncate(raw, MAX_SNIPPET), "action": action.clone(),
        }));
        let err_text = e.to_string();
        if let Some(prompt) = corrective_invalid_action_prompt(&action, &err_text, role) {
            return Err(InvalidActionFeedback {
                err_text: err_text.clone(),
                feedback: format!(
                    "{}\n\n{}",
                    build_invalid_action_feedback(Some(&action), &err_text, role),
                    prompt
                ),
            });
        }
        if err_text.contains("cargo_test missing 'crate'") {
            return Err(InvalidActionFeedback {
                err_text: err_text.clone(),
                feedback: format!(
                    "Invalid action: {e}\nCorrective action required: `cargo_test` must include a `crate` field.\nUse this exact format and fill in the crate name:\n```json\n{{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-runtime\",\n  \"observation\": \"Running canon-runtime test suite after latest changes.\",\n  \"rationale\": \"Validate that canon-runtime tests pass for the updated parser logic.\"\n}}\n```\nReturn exactly one action."
                ),
            });
        }
        return Err(InvalidActionFeedback {
            err_text: err_text.clone(),
            feedback: build_invalid_action_feedback(Some(&action), &err_text, role),
        });
    }

    Ok(action)
}

fn build_agent_prompt(
    role: &str,
    send_system_prompt: bool,
    step: usize,
    initial_prompt: &str,
    system_instructions: &str,
    last_result: Option<&str>,
    last_tab_id: Option<u32>,
    last_turn_id: Option<u64>,
    last_action: Option<&str>,
    total_steps: usize,
) -> (String, String) {
    if step == 0 {
        (
            if send_system_prompt {
                system_instructions.to_string()
            } else {
                String::new()
            },
            initial_prompt.to_string(),
        )
    } else {
        let result = last_result.unwrap_or("").to_string();
        let agent_type = role_key(role).to_uppercase();
        (
            String::new(),
            action_result_prompt(
                last_tab_id,
                last_turn_id,
                agent_type.as_str(),
                &result,
                last_action,
                if role.starts_with("executor") {
                    Some(total_steps)
                } else {
                    None
                },
            ),
        )
    }
}

fn enforce_executor_step_limit(
    role: &str,
    total_steps: usize,
    error_streak: &mut usize,
    last_result: &mut Option<String>,
) -> bool {
    if role.starts_with("executor") && executor_step_limit_exceeded(total_steps, EXECUTOR_STEP_LIMIT)
    {
        *error_streak = error_streak.saturating_add(1);
        *last_result = Some(format!(
            "Executor exceeded {EXECUTOR_STEP_LIMIT} actions without handoff. You must send a `message` action to planner (handoff or blocker) now."
        ));
        return true;
    }
    false
}

fn enforce_diagnostics_python(
    role: &str,
    step: usize,
    kind: &str,
    action: &Value,
    diagnostics_eventlog_python_done: &mut bool,
) -> Option<String> {
    if role != "diagnostics" || *diagnostics_eventlog_python_done {
        return None;
    }
    if diagnostics_python_reads_event_logs(action) {
        *diagnostics_eventlog_python_done = true;
        return None;
    }
    if step == 0 {
        return Some(format!(
            "Diagnostics must begin with a `python` action that analyzes {}/state/event_log/event.tlog.d to diagnose problems, detect inconsistencies, and extract concrete failure signals.",
            workspace()
        ));
    }
    if matches!(kind, "apply_patch" | "message") {
        return Some(format!(
            "Before writing diagnostics or finishing, run a `python` action that analyzes {}/state/event_log/event.tlog.d to find errors, inconsistencies, invariant violations, repeated failure patterns, and concrete repair targets. Diagnostics is for finding what is broken.",
            workspace()
        ));
    }
    None
}


fn take_inbound_message(role: &str) -> Option<String> {
    let role_key = role.trim().to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let path = agent_state_dir.join(format!("last_message_to_{role_key}.json"));
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let _ = std::fs::remove_file(&path);
    Some(trimmed)
}

fn append_inbound_to_prompt(prompt: &mut String, inbound: &str) {
    prompt.push_str("\n\nInbound handoff message (raw JSON):\n");
    prompt.push_str(inbound);
    prompt.push('\n');
}

fn inject_inbound_message(prompt: &mut String, role: &str) {
    if let Some(inbound) = take_inbound_message(role) {
        append_inbound_to_prompt(prompt, &inbound);
    }
}

fn extract_message_action(raw: &str) -> Option<String> {
    let marker = "message_action:";
    let idx = raw.find(marker)?;
    let after = raw[idx + marker.len()..].trim_start();
    if after.is_empty() {
        return None;
    }
    let json_start = after.find('{')?;
    let json_text = after[json_start..].trim();
    if json_text.is_empty() {
        return None;
    }
    Some(json_text.to_string())
}

fn is_reaction_only_response(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("assistant reaction-only terminal frame") {
        return true;
    }
    if trimmed.starts_with("assistant reaction-only") {
        return true;
    }
    if trimmed.len() <= 8 && trimmed.chars().all(|c| !c.is_ascii_alphanumeric()) {
        return true;
    }
    false
}

fn apply_wake_flags(
    agent_state_dir: &std::path::Path,
    dispatch_state: &mut DispatchState,
    scheduled_phase: &mut Option<String>,
) {
    let active_blocker = agent_state_dir.join("active_blocker_to_verifier.json").exists();

    let flag_paths = [
        ("planner",     agent_state_dir.join("wakeup_planner.flag")),
        ("solo",        agent_state_dir.join("wakeup_solo.flag")),
        ("verifier",    agent_state_dir.join("wakeup_verifier.flag")),
        ("diagnostics", agent_state_dir.join("wakeup_diagnostics.flag")),
        ("executor",    agent_state_dir.join("wakeup_executor.flag")),
    ];

    let mut inputs: Vec<WakeFlagInput> = Vec::new();
    let mut path_map: std::collections::HashMap<&str, std::path::PathBuf> =
        std::collections::HashMap::new();
    for (role, path) in &flag_paths {
        if !path.exists() {
            continue;
        }
        let modified_ms = path
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as u64)
            .unwrap_or(0);
        inputs.push(WakeFlagInput { role, modified_ms });
        path_map.insert(role, path.clone());
    }

    let decision = decide_wake_flags(active_blocker, &inputs);
    let Some(role) = decision.scheduled_phase.as_deref() else {
        return;
    };

    *scheduled_phase = Some(role.to_string());
    if let Some(path) = path_map.get(role) {
        eprintln!(
            "[orchestrate] wake_flag_triggered: role={} path={}",
            role,
            path.display()
        );
        let _ = std::fs::remove_file(path);
    }
    if decision.planner_pending {
        dispatch_state.planner_pending = true;
    }
    if decision.diagnostics_pending {
        dispatch_state.diagnostics_pending = true;
    }
    if decision.executor_wake {
        for lane in dispatch_state.lanes.values_mut() {
            lane.pending = true;
            lane.in_progress_by = None;
        }
    }
}

fn try_parse_blocker(raw: &str) -> Option<(String, String, Value)> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let msg_type = jstr(&value, "type");
    let status = jstr(&value, "status");
    if msg_type != "blocker" || status != "blocked" {
        return None;
    }
    let from = jstr(&value, "from").to_string();
    let to = jstr(&value, "to").to_string();
    let payload = value.get("payload").cloned().unwrap_or_else(|| Value::Null);
    Some((from, to, payload))
}

fn persist_planner_message(action: &Value) {
    let agent_state_dir =
        std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    let planner_path = agent_state_dir.join("last_message_to_planner.json");
    let _ = std::fs::write(
        &planner_path,
        serde_json::to_string_pretty(action).unwrap_or_default(),
    );
    let _ = std::fs::write(agent_state_dir.join("wakeup_planner.flag"), "handoff");
}

struct LlmResponseContext<'a> {
    role: &'a str,
    endpoint: &'a LlmEndpoint,
    prompt_kind: &'a str,
    submit_only: bool,
}

impl<'a> LlmResponseContext<'a> {
    fn log_request(&self, step: usize, exchange_id: &str, prompt: &str, role_schema: &str) {
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_request",
            json!({
                "submit_only": self.submit_only,
                "prompt_bytes": prompt.len(),
                "role_schema_bytes": role_schema.len(),
                "prompt": truncate(prompt, MAX_SNIPPET),
            }),
        );
        trace_message_forwarded(
            self.role,
            self.prompt_kind,
            step,
            &self.endpoint.id,
            self.submit_only,
            prompt.len(),
        );
    }

    fn log_response(&self, step: usize, exchange_id: &str, raw: &str) {
        trace_message_received(
            self.role,
            self.prompt_kind,
            step,
            &self.endpoint.id,
            self.submit_only,
            raw.len(),
        );
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_response",
            json!({
                "submit_only": self.submit_only,
                "response_bytes": raw.len(),
                "raw": truncate(raw, MAX_SNIPPET),
            }),
        );
    }

    fn handle_submit_ack(
        &self,
        step: usize,
        exchange_id: &str,
        raw: &str,
    ) -> Option<String> {
        if !self.submit_only {
            return None;
        }
        if let Ok(mut ack) = serde_json::from_str::<Value>(raw) {
            if ack.get("submit_ack").and_then(|v| v.as_bool()) == Some(true) {
                ack["command_id"] = Value::String(exchange_id.to_string());
                eprintln!("[{}] step={} submit_ack={}", self.role, step, raw);
                log_message_event(
                    self.role,
                    self.endpoint,
                    self.prompt_kind,
                    step,
                    exchange_id,
                    "llm_submit_ack",
                    ack.clone(),
                );
                append_orchestration_trace(
                    "llm_message_processed",
                    json!({
                        "role": self.role,
                        "prompt_kind": self.prompt_kind,
                        "step": step,
                        "endpoint_id": self.endpoint.id,
                        "submit_ack": ack,
                    }),
                );
                return Some(ack.to_string());
            }
        }
        None
    }

    fn log_error(&self, step: usize, exchange_id: &str, error: &str) {
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_error",
            json!({
                "error": error,
            }),
        );
    }

    fn handle_reaction_only(
        &self,
        step: usize,
        exchange_id: &str,
        raw: &str,
        reaction_only_streak: &mut usize,
        error_streak: &mut usize,
        last_error: &mut Option<String>,
    ) -> bool {
        if !is_reaction_only_response(raw) {
            return false;
        }
        *reaction_only_streak = reaction_only_streak.saturating_add(1);
        log_message_event(
            self.role,
            self.endpoint,
            self.prompt_kind,
            step,
            exchange_id,
            "llm_reaction_only",
            json!({
                "raw": truncate(raw, MAX_SNIPPET),
            }),
        );
        if !should_force_blocker(*reaction_only_streak) {
            *error_streak = error_streak.saturating_add(1);
            *last_error = Some("reaction_only_response".to_string());
            eprintln!(
                "[{}] step={} reaction_only_response retry {}",
                self.role,
                step,
                *reaction_only_streak
            );
        }
        true
    }
}

async fn continue_executor_completion(
    submitted: &SubmittedExecutorTurn,
    active_tab_id: u32,
    completion_text: &str,
    turn_id: u64,
    endpoint: &LlmEndpoint,
    bridge: &WsBridge,
    workspace: &Path,
    tabs: &TabManagerHandle,
) -> Result<String> {
    let role = submitted.actor.as_str();
    let prompt_kind = "executor";
    let step = 1usize;
    let command_id = submitted.command_id.as_str();

    let action = match parse_action_from_raw(
        role,
        endpoint,
        prompt_kind,
        step,
        command_id,
        completion_text,
        true,
        true,
        Some((submitted.lane_label.as_str(), active_tab_id)),
    ) {
        Ok(action) => action,
        Err(invalid) => {
            let agent_type = role.to_uppercase();
            let retry_prompt = action_result_prompt(
                Some(active_tab_id),
                Some(turn_id),
                agent_type.as_str(),
                &invalid.feedback,
                Some("invalid_action"),
                Some(submitted.steps_used),
            );
            return run_agent(
                role,
                prompt_kind,
                "",
                retry_prompt,
                endpoint,
                bridge,
                workspace,
                tabs,
                false,
                true,
                false,
                submitted.steps_used,
            )
            .await
            .map_err(|e| anyhow!("executor invalid_action recovery failed: {e}"));
        }
    };

    let (done, out) = process_action_and_execute(
        role,
        prompt_kind,
        endpoint,
        workspace,
        step,
        command_id,
        &action,
        true,
    )?;
    if done {
        return Ok(out);
    }

    append_orchestration_trace(
        "executor_tool_result_forwarded",
        json!({
            "lane_name": submitted.lane_label,
            "tab_id": active_tab_id,
            "command_id": command_id,
            "action": action.get("action").and_then(|v| v.as_str()),
            "result_bytes": out.len(),
        }),
    );

    let agent_type = role.to_uppercase();
    run_agent(
        role,
        prompt_kind,
        "",
        action_result_prompt(
            Some(active_tab_id),
            Some(turn_id),
            agent_type.as_str(),
            &out,
            action.get("action").and_then(|v| v.as_str()),
            Some(submitted.steps_used),
        ),
        endpoint,
        bridge,
        workspace,
        tabs,
        false,
        true,
        false,
        submitted.steps_used,
    )
    .await
}

// ── Agent loop ─────────────────────────────────────────────────────────────────

/// Run one agent role until it calls `message` with status=complete or exhausts MAX_STEPS.
/// Returns the completion summary on success, or an error on hard failure.
/// `check_on_done`: if true, run cargo build + test before accepting completion.
async fn run_agent(
    role: &str,
    prompt_kind: &str,
    system_instructions: &str,
    initial_prompt: String,
    endpoint: &LlmEndpoint,
    bridge: &WsBridge,
    workspace: &Path,
    tabs: &TabManagerHandle,
    submit_only: bool,
    check_on_done: bool,
    send_system_prompt: bool,
    initial_steps_used: usize,
) -> Result<String> {
    eprintln!(
        "[{role}] endpoint_id={} url={} prompt_kind={} submit_only={}",
        endpoint.id,
        endpoint.pick_url(0),
        prompt_kind,
        submit_only
    );
    let mut step = 0usize;
    let mut last_result: Option<String> = None;
    let mut last_tab_id: Option<u32> = None;
    let mut last_turn_id: Option<u64> = None;
    let mut last_action: Option<String> = None;
    let mut error_streak: usize = 0;
    #[allow(unused_assignments)]
    let mut last_error: Option<String> = None;
    let mut reaction_only_streak: usize = 0;
    let mut cargo_test_gate = CargoTestGate::new();
    let task_context = initial_prompt.clone();
    let mut diagnostics_eventlog_python_done = false;
    let mut idle_streak = 0usize;
    let shutdown = shutdown_signal();
    let ctx = LlmResponseContext {
        role,
        endpoint,
        prompt_kind,
        submit_only,
    };

    loop {
        if let Some(sig) = shutdown.as_ref() {
            if sig.flag.load(Ordering::SeqCst) {
                return Ok("shutdown requested".to_string());
            }
        }
        if step >= MAX_STEPS {
            bail!("[{role}] exhausted {MAX_STEPS} steps without completing");
        }

        let total_steps = if role.starts_with("executor") {
            initial_steps_used.saturating_add(step)
        } else {
            step
        };

        if role.starts_with("executor")
            && executor_step_limit_exceeded(total_steps, EXECUTOR_STEP_LIMIT)
        {
            last_result = Some(format!(
                "Step limit reached: executor must send a message to planner after {EXECUTOR_STEP_LIMIT} actions. Use a message action with evidence or blocker details."
            ));
        }

        let (role_schema, prompt) = build_agent_prompt(
            role,
            send_system_prompt,
            step,
            &initial_prompt,
            system_instructions,
            last_result.as_deref(),
            last_tab_id,
            last_turn_id,
            last_action.as_deref(),
            total_steps,
        );
        let exchange_id = make_command_id(role, prompt_kind, step + 1);

        eprintln!("[{role}] step={} prompt_bytes={}", step + 1, prompt.len());
        ctx.log_request(step + 1, &exchange_id, &prompt, &role_schema);

        let response_timeout_secs = response_timeout_for_role(role);
        let request_future = llm_worker_send_request_with_req_id_timeout(
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
            role,
            tabs,
            endpoint.max_tabs,
            submit_only,
            Some(response_timeout_secs),
        );
        let req_result = match shutdown.as_ref() {
            Some(sig) => {
                tokio::select! {
                    res = request_future => res,
                    _ = sig.notify.notified() => {
                        return Ok("shutdown requested".to_string());
                    }
                }
            }
            None => request_future.await,
        };
        let (req_id, resp) = match req_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[{role}] step={} llm_error: {e}", step + 1);
                ctx.log_error(step + 1, &exchange_id, &e.to_string());
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    &e.to_string(),
                    format!(
                        "LLM error: {e}\nReturn exactly one action as a single JSON object in a ```json code block.\n\nTask context:\n{}",
                        truncate(&task_context, MAX_SNIPPET)
                    ),
                );
                step += 1;
                continue;
            }
        };
        let _ = req_id;
        last_tab_id = resp.tab_id;
        last_turn_id = resp.turn_id;
        let raw = resp.raw;

        ctx.log_response(step + 1, &exchange_id, &raw);

        if let Some(ack) = ctx.handle_submit_ack(step + 1, &exchange_id, &raw) {
            return Ok(ack);
        }

        eprintln!("[{role}] step={} response_bytes={}", step + 1, raw.len());

        if ctx.handle_reaction_only(
            step + 1,
            &exchange_id,
            &raw,
            &mut reaction_only_streak,
            &mut error_streak,
            &mut last_error,
        ) {
            if !should_force_blocker(reaction_only_streak) {
                continue;
            }
            reaction_only_streak = 0;
            apply_error_result(
                role,
                &task_context,
                &mut error_streak,
                &mut last_error,
                &mut last_result,
                "reaction_only_response",
                build_invalid_action_feedback(None, "reaction-only response", role),
            );
            step += 1;
            continue;
        }

        let mut action = match parse_action_from_raw(
            role,
            endpoint,
            prompt_kind,
            step + 1,
            &exchange_id,
            &raw,
            false,
            false,
            None,
        ) {
            Ok(action) => action,
            Err(invalid) => {
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    &invalid.err_text,
                    invalid.feedback,
                );
                step += 1;
                continue;
            }
        };

        let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();

        if kind == "run_command" {
            let cmd = action.get("cmd").and_then(|v| v.as_str());
            cargo_test_gate.note_action(&kind, cmd);
        }
        if kind != "message"
            && enforce_executor_step_limit(role, total_steps, &mut error_streak, &mut last_result)
        {
            step += 1;
            continue;
        }
        if let Some(msg) = cargo_test_gate.message_blocker_if_needed(&kind, crate::constants::workspace()) {
            error_streak = error_streak.saturating_add(1);
            last_result = Some(msg);
            step += 1;
            continue;
        }

        reaction_only_streak = 0;
        error_streak = 0;
        eprintln!("[{role}] step={} action={}", step + 1, kind);
        last_action = Some(kind.clone());
        append_orchestration_trace(
            "llm_message_processed",
            json!({
                "role": role,
                "prompt_kind": prompt_kind,
                "step": step + 1,
                "endpoint_id": endpoint.id,
                "action": kind,
            }),
        );

        let command_id = exchange_id.clone();
        action["command_id"] = Value::String(command_id.clone());

        if let Some(msg) = enforce_diagnostics_python(
            role,
            step,
            kind.as_str(),
            &action,
            &mut diagnostics_eventlog_python_done,
        ) {
            last_result = Some(msg);
            step += 1;
            continue;
        }

        if is_explicit_idle_action(&action) {
            idle_streak += 1;
            if idle_streak >= 3 {
                bail!("[{role}] stuck: no progress in 3 steps (repeated explicit idle commands)");
            }
        } else {
            idle_streak = 0;
        }

        let step_result = process_action_and_execute(
            role,
            prompt_kind,
            endpoint,
            workspace,
            step + 1,
            &command_id,
            &action,
            check_on_done,
        )?;

        match step_result {
            (true, reason) => {
                eprintln!("[{role}] message complete: {reason}");
                return Ok(reason);
            }
            (false, out) => {
                cargo_test_gate.note_result(&kind, &out);
                last_result = Some(out);
            }
        }
        step += 1;
    }
}

fn find_endpoint<'a>(endpoints: &'a [LlmEndpoint], role: &str) -> Result<&'a LlmEndpoint> {
    endpoints
        .iter()
        .find(|e| e.role.as_deref() == Some(role))
        .ok_or_else(|| anyhow!("no endpoint with role '{role}' in constants"))
}

fn build_endpoints() -> Vec<LlmEndpoint> {
    ENDPOINT_SPECS
        .iter()
        .map(|spec| LlmEndpoint {
            id: spec.id.to_string(),
            url: spec.urls.iter().map(|s| s.to_string()).collect(),
            role_markdown: spec.role_markdown.to_string(),
            role: Some(spec.role.to_string()),
            stateful: spec.stateful,
            max_tabs: spec.max_tabs,
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
struct DispatchLaneState {
    plan_text: String,
    pending: bool,
    in_progress_by: Option<String>,
    latest_verifier_result: String,
}

#[derive(Clone)]
struct PendingSubmitState {
    job: PendingExecutorSubmit,
    started_ms: u64,
    command_id: String,
    endpoint_id: String,
    tabs: TabManagerHandle,
}

#[derive(Clone)]
struct DeferredExecutorCompletion {
    submitted: SubmittedExecutorTurn,
    turn_id: u64,
    tab_id: u32,
    exec_result: String,
}

#[derive(Clone)]
struct DispatchState {
    lanes: HashMap<usize, DispatchLaneState>,
    submitted_turns: std::collections::HashMap<(u32, u64), SubmittedExecutorTurn>,
    executor_submit_inflight: HashMap<usize, PendingSubmitState>,
    tab_id_to_lane: HashMap<u32, usize>,
    lane_active_tab: HashMap<usize, u32>,
    lane_prompt_in_flight: HashMap<usize, bool>,
    deferred_completions: HashMap<usize, VecDeque<DeferredExecutorCompletion>>,
    lane_steps_used: HashMap<usize, usize>,
    diagnostics_pending: bool,
    planner_pending: bool,
    diagnostics_text: String,
    last_plan_text: String,
    last_executor_diff: String,
    lane_next_submit_at_ms: HashMap<usize, u64>,
    lane_submit_in_flight: HashMap<usize, bool>,
}

fn new_dispatch_state(lanes: &[LaneConfig]) -> DispatchState {
    let mut lanes_state = HashMap::new();
    let mut lane_prompt_in_flight = HashMap::new();
    let mut deferred_completions = HashMap::new();
    let mut lane_next_submit_at_ms = HashMap::new();
    let mut lane_submit_in_flight = HashMap::new();
    let mut lane_steps_used = HashMap::new();
    for lane in lanes {
        lanes_state.insert(lane.index, DispatchLaneState::default());
        lane_prompt_in_flight.insert(lane.index, false);
        deferred_completions.insert(lane.index, VecDeque::new());
        lane_next_submit_at_ms.insert(lane.index, 0);
        lane_submit_in_flight.insert(lane.index, false);
        lane_steps_used.insert(lane.index, 0);
    }
    DispatchState {
        lanes: lanes_state,
        submitted_turns: std::collections::HashMap::new(),
        executor_submit_inflight: HashMap::new(),
        tab_id_to_lane: HashMap::new(),
        lane_active_tab: HashMap::new(),
        lane_prompt_in_flight,
        deferred_completions,
        lane_steps_used,
        diagnostics_pending: false,
        planner_pending: false,
        diagnostics_text: String::new(),
        last_plan_text: String::new(),
        last_executor_diff: String::new(),
        lane_next_submit_at_ms,
        lane_submit_in_flight,
    }
}

impl DispatchState {
    fn lane_in_flight(&self, lane_id: usize) -> bool {
        *self.lane_prompt_in_flight.get(&lane_id).unwrap_or(&false)
    }
    fn lane_submit_active(&self, lane_id: usize) -> bool {
        *self.lane_submit_in_flight.get(&lane_id).unwrap_or(&false)
    }
    fn lane_next_submit_ms(&self, lane_id: usize) -> u64 {
        *self.lane_next_submit_at_ms.get(&lane_id).unwrap_or(&0)
    }
    fn lane_steps_used(&self, lane_id: usize) -> usize {
        *self.lane_steps_used.get(&lane_id).unwrap_or(&0)
    }
    fn lane_active_tab(&self, lane_id: usize) -> Option<u32> {
        self.lane_active_tab.get(&lane_id).copied()
    }
}

#[derive(Clone)]
struct SubmittedExecutorTurn {
    tab_id: u32,
    lane: usize,
    lane_label: String,
    command_id: String,
    actor: String,
    endpoint_id: String,
    tabs: TabManagerHandle,
    steps_used: usize,
}

#[derive(Clone, Debug)]
struct PendingExecutorSubmit {
    executor_name: String,
    executor_display: String,
    lane_index: usize,
    label: String,
    latest_verify_result: String,
    executor_role: String,
}

fn parse_submit_ack(raw: &str) -> Option<(u32, u64, Option<String>)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v.get("submit_ack").and_then(|x| x.as_bool()) != Some(true) {
        return None;
    }
    let tab_id = v.get("tab_id").and_then(|x| x.as_u64())? as u32;
    let turn_id = v.get("turn_id").and_then(|x| x.as_u64())?;
    let command_id = v.get("command_id").and_then(|x| x.as_str()).map(str::to_string);
    Some((tab_id, turn_id, command_id))
}

fn append_executor_completion_log(
    submitted: &SubmittedExecutorTurn,
    step: usize,
    turn_id: u64,
    tab_id: u32,
    text: &str,
) -> Result<()> {
    let parsed = parse_actions(text)
        .ok()
        .and_then(|actions| actions.into_iter().next());
    let observation = parsed
        .as_ref()
        .and_then(|action| action_observation(action))
        .map(str::to_string);
    let rationale = parsed
        .as_ref()
        .and_then(|action| action_rationale(action))
        .map(str::to_string);
    let parsed_action = parsed
        .as_ref()
        .and_then(|action| action.get("action").and_then(|v| v.as_str()))
        .map(str::to_string);
    let parsed_command = parsed
        .as_ref()
        .map(|action| {
            let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("unknown");
            match kind {
                "run_command" => jstr(action, "cmd").to_string(),
                "python" => "python".to_string(),
                "read_file" => {
                    let path = jstr(action, "path");
                    let line = action.get("line").and_then(|v| v.as_u64());
                    match line {
                        Some(n) => format!("read_file {}:{}", path, n),
                        None => format!("read_file {}", path),
                    }
                }
                "list_dir" => format!("list_dir {}", jstr(action, "path")),
                "apply_patch" => "apply_patch".to_string(),
                "message" => {
                    let status = jstr(action, "status");
                    let summary = action
                        .get("payload")
                        .and_then(|v| v.get("summary"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    format!("message {} {}", status, summary)
                }
                _ => kind.to_string(),
            }
        })
        .filter(|s| !s.is_empty());
    let record = compact_log_record(
        "llm",
        "completion",
        Some(&submitted.actor),
        Some(submitted.lane_label.as_str()),
        Some(&submitted.endpoint_id),
        Some(step),
        Some(turn_id),
        Some(&submitted.command_id),
        parsed_action.map(|name| {
            let summary = parsed_command.clone().unwrap_or_else(|| name.clone());
            json!({
                "name": name,
                "summary": summary,
            })
        }),
        None,
        observation,
        rationale,
        Some(text.to_string()),
        Some(json!({ "tab_id": tab_id })),
    );
    append_action_log_record(&record)
}

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
    dispatch_state: &mut DispatchState,
    lanes: &[LaneConfig],
    bridge: &WsBridge,
    workspace: &PathBuf,
    continuation_joinset: &mut tokio::task::JoinSet<(SubmittedExecutorTurn, u64, Result<String>)>,
    _verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    submitted.steps_used = submitted.steps_used.saturating_add(1);
    dispatch_state
        .lane_steps_used
        .insert(submitted.lane, submitted.steps_used);
    let lane_cfg = &lanes[submitted.lane];
    let lane_name = lane_cfg.label.as_str();
    if dispatch_state.lane_in_flight(submitted.lane) {
        dispatch_state
            .deferred_completions
            .entry(submitted.lane)
            .or_default()
            .push_back(DeferredExecutorCompletion {
                submitted,
                turn_id,
                tab_id,
                exec_result,
            });
        append_orchestration_trace(
            "executor_completion_deferred",
            json!({
                "lane_name": lane_name,
                "tab_id": tab_id,
                "turn_id": turn_id,
            }),
        );
        return false;
    }

    append_orchestration_trace(
        "llm_message_processed",
        json!({
            "tab_id": tab_id,
            "turn_id": turn_id,
            "lane_name": lane_name,
        }),
    );
    if let Err(e) = append_executor_completion_log(&submitted, 1, turn_id, tab_id, &exec_result) {
        eprintln!("[orchestrate] executor_completion_log_error: {e}");
    }
    if let Ok(mut actions) = parse_actions(&exec_result) {
        if actions.first().and_then(|a| a.get("action")).and_then(|v| v.as_str()) == Some("message") {
            dispatch_state.lane_steps_used.insert(submitted.lane, 0);
            if let Some(action) = actions.pop() {
                log_action_result(&submitted.actor, &lane_cfg.endpoint, "executor", 1, &submitted.command_id, &action, true, &exec_result);
                let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");
                if to_role.eq_ignore_ascii_case("planner") {
                    persist_planner_message(&action);
                    dispatch_state.planner_pending = true;
                } else {
                    // Generic wakeup for other targets (verifier, diagnostics, etc.)
                    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
                    let _ = std::fs::create_dir_all(agent_state_dir);
                    let to_key = to_role.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_");
                    let msg_path = agent_state_dir.join(format!("last_message_to_{to_key}.json"));
                    let _ = std::fs::write(&msg_path, serde_json::to_string_pretty(&action).unwrap_or_default());
                    let _ = std::fs::write(agent_state_dir.join(format!("wakeup_{to_key}.flag")), "handoff");
                }
            }
        }
    }
    let mut submitted = submitted;
    if submitted.tab_id != tab_id {
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
        dispatch_state.lane_active_tab.insert(submitted.lane, tab_id);
        dispatch_state.tab_id_to_lane.insert(tab_id, submitted.lane);
        submitted.tab_id = tab_id;
    }
    eprintln!(
        "[orchestrate] executor turn requires tool execution: lane={} turn_id={}",
        lane_name,
        turn_id
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
    let executor_endpoint = lane_cfg.endpoint.clone();
    let bridge = bridge.clone();
    let workspace = workspace.clone();
    let exec_result = exec_result.clone();
    let submitted_clone = submitted.clone();
    let tabs = submitted.tabs.clone();
    dispatch_state
        .lane_prompt_in_flight
        .insert(submitted.lane, true);
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
        )
        .await;
        (submitted_clone, turn_id, result)
    });
    true
}

fn plan_has_incomplete_tasks(plan_text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(plan_text) else {
        return true;
    };
    value
        .get("tasks")
        .and_then(|v| v.as_array())
        .map(|tasks| {
            tasks.iter().any(|task| {
                task.get("status")
                    .and_then(|v| v.as_str())
                    .map(|status| status != "done")
                    .unwrap_or(true)
            })
        })
        .unwrap_or(true)
}

fn verifier_confirmed_with_plan_text(reason: &str, plan_text: &str) -> bool {
    if plan_has_incomplete_tasks(plan_text) {
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
    let plan_path = Path::new(workspace()).join(MASTER_PLAN_FILE);
    let plan_text = std::fs::read_to_string(plan_path).unwrap_or_default();
    verifier_confirmed_with_plan_text(reason, &plan_text)
}

fn dispatch_lane_mut<'a>(state: &'a mut DispatchState, lane_id: usize) -> &'a mut DispatchLaneState {
    state.lanes.entry(lane_id).or_default()
}

fn claim_next_lane(state: &mut DispatchState, lane: &LaneConfig) -> Option<(usize, String)> {
    let lane_id = lane.index;
    let lane_state = dispatch_lane_mut(state, lane_id);
    if lane_state.pending && lane_state.in_progress_by.is_none() {
        lane_state.pending = false;
        lane_state.in_progress_by = Some(lane.label.clone());
        return Some((lane_id, lane_state.latest_verifier_result.clone()));
    }
    None
}

fn claim_executor_submit(state: &mut DispatchState, lane: &LaneConfig) -> Option<PendingExecutorSubmit> {
    let (lane_id, latest_verify_result) = claim_next_lane(state, lane)?;
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

async fn submit_executor_turn(
    job: &PendingExecutorSubmit,
    endpoint: &LlmEndpoint,
    bridge: &WsBridge,
    tabs: &TabManagerHandle,
    send_system_prompt: bool,
    command_id: &str,
    response_timeout_secs: u64,
) -> Result<String> {
    let mut exec_prompt = executor_cycle_prompt(
        job.executor_display.as_str(),
        job.label.as_str(),
        &job.latest_verify_result,
    );
    inject_inbound_message(&mut exec_prompt, "executor");
    let executor_system = system_instructions(AgentPromptKind::Executor);
    let role_schema = if send_system_prompt {
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
    let workspace_override = args.windows(2).find(|w| w[0] == "--workspace").map(|w| w[1].clone());
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
    let state_dir_override = args.windows(2).find(|w| w[0] == "--state-dir").map(|w| w[1].clone());
    if let Some(ref path) = state_dir_override {
        let p = std::path::Path::new(path);
        if !p.is_absolute() {
            bail!("--state-dir must be an absolute path, got: {path}");
        }
        set_agent_state_dir(path.clone());
        eprintln!("[canon-mini-agent] state_dir={path} (--state-dir)");
    } else {
        eprintln!("[canon-mini-agent] state_dir={} (default)", DEFAULT_AGENT_STATE_DIR);
    }

    let orchestrate = args.iter().any(|a| a == "--orchestrate");
    let start_role = args
        .windows(2)
        .find(|w| w[0] == "--start")
        .map(|w| w[1].as_str())
        .unwrap_or("executor");
    if !matches!(start_role, "executor" | "verifier" | "planner" | "diagnostics" | "solo") {
        bail!("invalid --start value: {start_role} (expected executor|verifier|planner|diagnostics|solo)");
    }
    let role_arg = args
        .windows(2)
        .find(|w| w[0] == "--role")
        .map(|w| w[1].as_str());
    let role_flags = ["--verifier", "--planner", "--diagnostics"];
    let has_role_flag = args.iter().any(|a| role_flags.contains(&a.as_str()));
    if role_arg.is_some() && has_role_flag {
        bail!("--role cannot be combined with --planner, --verifier, or --diagnostics");
    }
    if role_arg.is_some() && orchestrate {
        bail!("--role cannot be combined with --orchestrate");
    }

    let mut is_verifier = !orchestrate && args.iter().any(|a| a == "--verifier");
    let mut is_planner = !orchestrate && args.iter().any(|a| a == "--planner");
    let mut is_diagnostics = !orchestrate && args.iter().any(|a| a == "--diagnostics");

    if let Some(role) = role_arg {
        match role {
            "executor" => {}
            "planner" => is_planner = true,
            "verifier" => is_verifier = true,
            "diagnostics" => is_diagnostics = true,
            _ => bail!("invalid --role value: {role} (expected executor|planner|verifier|diagnostics)"),
        }
    }
    let (ws_port, ws_port_explicit) = choose_ws_port(&args)?;
    if ws_port_explicit {
        eprintln!("[canon-mini-agent] ws_port={} (explicit)", ws_port);
    } else {
        eprintln!(
            "[canon-mini-agent] ws_port={} (auto-selected from {:?})",
            ws_port,
            WS_PORT_CANDIDATES
        );
    }

    let workspace = PathBuf::from(workspace());
    let spec_path = workspace.join(SPEC_FILE);
    let master_plan_path = workspace.join(MASTER_PLAN_FILE);
    let violations_path = workspace.join(VIOLATIONS_FILE);
    let instance_id = instance_arg(&args).map(str::to_string);
    let path_prefix = instance_id.clone().unwrap_or_else(|| "default".to_string());
    init_log_paths(&path_prefix);
    let diagnostics_rel = format!("PLANS/{}/diagnostics-{}.json", path_prefix, path_prefix);
    let diagnostics_path = workspace.join(&diagnostics_rel);
    let _ = DIAGNOSTICS_FILE_PATH.set(diagnostics_rel.clone());
    if let Err(err) = ensure_objectives_and_invariants_json(&workspace) {
        eprintln!("[canon-mini-agent] objectives/invariants conversion failed: {err:#}");
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
            plan_file: format!("PLANS/{}/executor-{}.json", path_prefix, ep.id),
            label: ep.id.clone(),
            endpoint: ep,
            tabs: llm_worker_new_tabs(),
        })
        .collect();
    if lanes.is_empty() {
        bail!("no executor endpoints with role = \"executor\" found in constants");
    }
    let plans_dir = workspace.join("PLANS").join(&path_prefix);
    let _ = std::fs::create_dir_all(&plans_dir);
    if !diagnostics_path.exists() {
        let legacy_path = workspace.join("DIAGNOSTICS.json");
        if let Ok(contents) = std::fs::read_to_string(&legacy_path) {
            let _ = std::fs::write(&diagnostics_path, contents);
        } else {
            let _ = std::fs::write(&diagnostics_path, "");
        }
    }
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
        if let Some(parent) = plan_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&plan_path, contents);
    }

    let ws_addr: std::net::SocketAddr = format!("127.0.0.1:{ws_port}").parse()?;
    let bridge = ws_server::spawn(ws_addr, DEFAULT_RESPONSE_TIMEOUT_SECS, Arc::new(OnceLock::new()));
    eprintln!("[canon-mini-agent] waiting for Chrome extension on ws://127.0.0.1:{ws_port}");
    bridge.wait_for_connection().await;
    eprintln!("[canon-mini-agent] Chrome extension connected");

    let tabs = llm_worker_new_tabs();

    if orchestrate {
        const SERVICE_POLL_MS: u64 = 500;
        const PENDING_SUBMIT_TIMEOUT_MS: u64 = 10_000;

        eprintln!("[orchestrate] start_role={start_role}");

        let diagnostics_ep = find_endpoint(&endpoints, "diagnostics")?.clone();
        let planner_ep = find_endpoint(&endpoints, "mini_planner")?.clone();
        let solo_ep = find_endpoint(&endpoints, "solo")?.clone();
        let verifier_ep = find_endpoint(&endpoints, "verifier")?.clone();

        let tabs_diagnostics = llm_worker_new_tabs();
        let tabs_planner = llm_worker_new_tabs();
        let tabs_solo = llm_worker_new_tabs();
        let tabs_verify = llm_worker_new_tabs();
        let mut verifier_summary: Vec<String> = vec!["(none yet)".to_string(); lanes.len()];
        let mut dispatch_state = new_dispatch_state(&lanes);
        dispatch_state.planner_pending = true;
        let mut current_phase = "bootstrap".to_string();
        let mut current_phase_lane: Option<usize> = None;
        let mut scheduled_phase: Option<String> = None;
        let mut resume_verifier_items: Vec<ResumeVerifierItem> = Vec::new();
        let mut solo_bootstrapped = false;
        if let Some(checkpoint) = load_checkpoint(&workspace) {
            eprintln!(
                "[orchestrate] resume checkpoint loaded: phase={} lane={:?} age_ms={}",
                checkpoint.phase,
                checkpoint.phase_lane,
                now_ms().saturating_sub(checkpoint.created_ms)
            );
            dispatch_state.planner_pending = checkpoint.planner_pending;
            dispatch_state.diagnostics_pending = checkpoint.diagnostics_pending;
            dispatch_state.diagnostics_text = checkpoint.diagnostics_text;
            dispatch_state.last_plan_text = checkpoint.last_plan_text;
            dispatch_state.last_executor_diff = checkpoint.last_executor_diff;
            for lane_snapshot in checkpoint.lanes {
                if let Some(state) = dispatch_state.lanes.get_mut(&lane_snapshot.lane_id) {
                    state.plan_text = lane_snapshot.plan_text;
                    state.pending = lane_snapshot.pending;
                    state.in_progress_by = lane_snapshot.in_progress_by;
                    state.latest_verifier_result = lane_snapshot.latest_verifier_result;
                }
            }
            if checkpoint.verifier_summary.len() == lanes.len() {
                verifier_summary = checkpoint.verifier_summary;
            }
            resume_verifier_items = checkpoint.verifier_pending_results;
            current_phase = checkpoint.phase;
            current_phase_lane = checkpoint.phase_lane;
            let resume_decision = decide_resume_phase(
                &current_phase,
                !resume_verifier_items.is_empty(),
                dispatch_state.planner_pending,
                dispatch_state.diagnostics_pending,
            );
            scheduled_phase = resume_decision.scheduled_phase;
            dispatch_state.planner_pending = resume_decision.planner_pending;
            dispatch_state.diagnostics_pending = resume_decision.diagnostics_pending;
            // On resume, DO NOT clear executor_submit_inflight.
            // Clearing inflight state while preserving active tabs and submitted_turns
            // causes valid submit_ack events to lose their pending context, triggering
            // "submit ack without pending submit" and orphaning executor state.
            // We preserve executor_submit_inflight so late acks remain reconcilable.
            // Keep submitted_turns, tab_id_to_lane, and lane_active_tab intact
            // so late completions and active tabs can still be reconciled.
            dispatch_state.deferred_completions.clear();
            for lane in &lanes {
                dispatch_state.lane_prompt_in_flight.insert(lane.index, false);
                dispatch_state.lane_submit_in_flight.insert(lane.index, false);
            }
            for (lane_id, lane) in dispatch_state.lanes.iter_mut() {
                if lane.in_progress_by.is_some() {
                    let has_active_tab = dispatch_state
                        .lane_active_tab
                        .get(lane_id)
                        .is_some();
                    // Only reset lanes that truly lost ownership (no active tab)
                    if !has_active_tab {
                        lane.in_progress_by = None;
                        lane.pending = true;
                    }
                }
            }
        }
        let mut planner_bootstrapped = false;
        let mut diagnostics_bootstrapped = false;
        let mut verifier_bootstrapped = false;
        let mut submit_joinset: tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)> =
            tokio::task::JoinSet::new();
        let mut continuation_joinset: tokio::task::JoinSet<(SubmittedExecutorTurn, u64, Result<String>)> =
            tokio::task::JoinSet::new();
        let mut verifier_joinset: tokio::task::JoinSet<(usize, String)> = tokio::task::JoinSet::new();
        let mut verifier_pending_results: VecDeque<(SubmittedExecutorTurn, u64, String)> = VecDeque::new();

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
                        actor: "executor".to_string(),
                        endpoint_id: lane.endpoint.id.clone(),
                        tabs: tabs_verify.clone(),
                        steps_used: 0,
                    };
                    verifier_pending_results.push_back((submitted, 0, item.final_exec_result));
                }
            }
        }

        eprintln!("[orchestrate] pipeline started: planner -> background executors -> verifier/diagnostics -> planner");

        loop {
            let _ = std::fs::remove_file(cycle_idle_marker_path());
            let mut cycle_progress = false;
            if shutdown.flag.load(Ordering::SeqCst) {
                eprintln!("[orchestrate] shutdown requested; saving checkpoint");
                if let Err(err) = save_checkpoint(
                    &workspace,
                    &current_phase,
                    current_phase_lane,
                    &dispatch_state,
                    &lanes,
                    &verifier_summary,
                    &verifier_pending_results,
                ) {
                    eprintln!("[orchestrate] checkpoint save failed: {err:#}");
                }
                return Ok(());
            }

            let agent_state_dir =
                std::path::Path::new(crate::constants::agent_state_dir());
            apply_wake_flags(agent_state_dir, &mut dispatch_state, &mut scheduled_phase);

            if scheduled_phase.is_none() && current_phase == "bootstrap" {
                if let Some(phase) = decide_bootstrap_phase(start_role) {
                    current_phase = phase;
                    eprintln!(
                        "[orchestrate] bootstrap_start_role: role={} scheduled_phase=None",
                        current_phase
                    );
                    if current_phase == "planner" {
                        dispatch_state.planner_pending = true;
                    }
                    if current_phase == "diagnostics" {
                        dispatch_state.diagnostics_pending = true;
                    }
                    if current_phase == "solo" {
                        scheduled_phase = Some("solo".to_string());
                    }
                }
            }

            let agent_state_dir =
                std::path::Path::new(crate::constants::agent_state_dir());
            let active_blocker = agent_state_dir.join("active_blocker_to_verifier.json").exists();
            let blocker_decision = decide_active_blocker(
                active_blocker,
                dispatch_state.planner_pending,
                scheduled_phase.as_deref(),
            );
            if active_blocker
                && (dispatch_state.planner_pending || scheduled_phase.as_deref() == Some("planner"))
            {
                eprintln!("[orchestrate] planner paused: active blocker to verifier");
            }
            dispatch_state.planner_pending = blocker_decision.planner_pending;
            scheduled_phase = blocker_decision.scheduled_phase;

            let phase_gates = decide_phase_gates(
                dispatch_state.planner_pending,
                dispatch_state.diagnostics_pending,
                !verifier_pending_results.is_empty(),
                !verifier_joinset.is_empty(),
                scheduled_phase.as_deref(),
            );

            let orchestrator_ctx = OrchestratorContext {
                lanes: &lanes,
                workspace: &workspace,
                bridge: &bridge,
                tabs_planner: &tabs_planner,
                tabs_solo: &tabs_solo,
                tabs_diagnostics: &tabs_diagnostics,
                tabs_verify: &tabs_verify,
                planner_ep: &planner_ep,
                solo_ep: &solo_ep,
                diagnostics_ep: &diagnostics_ep,
                verifier_ep: &verifier_ep,
                master_plan_path: &master_plan_path,
                violations_path: &violations_path,
                diagnostics_path: &diagnostics_path,
            };
            let cargo_test_failures = load_cargo_test_failures(&workspace);

            if phase_gates.planner {
                current_phase = "planner".to_string();
                current_phase_lane = None;
                if run_planner_phase(
                    &orchestrator_ctx,
                    &mut dispatch_state,
                    &verifier_summary,
                    &mut planner_bootstrapped,
                    &cargo_test_failures,
                )
                .await
                {
                    cycle_progress = true;
                }
            }

            if phase_gates.solo {
                current_phase = "solo".to_string();
                current_phase_lane = None;
                if run_solo_phase(&orchestrator_ctx, &mut solo_bootstrapped, &cargo_test_failures)
                    .await
                {
                    cycle_progress = true;
                }
            }

            let now = now_ms();
            if phase_gates.executor {
                if run_executor_phase(
                    &orchestrator_ctx,
                    &mut dispatch_state,
                    now,
                    PENDING_SUBMIT_TIMEOUT_MS,
                    &mut submit_joinset,
                    scheduled_phase.as_deref(),
                    &mut current_phase,
                    &mut current_phase_lane,
                ) {
                    cycle_progress = true;
                }
            }

            if process_completed_turns(
                &orchestrator_ctx,
                &mut dispatch_state,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            )
            .await
            {
                cycle_progress = true;
            }

            if drain_continuations(
                &mut dispatch_state,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            ) {
                cycle_progress = true;
            }

            if drain_deferred_completions(
                &orchestrator_ctx,
                &mut dispatch_state,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            ) {
                cycle_progress = true;
            }

            let mut verifier_changed = false;
            if !verifier_pending_results.is_empty() || !verifier_joinset.is_empty() {
                let (phase_progress, phase_changed) = run_verifier_phase(
                    &orchestrator_ctx,
                    &mut dispatch_state,
                    &mut verifier_pending_results,
                    &mut verifier_summary,
                    &mut verifier_joinset,
                    &mut verifier_bootstrapped,
                    &mut scheduled_phase,
                    &mut current_phase,
                    &mut current_phase_lane,
                    &cargo_test_failures,
                )
                .await;
                if phase_progress {
                    cycle_progress = true;
                }
                if phase_changed {
                    verifier_changed = true;
                }
            }

            if verifier_changed {
                dispatch_state.diagnostics_pending = true;
            }

            if dispatch_state.diagnostics_pending
                && allow_diagnostics_run(scheduled_phase.as_deref(), !verifier_joinset.is_empty())
            {
                current_phase = "diagnostics".to_string();
                current_phase_lane = None;
                if run_diagnostics_phase(
                    &orchestrator_ctx,
                    &mut dispatch_state,
                    &verifier_summary,
                    &mut diagnostics_bootstrapped,
                    verifier_changed,
                    &cargo_test_failures,
                )
                .await
                {
                    cycle_progress = true;
                }
            }

            if scheduled_phase.as_deref() == Some("diagnostics") && !dispatch_state.diagnostics_pending {
                scheduled_phase = None;
            }

            if let Some(phase) = scheduled_phase.as_deref() {
                let (executor_lane_pending, executor_in_progress) = current_phase_lane
                    .and_then(|lane_id| dispatch_state.lanes.get(&lane_id))
                    .map(|lane| (lane.pending, lane.in_progress_by.is_some()))
                    .unwrap_or((false, false));
                if scheduled_phase_resume_done(
                    phase,
                    dispatch_state.planner_pending,
                    dispatch_state.diagnostics_pending,
                    verifier_pending_results.len(),
                    verifier_joinset.is_empty(),
                    executor_lane_pending,
                    executor_in_progress,
                ) {
                    scheduled_phase = None;
                }
            }

            if !cycle_progress {
                let _ = std::fs::write(cycle_idle_marker_path(), "idle\n");
                tokio::time::sleep(std::time::Duration::from_millis(SERVICE_POLL_MS)).await;
            }
        }
    } else {
        // Single-role mode
        let single_role_ctx = SingleRoleContext {
            workspace: &workspace,
            spec_path: &spec_path,
            master_plan_path: &master_plan_path,
            violations_path: &violations_path,
            diagnostics_path: &diagnostics_path,
        };
        let (inputs, endpoint) =
            load_single_role_setup(&single_role_ctx, &endpoints, is_verifier, is_diagnostics, is_planner)?;
        let instructions = system_instructions(inputs.prompt_kind);
        eprintln!(
            "[canon-mini-agent] role={} input loaded ({} bytes)",
            inputs.role,
            inputs.primary_input.len()
        );
        eprintln!("[canon-mini-agent] endpoint id={} url={}", endpoint.id, endpoint.pick_url(0));

        let cargo_test_failures = load_cargo_test_failures(&workspace);
        let initial_prompt = build_single_role_prompt(&single_role_ctx, &inputs, &cargo_test_failures)?;

        let submit_only = inputs.role == "executor";
        let reason = run_agent(
            inputs.role.as_str(),
            canonical_role_label(inputs.role.as_str()),
            &instructions,
            initial_prompt,
            if inputs.role == "executor" { &lanes[0].endpoint } else { &endpoint },
            &bridge,
            &workspace,
            &tabs,
            submit_only,
            inputs.role == "executor",
            true,
            0,
        ).await?;
        println!("message: {reason}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_has_incomplete_tasks, verifier_confirmed_with_plan_text};

    #[test]
    fn verifier_confirmed_rejects_when_plan_has_incomplete_tasks() {
        let reason = r#"{"verified":true,"summary":"ok"}"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id": "T1", "status": "ready"},
            {"id": "T2", "status": "done"}
          ]
        }"#;
        assert!(plan_has_incomplete_tasks(plan));
        assert!(!verifier_confirmed_with_plan_text(reason, plan));
    }

    #[test]
    fn verifier_confirmed_accepts_only_verified_when_plan_is_done() {
        let verified = r#"{"verified":true,"summary":"ok"}"#;
        let unverified = r#"{"verified":false,"summary":"blocked"}"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id": "T1", "status": "done"},
            {"id": "T2", "status": "done"}
          ]
        }"#;
        assert!(!plan_has_incomplete_tasks(plan));
        assert!(verifier_confirmed_with_plan_text(verified, plan));
        assert!(!verifier_confirmed_with_plan_text(unverified, plan));
    }
}

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
use crate::tools::write_stage_graph;
use crate::tool_schema::write_tool_examples;
use crate::logging::{
    append_action_log_record, append_orchestration_trace, compact_log_record, init_log_paths,
    log_action_result, log_error_event, log_message_event, make_command_id, now_ms,
};
use crate::prompts::{
    action_intent, action_objective_id, action_observation, action_rationale, action_result_prompt,
    action_task_id, diagnostics_cycle_prompt,
    diagnostics_python_reads_event_logs, executor_cycle_prompt, is_explicit_idle_action,
    normalize_action, parse_actions, planner_cycle_prompt, single_role_solo_prompt, system_instructions,
    truncate, validate_action, verifier_cycle_prompt, AgentPromptKind,
};
use crate::invalid_action::{
    auto_fill_message_fields, build_invalid_action_feedback, corrective_invalid_action_prompt,
    default_message_route, ensure_action_base_schema, expected_message_format,
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
    DEFAULT_AGENT_STATE_DIR, DEFAULT_LLM_RETRY_COUNT, DEFAULT_LLM_RETRY_DELAY_SECS,
    DEFAULT_RESPONSE_TIMEOUT_SECS, DIAGNOSTICS_FILE_PATH, ENDPOINT_SPECS, EXECUTOR_STEP_LIMIT,
    INVARIANTS_FILE, ISSUES_FILE, MASTER_PLAN_FILE, MAX_SNIPPET, MAX_STEPS, OBJECTIVES_FILE,
    ROLE_TIMEOUT_SECS, SPEC_FILE, VIOLATIONS_FILE, WS_PORT_CANDIDATES, set_agent_state_dir,
    set_workspace, workspace,
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

fn find_flag_arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn ws_port_arg(args: &[String]) -> Option<&str> {
    find_flag_arg(args, "--port")
}

fn instance_arg(args: &[String]) -> Option<&str> {
    find_flag_arg(args, "--instance")
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
    trace_message_common(
        role,
        prompt_kind,
        step,
        endpoint_id,
        submit_only,
        prompt_bytes,
        "llm_message_forwarded",
        "prompt_bytes",
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
    trace_message_common(
        role,
        prompt_kind,
        step,
        endpoint_id,
        submit_only,
        response_bytes,
        "llm_message_received",
        "response_bytes",
    );
}

fn trace_message_common(
    role: &str,
    prompt_kind: &str,
    step: usize,
    endpoint_id: &str,
    submit_only: bool,
    bytes: usize,
    event_name: &str,
    bytes_field: &str,
) {
    let mut payload = serde_json::Map::new();
    payload.insert("role".to_string(), Value::String(role.to_string()));
    payload.insert("prompt_kind".to_string(), Value::String(prompt_kind.to_string()));
    payload.insert("step".to_string(), Value::Number(step.into()));
    payload.insert("endpoint_id".to_string(), Value::String(endpoint_id.to_string()));
    payload.insert("submit_only".to_string(), Value::Bool(submit_only));
    payload.insert(bytes_field.to_string(), Value::Number(bytes.into()));

    append_orchestration_trace(event_name, Value::Object(payload));
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
    verifier_blocker_ack_message(fields)
}

fn verifier_blocker_ack_message(fields: &BlockerFields) -> Value {
    json!({
        "action": "message",
        "from": "verifier",
        "to": "planner",
        "type": "blocker",
        "status": "blocked",
        "observation": "Inbound blocker received; verifier yielding without further work until resolved.",
        "rationale": "Blocker is not verifier-specific; pausing verification avoids unnecessary work.",
        "predicted_next_actions": verifier_blocker_ack_predicted_next_actions(),
        "payload": verifier_blocker_ack_payload(fields)
    })
}

fn verifier_blocker_ack_payload(fields: &BlockerFields) -> Value {
    build_blocker_payload(
        "Verifier paused due to upstream blocker.",
        &fields.blocker_display,
        &fields.evidence,
        &fields.required_action,
        &fields.severity,
    )
}

fn verifier_blocker_ack_predicted_next_actions() -> Value {
    json!([
        {
            "action": "message",
            "intent": "Resume verification only after planner addresses the upstream blocker and re-handoffs the lane."
        },
        {
            "action": "read_file",
            "intent": "Reinspect the updated planner handoff or affected artifacts after the blocker is resolved."
        }
    ])
}

fn file_modified_ms(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

/// Hash the contents of a set of files to detect net state changes across a cycle.
/// Missing files contribute a fixed sentinel so a file appearing or disappearing
/// also changes the hash.
fn cycle_state_hash(paths: &[&Path]) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    for path in paths {
        match std::fs::read(path) {
            Ok(bytes) => bytes.hash(&mut hasher),
            Err(_) => u8::MAX.hash(&mut hasher),
        }
    }
    hasher.finish()
}

fn write_livelock_report(
    agent_state_dir: &Path,
    stall_cycles: u32,
    watched_paths: &[&Path],
    planner_pending: bool,
    diagnostics_pending: bool,
) {
    let report = build_livelock_report(
        stall_cycles,
        watched_paths,
        planner_pending,
        diagnostics_pending,
    );
    let report_path = agent_state_dir.join("livelock_report.json");
    if let Ok(text) = serde_json::to_string_pretty(&report) {
        let _ = std::fs::write(&report_path, text);
    }
    eprintln!(
        "[orchestrate] livelock detected: {} stall cycles, pending flags cleared, \
         report written to {}",
        stall_cycles,
        report_path.display()
    );
    if let Some(workspace) = agent_state_dir.parent() {
        crate::blockers::record_action_failure(
            workspace,
            "orchestrator",
            "livelock",
            &format!("livelock after {stall_cycles} consecutive no-change cycles"),
            None,
        );
    }
    log_error_event(
        "orchestrate",
        "livelock_detected",
        None,
        &format!(
            "livelock detected after {} consecutive no-change cycles; pending flags cleared",
            stall_cycles
        ),
        Some(json!({
            "stage": "livelock_detected",
            "stall_cycles": stall_cycles,
            "planner_pending": planner_pending,
            "diagnostics_pending": diagnostics_pending,
        })),
    );
}

fn build_livelock_report(
    stall_cycles: u32,
    watched_paths: &[&Path],
    planner_pending: bool,
    diagnostics_pending: bool,
) -> Value {
    json!({
        "timestamp_ms": now_ms(),
        "stall_cycles": stall_cycles,
        "watched_files": watched_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "pending_at_detection": {
            "planner_pending": planner_pending,
            "diagnostics_pending": diagnostics_pending,
        },
        "message": format!(
            "Orchestrator detected {} consecutive cycles where work was dispatched but \
             no watched file changed. Pending flags cleared. Write a wakeup_*.flag or \
             restart to resume.",
            stall_cycles
        ),
    })
}

async fn run_planner_phase(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    verifier_summary: &[String],
    planner_bootstrapped: &mut bool,
    cargo_test_failures: &str,
) -> bool {
    {
        let mut state = std::collections::HashMap::new();
        state.insert("planner_pending".to_string(), dispatch_state.planner_pending.to_string());
        let blockers = crate::blockers::load_blockers(ctx.workspace);
        let now_ms = crate::logging::now_ms();
        let planner_blocker_escalated_count = crate::blockers::count_class_recent(
            &blockers,
            "planner",
            &crate::error_class::ErrorClass::BlockerEscalated,
            now_ms,
            5 * 60 * 1000, // 5 minute window
        );
        // Only inject invariant trigger when entering escalation, not while already blocked.
        // This prevents a poison-state where planner_pending=true causes perpetual re-blocking.
        if planner_blocker_escalated_count >= 3 && !dispatch_state.planner_pending {
            state.insert("actor_kind".to_string(), "planner".to_string());
            state.insert("error_class".to_string(), "blocker_escalated".to_string());
        }
        if let Err(reason) = crate::invariants::evaluate_invariant_gate("planner", &state, ctx.workspace) {
            eprintln!("[invariant_gate] planner G_p (BLOCKED): {reason}");
            crate::blockers::record_action_failure(
                ctx.workspace,
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
            dispatch_state.planner_pending = true;
            return false;
        }
    }

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
    let issues_text = crate::issues::read_top_open_issues(ctx.workspace, 10);
    let mut planner_prompt = planner_cycle_prompt(
        &inputs.summary_text,
        &inputs.objectives_text,
        &inputs.lessons_text,
        &inputs.invariants_text,
        &inputs.violations_text,
        &inputs.diagnostics_text,
        &issues_text,
        &inputs.plan_diff_text,
        &inputs.executor_diff_text,
        &inputs.cargo_test_failures,
    );
    inject_inbound_message(&mut planner_prompt, "planner");
    inject_post_restart_result(&mut planner_prompt, "planner");
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
        true,
        0,
    )
    .await;
    *planner_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!("[orchestrate] planner ok bytes={}", result.len());
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
            dispatch_state.last_plan_text = inputs.plan_text;

            // Semantic preflight: demote ready tasks that reference symbols not
            // found in the workspace graph.  Bounced tasks are reset to
            // `needs_planning` so the planner corrects them next cycle.
            crate::plan_preflight::preflight_ready_tasks(ctx.workspace);

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

async fn run_solo_phase(
    ctx: &OrchestratorContext<'_>,
    solo_bootstrapped: &mut bool,
    cargo_test_failures: &str,
    last_solo_executor_diff: &mut String,
    last_solo_plan_text: &mut String,
) -> bool {
    let spec = match read_required_text(ctx.workspace.join(SPEC_FILE), SPEC_FILE) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("[orchestrate] solo error: {err:#}");
            log_error_event(
                "solo",
                "orchestrate",
                None,
                &format!("solo error: {err:#}"),
                Some(json!({ "stage": "solo_load" })),
            );
            return false;
        }
    };
    let master_plan = crate::prompt_inputs::filter_pending_plan_json(&read_text_or_empty(
        ctx.master_plan_path,
    ));
    let agent_root = crate::constants::agent_state_dir().trim_end_matches("/agent_state");
    let agent_objectives = Path::new(agent_root).join(OBJECTIVES_FILE);
    let objectives = if agent_objectives.exists() {
        crate::objectives::read_objectives_compact(&agent_objectives)
    } else {
        crate::objectives::read_objectives_compact(&ctx.workspace.join(OBJECTIVES_FILE))
    };
    let invariants = read_text_or_empty(ctx.workspace.join(INVARIANTS_FILE));
    let violations = crate::prompt_inputs::filter_active_violations_json(&read_text_or_empty(
        ctx.violations_path,
    ));
    let diagnostics = crate::prompt_inputs::filter_active_diagnostics_json(&read_text_or_empty(
        ctx.diagnostics_path,
    ));
    let objectives_mtime_before = file_modified_ms(&agent_objectives)
        .or_else(|| file_modified_ms(&ctx.workspace.join(OBJECTIVES_FILE)));
    let plan_mtime_before = file_modified_ms(&ctx.workspace.join(MASTER_PLAN_FILE));
    // Compute diffs and ranked context (symmetric with planner cycle)
    let executor_diff_inputs = crate::prompt_inputs::load_executor_diff_inputs(
        ctx.workspace,
        last_solo_executor_diff,
        400,
    );
    let current_plan_text = read_text_or_empty(ctx.master_plan_path);
    let plan_diff_text = crate::prompt_inputs::solo_plan_diff(last_solo_plan_text, &current_plan_text, 400);
    *last_solo_plan_text = current_plan_text;
    let issues_text = crate::issues::read_top_open_issues(ctx.workspace, 5);
    let complexity_hotspots = crate::prompt_inputs::read_complexity_hotspots(ctx.workspace, 8);
    let loop_context_hint = crate::prompt_inputs::read_loop_context_hint(
        std::path::Path::new(crate::constants::agent_state_dir()),
    );
    let mut prompt = single_role_solo_prompt(
        &spec,
        &master_plan,
        &objectives,
        &crate::prompt_inputs::read_lessons_or_empty(ctx.workspace),
        &invariants,
        &violations,
        &diagnostics,
        cargo_test_failures,
        &crate::prompt_inputs::read_rename_candidates_or_empty(ctx.workspace),
        &issues_text,
        &executor_diff_inputs.diff_text,
        &plan_diff_text,
        &complexity_hotspots,
        &loop_context_hint,
    );
    inject_inbound_message(&mut prompt, "solo");
    inject_post_restart_result(&mut prompt, "solo");
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
        true,
        0,
    )
    .await;
    *solo_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!("[orchestrate] solo ok bytes={}", result.len());
            let lessons_text = crate::prompt_inputs::read_lessons_or_empty(ctx.workspace);
            append_orchestration_trace(
                "learning_loop_cycle_audit",
                json!({
                    "phase": "solo",
                    "lessons_present": !lessons_text.trim().is_empty(),
                    "lessons_bytes": lessons_text.len(),
                    "objectives_path": OBJECTIVES_FILE,
                    "plan_path": MASTER_PLAN_FILE,
                }),
            );
            // Minimal enforcement hook (OBJ-15): if lessons exist, require objective/plan follow-up
            if !lessons_text.trim().is_empty() {
                let objectives_mtime_after = file_modified_ms(&agent_objectives)
                    .or_else(|| file_modified_ms(&ctx.workspace.join(OBJECTIVES_FILE)));
                let plan_mtime_after = file_modified_ms(&ctx.workspace.join(MASTER_PLAN_FILE));
                let objective_or_plan_updated = objectives_mtime_before != objectives_mtime_after
                    || plan_mtime_before != plan_mtime_after;
                append_orchestration_trace(
                    "learning_loop_enforcement_signal",
                    json!({
                        "required_action": "objective_or_plan_update_required",
                        "reason": if objective_or_plan_updated {
                            "lessons_present_with_followup_update"
                        } else {
                            "lessons_present_without_verified_followup"
                        },
                        "objective_or_plan_updated": objective_or_plan_updated,
                        "objectives_path": OBJECTIVES_FILE,
                        "plan_path": MASTER_PLAN_FILE,
                    }),
                );
                if !objective_or_plan_updated {
                    log_error_event(
                        "solo",
                        "orchestrate",
                        None,
                        "learning loop follow-up missing: lessons were present but neither objectives nor plan changed during the solo cycle",
                        Some(json!({
                            "stage": "learning_loop_followup_missing",
                            "objectives_path": OBJECTIVES_FILE,
                            "plan_path": MASTER_PLAN_FILE,
                        })),
                    );
                    return false;
                }
            }
            crate::lessons::maybe_synthesize_lessons(ctx.workspace);
            crate::lessons::apply_promoted_lessons(ctx.workspace);
            crate::invariants::maybe_synthesize_invariants(ctx.workspace);
            true
        }
        Err(err) => {
            eprintln!("[orchestrate] solo error: {err:#}");
            log_error_event(
                "solo",
                "orchestrate",
                None,
                &format!("solo error: {err:#}"),
                Some(json!({ "stage": "solo_cycle" })),
            );
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
    {
        let mut state = std::collections::HashMap::new();
        state.insert("diagnostics_pending".to_string(), dispatch_state.diagnostics_pending.to_string());
        let blockers = crate::blockers::load_blockers(ctx.workspace);
        let now_ms = crate::logging::now_ms();
        let diagnostics_verification_failed_count = crate::blockers::count_class_recent(
            &blockers,
            "diagnostics",
            &crate::error_class::ErrorClass::VerificationFailed,
            now_ms,
            5 * 60 * 1000,
        );
        // Only inject when entering failure threshold to avoid livelock
        if diagnostics_verification_failed_count >= 3 && !dispatch_state.diagnostics_pending {
            state.insert("actor_kind".to_string(), "diagnostics".to_string());
            state.insert("error_class".to_string(), "verification_failed".to_string());
        }
        if let Err(reason) = crate::invariants::evaluate_invariant_gate("diagnostics", &state, ctx.workspace) {
            eprintln!("[invariant_gate] diagnostics G_d (BLOCKED): {reason}");
            crate::blockers::record_action_failure(
                ctx.workspace,
                "orchestrator",
                "diagnostics_dispatch",
                &reason,
                None,
            );
            let record = serde_json::json!({
                "kind": "invariant_gate",
                "phase": "diagnostics",
                "gate": "G_d",
                "proposed_role": "diagnostics",
                "blocked": true,
                "reason": reason,
                "ts_ms": crate::logging::now_ms(),
            });
            let _ = crate::logging::append_action_log_record(&record);
            dispatch_state.diagnostics_pending = true;
            return false;
        }
    }
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
        true,
        0,
    )
    .await;
    *diagnostics_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!("[orchestrate] diagnostics ok bytes={}", result.len());
            let raw_diagnostics_text = read_text_or_empty(ctx.diagnostics_path);
            let raw_violations_text = read_text_or_empty(ctx.violations_path);
            let reconciled_diagnostics_text = crate::prompt_inputs::reconcile_diagnostics_report(
                &raw_diagnostics_text,
                &raw_violations_text,
            );
            if reconciled_diagnostics_text != raw_diagnostics_text {
                if let Err(err) = std::fs::write(ctx.diagnostics_path, &reconciled_diagnostics_text) {
                    log_error_event(
                        "diagnostics",
                        "orchestrate",
                        None,
                        &format!("failed to persist reconciled diagnostics: {err:#}"),
                        Some(json!({ "stage": "diagnostics_reconcile_write" })),
                    );
                    return false;
                }
            }
            let new_diagnostics_text = crate::prompt_inputs::sanitize_diagnostics_for_planner(
                &reconciled_diagnostics_text,
                &raw_violations_text,
            );
            let diagnostics_changed = dispatch_state.diagnostics_text != new_diagnostics_text;
            dispatch_state.diagnostics_text = new_diagnostics_text;
            dispatch_state.diagnostics_pending = false;
            dispatch_state.planner_pending =
                decide_post_diagnostics(diagnostics_changed, verifier_changed);
            crate::lessons::maybe_synthesize_lessons(ctx.workspace);
            crate::lessons::apply_promoted_lessons(ctx.workspace);
            crate::invariants::maybe_synthesize_invariants(ctx.workspace);
            true
        }
        Err(err) => {
            eprintln!("[orchestrate] diagnostics error: {err:#}");
            log_error_event(
                "diagnostics",
                "orchestrate",
                None,
                &format!("diagnostics error: {err:#}"),
                Some(json!({ "stage": "diagnostics_cycle" })),
            );
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
                true,
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
                log_error_event(
                    "verifier",
                    "orchestrate",
                    None,
                    &format!("verifier join error: {err:#}"),
                    Some(json!({ "stage": "verifier_join" })),
                );
            }
        }
    }

    (cycle_progress, verifier_changed)
}

fn requeue_lane_after_submit_recovery(dispatch_state: &mut DispatchState, lane_id: usize) {
    let lane = dispatch_lane_mut(dispatch_state, lane_id);
    lane.in_progress_by = None;
    lane.pending = true;
    dispatch_state.executor_submit_inflight.remove(&lane_id);
    dispatch_state.lane_submit_in_flight.insert(lane_id, false);
}

fn register_submitted_executor_turn(
    dispatch_state: &mut DispatchState,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
    submitted_turn: SubmittedExecutorTurn,
) {
    dispatch_state.lane_active_tab.insert(lane_id, tab_id);
    dispatch_state.tab_id_to_lane.entry(tab_id).or_insert(lane_id);
    dispatch_state
        .submitted_turns
        .insert((tab_id, turn_id), submitted_turn);
    dispatch_state.lane_next_submit_at_ms.insert(lane_id, now_ms());
    dispatch_state.lane_submit_in_flight.insert(lane_id, false);
}

fn timed_out_executor_submit_lanes(
    dispatch_state: &DispatchState,
    now: u64,
    pending_submit_timeout_ms: u64,
) -> Vec<usize> {
    let mut timed_out = Vec::new();
    for (lane_id, pending) in dispatch_state.executor_submit_inflight.iter() {
        if executor_submit_timed_out(pending.started_ms, now, pending_submit_timeout_ms) {
            timed_out.push(*lane_id);
        }
    }
    timed_out
}

fn log_timed_out_executor_submit(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    pending: PendingSubmitState,
) {
    eprintln!(
        "[orchestrate] pending submit timeout: lane={} command_id={}",
        ctx.lanes[lane_id].label,
        pending.command_id
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &format!(
            "pending submit timeout: lane={} command_id={}",
            ctx.lanes[lane_id].label,
            pending.command_id
        ),
        Some(json!({
            "stage": "executor_submit_timeout",
            "lane": ctx.lanes[lane_id].label,
            "command_id": pending.command_id,
        })),
    );
    append_orchestration_trace(
        "executor_submit_timeout",
        json!({
            "lane_name": ctx.lanes[lane_id].label,
            "command_id": pending.command_id,
        }),
    );
    crate::blockers::record_action_failure(
        ctx.workspace.as_path(),
        "executor",
        "executor_submit_timeout",
        &format!(
            "executor submit timed out: lane={} command_id={}",
            ctx.lanes[lane_id].label,
            pending.command_id
        ),
        None,
    );
}

fn recover_timed_out_executor_submit_lane(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    lane_id: usize,
) {
    if let Some(pending) = dispatch_state.executor_submit_inflight.remove(&lane_id) {
        log_timed_out_executor_submit(ctx, lane_id, pending);
    }
    dispatch_state.lane_submit_in_flight.insert(lane_id, false);
    let lane = dispatch_lane_mut(dispatch_state, lane_id);
    lane.in_progress_by = None;
    lane.pending = true;
}

fn sweep_timed_out_executor_submits(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    now: u64,
    pending_submit_timeout_ms: u64,
) {
    if dispatch_state.executor_submit_inflight.is_empty() {
        return;
    }
    let timed_out = timed_out_executor_submit_lanes(dispatch_state, now, pending_submit_timeout_ms);
    for lane_id in timed_out {
        recover_timed_out_executor_submit_lane(ctx, dispatch_state, lane_id);
    }
}

fn dispatch_executor_submits(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    now: u64,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
    scheduled_phase: Option<&str>,
    current_phase: &mut String,
    current_phase_lane: &mut Option<usize>,
) {
    if block_executor_dispatch(scheduled_phase) {
        return;
    }

    // No-ready-tasks guard: if PLAN.json has no ready tasks, skip executor dispatch
    // entirely and wake the planner instead.  This eliminates the idle turn where
    // the executor discovers no work and sends an empty handoff message back.
    {
        let ws = std::path::PathBuf::from(workspace());
        let ready_tasks_text = crate::prompt_inputs::read_ready_tasks(&ws, 1);
        let ready_count = if ready_tasks_text == "(no ready tasks)" { "0" } else { "1+" };
        if ready_count == "0" {
            dispatch_state.planner_pending = true;
            return;
        }

        // Route gate G_r: check enforced invariants before dispatching the executor.
        // Currently observational — violations are logged but do not hard-block.
        // Once invariants accumulate enough support, this will become a hard gate.
        {
            let mut state = std::collections::HashMap::new();
            state.insert("ready_tasks".to_string(), ready_count.to_string());
            let blockers = crate::blockers::load_blockers(&ws);
            let now_ms = crate::logging::now_ms();
            let solo_invalid_schema_count = crate::blockers::count_class_recent(
                &blockers,
                "solo",
                &crate::error_class::ErrorClass::InvalidSchema,
                now_ms,
                5 * 60 * 1000, // 5 minute window
            );
            if solo_invalid_schema_count >= 3 {
                state.insert("actor_kind".to_string(), "solo".to_string());
                state.insert("error_class".to_string(), "invalid_schema".to_string());
            }
            let executor_invalid_schema_count = crate::blockers::count_class_recent(
                &blockers,
                "executor",
                &crate::error_class::ErrorClass::InvalidSchema,
                now_ms,
                5 * 60 * 1000, // 5 minute window
            );
            if executor_invalid_schema_count >= 3 {
                state.insert("actor_kind".to_string(), "executor".to_string());
                state.insert("error_class".to_string(), "invalid_schema".to_string());
            }
            let missing_target_count = blockers
                .blockers
                .iter()
                .filter(|b| {
                    now_ms.saturating_sub(b.ts_ms) <= 5 * 60 * 1000
                        && matches!(
                            b.error_class,
                            crate::error_class::ErrorClass::MissingTarget
                        )
                })
                .count();
            if missing_target_count >= 1 {
                state.insert("actor_kind".to_string(), "any".to_string());
                state.insert("error".to_string(), "missing_target".to_string());
            }
            // Detect orchestrator livelock conditions and surface into invariant state
            let livelock_count = crate::blockers::count_class_recent(
                &blockers,
                "orchestrator",
                &crate::error_class::ErrorClass::LivelockDetected,
                now_ms,
                5 * 60 * 1000,
            );
            if livelock_count >= 1 {
                state.insert("actor_kind".to_string(), "orchestrator".to_string());
                state.insert("error_class".to_string(), "livelock".to_string());
            }
            if let Err(reason) = crate::invariants::evaluate_invariant_gate("executor", &state, &ws) {
                eprintln!("[invariant_gate] route G_r (BLOCKED): {reason}");
                // Record failure for invariant synthesis
                crate::blockers::record_action_failure(
                    &ws,
                    "orchestrator",
                    "route_dispatch",
                    &reason,
                    None,
                );
                // Log the gate hit with blocking=true
                let record = serde_json::json!({
                    "kind": "invariant_gate",
                    "phase": "route",
                    "gate": "G_r",
                    "proposed_role": "executor",
                    "blocked": true,
                    "reason": reason,
                    "ts_ms": crate::logging::now_ms(),
                });
                let _ = crate::logging::append_action_log_record(&record);
                // HARD BLOCK: prevent executor dispatch when invariant gate fails
                dispatch_state.planner_pending = true;
                return;
            }
        }
    }

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
    sweep_timed_out_executor_submits(ctx, dispatch_state, now, pending_submit_timeout_ms);
    dispatch_executor_submits(
        ctx,
        dispatch_state,
        now,
        submit_joinset,
        scheduled_phase,
        current_phase,
        current_phase_lane,
    );

    while let Some(joined) = submit_joinset.try_join_next() {
        match joined {
            Ok((lane_id, job, result)) => {
                if handle_executor_submit_join_result(
                    ctx,
                    dispatch_state,
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
    dispatch_state: &mut DispatchState,
    lane_id: usize,
    job: PendingExecutorSubmit,
    result: Result<String>,
    pending_submit_timeout_ms: u64,
) -> bool {
    match result {
        Ok(exec_result) => handle_executor_submit_ack_result(
            ctx,
            dispatch_state,
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
            requeue_lane_after_submit_recovery(dispatch_state, job.lane_index);
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
    dispatch_state: &mut DispatchState,
    job: &PendingExecutorSubmit,
    exec_result: &str,
) -> bool {
    log_missing_submit_ack(job, exec_result);
    // Recovery: clear stuck ownership and requeue lane
    requeue_lane_after_submit_recovery(dispatch_state, job.lane_index);
    false
}

fn log_late_submit_ack(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
) {
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
    dispatch_state: &mut DispatchState,
    lane_id: usize,
    job: &PendingExecutorSubmit,
    tab_id: u32,
    turn_id: u64,
    command_id: Option<String>,
) -> bool {
    log_late_submit_ack(ctx, lane_id, tab_id, turn_id);
    register_submitted_executor_turn(
        dispatch_state,
        lane_id,
        tab_id,
        turn_id,
        SubmittedExecutorTurn {
            tab_id,
            lane: job.lane_index,
            lane_label: job.label.clone(),
            command_id: command_id
                .unwrap_or_else(|| make_command_id(&job.executor_role, "executor", 1)),
            actor: job.executor_role.clone(),
            endpoint_id: job.endpoint_id.clone(),
            tabs: job.tabs.clone(),
            steps_used: dispatch_state.lane_steps_used(job.lane_index),
        },
    );
    true
}

fn log_submit_ack_timeout(
    ctx: &OrchestratorContext<'_>,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
) {
    let lane_label = &ctx.lanes[lane_id].label;
    log_submit_ack_event(
        format!(
            "submit ack arrived after timeout: lane={} tab_id={} turn_id={}",
            lane_label,
            tab_id,
            turn_id
        ),
        json!({
            "stage": "executor_submit_ack_timeout",
            "lane": lane_label,
            "tab_id": tab_id,
            "turn_id": turn_id,
        }),
    );
}

fn handle_submit_ack_timeout(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
) -> bool {
    log_submit_ack_timeout(ctx, lane_id, tab_id, turn_id);
    crate::blockers::record_action_failure(
        ctx.workspace.as_path(),
        "executor",
        "submit_ack_timeout",
        &format!(
            "submit ack timed out: lane={} tab_id={} turn_id={}",
            ctx.lanes[lane_id].label, tab_id, turn_id
        ),
        None,
    );
    dispatch_state.lane_submit_in_flight.insert(lane_id, false);
    dispatch_state.lane_prompt_in_flight.insert(lane_id, false);
    false
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
            "submit ack tab mismatch: lane={} active_tab={} ack_tab={} (overwriting active tab)",
            lane_label,
            active_tab,
            tab_id
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

fn handle_executor_submit_ack_result(
    ctx: &OrchestratorContext<'_>,
    dispatch_state: &mut DispatchState,
    lane_id: usize,
    job: PendingExecutorSubmit,
    exec_result: String,
    pending_submit_timeout_ms: u64,
) -> bool {
    let Some((tab_id, turn_id, command_id)) = parse_submit_ack(&exec_result) else {
        return handle_missing_submit_ack(dispatch_state, &job, &exec_result);
    };

    let Some(pending) = dispatch_state.executor_submit_inflight.remove(&lane_id) else {
        // The timeout path already removed executor_submit_inflight for
        // this lane, but the submit actually succeeded.  Register the turn
        // so the completion can still be routed back to the LLM.
        return register_late_submit_ack(
            ctx,
            dispatch_state,
            lane_id,
            &job,
            tab_id,
            turn_id,
            command_id,
        );
    };

    if executor_submit_timed_out(
        pending.started_ms,
        now_ms(),
        pending_submit_timeout_ms,
    ) {
        return handle_submit_ack_timeout(ctx, dispatch_state, lane_id, tab_id, turn_id);
    }

    if let Some(active_tab) = dispatch_state.lane_active_tab(lane_id) {
        if active_tab != tab_id {
            log_submit_ack_tab_mismatch(ctx, lane_id, active_tab, tab_id);
        }
    }

    register_submitted_executor_turn(
        dispatch_state,
        lane_id,
        tab_id,
        turn_id,
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
    true
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
            Ok((submitted, turn_id, result)) => {
                cycle_progress |= handle_completed_continuation(
                    dispatch_state,
                    verifier_pending_results,
                    submitted,
                    turn_id,
                    result,
                );
            }
            Err(err) => {
                eprintln!("[orchestrate] continuation join error: {err:#}");
                log_error_event(
                    "orchestrate",
                    "orchestrate",
                    None,
                    &format!("continuation join error: {err:#}"),
                    Some(json!({ "stage": "continuation_join" })),
                );
            }
        }
    }
    cycle_progress
}

fn handle_completed_continuation(
    dispatch_state: &mut DispatchState,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    submitted: SubmittedExecutorTurn,
    turn_id: u64,
    result: Result<String>,
) -> bool {
    match result {
        Ok(final_exec_result) => {
            dispatch_state.lane_prompt_in_flight.insert(submitted.lane, false);
            // Continuations only return once the executor has reached completion,
            // and the returned value is the completion summary (not the raw action JSON).
            verifier_pending_results.push_back((submitted, turn_id, final_exec_result));
        }
        Err(err) => {
            let err_text = format!("{err:#}");
            recover_failed_continuation(dispatch_state, &submitted, &err_text);
        }
    }
    true
}

fn recover_failed_continuation(
    dispatch_state: &mut DispatchState,
    submitted: &SubmittedExecutorTurn,
    err_text: &str,
) {
    eprintln!(
        "[orchestrate] executor continuation error: lane={} err={}",
        submitted.lane_label,
        err_text
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &format!(
            "executor continuation error: lane={} err={}",
            submitted.lane_label,
            err_text
        ),
        Some(json!({ "stage": "executor_continuation", "lane": submitted.lane_label })),
    );
    dispatch_state.lane_prompt_in_flight.insert(submitted.lane, false);
    let lane = dispatch_lane_mut(dispatch_state, submitted.lane);
    lane.in_progress_by = None;
    lane.pending = true;
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
        "Repeated failures detected. You cannot proceed without external action.\nReturn exactly one action that reports a blocker using this schema:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"{from}\",\n  \"to\": \"{to}\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Summarize the blocked state based on evidence.\",\n  \"rationale\": \"Explain why you cannot proceed.\",\n  \"predicted_next_actions\": [\n    {{\n      \"action\": \"read_file\",\n      \"intent\": \"Reinspect the blocking artifact after the required external fix lands.\"\n    }},\n    {{\n      \"action\": \"message\",\n      \"intent\": \"Report completion or a narrower blocker once the external fix is available.\"\n    }}\n  ],\n  \"payload\": {{\n    \"summary\": \"Short blocker summary.\",\n    \"blocker\": \"Root cause that prevents progress.\",\n    \"evidence\": \"{evidence}\",\n    \"required_action\": \"What must be fixed to continue.\",\n    \"severity\": \"error\"\n  }}\n}}\n```\nTask context:\n{context}\nReturn exactly one action.",
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

fn shutdown_signal_cell() -> &'static OnceLock<ShutdownSignal> {
    &SHUTDOWN_SIGNAL
}

fn init_shutdown_signal() -> ShutdownSignal {
    shutdown_signal_cell()
        .get_or_init(|| ShutdownSignal {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        })
        .clone()
}

fn shutdown_signal() -> Option<ShutdownSignal> {
    shutdown_signal_cell().get().cloned()
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
    #[serde(default)]
    last_solo_plan_text: String,
    #[serde(default)]
    last_solo_executor_diff: String,
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

fn orchestrator_mode_flag_path() -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir()).join("orchestrator_mode.flag")
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
    let lane_snapshots = build_checkpoint_lane_snapshots(dispatch_state, lanes);
    let resume_items = build_resume_verifier_items(lanes, verifier_pending_results);
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
        last_solo_plan_text: dispatch_state.last_solo_plan_text.clone(),
        last_solo_executor_diff: dispatch_state.last_solo_executor_diff.clone(),
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

fn build_checkpoint_lane_snapshots(
    dispatch_state: &DispatchState,
    lanes: &[LaneConfig],
) -> Vec<CheckpointLane> {
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
    lane_snapshots
}

fn build_resume_verifier_items(
    lanes: &[LaneConfig],
    verifier_pending_results: &VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> Vec<ResumeVerifierItem> {
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
    resume_items
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
        return Some(guardrail_reaction_only_action(role));
    }
    if looks_like_diff(raw) {
        return Some(guardrail_diff_message_action(raw, role));
    }
    None
}

fn guardrail_reaction_only_action(role: &str) -> Value {
    let path = if role == "diagnostics" {
        "<workspace-local log/state artifacts discovered during diagnostics>"
    } else {
        "canon-utils"
    };
    json!({
        "action": "list_dir",
        "observation": "Received reaction-only response; forcing a concrete discovery action.",
        "rationale": "Reaction-only responses are invalid; gather fresh evidence instead.",
        "path": path
    })
}

fn guardrail_diff_message_action(raw: &str, role: &str) -> Value {
    let (from, to, msg_type, status) = default_message_route(role);
    json!({
        "action": "message",
        "from": from,
        "to": to,
        "type": msg_type,
        "status": status,
        "observation": "Model responded with diff-only text; wrapping as message payload.",
        "rationale": "Diff output must be wrapped in a valid message action.",
        "payload": guardrail_diff_message_payload(raw, from, to, msg_type, status)
    })
}

fn guardrail_diff_message_payload(
    raw: &str,
    from: &str,
    to: &str,
    msg_type: &str,
    status: &str,
) -> Value {
    json!({
        "summary": "diff-only output captured",
        "diff_excerpt": truncate(raw, 1500),
        "expected_format": expected_message_format(from, to, msg_type, status)
    })
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
        Ok(actions) => actions,
        Err(e) => {
            return handle_parse_actions_error(
                role,
                step,
                raw,
                allow_guardrail,
                &log,
                &trace,
                &e.to_string(),
            );
        }
    };

    let mut action = extract_single_action(role, step, raw, actions, &log, &trace)?;
    let raw_action = action.clone();

    normalize_action_or_feedback(role, raw, &raw_action, &mut action, &log)?;

    if allow_auto_fill_message {
        auto_fill_message_fields(&mut action, role);
    }

    // Always run the base-schema autofill so missing provenance fields (rationale,
    // predicted_next_actions, intent, task_id, objective_id) are populated before
    // validation.  This breaks the schema-rejection→identical-retry loop without
    // suppressing real structural errors.
    ensure_action_base_schema(&mut action);

    validate_action_or_feedback(role, raw, &action, &log)?;

    Ok(action)
}

fn handle_parse_actions_error(
    role: &str,
    step: usize,
    raw: &str,
    allow_guardrail: bool,
    log: &impl Fn(&str, Value),
    trace: &impl Fn(&str),
    err_text: &str,
) -> Result<Value, InvalidActionFeedback> {
    if let Some(guard_action) = maybe_guardrail_parse_action(role, raw, allow_guardrail, log, err_text)
    {
        return Ok(guard_action);
    }

    eprintln!(
        "[{role}] step={} parse_error: {}\n[{role}] step={} parse_error_raw: {}",
        step,
        err_text,
        step,
        truncate(raw, MAX_SNIPPET)
    );
    log(
        "llm_parse_error",
        json!({ "error": err_text, "raw": truncate(raw, MAX_SNIPPET) }),
    );
    trace("parse_error");
    Err(InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: build_invalid_action_feedback(None, err_text, role),
    })
}

fn maybe_guardrail_parse_action(
    role: &str,
    raw: &str,
    allow_guardrail: bool,
    log: &impl Fn(&str, Value),
    err_text: &str,
) -> Option<Value> {
    if !allow_guardrail {
        return None;
    }
    let guard_action = guardrail_action_from_raw(raw, role)?;
    log(
        "llm_guardrail_action",
        json!({
            "error": err_text,
            "raw": truncate(raw, MAX_SNIPPET),
            "action": guard_action,
        }),
    );
    Some(guard_action)
}

fn extract_single_action(
    role: &str,
    step: usize,
    raw: &str,
    actions: Vec<Value>,
    log: &impl Fn(&str, Value),
    trace: &impl Fn(&str),
) -> Result<Value, InvalidActionFeedback> {
    if actions.len() != 1 {
        let msg = format!("Got {} actions — emit exactly one action per turn.", actions.len());
        eprintln!("[{role}] step={} {msg}", step);
        log(
            "llm_invalid_action_count",
            json!({ "action_count": actions.len(), "raw": truncate(raw, MAX_SNIPPET) }),
        );
        trace("invalid_action_count");
        return Err(InvalidActionFeedback {
            err_text: msg.clone(),
            feedback: build_invalid_action_feedback(None, &msg, role),
        });
    }

    Ok(actions.into_iter().next().expect("validated single action"))
}

fn normalize_action_or_feedback(
    role: &str,
    raw: &str,
    raw_action: &Value,
    action: &mut Value,
    log: &impl Fn(&str, Value),
) -> Result<(), InvalidActionFeedback> {
    if let Err(e) = normalize_action(action) {
        let err_text = e.to_string();
        log(
            "llm_invalid_action",
            json!({
                "stage": "normalize_action",
                "error": err_text,
                "raw": truncate(raw, MAX_SNIPPET),
            }),
        );
        return Err(InvalidActionFeedback {
            err_text: err_text.clone(),
            feedback: format!(
                "{}\nFor any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise. Return exactly one action.",
                build_invalid_action_feedback(Some(raw_action), &err_text, role)
            ),
        });
    }

    Ok(())
}

fn validate_action_or_feedback(
    role: &str,
    raw: &str,
    action: &Value,
    log: &impl Fn(&str, Value),
) -> Result<(), InvalidActionFeedback> {
    if let Err(e) = validate_action(action) {
        return Err(handle_invalid_action_error(role, raw, action, log, &e.to_string()));
    }

    Ok(())
}

fn handle_invalid_action_error(
    role: &str,
    raw: &str,
    action: &Value,
    log: &impl Fn(&str, Value),
    err_text: &str,
) -> InvalidActionFeedback {
    log(
        "llm_invalid_action",
        json!({
            "stage": "validate_action",
            "error": err_text,
            "raw": truncate(raw, MAX_SNIPPET),
            "action": action.clone(),
        }),
    );
    if let Some(prompt) = corrective_invalid_action_prompt(action, err_text, role) {
        return invalid_action_feedback_with_prompt(action, err_text, role, &prompt);
    }
    if err_text.contains("cargo_test missing 'crate'") {
        return cargo_test_missing_crate_feedback(err_text);
    }
    invalid_action_feedback(action, err_text, role)
}

fn invalid_action_feedback(action: &Value, err_text: &str, role: &str) -> InvalidActionFeedback {
    InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: build_invalid_action_feedback(Some(action), err_text, role),
    }
}

fn invalid_action_feedback_with_prompt(
    action: &Value,
    err_text: &str,
    role: &str,
    prompt: &str,
) -> InvalidActionFeedback {
    InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: format!(
            "{}\n\n{}",
            build_invalid_action_feedback(Some(action), err_text, role),
            prompt
        ),
    }
}

fn cargo_test_missing_crate_feedback(err_text: &str) -> InvalidActionFeedback {
    InvalidActionFeedback {
        err_text: err_text.to_string(),
        feedback: format!(
            "Invalid action: {err_text}\nCorrective action required: `cargo_test` must include a `crate` field.\nUse this exact format and fill in the crate name:\n```json\n{{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-mini-agent\",\n  \"task_id\": \"<plan task id>\",\n  \"objective_id\": \"<objective id>\",\n  \"intent\": \"Run verification for the current task after the latest change.\",\n  \"observation\": \"Running canon-mini-agent test suite after latest changes.\",\n  \"rationale\": \"Validate that canon-mini-agent tests pass after the latest change.\",\n  \"predicted_next_actions\": [\n    {{\"action\": \"read_file\", \"intent\": \"Inspect the failing source or artifact if the test still fails.\"}},\n    {{\"action\": \"apply_patch\", \"intent\": \"Patch the verified defect if the test output identifies a code issue.\"}}\n  ]\n}}\n```\nFor any mutating retry (`apply_patch`, `plan`, `objectives`, `issue`, or `rename_symbol`), include a non-empty `question` field stating the decision-boundary premise.\nReturn exactly one action."
        ),
    }
}

#[derive(Clone, Debug, Default)]
struct ActionProvenance {
    task_id: Option<String>,
    objective_id: Option<String>,
    intent: Option<String>,
}

impl ActionProvenance {
    fn from_action(action: &Value) -> Self {
        Self {
            task_id: action_task_id(action).map(str::to_string),
            objective_id: action_objective_id(action).map(str::to_string),
            intent: action_intent(action).map(str::to_string),
        }
    }
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
    last_provenance: &ActionProvenance,
    total_steps: usize,
    last_predicted_next_actions: Option<&str>,
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
        let role_schema = if send_system_prompt {
            system_instructions.to_string()
        } else {
            String::new()
        };
        (
            role_schema,
            action_result_prompt(
                last_tab_id,
                last_turn_id,
                agent_type.as_str(),
                &result,
                last_action,
                last_provenance.task_id.as_deref(),
                last_provenance.objective_id.as_deref(),
                last_provenance.intent.as_deref(),
                if role.starts_with("executor") {
                    Some(total_steps)
                } else {
                    None
                },
                last_predicted_next_actions,
            ),
        )
    }
}

fn enforce_executor_step_limit(
    role: &str,
    total_steps: usize,
    error_streak: &mut usize,
    last_result: &mut Option<String>,
    workspace: &std::path::Path,
) -> bool {
    if role.starts_with("executor") && executor_step_limit_exceeded(total_steps, EXECUTOR_STEP_LIMIT)
    {
        *error_streak = error_streak.saturating_add(1);
        *last_result = Some(executor_step_limit_feedback());
        crate::blockers::record_action_failure(
            workspace,
            role,
            "step_limit",
            &format!("executor reached step limit ({EXECUTOR_STEP_LIMIT})"),
            None,
        );
        return true;
    }
    false
}

fn executor_step_limit_feedback() -> String {
    format!(
        "Step limit reached: executor must send a message to planner after {EXECUTOR_STEP_LIMIT} actions. Send exactly one `message` action now.\n\nRequired schema:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"handoff\" | \"blocker\",\n  \"status\": \"complete\" | \"blocked\",\n  \"observation\": \"What happened, based only on evidence.\",\n  \"rationale\": \"Why planner must act next.\",\n  \"payload\": {{\n    \"summary\": \"Short summary\",\n    \"evidence\": \"Concrete evidence or artifact paths\"\n  }}\n}}\n```\n\nExample complete handoff:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"handoff\",\n  \"status\": \"complete\",\n  \"observation\": \"Completed the assigned executor work and gathered verification evidence.\",\n  \"rationale\": \"Planner should record completion and schedule the next ready task.\",\n  \"payload\": {{\n    \"summary\": \"Executor work is complete.\",\n    \"evidence\": \"Include files changed, commands run, and test results.\"\n  }}\n}}\n```\n\nExample blocker:\n```json\n{{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Progress is blocked by a concrete failure.\",\n  \"rationale\": \"Planner must resolve the blocker before more executor actions.\",\n  \"payload\": {{\n    \"summary\": \"Executor is blocked.\",\n    \"blocker\": \"Root cause\",\n    \"evidence\": \"Exact error text or failed command\",\n    \"required_action\": \"What planner should do next\"\n  }}\n}}\n```"
    )
}

fn enforce_diagnostics_python(
    role: &str,
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
    // Broaden guard: before diagnostics Python scan completes, ONLY allow
    // read-only or scan-establishing actions. Block everything else.
    if !matches!(kind, "python" | "read_file") {
        return Some(format!(
            "Before writing diagnostics or finishing, run a `python` action earlier in this diagnostics cycle that discovers and analyzes workspace-local log/state artifacts under {} to find errors, inconsistencies, invariant violations, repeated failure patterns, and concrete repair targets. The scan may occur before read_file steps; it does not need to be the immediately previous action.",
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

    let (inputs, path_map) = collect_wake_flag_inputs(agent_state_dir);

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
            // Do NOT clear in_progress_by here. If the lane already has a submit
            // in flight, clearing ownership causes a double-submit on the next tick
            // (claim_next_lane sees pending=true + in_progress_by=None and spawns a
            // second request while the first is still running). The wake effect is
            // preserved: once the in-flight turn completes and in_progress_by is
            // cleared by the normal completion path, pending=true ensures the lane
            // is claimed again immediately.
        }
    }
}

fn collect_wake_flag_inputs(
    agent_state_dir: &std::path::Path,
) -> (
    Vec<WakeFlagInput>,
    std::collections::HashMap<&'static str, std::path::PathBuf>,
) {
    let flag_paths = [
        ("planner", agent_state_dir.join("wakeup_planner.flag")),
        ("solo", agent_state_dir.join("wakeup_solo.flag")),
        ("verifier", agent_state_dir.join("wakeup_verifier.flag")),
        ("diagnostics", agent_state_dir.join("wakeup_diagnostics.flag")),
        ("executor", agent_state_dir.join("wakeup_executor.flag")),
    ];

    let mut inputs = Vec::new();
    let mut path_map = std::collections::HashMap::new();
    for (role, path) in flag_paths {
        if !path.exists() {
            continue;
        }
        let modified_ms = path
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            })
            .unwrap_or(0);
        inputs.push(WakeFlagInput { role, modified_ms });
        path_map.insert(role, path);
    }

    (inputs, path_map)
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

fn full_exchange_path(exchange_id: &str, suffix: &str) -> PathBuf {
    let safe_id = exchange_id.replace(':', "_");
    let ts = exchange_id
        .rsplit(':')
        .next()
        .and_then(|v| v.parse::<u128>().ok())
        .unwrap_or(0);
    PathBuf::from(crate::constants::agent_state_dir())
        .join("llm_full")
        .join(format!("{ts}_{safe_id}_{suffix}.json"))
}

fn write_full_exchange(exchange_id: &str, suffix: &str, payload: &Value) {
    let path = full_exchange_path(exchange_id, suffix);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(payload) {
        let _ = std::fs::write(path, text);
    }
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
        write_full_exchange(
            exchange_id,
            "prompt",
            &json!({
                "role": self.role,
                "prompt_kind": self.prompt_kind,
                "submit_only": self.submit_only,
                "prompt": prompt,
                "role_schema": role_schema,
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
        write_full_exchange(
            exchange_id,
            "response",
            &json!({
                "role": self.role,
                "prompt_kind": self.prompt_kind,
                "submit_only": self.submit_only,
                "raw": raw,
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
    let executor_system = system_instructions(AgentPromptKind::Executor);

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
            crate::blockers::record_action_failure(
                workspace,
                role,
                "schema_validation",
                &invalid.err_text,
                None,
            );
            let agent_type = role.to_uppercase();
            let retry_prompt = action_result_prompt(
                Some(active_tab_id),
                Some(turn_id),
                agent_type.as_str(),
                &invalid.feedback,
                Some("invalid_action"),
                None,
                None,
                None,
                Some(submitted.steps_used),
                None,
            );
            return run_agent(
                role,
                prompt_kind,
                &executor_system,
                retry_prompt,
                endpoint,
                bridge,
                workspace,
                tabs,
                false,
                true,
                true,
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
    let provenance = ActionProvenance::from_action(&action);
    run_agent(
        role,
        prompt_kind,
        &executor_system,
        action_result_prompt(
            Some(active_tab_id),
            Some(turn_id),
            agent_type.as_str(),
            &out,
            action.get("action").and_then(|v| v.as_str()),
            provenance.task_id.as_deref(),
            provenance.objective_id.as_deref(),
            provenance.intent.as_deref(),
            Some(submitted.steps_used),
            None,
        ),
        endpoint,
        bridge,
        workspace,
        tabs,
        false,
        true,
        true,
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
    let mut last_provenance = ActionProvenance::default();
    let mut last_predicted_next_actions: Option<String> = None;
    let mut error_streak: usize = 0;
    #[allow(unused_assignments)]
    let mut last_error: Option<String> = None;
    let mut reaction_only_streak: usize = 0;
    let mut cargo_test_gate = CargoTestGate::new();
    let task_context = initial_prompt.clone();
    let mut diagnostics_eventlog_python_done = false;
    let mut idle_streak = 0usize;
    let mut repeated_failed_action_fingerprint: Option<String> = None;
    let mut last_failed_action_fingerprint: Option<String> = None;
    let mut repeated_failed_action_count: usize = 0;
    let shutdown = shutdown_signal();
    let ctx = LlmResponseContext {
        role,
        endpoint,
        prompt_kind,
        submit_only,
    };

    write_stage_graph(workspace);
    write_tool_examples(workspace);

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
            last_result = Some(executor_step_limit_feedback());
            crate::blockers::record_action_failure(
                workspace,
                role,
                "step_limit",
                &format!("executor reached step limit ({EXECUTOR_STEP_LIMIT})"),
                None,
            );
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
            &last_provenance,
            total_steps,
            last_predicted_next_actions.as_deref(),
        );
        let exchange_id = make_command_id(role, prompt_kind, step + 1);

        eprintln!("[{role}] step={} prompt_bytes={}", step + 1, prompt.len());
        crate::logging::record_prompt_overflow(workspace, role, prompt.len());
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
                crate::blockers::record_action_failure(
                    workspace,
                    role,
                    "llm_request",
                    &e.to_string(),
                    None,
                );
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
            crate::blockers::record_action_failure(
                workspace,
                role,
                "reaction_only",
                "LLM returned prose without a JSON action block",
                None,
            );
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
            true,  // always auto-fill message fields so `from` is forced to the actual role
            None,
        ) {
            Ok(action) => action,
            Err(invalid) => {
                crate::blockers::record_action_failure(
                    workspace,
                    role,
                    "schema_validation",
                    &invalid.err_text,
                    None,
                );
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
            && enforce_executor_step_limit(role, total_steps, &mut error_streak, &mut last_result, workspace)
        {
            step += 1;
            continue;
        }
        if let Some(msg) = cargo_test_gate.message_blocker_if_needed(&kind, crate::constants::workspace()) {
            crate::blockers::record_action_failure(
                workspace,
                role,
                "build_gate",
                &msg,
                None,
            );
            error_streak = error_streak.saturating_add(1);
            last_result = Some(msg);
            step += 1;
            continue;
        }

        reaction_only_streak = 0;
        error_streak = 0;
        eprintln!("[{role}] step={} action={}", step + 1, kind);
        last_action = Some(kind.clone());
        last_provenance = ActionProvenance::from_action(&action);
        last_predicted_next_actions = action
            .get("predicted_next_actions")
            .and_then(|v| serde_json::to_string(v).ok());
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
        let action_fingerprint = action_retry_fingerprint(&action);

        if repeated_failed_action_fingerprint.as_deref() == Some(action_fingerprint.as_str()) {
            crate::blockers::record_action_failure(
                workspace,
                role,
                "repeated_failed_action",
                &format!("identical action payload failed repeatedly: {}", &action_fingerprint),
                action.get("task_id").and_then(|v| v.as_str()),
            );
            apply_error_result(
                role,
                &task_context,
                &mut error_streak,
                &mut last_error,
                &mut last_result,
                "repeated_failed_action_payload",
                "This exact action payload has failed repeatedly. Choose a different action class or a materially different payload before retrying.".to_string(),
            );
            step += 1;
            continue;
        }

        if role == "solo" {
            let objectives_text = read_text_or_empty(preferred_objectives_path(workspace));
            let plan_text = read_text_or_empty(workspace.join(MASTER_PLAN_FILE));
            if should_reject_solo_self_complete(&action, &objectives_text, &plan_text) {
                crate::blockers::record_action_failure(
                    workspace,
                    role,
                    "solo_completion_gate",
                    "solo attempted self-complete with active objectives and no incomplete plan tasks",
                    None,
                );
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    "solo_completion_requires_plan_work_for_active_objectives",
                    "Create/update PLAN tasks for active objectives, or mark objectives deferred/blocked with rationale.".to_string(),
                );
                step += 1;
                continue;
            }
        }

        if let Some(msg) = enforce_diagnostics_python(
            role,
            kind.as_str(),
            &action,
            &mut diagnostics_eventlog_python_done,
        ) {
            crate::blockers::record_action_failure(
                workspace,
                role,
                "diagnostics_evidence_gate",
                &msg,
                None,
            );
            last_result = Some(msg);
            step += 1;
            continue;
        }

        if is_explicit_idle_action(&action) {
            idle_streak += 1;
            if idle_streak >= 3 {
                crate::blockers::record_action_failure(
                    workspace,
                    role,
                    "idle_streak",
                    "agent stuck: 3 consecutive explicit idle actions with no progress",
                    None,
                );
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
                if out.starts_with("Error executing action:") {
                    if last_failed_action_fingerprint
                        .as_deref()
                        .is_some_and(|f| f == action_fingerprint)
                    {
                        repeated_failed_action_count = repeated_failed_action_count.saturating_add(1);
                    } else {
                        last_failed_action_fingerprint = Some(action_fingerprint.clone());
                        repeated_failed_action_count = 1;
                    }
                    if repeated_failed_action_count >= 2 {
                        repeated_failed_action_fingerprint = Some(action_fingerprint.clone());
                    }
                } else {
                    last_failed_action_fingerprint = None;
                    repeated_failed_action_count = 0;
                    repeated_failed_action_fingerprint = None;
                }
                // Persist the last action result so it can be re-injected into the
                // initial prompt if the supervisor restarts the process mid-cycle
                // (e.g. after apply_patch triggers a binary rebuild).  The file is
                // consumed once on startup and then deleted.
                write_post_restart_result(role, kind.as_str(), &out, step + 1);
                last_result = Some(out);
            }
        }
        step += 1;
    }
}

/// Write the last completed action result to a state file so it survives a
/// supervisor-triggered restart.  Only the most recent action is kept (overwrites).
fn write_post_restart_result(role: &str, action: &str, result: &str, step: usize) {
    let path = std::path::Path::new(crate::constants::agent_state_dir())
        .join("post_restart_result.json");
    let payload = serde_json::json!({
        "role": role,
        "action": action,
        "result": result,
        "step": step,
    });
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&payload).unwrap_or_default());
}

/// Read and consume the post-restart result file.  Returns `Some((action, result, step))`
/// if the file exists and was written by `role`, then deletes the file.
fn take_post_restart_result(role: &str) -> Option<(String, String, usize)> {
    let path = std::path::Path::new(crate::constants::agent_state_dir())
        .join("post_restart_result.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let saved_role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    // Normalise: executor[executor_pool] → executor
    let role_key = if role.starts_with("executor") { "executor" } else { role };
    let saved_key = if saved_role.starts_with("executor") { "executor" } else { saved_role };
    if role_key != saved_key {
        return None;
    }
    let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("(unknown)").to_string();
    let result = v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string();
    let step = v.get("step").and_then(|s| s.as_u64()).unwrap_or(0) as usize;
    // Consume — delete so it isn't re-injected on a second restart
    let _ = std::fs::remove_file(&path);
    Some((action, result, step))
}

/// If a post-restart result exists for this role, append it to the prompt so the
/// agent continues from where it left off rather than re-running checks.
fn inject_post_restart_result(prompt: &mut String, role: &str) {
    if let Some((action, result, step)) = take_post_restart_result(role) {
        eprintln!(
            "[{role}] post-restart: injecting prior action result (action={action} step={step})"
        );
        prompt.push_str(&format!(
            "\n\n---\nRESTART CONTEXT: The agent process was restarted (likely because a source \
             build updated the binary). Your last completed action before restart was:\n\
             Action: `{action}` (step {step})\nResult:\n{result}\n\
             Continue from where you left off — do NOT re-run checks that already passed above.\n---\n"
        ));
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
    last_solo_plan_text: String,
    last_solo_executor_diff: String,
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
        last_solo_plan_text: String::new(),
        last_solo_executor_diff: String::new(),
        lane_next_submit_at_ms,
        lane_submit_in_flight,
    }
}

impl DispatchState {
    fn lane_value_or_default<T: Copy + Default>(
        map: &std::collections::HashMap<usize, T>,
        lane_id: usize,
    ) -> T {
        map.get(&lane_id).copied().unwrap_or_default()
    }

    fn lane_in_flight(&self, lane_id: usize) -> bool {
        Self::lane_value_or_default(&self.lane_prompt_in_flight, lane_id)
    }
    fn lane_submit_active(&self, lane_id: usize) -> bool {
        Self::lane_value_or_default(&self.lane_submit_in_flight, lane_id)
    }
    fn lane_next_submit_ms(&self, lane_id: usize) -> u64 {
        Self::lane_value_or_default(&self.lane_next_submit_at_ms, lane_id)
    }
    fn lane_steps_used(&self, lane_id: usize) -> usize {
        Self::lane_value_or_default(&self.lane_steps_used, lane_id)
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

#[derive(Clone)]
struct PendingExecutorSubmit {
    executor_name: String,
    executor_display: String,
    lane_index: usize,
    label: String,
    latest_verify_result: String,
    executor_role: String,
    // Carried for late-ack recovery: allows submitted_turns registration even
    // after executor_submit_inflight has been cleared by the timeout path.
    endpoint_id: String,
    tabs: TabManagerHandle,
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
    if maybe_defer_executor_completion(
        dispatch_state,
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
    maybe_rebind_executor_completion_tab(dispatch_state, &mut submitted, tab_id, turn_id, lane_name);
    if handle_executor_completion_message_action(dispatch_state, &submitted, lane_cfg, &exec_result) {
        return true;
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
    spawn_executor_completion_continuation(
        dispatch_state,
        &submitted,
        lane_cfg,
        tab_id,
        turn_id,
        &exec_result,
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

fn maybe_defer_executor_completion(
    dispatch_state: &mut DispatchState,
    submitted: &SubmittedExecutorTurn,
    turn_id: u64,
    tab_id: u32,
    exec_result: &str,
    lane_name: &str,
) -> bool {
    if !dispatch_state.lane_in_flight(submitted.lane) {
        return false;
    }
    dispatch_state
        .deferred_completions
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
    dispatch_state: &mut DispatchState,
    submitted: &mut SubmittedExecutorTurn,
    tab_id: u32,
    turn_id: u64,
    lane_name: &str,
) {
    if submitted.tab_id == tab_id {
        return;
    }
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

fn spawn_executor_completion_continuation(
    dispatch_state: &mut DispatchState,
    submitted: &SubmittedExecutorTurn,
    lane_cfg: &LaneConfig,
    tab_id: u32,
    turn_id: u64,
    exec_result: &str,
    bridge: &WsBridge,
    workspace: &PathBuf,
    continuation_joinset: &mut tokio::task::JoinSet<(SubmittedExecutorTurn, u64, Result<String>)>,
) {
    let executor_endpoint = lane_cfg.endpoint.clone();
    let bridge = bridge.clone();
    let workspace = workspace.clone();
    let exec_result = exec_result.to_string();
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
}

fn handle_executor_completion_message_action(
    dispatch_state: &mut DispatchState,
    submitted: &SubmittedExecutorTurn,
    lane_cfg: &LaneConfig,
    exec_result: &str,
) -> bool {
    let Ok(mut actions) = parse_actions(exec_result) else {
        return false;
    };
    if actions.first().and_then(|a| a.get("action")).and_then(|v| v.as_str()) != Some("message") {
        return false;
    }

    dispatch_state.lane_steps_used.insert(submitted.lane, 0);
    let Some(action) = actions.pop() else {
        return false;
    };

    log_action_result(
        &submitted.actor,
        &lane_cfg.endpoint,
        "executor",
        1,
        &submitted.command_id,
        &action,
        true,
        exec_result,
    );
    persist_executor_completion_message(dispatch_state, &action);
    true
}

fn persist_executor_completion_message(dispatch_state: &mut DispatchState, action: &Value) {
    let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");

    // Self-loop guard: an executor message routed back to "executor" would write
    // wakeup_executor.flag, wake the executor next cycle, and then complete again
    // with another self-addressed message — creating an oscillating stall that
    // permanently resets the convergence counter before it can reach the threshold.
    // Redirect such messages to the planner so the loop is broken deterministically.
    let effective_to = if to_role.eq_ignore_ascii_case("executor") {
        eprintln!(
            "[orchestrate] executor→executor message detected; redirecting to planner \
             to break self-wake stall loop"
        );
        "planner"
    } else {
        to_role
    };

    if effective_to.eq_ignore_ascii_case("planner") {
        persist_planner_message(action);
        dispatch_state.planner_pending = true;
        return;
    }

    // Generic wakeup for other targets (verifier, diagnostics, etc.)
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    let to_key = to_role
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let msg_path = agent_state_dir.join(format!("last_message_to_{to_key}.json"));
    let _ = std::fs::write(
        &msg_path,
        serde_json::to_string_pretty(action).unwrap_or_default(),
    );
    let _ = std::fs::write(agent_state_dir.join(format!("wakeup_{to_key}.flag")), "handoff");
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

fn preferred_objectives_path(workspace: &Path) -> PathBuf {
    let agent_root = crate::constants::agent_state_dir().trim_end_matches("/agent_state");
    let agent_objectives = Path::new(agent_root).join(OBJECTIVES_FILE);
    if agent_objectives.exists() {
        agent_objectives
    } else {
        workspace.join(OBJECTIVES_FILE)
    }
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
    let Ok(file) = serde_json::from_str::<crate::objectives::ObjectivesFile>(objectives_text) else {
        return false;
    };
    file.objectives.iter().any(objective_requires_plan_work)
}

fn should_reject_solo_self_complete(action: &Value, objectives_text: &str, plan_text: &str) -> bool {
    let is_complete_message = action.get("action").and_then(|v| v.as_str()) == Some("message")
        && action
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("complete"));
    if !is_complete_message {
        return false;
    }
    has_actionable_objectives(objectives_text) && !plan_has_incomplete_tasks(plan_text)
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
        endpoint_id: lane.endpoint.id.clone(),
        tabs: lane.tabs.clone(),
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
    let ready_tasks = crate::prompt_inputs::read_ready_tasks(
        &std::path::PathBuf::from(workspace()),
        10,
    );
    let mut exec_prompt = executor_cycle_prompt(
        job.executor_display.as_str(),
        job.label.as_str(),
        &job.latest_verify_result,
        &ready_tasks,
    );
    inject_inbound_message(&mut exec_prompt, "executor");
    inject_post_restart_result(&mut exec_prompt, "executor");
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
        log_error_event(
            "orchestrate",
            "startup",
            None,
            &format!("objectives/invariants conversion failed: {err:#}"),
            Some(json!({ "stage": "startup" })),
        );
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
    let bridge = ws_server::spawn(
        ws_addr,
        DEFAULT_RESPONSE_TIMEOUT_SECS,
        DEFAULT_LLM_RETRY_COUNT,
        DEFAULT_LLM_RETRY_DELAY_SECS,
        Arc::new(OnceLock::new()),
    );
    eprintln!("[canon-mini-agent] waiting for Chrome extension on ws://127.0.0.1:{ws_port}");
    bridge.wait_for_connection().await;
    eprintln!("[canon-mini-agent] Chrome extension connected");

    let tabs = llm_worker_new_tabs();

    if orchestrate {
        const SERVICE_POLL_MS: u64 = 500;
        const PENDING_SUBMIT_TIMEOUT_MS: u64 = 10_000;

        let solo_mode = start_role == "solo";
        let _ = std::fs::write(
            orchestrator_mode_flag_path(),
            if solo_mode { "single\n" } else { "orchestrate\n" },
        );

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
            dispatch_state.last_solo_plan_text = checkpoint.last_solo_plan_text;
            dispatch_state.last_solo_executor_diff = checkpoint.last_solo_executor_diff;
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
                // Only clear the submit guard for lanes that have no live inflight entry.
                // If executor_submit_inflight still holds an entry for this lane, the
                // submit_joinset task is still running and will deliver an ack — clearing
                // the guard now would allow a second submit to be spawned while the first
                // is in flight, causing a double-submit and the "submit ack without pending
                // submit" error when the first ack arrives after the inflight map entry has
                // been overwritten by the second submit.
                if !dispatch_state.executor_submit_inflight.contains_key(&lane.index) {
                    dispatch_state.lane_submit_in_flight.insert(lane.index, false);
                }
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

        const STALL_CYCLE_THRESHOLD: u32 = 5;
        let mut stall_count: u32 = 0;

        loop {
            let _ = std::fs::remove_file(cycle_idle_marker_path());
            let mut cycle_progress = false;
            let objectives_mtime_before = file_modified_ms(&workspace.join(OBJECTIVES_FILE));
            let plan_mtime_before = file_modified_ms(&master_plan_path);
            let diagnostics_mtime_before = file_modified_ms(&diagnostics_path);

            let objectives_path = workspace.join(OBJECTIVES_FILE);
            let issues_path = workspace.join(ISSUES_FILE);
            let convergence_watched: [&Path; 5] = [
                master_plan_path.as_path(),
                violations_path.as_path(),
                diagnostics_path.as_path(),
                objectives_path.as_path(),
                issues_path.as_path(),
            ];
            let state_hash_before = cycle_state_hash(&convergence_watched);
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
            let raw_diagnostics_text = read_text_or_empty(&diagnostics_path);
            let raw_violations_text = read_text_or_empty(&violations_path);
            let reconciled_diagnostics_text = crate::prompt_inputs::reconcile_diagnostics_report(
                &raw_diagnostics_text,
                &raw_violations_text,
            );
            let diagnostics_reconciliation_needed =
                reconciled_diagnostics_text != raw_diagnostics_text;
            if diagnostics_reconciliation_needed {
                if let Err(err) = std::fs::write(&diagnostics_path, &reconciled_diagnostics_text) {
                    log_error_event(
                        "orchestrate",
                        "diagnostics_reconcile_preflight",
                        None,
                        &format!("failed to persist reconciled diagnostics before scheduling diagnostics: {err:#}"),
                        Some(json!({ "stage": "diagnostics_reconcile_preflight" })),
                    );
                }
                dispatch_state.diagnostics_pending = true;
            }

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
                if run_solo_phase(
                    &orchestrator_ctx,
                    &mut solo_bootstrapped,
                    &cargo_test_failures,
                    &mut dispatch_state.last_solo_executor_diff,
                    &mut dispatch_state.last_solo_plan_text,
                )
                .await
                {
                    cycle_progress = true;
                } else {
                    dispatch_state.planner_pending = true;
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

            let raw_diagnostics_text = read_text_or_empty(&diagnostics_path);
            let raw_violations_text = read_text_or_empty(&violations_path);
            let reconciled_diagnostics_text = crate::prompt_inputs::reconcile_diagnostics_report(
                &raw_diagnostics_text,
                &raw_violations_text,
            );
            let stale_diagnostics_pending = reconciled_diagnostics_text != raw_diagnostics_text;

            if stale_diagnostics_pending {
                if let Err(err) = std::fs::write(&diagnostics_path, &reconciled_diagnostics_text) {
                    log_error_event(
                        "orchestrate",
                        "diagnostics_reconcile_post_verifier",
                        None,
                        &format!("failed to persist reconciled diagnostics after verifier phase: {err:#}"),
                        Some(json!({ "stage": "diagnostics_reconcile_post_verifier" })),
                    );
                }
            }

            if verifier_changed || stale_diagnostics_pending {
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

            let objectives_mtime_after = file_modified_ms(&workspace.join(OBJECTIVES_FILE));
            let plan_mtime_after = file_modified_ms(&master_plan_path);
            let diagnostics_mtime_after = file_modified_ms(&diagnostics_path);
            let objective_review_required = plan_mtime_before != plan_mtime_after
                || diagnostics_mtime_before != diagnostics_mtime_after;
            let objectives_updated = objectives_mtime_before != objectives_mtime_after;
            if objective_review_required && !objectives_updated {
                append_orchestration_trace(
                    "objective_evolution_enforcement_signal",
                    json!({
                        "required_action": "objective_review_or_update_required",
                        "reason": "plan_or_diagnostics_changed_without_objective_update",
                        "plan_changed": plan_mtime_before != plan_mtime_after,
                        "diagnostics_changed": diagnostics_mtime_before != diagnostics_mtime_after,
                        "objectives_updated": objectives_updated,
                        "objectives_path": OBJECTIVES_FILE,
                        "plan_path": MASTER_PLAN_FILE,
                    }),
                );
                dispatch_state.planner_pending = true;
                cycle_progress = true;
            }

            let objectives_text = read_text_or_empty(&workspace.join(OBJECTIVES_FILE));
            let plan_text = read_text_or_empty(&master_plan_path);
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
                dispatch_state.planner_pending = true;
                cycle_progress = true;
            }

            // Convergence guard: detect cycles where work was dispatched but no watched
            // file changed. Consecutive stalls indicate a livelock.
            //
            // Skip the stall increment when executor turns are still in flight: the
            // browser tab has accepted a submission (submitted_turns) or a submit is
            // being negotiated (executor_submit_inflight / lane_submit_in_flight).
            // In those cases the files will change once the result arrives; counting
            // the cycle as a stall would be a false positive.
            let executor_inflight = !dispatch_state.submitted_turns.is_empty()
                || !dispatch_state.executor_submit_inflight.is_empty()
                || dispatch_state.lane_submit_in_flight.values().any(|&v| v);
            let state_hash_after = cycle_state_hash(&convergence_watched);
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
                        &convergence_watched,
                        dispatch_state.planner_pending,
                        dispatch_state.diagnostics_pending,
                    );
                    dispatch_state.planner_pending = false;
                    dispatch_state.diagnostics_pending = false;
                    stall_count = 0;
                    cycle_progress = false;
                }
            } else {
                stall_count = 0;
            }

            if !cycle_progress {
                let _ = std::fs::write(cycle_idle_marker_path(), "idle\n");
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
        let _ = std::fs::write(cycle_idle_marker_path(), "idle\n");
        println!("message: {reason}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        action_retry_fingerprint, executor_step_limit_feedback, has_actionable_objectives,
        plan_has_incomplete_tasks, should_reject_solo_self_complete, verifier_confirmed_with_plan_text,
        ActionProvenance, DispatchState,
    };
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};

    #[test]
    fn build_agent_prompt_includes_role_schema_on_nonzero_steps_when_enabled() {
        let (schema0, prompt0) = super::build_agent_prompt(
            "planner",
            true,
            0,
            "INIT",
            "SYSTEM",
            None,
            None,
            None,
            None,
            &ActionProvenance::default(),
            0,
            None,
        );
        assert_eq!(schema0, "SYSTEM");
        assert_eq!(prompt0, "INIT");

        let (schema1, prompt1) = super::build_agent_prompt(
            "planner",
            true,
            1,
            "INIT",
            "SYSTEM",
            Some("LAST_RESULT"),
            None,
            None,
            Some("read_file"),
            &ActionProvenance::default(),
            1,
            None,
        );
        assert_eq!(schema1, "SYSTEM");
        assert!(
            prompt1.contains("LAST_RESULT"),
            "prompt must include last result"
        );

        let (schema_disabled, _) = super::build_agent_prompt(
            "planner",
            false,
            1,
            "INIT",
            "SYSTEM",
            Some("LAST_RESULT"),
            None,
            None,
            None,
            &ActionProvenance::default(),
            1,
            None,
        );
        assert!(
            schema_disabled.trim().is_empty(),
            "role_schema must be empty when disabled"
        );
    }

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

    #[test]
    fn executor_step_limit_feedback_includes_message_schema_and_examples() {
        let feedback = executor_step_limit_feedback();
        assert!(feedback.contains("\"action\": \"message\""));
        assert!(feedback.contains("\"type\": \"handoff\" | \"blocker\""));
        assert!(feedback.contains("\"status\": \"complete\" | \"blocked\""));
        assert!(feedback.contains("Example complete handoff"));
        assert!(feedback.contains("Example blocker"));
        assert!(feedback.contains("\"required_action\": \"What planner should do next\""));
    }

    #[test]
    fn actionable_objectives_ignore_deferred_or_blocked_and_done() {
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"done"},
            {"id":"o2","status":"deferred"},
            {"id":"o3","status":"blocked"}
          ]
        }"#;
        assert!(!has_actionable_objectives(objectives));
    }

    #[test]
    fn actionable_objectives_detect_active_entries() {
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"done"},
            {"id":"o2","status":"active"}
          ]
        }"#;
        assert!(has_actionable_objectives(objectives));
    }

    #[test]
    fn solo_complete_rejected_when_objectives_actionable_and_plan_done() {
        let action = json!({
            "action": "message",
            "status": "complete"
        });
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"active"}
          ]
        }"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id":"T1","status":"done"}
          ]
        }"#;
        assert!(should_reject_solo_self_complete(&action, objectives, plan));
    }

    #[test]
    fn solo_complete_not_rejected_when_plan_has_incomplete_tasks() {
        let action = json!({
            "action": "message",
            "status": "complete"
        });
        let objectives = r#"{
          "version": 1,
          "objectives": [
            {"id":"o1","status":"active"}
          ]
        }"#;
        let plan = r#"{
          "version": 1,
          "tasks": [
            {"id":"T1","status":"todo"}
          ]
        }"#;
        assert!(!should_reject_solo_self_complete(&action, objectives, plan));
    }

    #[test]
    fn dispatch_state_accessor_subset_uses_shared_default_helper() {
        let mut bool_map = HashMap::new();
        bool_map.insert(7usize, true);
        assert!(DispatchState::lane_value_or_default(&bool_map, 7));
        assert!(!DispatchState::lane_value_or_default(&bool_map, 8));

        let mut ms_map = HashMap::new();
        ms_map.insert(7usize, 42u64);
        assert_eq!(DispatchState::lane_value_or_default(&ms_map, 7), 42);
        assert_eq!(DispatchState::lane_value_or_default(&ms_map, 8), 0);

        let mut steps_map = HashMap::new();
        steps_map.insert(7usize, 3usize);
        assert_eq!(DispatchState::lane_value_or_default(&steps_map, 7), 3);
        assert_eq!(DispatchState::lane_value_or_default(&steps_map, 8), 0);

        let mut state = DispatchState {
            lanes: HashMap::new(),
            submitted_turns: HashMap::new(),
            executor_submit_inflight: HashMap::new(),
            tab_id_to_lane: HashMap::new(),
            lane_active_tab: HashMap::new(),
            lane_prompt_in_flight: HashMap::new(),
            deferred_completions: HashMap::<usize, VecDeque<_>>::new(),
            lane_steps_used: HashMap::new(),
            diagnostics_pending: false,
            planner_pending: false,
            diagnostics_text: String::new(),
            last_plan_text: String::new(),
            last_executor_diff: String::new(),
            last_solo_plan_text: String::new(),
            last_solo_executor_diff: String::new(),
            lane_next_submit_at_ms: HashMap::new(),
            lane_submit_in_flight: HashMap::new(),
        };

        assert!(!state.lane_in_flight(7));
        assert!(!state.lane_submit_active(7));
        assert_eq!(state.lane_next_submit_ms(7), 0);
        assert_eq!(state.lane_steps_used(7), 0);
        assert_eq!(state.lane_active_tab(7), None);

        state.lane_prompt_in_flight.insert(7, true);
        state.lane_submit_in_flight.insert(7, true);
        state.lane_next_submit_at_ms.insert(7, 42);
        state.lane_steps_used.insert(7, 3);
        state.lane_active_tab.insert(7, 99);

        assert!(state.lane_in_flight(7));
        assert!(state.lane_submit_active(7));
        assert_eq!(state.lane_next_submit_ms(7), 42);
        assert_eq!(state.lane_steps_used(7), 3);
        assert_eq!(state.lane_active_tab(7), Some(99));
    }

    #[test]
    fn action_retry_fingerprint_ignores_volatile_fields() {
        let a = json!({
            "action": "plan",
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "observation": "first",
            "rationale": "r1",
            "question": "q1",
            "predicted_next_actions": [{"action":"read_file","intent":"next"}],
            "command_id": "solo:solo:0001:1"
        });
        let b = json!({
            "action": "plan",
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "observation": "second",
            "rationale": "r2",
            "question": "q2",
            "predicted_next_actions": [{"action":"message","intent":"different"}],
            "command_id": "solo:solo:0002:2"
        });

        assert_eq!(action_retry_fingerprint(&a), action_retry_fingerprint(&b));
    }
}

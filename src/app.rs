use crate::llm_runtime::{
    config::LlmEndpoint,
    tab_management::TabManagerHandle,
    worker::{
        llm_worker_new_tabs, llm_worker_send_request_timeout,
        llm_worker_send_request_with_req_id_timeout,
    },
    ws_server,
    ws_server::WsBridge,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use tokio::sync::Notify;

use crate::canonical_writer::CanonicalWriter;
use crate::constants::{
    diagnostics_file, diagnostics_file_for_instance, lane_plan_file_for_instance,
    set_agent_state_dir, set_workspace, workspace, DEFAULT_AGENT_STATE_DIR,
    DEFAULT_LLM_RETRY_COUNT, DEFAULT_LLM_RETRY_DELAY_SECS, DEFAULT_RESPONSE_TIMEOUT_SECS,
    DIAGNOSTICS_FILE_PATH, ENDPOINT_SPECS, EXECUTOR_STEP_LIMIT, ISSUES_FILE, MASTER_PLAN_FILE,
    MAX_SNIPPET, MAX_STEPS, OBJECTIVES_FILE, ROLE_TIMEOUT_SECS, SPEC_FILE, VIOLATIONS_FILE,
    WS_PORT_CANDIDATES,
};
use crate::engine::process_action_and_execute;
use crate::events::{ControlEvent, EffectEvent, Event};
use crate::invalid_action::{
    auto_fill_message_fields, build_invalid_action_feedback, corrective_invalid_action_prompt,
    default_message_route, ensure_action_base_schema, expected_message_format,
};
use crate::issues::IssuesFile;
use crate::logging::{
    append_action_log_record, append_orchestration_trace, artifact_write_signature,
    compact_log_record, init_log_paths, log_action_result, log_error_event, log_message_event,
    make_command_id, now_ms, record_effect_for_workspace,
};
use crate::md_convert::ensure_objectives_and_invariants_json;
use crate::prompt_inputs::{
    build_single_role_prompt, lane_summary_text, load_planner_inputs, load_single_role_inputs,
    load_verifier_prompt_inputs, read_required_text, read_text_or_empty, LaneConfig,
    LessonsArtifact, OrchestratorContext, PlannerInputs, SingleRoleContext, SingleRoleInputs,
    VerifierPromptInputs,
};
use crate::prompts::{
    action_intent, action_objective_id, action_observation, action_rationale, action_result_prompt,
    action_task_id, diagnostics_cycle_prompt, executor_cycle_prompt, is_explicit_idle_action,
    normalize_action, parse_actions, planner_cycle_prompt, render_action_result_sections,
    single_role_solo_prompt, system_instructions, truncate, validate_action, verifier_cycle_prompt,
    AgentPromptKind,
};
use crate::state_space::{
    check_completion_endpoint, check_completion_tab, decide_bootstrap_phase,
    decide_post_diagnostics, decide_resume_phase, decide_wake_flags, executor_step_limit_exceeded,
    executor_submit_timed_out, is_verifier_specific_blocker, should_force_blocker,
    verifier_blocker_phase_override, CargoTestGate, CompletionEndpointCheck, CompletionTabCheck,
    SemanticControlState, WakeFlagInput,
};
use crate::system_state::SystemState;
use crate::tlog::Tlog;
use crate::tool_schema::write_tool_examples;
use crate::tools::write_stage_graph;

fn runtime_two_role_mode() -> bool {
    std::env::var("RUNTIME_TWO_ROLE")
        .map(|v| {
            let normalized = v.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn runtime_role_enabled(role: &str, two_role_mode: bool) -> bool {
    if !two_role_mode {
        return true;
    }
    matches!(role, "planner" | "executor")
}

fn sanitize_phase_for_runtime(phase: Option<&str>, two_role_mode: bool) -> Option<String> {
    let phase = phase?;
    if runtime_role_enabled(phase, two_role_mode) {
        Some(phase.to_string())
    } else {
        None
    }
}

fn write_json_if_missing_or_empty<T: Serialize>(
    workspace: &Path,
    path: &Path,
    artifact: &str,
    subject: &str,
    value: &T,
) -> Result<bool> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if path.exists() && !existing.trim().is_empty() {
        return Ok(false);
    }
    let text = serde_json::to_string_pretty(value)?;
    crate::logging::write_projection_with_artifact_effects(
        workspace, path, artifact, "write", subject, &text,
    )?;
    Ok(true)
}

fn touch_file_if_missing_or_empty(path: &Path) -> Result<bool> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if path.exists() && !existing.trim().is_empty() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, existing)?;
    Ok(true)
}

fn ensure_workspace_artifact_baseline(
    workspace: &Path,
    diagnostics_path: &Path,
) -> Result<Vec<String>> {
    let mut created = Vec::new();
    let tlog_path = workspace.join("agent_state/tlog.ndjson");
    let tlog_missing_or_empty_before = std::fs::read_to_string(&tlog_path)
        .map(|existing| existing.trim().is_empty())
        .unwrap_or(true);

    if crate::logging::migrate_projection_if_present(
        workspace,
        "PLAN.json",
        MASTER_PLAN_FILE,
        MASTER_PLAN_FILE,
        "baseline_master_plan_legacy_migration",
    )? {
        created.push(MASTER_PLAN_FILE.to_string());
    }

    if crate::logging::migrate_projection_if_present(
        workspace,
        "VIOLATIONS.json",
        VIOLATIONS_FILE,
        VIOLATIONS_FILE,
        "baseline_violations_legacy_migration",
    )? {
        created.push(VIOLATIONS_FILE.to_string());
    }

    if crate::logging::migrate_projection_if_present(
        workspace,
        "ISSUES.json",
        ISSUES_FILE,
        ISSUES_FILE,
        "baseline_issues_legacy_migration",
    )? {
        created.push(ISSUES_FILE.to_string());
    }

    if write_json_if_missing_or_empty(
        workspace,
        &workspace.join(MASTER_PLAN_FILE),
        MASTER_PLAN_FILE,
        "baseline_master_plan",
        &json!({
            "version": 2,
            "status": "in_progress",
            "ready_window": [],
            "tasks": [],
            "dag": { "edges": [] }
        }),
    )? {
        created.push(MASTER_PLAN_FILE.to_string());
    }

    if write_json_if_missing_or_empty(
        workspace,
        &workspace.join(VIOLATIONS_FILE),
        VIOLATIONS_FILE,
        "baseline_violations",
        &crate::reports::ViolationsReport {
            status: "ok".to_string(),
            summary: String::new(),
            violations: Vec::new(),
        },
    )? {
        created.push(VIOLATIONS_FILE.to_string());
    }

    if write_json_if_missing_or_empty(
        workspace,
        &workspace.join(ISSUES_FILE),
        ISSUES_FILE,
        "baseline_issues",
        &IssuesFile {
            version: 1,
            ..IssuesFile::default()
        },
    )? {
        created.push(ISSUES_FILE.to_string());
    }

    if write_json_if_missing_or_empty(
        workspace,
        &workspace.join("agent_state/blockers.json"),
        "agent_state/blockers.json",
        "baseline_blockers",
        &crate::blockers::BlockersFile {
            version: 1,
            blockers: Vec::new(),
        },
    )? {
        created.push("agent_state/blockers.json".to_string());
    }

    if touch_file_if_missing_or_empty(&tlog_path)? || tlog_missing_or_empty_before {
        created.push("agent_state/tlog.ndjson".to_string());
    }

    let lessons_path = workspace.join("agent_state/lessons.json");
    let lessons_ready = std::fs::metadata(&lessons_path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false);
    if !lessons_ready {
        crate::lessons::persist_lessons_projection(
            workspace,
            &LessonsArtifact::default(),
            "baseline_lessons",
        )?;
        created.push("agent_state/lessons.json".to_string());
    }

    let diagnostics_ready = std::fs::metadata(diagnostics_path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false);
    if !diagnostics_ready {
        crate::reports::persist_diagnostics_projection_with_writer_to_path(
            workspace,
            &crate::reports::DiagnosticsReport {
                status: "ok".to_string(),
                inputs_scanned: Vec::new(),
                ranked_failures: Vec::new(),
                planner_handoff: Vec::new(),
            },
            crate::constants::diagnostics_file(),
            None,
            "baseline_diagnostics",
        )?;
        created.push(diagnostics_path.display().to_string());
    }

    Ok(created)
}

/// Extract a string field from a JSON object, returning `""` on missing/non-string.
fn jstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn find_flag_arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn ws_port_is_available(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn choose_ws_port(args: &[String]) -> Result<(u16, bool)> {
    if let Some(raw) = find_flag_arg(args, "--port") {
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
    let role_key = role_key(role);
    let scoped_env = format!(
        "CANON_LLM_TIMEOUT_SECS_{}",
        role_key.replace('-', "_").to_ascii_uppercase()
    );
    if let Ok(raw) = std::env::var(&scoped_env) {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            if parsed > 0 {
                return parsed;
            }
        }
    }

    if let Ok(raw) = std::env::var("CANON_LLM_TIMEOUT_SECS") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            if parsed > 0 {
                return parsed;
            }
        }
    }

    ROLE_TIMEOUT_SECS
        .iter()
        .find(|(key, _)| *key == role_key)
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
    payload.insert(
        "prompt_kind".to_string(),
        Value::String(prompt_kind.to_string()),
    );
    payload.insert("step".to_string(), Value::Number(step.into()));
    payload.insert(
        "endpoint_id".to_string(),
        Value::String(endpoint_id.to_string()),
    );
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
        payload.insert(
            "lane_name".to_string(),
            Value::String(lane_name.to_string()),
        );
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

const FRAMES_ALL_SAMPLE_MAX_BYTES: u64 = 256 * 1024;
const FRAMES_ALL_RECENT_TYPES_MAX: usize = 24;
const FRAMES_ALL_TYPE_COUNTS_MAX: usize = 32;

fn frames_all_fingerprint(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some((modified_ms, meta.len()))
}

fn record_frames_all_debug_effect_if_changed(
    workspace: &Path,
    writer: &mut CanonicalWriter,
    last_fingerprint: &mut Option<(u128, u64)>,
) -> Result<()> {
    let frames_path = workspace.join("frames").join("all.jsonl");
    let Some(fingerprint) = frames_all_fingerprint(&frames_path) else {
        return Ok(());
    };
    if last_fingerprint.as_ref() == Some(&fingerprint) {
        return Ok(());
    }
    let mut file = std::fs::File::open(&frames_path)?;
    let file_size_bytes = fingerprint.1;
    let sample_bytes = file_size_bytes.min(FRAMES_ALL_SAMPLE_MAX_BYTES);
    let sample_start_offset = file_size_bytes.saturating_sub(sample_bytes);
    file.seek(SeekFrom::Start(sample_start_offset))?;

    let mut sample = Vec::with_capacity(sample_bytes as usize);
    file.read_to_end(&mut sample)?;
    if sample_start_offset > 0 {
        if let Some(first_newline) = sample.iter().position(|byte| *byte == b'\n') {
            sample.drain(..=first_newline);
        } else {
            sample.clear();
        }
    }

    let mut sample_lines = 0usize;
    let mut parsed_lines = 0usize;
    let mut parse_errors = 0usize;
    let mut type_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut recent_event_types = Vec::new();

    for line in String::from_utf8_lossy(&sample).lines() {
        if line.trim().is_empty() {
            continue;
        }
        sample_lines += 1;
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                parsed_lines += 1;
                let event_type = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("MISSING_TYPE")
                    .to_string();
                *type_counts.entry(event_type.clone()).or_insert(0) += 1;
                if recent_event_types.len() == FRAMES_ALL_RECENT_TYPES_MAX {
                    recent_event_types.remove(0);
                }
                recent_event_types.push(event_type);
            }
            Err(_) => {
                parse_errors += 1;
            }
        }
    }

    if type_counts.len() > FRAMES_ALL_TYPE_COUNTS_MAX {
        let mut ranked: Vec<(String, u64)> = type_counts.into_iter().collect();
        ranked.sort_by(|(left_kind, left_count), (right_kind, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_kind.cmp(right_kind))
        });
        ranked.truncate(FRAMES_ALL_TYPE_COUNTS_MAX);
        type_counts = ranked.into_iter().collect();
    }

    writer.try_record_effect(EffectEvent::FramesAllDebugSnapshot {
        source: "frames/all.jsonl".to_string(),
        file_size_bytes,
        sample_start_offset,
        sample_bytes,
        sample_lines,
        parsed_lines,
        parse_errors,
        type_counts,
        recent_event_types,
    })?;
    *last_fingerprint = Some(fingerprint);
    Ok(())
}

#[derive(Serialize)]
struct ControlConvergenceSnapshot<'a> {
    state: &'a SystemState,
    active_blocker: bool,
    verifier_pending: bool,
    verifier_running: bool,
}

/// Hash the semantic control snapshot that actually governs routing.
fn cycle_control_hash(snapshot: &ControlConvergenceSnapshot<'_>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(snapshot)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn write_livelock_report(
    agent_state_dir: &Path,
    stall_cycles: u32,
    control_surfaces: &[&str],
    planner_pending: bool,
    diagnostics_pending: bool,
) {
    let report = build_livelock_report(
        stall_cycles,
        control_surfaces,
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
        crate::blockers::record_action_failure_with_writer(
            workspace,
            None,
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
    control_surfaces: &[&str],
    planner_pending: bool,
    diagnostics_pending: bool,
) -> Value {
    json!({
        "timestamp_ms": now_ms(),
        "stall_cycles": stall_cycles,
        "watched_control_surfaces": control_surfaces,
        "pending_at_detection": {
            "planner_pending": planner_pending,
            "diagnostics_pending": diagnostics_pending,
        },
        "message": format!(
            "Orchestrator detected {} consecutive cycles where work was dispatched but \
             no semantic control state changed. Pending flags cleared. Write a wakeup_*.flag or \
             restart to resume.",
            stall_cycles
        ),
    })
}

async fn run_planner_phase(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    planner_bootstrapped: &mut bool,
    cargo_test_failures: &str,
) -> bool {
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
    );
    writer.apply(ControlEvent::LastExecutorDiffSet {
        text: last_executor_diff,
    });
    let restart_resume = peek_post_restart_result("planner");
    let mut planner_prompt = if let Some(resume) = restart_resume.as_ref() {
        let prompt = build_restart_resume_prompt("planner", resume);
        let _ = take_post_restart_result("planner");
        prompt
    } else {
        planner_cycle_prompt(
            &inputs.summary_text,
            &inputs.objectives_text,
            &inputs.lessons_text,
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
            writer.apply(ControlEvent::LastPlanTextSet {
                text: inputs.plan_text,
            });

            // Semantic preflight: demote ready tasks that reference symbols not
            // found in the workspace graph.
            crate::plan_preflight::preflight_ready_tasks(ctx.workspace);

            let lane_ids: Vec<usize> = ctx.lanes.iter().map(|l| l.index).collect();
            for lane_id in lane_ids {
                writer.apply(ControlEvent::LanePlanTextSet {
                    lane_id,
                    text: String::new(),
                });
                let (in_progress, verified) = {
                    let s = writer.state();
                    let ls = s.lanes.get(&lane_id);
                    let in_progress = ls.map(|l| l.in_progress_by.is_some()).unwrap_or(false);
                    let verified = ls
                        .map(|l| verifier_confirmed(&l.latest_verifier_result))
                        .unwrap_or(false);
                    (in_progress, verified)
                };
                if !in_progress && !verified {
                    writer.apply(ControlEvent::LanePendingSet {
                        lane_id,
                        pending: true,
                    });
                }
            }
            writer.apply(ControlEvent::PlannerPendingSet { pending: false });
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
    writer: &mut CanonicalWriter,
    solo_bootstrapped: &mut bool,
    cargo_test_failures: &str,
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
    let master_plan =
        crate::prompt_inputs::filter_pending_plan_json(&read_text_or_empty(ctx.master_plan_path));
    let objectives_path = preferred_objectives_path(ctx.workspace);
    let objectives = crate::objectives::read_objectives_compact(&objectives_path);
    let semantic_control =
        crate::prompt_inputs::read_semantic_control_prompt_context(ctx.workspace, 5);
    let objectives_mtime_before = file_modified_ms(&objectives_path);
    let plan_mtime_before = file_modified_ms(&ctx.workspace.join(MASTER_PLAN_FILE));
    // Compute diffs and ranked context (symmetric with planner cycle)
    let mut solo_exec_diff = writer.state().last_solo_executor_diff.clone();
    let executor_diff_inputs =
        crate::prompt_inputs::load_executor_diff_inputs(ctx.workspace, &mut solo_exec_diff, 400);
    writer.apply(ControlEvent::LastSoloExecutorDiffSet {
        text: solo_exec_diff,
    });
    let current_plan_text = read_text_or_empty(ctx.master_plan_path);
    let plan_diff_text = crate::prompt_inputs::solo_plan_diff(
        &writer.state().last_solo_plan_text.clone(),
        &current_plan_text,
        400,
    );
    writer.apply(ControlEvent::LastSoloPlanTextSet {
        text: current_plan_text,
    });
    let complexity_hotspots = crate::prompt_inputs::read_complexity_hotspots(ctx.workspace, 8);
    let loop_context_hint = crate::prompt_inputs::read_loop_context_hint(std::path::Path::new(
        crate::constants::agent_state_dir(),
    ));
    let restart_resume = peek_post_restart_result("solo");
    let mut prompt = if let Some(resume) = restart_resume.as_ref() {
        let prompt = build_restart_resume_prompt("solo", resume);
        let _ = take_post_restart_result("solo");
        prompt
    } else {
        single_role_solo_prompt(
            &spec,
            &master_plan,
            &objectives,
            &crate::prompt_inputs::read_lessons_or_empty(ctx.workspace),
            &semantic_control,
            cargo_test_failures,
            &crate::prompt_inputs::read_rename_candidates_or_empty(ctx.workspace),
            &executor_diff_inputs.diff_text,
            &plan_diff_text,
            &complexity_hotspots,
            &loop_context_hint,
        )
    };
    inject_inbound_message(&mut prompt, writer, "solo");
    trace_orchestrator_forwarded("orchestrator", "solo", "solo", None, None, None, None);
    let solo_system = system_instructions(AgentPromptKind::Solo);
    let send_system_prompt = if restart_resume.is_some() {
        false
    } else {
        !*solo_bootstrapped
    };
    let result = run_agent(
        "solo",
        "solo",
        &solo_system,
        prompt,
        ctx.solo_ep,
        ctx.bridge,
        ctx.workspace,
        ctx.tabs_solo,
        Some(writer),
        false,
        true,
        send_system_prompt,
        0,
    )
    .await;
    *solo_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!(
                "[orchestrate] solo ok bytes={}",
                result.summary_text().len()
            );
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
                let objectives_mtime_after = file_modified_ms(&objectives_path);
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
    writer: &mut CanonicalWriter,
    diagnostics_bootstrapped: &mut bool,
    verifier_changed: bool,
    cargo_test_failures: &str,
) -> bool {
    {
        let mut state = std::collections::HashMap::new();
        state.insert(
            "diagnostics_pending".to_string(),
            writer.state().diagnostics_pending.to_string(),
        );
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
        if diagnostics_verification_failed_count >= 3 && !writer.state().diagnostics_pending {
            state.insert("actor_kind".to_string(), "diagnostics".to_string());
            state.insert("error_class".to_string(), "verification_failed".to_string());
        }
        if let Err(reason) =
            crate::invariants::evaluate_invariant_gate("diagnostics", &state, ctx.workspace)
        {
            eprintln!("[invariant_gate] diagnostics G_d (BLOCKED): {reason}");
            crate::blockers::record_action_failure_with_writer(
                ctx.workspace,
                Some(writer),
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
            writer.apply(ControlEvent::DiagnosticsPendingSet { pending: true });
            return false;
        }
    }
    let summary_text = lane_summary_text(ctx.lanes, &writer.state().verifier_summary);
    let restart_resume = peek_post_restart_result("diagnostics");
    let mut prompt = if let Some(resume) = restart_resume.as_ref() {
        let prompt = build_restart_resume_prompt("diagnostics", resume);
        let _ = take_post_restart_result("diagnostics");
        prompt
    } else {
        diagnostics_cycle_prompt(&summary_text, cargo_test_failures)
    };
    inject_inbound_message(&mut prompt, writer, "diagnostics");
    trace_orchestrator_forwarded(
        "verifier",
        "diagnostics",
        "diagnostics",
        None,
        None,
        None,
        None,
    );
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
        Some(writer),
        false,
        false,
        if restart_resume.is_some() {
            false
        } else {
            !*diagnostics_bootstrapped
        },
        0,
    )
    .await;
    *diagnostics_bootstrapped = true;
    match result {
        Ok(result) => {
            eprintln!(
                "[orchestrate] diagnostics ok bytes={}",
                result.summary_text().len()
            );
            let new_diagnostics_text =
                crate::prompt_inputs::reconcile_diagnostics_report(ctx.workspace);
            if let Ok(report) =
                serde_json::from_str::<crate::reports::DiagnosticsReport>(&new_diagnostics_text)
            {
                let _ = crate::reports::persist_diagnostics_projection_with_writer(
                    ctx.workspace,
                    &report,
                    Some(writer),
                    "diagnostics_reconcile_projection",
                );
            } else {
                let diagnostics_projection_path = ctx.workspace.join(diagnostics_file());
                let _ = crate::logging::write_projection_with_artifact_effects(
                    ctx.workspace,
                    &diagnostics_projection_path,
                    diagnostics_file(),
                    "write",
                    "diagnostics_reconcile_projection",
                    &new_diagnostics_text,
                );
            }
            writer.apply(ControlEvent::DiagnosticsTextSet {
                text: new_diagnostics_text,
            });
            writer.apply(ControlEvent::DiagnosticsPendingSet { pending: false });
            writer.apply(ControlEvent::PlannerPendingSet {
                pending: decide_post_diagnostics(true, verifier_changed),
            });
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
    writer: &mut CanonicalWriter,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
    verifier_joinset: &mut tokio::task::JoinSet<(usize, String)>,
    verifier_bootstrapped: &mut bool,
    cargo_test_failures: &str,
) -> (bool, bool) {
    let mut cycle_progress = false;
    let mut verifier_changed = false;
    while let Some((submitted, turn_id, final_exec_result)) = verifier_pending_results.pop_front() {
        let semantic_control = SemanticControlState::from_system_state(writer.state(), true, false);
        if !semantic_control.verifier_run_allowed() {
            verifier_pending_results.push_front((submitted, turn_id, final_exec_result));
            break;
        }
        writer.apply(ControlEvent::PhaseSet {
            phase: "verifier".to_string(),
            lane: Some(submitted.lane),
        });
        let lane_plan_file = ctx.lanes[submitted.lane].plan_file.clone();
        let mut last_executor_diff = writer.state().last_executor_diff.clone();
        let prompt_inputs: VerifierPromptInputs = load_verifier_prompt_inputs(
            ctx.lanes,
            ctx.workspace,
            &writer.state().verifier_summary.clone(),
            &mut last_executor_diff,
            cargo_test_failures.to_string(),
        );
        writer.apply(ControlEvent::LastExecutorDiffSet {
            text: last_executor_diff,
        });
        let restart_resume = peek_post_restart_result("verifier");
        let mut verifier_prompt = if let Some(resume) = restart_resume.as_ref() {
            let prompt = build_restart_resume_prompt("verifier", resume);
            let _ = take_post_restart_result("verifier");
            prompt
        } else {
            verifier_cycle_prompt(
                submitted.lane_label.as_str(),
                &final_exec_result,
                &prompt_inputs.executor_diff_text,
                &prompt_inputs.cargo_test_failures,
            )
        };
        if let Some(inbound) = take_inbound_message(writer, "verifier") {
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
                    let override_phase =
                        verifier_blocker_phase_override(verifier_specific).unwrap();
                    writer.apply(ControlEvent::ScheduledPhaseSet {
                        phase: Some(override_phase.to_string()),
                    });
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
        let tabs_verify = ctx.tabs_verify.clone();
        let send_system_prompt = if restart_resume.is_some() {
            false
        } else {
            !*verifier_bootstrapped
        };
        *verifier_bootstrapped = true;
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
                None,
                false,
                false,
                send_system_prompt,
                0,
            )
            .await
            {
                Ok(result) => result.into_summary(),
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
                if verify_result
                    .trim()
                    .eq_ignore_ascii_case("shutdown requested")
                {
                    eprintln!(
                        "[orchestrate] verifier shutdown marker received; preserving previous verifier result"
                    );
                    cycle_progress = true;
                    continue;
                }
                let confirmed = verifier_confirmed(&verify_result);
                let changed = writer
                    .state()
                    .lanes
                    .get(&lane_id)
                    .map(|l| l.latest_verifier_result.as_str())
                    .unwrap_or("")
                    != verify_result;
                writer.apply(ControlEvent::LaneVerifierResultSet {
                    lane_id,
                    result: verify_result.clone(),
                });
                writer.apply(ControlEvent::LaneInProgressSet {
                    lane_id,
                    actor: None,
                });
                writer.apply(ControlEvent::LanePendingSet {
                    lane_id,
                    pending: !confirmed,
                });
                if changed {
                    writer.apply(ControlEvent::VerifierSummarySet {
                        lane_id,
                        result: verify_result,
                    });
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

fn requeue_lane_after_submit_recovery(
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
    writer.apply(ControlEvent::LaneSubmitInFlightSet {
        lane_id,
        in_flight: false,
    });
}

fn register_submitted_executor_turn(
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

fn timed_out_executor_submit_lanes(
    rt: &RuntimeState,
    now: u64,
    pending_submit_timeout_ms: u64,
) -> Vec<usize> {
    let mut timed_out = Vec::new();
    for (lane_id, pending) in rt.executor_submit_inflight.iter() {
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
        ctx.lanes[lane_id].label, pending.command_id
    );
    log_error_event(
        "executor",
        "orchestrate",
        None,
        &format!(
            "pending submit timeout: lane={} command_id={}",
            ctx.lanes[lane_id].label, pending.command_id
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

fn recover_timed_out_executor_submit_lane(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lane_id: usize,
) {
    if let Some(pending) = rt.executor_submit_inflight.remove(&lane_id) {
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

fn sweep_timed_out_executor_submits(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    pending_submit_timeout_ms: u64,
) {
    if rt.executor_submit_inflight.is_empty() {
        return;
    }
    let timed_out = timed_out_executor_submit_lanes(rt, now, pending_submit_timeout_ms);
    for lane_id in timed_out {
        recover_timed_out_executor_submit_lane(ctx, writer, rt, lane_id);
    }
}

fn timed_out_submitted_turns(
    _writer: &CanonicalWriter,
    rt: &RuntimeState,
    now: u64,
    submitted_turn_timeout_ms: u64,
) -> Vec<(u32, u64, usize)> {
    let mut timed_out = Vec::new();
    for (&(tab_id, turn_id), submitted) in rt.submitted_turns.iter() {
        if executor_submit_timed_out(submitted.started_ms, now, submitted_turn_timeout_ms) {
            timed_out.push((tab_id, turn_id, submitted.lane));
        }
    }
    timed_out
}

fn log_timed_out_submitted_turn(
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
    crate::blockers::record_action_failure_with_writer(
        ctx.workspace.as_path(),
        None,
        "executor",
        "executor_completion_timeout",
        &format!(
            "executor completion timed out: lane={} tab_id={} turn_id={} command_id={}",
            ctx.lanes[lane_id].label, tab_id, turn_id, command_id
        ),
        None,
    );
}

fn recover_timed_out_submitted_turn(
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
    log_timed_out_submitted_turn(ctx, lane_id, tab_id, turn_id, &submitted.command_id);
    writer.apply(ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id });
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    apply_lane_pending_if_changed(writer, lane_id, true);
}

fn sweep_timed_out_submitted_turns(
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

fn evaluate_executor_route_gates(writer: &mut CanonicalWriter, ready_count: &str) -> bool {
    let ws = std::path::PathBuf::from(workspace());
    let blockers = crate::blockers::load_blockers(&ws);
    let now_ms = crate::logging::now_ms();

    let mut state = std::collections::HashMap::new();
    state.insert("ready_tasks".to_string(), ready_count.to_string());

    let solo_invalid_schema_count = crate::blockers::count_class_recent(
        &blockers,
        "solo",
        &crate::error_class::ErrorClass::InvalidSchema,
        now_ms,
        5 * 60 * 1000,
    );
    if solo_invalid_schema_count >= 3 {
        state.insert("actor_kind".to_string(), "solo".to_string());
        state.insert("error_class".to_string(), "invalid_schema".to_string());
    }

    let solo_verification_failed_count = crate::blockers::count_class_recent(
        &blockers,
        "solo",
        &crate::error_class::ErrorClass::VerificationFailed,
        now_ms,
        5 * 60 * 1000,
    );
    if solo_verification_failed_count >= 1 {
        state.insert("actor_kind".to_string(), "solo".to_string());
        state.insert("error_class".to_string(), "verification_failed".to_string());
    }

    let executor_invalid_schema_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::InvalidSchema,
        now_ms,
        5 * 60 * 1000,
    );
    if executor_invalid_schema_count >= 3 {
        state.insert("actor_kind".to_string(), "executor".to_string());
        state.insert("error_class".to_string(), "invalid_schema".to_string());
    }

    let unauthorized_plan_op_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::UnauthorizedPlanOp,
        now_ms,
        5 * 60 * 1000,
    );
    if unauthorized_plan_op_count >= 1 {
        state.insert("actor_kind".to_string(), "executor".to_string());
        state.insert(
            "error_class".to_string(),
            "unauthorized_plan_op".to_string(),
        );
    }

    let executor_llm_timeout_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::LlmTimeout,
        now_ms,
        5 * 60 * 1000,
    );
    if executor_llm_timeout_count >= 1 {
        state.insert("actor_kind".to_string(), "executor".to_string());
        state.insert("error_class".to_string(), "llm_timeout".to_string());
    }

    let executor_step_limit_exceeded_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::StepLimitExceeded,
        now_ms,
        5 * 60 * 1000,
    );
    if executor_step_limit_exceeded_count >= 1 {
        state.insert("actor_kind".to_string(), "executor".to_string());
        state.insert("error".to_string(), "step_limit_exceeded".to_string());
    }

    let executor_verification_failed_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::VerificationFailed,
        now_ms,
        5 * 60 * 1000,
    );
    if executor_verification_failed_count >= 1 {
        state.insert("actor_kind".to_string(), "executor".to_string());
        state.insert("error_class".to_string(), "verification_failed".to_string());
    }

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

    let diagnostics_blocker_escalated_count = crate::blockers::count_class_recent(
        &blockers,
        "diagnostics",
        &crate::error_class::ErrorClass::BlockerEscalated,
        now_ms,
        5 * 60 * 1000,
    );
    if diagnostics_blocker_escalated_count >= 3 {
        state.insert("actor_kind".to_string(), "diagnostics".to_string());
        state.insert("error_class".to_string(), "blocker_escalated".to_string());
    }

    let diagnostics_invalid_schema_count = crate::blockers::count_class_recent(
        &blockers,
        "diagnostics",
        &crate::error_class::ErrorClass::InvalidSchema,
        now_ms,
        5 * 60 * 1000,
    );
    if diagnostics_invalid_schema_count >= 3 {
        state.insert("actor_kind".to_string(), "diagnostics".to_string());
        state.insert("error_class".to_string(), "invalid_schema".to_string());
    }

    let verifier_verification_failed_count = crate::blockers::count_class_recent(
        &blockers,
        "verifier",
        &crate::error_class::ErrorClass::VerificationFailed,
        now_ms,
        5 * 60 * 1000,
    );
    if verifier_verification_failed_count >= 1 {
        state.insert("actor_kind".to_string(), "solo".to_string());
        state.insert("error_class".to_string(), "verification_failed".to_string());
    }

    let block_route_gate = |reason: String| {
        eprintln!("[invariant_gate] route G_r (BLOCKED): {reason}");
        let blocker_message = route_gate_blocker_message(&reason);
        if !persist_planner_blocker_message(&blocker_message) {
            return;
        }
        crate::blockers::record_action_failure_with_writer(
            &ws,
            None,
            "orchestrator",
            "route_dispatch",
            &reason,
            None,
        );
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
    };

    if let Err(reason) = crate::invariants::evaluate_invariant_gate("route", &state, &ws) {
        block_route_gate(reason);
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        return false;
    }

    let executor_missing_target_count = crate::blockers::count_class_recent(
        &blockers,
        "executor",
        &crate::error_class::ErrorClass::MissingTarget,
        now_ms,
        5 * 60 * 1000,
    );
    if executor_missing_target_count >= 1 {
        let mut executor_missing_target_state = state.clone();
        executor_missing_target_state.insert("actor_kind".to_string(), "executor".to_string());
        executor_missing_target_state.insert("error".to_string(), "missing_target".to_string());
        if let Err(reason) = crate::invariants::evaluate_invariant_gate(
            "executor",
            &executor_missing_target_state,
            &ws,
        ) {
            block_route_gate(reason);
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            return false;
        }
    }

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

    let livelock_count = crate::blockers::count_class_recent(
        &blockers,
        "orchestrator",
        &crate::error_class::ErrorClass::LivelockDetected,
        now_ms,
        5 * 60 * 1000,
    );
    if livelock_count >= 1 {
        state.insert("actor_kind".to_string(), "orchestrator".to_string());
        state.insert("error_class".to_string(), "livelock_detected".to_string());
    }

    if let Err(reason) = crate::invariants::evaluate_invariant_gate("executor", &state, &ws) {
        block_route_gate(reason);
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        return false;
    }

    true
}

fn dispatch_executor_submits(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    now: u64,
    submit_joinset: &mut tokio::task::JoinSet<(usize, PendingExecutorSubmit, Result<String>)>,
) -> bool {
    let semantic_control = SemanticControlState::from_system_state(writer.state(), false, false);
    if semantic_control.executor_dispatch_blocked() {
        return false;
    }

    let mut cycle_progress = false;

    // No-ready-tasks guard: if PLAN.json has no ready tasks, skip executor dispatch
    // entirely and wake the planner instead.  This eliminates the idle turn where
    // the executor discovers no work and sends an empty handoff message back.
    {
        let ws = std::path::PathBuf::from(workspace());
        let ready_tasks_text = crate::prompt_inputs::read_ready_tasks(&ws, 1);
        let ready_count = if ready_tasks_text == "(no ready tasks)" {
            "0"
        } else {
            "1+"
        };
        if ready_count == "0" {
            writer.apply(ControlEvent::PlannerPendingSet { pending: true });
            return true;
        }
        // Route gate G_r: check enforced invariants before dispatching the executor.
        // Currently observational — violations are logged but do not hard-block.
        // Once invariants accumulate enough support, this will become a hard gate.
        if !evaluate_executor_route_gates(writer, ready_count) {
            return false;
        }

        // Clean-start guard: after agent_state reset, the executor can see ready
        // PLAN tasks but still have zero lane work seeded because lane.pending is
        // only populated by the planner bootstrap path. In that state, starting at
        // executor would silently idle forever while the browser backend stays live.
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
            return true;
        }
    }

    for lane in ctx.lanes {
        if writer.state().lane_submit_active(lane.index)
            || writer.state().lane_next_submit_ms(lane.index) > now
        {
            continue;
        }
        if let Some(job) = claim_executor_submit(writer, lane) {
            writer.apply(ControlEvent::PhaseSet {
                phase: "executor".to_string(),
                lane: Some(lane.index),
            });
            let lane_index = lane.index;
            let endpoint = lane.endpoint.clone();
            let bridge = ctx.bridge.clone();
            let tabs = lane.tabs.clone();
            let command_id = make_command_id(&job.executor_role, "executor", 1);
            let response_timeout_secs = response_timeout_for_role(&job.executor_role);
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
            cycle_progress = true;
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

    cycle_progress
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
    job: &PendingExecutorSubmit,
    tab_id: u32,
    turn_id: u64,
    command_id: Option<String>,
) -> bool {
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
    let steps_used = writer.state().lane_steps_used_count(job.lane_index);
    register_submitted_executor_turn(
        writer,
        rt,
        lane_id,
        tab_id,
        turn_id,
        SubmittedExecutorTurn {
            tab_id,
            lane: job.lane_index,
            lane_label: job.label.clone(),
            command_id: command_id
                .unwrap_or_else(|| make_command_id(&job.executor_role, "executor", 1)),
            started_ms: now_ms(),
            actor: job.executor_role.clone(),
            endpoint_id: job.endpoint_id.clone(),
            tabs: job.tabs.clone(),
            steps_used,
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
            lane_label, tab_id, turn_id
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
    writer: &mut CanonicalWriter,
    lane_id: usize,
    tab_id: u32,
    turn_id: u64,
) -> bool {
    log_submit_ack_timeout(ctx, lane_id, tab_id, turn_id);
    crate::blockers::record_action_failure_with_writer(
        ctx.workspace.as_path(),
        None,
        "executor",
        "submit_ack_timeout",
        &format!(
            "submit ack timed out: lane={} tab_id={} turn_id={}",
            ctx.lanes[lane_id].label, tab_id, turn_id
        ),
        None,
    );
    writer.apply(ControlEvent::LaneSubmitInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LanePromptInFlightSet {
        lane_id,
        in_flight: false,
    });
    writer.apply(ControlEvent::LaneInProgressSet {
        lane_id,
        actor: None,
    });
    apply_lane_pending_if_changed(writer, lane_id, true);
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

    let Some(pending) = rt.executor_submit_inflight.remove(&lane_id) else {
        // The timeout path already removed executor_submit_inflight for
        // this lane, but the submit actually succeeded.  Register the turn
        // so the completion can still be routed back to the LLM.
        return register_late_submit_ack(
            ctx, writer, rt, lane_id, &job, tab_id, turn_id, command_id,
        );
    };

    if executor_submit_timed_out(pending.started_ms, now_ms(), pending_submit_timeout_ms) {
        return handle_submit_ack_timeout(ctx, writer, lane_id, tab_id, turn_id);
    }

    if let Some(active_tab) = writer.state().lane_active_tab_id(lane_id) {
        if active_tab != tab_id {
            log_submit_ack_tab_mismatch(ctx, lane_id, active_tab, tab_id);
            crate::blockers::record_action_failure_with_writer(
                ctx.workspace.as_path(),
                None,
                "executor",
                "runtime_control_bypass",
                &format!(
                    "runtime-only control influence: submit ack changed lane={} active tab from {} to {}",
                    ctx.lanes[lane_id].label, active_tab, tab_id
                ),
                None,
            );
            writer.apply(ControlEvent::ExecutorSubmitAckTabRebound {
                lane_id,
                from_tab_id: active_tab,
                to_tab_id: tab_id,
            });
        }
    }

    let steps_used = writer.state().lane_steps_used_count(job.lane_index);
    register_submitted_executor_turn(
        writer,
        rt,
        lane_id,
        tab_id,
        turn_id,
        SubmittedExecutorTurn {
            tab_id,
            lane: job.lane_index,
            lane_label: job.label.clone(),
            command_id: command_id.unwrap_or_else(|| pending.command_id.clone()),
            started_ms: pending.started_ms,
            actor: job.executor_role.clone(),
            endpoint_id: pending.endpoint_id.clone(),
            tabs: pending.tabs.clone(),
            steps_used,
        },
    );
    true
}

async fn process_completed_turns(
    ctx: &OrchestratorContext<'_>,
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    continuation_joinset: &mut tokio::task::JoinSet<(
        SubmittedExecutorTurn,
        u64,
        Result<AgentCompletion>,
    )>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    let completed_turns = ctx.bridge.take_completed_turns().await;
    for item in completed_turns {
        append_orchestration_trace("llm_message_received", item.clone());
        let Some((tab_id, turn_id, exec_result, completed_endpoint_id)) =
            parse_completed_turn(&item)
        else {
            continue;
        };
        let submitted = if let Some(submitted) = rt.submitted_turns.remove(&(tab_id, turn_id)) {
            writer.apply(ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id });
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
            let lane_id = writer.state().tab_id_to_lane.get(&tab_id).copied();
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
            if check_completion_endpoint(
                &ctx.lanes[lane_id].endpoint.id,
                completed_endpoint_id.as_deref(),
            ) == CompletionEndpointCheck::Mismatch
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
            match check_completion_tab(writer.state().lane_active_tab_id(lane_id), tab_id) {
                CompletionTabCheck::Mismatch => {
                    append_orchestration_trace(
                        "executor_completion_tab_mismatch",
                        json!({
                            "lane_name": ctx.lanes[lane_id].label,
                            "active_tab": writer.state().lane_active_tab_id(lane_id),
                            "tab_id": tab_id,
                            "turn_id": turn_id,
                        }),
                    );
                    continue;
                }
                CompletionTabCheck::NoneSet | CompletionTabCheck::Ok => {}
            }
            let Some(pending) = rt.executor_submit_inflight.remove(&lane_id) else {
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
        };
        writer.apply(ControlEvent::LanePromptInFlightSet {
            lane_id: submitted.lane,
            in_flight: false,
        });
        if handle_executor_completion(
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
        ) {
            cycle_progress = true;
        }
    }
    cycle_progress
}

fn drain_continuations(
    writer: &mut CanonicalWriter,
    continuation_joinset: &mut tokio::task::JoinSet<(
        SubmittedExecutorTurn,
        u64,
        Result<AgentCompletion>,
    )>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    while let Some(joined) = continuation_joinset.try_join_next() {
        match joined {
            Ok((submitted, turn_id, result)) => {
                cycle_progress |= handle_completed_continuation(
                    writer,
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
    writer: &mut CanonicalWriter,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
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
                AgentCompletion::MessageAction { action, .. } => {
                    finalize_executor_message_completion(writer, submitted.lane);
                    persist_executor_completion_message(writer, &action);
                }
                AgentCompletion::Summary(final_exec_result) => {
                    if runtime_two_role_mode() {
                        finalize_executor_summary_without_verifier(
                            writer,
                            &submitted,
                            turn_id,
                            &final_exec_result,
                        );
                    } else {
                        verifier_pending_results.push_back((submitted, turn_id, final_exec_result));
                    }
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
    persist_executor_completion_message(writer, &action);
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
    continuation_joinset: &mut tokio::task::JoinSet<(
        SubmittedExecutorTurn,
        u64,
        Result<AgentCompletion>,
    )>,
    verifier_pending_results: &mut VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> bool {
    let mut cycle_progress = false;
    for lane_id in 0..ctx.lanes.len() {
        if writer.state().lane_in_flight(lane_id) {
            continue;
        }
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

fn is_chromium_transport_error(err_text: &str) -> bool {
    err_text.contains("chromium: early transport failure")
        || err_text.contains("chromium: timeout waiting for SUBMIT_ACK")
        || err_text.contains("chromium: timeout waiting for response")
}

fn local_transport_blocker_message(role: &str, err_text: &str, task_context: &str) -> Value {
    let from = canonical_role_label(role);
    let to = blocker_target_role(role);
    json!({
        "action": "message",
        "from": from,
        "to": to,
        "type": "blocker",
        "status": "blocked",
        "observation": "Chromium transport failed before an authoritative assistant completion was assembled.",
        "rationale": "Once the transport/backend has already failed, asking the model to restate the same failure only creates duplicate turns and extra room traffic.",
        "predicted_next_actions": [
            {
                "action": "read_file",
                "intent": "Inspect the latest Chromium backend, inbound frames, and tlog evidence for the failed turn."
            },
            {
                "action": "message",
                "intent": "Report a repaired handoff or a narrower blocker after the transport/runtime path is stable."
            }
        ],
        "payload": build_blocker_payload(
            &format!("{from} transport/runtime failure"),
            "Chromium transport/runtime failure prevented a usable assistant completion",
            &format!(
                "error: {}\n\ncontext: {}",
                truncate(err_text, MAX_SNIPPET),
                truncate(task_context, MAX_SNIPPET),
            ),
            "Repair the Chromium/backend session and rerun once assistant completion assembly is stable.",
            "error",
        ),
    })
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
    #[serde(default)]
    checkpoint_tlog_seq: u64,
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

fn artifact_signature(parts: &[&str]) -> String {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn persist_agent_state_projection(path: &Path, contents: &str, subject: &str) -> Result<()> {
    let workspace = Path::new(crate::constants::workspace());
    let artifact = path
        .strip_prefix(workspace)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"));
    let target = path.to_string_lossy().into_owned();
    let signature = artifact_signature(&[artifact.as_str(), subject, &contents.len().to_string()]);
    crate::logging::record_workspace_artifact_effect(
        workspace, true, &artifact, "write", &target, subject, &signature,
    )?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, path)?;
    crate::logging::record_workspace_artifact_effect(
        workspace, false, &artifact, "write", &target, subject, &signature,
    )?;
    Ok(())
}

fn save_checkpoint(
    workspace: &Path,
    writer: &mut CanonicalWriter,
    lanes: &[LaneConfig],
    verifier_pending_results: &VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> Result<()> {
    let state = writer.state().clone();
    let path = checkpoint_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Record the canonical checkpoint-save effect before materializing the file.
    // The log must lead the side effect so replay can observe the save attempt
    // in the same order the runtime produced it.
    writer.try_record_effect(crate::events::EffectEvent::CheckpointSaved {
        phase: state.phase.clone(),
    })?;
    let lane_snapshots = build_checkpoint_lane_snapshots(&state, lanes);
    let resume_items = build_resume_verifier_items(lanes, verifier_pending_results);
    let checkpoint = OrchestratorCheckpoint {
        workspace: workspace.to_string_lossy().into_owned(),
        checkpoint_tlog_seq: writer.tlog_seq(),
        created_ms: now_ms(),
        phase: state.phase.clone(),
        phase_lane: state.phase_lane,
        planner_pending: state.planner_pending,
        diagnostics_pending: state.diagnostics_pending,
        diagnostics_text: state.diagnostics_text.clone(),
        last_plan_text: state.last_plan_text.clone(),
        last_executor_diff: state.last_executor_diff.clone(),
        last_solo_plan_text: state.last_solo_plan_text.clone(),
        last_solo_executor_diff: state.last_solo_executor_diff.clone(),
        lanes: lane_snapshots,
        verifier_summary: state.verifier_summary.clone(),
        verifier_pending_results: resume_items,
    };
    persist_agent_state_projection(
        &path,
        &serde_json::to_string_pretty(&checkpoint)?,
        "orchestrator_checkpoint",
    )?;
    Ok(())
}

fn build_checkpoint_lane_snapshots(
    state: &SystemState,
    lanes: &[LaneConfig],
) -> Vec<CheckpointLane> {
    let mut lane_snapshots = Vec::new();
    for lane in lanes {
        if let Some(ls) = state.lanes.get(&lane.index) {
            lane_snapshots.push(CheckpointLane {
                lane_id: lane.index,
                lane_label: lane.label.clone(),
                plan_text: ls.plan_text.clone(),
                pending: ls.pending,
                in_progress_by: ls.in_progress_by.clone(),
                latest_verifier_result: ls.latest_verifier_result.clone(),
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

fn recover_verifier_item_from_executor_post_restart(
    lanes: &[LaneConfig],
) -> Option<ResumeVerifierItem> {
    let resume = peek_post_restart_result("executor")?;
    if resume.action != "apply_patch" || !resume.result.contains("apply_patch ok") {
        return None;
    }
    let lane = lanes
        .iter()
        .find(|lane| lane.endpoint.id == resume.endpoint_id)
        .or_else(|| (lanes.len() == 1).then(|| &lanes[0]))?;
    let _ = take_post_restart_result("executor");
    Some(ResumeVerifierItem {
        lane_id: lane.index,
        lane_label: lane.label.clone(),
        lane_plan_file: lane.plan_file.clone(),
        final_exec_result: resume.result,
    })
}

fn load_checkpoint(workspace: &Path) -> Option<OrchestratorCheckpoint> {
    let path = checkpoint_path(workspace);
    let raw = std::fs::read_to_string(path).ok()?;
    let cp: OrchestratorCheckpoint = serde_json::from_str(&raw).ok()?;
    if cp.workspace.is_empty() || cp.workspace != workspace.to_string_lossy().as_ref() {
        let msg = format!(
            "checkpoint/runtime divergence: checkpoint workspace mismatch (stored={} current={})",
            cp.workspace,
            workspace.display()
        );
        eprintln!(
            "[orchestrate] checkpoint workspace mismatch (stored={} current={}) — discarding",
            cp.workspace,
            workspace.display()
        );
        crate::blockers::record_action_failure_with_writer(
            workspace,
            None,
            "orchestrate",
            "checkpoint_runtime_divergence",
            &msg,
            None,
        );
        return None;
    }
    if cp.checkpoint_tlog_seq > 0 {
        let tlog_path = PathBuf::from(crate::constants::agent_state_dir()).join("tlog.ndjson");
        let current_tlog_seq = crate::tlog::Tlog::open(&tlog_path).seq();
        if cp.checkpoint_tlog_seq != current_tlog_seq {
            let msg = format!(
                "checkpoint/runtime divergence: checkpoint seq {} does not match tlog seq {}",
                cp.checkpoint_tlog_seq, current_tlog_seq
            );
            eprintln!(
                "[orchestrate] checkpoint seq {} does not match current tlog seq {} — discarding",
                cp.checkpoint_tlog_seq, current_tlog_seq
            );
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                "orchestrate",
                "checkpoint_runtime_divergence",
                &msg,
                None,
            );
            return None;
        }
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
    if let Some(guard_action) =
        maybe_guardrail_parse_action(role, raw, allow_guardrail, log, err_text)
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
        let msg = format!(
            "Got {} actions — emit exactly one action per turn.",
            actions.len()
        );
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
        return Err(handle_invalid_action_error(
            role,
            raw,
            action,
            log,
            &e.to_string(),
        ));
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
    let agent_type = role_key(role).to_uppercase();
    let header = format!("TAB_ID: pending\nTURN_ID: pending\nAGENT_TYPE: {agent_type}\n\n");
    if step == 0 {
        (
            if send_system_prompt {
                system_instructions.to_string()
            } else {
                String::new()
            },
            format!("{header}{initial_prompt}"),
        )
    } else {
        let result = last_result.unwrap_or("").to_string();
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

fn should_send_system_prompt(
    send_system_prompt: bool,
    endpoint_stateful: bool,
    step: usize,
) -> bool {
    send_system_prompt && (!endpoint_stateful || step == 0)
}

fn enforce_executor_step_limit(
    role: &str,
    total_steps: usize,
    error_streak: &mut usize,
    last_result: &mut Option<String>,
    workspace: &std::path::Path,
) -> bool {
    if role.starts_with("executor")
        && executor_step_limit_exceeded(total_steps, EXECUTOR_STEP_LIMIT)
    {
        *error_streak = error_streak.saturating_add(1);
        *last_result = Some(executor_step_limit_feedback());
        crate::blockers::record_action_failure_with_writer(
            workspace,
            None,
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
    diagnostics_eventlog_python_done: &mut bool,
) -> Option<String> {
    if role == "diagnostics" && kind == "python" {
        *diagnostics_eventlog_python_done = true;
    }

    None
}

fn canonical_tlog_read_path(agent_state_dir: &std::path::Path) -> PathBuf {
    let workspace_tlog = PathBuf::from(crate::constants::workspace())
        .join("agent_state")
        .join("tlog.ndjson");
    let agent_state_tlog = agent_state_dir.join("tlog.ndjson");

    let has_data = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false)
    };

    if has_data(&workspace_tlog) {
        workspace_tlog
    } else if has_data(&agent_state_tlog) || agent_state_tlog.exists() {
        agent_state_tlog
    } else {
        workspace_tlog
    }
}

fn canonical_inbound_message_from_tlog(
    agent_state_dir: &std::path::Path,
    state: &SystemState,
    role: &str,
) -> Option<(String, String)> {
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let records = Tlog::read_records(&tlog_path).ok()?;
    let mut latest: Option<(u64, String, String)> = None;
    for record in records {
        let Event::Effect {
            event:
                EffectEvent::InboundMessageRecorded {
                    to_role,
                    message,
                    signature,
                    ..
                },
        } = record.event
        else {
            continue;
        };
        if to_role != role {
            continue;
        }
        let replace = match latest.as_ref() {
            None => true,
            Some((seq, _, _)) => record.seq >= *seq,
        };
        if replace {
            latest = Some((record.seq, signature, message));
        }
    }
    latest.and_then(|(_, signature, message)| {
        let consumed_latest = state
            .inbound_message_signatures
            .get(role)
            .map(String::as_str)
            == Some(signature.as_str());
        if consumed_latest {
            None
        } else {
            Some((signature, message))
        }
    })
}

fn latest_inbound_message_from_tlog(
    agent_state_dir: &std::path::Path,
    role: &str,
) -> Option<(String, String)> {
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let records = Tlog::read_records(&tlog_path).ok()?;
    let mut latest: Option<(u64, String, String)> = None;
    for record in records {
        let Event::Effect {
            event:
                EffectEvent::InboundMessageRecorded {
                    to_role,
                    message,
                    signature,
                    ..
                },
        } = record.event
        else {
            continue;
        };
        if to_role != role {
            continue;
        }
        let replace = match latest.as_ref() {
            None => true,
            Some((seq, _, _)) => record.seq >= *seq,
        };
        if replace {
            latest = Some((record.seq, signature, message));
        }
    }
    latest.map(|(_, signature, message)| (signature, message))
}

fn take_inbound_message(writer: &mut CanonicalWriter, role: &str) -> Option<String> {
    let role_key = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let path = agent_state_dir.join(format!("last_message_to_{role_key}.json"));
    if let Some((signature, message)) =
        canonical_inbound_message_from_tlog(agent_state_dir, writer.state(), &role_key)
    {
        let trimmed = message.trim().to_string();
        writer.apply(ControlEvent::InboundMessageConsumed {
            role: role_key.clone(),
            signature,
        });
        let _ = std::fs::remove_file(&path);
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }
    None
}

fn take_inbound_message_without_writer(role: &str) -> Option<String> {
    let role_key = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let state = Tlog::replay(&tlog_path, SystemState::new(&[], 0)).ok();
    let canonical = state
        .as_ref()
        .and_then(|state| canonical_inbound_message_from_tlog(agent_state_dir, state, &role_key))
        .or_else(|| latest_inbound_message_from_tlog(agent_state_dir, &role_key));
    if let Some((signature, message)) = canonical {
        if let Some(state) = state {
            if let Ok(mut writer) = CanonicalWriter::try_new(
                state,
                Tlog::open(&tlog_path),
                PathBuf::from(crate::constants::workspace()),
            ) {
                let _ = writer.try_apply(ControlEvent::InboundMessageConsumed {
                    role: role_key.clone(),
                    signature,
                });
            }
        }
        let path = agent_state_dir.join(format!("last_message_to_{}.json", role));
        let _ = std::fs::remove_file(&path);
        let trimmed = message.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }
    None
}

fn canonical_external_user_message_from_tlog(
    agent_state_dir: &std::path::Path,
    state: &SystemState,
    role: &str,
) -> Option<(String, String)> {
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let records = Tlog::read_records(&tlog_path).ok()?;
    let mut latest: Option<(u64, String, String)> = None;
    for record in records {
        let Event::Effect {
            event:
                EffectEvent::ExternalUserMessageRecorded {
                    to_role,
                    message,
                    signature,
                },
        } = record.event
        else {
            continue;
        };
        if to_role != role {
            continue;
        }
        let replace = match latest.as_ref() {
            None => true,
            Some((seq, _, _)) => record.seq >= *seq,
        };
        if replace {
            latest = Some((record.seq, signature, message));
        }
    }
    latest.and_then(|(_, signature, message)| {
        let consumed_latest = state
            .external_user_message_signatures
            .get(role)
            .map(String::as_str)
            == Some(signature.as_str());
        if consumed_latest {
            None
        } else {
            Some((signature, message))
        }
    })
}

fn latest_external_user_message_from_tlog(
    agent_state_dir: &std::path::Path,
    role: &str,
) -> Option<(String, String)> {
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let records = Tlog::read_records(&tlog_path).ok()?;
    let mut latest: Option<(u64, String, String)> = None;
    for record in records {
        let Event::Effect {
            event:
                EffectEvent::ExternalUserMessageRecorded {
                    to_role,
                    message,
                    signature,
                },
        } = record.event
        else {
            continue;
        };
        if to_role != role {
            continue;
        }
        let replace = match latest.as_ref() {
            None => true,
            Some((seq, _, _)) => record.seq >= *seq,
        };
        if replace {
            latest = Some((record.seq, signature, message));
        }
    }
    latest.map(|(_, signature, message)| (signature, message))
}

fn take_external_user_message(writer: &mut CanonicalWriter, role: &str) -> Option<String> {
    let role_key = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let path = agent_state_dir.join(format!("external_user_message_to_{role_key}.json"));
    if let Some((signature, message)) =
        canonical_external_user_message_from_tlog(agent_state_dir, writer.state(), &role_key)
    {
        let trimmed = message.trim().to_string();
        writer.apply(ControlEvent::ExternalUserMessageConsumed {
            role: role_key,
            signature,
        });
        let _ = std::fs::remove_file(&path);
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }
    None
}

fn take_external_user_message_without_writer(role: &str) -> Option<String> {
    let role_key = role
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let tlog_path = canonical_tlog_read_path(agent_state_dir);
    let state = Tlog::replay(&tlog_path, SystemState::new(&[], 0)).ok();
    let canonical = state
        .as_ref()
        .and_then(|state| {
            canonical_external_user_message_from_tlog(agent_state_dir, state, &role_key)
        })
        .or_else(|| latest_external_user_message_from_tlog(agent_state_dir, &role_key));
    if let Some((signature, message)) = canonical {
        if let Some(state) = state {
            if let Ok(mut writer) = CanonicalWriter::try_new(
                state,
                Tlog::open(&tlog_path),
                PathBuf::from(crate::constants::workspace()),
            ) {
                let _ = writer.try_apply(ControlEvent::ExternalUserMessageConsumed {
                    role: role_key.clone(),
                    signature,
                });
            }
        }
        let path = agent_state_dir.join(format!("external_user_message_to_{role_key}.json"));
        let _ = std::fs::remove_file(&path);
        let trimmed = message.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed);
    }
    None
}

fn append_external_user_message_to_prompt(prompt: &mut String, inbound: &str) {
    let parsed = serde_json::from_str::<Value>(inbound).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(inbound.trim());

    prompt.push_str("\n\nExternal user request:\n");
    prompt.push_str(message);
    prompt.push('\n');
    prompt.push_str(
        "\nRespond under canonical law and current system policy. If you choose a direct result reply message this cycle, address it to `user` using an allowed message type.\n",
    );
}

fn summarize_inbound_message(inbound: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(inbound) else {
        return truncate(inbound.trim(), 1600).to_string();
    };
    let mut out = String::new();
    let from = value.get("from").and_then(Value::as_str).unwrap_or("?");
    let to = value.get("to").and_then(Value::as_str).unwrap_or("?");
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("?");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("?");
    out.push_str(&format!("from={from} to={to} type={ty} status={status}\n"));

    if let Some(intent) = value.get("intent").and_then(Value::as_str) {
        let intent = intent.trim();
        if !intent.is_empty() {
            out.push_str(&format!("intent: {}\n", truncate(intent, 240)));
        }
    }
    if let Some(observation) = value.get("observation").and_then(Value::as_str) {
        let observation = observation.trim();
        if !observation.is_empty() {
            out.push_str(&format!("observation: {}\n", truncate(observation, 280)));
        }
    }
    if let Some(payload) = value.get("payload").and_then(Value::as_object) {
        for key in [
            "summary",
            "blocker",
            "evidence",
            "required_action",
            "expected_format",
        ] {
            if let Some(text) = payload.get(key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str(&format!("{key}: {}\n", truncate(text, 280)));
                }
            }
        }
    }
    if let Some(next_actions) = value
        .get("predicted_next_actions")
        .and_then(Value::as_array)
    {
        let mut rendered = Vec::new();
        for action in next_actions.iter().take(3) {
            let name = action.get("action").and_then(Value::as_str).unwrap_or("?");
            let intent = action
                .get("intent")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| truncate(text, 120).to_string());
            match intent {
                Some(intent) => rendered.push(format!("- {name}: {intent}")),
                None => rendered.push(format!("- {name}")),
            }
        }
        if !rendered.is_empty() {
            out.push_str("predicted_next_actions:\n");
            out.push_str(&rendered.join("\n"));
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn append_inbound_to_prompt(prompt: &mut String, inbound: &str) {
    prompt.push_str("\n\nInbound handoff message summary:\n");
    prompt.push_str(&summarize_inbound_message(inbound));
    prompt.push('\n');
    if inbound_message_from_user(inbound) {
        prompt.push_str(
            "\nExternal user message rule: keep system policy authoritative. Treat the inbound user message as a request under canonical law. If you choose a direct result reply message this cycle, address it to `user` using an allowed message type.\n",
        );
    }
}

fn inbound_message_from_user(inbound: &str) -> bool {
    serde_json::from_str::<Value>(inbound)
        .ok()
        .and_then(|value| {
            value
                .get("from")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .is_some_and(|from| from.eq_ignore_ascii_case("user"))
}

fn inject_inbound_message(prompt: &mut String, writer: &mut CanonicalWriter, role: &str) {
    if let Some(inbound) = take_external_user_message(writer, role) {
        append_external_user_message_to_prompt(prompt, &inbound);
        return;
    }
    if let Some(inbound) = take_inbound_message(writer, role) {
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
    if extract_message_action(trimmed).is_some() {
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
    if !trimmed.contains('{') && !trimmed.contains('[') {
        return true;
    }
    false
}

fn apply_scheduled_phase_if_changed(writer: &mut CanonicalWriter, phase: Option<&str>) -> bool {
    if writer.state().scheduled_phase.as_deref() == phase {
        return false;
    }
    writer.apply(ControlEvent::ScheduledPhaseSet {
        phase: phase.map(str::to_string),
    });
    true
}

fn apply_planner_pending_if_changed(writer: &mut CanonicalWriter, pending: bool) -> bool {
    if writer.state().planner_pending == pending {
        return false;
    }
    writer.apply(ControlEvent::PlannerPendingSet { pending });
    true
}

fn apply_diagnostics_pending_if_changed(writer: &mut CanonicalWriter, pending: bool) -> bool {
    if writer.state().diagnostics_pending == pending {
        return false;
    }
    writer.apply(ControlEvent::DiagnosticsPendingSet { pending });
    true
}

fn apply_lane_pending_if_changed(
    writer: &mut CanonicalWriter,
    lane_id: usize,
    pending: bool,
) -> bool {
    let current = writer
        .state()
        .lanes
        .get(&lane_id)
        .map(|lane| lane.pending)
        .unwrap_or(false);
    if current == pending {
        return false;
    }
    writer.apply(ControlEvent::LanePendingSet { lane_id, pending });
    true
}

fn apply_wake_flags(
    agent_state_dir: &std::path::Path,
    writer: &mut CanonicalWriter,
    two_role_mode: bool,
) {
    let state_snapshot = writer.state().clone();
    let (inputs, path_map, signature_map) =
        collect_wake_flag_inputs(agent_state_dir, &state_snapshot, two_role_mode);
    let wake_inputs_debug = inputs
        .iter()
        .map(|input| format!("{}@{}", input.role, input.modified_ms))
        .collect::<Vec<_>>()
        .join(", ");

    let semantic_control = SemanticControlState::from_system_state(&state_snapshot, false, false);
    let decision = decide_wake_flags(semantic_control.active_blocker_to_verifier, &inputs);
    let Some(role) = decision.scheduled_phase.as_deref() else {
        return;
    };
    let selected_modified_ms = inputs
        .iter()
        .find(|input| input.role == role)
        .map(|input| input.modified_ms)
        .unwrap_or(0);

    apply_scheduled_phase_if_changed(writer, Some(role));
    let wake_flag_path = path_map.get(role).cloned();
    let wake_signature = signature_map.get(role).cloned();
    let mut clear_wake_flag = !decision.executor_wake;
    if decision.planner_pending {
        apply_planner_pending_if_changed(writer, true);
    }
    if decision.diagnostics_pending {
        apply_diagnostics_pending_if_changed(writer, true);
    }
    if decision.executor_wake {
        let lane_ids: Vec<usize> = writer.state().lanes.keys().copied().collect();
        for lane_id in lane_ids {
            let (pending, in_progress) = {
                let state = writer.state();
                let lane = state.lanes.get(&lane_id);
                (
                    lane.map(|l| l.pending).unwrap_or(false),
                    lane.and_then(|l| l.in_progress_by.as_ref()).is_some(),
                )
            };
            if pending {
                clear_wake_flag = true;
                continue;
            }
            if in_progress {
                continue;
            }
            clear_wake_flag |= apply_lane_pending_if_changed(writer, lane_id, true);
            // Do NOT clear in_progress_by here. If the lane already has a submit
            // in flight, clearing ownership causes a double-submit on the next tick
            // (claim_next_lane sees pending=true + in_progress_by=None and spawns a
            // second request while the first is still running). The wake effect
            // is preserved by leaving the wake flag on disk until an idle lane can
            // actually be marked pending.
        }
    }
    let suppress_deferred_repeat_log = !clear_wake_flag
        && role == "executor"
        && should_suppress_repeated_executor_deferred_log(selected_modified_ms);
    if !suppress_deferred_repeat_log {
        eprintln!(
            "[orchestrate] wake_flag_selected: role={} planner_pending={} diagnostics_pending={} executor_wake={} inputs=[{}]",
            role,
            decision.planner_pending,
            decision.diagnostics_pending,
            decision.executor_wake,
            wake_inputs_debug,
        );
    }
    if clear_wake_flag {
        if let Some(signature) = wake_signature {
            writer.apply(ControlEvent::WakeSignalConsumed {
                role: role.to_string(),
                signature,
            });
        }
    }
    if let Some(path) = wake_flag_path {
        if clear_wake_flag {
            clear_repeated_executor_deferred_log_memory(role);
            eprintln!(
                "[orchestrate] wake_flag_triggered: role={} path={}",
                role,
                path.display()
            );
            let _ = std::fs::remove_file(path);
        } else if !suppress_deferred_repeat_log {
            eprintln!(
                "[orchestrate] wake_flag_deferred: role={} path={} reason=all_executor_lanes_busy",
                role,
                path.display()
            );
        }
    }
}

fn should_suppress_repeated_executor_deferred_log(modified_ms: u64) -> bool {
    let map = repeated_executor_deferred_log_memory();
    let mut guard = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let last = guard.get("executor").copied();
    guard.insert("executor".to_string(), modified_ms);
    last == Some(modified_ms)
}

fn clear_repeated_executor_deferred_log_memory(role: &str) {
    if role != "executor" {
        return;
    }
    let map = repeated_executor_deferred_log_memory();
    let mut guard = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.remove("executor");
}

fn repeated_executor_deferred_log_memory(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static LAST_DEFERRED_BY_ROLE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, u64>>,
    > = std::sync::OnceLock::new();
    LAST_DEFERRED_BY_ROLE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn wake_role_for_artifact(artifact: &str) -> Option<&'static str> {
    match artifact {
        "agent_state/wakeup_planner.flag" => Some("planner"),
        "agent_state/wakeup_solo.flag" => Some("solo"),
        "agent_state/wakeup_verifier.flag" => Some("verifier"),
        "agent_state/wakeup_diagnostics.flag" => Some("diagnostics"),
        "agent_state/wakeup_executor.flag" => Some("executor"),
        _ => None,
    }
}

fn canonical_wake_signatures_from_tlog(
    agent_state_dir: &std::path::Path,
    state: &SystemState,
    two_role_mode: bool,
) -> std::collections::HashMap<&'static str, (u64, String)> {
    let mut latest_by_role = std::collections::HashMap::new();
    let tlog_path = agent_state_dir.join("tlog.ndjson");
    let records = Tlog::read_records(&tlog_path).unwrap_or_default();
    for record in records {
        let Event::Effect {
            event:
                EffectEvent::WorkspaceArtifactWriteApplied {
                    artifact,
                    signature,
                    ..
                },
        } = record.event
        else {
            continue;
        };
        let Some(role) = wake_role_for_artifact(&artifact) else {
            continue;
        };
        if !runtime_role_enabled(role, two_role_mode) {
            continue;
        }
        let replace = match latest_by_role.get(role) {
            None => true,
            Some((existing_ts_ms, _)) => record.ts_ms >= *existing_ts_ms,
        };
        if replace {
            latest_by_role.insert(role, (record.ts_ms, signature));
        }
    }
    let mut by_role = std::collections::HashMap::new();
    for (role, (ts_ms, signature)) in latest_by_role {
        let consumed_latest =
            state.wake_signal_signatures.get(role).map(String::as_str) == Some(signature.as_str());
        if !consumed_latest {
            by_role.insert(role, (ts_ms, signature));
        }
    }
    by_role
}

fn collect_wake_flag_inputs(
    agent_state_dir: &std::path::Path,
    state: &SystemState,
    two_role_mode: bool,
) -> (
    Vec<WakeFlagInput>,
    std::collections::HashMap<&'static str, std::path::PathBuf>,
    std::collections::HashMap<&'static str, String>,
) {
    let mut flag_paths: Vec<(&str, std::path::PathBuf)> = vec![
        ("planner", agent_state_dir.join("wakeup_planner.flag")),
        ("executor", agent_state_dir.join("wakeup_executor.flag")),
    ];
    if !two_role_mode {
        flag_paths.push(("solo", agent_state_dir.join("wakeup_solo.flag")));
        flag_paths.push(("verifier", agent_state_dir.join("wakeup_verifier.flag")));
        flag_paths.push((
            "diagnostics",
            agent_state_dir.join("wakeup_diagnostics.flag"),
        ));
    }

    let mut inputs = Vec::new();
    let mut path_map = std::collections::HashMap::new();
    let mut signature_map = std::collections::HashMap::new();
    let canonical_signals =
        canonical_wake_signatures_from_tlog(agent_state_dir, state, two_role_mode);
    for (role, (modified_ms, signature)) in canonical_signals {
        inputs.push(WakeFlagInput { role, modified_ms });
        signature_map.insert(role, signature);
    }
    for (role, path) in flag_paths {
        if !runtime_role_enabled(role, two_role_mode) {
            continue;
        }
        if path.exists() {
            path_map.insert(role, path.clone());
        }
        if signature_map.contains_key(role) || !path.exists() {
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
    }

    (inputs, path_map, signature_map)
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

fn record_canonical_inbound_message(
    workspace: &Path,
    from_role: &str,
    to_role: &str,
    message: &str,
) -> Result<String> {
    let signature = artifact_write_signature(&[
        "inbound_message",
        from_role,
        to_role,
        &message.len().to_string(),
        message,
    ]);
    record_effect_for_workspace(
        workspace,
        EffectEvent::InboundMessageRecorded {
            from_role: from_role.to_string(),
            to_role: to_role.to_string(),
            message: message.to_string(),
            signature: signature.clone(),
        },
    )?;
    Ok(signature)
}

fn persist_planner_message(action: &Value) {
    let workspace = Path::new(crate::constants::workspace());
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let planner_path = agent_state_dir.join("last_message_to_planner.json");
    let action_text = serde_json::to_string_pretty(action).unwrap_or_default();
    let from_role = action
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if let Err(err) =
        record_canonical_inbound_message(workspace, from_role, "planner", &action_text)
    {
        eprintln!(
            "[orchestrate] failed to record canonical planner handoff message {}: {err:#}",
            planner_path.display()
        );
    }
    if let Err(err) =
        persist_agent_state_projection(&planner_path, &action_text, "planner_handoff_message")
    {
        eprintln!(
            "[orchestrate] failed to persist planner handoff message {}: {err:#}",
            planner_path.display()
        );
    }
    let wake_path = agent_state_dir.join("wakeup_planner.flag");
    if let Err(err) = persist_agent_state_projection(&wake_path, "handoff", "planner_wakeup") {
        eprintln!(
            "[orchestrate] failed to persist planner wakeup flag {}: {err:#}",
            wake_path.display()
        );
    }
}

fn persist_planner_blocker_message(action: &Value) -> bool {
    let evidence = action
        .get("payload")
        .and_then(|payload| payload.get("evidence"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    if !evidence.is_empty() {
        let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
        if let Ok(prev) = std::fs::read_to_string(&evidence_path) {
            if prev.trim() == evidence {
                return false;
            }
        }
        let _ = std::fs::write(&evidence_path, &evidence);
    }
    persist_planner_message(action);
    true
}

fn invariant_id_from_reason(reason: &str) -> Option<&str> {
    let start = reason.find("[id=")? + 4;
    let end = reason[start..].find(']')? + start;
    let id = reason[start..end].trim();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn route_gate_blocker_message(reason: &str) -> Value {
    let summary = match invariant_id_from_reason(reason) {
        Some(id) => format!("Executor dispatch blocked by enforced invariant {id}"),
        None => "Executor dispatch blocked by enforced route invariant".to_string(),
    };
    let blocker = if reason.contains("does not exist") {
        "Plan references a path that does not exist yet"
    } else {
        "Executor dispatch blocked by an enforced invariant"
    };
    let required_action = if reason.contains("does not exist") {
        "Revise the plan so the target is created before it is referenced, or retarget the action to an existing path"
    } else {
        "Revise the plan or workspace state so the blocked invariant no longer fires"
    };
    json!({
        "action": "message",
        "from": "executor",
        "to": "planner",
        "type": "blocker",
        "status": "blocked",
        "observation": "Executor routing is blocked by an enforced invariant; planner must repair the plan before more executor work is dispatched.",
        "rationale": "Returning a structured blocker to the planner is more actionable than repeating route-gate stderr lines.",
        "predicted_next_actions": [
            {
                "action": "read_file",
                "intent": "Inspect the current plan and relevant artifacts to locate the invalid path reference."
            },
            {
                "action": "message",
                "intent": "Report a repaired handoff or a narrower blocker after updating the plan."
            }
        ],
        "payload": build_blocker_payload(
            &summary,
            blocker,
            reason,
            required_action,
            "error",
        ),
    })
}

struct LlmResponseContext<'a> {
    role: &'a str,
    endpoint: &'a LlmEndpoint,
    prompt_kind: &'a str,
    submit_only: bool,
}

fn full_exchange_path(kind: &str, ts_ms: u64, who: &str, step: usize) -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir())
        .join("llm_full")
        .join(format!("{ts_ms:013}_{kind}_{who}_message_{step:04}.txt"))
}

fn write_full_exchange(kind: &str, ts_ms: u64, who: &str, step: usize, raw: &str) {
    let path = full_exchange_path(kind, ts_ms, who, step);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, raw);
}

impl<'a> LlmResponseContext<'a> {
    fn log_request(&self, step: usize, exchange_id: &str, prompt: &str, role_schema: &str) {
        let ts_ms = crate::logging::now_ms();
        let who = if role_schema.trim().is_empty() {
            self.role
        } else {
            "system"
        };
        let raw_prompt = if role_schema.trim().is_empty() {
            prompt.to_string()
        } else {
            format!("{}\n\n{}", role_schema.trim_end(), prompt)
        };
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
        write_full_exchange("sent", ts_ms, who, step, &raw_prompt);
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
        let ts_ms = crate::logging::now_ms();
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
        write_full_exchange("received", ts_ms, self.role, step, raw);
    }

    fn handle_submit_ack(&self, step: usize, exchange_id: &str, raw: &str) -> Option<String> {
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
                self.role, step, *reaction_only_streak
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
) -> Result<AgentCompletion> {
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
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
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
                None,
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
        None,
    )?;
    if done {
        return Ok(
            if action.get("action").and_then(|v| v.as_str()) == Some("message") {
                AgentCompletion::MessageAction {
                    action,
                    summary: out,
                }
            } else {
                AgentCompletion::Summary(out)
            },
        );
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
        None,
        false,
        true,
        true,
        submitted.steps_used,
    )
    .await
}

// ── Agent loop ─────────────────────────────────────────────────────────────────

/// Run one agent role until it calls `message` with status=complete or exhausts MAX_STEPS.
/// Returns the typed completion on success, or an error on hard failure.
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
    mut writer: Option<&mut CanonicalWriter>,
    submit_only: bool,
    check_on_done: bool,
    send_system_prompt: bool,
    initial_steps_used: usize,
) -> Result<AgentCompletion> {
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
                return Ok(AgentCompletion::Summary("shutdown requested".to_string()));
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
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "step_limit",
                &format!("executor reached step limit ({EXECUTOR_STEP_LIMIT})"),
                None,
            );
        }

        let send_system_this_turn =
            should_send_system_prompt(send_system_prompt, endpoint.stateful, step);
        let (role_schema, prompt) = build_agent_prompt(
            role,
            send_system_this_turn,
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
                        return Ok(AgentCompletion::Summary("shutdown requested".to_string()));
                    }
                }
            }
            None => request_future.await,
        };
        let (_req_id, resp) = match req_result {
            Ok(r) => r,
            Err(e) => {
                let err_text = e.to_string();
                eprintln!("[{role}] step={} llm_error: {e}", step + 1);
                ctx.log_error(step + 1, &exchange_id, &err_text);
                if let Some(writer) = writer.as_mut() {
                    let _ =
                        writer.try_record_effect(crate::events::EffectEvent::LlmErrorBoundary {
                            role: role.to_string(),
                            prompt_kind: prompt_kind.to_string(),
                            step: step + 1,
                            endpoint_id: endpoint.id.clone(),
                            exchange_id: exchange_id.clone(),
                            error: err_text.clone(),
                        });
                }
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
                    role,
                    "llm_request",
                    &err_text,
                    None,
                );
                if is_chromium_transport_error(&err_text) {
                    let action = local_transport_blocker_message(role, &err_text, &task_context);
                    let (done, reason) = process_action_and_execute(
                        role,
                        prompt_kind,
                        endpoint,
                        workspace,
                        step + 1,
                        &exchange_id,
                        &action,
                        false,
                        writer.as_deref_mut(),
                    )?;
                    return Ok(if done {
                        AgentCompletion::MessageAction {
                            action,
                            summary: reason,
                        }
                    } else {
                        AgentCompletion::Summary(reason)
                    });
                }
                apply_error_result(
                    role,
                    &task_context,
                    &mut error_streak,
                    &mut last_error,
                    &mut last_result,
                    &err_text,
                    format!(
                        "LLM error: {e}\nReturn exactly one action as a single JSON object in a ```json code block.\n\nTask context:\n{}",
                        truncate(&task_context, MAX_SNIPPET)
                    ),
                );
                step += 1;
                continue;
            }
        };
        last_tab_id = resp.tab_id;
        last_turn_id = resp.turn_id;
        let raw = resp.raw;

        ctx.log_response(step + 1, &exchange_id, &raw);

        if let Some(ack) = ctx.handle_submit_ack(step + 1, &exchange_id, &raw) {
            return Ok(AgentCompletion::Summary(ack));
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
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
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
            true, // always auto-fill message fields so `from` is forced to the actual role
            None,
        ) {
            Ok(action) => action,
            Err(invalid) => {
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
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

        let kind = action
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        if kind == "run_command" {
            let cmd = action.get("cmd").and_then(|v| v.as_str());
            cargo_test_gate.note_action(&kind, cmd);
        }
        if kind != "message"
            && enforce_executor_step_limit(
                role,
                total_steps,
                &mut error_streak,
                &mut last_result,
                workspace,
            )
        {
            step += 1;
            continue;
        }
        if let Some(msg) =
            cargo_test_gate.message_blocker_if_needed(&kind, crate::constants::workspace())
        {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
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
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                role,
                "repeated_failed_action",
                &format!(
                    "identical action payload failed repeatedly: {}",
                    &action_fingerprint
                ),
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
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
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

        if let Some(msg) =
            enforce_diagnostics_python(role, kind.as_str(), &mut diagnostics_eventlog_python_done)
        {
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
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
                crate::blockers::record_action_failure_with_writer(
                    workspace,
                    None,
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
            writer.as_deref_mut(),
        )?;

        match step_result {
            (true, reason) => {
                eprintln!("[{role}] message complete: {reason}");
                return Ok(
                    if action.get("action").and_then(|v| v.as_str()) == Some("message") {
                        AgentCompletion::MessageAction {
                            action,
                            summary: reason,
                        }
                    } else {
                        AgentCompletion::Summary(reason)
                    },
                );
            }
            (false, out) => {
                cargo_test_gate.note_result(&kind, &out);
                if out.starts_with("Error executing action:") {
                    if last_failed_action_fingerprint
                        .as_deref()
                        .is_some_and(|f| f == action_fingerprint)
                    {
                        repeated_failed_action_count =
                            repeated_failed_action_count.saturating_add(1);
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
                write_post_restart_result(
                    role,
                    kind.as_str(),
                    &out,
                    step + 1,
                    last_tab_id,
                    last_turn_id,
                    &endpoint.id,
                    "process_restart",
                );
                last_result = Some(out);
                if kind.as_str() == "apply_patch" {
                    return Ok(AgentCompletion::Summary(last_result.unwrap_or_default()));
                }
            }
        }
        step += 1;
    }
}

/// Write the last completed action result to a state file so it survives a
/// supervisor-triggered restart.  Only the most recent action is kept (overwrites).
fn write_post_restart_result(
    role: &str,
    action: &str,
    result: &str,
    step: usize,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    endpoint_id: &str,
    restart_kind: &str,
) {
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("post_restart_result.json");
    let payload = serde_json::json!({
        "role": role,
        "action": action,
        "result": result,
        "step": step,
        "tab_id": tab_id,
        "turn_id": turn_id,
        "endpoint_id": endpoint_id,
        "restart_kind": restart_kind,
    });
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
}

#[derive(Clone, Debug)]
struct PostRestartResult {
    role: String,
    action: String,
    result: String,
    step: usize,
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    endpoint_id: String,
    restart_kind: String,
}

#[derive(Clone, Debug)]
enum AgentCompletion {
    Summary(String),
    MessageAction { action: Value, summary: String },
}

impl AgentCompletion {
    fn summary_text(&self) -> &str {
        match self {
            Self::Summary(summary) => summary,
            Self::MessageAction { summary, .. } => summary,
        }
    }

    fn into_summary(self) -> String {
        match self {
            Self::Summary(summary) => summary,
            Self::MessageAction { summary, .. } => summary,
        }
    }
}

/// Read the post-restart result file without consuming it.
fn peek_post_restart_result(role: &str) -> Option<PostRestartResult> {
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("post_restart_result.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let saved_role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    // Normalise: executor[executor_pool] → executor
    let role_key = if role.starts_with("executor") {
        "executor"
    } else {
        role
    };
    let saved_key = if saved_role.starts_with("executor") {
        "executor"
    } else {
        saved_role
    };
    if role_key != saved_key {
        return None;
    }
    Some(PostRestartResult {
        role: saved_role.to_string(),
        action: v
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("(unknown)")
            .to_string(),
        result: v
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string(),
        step: v.get("step").and_then(|s| s.as_u64()).unwrap_or(0) as usize,
        tab_id: v.get("tab_id").and_then(|s| s.as_u64()).map(|v| v as u32),
        turn_id: v.get("turn_id").and_then(|s| s.as_u64()),
        endpoint_id: v
            .get("endpoint_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        restart_kind: v
            .get("restart_kind")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Read and consume the post-restart result file.  Returns `Some(result)` if the
/// file exists and was written by `role`, then deletes the file.
fn take_post_restart_result(role: &str) -> Option<PostRestartResult> {
    let result = peek_post_restart_result(role)?;
    let path =
        std::path::Path::new(crate::constants::agent_state_dir()).join("post_restart_result.json");
    // Consume — delete so it isn't re-injected on a second restart
    let _ = std::fs::remove_file(&path);
    Some(result)
}

fn restart_resume_banner(role: &str, resume: &PostRestartResult) -> String {
    let agent_type = role_key(role).to_uppercase();
    let tab_id = resume
        .tab_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "pending".to_string());
    let turn_id = resume
        .turn_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "pending".to_string());
    let prefix = format!(
        "TAB_ID: {tab_id}\nTURN_ID: {turn_id}\nAGENT_TYPE: {agent_type}\n\nSYSTEM RESTART RESUME\n\
         Resume role: {}\nRestart kind: {}\nEndpoint: {}\nLast completed action: `{}` (step {})\n\
         Continue from the last completed action result below. Do not resend the bootstrap prompt.\n",
        resume.role,
        resume.restart_kind,
        if resume.endpoint_id.is_empty() {
            "unknown"
        } else {
            &resume.endpoint_id
        },
        resume.action,
        resume.step,
    );
    let suffix = "\n\nReturn to the same conversation and continue from this result.";
    render_action_result_sections(&prefix, &resume.result, suffix)
}

/// Build a continuation prompt for a resumed session instead of the full bootstrap prompt.
fn build_restart_resume_prompt(role: &str, resume: &PostRestartResult) -> String {
    restart_resume_banner(role, resume)
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

/// Non-serializable runtime-only state.  Everything serializable now lives in
/// `SystemState` (owned by `CanonicalWriter`); this struct holds the objects
/// that contain live OS handles and are therefore not checkpoint-able.
struct RuntimeState {
    submitted_turns: HashMap<(u32, u64), SubmittedExecutorTurn>,
    executor_submit_inflight: HashMap<usize, PendingSubmitState>,
    deferred_completions: HashMap<usize, VecDeque<DeferredExecutorCompletion>>,
}

fn new_runtime_state(lanes: &[LaneConfig]) -> RuntimeState {
    let mut deferred_completions = HashMap::new();
    for lane in lanes {
        deferred_completions.insert(lane.index, VecDeque::new());
    }
    RuntimeState {
        submitted_turns: HashMap::new(),
        executor_submit_inflight: HashMap::new(),
        deferred_completions,
    }
}

#[derive(Clone)]
struct SubmittedExecutorTurn {
    tab_id: u32,
    lane: usize,
    lane_label: String,
    command_id: String,
    started_ms: u64,
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
    let command_id = v
        .get("command_id")
        .and_then(|x| x.as_str())
        .map(str::to_string);
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
            let kind = action
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
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
    writer: &mut CanonicalWriter,
    rt: &mut RuntimeState,
    lanes: &[LaneConfig],
    bridge: &WsBridge,
    workspace: &PathBuf,
    continuation_joinset: &mut tokio::task::JoinSet<(
        SubmittedExecutorTurn,
        u64,
        Result<AgentCompletion>,
    )>,
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
    if handle_executor_completion_message_action(writer, &submitted, lane_cfg, &exec_result) {
        return true;
    }
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
    continuation_joinset: &mut tokio::task::JoinSet<(
        SubmittedExecutorTurn,
        u64,
        Result<AgentCompletion>,
    )>,
) {
    let executor_endpoint = lane_cfg.endpoint.clone();
    let bridge = bridge.clone();
    let workspace = workspace.clone();
    let exec_result = exec_result.to_string();
    let submitted_clone = submitted.clone();
    let tabs = submitted.tabs.clone();
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
        )
        .await;
        (submitted_clone, turn_id, result)
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
    persist_executor_completion_message(writer, &action);
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

fn persist_executor_completion_message(writer: &mut CanonicalWriter, action: &Value) {
    let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let action_text = serde_json::to_string_pretty(action).unwrap_or_default();
    let from_role = action
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("executor");

    // Self-loop guard: an executor message routed back to "executor" would write
    // wakeup_executor.flag, wake the executor next cycle, and then complete again
    // with another self-addressed message — creating an oscillating stall that
    // permanently resets the convergence counter before it can reach the threshold.
    // Redirect such messages to the planner so the loop is broken deterministically.
    let mut effective_to = if to_role.eq_ignore_ascii_case("executor") {
        eprintln!(
            "[orchestrate] executor→executor message detected; redirecting to planner \
             to break self-wake stall loop"
        );
        "planner"
    } else {
        to_role
    };
    if runtime_two_role_mode()
        && !effective_to.eq_ignore_ascii_case("planner")
        && !effective_to.eq_ignore_ascii_case("executor")
    {
        eprintln!(
            "[orchestrate] two-role mode rerouting executor message target `{}` -> `planner`",
            effective_to
        );
        effective_to = "planner";
    }

    if effective_to.eq_ignore_ascii_case("planner") {
        persist_planner_message(action);
        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        return;
    }

    // Generic wakeup for other targets (verifier, diagnostics, etc.)
    let workspace = Path::new(crate::constants::workspace());
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let to_key = effective_to
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    if let Err(err) = record_canonical_inbound_message(workspace, from_role, &to_key, &action_text)
    {
        writer.record_violation(
            "executor_completion_message",
            &format!("failed to record canonical message for {to_key}: {err:#}"),
        );
    }
    let msg_path = agent_state_dir.join(format!("last_message_to_{to_key}.json"));
    if let Err(err) = persist_agent_state_projection(
        &msg_path,
        &action_text,
        &format!("executor_completion_message:{to_key}"),
    ) {
        writer.record_violation(
            "executor_completion_message",
            &format!("failed to persist message for {to_key}: {err:#}"),
        );
    }
    let wake_path = agent_state_dir.join(format!("wakeup_{to_key}.flag"));
    if let Err(err) = persist_agent_state_projection(
        &wake_path,
        "handoff",
        &format!("executor_completion_wakeup:{to_key}"),
    ) {
        writer.record_violation(
            "executor_completion_message",
            &format!("failed to persist wakeup flag for {to_key}: {err:#}"),
        );
    }
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
    crate::objectives::ensure_runtime_objectives_file(workspace)
        .unwrap_or_else(|_| crate::objectives::resolve_objectives_path(workspace))
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
    let ready_tasks =
        crate::prompt_inputs::read_ready_tasks(&std::path::PathBuf::from(workspace()), 10);
    let restart_resume = peek_post_restart_result(&job.executor_role);
    let mut exec_prompt = if let Some(resume) = restart_resume.as_ref() {
        let prompt = build_restart_resume_prompt(&job.executor_role, resume);
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
        append_inbound_to_prompt(&mut exec_prompt, &inbound);
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
        "executor" | "verifier" | "planner" | "diagnostics" | "solo"
    ) {
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
            _ => bail!(
                "invalid --role value: {role} (expected executor|planner|verifier|diagnostics)"
            ),
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
    let diagnostics_rel = diagnostics_file_for_instance(&path_prefix);
    let diagnostics_path = workspace.join(&diagnostics_rel);
    let _ = DIAGNOSTICS_FILE_PATH.set(diagnostics_rel.clone());
    let legacy_diagnostics_rel = format!("PLANS/{}/diagnostics-{}.json", path_prefix, path_prefix);
    if let Err(err) = crate::logging::migrate_projection_if_present(
        &workspace,
        &legacy_diagnostics_rel,
        &diagnostics_rel,
        &diagnostics_rel,
        "baseline_diagnostics_legacy_migration",
    ) {
        eprintln!("[canon-mini-agent] diagnostics migration failed: {err:#}");
    }
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
    if let Err(err) = ensure_workspace_artifact_baseline(&workspace, &diagnostics_path) {
        eprintln!("[canon-mini-agent] workspace artifact bootstrap failed: {err:#}");
        log_error_event(
            "orchestrate",
            "startup",
            None,
            &format!("workspace artifact bootstrap failed: {err:#}"),
            Some(json!({ "stage": "startup" })),
        );
    }
    if let Err(err) = crate::issues::sweep_stale_issues(&workspace) {
        eprintln!("[canon-mini-agent] issue staleness sweep failed: {err:#}");
        log_error_event(
            "orchestrate",
            "startup",
            None,
            &format!("issue staleness sweep failed: {err:#}"),
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
    if !diagnostics_path.exists() {
        let _ = std::fs::write(&diagnostics_path, "");
    }
    let _ = ensure_workspace_artifact_baseline(&workspace, &diagnostics_path);
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
        Arc::<OnceLock<()>>::new(OnceLock::new()),
    );
    eprintln!("[canon-mini-agent] backend ready (chromium ws://127.0.0.1:{ws_port})");

    let tabs = llm_worker_new_tabs();

    if orchestrate {
        const SERVICE_POLL_MS: u64 = 500;
        const PENDING_SUBMIT_TIMEOUT_MS: u64 = 10_000;
        const SUBMITTED_TURN_TIMEOUT_MS: u64 = 120_000;

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

        let diagnostics_ep = find_endpoint(&endpoints, "diagnostics")?.clone();
        let planner_ep = find_endpoint(&endpoints, "mini_planner")?.clone();
        let solo_ep = find_endpoint(&endpoints, "solo")?.clone();
        let verifier_ep = find_endpoint(&endpoints, "verifier")?.clone();

        let tabs_diagnostics = llm_worker_new_tabs();
        let tabs_planner = llm_worker_new_tabs();
        let tabs_solo = llm_worker_new_tabs();
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
        if writer.tlog_seq() == 0 {
            writer.try_apply(ControlEvent::PlannerPendingSet { pending: true })?;
        }
        let two_role_mode = runtime_two_role_mode();
        let mut rt = new_runtime_state(&lanes);

        let mut resume_verifier_items: Vec<ResumeVerifierItem> = Vec::new();
        let mut solo_bootstrapped = false;
        if let Some(checkpoint) = load_checkpoint(&workspace) {
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
            if two_role_mode {
                if !runtime_role_enabled(
                    resume_decision.scheduled_phase.as_deref().unwrap_or(""),
                    true,
                ) {
                    resume_decision.scheduled_phase = None;
                }
                resume_decision.diagnostics_pending = false;
            }
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
            writer.try_record_effect(crate::events::EffectEvent::CheckpointLoaded {
                phase: state.phase.clone(),
            })?;
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
            // Reset lanes that lost ownership (no active tab) so they become pending.
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
        let mut planner_bootstrapped = false;
        let mut diagnostics_bootstrapped = false;
        let mut verifier_bootstrapped = false;
        let mut submit_joinset: tokio::task::JoinSet<(
            usize,
            PendingExecutorSubmit,
            Result<String>,
        )> = tokio::task::JoinSet::new();
        let mut continuation_joinset: tokio::task::JoinSet<(
            SubmittedExecutorTurn,
            u64,
            Result<AgentCompletion>,
        )> = tokio::task::JoinSet::new();
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

        eprintln!("[orchestrate] pipeline started: planner -> background executors -> verifier/diagnostics -> planner");

        const STALL_CYCLE_THRESHOLD: u32 = 5;
        let mut stall_count: u32 = 0;
        let mut last_frames_all_fingerprint: Option<(u128, u64)> = None;

        loop {
            let _ = std::fs::remove_file(cycle_idle_marker_path());
            let mut cycle_progress = false;
            let objectives_path = preferred_objectives_path(&workspace);
            let objectives_mtime_before = file_modified_ms(&objectives_path);
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

            let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
            apply_wake_flags(agent_state_dir, &mut writer, two_role_mode);
            if let Err(err) = record_frames_all_debug_effect_if_changed(
                workspace.as_path(),
                &mut writer,
                &mut last_frames_all_fingerprint,
            ) {
                eprintln!("[orchestrate] frames/all.jsonl snapshot failed: {err:#}");
            }

            if writer.state().scheduled_phase.is_none() && writer.state().phase == "bootstrap" {
                if let Some(phase) = decide_bootstrap_phase(start_role) {
                    let phase = if runtime_role_enabled(&phase, two_role_mode) {
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
                    if phase == "diagnostics" && !two_role_mode {
                        writer.apply(ControlEvent::DiagnosticsPendingSet { pending: true });
                    }
                    if phase == "solo" && !two_role_mode {
                        writer.apply(ControlEvent::ScheduledPhaseSet {
                            phase: Some("solo".to_string()),
                        });
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
                        if ready_tasks_exist && !lanes_seeded {
                            eprintln!(
                                "[orchestrate] bootstrap executor rerouted to planner: ready tasks exist but no lane work is seeded"
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

            if two_role_mode {
                apply_diagnostics_pending_if_changed(&mut writer, false);
                let scheduled_phase = writer.state().scheduled_phase.clone();
                if scheduled_phase
                    .as_deref()
                    .is_some_and(|phase| !runtime_role_enabled(phase, true))
                {
                    apply_scheduled_phase_if_changed(&mut writer, None);
                }
            }

            let active_blocker = writer.state().active_blocker_to_verifier;
            let semantic_control = SemanticControlState::from_system_state(
                writer.state(),
                !verifier_pending_results.is_empty(),
                !verifier_joinset.is_empty(),
            );
            let blocker_decision = if two_role_mode {
                crate::state_space::ActiveBlockerDecision {
                    planner_pending: writer.state().planner_pending,
                    scheduled_phase: sanitize_phase_for_runtime(
                        writer.state().scheduled_phase.as_deref(),
                        true,
                    ),
                }
            } else {
                semantic_control.active_blocker_decision()
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

            let state_hash_before = cycle_control_hash(&ControlConvergenceSnapshot {
                state: writer.state(),
                active_blocker,
                verifier_pending: !verifier_pending_results.is_empty(),
                verifier_running: !verifier_joinset.is_empty(),
            });

            let semantic_control = SemanticControlState::from_system_state(
                writer.state(),
                !verifier_pending_results.is_empty(),
                !verifier_joinset.is_empty(),
            );
            let mut phase_gates = semantic_control.phase_gates();
            if two_role_mode {
                phase_gates.verifier = false;
                phase_gates.diagnostics = false;
                phase_gates.solo = false;
            }

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
            };
            let cargo_test_failures = load_cargo_test_failures(&workspace);

            if phase_gates.planner {
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

            if phase_gates.solo && !two_role_mode {
                writer.apply(ControlEvent::PhaseSet {
                    phase: "solo".to_string(),
                    lane: None,
                });
                if run_solo_phase(
                    &orchestrator_ctx,
                    &mut writer,
                    &mut solo_bootstrapped,
                    &cargo_test_failures,
                )
                .await
                {
                    cycle_progress = true;
                } else {
                    writer.apply(ControlEvent::PlannerPendingSet { pending: true });
                    cycle_progress = true;
                }
            }

            let now = now_ms();
            if phase_gates.executor {
                if run_executor_phase(
                    &orchestrator_ctx,
                    &mut writer,
                    &mut rt,
                    now,
                    PENDING_SUBMIT_TIMEOUT_MS,
                    SUBMITTED_TURN_TIMEOUT_MS,
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

            let mut verifier_changed = false;
            if !two_role_mode
                && (!verifier_pending_results.is_empty() || !verifier_joinset.is_empty())
            {
                let (phase_progress, phase_changed) = run_verifier_phase(
                    &orchestrator_ctx,
                    &mut writer,
                    &mut verifier_pending_results,
                    &mut verifier_joinset,
                    &mut verifier_bootstrapped,
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

            if !two_role_mode && verifier_changed {
                writer.apply(ControlEvent::DiagnosticsVerifierFollowupQueued);
            }
            let semantic_control = SemanticControlState::from_system_state(
                writer.state(),
                !verifier_pending_results.is_empty(),
                !verifier_joinset.is_empty(),
            );
            if !two_role_mode
                && semantic_control.diagnostics_pending
                && semantic_control.diagnostics_allowed()
            {
                writer.apply(ControlEvent::PhaseSet {
                    phase: "diagnostics".to_string(),
                    lane: None,
                });
                if run_diagnostics_phase(
                    &orchestrator_ctx,
                    &mut writer,
                    &mut diagnostics_bootstrapped,
                    verifier_changed,
                    &cargo_test_failures,
                )
                .await
                {
                    cycle_progress = true;
                }
            }

            if writer.state().scheduled_phase.is_some() {
                let (executor_lane_pending, executor_in_progress) = writer
                    .state()
                    .phase_lane
                    .and_then(|lane_id| writer.state().lanes.get(&lane_id))
                    .map(|lane| (lane.pending, lane.in_progress_by.is_some()))
                    .unwrap_or((false, false));
                let semantic_control = SemanticControlState::from_system_state(
                    writer.state(),
                    !verifier_pending_results.is_empty(),
                    !verifier_joinset.is_empty(),
                );
                if semantic_control
                    .scheduled_phase_done(executor_lane_pending, executor_in_progress)
                {
                    apply_scheduled_phase_if_changed(&mut writer, None);
                }
            }

            let objectives_mtime_after = file_modified_ms(&objectives_path);
            let plan_mtime_after = file_modified_ms(&master_plan_path);
            let objective_review_required = plan_mtime_before != plan_mtime_after;
            let objectives_updated = objectives_mtime_before != objectives_mtime_after;
            let objectives_text = read_text_or_empty(&objectives_path);
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
            None,
            submit_only,
            inputs.role == "executor",
            true,
            0,
        )
        .await?;
        let _ = std::fs::write(cycle_idle_marker_path(), "idle\n");
        println!("message: {}", reason.into_summary());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        action_retry_fingerprint, canonical_inbound_message_from_tlog,
        canonical_wake_signatures_from_tlog, ensure_workspace_artifact_baseline,
        executor_step_limit_feedback, has_actionable_objectives, inbound_message_from_user,
        invariant_id_from_reason, is_chromium_transport_error, local_transport_blocker_message,
        plan_has_incomplete_tasks, route_gate_blocker_message, should_reject_solo_self_complete,
        take_external_user_message_without_writer, take_inbound_message_without_writer,
        verifier_confirmed_with_plan_text, ActionProvenance,
    };
    use crate::constants::{ISSUES_FILE, MASTER_PLAN_FILE, VIOLATIONS_FILE};
    use crate::events::EffectEvent;
    use crate::logging::{artifact_write_signature, record_effect_for_workspace};
    use crate::system_state::SystemState;
    use crate::{set_agent_state_dir, set_workspace};
    use serde_json::json;
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn global_state_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_workspace(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "canon-mini-agent-app-{label}-{}-{}",
            std::process::id(),
            unique
        ))
    }

    #[test]
    fn route_gate_source_injects_inv_171c039a_tuple_before_evaluation() {
        let source = include_str!("app.rs");
        let invalid_route_count = source
            .find("let orchestrator_invalid_route_count = crate::blockers::count_class_recent(")
            .expect("missing orchestrator invalid_route count wiring");
        let actor_kind_insert = source[invalid_route_count..]
            .find("state.insert(\"actor_kind\".to_string(), \"orchestrator\".to_string());")
            .map(|offset| invalid_route_count + offset)
            .expect("missing orchestrator actor_kind injection");
        let error_class_insert = source[actor_kind_insert..]
            .find("state.insert(\"error_class\".to_string(), \"invalid_route\".to_string());")
            .map(|offset| actor_kind_insert + offset)
            .expect("missing invalid_route error_class injection");
        let route_eval = source[error_class_insert..]
            .find("crate::invariants::evaluate_invariant_gate(\"route\", &state, &ws)")
            .map(|offset| error_class_insert + offset)
            .expect("missing route-gate invariant evaluation");

        assert!(
            invalid_route_count < actor_kind_insert
                && actor_kind_insert < error_class_insert
                && error_class_insert < route_eval,
            "INV-171c039a wiring must inject the exact orchestrator invalid_route tuple before route-gate evaluation"
        );
    }

    #[test]
    fn submit_ack_tab_mismatch_is_canonicalized_before_turn_registration() {
        let source = include_str!("app.rs");
        let mismatch = source
            .find("log_submit_ack_tab_mismatch(ctx, lane_id, active_tab, tab_id);")
            .expect("missing submit ack mismatch log");
        let rebind = source[mismatch..]
            .find("ControlEvent::ExecutorSubmitAckTabRebound {")
            .map(|offset| mismatch + offset)
            .expect("missing canonical submit ack tab rebound");
        let register = source[rebind..]
            .find("register_submitted_executor_turn(")
            .map(|offset| rebind + offset)
            .expect("missing turn registration after submit ack handling");

        assert!(
            mismatch < rebind && rebind < register,
            "submit ack mismatch must emit a canonical tab rebound before turn registration"
        );
    }

    #[test]
    fn executor_bootstrap_with_ready_tasks_wakes_planner_before_silent_idle() {
        let source = include_str!("app.rs");
        let ready_guard = source
            .find("let ready_count = if ready_tasks_text == \"(no ready tasks)\"")
            .expect("missing ready task guard");
        let bootstrap_guard = source[ready_guard..]
            .find(
                "executor bootstrap: ready tasks exist but no lane work is seeded; waking planner",
            )
            .map(|offset| ready_guard + offset)
            .expect("missing clean-start executor bootstrap guard");
        let planner_wake = source[bootstrap_guard..]
            .find("writer.apply(ControlEvent::PlannerPendingSet { pending: true });")
            .map(|offset| bootstrap_guard + offset)
            .expect("missing planner wake after executor bootstrap guard");
        let lane_claim = source[planner_wake..]
            .find("if let Some(job) = claim_executor_submit(writer, lane) {")
            .map(|offset| planner_wake + offset)
            .expect("missing executor lane claim after bootstrap guard");

        assert!(
            ready_guard < bootstrap_guard && bootstrap_guard < planner_wake && planner_wake < lane_claim,
            "executor bootstrap guard must wake planner before lane claim to avoid clean-start idle stalls"
        );
    }

    #[test]
    fn invariant_id_is_extracted_from_gate_reason() {
        let reason = "invariant gate blocked role `executor`: Action targeted a path that does not exist — plan is referencing a target that has not been created yet [id=INV-47232c36]";
        assert_eq!(invariant_id_from_reason(reason), Some("INV-47232c36"));
    }

    #[test]
    fn route_gate_blocker_message_is_structured_for_planner_repair() {
        let reason = "invariant gate blocked role `executor`: Action targeted a path that does not exist — plan is referencing a target that has not been created yet [id=INV-47232c36]";
        let message = route_gate_blocker_message(reason);
        assert_eq!(
            message.get("action").and_then(|v| v.as_str()),
            Some("message")
        );
        assert_eq!(message.get("to").and_then(|v| v.as_str()), Some("planner"));
        assert_eq!(
            message.get("type").and_then(|v| v.as_str()),
            Some("blocker")
        );
        assert_eq!(
            message.get("status").and_then(|v| v.as_str()),
            Some("blocked")
        );
        let payload = message.get("payload").expect("payload");
        assert_eq!(
            payload.get("summary").and_then(|v| v.as_str()),
            Some("Executor dispatch blocked by enforced invariant INV-47232c36")
        );
        assert_eq!(
            payload.get("blocker").and_then(|v| v.as_str()),
            Some("Plan references a path that does not exist yet")
        );
        assert_eq!(
            payload.get("evidence").and_then(|v| v.as_str()),
            Some(reason)
        );
    }

    #[test]
    fn chromium_transport_errors_are_detected_for_local_blocker_synthesis() {
        assert!(is_chromium_transport_error(
            "chromium: early transport failure (heartbeat_after_user_echo_before_turn_complete) (tab=1 turn=2)"
        ));
        assert!(is_chromium_transport_error(
            "chromium: timeout waiting for SUBMIT_ACK (tab=1 turn=2)"
        ));
        assert!(!is_chromium_transport_error("schema validation failed"));
    }

    #[test]
    fn local_transport_blocker_message_routes_without_extra_llm_turn() {
        let action = local_transport_blocker_message(
            "planner",
            "chromium: early transport failure (heartbeat_after_user_echo_before_turn_complete) (tab=633187572 turn=4)",
            "Planner task context",
        );
        assert_eq!(
            action.get("action").and_then(|v| v.as_str()),
            Some("message")
        );
        assert_eq!(action.get("from").and_then(|v| v.as_str()), Some("planner"));
        assert_eq!(action.get("to").and_then(|v| v.as_str()), Some("executor"));
        assert_eq!(action.get("type").and_then(|v| v.as_str()), Some("blocker"));
        let payload = action.get("payload").expect("payload");
        assert_eq!(
            payload.get("blocker").and_then(|v| v.as_str()),
            Some("Chromium transport/runtime failure prevented a usable assistant completion")
        );
        assert!(payload
            .get("evidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("heartbeat_after_user_echo_before_turn_complete"));
    }

    #[test]
    fn inbound_message_from_user_detects_external_user_sender() {
        let inbound = r#"{"action":"message","from":"user","to":"solo","type":"handoff","status":"ready","payload":{"summary":"hello"}}"#;
        assert!(inbound_message_from_user(inbound));
    }

    #[test]
    fn inbound_message_from_user_rejects_non_user_sender() {
        let inbound = r#"{"action":"message","from":"planner","to":"solo","type":"handoff","status":"ready","payload":{"summary":"hello"}}"#;
        assert!(!inbound_message_from_user(inbound));
    }

    #[test]
    fn inbound_message_without_writer_ignores_projection_without_canonical_tlog_record() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("projection-only-inbound");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        fs::write(
            state_dir.join("last_message_to_planner.json"),
            serde_json::to_string_pretty(&json!({
                "action": "message",
                "from": "executor",
                "to": "planner",
                "payload": {"summary": "projection only"}
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(take_inbound_message_without_writer("planner").is_none());
    }

    #[test]
    fn external_user_message_without_writer_ignores_projection_without_canonical_tlog_record() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("projection-only-external");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        fs::write(
            state_dir.join("external_user_message_to_executor.json"),
            serde_json::to_string_pretty(&json!({
                "kind": "external_user_message",
                "from": "user",
                "to": "executor",
                "message": "projection only"
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(take_external_user_message_without_writer("executor").is_none());
    }

    #[test]
    fn external_user_message_without_writer_reads_canonical_tlog_when_projection_missing() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("canonical-external");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        let message = serde_json::to_string_pretty(&json!({
            "kind": "external_user_message",
            "from": "user",
            "to": "executor",
            "message": "canonical event"
        }))
        .unwrap();
        let signature = artifact_write_signature(&[
            "external_user_message",
            "executor",
            &message.len().to_string(),
            message.as_str(),
        ]);
        record_effect_for_workspace(
            &workspace,
            EffectEvent::ExternalUserMessageRecorded {
                to_role: "executor".to_string(),
                message: message.clone(),
                signature,
            },
        )
        .unwrap();

        let recovered = take_external_user_message_without_writer("executor").unwrap();
        assert!(recovered.contains("canonical event"));
    }

    #[test]
    fn canonical_inbound_message_skips_historical_replay_when_latest_consumed() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("canonical-inbound-latest-only");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        record_effect_for_workspace(
            &workspace,
            EffectEvent::InboundMessageRecorded {
                from_role: "planner".to_string(),
                to_role: "executor".to_string(),
                message: "{\"payload\":{\"summary\":\"old\"}}".to_string(),
                signature: "sig-old".to_string(),
            },
        )
        .unwrap();
        record_effect_for_workspace(
            &workspace,
            EffectEvent::InboundMessageRecorded {
                from_role: "planner".to_string(),
                to_role: "executor".to_string(),
                message: "{\"payload\":{\"summary\":\"new\"}}".to_string(),
                signature: "sig-new".to_string(),
            },
        )
        .unwrap();

        let mut state = SystemState::new(&[], 0);
        state
            .inbound_message_signatures
            .insert("executor".to_string(), "sig-new".to_string());

        assert!(canonical_inbound_message_from_tlog(&state_dir, &state, "executor").is_none());
    }

    #[test]
    fn canonical_wake_signals_skip_historical_replay_when_latest_consumed() {
        let _guard = global_state_lock().lock().expect("lock");
        let workspace = temp_workspace("canonical-wake-latest-only");
        let state_dir = workspace.join("agent_state");
        fs::create_dir_all(&state_dir).unwrap();
        set_workspace(workspace.to_string_lossy().to_string());
        set_agent_state_dir(state_dir.to_string_lossy().to_string());

        record_effect_for_workspace(
            &workspace,
            EffectEvent::WorkspaceArtifactWriteApplied {
                artifact: "agent_state/wakeup_planner.flag".to_string(),
                op: "write".to_string(),
                target: state_dir
                    .join("wakeup_planner.flag")
                    .to_string_lossy()
                    .into_owned(),
                subject: "handoff_wakeup:executor:planner".to_string(),
                signature: "wake-old".to_string(),
            },
        )
        .unwrap();
        record_effect_for_workspace(
            &workspace,
            EffectEvent::WorkspaceArtifactWriteApplied {
                artifact: "agent_state/wakeup_planner.flag".to_string(),
                op: "write".to_string(),
                target: state_dir
                    .join("wakeup_planner.flag")
                    .to_string_lossy()
                    .into_owned(),
                subject: "handoff_wakeup:executor:planner".to_string(),
                signature: "wake-new".to_string(),
            },
        )
        .unwrap();

        let mut state = SystemState::new(&[], 0);
        state
            .wake_signal_signatures
            .insert("planner".to_string(), "wake-new".to_string());

        let wakes = canonical_wake_signatures_from_tlog(&state_dir, &state, false);
        assert!(!wakes.contains_key("planner"));
    }

    #[test]
    fn workspace_artifact_baseline_creates_missing_diagnostics_inputs() {
        let workspace = temp_workspace("baseline-create");
        let diagnostics_path = workspace.join("agent_state/default/diagnostics-default.json");

        let created = ensure_workspace_artifact_baseline(&workspace, &diagnostics_path)
            .expect("bootstrap baseline");

        assert!(created.iter().any(|p| p == VIOLATIONS_FILE));
        assert!(created.iter().any(|p| p == MASTER_PLAN_FILE));
        assert!(created.iter().any(|p| p == "agent_state/blockers.json"));
        assert!(created.iter().any(|p| p == "agent_state/tlog.ndjson"));
        assert!(created.iter().any(|p| p == "agent_state/lessons.json"));
        assert!(workspace.join(VIOLATIONS_FILE).exists());
        assert!(workspace.join(ISSUES_FILE).exists());
        assert!(workspace.join(MASTER_PLAN_FILE).exists());
        assert!(workspace.join("agent_state/blockers.json").exists());
        assert!(workspace.join("agent_state/tlog.ndjson").exists());
        assert!(workspace.join("agent_state/lessons.json").exists());
        assert!(diagnostics_path.exists());

        let violations = fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap();
        assert!(violations.contains("\"status\": \"ok\""));

        let plan = fs::read_to_string(workspace.join(MASTER_PLAN_FILE)).unwrap();
        assert!(plan.contains("\"ready_window\": []"));

        let blockers = fs::read_to_string(workspace.join("agent_state/blockers.json")).unwrap();
        assert!(blockers.contains("\"blockers\": []"));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_artifact_baseline_preserves_existing_nonempty_files() {
        let workspace = temp_workspace("baseline-preserve");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(VIOLATIONS_FILE),
            "{\n  \"status\": \"failed\",\n  \"summary\": \"keep\",\n  \"violations\": []\n}\n",
        )
        .unwrap();
        let diagnostics_path = workspace.join("agent_state/default/diagnostics-default.json");

        let created = ensure_workspace_artifact_baseline(&workspace, &diagnostics_path)
            .expect("bootstrap baseline");

        assert!(!created.iter().any(|p| p == VIOLATIONS_FILE));
        let violations = fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap();
        assert!(violations.contains("\"summary\": \"keep\""));

        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn workspace_artifact_baseline_migrates_legacy_root_plan_and_violations() {
        let workspace = temp_workspace("baseline-migrate-legacy");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join("PLAN.json"),
            "{\"version\":2,\"tasks\":[{\"id\":\"T1\",\"status\":\"ready\"}]}",
        )
        .unwrap();
        fs::write(
            workspace.join("VIOLATIONS.json"),
            "{\"status\":\"failed\",\"summary\":\"legacy\",\"violations\":[]}",
        )
        .unwrap();
        let diagnostics_path = workspace.join("agent_state/default/diagnostics-default.json");

        let created = ensure_workspace_artifact_baseline(&workspace, &diagnostics_path)
            .expect("bootstrap baseline");

        assert!(created.iter().any(|p| p == MASTER_PLAN_FILE));
        assert!(created.iter().any(|p| p == VIOLATIONS_FILE));
        assert!(!workspace.join("PLAN.json").exists());
        assert!(!workspace.join("VIOLATIONS.json").exists());
        assert!(workspace.join(MASTER_PLAN_FILE).exists());
        assert!(workspace.join(VIOLATIONS_FILE).exists());
        let plan = fs::read_to_string(workspace.join(MASTER_PLAN_FILE)).unwrap();
        assert!(plan.contains("\"T1\""));
        let violations: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap())
                .unwrap();
        assert_eq!(violations["summary"].as_str(), Some("legacy"));

        let _ = fs::remove_dir_all(workspace);
    }

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
        assert!(
            prompt0.starts_with("TAB_ID: pending\nTURN_ID: pending\nAGENT_TYPE: PLANNER\n\n"),
            "initial prompt must include the identity banner"
        );
        assert!(prompt0.ends_with("INIT"));

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
    fn stateful_endpoints_only_send_system_prompt_on_first_step() {
        assert!(super::should_send_system_prompt(true, true, 0));
        assert!(!super::should_send_system_prompt(true, true, 1));
        assert!(super::should_send_system_prompt(true, false, 1));
        assert!(!super::should_send_system_prompt(false, true, 0));
    }

    #[test]
    fn restart_resume_prompt_is_a_short_continuation_prompt() {
        let resume = super::PostRestartResult {
            role: "planner".to_string(),
            action: "read_file".to_string(),
            result: "file contents".to_string(),
            step: 4,
            tab_id: Some(433977893),
            turn_id: Some(1),
            endpoint_id: "mini_planner_chatgpt".to_string(),
            restart_kind: "process_restart".to_string(),
        };
        let prompt = super::build_restart_resume_prompt("planner", &resume);
        assert!(prompt.contains("SYSTEM RESTART RESUME"));
        assert!(prompt.contains("Resume role: planner"));
        assert!(prompt.contains("Restart kind: process_restart"));
        assert!(prompt.contains("Endpoint: mini_planner_chatgpt"));
        assert!(prompt.contains("Last completed action: `read_file` (step 4)"));
        assert!(prompt.contains("Continue from the last completed action result below."));
        assert!(!prompt.contains("canonical law"));
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
    fn system_state_lane_accessors_return_correct_defaults_and_values() {
        let mut state = SystemState::new(&[7], 1);

        assert!(!state.lane_in_flight(7));
        assert!(!state.lane_submit_active(7));
        assert_eq!(state.lane_next_submit_ms(7), 0);
        assert_eq!(state.lane_steps_used_count(7), 0);
        assert_eq!(state.lane_active_tab_id(7), None);

        // Defaults for absent lanes
        assert!(!state.lane_in_flight(99));
        assert!(!state.lane_submit_active(99));
        assert_eq!(state.lane_next_submit_ms(99), 0);
        assert_eq!(state.lane_steps_used_count(99), 0);
        assert_eq!(state.lane_active_tab_id(99), None);

        state.lane_prompt_in_flight.insert(7, true);
        state.lane_submit_in_flight.insert(7, true);
        state.lane_next_submit_at_ms.insert(7, 42);
        state.lane_steps_used.insert(7, 3);
        state.lane_active_tab.insert(7, 99);

        assert!(state.lane_in_flight(7));
        assert!(state.lane_submit_active(7));
        assert_eq!(state.lane_next_submit_ms(7), 42);
        assert_eq!(state.lane_steps_used_count(7), 3);
        assert_eq!(state.lane_active_tab_id(7), Some(99));
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

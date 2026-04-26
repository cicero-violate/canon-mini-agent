use super::*;

pub(super) fn runtime_role_enabled(role: &str) -> bool {
    matches!(role, "planner" | "executor")
}

pub(super) fn sanitize_phase_for_runtime(phase: Option<&str>) -> Option<String> {
    let phase = phase?;
    if runtime_role_enabled(phase) {
        Some(phase.to_string())
    } else {
        None
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path, &str, &str, &T
/// Outputs: std::result::Result<bool, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn write_json_if_missing_or_empty<T: Serialize>(
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

pub(super) fn touch_file_if_missing_or_empty(path: &Path) -> Result<bool> {
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

pub(super) fn push_created_path(created: &mut Vec<String>, tracked_path: &str, was_created: bool) {
    if was_created {
        created.push(tracked_path.to_string());
    }
}

/// Intent: repair_or_initialize
/// Resource: projection_migration
/// Inputs: &mut std::vec::Vec<std::string::String>, &std::path::Path, &str, &str, &str, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: migrates legacy projection when present and records created tracked path
/// Forbidden: mutation outside projection migration target and created path list
/// Invariants: created path is recorded only through push_created_path using migration result
/// Failure: returns projection migration errors
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn migrate_projection_and_track(
    created: &mut Vec<String>,
    workspace: &Path,
    legacy_name: &str,
    projection_path: &str,
    tracked_path: &str,
    reason: &str,
) -> Result<()> {
    push_created_path(
        created,
        tracked_path,
        crate::logging::migrate_projection_if_present(
            workspace,
            legacy_name,
            projection_path,
            tracked_path,
            reason,
        )?,
    );
    Ok(())
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &mut std::vec::Vec<std::string::String>, &std::path::Path, &std::path::Path, &str, &str, &T
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn write_json_baseline_and_track<T: serde::Serialize>(
    created: &mut Vec<String>,
    workspace: &Path,
    path: &Path,
    tracked_path: &str,
    reason: &str,
    value: &T,
) -> Result<()> {
    push_created_path(
        created,
        tracked_path,
        write_json_if_missing_or_empty(workspace, path, tracked_path, reason, value)?,
    );
    Ok(())
}

/// Intent: repair_or_initialize
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path
/// Outputs: std::result::Result<std::vec::Vec<std::string::String>, anyhow::Error>
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn ensure_workspace_artifact_baseline(
    workspace: &Path,
    planner_projection_path: &Path,
) -> Result<Vec<String>> {
    let mut created = Vec::new();
    let tlog_path = workspace.join("agent_state/tlog.ndjson");
    let tlog_missing_or_empty_before = std::fs::read_to_string(&tlog_path)
        .map(|existing| existing.trim().is_empty())
        .unwrap_or(true);

    ensure_legacy_projection_migrations(workspace, &mut created)?;

    write_workspace_baseline_files(workspace, &mut created)?;

    push_created_path(
        &mut created,
        "agent_state/tlog.ndjson",
        touch_file_if_missing_or_empty(&tlog_path)? || tlog_missing_or_empty_before,
    );

    ensure_lessons_baseline(workspace, &mut created)?;
    ensure_planner_projection_baseline(workspace, planner_projection_path, &mut created)?;

    Ok(created)
}

fn ensure_legacy_projection_migrations(workspace: &Path, created: &mut Vec<String>) -> Result<()> {
    migrate_projection_and_track(
        created,
        workspace,
        "PLAN.json",
        MASTER_PLAN_FILE,
        MASTER_PLAN_FILE,
        "baseline_master_plan_legacy_migration",
    )?;

    migrate_projection_and_track(
        created,
        workspace,
        "VIOLATIONS.json",
        VIOLATIONS_FILE,
        VIOLATIONS_FILE,
        "baseline_violations_legacy_migration",
    )?;

    migrate_projection_and_track(
        created,
        workspace,
        "ISSUES.json",
        ISSUES_FILE,
        ISSUES_FILE,
        "baseline_issues_legacy_migration",
    )?;
    Ok(())
}

fn write_workspace_baseline_files(workspace: &Path, created: &mut Vec<String>) -> Result<()> {
    write_json_baseline_and_track(
        created,
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
    )?;

    write_json_baseline_and_track(
        created,
        workspace,
        &workspace.join(VIOLATIONS_FILE),
        VIOLATIONS_FILE,
        "baseline_violations",
        &crate::reports::ViolationsReport {
            status: "ok".to_string(),
            summary: String::new(),
            violations: Vec::new(),
        },
    )?;

    write_json_baseline_and_track(
        created,
        workspace,
        &workspace.join(ISSUES_FILE),
        ISSUES_FILE,
        "baseline_issues",
        &IssuesFile {
            version: 1,
            ..IssuesFile::default()
        },
    )?;

    write_json_baseline_and_track(
        created,
        workspace,
        &workspace.join("agent_state/blockers.json"),
        "agent_state/blockers.json",
        "baseline_blockers",
        &crate::blockers::BlockersFile {
            version: 1,
            blockers: Vec::new(),
        },
    )?;
    Ok(())
}

pub(super) fn artifact_file_ready(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

pub(super) fn ensure_lessons_baseline(workspace: &Path, created: &mut Vec<String>) -> Result<()> {
    let lessons_path = workspace.join("agent_state/lessons.json");
    if artifact_file_ready(&lessons_path) {
        return Ok(());
    }
    crate::lessons::persist_lessons_projection(
        workspace,
        &LessonsArtifact::default(),
        "baseline_lessons",
    )?;
    created.push("agent_state/lessons.json".to_string());
    Ok(())
}

pub(super) fn ensure_planner_projection_baseline(
    workspace: &Path,
    planner_projection_path: &Path,
    created: &mut Vec<String>,
) -> Result<()> {
    if artifact_file_ready(planner_projection_path) {
        return Ok(());
    }
    crate::reports::persist_diagnostics_projection_with_writer_to_path(
        workspace,
        &crate::reports::DiagnosticsReport {
            status: "ok".to_string(),
            inputs_scanned: Vec::new(),
            ranked_failures: Vec::new(),
            planner_handoff: Vec::new(),
        },
        crate::constants::planner_projection_file(),
        None,
        "baseline_planner_projection",
    )?;
    created.push(planner_projection_path.display().to_string());
    Ok(())
}

/// Extract a string field from a JSON object, returning `""` on missing/non-string.
pub(super) fn jstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Intent: diagnostic_scan
/// Resource: flag_argument_lookup
/// Inputs: &[std::string::String], &str
/// Outputs: std::option::Option<&str>
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns the argument immediately following the first matching flag pair
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn find_flag_arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

pub(super) fn ws_port_is_available(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

pub(super) fn choose_ws_port(args: &[String]) -> Result<(u16, bool)> {
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

pub(super) fn role_key(role: &str) -> &str {
    if role.starts_with("executor") {
        "executor"
    } else if role == "solo" {
        "solo"
    } else {
        role
    }
}

pub(super) fn response_timeout_for_role(role: &str) -> u64 {
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

/// Intent: pure_transform
/// Resource: cargo_test_failure_summary
/// Inputs: &str
/// Outputs: std::string::String
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns raw input for invalid JSON; otherwise emits only known cargo-test failure summary fields
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn summarize_cargo_test_failures(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let mut out = serde_json::Map::new();
    for key in [
        "source",
        "command",
        "output_log",
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

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::string::String
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn load_cargo_test_failures(workspace: &Path) -> String {
    let path = workspace.join("cargo_test_failures.json");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    summarize_cargo_test_failures(&raw)
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &prompt_inputs::SingleRoleContext<'_>, &[llm_runtime::config::LlmEndpoint], bool, bool, bool
/// Outputs: std::result::Result<(prompt_inputs::SingleRoleInputs, llm_runtime::config::LlmEndpoint), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn load_single_role_setup(
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

pub(super) fn trace_message_forwarded(
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

pub(super) fn trace_message_received(
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

pub(super) fn trace_message_common(
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

pub(super) fn trace_orchestrator_forwarded(
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str, &str, &str
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn build_blocker_payload(
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

pub(super) fn file_modified_ms(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

#[derive(Serialize)]
pub(super) struct ControlConvergenceSnapshot<'a> {
    pub(super) state: &'a SystemState,
    pub(super) active_blocker: bool,
    pub(super) verifier_pending: bool,
    pub(super) verifier_running: bool,
}

/// Hash the semantic control snapshot that actually governs routing.
pub(super) fn cycle_control_hash(snapshot: &ControlConvergenceSnapshot<'_>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(snapshot)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Intent: diagnostic_scan
/// Resource: livelock_report
/// Inputs: &std::path::Path, u32, &[&str], bool, bool
/// Outputs: ()
/// Effects: fs_write, state_write
/// Forbidden: network access, process spawning
/// Invariants: report records stall cycles, control surfaces, planner pending flag, and diagnostics pending flag
/// Failure: serialization and report write errors are best-effort suppressed
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn write_livelock_report(
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
        "[orchestrate] livelock detected: {} stall cycles, pending wake signals cleared, \
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
            "livelock detected after {} consecutive no-change cycles; pending wake signals cleared",
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

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: u32, &[&str], bool, bool
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(super) fn build_livelock_report(
    stall_cycles: u32,
    control_surfaces: &[&str],
    planner_pending: bool,
    diagnostics_pending: bool,
) -> Value {
    json!({
        "timestamp_ms": now_ms(),
        "stall_cycles": stall_cycles,
        "watched_control_surfaces": control_surfaces,
        "pending_at_detection": livelock_pending_state(planner_pending, diagnostics_pending),
        "message": format!(
            "Orchestrator detected {} consecutive cycles where work was dispatched but \
             no semantic control state changed. Pending signals cleared. Queue a canonical wake signal or \
             restart to resume.",
            stall_cycles
        ),
    })
}

pub(super) fn livelock_pending_state(planner_pending: bool, diagnostics_pending: bool) -> Value {
    json!({
        "planner_pending": planner_pending,
        "diagnostics_pending": diagnostics_pending,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlannerActionResultClass {
    ReadyHandoff,
    BlockedHandoff,
    CompleteHandoff,
    StayPlanner,
}

pub(super) fn classify_planner_summary_result(summary: &str) -> PlannerActionResultClass {
    let text = summary.to_ascii_lowercase();
    if text.contains("ready task") && text.contains("dispatched") {
        PlannerActionResultClass::ReadyHandoff
    } else {
        PlannerActionResultClass::StayPlanner
    }
}

pub(super) fn classify_planner_message_result(action: &Value) -> PlannerActionResultClass {
    let status = action
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let to_role = action
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if status == "blocked" {
        return PlannerActionResultClass::BlockedHandoff;
    }
    if to_role == "executor" && status == "ready" {
        return PlannerActionResultClass::ReadyHandoff;
    }
    if to_role == "executor" && status == "complete" {
        return PlannerActionResultClass::CompleteHandoff;
    }
    PlannerActionResultClass::StayPlanner
}

pub(super) fn classify_planner_action_result_class(completion: &AgentCompletion) -> PlannerActionResultClass {
    if let AgentCompletion::Summary(summary) = completion {
        return classify_planner_summary_result(summary);
    }
    let AgentCompletion::MessageAction { action, .. } = completion else {
        return PlannerActionResultClass::StayPlanner;
    };
    classify_planner_message_result(action)
}

pub(super) fn planner_completion_allows_executor_dispatch(completion: &AgentCompletion) -> bool {
    !matches!(
        classify_planner_action_result_class(completion),
        PlannerActionResultClass::BlockedHandoff
    )
}

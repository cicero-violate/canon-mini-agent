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
    append_action_log_record, append_action_result_log, append_message_log, append_orchestration_trace,
    compact_log_record, make_command_id, now_ms, init_log_paths,
};
use crate::prompts::{
    action_observation, action_rationale, action_result_prompt, diagnostics_cycle_prompt,
    diagnostics_python_reads_event_logs, executor_cycle_prompt, is_explicit_idle_action,
    normalize_action, parse_actions, planner_cycle_prompt, system_instructions,
    truncate, validate_action, verifier_cycle_prompt, AgentPromptKind,
};
use crate::constants::{
    DEFAULT_RESPONSE_TIMEOUT_SECS, DIAGNOSTICS_FILE_PATH, ENDPOINT_SPECS, INVARIANTS_FILE, MASTER_PLAN_FILE, MAX_SNIPPET,
    EXECUTOR_STEP_LIMIT, MAX_STEPS, OBJECTIVES_FILE, ROLE_TIMEOUT_SECS, SPEC_FILE, VIOLATIONS_FILE, WORKSPACE, WS_PORT_CANDIDATES,
};
use crate::md_convert::ensure_objectives_and_invariants_json;
use std::process::Command;

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

fn extract_progress_path_from_result(result: &str) -> Option<String> {
    for line in result.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("progress_path:") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
        if let Some(idx) = trimmed.find("output_log=") {
            let mut path = trimmed[idx + "output_log=".len()..].trim();
            if let Some(end) = path.find(' ') {
                path = &path[..end];
            }
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn plan_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    if old_text.is_empty() {
        let mut out = String::from("+++ PLAN.json (initial)\n");
        for (idx, line) in new_text.lines().enumerate() {
            if idx >= max_lines {
                out.push_str("... (truncated)\n");
                break;
            }
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }
    if old_text == new_text {
        return "(no changes)".to_string();
    }
    let mut out = String::new();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut emitted = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if emitted >= max_lines {
            out.push_str("... (truncated)\n");
            break;
        }
        match (old_lines.get(i), new_lines.get(j)) {
            (Some(ol), Some(nl)) if ol == nl => {
                i += 1;
                j += 1;
            }
            (Some(ol), Some(nl)) => {
                out.push_str("- ");
                out.push_str(ol);
                out.push('\n');
                out.push_str("+ ");
                out.push_str(nl);
                out.push('\n');
                i += 1;
                j += 1;
                emitted += 2;
            }
            (Some(ol), None) => {
                out.push_str("- ");
                out.push_str(ol);
                out.push('\n');
                i += 1;
                emitted += 1;
            }
            (None, Some(nl)) => {
                out.push_str("+ ");
                out.push_str(nl);
                out.push('\n');
                j += 1;
                emitted += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn default_message_route(role: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    if role.starts_with("executor") {
        ("executor", "verifier", "handoff", "complete")
    } else if role == "verifier" {
        ("verifier", "planner", "verification", "verified")
    } else if role == "diagnostics" {
        ("diagnostics", "planner", "diagnostics", "complete")
    } else if role == "planner" || role == "mini_planner" {
        ("planner", "executor", "plan", "complete")
    } else {
        ("executor", "verifier", "handoff", "complete")
    }
}

fn expected_message_format(from: &str, to: &str, msg_type: &str, status: &str) -> String {
    format!(
        "{{ \"action\": \"message\", \"from\": \"{from}\", \"to\": \"{to}\", \"type\": \"{msg_type}\", \"status\": \"{status}\", \"payload\": {{ \"summary\": \"...\" }} }}"
    )
}

fn canonical_role_label(role: &str) -> &'static str {
    if role.starts_with("executor") {
        "executor"
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
    PathBuf::from("/workspace/ai_sandbox/canon-mini-agent/agent_state/mini_agent_checkpoint.json")
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
    serde_json::from_str(&raw).ok()
}

fn diff_since_last_cycle(current: &str, last: &str) -> String {
    if current.trim().is_empty() {
        return "(no changes)".to_string();
    }
    if current == last {
        return "(no changes)".to_string();
    }
    if last.trim().is_empty() {
        return current.to_string();
    }
    if current.starts_with("(") {
        return current.to_string();
    }
    let last_lines: std::collections::HashSet<&str> = last.lines().collect();
    let mut out_lines = Vec::new();
    for line in current.lines() {
        if !last_lines.contains(line) {
            out_lines.push(line);
        }
    }
    if out_lines.is_empty() {
        "(no changes)".to_string()
    } else {
        let mut out = out_lines.join("\n");
        out.push('\n');
        out
    }
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

fn unsupported_action_templates() -> [&'static str; 13] {
    [
        "```json\n{\n  \"action\": \"list_dir\",\n  \"path\": \".\",\n  \"observation\": \"List workspace root to locate targets.\",\n  \"rationale\": \"Confirm files before acting.\"\n}\n```",
        "```json\n{\n  \"action\": \"read_file\",\n  \"path\": \"canon-utils/canon-loop/src/executor.rs\",\n  \"observation\": \"Read file to understand current logic.\",\n  \"rationale\": \"Need context before patching.\"\n}\n```",
        "```json\n{\n  \"action\": \"apply_patch\",\n  \"patch\": \"*** Begin Patch\\n*** Update File: path/to/file.rs\\n@@\\n- old\\n+ new\\n*** End Patch\",\n  \"observation\": \"Apply the required edit.\",\n  \"rationale\": \"Implement the change directly.\"\n}\n```",
        "```json\n{\n  \"action\": \"run_command\",\n  \"cmd\": \"rg -n \\\"trigger_observe\\\" canon-utils/canon-loop/src/executor.rs\",\n  \"observation\": \"Search for observe triggers.\",\n  \"rationale\": \"Locate all callsites before patching.\"\n}\n```",
        "```json\n{\n  \"action\": \"python\",\n  \"code\": \"import json; print('analyze')\",\n  \"observation\": \"Run structured analysis.\",\n  \"rationale\": \"Use Python for parsing tasks.\"\n}\n```",
        "```json\n{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-runtime\",\n  \"test\": \"optional_test_name\",\n  \"observation\": \"Run tests for the target crate.\",\n  \"rationale\": \"Verify changes.\"\n}\n```",
        "```json\n{\n  \"action\": \"rustc_hir\",\n  \"crate\": \"canon-runtime\",\n  \"mode\": \"hir-tree\",\n  \"extra\": \"\",\n  \"observation\": \"Inspect HIR output.\",\n  \"rationale\": \"Diagnose compiler-level behavior.\"\n}\n```",
        "```json\n{\n  \"action\": \"rustc_mir\",\n  \"crate\": \"canon-runtime\",\n  \"mode\": \"mir\",\n  \"extra\": \"\",\n  \"observation\": \"Inspect MIR output.\",\n  \"rationale\": \"Diagnose compiler-level behavior.\"\n}\n```",
        "```json\n{\n  \"action\": \"graph_call\",\n  \"crate\": \"canon-runtime\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate call graph.\",\n  \"rationale\": \"Inspect call graph output.\"\n}\n```",
        "```json\n{\n  \"action\": \"graph_cfg\",\n  \"crate\": \"canon-runtime\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate CFG graph.\",\n  \"rationale\": \"Inspect CFG output.\"\n}\n```",
        "```json\n{\n  \"action\": \"graph_dataflow\",\n  \"crate\": \"canon-runtime\",\n  \"tlog\": \"\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate dataflow report.\",\n  \"rationale\": \"Inspect dataflow metrics.\"\n}\n```",
        "```json\n{\n  \"action\": \"graph_reachability\",\n  \"crate\": \"canon-runtime\",\n  \"tlog\": \"\",\n  \"out_dir\": \"\",\n  \"observation\": \"Generate reachability report.\",\n  \"rationale\": \"Inspect reachability metrics.\"\n}\n```",
        "```json\n{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"verifier\",\n  \"type\": \"result\",\n  \"status\": \"complete\",\n  \"payload\": {\"summary\": \"...\"},\n  \"observation\": \"Send structured status update.\",\n  \"rationale\": \"Report the outcome to the next role.\"\n}\n```\n```json\n{\n  \"action\": \"message\",\n  \"from\": \"executor\",\n  \"to\": \"planner\",\n  \"type\": \"blocker\",\n  \"status\": \"blocked\",\n  \"observation\": \"Describe the blocked state.\",\n  \"rationale\": \"Explain why progress is impossible without external action.\",\n  \"payload\": {\n    \"summary\": \"Short blocker summary\",\n    \"blocker\": \"Root cause\",\n    \"evidence\": \"Concrete error text or failing command\",\n    \"required_action\": \"What must be done to unblock\",\n    \"severity\": \"error\"\n  }\n}\n```",
    ]
}

fn unsupported_action_correction(kind: &str) -> String {
    let mut msg = String::new();
    msg.push_str(&format!(
        "Invalid action: unsupported action '{kind}'.\nCorrective action required: use a supported tool action. Templates:\n"
    ));
    for template in unsupported_action_templates() {
        msg.push_str(template);
        msg.push('\n');
    }
    msg.push_str("Return exactly one action.");
    msg
}

fn message_schema_correction(missing_field: &str, role: &str) -> String {
    let (from, to, msg_type, status) = default_message_route(role);
    let mut msg = String::new();
    msg.push_str(&format!(
        "Invalid action: message missing non-empty '{missing_field}'.\nCorrective action required: use a full message schema with required fields. Do not use `content`; use `payload`.\nTemplate:\n"
    ));
    msg.push_str(&format!(
        "```json\n{{\n  \"action\": \"message\",\n  \"from\": \"{from}\",\n  \"to\": \"{to}\",\n  \"type\": \"{msg_type}\",\n  \"status\": \"{status}\",\n  \"observation\": \"Summarize what you are sending.\",\n  \"rationale\": \"Explain why this message is being sent.\",\n  \"payload\": {{\n    \"summary\": \"One-line summary for the next role.\",\n    \"details\": \"Optional extra context.\",\n    \"expected_format\": \"{format}\"\n  }}\n}}\n```\nReturn exactly one action.",
        format = expected_message_format(from, to, msg_type, status),
    ));
    msg
}

fn corrective_invalid_action_prompt(action: &Value, err_text: &str, role: &str) -> Option<String> {
    if err_text.contains("unsupported action") {
        let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Some(unsupported_action_correction(kind));
    }
    if let Some(field) = err_text.strip_prefix("message missing non-empty '").and_then(|rest| rest.strip_suffix("'")) {
        return Some(message_schema_correction(field, role));
    }
    if err_text.contains("message missing object payload") {
        return Some(message_schema_correction("payload", role));
    }
    None
}

fn invalid_action_expected_fields(kind: &str) -> Vec<&'static str> {
    match kind {
        "run_command" => vec!["action", "cmd", "observation", "rationale"],
        "read_file" => vec!["action", "path", "observation", "rationale"],
        "apply_patch" => vec!["action", "patch", "observation", "rationale"],
        "cargo_test" => vec!["action", "crate", "observation", "rationale"],
        "list_dir" => vec!["action", "path", "observation", "rationale"],
        "python" => vec!["action", "code", "observation", "rationale"],
        "message" => vec![
            "action",
            "from",
            "to",
            "type",
            "status",
            "payload",
            "observation",
            "rationale",
        ],
        _ => vec!["action", "observation", "rationale"],
    }
}

fn build_invalid_action_feedback(raw_action: Option<&Value>, err_text: &str) -> String {
    let mut schema_diff: Vec<String> = Vec::new();
    let mut expected_fields: Vec<&'static str> = Vec::new();
    if let Some(action) = raw_action {
        let obj = action.as_object();
        let kind = obj
            .and_then(|o| o.get("action"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        expected_fields = invalid_action_expected_fields(kind);
        if let Some(obj) = obj {
            if obj
                .get("observation")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                schema_diff.push("missing field: observation".to_string());
            }
            if obj
                .get("rationale")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                schema_diff.push("missing field: rationale".to_string());
            }
            match kind {
                "run_command" => {
                    if obj.get("cmd").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
                        schema_diff.push("missing field: cmd".to_string());
                    }
                }
                "read_file" | "list_dir" => {
                    if obj.get("path").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
                        schema_diff.push("missing field: path".to_string());
                    }
                }
                "apply_patch" => {
                    if obj.get("patch").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
                        schema_diff.push("missing field: patch".to_string());
                    }
                }
                "python" => {
                    if obj.get("code").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
                        schema_diff.push("missing field: code".to_string());
                    }
                }
                "cargo_test" => {
                    if obj.get("crate").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
                        schema_diff.push("missing field: crate".to_string());
                    }
                }
                "message" => {
                    for field in ["from", "to", "type", "status"] {
                        if let Some(val) = obj.get(field).and_then(|v| v.as_str()) {
                            if val != val.to_lowercase() {
                                schema_diff.push(format!("role casing invalid: {field}={val}"));
                            }
                        }
                        if obj
                            .get(field)
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .is_none()
                        {
                            schema_diff.push(format!("missing field: {field}"));
                        }
                    }
                    if obj.get("payload").and_then(|v| v.as_object()).is_none() {
                        schema_diff.push("missing field: payload".to_string());
                    }
                    if obj.contains_key("blocker")
                        || obj.contains_key("evidence")
                        || obj.contains_key("required_action")
                    {
                        schema_diff.push(
                            "blocker fields must be inside payload: blocker/evidence/required_action".to_string(),
                        );
                    }
                    let is_blocker = obj
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|v| v.eq_ignore_ascii_case("blocker"))
                        .unwrap_or(false)
                        || obj
                            .get("status")
                            .and_then(|v| v.as_str())
                            .map(|v| v.eq_ignore_ascii_case("blocked"))
                            .unwrap_or(false);
                    if is_blocker {
                        let payload = obj.get("payload").and_then(|v| v.as_object());
                        for field in ["blocker", "evidence", "required_action"] {
                            if payload
                                .and_then(|p| p.get(field))
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                schema_diff.push(format!("payload.{field} missing"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let feedback = json!({
        "error_type": "invalid_action",
        "reason": err_text,
        "expected_fields": expected_fields,
        "schema_diff": schema_diff,
    });
    format!(
        "Invalid action rejected.\naction_result:\n{}\nReturn exactly one action as a single JSON object in a ```json code block. No prose outside it.",
        feedback.to_string()
    )
}

fn auto_fill_message_fields(action: &mut Value, role: &str) -> bool {
    let obj = match action.as_object_mut() {
        Some(obj) => obj,
        None => return false,
    };
    if obj.get("action").and_then(|v| v.as_str()) != Some("message") {
        return false;
    }
    let (default_from, default_to, default_type, default_status) = default_message_route(role);
    let mut changed = false;
    if obj.get("from").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
        obj.insert("from".to_string(), Value::String(default_from.to_string()));
        changed = true;
    }
    if obj.get("to").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
        obj.insert("to".to_string(), Value::String(default_to.to_string()));
        changed = true;
    }
    if obj.get("type").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
        obj.insert("type".to_string(), Value::String(default_type.to_string()));
        changed = true;
    }
    if obj.get("status").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).is_none() {
        obj.insert("status".to_string(), Value::String(default_status.to_string()));
        changed = true;
    }
    if obj.get("payload").and_then(|v| v.as_object()).is_none() {
        obj.insert("payload".to_string(), json!({ "summary": "auto-filled message fields" }));
        changed = true;
    }
    if changed {
        let from_val = obj.get("from").and_then(|v| v.as_str()).unwrap_or(default_from).to_string();
        let to_val = obj.get("to").and_then(|v| v.as_str()).unwrap_or(default_to).to_string();
        let type_val = obj.get("type").and_then(|v| v.as_str()).unwrap_or(default_type).to_string();
        let status_val = obj.get("status").and_then(|v| v.as_str()).unwrap_or(default_status).to_string();
        if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
            payload.entry("expected_format").or_insert(Value::String(
                expected_message_format(&from_val, &to_val, &type_val, &status_val),
            ));
        }
        obj.entry("observation".to_string())
            .or_insert(Value::String("Auto-filled missing message fields.".to_string()));
        obj.entry("rationale".to_string())
            .or_insert(Value::String("Repair invalid message schema to continue execution.".to_string()));
    }
    changed
}

fn executor_diff(workspace: &Path, max_lines: usize) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(workspace).args(["diff", "--name-only"]);
    let Ok(output) = cmd.output() else {
        return "(executor diff unavailable: failed to run git diff --name-only)".to_string();
    };
    if !output.status.success() {
        return "(executor diff unavailable: git diff --name-only failed)".to_string();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let files: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !line.starts_with("PLAN.json")
                && !line.starts_with("PLAN.md")
                && !line.starts_with("PLANS/")
                && *line != "VIOLATIONS.json"
                && *line != "DIAGNOSTICS.json"
        })
        .collect();
    if files.is_empty() {
        return "(no executor diff)".to_string();
    }
    let mut diff_cmd = Command::new("git");
    diff_cmd
        .current_dir(workspace)
        .arg("diff")
        .arg("--unified=3")
        .arg("--")
        .args(&files);
    let Ok(diff_out) = diff_cmd.output() else {
        return "(executor diff unavailable: failed to run git diff)".to_string();
    };
    if !diff_out.status.success() {
        return "(executor diff unavailable: git diff failed)".to_string();
    }
    let diff_text = String::from_utf8_lossy(&diff_out.stdout);
    if diff_text.trim().is_empty() {
        return "(no executor diff)".to_string();
    }
    let mut out = String::new();
    for (idx, line) in diff_text.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... (truncated)\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn take_inbound_message(role: &str) -> Option<String> {
    let role_key = role.trim().to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new("/workspace/ai_sandbox/canon-mini-agent/agent_state");
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
    let active_blocker_to_verifier = agent_state_dir.join("active_blocker_to_verifier.json");
    let planner_wake = agent_state_dir.join("wakeup_planner.flag");
    let verifier_wake = agent_state_dir.join("wakeup_verifier.flag");
    let diagnostics_wake = agent_state_dir.join("wakeup_diagnostics.flag");
    let executor_wake = agent_state_dir.join("wakeup_executor.flag");

    let mut newest_wake: Option<(&str, std::path::PathBuf, std::time::SystemTime)> = None;
    for (role, path) in [
        ("planner", planner_wake),
        ("verifier", verifier_wake),
        ("diagnostics", diagnostics_wake),
        ("executor", executor_wake),
    ] {
        if !path.exists() {
            continue;
        }
        if role == "planner" && active_blocker_to_verifier.exists() {
            continue;
        }
        let modified = path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let replace = match newest_wake {
            None => true,
            Some((_, _, prev_modified)) => modified > prev_modified,
        };
        if replace {
            newest_wake = Some((role, path, modified));
        }
    }

    if let Some((role, path, _)) = newest_wake {
        *scheduled_phase = Some(role.to_string());
        eprintln!(
            "[orchestrate] wake_flag_triggered: role={} path={}",
            role,
            path.display()
        );
        match role {
            "planner" => {
                dispatch_state.planner_pending = true;
            }
            "diagnostics" => {
                dispatch_state.diagnostics_pending = true;
            }
            "executor" => {
                for lane in dispatch_state.lanes.values_mut() {
                    lane.pending = true;
                    lane.in_progress_by = None;
                }
            }
            _ => {}
        }
        let _ = std::fs::remove_file(path);
    }
}

fn is_verifier_specific_blocker(payload: &Value) -> bool {
    let blocker = payload.get("blocker").and_then(|v| v.as_str()).unwrap_or("");
    let required = payload
        .get("required_action")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let combined = format!("{} {}", blocker.to_lowercase(), required.to_lowercase());
    combined.contains("verifier")
}

fn try_parse_blocker(raw: &str) -> Option<(String, String, Value)> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if msg_type != "blocker" || status != "blocked" {
        return None;
    }
    let from = value.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = value.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let payload = value.get("payload").cloned().unwrap_or_else(|| Value::Null);
    Some((from, to, payload))
}

fn persist_planner_message(action: &Value) {
    let agent_state_dir =
        std::path::Path::new("/workspace/ai_sandbox/canon-mini-agent/agent_state");
    let _ = std::fs::create_dir_all(agent_state_dir);
    let planner_path = agent_state_dir.join("last_message_to_planner.json");
    let _ = std::fs::write(
        &planner_path,
        serde_json::to_string_pretty(action).unwrap_or_default(),
    );
    let _ = std::fs::write(agent_state_dir.join("wakeup_planner.flag"), "handoff");
}

#[derive(Clone)]
struct LaneConfig {
    index: usize,
    endpoint: LlmEndpoint,
    plan_file: String,
    label: String,
    tabs: TabManagerHandle,
}

async fn continue_executor_completion(
    submitted: &SubmittedExecutorTurn,
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

    let actions = match parse_actions(completion_text) {
        Ok(actions) => actions,
        Err(e) => {
            if let Some(guard_action) = guardrail_action_from_raw(completion_text, role) {
                if let Err(log_err) = append_message_log(
                    role,
                    endpoint,
                    prompt_kind,
                    step,
                    command_id,
                    "llm_guardrail_action",
                    json!({
                        "error": e.to_string(),
                        "raw": truncate(completion_text, MAX_SNIPPET),
                        "action": guard_action,
                    }),
                ) {
                    eprintln!("[{role}] step={} action_log_error: {log_err}", step);
                }
                vec![guard_action]
            } else {
                if let Err(log_err) = append_message_log(
                    role,
                    endpoint,
                    prompt_kind,
                    step,
                    command_id,
                    "llm_parse_error",
                    json!({
                        "error": e.to_string(),
                        "raw": truncate(completion_text, MAX_SNIPPET),
                    }),
                ) {
                    eprintln!("[{role}] step={} action_log_error: {log_err}", step);
                }
                append_orchestration_trace(
                    "executor_tool_result_forwarded",
                    json!({
                        "lane_name": submitted.lane_label,
                        "tab_id": submitted.tab_id,
                        "turn_id": command_id,
                        "status": "parse_error",
                    }),
                );
                return Err(anyhow!("executor parse_error: {e}"));
            }
        }
    };

    if actions.len() != 1 {
        let msg = format!("Got {} actions — emit exactly one action per turn.", actions.len());
        if let Err(log_err) = append_message_log(
            role,
            endpoint,
            prompt_kind,
            step,
            command_id,
            "llm_invalid_action_count",
            json!({
                "action_count": actions.len(),
                "raw": truncate(completion_text, MAX_SNIPPET),
            }),
        ) {
            eprintln!("[{role}] step={} action_log_error: {log_err}", step);
        }
        append_orchestration_trace(
            "executor_tool_result_forwarded",
            json!({
                "lane_name": submitted.lane_label,
                "tab_id": submitted.tab_id,
                "turn_id": command_id,
                "status": "invalid_action_count",
                "action_count": actions.len(),
            }),
        );
        return Err(anyhow!("executor invalid_action_count: {msg}"));
    }

    let mut action = actions[0].clone();
    if let Err(e) = normalize_action(&mut action) {
        let msg = format!(
            "Invalid action: {e}\nReturn exactly one action with a non-empty `observation`, a non-empty `rationale`, and any required fields."
        );
        if let Err(log_err) = append_message_log(
            role,
            endpoint,
            prompt_kind,
            step,
            command_id,
            "llm_invalid_action",
            json!({
                "stage": "normalize_action",
                "error": e.to_string(),
                "raw": truncate(completion_text, MAX_SNIPPET),
            }),
        ) {
            eprintln!("[{role}] step={} action_log_error: {log_err}", step);
        }
        return Err(anyhow!("executor invalid_action: {msg}"));
    }

    auto_fill_message_fields(&mut action, role);
    if let Err(e) = validate_action(&action) {
        if let Some(prompt) = corrective_invalid_action_prompt(&action, &e.to_string(), role) {
            let msg = format!(
                "{prompt}\nReturn exactly one action with a non-empty `observation`, a non-empty `rationale`, and any required fields."
            );
            if let Err(log_err) = append_message_log(
                role,
                endpoint,
                prompt_kind,
                step,
                command_id,
                "llm_invalid_action",
                json!({
                    "stage": "validate_action",
                    "error": e.to_string(),
                    "raw": truncate(completion_text, MAX_SNIPPET),
                    "action": action.clone(),
                }),
            ) {
                eprintln!("[{role}] step={} action_log_error: {log_err}", step);
            }
            return Err(anyhow!("executor invalid_action: {msg}"));
        }
        let msg = format!(
            "Invalid action: {e}\nReturn exactly one action with a non-empty `observation`, a non-empty `rationale`, and any required fields."
        );
        if let Err(log_err) = append_message_log(
            role,
            endpoint,
            prompt_kind,
            step,
            command_id,
            "llm_invalid_action",
            json!({
                "stage": "validate_action",
                "error": e.to_string(),
                "raw": truncate(completion_text, MAX_SNIPPET),
                "action": action.clone(),
            }),
        ) {
            eprintln!("[{role}] step={} action_log_error: {log_err}", step);
        }
        return Err(anyhow!("executor invalid_action: {msg}"));
    }

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
            "tab_id": submitted.tab_id,
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
            Some(submitted.tab_id),
            Some(turn_id),
            agent_type.as_str(),
            &out,
            action.get("action").and_then(|v| v.as_str()),
            Some(EXECUTOR_STEP_LIMIT.saturating_sub(submitted.steps_used)),
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
    let mut require_tail_path: Option<String> = None;
    let task_context = initial_prompt.clone();
    let mut diagnostics_eventlog_python_done = false;
    let mut idle_streak = 0usize;
    let shutdown = shutdown_signal();

    loop {
        if let Some(sig) = shutdown.as_ref() {
            if sig.flag.load(Ordering::SeqCst) {
                return Ok("shutdown requested".to_string());
            }
        }
        if step >= MAX_STEPS {
            bail!("[{role}] exhausted {MAX_STEPS} steps without completing");
        }

        let total_steps = if role == "executor" {
            initial_steps_used.saturating_add(step)
        } else {
            step
        };

        if role == "executor" && total_steps >= EXECUTOR_STEP_LIMIT {
            last_result = Some(format!(
                "Step limit reached: executor must send a message to planner after {EXECUTOR_STEP_LIMIT} actions. Use a message action with evidence or blocker details."
            ));
        }

        let (role_schema, prompt) = if step == 0 {
            (
                if send_system_prompt {
                    system_instructions.to_string()
                } else {
                    String::new()
                },
                initial_prompt.clone(),
            )
        } else {
            let mut result = last_result.as_deref().unwrap_or("").to_string();
            if role == "executor" {
                let remaining = EXECUTOR_STEP_LIMIT.saturating_sub(total_steps);
                result = format!("step_limit_remaining: {remaining}\n{result}");
            }
            let agent_type = role_key(role).to_uppercase();
            (
                String::new(),
                action_result_prompt(
                    last_tab_id,
                    last_turn_id,
                    agent_type.as_str(),
                    &result,
                    last_action.as_deref(),
                    if role == "executor" {
                        Some(EXECUTOR_STEP_LIMIT.saturating_sub(total_steps))
                    } else {
                        None
                    },
                ),
            )
        };
        let exchange_id = make_command_id(role, prompt_kind, step + 1);

        eprintln!("[{role}] step={} prompt_bytes={}", step + 1, prompt.len());
        if let Err(e) = append_message_log(
            role,
            endpoint,
            prompt_kind,
            step + 1,
            &exchange_id,
            "llm_request",
            json!({
                "submit_only": submit_only,
                "prompt_bytes": prompt.len(),
                "role_schema_bytes": role_schema.len(),
                "prompt": truncate(&prompt, MAX_SNIPPET),
            }),
        ) {
            eprintln!("[{role}] step={} action_log_error: {e}", step + 1);
        }
        append_orchestration_trace(
            "llm_message_forwarded",
            json!({
                "role": role,
                "prompt_kind": prompt_kind,
                "step": step + 1,
                "endpoint_id": endpoint.id,
                "submit_only": submit_only,
                "prompt_bytes": prompt.len(),
            }),
        );

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
                error_streak = error_streak.saturating_add(1);
                last_error = Some(e.to_string());
                if let Err(log_err) = append_message_log(
                    role,
                    endpoint,
                    prompt_kind,
                    step + 1,
                    &exchange_id,
                    "llm_error",
                    json!({
                        "error": e.to_string(),
                    }),
                ) {
                    eprintln!("[{role}] step={} action_log_error: {log_err}", step + 1);
                }
                last_result = Some(format!(
                    "LLM error: {e}\nReturn exactly one action as a single JSON object in a ```json code block.\n\nTask context:\n{}",
                    truncate(&task_context, MAX_SNIPPET)
                ));
                if error_streak >= 3 {
                    last_result = Some(blocker_escalation_prompt(
                        role,
                        last_error.as_deref().unwrap_or("LLM error"),
                        &task_context,
                    ));
                }
                step += 1;
                continue;
            }
        };
        let _ = req_id;
        last_tab_id = resp.tab_id;
        last_turn_id = resp.turn_id;
        let raw = resp.raw;

        append_orchestration_trace(
            "llm_message_received",
            json!({
                "role": role,
                "prompt_kind": prompt_kind,
                "step": step + 1,
                "endpoint_id": endpoint.id,
                "submit_only": submit_only,
                "response_bytes": raw.len(),
            }),
        );
        if let Err(e) = append_message_log(
            role,
            endpoint,
            prompt_kind,
            step + 1,
            &exchange_id,
            "llm_response",
            json!({
                "submit_only": submit_only,
                "response_bytes": raw.len(),
                "raw": truncate(&raw, MAX_SNIPPET),
            }),
        ) {
            eprintln!("[{role}] step={} action_log_error: {e}", step + 1);
        }

        if submit_only {
            if let Ok(mut ack) = serde_json::from_str::<Value>(&raw) {
                if ack.get("submit_ack").and_then(|v| v.as_bool()) == Some(true) {
                    ack["command_id"] = Value::String(exchange_id.clone());
                    eprintln!("[{role}] step={} submit_ack={}", step + 1, raw);
                    if let Err(e) = append_message_log(
                        role,
                        endpoint,
                        prompt_kind,
                        step + 1,
                        &exchange_id,
                        "llm_submit_ack",
                        ack.clone(),
                    ) {
                        eprintln!("[{role}] step={} action_log_error: {e}", step + 1);
                    }
                    append_orchestration_trace(
                        "llm_message_processed",
                        json!({
                            "role": role,
                            "prompt_kind": prompt_kind,
                            "step": step + 1,
                            "endpoint_id": endpoint.id,
                            "submit_ack": ack,
                        }),
                    );
                    return Ok(ack.to_string());
                }
            }
        }

        eprintln!("[{role}] step={} response_bytes={}", step + 1, raw.len());

        if is_reaction_only_response(&raw) {
            reaction_only_streak = reaction_only_streak.saturating_add(1);
            error_streak = error_streak.saturating_add(1);
            last_error = Some("reaction_only_response".to_string());
            if let Err(log_err) = append_message_log(
                role,
                endpoint,
                prompt_kind,
                step + 1,
                &exchange_id,
                "llm_reaction_only",
                json!({
                    "raw": truncate(&raw, MAX_SNIPPET),
                }),
            ) {
                eprintln!("[{role}] step={} action_log_error: {log_err}", step + 1);
            }
            if reaction_only_streak < 3 {
                eprintln!(
                    "[{role}] step={} reaction_only_response retry {}",
                    step + 1,
                    reaction_only_streak
                );
                continue;
            }
            reaction_only_streak = 0;
            last_result = Some(build_invalid_action_feedback(
                None,
                "reaction-only response",
            ));
            if error_streak >= 3 {
                last_result = Some(blocker_escalation_prompt(
                    role,
                    last_error.as_deref().unwrap_or("reaction_only"),
                    &task_context,
                ));
            }
            step += 1;
            continue;
        }

        let actions = match parse_actions(&raw) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[{role}] step={} parse_error: {e}", step + 1);
                error_streak = error_streak.saturating_add(1);
                last_error = Some(e.to_string());
                reaction_only_streak = 0;
                if let Err(log_err) = append_message_log(
                    role,
                    endpoint,
                    prompt_kind,
                    step + 1,
                    &exchange_id,
                    "llm_parse_error",
                    json!({
                        "error": e.to_string(),
                        "raw": truncate(&raw, MAX_SNIPPET),
                    }),
                ) {
                    eprintln!("[{role}] step={} action_log_error: {log_err}", step + 1);
                }
                last_result = Some(build_invalid_action_feedback(None, &e.to_string()));
                if error_streak >= 3 {
                    last_result = Some(blocker_escalation_prompt(
                        role,
                        last_error.as_deref().unwrap_or("parse_error"),
                        &task_context,
                    ));
                }
                step += 1;
                continue;
            }
        };

        if actions.len() != 1 {
            let msg = format!("Got {} actions — emit exactly one action per turn.", actions.len());
            eprintln!("[{role}] step={} {msg}", step + 1);
            error_streak = error_streak.saturating_add(1);
            last_error = Some(msg.clone());
            if let Err(log_err) = append_message_log(
                role,
                endpoint,
                prompt_kind,
                step + 1,
                &exchange_id,
                "llm_invalid_action_count",
                json!({
                    "action_count": actions.len(),
                    "raw": truncate(&raw, MAX_SNIPPET),
                }),
            ) {
                eprintln!("[{role}] step={} action_log_error: {log_err}", step + 1);
            }
            last_result = Some(build_invalid_action_feedback(None, &msg));
            if error_streak >= 3 {
                last_result = Some(blocker_escalation_prompt(
                    role,
                    last_error.as_deref().unwrap_or("invalid_action_count"),
                    &task_context,
                ));
            }
            step += 1;
            continue;
        }

        let mut action = actions[0].clone();
        let raw_action = action.clone();
        if let Err(e) = normalize_action(&mut action) {
            error_streak = error_streak.saturating_add(1);
            last_error = Some(e.to_string());
            if let Err(log_err) = append_message_log(
                role,
                endpoint,
                prompt_kind,
                step + 1,
                &exchange_id,
                "llm_invalid_action",
                json!({
                    "stage": "normalize_action",
                    "error": e.to_string(),
                    "raw": truncate(&raw, MAX_SNIPPET),
                }),
            ) {
                eprintln!("[{role}] step={} action_log_error: {log_err}", step + 1);
            }
            last_result = Some(build_invalid_action_feedback(Some(&raw_action), &e.to_string()));
            if error_streak >= 3 {
                last_result = Some(blocker_escalation_prompt(
                    role,
                    last_error.as_deref().unwrap_or("invalid_action"),
                    &task_context,
                ));
            }
            step += 1;
            continue;
        }
        if let Err(e) = validate_action(&action) {
            error_streak = error_streak.saturating_add(1);
            last_error = Some(e.to_string());
            if let Err(log_err) = append_message_log(
                role,
                endpoint,
                prompt_kind,
                step + 1,
                &exchange_id,
                "llm_invalid_action",
                json!({
                    "stage": "validate_action",
                    "error": e.to_string(),
                    "raw": truncate(&raw, MAX_SNIPPET),
                    "action": action.clone(),
                }),
            ) {
                eprintln!("[{role}] step={} action_log_error: {log_err}", step + 1);
            }
            let err_text = e.to_string();
            if let Some(prompt) = corrective_invalid_action_prompt(&action, &err_text, role) {
                last_result = Some(format!(
                    "{}\n\n{}",
                    build_invalid_action_feedback(Some(&action), &err_text),
                    prompt
                ));
                if error_streak >= 3 {
                    last_result = Some(blocker_escalation_prompt(
                        role,
                        last_error.as_deref().unwrap_or("invalid_action"),
                        &task_context,
                    ));
                }
                step += 1;
                continue;
            }
            if err_text.contains("cargo_test missing 'crate'") {
                last_result = Some(format!(
                    "Invalid action: {e}\nCorrective action required: `cargo_test` must include a `crate` field.\nUse this exact format and fill in the crate name:\n```json\n{{\n  \"action\": \"cargo_test\",\n  \"crate\": \"canon-runtime\",\n  \"observation\": \"Running canon-runtime test suite after latest changes.\",\n  \"rationale\": \"Validate that canon-runtime tests pass for the updated parser logic.\"\n}}\n```\nReturn exactly one action."
                ));
            } else {
                last_result = Some(build_invalid_action_feedback(Some(&action), &err_text));
            }
            if error_streak >= 3 {
                last_result = Some(blocker_escalation_prompt(
                    role,
                    last_error.as_deref().unwrap_or("invalid_action"),
                    &task_context,
                ));
            }
            step += 1;
            continue;
        }

        let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();

        if kind == "run_command" {
            if let Some(path) = require_tail_path.as_ref() {
                if let Some(cmd) = action.get("cmd").and_then(|v| v.as_str()) {
                    if cmd.contains(path) && cmd.contains("tail") {
                        require_tail_path = None;
                    }
                }
            }
        }
        if role == "executor" && total_steps >= EXECUTOR_STEP_LIMIT {
            if action.get("action").and_then(|v| v.as_str()) != Some("message") {
                error_streak = error_streak.saturating_add(1);
                last_result = Some(format!(
                    "Executor exceeded {EXECUTOR_STEP_LIMIT} actions without handoff. You must send a `message` action to planner (handoff or blocker) now."
                ));
                step += 1;
                continue;
            }
        }
        if kind == "message" {
            if let Some(path) = require_tail_path.as_ref() {
                let msg = format!(
                    "Detached cargo test output must be inspected before sending a message. Run:\n```json\n{{\n  \"action\": \"run_command\",\n  \"cmd\": \"tail -n 200 {}\",\n  \"cwd\": \"{}\",\n  \"observation\": \"Inspect live cargo test output.\",\n  \"rationale\": \"Detached cargo test output is in the log file; tail it for progress and failures.\"\n}}\n```\nReturn exactly one action.",
                    path,
                    WORKSPACE
                );
                error_streak = error_streak.saturating_add(1);
                last_result = Some(msg);
                step += 1;
                continue;
            }
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

        if role == "diagnostics" && !diagnostics_eventlog_python_done {
            if diagnostics_python_reads_event_logs(&action) {
                diagnostics_eventlog_python_done = true;
            } else if step == 0 {
                last_result = Some(
                    "Diagnostics must begin with a `python` action that analyzes /workspace/ai_sandbox/canon/state/event_log/event.tlog.d to diagnose problems, detect inconsistencies, and extract concrete failure signals."
                        .to_string(),
                );
                step += 1;
                continue;
            } else if matches!(kind.as_str(), "apply_patch" | "message") {
                last_result = Some(
                    "Before writing diagnostics or finishing, run a `python` action that analyzes /workspace/ai_sandbox/canon/state/event_log/event.tlog.d to find errors, inconsistencies, invariant violations, repeated failure patterns, and concrete repair targets. Diagnostics is for finding what is broken."
                        .to_string(),
                );
                step += 1;
                continue;
            }
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
                if kind == "cargo_test" && out.contains("note: cargo test detached") {
                    require_tail_path = extract_progress_path_from_result(&out);
                }
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
    lane_plan_file: String,
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
                "run_command" => action.get("cmd").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                "python" => "python".to_string(),
                "read_file" => {
                    let path = action.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let line = action.get("line").and_then(|v| v.as_u64());
                    match line {
                        Some(n) => format!("read_file {}:{}", path, n),
                        None => format!("read_file {}", path),
                    }
                }
                "list_dir" => format!("list_dir {}", action.get("path").and_then(|v| v.as_str()).unwrap_or("")),
                "apply_patch" => "apply_patch".to_string(),
                "message" => {
                    let status = action.get("status").and_then(|v| v.as_str()).unwrap_or("");
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

fn completed_turn_is_complete(text: &str) -> bool {
    parse_actions(text)
        .ok()
        .and_then(|actions| actions.into_iter().next())
        .and_then(|action| action.get("action").and_then(|v| v.as_str()).map(str::to_string))
        .as_deref()
        == Some("message")
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
    if *dispatch_state
        .lane_prompt_in_flight
        .get(&submitted.lane)
        .unwrap_or(&false)
    {
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
    if completed_turn_is_complete(&exec_result) {
        dispatch_state.lane_steps_used.insert(submitted.lane, 0);
        if let Ok(mut actions) = parse_actions(&exec_result) {
            if let Some(action) = actions.pop() {
                if let Err(e) = append_action_result_log(
                    &submitted.actor,
                    &lane_cfg.endpoint,
                    "executor",
                    1,
                    &submitted.command_id,
                    &action,
                    true,
                    &exec_result,
                ) {
                    eprintln!("[orchestrate] executor_message_result_log_error: {e}");
                }
            }
        }
    }
    if submitted.tab_id != tab_id {
        eprintln!(
            "[orchestrate] completed turn tab mismatch: turn_id={} expected_tab={} actual_tab={}",
            turn_id, submitted.tab_id, tab_id
        );
        let lane = dispatch_lane_mut(dispatch_state, submitted.lane);
        lane.in_progress_by = None;
        lane.pending = true;
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

fn verifier_confirmed(reason: &str) -> bool {
    if let Ok(v) = serde_json::from_str::<Value>(reason) {
        if let Some(verified) = v.get("verified").and_then(|x| x.as_bool()) {
            return verified;
        }
    }
    false
}

fn dispatch_lane_mut<'a>(state: &'a mut DispatchState, lane_id: usize) -> &'a mut DispatchLaneState {
    state
        .lanes
        .get_mut(&lane_id)
        .unwrap_or_else(|| panic!("missing lane state for {:?}", lane_id))
}

fn claim_next_lane(state: &mut DispatchState, lane: &LaneConfig) -> Option<(usize, String)> {
    let lane_id = lane.index;
    let lane_state = dispatch_lane_mut(state, lane_id);
    if lane_state.pending && lane_state.in_progress_by.is_none() && !lane_state.plan_text.trim().is_empty() {
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
        lane_plan_file: lane.plan_file.clone(),
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
    let lane_plan_text = std::fs::read_to_string(Path::new(WORKSPACE).join(&job.lane_plan_file)).unwrap_or_default();
    let mut exec_prompt = executor_cycle_prompt(
        job.executor_display.as_str(),
        job.label.as_str(),
        job.lane_plan_file.as_str(),
        lane_plan_text.as_str(),
        &job.latest_verify_result,
    );
    if let Some(inbound) = take_inbound_message("executor") {
        exec_prompt.push_str("\n\nInbound handoff message (raw JSON):\n");
        exec_prompt.push_str(&inbound);
        exec_prompt.push('\n');
    }
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
    if let Err(e) = append_message_log(
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
    ) {
        eprintln!("[{}] step=1 action_log_error: {e}", job.executor_role);
    }
    append_orchestration_trace(
        "llm_message_forwarded",
        json!({
            "role": job.executor_role,
            "prompt_kind": "executor",
            "step": 1,
            "endpoint_id": endpoint.id,
            "submit_only": true,
            "prompt_bytes": prompt.len(),
        }),
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
    append_orchestration_trace(
        "llm_message_received",
        json!({
            "role": job.executor_role,
            "prompt_kind": "executor",
            "step": 1,
            "endpoint_id": endpoint.id,
            "submit_only": true,
            "response_bytes": raw.len(),
        }),
    );
    if let Err(e) = append_message_log(
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
    ) {
        eprintln!("[{}] step=1 action_log_error: {e}", job.executor_role);
    }
    if let Ok(mut ack) = serde_json::from_str::<Value>(&raw) {
        if ack.get("submit_ack").and_then(|v| v.as_bool()) == Some(true) {
            ack["command_id"] = Value::String(command_id.to_string());
            eprintln!("[{}] step=1 submit_ack={}", job.executor_role, raw);
            if let Err(e) = append_message_log(
                &job.executor_role,
                endpoint,
                "executor",
                1,
                command_id,
                "llm_submit_ack",
                ack.clone(),
            ) {
                eprintln!("[{}] step=1 action_log_error: {e}", job.executor_role);
            }
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
    let orchestrate = args.iter().any(|a| a == "--orchestrate");
    let start_role = args.windows(2).find(|w| w[0] == "--start").map(|w| w[1].as_str()).unwrap_or("executor");
    if !matches!(start_role, "executor" | "verifier" | "planner" | "diagnostics") {
        bail!("invalid --start value: {start_role} (expected executor|verifier|planner|diagnostics)");
    }
    let is_verifier = !orchestrate && args.iter().any(|a| a == "--verifier");
    let is_planner = !orchestrate && args.iter().any(|a| a == "--planner");
    let is_diagnostics = !orchestrate && args.iter().any(|a| a == "--diagnostics");
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

    let workspace = PathBuf::from(WORKSPACE);
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
        if let Ok(contents) = std::fs::read_to_string(&legacy_json) {
            if let Some(parent) = plan_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&plan_path, contents);
        } else if let Ok(contents) = std::fs::read_to_string(&legacy_md) {
            if let Some(parent) = plan_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&plan_path, contents);
        } else {
            if let Some(parent) = plan_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&plan_path, "");
        }
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
        let verifier_ep = find_endpoint(&endpoints, "verifier")?.clone();

        let tabs_diagnostics = llm_worker_new_tabs();
        let tabs_planner = llm_worker_new_tabs();
        let tabs_verify = llm_worker_new_tabs();
        let mut verifier_summary: Vec<String> = vec!["(none yet)".to_string(); lanes.len()];
        let mut dispatch_state = new_dispatch_state(&lanes);
        dispatch_state.planner_pending = true;
        let mut current_phase = "bootstrap".to_string();
        let mut current_phase_lane: Option<usize> = None;
        let mut scheduled_phase: Option<String> = None;
        let mut resume_verifier_items: Vec<ResumeVerifierItem> = Vec::new();
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
            scheduled_phase = Some(current_phase.clone());
            if current_phase == "planner" {
                dispatch_state.planner_pending = true;
            }
            if current_phase == "diagnostics" {
                dispatch_state.diagnostics_pending = true;
            }
            if current_phase == "verifier" && resume_verifier_items.is_empty() {
                // No verifier work to resume; route through planner to avoid executor-only resume.
                dispatch_state.planner_pending = true;
                scheduled_phase = Some("planner".to_string());
            }
            // Drop in-flight submit state on resume: tabs/acks are stale when URLs rotate.
            dispatch_state.submitted_turns.clear();
            dispatch_state.executor_submit_inflight.clear();
            dispatch_state.tab_id_to_lane.clear();
            dispatch_state.lane_active_tab.clear();
            dispatch_state.deferred_completions.clear();
            for lane in &lanes {
                dispatch_state.lane_prompt_in_flight.insert(lane.index, false);
                dispatch_state.lane_submit_in_flight.insert(lane.index, false);
            }
            for lane in dispatch_state.lanes.values_mut() {
                if lane.in_progress_by.is_some() {
                    lane.in_progress_by = None;
                    lane.pending = true;
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
                std::path::Path::new("/workspace/ai_sandbox/canon-mini-agent/agent_state");
            apply_wake_flags(agent_state_dir, &mut dispatch_state, &mut scheduled_phase);

            if scheduled_phase.is_none() && current_phase == "bootstrap" {
                current_phase = start_role.to_string();
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
            }

            let agent_state_dir =
                std::path::Path::new("/workspace/ai_sandbox/canon-mini-agent/agent_state");
            let active_blocker_to_verifier = agent_state_dir.join("active_blocker_to_verifier.json");
            if active_blocker_to_verifier.exists()
                && (dispatch_state.planner_pending
                    || matches!(scheduled_phase.as_deref(), Some("planner")))
            {
                dispatch_state.planner_pending = false;
                if scheduled_phase.as_deref() == Some("planner") {
                    scheduled_phase = None;
                }
                eprintln!(
                    "[orchestrate] planner paused: active blocker to verifier"
                );
            }

            if dispatch_state.planner_pending
                && !matches!(scheduled_phase.as_deref(), Some(phase) if phase != "planner")
            {
                current_phase = "planner".to_string();
                current_phase_lane = None;
                let summary_text = lanes
                    .iter()
                    .map(|lane| format!("{}={}", lane.label, verifier_summary[lane.index]))
                    .collect::<Vec<_>>()
                    .join("\n");
                let lane_plan_list = lanes
                    .iter()
                    .map(|lane| lane.plan_file.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let objectives_text =
                    std::fs::read_to_string(workspace.join(OBJECTIVES_FILE)).unwrap_or_default();
                let invariants_text =
                    std::fs::read_to_string(workspace.join(INVARIANTS_FILE)).unwrap_or_default();
                let violations_text =
                    std::fs::read_to_string(&violations_path).unwrap_or_default();
                let diagnostics_text =
                    std::fs::read_to_string(&diagnostics_path).unwrap_or_default();
                let cargo_test_failures = summarize_cargo_test_failures(
                    &std::fs::read_to_string(workspace.join("cargo_test_failures.json"))
                        .unwrap_or_default(),
                );
                let plan_text =
                    std::fs::read_to_string(&master_plan_path).unwrap_or_default();
                let plan_diff_text = plan_diff(&dispatch_state.last_plan_text, &plan_text, 400);
                let current_executor_diff = executor_diff(&workspace, 400);
                let executor_diff_text =
                    diff_since_last_cycle(&current_executor_diff, &dispatch_state.last_executor_diff);
                dispatch_state.last_executor_diff = current_executor_diff;
                let mut planner_prompt = planner_cycle_prompt(
                    &summary_text,
                    &lane_plan_list,
                    &objectives_text,
                    &invariants_text,
                    &violations_text,
                    &diagnostics_text,
                    &plan_diff_text,
                    &executor_diff_text,
                    &cargo_test_failures,
                );
                if let Some(inbound) = take_inbound_message("planner") {
                    planner_prompt.push_str("\n\nInbound handoff message (raw JSON):\n");
                    planner_prompt.push_str(&inbound);
                    planner_prompt.push('\n');
                }
                append_orchestration_trace(
                    "llm_message_forwarded",
                    json!({
                        "from": "orchestrator",
                        "to": "planner",
                        "phase": "planner",
                    }),
                );
                let planner_system = system_instructions(AgentPromptKind::Planner);
                let result = run_agent(
                    "planner",
                    "planner",
                    &planner_system,
                    planner_prompt,
                    &planner_ep,
                    &bridge,
                    &workspace,
                    &tabs_planner,
                    false,
                    false,
                    !planner_bootstrapped,
                    0,
                )
                .await;
                match result {
                    Ok(result) => {
                        eprintln!("[orchestrate] planner ok bytes={}", result.len());
                        dispatch_state.last_plan_text =
                            std::fs::read_to_string(&master_plan_path).unwrap_or_default();
                        for lane in &lanes {
                            let mut plan_text = std::fs::read_to_string(workspace.join(&lane.plan_file)).unwrap_or_default();
                            if plan_text.trim().is_empty() {
                                let legacy_paths = match lane.index {
                                    0 => vec!["PLANS/executor-a.json", "PLANS/executor-a.md"],
                                    1 => vec!["PLANS/executor-b.json", "PLANS/executor-b.md"],
                                    _ => Vec::new(),
                                };
                                for legacy in legacy_paths {
                                    let legacy_text =
                                        std::fs::read_to_string(workspace.join(legacy)).unwrap_or_default();
                                    if !legacy_text.trim().is_empty() {
                                        eprintln!(
                                            "[orchestrate] legacy lane plan fallback: {} -> {}",
                                            legacy,
                                            lane.plan_file
                                        );
                                        plan_text = legacy_text;
                                        break;
                                    }
                                }
                            }
                            let lane_state = dispatch_lane_mut(&mut dispatch_state, lane.index);
                            let changed = lane_state.plan_text != plan_text;
                            lane_state.plan_text = plan_text;
                            if lane_state.in_progress_by.is_none()
                                && (changed || !verifier_confirmed(&lane_state.latest_verifier_result))
                            {
                                lane_state.pending = !lane_state.plan_text.trim().is_empty();
                            }
                        }

                        dispatch_state.planner_pending = false;
                        cycle_progress = true;
                    }
                    Err(err) => {
                        eprintln!("[orchestrate] planner error: {err:#}");
                    }
                }
                planner_bootstrapped = true;
            }

            if scheduled_phase.as_deref() == Some("planner") && !dispatch_state.planner_pending {
                scheduled_phase = None;
            }

            apply_wake_flags(agent_state_dir, &mut dispatch_state, &mut scheduled_phase);
            let now = now_ms();
            let resume_gate = scheduled_phase.as_deref();
            let block_executors = matches!(resume_gate, Some("planner") | Some("verifier") | Some("diagnostics"));
            if !dispatch_state.executor_submit_inflight.is_empty() {
                let mut timed_out = Vec::new();
                for (lane_id, pending) in dispatch_state.executor_submit_inflight.iter() {
                    if now.saturating_sub(pending.started_ms) >= PENDING_SUBMIT_TIMEOUT_MS {
                        timed_out.push(*lane_id);
                    }
                }
                for lane_id in timed_out {
                    if let Some(pending) = dispatch_state.executor_submit_inflight.remove(&lane_id) {
                        eprintln!(
                            "[orchestrate] pending submit timeout: lane={} command_id={}",
                            lanes[lane_id].label,
                            pending.command_id
                        );
                        append_orchestration_trace(
                            "executor_submit_timeout",
                            json!({
                                "lane_name": lanes[lane_id].label,
                                "command_id": pending.command_id,
                            }),
                        );
                    }
                   dispatch_state.lane_submit_in_flight.insert(lane_id, false);
                   let lane = dispatch_lane_mut(&mut dispatch_state, lane_id);
                    lane.in_progress_by = None;
                    lane.pending = true;
                }
            }
            if !block_executors {
                for lane in &lanes {
                    let in_flight = *dispatch_state
                        .lane_submit_in_flight
                        .get(&lane.index)
                        .unwrap_or(&false);
                    let next_at = *dispatch_state
                        .lane_next_submit_at_ms
                        .get(&lane.index)
                        .unwrap_or(&0);
                    if in_flight || next_at > now {
                        continue;
                    }
                    if let Some(job) = claim_executor_submit(&mut dispatch_state, lane) {
                        current_phase = "executor".to_string();
                        current_phase_lane = Some(lane.index);
                        let lane_index = lane.index;
                        let endpoint = lane.endpoint.clone();
                        let bridge = bridge.clone();
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
                                            lanes[lane_id].label,
                                            tab_id,
                                            turn_id
                                        );
                                        continue;
                                    };
                                    if now_ms().saturating_sub(pending.started_ms) >= PENDING_SUBMIT_TIMEOUT_MS {
                                        eprintln!(
                                            "[orchestrate] submit ack arrived after timeout: lane={} tab_id={} turn_id={}",
                                            lanes[lane_id].label,
                                            tab_id,
                                            turn_id
                                        );
                                        dispatch_state.lane_submit_in_flight.insert(lane_id, false);
                                        dispatch_state.lane_prompt_in_flight.insert(lane_id, false);
                                        continue;
                                    }
                                    if let Some(active_tab) = dispatch_state.lane_active_tab.get(&lane_id) {
                                        if *active_tab != tab_id {
                                            eprintln!(
                                                "[orchestrate] submit ack tab mismatch: lane={} active_tab={} ack_tab={} (overwriting active tab)",
                                                lanes[lane_id].label,
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
                                            steps_used: *dispatch_state
                                                .lane_steps_used
                                                .get(&job.lane_index)
                                                .unwrap_or(&0),
                                        },
                                    );
                                    dispatch_state.lane_next_submit_at_ms.insert(lane_id, now_ms());
                                    dispatch_state.lane_submit_in_flight.insert(lane_id, false);
                                    cycle_progress = true;
                                } else {
                                    eprintln!("[orchestrate] {} missing submit_ack: {exec_result}", job.executor_name);
                                    let lane = dispatch_lane_mut(&mut dispatch_state, job.lane_index);
                                    lane.in_progress_by = None;
                                    lane.pending = true;
                                    dispatch_state.executor_submit_inflight.remove(&job.lane_index);
                   dispatch_state.lane_submit_in_flight.insert(job.lane_index, false);
                               }
                           }
                           Err(err) => {
                               eprintln!("[orchestrate] {} submit error: {err:#}", job.executor_name);
                               let lane = dispatch_lane_mut(&mut dispatch_state, job.lane_index);
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

            let completed_turns = bridge.take_completed_turns().await;
            let mut verifier_changed = false;
            for item in completed_turns {
                append_orchestration_trace("llm_message_received", item.clone());
                let Some((tab_id, turn_id, exec_result, completed_endpoint_id)) = parse_completed_turn(&item) else {
                    continue;
                };
                let submitted = if let Some(submitted) =
                    dispatch_state.submitted_turns.remove(&(tab_id, turn_id))
                {
                    if let Some(endpoint_id) = completed_endpoint_id.as_deref() {
                        if endpoint_id != submitted.endpoint_id {
                            append_orchestration_trace(
                                "executor_completion_endpoint_mismatch",
                                json!({
                                    "tab_id": tab_id,
                                    "turn_id": turn_id,
                                    "expected_endpoint_id": submitted.endpoint_id,
                                    "completed_endpoint_id": endpoint_id,
                                }),
                            );
                            continue;
                        }
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
                    if let Some(endpoint_id) = completed_endpoint_id.as_deref() {
                        if endpoint_id != lanes[lane_id].endpoint.id {
                            append_orchestration_trace(
                                "executor_completion_endpoint_mismatch",
                                json!({
                                    "lane_name": lanes[lane_id].label,
                                    "tab_id": tab_id,
                                    "turn_id": turn_id,
                                    "expected_endpoint_id": lanes[lane_id].endpoint.id,
                                    "completed_endpoint_id": endpoint_id,
                                }),
                            );
                            continue;
                        }
                    }
                    if let Some(active_tab) = dispatch_state.lane_active_tab.get(&lane_id) {
                        if *active_tab != tab_id {
                            append_orchestration_trace(
                                "executor_completion_tab_mismatch",
                                json!({
                                    "lane_name": lanes[lane_id].label,
                                    "active_tab": active_tab,
                                    "tab_id": tab_id,
                                    "turn_id": turn_id,
                                }),
                            );
                            continue;
                        }
                    } else {
                        dispatch_state.lane_active_tab.insert(lane_id, tab_id);
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
                    lane_label: lanes[lane_id].label.clone(),
                    command_id: pending.command_id,
                    actor: pending.job.executor_role,
                    endpoint_id: pending.endpoint_id,
                    tabs: pending.tabs,
                    steps_used: *dispatch_state
                        .lane_steps_used
                        .get(&lane_id)
                        .unwrap_or(&0),
                }
            };
            dispatch_state.lane_prompt_in_flight.insert(submitted.lane, false);
            if handle_executor_completion(
                submitted,
                tab_id,
                turn_id,
                exec_result,
                &mut dispatch_state,
                &lanes,
                &bridge,
                &workspace,
                &mut continuation_joinset,
                &mut verifier_pending_results,
            ) {
                cycle_progress = true;
            }
        }

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
                            let lane = dispatch_lane_mut(&mut dispatch_state, submitted.lane);
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

            for lane_id in 0..lanes.len() {
                let in_flight = *dispatch_state
                    .lane_prompt_in_flight
                    .get(&lane_id)
                    .unwrap_or(&false);
                if in_flight {
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
                        &mut dispatch_state,
                        &lanes,
                        &bridge,
                        &workspace,
                        &mut continuation_joinset,
                        &mut verifier_pending_results,
                    ) {
                        cycle_progress = true;
                    }
                    let now_in_flight = *dispatch_state
                        .lane_prompt_in_flight
                        .get(&lane_id)
                        .unwrap_or(&false);
                    if now_in_flight {
                        break;
                    }
                }
            }

            while let Some((submitted, turn_id, final_exec_result)) = verifier_pending_results.pop_front() {
                if matches!(scheduled_phase.as_deref(), Some(phase) if phase != "verifier") {
                    verifier_pending_results.push_front((submitted, turn_id, final_exec_result));
                    break;
                }
                current_phase = "verifier".to_string();
                current_phase_lane = Some(submitted.lane);
                let lane_plan_file = lanes[submitted.lane].plan_file.clone();
                let current_executor_diff = executor_diff(&workspace, 400);
                let executor_diff_text =
                    diff_since_last_cycle(&current_executor_diff, &dispatch_state.last_executor_diff);
                dispatch_state.last_executor_diff = current_executor_diff;
                let mut verifier_prompt = verifier_cycle_prompt(
                    submitted.lane_label.as_str(),
                    lane_plan_file.as_str(),
                    &final_exec_result,
                    &executor_diff_text,
                    &summarize_cargo_test_failures(
                        &std::fs::read_to_string(workspace.join("cargo_test_failures.json"))
                            .unwrap_or_default(),
                    ),
                );
                if let Some(inbound) = take_inbound_message("verifier") {
                    if let Some((_, to, payload)) = try_parse_blocker(&inbound) {
                        if to.eq_ignore_ascii_case("verifier")
                            && !is_verifier_specific_blocker(&payload)
                        {
                            let ack = json!({
                                "action": "message",
                                "from": "verifier",
                                "to": "planner",
                                "type": "blocker",
                                "status": "blocked",
                                "observation": "Inbound blocker received; verifier yielding without further work until resolved.",
                                "rationale": "Blocker is not verifier-specific; pausing verification avoids unnecessary work.",
                                "payload": {
                                    "summary": "Verifier paused due to upstream blocker.",
                                    "blocker": payload.get("blocker").and_then(|v| v.as_str()).unwrap_or("upstream blocker"),
                                    "evidence": payload.get("evidence").and_then(|v| v.as_str()).unwrap_or(""),
                                    "required_action": payload.get("required_action").and_then(|v| v.as_str()).unwrap_or(""),
                                    "severity": payload.get("severity").and_then(|v| v.as_str()).unwrap_or("error")
                                }
                            });
                            persist_planner_message(&ack);
                            verifier_pending_results.push_front((submitted, turn_id, final_exec_result));
                            scheduled_phase = Some("planner".to_string());
                            continue;
                        }
                    }
                    verifier_prompt.push_str("\n\nInbound handoff message (raw JSON):\n");
                    verifier_prompt.push_str(&inbound);
                    verifier_prompt.push('\n');
                } else if let Some(inbound) = extract_message_action(&final_exec_result) {
                    verifier_prompt.push_str("\n\nInbound handoff message (raw JSON):\n");
                    verifier_prompt.push_str(&inbound);
                    verifier_prompt.push('\n');
                }
                append_orchestration_trace(
                    "llm_message_forwarded",
                    json!({
                        "from": format!("executor:{}", submitted.lane_label),
                        "to": "verifier",
                        "tab_id": submitted.tab_id,
                        "turn_id": turn_id,
                        "lane_name": submitted.lane_label.as_str(),
                        "lane_plan_file": lane_plan_file,
                    }),
                );
                let verifier_system = system_instructions(AgentPromptKind::Verifier);
                let verifier_ep = verifier_ep.clone();
                let bridge = bridge.clone();
                let workspace = workspace.clone();
                let send_system = !verifier_bootstrapped;
                verifier_bootstrapped = true;
                let tabs_verify = tabs_verify.clone();
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
                        let lane = dispatch_lane_mut(&mut dispatch_state, lane_id);
                        let changed = lane.latest_verifier_result != verify_result;
                        lane.latest_verifier_result = verify_result.clone();
                        lane.in_progress_by = None;
                        lane.pending = !verifier_confirmed(&verify_result);
                        verifier_changed |= changed;
                        verifier_summary[lane_id] = verify_result;
                        cycle_progress = true;
                    }
                    Err(err) => {
                        eprintln!("[orchestrate] verifier join error: {err:#}");
                    }
                }
            }

            if verifier_changed {
                dispatch_state.diagnostics_pending = true;
            }

            if scheduled_phase.as_deref() == Some("verifier") && verifier_pending_results.is_empty() {
                scheduled_phase = None;
            }

            if dispatch_state.diagnostics_pending
                && !matches!(scheduled_phase.as_deref(), Some(phase) if phase != "diagnostics")
            {
                current_phase = "diagnostics".to_string();
                current_phase_lane = None;
                let summary_text = lanes
                    .iter()
                    .map(|lane| format!("{}={}", lane.label, verifier_summary[lane.index]))
                    .collect::<Vec<_>>()
                    .join("\n");
                let cargo_test_failures = summarize_cargo_test_failures(
                    &std::fs::read_to_string(workspace.join("cargo_test_failures.json"))
                        .unwrap_or_default(),
                );
                let mut prompt = diagnostics_cycle_prompt(&summary_text, &cargo_test_failures);
                if let Some(inbound) = take_inbound_message("diagnostics") {
                    prompt.push_str("\n\nInbound handoff message (raw JSON):\n");
                    prompt.push_str(&inbound);
                    prompt.push('\n');
                }
                append_orchestration_trace(
                    "llm_message_forwarded",
                    json!({
                        "from": "verifier",
                        "to": "diagnostics",
                        "phase": "diagnostics",
                    }),
                );
                let diagnostics_system = system_instructions(AgentPromptKind::Diagnostics);
                match run_agent(
                    "diagnostics",
                    "diagnostics",
                    &diagnostics_system,
                    prompt,
                    &diagnostics_ep,
                    &bridge,
                    &workspace,
                    &tabs_diagnostics,
                    false,
                    false,
                    !diagnostics_bootstrapped,
                    0,
                )
                .await
                {
                    Ok(result) => {
                        eprintln!("[orchestrate] diagnostics ok bytes={}", result.len());
                        let new_diagnostics_text =
                            std::fs::read_to_string(&diagnostics_path).unwrap_or_default();
                        let diagnostics_changed = dispatch_state.diagnostics_text != new_diagnostics_text;
                        dispatch_state.diagnostics_text = new_diagnostics_text;
                        dispatch_state.diagnostics_pending = false;
                        dispatch_state.planner_pending = diagnostics_changed || verifier_changed;
                        cycle_progress = true;
                    }
                    Err(err) => {
                        eprintln!("[orchestrate] diagnostics error: {err:#}");
                    }
                }
                diagnostics_bootstrapped = true;
            }

            if scheduled_phase.as_deref() == Some("diagnostics") && !dispatch_state.diagnostics_pending {
                scheduled_phase = None;
            }

            if let Some(phase) = scheduled_phase.as_deref() {
                let resume_done = match phase {
                    "planner" => !dispatch_state.planner_pending,
                    "verifier" => verifier_pending_results.is_empty() && verifier_joinset.is_empty(),
                    "diagnostics" => !dispatch_state.diagnostics_pending,
                    "executor" => {
                        let lane_ok = current_phase_lane
                            .and_then(|lane_id| dispatch_state.lanes.get(&lane_id))
                            .map(|lane| !lane.pending && lane.in_progress_by.is_none())
                            .unwrap_or(true);
                        lane_ok
                    }
                    _ => true,
                };
                if resume_done {
                    scheduled_phase = None;
                }
            }

            if !cycle_progress {
                tokio::time::sleep(std::time::Duration::from_millis(SERVICE_POLL_MS)).await;
            }
        }
    } else {
        // Single-role mode
        let (role, prompt_kind) = if is_verifier {
            ("verifier", AgentPromptKind::Verifier)
        } else if is_diagnostics {
            ("diagnostics", AgentPromptKind::Diagnostics)
        } else if is_planner {
            ("mini_planner", AgentPromptKind::Planner)
        } else {
            ("executor", AgentPromptKind::Executor)
        };
        let instructions = system_instructions(prompt_kind);

        let primary_input_path = if is_verifier || is_planner {
            &spec_path
        } else {
            &workspace.join(&lanes[0].plan_file)
        };
        let primary_input_name = if is_verifier || is_planner {
            SPEC_FILE
        } else {
            lanes[0].plan_file.as_str()
        };
        let primary_input = std::fs::read_to_string(primary_input_path).with_context(|| format!("failed to read {primary_input_name}"))?;
        if primary_input.trim().is_empty() {
            bail!("input file is empty — write content into {primary_input_name} before running");
        }
        eprintln!("[canon-mini-agent] role={role} input loaded ({} bytes)", primary_input.len());

        let endpoint = find_endpoint(&endpoints, role)?.clone();
        eprintln!("[canon-mini-agent] endpoint id={} url={}", endpoint.id, endpoint.pick_url(0));

        let initial_prompt = if is_verifier {
            let invariants = std::fs::read_to_string(workspace.join(INVARIANTS_FILE)).unwrap_or_default();
            let objectives = std::fs::read_to_string(workspace.join(OBJECTIVES_FILE)).unwrap_or_default();
            let executor_diff_text = executor_diff(&workspace, 400);
            let cargo_test_failures = summarize_cargo_test_failures(
                &std::fs::read_to_string(workspace.join("cargo_test_failures.json"))
                    .unwrap_or_default(),
            );
            format!(
                "WORKSPACE: {WORKSPACE}\nAll relative paths resolve against WORKSPACE.\n\nCanonical spec (from {SPEC_FILE}):\n{primary_input}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}\n\nExecutor diff (workspace changes excluding plans/diagnostics/violations):\n{executor_diff_text}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nVerify that objectives in {OBJECTIVES_FILE} are completed properly.\nUpdate task status fields in {MASTER_PLAN_FILE} to reflect verified results.\nWrite violations to {VIOLATIONS_FILE} if any are found.\nWhen complete, report verified/unverified/false items in `message.payload`.\nEmit exactly one action to begin."
            )
        } else if is_diagnostics {
            let violations = std::fs::read_to_string(&violations_path).unwrap_or_default();
            let objectives = std::fs::read_to_string(workspace.join(OBJECTIVES_FILE)).unwrap_or_default();
            let cargo_test_failures = summarize_cargo_test_failures(
                &std::fs::read_to_string(workspace.join("cargo_test_failures.json"))
                    .unwrap_or_default(),
            );
            format!(
                "WORKSPACE: {WORKSPACE}\nAll relative paths resolve against WORKSPACE.\n\nAlways inspect state/event_log/event.tlog.d and the relevant canon system files.\nRead files and search the source code for the bugs (use read_file + run_command/ripgrep).\nRun 5+ python analysis actions over event logs and code evidence.\nInfer the root cause from the evidence and cite detailed sources of errors (file paths, functions, and log evidence).\nPrioritize canon-route, canon-loop, canon-runtime, canon-semantic-state, and canon-mini-agent when control flow or prompt contracts are implicated.\nLatest verifier summary:\n(none yet)\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nVerify whether objectives in {OBJECTIVES_FILE} are being met and note gaps.\nUse {SPEC_FILE}, {OBJECTIVES_FILE}, and {INVARIANTS_FILE} as the contract, not lane plans.\nInfer failures from code, logs, runtime state, and verifier findings.\nCanonical law:\n- SemanticStateSummary is the single source of truth for routing.\n- scheduler_len / planned_pending are not routing authority.\nFocus on route/control-flow correctness, event successor discharge, duplicate fanout, state-authority drift, queue-driven routing, synthetic dispatch bypasses, and prompt-shell mismatches.\n\nWrite a ranked diagnostics report to {diagnostics_rel}. Emit exactly one action to begin."
            )
        } else if is_planner {
            let violations = std::fs::read_to_string(&violations_path).unwrap_or_default();
            let diagnostics = std::fs::read_to_string(&diagnostics_path).unwrap_or_default();
            let objectives = std::fs::read_to_string(workspace.join(OBJECTIVES_FILE)).unwrap_or_default();
            let invariants = std::fs::read_to_string(workspace.join(INVARIANTS_FILE)).unwrap_or_default();
            let cargo_test_failures = summarize_cargo_test_failures(
                &std::fs::read_to_string(workspace.join("cargo_test_failures.json"))
                    .unwrap_or_default(),
            );
            let lane_plan_list = lanes
                .iter()
                .map(|lane| lane.plan_file.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "WORKSPACE: {WORKSPACE}\nAll relative paths resolve against WORKSPACE.\n\nCanonical spec (from {SPEC_FILE}):\n{primary_input}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nDiagnostics report (from {diagnostics_rel}):\n{diagnostics}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nCanonical law:\n- SemanticStateSummary is the single source of truth for routing.\n- scheduler_len / planned_pending are not routing authority.\n- Prioritize migration to state-authority before edge patches.\n\nUse {INVARIANTS_FILE} when deriving plan constraints.\nRead files and search the source code before issuing plan changes.\nWrite imperative, actionable instructions in {MASTER_PLAN_FILE} and derive lane plans: {lane_plan_list}.\nOnly use plan diffs when available; avoid re-reading the full plan unless necessary.\nEmit exactly one action to begin."
            )
        } else {
            let spec = std::fs::read_to_string(&spec_path).with_context(|| format!("failed to read {SPEC_FILE}"))?;
            let master_plan = std::fs::read_to_string(&master_plan_path).unwrap_or_default();
            let violations = std::fs::read_to_string(&violations_path).unwrap_or_default();
            let diagnostics = std::fs::read_to_string(&diagnostics_path).unwrap_or_default();
            let invariants = std::fs::read_to_string(workspace.join(INVARIANTS_FILE)).unwrap_or_default();
            format!(
                "WORKSPACE: {WORKSPACE}\nAll relative paths resolve against WORKSPACE.\n\nCanonical spec (from {SPEC_FILE}):\n{spec}\n\nMaster plan (from {MASTER_PLAN_FILE}):\n{master_plan}\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nDiagnostics (from {diagnostics_rel}):\n{diagnostics}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}\n\nAssigned lane plan (from {primary_input_name}):\n{primary_input}\n\nDo not modify spec, plan, lane plans, violations, or diagnostics. Use `message.payload` to report evidence for verifier review. Emit exactly one action to begin."
            )
        };

        let submit_only = role == "executor";
        let reason = run_agent(
            role,
            if is_verifier { "verifier" } else if is_diagnostics { "diagnostics" } else if is_planner { "planner" } else { "executor" },
            &instructions,
            initial_prompt,
            if role == "executor" { &lanes[0].endpoint } else { &endpoint },
            &bridge,
            &workspace,
            &tabs,
            submit_only,
            role == "executor",
            true,
            0,
        ).await?;
        println!("message: {reason}");
        Ok(())
    }
}

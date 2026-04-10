use anyhow::{anyhow, bail, Context, Result};
use canon_llm::config::LlmEndpoint;
use canon_tools_patch::apply_patch;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::constants::{
    diagnostics_file, is_self_modification_mode, ISSUES_FILE, MASTER_PLAN_FILE,
    MAX_FULL_READ_LINES, MAX_SNIPPET, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::issues::{is_closed, IssuesFile, Issue};
use crate::objectives::filter_incomplete_objectives_json;
use crate::logging::{
    append_orchestration_trace, log_action_event, log_action_result, log_error_event, now_ms,
};
use crate::prompts::truncate;
use crate::tool_schema::{
    plan_set_plan_status_action_example, plan_set_task_status_action_example,
};

/// Extract the first file path touched by the patch (*** Update File: / *** Add File:).
fn patch_first_file(patch: &str) -> Option<&str> {
    for line in patch.lines() {
        if let Some(rest) = line
            .strip_prefix("*** Update File:")
            .or_else(|| line.strip_prefix("*** Add File:"))
        {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
}

fn patch_targets<'a>(patch: &'a str) -> Vec<&'a str> {
    patch
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Update File:")
                .or_else(|| line.strip_prefix("*** Add File:"))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect()
}

fn is_lane_plan(path: &str) -> bool {
    if !path.starts_with("PLANS/") {
        return false;
    }
    let is_json = path.ends_with(".json");
    let is_md = path.ends_with(".md");
    if !is_json && !is_md {
        return false;
    }
    // Allow both legacy and instance-scoped lane plans:
    // - PLANS/executor-<id>.json
    // - PLANS/<instance>/executor-<id>.json
    // - legacy .md variants
    path.starts_with("PLANS/executor-") || path.contains("/executor-")
}

fn is_src_path(path: &str) -> bool {
    path == "src" || path.starts_with("src/")
}

fn is_tests_path(path: &str) -> bool {
    path == "tests" || path.starts_with("tests/")
}

fn default_graph_out_dir(workspace: &Path, crate_name: &str) -> PathBuf {
    workspace
        .join("state")
        .join("reports_out")
        .join("crates")
        .join(crate_name)
}

fn report_crate_dir(out_dir: &Path, crate_name: &str) -> PathBuf {
    let name_matches = out_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == crate_name)
        .unwrap_or(false);
    let parent_is_crates = out_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n == "crates")
        .unwrap_or(false);
    if name_matches && parent_is_crates {
        out_dir.to_path_buf()
    } else {
        out_dir.join("crates").join(crate_name)
    }
}

fn graph_artifact_edges(workspace: &Path, crate_name: &str) -> Option<(u64, u64)> {
    let path = workspace
        .join("state")
        .join("graph")
        .join("index")
        .join("by_crate")
        .join(format!("{crate_name}.json"));
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let call = value
        .get("call_edge_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cfg = value
        .get("cfg_edge_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some((call, cfg))
}

fn resolve_graph_crate_name(workspace: &Path, crate_name: &str) -> Option<String> {
    let primary = crate_name.to_string();
    let alt = crate_name.replace('-', "_");
    let candidates = if primary == alt {
        vec![primary]
    } else {
        vec![primary, alt]
    };
    for name in candidates {
        if let Some((call, cfg)) = graph_artifact_edges(workspace, &name) {
            if call > 0 || cfg > 0 {
                return Some(name);
            }
        }
    }
    None
}

fn ensure_graph_artifact(
    workspace: &Path,
    crate_name: &str,
    role: &str,
    step: usize,
) -> Result<String> {
    if let Some(name) = resolve_graph_crate_name(workspace, crate_name) {
        return Ok(name);
    }
    // Try rustc wrapper build to refresh artifacts.
    let build_cmd = format!("cargo build -p {crate_name}");
    eprintln!(
        "[{role}] step={} graph_artifact build cmd={build_cmd}",
        step
    );
    let (build_ok, build_out) =
        exec_run_command(workspace, &build_cmd, crate::constants::workspace())?;
    eprintln!(
        "[{role}] step={} graph_artifact build {} output_bytes={}",
        step,
        if build_ok { "ok" } else { "failed" },
        build_out.len()
    );
    if build_ok {
        if let Some(name) = resolve_graph_crate_name(workspace, crate_name) {
            return Ok(name);
        }
    }
    bail!(
        "graph artifact missing or lacks call/cfg edges; build the crate with rustc-wrapper enabled (e.g. `cargo build -p {crate_name}`) to generate a capture artifact"
    )
}

fn read_first_lines(path: &Path, max_lines: usize, max_bytes: usize) -> Result<String> {
    let content = ctx_read(path)?;
    let mut out = String::new();
    for (idx, line) in content.lines().enumerate() {
        if idx >= max_lines || out.len() >= max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn read_json_report(path: &Path, max_bytes: usize) -> Result<String> {
    let content = ctx_read(path)?;
    let trimmed = truncate(&content, max_bytes);
    Ok(trimmed.to_string())
}

fn normalize_objective_id_for_match(value: &str) -> String {
    value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
}

fn objective_id_matches(candidate: &str, requested: &str) -> bool {
    normalize_objective_id_for_match(candidate) == normalize_objective_id_for_match(requested)
}

fn objective_compared_ids(objectives: &[crate::objectives::Objective]) -> Vec<String> {
    objectives.iter().map(|obj| obj.id.clone()).collect()
}

fn objective_compared_normalized_ids(objectives: &[crate::objectives::Objective]) -> Vec<String> {
    objectives
        .iter()
        .map(|obj| normalize_objective_id_for_match(&obj.id))
        .collect()
}

fn objective_not_found_message(
    objectives: &[crate::objectives::Objective],
    requested: &str,
) -> String {
    let requested_id = normalize_objective_id_for_match(requested);
    let requested_raw = requested.to_string();
    let compared_ids = objective_compared_ids(objectives);
    let compared_normalized_ids = objective_compared_normalized_ids(objectives);
    format!(
        "objective not found: requested_raw={requested_raw:?}; requested_id={requested_id}; compared_ids={compared_ids:?}; compared_normalized_ids={compared_normalized_ids:?}"
    )
}

fn objective_already_exists_message(
    objectives: &[crate::objectives::Objective],
    requested: &str,
) -> String {
    let requested_id = normalize_objective_id_for_match(requested);
    let requested_raw = requested.to_string();
    let compared_ids = objective_compared_ids(objectives);
    let compared_normalized_ids = objective_compared_normalized_ids(objectives);
    format!(
        "objective id already exists: requested_raw={requested_raw:?}; requested_id={requested_id}; compared_ids={compared_ids:?}; compared_normalized_ids={compared_normalized_ids:?}"
    )
}

fn log_objective_operation_context(
    op: &str,
    outcome: &str,
    requested: Option<&str>,
    objectives: &[crate::objectives::Objective],
) {
    let requested_raw = requested.map(str::to_string);
    let requested_id = requested.map(normalize_objective_id_for_match);
    append_orchestration_trace(
        "objective_operation_context",
        json!({
            "operation": op,
            "outcome": outcome,
            "requested_raw": requested_raw,
            "requested_id": requested_id,
            "compared_ids": objective_compared_ids(objectives),
            "compared_normalized_ids": objective_compared_normalized_ids(objectives),
        }),
    );
}

fn handle_objectives_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let op_raw = action
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| action.get("operation").and_then(|v| v.as_str()))
        .unwrap_or("read");
    let include_done = action
        .get("include_done")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let path = workspace.join(OBJECTIVES_FILE);
    let raw = fs::read_to_string(&path).unwrap_or_default();
    if raw.trim().is_empty() && op_raw == "read" {
        return Ok((false, "(no objectives)".to_string()));
    }
    match op_raw {
        "read" => {
            if include_done {
                return Ok((false, raw));
            }
            let filtered = filter_incomplete_objectives_json(&raw).unwrap_or(raw);
            Ok((false, filtered))
        }
        "sorted_view" => {
            let mut file: crate::objectives::ObjectivesFile =
                serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("failed to parse OBJECTIVES.json: {e}"))?;
            file.objectives.sort_by(|a, b| {
                let rank = |status: &str| match status.trim().to_lowercase().as_str() {
                    "active" => 0,
                    "ready" => 1,
                    "in_progress" => 2,
                    "blocked" => 3,
                    "done" | "complete" | "completed" => 4,
                    _ => 5,
                };
                rank(&a.status)
                    .cmp(&rank(&b.status))
                    .then_with(|| a.id.cmp(&b.id))
            });
            if !include_done {
                file.objectives = file
                    .objectives
                    .into_iter()
                    .filter(|obj| !crate::objectives::is_completed(obj))
                    .collect();
            }
            Ok((false, serde_json::to_string_pretty(&file).unwrap_or(raw)))
        }
        "create_objective" => {
            let objective_val = action
                .get("objective")
                .ok_or_else(|| anyhow!("objectives create_objective missing objective"))?;
            let mut file: crate::objectives::ObjectivesFile =
                serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("failed to parse OBJECTIVES.json: {e}"))?;
            let objective: crate::objectives::Objective =
                serde_json::from_value(objective_val.clone())
                    .map_err(|e| anyhow!("invalid objective payload: {e}"))?;
            if objective.id.trim().is_empty() {
                bail!("objective.id must be non-empty");
            }
            log_objective_operation_context(
                "create_objective",
                "attempt",
                Some(&objective.id),
                &file.objectives,
            );
            if file
                .objectives
                .iter()
                .any(|o| objective_id_matches(&o.id, &objective.id))
            {
                log_objective_operation_context(
                    "create_objective",
                    "duplicate",
                    Some(&objective.id),
                    &file.objectives,
                );
                bail!("{}", objective_already_exists_message(&file.objectives, &objective.id));
            }
            file.objectives.push(objective);
            let created_id = file.objectives.last().map(|obj| obj.id.as_str());
            log_objective_operation_context(
                "create_objective",
                "success",
                created_id,
                &file.objectives,
            );
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "objectives create_objective ok".to_string()))
        }
        "update_objective" => {
            let objective_id = action
                .get("objective_id")
                .or_else(|| action.get("id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("objectives update_objective missing objective_id"))?;
            let updates = action
                .get("updates")
                .and_then(|v| v.as_object())
                .ok_or_else(|| anyhow!("objectives update_objective missing updates object"))?;
            let mut file: crate::objectives::ObjectivesFile =
                serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("failed to parse OBJECTIVES.json: {e}"))?;
            log_objective_operation_context(
                "update_objective",
                "attempt",
                Some(objective_id),
                &file.objectives,
            );
            let mut found = false;
            for obj in file.objectives.iter_mut() {
                if objective_id_matches(&obj.id, objective_id) {
                    let mut value = serde_json::to_value(obj.clone())?;
                    if let Some(map) = value.as_object_mut() {
                        for (k, v) in updates {
                            map.insert(k.clone(), v.clone());
                        }
                    }
                    *obj = serde_json::from_value(value)?;
                    found = true;
                    break;
                }
            }
            if !found {
                log_objective_operation_context(
                    "update_objective",
                    "not_found",
                    Some(objective_id),
                    &file.objectives,
                );
                bail!("{}", objective_not_found_message(&file.objectives, objective_id));
            }
            log_objective_operation_context(
                "update_objective",
                "success",
                Some(objective_id),
                &file.objectives,
            );
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "objectives update_objective ok".to_string()))
        }
        "delete_objective" => {
            let objective_id = action
                .get("objective_id")
                .or_else(|| action.get("id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("objectives delete_objective missing objective_id"))?;
            let mut file: crate::objectives::ObjectivesFile =
                serde_json::from_str(&raw).unwrap_or_default();
            log_objective_operation_context(
                "delete_objective",
                "attempt",
                Some(objective_id),
                &file.objectives,
            );
            let before = file.objectives.len();
            file.objectives
                .retain(|obj| !objective_id_matches(&obj.id, objective_id));
            if file.objectives.len() == before {
                log_objective_operation_context(
                    "delete_objective",
                    "not_found",
                    Some(objective_id),
                    &file.objectives,
                );
                bail!("{}", objective_not_found_message(&file.objectives, objective_id));
            }
            log_objective_operation_context(
                "delete_objective",
                "success",
                Some(objective_id),
                &file.objectives,
            );
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "objectives delete_objective ok".to_string()))
        }
        "set_status" => {
            let objective_id = action
                .get("objective_id")
                .or_else(|| action.get("id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("objectives set_status missing objective_id"))?;
            let status = action
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("objectives set_status missing status"))?;
            let mut file: crate::objectives::ObjectivesFile =
                serde_json::from_str(&raw).unwrap_or_default();
            log_objective_operation_context(
                "set_status",
                "attempt",
                Some(objective_id),
                &file.objectives,
            );
            let mut found = false;
            for obj in file.objectives.iter_mut() {
                if objective_id_matches(&obj.id, objective_id) {
                    obj.status = status.to_string();
                    found = true;
                    break;
                }
            }
            if !found {
                log_objective_operation_context(
                    "set_status",
                    "not_found",
                    Some(objective_id),
                    &file.objectives,
                );
                bail!("{}", objective_not_found_message(&file.objectives, objective_id));
            }
            log_objective_operation_context(
                "set_status",
                "success",
                Some(objective_id),
                &file.objectives,
            );
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "objectives set_status ok".to_string()))
        }
        "replace_objectives" => {
            let mut file: crate::objectives::ObjectivesFile =
                serde_json::from_str(&raw).unwrap_or_default();
            if let Some(obj_value) = action.get("objectives") {
                if obj_value.is_array() {
                    let objectives: Vec<crate::objectives::Objective> =
                        serde_json::from_value(obj_value.clone())
                            .map_err(|e| anyhow!("invalid objectives array: {e}"))?;
                    file.objectives = objectives;
                } else if obj_value.is_object() {
                    file = serde_json::from_value(obj_value.clone())
                        .map_err(|e| anyhow!("invalid objectives file payload: {e}"))?;
                } else {
                    bail!("objectives replace_objectives requires objectives array or object");
                }
            } else {
                bail!("objectives replace_objectives missing objectives");
            }
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "objectives replace_objectives ok".to_string()))
        }
        _ => bail!("unknown objectives op: {op_raw}"),
    }
}

fn handle_issue_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let op_raw = action
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("read");
    let path = workspace.join(ISSUES_FILE);
    let raw = fs::read_to_string(&path).unwrap_or_default();
    match op_raw {
        "read" => {
            if raw.trim().is_empty() {
                return Ok((false, "(no open issues)".to_string()));
            }
            let mut file: IssuesFile = serde_json::from_str(&raw).unwrap_or_default();
            file.issues.retain(|i| !is_closed(i));
            if file.issues.is_empty() {
                return Ok((false, "(no open issues)".to_string()));
            }
            Ok((false, serde_json::to_string_pretty(&file).unwrap_or(raw)))
        }
        "create" => {
            let issue_val = action
                .get("issue")
                .ok_or_else(|| anyhow!("issue create missing 'issue' field"))?;
            let mut file: IssuesFile = if raw.trim().is_empty() {
                IssuesFile::default()
            } else {
                serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("failed to parse ISSUES.json: {e}"))?
            };
            let issue: Issue = serde_json::from_value(issue_val.clone())
                .map_err(|e| anyhow!("invalid issue payload: {e}"))?;
            if issue.id.trim().is_empty() {
                bail!("issue.id must be non-empty");
            }
            if file.issues.iter().any(|i| i.id == issue.id) {
                bail!("issue id already exists: {}", issue.id);
            }
            file.issues.push(issue);
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "issue create ok".to_string()))
        }
        "update" => {
            let issue_id = action
                .get("issue_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("issue update missing 'issue_id'"))?;
            let updates = action
                .get("updates")
                .and_then(|v| v.as_object())
                .ok_or_else(|| anyhow!("issue update missing 'updates' object"))?;
            let mut file: IssuesFile = serde_json::from_str(&raw)
                .map_err(|e| anyhow!("failed to parse ISSUES.json: {e}"))?;
            let found = file.issues.iter_mut().find(|i| i.id == issue_id);
            let Some(issue) = found else {
                bail!("issue not found: {issue_id}");
            };
            let mut value = serde_json::to_value(issue.clone())?;
            if let Some(map) = value.as_object_mut() {
                for (k, v) in updates {
                    map.insert(k.clone(), v.clone());
                }
            }
            *issue = serde_json::from_value(value)?;
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "issue update ok".to_string()))
        }
        "delete" => {
            let issue_id = action
                .get("issue_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("issue delete missing 'issue_id'"))?;
            let mut file: IssuesFile = serde_json::from_str(&raw).unwrap_or_default();
            let before = file.issues.len();
            file.issues.retain(|i| i.id != issue_id);
            if file.issues.len() == before {
                bail!("issue not found: {issue_id}");
            }
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "issue delete ok".to_string()))
        }
        "set_status" => {
            let issue_id = action
                .get("issue_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("issue set_status missing 'issue_id'"))?;
            let status = action
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("issue set_status missing 'status'"))?;
            let mut file: IssuesFile = serde_json::from_str(&raw)
                .map_err(|e| anyhow!("failed to parse ISSUES.json: {e}"))?;
            let found = file.issues.iter_mut().find(|i| i.id == issue_id);
            let Some(issue) = found else {
                bail!("issue not found: {issue_id}");
            };
            issue.status = status.to_string();
            std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
            Ok((false, "issue set_status ok".to_string()))
        }
        _ => bail!("unknown issue op '{op_raw}' — use read | create | update | delete | set_status"),
    }
}

fn handle_plan_sorted_view_action(workspace: &Path) -> Result<(bool, String)> {
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let plan = load_or_init_plan(&plan_path)?;
    let obj = plan
        .as_object()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    let tasks = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let edges = obj
        .get("dag")
        .and_then(|v| v.as_object())
        .and_then(|d| d.get("edges"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    ensure_dag(tasks, edges)?;

    let mut task_map: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for task in tasks {
        if let Some(id) = task.get("id").and_then(|v| v.as_str()) {
            task_map.insert(id.to_string(), task.clone());
        }
    }

    let mut indegree: std::collections::HashMap<String, usize> =
        task_map.keys().map(|k| (k.clone(), 0)).collect();
    let mut adj: std::collections::HashMap<String, BTreeSet<String>> = std::collections::HashMap::new();
    for id in task_map.keys() {
        adj.insert(id.clone(), BTreeSet::new());
    }
    for edge in edges {
        let from = edge.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = edge.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() || to.is_empty() {
            bail!("plan edge missing from/to");
        }
        if let Some(nexts) = adj.get_mut(from) {
            nexts.insert(to.to_string());
        }
        if let Some(count) = indegree.get_mut(to) {
            *count += 1;
        }
    }

    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut order: Vec<String> = Vec::new();
    while let Some(id) = ready.iter().next().cloned() {
        ready.remove(&id);
        order.push(id.clone());
        if let Some(nexts) = adj.get(&id) {
            for next in nexts {
                if let Some(count) = indegree.get_mut(next) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        ready.insert(next.clone());
                    }
                }
            }
        }
    }

    let mut ordered_tasks: Vec<Value> = Vec::new();
    for id in &order {
        if let Some(task) = task_map.get(id) {
            ordered_tasks.push(task.clone());
        }
    }

    let mut output = serde_json::Map::new();
    if let Some(version) = obj.get("version") {
        output.insert("version".to_string(), version.clone());
    }
    if let Some(status) = obj.get("status") {
        output.insert("status".to_string(), status.clone());
    }
    output.insert(
        "order".to_string(),
        Value::Array(order.into_iter().map(Value::String).collect()),
    );
    output.insert("tasks".to_string(), Value::Array(ordered_tasks));
    output.insert("edges".to_string(), Value::Array(edges.clone()));
    let rendered = serde_json::to_string_pretty(&Value::Object(output))?;
    Ok((false, rendered))
}

fn extract_output_log_path(out: &str) -> Option<PathBuf> {
    let needle = "output_log=";
    let idx = out.find(needle)?;
    let rest = &out[idx + needle.len()..];
    let path = rest.split_whitespace().next()?;
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn parse_cargo_test_failures(out: &str) -> Value {
    let mut locations = BTreeSet::new();
    let mut failed_tests = BTreeSet::new();
    let mut stalled_tests = BTreeSet::new();
    let mut failure_block: Vec<String> = Vec::new();
    let mut rerun_hint: Option<String> = None;

    let log_path = extract_output_log_path(out);
    let mut scan = out.to_string();
    if let Some(path) = log_path.as_ref() {
        if let Ok(content) = fs::read_to_string(path) {
            scan = truncate(&content, MAX_SNIPPET * 4).to_string();
        }
    }

    for line in scan.lines() {
        let trimmed = line.trim();
        if let Some(idx) = trimmed.find(".rs:") {
            let path = &trimmed[..idx + 3];
            let rest = &trimmed[idx + 3..];
            let mut it = rest.splitn(3, ':');
            let line_no = it.next().unwrap_or("");
            let col_no = it.next().unwrap_or("");
            if !line_no.is_empty() && !col_no.is_empty() {
                locations.insert(format!("{}:{}:{}", path, line_no, col_no));
            }
        }
        if let Some(stripped) = trimmed.strip_prefix("test ") {
            if let Some(name) = stripped.strip_suffix(" ... FAILED") {
                failed_tests.insert(name.trim().to_string());
            }
            if let Some(name) = stripped.strip_suffix(" has been running for over 60 seconds") {
                stalled_tests.insert(name.trim().to_string());
            }
            if let Some(name) = stripped.strip_suffix(" has been running for over 30 seconds") {
                stalled_tests.insert(name.trim().to_string());
            }
            if let Some(name) = stripped.strip_suffix(" has been running for over 10 seconds") {
                stalled_tests.insert(name.trim().to_string());
            }
        }
        if rerun_hint.is_none() && trimmed.contains("To rerun") {
            rerun_hint = Some(trimmed.to_string());
        }
        if trimmed.contains("panicked at")
            || trimmed.contains("FAILED")
            || trimmed.contains("has been running for over")
        {
            failure_block.push(trimmed.to_string());
        }
    }

    let mut payload = serde_json::Map::new();
    payload.insert(
        "error_locations".to_string(),
        Value::Array(locations.into_iter().map(Value::String).collect()),
    );
    if let Some(path) = log_path {
        payload.insert(
            "output_log".to_string(),
            Value::String(path.display().to_string()),
        );
    }
    if !stalled_tests.is_empty() {
        payload.insert(
            "stalled_tests".to_string(),
            Value::Array(stalled_tests.iter().cloned().map(Value::String).collect()),
        );
        if failed_tests.is_empty() {
            failed_tests.extend(stalled_tests.iter().cloned());
        }
        if rerun_hint.is_none() {
            rerun_hint =
                Some("tests appear stalled; re-run with timeout or inspect output_log".to_string());
        }
    }
    if !failed_tests.is_empty() {
        payload.insert(
            "failed_tests".to_string(),
            Value::Array(failed_tests.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(hint) = rerun_hint {
        payload.insert("rerun_hint".to_string(), Value::String(hint));
    }
    if !failure_block.is_empty() {
        payload.insert(
            "failure_block".to_string(),
            Value::Array(failure_block.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(payload)
}

fn load_graph_symbols(
    graph_json: &Path,
) -> Result<std::collections::HashMap<u32, (String, String)>> {
    let content = ctx_read(graph_json)?;
    let value: Value = serde_json::from_str(&content)?;
    let mut out = std::collections::HashMap::new();
    let nodes = value
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for node in nodes {
        let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if id == 0 {
            continue;
        }
        let kind = node
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let symbol = node
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(id, (kind, symbol));
    }
    Ok(out)
}

fn load_nodes_symbols(
    nodes_csv: &Path,
) -> Result<std::collections::HashMap<u32, (String, String)>> {
    let content = ctx_read(nodes_csv)?;
    let mut out = std::collections::HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        if idx == 0 {
            continue;
        }
        let mut parts = line.splitn(4, ',');
        let id_raw = parts.next().unwrap_or("").trim();
        let kind = parts.next().unwrap_or("").trim().to_string();
        let symbol = parts.next().unwrap_or("").trim().to_string();
        if let Ok(id) = id_raw.parse::<u32>() {
            if id == 0 {
                continue;
            }
            out.insert(id, (kind, symbol));
        }
    }
    Ok(out)
}

fn symbol_label(map: &std::collections::HashMap<u32, (String, String)>, raw: &str) -> String {
    let id = raw.parse::<u32>().ok();
    if let Some(id) = id {
        if let Some((kind, symbol)) = map.get(&id) {
            if symbol.is_empty() {
                return format!("{id} {kind}").trim().to_string();
            }
            return format!("{id} {kind} {symbol}").trim().to_string();
        }
        return id.to_string();
    }
    raw.to_string()
}

pub(crate) fn patch_scope_error_with_mode(role: &str, patch: &str, self_mod: bool) -> Option<String> {
    let targets = patch_targets(patch);
    if targets.is_empty() {
        return None;
    }

    let diagnostics_file = diagnostics_file();
    let legacy_diagnostics_file = "DIAGNOSTICS.json";
    let touches_spec = targets.iter().any(|path| *path == SPEC_FILE);
    let touches_lane = targets.iter().any(|path| is_lane_plan(path));
    let touches_master_plan = targets.iter().any(|path| *path == MASTER_PLAN_FILE);
    let touches_violations = targets.iter().any(|path| *path == VIOLATIONS_FILE);
    let touches_objectives = targets.iter().any(|path| *path == OBJECTIVES_FILE);
    let touches_diagnostics = targets
        .iter()
        .any(|path| *path == diagnostics_file || *path == legacy_diagnostics_file);
    let touches_src = targets.iter().any(|path| is_src_path(path));
    let touches_tests = targets.iter().any(|path| is_tests_path(path));
    let touches_other = targets.iter().any(|path| {
        *path != SPEC_FILE
        && *path != MASTER_PLAN_FILE
            && !is_src_path(path)
            && !is_tests_path(path)
            && !is_lane_plan(path)
            && *path != VIOLATIONS_FILE
            && *path != OBJECTIVES_FILE
            && *path != diagnostics_file
            && *path != legacy_diagnostics_file
    });

    match role {
        "solo" => None,
        role if role.starts_with("executor") => {
            // In self-modification mode the executor is allowed to patch SPEC.md and src/ files.
            let spec_blocked = touches_spec && !self_mod;
            let src_blocked = (touches_src || touches_tests) && !self_mod;
            let other_blocked = touches_other;
            if spec_blocked
                || touches_master_plan
                || touches_lane
                || touches_violations
                || touches_diagnostics
                || src_blocked
                || other_blocked
            {
                Some(
                    "Executor may not patch plan files, violations, diagnostics, invariants, objectives, or normal-mode source files. Execute code/tests only and report evidence in `message.payload`."
                        .to_string(),
                )
            } else {
                None
            }
        }
        "verifier" | "verifier_a" | "verifier_b" => {
            if touches_spec || touches_lane || touches_diagnostics || touches_other {
                Some(
                    "Verifier may only patch `VIOLATIONS.json`. Use the `plan` action for `PLAN.json` updates. Do not modify `SPEC.md`, lane plans, diagnostics, or source files."
                        .to_string(),
                )
            } else if touches_violations {
                None
            } else {
                Some(
                    "Verifier may only patch `VIOLATIONS.json`. Use the `plan` action for `PLAN.json` updates; no other patches are allowed."
                        .to_string(),
                )
            }
        }
        "planner" | "mini_planner" => {
            if touches_spec
                || touches_violations
                || targets.iter().any(|path| is_src_path(path) || is_tests_path(path))
            {
                Some(
                    "Planner may patch lane plans under `PLANS/<instance>/executor-<id>.json` (or legacy `PLANS/executor-<id>.md`); planner may not patch `src/`, `tests/`, `SPEC.md`, or `VIOLATIONS.json`."
                        .to_string(),
                )
            } else if touches_lane || touches_objectives {
                // Allow planner to update lane plans and objectives only (SPEC §4.1 compliant)
                None
            } else {
                Some(
                    "Planner may patch lane plans or `PLANS/OBJECTIVES.json` only. Use the `plan` action for `PLAN.json` updates; no other patches are allowed."
                        .to_string(),
                )
            }
        }
        "diagnostics" => {
            if touches_spec
                || touches_master_plan
                || touches_lane
                || touches_violations
                || touches_src
                || touches_tests
                || touches_other
            {
                Some(
                    format!(
                        "Diagnostics may only patch {} or {} because diagnostics owns ranked failure reporting.",
                        diagnostics_file,
                        legacy_diagnostics_file
                    ),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn patch_scope_error(role: &str, patch: &str) -> Option<String> {
    patch_scope_error_with_mode(role, patch, is_self_modification_mode())
}

/// Walk up from `file_path` (workspace-relative) to find the nearest Cargo.toml.
/// Returns the package name from that manifest, or None if not found.
fn infer_crate_for_patch(workspace: &Path, file_path: &str) -> Option<String> {
    let mut dir = workspace.join(file_path);
    dir.pop(); // start from parent of the file
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let text = std::fs::read_to_string(&manifest).ok()?;
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("name") {
                    let name = rest.trim().trim_start_matches('=').trim().trim_matches('"');
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }
        if dir == workspace {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

// ── Patch-anchor auto-read (mirrors harness_repair logic) ─────────────────────

const AUTO_READ_CONTEXT_BEFORE: usize = 20;
const AUTO_READ_CONTEXT_AFTER: usize = 40;

/// Extract the file path from an apply_patch anchor-miss error.
/// Matches: "Failed to find expected lines in PATH:\n..."
fn extract_anchor_fail_path(err_msg: &str) -> Option<String> {
    let prefix = "Failed to find expected lines in ";
    for line in err_msg.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            let path = rest.trim_end_matches(':').trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Parse the indented anchor lines out of the patch error message.
fn extract_expected_anchor_lines(err_msg: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut capture = false;
    for line in err_msg.lines() {
        if line.starts_with("Failed to find expected lines in ") {
            capture = true;
            continue;
        }
        if !capture {
            continue;
        }
        if line.trim().is_empty() {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            lines.push(line.trim().to_string());
            continue;
        }
        if !lines.is_empty() {
            break;
        }
    }
    lines
}

fn patch_failure_guidance(path: Option<&str>, err_msg: &str) -> String {
    let mut hints = Vec::new();
    hints.push(
        "Patch anchor miss: deleted/context lines must match the current file EXACTLY.".to_string(),
    );
    hints.push("Do not abbreviate deleted lines like `-1. Centralize d`; copy exact text from read_file output.".to_string());
    hints.push("Next step: emit `read_file` for the target file, then build a new patch with at least 3 unchanged context lines.".to_string());

    if let Some(file) = path {
        let diagnostics_file = diagnostics_file();
        let legacy_diagnostics_file = "DIAGNOSTICS.json";
        if file == diagnostics_file || file == legacy_diagnostics_file || file.ends_with(".md") {
            hints.push("This is a prose/markdown file: prefer rewriting the whole section or the whole file instead of a tiny surgical hunk.".to_string());
            hints.push(format!(
                "For {}, one full-file rewrite is usually more reliable than repeated partial patches.",
                diagnostics_file
            ));
        }
    }

    let anchors = extract_expected_anchor_lines(err_msg);
    if !anchors.is_empty() {
        hints.push(format!("Failed anchor lines: {}", anchors.join(" | ")));
    }

    hints.join("\n")
}

/// Find the file region closest to the failed anchor and return a numbered excerpt.
fn extract_anchor_context_excerpt(full: &str, err_msg: &str) -> Option<(usize, usize, String)> {
    let anchor_lines = extract_expected_anchor_lines(err_msg);
    if anchor_lines.is_empty() {
        return None;
    }
    let file_lines: Vec<&str> = full.lines().collect();
    let mut best_idx: Option<usize> = None;
    for anchor in anchor_lines.iter().rev() {
        let needle = anchor.trim();
        if needle.len() < 8 {
            continue;
        }
        if let Some(idx) = file_lines.iter().position(|l| l.contains(needle)) {
            best_idx = Some(idx);
            break;
        }
    }
    let idx = best_idx?;
    let start_idx = idx.saturating_sub(AUTO_READ_CONTEXT_BEFORE);
    let end_idx = (idx + AUTO_READ_CONTEXT_AFTER + 1).min(file_lines.len());
    let start_line = start_idx + 1;
    let excerpt = file_lines[start_idx..end_idx]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", start_line + i, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some((start_line, end_idx, excerpt))
}

/// Auto-read the region near the failed anchor, falling back to the full file.
fn auto_read_for_patch_anchor(workspace: &Path, relative: &str, err_msg: &str) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let full = std::fs::read_to_string(&path)
        .with_context(|| format!("auto-read failed: {}", path.display()))?;
    if let Some((start, end, excerpt)) = extract_anchor_context_excerpt(&full, err_msg) {
        return Ok(format!("Current content near likely match of failed anchor in {relative} (lines {start}-{end}):\n{excerpt}"));
    }
    // Fallback: first MAX_FULL_READ_LINES lines of the file.
    let text = full
        .lines()
        .take(MAX_FULL_READ_LINES)
        .enumerate()
        .map(|(i, l)| format!("{}: {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("Current content of {relative}:\n{text}"))
}

// ── Action executors ───────────────────────────────────────────────────────────

fn exec_list_dir(workspace: &Path, relative: &str) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let mut entries = std::fs::read_dir(&path)
        .with_context(|| format!("list_dir: {}", path.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries.join("\n"))
}

fn exec_read_file(
    workspace: &Path,
    relative: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let path = safe_join(workspace, relative)?;
    let full =
        std::fs::read_to_string(&path).with_context(|| format!("read_file: {}", path.display()))?;
    let lines: Vec<&str> = full.lines().collect();
    let total = lines.len();
    let from = start_line.unwrap_or(1).saturating_sub(1).min(total);
    let max_lines = match (start_line, end_line) {
        (Some(start), Some(end)) if end >= start && end > 0 => {
            let end_idx = end.saturating_sub(1).min(total.saturating_sub(1));
            let start_idx = start.saturating_sub(1).min(total);
            end_idx.saturating_sub(start_idx).saturating_add(1)
        }
        (Some(_), Some(_)) => 0,
        (Some(_), None) => 500,
        (None, _) => MAX_FULL_READ_LINES,
    };
    let text = lines[from..]
        .iter()
        .take(max_lines)
        .enumerate()
        .map(|(i, l)| format!("{}: {}", from + i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    let shown = max_lines.min(total.saturating_sub(from));
    if total > from + shown {
        Ok(format!(
            "{text}\n(file has {total} lines total; use \"line\":{} or \"line_end\" to read more)",
            from + shown + 1
        ))
    } else {
        Ok(text)
    }
}

fn handle_message_action(role: &str, step: usize, action: &Value) -> Result<(bool, String)> {
    let status = action.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let payload = action
        .get("payload")
        .cloned()
        .unwrap_or_else(|| Value::Null);
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("message accepted");
    let full_message = serde_json::to_string_pretty(action).unwrap_or_else(|_| "{}".to_string());
    let msg_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let to_role = action.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);

    if role == "planner" && msg_type == "blocker" {
        let evidence = payload
            .get("evidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !evidence.is_empty() {
            let evidence_path = agent_state_dir.join("last_planner_blocker_evidence.txt");
            if let Ok(prev) = std::fs::read_to_string(&evidence_path) {
                if prev.trim() == evidence {
                    return Ok((
                        true,
                        "planner blocker suppressed: evidence unchanged".to_string(),
                    ));
                }
            }
            let _ = std::fs::write(evidence_path, evidence);
        }
    }

    if to_role.eq_ignore_ascii_case("verifier") {
        let active_path = agent_state_dir.join("active_blocker_to_verifier.json");
        if msg_type == "blocker" && status == "blocked" {
            let blocker_state = json!({
                "from": role,
                "summary": summary,
                "evidence": payload.get("evidence").and_then(|v| v.as_str()).unwrap_or(""),
                "required_action": payload.get("required_action").and_then(|v| v.as_str()).unwrap_or(""),
                "severity": payload.get("severity").and_then(|v| v.as_str()).unwrap_or(""),
            });
            let _ = std::fs::write(
                &active_path,
                serde_json::to_string_pretty(&blocker_state).unwrap_or_default(),
            );
        } else if active_path.exists() {
            let _ = std::fs::remove_file(active_path);
        }
    }
    persist_inbound_message(role, step, action, &full_message);
    Ok((
        true,
        format!("{summary}\n\nmessage_action:\n{full_message}"),
    ))
}

fn handle_list_dir_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let path = action
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("list_dir missing 'path'"))?;
    let out = exec_list_dir(workspace, path)?;
    Ok((false, format!("list_dir {path}:\n{out}")))
}

fn handle_read_file_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let path = action
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("read_file missing 'path'"))?;
    let line = action
        .get("line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let line_start = action
        .get("line_start")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let line_end = action
        .get("line_end")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let start = line_start.or(line);
    let out = exec_read_file(workspace, path, start, line_end)?;
    eprintln!(
        "[{role}] step={} read_file path={path} bytes={}",
        step,
        out.len()
    );
    Ok((false, format!("read_file {path}:\n{out}")))
}

/// Validate that a just-written JSON state file conforms to its canonical schema.
/// Returns `Some(rejection_message)` if the file violates the schema, `None` if valid
/// or if the file is not a schema-guarded type.
/// Uses `additionalProperties: false` so any extra field is caught and reported to the LLM.
fn validate_state_file_schema(file_path: &str, content: &str) -> Option<String> {
    use crate::reports::{DiagnosticsReport, ViolationsReport};
    use jsonschema::JSONSchema;
    use schemars::schema_for;
    use std::sync::OnceLock;

    let json_value = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(v) => v,
        Err(e) => {
            return Some(format!(
                "apply_patch rejected: file is not valid JSON after patch: {e}"
            ))
        }
    };

    let diag = diagnostics_file();
    let legacy_diag = "DIAGNOSTICS.json";

    if file_path == diag || file_path == legacy_diag {
        static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
        let compiled = SCHEMA.get_or_init(|| {
            let mut val =
                serde_json::to_value(schema_for!(DiagnosticsReport)).expect("diagnostics schema");
            // Enforce no additional properties beyond the canonical four fields.
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            JSONSchema::compile(&val).expect("compile diagnostics schema")
        });
        if let Err(errors) = compiled.validate(&json_value) {
            let msgs: Vec<String> = errors.take(5).map(|e| e.to_string()).collect();
            return Some(format!(
                "apply_patch rejected: DiagnosticsReport schema violation\n{}\n\
                 Canonical fields: status, inputs_scanned, ranked_failures, planner_handoff.\n\
                 No additional fields are permitted. Remove any extra fields and retry.",
                msgs.join("\n")
            ));
        }
    } else if file_path == VIOLATIONS_FILE {
        static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
        let compiled = SCHEMA.get_or_init(|| {
            let mut val =
                serde_json::to_value(schema_for!(ViolationsReport)).expect("violations schema");
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            JSONSchema::compile(&val).expect("compile violations schema")
        });
        if let Err(errors) = compiled.validate(&json_value) {
            let msgs: Vec<String> = errors.take(5).map(|e| e.to_string()).collect();
            return Some(format!(
                "apply_patch rejected: ViolationsReport schema violation\n{}\n\
                 Canonical fields: status, summary, violations (each with: id, title, severity, \
                 evidence, issue, impact, required_fix, files).\n\
                 No additional fields are permitted. Remove any extra fields and retry.",
                msgs.join("\n")
            ));
        }
    }

    None
}

fn handle_apply_patch_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let patch = action
        .get("patch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("apply_patch missing 'patch'"))?;
    let diagnostics_targeted = if role == "diagnostics" {
        let current_diagnostics_file = diagnostics_file();
        let legacy_diagnostics_file = "DIAGNOSTICS.json";
        patch_targets(patch)
            .into_iter()
            .any(|path| path == current_diagnostics_file || path == legacy_diagnostics_file)
    } else {
        false
    };
    let previous_diagnostics_text = if diagnostics_targeted {
        let current_diagnostics_file = diagnostics_file();
        Some(fs::read_to_string(workspace.join(current_diagnostics_file)).unwrap_or_default())
    } else {
        None
    };
    // Snapshot contents of all schema-guarded files before the patch so we can
    // revert them if schema validation fails after the write.
    let diag_file = diagnostics_file();
    let schema_snapshots: Vec<(String, Option<String>)> = {
        let legacy_diag = "DIAGNOSTICS.json";
        patch_targets(patch)
            .into_iter()
            .filter(|p| {
                *p == VIOLATIONS_FILE || *p == diag_file || *p == legacy_diag
            })
            .map(|p| {
                let content = fs::read_to_string(workspace.join(p)).ok();
                (p.to_string(), content)
            })
            .collect()
    };
    if let Some(msg) = patch_scope_error(role, &patch) {
        return Ok((false, msg));
    }
    match apply_patch(&patch, workspace) {
        Ok(_) => {
            if diagnostics_targeted {
                let current_diagnostics_file = diagnostics_file();
                let diagnostics_path = workspace.join(current_diagnostics_file);
                let new_diagnostics_text = fs::read_to_string(&diagnostics_path).unwrap_or_default();
                let raw_violations_text = fs::read_to_string(workspace.join(VIOLATIONS_FILE)).unwrap_or_default();
                let sanitized = crate::prompt_inputs::sanitize_diagnostics_for_planner(
                    &new_diagnostics_text,
                    &raw_violations_text,
                );
                let introduces_unvalidated_ranked_failures = new_diagnostics_text.contains("\"ranked_failures\"")
                    && sanitized.starts_with("(suppressed stale or unverified diagnostics:");
                if introduces_unvalidated_ranked_failures {
                    if let Some(previous) = previous_diagnostics_text {
                        std::fs::write(&diagnostics_path, previous)?;
                    }
                    // Build a specific error explaining which failure is missing
                    // validation and exactly what needs to be added.
                    let detail = serde_json::from_str::<Value>(&new_diagnostics_text)
                        .ok()
                        .and_then(|v| v.get("ranked_failures").and_then(Value::as_array).cloned())
                        .map(|failures| {
                            crate::prompt_inputs::describe_missing_source_validation(&failures)
                        })
                        .unwrap_or_else(|| "ranked_failures missing current-source validation".to_string());
                    let rejection_msg = format!(
                        "apply_patch rejected: ranked_failures require current-source validation before persistence\n\
                         Detail: {detail}\n\
                         Fix: add a read_file evidence entry that cites the specific file and line range \
                         you read, e.g. \"read_file src/app.rs:420-450 — confirmed X\"."
                    );
                    log_error_event(
                        role,
                        "apply_patch",
                        Some(step),
                        &rejection_msg,
                        Some(json!({
                            "stage": "diagnostics_emission_validation",
                            "path": current_diagnostics_file,
                        })),
                    );
                    return Ok((false, rejection_msg));
                }
            }
            // Schema validation: reject writes that introduce fields outside the canonical schema.
            for (target, prev_content) in &schema_snapshots {
                let new_content = fs::read_to_string(workspace.join(target)).unwrap_or_default();
                if let Some(err_msg) = validate_state_file_schema(target, &new_content) {
                    // Revert the file to its pre-patch content.
                    if let Some(prev) = prev_content {
                        let _ = fs::write(workspace.join(target), prev);
                    }
                    log_error_event(
                        role,
                        "apply_patch",
                        Some(step),
                        &err_msg,
                        Some(json!({"stage": "schema_validation", "path": target})),
                    );
                    return Ok((false, err_msg));
                }
            }
            eprintln!("[{role}] step={} apply_patch ok", step);
            let check_result = patch_first_file(&patch)
                .and_then(|f| infer_crate_for_patch(workspace, f))
                .map(|krate| {
                    eprintln!("[{role}] step={} cargo check -p {krate}", step);
                    exec_run_command(
                        workspace,
                        &format!("cargo check -p {krate}"),
                        crate::constants::workspace(),
                    )
                    .unwrap_or_else(|e| (false, e.to_string()))
                });
            match check_result {
                Some((ok, out)) => {
                    let label = if ok {
                        "cargo check ok"
                    } else {
                        "cargo check failed"
                    };
                    eprintln!("[{role}] step={} {label}", step);
                    Ok((
                        false,
                        format!(
                            "apply_patch ok\n\n{label}:\n{}",
                            truncate(&out, MAX_SNIPPET)
                        ),
                    ))
                }
                None => Ok((false, "apply_patch ok".to_string())),
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            eprintln!("[{role}] step={} apply_patch failed: {err_str}", step);
            log_error_event(
                role,
                "apply_patch",
                Some(step),
                &format!("apply_patch failed: {err_str}"),
                patch_first_file(&patch).map(|path| json!({
                    "stage": "apply_patch",
                    "path": path,
                })),
            );
            let read_path = extract_anchor_fail_path(&err_str)
                .or_else(|| patch_first_file(&patch).map(|s| s.to_string()));
            let guidance = patch_failure_guidance(read_path.as_deref(), &err_str);
            let mut msg = format!("apply_patch failed: {err_str}\n\n{guidance}");
            if let Some(fp) = read_path {
                if let Ok(content) = auto_read_for_patch_anchor(workspace, &fp, &err_str) {
                    eprintln!("[{role}] step={} auto_read path={fp}", step);
                    msg = format!("apply_patch failed: {err_str}\n\n{guidance}\n\n{content}");
                }
            }
            Ok((false, msg))
        }
    }
}

fn handle_run_command_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let cmd = action
        .get("cmd")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("run_command missing 'cmd'"))?;
    let cwd = action
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::constants::workspace());
    eprintln!("[{role}] step={} run_command cmd={cmd}", step);
    let (success, out) = exec_run_command(workspace, cmd, cwd)?;
    let label = if success {
        "run_command ok"
    } else {
        "run_command failed"
    };
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_error_event(
            role,
            "run_command",
            Some(step),
            &format!("run_command failed: {cmd}"),
            Some(json!({
                "stage": "run_command",
                "cmd": cmd,
                "cwd": cwd,
            })),
        );
    }
    Ok((false, format!("{label}:\n{}", truncate(&out, MAX_SNIPPET))))
}

fn handle_python_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let code = action
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("python missing 'code'"))?;
    let cwd = action
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::constants::workspace());
    eprintln!("[{role}] step={} python bytes={}", step, code.len());
    let (success, mut out) = exec_python(workspace, code, cwd)?;
    if !success {
        let lowered = out.to_lowercase();
        let mut context = String::new();
        if !PathBuf::from(cwd).starts_with(workspace) && !cwd.starts_with("/tmp") {
            context.push_str(&format!(
                "python cwd escapes workspace; set cwd to {} or /tmp.\n",
                crate::constants::workspace()
            ));
        }
        if !context.is_empty() {
            out.push('\n');
            out.push_str(context.trim_end());
        }
        if lowered.contains("permission denied") || lowered.contains("errno 13") {
            out.push_str(
                &format!("\npython write denied: verify the target path is under {ws} and set cwd={ws}; if still blocked, use apply_patch for `src/`, `PLAN.json`, or lane plan edits.", ws = crate::constants::workspace()),
            );
        }
    }
    let label = if success {
        "python ok"
    } else {
        "python failed"
    };
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_error_event(
            role,
            "python",
            Some(step),
            "python action failed",
            Some(json!({
                "stage": "python",
                "cwd": cwd,
            })),
        );
    }
    Ok((false, format!("{label}:\n{}", truncate(&out, MAX_SNIPPET))))
}

fn handle_rustc_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{action_kind} missing 'crate'"))?;
    let mode =
        action
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or(if action_kind == "rustc_hir" {
                "hir-tree"
            } else {
                "mir"
            });
    let extra = action.get("extra").and_then(|v| v.as_str()).unwrap_or("");
    let cmd = if extra.trim().is_empty() {
        format!("cargo rustc -p {crate_name} -- -Zunpretty={mode}")
    } else {
        format!("cargo rustc -p {crate_name} -- -Zunpretty={mode} {extra}")
    };
    eprintln!("[{role}] step={} {action_kind} cmd={cmd}", step);
    let (success, out) = exec_run_command(workspace, &cmd, crate::constants::workspace())?;
    let label = if success {
        format!("{action_kind} ok")
    } else {
        format!("{action_kind} failed")
    };
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_error_event(
            role,
            action_kind,
            Some(step),
            &format!("{action_kind} failed for crate {crate_name}"),
            Some(json!({
                "stage": action_kind,
                "crate": crate_name,
                "mode": mode,
                "extra": extra,
                "cmd": cmd,
            })),
        );
    }
    Ok((false, format!("{label}:\n{}", truncate(&out, MAX_SNIPPET))))
}

fn handle_graph_call_cfg_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{action_kind} missing 'crate'"))?;
    let out_dir = action
        .get("out_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_graph_out_dir(workspace, crate_name));
    let out_dir_str = out_dir.to_string_lossy();
    let artifact_crate = ensure_graph_artifact(workspace, crate_name, role, step)?;
    let bin_cmd =
        format!(
        "cargo run -p canon-tools-analysis --bin graph_bin -- --workspace {} --crate {} --out {}",
        crate::constants::workspace(), artifact_crate, out_dir_str
    );
    eprintln!("[{role}] step={} graph_bin cmd={bin_cmd}", step);
    let (bin_ok, bin_out) = exec_graph_command(workspace, &bin_cmd)?;
    let bin_label = if bin_ok {
        "graph_bin ok"
    } else {
        "graph_bin failed"
    };
    eprintln!(
        "[{role}] step={} {bin_label} output_bytes={}",
        step,
        bin_out.len()
    );
    if !bin_ok {
        log_error_event(
            role,
            action_kind,
            Some(step),
            &format!("graph_bin failed for crate {crate_name}"),
            Some(json!({
                "stage": action_kind,
                "crate": crate_name,
                "artifact_crate": artifact_crate,
                "cmd": bin_cmd,
                "out_dir": out_dir_str.to_string(),
            })),
        );
    }
    let label = if bin_ok {
        format!("{action_kind} ok")
    } else {
        format!("{action_kind} failed")
    };
    let target_path = if action_kind == "graph_call" {
        out_dir.join("graphs").join("callgraph.csv")
    } else {
        out_dir.join("graphs").join("cfg.csv")
    };
    let preview = if target_path.exists() {
        read_first_lines(&target_path, 50, MAX_SNIPPET)?
    } else {
        String::new()
    };
    let mut symbol_preview = String::new();
    let mut symbol_path = None;
    if target_path.exists() {
        let mut out_lines = Vec::new();
        let content = fs::read_to_string(&target_path)?;
        let mut lines = content.lines();
        let header = lines.next().unwrap_or("");
        let header_cols: Vec<&str> = header.split(',').collect();
        let has_symbol_cols = header_cols
            .iter()
            .any(|c| *c == "caller_symbol" || *c == "callee_symbol");
        let map = if !has_symbol_cols {
            let graph_json = out_dir.join("graph").join("graph.json");
            if graph_json.exists() {
                Some(load_graph_symbols(&graph_json)?)
            } else {
                let nodes_csv = out_dir.join("graph").join("nodes.csv");
                if nodes_csv.exists() {
                    Some(load_nodes_symbols(&nodes_csv)?)
                } else {
                    None
                }
            }
        } else {
            None
        };
        let mut count = 0usize;
        for line in lines {
            if count >= 200 {
                break;
            }
            let cols: Vec<&str> = line.split(',').collect();
            if has_symbol_cols {
                let caller_idx = header_cols.iter().position(|c| *c == "caller_symbol");
                let callee_idx = header_cols.iter().position(|c| *c == "callee_symbol");
                let caller = caller_idx
                    .and_then(|i| cols.get(i))
                    .map(|s| s.trim())
                    .unwrap_or("");
                let callee = callee_idx
                    .and_then(|i| cols.get(i))
                    .map(|s| s.trim())
                    .unwrap_or("");
                if !caller.is_empty() || !callee.is_empty() {
                    out_lines.push(format!("{caller} -> {callee}"));
                    count += 1;
                    continue;
                }
            }
            if cols.len() < 2 {
                continue;
            }
            let src = cols[0].trim();
            let dst = cols[1].trim();
            if let Some(map) = map.as_ref() {
                out_lines.push(format!(
                    "{} -> {}",
                    symbol_label(map, src),
                    symbol_label(map, dst)
                ));
            } else {
                out_lines.push(format!("{src} -> {dst}"));
            }
            count += 1;
        }
        if !out_lines.is_empty() {
            symbol_preview = out_lines.join("\n");
            let fname = if action_kind == "graph_call" {
                "callgraph.symbol.txt"
            } else {
                "cfg.symbol.txt"
            };
            let out_path = out_dir.join("graphs").join(fname);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out_path, format!("{}\n", symbol_preview))?;
            symbol_path = Some(out_path);
        }
    }
    let mut summary = format!(
        "{label}\noutput_dir: {}\n{}",
        out_dir_str,
        target_path.display()
    );
    if !preview.is_empty() {
        summary.push_str("\npreview:\n");
        summary.push_str(&preview);
    }
    if let Some(path) = symbol_path {
        summary.push_str(&format!("\nsymbol_edges: {}", path.display()));
        if !symbol_preview.is_empty() {
            summary.push_str("\nsymbol_preview:\n");
            summary.push_str(&symbol_preview);
        }
    }
    let mut full_out = String::new();
    full_out.push_str(&format!(
        "{bin_label}:\n{}\n",
        truncate(&bin_out, MAX_SNIPPET)
    ));
    Ok((false, format!("{summary}\n\nfull_output:\n{full_out}")))
}

fn handle_graph_reports_action(
    role: &str,
    step: usize,
    action_kind: &str,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{action_kind} missing 'crate'"))?;
    let tlog = action.get("tlog").and_then(|v| v.as_str());
    let out_dir = action
        .get("out_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_graph_out_dir(workspace, crate_name));
    let artifact_crate = ensure_graph_artifact(workspace, crate_name, role, step)?;
    let out_dir_str = out_dir.to_string_lossy();
    let crate_dir = report_crate_dir(&out_dir, crate_name);
    let mut cmd = format!(
        "cargo run -p canon-tools-analysis --bin graph_reports -- --workspace {} --crate {} --out {} --artifact",
        crate::constants::workspace(), artifact_crate, out_dir_str
    );
    if let Some(path) = tlog {
        cmd.push_str(&format!(" --tlog {path}"));
    }
    eprintln!("[{role}] step={} {action_kind} cmd={cmd}", step);
    let (success, out) = exec_graph_command(workspace, &cmd)?;
    let label = if success {
        format!("{action_kind} ok")
    } else {
        format!("{action_kind} failed")
    };
    if !success {
        log_error_event(
            role,
            action_kind,
            Some(step),
            &format!("{action_kind} failed for crate {crate_name}"),
            Some(json!({
                "stage": action_kind,
                "crate": crate_name,
                "artifact_crate": artifact_crate,
                "cmd": cmd,
                "out_dir": out_dir_str.to_string(),
                "tlog": tlog,
            })),
        );
    }
    let (report_path, report_label) = if action_kind == "graph_dataflow" {
        (
            crate_dir
                .join("metrics")
                .join("dataflow_fanout_report.json"),
            "dataflow_fanout_report.json",
        )
    } else {
        let runtime_path = crate_dir
            .join("analysis")
            .join("runtime_reachability_report.json");
        if runtime_path.exists() {
            (runtime_path, "runtime_reachability_report.json")
        } else {
            (
                crate_dir.join("metrics").join("reachability_report.json"),
                "reachability_report.json",
            )
        }
    };
    let report_preview = if report_path.exists() {
        read_json_report(&report_path, MAX_SNIPPET)?
    } else {
        String::new()
    };
    let mut summary = format!(
        "{label}\noutput_dir: {}\nreport: {}",
        out_dir_str,
        report_path.display()
    );
    if !report_preview.is_empty() {
        summary.push_str("\nreport_preview:\n");
        summary.push_str(&report_preview);
    } else {
        summary.push_str(&format!("\nreport_note: {} not found", report_label));
    }
    Ok((
        false,
        format!("{summary}\n\nfull_output:\n{}", truncate(&out, MAX_SNIPPET)),
    ))
}

fn handle_cargo_test_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let crate_name = action
        .get("crate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cargo_test missing 'crate'"))?;
    let test_name = action.get("test").and_then(|v| v.as_str());
    let cmd = if let Some(test_name) = test_name {
        format!(
            "cargo test -p {} {} -- --exact --nocapture",
            crate_name, test_name
        )
    } else {
        format!("cargo test -p {} -- --nocapture", crate_name)
    };
    eprintln!("[{role}] step={} cargo_test cmd={}", step, cmd);
    let (success, out) = exec_run_command(workspace, &cmd, crate::constants::workspace())?;
    let label = if success {
        "cargo_test ok"
    } else {
        "cargo_test failed"
    };
    eprintln!("[{role}] step={} {label} output_bytes={}", step, out.len());
    if !success {
        log_error_event(
            role,
            "cargo_test",
            Some(step),
            &format!("cargo_test failed for crate {crate_name}"),
            Some(json!({
                "stage": "cargo_test",
                "crate": crate_name,
                "test": test_name,
                "cmd": cmd,
            })),
        );
    }
    let failures_json = parse_cargo_test_failures(&out);
    let mut summary = format!("{label}");
    let mut progress_path: Option<String> = None;
    if let Some(line) = out.lines().find(|line| line.contains("output_log=")) {
        if let Some(idx) = line.find("output_log=") {
            let mut path = line[idx + "output_log=".len()..].trim();
            if let Some(end) = path.find(' ') {
                path = &path[..end];
            }
            if !path.is_empty() {
                progress_path = Some(path.to_string());
            }
        }
    }
    if out.contains("detached cargo test") {
        summary.push_str("\nnote: cargo test detached; see output_log for live results");
    }
    if let Some(arr) = failures_json.get("failed_tests").and_then(|v| v.as_array()) {
        if !arr.is_empty() {
            summary.push_str("\nfailed_tests:");
            for name in arr {
                if let Some(name) = name.as_str() {
                    summary.push_str(&format!("\n- {}", name));
                }
            }
        }
    }
    if let Some(arr) = failures_json
        .get("error_locations")
        .and_then(|v| v.as_array())
    {
        if !arr.is_empty() {
            summary.push_str("\nerror_locations:");
            for loc in arr {
                if let Some(loc) = loc.as_str() {
                    summary.push_str(&format!("\n- {}", loc));
                }
            }
        }
    }
    if let Some(hint) = failures_json.get("rerun_hint").and_then(|v| v.as_str()) {
        if !hint.is_empty() {
            summary.push_str(&format!("\nrerun_hint: {}", hint));
        }
    }
    if let Some(arr) = failures_json
        .get("failure_block")
        .and_then(|v| v.as_array())
    {
        if !arr.is_empty() {
            summary.push_str("\nfailure_block:");
            for line in arr {
                if let Some(line) = line.as_str() {
                    summary.push_str("\n");
                    summary.push_str(line);
                }
            }
        }
    }
    if let Some(path) = progress_path.as_ref() {
        summary.push_str(&format!("\noutput_log: {}", path));
        summary.push_str(&format!("\nprogress_path: {}", path));
        if out.contains("detached cargo test") {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Some(summary_line) = summarize_cargo_test_log(Path::new(path)) {
                summary.push_str(&format!("\nsummary: {summary_line}"));
            }
        }
    }
    Ok((
        false,
        format!("{summary}\n\nfull_output:\n{}", truncate(&out, MAX_SNIPPET)),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanOp {
    CreateTask,
    UpdateTask,
    DeleteTask,
    AddEdge,
    RemoveEdge,
    SetPlanStatus,
    SetTaskStatus,
    ReplacePlan,
}

impl PlanOp {
    const VALID_OPS: &'static str =
        "create_task, update_task, delete_task, add_edge, remove_edge, set_plan_status, set_task_status, replace_plan";

    fn parse(op: &str) -> Result<Self> {
        match op {
            "create_task" => Ok(Self::CreateTask),
            "update_task" => Ok(Self::UpdateTask),
            "delete_task" => Ok(Self::DeleteTask),
            "add_edge" => Ok(Self::AddEdge),
            "remove_edge" => Ok(Self::RemoveEdge),
            "set_plan_status" => Ok(Self::SetPlanStatus),
            "set_task_status" => Ok(Self::SetTaskStatus),
            "replace_plan" => Ok(Self::ReplacePlan),
            _ => bail!("unknown plan op: {op} (valid ops: {})", Self::VALID_OPS),
        }
    }
}

fn task_status_value(task: &serde_json::Map<String, Value>) -> Option<&str> {
    task.get("status").and_then(|v| v.as_str()).map(str::trim)
}

fn reopened_task_needs_regression_linkage(
    existing: &serde_json::Map<String, Value>,
    updated: &serde_json::Map<String, Value>,
) -> bool {
    let was_done = matches!(task_status_value(existing), Some("done"));
    let next_status = updated
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .or_else(|| task_status_value(existing));
    let is_reopened = was_done && !matches!(next_status, Some("done"));
    if !is_reopened {
        return false;
    }
    let steps = updated
        .get("steps")
        .or_else(|| existing.get("steps"))
        .and_then(|v| v.as_array());
    let has_regression_linkage = steps
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .any(|s| s.contains("regression"));
    !has_regression_linkage
}

fn ensure_reopened_task_has_regression_linkage(
    existing: &serde_json::Map<String, Value>,
    updated: &serde_json::Map<String, Value>,
    task_id: &str,
) -> Result<()> {
    if reopened_task_needs_regression_linkage(existing, updated) {
        bail!(
            "reopened task {task_id} must include regression-test linkage in steps when status changes from done to a non-done state"
        );
    }
    Ok(())
}

fn handle_plan_action(role: &str, workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let op_raw = action
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| action.get("operation").and_then(|v| v.as_str()))
        .unwrap_or("update");
    let op_raw = if op_raw == "set_status" {
        // Backward-compatible alias: set_status with task_id maps to task status;
        // otherwise it maps to plan status.
        if action.get("task_id").and_then(|v| v.as_str()).is_some() {
            "set_task_status"
        } else {
            "set_plan_status"
        }
    } else {
        op_raw
    };
    if op_raw != "sorted_view" && role.starts_with("executor") {
        bail!("plan action is not allowed for executor roles");
    }
    if op_raw != "sorted_view" {
        capture_plan_schema(action);
    }
    if matches!(role, "planner" | "mini_planner") {
        let rationale = action.get("rationale").and_then(|v| v.as_str()).unwrap_or("");
        let observation = action.get("observation").and_then(|v| v.as_str()).unwrap_or("");
        let combined = format!("{observation}\n{rationale}").to_ascii_lowercase();
        let references_diagnostics = combined.contains("diagnostic")
            || combined.contains("stale")
            || combined.contains("violation");
        let has_source_validation = combined.contains("read_file")
            || combined.contains("source")
            || combined.contains("verified")
            || combined.contains("current-cycle")
            || combined.contains("rg ")
            || combined.contains("run_command");
        if references_diagnostics && !has_source_validation {
            bail!("planner plan actions that rely on diagnostics must cite current source validation in observation/rationale (for example read_file, run_command, or verified source evidence)");
        }
    }
    if let Some(path) = action.get("path").and_then(|v| v.as_str()) {
        if path != MASTER_PLAN_FILE {
            bail!("plan path must be {MASTER_PLAN_FILE}, got {path}");
        }
    }
    if op_raw == "sorted_view" {
        return handle_plan_sorted_view_action(workspace);
    }
    if op_raw == "update" {
        if action.get("updates").is_none() && action.get("plan").is_some() {
            return handle_plan_replace_bundle(workspace, action);
        }
        return handle_plan_update_bundle(workspace, action);
    }
    let op = PlanOp::parse(op_raw)?;
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let mut plan = load_or_init_plan(&plan_path)?;
    let obj = plan
        .as_object_mut()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    match op {
        PlanOp::CreateTask => {
            let tasks = obj
                .get_mut("tasks")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            let task = action
                .get("task")
                .and_then(|v| v.as_object())
                .ok_or_else(|| anyhow!("plan create_task missing task object"))?;
            let id = task
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan task missing id"))?;
            if tasks.iter().any(|t| t.get("id").and_then(|v| v.as_str()) == Some(id)) {
                bail!("plan task already exists: {id}");
            }
            let mut new_task = serde_json::Map::new();
            new_task.insert("id".to_string(), Value::String(id.to_string()));
            if let Some(title) = task.get("title").and_then(|v| v.as_str()) {
                new_task.insert("title".to_string(), Value::String(title.to_string()));
            }
            let status = task
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("todo");
            new_task.insert("status".to_string(), Value::String(status.to_string()));
            if let Some(priority) = task.get("priority") {
                new_task.insert("priority".to_string(), priority.clone());
            }
            if let Some(steps) = task.get("steps") {
                new_task.insert("steps".to_string(), steps.clone());
            }
            tasks.push(Value::Object(new_task));
        }
        PlanOp::UpdateTask => {
            let tasks = obj
                .get_mut("tasks")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            let task = action
                .get("task")
                .and_then(|v| v.as_object())
                .ok_or_else(|| anyhow!("plan update_task missing task object"))?;
            let id = task
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan task missing id"))?;
            let Some(existing) = tasks
                .iter_mut()
                .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
                .and_then(|t| t.as_object_mut())
            else {
                bail!("plan task not found: {id}");
            };
            ensure_reopened_task_has_regression_linkage(existing, task, id)?;
            for (key, value) in task {
                if key != "id" {
                    existing.insert(key.to_string(), value.clone());
                }
            }
        }
        PlanOp::DeleteTask => {
            let tasks = obj
                .get_mut("tasks")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            let task_id = action
                .get("task_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan delete_task missing task_id"))?;
            tasks.retain(|t| t.get("id").and_then(|v| v.as_str()) != Some(task_id));
            let dag = obj
                .get_mut("dag")
                .and_then(|v| v.as_object_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
            let edges = dag
                .get_mut("edges")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
            edges.retain(|e| {
                let from = e.get("from").and_then(|v| v.as_str());
                let to = e.get("to").and_then(|v| v.as_str());
                from != Some(task_id) && to != Some(task_id)
            });
        }
        PlanOp::AddEdge => {
            let ids = {
                let tasks = obj
                    .get("tasks")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
                collect_task_ids(tasks)
            };
            let from = action
                .get("from")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan add_edge missing from"))?;
            let to = action
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan add_edge missing to"))?;
            if !ids.contains(from) || !ids.contains(to) {
                bail!("plan edge refers to unknown task id");
            }
            let dag = obj
                .get_mut("dag")
                .and_then(|v| v.as_object_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
            let edges = dag
                .get_mut("edges")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
            if edges.iter().any(|e| e.get("from").and_then(|v| v.as_str()) == Some(from)
                && e.get("to").and_then(|v| v.as_str()) == Some(to))
            {
                return Ok((false, "plan edge already exists".to_string()));
            }
            let mut edge = serde_json::Map::new();
            edge.insert("from".to_string(), Value::String(from.to_string()));
            edge.insert("to".to_string(), Value::String(to.to_string()));
            edges.push(Value::Object(edge));
            let edges_snapshot = edges.clone();
            let _ = edges;
            let tasks = obj
                .get("tasks")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            ensure_dag(tasks, &edges_snapshot)?;
        }
        PlanOp::RemoveEdge => {
            let dag = obj
                .get_mut("dag")
                .and_then(|v| v.as_object_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
            let edges = dag
                .get_mut("edges")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
            let from = action
                .get("from")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan remove_edge missing from"))?;
            let to = action
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan remove_edge missing to"))?;
            edges.retain(|e| {
                let e_from = e.get("from").and_then(|v| v.as_str());
                let e_to = e.get("to").and_then(|v| v.as_str());
                !(e_from == Some(from) && e_to == Some(to))
            });
        }
        PlanOp::SetPlanStatus => {
            let status = action
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan set_plan_status missing status"))?;
            // Invariant: cannot mark plan done if any task is not done
            if status == "done" {
                let tasks = obj
                    .get("tasks")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
                let any_incomplete = tasks.iter().any(|t| {
                    t.get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim() != "done")
                        .unwrap_or(true)
                });
                if any_incomplete {
                    bail!("plan status cannot be set to done while tasks remain incomplete");
                }
            }
            obj.insert("status".to_string(), Value::String(status.to_string()));
        }
        PlanOp::SetTaskStatus => {
            let task_id = action
                .get("task_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan set_task_status missing task_id"))?;
            let status = action
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan set_task_status missing status"))?;
            let tasks = obj
                .get_mut("tasks")
                .and_then(|v| v.as_array_mut())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            let Some(existing) = tasks
                .iter_mut()
                .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(task_id))
                .and_then(|t| t.as_object_mut())
            else {
                bail!("plan task not found: {task_id}");
            };
            let mut updated = serde_json::Map::new();
            updated.insert("status".to_string(), Value::String(status.to_string()));
            ensure_reopened_task_has_regression_linkage(existing, &updated, task_id)?;
            existing.insert("status".to_string(), Value::String(status.to_string()));
        }
        PlanOp::ReplacePlan => {
            let mut next_plan = action
                .get("plan")
                .cloned()
                .ok_or_else(|| anyhow!("plan replace_plan missing plan object"))?;
            normalize_plan_object(&mut next_plan)?;
            let tasks = next_plan
                .get("tasks")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            let edges = next_plan
                .get("dag")
                .and_then(|v| v.get("edges"))
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
            ensure_dag(tasks, edges)?;
            plan = next_plan;
        }
    }

    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?)?;
    // Emit control-plane log for plan mutation
    if let Ok(paths) = crate::logging::append_action_log_record(&crate::logging::compact_log_record(
        "control",
        "plan_update",
        Some(role),
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        Some("PLAN.json updated via plan action".to_string()),
        action.get("rationale").and_then(|v| v.as_str()).map(|s| s.to_string()),
        None,
        Some(json!({"op": op_raw, "path": MASTER_PLAN_FILE}))
    )) {
        let _ = paths;
    }
    Ok((
        false,
        format!("plan ok\nplan_path: {}", plan_path.display()),
    ))
}

fn capture_plan_schema(action: &Value) {
    let path = std::path::Path::new(crate::constants::agent_state_dir())
        .join("plan_action_schemas.jsonl");
    let record = json!({
        "ts_ms": now_ms(),
        "action": action,
    });
    if let Ok(line) = serde_json::to_string(&record) {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{}", line));
    }
}

fn handle_plan_update_bundle(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let updates = action
        .get("updates")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("plan update missing updates object"))?;
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let mut plan = load_or_init_plan(&plan_path)?;
    let obj = plan
        .as_object_mut()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;

    if let Some(status) = updates.get("status").and_then(|v| v.as_str()) {
        obj.insert("status".to_string(), Value::String(status.to_string()));
    }

    if let Some(tasks) = updates.get("tasks").and_then(|v| v.as_array()) {
        let tasks_obj = obj
            .get_mut("tasks")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
        for task in tasks {
            let task_obj = task
                .as_object()
                .ok_or_else(|| anyhow!("plan update tasks must be objects"))?;
            let id = task_obj
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan update task missing id"))?;
            let Some(existing) = tasks_obj
                .iter_mut()
                .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
                .and_then(|t| t.as_object_mut())
            else {
                bail!("plan task not found: {id}");
            };
            ensure_reopened_task_has_regression_linkage(existing, task_obj, id)?;
            for (key, value) in task_obj {
                if key != "id" {
                    existing.insert(key.to_string(), value.clone());
                }
            }
        }
    }

    if let Some(edges) = updates.get("remove_edges").and_then(|v| v.as_array()) {
        let dag = obj
            .get_mut("dag")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
        let edges_obj = dag
            .get_mut("edges")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
        for edge in edges {
            let from = edge.get("from").and_then(|v| v.as_str());
            let to = edge.get("to").and_then(|v| v.as_str());
            if let (Some(from), Some(to)) = (from, to) {
                edges_obj.retain(|e| {
                    let e_from = e.get("from").and_then(|v| v.as_str());
                    let e_to = e.get("to").and_then(|v| v.as_str());
                    !(e_from == Some(from) && e_to == Some(to))
                });
            }
        }
    }

    if let Some(edges) = updates.get("add_edges").and_then(|v| v.as_array()) {
        let ids = {
            let tasks = obj
                .get("tasks")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
            collect_task_ids(tasks)
        };
        let dag = obj
            .get_mut("dag")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| anyhow!("PLAN.json missing dag object"))?;
        let edges_obj = dag
            .get_mut("edges")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
        for edge in edges {
            let from = edge
                .get("from")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan add_edge missing from"))?;
            let to = edge
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan add_edge missing to"))?;
            if !ids.contains(from) || !ids.contains(to) {
                bail!("plan edge refers to unknown task id");
            }
            if edges_obj.iter().any(|e| e.get("from").and_then(|v| v.as_str()) == Some(from)
                && e.get("to").and_then(|v| v.as_str()) == Some(to))
            {
                continue;
            }
            let mut edge_obj = serde_json::Map::new();
            edge_obj.insert("from".to_string(), Value::String(from.to_string()));
            edge_obj.insert("to".to_string(), Value::String(to.to_string()));
            edges_obj.push(Value::Object(edge_obj));
        }
        let edges_snapshot = edges_obj.clone();
        let tasks = obj
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
        ensure_dag(tasks, &edges_snapshot)?;
    }

    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?)?;
    // Emit control-plane log for plan mutation (update bundle)
    let _ = crate::logging::append_action_log_record(&crate::logging::compact_log_record(
        "control",
        "plan_update",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        Some("PLAN.json updated via plan update bundle".to_string()),
        action.get("rationale").and_then(|v| v.as_str()).map(|s| s.to_string()),
        None,
        Some(json!({"op": "update_bundle", "path": MASTER_PLAN_FILE}))
    ));
    Ok((
        false,
        format!("plan ok\nplan_path: {}", plan_path.display()),
    ))
}

fn handle_plan_replace_bundle(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let plan_path = workspace.join(MASTER_PLAN_FILE);
    let mut next_plan = action
        .get("plan")
        .cloned()
        .ok_or_else(|| anyhow!("plan replace_plan missing plan object"))?;
    normalize_plan_object(&mut next_plan)?;
    let tasks = next_plan
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing tasks array"))?;
    let edges = next_plan
        .get("dag")
        .and_then(|v| v.get("edges"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("PLAN.json missing dag.edges array"))?;
    ensure_dag(tasks, edges)?;
    std::fs::write(&plan_path, serde_json::to_string_pretty(&next_plan)?)?;
    // Emit control-plane log for plan mutation (replace bundle)
    let _ = crate::logging::append_action_log_record(&crate::logging::compact_log_record(
        "control",
        "plan_update",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(true),
        Some("PLAN.json replaced via plan action".to_string()),
        action.get("rationale").and_then(|v| v.as_str()).map(|s| s.to_string()),
        None,
        Some(json!({"op": "replace_bundle", "path": MASTER_PLAN_FILE}))
    ));
    Ok((
        false,
        format!("plan ok\nplan_path: {}", plan_path.display()),
    ))
}

fn load_or_init_plan(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut plan = if raw.trim().is_empty() {
        json!({
            "version": 2,
            "status": "in_progress",
            "tasks": [],
            "dag": { "edges": [] }
        })
    } else {
        serde_json::from_str(&raw)?
    };
    normalize_plan_object(&mut plan)?;
    Ok(plan)
}

fn normalize_plan_object(plan: &mut Value) -> Result<()> {
    let obj = plan
        .as_object_mut()
        .ok_or_else(|| anyhow!("PLAN.json must be a JSON object"))?;
    let version_val = obj.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    if version_val < 2 {
        obj.insert("version".to_string(), Value::Number(2.into()));
    }
    if obj.get("status").and_then(|v| v.as_str()).is_none() {
        obj.insert("status".to_string(), Value::String("in_progress".to_string()));
    }
    if obj.get("tasks").and_then(|v| v.as_array()).is_none() {
        obj.insert("tasks".to_string(), Value::Array(Vec::new()));
    }
    let dag = obj.entry("dag".to_string()).or_insert_with(|| json!({}));
    if dag.get("edges").and_then(|v| v.as_array()).is_none() {
        dag.as_object_mut()
            .ok_or_else(|| anyhow!("PLAN.json dag must be object"))?
            .insert("edges".to_string(), Value::Array(Vec::new()));
    }
    Ok(())
}

fn collect_task_ids(tasks: &[Value]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for task in tasks {
        if let Some(id) = task.get("id").and_then(|v| v.as_str()) {
            ids.insert(id.to_string());
        }
    }
    ids
}

fn ensure_dag(tasks: &[Value], edges: &[Value]) -> Result<()> {
    let ids = collect_task_ids(tasks);
    let mut adj: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for id in &ids {
        adj.insert(id.clone(), Vec::new());
    }
    for edge in edges {
        let from = edge.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = edge.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() || to.is_empty() {
            bail!("plan edge missing from/to");
        }
        if !ids.contains(from) || !ids.contains(to) {
            bail!("plan edge refers to unknown task id");
        }
        adj.entry(from.to_string())
            .or_default()
            .push(to.to_string());
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();

    fn dfs(
        node: &str,
        adj: &std::collections::HashMap<String, Vec<String>>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<()> {
        if visited.contains(node) {
            return Ok(());
        }
        if visiting.contains(node) {
            bail!("plan DAG cycle detected at {node}");
        }
        visiting.insert(node.to_string());
        if let Some(nexts) = adj.get(node) {
            for next in nexts {
                dfs(next, adj, visiting, visited)?;
            }
        }
        visiting.remove(node);
        visited.insert(node.to_string());
        Ok(())
    }

    for id in ids {
        dfs(&id, &adj, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn shell_tokens(cmd: &str) -> Vec<&str> {
    cmd.split(|c: char| c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>'))
        .filter(|part| !part.is_empty())
        .collect()
}

fn contains_token_pair(cmd: &str, first: &str, second: &str) -> bool {
    let tokens = shell_tokens(cmd);
    tokens
        .windows(2)
        .any(|window| window[0] == first && window[1] == second)
}

fn looks_like_cargo_test(cmd: &str) -> bool {
    contains_token_pair(cmd, "cargo", "test")
}

fn starts_direct_debug_binary(cmd: &str) -> bool {
    let first = shell_tokens(cmd).into_iter().next().unwrap_or("");
    first.starts_with("./target/debug/") || first.contains("/target/debug/")
}

fn looks_like_long_running_command(cmd: &str) -> bool {
    contains_token_pair(cmd, "cargo", "run")
        || contains_token_pair(cmd, "cargo", "watch")
        || starts_direct_debug_binary(cmd)
        || cmd.contains(" --tlog ")
        || cmd.contains("| tee")
}

fn spawn_detached_with_log(cmd: &str, cwd_path: &Path) -> Result<(u32, PathBuf)> {
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis();
    let log_path = env::temp_dir().join(format!("canon-mini-agent-{pid}-{ts}.log"));
    let stdout_file = File::create(&log_path)
        .with_context(|| format!("failed to create log file {}", log_path.display()))?;
    let stderr_file = stdout_file
        .try_clone()
        .with_context(|| format!("failed to clone log file {}", log_path.display()))?;
    let child = Command::new("/bin/bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .with_context(|| ctx_spawn(cmd))?;
    Ok((child.id(), log_path))
}

fn summarize_cargo_test_log(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    if contents.trim().is_empty() {
        return None;
    }
    let mut passed = 0usize;
    let mut failed = 0usize;
    for line in contents.lines() {
        let line = line.trim_start();
        if line.starts_with("test ") {
            if line.ends_with("... ok") {
                passed += 1;
            } else if line.ends_with("... FAILED") {
                failed += 1;
            }
        }
    }
    for line in contents.lines() {
        if let Some(idx) = line.find("test result:") {
            let tail = line[idx..].trim();
            if failed == 0 {
                return Some(format!(
                    "all tests passed (counted: passed={passed} failed={failed}). last: {tail}"
                ));
            }
            return Some(format!(
                "tests failed (counted: passed={passed} failed={failed}). last: {tail}"
            ));
        }
    }
    if failed == 0 {
        Some(format!(
            "all tests passed (counted: passed={passed} failed={failed})"
        ))
    } else {
        Some(format!(
            "tests failed (counted: passed={passed} failed={failed})"
        ))
    }
}

fn exec_run_command(workspace: &Path, cmd: &str, cwd: &str) -> Result<(bool, String)> {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.is_absolute() {
        bail!("run_command cwd must be absolute: {cwd}");
    }
    if !cwd_path.starts_with(workspace) && !cwd_path.starts_with("/tmp") {
        bail!("run_command cwd escapes workspace: {cwd}");
    }
    ensure_safe_command(cmd)?;
    // Hybrid execution model:
    // - long-running commands → spawn (non-blocking)
    // - short commands → capture output (blocking)

    if looks_like_cargo_test(cmd) {
        let timeout_secs = env::var("CANON_CARGO_TEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20 * 60);
        let wrapped_cmd = format!("timeout -s TERM {}s {}", timeout_secs, cmd);
        let (pid, log_path) = spawn_detached_with_log(&wrapped_cmd, &cwd_path)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        let summary_line = summarize_cargo_test_log(&log_path);
        let mut summary = format!(
            "detached cargo test pid={} output_log={} timeout_secs={}",
            pid,
            log_path.display(),
            timeout_secs
        );
        summary.push_str(&format!("\noutput_log: {}", log_path.display()));
        summary.push_str(&format!("\nprogress_path: {}", log_path.display()));
        if let Some(summary_line) = summary_line {
            summary.push_str(&format!("\nsummary: {summary_line}"));
        }
        return Ok((true, summary));
    }

    let is_long_running = looks_like_long_running_command(cmd);

    if is_long_running {
        let child = Command::new("/bin/bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(&cwd_path)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| ctx_spawn(cmd))?;

        Ok((true, format!("spawned pid={}", child.id())))
    } else {
        let output = Command::new("/bin/bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(&cwd_path)
            .output()
            .with_context(|| ctx_spawn(cmd))?;

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if combined.trim().is_empty() && !output.status.success() {
            if cmd.contains("rg ") || cmd.contains("grep ") {
                combined = format!("no matches (exit={})", output.status.code().unwrap_or(-1));
                if cmd.contains("/tmp/runtime.trace") {
                    combined.push_str("\ntrace probe returned no matches; file may be stale, missing, or the pattern may not be present yet");
                }
            }
        }
        if cmd.contains("/tmp/runtime.trace") && (cmd.contains("rg ") || cmd.contains("grep ")) {
            let trace = PathBuf::from("/tmp/runtime.trace");
            match std::fs::metadata(&trace) {
                Ok(meta) => {
                    combined.push_str(&format!(
                        "\ntrace_path=/tmp/runtime.trace trace_size={}B",
                        meta.len()
                    ));
                }
                Err(_) => {
                    combined.push_str("\ntrace_path=/tmp/runtime.trace trace_missing=true");
                }
            }
        }

        Ok((output.status.success(), combined))
    }
}

fn tail_file_lines(path: &Path, max_lines: usize) -> Option<String> {
    use std::thread::sleep;
    use std::time::Duration;

    for _ in 0..3 {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let lines: Vec<&str> = contents.lines().collect();
                if lines.is_empty() {
                    return Some(String::new());
                }
                let start = lines.len().saturating_sub(max_lines);
                return Some(lines[start..].join("\n"));
            }
            Err(_) => {
                sleep(Duration::from_millis(200));
            }
        }
    }
    None
}

fn exec_run_command_blocking_with_timeout(
    workspace: &Path,
    cmd: &str,
    cwd: &str,
    timeout_secs: u64,
) -> Result<(bool, String)> {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.is_absolute() {
        bail!("run_command cwd must be absolute: {cwd}");
    }
    if !cwd_path.starts_with(workspace) && !cwd_path.starts_with("/tmp") {
        bail!("run_command cwd escapes workspace: {cwd}");
    }
    ensure_safe_command(cmd)?;
    let wrapped_cmd = format!("timeout -s TERM {}s {}", timeout_secs, cmd);
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(&wrapped_cmd)
        .current_dir(&cwd_path)
        .output()
        .with_context(|| ctx_spawn(&wrapped_cmd))?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok((output.status.success(), combined))
}

fn ctx_read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn ctx_spawn(cmd: &str) -> String {
    format!("failed to spawn: {cmd}")
}

fn exec_graph_command(workspace: &Path, cmd: &str) -> Result<(bool, String)> {
    let timeout_secs = env::var("CANON_GRAPH_CMD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5 * 60);
    exec_run_command_blocking_with_timeout(
        workspace,
        cmd,
        crate::constants::workspace(),
        timeout_secs,
    )
}

fn exec_python(workspace: &Path, code: &str, cwd: &str) -> Result<(bool, String)> {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.is_absolute() {
        bail!("python cwd must be absolute: {cwd}");
    }
    if !cwd_path.starts_with(workspace) && !cwd_path.starts_with("/tmp") {
        bail!("python cwd escapes workspace: {cwd}");
    }
    let mut child = Command::new("python3")
        .arg("-")
        .current_dir(&cwd_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn python3 in {}", cwd_path.display()))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(code.as_bytes())
            .context("failed writing python stdin")?;
    }
    let output = child
        .wait_with_output()
        .context("failed waiting for python3")?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok((output.status.success(), combined))
}

fn ensure_safe_command(cmd: &str) -> Result<()> {
    const BLOCKED: &[&str] = &[
        "rm -rf",
        "git reset --hard",
        "git clean -f",
        "dd if=",
        "mkfs",
        "shred",
    ];
    for needle in BLOCKED {
        if cmd.contains(needle) {
            bail!("blocked command: {cmd}");
        }
    }
    Ok(())
}

fn safe_join(workspace: &Path, relative: &str) -> Result<PathBuf> {
    let p = Path::new(relative);
    if p.is_absolute() {
        if p.starts_with(workspace) {
            return Ok(p.to_path_buf());
        }
        if p.starts_with("/tmp") {
            return Ok(p.to_path_buf());
        }
        bail!("absolute paths not allowed: {relative}");
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("path traversal not allowed: {relative}");
    }
    Ok(workspace.join(p))
}

fn execute_action(
    role: &str,
    step: usize,
    action: &Value,
    workspace: &Path,
    check_on_done: bool,
) -> Result<(bool, String)> {
    let _ = check_on_done;
    let kind = action
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    tokio::task::block_in_place(|| {
        match kind.as_str() {
        "message" => handle_message_action(role, step, action),
        "list_dir" => handle_list_dir_action(workspace, action),
        "read_file" => handle_read_file_action(role, step, workspace, action),
        "objectives" => handle_objectives_action(workspace, action),
        "issue" => handle_issue_action(workspace, action),
        "apply_patch" => handle_apply_patch_action(role, step, workspace, action),
        "run_command" => handle_run_command_action(role, step, workspace, action),
        "python" => handle_python_action(role, step, workspace, action),
        k @ ("rustc_hir" | "rustc_mir") => handle_rustc_action(role, step, k, workspace, action),
        k @ ("graph_call" | "graph_cfg") => handle_graph_call_cfg_action(role, step, k, workspace, action),
        k @ ("graph_dataflow" | "graph_reachability") => {
            handle_graph_reports_action(role, step, k, workspace, action)
        }
        "cargo_test" => handle_cargo_test_action(role, step, workspace, action),
        "plan" => handle_plan_action(role, workspace, action),
        other => Ok((
            false,
            format!(
                "unsupported action '{other}' — use list_dir, read_file, objectives, issue, apply_patch, run_command, python, cargo_test, plan, rustc_hir, rustc_mir, graph_call, graph_cfg, graph_dataflow, graph_reachability, or message"
            ),
        )),
    }
    })
}

fn persist_inbound_message(role: &str, step: usize, action: &Value, full_message: &str) {
    let Some(to_raw) = action.get("to").and_then(|v| v.as_str()) else {
        return;
    };
    if to_raw.trim().is_empty() {
        return;
    }
    let to = to_raw
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let agent_state_dir = std::path::Path::new(crate::constants::agent_state_dir());
    let _ = std::fs::create_dir_all(agent_state_dir);
    let path = agent_state_dir.join(format!("last_message_to_{to}.json"));
    if let Err(err) = std::fs::write(&path, full_message) {
        eprintln!(
            "[{role}] step={} failed to persist inbound message for {}: {}",
            step, to, err
        );
        log_error_event(
            role,
            "persist_inbound_message",
            Some(step),
            &format!("failed to persist inbound message for {}: {}", to, err),
            Some(json!({ "path": path.to_string_lossy(), "to": to })),
        );
    }
    let wake_path = agent_state_dir.join(format!("wakeup_{to}.flag"));
    let _ = std::fs::write(wake_path, "handoff");
}

pub(crate) fn execute_logged_action(
    role: &str,
    prompt_kind: &str,
    endpoint: &LlmEndpoint,
    workspace: &Path,
    step: usize,
    command_id: &str,
    action: &Value,
    check_on_done: bool,
) -> Result<(bool, String)> {
    log_action_event(role, endpoint, prompt_kind, step, command_id, action);
    match execute_action(role, step, action, workspace, check_on_done) {
        Ok((done, out)) => {
            log_action_result(
                role,
                endpoint,
                prompt_kind,
                step,
                command_id,
                action,
                true,
                &out,
            );
            Ok((done, out))
        }
        Err(e) => {
            let err_text = if action.get("action").and_then(|v| v.as_str()) == Some("plan") {
                format!(
                    "Error executing action: {e}\n\nPlan tool examples:\n{}\n{}\n{{\"action\":\"plan\",\"op\":\"update_task\",\"task\":{{\"id\":\"T4\",\"status\":\"done\"}},\"rationale\":\"Update a task by id using task payload.\"}}\n\nTo mark a task done, use update_task or set_task_status. set_plan_status changes only PLAN.status.",
                    plan_set_task_status_action_example(
                        "T1",
                        "in_progress",
                        "Update one task status in PLAN.json."
                    ),
                    plan_set_plan_status_action_example(
                        "in_progress",
                        "Update top-level PLAN.json status."
                    ),
                )
            } else {
                format!("Error executing action: {e}")
            };
            log_action_result(
                role,
                endpoint,
                prompt_kind,
                step,
                command_id,
                action,
                false,
                &err_text,
            );
            log_error_event(
                role,
                "execute_logged_action",
                Some(step),
                &format!("execute_logged_action error: {e}"),
                Some(json!({
                    "prompt_kind": prompt_kind,
                    "command_id": command_id,
                    "action": action.get("action").and_then(|v| v.as_str()),
                })),
            );
            eprintln!("[{role}] step={} error: {e}", step);
            Ok((false, err_text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::handle_apply_patch_action;
    use super::handle_objectives_action;
    use super::handle_plan_action;
    use serde_json::json;
    use std::path::PathBuf;

    fn fresh_test_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("canon-mini-agent-{name}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn diagnostics_apply_patch_rejects_unvalidated_ranked_failures() {
        let tmp = fresh_test_dir("rejects-unvalidated-ranked-failures");
        std::fs::write(
            tmp.join("DIAGNOSTICS.json"),
            "{\"status\":\"healthy\",\"ranked_failures\":[]}",
        )
        .unwrap();
        let action = json!({
            "patch": "*** Begin Patch\n*** Delete File: DIAGNOSTICS.json\n*** Add File: DIAGNOSTICS.json\n+{\n+  \"status\": \"critical_failure\",\n+  \"summary\": \"stale issue\",\n+  \"ranked_failures\": [\n+    {\n+      \"id\": \"D1\",\n+      \"evidence\": [\"old report without source validation\"]\n+    }\n+  ]\n+}\n*** End Patch"
        });

        let (_done, out) = handle_apply_patch_action("diagnostics", 1, &tmp, &action).unwrap();

        assert!(out.contains("ranked_failures require current-source validation before persistence"));
        let persisted = std::fs::read_to_string(tmp.join("DIAGNOSTICS.json")).unwrap();
        assert_eq!(persisted, "{\"status\":\"healthy\",\"ranked_failures\":[]}");
    }

    #[test]
    fn diagnostics_apply_patch_allows_source_validated_ranked_failures() {
        let tmp = fresh_test_dir("allows-source-validated-ranked-failures");
        std::fs::write(
            tmp.join("DIAGNOSTICS.json"),
            r#"{"status":"healthy","inputs_scanned":[],"ranked_failures":[],"planner_handoff":[]}"#,
        )
        .unwrap();
        let action = json!({
            "patch": "*** Begin Patch\n*** Delete File: DIAGNOSTICS.json\n*** Add File: DIAGNOSTICS.json\n+{\n+  \"status\": \"critical_failure\",\n+  \"inputs_scanned\": [\"agent_state/default/log.jsonl\"],\n+  \"ranked_failures\": [\n+    {\n+      \"id\": \"D1\",\n+      \"impact\": \"high\",\n+      \"signal\": \"read_file src/app.rs verified against current source\",\n+      \"evidence\": [\"read_file src/app.rs:1-50 — confirmed missing check\"],\n+      \"root_cause\": \"missing validation\",\n+      \"repair_targets\": [\"src/app.rs\"]\n+    }\n+  ],\n+  \"planner_handoff\": [\"Fix missing validation in src/app.rs\"]\n+}\n*** End Patch"
        });

        let (_done, out) = handle_apply_patch_action("diagnostics", 1, &tmp, &action).unwrap();

        assert!(out.contains("apply_patch ok"), "unexpected: {out}");
        let persisted = std::fs::read_to_string(tmp.join("DIAGNOSTICS.json")).unwrap();
        assert!(persisted.contains("critical_failure"));
    }

    #[test]
    fn plan_update_task_rejects_reopened_task_without_regression_linkage() {
        let tmp = fresh_test_dir("rejects-reopened-task-without-regression-linkage");
        std::fs::write(
            tmp.join("PLAN.json"),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Regression-linked task",
      "status": "done",
      "priority": 1,
      "steps": ["existing regression coverage"]
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "update_task",
            "task": {
                "id": "T1",
                "status": "in_progress",
                "steps": ["resume implementation without linked test"]
            },
            "rationale": "Exercise reopened-task enforcement"
        });

        let err = handle_plan_action("solo", &tmp, &action).unwrap_err().to_string();

        assert!(err.contains("reopened task T1 must include regression-test linkage"));
    }

    #[test]
    fn plan_update_task_allows_reopened_task_with_regression_linkage() {
        let tmp = fresh_test_dir("allows-reopened-task-with-regression-linkage");
        std::fs::write(
            tmp.join("PLAN.json"),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Regression-linked task",
      "status": "done",
      "priority": 1,
      "steps": ["existing regression coverage"]
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "update_task",
            "task": {
                "id": "T1",
                "status": "in_progress",
                "steps": ["add regression test linkage before reopening"]
            },
            "rationale": "Exercise reopened-task allowance"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();

        assert!(out.contains("plan ok"));
        let persisted = std::fs::read_to_string(tmp.join("PLAN.json")).unwrap();
        assert!(persisted.contains("\"status\": \"in_progress\""));
        assert!(persisted.contains("add regression test linkage before reopening"));
    }

    #[test]
    fn plan_set_status_rejects_done_when_any_task_is_incomplete() {
        let tmp = fresh_test_dir("rejects-plan-done-while-task-incomplete");
        std::fs::write(
            tmp.join("PLAN.json"),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Completed task",
      "status": "done",
      "priority": 1
    },
    {
      "id": "T2",
      "title": "Incomplete task",
      "status": "in_progress",
      "priority": 2
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "set_plan_status",
            "status": "done",
            "rationale": "Exercise plan/task convergence guard"
        });

        let err = handle_plan_action("solo", &tmp, &action).unwrap_err().to_string();

        assert!(err.contains("plan status cannot be set to done while tasks remain incomplete"));
        let persisted = std::fs::read_to_string(tmp.join("PLAN.json")).unwrap();
        assert!(persisted.contains("\"status\": \"in_progress\""));
    }

    #[test]
    fn plan_set_task_status_marks_only_target_task_done() {
        let tmp = fresh_test_dir("set-task-status-only-target-task");
        std::fs::write(
            tmp.join("PLAN.json"),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Task one",
      "status": "in_progress",
      "priority": 1
    },
    {
      "id": "T2",
      "title": "Task two",
      "status": "todo",
      "priority": 2
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_task_status",
            "task_id": "T1",
            "status": "done",
            "rationale": "Close only one task"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();
        assert!(out.contains("plan ok"));

        let persisted = std::fs::read_to_string(tmp.join("PLAN.json")).unwrap();
        assert!(persisted.contains("\"id\": \"T1\""));
        assert!(persisted.contains("\"status\": \"done\""));
        assert!(persisted.contains("\"id\": \"T2\""));
        assert!(persisted.contains("\"status\": \"todo\""));
        assert!(persisted.contains("\"status\": \"in_progress\""));
    }

    #[test]
    fn plan_legacy_set_status_with_task_id_maps_to_set_task_status() {
        let tmp = fresh_test_dir("legacy-set-status-task-id-maps");
        std::fs::write(
            tmp.join("PLAN.json"),
            r#"{
  "version": 2,
  "status": "in_progress",
  "tasks": [
    {
      "id": "T1",
      "title": "Task one",
      "status": "in_progress",
      "priority": 1
    }
  ],
  "dag": { "edges": [] }
}"#,
        )
        .unwrap();
        let action = json!({
            "op": "set_status",
            "task_id": "T1",
            "status": "done",
            "rationale": "Backward compatibility for old op naming"
        });

        let (_done, out) = handle_plan_action("solo", &tmp, &action).unwrap();
        assert!(out.contains("plan ok"));

        let persisted = std::fs::read_to_string(tmp.join("PLAN.json")).unwrap();
        assert!(persisted.contains("\"id\": \"T1\""));
        assert!(persisted.contains("\"status\": \"done\""));
    }

    #[test]
    fn objectives_update_objective_reports_requested_and_compared_ids() {
        let tmp = fresh_test_dir("objective-update-not-found-context");
        std::fs::create_dir_all(tmp.join("PLANS")).unwrap();
        std::fs::write(
            tmp.join("PLANS").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    },
    {
      "id": "obj_beta",
      "title": "Beta",
      "status": "active",
      "scope": "beta scope",
      "authority_files": ["src/objectives.rs"],
      "category": "quality",
      "level": "low",
      "description": "beta",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "update_objective",
            "objective_id": "obj_missing",
            "updates": {
                "scope": "updated"
            }
        });

        let err = handle_objectives_action(&tmp, &action).unwrap_err().to_string();

        assert!(err.contains("requested_raw=\"obj_missing\""));
        assert!(err.contains("objective not found:"));
        assert!(err.contains("requested_id=obj_missing"));
        assert!(err.contains("compared_ids=[\"obj_alpha\", \"obj_beta\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\", \"obj_beta\"]"));
    }

    #[test]
    fn objectives_set_status_matches_normalized_id() {
        let tmp = fresh_test_dir("objective-set-status-normalized-id");
        std::fs::create_dir_all(tmp.join("PLANS")).unwrap();
        std::fs::write(
            tmp.join("PLANS").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "set_status",
            "objective_id": "`obj_alpha`",
            "status": "done"
        });

        let (_done, out) = handle_objectives_action(&tmp, &action).unwrap();

        assert!(out.contains("objectives set_status ok"));
        let persisted = std::fs::read_to_string(tmp.join("PLANS").join("OBJECTIVES.json")).unwrap();
        assert!(persisted.contains("\"status\": \"done\""));
    }

    #[test]
    fn objectives_update_objective_reports_raw_and_normalized_lookup_context() {
        let tmp = fresh_test_dir("objective-update-raw-and-normalized-context");
        std::fs::create_dir_all(tmp.join("PLANS")).unwrap();
        std::fs::write(
            tmp.join("PLANS").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "update_objective",
            "objective_id": "`obj_missing`",
            "updates": {
                "scope": "updated"
            }
        });

        let err = handle_objectives_action(&tmp, &action).unwrap_err().to_string();

        assert!(err.contains("requested_raw=\"`obj_missing`\""));
        assert!(err.contains("requested_id=obj_missing"));
        assert!(err.contains("compared_ids=[\"obj_alpha\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\"]"));
    }

    #[test]
    fn objectives_create_objective_reports_raw_and_normalized_duplicate_context() {
        let tmp = fresh_test_dir("objective-create-duplicate-context");
        std::fs::create_dir_all(tmp.join("PLANS")).unwrap();
        std::fs::write(
            tmp.join("PLANS").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [
    {
      "id": "obj_alpha",
      "title": "Alpha",
      "status": "active",
      "scope": "alpha scope",
      "authority_files": ["src/tools.rs"],
      "category": "quality",
      "level": "low",
      "description": "alpha",
      "requirement": [],
      "verification": [],
      "success_criteria": []
    }
  ],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let action = json!({
            "op": "create_objective",
            "objective": {
                "id": "`obj_alpha`",
                "title": "Alpha duplicate",
                "status": "active",
                "scope": "duplicate scope",
                "authority_files": ["src/tools.rs"],
                "category": "quality",
                "level": "low",
                "description": "duplicate",
                "requirement": [],
                "verification": [],
                "success_criteria": []
            }
        });

        let err = handle_objectives_action(&tmp, &action).unwrap_err().to_string();

        assert!(err.contains("objective id already exists:"));
        assert!(err.contains("requested_raw="));
        assert!(err.contains("requested_id=obj_alpha"));
        assert!(err.contains("compared_ids=[\"obj_alpha\"]"));
        assert!(err.contains("compared_normalized_ids=[\"obj_alpha\"]"));
    }

    #[test]
    fn objectives_create_update_read_lifecycle_succeeds() {
        let tmp = fresh_test_dir("objective-create-update-read-lifecycle");
        std::fs::create_dir_all(tmp.join("PLANS")).unwrap();
        std::fs::write(
            tmp.join("PLANS").join("OBJECTIVES.json"),
            r#"{
  "version": 1,
  "objectives": [],
  "goal": [],
  "instrumentation": [],
  "definition_of_done": [],
  "non_goals": []
}"#,
        )
        .unwrap();

        let create_action = json!({
            "op": "create_objective",
            "objective": {
                "id": "obj_lifecycle",
                "title": "Lifecycle",
                "status": "active",
                "scope": "objective lifecycle coverage",
                "authority_files": ["src/tools.rs", "PLANS/OBJECTIVES.json"],
                "category": "quality",
                "level": "medium",
                "description": "create/update/read lifecycle objective",
                "requirement": ["create succeeds"],
                "verification": [],
                "success_criteria": ["updated objective is readable"]
            }
        });
        let (_done, create_out) = handle_objectives_action(&tmp, &create_action).unwrap();
        assert!(create_out.contains("objectives create_objective ok"));

        let update_action = json!({
            "op": "update_objective",
            "objective_id": "obj_lifecycle",
            "updates": {
                "scope": "updated lifecycle scope",
                "description": "updated lifecycle objective",
                "verification": ["updated through handle_objectives_action"]
            }
        });
        let (_done, update_out) = handle_objectives_action(&tmp, &update_action).unwrap();
        assert!(update_out.contains("objectives update_objective ok"));

        let read_action = json!({ "op": "read", "include_done": true });
        let (_done, read_out) = handle_objectives_action(&tmp, &read_action).unwrap();
        assert!(read_out.contains("\"id\": \"obj_lifecycle\""));
        assert!(read_out.contains("\"scope\": \"updated lifecycle scope\""));
        assert!(read_out.contains("\"description\": \"updated lifecycle objective\""));
        assert!(read_out.contains("updated through handle_objectives_action"));
    }
}

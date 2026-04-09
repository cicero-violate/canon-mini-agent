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
    diagnostics_file, is_self_modification_mode, MASTER_PLAN_FILE, MAX_FULL_READ_LINES,
    MAX_SNIPPET, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::objectives::filter_incomplete_objectives_json;
use crate::logging::{log_action_event, log_action_result, log_error_event, now_ms};
use crate::prompts::truncate;

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

fn handle_objectives_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let include_done = action
        .get("include_done")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let raw = fs::read_to_string(workspace.join(OBJECTIVES_FILE)).unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok((false, "(no objectives)".to_string()));
    }
    if include_done {
        return Ok((false, raw));
    }
    let filtered = filter_incomplete_objectives_json(&raw).unwrap_or(raw);
    Ok((false, filtered))
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
                || touches_diagnostics
                || targets.iter().any(|path| is_src_path(path) || is_tests_path(path))
            {
                Some(
                    "Planner may patch lane plans under `PLANS/<instance>/executor-<id>.json` (or legacy `PLANS/executor-<id>.md`); planner may not patch `src/`, `tests/`, `SPEC.md`, `VIOLATIONS.json`, or diagnostics files."
                        .to_string(),
                )
            } else if touches_lane {
                None
            } else {
                Some(
                    "Planner may patch lane plans only. Use the `plan` action for `PLAN.json` updates; no other patches are allowed."
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
    if let Some(msg) = patch_scope_error(role, patch) {
        return Ok((false, msg));
    }
    match apply_patch(patch, workspace) {
        Ok(_) => {
            eprintln!("[{role}] step={} apply_patch ok", step);
            let check_result = patch_first_file(patch)
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
            let read_path = extract_anchor_fail_path(&err_str)
                .or_else(|| patch_first_file(patch).map(|s| s.to_string()));
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
    let mut tail_preview: Option<String> = None;
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
            tail_preview = tail_file_lines(Path::new(path), 200);
        }
        if let Some(tail) = tail_preview.as_ref() {
            if tail.trim().is_empty() {
                summary.push_str("\noutput_log_tail: (log empty)");
            } else {
                summary.push_str("\noutput_log_tail:\n");
                summary.push_str(tail);
            }
        } else if out.contains("detached cargo test") {
            summary.push_str("\noutput_log_tail: (not ready yet)");
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
    SetStatus,
    ReplacePlan,
}

impl PlanOp {
    const VALID_OPS: &'static str =
        "create_task, update_task, delete_task, add_edge, remove_edge, set_status, replace_plan";

    fn parse(op: &str) -> Result<Self> {
        match op {
            "create_task" => Ok(Self::CreateTask),
            "update_task" => Ok(Self::UpdateTask),
            "delete_task" => Ok(Self::DeleteTask),
            "add_edge" => Ok(Self::AddEdge),
            "remove_edge" => Ok(Self::RemoveEdge),
            "set_status" => Ok(Self::SetStatus),
            "replace_plan" => Ok(Self::ReplacePlan),
            _ => bail!("unknown plan op: {op} (valid ops: {})", Self::VALID_OPS),
        }
    }
}

fn handle_plan_action(role: &str, workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let op_raw = action
        .get("op")
        .and_then(|v| v.as_str())
        .or_else(|| action.get("operation").and_then(|v| v.as_str()))
        .unwrap_or("update");
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
        PlanOp::SetStatus => {
            let status = action
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan set_status missing status"))?;
            obj.insert("status".to_string(), Value::String(status.to_string()));
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
        return Ok((
            true,
            format!(
                "detached cargo test pid={} output_log={} timeout_secs={}",
                pid,
                log_path.display(),
                timeout_secs
            ),
        ));
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
                "unsupported action '{other}' — use list_dir, read_file, objectives, apply_patch, run_command, python, cargo_test, plan, rustc_hir, rustc_mir, graph_call, graph_cfg, graph_dataflow, graph_reachability, or message"
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
                    "Error executing action: {e}\n\nPlan tool examples:\n{{\"action\":\"plan\",\"op\":\"set_status\",\"task_id\":\"T1\",\"status\":\"in_progress\",\"rationale\":\"Update PLAN.json via the plan tool while running solo.\"}}\n{{\"action\":\"plan\",\"op\":\"create_task\",\"task\":{{\"id\":\"T4\",\"title\":\"Add plan DAG\",\"status\":\"todo\",\"priority\":3}},\"rationale\":\"Add a new task to PLAN.json without manual patching.\"}}"
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
            eprintln!("[{role}] step={} error: {e}", step);
            Ok((false, err_text))
        }
    }
}

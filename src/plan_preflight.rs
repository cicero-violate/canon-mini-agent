/// Plan preflight validator — semantic oracle between planner and executor.
///
/// Runs after the planner marks tasks `ready` and before the executor picks them
/// up.  For each ready task it:
///
/// 1. Extracts Rust symbol references (tokens containing `::` that start with a
///    known workspace crate name) from the task's `title`, `description`, and
///    `steps` fields.
/// 2. Checks each reference against the workspace `SemanticIndex`.
/// 3. If any reference cannot be resolved, demotes the task from `ready` to
///    `needs_planning` and writes a `preflight_note` explaining which symbols
///    were missing.  The planner sees this on the next cycle and corrects the
///    task before re-marking it ready.
///
/// Returns one `PreflightBounce` per demoted task.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::SemanticIndex;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PreflightBounce {
    pub task_id: String,
    pub missing_symbols: Vec<String>,
    pub note: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Intent: validation_gate
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::vec::Vec<plan_preflight::PreflightBounce>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn preflight_ready_tasks(workspace: &Path) -> Vec<PreflightBounce> {
    // ── Plan-task gap check ────────────────────────────────────────────────────
    // Record a PlanPreflightFailed blocker for every active repair plan that
    // has no open task.  This feeds blocker_class_coverage → eval pressure →
    // REPAIR_PLAN → planner creates the missing tasks.
    let missing_tasks = plans_without_open_tasks(workspace);
    for plan_gap in &missing_tasks {
        crate::blockers::record_action_failure_with_writer(
            workspace,
            None,
            "orchestrator",
            "plan_preflight",
            &format!(
                "active repair plan binding failed: {plan_gap} — planner must \
                create/update a PLAN task with exact repair_plan_id, \
                required_mutation, and target_files before executor dispatch"
            ),
            None,
        );
    }

    // ── Symbol reference check ────────────────────────────────────────────────
    match try_preflight_ready_tasks(workspace) {
        Ok(bounces) => {
            if !bounces.is_empty() {
                eprintln!(
                    "[plan_preflight] demoted {} task(s) with missing symbol references",
                    bounces.len()
                );
                for b in &bounces {
                    eprintln!(
                        "[plan_preflight] task={} missing={:?}",
                        b.task_id, b.missing_symbols
                    );
                    crate::blockers::record_action_failure_with_writer(
                        workspace,
                        None,
                        "orchestrator",
                        "plan_preflight",
                        &b.note,
                        Some(&b.task_id),
                    );
                }
            }
            bounces
        }
        Err(e) => {
            eprintln!("[plan_preflight] error: {e:#}");
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                "orchestrator",
                "plan_preflight",
                &e.to_string(),
                None,
            );
            vec![]
        }
    }
}

// ── Implementation ────────────────────────────────────────────────────────────

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::vec::Vec<plan_preflight::PreflightBounce>, anyhow::Error>
/// Effects: fs_read, fs_write, state_read, state_write, transitions_state
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn try_preflight_ready_tasks(workspace: &Path) -> Result<Vec<PreflightBounce>> {
    let plan_path = workspace.join(crate::constants::MASTER_PLAN_FILE);
    let raw = match std::fs::read_to_string(&plan_path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return Ok(vec![]),
    };
    let mut plan: Value = serde_json::from_str(&raw)?;

    let crate_names = SemanticIndex::available_crates(workspace);
    if crate_names.is_empty() {
        // No graph data — skip validation to avoid false positives.
        return Ok(vec![]);
    }

    // Load all available indexes once; cache by crate name.
    let indexes: HashMap<String, SemanticIndex> = crate_names
        .iter()
        .filter_map(|cn| {
            SemanticIndex::load(workspace, cn)
                .ok()
                .map(|idx| (cn.clone(), idx))
        })
        .collect();

    if indexes.is_empty() {
        return Ok(vec![]);
    }

    let Some(tasks) = plan.get_mut("tasks").and_then(|v| v.as_array_mut()) else {
        return Ok(vec![]);
    };

    let mut bounces = Vec::new();

    for task in tasks.iter_mut() {
        let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !status.eq_ignore_ascii_case("ready") {
            continue;
        }

        let task_id = task
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if task_id.is_empty() {
            continue;
        }

        // Collect symbol candidates from all text fields.
        let text = collect_task_text(task);
        let candidates = extract_workspace_symbol_refs(&text, &crate_names);

        if candidates.is_empty() {
            continue;
        }

        // Check each candidate against any loaded index.
        let missing: Vec<String> = candidates
            .into_iter()
            .map(|sym| canonicalize_symbol_ref(&indexes, &sym))
            .filter(|sym| !symbol_exists(&indexes, sym))
            .collect();

        if missing.is_empty() {
            continue;
        }

        // Demote the task.
        let note = format!(
            "Preflight: the following symbol(s) referenced in this task were not found \
             in the workspace semantic graph — correct them before marking ready: {}",
            missing.join(", ")
        );

        if let Some(obj) = task.as_object_mut() {
            obj.insert(
                "status".to_string(),
                Value::String("needs_planning".to_string()),
            );
            obj.insert("preflight_note".to_string(), Value::String(note.clone()));
        }

        bounces.push(PreflightBounce {
            task_id,
            missing_symbols: missing,
            note,
        });
    }

    if !bounces.is_empty() {
        std::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?)?;
        log_preflight_bounces(workspace, &bounces);
    }

    Ok(bounces)
}

/// Concatenate all text fields of a plan task into one string for symbol extraction.
fn collect_task_text(task: &Value) -> String {
    let mut parts = Vec::new();
    for field in &["title", "description"] {
        if let Some(s) = task.get(field).and_then(|v| v.as_str()) {
            parts.push(s.to_string());
        }
    }
    if let Some(steps) = task.get("steps").and_then(|v| v.as_array()) {
        for step in steps {
            if let Some(s) = step.as_str() {
                parts.push(s.to_string());
            }
        }
    }
    parts.join(" ")
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &[std::string::String]
/// Outputs: std::vec::Vec<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn extract_workspace_symbol_refs(text: &str, crate_names: &[String]) -> Vec<String> {
    let normalized_crates: Vec<String> = crate_names.iter().map(|n| n.replace('-', "_")).collect();

    let mut result = Vec::new();
    let mut token = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == ':' {
            token.push(ch);
        } else {
            flush_token(&token, &normalized_crates, &mut result);
            token.clear();
        }
    }
    flush_token(&token, &normalized_crates, &mut result);

    result.sort();
    result.dedup();
    result
}

fn flush_token(token: &str, normalized_crates: &[String], out: &mut Vec<String>) {
    if !token.contains("::") || token.len() < 5 {
        return;
    }
    // Must consist entirely of valid path chars.
    if !token
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
    {
        return;
    }
    // Collapse consecutive colons to exactly "::".
    if token.contains(":::") {
        return;
    }
    let first_seg = token.split("::").next().unwrap_or("");
    // Skip pure keyword prefixes.
    if matches!(
        first_seg,
        "use" | "pub" | "mod" | "crate" | "super" | "self" | "std" | "core" | "alloc"
    ) {
        return;
    }
    // Only proceed if the first segment matches a known workspace crate.
    if !normalized_crates.iter().any(|cn| cn == first_seg) {
        return;
    }
    out.push(token.to_string());
}

/// Return true if `symbol` resolves in any of the loaded indexes.
fn symbol_exists(indexes: &HashMap<String, SemanticIndex>, symbol: &str) -> bool {
    indexes.values().any(|idx| idx.has_symbol(symbol))
}

fn canonicalize_symbol_ref(indexes: &HashMap<String, SemanticIndex>, symbol: &str) -> String {
    if symbol_exists(indexes, symbol) {
        return symbol.to_string();
    }
    canonicalize_missing_owner_method_ref(indexes, symbol).unwrap_or_else(|| symbol.to_string())
}

fn canonicalize_missing_owner_method_ref(
    indexes: &HashMap<String, SemanticIndex>,
    symbol: &str,
) -> Option<String> {
    let segments: Vec<&str> = symbol.split("::").filter(|seg| !seg.is_empty()).collect();
    if segments.len() < 3 {
        return None;
    }
    let method = segments.last()?;
    let module_prefix = segments[..segments.len() - 1].join("::");
    let mut matches = Vec::new();
    let method_suffix = format!("::{method}");
    let module_start = format!("{module_prefix}::");

    for index in indexes.values() {
        for summary in index.symbol_summaries() {
            let candidate = summary.symbol;
            if !candidate.starts_with(&module_start) || !candidate.ends_with(&method_suffix) {
                continue;
            }
            let middle_start = module_start.len();
            let middle_end = candidate.len() - method_suffix.len();
            if middle_end <= middle_start {
                continue;
            }
            let owner = &candidate[middle_start..middle_end];
            let owner_tail = owner.rsplit("::").next().unwrap_or(owner);
            // Treat "module::method" misses as missing owner only when the inserted
            // owner segment looks type-like (e.g. SemanticIndex).
            let type_like_owner = owner_tail
                .chars()
                .next()
                .map(|ch| ch.is_ascii_uppercase())
                .unwrap_or(false);
            if type_like_owner {
                matches.push(candidate);
            }
        }
    }

    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        Some(matches[0].clone())
    } else {
        None
    }
}

/// Write a structured log entry for each bounce so the lessons synthesis can
/// detect the "planner referenced unknown symbol" pattern.
fn log_preflight_bounces(_workspace: &Path, bounces: &[PreflightBounce]) {
    for bounce in bounces {
        let record = serde_json::json!({
            "kind": "tool",
            "phase": "result",
            "action": "plan_preflight",
            "ok": false,
            "actor": "orchestrator",
            "task_id": bounce.task_id,
            "text": bounce.note,
            "ts_ms": crate::logging::now_ms(),
        });
        let _ = crate::logging::append_action_log_record(&record);
    }
}

// ── Plan-task gap detection ───────────────────────────────────────────────────

/// Check whether every active repair plan has a strict PLAN binding.
/// Returns a planner-readable failure description for each broken binding.
///
/// "Open" means status ∈ {ready, in_progress, needs_planning}.
/// Matching requires task.repair_plan_id == plan.id, required_mutation ==
/// plan.required_mutation, and all target_files required by the repair plan.
pub fn plans_without_open_tasks(workspace: &Path) -> Vec<String> {
    let plan_raw =
        match std::fs::read_to_string(workspace.join(crate::constants::MASTER_PLAN_FILE)) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return Vec::new(),
        };

    // Build active plans from the eval map in latest.json.
    let latest_path = workspace
        .join("agent_state")
        .join("reports")
        .join("complexity")
        .join("latest.json");
    let eval_map = match std::fs::read_to_string(&latest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("eval")
                .and_then(|e| e.as_object())
                .map(|m| m.clone())
        }) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let plans = crate::repair_plans::build_all_active_plans(&eval_map, workspace, usize::MAX);

    plans
        .iter()
        .filter_map(|plan| {
            let binding = crate::repair_plans::verify_plan_binding(plan, &plan_raw);
            if binding.passed {
                None
            } else {
                Some(format!("{} — {}", plan.id, binding.description))
            }
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_workspace_symbols_only() {
        let crates = vec!["canon_route".to_string(), "canon_mini_agent".to_string()];
        let text = "Refactor canon_route::dispatch and std::fs::read_to_string and \
                    canon_mini_agent::app::run_agent. Also fix use::something.";
        let syms = extract_workspace_symbol_refs(text, &crates);
        assert!(syms.contains(&"canon_route::dispatch".to_string()));
        assert!(syms.contains(&"canon_mini_agent::app::run_agent".to_string()));
        // std:: and use:: must be excluded
        assert!(!syms.iter().any(|s| s.starts_with("std::")));
        assert!(!syms.iter().any(|s| s.starts_with("use::")));
    }

    #[test]
    fn no_symbols_in_plain_text() {
        let crates = vec!["canon_route".to_string()];
        let text = "Reduce branch complexity in the dispatch handler.";
        let syms = extract_workspace_symbol_refs(text, &crates);
        assert!(syms.is_empty());
    }

    #[test]
    fn deduplicates_repeated_refs() {
        let crates = vec!["canon_route".to_string()];
        let text = "canon_route::dispatch is too complex. Simplify canon_route::dispatch.";
        let syms = extract_workspace_symbol_refs(text, &crates);
        assert_eq!(syms.len(), 1);
    }

    #[test]
    fn dash_in_crate_name_normalised() {
        let crates = vec!["canon-route".to_string()];
        let text = "Refactor canon_route::handler";
        let syms = extract_workspace_symbol_refs(text, &crates);
        assert!(syms.contains(&"canon_route::handler".to_string()));
    }

    #[test]
    fn triple_colon_is_rejected() {
        let crates = vec!["canon_route".to_string()];
        let text = "canon_route:::bad_path";
        let syms = extract_workspace_symbol_refs(text, &crates);
        assert!(syms.is_empty());
    }
}

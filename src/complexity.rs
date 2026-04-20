use anyhow::{Context, Result};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::semantic::{shorten_display_path, SemanticIndex};

fn reports_dir(workspace: &Path) -> PathBuf {
    workspace
        .join("agent_state")
        .join("reports")
        .join("complexity")
}

fn sort_by_objective_desc(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    let score = |v: &serde_json::Value| {
        v.get("objective_score")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
            .to_bits()
    };
    score(b).cmp(&score(a))
}

/// Compute normalized [0.0, 1.0] objective scores for all items in-place.
///
///   B_norm  = branch_score / max_branch_score      (terminator-weighted branching, weight 0.6)
///   R_norm  = stmt_density / max_stmt_density      (redundancy proxy, weight 0.4)
///   stmt_density = mir_stmts / max(mir_blocks, 1)
///   objective_score = 0.6 * B_norm + 0.4 * R_norm
///
/// branch_score = SwitchInt×2 + Call×1 + Assert×0.5 over non-cleanup blocks.
/// Falls back to mir_blocks when branch_score is absent.
fn apply_objective_scores(items: &mut Vec<serde_json::Value>) {
    let max_branch = items
        .iter()
        .filter_map(|v| {
            v.get("branch_score")
                .and_then(|x| x.as_f64())
                .or_else(|| v.get("mir_blocks").and_then(|x| x.as_f64()))
        })
        .fold(0.0_f64, f64::max);
    let max_density = items
        .iter()
        .filter_map(|v| {
            let blocks = v.get("mir_blocks").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let stmts = v.get("mir_stmts").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if blocks > 0.0 {
                Some(stmts / blocks)
            } else {
                None
            }
        })
        .fold(0.0_f64, f64::max);

    for item in items.iter_mut() {
        let branch = item
            .get("branch_score")
            .and_then(|x| x.as_f64())
            .or_else(|| item.get("mir_blocks").and_then(|x| x.as_f64()))
            .unwrap_or(0.0);
        let blocks = item
            .get("mir_blocks")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let stmts = item
            .get("mir_stmts")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let b_norm = if max_branch > 0.0 {
            branch / max_branch
        } else {
            0.0
        };
        let density = if blocks > 0.0 { stmts / blocks } else { 0.0 };
        let r_norm = if max_density > 0.0 {
            density / max_density
        } else {
            0.0
        };
        let score = (0.6 * b_norm + 0.4 * r_norm).clamp(0.0, 1.0);
        if let Some(map) = item.as_object_mut() {
            map.insert(
                "stmt_density".to_string(),
                serde_json::json!(format!("{density:.2}")),
            );
            map.insert(
                "objective_score".to_string(),
                serde_json::json!(format!("{score:.3}")),
            );
        }
    }
}

fn process_crate(
    workspace: &Path,
    crate_name: &str,
    global: &mut Vec<serde_json::Value>,
    all_summaries: &mut Vec<crate::semantic::SymbolSummary>,
) -> serde_json::Value {
    let idx = match SemanticIndex::load(workspace, crate_name) {
        Ok(idx) => idx,
        Err(err) => {
            return json!({
                "crate": crate_name,
                "status": "error",
                "error": err.to_string(),
            });
        }
    };

    let mut items = collect_complexity_items(&idx, crate_name, global, all_summaries);

    apply_objective_scores(&mut items);
    items.sort_by(sort_by_objective_desc);
    let top = items.into_iter().take(50).collect::<Vec<_>>();

    json!({
        "crate": crate_name,
        "status": "ok",
        "metric": "objective_score(B*0.6+R*0.4)",
        "top": top,
    })
}

fn collect_complexity_items(
    idx: &crate::semantic::SemanticIndex,
    crate_name: &str,
    global: &mut Vec<serde_json::Value>,
    all_summaries: &mut Vec<crate::semantic::SymbolSummary>,
) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    for s in idx.symbol_summaries() {
        let blocks = s.mir_blocks.unwrap_or(0);
        let stmts = s.mir_stmts.unwrap_or(0);
        if blocks == 0 && stmts == 0 {
            continue;
        }
        all_summaries.push(s.clone());
        let entry = build_complexity_entry(&s, blocks, stmts);
        global.push(build_global_complexity_entry(crate_name, &entry));
        items.push(entry);
    }
    items
}

fn build_global_complexity_entry(crate_name: &str, entry: &serde_json::Value) -> serde_json::Value {
    json!({
        "crate": crate_name,
        "symbol": entry.get("symbol"),
        "file": entry.get("file"),
        "line": entry.get("line"),
        "complexity_proxy": entry.get("complexity_proxy"),
        "mir_blocks": entry.get("mir_blocks"),
        "mir_stmts": entry.get("mir_stmts"),
    })
}

fn build_complexity_entry(
    s: &crate::semantic::SymbolSummary,
    blocks: usize,
    stmts: usize,
) -> serde_json::Value {
    json!({
        "symbol": s.symbol,
        "file": shorten_display_path(&s.file),
        "line": s.line,
        "mir_fingerprint": s.mir_fingerprint,
        "mir_blocks": blocks,
        "mir_stmts": stmts,
        "branch_score": s.branch_score,
        "is_directly_recursive": s.is_directly_recursive,
        "complexity_proxy": s.branch_score.unwrap_or(blocks as f64),
    })
}

/// Emit a cyclomatic-complexity-style report on startup/restart.
///
/// Current implementation is a proxy based on MIR metadata already captured in
/// `state/rustc/<crate>/graph.json`:
/// - `complexity_proxy = mir_blocks` (higher tends to correlate with more branching).
///
/// This is intentionally cheap and deterministic; it can be upgraded later to true cyclomatic
/// complexity when canon-rustc-v2 records per-item CFG nodes/edges.
pub fn write_complexity_report(workspace: &Path) -> Result<Option<PathBuf>> {
    let crates = SemanticIndex::available_crates(workspace);
    if crates.is_empty() {
        return Ok(None);
    }

    let mut per_crate = Vec::new();
    let mut global = Vec::new();
    let mut current_summaries = Vec::new();

    for crate_name in crates {
        let entry = process_crate(workspace, &crate_name, &mut global, &mut current_summaries);
        per_crate.push(entry);
    }

    apply_objective_scores(&mut global);
    global.sort_by(sort_by_objective_desc);
    let global_top = global.into_iter().take(100).collect::<Vec<_>>();

    // Inter-function analysis: transitive B, MIR duplicate R, D_det
    let mut inter_sections = serde_json::json!({});
    for crate_name in SemanticIndex::available_crates(workspace) {
        if let Ok(analysis) = crate::inter_complexity::analyze(workspace, &crate_name) {
            inter_sections[&crate_name] = crate::inter_complexity::to_report_value(&analysis, 20);
        }
    }

    // Bridge-connectivity analysis: emit deterministic graph-overconnectivity issues.
    let _ = crate::graph_metrics::generate_bridge_connectivity_issues(workspace);

    let eval = crate::evaluation::evaluate_workspace(workspace);
    let drift = compute_and_persist_fingerprint_drift(workspace, &current_summaries)?;
    let report = build_complexity_report(per_crate, global_top, inter_sections, &eval, &drift);

    // Auto-generate issues for top hotspots (Detect → Propose step)
    let _ = crate::inter_complexity::generate_hotspot_issues(workspace, 5);
    // Auto-generate structural refactor issues (dead code, branch reduction, helper extraction, call chains)
    let _ = crate::refactor_analysis::generate_all_refactor_issues(workspace);
    let _ = crate::refactor_analysis::generate_panic_surface_issues(workspace);
    let _ = crate::refactor_analysis::generate_state_machine_issues(workspace);
    let _ = crate::refactor_analysis::generate_drop_complexity_issues(workspace);
    let _ = crate::refactor_analysis::generate_clone_pressure_issues(workspace);
    let _ = crate::refactor_analysis::generate_visibility_leak_issues(workspace);
    let _ = crate::refactor_analysis::generate_mono_explosion_issues(workspace);
    let _ = crate::refactor_analysis::generate_generic_overreach_issues(workspace);
    let _ = crate::refactor_analysis::generate_dead_impl_issues(workspace);
    let _ = crate::refactor_analysis::generate_dark_assignment_issues(workspace);
    let _ = crate::refactor_analysis::generate_loop_invariant_issues(workspace);
    let _ = crate::graph_metrics::generate_module_cohesion_issues(workspace);
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    if let Ok(dataset) = crate::grpo::extract_grpo_dataset(workspace, &tlog_path) {
        let _ = crate::grpo::record_grpo_dataset_effect(workspace, &dataset, None);
    }
    // Auto-generate invariant lifecycle issues (action surface gap, prompt injection gap, per-promoted gates)
    let _ = crate::invariants::generate_invariant_issues(workspace);

    let dir = reports_dir(workspace);
    let latest = persist_complexity_report(&dir, &report)?;

    Ok(Some(latest))
}

fn build_complexity_report(
    per_crate: Vec<serde_json::Value>,
    global_top: Vec<serde_json::Value>,
    inter: serde_json::Value,
    eval: &crate::evaluation::EvaluationWorkspaceSnapshot,
    drift: &crate::drift_analysis::FingerprintDrift,
) -> serde_json::Value {
    let intra_scoring = complexity_intra_scoring();
    let inter_scoring = complexity_inter_scoring();
    json!({
        "version": 2,
        "objective": "min(B) + min(R)  s.t. correctness invariant",
        "intra_scoring": intra_scoring,
        "inter_scoring": inter_scoring,
        "execution_model": "Detect(this_report) → Propose(LLM/issues) → Apply(patch/rename) → Verify(build+test)",
        "generated_at_ms": crate::logging::now_ms(),
        "global_top": global_top,
        "inter": inter,
        "eval": {
            "overall_score": eval.overall_score(),
            "objective_progress": eval.vector.objective_progress,
            "safety": eval.vector.safety,
            "task_velocity": eval.vector.task_velocity,
            "issue_health": eval.vector.issue_health,
            "diagnostics_repair_pressure": eval.diagnostics_repair_pressure,
            "objectives": format!("{}/{}", eval.objectives_completed, eval.objectives_total),
            "tasks": format!("{}/{}", eval.completed_tasks, eval.total_tasks),
        },
        "fingerprint_drift": drift,
        "per_crate": per_crate,
    })
}

fn compute_and_persist_fingerprint_drift(
    workspace: &Path,
    current_summaries: &[crate::semantic::SymbolSummary],
) -> Result<crate::drift_analysis::FingerprintDrift> {
    let dir = reports_dir(workspace);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let snapshot = dir.join("fingerprint_snapshot.json");
    let prev_summaries: Vec<crate::semantic::SymbolSummary> = fs::read_to_string(&snapshot)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    let drift =
        crate::drift_analysis::compute_fingerprint_drift(workspace, &prev_summaries, current_summaries);
    let body = serde_json::to_string_pretty(current_summaries)?;
    fs::write(&snapshot, body).with_context(|| format!("write {}", snapshot.display()))?;

    let _ = crate::logging::record_effect_for_workspace(
        workspace,
        crate::events::EffectEvent::FingerprintDriftRecorded {
            drift: drift.clone(),
        },
    );

    Ok(drift)
}

fn complexity_intra_scoring() -> serde_json::Value {
    json!({
        "objective_score": "0.6·B_norm + 0.4·R_norm  ∈ [0,1]  (higher = higher-value target)",
        "B_norm": "branch_score / max_branch_score  (terminator-weighted: SwitchInt×2+Call×1+Assert×0.5)",
        "R_norm": "stmt_density / max_stmt_density  (redundancy proxy: dense logic per branch)",
        "stmt_density": "mir_stmts / mir_blocks"
    })
}

fn complexity_inter_scoring() -> serde_json::Value {
    json!({
        "inter_objective": "0.30·B_transitive_norm + 0.20·R_body + 0.20·(1−D_det) + 0.30·heat_norm",
        "branch_score": "SwitchInt×2.0 + Call×1.0 + Assert×0.5 over non-cleanup MIR blocks",
        "B_transitive": "branch_score(F) + mean(branch_score(callee)) — depth-1 propagation",
        "R_body": "1.0 if MIR fingerprint+signature+callees match another function (semantic duplicate)",
        "D_det": "1.0 − branch_score_norm  (determinism proxy)",
        "heat_score": "branch_score × ln(call_in + 1) — complexity weighted by call frequency"
    })
}

fn persist_complexity_report(dir: &Path, report: &serde_json::Value) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let body = serde_json::to_string_pretty(report)?;
    let ts = crate::logging::now_ms();
    let path = dir.join(format!("{ts}.json"));
    std::fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    let latest = dir.join("latest.json");
    std::fs::write(&latest, body).with_context(|| format!("write {}", latest.display()))?;
    Ok(latest)
}

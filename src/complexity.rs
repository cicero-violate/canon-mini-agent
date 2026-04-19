use anyhow::{Context, Result};
use serde_json::json;
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
/// Implements: objective = min(B) + min(R)  s.t. correctness invariant
///   B_norm  = mir_blocks / max_mir_blocks          (branching proxy, weight 0.6)
///   R_norm  = stmt_density / max_stmt_density      (redundancy proxy, weight 0.4)
///   stmt_density = mir_stmts / max(mir_blocks, 1)  (dense logic per branch → redundancy signal)
///   objective_score = 0.6 * B_norm + 0.4 * R_norm
///
/// Higher score = higher-value reduction target.
fn apply_objective_scores(items: &mut Vec<serde_json::Value>) {
    let max_blocks = items
        .iter()
        .filter_map(|v| v.get("mir_blocks").and_then(|x| x.as_f64()))
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
        let blocks = item
            .get("mir_blocks")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let stmts = item
            .get("mir_stmts")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let b_norm = if max_blocks > 0.0 {
            blocks / max_blocks
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

    let mut items = collect_complexity_items(&idx, crate_name, global);

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
) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    for s in idx.symbol_summaries() {
        let blocks = s.mir_blocks.unwrap_or(0);
        let stmts = s.mir_stmts.unwrap_or(0);
        if blocks == 0 && stmts == 0 {
            continue;
        }
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
        "mir_blocks": blocks,
        "mir_stmts": stmts,
        "complexity_proxy": blocks,
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

    for crate_name in crates {
        let entry = process_crate(workspace, &crate_name, &mut global);
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

    let report = build_complexity_report(per_crate, global_top, inter_sections);

    // Auto-generate issues for top hotspots (Detect → Propose step)
    let _ = crate::inter_complexity::generate_hotspot_issues(workspace, 5);
    // Auto-generate structural refactor issues (dead code, branch reduction, helper extraction, call chains)
    let _ = crate::refactor_analysis::generate_all_refactor_issues(workspace);
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
        "per_crate": per_crate,
    })
}

fn complexity_intra_scoring() -> serde_json::Value {
    json!({
        "objective_score": "0.6·B_norm + 0.4·R_norm  ∈ [0,1]  (higher = higher-value target)",
        "B_norm": "mir_blocks / max_mir_blocks  (branching proxy)",
        "R_norm": "stmt_density / max_stmt_density  (redundancy proxy: dense logic per branch)",
        "stmt_density": "mir_stmts / mir_blocks"
    })
}

fn complexity_inter_scoring() -> serde_json::Value {
    json!({
        "inter_objective": "0.40·B_transitive_norm + 0.30·R_body + 0.30·(1−D_det)",
        "B_transitive": "B(F) + mean(B(callee)) — depth-1 branching propagation",
        "R_body": "1.0 if MIR fingerprint matches another function (exact duplicate)",
        "D_det": "1.0 − B_norm  (determinism proxy)"
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

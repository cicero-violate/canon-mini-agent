use anyhow::{Context, Result};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::semantic::{shorten_display_path, SemanticIndex};

fn reports_dir(workspace: &Path) -> PathBuf {
    workspace
        .join("agent_state")
        .join("reports")
        .join("complexity")
}

fn graph_only_report_path(workspace: &Path) -> PathBuf {
    reports_dir(workspace).join("graph_only_latest.json")
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

#[derive(Debug, Clone)]
struct GraphOnlyEntry {
    crate_name: String,
    symbol: String,
    file: String,
    line: u32,
    mir_blocks: usize,
    mir_stmts: usize,
    branch_score: f64,
    stmt_density: f64,
    b_transitive: f64,
    heat_score: f64,
    call_in: usize,
    call_out: usize,
    duplicate_body_count: usize,
    redundant_path_count: usize,
    pathway_membership_count: usize,
    pathway_wrapper_count: usize,
    scc_size: usize,
    is_directly_recursive: bool,
    graph_complexity_score: f64,
}

fn graph_only_entry_json(entry: &GraphOnlyEntry) -> serde_json::Value {
    json!({
        "crate": entry.crate_name,
        "symbol": entry.symbol,
        "file": shorten_display_path(&entry.file),
        "line": entry.line,
        "mir_blocks": entry.mir_blocks,
        "mir_stmts": entry.mir_stmts,
        "branch_score": format!("{:.2}", entry.branch_score),
        "stmt_density": format!("{:.2}", entry.stmt_density),
        "b_transitive": format!("{:.2}", entry.b_transitive),
        "heat_score": format!("{:.2}", entry.heat_score),
        "call_in": entry.call_in,
        "call_out": entry.call_out,
        "duplicate_body_count": entry.duplicate_body_count,
        "redundant_path_count": entry.redundant_path_count,
        "pathway_membership_count": entry.pathway_membership_count,
        "pathway_wrapper_count": entry.pathway_wrapper_count,
        "scc_size": entry.scc_size,
        "is_directly_recursive": entry.is_directly_recursive,
        "graph_complexity_score": format!("{:.3}", entry.graph_complexity_score),
    })
}

fn graph_only_sort_desc(a: &GraphOnlyEntry, b: &GraphOnlyEntry) -> std::cmp::Ordering {
    b.graph_complexity_score
        .partial_cmp(&a.graph_complexity_score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(b.branch_score.partial_cmp(&a.branch_score).unwrap_or(std::cmp::Ordering::Equal))
        .then(a.symbol.cmp(&b.symbol))
}

fn normalize_by_max(value: f64, max_value: f64) -> f64 {
    if max_value <= 0.0 {
        0.0
    } else {
        (value / max_value).clamp(0.0, 1.0)
    }
}

fn build_graph_only_entries(
    idx: &SemanticIndex,
    crate_name: &str,
) -> Vec<GraphOnlyEntry> {
    let summaries = idx.symbol_summaries();
    let call_edges = idx.call_edges();
    let redundant_pairs = idx.redundant_path_pairs();
    let alpha_pathways = idx.alpha_pathways();

    let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
    for (from, to) in &call_edges {
        outgoing.entry(from.clone()).or_default().push(to.clone());
    }

    let mut fingerprint_groups: HashMap<String, Vec<String>> = HashMap::new();
    for summary in &summaries {
        if let Some(fp) = &summary.mir_fingerprint {
            fingerprint_groups
                .entry(fp.clone())
                .or_default()
                .push(summary.symbol.clone());
        }
    }

    let mut duplicate_body_count: HashMap<String, usize> = HashMap::new();
    for group in fingerprint_groups.values() {
        if group.len() < 2 {
            continue;
        }
        for symbol in group {
            duplicate_body_count.insert(symbol.clone(), group.len() - 1);
        }
    }

    let mut redundant_path_count: HashMap<String, usize> = HashMap::new();
    for pair in &redundant_pairs {
        *redundant_path_count
            .entry(pair.path_a.owner.clone())
            .or_insert(0) += 1;
    }

    let mut pathway_membership_count: HashMap<String, usize> = HashMap::new();
    let mut pathway_wrapper_count: HashMap<String, usize> = HashMap::new();
    for pathway in alpha_pathways {
        for symbol in &pathway.chain {
            *pathway_membership_count.entry(symbol.clone()).or_insert(0) += 1;
        }
        for symbol in pathway.chain.iter().take(pathway.chain.len().saturating_sub(1)) {
            *pathway_wrapper_count.entry(symbol.clone()).or_insert(0) += 1;
        }
    }

    let branch_by_symbol: HashMap<String, f64> = summaries
        .iter()
        .map(|s| {
            (
                s.symbol.clone(),
                s.branch_score.unwrap_or(s.mir_blocks.unwrap_or(0) as f64),
            )
        })
        .collect();

    let scc_size = compute_call_scc_sizes(&summaries, &outgoing);

    let mut entries = Vec::new();
    for summary in summaries {
        let mir_blocks = summary.mir_blocks.unwrap_or(0);
        let mir_stmts = summary.mir_stmts.unwrap_or(0);
        if mir_blocks == 0 && mir_stmts == 0 {
            continue;
        }
        let branch_score = branch_by_symbol
            .get(&summary.symbol)
            .copied()
            .unwrap_or(0.0);
        let stmt_density = if mir_blocks > 0 {
            mir_stmts as f64 / mir_blocks as f64
        } else {
            0.0
        };
        let callee_mean = outgoing
            .get(&summary.symbol)
            .map(|callees| {
                let mut total = 0.0;
                let mut count = 0usize;
                for callee in callees {
                    if let Some(score) = branch_by_symbol.get(callee) {
                        total += *score;
                        count += 1;
                    }
                }
                if count == 0 {
                    0.0
                } else {
                    total / count as f64
                }
            })
            .unwrap_or(0.0);
        let b_transitive = branch_score + callee_mean;
        let heat_score = branch_score * ((summary.call_in as f64 + 1.0).ln());

        entries.push(GraphOnlyEntry {
            crate_name: crate_name.to_string(),
            symbol: summary.symbol.clone(),
            file: summary.file.clone(),
            line: summary.line,
            mir_blocks,
            mir_stmts,
            branch_score,
            stmt_density,
            b_transitive,
            heat_score,
            call_in: summary.call_in,
            call_out: summary.call_out,
            duplicate_body_count: *duplicate_body_count.get(&summary.symbol).unwrap_or(&0),
            redundant_path_count: *redundant_path_count.get(&summary.symbol).unwrap_or(&0),
            pathway_membership_count: *pathway_membership_count.get(&summary.symbol).unwrap_or(&0),
            pathway_wrapper_count: *pathway_wrapper_count.get(&summary.symbol).unwrap_or(&0),
            scc_size: *scc_size.get(&summary.symbol).unwrap_or(&1),
            is_directly_recursive: summary.is_directly_recursive,
            graph_complexity_score: 0.0,
        });
    }

    let max_branch = entries.iter().map(|e| e.branch_score).fold(0.0_f64, f64::max);
    let max_density = entries.iter().map(|e| e.stmt_density).fold(0.0_f64, f64::max);
    let max_transitive = entries.iter().map(|e| e.b_transitive).fold(0.0_f64, f64::max);
    let max_heat = entries.iter().map(|e| e.heat_score).fold(0.0_f64, f64::max);
    let max_dup = entries
        .iter()
        .map(|e| e.duplicate_body_count as f64)
        .fold(0.0_f64, f64::max);
    let max_redundant = entries
        .iter()
        .map(|e| e.redundant_path_count as f64)
        .fold(0.0_f64, f64::max);
    let max_pathway = entries
        .iter()
        .map(|e| (e.pathway_membership_count + e.pathway_wrapper_count) as f64)
        .fold(0.0_f64, f64::max);
    let max_scc = entries.iter().map(|e| e.scc_size as f64).fold(0.0_f64, f64::max);

    for entry in &mut entries {
        let branch_norm = normalize_by_max(entry.branch_score, max_branch);
        let density_norm = normalize_by_max(entry.stmt_density, max_density);
        let transitive_norm = normalize_by_max(entry.b_transitive, max_transitive);
        let heat_norm = normalize_by_max(entry.heat_score, max_heat);
        let duplicate_norm = normalize_by_max(entry.duplicate_body_count as f64, max_dup);
        let redundant_norm = normalize_by_max(entry.redundant_path_count as f64, max_redundant);
        let pathway_norm = normalize_by_max(
            (entry.pathway_membership_count + entry.pathway_wrapper_count) as f64,
            max_pathway,
        );
        let loop_norm = normalize_by_max(entry.scc_size as f64, max_scc);
        entry.graph_complexity_score = (
            0.25 * branch_norm
                + 0.15 * density_norm
                + 0.20 * transitive_norm
                + 0.15 * heat_norm
                + 0.10 * duplicate_norm
                + 0.05 * redundant_norm
                + 0.05 * pathway_norm
                + 0.05 * loop_norm
        )
        .clamp(0.0, 1.0);
    }

    entries.sort_by(graph_only_sort_desc);
    entries
}

fn compute_call_scc_sizes(
    summaries: &[crate::semantic::SymbolSummary],
    outgoing: &HashMap<String, Vec<String>>,
) -> HashMap<String, usize> {
    fn dfs(
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        seen: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) {
        if !seen.insert(node.to_string()) {
            return;
        }
        if let Some(nexts) = graph.get(node) {
            for next in nexts {
                dfs(next, graph, seen, order);
            }
        }
        order.push(node.to_string());
    }

    fn dfs_collect(
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        seen: &mut HashSet<String>,
        component: &mut Vec<String>,
    ) {
        if !seen.insert(node.to_string()) {
            return;
        }
        component.push(node.to_string());
        if let Some(nexts) = graph.get(node) {
            for next in nexts {
                dfs_collect(next, graph, seen, component);
            }
        }
    }

    let nodes: Vec<String> = summaries.iter().map(|s| s.symbol.clone()).collect();
    let node_set: HashSet<String> = nodes.iter().cloned().collect();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for (from, tos) in outgoing {
        for to in tos {
            if node_set.contains(to) {
                reverse.entry(to.clone()).or_default().push(from.clone());
            }
        }
    }

    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for node in &nodes {
        dfs(node, outgoing, &mut seen, &mut order);
    }

    let mut component_sizes = HashMap::new();
    let mut reverse_seen = HashSet::new();
    for node in order.into_iter().rev() {
        if reverse_seen.contains(&node) {
            continue;
        }
        let mut component = Vec::new();
        dfs_collect(&node, &reverse, &mut reverse_seen, &mut component);
        let size = component.len();
        for member in component {
            component_sizes.insert(member, size);
        }
    }
    component_sizes
}

pub fn build_graph_only_complexity_report(workspace: &Path) -> Result<serde_json::Value> {
    let crates = SemanticIndex::available_crates(workspace);
    if crates.is_empty() {
        return Ok(json!({
            "version": 1,
            "kind": "graph_only_complexity",
            "crates": [],
            "global_top": [],
        }));
    }

    let mut per_crate = Vec::new();
    let mut global_entries = Vec::new();

    for crate_name in crates {
        let idx = match SemanticIndex::load(workspace, &crate_name) {
            Ok(idx) => idx,
            Err(err) => {
                per_crate.push(json!({
                    "crate": crate_name,
                    "status": "error",
                    "error": err.to_string(),
                }));
                continue;
            }
        };

        let entries = build_graph_only_entries(&idx, &crate_name);
        let crate_score = if entries.is_empty() {
            0.0
        } else {
            entries
                .iter()
                .map(|e| e.graph_complexity_score)
                .sum::<f64>()
                / entries.len() as f64
        };
        global_entries.extend(entries.iter().cloned());
        per_crate.push(json!({
            "crate": crate_name,
            "status": "ok",
            "graph_entropy_score": format!("{:.3}", crate_score),
            "top": entries.iter().take(50).map(graph_only_entry_json).collect::<Vec<_>>(),
        }));
    }

    global_entries.sort_by(graph_only_sort_desc);
    let overall_score = if global_entries.is_empty() {
        0.0
    } else {
        global_entries
            .iter()
            .map(|e| e.graph_complexity_score)
            .sum::<f64>()
            / global_entries.len() as f64
    };

    Ok(json!({
        "version": 1,
        "kind": "graph_only_complexity",
        "generated_at_ms": crate::logging::now_ms(),
        "objective": "min structural control entropy using graph.json only",
        "scoring": {
            "graph_complexity_score": "0.25·B_norm + 0.15·stmt_density_norm + 0.20·B_transitive_norm + 0.15·heat_norm + 0.10·duplicate_norm + 0.05·redundant_path_norm + 0.05·pathway_norm + 0.05·loop_norm",
            "B_norm": "terminator-weighted local branch score normalized by crate max",
            "B_transitive_norm": "local branch score plus mean direct callee branch score, normalized",
            "heat_norm": "branch_score × ln(call_in + 1), normalized",
            "duplicate_norm": "MIR fingerprint sibling count normalized",
            "redundant_path_norm": "duplicate CFG path pair count normalized",
            "pathway_norm": "alpha-pathway chain participation normalized",
            "loop_norm": "call-graph SCC size normalized",
        },
        "overall_graph_entropy_score": format!("{:.3}", overall_score),
        "global_top": global_entries.iter().take(100).map(graph_only_entry_json).collect::<Vec<_>>(),
        "crates": per_crate,
    }))
}

pub fn write_graph_only_complexity_report(workspace: &Path) -> Result<PathBuf> {
    let report = build_graph_only_complexity_report(workspace)?;
    let dir = reports_dir(workspace);
    fs::create_dir_all(&dir)?;
    let path = graph_only_report_path(workspace);
    let bytes = serde_json::to_vec_pretty(&report)?;
    fs::write(&path, bytes)?;
    Ok(path)
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
        if let Some(entry) = collect_complexity_item(crate_name, global, all_summaries, s) {
            items.push(entry);
        }
    }
    items
}

fn collect_complexity_item(
    crate_name: &str,
    global: &mut Vec<serde_json::Value>,
    all_summaries: &mut Vec<crate::semantic::SymbolSummary>,
    s: crate::semantic::SymbolSummary,
) -> Option<serde_json::Value> {
    let blocks = s.mir_blocks.unwrap_or(0);
    let stmts = s.mir_stmts.unwrap_or(0);
    if blocks == 0 && stmts == 0 {
        return None;
    }
    all_summaries.push(s.clone());
    let entry = build_complexity_entry(&s, blocks, stmts);
    global.push(build_global_complexity_entry(crate_name, &entry));
    Some(entry)
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

    enqueue_issue_task_generation(workspace);

    let eval = crate::evaluation::evaluate_workspace(workspace);
    let drift = compute_and_persist_fingerprint_drift(workspace, &current_summaries)?;
    let report = build_complexity_report(per_crate, global_top, inter_sections, &eval, &drift);

    enqueue_grpo_extraction(workspace);

    let dir = reports_dir(workspace);
    let latest = persist_complexity_report(&dir, &report)?;

    Ok(Some(latest))
}

/// Regenerate issue artifacts without starting the supervisor loop.
///
/// This runs the same issue-generation batch that the supervisor triggers from
/// complexity-report startup, but avoids websocket/orchestration side effects.
pub fn refresh_issue_artifacts(workspace: &Path) -> Result<()> {
    generate_graph_and_hotspot_issues(workspace);
    generate_refactor_issue_batch(workspace);
    generate_invariant_lifecycle_issues(workspace);
    Ok(())
}

fn in_flight_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

fn enqueue_issue_task_generation(workspace: &Path) {
    let ws = workspace.to_path_buf();
    {
        let mut guard = match in_flight_paths().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !guard.insert(ws.clone()) {
            return;
        }
    }
    std::thread::spawn(move || {
        let _ = refresh_issue_artifacts(&ws);
        if let Ok(mut guard) = in_flight_paths().lock() {
            guard.remove(&ws);
        }
    });
}

fn enqueue_grpo_extraction(workspace: &Path) {
    let ws = workspace.to_path_buf();
    std::thread::spawn(move || {
        let tlog_path = ws.join("agent_state").join("tlog.ndjson");
        if let Ok(dataset) = crate::grpo::extract_grpo_dataset(&ws, &tlog_path) {
            let _ = crate::grpo::record_grpo_dataset_effect(&ws, &dataset, None);
        }
    });
}

fn generate_graph_and_hotspot_issues(workspace: &Path) {
    // Bridge-connectivity analysis: emit deterministic graph-overconnectivity issues.
    let _ = crate::graph_metrics::generate_bridge_connectivity_issues(workspace);

    // Auto-generate issues for top hotspots (Detect → Propose step)
    let _ = crate::inter_complexity::generate_hotspot_issues(workspace, 5);
}

fn generate_refactor_issue_batch(workspace: &Path) {
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
    let _ = crate::refactor_analysis::generate_rename_symbol_issues(workspace);
    let _ = crate::refactor_analysis::generate_dark_assignment_issues(workspace);
    let _ = crate::refactor_analysis::generate_loop_invariant_issues(workspace);
    let _ = crate::refactor_analysis::generate_redundant_path_issues(workspace);
    let _ = crate::refactor_analysis::generate_alpha_pathway_issues(workspace);
    let _ = crate::graph_metrics::generate_module_cohesion_issues(workspace);
    let _ = crate::graph_metrics::generate_artifact_writer_dispersion_issues(workspace);
}

fn generate_invariant_lifecycle_issues(workspace: &Path) {
    // Auto-generate invariant lifecycle issues (action surface gap, prompt injection gap, per-promoted gates)
    let _ = crate::invariants::generate_invariant_issues(workspace);
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

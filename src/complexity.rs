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

fn graph_verification_snapshot_latest_path(workspace: &Path) -> PathBuf {
    reports_dir(workspace).join("graph_verification_snapshot_latest.json")
}

fn graph_delta_report_latest_path(workspace: &Path) -> PathBuf {
    reports_dir(workspace).join("graph_delta_latest.json")
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
        "metrics": graph_only_entry_metrics_json(entry),
    })
}

fn graph_only_entry_metrics_json(entry: &GraphOnlyEntry) -> serde_json::Value {
    let mut metrics = serde_json::Map::new();
    insert_graph_only_size_metrics(entry, &mut metrics);
    insert_graph_only_topology_metrics(entry, &mut metrics);
    serde_json::Value::Object(metrics)
}

fn insert_graph_only_size_metrics(
    entry: &GraphOnlyEntry,
    metrics: &mut serde_json::Map<String, serde_json::Value>,
) {
    insert_graph_only_metric(metrics, "mir_blocks", graph_only_mir_blocks(entry));
    insert_graph_only_metric(metrics, "mir_stmts", graph_only_mir_stmts(entry));
    insert_graph_only_metric(metrics, "branch_score", graph_only_branch_score(entry));
    insert_graph_only_metric(metrics, "stmt_density", graph_only_stmt_density(entry));
    insert_graph_only_metric(metrics, "b_transitive", graph_only_b_transitive(entry));
    insert_graph_only_metric(metrics, "heat_score", graph_only_heat_score(entry));
}

fn insert_graph_only_metric(
    metrics: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: impl serde::Serialize,
) {
    metrics.insert(key.to_string(), json!(value));
}

fn insert_graph_only_topology_metrics(
    entry: &GraphOnlyEntry,
    metrics: &mut serde_json::Map<String, serde_json::Value>,
) {
    insert_graph_only_call_metrics(entry, metrics);
    insert_graph_only_path_metrics(entry, metrics);
    insert_graph_only_scc_metrics(entry, metrics);
}

fn insert_graph_only_call_metrics(
    entry: &GraphOnlyEntry,
    metrics: &mut serde_json::Map<String, serde_json::Value>,
) {
    metrics.insert("call_in".to_string(), json!(graph_only_call_in(entry)));
    metrics.insert("call_out".to_string(), json!(graph_only_call_out(entry)));
    metrics.insert(
        "duplicate_body_count".to_string(),
        json!(graph_only_duplicate_body_count(entry)),
    );
}

fn insert_graph_only_path_metrics(
    entry: &GraphOnlyEntry,
    metrics: &mut serde_json::Map<String, serde_json::Value>,
) {
    metrics.insert(
        "redundant_path_count".to_string(),
        json!(graph_only_redundant_path_count(entry)),
    );
    metrics.insert(
        "pathway_membership_count".to_string(),
        json!(graph_only_pathway_membership_count(entry)),
    );
    metrics.insert(
        "pathway_wrapper_count".to_string(),
        json!(graph_only_pathway_wrapper_count(entry)),
    );
}

fn insert_graph_only_scc_metrics(
    entry: &GraphOnlyEntry,
    metrics: &mut serde_json::Map<String, serde_json::Value>,
) {
    metrics.insert("scc_size".to_string(), json!(graph_only_scc_size(entry)));
    metrics.insert(
        "is_directly_recursive".to_string(),
        json!(graph_only_is_directly_recursive(entry)),
    );
    metrics.insert(
        "graph_complexity_score".to_string(),
        json!(graph_only_complexity_score(entry)),
    );
}

fn graph_only_mir_blocks(entry: &GraphOnlyEntry) -> usize { entry.mir_blocks }
fn graph_only_mir_stmts(entry: &GraphOnlyEntry) -> usize { entry.mir_stmts }
fn graph_only_branch_score(entry: &GraphOnlyEntry) -> String { format_graph_score_2(entry.branch_score) }
fn graph_only_stmt_density(entry: &GraphOnlyEntry) -> String { format_graph_score_2(entry.stmt_density) }
fn graph_only_b_transitive(entry: &GraphOnlyEntry) -> String { format_graph_score_2(entry.b_transitive) }
fn graph_only_heat_score(entry: &GraphOnlyEntry) -> String { format_graph_score_2(entry.heat_score) }
fn graph_only_call_in(entry: &GraphOnlyEntry) -> usize { entry.call_in }
fn graph_only_call_out(entry: &GraphOnlyEntry) -> usize { entry.call_out }
fn graph_only_duplicate_body_count(entry: &GraphOnlyEntry) -> usize { entry.duplicate_body_count }
fn graph_only_redundant_path_count(entry: &GraphOnlyEntry) -> usize { entry.redundant_path_count }
fn graph_only_pathway_membership_count(entry: &GraphOnlyEntry) -> usize { entry.pathway_membership_count }
fn graph_only_pathway_wrapper_count(entry: &GraphOnlyEntry) -> usize { entry.pathway_wrapper_count }
fn graph_only_scc_size(entry: &GraphOnlyEntry) -> usize { entry.scc_size }
fn graph_only_is_directly_recursive(entry: &GraphOnlyEntry) -> bool { entry.is_directly_recursive }
fn graph_only_complexity_score(entry: &GraphOnlyEntry) -> String { format_graph_score_3(entry.graph_complexity_score) }

/// Intent: pure_transform
/// Resource: error
/// Inputs: f64
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn format_graph_score_2(value: f64) -> String {
    format!("{value:.2}")
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: f64
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn format_graph_score_3(value: f64) -> String {
    format!("{value:.3}")
}

fn graph_only_sort_desc(a: &GraphOnlyEntry, b: &GraphOnlyEntry) -> std::cmp::Ordering {
    b.graph_complexity_score
        .partial_cmp(&a.graph_complexity_score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(
            b.branch_score
                .partial_cmp(&a.branch_score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
        .then(a.symbol.cmp(&b.symbol))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: f64, f64
/// Outputs: f64
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn normalize_by_max(value: f64, max_value: f64) -> f64 {
    if max_value <= 0.0 {
        0.0
    } else {
        (value / max_value).clamp(0.0, 1.0)
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &semantic::SemanticIndex, &str
/// Outputs: std::vec::Vec<complexity::GraphOnlyEntry>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_graph_only_entries(idx: &SemanticIndex, crate_name: &str) -> Vec<GraphOnlyEntry> {
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

    let duplicate_body_count = duplicate_body_counts(&fingerprint_groups);

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
        for symbol in pathway
            .chain
            .iter()
            .take(pathway.chain.len().saturating_sub(1))
        {
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

    apply_graph_only_complexity_scores(&mut entries);

    entries.sort_by(graph_only_sort_desc);
    entries
}

fn apply_graph_only_complexity_scores(entries: &mut [GraphOnlyEntry]) {
    let max = graph_only_normalization_maxima(entries);

    for entry in entries {
        let branch_norm = normalize_by_max(entry.branch_score, max.branch);
        let density_norm = normalize_by_max(entry.stmt_density, max.density);
        let transitive_norm = normalize_by_max(entry.b_transitive, max.transitive);
        let heat_norm = normalize_by_max(entry.heat_score, max.heat);
        let duplicate_norm = normalize_by_max(entry.duplicate_body_count as f64, max.duplicate);
        let redundant_norm = normalize_by_max(entry.redundant_path_count as f64, max.redundant);
        let pathway_norm = normalize_by_max(
            (entry.pathway_membership_count + entry.pathway_wrapper_count) as f64,
            max.pathway,
        );
        let loop_norm = normalize_by_max(entry.scc_size as f64, max.scc);
        entry.graph_complexity_score = (0.25 * branch_norm
            + 0.15 * density_norm
            + 0.20 * transitive_norm
            + 0.15 * heat_norm
            + 0.10 * duplicate_norm
            + 0.05 * redundant_norm
            + 0.05 * pathway_norm
            + 0.05 * loop_norm)
            .clamp(0.0, 1.0);
    }
}

fn duplicate_body_counts(
    fingerprint_groups: &HashMap<String, Vec<String>>,
) -> HashMap<String, usize> {
    let mut duplicate_body_count = HashMap::new();
    for group in fingerprint_groups.values().filter(|group| group.len() >= 2) {
        for symbol in group {
            duplicate_body_count.insert(symbol.clone(), group.len() - 1);
        }
    }
    duplicate_body_count
}

struct GraphOnlyNormalizationMaxima {
    branch: f64,
    density: f64,
    transitive: f64,
    heat: f64,
    duplicate: f64,
    redundant: f64,
    pathway: f64,
    scc: f64,
}

fn graph_only_normalization_maxima(entries: &[GraphOnlyEntry]) -> GraphOnlyNormalizationMaxima {
    GraphOnlyNormalizationMaxima {
        branch: entries
            .iter()
            .map(|e| e.branch_score)
            .fold(0.0_f64, f64::max),
        density: entries
            .iter()
            .map(|e| e.stmt_density)
            .fold(0.0_f64, f64::max),
        transitive: entries
            .iter()
            .map(|e| e.b_transitive)
            .fold(0.0_f64, f64::max),
        heat: entries.iter().map(|e| e.heat_score).fold(0.0_f64, f64::max),
        duplicate: entries
            .iter()
            .map(|e| e.duplicate_body_count as f64)
            .fold(0.0_f64, f64::max),
        redundant: entries
            .iter()
            .map(|e| e.redundant_path_count as f64)
            .fold(0.0_f64, f64::max),
        pathway: entries
            .iter()
            .map(|e| (e.pathway_membership_count + e.pathway_wrapper_count) as f64)
            .fold(0.0_f64, f64::max),
        scc: entries
            .iter()
            .map(|e| e.scc_size as f64)
            .fold(0.0_f64, f64::max),
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &[semantic::SymbolSummary], &std::collections::HashMap<std::string::String, std::vec::Vec<std::string::String>>
/// Outputs: std::collections::HashMap<std::string::String, usize>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
        let mut stack = vec![(node.to_string(), false)];
        while let Some((current, expanded)) = stack.pop() {
            if expanded {
                order.push(current);
                continue;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            stack.push((current.clone(), true));
            if let Some(nexts) = graph.get(&current) {
                for next in nexts.iter().rev() {
                    stack.push((next.clone(), false));
                }
            }
        }
    }

    fn dfs_collect(
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        seen: &mut HashSet<String>,
        component: &mut Vec<String>,
    ) {
        let mut stack = vec![node.to_string()];
        while let Some(current) = stack.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            component.push(current.clone());
            if let Some(nexts) = graph.get(&current) {
                for next in nexts.iter().rev() {
                    stack.push(next.clone());
                }
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

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<serde_json::Value, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::path::PathBuf, anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn write_graph_only_complexity_report(workspace: &Path) -> Result<PathBuf> {
    let report = build_graph_only_complexity_report(workspace)?;
    let dir = reports_dir(workspace);
    fs::create_dir_all(&dir)?;
    let path = graph_only_report_path(workspace);
    let bytes = serde_json::to_vec_pretty(&report)?;
    fs::write(&path, bytes)?;
    Ok(path)
}

fn count_artifact_writer_dispersion(idx: &SemanticIndex) -> usize {
    let mut writers_by_artifact: HashMap<String, HashSet<String>> = HashMap::new();
    for (writer, artifact) in idx.artifact_write_edges() {
        if artifact.trim().is_empty() {
            continue;
        }
        writers_by_artifact
            .entry(artifact)
            .or_default()
            .insert(writer);
    }
    writers_by_artifact
        .into_values()
        .filter(|writers| writers.len() > 1)
        .count()
}

fn count_error_shaping_dispersion(idx: &SemanticIndex) -> usize {
    let mut functions = HashSet::new();
    for (symbol, style) in idx.semantic_edges_by_relation("ShapesError") {
        if !style.trim().is_empty() {
            functions.insert(symbol);
        }
    }
    usize::from(functions.len() >= 4)
}

fn count_state_transition_dispersion(idx: &SemanticIndex) -> usize {
    let mut transitions_by_state: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, state) in idx.state_transition_edges() {
        if state.trim().is_empty() {
            continue;
        }
        transitions_by_state
            .entry(state)
            .or_default()
            .insert(symbol);
    }
    transitions_by_state
        .into_values()
        .filter(|transitions| transitions.len() > 1)
        .count()
}

fn count_representation_fanout(idx: &SemanticIndex) -> usize {
    let mut sources_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
    let mut targets_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, artifact) in idx.artifact_read_edges() {
        sources_by_symbol
            .entry(symbol)
            .or_default()
            .insert(format!("artifact::{artifact}"));
    }
    for (symbol, state) in idx.state_read_edges() {
        sources_by_symbol
            .entry(symbol)
            .or_default()
            .insert(format!("state::{state}"));
    }
    for (symbol, artifact) in idx.artifact_write_edges() {
        targets_by_symbol
            .entry(symbol)
            .or_default()
            .insert(format!("artifact::{artifact}"));
    }
    for (symbol, state) in idx.state_write_edges() {
        targets_by_symbol
            .entry(symbol)
            .or_default()
            .insert(format!("state::{state}"));
    }

    let mut symbols_by_pair: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for (symbol, sources) in sources_by_symbol {
        let Some(targets) = targets_by_symbol.get(&symbol) else {
            continue;
        };
        for source in &sources {
            for target in targets {
                if source == target {
                    continue;
                }
                symbols_by_pair
                    .entry((source.clone(), target.clone()))
                    .or_default()
                    .insert(symbol.clone());
            }
        }
    }

    symbols_by_pair
        .into_values()
        .filter(|symbols| symbols.len() > 1)
        .count()
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<serde_json::Value, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn build_graph_verification_snapshot(workspace: &Path) -> Result<serde_json::Value> {
    let crates = SemanticIndex::available_crates(workspace);
    let mut per_crate = Vec::new();
    let mut global_entries = Vec::new();
    let mut totals = serde_json::Map::new();
    let mut total_pathways = 0usize;
    let mut total_redundant_paths = 0usize;
    let mut total_artifact_writer_dispersion = 0usize;
    let mut total_error_shaping_dispersion = 0usize;
    let mut total_state_transition_dispersion = 0usize;
    let mut total_representation_fanout = 0usize;

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
        let crate_entropy = if entries.is_empty() {
            0.0
        } else {
            entries
                .iter()
                .map(|e| e.graph_complexity_score)
                .sum::<f64>()
                / entries.len() as f64
        };
        let pathway_count = idx.alpha_pathways().len();
        let redundant_path_count = idx.redundant_path_pairs().len();
        let artifact_writer_dispersion_count = count_artifact_writer_dispersion(&idx);
        let error_shaping_dispersion_count = count_error_shaping_dispersion(&idx);
        let state_transition_dispersion_count = count_state_transition_dispersion(&idx);
        let representation_fanout_count = count_representation_fanout(&idx);
        let call_edge_count = idx.call_edges().len();

        total_pathways += pathway_count;
        total_redundant_paths += redundant_path_count;
        total_artifact_writer_dispersion += artifact_writer_dispersion_count;
        total_error_shaping_dispersion += error_shaping_dispersion_count;
        total_state_transition_dispersion += state_transition_dispersion_count;
        total_representation_fanout += representation_fanout_count;
        global_entries.extend(entries.iter().cloned());

        per_crate.push(json!({
            "crate": crate_name,
            "status": "ok",
            "overall_graph_entropy_score": crate_entropy,
            "pathway_count": pathway_count,
            "redundant_path_count": redundant_path_count,
            "artifact_writer_dispersion_count": artifact_writer_dispersion_count,
            "error_shaping_dispersion_count": error_shaping_dispersion_count,
            "state_transition_dispersion_count": state_transition_dispersion_count,
            "representation_fanout_count": representation_fanout_count,
            "call_edge_count": call_edge_count,
            "top_entropy_hotspots": entries.iter().take(10).map(graph_only_entry_json).collect::<Vec<_>>(),
        }));
    }

    global_entries.sort_by(graph_only_sort_desc);
    let overall_entropy = if global_entries.is_empty() {
        0.0
    } else {
        global_entries
            .iter()
            .map(|e| e.graph_complexity_score)
            .sum::<f64>()
            / global_entries.len() as f64
    };

    totals.insert(
        "overall_graph_entropy_score".to_string(),
        json!(overall_entropy),
    );
    totals.insert("pathway_count".to_string(), json!(total_pathways));
    totals.insert(
        "redundant_path_count".to_string(),
        json!(total_redundant_paths),
    );
    totals.insert(
        "artifact_writer_dispersion_count".to_string(),
        json!(total_artifact_writer_dispersion),
    );
    totals.insert(
        "error_shaping_dispersion_count".to_string(),
        json!(total_error_shaping_dispersion),
    );
    totals.insert(
        "state_transition_dispersion_count".to_string(),
        json!(total_state_transition_dispersion),
    );
    totals.insert(
        "representation_fanout_count".to_string(),
        json!(total_representation_fanout),
    );

    Ok(json!({
        "version": 1,
        "kind": "graph_verification_snapshot",
        "generated_at_ms": crate::logging::now_ms(),
        "totals": totals,
        "global_top": global_entries.iter().take(25).map(graph_only_entry_json).collect::<Vec<_>>(),
        "crates": per_crate,
    }))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<std::path::PathBuf, anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_graph_verification_snapshot(
    workspace: &Path,
    snapshot: &serde_json::Value,
) -> Result<PathBuf> {
    let dir = reports_dir(workspace);
    fs::create_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(snapshot)?;
    let ts = crate::logging::now_ms();
    let path = dir.join(format!("graph_verification_snapshot_{ts}.json"));
    fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    let latest = graph_verification_snapshot_latest_path(workspace);
    fs::write(&latest, body).with_context(|| format!("write {}", latest.display()))?;
    Ok(latest)
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::path::PathBuf, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn write_graph_verification_snapshot(workspace: &Path) -> Result<PathBuf> {
    let snapshot = build_graph_verification_snapshot(workspace)?;
    persist_graph_verification_snapshot(workspace, &snapshot)
}

fn metric_delta(
    before: &serde_json::Value,
    after: &serde_json::Value,
    key: &str,
) -> serde_json::Value {
    let before_v = before.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let after_v = after.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let delta = after_v - before_v;
    json!({
        "before": before_v,
        "after": after_v,
        "delta": delta,
        "improved": delta < 0.0,
    })
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &serde_json::Value, &serde_json::Value
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn build_graph_delta_report(
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> serde_json::Value {
    let before_totals = before.get("totals").cloned().unwrap_or_else(|| json!({}));
    let after_totals = after.get("totals").cloned().unwrap_or_else(|| json!({}));

    let metrics = json!({
        "overall_graph_entropy_score": metric_delta(&before_totals, &after_totals, "overall_graph_entropy_score"),
        "pathway_count": metric_delta(&before_totals, &after_totals, "pathway_count"),
        "redundant_path_count": metric_delta(&before_totals, &after_totals, "redundant_path_count"),
        "artifact_writer_dispersion_count": metric_delta(&before_totals, &after_totals, "artifact_writer_dispersion_count"),
        "error_shaping_dispersion_count": metric_delta(&before_totals, &after_totals, "error_shaping_dispersion_count"),
        "state_transition_dispersion_count": metric_delta(&before_totals, &after_totals, "state_transition_dispersion_count"),
        "representation_fanout_count": metric_delta(&before_totals, &after_totals, "representation_fanout_count"),
    });

    let keys = [
        "overall_graph_entropy_score",
        "pathway_count",
        "redundant_path_count",
        "artifact_writer_dispersion_count",
        "error_shaping_dispersion_count",
        "state_transition_dispersion_count",
        "representation_fanout_count",
    ];
    let improved_metrics = keys
        .iter()
        .filter(|key| metrics[*key]["improved"].as_bool().unwrap_or(false))
        .copied()
        .collect::<Vec<_>>();
    let regressed_metrics = keys
        .iter()
        .filter(|key| metrics[*key]["delta"].as_f64().unwrap_or(0.0) > 0.0)
        .copied()
        .collect::<Vec<_>>();

    let before_crates = before
        .get("crates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let after_crates = after
        .get("crates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut before_by_crate = HashMap::new();
    for crate_entry in before_crates {
        if let Some(name) = crate_entry.get("crate").and_then(|v| v.as_str()) {
            before_by_crate.insert(name.to_string(), crate_entry);
        }
    }
    let per_crate = after_crates
        .into_iter()
        .filter_map(|after_entry| {
            let name = after_entry.get("crate").and_then(|v| v.as_str())?.to_string();
            let before_entry = before_by_crate.get(&name).cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "crate": name,
                "overall_graph_entropy_score": metric_delta(&before_entry, &after_entry, "overall_graph_entropy_score"),
                "pathway_count": metric_delta(&before_entry, &after_entry, "pathway_count"),
                "redundant_path_count": metric_delta(&before_entry, &after_entry, "redundant_path_count"),
                "artifact_writer_dispersion_count": metric_delta(&before_entry, &after_entry, "artifact_writer_dispersion_count"),
                "error_shaping_dispersion_count": metric_delta(&before_entry, &after_entry, "error_shaping_dispersion_count"),
                "state_transition_dispersion_count": metric_delta(&before_entry, &after_entry, "state_transition_dispersion_count"),
                "representation_fanout_count": metric_delta(&before_entry, &after_entry, "representation_fanout_count"),
            }))
        })
        .collect::<Vec<_>>();

    json!({
        "version": 1,
        "kind": "graph_delta_verification",
        "generated_at_ms": crate::logging::now_ms(),
        "before_generated_at_ms": before.get("generated_at_ms").cloned().unwrap_or(serde_json::Value::Null),
        "after_generated_at_ms": after.get("generated_at_ms").cloned().unwrap_or(serde_json::Value::Null),
        "metrics": metrics,
        "improved_metrics": improved_metrics,
        "regressed_metrics": regressed_metrics,
        "verification_passed": regressed_metrics.is_empty(),
        "per_crate": per_crate,
    })
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::path::PathBuf, anyhow::Error>
/// Effects: fs_read, fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn write_graph_delta_report(workspace: &Path) -> Result<PathBuf> {
    let latest_snapshot_path = graph_verification_snapshot_latest_path(workspace);
    let previous = fs::read_to_string(&latest_snapshot_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let current = build_graph_verification_snapshot(workspace)?;
    persist_graph_verification_snapshot(workspace, &current)?;

    let report = match previous {
        Some(before) => build_graph_delta_report(&before, &current),
        None => json!({
            "version": 1,
            "kind": "graph_delta_verification",
            "generated_at_ms": crate::logging::now_ms(),
            "status": "no_baseline",
            "message": "No previous graph verification snapshot was available; current snapshot has been recorded as the new baseline.",
            "after_generated_at_ms": current.get("generated_at_ms").cloned().unwrap_or(serde_json::Value::Null),
        }),
    };

    let dir = reports_dir(workspace);
    fs::create_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(&report)?;
    let ts = crate::logging::now_ms();
    let path = dir.join(format!("graph_delta_{ts}.json"));
    fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    let latest = graph_delta_report_latest_path(workspace);
    fs::write(&latest, body).with_context(|| format!("write {}", latest.display()))?;
    Ok(latest)
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
    let idx = match load_complexity_index(workspace, crate_name) {
        Ok(idx) => idx,
        Err(error) => return graph_complexity_crate_error(crate_name, error),
    };
    let top = top_complexity_items(&idx, crate_name, global, all_summaries);

    json!({
        "crate": crate_name,
        "status": "ok",
        "metric": "objective_score(B*0.6+R*0.4)",
        "top": top,
    })
}

fn load_complexity_index(workspace: &Path, crate_name: &str) -> Result<SemanticIndex> {
    SemanticIndex::load(workspace, crate_name)
}

fn graph_complexity_crate_error(crate_name: &str, error: anyhow::Error) -> serde_json::Value {
    json!({
        "crate": crate_name,
        "status": "error",
        "error": error.to_string(),
    })
}

fn top_complexity_items(
    idx: &SemanticIndex,
    crate_name: &str,
    global: &mut Vec<serde_json::Value>,
    all_summaries: &mut Vec<crate::semantic::SymbolSummary>,
) -> Vec<serde_json::Value> {
    let mut items = collect_complexity_items(idx, crate_name, global, all_summaries);
    apply_objective_scores(&mut items);
    items.sort_by(sort_by_objective_desc);
    items.into_iter().take(50).collect()
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
    global.push(global_complexity_entry_value(crate_name, &entry));
    Some(entry)
}

fn global_complexity_entry_value(crate_name: &str, entry: &serde_json::Value) -> serde_json::Value {
    let fields = global_complexity_entry_fields(entry);
    global_complexity_fields_value(crate_name, fields)
}

fn global_complexity_fields_value(
    crate_name: &str,
    fields: GlobalComplexityEntryFields,
) -> serde_json::Value {
    json!({
        "crate": crate_name,
        "symbol": fields.symbol,
        "file": fields.file,
        "line": fields.line,
        "complexity_proxy": fields.complexity_proxy,
        "mir_blocks": fields.mir_blocks,
        "mir_stmts": fields.mir_stmts,
    })
}

struct GlobalComplexityEntryFields<'a> {
    symbol: Option<&'a serde_json::Value>,
    file: Option<&'a serde_json::Value>,
    line: Option<&'a serde_json::Value>,
    complexity_proxy: Option<&'a serde_json::Value>,
    mir_blocks: Option<&'a serde_json::Value>,
    mir_stmts: Option<&'a serde_json::Value>,
}

fn global_complexity_entry_fields(entry: &serde_json::Value) -> GlobalComplexityEntryFields<'_> {
    let symbol = entry.get("symbol");
    let file = entry.get("file");
    let line = entry.get("line");
    let complexity_proxy = entry.get("complexity_proxy");
    let mir_blocks = entry.get("mir_blocks");
    let mir_stmts = entry.get("mir_stmts");
    GlobalComplexityEntryFields {
        symbol,
        file,
        line,
        complexity_proxy,
        mir_blocks,
        mir_stmts,
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &semantic::SymbolSummary, usize, usize
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_complexity_entry(
    s: &crate::semantic::SymbolSummary,
    blocks: usize,
    stmts: usize,
) -> serde_json::Value {
    let branch_score = s.branch_score;
    let proxy = complexity_proxy(branch_score, blocks);
    complexity_entry_json(s, blocks, stmts, branch_score, proxy)
}

fn complexity_entry_json(
    s: &crate::semantic::SymbolSummary,
    blocks: usize,
    stmts: usize,
    branch_score: Option<f64>,
    proxy: f64,
) -> serde_json::Value {
    let file = complexity_entry_file(s);
    let identity = complexity_entry_identity(s, file);
    complexity_entry_value(identity, s.is_directly_recursive, blocks, stmts, branch_score, proxy)
}

fn complexity_entry_value(
    identity: ComplexityEntryIdentity,
    is_directly_recursive: bool,
    blocks: usize,
    stmts: usize,
    branch_score: Option<f64>,
    proxy: f64,
) -> serde_json::Value {
    let mut value = complexity_entry_identity_value(identity);
    add_complexity_entry_metrics(
        &mut value,
        is_directly_recursive,
        blocks,
        stmts,
        branch_score,
        proxy,
    );
    value
}

fn complexity_entry_identity_value(identity: ComplexityEntryIdentity) -> serde_json::Value {
    json!({
        "symbol": identity.symbol,
        "file": identity.file,
        "line": identity.line,
        "mir_fingerprint": identity.mir_fingerprint,
    })
}

fn add_complexity_entry_metrics(
    value: &mut serde_json::Value,
    is_directly_recursive: bool,
    blocks: usize,
    stmts: usize,
    branch_score: Option<f64>,
    proxy: f64,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    obj.insert("mir_blocks".to_string(), json!(blocks));
    obj.insert("mir_stmts".to_string(), json!(stmts));
    obj.insert("branch_score".to_string(), json!(branch_score));
    obj.insert(
        "is_directly_recursive".to_string(),
        json!(is_directly_recursive),
    );
    obj.insert("complexity_proxy".to_string(), json!(proxy));
}

fn complexity_entry_file(s: &crate::semantic::SymbolSummary) -> String {
    shorten_display_path(&s.file)
}

struct ComplexityEntryIdentity<'a> {
    symbol: &'a str,
    file: String,
    line: u32,
    mir_fingerprint: &'a Option<String>,
}

fn complexity_entry_identity<'a>(
    s: &'a crate::semantic::SymbolSummary,
    file: String,
) -> ComplexityEntryIdentity<'a> {
    ComplexityEntryIdentity {
        symbol: &s.symbol,
        file,
        line: s.line,
        mir_fingerprint: &s.mir_fingerprint,
    }
}

fn complexity_proxy(branch_score: Option<f64>, blocks: usize) -> f64 {
    branch_score.unwrap_or(blocks as f64)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<std::option::Option<std::path::PathBuf>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

pub fn enqueue_issue_task_generation(workspace: &Path) {
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

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
    let _ = crate::graph_metrics::generate_error_shaping_dispersion_issues(workspace);
    let _ = crate::graph_metrics::generate_state_transition_dispersion_issues(workspace);
    let _ = crate::graph_metrics::generate_planner_loop_fragmentation_issues(workspace);
    let _ = crate::graph_metrics::generate_implicit_state_machine_issues(workspace);
    let _ = crate::graph_metrics::generate_effect_boundary_leak_issues(workspace);
    let _ = crate::graph_metrics::generate_logging_dispersion_issues(workspace);
    let _ = crate::graph_metrics::generate_process_spawn_dispersion_issues(workspace);
    let _ = crate::graph_metrics::generate_network_usage_dispersion_issues(workspace);
    let _ = crate::graph_metrics::generate_representation_fanout_issues(workspace);
    let _ = crate::graph_metrics::generate_scc_region_reduction_issues(workspace);
    let _ = crate::graph_metrics::generate_dominator_region_reduction_issues(workspace);
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn generate_invariant_lifecycle_issues(workspace: &Path) {
    // Auto-generate invariant lifecycle issues (action surface gap, prompt injection gap, per-promoted gates)
    let _ = crate::invariants::generate_invariant_issues(workspace);
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: std::vec::Vec<serde_json::Value>, std::vec::Vec<serde_json::Value>, serde_json::Value, &evaluation::EvaluationWorkspaceSnapshot, &drift_analysis::FingerprintDrift
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
            "semantic_contract": eval.vector.semantic_contract,
            "semantic_fn_total": eval.semantic_fn_total,
            "semantic_fn_with_any_error": eval.semantic_fn_with_any_error,
            "semantic_fn_error_rate": eval.semantic_fn_error_rate,
            "diagnostics_repair_pressure": eval.diagnostics_repair_pressure,
            "objectives": format!("{}/{}", eval.objectives_completed, eval.objectives_total),
            "tasks": format!("{}/{}", eval.completed_tasks, eval.total_tasks),
        },
        "fingerprint_drift": drift,
        "per_crate": per_crate,
    })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path, &[semantic::SymbolSummary]
/// Outputs: std::result::Result<drift_analysis::FingerprintDrift, anyhow::Error>
/// Effects: fs_read, fs_write, logging, state_read, state_write, transitions_state
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

    let drift = crate::drift_analysis::compute_fingerprint_drift(
        workspace,
        &prev_summaries,
        current_summaries,
    );
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

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<std::path::PathBuf, anyhow::Error>
/// Effects: fs_write, state_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

#[cfg(test)]
mod tests {
    use super::build_graph_delta_report;
    use serde_json::json;

    #[test]
    fn graph_delta_report_marks_reductions_as_improvements() {
        let before = json!({
            "generated_at_ms": 100,
            "totals": {
                "overall_graph_entropy_score": 0.40,
                "pathway_count": 12,
                "redundant_path_count": 30,
                "artifact_writer_dispersion_count": 3,
                "error_shaping_dispersion_count": 2,
                "state_transition_dispersion_count": 4,
                "representation_fanout_count": 5
            },
            "crates": [{
                "crate": "canon_mini_agent",
                "overall_graph_entropy_score": 0.40,
                "pathway_count": 12,
                "redundant_path_count": 30,
                "artifact_writer_dispersion_count": 3,
                "error_shaping_dispersion_count": 2,
                "state_transition_dispersion_count": 4,
                "representation_fanout_count": 5
            }]
        });
        let after = json!({
            "generated_at_ms": 200,
            "totals": {
                "overall_graph_entropy_score": 0.32,
                "pathway_count": 11,
                "redundant_path_count": 24,
                "artifact_writer_dispersion_count": 2,
                "error_shaping_dispersion_count": 1,
                "state_transition_dispersion_count": 2,
                "representation_fanout_count": 3
            },
            "crates": [{
                "crate": "canon_mini_agent",
                "overall_graph_entropy_score": 0.32,
                "pathway_count": 11,
                "redundant_path_count": 24,
                "artifact_writer_dispersion_count": 2,
                "error_shaping_dispersion_count": 1,
                "state_transition_dispersion_count": 2,
                "representation_fanout_count": 3
            }]
        });

        let report = build_graph_delta_report(&before, &after);
        let delta = report["metrics"]["overall_graph_entropy_score"]["delta"]
            .as_f64()
            .unwrap_or_default();
        assert!((delta + 0.08).abs() < 1e-9, "unexpected delta: {delta}");
        assert_eq!(
            report["metrics"]["pathway_count"]["improved"].as_bool(),
            Some(true)
        );
        assert_eq!(report["verification_passed"].as_bool(), Some(true));
        assert_eq!(
            report["per_crate"][0]["artifact_writer_dispersion_count"]["delta"].as_f64(),
            Some(-1.0)
        );
        assert_eq!(
            report["metrics"]["representation_fanout_count"]["delta"].as_f64(),
            Some(-2.0)
        );
    }
}

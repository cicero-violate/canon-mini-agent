//! Graph-metric issue generation.
//!
//! Detects bridge-connectivity overload directly from `state/rustc/<crate>/graph.json`
//! and emits deterministic `ISSUES.json` entries when the bridge edge density
//! exceeds a threshold.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::issues::{
    load_issues_file, persist_issues_projection_with_writer, rescore_all, Issue, IssuesFile,
};
use crate::semantic::{GraphCountKind, SemanticIndex, SymbolSummary};

const DEFAULT_BRIDGE_RATIO_THRESHOLD: f64 = 10.0;
const CANDIDATE_FUNCTION_LIMIT: usize = 5;
const MIN_ACTIONABLE_BRIDGE_GRAPH_NODES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionReductionMode {
    Scc,
    Dominator,
}

#[derive(Debug, Clone)]
pub struct BridgeConnectivityStats {
    pub crate_name: String,
    pub node_count: usize,
    pub bridge_edge_count: usize,
    pub semantic_edge_count: usize,
    pub cfg_node_count: usize,
    pub cfg_edge_count: usize,
    pub bridge_ratio: f64,
    pub threshold: f64,
    pub candidate_functions: Vec<(String, usize)>,
}

pub fn generate_bridge_connectivity_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);

    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(stats) = analyze_bridge_connectivity(workspace, &crate_name) else {
            continue;
        };
        let desired_issue = build_bridge_issue(&stats);
        let actionable = bridge_signal_is_actionable(&stats);
        mutated += upsert_bridge_issue(
            &mut file,
            desired_issue,
            actionable && stats.bridge_ratio > stats.threshold,
        );
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_bridge_connectivity_issues",
        )?;
    }

    Ok(mutated)
}

fn bridge_signal_is_actionable(stats: &BridgeConnectivityStats) -> bool {
    !(stats.node_count < MIN_ACTIONABLE_BRIDGE_GRAPH_NODES && stats.candidate_functions.is_empty())
}

pub fn generate_module_cohesion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let existing: HashSet<String> = file.issues.iter().map(|i| i.id.clone()).collect();
    let mut created = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        created += sync_module_cohesion_issues_for_crate(&mut file, &existing, &crate_name, &idx);
    }

    if created > 0 {
        rescore_all(&mut file);
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_module_cohesion_issues",
        )?;
    }
    Ok(created)
}

fn sync_module_cohesion_issues_for_crate(
    file: &mut IssuesFile,
    existing: &HashSet<String>,
    crate_name: &str,
    idx: &SemanticIndex,
) -> usize {
    let call_edges = idx.call_edges();
    if call_edges.is_empty() {
        return 0;
    }
    let (fn_to_module, module_symbols) = collect_module_functions(idx);
    if fn_to_module.is_empty() {
        return 0;
    }
    let (internal_edges, external_edges) = collect_module_edge_counts(call_edges, &fn_to_module);

    let crate_name = crate_name.replace('-', "_");
    let mut created = 0usize;
    for (module, symbols) in module_symbols {
        created += maybe_insert_module_cohesion_issue(
            file,
            existing,
            &crate_name,
            module,
            symbols,
            &internal_edges,
            &external_edges,
        );
    }
    created
}

fn collect_module_functions(
    idx: &SemanticIndex,
) -> (HashMap<String, String>, HashMap<String, HashSet<String>>) {
    let summaries = idx.symbol_summaries();
    let fn_summaries: Vec<_> = summaries.into_iter().filter(|s| s.kind == "fn").collect();

    let mut fn_to_module: HashMap<String, String> = HashMap::new();
    let mut module_symbols: HashMap<String, HashSet<String>> = HashMap::new();
    for summary in &fn_summaries {
        let module = module_partition_key(summary);
        fn_to_module.insert(summary.symbol.clone(), module.clone());
        module_symbols
            .entry(module)
            .or_default()
            .insert(summary.symbol.clone());
    }

    (fn_to_module, module_symbols)
}

fn collect_module_edge_counts(
    call_edges: Vec<(String, String)>,
    fn_to_module: &HashMap<String, String>,
) -> (HashMap<String, usize>, HashMap<String, usize>) {
    let mut internal_edges: HashMap<String, usize> = HashMap::new();
    let mut external_edges: HashMap<String, usize> = HashMap::new();

    for (from, to) in call_edges {
        let Some(from_module) = fn_to_module.get(&from) else {
            continue;
        };
        let Some(to_module) = fn_to_module.get(&to) else {
            continue;
        };
        if from_module == to_module {
            *internal_edges.entry(from_module.clone()).or_insert(0) += 1;
        } else {
            *external_edges.entry(from_module.clone()).or_insert(0) += 1;
        }
    }

    (internal_edges, external_edges)
}

fn maybe_insert_module_cohesion_issue(
    file: &mut IssuesFile,
    existing: &HashSet<String>,
    crate_name: &str,
    module: String,
    symbols: HashSet<String>,
    internal_edges: &HashMap<String, usize>,
    external_edges: &HashMap<String, usize>,
) -> usize {
    let internal = internal_edges.get(&module).copied().unwrap_or(0);
    let external = external_edges.get(&module).copied().unwrap_or(0);
    let total = internal + external;
    if total == 0 {
        return 0;
    }

    let cohesion = internal as f64 / total as f64;
    let task = if cohesion < 0.2 && symbols.len() > 5 {
        Some("DissolveModule")
    } else if cohesion > 0.8 && external > 10 {
        Some("FormalizeBoundary")
    } else {
        None
    };
    let Some(task) = task else {
        return 0;
    };

    let id = format!(
        "auto_cohesion_{}_{}",
        crate_name,
        stable_hash(&format!("{module}:{task}"))
    );
    if existing.contains(&id) {
        return 0;
    }

    let confidence_tier = if symbols.len() > 5 { "high" } else { "medium" };
    file.issues.push(Issue {
        id,
        title: format!(
            "Module cohesion signal `{}` in `{}` (cohesion={:.2})",
            task, module, cohesion
        ),
        status: "open".to_string(),
        priority: "medium".to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Module `{module}` in crate `{crate}` has cohesion={cohesion:.2} (internal={internal}, external={external}, symbols={symbol_count}).\n\
             Recommended task: {task}.",
            crate = crate_name,
            symbol_count = symbols.len()
        ),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "module": module,
            "internal_edges": internal,
            "external_edges": external,
            "cohesion": cohesion,
            "symbol_count": symbols.len(),
            "task": task,
            "confidence_tier": confidence_tier,
            "correctness_level": confidence_tier == "high"
        }),
        acceptance_criteria: vec![
            "module boundary complexity reduced and validated".to_string(),
            "build and tests remain green".to_string(),
        ],
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    });
    1
}

pub fn generate_artifact_writer_dispersion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        mutated += sync_artifact_writer_dispersion_issues_for_crate(&mut file, &crate_name, &idx);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_artifact_writer_dispersion_issues",
        )?;
    }

    Ok(mutated)
}

fn sync_artifact_writer_dispersion_issues_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
) -> usize {
    let crate_name = crate_name.replace('-', "_");
    let writers_by_artifact = collect_artifact_writers_by_artifact(idx);
    let (desired_ids, mutated) =
        upsert_artifact_writer_dispersion_issues(file, &crate_name, writers_by_artifact);
    mutated + resolve_stale_artifact_writer_dispersion_issues(file, &crate_name, &desired_ids)
}

fn collect_artifact_writers_by_artifact(idx: &SemanticIndex) -> HashMap<String, HashSet<String>> {
    let mut writers_by_artifact: HashMap<String, HashSet<String>> = HashMap::new();
    for (writer, artifact) in idx.artifact_write_edges() {
        if !looks_like_symbol(&writer) || artifact.trim().is_empty() {
            continue;
        }
        writers_by_artifact.entry(artifact).or_default().insert(writer);
    }
    writers_by_artifact
}

fn upsert_artifact_writer_dispersion_issues(
    file: &mut IssuesFile,
    crate_name: &str,
    writers_by_artifact: HashMap<String, HashSet<String>>,
) -> (HashSet<String>, usize) {
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for (artifact, writers) in writers_by_artifact {
        let mut writers: Vec<String> = writers.into_iter().collect();
        writers.sort();
        let issue = build_artifact_writer_dispersion_issue(crate_name, &artifact, &writers);
        desired_ids.insert(issue.id.clone());
        mutated += upsert_bridge_issue(file, issue, writers.len() > 1);
    }

    (desired_ids, mutated)
}

fn resolve_stale_artifact_writer_dispersion_issues(
    file: &mut IssuesFile,
    crate_name: &str,
    desired_ids: &HashSet<String>,
) -> usize {
    let mut mutated = 0usize;
    let prefix = format!("auto_artifact_writer_dispersion_{crate_name}_");

    for issue in &mut file.issues {
        if issue.id.starts_with(&prefix)
            && !desired_ids.contains(&issue.id)
            && issue.status != "resolved"
        {
            issue.status = "resolved".to_string();
            mutated += 1;
        }
    }

    mutated
}

pub fn generate_error_shaping_dispersion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        mutated += sync_error_shaping_dispersion_issue_for_crate(&mut file, &crate_name, &idx);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_error_shaping_dispersion_issues",
        )?;
    }

    Ok(mutated)
}

fn sync_error_shaping_dispersion_issue_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
) -> usize {
    let crate_name = crate_name.replace('-', "_");
    let by_style = collect_error_styles_by_style(idx);
    let desired = build_error_shaping_dispersion_issue(&crate_name, &by_style);
    let active = desired
        .metrics
        .get("function_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        >= 4;
    upsert_bridge_issue(file, desired, active)
}

fn collect_error_styles_by_style(idx: &SemanticIndex) -> HashMap<String, HashSet<String>> {
    let mut by_style: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, style) in idx.semantic_edges_by_relation("ShapesError") {
        if !looks_like_symbol(&symbol) || style.trim().is_empty() {
            continue;
        }
        by_style.entry(style).or_default().insert(symbol);
    }
    by_style
}

fn collect_state_transitions_by_domain(idx: &SemanticIndex) -> HashMap<String, HashSet<String>> {
    let mut transitions_by_state: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, state) in idx.state_transition_edges() {
        if !looks_like_symbol(&symbol) || state.trim().is_empty() {
            continue;
        }
        transitions_by_state.entry(state).or_default().insert(symbol);
    }
    transitions_by_state
}

fn resolve_missing_state_transition_dispersion_issues(
    file: &mut IssuesFile,
    desired_ids: &HashSet<String>,
    crate_name: &str,
) -> usize {
    let mut mutated = 0usize;
    let prefix = format!("auto_state_transition_dispersion_{crate_name}_");
    for issue in &mut file.issues {
        if issue.id.starts_with(&prefix)
            && !desired_ids.contains(&issue.id)
            && issue.status != "resolved"
        {
            issue.status = "resolved".to_string();
            mutated += 1;
        }
    }
    mutated
}

fn collect_coordinated_transitions_by_symbol(
    idx: &SemanticIndex,
) -> HashMap<String, Vec<String>> {
    let mut coordinated_by_symbol: HashMap<String, Vec<String>> = HashMap::new();
    for (symbol, proof_type) in idx.semantic_edges_by_relation("CoordinatesTransition") {
        coordinated_by_symbol
            .entry(symbol)
            .or_default()
            .push(proof_type);
    }
    for proof_types in coordinated_by_symbol.values_mut() {
        proof_types.sort();
        proof_types.dedup();
    }
    coordinated_by_symbol
}

fn sync_state_transition_dispersion_issues_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
) -> usize {
    let coordinated_by_symbol = collect_coordinated_transitions_by_symbol(idx);
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for (state_domain, transitions) in collect_state_transitions_by_domain(idx) {
        let mut transitions: Vec<String> = transitions.into_iter().collect();
        transitions.sort();
        let issue = build_state_transition_dispersion_issue(
            crate_name,
            &state_domain,
            &transitions,
            &coordinated_by_symbol,
        );
        desired_ids.insert(issue.id.clone());
        mutated += upsert_bridge_issue(file, issue, transitions.len() > 1);
    }

    mutated + resolve_missing_state_transition_dispersion_issues(file, &desired_ids, crate_name)
}

pub fn generate_state_transition_dispersion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        mutated += sync_state_transition_dispersion_issues_for_crate(
            &mut file,
            &crate_name,
            &idx,
        );
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_state_transition_dispersion_issues",
        )?;
    }

    Ok(mutated)
}

pub fn generate_planner_loop_fragmentation_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        mutated += sync_planner_loop_fragmentation_issue_for_crate(&mut file, &crate_name, &idx);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_planner_loop_fragmentation_issues",
        )?;
    }

    Ok(mutated)
}

fn sync_planner_loop_fragmentation_issue_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
) -> usize {
    let crate_name = crate_name.replace('-', "_");
    let desired = build_planner_loop_fragmentation_issue(&crate_name, idx);
    let active = desired
        .metrics
        .get("owner_candidate_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 1;
    upsert_bridge_issue(file, desired, active)
}

pub fn generate_implicit_state_machine_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        mutated += sync_implicit_state_machine_issues_for_crate(&mut file, &crate_name, &idx);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_implicit_state_machine_issues",
        )?;
    }

    Ok(mutated)
}

fn sync_implicit_state_machine_issues_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
) -> usize {
    let crate_name = crate_name.replace('-', "_");
    let states_by_symbol = collect_states_by_symbol(idx);
    let (desired_ids, mutated) =
        upsert_implicit_state_machine_issues(file, &crate_name, idx, &states_by_symbol);
    mutated + resolve_stale_implicit_state_machine_issues(file, &crate_name, &desired_ids)
}

fn collect_states_by_symbol(idx: &SemanticIndex) -> HashMap<String, HashSet<String>> {
    let mut states_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, state) in idx.state_transition_edges() {
        states_by_symbol.entry(symbol).or_default().insert(state);
    }
    states_by_symbol
}

fn upsert_implicit_state_machine_issues(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
    states_by_symbol: &HashMap<String, HashSet<String>>,
) -> (HashSet<String>, usize) {
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for summary in idx.symbol_summaries() {
        if summary.kind != "fn" {
            continue;
        }
        let Some(state_domains) = states_by_symbol.get(&summary.symbol) else {
            continue;
        };
        let branch_score = summary.branch_score.unwrap_or(0.0);
        let workflow_like = implicit_state_machine_candidate_symbol(&summary.symbol);
        let state_count = state_domains.len();
        let qualifies = workflow_like
            && branch_score >= 4.0
            && (summary.has_back_edges || summary.switchint_count >= 2 || state_count >= 2);
        let issue = build_implicit_state_machine_issue(crate_name, &summary, state_domains);
        desired_ids.insert(issue.id.clone());
        mutated += upsert_bridge_issue(file, issue, qualifies);
    }

    (desired_ids, mutated)
}

fn resolve_stale_implicit_state_machine_issues(
    file: &mut IssuesFile,
    crate_name: &str,
    desired_ids: &HashSet<String>,
) -> usize {
    let mut mutated = 0usize;
    let prefix = format!("auto_implicit_state_machine_{crate_name}_");

    for issue in &mut file.issues {
        if issue.id.starts_with(&prefix)
            && !desired_ids.contains(&issue.id)
            && issue.status != "resolved"
        {
            issue.status = "resolved".to_string();
            mutated += 1;
        }
    }

    mutated
}

pub fn generate_effect_boundary_leak_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        mutated += sync_effect_boundary_leak_issues_for_crate(workspace, &mut file, &crate_name)?;
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_effect_boundary_leak_issues",
        )?;
    }

    Ok(mutated)
}

fn sync_effect_boundary_leak_issues_for_crate(
    workspace: &Path,
    file: &mut IssuesFile,
    crate_name: &str,
) -> Result<usize> {
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Ok(0);
    };
    let crate_name = crate_name.replace('-', "_");
    let by_module = collect_effect_boundary_rows_by_module(&idx);
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for (module, rows) in by_module {
        let issue = build_effect_boundary_leak_issue(&crate_name, &module, &rows);
        desired_ids.insert(issue.id.clone());
        mutated += upsert_bridge_issue(file, issue, !rows.is_empty());
    }

    mutated += resolve_stale_effect_boundary_leak_issues(file, &crate_name, &desired_ids);
    Ok(mutated)
}

fn collect_effect_boundary_rows_by_module(
    idx: &SemanticIndex,
) -> HashMap<String, Vec<(String, Vec<&'static str>)>> {
    let effects_by_symbol = collect_effects_by_symbol(idx);
    let effectful_symbols: HashSet<String> = effects_by_symbol.keys().cloned().collect();
    let canonical_modules = derive_canonical_effect_boundary_modules(idx, &effectful_symbols);
    let mut by_module: HashMap<String, Vec<(String, Vec<&'static str>)>> = HashMap::new();

    for (symbol, effects) in effects_by_symbol {
        if is_canonical_effect_boundary_symbol(&canonical_modules, &symbol) || effects.len() < 3 {
            continue;
        }
        let module = symbol_module(&symbol);
        let mut effects_vec: Vec<&'static str> = effects.into_iter().collect();
        effects_vec.sort();
        by_module.entry(module).or_default().push((symbol, effects_vec));
    }

    for rows in by_module.values_mut() {
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    }

    by_module
}

fn collect_effects_by_symbol(idx: &SemanticIndex) -> HashMap<String, HashSet<&'static str>> {
    let mut effects_by_symbol: HashMap<String, HashSet<&'static str>> = HashMap::new();
    insert_effect_labels(&mut effects_by_symbol, idx.artifact_write_edges(), "artifact_write");
    insert_effect_labels(&mut effects_by_symbol, idx.artifact_read_edges(), "artifact_read");
    insert_effect_labels(&mut effects_by_symbol, idx.state_write_edges(), "state_write");
    insert_effect_labels(&mut effects_by_symbol, idx.state_read_edges(), "state_read");
    insert_effect_labels(
        &mut effects_by_symbol,
        idx.state_transition_edges(),
        "state_transition",
    );
    insert_effect_labels(
        &mut effects_by_symbol,
        idx.semantic_edges_by_relation("ShapesError"),
        "error_shape",
    );
    insert_effect_labels(
        &mut effects_by_symbol,
        idx.semantic_edges_by_relation("PerformsLogging"),
        "logging",
    );
    insert_effect_labels(
        &mut effects_by_symbol,
        idx.semantic_edges_by_relation("SpawnsProcess"),
        "process_spawn",
    );
    insert_effect_labels(
        &mut effects_by_symbol,
        idx.semantic_edges_by_relation("UsesNetwork"),
        "network",
    );
    effects_by_symbol
}

fn insert_effect_labels<I>(
    effects_by_symbol: &mut HashMap<String, HashSet<&'static str>>,
    edges: I,
    label: &'static str,
) where
    I: IntoIterator<Item = (String, String)>,
{
    for (symbol, _) in edges {
        effects_by_symbol.entry(symbol).or_default().insert(label);
    }
}

fn resolve_stale_effect_boundary_leak_issues(
    file: &mut IssuesFile,
    crate_name: &str,
    desired_ids: &HashSet<String>,
) -> usize {
    let prefix = format!("auto_effect_boundary_leak_{crate_name}_");
    let mut mutated = 0usize;
    for issue in &mut file.issues {
        if issue.id.starts_with(&prefix)
            && !desired_ids.contains(&issue.id)
            && issue.status != "resolved"
        {
            issue.status = "resolved".to_string();
            mutated += 1;
        }
    }
    mutated
}

pub fn generate_logging_dispersion_issues(workspace: &Path) -> Result<usize> {
    generate_effect_dispersion_issues(workspace, EffectDispersionMode::Logging)
}

pub fn generate_process_spawn_dispersion_issues(workspace: &Path) -> Result<usize> {
    generate_effect_dispersion_issues(workspace, EffectDispersionMode::ProcessSpawn)
}

pub fn generate_network_usage_dispersion_issues(workspace: &Path) -> Result<usize> {
    generate_effect_dispersion_issues(workspace, EffectDispersionMode::Network)
}

fn generate_effect_dispersion_issues(
    workspace: &Path,
    mode: EffectDispersionMode,
) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        mutated += sync_effect_dispersion_issue_for_crate(&mut file, &crate_name, &idx, mode);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            mode.persist_label(),
        )?;
    }

    Ok(mutated)
}

fn sync_effect_dispersion_issue_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
    mode: EffectDispersionMode,
) -> usize {
    let by_module = collect_effect_modules_by_relation(idx, mode.relation());
    let desired = mode.build_issue(crate_name, &by_module);
    upsert_bridge_issue(file, desired, mode.is_active(&by_module))
}

#[derive(Clone, Copy)]
enum EffectDispersionMode {
    Logging,
    ProcessSpawn,
    Network,
}

impl EffectDispersionMode {
    fn relation(self) -> &'static str {
        match self {
            EffectDispersionMode::Logging => "PerformsLogging",
            EffectDispersionMode::ProcessSpawn => "SpawnsProcess",
            EffectDispersionMode::Network => "UsesNetwork",
        }
    }

    fn persist_label(self) -> &'static str {
        match self {
            EffectDispersionMode::Logging => "generate_logging_dispersion_issues",
            EffectDispersionMode::ProcessSpawn => "generate_process_spawn_dispersion_issues",
            EffectDispersionMode::Network => "generate_network_usage_dispersion_issues",
        }
    }

    fn build_issue(self, crate_name: &str, by_module: &HashMap<String, Vec<String>>) -> Issue {
        match self {
            EffectDispersionMode::Logging => build_logging_dispersion_issue(crate_name, by_module),
            EffectDispersionMode::ProcessSpawn => {
                build_process_spawn_dispersion_issue(crate_name, by_module)
            }
            EffectDispersionMode::Network => {
                build_network_usage_dispersion_issue(crate_name, by_module)
            }
        }
    }

    fn is_active(self, by_module: &HashMap<String, Vec<String>>) -> bool {
        match self {
            EffectDispersionMode::Logging => {
                count_symbols_outside_boundary(by_module, is_canonical_logging_module) > 10
            }
            EffectDispersionMode::ProcessSpawn => {
                count_modules_outside_boundary(by_module, is_canonical_process_boundary) > 1
            }
            EffectDispersionMode::Network => {
                by_module.values().map(|symbols| symbols.len()).sum::<usize>() > 0
            }
        }
    }
}

pub fn generate_representation_fanout_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        let (sources_by_symbol, targets_by_symbol) = collect_representation_domains(&idx);
        let canonical_modules = derive_canonical_effect_boundary_modules(
            &idx,
            &sources_by_symbol
                .keys()
                .chain(targets_by_symbol.keys())
                .cloned()
                .collect(),
        );
        let symbols_by_pair =
            build_representation_symbols_by_pair(sources_by_symbol, &targets_by_symbol);

        for ((source, target), mut symbols) in symbols_by_pair {
            symbols.sort();
            symbols.dedup();
            let issue = build_representation_fanout_issue(
                &crate_name,
                &source,
                &target,
                &symbols,
                &canonical_modules,
            );
            desired_ids.insert(issue.id.clone());
            mutated += upsert_bridge_issue(&mut file, issue, symbols.len() > 1);
        }

        let prefix = format!("auto_representation_fanout_{crate_name}_");
        for issue in &mut file.issues {
            if issue.id.starts_with(&prefix)
                && !desired_ids.contains(&issue.id)
                && issue.status != "resolved"
            {
                issue.status = "resolved".to_string();
                mutated += 1;
            }
        }
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            "generate_representation_fanout_issues",
        )?;
    }

    Ok(mutated)
}

fn collect_representation_domains(
    idx: &SemanticIndex,
) -> (
    HashMap<String, HashSet<String>>,
    HashMap<String, HashSet<String>>,
) {
    let mut sources_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
    let mut targets_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();

    for (symbol, artifact) in idx.artifact_read_edges() {
        sources_by_symbol
            .entry(symbol)
            .or_default()
            .insert(normalize_representation_domain("artifact", &artifact));
    }
    for (symbol, state) in idx.state_read_edges() {
        sources_by_symbol
            .entry(symbol)
            .or_default()
            .insert(normalize_representation_domain("state", &state));
    }
    for (symbol, artifact) in idx.artifact_write_edges() {
        targets_by_symbol
            .entry(symbol)
            .or_default()
            .insert(normalize_representation_domain("artifact", &artifact));
    }
    for (symbol, state) in idx.state_write_edges() {
        targets_by_symbol
            .entry(symbol)
            .or_default()
            .insert(normalize_representation_domain("state", &state));
    }

    (sources_by_symbol, targets_by_symbol)
}

fn collect_effect_modules_by_relation(
    idx: &SemanticIndex,
    relation: &str,
) -> HashMap<String, Vec<String>> {
    let mut by_module: HashMap<String, Vec<String>> = HashMap::new();
    for (symbol, _) in idx.semantic_edges_by_relation(relation) {
        if !looks_like_symbol(&symbol) {
            continue;
        }
        let module = symbol
            .rsplit_once("::")
            .map(|(m, _)| m.to_string())
            .unwrap_or_else(|| symbol.clone());
        by_module.entry(module).or_default().push(symbol);
    }
    by_module
}

fn count_symbols_outside_boundary(
    by_module: &HashMap<String, Vec<String>>,
    is_canonical: fn(&str) -> bool,
) -> usize {
    by_module
        .iter()
        .filter(|(module, _)| !is_canonical(module))
        .map(|(_, symbols)| symbols.len())
        .sum()
}

fn count_modules_outside_boundary(
    by_module: &HashMap<String, Vec<String>>,
    is_canonical: fn(&str) -> bool,
) -> usize {
    by_module
        .keys()
        .filter(|module| !is_canonical(module))
        .count()
}

fn build_representation_symbols_by_pair(
    sources_by_symbol: HashMap<String, HashSet<String>>,
    targets_by_symbol: &HashMap<String, HashSet<String>>,
) -> HashMap<(String, String), Vec<String>> {
    let mut symbols_by_pair: HashMap<(String, String), Vec<String>> = HashMap::new();

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
                    .push(symbol.clone());
            }
        }
    }

    symbols_by_pair
}

pub fn generate_scc_region_reduction_issues(workspace: &Path) -> Result<usize> {
    generate_region_reduction_issues(workspace, RegionReductionMode::Scc)
}

pub fn generate_dominator_region_reduction_issues(workspace: &Path) -> Result<usize> {
    generate_region_reduction_issues(workspace, RegionReductionMode::Dominator)
}

fn generate_region_reduction_issues(
    workspace: &Path,
    mode: RegionReductionMode,
) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        mutated += sync_region_reduction_issues_for_crate(
            &mut file,
            &crate_name,
            &idx,
            &mut desired_ids,
            mode,
        );
        mutated += resolve_legacy_cfg_region_issues(&mut file, &crate_name);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        persist_issues_projection_with_writer(
            workspace,
            &file,
            None,
            match mode {
                RegionReductionMode::Scc => "generate_scc_region_reduction_issues",
                RegionReductionMode::Dominator => "generate_dominator_region_reduction_issues",
            },
        )?;
    }

    Ok(mutated)
}

fn sync_region_reduction_issues_for_crate(
    file: &mut IssuesFile,
    crate_name: &str,
    idx: &SemanticIndex,
    desired_ids: &mut HashSet<String>,
    mode: RegionReductionMode,
) -> usize {
    let mut mutated = 0usize;
    let mut candidates = collect_region_reduction_candidates(crate_name, idx, mode);
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.id.cmp(&b.1.id))
    });

    for (_, issue) in candidates.into_iter().take(25) {
        desired_ids.insert(issue.id.clone());
        mutated += upsert_bridge_issue(file, issue, true);
    }

    let prefix = region_reduction_prefix(crate_name, mode);
    for issue in &mut file.issues {
        if issue.id.starts_with(&prefix)
            && !desired_ids.contains(&issue.id)
            && issue.status != "resolved"
        {
            issue.status = "resolved".to_string();
            mutated += 1;
        }
    }

    mutated
}

fn collect_region_reduction_candidates(
    crate_name: &str,
    idx: &SemanticIndex,
    mode: RegionReductionMode,
) -> Vec<(f64, Issue)> {
    let redundant_by_symbol = redundant_path_counts_by_symbol(idx);
    idx.symbol_summaries()
        .into_iter()
        .filter(|summary| summary.kind == "fn")
        .filter_map(|summary| {
            qualifying_region_reduction_issue(crate_name, idx, &summary, &redundant_by_symbol, mode)
        })
        .collect()
}

fn qualifying_region_reduction_issue(
    crate_name: &str,
    idx: &SemanticIndex,
    summary: &SymbolSummary,
    redundant_by_symbol: &HashMap<String, usize>,
    mode: RegionReductionMode,
) -> Option<(f64, Issue)> {
    let symbol = summary.symbol.clone();
    let cfg_edges = idx.symbol_cfg_edges(&symbol);
    let back_edge_count = cfg_edges.iter().filter(|edge| edge.is_back_edge).count();
    let redundant_path_count = redundant_by_symbol.get(&symbol).copied().unwrap_or(0);
    let branch_score = summary.branch_score.unwrap_or(0.0);

    match mode {
        RegionReductionMode::Scc => {
            let qualifies = branch_score >= 4.0 && back_edge_count > 0;
            qualifies.then(|| {
                let issue = build_scc_region_reduction_issue(
                    crate_name,
                    summary,
                    back_edge_count,
                    redundant_path_count,
                );
                let score = branch_score
                    + (back_edge_count as f64 * 4.0)
                    + (redundant_path_count as f64 * 2.0)
                    + (summary.switchint_count as f64);
                (score, issue)
            })
        }
        RegionReductionMode::Dominator => {
            // Dominator-region candidates: high branching, no back-edges (loop-free),
            // and at least 2 SwitchInt terminators. non_cleanup_fraction is retained
            // for context only — it does not gate emission.
            let qualifies = branch_score >= 4.0
                && back_edge_count == 0
                && (summary.switchint_count >= 2 || redundant_path_count > 0);
            qualifies.then(|| {
                let non_cleanup_fraction = idx.cfg_dominance_score(&symbol);
                let issue = build_dominator_region_reduction_issue(
                    crate_name,
                    summary,
                    non_cleanup_fraction,
                    redundant_path_count,
                );
                let score = branch_score
                    + (redundant_path_count as f64 * 2.0)
                    + (summary.switchint_count as f64);
                (score, issue)
            })
        }
    }
}

fn redundant_path_counts_by_symbol(idx: &SemanticIndex) -> HashMap<String, usize> {
    idx.redundant_path_pairs()
        .into_iter()
        .fold(HashMap::new(), |mut acc, pair| {
            *acc.entry(pair.path_a.owner).or_insert(0) += 1;
            acc
        })
}

fn region_reduction_prefix(crate_name: &str, mode: RegionReductionMode) -> String {
    match mode {
        RegionReductionMode::Scc => format!("auto_scc_region_reduction_{crate_name}_"),
        RegionReductionMode::Dominator => {
            format!("auto_dominator_region_reduction_{crate_name}_")
        }
    }
}

pub fn analyze_bridge_connectivity(
    workspace: &Path,
    crate_name: &str,
) -> Result<BridgeConnectivityStats> {
    let idx = SemanticIndex::load(workspace, crate_name)?;
    let node_count = idx.graph_count(GraphCountKind::Node);
    let bridge_edge_count = idx.graph_count(GraphCountKind::BridgeEdge);
    let semantic_edge_count = idx.graph_count(GraphCountKind::SemanticEdge);
    let cfg_node_count = idx.graph_count(GraphCountKind::CfgNode);
    let cfg_edge_count = idx.graph_count(GraphCountKind::CfgEdge);
    let bridge_ratio = bridge_edge_count as f64 / node_count.max(1) as f64;
    let threshold = bridge_ratio_threshold(crate_name);
    let candidate_functions = top_bridge_candidate_functions(&idx);

    Ok(BridgeConnectivityStats {
        crate_name: crate_name.replace('-', "_"),
        node_count,
        bridge_edge_count,
        semantic_edge_count,
        cfg_node_count,
        cfg_edge_count,
        bridge_ratio,
        threshold,
        candidate_functions,
    })
}

fn bridge_ratio_threshold(_crate_name: &str) -> f64 {
    std::env::var("CANON_GRAPH_BRIDGE_RATIO_THRESHOLD")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_BRIDGE_RATIO_THRESHOLD)
}

fn top_bridge_candidate_functions(idx: &SemanticIndex) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_from, relation, to) in idx.bridge_edges() {
        if relation == "Call" && looks_like_symbol(&to) {
            *counts.entry(to).or_insert(0) += 1;
        }
    }
    let mut entries: Vec<(String, usize)> = counts.into_iter().collect();
    entries.sort_by(|(sym_a, count_a), (sym_b, count_b)| {
        count_b.cmp(count_a).then_with(|| sym_a.cmp(sym_b))
    });
    entries.truncate(CANDIDATE_FUNCTION_LIMIT);
    entries
}

fn looks_like_symbol(raw: &str) -> bool {
    !raw.starts_with("cfg::") && !raw.starts_with("path::") && raw.contains("::")
}

fn sanitize_fragment(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn stable_hash(raw: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    hasher.finish()
}

fn display_artifact_domain(raw: &str) -> String {
    let trimmed = raw.strip_prefix("artifact::").unwrap_or(raw);
    trimmed
        .split('/')
        .last()
        .unwrap_or(trimmed)
        .rsplit("::")
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

fn canonical_writer_candidate(artifact: &str, writers: &[String]) -> String {
    let artifact_hint = display_artifact_domain(artifact).to_ascii_lowercase();
    let mut ranked = writers.to_vec();
    ranked.sort_by(|a, b| {
        writer_rank(a, &artifact_hint)
            .cmp(&writer_rank(b, &artifact_hint))
            .then(a.len().cmp(&b.len()))
            .then(a.cmp(b))
    });
    ranked.into_iter().next().unwrap_or_default()
}

fn writer_rank(writer: &str, artifact_hint: &str) -> (usize, usize, usize) {
    let lower = writer.to_ascii_lowercase();
    let persist_bias = usize::from(!lower.contains("persist"));
    let artifact_bias = usize::from(!lower.contains(artifact_hint));
    let projection_bias = usize::from(!lower.contains("projection"));
    (persist_bias, artifact_bias, projection_bias)
}

fn build_artifact_writer_dispersion_issue(
    crate_name: &str,
    artifact: &str,
    writers: &[String],
) -> Issue {
    let display_artifact = display_artifact_domain(artifact);
    let canonical_writer = canonical_writer_candidate(artifact, writers);
    let canonical_short = canonical_writer
        .rsplit("::")
        .next()
        .unwrap_or(&canonical_writer)
        .to_string();
    let evidence = writers
        .iter()
        .map(|writer| format!("writer `{writer}` emits `{display_artifact}`"))
        .collect::<Vec<_>>();
    let writer_list = writers
        .iter()
        .map(|writer| format!("`{writer}`"))
        .collect::<Vec<_>>()
        .join(", ");

    Issue {
        id: format!(
            "auto_artifact_writer_dispersion_{}_{}",
            sanitize_fragment(crate_name),
            stable_hash(&format!("{crate_name}:{artifact}"))
        ),
        title: format!(
            "Artifact writer dispersion for `{display_artifact}` ({} writers)",
            writers.len()
        ),
        status: if writers.len() > 1 { "open" } else { "resolved" }.to_string(),
        priority: if writers.len() >= 4 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Artifact domain `{artifact}` in crate `{crate_name}` is written from multiple functions: {writer_list}.\n\n\
             Persistent artifact writes should flow through one canonical entrypoint per artifact domain. \
             Multiple writers increase state dispersion, make replay semantics harder to reason about, \
             and encourage divergent write behavior.\n\n\
             Recommended canonical entrypoint: `{canonical_writer}`."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "artifact": artifact,
            "display_artifact": display_artifact,
            "writer_count": writers.len(),
            "writers": writers,
            "canonical_writer_candidate": canonical_writer,
            "task": "CentralizeArtifactWriter",
        }),
        acceptance_criteria: vec![
            format!("all writes to `{display_artifact}` route through `{canonical_short}`"),
            format!("redundant writer entrypoints for `{display_artifact}` are deleted or converted to thin delegates"),
            "graph.json is regenerated and the detector reports at most one writer for the artifact".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn error_shaping_issue_id(crate_name: &str) -> String {
    format!(
        "auto_error_shaping_dispersion_{}",
        sanitize_fragment(crate_name)
    )
}

fn display_error_style(raw: &str) -> String {
    raw.strip_prefix("error_shape::").unwrap_or(raw).to_string()
}

fn build_error_shaping_dispersion_issue(
    crate_name: &str,
    by_style: &HashMap<String, HashSet<String>>,
) -> Issue {
    let mut styles: Vec<(String, Vec<String>)> = by_style
        .iter()
        .map(|(style, symbols)| {
            let mut symbols: Vec<String> = symbols.iter().cloned().collect();
            symbols.sort();
            (style.clone(), symbols)
        })
        .collect();
    styles.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    let mut all_functions: Vec<String> = styles
        .iter()
        .flat_map(|(_, symbols)| symbols.iter().cloned())
        .collect();
    all_functions.sort();
    all_functions.dedup();

    let style_metrics = styles
        .iter()
        .map(|(style, symbols)| {
            json!({
                "style": display_error_style(style),
                "function_count": symbols.len(),
                "functions": symbols,
            })
        })
        .collect::<Vec<_>>();
    let evidence = styles
        .iter()
        .map(|(style, symbols)| {
            format!(
                "error shaping `{}` used by {}",
                display_error_style(style),
                symbols.join(", ")
            )
        })
        .collect::<Vec<_>>();

    Issue {
        id: error_shaping_issue_id(crate_name),
        title: format!(
            "Error shaping dispersion in `{}` ({} functions, {} styles)",
            crate_name,
            all_functions.len(),
            styles.len()
        ),
        status: if all_functions.len() >= 4 { "open" } else { "resolved" }.to_string(),
        priority: if all_functions.len() >= 8 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Compiler-observed error shaping is distributed across {} function(s) in crate `{}`.\n\n\
             Direct use of `map_err`, `context`, and `with_context` across many functions increases \
             error-reporting drift and makes failure semantics harder to standardize. Centralize error \
             shaping behind a smaller helper layer or canonical report boundary.",
            all_functions.len(),
            crate_name
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizeErrorShaping",
            "function_count": all_functions.len(),
            "style_count": styles.len(),
            "functions": all_functions,
            "styles": style_metrics,
        }),
        acceptance_criteria: vec![
            "direct error shaping is centralized through a smaller helper surface".to_string(),
            "remaining call sites use consistent error-context conventions".to_string(),
            "graph.json is regenerated and the detector reports a lower function_count".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn display_state_domain(raw: &str) -> String {
    raw.strip_prefix("state::").unwrap_or(raw).to_string()
}

fn build_state_transition_dispersion_issue(
    crate_name: &str,
    state_domain: &str,
    transitions: &[String],
    coordinated_by_symbol: &HashMap<String, Vec<String>>,
) -> Issue {
    let display_state = display_state_domain(state_domain);
    let transition_list = transitions
        .iter()
        .map(|symbol| format!("`{symbol}`"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut workflow_coordinated_count = 0usize;
    let mut read_write_cycle_count = 0usize;
    for symbol in transitions {
        if let Some(proof_types) = coordinated_by_symbol.get(symbol) {
            for pt in proof_types {
                if pt == "transition::workflow_coordinated" {
                    workflow_coordinated_count += 1;
                } else if pt == "transition::read_write_cycle" {
                    read_write_cycle_count += 1;
                }
            }
        }
    }
    // Count symbols with any coordination backing (not edges — one symbol can have both proof types).
    let coordinated_count = transitions
        .iter()
        .filter(|s| coordinated_by_symbol.contains_key(*s))
        .count();
    let all_coordinated = !transitions.is_empty() && coordinated_count == transitions.len();
    let proof_tier = if all_coordinated { "proof" } else { "hypothesis" };

    let evidence = transitions
        .iter()
        .map(|symbol| {
            let coordination = coordinated_by_symbol.get(symbol).map(|pts| {
                pts.iter()
                    .filter_map(|pt| pt.strip_prefix("transition::"))
                    .collect::<Vec<_>>()
                    .join("+")
            });
            match coordination.as_deref() {
                Some(proof) if !proof.is_empty() => format!(
                    "transition site `{symbol}` mutates `{display_state}` [{proof}]"
                ),
                _ => format!(
                    "transition site `{symbol}` mutates `{display_state}` after branching"
                ),
            }
        })
        .collect::<Vec<_>>();

    Issue {
        id: format!(
            "auto_state_transition_dispersion_{}_{}",
            sanitize_fragment(crate_name),
            stable_hash(&format!("{crate_name}:{state_domain}"))
        ),
        title: format!(
            "State transition dispersion for `{display_state}` ({} transition sites)",
            transitions.len()
        ),
        status: if transitions.len() > 1 { "open" } else { "resolved" }.to_string(),
        priority: if transitions.len() >= 4 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Compiler-observed coordinated transitions for state domain `{state_domain}` \
             are distributed across multiple functions in crate `{crate_name}`: {transition_list}.\n\n\
             These sites are stronger than a bare branch-plus-write signal: each transition is workflow-coordinated \
             or forms a read/write transition cycle before mutating the domain. Transition logic that mutates \
             the same state domain from several unrelated sites tends to \
             create implicit state machines, duplicated guards, and divergent recovery semantics.\n\n\
             Extract one canonical transition layer for `{display_state}` and route these sites through it."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizeStateTransitions",
            "state_domain": state_domain,
            "display_state": display_state,
            "transition_count": transitions.len(),
            "transitions": transitions,
            "proof_tier": proof_tier,
            "coordinates_transition_count": coordinated_count,
            "workflow_coordinated_count": workflow_coordinated_count,
            "read_write_cycle_count": read_write_cycle_count,
            "all_coordinated": all_coordinated,
        }),
        acceptance_criteria: vec![
            format!("state mutations for `{display_state}` route through one canonical transition layer"),
            "branch-local transition logic is reduced or converted to thin delegates".to_string(),
            "graph.json is regenerated and the detector reports at most one transition site for the state domain".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn planner_loop_issue_id(crate_name: &str) -> String {
    format!(
        "auto_planner_loop_fragmentation_{}",
        sanitize_fragment(crate_name)
    )
}

fn workflow_phase_from_domain(raw: &str) -> Option<String> {
    raw.rsplit("workflow::").next().and_then(|phase| {
        let trimmed = phase.trim();
        if trimmed.is_empty() || trimmed == raw {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

type WorkflowOrchestratorRow = (String, Vec<String>, Vec<String>, usize, usize);

fn workflow_phases_by_symbol(idx: &SemanticIndex) -> HashMap<String, HashSet<String>> {
    let mut phases_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, workflow) in idx.workflow_domain_edges() {
        let Some(phase) = workflow_phase_from_domain(&workflow) else {
            continue;
        };
        phases_by_symbol.entry(symbol).or_default().insert(phase);
    }
    phases_by_symbol
}

fn workflow_phase_symbols(
    phases_by_symbol: &HashMap<String, HashSet<String>>,
) -> HashMap<String, HashSet<String>> {
    let mut phase_symbols: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, phases) in phases_by_symbol {
        for phase in phases {
            phase_symbols
                .entry(phase.clone())
                .or_default()
                .insert(symbol.clone());
        }
    }
    phase_symbols
}

fn workflow_call_stats(
    idx: &SemanticIndex,
    phases_by_symbol: &HashMap<String, HashSet<String>>,
) -> (
    HashMap<String, HashSet<String>>,
    HashMap<String, usize>,
    HashMap<String, usize>,
) {
    let mut workflow_callers: HashMap<String, HashSet<String>> = HashMap::new();
    let mut incoming_workflow: HashMap<String, usize> = HashMap::new();
    let mut outgoing_workflow: HashMap<String, usize> = HashMap::new();
    for (from, to) in idx.call_edges() {
        let Some(_from_phases) = phases_by_symbol.get(&from) else {
            continue;
        };
        let Some(to_phases) = phases_by_symbol.get(&to) else {
            continue;
        };
        workflow_callers
            .entry(from.clone())
            .or_default()
            .extend(to_phases.iter().cloned());
        *outgoing_workflow.entry(from).or_insert(0) += 1;
        *incoming_workflow.entry(to).or_insert(0) += 1;
    }
    (workflow_callers, incoming_workflow, outgoing_workflow)
}

fn collect_workflow_orchestrators(
    phases_by_symbol: &HashMap<String, HashSet<String>>,
    workflow_callers: &HashMap<String, HashSet<String>>,
    incoming_workflow: &HashMap<String, usize>,
    outgoing_workflow: &HashMap<String, usize>,
    summary_by_symbol: &HashMap<String, crate::semantic::SymbolSummary>,
) -> Vec<WorkflowOrchestratorRow> {
    let mut orchestrators: Vec<WorkflowOrchestratorRow> = phases_by_symbol
        .iter()
        .filter_map(|(symbol, direct_phases)| {
            let mut direct_vec: Vec<String> = direct_phases.iter().cloned().collect();
            direct_vec.sort();
            let mut reached_vec: Vec<String> = workflow_callers
                .get(symbol)
                .map(|phases| phases.iter().cloned().collect())
                .unwrap_or_default();
            reached_vec.sort();
            reached_vec.dedup();
            let incoming = incoming_workflow.get(symbol).copied().unwrap_or(0);
            let outgoing = outgoing_workflow.get(symbol).copied().unwrap_or(0);
            let root_like = incoming == 0 && outgoing > 0;
            let branch_score = summary_by_symbol
                .get(symbol)
                .and_then(|summary| summary.branch_score)
                .unwrap_or(0.0);
            let coordinating = outgoing >= 4
                && branch_score >= 2.0
                && (root_like || reached_vec.len() >= 2 || direct_vec.len() >= 2);
            coordinating.then_some((symbol.clone(), direct_vec, reached_vec, incoming, outgoing))
        })
        .collect();
    orchestrators.sort_by(|a, b| {
        (b.1.len() + b.2.len())
            .cmp(&(a.1.len() + a.2.len()))
            .then(b.4.cmp(&a.4))
            .then(a.0.len().cmp(&b.0.len()))
            .then(a.0.cmp(&b.0))
    });
    orchestrators
}

fn planner_loop_evidence(
    owner_candidates: &[WorkflowOrchestratorRow],
    orchestrators: &[WorkflowOrchestratorRow],
) -> Vec<String> {
    let mut evidence_rows = owner_candidates.to_vec();
    for row in orchestrators {
        if !evidence_rows.iter().any(|existing| existing.0 == row.0) {
            evidence_rows.push(row.clone());
        }
    }
    evidence_rows
        .iter()
        .take(8)
        .map(|(symbol, direct_phases, reached_phases, incoming, outgoing)| {
            let role = if owner_candidates.iter().any(|row| row.0 == *symbol) {
                "workflow owner candidate"
            } else {
                "strong workflow orchestrator"
            };
            format!(
                "{role} `{symbol}` touches phases [{}] and reaches [{}] (incoming_workflow={incoming}, outgoing_workflow={outgoing})",
                direct_phases.join(", "),
                reached_phases.join(", ")
            )
        })
        .collect()
}

fn planner_loop_row_metrics(rows: &[WorkflowOrchestratorRow]) -> Vec<Value> {
    rows.iter()
        .map(|(symbol, direct_phases, reached_phases, incoming, outgoing)| {
            json!({
                "symbol": symbol,
                "direct_phases": direct_phases,
                "reached_phases": reached_phases,
                "incoming_workflow": incoming,
                "outgoing_workflow": outgoing,
            })
        })
        .collect()
}

fn build_planner_loop_fragmentation_issue(crate_name: &str, idx: &SemanticIndex) -> Issue {
    let summary_by_symbol: HashMap<String, crate::semantic::SymbolSummary> = idx
        .symbol_summaries()
        .into_iter()
        .map(|summary| (summary.symbol.clone(), summary))
        .collect();
    let phases_by_symbol = workflow_phases_by_symbol(idx);
    let phase_symbols = workflow_phase_symbols(&phases_by_symbol);
    let (workflow_callers, incoming_workflow, outgoing_workflow) =
        workflow_call_stats(idx, &phases_by_symbol);
    let orchestrators = collect_workflow_orchestrators(
        &phases_by_symbol,
        &workflow_callers,
        &incoming_workflow,
        &outgoing_workflow,
        &summary_by_symbol,
    );
    let mut owner_candidates: Vec<WorkflowOrchestratorRow> = orchestrators
        .iter()
        .filter(|(_, direct_phases, reached_phases, incoming, outgoing)| {
            *incoming == 0
                && *outgoing >= 4
                && direct_phases.len() >= 3
                && reached_phases.len() >= 3
        })
        .cloned()
        .collect();
    owner_candidates.sort_by(|a, b| {
        (b.1.len() + b.2.len())
            .cmp(&(a.1.len() + a.2.len()))
            .then(b.4.cmp(&a.4))
            .then(a.0.len().cmp(&b.0.len()))
            .then(a.0.cmp(&b.0))
    });

    let phase_count = phase_symbols.len();
    let planner_count = phase_symbols.get("planner").map(|s| s.len()).unwrap_or(0);
    let apply_count = phase_symbols.get("apply").map(|s| s.len()).unwrap_or(0);
    let verify_count = phase_symbols.get("verify").map(|s| s.len()).unwrap_or(0);
    let canonical_target = owner_candidates
        .first()
        .or_else(|| orchestrators.first())
        .map(|row| row.0.clone())
        .unwrap_or_else(|| "app::run_planner_phase".to_string());
    let evidence = planner_loop_evidence(&owner_candidates, &orchestrators);
    let orchestrator_metrics = planner_loop_row_metrics(&orchestrators);
    let owner_candidate_metrics = planner_loop_row_metrics(&owner_candidates);

    Issue {
        id: planner_loop_issue_id(crate_name),
        title: format!(
            "Planner/apply/verify fragmentation in `{}` ({} owner candidates)",
            crate_name,
            owner_candidates.len()
        ),
        status: if owner_candidates.len() > 1 { "open" } else { "resolved" }.to_string(),
        priority: if owner_candidates.len() >= 4 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
             "Compiler-observed workflow-domain functions in crate `{crate_name}` are spread across planner/apply/verify phases \
             without a single clear orchestrator. Detected phase counts: planner={planner_count}, \
             apply={apply_count}, verify={verify_count}. Hypothesis-grade workflow analysis found {} \
             strong orchestrators and {} owner candidates.\n\n\
             This usually means the planner/apply/verify loop is fragmented across multiple entrypoints \
             instead of one canonical control path. Consolidate orchestration around `{canonical_target}` \
             or another single workflow owner, and downgrade the remaining sites to thin delegates.",
            orchestrators.len(),
            owner_candidates.len()
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizePlannerLoop",
            "proof_tier": "hypothesis",
            "phase_count": phase_count,
            "planner_count": planner_count,
            "apply_count": apply_count,
            "verify_count": verify_count,
            "orchestrator_count": orchestrators.len(),
            "owner_candidate_count": owner_candidates.len(),
            "canonical_target_candidate": canonical_target,
            "orchestrators": orchestrator_metrics,
            "owner_candidates": owner_candidate_metrics,
        }),
        acceptance_criteria: vec![
            "planner/apply/verify orchestration is centralized through one canonical control path".to_string(),
            "secondary workflow entrypoints are deleted or reduced to thin delegates".to_string(),
            "graph.json is regenerated and the detector reports at most one owner candidate".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn implicit_state_machine_issue_id(crate_name: &str, symbol: &str) -> String {
    format!(
        "auto_implicit_state_machine_{}_{}",
        sanitize_fragment(crate_name),
        stable_hash(&format!("{crate_name}:{symbol}"))
    )
}

fn implicit_state_machine_candidate_symbol(symbol: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "app::",
        "tools::",
        "system_state::",
        "transition_policy::",
        "canonical_writer::",
        "supervisor::",
    ];
    const EXCLUDED_SUBSTRINGS: &[&str] = &["persist_", "write_", "save_", "append_", "record_"];
    PREFIXES.iter().any(|prefix| symbol.starts_with(prefix))
        && !EXCLUDED_SUBSTRINGS.iter().any(|frag| symbol.contains(frag))
}

fn build_implicit_state_machine_issue(
    crate_name: &str,
    summary: &crate::semantic::SymbolSummary,
    state_domains: &HashSet<String>,
) -> Issue {
    let mut states: Vec<String> = state_domains.iter().cloned().collect();
    states.sort();
    let display_states = states
        .iter()
        .map(|s| display_state_domain(s))
        .collect::<Vec<_>>();
    let evidence = vec![
        format!("state transition domains: {}", display_states.join(", ")),
        format!(
            "branch_score={:.2}, switchint_count={}, has_back_edges={}",
            summary.branch_score.unwrap_or(0.0),
            summary.switchint_count,
            summary.has_back_edges
        ),
    ];

    Issue {
        id: implicit_state_machine_issue_id(crate_name, &summary.symbol),
        title: format!("Implicit state machine candidate in `{}`", summary.symbol),
        status: "open".to_string(),
        priority: if summary.has_back_edges || states.len() >= 2 {
            "high".to_string()
        } else {
            "medium".to_string()
        },
        kind: "logic".to_string(),
        description: format!(
            "Function `{}` in crate `{}` combines compiler-observed state transitions with non-trivial branching. \
             This is a hypothesis-grade signal that the function is encoding a state machine implicitly rather than \
             through an explicit enum + transition table.\n\n\
             Observed state domains: {}. Consider extracting a first-class transition type or routing the mutation \
             through a canonical transition engine.",
            summary.symbol,
            crate_name,
            display_states.join(", ")
        ),
        location: shorten_symbol_location(summary),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "ExtractImplicitStateMachine",
            "proof_tier": "hypothesis",
            "symbol": summary.symbol,
            "state_domains": states,
            "branch_score": summary.branch_score.unwrap_or(0.0),
            "switchint_count": summary.switchint_count,
            "has_back_edges": summary.has_back_edges,
        }),
        acceptance_criteria: vec![
            "branch-driven state mutation is extracted into an explicit state transition abstraction".to_string(),
            "remaining function body delegates to the extracted transition helper or table".to_string(),
            "graph.json is regenerated and the detector no longer reports the function".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn derive_canonical_effect_boundary_modules(
    idx: &SemanticIndex,
    candidate_symbols: &HashSet<String>,
) -> HashSet<String> {
    let mut candidate_modules: HashSet<String> = candidate_symbols
        .iter()
        .map(|symbol| symbol_module(symbol))
        .collect();
    let workflow_modules: HashSet<String> = idx
        .workflow_domain_edges()
        .into_iter()
        .map(|(symbol, _)| symbol_module(&symbol))
        .collect();
    candidate_modules.extend(workflow_modules.iter().cloned());

    let mut incoming_callers_by_module: HashMap<String, HashSet<String>> = HashMap::new();
    for (caller, callee) in idx.call_edges() {
        let callee_module = symbol_module(&callee);
        if !candidate_modules.contains(&callee_module) {
            continue;
        }
        let caller_module = symbol_module(&caller);
        if caller_module != callee_module {
            incoming_callers_by_module
                .entry(callee_module)
                .or_default()
                .insert(caller_module);
        }
    }

    let mut canonical_modules = workflow_modules;
    let mut ordered_modules: Vec<String> = candidate_modules.into_iter().collect();
    ordered_modules.sort();
    let mut changed = true;
    while changed {
        changed = false;
        for module in &ordered_modules {
            let has_observed_canonical_callers = incoming_callers_by_module
                .get(module)
                .filter(|callers| !callers.is_empty())
                .map(|callers| callers.iter().all(|caller| canonical_modules.contains(caller)))
                .unwrap_or(false);
            if has_observed_canonical_callers && canonical_modules.insert(module.clone()) {
                changed = true;
            }
        }
    }

    canonical_modules
}

fn is_canonical_effect_boundary_symbol(
    canonical_modules: &HashSet<String>,
    symbol: &str,
) -> bool {
    canonical_modules.contains(&symbol_module(symbol))
}

fn symbol_module(symbol: &str) -> String {
    symbol
        .rsplit_once("::")
        .map(|(module, _)| module.to_string())
        .unwrap_or_else(|| symbol.to_string())
}

fn normalize_representation_domain(kind: &str, raw: &str) -> String {
    match kind {
        "artifact" => format!("artifact::{}", display_artifact_domain(raw)),
        "state" => format!("state::{}", display_state_domain(raw)),
        _ => format!("{kind}::{raw}"),
    }
}

fn display_representation_domain(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("artifact::") {
        format!("artifact:{rest}")
    } else if let Some(rest) = raw.strip_prefix("state::") {
        format!("state:{rest}")
    } else {
        raw.to_string()
    }
}

fn representation_fanout_issue_id(crate_name: &str, source: &str, target: &str) -> String {
    format!(
        "auto_representation_fanout_{}_{}",
        sanitize_fragment(crate_name),
        stable_hash(&format!("{crate_name}:{source}->{target}"))
    )
}

fn canonical_representation_target(
    symbols: &[String],
    canonical_modules: &HashSet<String>,
) -> String {
    let mut ranked = symbols.to_vec();
    ranked.sort_by(|a, b| {
        is_canonical_effect_boundary_symbol(canonical_modules, b)
            .cmp(&is_canonical_effect_boundary_symbol(canonical_modules, a))
            .then(a.len().cmp(&b.len()))
            .then(a.cmp(b))
    });
    ranked
        .into_iter()
        .next()
        .unwrap_or_else(|| "one canonical translator".to_string())
}

fn effect_boundary_leak_issue_id(crate_name: &str, module: &str) -> String {
    format!(
        "auto_effect_boundary_leak_{}_{}",
        sanitize_fragment(crate_name),
        stable_hash(&format!("{crate_name}:{module}"))
    )
}

fn build_effect_boundary_leak_issue(
    crate_name: &str,
    module: &str,
    rows: &[(String, Vec<&'static str>)],
) -> Issue {
    let canonical_target = "logging / canonical_writer / app orchestration boundary";
    let evidence = rows
        .iter()
        .map(|(symbol, effects)| format!("`{symbol}` directly performs [{}]", effects.join(", ")))
        .collect::<Vec<_>>();
    let symbol_metrics = rows
        .iter()
        .map(|(symbol, effects)| {
            json!({
                "symbol": symbol,
                "effects": effects,
            })
        })
        .collect::<Vec<_>>();

    Issue {
        id: effect_boundary_leak_issue_id(crate_name, module),
        title: format!(
            "Effect boundary leak in `{}` ({} effectful symbols)",
            module,
            rows.len()
        ),
        status: if rows.is_empty() { "resolved" } else { "open" }.to_string(),
        priority: if rows.len() >= 2 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Module `{module}` in crate `{crate_name}` directly mixes multiple effect families instead of routing them through a narrower boundary layer. \
             This is a hypothesis-grade boundary leak signal: symbols in the module are simultaneously handling state/artifact IO and error shaping or transition logic.\n\n\
             Recommended direction: move direct effects behind `{canonical_target}` and leave `{module}` with orchestration or pure transformation responsibilities only."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "RepairEffectBoundary",
            "proof_tier": "hypothesis",
            "module": module,
            "symbol_count": rows.len(),
            "symbols": symbol_metrics,
            "canonical_target_candidate": canonical_target,
        }),
        acceptance_criteria: vec![
            format!("direct multi-effect work is removed from `{module}` or routed through a canonical boundary layer"),
            "remaining symbols in the module focus on orchestration or pure transformation".to_string(),
            "graph.json is regenerated and the detector reports fewer leaked effectful symbols".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn build_representation_fanout_issue(
    crate_name: &str,
    source: &str,
    target: &str,
    symbols: &[String],
    canonical_modules: &HashSet<String>,
) -> Issue {
    let display_source = display_representation_domain(source);
    let display_target = display_representation_domain(target);
    let canonical_target = canonical_representation_target(symbols, canonical_modules);
    let evidence = symbols
        .iter()
        .map(|symbol| format!("translation site `{symbol}` reads `{display_source}` and writes `{display_target}`"))
        .collect::<Vec<_>>();

    Issue {
        id: representation_fanout_issue_id(crate_name, source, target),
        title: format!(
            "Representation fanout from `{display_source}` to `{display_target}` ({} translation sites)",
            symbols.len()
        ),
        status: if symbols.len() > 1 { "open" } else { "resolved" }.to_string(),
        priority: if symbols.len() >= 3 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Crate `{crate_name}` contains multiple translation sites from `{display_source}` to `{display_target}`. \
             This is a hypothesis-grade representation fanout signal: the same domain conversion is implemented in several functions instead of being routed through one canonical translator.\n\n\
             Recommended direction: keep one canonical translation entrypoint such as `{canonical_target}` and redirect the remaining sites through it."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizeRepresentationTranslation",
            "proof_tier": "hypothesis",
            "source_domain": source,
            "target_domain": target,
            "translation_site_count": symbols.len(),
            "symbols": symbols,
            "canonical_target_candidate": canonical_target,
        }),
        acceptance_criteria: vec![
            format!("one canonical translation path remains for `{display_source}` -> `{display_target}`"),
            "other translation sites delegate to the canonical translator or are deleted".to_string(),
            "graph.json is regenerated and the detector reports fewer translation sites for the pair".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn resolve_legacy_cfg_region_issues(file: &mut IssuesFile, crate_name: &str) -> usize {
    let prefix = format!("auto_cfg_region_reduction_{crate_name}_");
    let mut mutated = 0usize;
    for issue in &mut file.issues {
        if issue.id.starts_with(&prefix) && issue.status != "resolved" {
            issue.status = "resolved".to_string();
            mutated += 1;
        }
    }
    mutated
}

fn scc_region_reduction_issue_id(crate_name: &str, symbol: &str) -> String {
    format!(
        "auto_scc_region_reduction_{}_{}",
        sanitize_fragment(crate_name),
        stable_hash(&format!("{crate_name}:{symbol}"))
    )
}

fn build_scc_region_reduction_issue(
    crate_name: &str,
    summary: &crate::semantic::SymbolSummary,
    back_edge_count: usize,
    redundant_path_count: usize,
) -> Issue {
    let branch_score = summary.branch_score.unwrap_or(0.0);
    let evidence = vec![
        format!(
            "branch_score={branch_score:.2}, switchint_count={}, has_back_edges={}",
            summary.switchint_count,
            summary.has_back_edges
        ),
        format!(
            "back_edge_count={back_edge_count}, redundant_path_count={redundant_path_count}"
        ),
    ];

    Issue {
        id: scc_region_reduction_issue_id(crate_name, &summary.symbol),
        title: format!("SCC region reduction candidate in `{}`", summary.symbol),
        status: "open".to_string(),
        priority: if back_edge_count >= 2 || redundant_path_count >= 2 {
            "high".to_string()
        } else {
            "medium".to_string()
        },
        kind: "logic".to_string(),
        description: format!(
            "Function `{}` in crate `{}` contains a loop-heavy SCC control region. \
             The CFG shows explicit back edges and branching pressure concentrated in one cycle-heavy region.\n\n\
             Recommended direction: isolate the SCC behind one dispatcher, collapse loop exits, or extract a smaller transition reducer for the cycle."
            ,
            summary.symbol,
            crate_name
        ),
        location: shorten_symbol_location(summary),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "ReduceSccRegion",
            "proof_tier": "hypothesis",
            "symbol": summary.symbol,
            "region_kind": "loop/SCC",
            "branch_score": branch_score,
            "switchint_count": summary.switchint_count,
            "back_edge_count": back_edge_count,
            "redundant_path_count": redundant_path_count,
        }),
        acceptance_criteria: vec![
            "the loop-heavy SCC is reduced to a simpler control cycle or dispatcher".to_string(),
            "remaining back-edge behavior is routed through one canonical reducer".to_string(),
            "graph.json is regenerated and the detector reports lower back-edge pressure for the function".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn dominator_region_reduction_issue_id(crate_name: &str, symbol: &str) -> String {
    format!(
        "auto_dominator_region_reduction_{}_{}",
        sanitize_fragment(crate_name),
        stable_hash(&format!("{crate_name}:{symbol}"))
    )
}

fn build_dominator_region_reduction_issue(
    crate_name: &str,
    summary: &crate::semantic::SymbolSummary,
    non_cleanup_fraction: f32,
    redundant_path_count: usize,
) -> Issue {
    let branch_score = summary.branch_score.unwrap_or(0.0);
    let mir_blocks = summary.mir_blocks.unwrap_or(0);
    // switchint_density: SwitchInt terminators per MIR block — higher = more dispatch-shaped
    let switchint_density = if mir_blocks > 0 {
        summary.switchint_count as f64 / mir_blocks as f64
    } else {
        0.0
    };
    let evidence = vec![
        format!(
            "branch_score={branch_score:.1}, switchint_count={}, switchint_density={switchint_density:.2}",
            summary.switchint_count,
        ),
        format!(
            "non_cleanup_fraction={non_cleanup_fraction:.2}, redundant_path_count={redundant_path_count}, back_edges=0"
        ),
    ];

    Issue {
        id: dominator_region_reduction_issue_id(crate_name, &summary.symbol),
        title: format!("Dominator region reduction candidate in `{}`", summary.symbol),
        status: "open".to_string(),
        priority: if redundant_path_count >= 2 || summary.switchint_count >= 3 {
            "high".to_string()
        } else {
            "medium".to_string()
        },
        kind: "logic".to_string(),
        description: format!(
            "Function `{}` in crate `{}` is a loop-free, heavily-branching dispatch function \
             (branch_score={branch_score:.1}, switchint_count={}, switchint_density={switchint_density:.2} per block).\n\n\
             Functions with many SwitchInt terminators and no back-edges are candidates for \
             dominator-region reduction: extract a table-driven dispatcher, collapse parallel \
             match arms into a shared helper, or split the function along its natural branch regions.\n\n\
             Recommended direction: identify the dominant SwitchInt bottleneck and extract each \
             branch arm into a named helper, leaving a thin dispatch entry.",
            summary.symbol,
            crate_name,
            summary.switchint_count,
        ),
        location: shorten_symbol_location(summary),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "ReduceDominatorRegion",
            "proof_tier": "hypothesis",
            "symbol": summary.symbol,
            "region_kind": "loop-free dispatch funnel",
            "branch_score": branch_score,
            "switchint_count": summary.switchint_count,
            "switchint_density": switchint_density,
            "non_cleanup_fraction": non_cleanup_fraction,
            "back_edge_count": 0,
            "redundant_path_count": redundant_path_count,
        }),
        acceptance_criteria: vec![
            "the dominant SwitchInt bottleneck is extracted into named branch helpers".to_string(),
            "branch_score and switchint_count decrease measurably after refactor".to_string(),
            "graph.json is regenerated and the detector no longer emits this symbol".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn shorten_symbol_location(summary: &crate::semantic::SymbolSummary) -> String {
    format!("{}:{}", crate::semantic::shorten_display_path(&summary.file), summary.line)
}

fn module_partition_key(summary: &crate::semantic::SymbolSummary) -> String {
    let sym_scope = summary
        .symbol
        .rsplit_once("::")
        .map(|(scope, _)| scope.to_string())
        .unwrap_or_else(|| summary.symbol.clone());
    let file_scope = summary
        .file
        .find("/src/")
        .map(|idx| &summary.file[(idx + 5)..])
        .map(|path| path.trim_end_matches(".rs").replace('/', "::"))
        .unwrap_or_default();
    if !file_scope.is_empty() && sym_scope.contains(&file_scope) {
        file_scope
    } else if !file_scope.is_empty() {
        format!("{file_scope}::{sym_scope}")
    } else {
        sym_scope
    }
}

fn issue_id(crate_name: &str) -> String {
    format!(
        "graph_bridge_connectivity_{}",
        sanitize_fragment(crate_name)
    )
}

fn priority_from_ratio(ratio: f64) -> &'static str {
    if ratio >= 20.0 {
        "high"
    } else if ratio >= DEFAULT_BRIDGE_RATIO_THRESHOLD {
        "medium"
    } else {
        "low"
    }
}

fn build_bridge_issue(stats: &BridgeConnectivityStats) -> Issue {
    let issue_id = issue_id(&stats.crate_name);
    let active = stats.bridge_ratio > stats.threshold;
    let status = if active { "open" } else { "resolved" };
    let priority = priority_from_ratio(stats.bridge_ratio);
    let ratio_text = format!("{:.2}", stats.bridge_ratio);
    let threshold_text = format!("{:.2}", stats.threshold);
    let scope = format!(
        "state/rustc/{}/graph.json",
        stats.crate_name.replace('-', "_")
    );
    let candidate_functions = stats
        .candidate_functions
        .iter()
        .map(|(symbol, count)| json!({ "symbol": symbol, "bridge_calls": count }))
        .collect::<Vec<Value>>();

    Issue {
        id: issue_id,
        title: format!(
            "Bridge connectivity overload in `{}` graph ({ratio_text} bridge edges/node)",
            stats.crate_name
        ),
        status: status.to_string(),
        priority: priority.to_string(),
        kind: "performance".to_string(),
        description: format!(
            "Bridge connectivity is measured as bridge_edge_count / node_count.\n\
             For crate `{crate_name}`:\n\
             - bridge_edge_count = {bridge_edge_count}\n\
             - node_count = {node_count}\n\
             - bridge_ratio = {ratio_text}\n\
             - threshold = {threshold_text}\n\
             - semantic_edge_count = {semantic_edge_count}\n\
             - cfg_node_count = {cfg_node_count}\n\
             - cfg_edge_count = {cfg_edge_count}\n\n\
             This exceeds the detector threshold and indicates the graph is too bridge-dense.\n\
             High bridge density increases coupling, slows traversal, and makes the execution graph\n\
             harder to reason about deterministically.\n\n\
             Candidate functions most frequently touched by bridge-connected call edges:\n\
             {candidates}\n",
            crate_name = stats.crate_name,
            bridge_edge_count = stats.bridge_edge_count,
            node_count = stats.node_count,
            ratio_text = ratio_text,
            threshold_text = threshold_text,
            semantic_edge_count = stats.semantic_edge_count,
            cfg_node_count = stats.cfg_node_count,
            cfg_edge_count = stats.cfg_edge_count,
            candidates = if stats.candidate_functions.is_empty() {
                "(none found)".to_string()
            } else {
                stats
                    .candidate_functions
                    .iter()
                    .map(|(symbol, count)| format!("- {symbol} ({count} bridge call edge(s))"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        ),
        location: scope.clone(),
        metrics: json!({
            "measured": {
                "bridge_edge_count": stats.bridge_edge_count,
                "node_count": stats.node_count,
                "bridge_ratio": stats.bridge_ratio,
                "semantic_edge_count": stats.semantic_edge_count,
                "cfg_node_count": stats.cfg_node_count,
                "cfg_edge_count": stats.cfg_edge_count,
            },
            "target": {
                "bridge_ratio_max": stats.threshold,
                "bridge_edge_count_per_node_max": stats.threshold,
            },
            "candidate_functions": candidate_functions,
        }),
        scope,
        acceptance_criteria: vec![
            format!("bridge_ratio <= {threshold_text}"),
            format!("bridge_edge_count / node_count <= {threshold_text}"),
            "graph.json is regenerated and the detector no longer reports the crate as bridge-dense"
                .to_string(),
        ],
        evidence: vec![
            format!("bridge_edge_count={}", stats.bridge_edge_count),
            format!("node_count={}", stats.node_count),
            format!("bridge_ratio={ratio_text} threshold={threshold_text}"),
        ],
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn is_canonical_logging_module(module: &str) -> bool {
    module == "logging" || module.starts_with("logging::")
}

fn is_canonical_process_boundary(module: &str) -> bool {
    module == "process"
        || module.starts_with("process::")
        || module == "tools"
        || module.starts_with("tools::")
}

fn build_logging_dispersion_issue(
    crate_name: &str,
    by_module: &HashMap<String, Vec<String>>,
) -> Issue {
    let mut non_canonical: Vec<(String, Vec<String>)> = Vec::new();
    let mut canonical_count = 0usize;
    for (module, symbols) in by_module {
        if is_canonical_logging_module(module) {
            canonical_count += symbols.len();
        } else {
            let mut syms = symbols.clone();
            syms.sort();
            non_canonical.push((module.clone(), syms));
        }
    }
    non_canonical.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    let total_non_canonical: usize = non_canonical.iter().map(|(_, s)| s.len()).sum();
    let total = canonical_count + total_non_canonical;

    let evidence = non_canonical
        .iter()
        .take(8)
        .map(|(module, syms)| {
            let sample = syms
                .iter()
                .take(3)
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{module}` has {} direct logging site(s): {sample}", syms.len())
        })
        .collect::<Vec<_>>();

    let module_metrics: Vec<Value> = non_canonical
        .iter()
        .map(|(module, syms)| {
            json!({ "module": module, "direct_logging_count": syms.len(), "symbols": syms })
        })
        .collect();

    Issue {
        id: format!("auto_logging_dispersion_{}", sanitize_fragment(crate_name)),
        title: format!(
            "Logging dispersion in `{crate_name}` ({total_non_canonical} direct sites across {} modules)",
            non_canonical.len()
        ),
        status: if total_non_canonical > 10 { "open" } else { "resolved" }.to_string(),
        priority: if total_non_canonical >= 20 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Crate `{crate_name}` has {total_non_canonical} function(s) outside the `logging` module that \
             perform direct logging calls ({total} total, {canonical_count} in canonical boundary).\n\n\
             Dispersed logging makes it hard to add structured event metadata, change log backends, \
             or enforce consistent log levels. Centralizing logging calls through a structured event \
             emission layer reduces this surface.\n\n\
             Recommended direction: route direct log calls through the `logging` module boundary \
             or emit structured events that the logging layer serializes."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizeLogging",
            "proof_tier": "hypothesis",
            "total_logging_sites": total,
            "canonical_logging_sites": canonical_count,
            "non_canonical_logging_sites": total_non_canonical,
            "non_canonical_module_count": non_canonical.len(),
            "modules": module_metrics,
        }),
        acceptance_criteria: vec![
            format!("direct logging calls outside `logging::` in `{crate_name}` reduced by ≥50%"),
            "remaining direct logging calls are in leaf call sites where delegation adds no value".to_string(),
            "graph.json is regenerated and non_canonical_logging_sites decreases".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn build_process_spawn_dispersion_issue(
    crate_name: &str,
    by_module: &HashMap<String, Vec<String>>,
) -> Issue {
    let mut canonical: Vec<(String, Vec<String>)> = Vec::new();
    let mut non_canonical: Vec<(String, Vec<String>)> = Vec::new();
    for (module, symbols) in by_module {
        let mut syms = symbols.clone();
        syms.sort();
        if is_canonical_process_boundary(module) {
            canonical.push((module.clone(), syms));
        } else {
            non_canonical.push((module.clone(), syms));
        }
    }
    non_canonical.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    canonical.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    let total_non_canonical: usize = non_canonical.iter().map(|(_, s)| s.len()).sum();
    let total_canonical: usize = canonical.iter().map(|(_, s)| s.len()).sum();
    let total = total_canonical + total_non_canonical;

    let evidence = non_canonical
        .iter()
        .map(|(module, syms)| {
            format!(
                "`{module}` spawns processes outside canonical boundary: {}",
                syms.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ")
            )
        })
        .collect::<Vec<_>>();

    let module_metrics: Vec<Value> = {
        let mut all: Vec<(String, Vec<String>)> = by_module
            .iter()
            .map(|(m, s)| {
                let mut syms = s.clone();
                syms.sort();
                (m.clone(), syms)
            })
            .collect();
        all.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        all.iter()
            .map(|(module, syms)| {
                json!({
                    "module": module,
                    "canonical": is_canonical_process_boundary(module),
                    "spawn_count": syms.len(),
                    "symbols": syms,
                })
            })
            .collect()
    };

    Issue {
        id: format!(
            "auto_process_spawn_dispersion_{}",
            sanitize_fragment(crate_name)
        ),
        title: format!(
            "Process spawn dispersion in `{crate_name}` ({total_non_canonical} non-canonical sites across {} modules)",
            non_canonical.len()
        ),
        status: if non_canonical.len() > 1 { "open" } else { "resolved" }.to_string(),
        priority: if non_canonical.len() >= 3 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Crate `{crate_name}` has {total} process-spawning function(s): {total_canonical} in canonical \
             boundary module(s) (`process`, `tools`) and {total_non_canonical} outside.\n\n\
             Process spawning is a high-blast-radius side effect that should flow through a narrow execution boundary. \
             When multiple non-canonical modules spawn subprocesses directly, error handling, environment setup, \
             and stdout/stderr routing tend to diverge.\n\n\
             Recommended direction: route all process spawning through `process::` and restrict callers to that module."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizeProcessSpawning",
            "proof_tier": "hypothesis",
            "total_spawn_sites": total,
            "canonical_spawn_sites": total_canonical,
            "non_canonical_spawn_sites": total_non_canonical,
            "non_canonical_module_count": non_canonical.len(),
            "modules": module_metrics,
        }),
        acceptance_criteria: vec![
            format!("all process spawning in `{crate_name}` routes through `process::` boundary"),
            "non-canonical process-spawning symbols are eliminated or converted to thin delegates".to_string(),
            "graph.json is regenerated and non_canonical_spawn_sites = 0".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn build_network_usage_dispersion_issue(
    crate_name: &str,
    by_module: &HashMap<String, Vec<String>>,
) -> Issue {
    let mut modules: Vec<(String, Vec<String>)> = by_module
        .iter()
        .map(|(module, symbols)| {
            let mut syms = symbols.clone();
            syms.sort();
            (module.clone(), syms)
        })
        .collect();
    modules.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    let total: usize = modules.iter().map(|(_, s)| s.len()).sum();
    let module_count = modules.len();

    let evidence = modules
        .iter()
        .map(|(module, syms)| {
            format!(
                "`{module}` uses network I/O: {}",
                syms.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ")
            )
        })
        .collect::<Vec<_>>();

    let module_metrics: Vec<Value> = modules
        .iter()
        .map(|(module, syms)| {
            json!({ "module": module, "network_site_count": syms.len(), "symbols": syms })
        })
        .collect();

    Issue {
        id: format!(
            "auto_network_usage_dispersion_{}",
            sanitize_fragment(crate_name)
        ),
        title: format!(
            "Network usage dispersion in `{crate_name}` ({total} sites across {module_count} modules)"
        ),
        status: if total > 0 { "open" } else { "resolved" }.to_string(),
        priority: if module_count >= 3 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Crate `{crate_name}` has {total} network-using function(s) across {module_count} module(s).\n\n\
             No canonical network access layer has been identified. Network calls scattered across modules \
             resist uniform retry/timeout policy, make offline testing harder, and leak transport \
             concerns into business logic.\n\n\
             Recommended direction: establish a dedicated network access layer (e.g. `llm_runtime::`) \
             and route all network I/O through it."
        ),
        location: format!("state/rustc/{crate_name}/graph.json"),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "CentralizeNetworkAccess",
            "proof_tier": "hypothesis",
            "total_network_sites": total,
            "module_count": module_count,
            "modules": module_metrics,
        }),
        acceptance_criteria: vec![
            format!("all network I/O in `{crate_name}` routes through one canonical access layer"),
            "modules outside the network layer do not hold direct network-using symbols".to_string(),
            "graph.json is regenerated and network sites collapse to one module".to_string(),
        ],
        evidence,
        discovered_by: "graph_metrics_detector".to_string(),
        ..Issue::default()
    }
}

fn upsert_bridge_issue(file: &mut IssuesFile, desired: Issue, active: bool) -> usize {
    let issue_id = desired.id.clone();
    let mut mutated = 0usize;
    match file.issues.iter_mut().find(|issue| issue.id == issue_id) {
        Some(existing) => {
            if existing.title != desired.title {
                existing.title = desired.title.clone();
                mutated += 1;
            }
            let target_status = if active { "open" } else { "resolved" };
            if existing.status != target_status {
                existing.status = target_status.to_string();
                mutated += 1;
            }
            if existing.priority != desired.priority {
                existing.priority = desired.priority.clone();
                mutated += 1;
            }
            if existing.kind != desired.kind {
                existing.kind = desired.kind.clone();
                mutated += 1;
            }
            if existing.description != desired.description {
                existing.description = desired.description.clone();
                mutated += 1;
            }
            if existing.location != desired.location {
                existing.location = desired.location.clone();
                mutated += 1;
            }
            if existing.metrics != desired.metrics {
                existing.metrics = desired.metrics.clone();
                mutated += 1;
            }
            if existing.scope != desired.scope {
                existing.scope = desired.scope.clone();
                mutated += 1;
            }
            if existing.acceptance_criteria != desired.acceptance_criteria {
                existing.acceptance_criteria = desired.acceptance_criteria.clone();
                mutated += 1;
            }
            if existing.evidence != desired.evidence {
                existing.evidence = desired.evidence.clone();
                mutated += 1;
            }
            if existing.discovered_by != desired.discovered_by {
                existing.discovered_by = desired.discovered_by.clone();
                mutated += 1;
            }
        }
        None => {
            if active {
                file.issues.push(desired);
                mutated += 1;
            }
        }
    }
    mutated
}

#[cfg(test)]
mod tests {
    use super::{
        analyze_bridge_connectivity, generate_artifact_writer_dispersion_issues,
        generate_bridge_connectivity_issues, generate_dominator_region_reduction_issues,
        generate_effect_boundary_leak_issues, generate_error_shaping_dispersion_issues,
        generate_implicit_state_machine_issues, generate_logging_dispersion_issues,
        generate_network_usage_dispersion_issues, generate_planner_loop_fragmentation_issues,
        generate_process_spawn_dispersion_issues, generate_representation_fanout_issues,
        generate_scc_region_reduction_issues, generate_state_transition_dispersion_issues,
        issue_id, priority_from_ratio, sanitize_fragment,
    };
    use crate::constants::ISSUES_FILE;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn unique_workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "canon-mini-agent-graph-metrics-{name}-{}-{}",
            std::process::id(),
            crate::logging::now_ms()
        ));
        fs::create_dir_all(&path).expect("create temp workspace");
        path
    }

    fn write_index(workspace: &Path, crates: &[&str]) {
        let rustc_dir = workspace.join("state").join("rustc");
        fs::create_dir_all(&rustc_dir).expect("create rustc dir");
        let mut index = serde_json::Map::new();
        for crate_name in crates {
            index.insert((*crate_name).to_string(), json!({}));
        }
        fs::write(
            rustc_dir.join("index.json"),
            serde_json::to_string_pretty(&serde_json::Value::Object(index)).unwrap(),
        )
        .expect("write index");
    }

    fn graph_node(symbol: &str) -> serde_json::Value {
        json!({
            "kind": "fn",
            "path": symbol,
            "def": {
                "file": format!("src/{}.rs", symbol.replace("::", "_")),
                "line": 1,
                "col": 1,
                "start_offset": 0,
                "end_offset": 10,
            },
            "refs": [],
            "signature": "()",
            "mir": {
                "fingerprint": format!("fp_{symbol}"),
                "blocks": 1,
                "stmts": 1,
            },
        })
    }

    fn write_graph(
        workspace: &Path,
        crate_name: &str,
        node_symbols: &[&str],
        bridge_calls: &[(&str, &str)],
        extra_bridge_edges: usize,
    ) {
        let crate_dir = workspace.join("state").join("rustc").join(crate_name);
        fs::create_dir_all(&crate_dir).expect("create crate dir");

        let mut nodes = serde_json::Map::new();
        for symbol in node_symbols {
            nodes.insert((*symbol).to_string(), graph_node(symbol));
        }

        let mut bridge_edges = Vec::new();
        for (idx, (from, to)) in bridge_calls.iter().enumerate() {
            bridge_edges.push(json!({
                "relation": "Call",
                "from": format!("{from}::{idx}"),
                "to": *to,
            }));
        }
        for idx in 0..extra_bridge_edges {
            bridge_edges.push(json!({
                "relation": if idx % 2 == 0 { "Entry" } else { "BelongsTo" },
                "from": format!("cfg::{crate_name}::bb{idx}"),
                "to": format!("{crate_name}::owner"),
            }));
        }

        let graph = json!({
            "nodes": nodes,
            "edges": [],
            "cfg_nodes": {},
            "cfg_edges": [],
            "bridge_edges": bridge_edges,
        });
        fs::write(
            crate_dir.join("graph.json"),
            serde_json::to_string_pretty(&graph).expect("serialize graph"),
        )
        .expect("write graph");
    }

    fn write_graph_with_edges(
        workspace: &Path,
        crate_name: &str,
        node_symbols: &[&str],
        edges: Vec<serde_json::Value>,
    ) {
        let crate_dir = workspace.join("state").join("rustc").join(crate_name);
        fs::create_dir_all(&crate_dir).expect("create crate dir");

        let mut nodes = serde_json::Map::new();
        for symbol in node_symbols {
            nodes.insert((*symbol).to_string(), graph_node(symbol));
        }
        for edge in &edges {
            if let Some(to) = edge.get("to").and_then(|v| v.as_str()) {
                let key = to.strip_prefix("path::").unwrap_or(to);
                if key.starts_with("artifact::") {
                    nodes.entry(to.to_string()).or_insert(json!({
                        "def_id": to,
                        "path": key,
                        "kind": "external",
                    }));
                }
            }
        }

        let graph = json!({
            "nodes": nodes,
            "edges": edges,
            "cfg_nodes": {},
            "cfg_edges": [],
            "bridge_edges": [],
        });
        fs::write(
            crate_dir.join("graph.json"),
            serde_json::to_string_pretty(&graph).expect("serialize graph"),
        )
        .expect("write graph");
    }

    fn read_issues(workspace: &Path) -> Vec<crate::issues::Issue> {
        let path = workspace.join(ISSUES_FILE);
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => panic!("read issues: {err:?}"),
        };
        let file: crate::issues::IssuesFile = serde_json::from_str(&raw).expect("parse issues");
        file.issues
    }

    #[test]
    fn issue_id_is_stable_and_sanitized() {
        assert_eq!(
            issue_id("canon-mini-agent"),
            "graph_bridge_connectivity_canon_mini_agent"
        );
        assert_eq!(sanitize_fragment("a/b:c"), "a_b_c");
    }

    #[test]
    fn ratio_threshold_bands_are_reasonable() {
        assert_eq!(priority_from_ratio(22.0), "high");
        assert_eq!(priority_from_ratio(10.0), "medium");
        assert_eq!(priority_from_ratio(2.0), "low");
    }

    #[test]
    fn analyze_uses_bridge_call_edges_for_candidate_ranking() {
        let workspace = unique_workspace("candidate-ranking");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph(
            &workspace,
            "canon_mini_agent",
            &["foo::hot", "foo::cool"],
            &[
                ("cfg::foo::bb0", "foo::hot"),
                ("cfg::foo::bb1", "foo::hot"),
                ("cfg::foo::bb2", "foo::hot"),
                ("cfg::foo::bb3", "foo::cool"),
            ],
            0,
        );

        let stats = analyze_bridge_connectivity(&workspace, "canon_mini_agent").unwrap();
        assert_eq!(stats.bridge_edge_count, 4);
        assert_eq!(stats.candidate_functions[0], ("foo::hot".to_string(), 3));
        assert_eq!(stats.candidate_functions[1], ("foo::cool".to_string(), 1));
    }

    #[test]
    fn generator_opens_and_resolves_without_duplication() {
        let workspace = unique_workspace("open-resolve");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph(
            &workspace,
            "canon_mini_agent",
            &["foo::hot"],
            &[
                ("cfg::foo::bb0", "foo::hot"),
                ("cfg::foo::bb1", "foo::hot"),
                ("cfg::foo::bb2", "foo::hot"),
                ("cfg::foo::bb3", "foo::hot"),
                ("cfg::foo::bb4", "foo::hot"),
                ("cfg::foo::bb5", "foo::hot"),
                ("cfg::foo::bb6", "foo::hot"),
                ("cfg::foo::bb7", "foo::hot"),
                ("cfg::foo::bb8", "foo::hot"),
                ("cfg::foo::bb9", "foo::hot"),
                ("cfg::foo::bb10", "foo::hot"),
                ("cfg::foo::bb11", "foo::hot"),
            ],
            0,
        );

        assert!(generate_bridge_connectivity_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].status, "open");

        write_graph(&workspace, "canon_mini_agent", &["foo::hot"], &[], 0);
        assert!(generate_bridge_connectivity_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].status, "resolved");

        assert_eq!(generate_bridge_connectivity_issues(&workspace).unwrap(), 0);
        let issues = read_issues(&workspace);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].status, "resolved");
    }

    #[test]
    fn missing_graph_is_a_noop() {
        let workspace = unique_workspace("missing-graph");
        write_index(&workspace, &["canon_mini_agent"]);
        assert_eq!(generate_bridge_connectivity_issues(&workspace).unwrap(), 0);
        assert!(
            !workspace.join(ISSUES_FILE).exists(),
            "no issues file should be written when graph is missing"
        );
    }

    #[test]
    fn artifact_writer_dispersion_emits_issue_for_shared_artifact_domain() {
        let workspace = unique_workspace("artifact-writer-dispersion");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["logging::persist_prompt_overflow_report", "tools::save_violations"],
            vec![
                json!({
                    "relation": "WritesArtifact",
                    "from": "logging::persist_prompt_overflow_report",
                    "to": "path::artifact::VIOLATIONS_FILE",
                }),
                json!({
                    "relation": "WritesArtifact",
                    "from": "tools::save_violations",
                    "to": "path::artifact::VIOLATIONS_FILE",
                }),
            ],
        );

        assert!(generate_artifact_writer_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_artifact_writer_dispersion_"))
            .expect("artifact writer issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["writer_count"].as_u64(), Some(2));
        assert_eq!(issue.metrics["display_artifact"].as_str(), Some("VIOLATIONS_FILE"));
    }

    #[test]
    fn error_shaping_dispersion_emits_issue_for_shared_styles() {
        let workspace = unique_workspace("error-shaping-dispersion");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &[
                "semantic::SemanticIndex::load",
                "logging::write_projection_with_artifact_effects",
                "tools::load_git_head",
                "tools::exec_python",
            ],
            vec![
                json!({
                    "relation": "ShapesError",
                    "from": "semantic::SemanticIndex::load",
                    "to": "error_shape::with_context",
                }),
                json!({
                    "relation": "ShapesError",
                    "from": "logging::write_projection_with_artifact_effects",
                    "to": "error_shape::with_context",
                }),
                json!({
                    "relation": "ShapesError",
                    "from": "tools::load_git_head",
                    "to": "error_shape::context",
                }),
                json!({
                    "relation": "ShapesError",
                    "from": "tools::exec_python",
                    "to": "error_shape::map_err",
                }),
            ],
        );

        assert!(generate_error_shaping_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_error_shaping_dispersion_"))
            .expect("error shaping issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["function_count"].as_u64(), Some(4));
        assert_eq!(issue.metrics["style_count"].as_u64(), Some(3));
    }

    #[test]
    fn state_transition_dispersion_emits_issue_for_shared_state_domain() {
        let workspace = unique_workspace("state-transition-dispersion");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &[
                "planner::advance_plan_state",
                "tools::save_violations_after_review",
            ],
            vec![
                json!({
                    "relation": "TransitionsState",
                    "from": "planner::advance_plan_state",
                    "to": "state::VIOLATIONS_FILE",
                }),
                json!({
                    "relation": "TransitionsState",
                    "from": "tools::save_violations_after_review",
                    "to": "state::VIOLATIONS_FILE",
                }),
            ],
        );

        assert!(generate_state_transition_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_state_transition_dispersion_"))
            .expect("state transition issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["transition_count"].as_u64(), Some(2));
        assert_eq!(issue.metrics["display_state"].as_str(), Some("VIOLATIONS_FILE"));
    }

    #[test]
    fn planner_loop_fragmentation_emits_issue_for_multiple_workflow_orchestrators() {
        let workspace = unique_workspace("planner-loop-fragmentation");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &[
                "app::run_planner_phase",
                "tools::handle_apply_patch_action",
                "tools::verify_apply_patch_crate",
                "app::apply_wake_signals",
                "tools::handle_plan_action",
                "tools::apply_patch_queue",
                "tools::dispatch_verify_followups",
            ],
            vec![
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::run_planner_phase",
                    "to": "workflow::planner",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::run_planner_phase",
                    "to": "workflow::apply",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::run_planner_phase",
                    "to": "workflow::verify",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "tools::handle_apply_patch_action",
                    "to": "workflow::apply",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "tools::verify_apply_patch_crate",
                    "to": "workflow::verify",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::apply_wake_signals",
                    "to": "workflow::apply",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::apply_wake_signals",
                    "to": "workflow::planner",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::apply_wake_signals",
                    "to": "workflow::verify",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "tools::handle_plan_action",
                    "to": "workflow::planner",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "tools::apply_patch_queue",
                    "to": "workflow::apply",
                }),
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "tools::dispatch_verify_followups",
                    "to": "workflow::verify",
                }),
                json!({
                    "relation": "call",
                    "from": "app::run_planner_phase",
                    "to": "tools::handle_apply_patch_action",
                }),
                json!({
                    "relation": "call",
                    "from": "app::run_planner_phase",
                    "to": "tools::verify_apply_patch_crate",
                }),
                json!({
                    "relation": "call",
                    "from": "app::run_planner_phase",
                    "to": "tools::handle_plan_action",
                }),
                json!({
                    "relation": "call",
                    "from": "app::run_planner_phase",
                    "to": "tools::apply_patch_queue",
                }),
                json!({
                    "relation": "call",
                    "from": "app::apply_wake_signals",
                    "to": "tools::handle_plan_action",
                }),
                json!({
                    "relation": "call",
                    "from": "app::apply_wake_signals",
                    "to": "tools::verify_apply_patch_crate",
                }),
                json!({
                    "relation": "call",
                    "from": "app::apply_wake_signals",
                    "to": "tools::handle_apply_patch_action",
                }),
                json!({
                    "relation": "call",
                    "from": "app::apply_wake_signals",
                    "to": "tools::dispatch_verify_followups",
                }),
            ],
        );
        let graph_path = workspace
            .join("state")
            .join("rustc")
            .join("canon_mini_agent")
            .join("graph.json");
        let mut graph: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&graph_path).unwrap()).unwrap();
        graph["cfg_nodes"] = json!({
            "cfg::app::run_planner_phase::bb0": {
                "owner": "app::run_planner_phase",
                "block": 0,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::app::run_planner_phase::bb1": {
                "owner": "app::run_planner_phase",
                "block": 1,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::app::apply_wake_signals::bb0": {
                "owner": "app::apply_wake_signals",
                "block": 0,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::app::apply_wake_signals::bb1": {
                "owner": "app::apply_wake_signals",
                "block": 1,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            }
        });
        fs::write(&graph_path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();

        assert!(generate_planner_loop_fragmentation_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_planner_loop_fragmentation_"))
            .expect("planner loop issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["orchestrator_count"].as_u64(), Some(2));
        assert_eq!(issue.metrics["owner_candidate_count"].as_u64(), Some(2));
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
    }

    #[test]
    fn implicit_state_machine_emits_issue_for_branching_state_transition_function() {
        let workspace = unique_workspace("implicit-state-machine");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["app::apply_wake_signals"],
            vec![json!({
                "relation": "TransitionsState",
                "from": "app::apply_wake_signals",
                "to": "state::VIOLATIONS_FILE",
            })],
        );

        let graph_path = workspace
            .join("state")
            .join("rustc")
            .join("canon_mini_agent")
            .join("graph.json");
        let mut graph: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&graph_path).expect("read graph")).unwrap();
        graph["nodes"]["app::apply_wake_signals"]["mir"] = json!({
            "fingerprint": "fp_apply_wake_signals",
            "blocks": 5,
            "stmts": 9,
        });
        let def = graph["nodes"]["app::apply_wake_signals"]["def"].clone();
        graph["nodes"]["app::apply_wake_signals"]["def"] = def;
        graph["cfg_nodes"] = json!({
            "cfg::app::apply_wake_signals::bb0": {
                "owner": "app::apply_wake_signals",
                "block": 0,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::app::apply_wake_signals::bb1": {
                "owner": "app::apply_wake_signals",
                "block": 1,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::app::apply_wake_signals::bb2": {
                "owner": "app::apply_wake_signals",
                "block": 2,
                "is_cleanup": false,
                "terminator": "Goto",
                "statements": [],
                "in_loop": true
            }
        });
        graph["cfg_edges"] = json!([
            {
                "relation": "normal",
                "from": "cfg::app::apply_wake_signals::bb2",
                "to": "cfg::app::apply_wake_signals::bb0",
                "is_back_edge": true
            }
        ]);
        fs::write(&graph_path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();

        assert!(generate_implicit_state_machine_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_implicit_state_machine_"))
            .expect("implicit state machine issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
    }

    #[test]
    fn effect_boundary_leak_emits_issue_for_non_boundary_module() {
        let workspace = unique_workspace("effect-boundary-leak");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["plan_preflight::try_preflight_ready_tasks"],
            vec![
                json!({
                    "relation": "ReadsArtifact",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::artifact::MASTER_PLAN_FILE",
                }),
                json!({
                    "relation": "ReadsState",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::state::MASTER_PLAN_FILE",
                }),
                json!({
                    "relation": "WritesArtifact",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::artifact::last_planner_blocker_evidence.txt",
                }),
                json!({
                    "relation": "WritesState",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::state::last_planner_blocker_evidence.txt",
                }),
                json!({
                    "relation": "TransitionsState",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::state::last_planner_blocker_evidence.txt",
                }),
            ],
        );

        assert!(generate_effect_boundary_leak_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_effect_boundary_leak_"))
            .expect("effect boundary leak issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
        assert_eq!(
            issue.metrics["module"].as_str(),
            Some("plan_preflight")
        );
    }

    #[test]
    fn effect_boundary_leak_skips_workflow_derived_boundary_module() {
        let workspace = unique_workspace("effect-boundary-derived-boundary");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["app::run", "issues::persist_projection"],
            vec![
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::run",
                    "to": "path::workflow::planner",
                }),
                json!({
                    "relation": "Calls",
                    "from": "app::run",
                    "to": "issues::persist_projection",
                }),
                json!({
                    "relation": "ReadsArtifact",
                    "from": "issues::persist_projection",
                    "to": "path::artifact::ISSUES.json",
                }),
                json!({
                    "relation": "WritesArtifact",
                    "from": "issues::persist_projection",
                    "to": "path::artifact::ISSUES.json",
                }),
                json!({
                    "relation": "ShapesError",
                    "from": "issues::persist_projection",
                    "to": "shape::issues::projection",
                }),
            ],
        );

        assert_eq!(generate_effect_boundary_leak_issues(&workspace).unwrap(), 0);
        assert!(read_issues(&workspace).is_empty());
    }

    #[test]
    fn representation_fanout_emits_issue_for_repeated_translation_pair() {
        let workspace = unique_workspace("representation-fanout");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &[
                "plan_preflight::try_preflight_ready_tasks",
                "complexity::compute_and_persist_fingerprint_drift",
            ],
            vec![
                json!({
                    "relation": "ReadsArtifact",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::artifact::MASTER_PLAN_FILE",
                }),
                json!({
                    "relation": "WritesArtifact",
                    "from": "plan_preflight::try_preflight_ready_tasks",
                    "to": "path::artifact::last_planner_blocker_evidence.txt",
                }),
                json!({
                    "relation": "ReadsArtifact",
                    "from": "complexity::compute_and_persist_fingerprint_drift",
                    "to": "path::artifact::MASTER_PLAN_FILE",
                }),
                json!({
                    "relation": "WritesArtifact",
                    "from": "complexity::compute_and_persist_fingerprint_drift",
                    "to": "path::artifact::last_planner_blocker_evidence.txt",
                }),
            ],
        );

        assert!(generate_representation_fanout_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_representation_fanout_"))
            .expect("representation fanout issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
        assert_eq!(issue.metrics["translation_site_count"].as_u64(), Some(2));
    }

    #[test]
    fn scc_region_reduction_emits_issue_for_loop_dominated_function() {
        let workspace = unique_workspace("cfg-region-reduction");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["app::apply_wake_signals"],
            vec![],
        );
        let graph_path = workspace
            .join("state")
            .join("rustc")
            .join("canon_mini_agent")
            .join("graph.json");
        let mut graph: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&graph_path).unwrap()).unwrap();
        graph["nodes"]["app::apply_wake_signals"]["mir"] = json!({
            "fingerprint": "fp_apply_wake_signals",
            "blocks": 4,
            "stmts": 8,
        });
        graph["cfg_nodes"] = json!({
            "cfg::app::apply_wake_signals::bb0": {
                "owner": "app::apply_wake_signals",
                "block": 0,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::app::apply_wake_signals::bb1": {
                "owner": "app::apply_wake_signals",
                "block": 1,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": true
            },
            "cfg::app::apply_wake_signals::bb2": {
                "owner": "app::apply_wake_signals",
                "block": 2,
                "is_cleanup": false,
                "terminator": "Goto",
                "statements": [],
                "in_loop": true
            }
        });
        graph["cfg_edges"] = json!([
            {
                "relation": "normal",
                "from": "cfg::app::apply_wake_signals::bb0",
                "to": "cfg::app::apply_wake_signals::bb1",
                "is_back_edge": false
            },
            {
                "relation": "normal",
                "from": "cfg::app::apply_wake_signals::bb1",
                "to": "cfg::app::apply_wake_signals::bb2",
                "is_back_edge": false
            },
            {
                "relation": "normal",
                "from": "cfg::app::apply_wake_signals::bb2",
                "to": "cfg::app::apply_wake_signals::bb1",
                "is_back_edge": true
            }
        ]);
        graph["bridge_edges"] = json!([
            {
                "relation": "BelongsTo",
                "from": "cfg::app::apply_wake_signals::bb0",
                "to": "app::apply_wake_signals"
            },
            {
                "relation": "BelongsTo",
                "from": "cfg::app::apply_wake_signals::bb1",
                "to": "app::apply_wake_signals"
            },
            {
                "relation": "BelongsTo",
                "from": "cfg::app::apply_wake_signals::bb2",
                "to": "app::apply_wake_signals"
            }
        ]);
        fs::write(&graph_path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();

        assert!(generate_scc_region_reduction_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_scc_region_reduction_"))
            .expect("scc region issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
        assert_eq!(issue.metrics["back_edge_count"].as_u64(), Some(1));
    }

    #[test]
    fn dominator_region_reduction_emits_issue_for_branch_funnel_function() {
        let workspace = unique_workspace("dominator-region-reduction");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["tools::handle_plan_action"],
            vec![],
        );
        let graph_path = workspace
            .join("state")
            .join("rustc")
            .join("canon_mini_agent")
            .join("graph.json");
        let mut graph: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&graph_path).unwrap()).unwrap();
        graph["nodes"]["tools::handle_plan_action"]["mir"] = json!({
            "fingerprint": "fp_handle_plan_action",
            "blocks": 4,
            "stmts": 8,
        });
        graph["cfg_nodes"] = json!({
            "cfg::tools::handle_plan_action::bb0": {
                "owner": "tools::handle_plan_action",
                "block": 0,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::tools::handle_plan_action::bb1": {
                "owner": "tools::handle_plan_action",
                "block": 1,
                "is_cleanup": false,
                "terminator": "SwitchInt",
                "statements": [],
                "in_loop": false
            },
            "cfg::tools::handle_plan_action::bb2": {
                "owner": "tools::handle_plan_action",
                "block": 2,
                "is_cleanup": false,
                "terminator": "Goto",
                "statements": [],
                "in_loop": false
            }
        });
        graph["bridge_edges"] = json!([
            {
                "relation": "BelongsTo",
                "from": "cfg::tools::handle_plan_action::bb0",
                "to": "tools::handle_plan_action"
            },
            {
                "relation": "BelongsTo",
                "from": "cfg::tools::handle_plan_action::bb1",
                "to": "tools::handle_plan_action"
            },
            {
                "relation": "BelongsTo",
                "from": "cfg::tools::handle_plan_action::bb2",
                "to": "tools::handle_plan_action"
            },
            {
                "relation": "BelongsTo",
                "from": "cfg::tools::handle_plan_action::cleanup",
                "to": "tools::handle_plan_action"
            }
        ]);
        graph["redundant_paths"] = json!([
            {
                "signature": "dup-path",
                "shared_prefix_len": 2,
                "path_a": {
                    "owner": "tools::handle_plan_action",
                    "blocks": [0, 1, 2]
                },
                "path_b": {
                    "owner": "tools::handle_plan_action",
                    "blocks": [0, 1, 3]
                }
            }
        ]);
        fs::write(&graph_path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();

        assert!(generate_dominator_region_reduction_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_dominator_region_reduction_"))
            .expect("dominator region issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
        assert_eq!(issue.metrics["region_kind"].as_str(), Some("loop-free dispatch funnel"));
        assert_eq!(issue.metrics["back_edge_count"].as_u64(), Some(0));
        assert_eq!(issue.metrics["redundant_path_count"].as_u64(), Some(1));
        assert!(issue.metrics.get("non_cleanup_fraction").is_some());
        assert!(issue.metrics.get("switchint_density").is_some());
    }

    #[test]
    fn multiple_crates_keep_distinct_ids_and_independent_lifecycle() {
        let workspace = unique_workspace("multi-crate");
        write_index(&workspace, &["crate_a", "crate_b"]);
        write_graph(
            &workspace,
            "crate_a",
            &["a::one"],
            &[
                ("cfg::a::bb0", "a::one"),
                ("cfg::a::bb1", "a::one"),
                ("cfg::a::bb2", "a::one"),
                ("cfg::a::bb3", "a::one"),
                ("cfg::a::bb4", "a::one"),
                ("cfg::a::bb5", "a::one"),
                ("cfg::a::bb6", "a::one"),
                ("cfg::a::bb7", "a::one"),
                ("cfg::a::bb8", "a::one"),
                ("cfg::a::bb9", "a::one"),
                ("cfg::a::bb10", "a::one"),
                ("cfg::a::bb11", "a::one"),
            ],
            0,
        );
        write_graph(
            &workspace,
            "crate_b",
            &["b::one"],
            &[
                ("cfg::b::bb0", "b::one"),
                ("cfg::b::bb1", "b::one"),
                ("cfg::b::bb2", "b::one"),
                ("cfg::b::bb3", "b::one"),
                ("cfg::b::bb4", "b::one"),
                ("cfg::b::bb5", "b::one"),
                ("cfg::b::bb6", "b::one"),
                ("cfg::b::bb7", "b::one"),
                ("cfg::b::bb8", "b::one"),
                ("cfg::b::bb9", "b::one"),
                ("cfg::b::bb10", "b::one"),
                ("cfg::b::bb11", "b::one"),
                ("cfg::b::bb12", "b::one"),
            ],
            0,
        );

        assert!(generate_bridge_connectivity_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        assert_eq!(issues.len(), 2);
        let ids: std::collections::HashSet<_> = issues.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains("graph_bridge_connectivity_crate_a"));
        assert!(ids.contains("graph_bridge_connectivity_crate_b"));

        write_graph(&workspace, "crate_a", &["a::one"], &[], 0);
        assert!(generate_bridge_connectivity_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let crate_a = issues
            .iter()
            .find(|i| i.id == "graph_bridge_connectivity_crate_a")
            .expect("crate_a issue");
        let crate_b = issues
            .iter()
            .find(|i| i.id == "graph_bridge_connectivity_crate_b")
            .expect("crate_b issue");
        assert_eq!(crate_a.status, "resolved");
        assert_eq!(crate_b.status, "open");
    }

    #[test]
    fn logging_dispersion_emits_issue_for_non_canonical_logging_sites() {
        let workspace = unique_workspace("logging-dispersion");
        write_index(&workspace, &["canon_mini_agent"]);
        let all_syms: Vec<String> = (0..12)
            .map(|i| format!("tools::run_action_{i}"))
            .chain((0..6).map(|i| format!("planner::step_{i}")))
            .collect();
        let node_symbols: Vec<&str> = all_syms.iter().map(|s| s.as_str()).collect();
        let edges: Vec<serde_json::Value> = all_syms
            .iter()
            .map(|sym| {
                json!({
                    "relation": "PerformsLogging",
                    "from": sym,
                    "to": "effect::logging",
                })
            })
            .collect();
        write_graph_with_edges(&workspace, "canon_mini_agent", &node_symbols, edges);

        assert!(generate_logging_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|i| i.id.starts_with("auto_logging_dispersion_"))
            .expect("logging dispersion issue");
        assert_eq!(issue.status, "open");
        assert!(
            issue.metrics["non_canonical_logging_sites"]
                .as_u64()
                .unwrap_or(0)
                > 10
        );
    }

    #[test]
    fn process_spawn_dispersion_emits_issue_for_multiple_non_canonical_modules() {
        let workspace = unique_workspace("process-spawn-dispersion");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &[
                "process::run_command",
                "tools::exec_git",
                "planner::invoke_rustc",
                "engine::spawn_worker",
            ],
            vec![
                json!({ "relation": "SpawnsProcess", "from": "process::run_command", "to": "effect::process_spawn" }),
                json!({ "relation": "SpawnsProcess", "from": "tools::exec_git", "to": "effect::process_spawn" }),
                json!({ "relation": "SpawnsProcess", "from": "planner::invoke_rustc", "to": "effect::process_spawn" }),
                json!({ "relation": "SpawnsProcess", "from": "engine::spawn_worker", "to": "effect::process_spawn" }),
            ],
        );

        assert!(generate_process_spawn_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|i| i.id.starts_with("auto_process_spawn_dispersion_"))
            .expect("process spawn dispersion issue");
        assert_eq!(issue.status, "open");
        assert!(
            issue.metrics["non_canonical_module_count"]
                .as_u64()
                .unwrap_or(0)
                >= 2
        );
    }

    #[test]
    fn network_usage_dispersion_emits_issue_when_network_sites_exist() {
        let workspace = unique_workspace("network-usage-dispersion");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &["llm_runtime::send_request", "tools::fetch_remote"],
            vec![
                json!({ "relation": "UsesNetwork", "from": "llm_runtime::send_request", "to": "effect::network" }),
                json!({ "relation": "UsesNetwork", "from": "tools::fetch_remote", "to": "effect::network" }),
            ],
        );

        assert!(generate_network_usage_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|i| i.id.starts_with("auto_network_usage_dispersion_"))
            .expect("network usage dispersion issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["total_network_sites"].as_u64(), Some(2));
        assert_eq!(issue.metrics["module_count"].as_u64(), Some(2));
    }

    #[test]
    fn state_transition_dispersion_records_coordinates_transition_proof() {
        let workspace = unique_workspace("state-transition-coordinated");
        write_index(&workspace, &["canon_mini_agent"]);
        write_graph_with_edges(
            &workspace,
            "canon_mini_agent",
            &[
                "planner::advance_plan_state",
                "tools::save_violations_after_review",
            ],
            vec![
                json!({ "relation": "TransitionsState", "from": "planner::advance_plan_state", "to": "state::VIOLATIONS_FILE" }),
                json!({ "relation": "TransitionsState", "from": "tools::save_violations_after_review", "to": "state::VIOLATIONS_FILE" }),
                json!({ "relation": "CoordinatesTransition", "from": "planner::advance_plan_state", "to": "transition::workflow_coordinated" }),
                json!({ "relation": "CoordinatesTransition", "from": "tools::save_violations_after_review", "to": "transition::read_write_cycle" }),
            ],
        );

        assert!(generate_state_transition_dispersion_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|i| i.id.starts_with("auto_state_transition_dispersion_"))
            .expect("state transition issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("proof"));
        assert_eq!(
            issue.metrics["coordinates_transition_count"].as_u64(),
            Some(2)
        );
        assert_eq!(
            issue.metrics["workflow_coordinated_count"].as_u64(),
            Some(1)
        );
        assert_eq!(issue.metrics["read_write_cycle_count"].as_u64(), Some(1));
        assert_eq!(issue.metrics["all_coordinated"].as_bool(), Some(true));
    }
}

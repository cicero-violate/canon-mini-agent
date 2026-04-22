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
use crate::semantic::{GraphCountKind, SemanticIndex};

const DEFAULT_BRIDGE_RATIO_THRESHOLD: f64 = 10.0;
const CANDIDATE_FUNCTION_LIMIT: usize = 5;
const MIN_ACTIONABLE_BRIDGE_GRAPH_NODES: usize = 32;

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
        let call_edges = idx.call_edges();
        if call_edges.is_empty() {
            continue;
        }
        let summaries = idx.symbol_summaries();
        let fn_summaries: Vec<_> = summaries.into_iter().filter(|s| s.kind == "fn").collect();
        if fn_summaries.is_empty() {
            continue;
        }
        let mut fn_to_module: HashMap<String, String> = HashMap::new();
        let mut module_symbols: HashMap<String, HashSet<String>> = HashMap::new();
        for s in &fn_summaries {
            let module = module_partition_key(s);
            fn_to_module.insert(s.symbol.clone(), module.clone());
            module_symbols.entry(module).or_default().insert(s.symbol.clone());
        }

        let mut internal_edges: HashMap<String, usize> = HashMap::new();
        let mut external_edges: HashMap<String, usize> = HashMap::new();
        for (from, to) in call_edges {
            let Some(from_module) = fn_to_module.get(&from).cloned() else {
                continue;
            };
            let Some(to_module) = fn_to_module.get(&to).cloned() else {
                continue;
            };
            if from_module == to_module {
                *internal_edges.entry(from_module).or_insert(0) += 1;
            } else {
                *external_edges.entry(from_module).or_insert(0) += 1;
            }
        }

        for (module, symbols) in module_symbols {
            let internal = internal_edges.get(&module).copied().unwrap_or(0);
            let external = external_edges.get(&module).copied().unwrap_or(0);
            let total = internal + external;
            if total == 0 {
                continue;
            }
            let cohesion = internal as f64 / total as f64;
            let task = if cohesion < 0.2 && symbols.len() > 5 {
                Some("DissolveModule")
            } else if cohesion > 0.8 && external > 10 {
                Some("FormalizeBoundary")
            } else {
                None
            };
            let Some(task) = task else { continue };
            let id = format!(
                "auto_cohesion_{}_{}",
                crate_name.replace('-', "_"),
                stable_hash(&format!("{module}:{task}"))
            );
            if existing.contains(&id) {
                continue;
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
                    crate = crate_name.replace('-', "_"),
                    symbol_count = symbols.len()
                ),
                scope: format!("crate:{}", crate_name.replace('-', "_")),
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
            created += 1;
        }
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

pub fn generate_artifact_writer_dispersion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };

        let mut writers_by_artifact: HashMap<String, HashSet<String>> = HashMap::new();
        for (writer, artifact) in idx.artifact_write_edges() {
            if !looks_like_symbol(&writer) || artifact.trim().is_empty() {
                continue;
            }
            writers_by_artifact.entry(artifact).or_default().insert(writer);
        }

        let crate_name = crate_name.replace('-', "_");
        for (artifact, writers) in writers_by_artifact {
            let mut writers: Vec<String> = writers.into_iter().collect();
            writers.sort();
            let issue = build_artifact_writer_dispersion_issue(&crate_name, &artifact, &writers);
            desired_ids.insert(issue.id.clone());
            mutated += upsert_bridge_issue(&mut file, issue, writers.len() > 1);
        }

        let prefix = format!("auto_artifact_writer_dispersion_{crate_name}_");
        for issue in &mut file.issues {
            if issue.id.starts_with(&prefix) && !desired_ids.contains(&issue.id) && issue.status != "resolved" {
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
            "generate_artifact_writer_dispersion_issues",
        )?;
    }

    Ok(mutated)
}

pub fn generate_error_shaping_dispersion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };

        let mut by_style: HashMap<String, HashSet<String>> = HashMap::new();
        for (symbol, style) in idx.semantic_edges_by_relation("ShapesError") {
            if !looks_like_symbol(&symbol) || style.trim().is_empty() {
                continue;
            }
            by_style.entry(style).or_default().insert(symbol);
        }

        let crate_name = crate_name.replace('-', "_");
        let desired = build_error_shaping_dispersion_issue(&crate_name, &by_style);
        let active = desired
            .metrics
            .get("function_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 4;
        mutated += upsert_bridge_issue(&mut file, desired, active);
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

pub fn generate_state_transition_dispersion_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };

        let mut transitions_by_state: HashMap<String, HashSet<String>> = HashMap::new();
        for (symbol, state) in idx.state_transition_edges() {
            if !looks_like_symbol(&symbol) || state.trim().is_empty() {
                continue;
            }
            transitions_by_state.entry(state).or_default().insert(symbol);
        }

        let crate_name = crate_name.replace('-', "_");
        for (state_domain, transitions) in transitions_by_state {
            let mut transitions: Vec<String> = transitions.into_iter().collect();
            transitions.sort();
            let issue =
                build_state_transition_dispersion_issue(&crate_name, &state_domain, &transitions);
            desired_ids.insert(issue.id.clone());
            mutated += upsert_bridge_issue(&mut file, issue, transitions.len() > 1);
        }

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
        let crate_name = crate_name.replace('-', "_");
        let desired = build_planner_loop_fragmentation_issue(&crate_name, &idx);
        let active = desired
            .metrics
            .get("orchestrator_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 1;
        mutated += upsert_bridge_issue(&mut file, desired, active);
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

pub fn generate_implicit_state_machine_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        let summaries = idx.symbol_summaries();
        let mut states_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
        for (symbol, state) in idx.state_transition_edges() {
            states_by_symbol.entry(symbol).or_default().insert(state);
        }

        for summary in summaries {
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
            let issue = build_implicit_state_machine_issue(&crate_name, &summary, state_domains);
            desired_ids.insert(issue.id.clone());
            mutated += upsert_bridge_issue(&mut file, issue, qualifies);
        }

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

pub fn generate_effect_boundary_leak_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        let mut effects_by_symbol: HashMap<String, HashSet<&'static str>> = HashMap::new();
        for (symbol, _) in idx.artifact_write_edges() {
            effects_by_symbol
                .entry(symbol)
                .or_default()
                .insert("artifact_write");
        }
        for (symbol, _) in idx.artifact_read_edges() {
            effects_by_symbol
                .entry(symbol)
                .or_default()
                .insert("artifact_read");
        }
        for (symbol, _) in idx.state_write_edges() {
            effects_by_symbol
                .entry(symbol)
                .or_default()
                .insert("state_write");
        }
        for (symbol, _) in idx.state_read_edges() {
            effects_by_symbol
                .entry(symbol)
                .or_default()
                .insert("state_read");
        }
        for (symbol, _) in idx.state_transition_edges() {
            effects_by_symbol
                .entry(symbol)
                .or_default()
                .insert("state_transition");
        }
        for (symbol, _) in idx.semantic_edges_by_relation("ShapesError") {
            effects_by_symbol
                .entry(symbol)
                .or_default()
                .insert("error_shape");
        }

        let mut by_module: HashMap<String, Vec<(String, Vec<&'static str>)>> = HashMap::new();
        for (symbol, effects) in effects_by_symbol {
            if canonical_effect_boundary_symbol(&symbol) || effects.len() < 3 {
                continue;
            }
            let module = symbol
                .rsplit_once("::")
                .map(|(m, _)| m.to_string())
                .unwrap_or(symbol.clone());
            let mut effects_vec: Vec<&'static str> = effects.into_iter().collect();
            effects_vec.sort();
            by_module.entry(module).or_default().push((symbol, effects_vec));
        }

        for (module, mut rows) in by_module {
            rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
            let issue = build_effect_boundary_leak_issue(&crate_name, &module, &rows);
            desired_ids.insert(issue.id.clone());
            mutated += upsert_bridge_issue(&mut file, issue, !rows.is_empty());
        }

        let prefix = format!("auto_effect_boundary_leak_{crate_name}_");
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
            "generate_effect_boundary_leak_issues",
        )?;
    }

    Ok(mutated)
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

        for ((source, target), mut symbols) in symbols_by_pair {
            symbols.sort();
            symbols.dedup();
            let issue = build_representation_fanout_issue(&crate_name, &source, &target, &symbols);
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

pub fn generate_cfg_region_reduction_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = load_issues_file(workspace);
    let before = serde_json::to_value(&file)?;
    let mut desired_ids = HashSet::new();
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let crate_name = crate_name.replace('-', "_");
        let redundant_by_symbol: HashMap<String, usize> = idx
            .redundant_path_pairs()
            .into_iter()
            .fold(HashMap::new(), |mut acc, pair| {
                *acc.entry(pair.path_a.owner).or_insert(0) += 1;
                acc
            });

        for summary in idx.symbol_summaries() {
            if summary.kind != "fn" {
                continue;
            }
            let symbol = summary.symbol.clone();
            let dominance_score = idx.cfg_dominance_score(&symbol);
            let cfg_edges = idx.symbol_cfg_edges(&symbol);
            let back_edge_count = cfg_edges.iter().filter(|edge| edge.is_back_edge).count();
            let redundant_path_count = redundant_by_symbol.get(&symbol).copied().unwrap_or(0);
            let branch_score = summary.branch_score.unwrap_or(0.0);
            let qualifies = branch_score >= 4.0
                && dominance_score >= 0.45
                && (back_edge_count > 0 || summary.switchint_count >= 2 || redundant_path_count > 0);
            let issue = build_cfg_region_reduction_issue(
                &crate_name,
                &summary,
                dominance_score,
                back_edge_count,
                redundant_path_count,
            );
            desired_ids.insert(issue.id.clone());
            mutated += upsert_bridge_issue(&mut file, issue, qualifies);
        }

        let prefix = format!("auto_cfg_region_reduction_{crate_name}_");
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
            "generate_cfg_region_reduction_issues",
        )?;
    }

    Ok(mutated)
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
) -> Issue {
    let display_state = display_state_domain(state_domain);
    let transition_list = transitions
        .iter()
        .map(|symbol| format!("`{symbol}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let evidence = transitions
        .iter()
        .map(|symbol| format!("transition site `{symbol}` mutates `{display_state}` after branching"))
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
            "Compiler-observed branch-plus-state-write transitions for state domain `{state_domain}` \
             are distributed across multiple functions in crate `{crate_name}`: {transition_list}.\n\n\
             Transition logic that mutates the same state domain from several unrelated sites tends to \
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
            "proof_tier": "hypothesis",
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

fn build_planner_loop_fragmentation_issue(crate_name: &str, idx: &SemanticIndex) -> Issue {
    let summary_by_symbol: HashMap<String, crate::semantic::SymbolSummary> = idx
        .symbol_summaries()
        .into_iter()
        .map(|summary| (summary.symbol.clone(), summary))
        .collect();
    let mut phases_by_symbol: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, workflow) in idx.workflow_domain_edges() {
        let Some(phase) = workflow_phase_from_domain(&workflow) else {
            continue;
        };
        phases_by_symbol
            .entry(symbol)
            .or_default()
            .insert(phase);
    }

    let mut phase_symbols: HashMap<String, HashSet<String>> = HashMap::new();
    for (symbol, phases) in &phases_by_symbol {
        for phase in phases {
            phase_symbols
                .entry(phase.clone())
                .or_default()
                .insert(symbol.clone());
        }
    }

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
        workflow_callers.entry(from.clone()).or_default().extend(to_phases.iter().cloned());
        *outgoing_workflow.entry(from).or_insert(0) += 1;
        *incoming_workflow.entry(to).or_insert(0) += 1;
    }

    let mut orchestrators: Vec<(String, Vec<String>, Vec<String>, usize, usize)> = phases_by_symbol
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
            let coordinating =
                outgoing >= 2 && branch_score >= 2.0 && (root_like || reached_vec.len() >= 2 || direct_vec.len() >= 2);
            if coordinating {
                Some((symbol.clone(), direct_vec, reached_vec, incoming, outgoing))
            } else {
                None
            }
        })
        .collect();
    orchestrators.sort_by(|a, b| {
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
    let canonical_target = orchestrators
        .first()
        .map(|row| row.0.clone())
        .unwrap_or_else(|| "app::run_planner_phase".to_string());
    let evidence = orchestrators
        .iter()
        .take(8)
        .map(|(symbol, direct_phases, reached_phases, incoming, outgoing)| {
            format!(
                "workflow orchestrator `{symbol}` touches phases [{}] and reaches [{}] (incoming_workflow={incoming}, outgoing_workflow={outgoing})",
                direct_phases.join(", "),
                reached_phases.join(", ")
            )
        })
        .collect::<Vec<_>>();
    let orchestrator_metrics = orchestrators
        .iter()
        .map(|(symbol, direct_phases, reached_phases, incoming, outgoing)| {
            json!({
                "symbol": symbol,
                "direct_phases": direct_phases,
                "reached_phases": reached_phases,
                "incoming_workflow": incoming,
                "outgoing_workflow": outgoing,
            })
        })
        .collect::<Vec<_>>();

    Issue {
        id: planner_loop_issue_id(crate_name),
        title: format!(
            "Planner/apply/verify fragmentation in `{}` ({} orchestrators)",
            crate_name,
            orchestrators.len()
        ),
        status: if orchestrators.len() > 1 { "open" } else { "resolved" }.to_string(),
        priority: if orchestrators.len() >= 4 { "high" } else { "medium" }.to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Compiler-observed workflow-domain functions in crate `{crate_name}` are spread across planner/apply/verify phases \
             without a single clear orchestrator. Detected phase counts: planner={planner_count}, \
             apply={apply_count}, verify={verify_count}. Hypothesis-grade workflow analysis found {} \
             root-like or multi-phase orchestrators.\n\n\
             This usually means the planner/apply/verify loop is fragmented across multiple entrypoints \
             instead of one canonical control path. Consolidate orchestration around `{canonical_target}` \
             or another single workflow owner, and downgrade the remaining sites to thin delegates.",
            orchestrators.len()
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
            "canonical_target_candidate": canonical_target,
            "orchestrators": orchestrator_metrics,
        }),
        acceptance_criteria: vec![
            "planner/apply/verify orchestration is centralized through one canonical control path".to_string(),
            "secondary workflow entrypoints are deleted or reduced to thin delegates".to_string(),
            "graph.json is regenerated and the detector reports at most one orchestrator".to_string(),
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

fn canonical_effect_boundary_symbol(symbol: &str) -> bool {
    const CANONICAL_PREFIXES: &[&str] = &[
        "app::",
        "tools::",
        "logging::",
        "supervisor::",
        "canonical_writer::",
        "issues::",
        "reports::",
        "invariants::",
        "lessons::",
        "objectives::",
        "blockers::",
    ];
    CANONICAL_PREFIXES.iter().any(|prefix| symbol.starts_with(prefix))
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

fn canonical_representation_target(symbols: &[String]) -> String {
    let mut ranked = symbols.to_vec();
    ranked.sort_by(|a, b| {
        canonical_effect_boundary_symbol(b)
            .cmp(&canonical_effect_boundary_symbol(a))
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
) -> Issue {
    let display_source = display_representation_domain(source);
    let display_target = display_representation_domain(target);
    let canonical_target = canonical_representation_target(symbols);
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

fn cfg_region_reduction_issue_id(crate_name: &str, symbol: &str) -> String {
    format!(
        "auto_cfg_region_reduction_{}_{}",
        sanitize_fragment(crate_name),
        stable_hash(&format!("{crate_name}:{symbol}"))
    )
}

fn build_cfg_region_reduction_issue(
    crate_name: &str,
    summary: &crate::semantic::SymbolSummary,
    dominance_score: f32,
    back_edge_count: usize,
    redundant_path_count: usize,
) -> Issue {
    let branch_score = summary.branch_score.unwrap_or(0.0);
    let region_kind = if back_edge_count > 0 {
        "loop/SCC"
    } else if redundant_path_count > 0 {
        "duplicate-path"
    } else {
        "dominator branch funnel"
    };
    let evidence = vec![
        format!(
            "branch_score={branch_score:.2}, switchint_count={}, has_back_edges={}",
            summary.switchint_count,
            summary.has_back_edges
        ),
        format!(
            "cfg_dominance_score={dominance_score:.2}, back_edge_count={back_edge_count}, redundant_path_count={redundant_path_count}"
        ),
    ];

    Issue {
        id: cfg_region_reduction_issue_id(crate_name, &summary.symbol),
        title: format!("CFG region reduction candidate in `{}`", summary.symbol),
        status: "open".to_string(),
        priority: if back_edge_count > 0 || redundant_path_count >= 2 {
            "high".to_string()
        } else {
            "medium".to_string()
        },
        kind: "logic".to_string(),
        description: format!(
            "Function `{}` in crate `{}` contains a concentrated `{region_kind}` control region. \
             The CFG shows enough branching pressure, dominance concentration, or loop structure to justify a focused reduction pass.\n\n\
             Recommended direction: collapse the hot region into a smaller canonical control node, extract a transition table, or isolate the SCC behind a single dispatcher."
            ,
            summary.symbol,
            crate_name
        ),
        location: shorten_symbol_location(summary),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "ReduceCfgRegion",
            "proof_tier": "hypothesis",
            "symbol": summary.symbol,
            "region_kind": region_kind,
            "branch_score": branch_score,
            "switchint_count": summary.switchint_count,
            "cfg_dominance_score": dominance_score,
            "back_edge_count": back_edge_count,
            "redundant_path_count": redundant_path_count,
        }),
        acceptance_criteria: vec![
            "the concentrated CFG region is reduced to a simpler control surface".to_string(),
            "remaining loop or branch structure is routed through one canonical reducer or dispatcher".to_string(),
            "graph.json is regenerated and the detector reports lower branch/back-edge pressure for the function".to_string(),
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
        generate_cfg_region_reduction_issues,
        generate_bridge_connectivity_issues, generate_effect_boundary_leak_issues,
        generate_error_shaping_dispersion_issues, generate_implicit_state_machine_issues,
        generate_planner_loop_fragmentation_issues, generate_representation_fanout_issues,
        generate_state_transition_dispersion_issues, issue_id, priority_from_ratio,
        sanitize_fragment,
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
        let raw = fs::read_to_string(path).expect("read issues");
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
            ],
            vec![
                json!({
                    "relation": "TouchesWorkflowDomain",
                    "from": "app::run_planner_phase",
                    "to": "workflow::planner",
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
                    "from": "tools::handle_plan_action",
                    "to": "workflow::planner",
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
                    "from": "app::apply_wake_signals",
                    "to": "tools::handle_plan_action",
                }),
                json!({
                    "relation": "call",
                    "from": "app::apply_wake_signals",
                    "to": "tools::verify_apply_patch_crate",
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
    fn cfg_region_reduction_emits_issue_for_loop_dominated_function() {
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

        assert!(generate_cfg_region_reduction_issues(&workspace).unwrap() > 0);
        let issues = read_issues(&workspace);
        let issue = issues
            .iter()
            .find(|issue| issue.id.starts_with("auto_cfg_region_reduction_"))
            .expect("cfg region issue");
        assert_eq!(issue.status, "open");
        assert_eq!(issue.metrics["proof_tier"].as_str(), Some("hypothesis"));
        assert_eq!(issue.metrics["back_edge_count"].as_u64(), Some(1));
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
}

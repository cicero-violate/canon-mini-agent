//! Graph-metric issue generation.
//!
//! Detects bridge-connectivity overload directly from `state/rustc/<crate>/graph.json`
//! and emits deterministic `ISSUES.json` entries when the bridge edge density
//! exceeds a threshold.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::constants::ISSUES_FILE;
use crate::issues::{rescore_all, Issue, IssuesFile};
use crate::semantic::SemanticIndex;

const DEFAULT_BRIDGE_RATIO_THRESHOLD: f64 = 10.0;
const CANDIDATE_FUNCTION_LIMIT: usize = 5;

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
    let issues_path = workspace.join(ISSUES_FILE);
    let raw = std::fs::read_to_string(&issues_path).unwrap_or_default();
    let mut file: IssuesFile = if raw.trim().is_empty() {
        IssuesFile::default()
    } else {
        serde_json::from_str(&raw).unwrap_or_default()
    };

    let before = serde_json::to_value(&file)?;
    let mut mutated = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(stats) = analyze_bridge_connectivity(workspace, &crate_name) else {
            continue;
        };
        let desired_issue = build_bridge_issue(&stats);
        mutated += upsert_bridge_issue(&mut file, desired_issue, stats.bridge_ratio > stats.threshold);
    }

    rescore_all(&mut file);
    let after = serde_json::to_value(&file)?;
    if before != after {
        std::fs::write(&issues_path, serde_json::to_string_pretty(&file)?)?;
    }

    Ok(mutated)
}

pub fn analyze_bridge_connectivity(
    workspace: &Path,
    crate_name: &str,
) -> Result<BridgeConnectivityStats> {
    let idx = SemanticIndex::load(workspace, crate_name)?;
    let node_count = idx.node_count();
    let bridge_edge_count = idx.bridge_edge_count();
    let semantic_edge_count = idx.semantic_edge_count();
    let cfg_node_count = idx.cfg_node_count();
    let cfg_edge_count = idx.cfg_edge_count();
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
    !raw.starts_with("cfg::") && raw.contains("::")
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
        analyze_bridge_connectivity, generate_bridge_connectivity_issues, issue_id,
        priority_from_ratio, sanitize_fragment,
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

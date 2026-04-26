/// Recovery gap analyzer — reads graph.json after each build and appends
/// typed `ErrorClass` blocker records for structural recovery gaps.
///
/// ## Pipeline position
///
///   cargo check ok
///     → canon-generate-issues (refresh_issue_artifacts)
///       → analyze_and_record_recovery_gaps (this module)
///         → blockers::append_blocker(ErrorClass::MissingClassificationPath | ...)
///           → blockers.json
///             → compute_blocker_class_coverage → eval pressure
///               → REPAIR_PLAN → task → executor patches source
///                 → cargo build → graph.json regenerated
///                   → gaps disappear → blocker_class_coverage recovers
///
/// ## Gap classes
///
///   MissingClassificationPath
///     A route_gate function in the app:: module has no forward call-graph
///     path to any classifier (classify_result, classify_blocker_summary,
///     apply_recovery_decision, etc.).  Runtime route failures it handles
///     never enter blockers.json.
///
///   UnreachableRecoveryDispatch
///     A repair_or_initialize function whose name contains "recover" has no
///     forward path to the canonical recovery dispatch
///     (apply_recovery_decision, record_recovery_triggered, etc.).
///     Recovery it performs is ad-hoc and not tracked by eval.
///
///   UncanonicalizedStateTransition
///     A function has TransitionsState outgoing edges but is NOT reachable
///     from any canonical_writer function via the forward call graph.
///     State mutation bypasses the canonical writer — potential loophole.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use serde::Deserialize;

// ── Graph types ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct Graph {
    #[serde(default)]
    nodes: HashMap<String, GraphNode>,
    #[serde(default)]
    edges: Vec<GraphEdge>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphNode {
    #[serde(default)]
    path: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    intent_class: Option<String>,
    #[serde(default)]
    def: Option<NodeDef>,
}

#[derive(Debug, Default, Deserialize)]
struct NodeDef {
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: u32,
}

#[derive(Debug, Default, Deserialize)]
struct GraphEdge {
    #[serde(default)]
    relation: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

// ── Loading ───────────────────────────────────────────────────────────────────

fn load_graph(workspace: &Path) -> Option<Graph> {
    // Scan all available crate graph paths and return the first that parses.
    for crate_name in crate::SemanticIndex::available_crates(workspace) {
        let path = workspace
            .join("state")
            .join("rustc")
            .join(&crate_name)
            .join("graph.json");
        if !path.exists() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(g) = serde_json::from_slice::<Graph>(&bytes) {
                if !g.nodes.is_empty() {
                    return Some(g);
                }
            }
        }
    }
    None
}

// ── BFS helpers ───────────────────────────────────────────────────────────────

fn build_forward_call_graph(edges: &[GraphEdge]) -> HashMap<&str, Vec<&str>> {
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges {
        if e.relation == "Calls" && !e.from.is_empty() && !e.to.is_empty() {
            forward.entry(&e.from).or_default().push(&e.to);
        }
    }
    forward
}

fn reachable_from(start: &str, forward: &HashMap<&str, Vec<&str>>) -> HashSet<String> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(start);
    while let Some(nid) = queue.pop_front() {
        if !visited.insert(nid.to_string()) {
            continue;
        }
        if let Some(succs) = forward.get(nid) {
            for &succ in succs {
                if !visited.contains(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }
    visited
}

// ── Utility filters ───────────────────────────────────────────────────────────

fn is_generated(path: &str) -> bool {
    path.contains("::_::_serde::")
        || path.contains("::_serde::")
        || path.contains("::tests::")
        || path.contains("::test::")
}

fn is_fn_node(node: &GraphNode) -> bool {
    node.kind == "fn" && !is_generated(&node.path)
}

// ── Stable blocker id ─────────────────────────────────────────────────────────

fn stable_gap_id(error_class_key: &str, fn_path: &str) -> String {
    // FNV-1a hash — deterministic across processes (DefaultHasher is NOT).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in fn_path.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let ec_slug = error_class_key.replace('_', "-");
    format!("graph-{ec_slug}-{h:016x}")
}

// ── Gap 1: MissingClassificationPath ─────────────────────────────────────────

const CLASSIFIER_PATH_SUFFIXES: &[&str] = &[
    "::classify_result",
    "::classify_blocker_summary",
    "::classify_blocker_summary_match",
    "::classify_action_kind_failure",
    "::classify_route_gate_reason",
    "::invalid_route_class",
    "record_action_failure_with_writer",
    "record_blocker_message_with_writer",
    "::append_blocker",
    "apply_recovery_decision",
    "record_recovery_triggered",
    "record_recovery_suppressed",
];

// Utility helper suffixes that are not dispatch points.
const ROUTE_GATE_SKIP_SUFFIXES: &[&str] = &[
    "_count", "_state", "_payload", "_record",
    "annotate_", "recent_",
];

fn find_missing_classification_gaps<'a>(
    nodes: &'a HashMap<String, GraphNode>,
    forward: &HashMap<&str, Vec<&str>>,
) -> Vec<&'a GraphNode> {
    // Classifier target node ids
    let target_ids: HashSet<&str> = nodes
        .iter()
        .filter(|(_, n)| {
            is_fn_node(n)
                && CLASSIFIER_PATH_SUFFIXES
                    .iter()
                    .any(|s| n.path.contains(s))
        })
        .map(|(id, _)| id.as_str())
        .collect();

    if target_ids.is_empty() {
        return Vec::new();
    }

    let mut gaps = Vec::new();
    for (nid, node) in nodes {
        if !is_fn_node(node) {
            continue;
        }
        let intent = node.intent_class.as_deref().unwrap_or("");
        if intent != "route_gate" {
            continue;
        }
        // Only orchestrator-level gate functions
        if !node.path.starts_with("app::") {
            continue;
        }
        // Skip utility helpers
        let fn_name = node.path.rsplit("::").next().unwrap_or("");
        if ROUTE_GATE_SKIP_SUFFIXES
            .iter()
            .any(|s| fn_name.starts_with(s) || fn_name.ends_with(s))
        {
            continue;
        }
        // Don't flag functions that are themselves classifiers
        if CLASSIFIER_PATH_SUFFIXES
            .iter()
            .any(|s| node.path.contains(s))
        {
            continue;
        }
        let reachable = reachable_from(nid, forward);
        let has_target = target_ids.iter().any(|t| reachable.contains(*t));
        if !has_target {
            gaps.push(node);
        }
    }
    gaps
}

// ── Gap 2: UnreachableRecoveryDispatch ────────────────────────────────────────

const DISPATCH_PATH_SUFFIXES: &[&str] = &[
    "apply_recovery_decision",
    "record_recovery_triggered",
    "record_recovery_outcome",
    "record_recovery_suppressed",
];

fn find_unreachable_dispatch_gaps<'a>(
    nodes: &'a HashMap<String, GraphNode>,
    forward: &HashMap<&str, Vec<&str>>,
) -> Vec<&'a GraphNode> {

    let target_ids: HashSet<&str> = nodes
        .iter()
        .filter(|(_, n)| {
            is_fn_node(n)
                && DISPATCH_PATH_SUFFIXES
                    .iter()
                    .any(|s| n.path.contains(s))
        })
        .map(|(id, _)| id.as_str())
        .collect();

    if target_ids.is_empty() {
        return Vec::new();
    }

    let mut gaps = Vec::new();
    for (nid, node) in nodes {
        if !is_fn_node(node) {
            continue;
        }
        let intent = node.intent_class.as_deref().unwrap_or("");
        if intent != "repair_or_initialize" {
            continue;
        }
        // Only functions whose name contains "recover"
        if !node.path.to_lowercase().contains("recover") {
            continue;
        }
        // Skip functions that are themselves dispatch targets
        if DISPATCH_PATH_SUFFIXES
            .iter()
            .any(|s| node.path.contains(s))
        {
            continue;
        }
        let reachable = reachable_from(nid, forward);
        let has_target = target_ids.iter().any(|t| reachable.contains(*t));
        if !has_target {
            gaps.push(node);
        }
    }
    gaps
}

// ── Gap 3: UncanonicalizedStateTransition ─────────────────────────────────────

const CANONICAL_WRITER_PREFIX: &str = "canonical_writer::";

fn find_uncanonicalized_transition_gaps<'a>(
    nodes: &'a HashMap<String, GraphNode>,
    edges: &[GraphEdge],
    forward: &HashMap<&str, Vec<&str>>,
) -> Vec<&'a GraphNode> {
    // canonical_writer source nodes
    let canonical_ids: Vec<&str> = nodes
        .keys()
        .filter(|k| {
            nodes[*k].path.contains(CANONICAL_WRITER_PREFIX) && is_fn_node(&nodes[*k])
        })
        .map(|k| k.as_str())
        .collect();

    if canonical_ids.is_empty() {
        return Vec::new();
    }

    // All nodes reachable from ANY canonical_writer function
    let mut canonical_reachable: HashSet<String> = HashSet::new();
    for cid in &canonical_ids {
        canonical_reachable.extend(reachable_from(cid, forward));
    }

    // Functions that are the FROM side of a TransitionsState edge (owned to avoid borrow conflict)
    let transitions_state_fns: HashSet<String> = edges
        .iter()
        .filter(|e| e.relation == "TransitionsState" && !e.from.is_empty())
        .map(|e| e.from.clone())
        .collect();

    let mut gaps = Vec::new();
    for nid in &transitions_state_fns {
        let Some(node) = nodes.get(nid.as_str()) else { continue };
        if !is_fn_node(node) {
            continue;
        }
        // OK if canonical_writer can reach this function
        if canonical_reachable.contains(nid.as_str()) {
            continue;
        }
        // OK if this function IS a canonical_writer function
        if node.path.contains(CANONICAL_WRITER_PREFIX) {
            continue;
        }
        gaps.push(node);
    }
    gaps
}

// ── Gap → BlockerRecord ───────────────────────────────────────────────────────

fn append_gap_blocker(
    workspace: &Path,
    existing_ids: &HashSet<String>,
    error_class: crate::error_class::ErrorClass,
    fn_path: &str,
    summary: String,
) -> bool {
    let id = stable_gap_id(error_class.as_key(), fn_path);
    if existing_ids.contains(&id) {
        return false; // already recorded
    }
    crate::blockers::append_blocker(
        workspace,
        crate::blockers::BlockerRecord {
            id,
            error_class,
            actor: "graph_analyzer".to_string(),
            task_id: None,
            objective_id: None,
            summary,
            action_kind: "graph_analysis".to_string(),
            source: "graph_analyzer".to_string(),
            ts_ms: crate::logging::now_ms(),
        },
    );
    true
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Analyze `graph.json` for structural recovery gaps and append any new gaps
/// as typed `ErrorClass` blockers into `agent_state/blockers.json`.
///
/// Returns the number of new blocker records appended.
pub fn analyze_and_record_recovery_gaps(workspace: &Path) -> usize {
    let Some(graph) = load_graph(workspace) else {
        return 0;
    };

    let forward = build_forward_call_graph(&graph.edges);

    // Collect existing blocker ids to avoid duplicates.
    let existing_blockers = crate::blockers::load_blockers(workspace);
    let existing_ids: HashSet<String> = existing_blockers
        .blockers
        .iter()
        .map(|b| b.id.clone())
        .collect();

    let mut added = 0usize;

    // Gap 1: MissingClassificationPath
    let gap1 = find_missing_classification_gaps(&graph.nodes, &forward);
    for node in &gap1 {
        let file_loc = node
            .def
            .as_ref()
            .map(|d| format!("{}:{}", d.file, d.line))
            .unwrap_or_default();
        let summary = format!(
            "route_gate function '{}' has no reachable classifier in call graph \
            — runtime route failures are silently discarded ({})",
            node.path, file_loc
        );
        if append_gap_blocker(
            workspace,
            &existing_ids,
            crate::error_class::ErrorClass::MissingClassificationPath,
            &node.path,
            summary,
        ) {
            added += 1;
        }
    }

    // Gap 2: UnreachableRecoveryDispatch
    let gap2 = find_unreachable_dispatch_gaps(&graph.nodes, &forward);
    for node in &gap2 {
        let file_loc = node
            .def
            .as_ref()
            .map(|d| format!("{}:{}", d.file, d.line))
            .unwrap_or_default();
        let summary = format!(
            "repair function '{}' has no path to canonical recovery dispatch \
            — recovery is ad-hoc and untracked by eval ({})",
            node.path, file_loc
        );
        if append_gap_blocker(
            workspace,
            &existing_ids,
            crate::error_class::ErrorClass::UnreachableRecoveryDispatch,
            &node.path,
            summary,
        ) {
            added += 1;
        }
    }

    // Gap 3: UncanonicalizedStateTransition
    let gap3 =
        find_uncanonicalized_transition_gaps(&graph.nodes, &graph.edges, &forward);
    for node in &gap3 {
        let file_loc = node
            .def
            .as_ref()
            .map(|d| format!("{}:{}", d.file, d.line))
            .unwrap_or_default();
        let summary = format!(
            "function '{}' transitions state without being reachable from \
            canonical_writer::apply — structural loophole ({})",
            node.path, file_loc
        );
        if append_gap_blocker(
            workspace,
            &existing_ids,
            crate::error_class::ErrorClass::UncanonicalizedStateTransition,
            &node.path,
            summary,
        ) {
            added += 1;
        }
    }

    if added > 0 {
        eprintln!(
            "[recovery_gap_analysis] appended {added} new gap blockers \
            ({} missing_classification, {} unreachable_dispatch, \
            {} uncanonicalized_transition)",
            gap1.len(),
            gap2.len(),
            gap3.len()
        );
    }

    added
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(path: &str, kind: &str, intent: Option<&str>) -> GraphNode {
        GraphNode {
            path: path.to_string(),
            kind: kind.to_string(),
            intent_class: intent.map(str::to_string),
            def: None,
        }
    }

    fn make_edge(from: &str, to: &str, relation: &str) -> GraphEdge {
        GraphEdge {
            relation: relation.to_string(),
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    #[test]
    fn reachable_from_follows_calls() {
        let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
        forward.insert("a", vec!["b", "c"]);
        forward.insert("b", vec!["d"]);
        let r = reachable_from("a", &forward);
        assert!(r.contains("a"));
        assert!(r.contains("b"));
        assert!(r.contains("c"));
        assert!(r.contains("d"));
        assert!(!r.contains("e"));
    }

    #[test]
    fn reachable_from_handles_cycles() {
        let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
        forward.insert("a", vec!["b"]);
        forward.insert("b", vec!["a"]);
        let r = reachable_from("a", &forward);
        assert!(r.contains("a"));
        assert!(r.contains("b"));
    }

    #[test]
    fn stable_gap_id_is_deterministic() {
        let id1 = stable_gap_id("missing_classification_path", "app::foo::bar");
        let id2 = stable_gap_id("missing_classification_path", "app::foo::bar");
        assert_eq!(id1, id2);
        assert!(id1.starts_with("graph-missing-classification-path-"));
    }

    #[test]
    fn stable_gap_id_differs_for_different_paths() {
        let id1 = stable_gap_id("missing_classification_path", "app::foo::bar");
        let id2 = stable_gap_id("missing_classification_path", "app::foo::baz");
        assert_ne!(id1, id2);
    }

    #[test]
    fn is_generated_filters_serde_paths() {
        assert!(is_generated("foo::_::_serde::Deserialize::deserialize"));
        assert!(is_generated("foo::tests::my_test"));
        assert!(!is_generated("app::app_planner_executor::apply_route_gate_block"));
    }

    #[test]
    fn missing_classification_flags_app_route_gate_without_classifier() {
        let mut nodes = HashMap::new();
        // Route gate with no path to classifier
        nodes.insert(
            "n1".to_string(),
            make_node("app::app_foo::apply_gate_signal", "fn", Some("route_gate")),
        );
        // A classifier (not reachable from n1)
        nodes.insert(
            "n2".to_string(),
            make_node("error_class::classify_result", "fn", Some("unknown_low_confidence")),
        );
        let forward: HashMap<&str, Vec<&str>> = HashMap::new(); // no edges
        let gaps = find_missing_classification_gaps(&nodes, &forward);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].path, "app::app_foo::apply_gate_signal");
    }

    #[test]
    fn missing_classification_passes_when_classifier_reachable() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "n1".to_string(),
            make_node("app::app_foo::apply_gate_signal", "fn", Some("route_gate")),
        );
        nodes.insert(
            "n2".to_string(),
            make_node("error_class::classify_result", "fn", Some("unknown_low_confidence")),
        );
        let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
        forward.insert("n1", vec!["n2"]);
        let gaps = find_missing_classification_gaps(&nodes, &forward);
        assert!(gaps.is_empty());
    }

    #[test]
    fn uncanonicalized_transition_flags_when_canonical_writer_unreachable() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "cw".to_string(),
            make_node("canonical_writer::apply", "fn", None),
        );
        nodes.insert(
            "st".to_string(),
            make_node("tools::sync_state", "fn", None),
        );
        let edges = vec![make_edge("st", "db", "TransitionsState")];
        let forward: HashMap<&str, Vec<&str>> = HashMap::new(); // canonical_writer can't reach st
        let gaps = find_uncanonicalized_transition_gaps(&nodes, &edges, &forward);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].path, "tools::sync_state");
    }

    #[test]
    fn uncanonicalized_transition_passes_when_reachable_from_canonical_writer() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "cw".to_string(),
            make_node("canonical_writer::apply", "fn", None),
        );
        nodes.insert(
            "st".to_string(),
            make_node("system_state::apply_control_event", "fn", None),
        );
        let edges = vec![make_edge("st", "db", "TransitionsState")];
        let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
        forward.insert("cw", vec!["st"]); // canonical_writer reaches st → OK
        let gaps = find_uncanonicalized_transition_gaps(&nodes, &edges, &forward);
        assert!(gaps.is_empty());
    }
}

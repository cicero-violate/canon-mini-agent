//! Ranks redundant CFG path pairs from graph.json by semantic equivalence.
//!
//! Reads graph.json produced by canon-rustc-v2, scores each owner function
//! that contains redundant path pairs, and writes safe_patch_candidates.json
//! with deterministically-ranked merge/delete candidates.
//!
//! Scoring combines:
//!   - intent_class (from docstring or name/effect seeding)
//!   - side-effect profile (WritesState, SpawnsProcess, UsesNetwork, …)
//!   - docstring provenance quality
//!   - pair-count signal (many pairs from one function → systematic pattern)
//!   - MIR structural complexity
//!
//! Usage:
//!   canon-mini-agent semantic-rank-candidates [graph.json] [out.json]
//!
//! Defaults:
//!   graph.json  →  state/rustc/canon_mini_agent/graph.json (relative to cwd)
//!   out.json    →  <graph_dir>/safe_patch_candidates.json

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Graph types — mirrors canon-rustc-v2/src/graph.rs (schema v7).
// All fields use #[serde(default)] so this binary stays compatible if the
// schema gains new fields in future versions.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct CrateGraph {
    meta: GraphMeta,
    #[serde(default)]
    nodes: HashMap<String, GraphNode>,
    #[serde(default)]
    edges: Vec<GraphEdge>,
    #[serde(default)]
    redundant_paths: Vec<RedundantPathPair>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphMeta {
    #[serde(default)]
    schema_version: u32,
}

#[derive(Debug, Default, Deserialize)]
struct GraphNode {
    #[serde(default)]
    path: String,
    #[serde(default)]
    intent_class: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    provenance: Vec<String>,
    #[serde(default)]
    mir: Option<MirInfo>,
}

#[derive(Debug, Default, Deserialize)]
struct MirInfo {
    #[serde(default)]
    blocks: usize,
}

#[derive(Debug, Default, Deserialize)]
struct GraphEdge {
    #[serde(default)]
    relation: String,
    #[serde(default)]
    from: String,
}

#[derive(Debug, Default, Deserialize)]
struct RedundantPathPair {
    path_a: PathRecord,
    path_b: PathRecord,
    #[serde(default)]
    shared_signature: u64,
}

#[derive(Debug, Default, Deserialize)]
struct PathRecord {
    #[serde(default)]
    owner: String,
    #[serde(default)]
    blocks: Vec<usize>,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CandidatesOutput {
    generated_at_ms: u64,
    /// Schema version of this output format.
    schema_version: u32,
    /// Schema version of the graph.json that was ranked.
    graph_schema_version: u32,
    summary: Summary,
    /// Ranked candidates, best (highest confidence) first.
    candidates: Vec<Candidate>,
}

#[derive(Debug, Serialize)]
struct Summary {
    total_redundant_pairs: usize,
    unique_owner_functions: usize,
    safe_merge: usize,
    investigate: usize,
    skip: usize,
    /// Owner paths present in redundant_paths but not found as nodes.
    unmatched_owners: usize,
}

#[derive(Debug, Serialize)]
struct Candidate {
    /// 1-based rank (1 = highest confidence).
    rank: usize,
    /// Fully-qualified function path.
    owner: String,
    /// Key in the graph.json nodes dict.
    owner_node_id: String,
    /// Number of redundant path pairs found in this function.
    pair_count: usize,
    /// Confidence score in [0.0, 1.0] that merging/deleting is safe.
    confidence: f64,
    /// safe_merge | investigate | skip
    recommended_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    provenance: Vec<String>,
    /// Effect labels observed on this function.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    effects: Vec<String>,
    /// MIR block count for context.
    #[serde(skip_serializing_if = "Option::is_none")]
    mir_blocks: Option<usize>,
    /// Ordered scoring factors applied — explains how confidence was reached.
    reasoning: Vec<String>,
    /// The redundant path pairs, with differing blocks highlighted.
    pairs: Vec<PairSummary>,
}

#[derive(Debug, Serialize)]
struct PairSummary {
    shared_signature: u64,
    blocks_a: Vec<usize>,
    blocks_b: Vec<usize>,
    /// Blocks that appear in path_a but not path_b.
    only_in_a: Vec<usize>,
    /// Blocks that appear in path_b but not path_a.
    only_in_b: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct SemanticRankCandidatesOptions {
    pub workspace_root: PathBuf,
    pub graph_path: PathBuf,
    pub out_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticRankCandidatesReport {
    pub candidates: usize,
    pub safe_merge: usize,
    pub investigate: usize,
    pub skip: usize,
    pub unmatched_owners: usize,
    pub out_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Effect aggregation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EffectFlags {
    reads: bool,
    writes: bool,
    process: bool,
    network: bool,
    transitions: bool,
}

impl EffectFlags {
    fn labels(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.reads       { v.push("reads_state".to_string()); }
        if self.writes      { v.push("writes_state".to_string()); }
        if self.process     { v.push("spawns_process".to_string()); }
        if self.network     { v.push("uses_network".to_string()); }
        if self.transitions { v.push("transitions_state".to_string()); }
        v
    }
    fn is_heavy(&self) -> bool { self.process || self.network }
    fn has_side_effects(&self) -> bool { self.writes || self.transitions || self.is_heavy() }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

fn score(
    intent_class: Option<&str>,
    provenance: &[String],
    effects: &EffectFlags,
    pair_count: usize,
    mir_blocks: Option<usize>,
    reasoning: &mut Vec<String>,
) -> f64 {
    let mut s = 0.5_f64;

    // ── intent_class ──────────────────────────────────────────────────────────
    match intent_class {
        Some(ic) => {
            s += 0.10;
            reasoning.push(format!("intent:{ic}"));
            match ic {
                "pure_transform" => {
                    s += 0.25;
                    reasoning.push("pure_transform:+0.25".into());
                }
                "diagnostic_scan" => {
                    s += 0.20;
                    reasoning.push("diagnostic_scan:+0.20".into());
                }
                "canonical_read" | "projection_read" => {
                    s += 0.20;
                    reasoning.push("read_intent:+0.20".into());
                }
                "validation_gate" | "route_gate" => {
                    s += 0.15;
                    reasoning.push("gate_intent:+0.15".into());
                }
                "test_assertion" => {
                    s += 0.10;
                    reasoning.push("test_assertion:+0.10".into());
                }
                "canonical_write" | "projection_write" => {
                    s -= 0.15;
                    reasoning.push("write_intent:-0.15".into());
                }
                "repair_or_initialize" => {
                    s -= 0.20;
                    reasoning.push("repair_intent:-0.20".into());
                }
                "event_append" => {
                    s -= 0.15;
                    reasoning.push("event_append:-0.15".into());
                }
                "transport_effect" => {
                    s -= 0.30;
                    reasoning.push("transport_intent:-0.30".into());
                }
                _ => {}
            }
        }
        None => {
            reasoning.push("intent_class:unknown".into());
        }
    }

    // ── docstring provenance ──────────────────────────────────────────────────
    if provenance.contains(&"rustc:docstring".to_string()) {
        s += 0.10;
        reasoning.push("doc_confirmed:+0.10".into());
    }

    // ── side-effect profile ───────────────────────────────────────────────────
    if effects.is_heavy() {
        s -= 0.30;
        reasoning.push("heavy_effects:-0.30".into());
    } else if effects.has_side_effects() {
        s -= 0.20;
        reasoning.push("write_effects:-0.20".into());
    } else {
        s += 0.20;
        reasoning.push("no_side_effects:+0.20".into());
    }

    // ── pair-count signal ─────────────────────────────────────────────────────
    // Many pairs from the same function usually indicates a systematic
    // exhaustive-match pattern where each arm is structurally equivalent.
    if pair_count >= 20 {
        s += 0.10;
        reasoning.push(format!("systematic_pairs({pair_count}):+0.10"));
    } else if pair_count >= 5 {
        s += 0.05;
        reasoning.push(format!("multi_pairs({pair_count}):+0.05"));
    }

    // ── MIR structural complexity ─────────────────────────────────────────────
    if let Some(blocks) = mir_blocks {
        if blocks <= 5 {
            s += 0.10;
            reasoning.push("simple_mir:+0.10".into());
        } else if blocks >= 200 {
            // Large functions may have intentional exhaustive matching —
            // redundant paths here could be load-bearing match arms.
            s -= 0.05;
            reasoning.push("large_mir:-0.05".into());
        }
    }

    s.clamp(0.0, 1.0)
}

fn action(confidence: f64) -> &'static str {
    if confidence >= 0.75 { "safe_merge" }
    else if confidence >= 0.45 { "investigate" }
    else { "skip" }
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub fn run_with_options(
    options: SemanticRankCandidatesOptions,
) -> anyhow::Result<SemanticRankCandidatesReport> {
    let graph_path = resolve_path(&options.workspace_root, &options.graph_path);
    let out_path = options
        .out_path
        .as_ref()
        .map(|p| resolve_path(&options.workspace_root, p))
        .unwrap_or_else(|| {
            graph_path
                .parent()
                .unwrap_or(&graph_path)
                .join("safe_patch_candidates.json")
        });

    eprintln!("reading {}", graph_path.display());
    let bytes = std::fs::read(&graph_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", graph_path.display()))?;
    let graph: CrateGraph = serde_json::from_slice(&bytes)?;
    eprintln!("  {} nodes  {} edges  {} redundant pairs  schema_version={}",
        graph.nodes.len(), graph.edges.len(),
        graph.redundant_paths.len(), graph.meta.schema_version);

    // ── path → node_id map ────────────────────────────────────────────────────
    let path_to_id: HashMap<&str, &str> = graph.nodes
        .iter()
        .map(|(id, n)| (n.path.as_str(), id.as_str()))
        .collect();

    // ── per-node effect flags (keyed by node_id) ──────────────────────────────
    let mut effect_map: HashMap<&str, EffectFlags> = HashMap::new();
    for edge in &graph.edges {
        let f = effect_map.entry(edge.from.as_str()).or_default();
        match edge.relation.as_str() {
            "ReadsState" | "ReadsArtifact"    => { f.reads = true; }
            "WritesState" | "WritesArtifact"  => { f.writes = true; }
            "SpawnsProcess"                   => { f.process = true; }
            "UsesNetwork"                     => { f.network = true; }
            "TransitionsState"                => { f.transitions = true; }
            _ => {}
        }
    }
    let empty = EffectFlags::default();

    // ── group pairs by owner path ─────────────────────────────────────────────
    let mut by_owner: HashMap<&str, Vec<&RedundantPathPair>> = HashMap::new();
    for pair in &graph.redundant_paths {
        by_owner.entry(pair.path_a.owner.as_str()).or_default().push(pair);
    }

    // ── score each owner ──────────────────────────────────────────────────────
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut unmatched: usize = 0;

    for (owner_path, pairs) in &by_owner {
        let node_id = match path_to_id.get(owner_path) {
            Some(id) => *id,
            None => { unmatched += 1; continue; }
        };
        let node = graph.nodes.get(node_id);
        let intent  = node.and_then(|n| n.intent_class.as_deref());
        let prov    = node.map(|n| n.provenance.as_slice()).unwrap_or(&[]);
        let mir_bl  = node.and_then(|n| n.mir.as_ref()).map(|m| m.blocks);
        let effects = effect_map.get(node_id).unwrap_or(&empty);

        let mut reasoning = Vec::new();
        let s = score(intent, prov, effects, pairs.len(), mir_bl, &mut reasoning);
        let confidence = (s * 100.0).round() / 100.0;

        let pair_summaries: Vec<PairSummary> = pairs.iter().map(|p| {
            use std::collections::HashSet;
            let a: HashSet<usize> = p.path_a.blocks.iter().copied().collect();
            let b: HashSet<usize> = p.path_b.blocks.iter().copied().collect();
            let mut only_a: Vec<usize> = a.difference(&b).copied().collect();
            let mut only_b: Vec<usize> = b.difference(&a).copied().collect();
            only_a.sort_unstable();
            only_b.sort_unstable();
            PairSummary {
                shared_signature: p.shared_signature,
                blocks_a: p.path_a.blocks.clone(),
                blocks_b: p.path_b.blocks.clone(),
                only_in_a: only_a,
                only_in_b: only_b,
            }
        }).collect();

        candidates.push(Candidate {
            rank: 0,
            owner: node.map(|n| n.path.as_str()).unwrap_or(owner_path).to_string(),
            owner_node_id: node_id.to_string(),
            pair_count: pairs.len(),
            confidence,
            recommended_action: action(confidence).to_string(),
            intent_class: intent.map(str::to_string),
            resource: node.and_then(|n| n.resource.clone()),
            provenance: prov.to_vec(),
            effects: effects.labels(),
            mir_blocks: mir_bl,
            reasoning,
            pairs: pair_summaries,
        });
    }

    // ── sort: action tier → confidence desc → pair_count desc ─────────────────
    candidates.sort_by(|a, b| {
        let tier = |s: &str| match s { "safe_merge" => 0u8, "investigate" => 1, _ => 2 };
        tier(&a.recommended_action).cmp(&tier(&b.recommended_action))
            .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
            .then(b.pair_count.cmp(&a.pair_count))
    });
    for (i, c) in candidates.iter_mut().enumerate() {
        c.rank = i + 1;
    }

    let safe_merge  = candidates.iter().filter(|c| c.recommended_action == "safe_merge").count();
    let investigate = candidates.iter().filter(|c| c.recommended_action == "investigate").count();
    let skip        = candidates.iter().filter(|c| c.recommended_action == "skip").count();
    let total_pairs: usize = candidates.iter().map(|c| c.pair_count).sum();

    let out = CandidatesOutput {
        generated_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        schema_version: 1,
        graph_schema_version: graph.meta.schema_version,
        summary: Summary {
            total_redundant_pairs: total_pairs,
            unique_owner_functions: candidates.len(),
            safe_merge,
            investigate,
            skip,
            unmatched_owners: unmatched,
        },
        candidates,
    };

    let json = serde_json::to_vec_pretty(&out)?;
    std::fs::write(&out_path, &json)?;

    eprintln!("wrote {} candidates → {}", out.candidates.len(), out_path.display());
    eprintln!("  safe_merge={}  investigate={}  skip={}  unmatched={}",
        safe_merge, investigate, skip, unmatched);

    Ok(SemanticRankCandidatesReport {
        candidates: out.candidates.len(),
        safe_merge,
        investigate,
        skip,
        unmatched_owners: unmatched,
        out_path,
    })
}

pub fn run_from_cli_args(
    args: &[String],
    workspace_root: PathBuf,
) -> anyhow::Result<SemanticRankCandidatesReport> {
    let graph_path = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("state/rustc/canon_mini_agent/graph.json"));
    let out_path = args.get(1).map(PathBuf::from);
    run_with_options(SemanticRankCandidatesOptions {
        workspace_root,
        graph_path,
        out_path,
    })
}

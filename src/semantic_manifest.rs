//! Step 3/4 semantic manifest joiner.
//!
//! Reads graph.json, joins rustc facts + docstrings, proposes deterministic
//! defaults for missing fields, and writes semantic_manifest for every node.
//!
//! Usage:
//!   semantic_manifest [graph.json] [--write] [--out path]

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MISSING: &str = "error";

#[derive(Debug, Clone)]
pub struct SemanticManifestRunOptions {
    pub workspace: PathBuf,
    pub graph_path: PathBuf,
    pub out_path: PathBuf,
    pub write_mode: bool,
    pub max_error_rate: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticManifestRunReport {
    pub updated: usize,
    pub fn_total: usize,
    pub fn_with_any_error: usize,
    pub fn_error_rate: f64,
    pub target: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct CrateGraph {
    #[serde(default)]
    meta: serde_json::Value,
    #[serde(default)]
    nodes: HashMap<String, GraphNode>,
    #[serde(default)]
    edges: Vec<GraphEdge>,
    #[serde(flatten)]
    rest: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct ProposalFile {
    generated_at_ms: u64,
    graph_path: String,
    fn_total: usize,
    fn_with_any_error: usize,
    fn_error_rate: f64,
    proposals: HashMap<String, SemanticManifest>,
}

#[derive(Deserialize, Serialize, Clone, Default)]
struct GraphNode {
    #[serde(default)]
    path: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    intent_class: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    forbidden_effects: Vec<String>,
    #[serde(default)]
    invariants: Vec<String>,
    #[serde(default)]
    failure_mode: Option<String>,
    #[serde(default)]
    provenance: Vec<String>,
    #[serde(default)]
    docstring: Option<String>,
    #[serde(default)]
    semantic_manifest: Option<SemanticManifest>,
    #[serde(default)]
    def: Option<SourceSpan>,
}

#[derive(Deserialize, Serialize, Clone, Default)]
struct SourceSpan {
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: u32,
}

#[derive(Deserialize, Serialize, Clone, Default)]
struct GraphEdge {
    #[serde(default)]
    relation: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

#[derive(Deserialize, Serialize, Clone, Default)]
struct SemanticManifest {
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: String,
    #[serde(default)]
    intent_class: String,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    effects: Vec<String>,
    #[serde(default)]
    forbidden_effects: Vec<String>,
    #[serde(default)]
    calls: Vec<String>,
    #[serde(default)]
    failure_mode: String,
    #[serde(default)]
    invariants: Vec<String>,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    mutations: Vec<String>,
    #[serde(default)]
    tests: Vec<String>,
    #[serde(default)]
    provenance: Vec<String>,
    #[serde(default)]
    manifest_status: String,
}

#[derive(Default)]
struct DocContract {
    intent: Option<String>,
    resource: Option<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    effects: Vec<String>,
    forbidden: Vec<String>,
    invariants: Vec<String>,
    failure: Option<String>,
    provenance: Vec<String>,
}

fn parse_signature(signature: Option<&str>) -> (Vec<String>, Vec<String>) {
    let Some(sig) = signature.map(str::trim) else {
        return (vec![MISSING.to_string()], vec![MISSING.to_string()]);
    };
    let Some(rest) = sig.strip_prefix("fn(") else {
        return (vec![MISSING.to_string()], vec![MISSING.to_string()]);
    };
    let Some(close_idx) = rest.find(')') else {
        return (vec![MISSING.to_string()], vec![MISSING.to_string()]);
    };
    let input_src = rest[..close_idx].trim();
    let inputs = if input_src.is_empty() {
        vec!["()".to_string()]
    } else {
        input_src
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    };
    let tail = rest[close_idx + 1..].trim();
    let outputs = if let Some(out) = tail.strip_prefix("->") {
        let out = out.trim();
        if out.is_empty() {
            vec![MISSING.to_string()]
        } else {
            vec![out.to_string()]
        }
    } else {
        vec!["()".to_string()]
    };
    (inputs, outputs)
}

fn parse_doc_contract(lines: &[String]) -> DocContract {
    let mut out = DocContract::default();
    for raw in lines {
        let line = raw.trim().trim_start_matches("///").trim();
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim();
        match key.as_str() {
            "intent" => out.intent = Some(val.to_string()),
            "resource" => out.resource = Some(val.to_string()),
            "inputs" => out.inputs = split_csv(val),
            "outputs" => out.outputs = split_csv(val),
            "effects" => out.effects = split_csv(val),
            "forbidden" => out.forbidden = split_csv(val),
            "invariants" => out.invariants = split_csv(val),
            "failure" | "errors" => out.failure = Some(val.to_string()),
            "provenance" => out.provenance = split_plus(val),
            _ => {}
        }
    }
    out
}

fn split_csv(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn split_plus(v: &str) -> Vec<String> {
    v.split('+')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn normalize_list(xs: Vec<String>) -> Vec<String> {
    let out = xs
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "TODO")
        .collect::<Vec<_>>();
    if out.is_empty() {
        vec![MISSING.to_string()]
    } else {
        out
    }
}

fn choose_scalar(cands: &[Option<String>]) -> String {
    cands
        .iter()
        .flatten()
        .map(|v| v.trim())
        .find(|s| !s.is_empty() && *s != "TODO" && *s != MISSING)
        .map(str::to_string)
        .unwrap_or_else(|| MISSING.to_string())
}

fn choose_list(cands: &[Vec<String>]) -> Vec<String> {
    for cand in cands {
        let normalized = normalize_list(cand.clone());
        if !(normalized.len() == 1 && normalized[0] == MISSING) {
            return normalized;
        }
    }
    vec![MISSING.to_string()]
}

fn map_effect_relation(rel: &str) -> Option<&'static str> {
    match rel {
        "ReadsArtifact" => Some("fs_read"),
        "WritesArtifact" => Some("fs_write"),
        "ReadsState" => Some("state_read"),
        "WritesState" => Some("state_write"),
        "SpawnsProcess" => Some("spawns_process"),
        "UsesNetwork" => Some("uses_network"),
        "TransitionsState" => Some("transitions_state"),
        "PerformsLogging" => Some("logging"),
        _ => None,
    }
}

fn normalize_effect_label(effect: &str) -> String {
    match effect.trim() {
        "reads_artifact" | "ReadsArtifact" => "fs_read".to_string(),
        "writes_artifact" | "WritesArtifact" => "fs_write".to_string(),
        "reads_state" | "ReadsState" => "state_read".to_string(),
        "writes_state" | "WritesState" => "state_write".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => MISSING.to_string(),
    }
}

fn normalize_provenance_label(p: &str) -> String {
    match p.trim() {
        "generated" => "syn:docstring".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => MISSING.to_string(),
    }
}

fn infer_effects_from_calls(calls: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for call in calls {
        let c = call.to_ascii_lowercase();
        if c.contains("::fs::read")
            || c.contains("::read_to_string")
            || c.contains("::file::open")
            || c.contains("::read_dir")
            || c.contains("load_")
        {
            out.insert("fs_read".to_string());
        }
        if c.contains("::fs::write")
            || c.contains("::write_all")
            || c.contains("::file::create")
            || c.contains("::openoptions")
            || c.contains("save_")
            || c.contains("persist_")
        {
            out.insert("fs_write".to_string());
        }
        if c.contains("log::") || c.contains("tracing::") || c.contains("eprintln!") {
            out.insert("logging".to_string());
        }
        if c.contains("command::new") || c.contains("spawn") {
            out.insert("spawns_process".to_string());
        }
        if c.contains("http") || c.contains("reqwest") || c.contains("tcp") || c.contains("ws") {
            out.insert("uses_network".to_string());
        }
    }
    out.into_iter().collect()
}

fn resolve_doc_lines(
    node: &GraphNode,
    workspace: &Path,
    src_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Vec<String> {
    if let Some(doc) = node.docstring.as_deref() {
        let lines = doc
            .lines()
            .map(|l| format!("/// {}", l.trim()))
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            return lines;
        }
    }
    let Some(def) = node.def.as_ref() else {
        return vec![];
    };
    if def.file.is_empty() || def.line == 0 {
        return vec![];
    }
    let p = {
        let raw = PathBuf::from(&def.file);
        if raw.is_absolute() { raw } else { workspace.join(raw) }
    };
    let lines = src_cache.entry(p.clone()).or_insert_with(|| {
        std::fs::read_to_string(&p)
            .ok()
            .map(|s| s.lines().map(|l| l.to_string()).collect::<Vec<_>>())
            .unwrap_or_default()
    });
    if lines.is_empty() {
        return vec![];
    }
    let mut idx = (def.line as usize).saturating_sub(1);
    if idx > lines.len() {
        idx = lines.len();
    }
    let mut out = Vec::new();
    while idx > 0 {
        let prev = lines[idx - 1].trim_start();
        if prev.starts_with("///") {
            out.push(prev.to_string());
            idx -= 1;
            continue;
        }
        if prev.starts_with("#[") || prev.is_empty() {
            idx -= 1;
            continue;
        }
        break;
    }
    out.reverse();
    out
}

fn infer_resource(calls: &[String], effects: &[String], symbol: &str) -> String {
    let joined = calls.join(" ").to_ascii_lowercase();
    let sym = symbol.to_ascii_lowercase();
    if joined.contains("plan") {
        return "PLAN.json".to_string();
    }
    if joined.contains("issues") {
        return "ISSUES.json".to_string();
    }
    if joined.contains("invariant") {
        return "INVARIANTS.json".to_string();
    }
    if joined.contains("tlog") || joined.contains("event") {
        return "tlog.ndjson".to_string();
    }
    if sym.contains("plan") {
        return "PLAN.json".to_string();
    }
    if sym.contains("issue") {
        return "ISSUES.json".to_string();
    }
    if sym.contains("invariant") {
        return "INVARIANTS.json".to_string();
    }
    if sym.contains("spec") {
        return "SPEC.md".to_string();
    }
    if sym.contains("objective") {
        return "OBJECTIVES.json".to_string();
    }
    if effects.iter().any(|e| e == "fs_read" || e == "fs_write") {
        return "filesystem".to_string();
    }
    if effects.iter().any(|e| e == "state_read" || e == "state_write") {
        return "state".to_string();
    }
    if effects.iter().any(|e| e == "uses_network") {
        return "network".to_string();
    }
    if effects.iter().any(|e| e == "spawns_process") {
        return "process".to_string();
    }
    MISSING.to_string()
}

fn infer_intent(intent: Option<String>, effects: &[String], symbol: &str) -> String {
    if let Some(v) = intent {
        if !v.trim().is_empty() && v != MISSING && v != "TODO" {
            return v;
        }
    }
    let symbol = symbol.to_ascii_lowercase();
    if symbol.contains("validate") || symbol.contains("check_") || symbol.contains("assert") {
        return "validation_gate".to_string();
    }
    if symbol.contains("route") || symbol.contains("dispatch") {
        return "route_gate".to_string();
    }
    if symbol.contains("scan") || symbol.contains("analy") || symbol.contains("diagnostic") {
        return "diagnostic_scan".to_string();
    }
    if symbol.contains("repair") || symbol.contains("init") || symbol.contains("bootstrap") {
        return "repair_or_initialize".to_string();
    }
    if symbol.contains("append") || symbol.contains("record") || symbol.contains("emit") {
        return "event_append".to_string();
    }
    let has_read = effects.iter().any(|e| e == "fs_read" || e == "state_read");
    let has_write = effects.iter().any(|e| e == "fs_write" || e == "state_write");
    if has_read && !has_write {
        return "canonical_read".to_string();
    }
    if has_write && !has_read {
        return "canonical_write".to_string();
    }
    if effects
        .iter()
        .any(|e| e == "uses_network" || e == "spawns_process")
    {
        return "transport_effect".to_string();
    }
    if !has_read && !has_write {
        return "pure_transform".to_string();
    }
    "repair_or_initialize".to_string()
}

fn infer_failure(outputs: &[String], symbol: &str) -> String {
    let joined = outputs.join(", ");
    let sym = symbol.to_ascii_lowercase();
    if joined.contains("Result<") {
        "fail_closed".to_string()
    } else if joined.contains("Option<") {
        "propagates".to_string()
    } else if sym.contains("try_") || sym.contains("load_") {
        "fail_closed".to_string()
    } else {
        "infallible".to_string()
    }
}

fn infer_forbidden(intent: &str, effects: &[String]) -> Vec<String> {
    let has_write = effects.iter().any(|e| e == "fs_write" || e == "state_write");
    let has_network = effects.iter().any(|e| e == "uses_network");
    let has_process = effects.iter().any(|e| e == "spawns_process");
    match intent {
        "canonical_read" => vec!["fs_write".to_string(), "default_overwrite".to_string()],
        "canonical_write" => vec!["default_overwrite".to_string()],
        "validation_gate" => vec!["fs_write".to_string(), "state_write".to_string()],
        "diagnostic_scan" => vec!["fs_write".to_string()],
        "pure_transform" => vec![
            "fs_write".to_string(),
            "uses_network".to_string(),
            "spawns_process".to_string(),
        ],
        "transport_effect" => vec!["default_overwrite".to_string()],
        _ => {
            let mut out = Vec::new();
            if !has_write {
                out.push("fs_write".to_string());
                out.push("state_write".to_string());
            }
            if !has_network {
                out.push("uses_network".to_string());
            }
            if !has_process {
                out.push("spawns_process".to_string());
            }
            if out.is_empty() {
                vec!["default_overwrite".to_string()]
            } else {
                out
            }
        }
    }
}

fn infer_invariants(intent: &str, effects: &[String], resource: &str) -> Vec<String> {
    match (intent, resource) {
        ("canonical_read", "PLAN.json") => vec!["plan_is_authoritative".to_string()],
        ("canonical_write", "PLAN.json") => vec!["no_direct_plan_patch".to_string()],
        ("event_append", "tlog.ndjson") => vec!["append_only_log".to_string()],
        ("pure_transform", _) => vec!["no_external_effects".to_string()],
        ("validation_gate", _) => vec!["checks_must_gate_state_transition".to_string()],
        ("route_gate", _) => vec!["route_decision_is_explicit".to_string()],
        _ => {
            let has_external = effects.iter().any(|e| {
                e == "fs_write" || e == "state_write" || e == "uses_network" || e == "spawns_process"
            });
            if has_external {
                vec!["side_effects_are_intentional".to_string()]
            } else {
                vec!["deterministic_for_same_inputs".to_string()]
            }
        }
    }
}

pub fn run_with_options(options: SemanticManifestRunOptions) -> anyhow::Result<SemanticManifestRunReport> {
    let write_mode = options.write_mode;
    let max_error_rate = options.max_error_rate;
    let out_path = Some(options.out_path.clone());
    let graph_path = options.graph_path.clone();

    let workspace = options.workspace;
    let bytes = std::fs::read(&graph_path)?;
    let graph: CrateGraph = serde_json::from_slice(&bytes)?;

    let (effects_by_owner, calls_by_owner) = index_manifest_edges(&graph);

    let mut src_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut proposals: HashMap<String, SemanticManifest> = HashMap::new();
    let mut updated = 0usize;
    let mut fn_total = 0usize;
    let mut fn_with_any_error = 0usize;
    for (node_id, node) in &graph.nodes {
        let old = node.semantic_manifest.clone().unwrap_or_default();
        let doc_lines = resolve_doc_lines(node, &workspace, &mut src_cache);
        let doc = parse_doc_contract(&doc_lines);
        let (sig_inputs, sig_outputs) = parse_signature(node.signature.as_deref());
        let calls = calls_by_owner
            .get(node_id)
            .map(|s| s.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let inferred_effects = infer_effects_from_calls(&calls);
        let effects = choose_list(&[
            doc.effects.clone(),
            effects_by_owner
                .get(node_id)
                .map(|s| s.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            inferred_effects,
            old.effects.clone(),
        ]);
        let symbol = if node.path.is_empty() {
            node_id.clone()
        } else {
            node.path.clone()
        };
        let normalized_effects = normalize_list(
            effects
                .into_iter()
                .map(|e| normalize_effect_label(&e))
                .collect::<Vec<_>>(),
        );
        let intent = infer_intent(
            Some(choose_scalar(&[
                doc.intent.clone(),
                node.intent_class.clone(),
                Some(old.intent_class.clone()),
            ])),
            &normalized_effects,
            &symbol,
        );
        let resource = choose_scalar(&[
            doc.resource.clone(),
            node.resource.clone(),
            Some(old.resource.clone()),
            Some(infer_resource(&calls, &normalized_effects, &symbol)),
        ]);
        let outputs = choose_list(&[doc.outputs.clone(), sig_outputs, old.outputs.clone()]);
        let failure_mode = choose_scalar(&[
            doc.failure.clone(),
            node.failure_mode.clone(),
            Some(old.failure_mode.clone()),
            Some(infer_failure(&outputs, &symbol)),
        ]);
        let manifest = SemanticManifest {
            symbol: choose_scalar(&[
                Some(symbol),
                Some(old.symbol.clone()),
            ]),
            kind: choose_scalar(&[Some(node.kind.clone()), Some(old.kind.clone())]),
            file: choose_scalar(&[
                node.def.as_ref().map(|d| d.file.clone()),
                Some(old.file.clone()),
            ]),
            line: choose_scalar(&[
                node.def.as_ref().map(|d| d.line.to_string()),
                Some(old.line.clone()),
            ]),
            intent_class: intent.clone(),
            resource: resource.clone(),
            inputs: choose_list(&[doc.inputs, sig_inputs, old.inputs.clone()]),
            outputs,
            effects: normalized_effects.clone(),
            forbidden_effects: choose_list(&[
                doc.forbidden,
                node.forbidden_effects.clone(),
                old.forbidden_effects.clone(),
                infer_forbidden(&intent, &normalized_effects),
            ]),
            calls: choose_list(&[calls, old.calls.clone()]),
            failure_mode,
            invariants: choose_list(&[
                doc.invariants,
                node.invariants.clone(),
                old.invariants.clone(),
                infer_invariants(&intent, &normalized_effects, &resource),
            ]),
            branches: choose_list(&[old.branches.clone()]),
            mutations: choose_list(&[old.mutations.clone()]),
            tests: choose_list(&[old.tests.clone()]),
            provenance: normalize_list(
                choose_list(&[
                    doc.provenance,
                    node.provenance.clone(),
                    old.provenance.clone(),
                    vec!["rustc:facts".to_string()],
                ])
                .into_iter()
                .map(|p| normalize_provenance_label(&p))
                .collect::<Vec<_>>(),
            ),
            manifest_status: String::new(),
        };
        let has_error = manifest.intent_class == MISSING
            || manifest.resource == MISSING
            || manifest.inputs.iter().any(|v| v == MISSING)
            || manifest.outputs.iter().any(|v| v == MISSING)
            || manifest.effects.iter().any(|v| v == MISSING)
            || manifest.forbidden_effects.iter().any(|v| v == MISSING)
            || manifest.failure_mode == MISSING
            || manifest.invariants.iter().any(|v| v == MISSING)
            || manifest.provenance.iter().any(|v| v == MISSING);
        let mut manifest = manifest;
        manifest.manifest_status = if has_error {
            "partial_error".to_string()
        } else {
            "complete".to_string()
        };
        proposals.insert(node_id.clone(), manifest);
        if node.kind == "fn" {
            fn_total += 1;
            if has_error {
                fn_with_any_error += 1;
            }
        }
        updated += 1;
    }

    let error_rate = if fn_total == 0 {
        0.0
    } else {
        fn_with_any_error as f64 / fn_total as f64
    };

    let target = out_path
        .unwrap_or_else(|| PathBuf::from("agent_state/semantic_manifest_proposals.json"));
    let proposal_file = ProposalFile {
        generated_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        graph_path: graph_path.display().to_string(),
        fn_total,
        fn_with_any_error,
        fn_error_rate: error_rate,
        proposals,
    };
    if write_mode {
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&target, serde_json::to_vec_pretty(&proposal_file)?)?;
        eprintln!(
            "semantic_manifest: wrote {} proposals -> {}",
            updated,
            target.display()
        );
    } else {
        eprintln!("semantic_manifest: would write {updated} proposals (dry-run)");
    }
    eprintln!(
        "semantic_manifest: fn_error_rate={:.3} ({}/{})",
        error_rate, fn_with_any_error, fn_total
    );
    if let Some(max_allowed) = max_error_rate {
        if error_rate > max_allowed {
            anyhow::bail!(
                "semantic_manifest error rate {:.3} exceeds max {:.3}",
                error_rate,
                max_allowed
            );
        }
    }
    Ok(SemanticManifestRunReport {
        updated,
        fn_total,
        fn_with_any_error,
        fn_error_rate: error_rate,
        target,
    })
}

fn index_manifest_edges(
    graph: &CrateGraph,
) -> (HashMap<String, BTreeSet<String>>, HashMap<String, BTreeSet<String>>) {
    let node_path: HashMap<String, String> = graph
        .nodes
        .iter()
        .map(|(id, n)| (id.clone(), if n.path.is_empty() { id.clone() } else { n.path.clone() }))
        .collect();

    let mut effects_by_owner: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut calls_by_owner: HashMap<String, BTreeSet<String>> = HashMap::new();
    for e in &graph.edges {
        if let Some(lbl) = map_effect_relation(&e.relation) {
            effects_by_owner
                .entry(e.from.clone())
                .or_default()
                .insert(lbl.to_string());
        }
        if e.relation == "Calls" {
            calls_by_owner
                .entry(e.from.clone())
                .or_default()
                .insert(node_path.get(&e.to).cloned().unwrap_or_else(|| e.to.clone()));
        }
    }
    (effects_by_owner, calls_by_owner)
}

pub fn run_from_cli_args(args: &[String], workspace: PathBuf) -> anyhow::Result<SemanticManifestRunReport> {
    let write_mode = args.iter().any(|a| a == "--write");
    let max_error_rate = args
        .windows(2)
        .find(|w| w[0] == "--max-error-rate")
        .and_then(|w| w[1].parse::<f64>().ok());
    let out_path = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| PathBuf::from(&w[1]));
    let mut graph_path: Option<PathBuf> = None;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--out" || arg == "--max-error-rate" {
            i += 2;
            continue;
        }
        if arg.ends_with(".json") {
            graph_path = Some(PathBuf::from(arg));
            break;
        }
        i += 1;
    }
    run_with_options(SemanticManifestRunOptions {
        workspace,
        graph_path: graph_path.unwrap_or_else(|| PathBuf::from("state/rustc/canon_mini_agent/graph.json")),
        out_path: out_path.unwrap_or_else(|| PathBuf::from("agent_state/semantic_manifest_proposals.json")),
        write_mode,
        max_error_rate,
    })
}

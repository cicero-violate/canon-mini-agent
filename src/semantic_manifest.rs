//! Step 3/4 semantic manifest joiner.
//!
//! Reads graph.json, joins rustc facts + docstrings, proposes deterministic
//! defaults for missing fields, and writes semantic_manifest for every node.
//!
//! Usage:
//!   semantic_manifest [graph.json] [--write] [--out path]
//!   semantic_manifest [graph.json] --write
//!     writes semantic_manifest fields back into graph.json in place.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MISSING: &str = "error";
const NO_EFFECT: &str = "none";
const UNKNOWN_LOW_CONFIDENCE: &str = "unknown_low_confidence";

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
    pub fn_intent_classified: usize,
    pub fn_low_confidence: usize,
    pub fn_intent_coverage: f64,
    pub fn_low_confidence_rate: f64,
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
    fn_intent_classified: usize,
    fn_low_confidence: usize,
    fn_intent_coverage: f64,
    fn_low_confidence_rate: f64,
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
    intent_evidence: IntentEvidence,
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
struct IntentEvidence {
    #[serde(default)]
    from_doc: Option<String>,
    #[serde(default)]
    from_name: Option<String>,
    #[serde(default)]
    from_effects: Option<String>,
    #[serde(default)]
    confidence: f64,
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
        return missing_signature();
    };
    let Some(rest) = sig.strip_prefix("fn(") else {
        return missing_signature();
    };
    let Some(close_idx) = rest.find(')') else {
        return missing_signature();
    };
    parse_signature_parts(&rest[..close_idx], &rest[close_idx + 1..])
}

fn parse_source_decl_signature(decl: Option<&str>) -> (Vec<String>, Vec<String>) {
    let Some(decl) = decl.map(str::trim) else {
        return missing_signature();
    };
    let Some(fn_idx) = decl.find("fn ") else {
        return missing_signature();
    };
    let Some(open_rel) = decl[fn_idx..].find('(') else {
        return missing_signature();
    };
    let open_idx = fn_idx + open_rel;
    let Some(close_idx) = find_matching_paren(decl, open_idx) else {
        return missing_signature();
    };
    parse_signature_parts(
        &decl[open_idx + 1..close_idx],
        clean_decl_tail(&decl[close_idx + 1..]),
    )
}

fn missing_signature() -> (Vec<String>, Vec<String>) {
    (vec![MISSING.to_string()], vec![MISSING.to_string()])
}

fn unit_signature_part() -> Vec<String> {
    vec!["()".to_string()]
}

fn parse_signature_parts(input_src: &str, tail: &str) -> (Vec<String>, Vec<String>) {
    (
        parse_signature_inputs(input_src),
        parse_signature_outputs(tail),
    )
}

fn parse_signature_inputs(input_src: &str) -> Vec<String> {
    let input_src = input_src.trim();
    if input_src.is_empty() {
        return unit_signature_part();
    }
    split_csv(input_src)
}

fn parse_signature_outputs(tail: &str) -> Vec<String> {
    let Some(out) = tail.trim().strip_prefix("->") else {
        return unit_signature_part();
    };
    let out = out.trim();
    if out.is_empty() {
        vec![MISSING.to_string()]
    } else {
        vec![out.to_string()]
    }
}

fn clean_decl_tail(tail: &str) -> &str {
    tail.split(['{', ';'])
        .next()
        .unwrap_or("")
        .split(" where ")
        .next()
        .unwrap_or("")
        .trim()
}

fn find_matching_paren(src: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (idx, ch) in src[open_idx..].char_indices() {
        let delta = paren_depth_delta(ch);
        if delta == 0 {
            continue;
        }
        depth += delta;
        if depth == 0 {
            return Some(open_idx + idx);
        }
    }
    None
}

fn paren_depth_delta(ch: char) -> i32 {
    match ch {
        '(' => 1,
        ')' => -1,
        _ => 0,
    }
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
        .filter(|s| !is_missing_contract_value(s))
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
        .find(|s| !is_missing_contract_value(s))
        .map(str::to_string)
        .unwrap_or_else(|| MISSING.to_string())
}

fn is_missing_contract_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty()
        || trimmed == "TODO"
        || trimmed == MISSING
        || trimmed.eq_ignore_ascii_case(UNKNOWN_LOW_CONFIDENCE)
        || trimmed.eq_ignore_ascii_case("unknown")
        || trimmed.eq_ignore_ascii_case("missing")
}

fn doc_scalar_forces_error(value: Option<&str>) -> bool {
    value.is_some_and(is_hard_contract_error_value)
}

fn doc_list_forces_error(values: &[String]) -> bool {
    values.iter().any(|value| is_hard_contract_error_value(value))
}

fn is_hard_contract_error_value(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "hard_error" | "extractor_error" | "schema_error" | "schema_corruption" | "parse_error"
    ) || normalized.starts_with("hard_error:")
        || normalized.starts_with("extractor_error:")
        || normalized.starts_with("schema_error:")
        || normalized.starts_with("schema_corruption:")
        || normalized.starts_with("parse_error:")
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

fn choose_optional_list(cands: &[Vec<String>]) -> Vec<String> {
    for cand in cands {
        let normalized = normalize_list(cand.clone());
        if !(normalized.len() == 1 && normalized[0] == MISSING) {
            return normalized;
        }
    }
    Vec::new()
}

fn map_effect_relation(rel: &str) -> Option<&'static str> {
    match rel {
        "ReadsArtifact" => Some("fs_read"),
        "WritesArtifact" => Some("fs_write"),
        "ReadsState" => Some("state_read"),
        "WritesState" => Some("state_write"),
        "SpawnsProcess" => Some("spawns_process"),
        "UsesNetwork" => Some("uses_network"),
        "TransitionsState" | "CoordinatesTransition" => Some("state_write"),
        "TouchesWorkflowDomain" => Some("state_read"),
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
        "transitions_state" | "TransitionsState" | "CoordinatesTransition" => {
            "state_write".to_string()
        }
        "no_effect" | "NoEffect" | "none" => NO_EFFECT.to_string(),
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

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn effects_contain_any(effects: &[String], labels: &[&str]) -> bool {
    effects
        .iter()
        .any(|effect| labels.contains(&effect.as_str()))
}

fn infer_effects_from_calls(calls: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for call in calls {
        let c = call.to_ascii_lowercase();
        if contains_any(
            &c,
            &[
                "::fs::read",
                "::read_to_string",
                "::file::open",
                "::read_dir",
                "load_",
            ],
        ) {
            out.insert("fs_read".to_string());
        }
        if contains_any(
            &c,
            &[
                "::fs::write",
                "::write_all",
                "::file::create",
                "::openoptions",
                "save_",
                "persist_",
            ],
        ) {
            out.insert("fs_write".to_string());
        }
        if contains_any(&c, &["log::", "tracing::", "eprintln!"]) {
            out.insert("logging".to_string());
        }
        if contains_any(&c, &["command::new", "spawn"]) {
            out.insert("spawns_process".to_string());
        }
        if contains_any(&c, &["http", "reqwest", "tcp", "ws"]) {
            out.insert("uses_network".to_string());
        }
    }
    out.into_iter().collect()
}

fn infer_effects_from_symbol(symbol: &str) -> Vec<String> {
    let sym = symbol.to_ascii_lowercase();
    let mut out = BTreeSet::new();

    if contains_any(
        &sym,
        &["read", "load", "scan", "parse_", "from_file", "from_path"],
    ) {
        out.insert("fs_read".to_string());
    }
    if contains_any(
        &sym,
        &["write", "save", "persist", "flush", "append", "record"],
    ) {
        out.insert("fs_write".to_string());
    }
    if contains_any(
        &sym,
        &["update", "transition", "set_status", "apply_", "consume"],
    ) {
        out.insert("state_write".to_string());
    }
    if contains_any(&sym, &["command", "spawn", "process", "subprocess"]) {
        out.insert("spawns_process".to_string());
    }
    if contains_any(&sym, &["http", "chromium", "transport", "websocket", "ws_"]) {
        out.insert("uses_network".to_string());
    }

    if out.is_empty() {
        vec![NO_EFFECT.to_string()]
    } else {
        out.into_iter().collect()
    }
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
        if raw.is_absolute() {
            raw
        } else {
            workspace.join(raw)
        }
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

fn resolve_source_decl(
    node: &GraphNode,
    workspace: &Path,
    src_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Option<String> {
    let def = node.def.as_ref()?;
    if def.file.is_empty() || def.line == 0 {
        return None;
    }
    let p = {
        let raw = PathBuf::from(&def.file);
        if raw.is_absolute() {
            raw
        } else {
            workspace.join(raw)
        }
    };
    let lines = src_cache.entry(p.clone()).or_insert_with(|| {
        std::fs::read_to_string(&p)
            .ok()
            .map(|s| s.lines().map(|l| l.to_string()).collect::<Vec<_>>())
            .unwrap_or_default()
    });
    let start = (def.line as usize).saturating_sub(1);
    if start >= lines.len() {
        return None;
    }
    let mut decl = String::new();
    for line in lines.iter().skip(start).take(32) {
        let trimmed = line.trim();
        if !decl.is_empty() {
            decl.push(' ');
        }
        decl.push_str(trimmed);
        if trimmed.ends_with(";") || trimmed.ends_with("{") {
            break;
        }
    }
    if decl.is_empty() {
        None
    } else {
        Some(decl)
    }
}

fn infer_resource(calls: &[String], effects: &[String], symbol: &str) -> String {
    const TEXT_RULES: &[(&[&str], &str)] = &[
        (&["graph"], "graph.json"),
        (&["plan"], "PLAN.json"),
        (&["issues", "issue"], "ISSUES.json"),
        (&["invariant"], "INVARIANTS.json"),
        (&["tlog", "event"], "tlog.ndjson"),
        (&["spec"], "SPEC.md"),
        (&["objective"], "OBJECTIVES.json"),
        (&["diagnostic"], "diagnostics"),
        (&["message", "inbound", "outbound"], "message_frame"),
        (&["prompt"], "prompt_context"),
        (&["action"], "action_payload"),
    ];
    const EFFECT_RULES: &[(&[&str], &str)] = &[
        (&["fs_read", "fs_write"], "filesystem"),
        (&["state_read", "state_write"], "state"),
        (&["uses_network"], "network"),
        (&["spawns_process"], "process"),
    ];

    let text = format!("{} {}", calls.join(" "), symbol).to_ascii_lowercase();
    if let Some((_, resource)) = TEXT_RULES
        .iter()
        .find(|(needles, _)| contains_any(&text, needles))
    {
        return (*resource).to_string();
    }
    EFFECT_RULES
        .iter()
        .find(|(labels, _)| effects_contain_any(effects, labels))
        .map(|(_, resource)| (*resource).to_string())
        .unwrap_or_else(|| "memory".to_string())
}

fn infer_intent(
    evidence: &IntentEvidence,
    intent: Option<String>,
    effects: &[String],
    symbol: &str,
) -> String {
    if let Some(v) = evidence.from_doc.as_deref() {
        if !is_missing_contract_value(v) {
            return v.to_string();
        }
    }
    if let Some(v) = evidence.from_name.as_deref() {
        if !is_missing_contract_value(v) {
            return v.to_string();
        }
    }
    if let Some(v) = evidence.from_effects.as_deref() {
        if !is_missing_contract_value(v) {
            return v.to_string();
        }
    }
    if let Some(v) = intent {
        if !is_missing_contract_value(&v) {
            return v;
        }
    }
    const SYMBOL_RULES: &[(&[&str], &str)] = &[
        (&["validate", "check_", "assert"], "validation_gate"),
        (&["route", "dispatch"], "route_gate"),
        (&["scan", "analy", "diagnostic"], "diagnostic_scan"),
        (&["repair", "init", "bootstrap"], "repair_or_initialize"),
        (&["append", "record", "emit"], "event_append"),
    ];

    let symbol = symbol.to_ascii_lowercase();
    if let Some((_, intent)) = SYMBOL_RULES
        .iter()
        .find(|(needles, _)| contains_any(&symbol, needles))
    {
        return (*intent).to_string();
    }

    let has_read = effects_contain_any(effects, &["fs_read", "state_read"]);
    let has_write = effects_contain_any(effects, &["fs_write", "state_write"]);
    if has_read ^ has_write {
        return if has_read {
            "canonical_read"
        } else {
            "canonical_write"
        }
        .to_string();
    }
    if effects_contain_any(effects, &["uses_network", "spawns_process"]) {
        return "transport_effect".to_string();
    }
    if has_read && has_write {
        return "repair_or_initialize".to_string();
    }
    UNKNOWN_LOW_CONFIDENCE.to_string()
}

fn infer_failure(outputs: &[String], symbol: &str) -> String {
    let joined = outputs.join(", ");
    let sym = symbol.to_ascii_lowercase();
    if contains_any(&joined, &["Result<", "std::result::Result<"]) {
        "fail_closed".to_string()
    } else if contains_any(&joined, &["Option<", "std::option::Option<"]) {
        "propagates".to_string()
    } else if contains_any(&sym, &["try_", "load_"]) {
        "fail_closed".to_string()
    } else {
        "infallible".to_string()
    }
}

struct EffectPresence {
    write: bool,
    network: bool,
    process: bool,
}

impl EffectPresence {
    fn from_effects(effects: &[String]) -> Self {
        Self {
            write: effects_contain_any(effects, &["fs_write", "state_write"]),
            network: effects_contain_any(effects, &["uses_network"]),
            process: effects_contain_any(effects, &["spawns_process"]),
        }
    }
}

fn fallback_forbidden_effects(effects: &[String]) -> Vec<String> {
    let p = EffectPresence::from_effects(effects);
    let mut out = Vec::new();
    if !p.write {
        out.push("fs_write".to_string());
        out.push("state_write".to_string());
    }
    if !p.network {
        out.push("uses_network".to_string());
    }
    if !p.process {
        out.push("spawns_process".to_string());
    }
    if out.is_empty() {
        vec!["default_overwrite".to_string()]
    } else {
        out
    }
}

fn infer_forbidden(intent: &str, effects: &[String]) -> Vec<String> {
    match intent {
        "canonical_read" => vec!["fs_write".to_string(), "default_overwrite".to_string()],
        "canonical_write" => vec!["default_overwrite".to_string()],
        "event_append" => vec!["default_overwrite".to_string()],
        "validation_gate" => vec!["fs_write".to_string(), "state_write".to_string()],
        "diagnostic_scan" => vec!["fs_write".to_string()],
        "pure_transform" => vec![
            "fs_write".to_string(),
            "uses_network".to_string(),
            "spawns_process".to_string(),
        ],
        "transport_effect" => vec!["default_overwrite".to_string()],
        _ => fallback_forbidden_effects(effects),
    }
}

fn infer_invariants(intent: &str, effects: &[String], resource: &str) -> Vec<String> {
    const RULES: &[(&str, &str, &str)] = &[
        ("canonical_read", "PLAN.json", "plan_is_authoritative"),
        ("canonical_write", "PLAN.json", "no_direct_plan_patch"),
        ("event_append", "tlog.ndjson", "append_only_log"),
        ("*", "graph.json", "graph_is_derived_projection"),
        ("pure_transform", "*", "no_external_effects"),
        ("validation_gate", "*", "checks_must_gate_state_transition"),
        ("route_gate", "*", "route_decision_is_explicit"),
    ];
    if let Some((_, _, invariant)) = RULES
        .iter()
        .find(|(i, r, _)| (*i == "*" || *i == intent) && (*r == "*" || *r == resource))
    {
        return vec![(*invariant).to_string()];
    }
    if effects_contain_any(
        effects,
        &["fs_write", "state_write", "uses_network", "spawns_process"],
    ) {
        vec!["side_effects_are_intentional".to_string()]
    } else {
        vec!["deterministic_for_same_inputs".to_string()]
    }
}

pub fn run_with_options(
    options: SemanticManifestRunOptions,
) -> anyhow::Result<SemanticManifestRunReport> {
    let write_mode = options.write_mode;
    let max_error_rate = options.max_error_rate;
    let out_path = Some(options.out_path.clone());
    let graph_path = options.graph_path.clone();

    let workspace = options.workspace;
    let bytes = std::fs::read(&graph_path)?;
    let graph: CrateGraph = serde_json::from_slice(&bytes)?;

    let (effects_by_owner, calls_by_owner) = index_manifest_edges(&graph);

    let (
        proposals,
        updated,
        fn_total,
        fn_with_any_error,
        fn_intent_classified,
        fn_low_confidence,
    ) =
        build_semantic_manifest_proposals(&graph, &workspace, &effects_by_owner, &calls_by_owner);

    let error_rate = if fn_total == 0 {
        0.0
    } else {
        fn_with_any_error as f64 / fn_total as f64
    };
    let intent_coverage = if fn_total == 0 {
        1.0
    } else {
        fn_intent_classified as f64 / fn_total as f64
    };
    let low_confidence_rate = if fn_total == 0 {
        0.0
    } else {
        fn_low_confidence as f64 / fn_total as f64
    };

    finish_semantic_manifest_run(SemanticManifestRunFinish {
        write_mode,
        max_error_rate,
        out_path,
        graph_path,
        graph_bytes: &bytes,
        updated,
        fn_total,
        fn_with_any_error,
        fn_error_rate: error_rate,
        fn_intent_classified,
        fn_low_confidence,
        fn_intent_coverage: intent_coverage,
        fn_low_confidence_rate: low_confidence_rate,
        proposals,
    })
}

struct SemanticManifestRunFinish<'a> {
    write_mode: bool,
    max_error_rate: Option<f64>,
    out_path: Option<PathBuf>,
    graph_path: PathBuf,
    graph_bytes: &'a [u8],
    updated: usize,
    fn_total: usize,
    fn_with_any_error: usize,
    fn_error_rate: f64,
    fn_intent_classified: usize,
    fn_low_confidence: usize,
    fn_intent_coverage: f64,
    fn_low_confidence_rate: f64,
    proposals: HashMap<String, SemanticManifest>,
}

fn finish_semantic_manifest_run(
    finish: SemanticManifestRunFinish<'_>,
) -> anyhow::Result<SemanticManifestRunReport> {
    let target = finish
        .out_path
        .unwrap_or_else(|| PathBuf::from("agent_state/semantic_manifest_proposals.json"));
    let proposal_file = ProposalFile {
        generated_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        graph_path: finish.graph_path.display().to_string(),
        fn_total: finish.fn_total,
        fn_with_any_error: finish.fn_with_any_error,
        fn_error_rate: finish.fn_error_rate,
        fn_intent_classified: finish.fn_intent_classified,
        fn_low_confidence: finish.fn_low_confidence,
        fn_intent_coverage: finish.fn_intent_coverage,
        fn_low_confidence_rate: finish.fn_low_confidence_rate,
        proposals: finish.proposals,
    };
    write_semantic_manifest_proposals(
        finish.write_mode,
        &target,
        &finish.graph_path,
        finish.graph_bytes,
        finish.updated,
        &proposal_file,
    )?;
    report_semantic_manifest_error_rate(
        finish.max_error_rate,
        finish.fn_error_rate,
        finish.fn_with_any_error,
        finish.fn_total,
    )?;
    Ok(SemanticManifestRunReport {
        updated: finish.updated,
        fn_total: finish.fn_total,
        fn_with_any_error: finish.fn_with_any_error,
        fn_error_rate: finish.fn_error_rate,
        fn_intent_classified: finish.fn_intent_classified,
        fn_low_confidence: finish.fn_low_confidence,
        fn_intent_coverage: finish.fn_intent_coverage,
        fn_low_confidence_rate: finish.fn_low_confidence_rate,
        target,
    })
}

fn write_semantic_manifest_proposals(
    write_mode: bool,
    target: &Path,
    graph_path: &Path,
    graph_bytes: &[u8],
    updated: usize,
    proposal_file: &ProposalFile,
) -> anyhow::Result<()> {
    if !write_mode {
        eprintln!("semantic_manifest: would write {updated} proposals (dry-run)");
        return Ok(());
    }
    if target == graph_path {
        let mut raw_graph: serde_json::Value = serde_json::from_slice(graph_bytes)?;
        for (node_id, manifest) in &proposal_file.proposals {
            if let Some(node) = raw_graph
                .get_mut("nodes")
                .and_then(|nodes| nodes.get_mut(node_id))
                .and_then(|node| node.as_object_mut())
            {
                node.insert(
                    "semantic_manifest".to_string(),
                    serde_json::to_value(manifest)?,
                );
            }
        }
        std::fs::write(target, serde_json::to_vec_pretty(&raw_graph)?)?;
        eprintln!(
            "semantic_manifest: wrote {} semantic manifests into {}",
            updated,
            target.display()
        );
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(target, serde_json::to_vec_pretty(proposal_file)?)?;
    eprintln!(
        "semantic_manifest: wrote {} proposals -> {}",
        updated,
        target.display()
    );
    Ok(())
}

fn report_semantic_manifest_error_rate(
    max_error_rate: Option<f64>,
    error_rate: f64,
    fn_with_any_error: usize,
    fn_total: usize,
) -> anyhow::Result<()> {
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
    Ok(())
}

struct DocForcedErrorFlags {
    intent: bool,
    resource: bool,
    inputs: bool,
    outputs: bool,
    effects: bool,
    forbidden: bool,
    invariants: bool,
    failure: bool,
    provenance: bool,
}

fn doc_forced_error_flags(doc: &DocContract) -> DocForcedErrorFlags {
    DocForcedErrorFlags {
        intent: doc_scalar_forces_error(doc.intent.as_deref()),
        resource: doc_scalar_forces_error(doc.resource.as_deref()),
        inputs: doc_list_forces_error(&doc.inputs),
        outputs: doc_list_forces_error(&doc.outputs),
        effects: doc_list_forces_error(&doc.effects),
        forbidden: doc_list_forces_error(&doc.forbidden),
        invariants: doc_list_forces_error(&doc.invariants),
        failure: doc_scalar_forces_error(doc.failure.as_deref()),
        provenance: doc_list_forces_error(&doc.provenance),
    }
}

fn apply_doc_forced_error_flags(manifest: &mut SemanticManifest, flags: &DocForcedErrorFlags) {
    if flags.intent {
        manifest.intent_class = MISSING.to_string();
    }
    if flags.resource {
        manifest.resource = MISSING.to_string();
    }
    if flags.inputs {
        manifest.inputs = vec![MISSING.to_string()];
    }
    if flags.outputs {
        manifest.outputs = vec![MISSING.to_string()];
    }
    if flags.effects {
        manifest.effects = vec![MISSING.to_string()];
    }
    if flags.forbidden {
        manifest.forbidden_effects = vec![MISSING.to_string()];
    }
    if flags.invariants {
        manifest.invariants = vec![MISSING.to_string()];
    }
    if flags.failure {
        manifest.failure_mode = MISSING.to_string();
    }
    if flags.provenance {
        manifest.provenance = vec![MISSING.to_string()];
    }
}

fn build_semantic_manifest_proposals(
    graph: &CrateGraph,
    workspace: &Path,
    effects_by_owner: &HashMap<String, BTreeSet<String>>,
    calls_by_owner: &HashMap<String, BTreeSet<String>>,
) -> (
    HashMap<String, SemanticManifest>,
    usize,
    usize,
    usize,
    usize,
    usize,
) {
    let mut src_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut proposals: HashMap<String, SemanticManifest> = HashMap::new();
    let mut updated = 0usize;
    let mut fn_total = 0usize;
    let mut fn_with_any_error = 0usize;
    let mut fn_intent_classified = 0usize;
    let mut fn_low_confidence = 0usize;
    for (node_id, node) in &graph.nodes {
        let mut manifest = build_node_semantic_manifest_proposal(
            node_id,
            node,
            workspace,
            &mut src_cache,
            effects_by_owner,
            calls_by_owner,
        );
        let manifest_eval = finalize_manifest_status(&mut manifest);
        proposals.insert(node_id.clone(), manifest);
        if node.kind == "fn" {
            fn_total += 1;
            if manifest_eval.has_error {
                fn_with_any_error += 1;
            }
            if manifest_eval.intent_is_classified {
                fn_intent_classified += 1;
            }
            if manifest_eval.has_low_confidence {
                fn_low_confidence += 1;
            }
        }
        updated += 1;
    }

    (
        proposals,
        updated,
        fn_total,
        fn_with_any_error,
        fn_intent_classified,
        fn_low_confidence,
    )
}

struct ManifestProposalEval {
    has_error: bool,
    has_low_confidence: bool,
    intent_is_classified: bool,
}

fn finalize_manifest_status(manifest: &mut SemanticManifest) -> ManifestProposalEval {
    let has_error = manifest_has_error(manifest);
    let has_low_confidence = manifest_has_low_confidence(manifest);
    manifest.manifest_status = if has_error {
        "partial_error".to_string()
    } else if has_low_confidence {
        "low_confidence".to_string()
    } else {
        "complete".to_string()
    };
    ManifestProposalEval {
        has_error,
        has_low_confidence,
        intent_is_classified: intent_is_classified(&manifest.intent_class),
    }
}

fn build_node_semantic_manifest_proposal(
    node_id: &str,
    node: &GraphNode,
    workspace: &Path,
    src_cache: &mut HashMap<PathBuf, Vec<String>>,
    effects_by_owner: &HashMap<String, BTreeSet<String>>,
    calls_by_owner: &HashMap<String, BTreeSet<String>>,
) -> SemanticManifest {
    let old = node.semantic_manifest.clone().unwrap_or_default();
    let doc_lines = resolve_doc_lines(node, workspace, src_cache);
    let doc = parse_doc_contract(&doc_lines);
    let doc_error_flags = doc_forced_error_flags(&doc);
    let (sig_inputs, sig_outputs) = parse_signature(node.signature.as_deref());
    let source_decl = resolve_source_decl(node, workspace, src_cache);
    let (src_inputs, src_outputs) = parse_source_decl_signature(source_decl.as_deref());
    let symbol = if node.path.is_empty() {
        node_id.to_string()
    } else {
        node.path.clone()
    };
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
        infer_effects_from_symbol(&symbol),
        old.effects.clone(),
    ]);
    let normalized_effects = normalize_list(
        effects
            .into_iter()
            .map(|e| normalize_effect_label(&e))
            .collect::<Vec<_>>(),
    );
    let intent = infer_intent(
        &node.intent_evidence,
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
    let outputs = choose_list(&[
        doc.outputs.clone(),
        sig_outputs,
        src_outputs,
        old.outputs.clone(),
    ]);
    let failure_mode = choose_scalar(&[
        doc.failure.clone(),
        node.failure_mode.clone(),
        Some(old.failure_mode.clone()),
        Some(infer_failure(&outputs, &symbol)),
    ]);
    let mut manifest = SemanticManifest {
        symbol: choose_scalar(&[Some(symbol), Some(old.symbol.clone())]),
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
        inputs: choose_list(&[doc.inputs, sig_inputs, src_inputs, old.inputs.clone()]),
        outputs,
        effects: normalized_effects.clone(),
        forbidden_effects: choose_list(&[
            doc.forbidden,
            node.forbidden_effects.clone(),
            old.forbidden_effects.clone(),
            infer_forbidden(&intent, &normalized_effects),
        ]),
        calls: choose_optional_list(&[calls, old.calls.clone()]),
        failure_mode,
        invariants: choose_list(&[
            doc.invariants,
            node.invariants.clone(),
            old.invariants.clone(),
            infer_invariants(&intent, &normalized_effects, &resource),
        ]),
        branches: choose_optional_list(&[old.branches.clone()]),
        mutations: choose_optional_list(&[old.mutations.clone()]),
        tests: choose_optional_list(&[old.tests.clone()]),
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
    apply_doc_forced_error_flags(&mut manifest, &doc_error_flags);
    manifest
}

fn manifest_has_error(manifest: &SemanticManifest) -> bool {
    manifest_scalar_has_error(manifest)
        || contains_missing(&manifest.inputs)
        || contains_missing(&manifest.outputs)
        || contains_missing(&manifest.effects)
        || contains_missing(&manifest.forbidden_effects)
        || contains_missing(&manifest.invariants)
        || contains_missing(&manifest.provenance)
}

fn manifest_has_low_confidence(manifest: &SemanticManifest) -> bool {
    intent_is_low_confidence(&manifest.intent_class)
}

fn intent_is_classified(intent: &str) -> bool {
    !is_missing_contract_value(intent) && !intent_is_low_confidence(intent)
}

fn intent_is_low_confidence(intent: &str) -> bool {
    let intent = intent.trim();
    intent.is_empty()
        || intent.eq_ignore_ascii_case("unknown")
        || intent.eq_ignore_ascii_case(UNKNOWN_LOW_CONFIDENCE)
}

fn manifest_scalar_has_error(manifest: &SemanticManifest) -> bool {
    manifest.intent_class == MISSING
        || manifest.resource == MISSING
        || manifest.failure_mode == MISSING
}

fn contains_missing(values: &[String]) -> bool {
    values.iter().any(|v| v == MISSING)
}

fn index_manifest_edges(
    graph: &CrateGraph,
) -> (
    HashMap<String, BTreeSet<String>>,
    HashMap<String, BTreeSet<String>>,
) {
    let node_path: HashMap<String, String> = graph
        .nodes
        .iter()
        .map(|(id, n)| {
            (
                id.clone(),
                if n.path.is_empty() {
                    id.clone()
                } else {
                    n.path.clone()
                },
            )
        })
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
            calls_by_owner.entry(e.from.clone()).or_default().insert(
                node_path
                    .get(&e.to)
                    .cloned()
                    .unwrap_or_else(|| e.to.clone()),
            );
        }
    }
    (effects_by_owner, calls_by_owner)
}

pub fn run_from_cli_args(
    args: &[String],
    workspace: PathBuf,
) -> anyhow::Result<SemanticManifestRunReport> {
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
    let resolved_graph_path =
        graph_path.unwrap_or_else(|| PathBuf::from("state/rustc/canon_mini_agent/graph.json"));
    let resolved_out_path = out_path.unwrap_or_else(|| {
        if write_mode {
            resolved_graph_path.clone()
        } else {
            PathBuf::from("agent_state/semantic_manifest_proposals.json")
        }
    });
    run_with_options(SemanticManifestRunOptions {
        workspace,
        graph_path: resolved_graph_path,
        out_path: resolved_out_path,
        write_mode,
        max_error_rate,
    })
}

//! Phase 2 write-back: generates structured docstrings and writes them to source.
//!
//! Modes:
//!   (default)  Generate full docstrings for fn nodes with a seeded intent_class
//!              but no structured docstring yet (provenance lacks "rustc:docstring").
//!   --augment  Extend existing single-line `/// Intent: X` docstrings with the
//!              remaining canonical contract fields.
//!   --rewrite-existing  Force-normalize existing `///` doc blocks to canonical
//!              contract shape (replace block if present, insert if missing).
//!
//! Usage:
//!   canon-mini-agent syn-writer [graph.json] [--write] [--augment|--rewrite-existing]
//!
//! Generated docstring format:
//!   /// Intent: <class>
//!   /// Resource: <target>
//!   /// Inputs: <typed inputs>
//!   /// Outputs: <typed outputs>
//!   /// Effects: <state changes / IO>
//!   /// Forbidden: <disallowed effects>
//!   /// Invariants: <must always hold>
//!   /// Failure: <failure modes>
//!   /// Provenance: <origin chain>
//!
//! After --write, rebuild with the canon-rustc-v2 RUSTC_WRAPPER to upgrade
//! provenance to "rustc:docstring" in the next graph.json capture.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Graph types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CrateGraph {
    #[serde(default)]
    nodes: HashMap<String, GraphNode>,
    #[serde(default)]
    edges: Vec<GraphEdge>,
}

#[derive(Deserialize, Default)]
struct ManifestProposalFile {
    #[serde(default)]
    proposals: HashMap<String, SemanticManifest>,
}

#[derive(Deserialize)]
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
    provenance: Vec<String>,
    #[serde(default)]
    docstring: Option<String>,
    #[serde(default)]
    semantic_manifest: Option<SemanticManifest>,
    #[serde(default)]
    def: Option<SourceSpan>,
}

#[derive(Deserialize, Default)]
struct SemanticManifest {
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
    invariants: Vec<String>,
    #[serde(default)]
    failure_mode: String,
    #[serde(default)]
    provenance: Vec<String>,
}

#[derive(Deserialize)]
struct SourceSpan {
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: u32,
}

#[derive(Deserialize)]
struct GraphEdge {
    #[serde(default)]
    relation: String,
    #[serde(default)]
    from: String,
}

// ---------------------------------------------------------------------------
// Log output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WriterLog {
    generated_at_ms: u64,
    dry_run: bool,
    mode: String,
    summary: LogSummary,
    actions: Vec<LogAction>,
}

#[derive(Debug, Clone)]
pub struct SynWriterRunOptions {
    pub workspace_root: PathBuf,
    pub graph_path: PathBuf,
    pub manifest_path: PathBuf,
    pub log_path: PathBuf,
    pub write_mode: bool,
    pub augment: bool,
    pub rewrite_existing: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SynWriterRunReport {
    pub candidates: usize,
    pub generated: usize,
    pub dry_run_pending: usize,
    pub skip_existing_doc: usize,
    pub skip_complex_attr: usize,
    pub skip_bad_span: usize,
    pub skip_no_change: usize,
    pub skip_write_error: usize,
}

#[derive(Serialize)]
struct LogSummary {
    candidates: usize,
    generated: usize,
    dry_run_pending: usize,
    skip_existing_doc: usize,
    skip_complex_attr: usize,
    skip_bad_span: usize,
    skip_no_change: usize,
    skip_write_error: usize,
}

#[derive(Serialize)]
struct LogAction {
    status: String,
    fn_path: String,
    file: String,
    fn_line: u32,
    intent_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    generated_text: Option<String>,
}

// ---------------------------------------------------------------------------
// Effect derivation
// ---------------------------------------------------------------------------

struct EffectSet {
    reads_artifact: bool,
    reads_state: bool,
    writes_artifact: bool,
    writes_state: bool,
    spawns_process: bool,
    uses_network: bool,
    transitions_state: bool,
    logging: bool,
}

impl EffectSet {
    fn labels(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.reads_artifact {
            v.push("reads_artifact");
        }
        if self.reads_state {
            v.push("reads_state");
        }
        if self.writes_artifact {
            v.push("writes_artifact");
        }
        if self.writes_state {
            v.push("writes_state");
        }
        if self.transitions_state {
            v.push("transitions_state");
        }
        if self.spawns_process {
            v.push("spawns_process");
        }
        if self.uses_network {
            v.push("uses_network");
        }
        if self.logging {
            v.push("logging");
        }
        v
    }
}

fn build_effect_map(edges: &[GraphEdge]) -> HashMap<String, EffectSet> {
    let mut map: HashMap<String, EffectSet> = HashMap::new();
    for edge in edges {
        let e = map.entry(edge.from.clone()).or_insert(EffectSet {
            reads_artifact: false,
            reads_state: false,
            writes_artifact: false,
            writes_state: false,
            spawns_process: false,
            uses_network: false,
            transitions_state: false,
            logging: false,
        });
        match edge.relation.as_str() {
            "ReadsArtifact" => {
                e.reads_artifact = true;
            }
            "ReadsState" => {
                e.reads_state = true;
            }
            "WritesArtifact" => {
                e.writes_artifact = true;
            }
            "WritesState" => {
                e.writes_state = true;
            }
            "SpawnsProcess" => {
                e.spawns_process = true;
            }
            "UsesNetwork" => {
                e.uses_network = true;
            }
            "TransitionsState" => {
                e.transitions_state = true;
            }
            "PerformsLogging" => {
                e.logging = true;
            }
            _ => {}
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Docstring builder
// ---------------------------------------------------------------------------

fn parse_signature(signature: Option<&str>) -> (Vec<String>, Vec<String>) {
    let Some(sig) = signature.map(str::trim) else {
        return (vec!["error".to_string()], vec!["error".to_string()]);
    };
    let Some(rest) = sig.strip_prefix("fn(") else {
        return (vec!["error".to_string()], vec!["error".to_string()]);
    };
    let Some(close_idx) = rest.find(')') else {
        return (vec!["error".to_string()], vec!["error".to_string()]);
    };
    let input_src = rest[..close_idx].trim();
    let inputs = if input_src.is_empty() {
        vec!["()".to_string()]
    } else {
        split_top_level_csv(input_src)
    };
    let tail = rest[close_idx + 1..].trim();
    let outputs = if let Some(out) = tail.strip_prefix("->") {
        let out = out.trim();
        if out.is_empty() {
            vec!["error".to_string()]
        } else {
            vec![out.to_string()]
        }
    } else {
        vec!["()".to_string()]
    };
    (inputs, outputs)
}

fn split_top_level_csv(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;

    for (idx, ch) in v.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let part = v[start..idx].trim();
                if !part.is_empty() {
                    out.push(part.to_string());
                }
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    let part = v[start..].trim();
    if !part.is_empty() {
        out.push(part.to_string());
    }
    out
}

fn normalize_list(items: &[String]) -> Vec<String> {
    let filtered: Vec<String> = items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if filtered.is_empty() {
        vec!["error".to_string()]
    } else {
        filtered
    }
}

fn normalize_scalar(s: Option<&str>) -> String {
    s.map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("error")
        .to_string()
}

fn build_doc(
    node: &GraphNode,
    proposal_manifest: Option<&SemanticManifest>,
    effects: Option<&EffectSet>,
    indent: &str,
) -> String {
    let manifest = proposal_manifest.or(node.semantic_manifest.as_ref());
    let intent = normalize_scalar(
        manifest
            .and_then(|m| {
                if scalar_is_unclassified_intent(&m.intent_class) {
                    None
                } else {
                    Some(m.intent_class.as_str())
                }
            })
            .or(node.intent_class.as_deref()),
    );

    let (sig_inputs, sig_outputs) = parse_signature(node.signature.as_deref());
    let inputs = normalize_list(
        manifest
            .map(|m| m.inputs.as_slice())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_vec())
            .unwrap_or(sig_inputs)
            .as_slice(),
    );
    let outputs = normalize_list(
        manifest
            .map(|m| m.outputs.as_slice())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_vec())
            .unwrap_or(sig_outputs)
            .as_slice(),
    );

    let effects_from_manifest = manifest
        .map(|m| normalize_list(&m.effects))
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v != "error")
        .collect::<Vec<_>>();
    let effects_labels = if !effects_from_manifest.is_empty() {
        effects_from_manifest
    } else if let Some(eff) = effects {
        let lbs = eff
            .labels()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if lbs.is_empty() {
            vec!["error".to_string()]
        } else {
            lbs
        }
    } else {
        vec!["error".to_string()]
    };

    let invariants = normalize_list(
        manifest
            .map(|m| m.invariants.clone())
            .unwrap_or_else(|| vec!["error".to_string()])
            .as_slice(),
    );
    let forbidden = normalize_list(
        manifest
            .map(|m| m.forbidden_effects.clone())
            .unwrap_or_else(|| vec!["error".to_string()])
            .as_slice(),
    );
    let failure = normalize_scalar(manifest.and_then(|m| {
        if m.failure_mode == "error" {
            None
        } else {
            Some(m.failure_mode.as_str())
        }
    }));
    let resource = normalize_scalar(
        manifest
            .and_then(|m| {
                if m.resource == "error" {
                    None
                } else {
                    Some(m.resource.as_str())
                }
            })
            .or(node.resource.as_deref()),
    );
    let provenance = normalize_list(
        manifest
            .map(|m| m.provenance.clone())
            .unwrap_or_else(|| vec!["error".to_string()])
            .as_slice(),
    );

    let mut lines = vec![format!("{indent}/// Intent: {intent}")];
    lines.push(format!("{indent}/// Resource: {resource}"));
    lines.push(format!("{indent}/// Inputs: {}", inputs.join(", ")));
    lines.push(format!("{indent}/// Outputs: {}", outputs.join(", ")));
    lines.push(format!(
        "{indent}/// Effects: {}",
        effects_labels.join(", ")
    ));
    lines.push(format!("{indent}/// Forbidden: {}", forbidden.join(", ")));
    lines.push(format!("{indent}/// Invariants: {}", invariants.join(", ")));
    lines.push(format!("{indent}/// Failure: {failure}"));
    lines.push(format!(
        "{indent}/// Provenance: {}",
        provenance.join(" + ")
    ));
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

// ---------------------------------------------------------------------------
// Source manipulation helpers
// ---------------------------------------------------------------------------

fn leading_ws(line: &str) -> &str {
    let r = line.trim_start_matches([' ', '\t']);
    &line[..line.len() - r.len()]
}

/// 0-based index to insert a new docstring block, scanning backward past `#[...]`.
fn find_insert_point(lines: &[&str], fn_line_0: usize) -> Result<usize, &'static str> {
    let mut scan = fn_line_0;
    while scan > 0 {
        let prev = lines[scan - 1].trim();
        if prev.starts_with("///") || prev.starts_with("//!") {
            return Err("existing_doc");
        }
        if prev.starts_with("#[") {
            if !prev.contains(']') {
                return Err("complex_attr");
            }
            scan -= 1;
        } else {
            break;
        }
    }
    Ok(scan)
}

/// 0-based index of the `/// Intent:` line immediately before `fn_line_0`.
fn find_intent_line(lines: &[&str], fn_line_0: usize) -> Option<usize> {
    let mut scan = fn_line_0;
    while scan > 0 {
        let prev = lines[scan - 1].trim();
        if prev.starts_with("#[") {
            scan -= 1;
            continue;
        }
        if prev.starts_with("/// Intent:") {
            return Some(scan - 1);
        }
        return None;
    }
    None
}

/// Expands around `intent_line_idx` to include the full contiguous `///` block.
fn find_doc_block_range(lines: &[&str], intent_line_idx: usize) -> (usize, usize) {
    let mut start = intent_line_idx;
    while start > 0 && lines[start - 1].trim_start().starts_with("///") {
        start -= 1;
    }
    let mut end = intent_line_idx + 1;
    while end < lines.len() && lines[end].trim_start().starts_with("///") {
        end += 1;
    }
    (start, end)
}

/// Finds a contiguous `///` block immediately before a fn declaration,
/// optionally separated from `fn` by simple `#[...]` attributes.
fn find_doc_block_before_fn(lines: &[&str], fn_line_0: usize) -> Option<(usize, usize)> {
    let mut scan = fn_line_0;
    while scan > 0 {
        let prev = lines[scan - 1].trim();
        if prev.starts_with("#[") {
            if !prev.contains(']') {
                return None;
            }
            scan -= 1;
            continue;
        }
        break;
    }
    if scan == 0 || !lines[scan - 1].trim_start().starts_with("///") {
        return None;
    }
    let end = scan;
    let mut start = scan - 1;
    while start > 0 && lines[start - 1].trim_start().starts_with("///") {
        start -= 1;
    }
    Some((start, end))
}

fn node_has_contract_intent(
    node: &GraphNode,
    proposal_manifest: Option<&SemanticManifest>,
) -> bool {
    node.intent_class
        .as_deref()
        .map(str::trim)
        .filter(|v| !scalar_is_unclassified_intent(v))
        .is_some()
        || node
            .semantic_manifest
            .as_ref()
            .or(proposal_manifest)
            .map(|m| m.intent_class.trim())
            .filter(|v| !scalar_is_unclassified_intent(v))
            .is_some()
}

fn scalar_is_unclassified_intent(value: &str) -> bool {
    matches!(
        value.trim(),
        "" | "error" | "unknown" | "unknown_low_confidence"
    )
}

fn path_under_workspace_src(file: &str, workspace_root: &Path, workspace_src: &Path) -> bool {
    let raw = PathBuf::from(file);
    let joined = if raw.is_absolute() {
        raw.clone()
    } else {
        workspace_root.join(&raw)
    };
    if let Ok(canon) = joined.canonicalize() {
        return canon.starts_with(workspace_src);
    }
    if raw.is_absolute() {
        raw.starts_with(workspace_src)
    } else {
        raw.starts_with("src")
    }
}

fn resolve_path(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn is_fn_decl_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || !t.contains('(') {
        return false;
    }
    let prefixes = [
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "pub(super) fn ",
        "async fn ",
        "pub async fn ",
        "pub(crate) async fn ",
        "pub(super) async fn ",
        "unsafe fn ",
        "pub unsafe fn ",
        "const fn ",
        "pub const fn ",
        "pub(crate) const fn ",
        "pub(super) const fn ",
    ];
    prefixes.iter().any(|p| t.starts_with(p))
}

fn resolve_fn_decl_line(lines: &[&str], hint_line_0: usize) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    let hint = hint_line_0.min(lines.len() - 1);
    let lo = hint.saturating_sub(8);
    for idx in (lo..=hint).rev() {
        if is_fn_decl_line(lines[idx]) {
            return Some(idx);
        }
    }
    let hi = (hint + 8).min(lines.len() - 1);
    for (idx, line) in lines.iter().enumerate().take(hi + 1).skip(hint + 1) {
        if is_fn_decl_line(line) {
            return Some(idx);
        }
    }
    None
}

fn mark_actions_after_write(
    actions: &mut [LogAction],
    indices: &[usize],
    write_ok: bool,
    success_status: &str,
    n_gen: &mut usize,
    n_write_err: &mut usize,
) {
    for idx in indices {
        if let Some(action) = actions.get_mut(*idx) {
            if write_ok {
                action.status = success_status.to_string();
                *n_gen += 1;
            } else {
                action.status = "error:write_failed".to_string();
                action.generated_text = None;
                *n_write_err += 1;
            }
        }
    }
}

struct Insertion {
    insert_at: usize,
    text: String,
    fn_line: u32,
}
struct Replacement {
    start_idx: usize,
    end_idx: usize,
    new_text: String,
    fn_line: u32,
}

fn apply_insertions(source: &str, ins: &[Insertion]) -> String {
    let mut lines: Vec<&str> = source.split('\n').collect();
    for i in ins {
        for (off, new_line) in i.text.trim_end_matches('\n').split('\n').enumerate() {
            lines.insert(i.insert_at + off, new_line);
        }
    }
    lines.join("\n")
}

fn apply_replacements(source: &str, reps: &[Replacement]) -> String {
    let mut lines: Vec<String> = source.split('\n').map(str::to_string).collect();
    for r in reps {
        if r.start_idx < lines.len() && r.start_idx < r.end_idx && r.end_idx <= lines.len() {
            let block: Vec<String> = r
                .new_text
                .trim_end_matches('\n')
                .split('\n')
                .map(str::to_string)
                .collect();
            lines.splice(r.start_idx..r.end_idx, block);
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

pub fn run_with_options(options: SynWriterRunOptions) -> anyhow::Result<SynWriterRunReport> {
    let write_mode = options.write_mode;
    let augment = options.augment;
    let rewrite_existing = options.rewrite_existing;
    if augment && rewrite_existing {
        anyhow::bail!("--augment and --rewrite-existing are mutually exclusive");
    }

    let workspace_root = options.workspace_root;
    let graph_path = resolve_path(&workspace_root, &options.graph_path);
    let manifest_path = resolve_path(&workspace_root, &options.manifest_path);
    let log_path = resolve_path(&workspace_root, &options.log_path);

    eprintln!("reading {}", graph_path.display());
    let bytes = std::fs::read(&graph_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", graph_path.display()))?;
    let graph: CrateGraph = serde_json::from_slice(&bytes)?;
    let proposal_map = std::fs::read(&manifest_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<ManifestProposalFile>(&b).ok())
        .map(|f| f.proposals)
        .unwrap_or_default();

    let effect_map = build_effect_map(&graph.edges);
    let workspace_src = workspace_root
        .join("src")
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve workspace src: {e}"))?;

    // Collect candidates.
    let candidates: Vec<(&str, &GraphNode, &SourceSpan)> = graph
        .nodes
        .iter()
        .filter(|(_, n)| n.kind == "fn")
        .filter(|(id, n)| node_has_contract_intent(n, proposal_map.get(*id)))
        .filter_map(|(id, n)| n.def.as_ref().map(|d| (id.as_str(), n, d)))
        .filter(|(_, _, d)| {
            d.file.ends_with(".rs")
                && path_under_workspace_src(&d.file, &workspace_root, &workspace_src)
        })
        .filter(|(_, n, _)| {
            if rewrite_existing {
                true
            } else if augment {
                n.provenance.iter().any(|p| p == "rustc:docstring")
                    && n.docstring
                        .as_deref()
                        .map(|d| d.trim().starts_with("Intent:") && !d.contains('\n'))
                        .unwrap_or(false)
            } else {
                !n.provenance.iter().any(|p| p == "rustc:docstring")
            }
        })
        .collect();

    let mode_str = if rewrite_existing {
        "rewrite"
    } else if augment {
        "augment"
    } else {
        "generate"
    };
    eprintln!(
        "  {} candidates  mode={}{}",
        candidates.len(),
        mode_str,
        if write_mode { "" } else { " (dry-run)" }
    );

    // Group by file, highest line first within each file.
    let mut by_file: HashMap<&str, Vec<(&str, &GraphNode, &SourceSpan)>> = HashMap::new();
    for (id, n, d) in &candidates {
        by_file.entry(d.file.as_str()).or_default().push((id, n, d));
    }

    let mut log_actions: Vec<LogAction> = Vec::new();
    let mut n_gen = 0usize;
    let mut n_dry = 0usize;
    let mut n_doc = 0usize;
    let mut n_attr = 0usize;
    let mut n_span = 0usize;
    let mut n_nc = 0usize;
    let mut n_write_err = 0usize;

    for (file, mut nodes_in_file) in by_file {
        nodes_in_file.sort_by(|a, b| b.2.line.cmp(&a.2.line));
        let file_path = resolve_path(&workspace_root, Path::new(file));

        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  cannot read {file}: {e}");
                n_span += nodes_in_file.len();
                continue;
            }
        };
        let lv: Vec<&str> = source.split('\n').collect();
        let mut insertions: Vec<Insertion> = Vec::new();
        let mut replacements: Vec<Replacement> = Vec::new();
        let mut pending_action_indices: Vec<usize> = Vec::new();

        for (node_id, node, span) in &nodes_in_file {
            let intent = node.intent_class.as_deref().unwrap_or("unknown");
            let effects = effect_map.get(*node_id);
            let raw_fn_0 = (span.line as usize).saturating_sub(1);

            if raw_fn_0 >= lv.len() {
                n_span += 1;
                log_actions.push(LogAction {
                    status: "skip:bad_span".into(),
                    fn_path: node.path.clone(),
                    file: file.to_string(),
                    fn_line: span.line,
                    intent_class: intent.to_string(),
                    generated_text: None,
                });
                continue;
            }

            let Some(fn_0) = resolve_fn_decl_line(&lv, raw_fn_0) else {
                n_span += 1;
                log_actions.push(LogAction {
                    status: "skip:no_fn_decl".into(),
                    fn_path: node.path.clone(),
                    file: file.to_string(),
                    fn_line: span.line,
                    intent_class: intent.to_string(),
                    generated_text: None,
                });
                continue;
            };

            let indent = leading_ws(lv[fn_0]).to_string();
            let proposal_manifest = proposal_map.get(*node_id);
            let doc = build_doc(node, proposal_manifest, effects, &indent);

            if rewrite_existing {
                if let Some((start_idx, end_idx)) = find_doc_block_before_fn(&lv, fn_0) {
                    if write_mode {
                        log_actions.push(LogAction {
                            status: "pending_write".into(),
                            fn_path: node.path.clone(),
                            file: file.to_string(),
                            fn_line: span.line,
                            intent_class: intent.to_string(),
                            generated_text: Some(doc.clone()),
                        });
                        pending_action_indices.push(log_actions.len() - 1);
                    } else {
                        n_dry += 1;
                        log_actions.push(LogAction {
                            status: "dry_run".into(),
                            fn_path: node.path.clone(),
                            file: file.to_string(),
                            fn_line: span.line,
                            intent_class: intent.to_string(),
                            generated_text: Some(doc.clone()),
                        });
                    }
                    replacements.push(Replacement {
                        start_idx,
                        end_idx,
                        new_text: doc,
                        fn_line: span.line,
                    });
                } else {
                    match find_insert_point(&lv, fn_0) {
                        Err("complex_attr") => {
                            n_attr += 1;
                            log_actions.push(LogAction {
                                status: "skip:complex_attr".into(),
                                fn_path: node.path.clone(),
                                file: file.to_string(),
                                fn_line: span.line,
                                intent_class: intent.to_string(),
                                generated_text: None,
                            });
                        }
                        Err(o) => {
                            n_span += 1;
                            log_actions.push(LogAction {
                                status: format!("skip:{o}"),
                                fn_path: node.path.clone(),
                                file: file.to_string(),
                                fn_line: span.line,
                                intent_class: intent.to_string(),
                                generated_text: None,
                            });
                        }
                        Ok(at) => {
                            if write_mode {
                                log_actions.push(LogAction {
                                    status: "pending_write".into(),
                                    fn_path: node.path.clone(),
                                    file: file.to_string(),
                                    fn_line: span.line,
                                    intent_class: intent.to_string(),
                                    generated_text: Some(doc.clone()),
                                });
                                pending_action_indices.push(log_actions.len() - 1);
                            } else {
                                n_dry += 1;
                                log_actions.push(LogAction {
                                    status: "dry_run".into(),
                                    fn_path: node.path.clone(),
                                    file: file.to_string(),
                                    fn_line: span.line,
                                    intent_class: intent.to_string(),
                                    generated_text: Some(doc.clone()),
                                });
                            }
                            insertions.push(Insertion {
                                insert_at: at,
                                text: doc,
                                fn_line: span.line,
                            });
                        }
                    }
                }
            } else if augment {
                match find_intent_line(&lv, fn_0) {
                    None => {
                        n_nc += 1;
                        log_actions.push(LogAction {
                            status: "skip:intent_not_found".into(),
                            fn_path: node.path.clone(),
                            file: file.to_string(),
                            fn_line: span.line,
                            intent_class: intent.to_string(),
                            generated_text: None,
                        });
                    }
                    Some(idx) => {
                        // Only augment if the new doc adds lines beyond just Intent:.
                        if doc.trim().lines().count() <= 1 {
                            n_nc += 1;
                            log_actions.push(LogAction {
                                status: "skip:no_new_fields".into(),
                                fn_path: node.path.clone(),
                                file: file.to_string(),
                                fn_line: span.line,
                                intent_class: intent.to_string(),
                                generated_text: None,
                            });
                        } else {
                            if write_mode {
                                log_actions.push(LogAction {
                                    status: "pending_write".into(),
                                    fn_path: node.path.clone(),
                                    file: file.to_string(),
                                    fn_line: span.line,
                                    intent_class: intent.to_string(),
                                    generated_text: Some(doc.clone()),
                                });
                                pending_action_indices.push(log_actions.len() - 1);
                            } else {
                                n_dry += 1;
                                log_actions.push(LogAction {
                                    status: "dry_run".into(),
                                    fn_path: node.path.clone(),
                                    file: file.to_string(),
                                    fn_line: span.line,
                                    intent_class: intent.to_string(),
                                    generated_text: Some(doc.clone()),
                                });
                            }
                            let (start_idx, end_idx) = find_doc_block_range(&lv, idx);
                            replacements.push(Replacement {
                                start_idx,
                                end_idx,
                                new_text: doc,
                                fn_line: span.line,
                            });
                        }
                    }
                }
            } else {
                match find_insert_point(&lv, fn_0) {
                    Err("existing_doc") => {
                        n_doc += 1;
                        log_actions.push(LogAction {
                            status: "skip:existing_doc".into(),
                            fn_path: node.path.clone(),
                            file: file.to_string(),
                            fn_line: span.line,
                            intent_class: intent.to_string(),
                            generated_text: None,
                        });
                    }
                    Err("complex_attr") => {
                        n_attr += 1;
                        log_actions.push(LogAction {
                            status: "skip:complex_attr".into(),
                            fn_path: node.path.clone(),
                            file: file.to_string(),
                            fn_line: span.line,
                            intent_class: intent.to_string(),
                            generated_text: None,
                        });
                    }
                    Err(o) => {
                        n_span += 1;
                        log_actions.push(LogAction {
                            status: format!("skip:{o}"),
                            fn_path: node.path.clone(),
                            file: file.to_string(),
                            fn_line: span.line,
                            intent_class: intent.to_string(),
                            generated_text: None,
                        });
                    }
                    Ok(at) => {
                        if write_mode {
                            log_actions.push(LogAction {
                                status: "pending_write".into(),
                                fn_path: node.path.clone(),
                                file: file.to_string(),
                                fn_line: span.line,
                                intent_class: intent.to_string(),
                                generated_text: Some(doc.clone()),
                            });
                            pending_action_indices.push(log_actions.len() - 1);
                        } else {
                            n_dry += 1;
                            log_actions.push(LogAction {
                                status: "dry_run".into(),
                                fn_path: node.path.clone(),
                                file: file.to_string(),
                                fn_line: span.line,
                                intent_class: intent.to_string(),
                                generated_text: Some(doc.clone()),
                            });
                        }
                        insertions.push(Insertion {
                            insert_at: at,
                            text: doc,
                            fn_line: span.line,
                        });
                    }
                }
            }
        }

        let short = Path::new(file)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();

        if write_mode && (!insertions.is_empty() || !replacements.is_empty()) {
            let mut new_source = source.clone();
            if !replacements.is_empty() {
                replacements.sort_by(|a, b| b.start_idx.cmp(&a.start_idx));
                new_source = apply_replacements(&new_source, &replacements);
            }
            if !insertions.is_empty() {
                insertions.sort_by(|a, b| b.insert_at.cmp(&a.insert_at));
                new_source = apply_insertions(&new_source, &insertions);
            }
            let total = replacements.len() + insertions.len();
            match std::fs::write(&file_path, &new_source) {
                Ok(()) => {
                    let success_status = if rewrite_existing {
                        "rewritten"
                    } else if augment {
                        "augmented"
                    } else {
                        "generated"
                    };
                    mark_actions_after_write(
                        &mut log_actions,
                        &pending_action_indices,
                        true,
                        success_status,
                        &mut n_gen,
                        &mut n_write_err,
                    );
                    eprintln!("  generated {} docstrings → {short}", total);
                }
                Err(e) => {
                    mark_actions_after_write(
                        &mut log_actions,
                        &pending_action_indices,
                        false,
                        "",
                        &mut n_gen,
                        &mut n_write_err,
                    );
                    eprintln!("  error writing {short}: {e}");
                }
            }
        } else {
            let total = replacements.len() + insertions.len();
            if total > 0 {
                eprintln!("  dry-run: {} pending in {short}", total);
                for i in &insertions {
                    eprintln!(
                        "    line {:4}  {}",
                        i.fn_line,
                        i.text.trim_end().replace('\n', " | ")
                    );
                }
                for r in &replacements {
                    let tag = if rewrite_existing {
                        "rewrite"
                    } else if augment {
                        "augment"
                    } else {
                        "replace"
                    };
                    eprintln!(
                        "    line {:4}  {} [{}]",
                        r.fn_line,
                        r.new_text.trim_end().replace('\n', " | "),
                        tag
                    );
                }
            }
        }
    }

    let log = WriterLog {
        generated_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        dry_run: !write_mode,
        mode: mode_str.to_string(),
        summary: LogSummary {
            candidates: candidates.len(),
            generated: n_gen,
            dry_run_pending: n_dry,
            skip_existing_doc: n_doc,
            skip_complex_attr: n_attr,
            skip_bad_span: n_span,
            skip_no_change: n_nc,
            skip_write_error: n_write_err,
        },
        actions: log_actions,
    };
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(&log) {
        let _ = std::fs::write(&log_path, json);
    }

    eprintln!("\n{}  mode={}  generated={}  dry_run={}  skip_doc={}  skip_attr={}  skip_nochange={}  skip_write_error={}",
        if write_mode { "WRITE" } else { "DRY-RUN" },
        mode_str, n_gen, n_dry, n_doc, n_attr, n_nc, n_write_err);
    if !write_mode {
        eprintln!("re-run with --write to apply");
    }
    Ok(SynWriterRunReport {
        candidates: candidates.len(),
        generated: n_gen,
        dry_run_pending: n_dry,
        skip_existing_doc: n_doc,
        skip_complex_attr: n_attr,
        skip_bad_span: n_span,
        skip_no_change: n_nc,
        skip_write_error: n_write_err,
    })
}

pub fn run_from_cli_args(
    args: &[String],
    workspace_root: PathBuf,
) -> anyhow::Result<SynWriterRunReport> {
    let write_mode = args.iter().any(|a| a == "--write");
    let augment = args.iter().any(|a| a == "--augment");
    let rewrite_existing = args.iter().any(|a| a == "--rewrite-existing");
    let graph_path = args
        .iter()
        .find(|a| a.ends_with(".json") && !a.ends_with("log.json"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("state/rustc/canon_mini_agent/graph.json"));
    let manifest_path = args
        .windows(2)
        .find(|w| w[0] == "--manifest")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from("agent_state/semantic_manifest_proposals.json"));
    run_with_options(SynWriterRunOptions {
        workspace_root,
        graph_path,
        manifest_path,
        log_path: PathBuf::from("agent_state/syn_writer_log.json"),
        write_mode,
        augment,
        rewrite_existing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_doc_block_range_spans_contiguous_doc_lines() {
        let lines = vec![
            "/// Intent: pure_transform",
            "/// Effects: reads_state",
            "/// Provenance: generated",
            "fn run() {}",
        ];
        let (start, end) = find_doc_block_range(&lines, 0);
        assert_eq!((start, end), (0, 3));
    }

    #[test]
    fn apply_replacements_replaces_entire_doc_block() {
        let source = "/// Intent: old\n/// Effects: stale\nfn run() {}\n";
        let reps = vec![Replacement {
            start_idx: 0,
            end_idx: 2,
            new_text: "/// Intent: new\n/// Provenance: generated\n".to_string(),
            fn_line: 3,
        }];
        let out = apply_replacements(source, &reps);
        assert_eq!(
            out,
            "/// Intent: new\n/// Provenance: generated\nfn run() {}\n"
        );
    }

    #[test]
    fn find_insert_point_skips_attributes() {
        let lines = vec!["#[inline]", "fn run() {}"];
        let idx = find_insert_point(&lines, 1).unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn mark_actions_after_write_updates_status_and_counts() {
        let mut actions = vec![LogAction {
            status: "pending_write".to_string(),
            fn_path: "x::f".to_string(),
            file: "src/x.rs".to_string(),
            fn_line: 10,
            intent_class: "pure_transform".to_string(),
            generated_text: Some("/// Intent: pure_transform\n".to_string()),
        }];
        let mut n_gen = 0usize;
        let mut n_write_err = 0usize;

        mark_actions_after_write(
            &mut actions,
            &[0],
            false,
            "generated",
            &mut n_gen,
            &mut n_write_err,
        );
        assert_eq!(actions[0].status, "error:write_failed");
        assert!(actions[0].generated_text.is_none());
        assert_eq!(n_gen, 0);
        assert_eq!(n_write_err, 1);
    }

    #[test]
    fn path_filter_accepts_relative_src_path() {
        let root = PathBuf::from("/workspace/ai_sandbox/canon-mini-agent");
        let src = root.join("src");
        assert!(path_under_workspace_src("src/lib.rs", &root, &src));
    }

    #[test]
    fn find_doc_block_before_fn_across_attributes() {
        let lines = vec![
            "/// Intent: old",
            "/// Effects: stale",
            "#[inline]",
            "fn run() {}",
        ];
        assert_eq!(find_doc_block_before_fn(&lines, 3), Some((0, 2)));
    }

    #[test]
    fn build_doc_matches_plan_schema_shape() {
        let node = GraphNode {
            path: "plans::load_plan".to_string(),
            kind: "fn".to_string(),
            signature: Some("fn(path: &Path) -> Result<Plan>".to_string()),
            intent_class: Some("canonical_read".to_string()),
            resource: Some("PLAN.json".to_string()),
            provenance: vec!["rustc:docstring".to_string()],
            docstring: None,
            semantic_manifest: Some(SemanticManifest {
                intent_class: "canonical_read".to_string(),
                resource: "PLAN.json".to_string(),
                inputs: vec!["path: &Path".to_string()],
                outputs: vec!["Result<Plan>".to_string()],
                effects: vec!["fs_read".to_string()],
                forbidden_effects: vec!["fs_write".to_string(), "default_overwrite".to_string()],
                invariants: vec![
                    "plan_is_authoritative".to_string(),
                    "no_direct_plan_patch".to_string(),
                ],
                failure_mode: "fail_closed".to_string(),
                provenance: vec![
                    "rustc:facts".to_string(),
                    "syn:docstring".to_string(),
                    "tests:verified".to_string(),
                ],
            }),
            def: None,
        };
        let doc = build_doc(&node, None, None, "");
        let expected = "\
/// Intent: canonical_read
/// Resource: PLAN.json
/// Inputs: path: &Path
/// Outputs: Result<Plan>
/// Effects: fs_read
/// Forbidden: fs_write, default_overwrite
/// Invariants: plan_is_authoritative, no_direct_plan_patch
/// Failure: fail_closed
/// Provenance: rustc:facts + syn:docstring + tests:verified
";
        assert_eq!(doc, expected);
    }
}

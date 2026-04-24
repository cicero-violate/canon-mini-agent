//! Inter-function complexity analysis.
//!
//! Extends per-function metrics with three cross-boundary signals:
//!
//! 1. **B_transitive** — depth-1 call-graph branching propagation.
//!    B_transitive(F) = B(F) + mean(B(callee) for direct callees of F).
//!    Captures functions that look simple locally but fan out into complex callees.
//!
//! 2. **R_body** — exact MIR duplicate detection via the `fingerprint` field in
//!    graph.json.  Two functions with identical fingerprints are functionally
//!    equivalent at the MIR level; one is redundant (R_body = 1.0).
//!    This is ground-truth redundancy, not a proxy.
//!
//! 3. **D_det** — determinism proxy: D_det(F) = 1.0 − B_norm(F).
//!    Reducing branching directly increases determinism per the objective.
//!
//! Composite inter-function objective score (all weights sum to 1.0):
//!   inter_objective = 0.40·B_transitive_norm + 0.30·R_body + 0.30·(1 − D_det)
//!
//! The **task generator** (`generate_hotspot_issues`) reads the analysis and
//! auto-creates entries in ISSUES.json for the top-N hotspots not already tracked,
//! closing the Detect → Propose step of the execution loop.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Result;

use crate::issues::{
    is_closed, load_issues_file, persist_issues_projection_with_writer, rescore_all, Issue,
    IssuesFile,
};
use crate::semantic::SemanticIndex;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-symbol inter-function complexity result.
#[derive(Debug, Clone)]
pub struct InterEntry {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    /// Local MIR block count (B direct).
    pub b_direct: usize,
    /// B(F) + mean(B(callee)) over direct callees — depth-1 transitive branching.
    pub b_transitive: f64,
    /// Number of distinct direct callees (out-degree in call graph).
    pub call_out: usize,
    /// Number of direct callers (in-degree in call graph).
    pub call_in: usize,
    /// MIR fingerprint from canon-rustc-v2.
    pub mir_fingerprint: Option<String>,
    /// Other symbols that share the same MIR fingerprint (exact duplicates).
    pub duplicate_of: Vec<String>,
    /// R_body: 1.0 if at least one MIR-identical sibling exists, else 0.0.
    pub r_body: f64,
    /// D_det proxy: 1.0 − branch_score_norm (higher = more deterministic).
    pub d_det: f64,
    /// Terminator-weighted branch score: SwitchInt×2 + Call×1 + Assert×0.5 over non-cleanup blocks.
    /// More accurate than raw mir_blocks as a cyclomatic complexity proxy.
    pub branch_score: f64,
    /// Heat score: branch_score × ln(call_in + 1). Surfaces complex AND frequently-called functions.
    pub heat_score: f64,
    /// True if the function directly calls itself.
    pub is_directly_recursive: bool,
    /// Composite inter-function objective score ∈ [0, 1].
    pub inter_objective: f64,
}

/// Full analysis result for one crate.
pub struct InterAnalysis {
    /// All entries sorted by inter_objective descending.
    pub entries: Vec<InterEntry>,
    /// Groups of symbols with identical MIR fingerprints (len ≥ 2).
    pub duplicate_groups: Vec<Vec<String>>,
    /// Total call edges analyzed.
    pub call_edge_count: usize,
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Run inter-function analysis for `crate_name`.
pub fn analyze(workspace: &Path, crate_name: &str) -> Result<InterAnalysis> {
    let idx = SemanticIndex::load(workspace, crate_name)?;
    let summaries = idx.symbol_summaries();
    let call_edges = idx.call_edges();
    let call_edge_count = call_edges.len();

    // callee map: caller → [callee, ...]
    let mut callee_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut caller_map: HashMap<String, usize> = HashMap::new();
    for (from, to) in &call_edges {
        callee_map.entry(from.clone()).or_default().push(to.clone());
        *caller_map.entry(to.clone()).or_insert(0) += 1;
    }

    // Complexity map: symbol → terminator-weighted branch score.
    // Falls back to mir_blocks when cfg_nodes are absent (e.g. stub crates).
    let complexity_map: HashMap<String, f64> = summaries
        .iter()
        .filter_map(|s| {
            let c = s.branch_score.or_else(|| s.mir_blocks.map(|b| b as f64))?;
            Some((s.symbol.clone(), c))
        })
        .collect();
    let max_c = complexity_map
        .values()
        .copied()
        .fold(0.0_f64, f64::max)
        .max(1.0);

    // Build callee sequences from bridge_edges: symbol_path → ordered list of callee paths.
    // Two functions are semantic duplicates only if they call the same callees in the same
    // block order — not just share the same MIR structural shape.
    let mut raw_callee_pairs: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (from, relation, to) in idx.bridge_edges() {
        if relation != "Call" {
            continue;
        }
        // from pattern: "cfg::{symbol_path}::bb{N}"
        let Some(bb_pos) = from.rfind("::bb") else {
            continue;
        };
        let Ok(block_idx) = from[bb_pos + 4..].parse::<usize>() else {
            continue;
        };
        let sym_path = &from[5..bb_pos]; // strip "cfg::" prefix (5 chars)
        raw_callee_pairs
            .entry(sym_path.to_string())
            .or_default()
            .push((block_idx, to));
    }
    let callee_seq: HashMap<String, String> = raw_callee_pairs
        .into_iter()
        .map(|(sym, mut pairs)| {
            pairs.sort_by_key(|(idx, _)| *idx);
            let seq = pairs
                .into_iter()
                .map(|(_, c)| c)
                .collect::<Vec<_>>()
                .join(",");
            (sym, seq)
        })
        .collect();

    // Semantic duplicate groups: (mir_fingerprint, signature, callee_sequence) → [symbol, ...]
    // This rejects false positives where structurally identical MIR bodies call different callees.
    let mut sem_groups: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for s in &summaries {
        let Some(fp) = &s.mir_fingerprint else {
            continue;
        };
        let sig = s.signature.as_deref().unwrap_or("").to_string();
        let callees = callee_seq.get(&s.symbol).cloned().unwrap_or_default();
        sem_groups
            .entry((fp.clone(), sig, callees))
            .or_default()
            .push(s.symbol.clone());
    }

    // sym_to_siblings: precomputed sibling list for O(1) lookup in the entries loop
    let mut sym_to_siblings: HashMap<String, Vec<String>> = HashMap::new();
    for group in sem_groups.values().filter(|g| g.len() >= 2) {
        for sym in group {
            let siblings: Vec<String> = group.iter().filter(|s| *s != sym).cloned().collect();
            sym_to_siblings.insert(sym.clone(), siblings);
        }
    }

    // Build per-symbol entries
    let mut entries: Vec<InterEntry> = summaries
        .iter()
        .filter_map(|s| {
            let c = *complexity_map.get(&s.symbol)?;
            if c == 0.0 && s.mir_blocks.unwrap_or(0) == 0 {
                return None;
            }

            let call_in = *caller_map.get(&s.symbol).unwrap_or(&0);

            // B_transitive: local branch_score + mean of direct callee branch_scores
            let callee_complexities: Vec<f64> = callee_map
                .get(&s.symbol)
                .map(|cs| {
                    cs.iter()
                        .filter_map(|callee| complexity_map.get(callee))
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            let callee_mean = if callee_complexities.is_empty() {
                0.0
            } else {
                callee_complexities.iter().sum::<f64>() / callee_complexities.len() as f64
            };
            let b_transitive = c + callee_mean;

            // Heat: complexity weighted by call frequency (ln smoothing prevents domination)
            let heat_score = c * ((call_in as f64 + 1.0).ln());

            // Semantic duplicate detection using composite key
            let fp = s.mir_fingerprint.clone();
            let siblings = sym_to_siblings.get(&s.symbol).cloned().unwrap_or_default();
            let r_body = if siblings.is_empty() { 0.0 } else { 1.0 };

            let c_norm = c / max_c;
            let d_det = 1.0 - c_norm;

            Some(InterEntry {
                symbol: s.symbol.clone(),
                file: s.file.clone(),
                line: s.line,
                b_direct: s.mir_blocks.unwrap_or(0),
                b_transitive,
                call_out: s.call_out,
                call_in,
                mir_fingerprint: fp,
                duplicate_of: siblings,
                r_body,
                d_det,
                branch_score: c,
                heat_score,
                is_directly_recursive: s.is_directly_recursive,
                inter_objective: 0.0, // filled after normalization
            })
        })
        .collect();

    // Normalize B_transitive and heat across all entries, then compute inter_objective.
    // Weights (sum = 1.0): B_transitive 0.30 · R_body 0.20 · D_det 0.20 · heat 0.30
    let max_bt = entries
        .iter()
        .map(|e| e.b_transitive)
        .fold(0.0_f64, f64::max);
    let max_heat = entries.iter().map(|e| e.heat_score).fold(0.0_f64, f64::max);

    for entry in &mut entries {
        let bt_norm = if max_bt > 0.0 {
            entry.b_transitive / max_bt
        } else {
            0.0
        };
        let heat_norm = if max_heat > 0.0 {
            entry.heat_score / max_heat
        } else {
            0.0
        };
        entry.inter_objective =
            (0.30 * bt_norm + 0.20 * entry.r_body + 0.20 * (1.0 - entry.d_det) + 0.30 * heat_norm)
                .clamp(0.0, 1.0);
    }

    entries.sort_by(|a, b| {
        b.inter_objective
            .partial_cmp(&a.inter_objective)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let duplicate_groups: Vec<Vec<String>> =
        sem_groups.into_values().filter(|g| g.len() >= 2).collect();

    Ok(InterAnalysis {
        entries,
        duplicate_groups,
        call_edge_count,
    })
}

// ---------------------------------------------------------------------------
// Task generator
// ---------------------------------------------------------------------------

/// Auto-create ISSUES.json entries for the top-`top_n` inter-function hotspots
/// not already tracked as open issues.  Returns the number of new issues created.
///
/// This closes the Detect → Propose gap: the analysis is the Detect step;
/// creating the issue is the Propose step that routes work to the LLM/planner.
pub fn generate_hotspot_issues(workspace: &Path, top_n: usize) -> Result<usize> {
    let (mut file, existing_ids, open_locations) = load_issue_file_with_indexes(workspace);

    let mut created = 0;

    for (crate_name, analysis) in collect_crate_analyses(workspace) {
        created += append_hotspot_issues(
            &mut file,
            &analysis,
            &crate_name,
            &existing_ids,
            &open_locations,
            top_n,
        );
        created += append_duplicate_issues(&mut file, &analysis, &existing_ids);
    }

    persist_if_created(workspace, &mut file, created)?;

    Ok(created)
}

/// Intent: canonical_read
/// Provenance: generated
fn load_issue_file_with_indexes(
    workspace: &Path,
) -> (IssuesFile, HashSet<String>, HashSet<String>) {
    let file: IssuesFile = load_issues_file(workspace);
    let existing_ids = file.issues.iter().map(|i| i.id.clone()).collect();
    let open_locations = file
        .issues
        .iter()
        .filter(|i| !is_closed(i))
        .map(|i| i.location.clone())
        .collect();
    (file, existing_ids, open_locations)
}

fn collect_crate_analyses(workspace: &Path) -> Vec<(String, InterAnalysis)> {
    SemanticIndex::available_crates(workspace)
        .into_iter()
        .filter_map(|crate_name| {
            analyze(workspace, &crate_name)
                .ok()
                .map(|analysis| (crate_name, analysis))
        })
        .collect()
}

/// Intent: diagnostic_scan
/// Provenance: generated
fn append_hotspot_issues(
    file: &mut IssuesFile,
    analysis: &InterAnalysis,
    crate_name: &str,
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
    top_n: usize,
) -> usize {
    let mut created = 0;
    for entry in analysis.entries.iter().take(top_n) {
        if let Some(issue) = build_hotspot_issue(entry, crate_name, existing_ids, open_locations) {
            file.issues.push(issue);
            created += 1;
        }
    }
    // Recursive function issues — separate from hotspot threshold
    for entry in &analysis.entries {
        if !entry.is_directly_recursive {
            continue;
        }
        if let Some(issue) = build_recursive_issue(entry, crate_name, existing_ids) {
            file.issues.push(issue);
            created += 1;
        }
    }
    created
}

/// Intent: pure_transform
/// Provenance: generated
fn build_hotspot_issue(
    entry: &InterEntry,
    crate_name: &str,
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
) -> Option<Issue> {
    if entry.inter_objective < 0.20 {
        return None;
    }

    let location = hotspot_location(entry);
    if open_locations.iter().any(|l| l.contains(&location)) {
        return None;
    }

    let id = inter_issue_id(crate_name, &entry.symbol);
    if existing_ids.contains(&id) {
        return None;
    }

    let (title, kind, description, evidence) = build_issue_fields(entry, crate_name);
    let priority = priority_from_score(entry.inter_objective);
    Some(Issue {
        id,
        title,
        status: "open".to_string(),
        priority: priority.to_string(),
        kind,
        description,
        location,
        evidence,
        discovered_by: "inter_complexity_analyzer".to_string(),
        score: 0.0,
        ..Issue::default()
    })
}

fn hotspot_location(entry: &InterEntry) -> String {
    let loc = shorten_file(&entry.file);
    format!("{loc}:{}", entry.line)
}

/// Intent: diagnostic_scan
/// Provenance: generated
fn append_duplicate_issues(
    file: &mut IssuesFile,
    analysis: &InterAnalysis,
    existing_ids: &HashSet<String>,
) -> usize {
    let mut created = 0;
    for group in &analysis.duplicate_groups {
        let filtered = actionable_duplicate_symbols(group);
        if let Some(issue) = build_duplicate_issue(&filtered, existing_ids) {
            file.issues.push(issue);
            created += 1;
        }
    }
    created
}

/// Intent: pure_transform
/// Provenance: generated
fn build_recursive_issue(
    entry: &InterEntry,
    crate_name: &str,
    existing_ids: &HashSet<String>,
) -> Option<Issue> {
    let id = format!("auto_recursive_{crate_name}_{}", stable_hash(&entry.symbol));
    if existing_ids.contains(&id) {
        return None;
    }
    let short = entry.symbol.rsplit("::").next().unwrap_or(&entry.symbol);
    Some(Issue {
        id,
        title: format!("Direct recursion detected: {short}"),
        status: "open".to_string(),
        priority: if entry.branch_score >= 10.0 {
            "high".to_string()
        } else {
            "medium".to_string()
        },
        kind: "performance".to_string(),
        description: format!(
            "`{}` calls itself directly (branch_score={:.1}, call_in={}).\n\
             Direct recursion prevents inlining, risks stack overflow under deep inputs, \
             and complicates static analysis. Consider converting to iteration or \
             introducing a trampoline/accumulator pattern.",
            entry.symbol, entry.branch_score, entry.call_in
        ),
        location: hotspot_location(entry),
        evidence: vec![
            format!("is_directly_recursive=true"),
            format!("branch_score={:.1}", entry.branch_score),
            format!("call_in={} call_out={}", entry.call_in, entry.call_out),
        ],
        discovered_by: "inter_complexity_analyzer".to_string(),
        score: 0.0,
        ..Issue::default()
    })
}

/// Intent: pure_transform
/// Provenance: generated
fn build_duplicate_issue(group: &[String], existing_ids: &HashSet<String>) -> Option<Issue> {
    if group.len() < 2 {
        return None;
    }
    let id = mir_dup_issue_id(group);
    if existing_ids.contains(&id) {
        return None;
    }
    Some(Issue {
        id,
        title: format!(
            "MIR-identical functions: {} candidates for deduplication",
            group.len()
        ),
        status: "open".to_string(),
        priority: "medium".to_string(),
        kind: "redundancy".to_string(),
        description: format!(
            "These {} functions share an identical MIR fingerprint and are \
             functionally equivalent. All but one can be replaced with a shared \
             helper, eliminating R directly.\n\nSymbols: {}",
            group.len(),
            summarize_duplicate_symbols(group)
        ),
        location: String::new(),
        evidence: vec![format!(
            "MIR fingerprint shared by: {}",
            summarize_duplicate_symbols(group)
        )],
        discovered_by: "inter_complexity_analyzer".to_string(),
        score: 0.0,
        ..Issue::default()
    })
}

/// Intent: canonical_write
/// Provenance: generated
fn persist_if_created(workspace: &Path, file: &mut IssuesFile, created: usize) -> Result<()> {
    if created > 0 {
        rescore_all(file);
        persist_issues_projection_with_writer(workspace, file, None, "generate_hotspot_issues")?;
    }
    Ok(())
}

/// Serialize the inter-analysis into a JSON value for embedding in the complexity report.
pub fn to_report_value(analysis: &InterAnalysis, top_n: usize) -> serde_json::Value {
    let top: Vec<serde_json::Value> = analysis
        .entries
        .iter()
        .take(top_n)
        .map(|e| {
            serde_json::json!({
                "symbol": e.symbol,
                "file": shorten_file(&e.file),
                "line": e.line,
                "inter_objective": format!("{:.3}", e.inter_objective),
                "branch_score": format!("{:.1}", e.branch_score),
                "heat_score": format!("{:.1}", e.heat_score),
                "is_directly_recursive": e.is_directly_recursive,
                "b_direct": e.b_direct,
                "b_transitive": format!("{:.1}", e.b_transitive),
                "r_body": e.r_body,
                "d_det": format!("{:.3}", e.d_det),
                "call_out": e.call_out,
                "call_in": e.call_in,
                "duplicate_of": e.duplicate_of,
            })
        })
        .collect();

    let dup_groups: Vec<serde_json::Value> = analysis
        .duplicate_groups
        .iter()
        .map(|g| serde_json::json!(g))
        .collect();

    serde_json::json!({
        "scoring": {
            "inter_objective": "0.30·B_transitive_norm + 0.20·R_body + 0.20·(1−D_det) + 0.30·heat_norm",
            "branch_score": "SwitchInt×2.0 + Call×1.0 + Assert×0.5 over non-cleanup MIR blocks",
            "B_transitive": "branch_score(F) + mean(branch_score(callee)) — depth-1 propagation",
            "R_body": "1.0 if MIR fingerprint+signature+callees match another function, else 0.0",
            "D_det": "1.0 − branch_score_norm (higher = more deterministic)",
            "heat_score": "branch_score × ln(call_in + 1) — complexity weighted by call frequency",
        },
        "call_edges_analyzed": analysis.call_edge_count,
        "top": top,
        "mir_duplicate_groups": dup_groups,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn inter_issue_id(crate_name: &str, symbol: &str) -> String {
    let h = stable_hash(symbol);
    format!("auto_inter_complexity_{crate_name}_{h:x}")
}

fn mir_dup_issue_id(group: &[String]) -> String {
    let mut sorted = group.to_vec();
    sorted.sort();
    let h = stable_hash(&sorted.join(","));
    format!("auto_mir_dup_{h:x}")
}

const MAX_DUPLICATE_SYMBOLS_IN_ISSUE: usize = 24;

/// Intent: pure_transform
/// Provenance: generated
fn summarize_duplicate_symbols(group: &[String]) -> String {
    if group.len() <= MAX_DUPLICATE_SYMBOLS_IN_ISSUE {
        return group.join(", ");
    }
    let shown = group[..MAX_DUPLICATE_SYMBOLS_IN_ISSUE].join(", ");
    let remaining = group.len() - MAX_DUPLICATE_SYMBOLS_IN_ISSUE;
    format!("{shown}, ... (+{remaining} more)")
}

fn actionable_duplicate_symbols(group: &[String]) -> Vec<String> {
    group
        .iter()
        .filter(|sym| !looks_like_const_symbol(sym))
        .cloned()
        .collect()
}

fn looks_like_const_symbol(symbol: &str) -> bool {
    let leaf = symbol.rsplit("::").next().unwrap_or(symbol);
    let has_lower = leaf.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = leaf.chars().any(|c| c.is_ascii_uppercase());
    has_upper && !has_lower
}

fn stable_hash(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn shorten_file(path: &str) -> String {
    // Keep only the src/... part
    if let Some(idx) = path.find("/src/") {
        return path[idx + 1..].to_string();
    }
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn priority_from_score(score: f64) -> &'static str {
    if score >= 0.70 {
        "high"
    } else if score >= 0.40 {
        "medium"
    } else {
        "low"
    }
}

/// Intent: pure_transform
/// Provenance: generated
fn build_issue_fields(
    entry: &InterEntry,
    crate_name: &str,
) -> (String, String, String, Vec<String>) {
    let short_sym = issue_short_symbol(entry);
    let file = shorten_file(&entry.file);
    let title = issue_title(entry, short_sym);
    let kind = issue_kind(entry);
    let dup_note = duplicate_note(entry);

    let recursive_note = if entry.is_directly_recursive {
        "\n- is_directly_recursive=true  (consider converting to iteration)"
    } else {
        ""
    };
    let description = format!(
        "Inter-function objective score {score:.3} in crate `{crate_name}` \
         (threshold for action: ≥0.20).\n\n\
         Execution model: Detect(this issue) → Propose(LLM refactor) → Apply(patch) → Verify(build+test)\n\n\
         Metrics:\n\
         - branch_score={bs:.1}  (SwitchInt×2+Call×1+Assert×0.5 over non-cleanup blocks)\n\
         - heat_score={heat:.1}  (branch_score × ln(call_in+1))\n\
         - B_transitive={b_t:.1}  (local + mean callee branch_score)\n\
         - R_body={r:.1}  (1.0 = MIR duplicate exists)\n\
         - D_det={d:.3}  (1.0 = fully deterministic)\n\
         - call_out={co}  call_in={ci}\
         {recursive_note}\
         {dup_note}",
        score = entry.inter_objective,
        bs = entry.branch_score,
        heat = entry.heat_score,
        b_t = entry.b_transitive,
        r = entry.r_body,
        d = entry.d_det,
        co = entry.call_out,
        ci = entry.call_in,
        recursive_note = recursive_note,
        dup_note = dup_note,
    );

    let evidence = build_issue_evidence(entry, &file);

    (title, kind, description, evidence)
}

fn issue_short_symbol<'a>(entry: &'a InterEntry) -> &'a str {
    entry.symbol.rsplit("::").next().unwrap_or(&entry.symbol)
}

fn has_duplicates(entry: &InterEntry) -> bool {
    entry.r_body > 0.0
}

fn issue_title(entry: &InterEntry, short_sym: &str) -> String {
    if has_duplicates(entry) {
        format!(
            "Reduce inter-function complexity + eliminate duplicate: {}",
            short_sym
        )
    } else {
        format!("Reduce inter-function complexity: {}", short_sym)
    }
}

fn issue_kind(entry: &InterEntry) -> String {
    if has_duplicates(entry) {
        "redundancy".to_string()
    } else {
        "performance".to_string()
    }
}

fn duplicate_note(entry: &InterEntry) -> String {
    if has_duplicates(entry) {
        format!(
            "\n\nThis function has MIR-identical siblings: {}. \
             Consolidate into a shared helper to eliminate R directly.",
            entry.duplicate_of.join(", ")
        )
    } else {
        String::new()
    }
}

/// Intent: pure_transform
/// Provenance: generated
fn build_issue_evidence(entry: &InterEntry, file: &str) -> Vec<String> {
    let mut evidence = vec![
        format!("inter_objective={:.3}", entry.inter_objective),
        format!(
            "branch_score={:.1} heat_score={:.1}",
            entry.branch_score, entry.heat_score
        ),
        format!("b_transitive={:.1}", entry.b_transitive),
        format!("r_body={:.1} d_det={:.3}", entry.r_body, entry.d_det),
        format!("call_out={} call_in={}", entry.call_out, entry.call_in),
        format!("location: {file}:{}", entry.line),
    ];
    if entry.is_directly_recursive {
        evidence.push("is_directly_recursive=true".to_string());
    }
    if !entry.duplicate_of.is_empty() {
        evidence.push(format!("MIR duplicates: {}", entry.duplicate_of.join(", ")));
    }
    evidence
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        actionable_duplicate_symbols, inter_issue_id, looks_like_const_symbol, mir_dup_issue_id,
        priority_from_score, shorten_file, summarize_duplicate_symbols,
    };

    #[test]
    fn priority_bands_are_correct() {
        assert_eq!(priority_from_score(0.70), "high");
        assert_eq!(priority_from_score(0.71), "high");
        assert_eq!(priority_from_score(0.40), "medium");
        assert_eq!(priority_from_score(0.69), "medium");
        assert_eq!(priority_from_score(0.19), "low");
    }

    #[test]
    fn stable_ids_are_deterministic() {
        let id1 = inter_issue_id("canon_mini_agent", "tools::handle_batch_action");
        let id2 = inter_issue_id("canon_mini_agent", "tools::handle_batch_action");
        assert_eq!(id1, id2, "same symbol must produce same issue id");

        let other = inter_issue_id("canon_mini_agent", "tools::other_fn");
        assert_ne!(id1, other, "different symbols must produce different ids");
    }

    #[test]
    fn mir_dup_id_is_order_independent() {
        let a = vec!["foo::bar".to_string(), "baz::qux".to_string()];
        let b = vec!["baz::qux".to_string(), "foo::bar".to_string()];
        assert_eq!(
            mir_dup_issue_id(&a),
            mir_dup_issue_id(&b),
            "group id must be independent of input order"
        );
    }

    #[test]
    fn shorten_file_extracts_src_path() {
        let full = "/workspace/ai_sandbox/canon-mini-agent/src/tools.rs";
        assert_eq!(shorten_file(full), "src/tools.rs");
    }

    #[test]
    fn const_like_symbols_are_not_actionable_for_dup_issues() {
        assert!(looks_like_const_symbol("constants::MASTER_PLAN_FILE"));
        assert!(looks_like_const_symbol(
            "llm_runtime::chromium_backend::FRAMES_DIR"
        ));
        assert!(!looks_like_const_symbol("app::run_planner_phase"));
        assert!(!looks_like_const_symbol("tools::handle_batch_action"));
    }

    #[test]
    fn duplicate_issue_symbols_drop_const_like_entries() {
        let group = vec![
            "constants::MASTER_PLAN_FILE".to_string(),
            "app::run_planner_phase".to_string(),
            "tools::handle_batch_action".to_string(),
        ];
        let filtered = actionable_duplicate_symbols(&group);
        assert_eq!(
            filtered,
            vec![
                "app::run_planner_phase".to_string(),
                "tools::handle_batch_action".to_string()
            ]
        );
    }

    #[test]
    fn duplicate_symbol_summary_is_capped() {
        let group: Vec<String> = (0..30).map(|i| format!("mod::f{i}")).collect();
        let summary = summarize_duplicate_symbols(&group);
        assert!(summary.contains("... (+6 more)"));
        assert!(!summary.contains("mod::f29"));
    }
}

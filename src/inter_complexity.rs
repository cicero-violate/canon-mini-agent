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

use crate::constants::ISSUES_FILE;
use crate::issues::{is_closed, persist_issues_projection, rescore_all, Issue, IssuesFile};
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
    /// D_det proxy: 1.0 − B_norm (higher = more deterministic).
    pub d_det: f64,
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

    // blocks map: symbol → mir_blocks
    let blocks_map: HashMap<String, usize> = summaries
        .iter()
        .filter_map(|s| s.mir_blocks.map(|b| (s.symbol.clone(), b)))
        .collect();

    let max_b = blocks_map.values().copied().max().unwrap_or(1) as f64;

    // Fingerprint groups: fingerprint → [symbol, ...]
    let mut fp_groups: HashMap<String, Vec<String>> = HashMap::new();
    for s in &summaries {
        if let Some(fp) = &s.mir_fingerprint {
            fp_groups
                .entry(fp.clone())
                .or_default()
                .push(s.symbol.clone());
        }
    }

    // Build per-symbol entries
    let mut entries: Vec<InterEntry> = summaries
        .iter()
        .filter_map(|s| {
            let b = s.mir_blocks?;
            if b == 0 {
                return None;
            }

            // B_transitive: local + mean of direct callee blocks
            let callee_blocks: Vec<f64> = callee_map
                .get(&s.symbol)
                .map(|cs| {
                    cs.iter()
                        .filter_map(|c| blocks_map.get(c))
                        .map(|&b| b as f64)
                        .collect()
                })
                .unwrap_or_default();
            let callee_mean = if callee_blocks.is_empty() {
                0.0
            } else {
                callee_blocks.iter().sum::<f64>() / callee_blocks.len() as f64
            };
            let b_transitive = b as f64 + callee_mean;

            // Duplicate detection
            let fp = s.mir_fingerprint.clone();
            let siblings: Vec<String> = fp
                .as_ref()
                .and_then(|f| fp_groups.get(f))
                .map(|g| g.iter().filter(|sym| **sym != s.symbol).cloned().collect())
                .unwrap_or_default();
            let r_body = if siblings.is_empty() { 0.0 } else { 1.0 };

            let b_norm = b as f64 / max_b;
            let d_det = 1.0 - b_norm;

            Some(InterEntry {
                symbol: s.symbol.clone(),
                file: s.file.clone(),
                line: s.line,
                b_direct: b,
                b_transitive,
                call_out: s.call_out,
                call_in: *caller_map.get(&s.symbol).unwrap_or(&0),
                mir_fingerprint: fp,
                duplicate_of: siblings,
                r_body,
                d_det,
                inter_objective: 0.0, // filled after normalization
            })
        })
        .collect();

    // Normalize B_transitive across all entries, then compute inter_objective
    let max_bt = entries
        .iter()
        .map(|e| e.b_transitive)
        .fold(0.0_f64, f64::max);

    for entry in &mut entries {
        let bt_norm = if max_bt > 0.0 {
            entry.b_transitive / max_bt
        } else {
            0.0
        };
        entry.inter_objective =
            (0.40 * bt_norm + 0.30 * entry.r_body + 0.30 * (1.0 - entry.d_det)).clamp(0.0, 1.0);
    }

    entries.sort_by(|a, b| {
        b.inter_objective
            .partial_cmp(&a.inter_objective)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let duplicate_groups: Vec<Vec<String>> =
        fp_groups.into_values().filter(|g| g.len() >= 2).collect();

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
    let issues_path = workspace.join(ISSUES_FILE);
    let (mut file, existing_ids, open_locations) = load_issue_file_with_indexes(&issues_path);

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

fn load_issue_file_with_indexes(
    issues_path: &Path,
) -> (IssuesFile, HashSet<String>, HashSet<String>) {
    let raw = std::fs::read_to_string(issues_path).unwrap_or_default();
    let file: IssuesFile = if raw.trim().is_empty() {
        IssuesFile::default()
    } else {
        serde_json::from_str(&raw).unwrap_or_default()
    };
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
    created
}

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

fn append_duplicate_issues(
    file: &mut IssuesFile,
    analysis: &InterAnalysis,
    existing_ids: &HashSet<String>,
) -> usize {
    let mut created = 0;
    for group in &analysis.duplicate_groups {
        if let Some(issue) = build_duplicate_issue(group, existing_ids) {
            file.issues.push(issue);
            created += 1;
        }
    }
    created
}

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
            group.join(", ")
        ),
        location: String::new(),
        evidence: vec![format!("MIR fingerprint shared by: {}", group.join(", "))],
        discovered_by: "inter_complexity_analyzer".to_string(),
        score: 0.0,
        ..Issue::default()
    })
}

fn persist_if_created(workspace: &Path, file: &mut IssuesFile, created: usize) -> Result<()> {
    if created > 0 {
        rescore_all(file);
        persist_issues_projection(workspace, file, "generate_hotspot_issues")?;
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
            "inter_objective": "0.40·B_transitive_norm + 0.30·R_body + 0.30·(1−D_det)",
            "B_transitive": "B(F) + mean(B(callee)) for direct callees — depth-1 propagation",
            "R_body": "1.0 if MIR fingerprint matches another function (exact duplicate), else 0.0",
            "D_det": "1.0 − B_norm (higher = more deterministic; reduces with branching)",
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

fn build_issue_fields(
    entry: &InterEntry,
    crate_name: &str,
) -> (String, String, String, Vec<String>) {
    let short_sym = issue_short_symbol(entry);
    let file = shorten_file(&entry.file);
    let title = issue_title(entry, short_sym);
    let kind = issue_kind(entry);
    let dup_note = duplicate_note(entry);

    let description = format!(
        "Inter-function objective score {score:.3} in crate `{crate_name}` \
         (threshold for action: ≥0.20).\n\n\
         Execution model: Detect(this issue) → Propose(LLM refactor) → Apply(patch) → Verify(build+test)\n\n\
         Metrics:\n\
         - B_direct={b_d}  B_transitive={b_t:.1}  (local + mean callee branching)\n\
         - R_body={r:.1}  (1.0 = MIR duplicate exists)\n\
         - D_det={d:.3}  (1.0 = fully deterministic)\n\
         - call_out={co}  call_in={ci}\
         {dup_note}",
        score = entry.inter_objective,
        b_d = entry.b_direct,
        b_t = entry.b_transitive,
        r = entry.r_body,
        d = entry.d_det,
        co = entry.call_out,
        ci = entry.call_in,
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

fn build_issue_evidence(entry: &InterEntry, file: &str) -> Vec<String> {
    let mut evidence = vec![
        format!("inter_objective={:.3}", entry.inter_objective),
        format!(
            "b_direct={} b_transitive={:.1}",
            entry.b_direct, entry.b_transitive
        ),
        format!("r_body={:.1} d_det={:.3}", entry.r_body, entry.d_det),
        format!("call_out={} call_in={}", entry.call_out, entry.call_in),
        format!("location: {file}:{}", entry.line),
    ];
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
    use super::{inter_issue_id, mir_dup_issue_id, priority_from_score, shorten_file};

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
}

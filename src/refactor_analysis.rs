//! Graph-driven refactor issue generation.
//!
//! Implements four analyses derived from the execution graph (graph.json):
//!
//! 1. **Dead code**       — U(D) = 0 : symbols never called and never referenced.
//! 2. **Branch reduction** — CFG BFS from entry reveals unreachable basic blocks.
//! 3. **Helper extraction** — callee-set overlap across callers signals duplicated
//!    logic that could be extracted into a shared function.
//! 4. **Call chain simplification** — single-in single-out pass-through wrappers
//!    that add indirection without adding behaviour.
//!
//! Each generator appends new `Issue` entries to ISSUES.json using the same
//! conventions as `inter_complexity::generate_hotspot_issues`:
//!   - Stable deterministic IDs (skip if already present).
//!   - Threshold-gated (only real signals above noise floor).
//!   - Execution model: Detect(this) → Propose(LLM) → Apply(patch) → Verify.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path as StdPath;
use std::path::Path;

use anyhow::Result;
use serde_json::json;

use crate::issues::{
    is_closed, load_issues_file, persist_issues_projection_with_writer, rescore_all, Issue,
    IssuesFile,
};
use crate::semantic::{SemanticIndex, SymbolOccurrence, SymbolSummary};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_all_refactor_issues(workspace: &Path) -> Result<usize> {
    let crates = SemanticIndex::available_crates(workspace);
    if crates.is_empty() {
        return Ok(0);
    }

    let mut file: IssuesFile = load_issues_file(workspace);
    let (existing_ids, open_locations) = issue_scan_context(&file);

    let mut created = 0;

    for crate_name in &crates {
        let idx = match SemanticIndex::load(workspace, crate_name) {
            Ok(idx) => idx,
            Err(_) => continue,
        };
        let summaries = idx.symbol_summaries();

        created += dead_code_issues(
            &mut file,
            &idx,
            &summaries,
            crate_name,
            &existing_ids,
            &open_locations,
        );
        created += branch_reduction_issues(
            &mut file,
            &idx,
            &summaries,
            crate_name,
            &existing_ids,
            &open_locations,
        );
        created += helper_extraction_issues(&mut file, &idx, &summaries, crate_name, &existing_ids);
        created += call_chain_issues(
            &mut file,
            &summaries,
            crate_name,
            &existing_ids,
            &open_locations,
        );
    }

    persist_generated_refactor_issues(workspace, &mut file, created)?;

    Ok(created)
}

fn issue_scan_context(file: &IssuesFile) -> (HashSet<String>, HashSet<String>) {
    let existing_ids = file.issues.iter().map(|i| i.id.clone()).collect();
    let open_locations = file
        .issues
        .iter()
        .filter(|i| !is_closed(i))
        .map(|i| i.location.clone())
        .collect();
    (existing_ids, open_locations)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &mut issues::IssuesFile, usize
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_generated_refactor_issues(
    workspace: &Path,
    file: &mut IssuesFile,
    created: usize,
) -> Result<()> {
    if created > 0 {
        rescore_all(file);
        persist_issues_projection_with_writer(
            workspace,
            file,
            None,
            "generate_all_refactor_issues",
        )?;
    }
    Ok(())
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_panic_surface_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_panic_surface_issues",
        "auto_panic_surface_",
        32,
        panic_surface_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_state_machine_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_state_machine_issues",
        "auto_state_machine_",
        32,
        state_machine_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_drop_complexity_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_drop_complexity_issues",
        "auto_drop_complexity_",
        32,
        drop_complexity_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_clone_pressure_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_clone_pressure_issues",
        "auto_clone_pressure_",
        32,
        clone_pressure_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_visibility_leak_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_visibility_leak_issues",
        "auto_visibility_leak_",
        32,
        visibility_leak_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_mono_explosion_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_mono_explosion_issues",
        "auto_mono_explosion_",
        24,
        mono_explosion_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_generic_overreach_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_generic_overreach_issues",
        "auto_generic_overreach_",
        24,
        generic_overreach_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_dead_impl_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_dead_impl_issues",
        "auto_dead_impl_",
        24,
        dead_impl_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_rename_symbol_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_rename_symbol_issues",
        "auto_rename_symbol_",
        24,
        rename_symbol_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_dark_assignment_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_dark_assignment_issues",
        "auto_dark_assignment_",
        24,
        dark_assignment_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_loop_invariant_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_loop_invariant_issues",
        "auto_loop_invariant_",
        24,
        loop_invariant_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_redundant_path_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_redundant_path_issues",
        "auto_redundant_path_",
        24,
        redundant_path_issues,
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_alpha_pathway_issues(workspace: &Path) -> Result<usize> {
    generate_detector_issues(
        workspace,
        "generate_alpha_pathway_issues",
        "auto_alpha_pathway_",
        16,
        alpha_pathway_issues,
    )
}

type DetectorFn = fn(&Path, &str, &[SymbolSummary], usize) -> Vec<Issue>;

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &str, usize, for<'a, 'b, 'c> fn(&'a std::path::Path, &'b str, &'c [semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>) -> std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn generate_detector_issues(
    workspace: &Path,
    subject: &str,
    family_prefix: &str,
    limit: usize,
    detector: DetectorFn,
) -> Result<usize> {
    let mut issues = load_issues_file(workspace);
    let previous_open_family_ids: HashSet<String> = issues
        .issues
        .iter()
        .filter(|issue| !is_closed(issue) && issue.id.starts_with(family_prefix))
        .map(|issue| issue.id.clone())
        .collect();
    let mut seen: HashSet<String> = issues.issues.iter().map(|i| i.id.clone()).collect();
    let mut current_ids: HashSet<String> = HashSet::new();
    let mut created = 0usize;

    for crate_name in SemanticIndex::available_crates(workspace) {
        let Ok(idx) = SemanticIndex::load(workspace, &crate_name) else {
            continue;
        };
        let summaries = idx.symbol_summaries();
        for issue in detector(workspace, &crate_name, &summaries, limit) {
            current_ids.insert(issue.id.clone());
            if seen.insert(issue.id.clone()) {
                issues.issues.push(issue);
                created += 1;
            }
        }
    }

    let stale_open_ids: HashSet<String> = previous_open_family_ids
        .difference(&current_ids)
        .cloned()
        .collect();
    let removed = if stale_open_ids.is_empty() {
        0
    } else {
        let before = issues.issues.len();
        issues
            .issues
            .retain(|issue| !stale_open_ids.contains(&issue.id));
        before.saturating_sub(issues.issues.len())
    };

    if created > 0 || removed > 0 {
        rescore_all(&mut issues);
        persist_issues_projection_with_writer(workspace, &issues, None, subject)?;
    }
    Ok(created + removed)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: std::vec::Vec<issues::Issue>, usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn sorted_limited_issues(mut out: Vec<Issue>, limit: usize) -> Vec<Issue> {
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.truncate(limit);
    out
}

fn parent_dir(path: &str) -> std::path::PathBuf {
    StdPath::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
}

fn occurrences_are_local_to_def(def_file: &str, occurrences: &[SymbolOccurrence]) -> bool {
    let def_dir = parent_dir(def_file);
    occurrences
        .iter()
        .all(|occ| parent_dir(&occ.file) == def_dir)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn panic_surface_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let mut out = Vec::new();
    for s in summaries {
        let Some((blocks, panic_surface)) = panic_surface_candidate(s) else {
            continue;
        };
        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!("auto_panic_surface_{crate_name}_{:x}", stable_hash(&s.symbol)),
            title: format!(
                "Panic surface area high in `{}` ({:.2})",
                short_name(&s.symbol),
                panic_surface
            ),
            status: "open".to_string(),
            priority: "medium".to_string(),
            kind: "performance".to_string(),
            description: format!(
                "Function `{}` has panic-heavy branching (assert_count={} over {} MIR blocks) with call_in={}.\n\
                 Convert assert-driven failure paths to explicit Result propagation where possible.",
                s.symbol, s.assert_count, blocks, s.call_in
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "assert_count": s.assert_count,
                "mir_blocks": blocks,
                "panic_surface": panic_surface,
                "call_in": s.call_in
            }),
            acceptance_criteria: vec![
                "assert_count reduced or panic paths moved to Result propagation".to_string(),
                "cargo build and cargo test pass".to_string(),
            ],
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    sorted_limited_issues(out, limit)
}

fn panic_surface_candidate(s: &SymbolSummary) -> Option<(usize, f64)> {
    if s.kind != "fn" {
        return None;
    }
    let blocks = s.mir_blocks.unwrap_or(0);
    if blocks == 0 || s.call_in <= 2 {
        return None;
    }
    let panic_surface = s.assert_count as f64 / blocks.max(1) as f64;
    (panic_surface > 0.4).then_some((blocks, panic_surface))
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn state_machine_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let out: Vec<Issue> = summaries
        .iter()
        .filter(|s| s.switchint_count > 3 && s.has_back_edges)
        .map(|s| build_state_machine_issue(crate_name, s))
        .collect();
    sorted_limited_issues(out, limit)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &semantic::SymbolSummary
/// Outputs: issues::Issue
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_state_machine_issue(crate_name: &str, summary: &SymbolSummary) -> Issue {
    let blocks = summary.mir_blocks.unwrap_or(0);
    let location = shorten_location(&summary.file, summary.line);
    Issue {
        id: format!("auto_state_machine_{crate_name}_{:x}", stable_hash(&summary.symbol)),
        title: format!(
            "Implicit state machine in `{}` (switches={}, loopback)",
            short_name(&summary.symbol),
            summary.switchint_count
        ),
        status: "open".to_string(),
        priority: "medium".to_string(),
        kind: "logic".to_string(),
        description: format!(
            "Function `{}` behaves like an implicit state machine (SwitchInt count {} with CFG back-edges).\n\
             Extract explicit state enum and transition handling.",
            summary.symbol, summary.switchint_count
        ),
        location: location.clone(),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "switchint_count": summary.switchint_count,
            "mir_blocks": blocks,
            "has_back_edges": summary.has_back_edges
        }),
        acceptance_criteria: vec![
            "state transitions are represented explicitly".to_string(),
            "top-level CFG complexity reduced".to_string(),
        ],
        evidence: vec![format!("location: {location}")],
        discovered_by: "refactor_analyzer".to_string(),
        ..Issue::default()
    }
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn drop_complexity_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let mut out = Vec::new();
    for s in summaries {
        let blocks = s.mir_blocks.unwrap_or(0);
        if blocks <= 4 {
            continue;
        }
        let drop_complexity = s.drop_count as f64 / blocks.max(1) as f64;
        if drop_complexity <= 0.3 {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!("auto_drop_complexity_{crate_name}_{:x}", stable_hash(&s.symbol)),
            title: format!(
                "Drop elaboration complexity in `{}` ({:.2})",
                short_name(&s.symbol),
                drop_complexity
            ),
            status: "open".to_string(),
            priority: "medium".to_string(),
            kind: "logic".to_string(),
            description: format!(
                "Function `{}` has heavy conditional-drop handling (drop_count={} across {} MIR blocks).\n\
                 Simplify ownership/drop branching to reduce cleanup complexity.",
                s.symbol, s.drop_count, blocks
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "drop_count": s.drop_count,
                "mir_blocks": blocks,
                "drop_complexity": drop_complexity
            }),
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    sorted_limited_issues(out, limit)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn clone_pressure_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let mut out = Vec::new();
    for s in summaries {
        let stmts = s.mir_stmts.unwrap_or(0);
        if stmts == 0 || s.call_in <= 3 {
            continue;
        }
        let clone_pressure = s.clone_call_count as f64 / stmts.max(1) as f64;
        if clone_pressure <= 0.1 {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!("auto_clone_pressure_{crate_name}_{:x}", stable_hash(&s.symbol)),
            title: format!(
                "Clone pressure in `{}` ({:.2})",
                short_name(&s.symbol),
                clone_pressure
            ),
            status: "open".to_string(),
            priority: "medium".to_string(),
            kind: "performance".to_string(),
            description: format!(
                "Function `{}` has elevated clone-call pressure ({} clone calls over {} MIR stmts, call_in={}).\n\
                 Prefer borrowing/reference-oriented data flow where feasible.",
                s.symbol, s.clone_call_count, stmts, s.call_in
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "clone_call_count": s.clone_call_count,
                "mir_stmts": stmts,
                "clone_pressure": clone_pressure,
                "call_in": s.call_in
            }),
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    sorted_limited_issues(out, limit)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn visibility_leak_issues(
    workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in summaries {
        if s.ref_count == 0 || !symbol_is_pub(s) {
            continue;
        }
        let Ok(occurrences) = idx.symbol_occurrences(&s.symbol) else {
            continue;
        };
        if occurrences.is_empty() {
            continue;
        }
        if !occurrences_are_local_to_def(&s.file, &occurrences) {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!(
                "auto_visibility_leak_{crate_name}_{:x}",
                stable_hash(&s.symbol)
            ),
            title: format!(
                "Visibility can be tightened for `{}`",
                short_name(&s.symbol)
            ),
            status: "open".to_string(),
            priority: "low".to_string(),
            kind: "logic".to_string(),
            description: format!(
                "Public symbol `{}` is only referenced from its defining module directory.\n\
                 Tighten visibility to reduce API surface.",
                s.symbol
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "ref_count": s.ref_count,
                "module_local_refs_only": true
            }),
            acceptance_criteria: vec![
                "visibility narrowed without behavior changes".to_string(),
                "build and tests remain green".to_string(),
            ],
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    sorted_limited_issues(out, limit)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn mono_explosion_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let mut groups: HashMap<String, Vec<&SymbolSummary>> = HashMap::new();
    for s in summaries {
        let base = generic_base_symbol(&s.symbol);
        groups.entry(base).or_default().push(s);
    }
    let mut out = Vec::new();
    for (base, group) in groups {
        if group.len() <= 2 {
            continue;
        }
        let mut fingerprints = HashSet::new();
        let mut all_some = true;
        for s in &group {
            match &s.mir_fingerprint {
                Some(fp) => {
                    fingerprints.insert(fp.as_str());
                }
                None => {
                    all_some = false;
                    break;
                }
            }
        }
        if !all_some || fingerprints.len() != 1 {
            continue;
        }
        let fp = group[0].mir_fingerprint.clone().unwrap_or_default();
        out.push(Issue {
            id: format!("auto_mono_explosion_{crate_name}_{:x}", stable_hash(&base)),
            title: format!(
                "Monomorphization explosion candidate `{}` ({})",
                base,
                group.len()
            ),
            status: "open".to_string(),
            priority: "medium".to_string(),
            kind: "performance".to_string(),
            description: format!(
                "Multiple monomorphized instances share identical MIR fingerprint for base `{}`.\n\
                 Consider trait-object erasure or API reshaping to reduce codegen duplication.",
                base
            ),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "base_symbol": base,
                "monomorphization_count": group.len(),
                "fingerprint": fp
            }),
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    sorted_limited_issues(out, limit)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generic_overreach_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let mut group_size: HashMap<String, usize> = HashMap::new();
    for s in summaries {
        let base = generic_base_symbol(&s.symbol);
        *group_size.entry(base).or_insert(0) += 1;
    }
    let mut out = Vec::new();
    for s in summaries {
        if !s.symbol.contains('<') {
            continue;
        }
        let base = generic_base_symbol(&s.symbol);
        if group_size.get(&base).copied().unwrap_or(0) != 1 {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!(
                "auto_generic_overreach_{crate_name}_{:x}",
                stable_hash(&s.symbol)
            ),
            title: format!("Generic overreach candidate `{}`", short_name(&s.symbol)),
            status: "open".to_string(),
            priority: "low".to_string(),
            kind: "logic".to_string(),
            description: format!(
                "Generic symbol `{}` appears in only one specialization path.\n\
                 Consider concretizing the API to reduce abstraction overhead.",
                s.symbol
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "base_symbol": base,
                "group_size": 1
            }),
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    sorted_limited_issues(out, limit)
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn dead_impl_issues(
    workspace: &Path,
    crate_name: &str,
    _summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Vec::new();
    };
    let triples = idx.semantic_triples(None);
    let impl_edges = implementation_edges(&triples);
    if impl_edges.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (implementer, trait_symbol) in impl_edges {
        if trait_has_dispatch_usage(&triples, &trait_symbol) {
            continue;
        }
        out.push(dead_impl_issue(crate_name, &implementer, &trait_symbol));
    }
    sorted_limited_issues(out, limit)
}

fn implementation_edges(triples: &[crate::semantic::SemanticTriple]) -> Vec<(String, String)> {
    triples
        .iter()
        .filter(|triple| triple.relation.eq_ignore_ascii_case("implements"))
        .map(|triple| (triple.from.clone(), triple.to.clone()))
        .collect()
}

fn dead_impl_issue(crate_name: &str, implementer: &str, trait_symbol: &str) -> Issue {
    Issue {
        id: format!(
            "auto_dead_impl_{crate_name}_{:x}",
            stable_hash(&format!("{implementer}->{trait_symbol}"))
        ),
        title: format!(
            "Unreferenced trait implementation: `{}` implements `{}`",
            short_name(implementer),
            short_name(trait_symbol)
        ),
        status: "open".to_string(),
        priority: "low".to_string(),
        kind: "redundancy".to_string(),
        description: format!(
            "Trait implementation edge `{implementer}` -> `{trait_symbol}` has no observed downstream trait usage in the semantic graph.\n\
             Remove or justify the impl to reduce maintenance overhead."
        ),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "implementer": implementer,
            "trait": trait_symbol,
            "trait_dispatch_usage": 0
        }),
        discovered_by: "refactor_analyzer".to_string(),
        ..Issue::default()
    }
}

fn trait_has_dispatch_usage(triples: &[crate::semantic::SemanticTriple], trait_symbol: &str) -> bool {
    let dyn_trait = format!("dyn {trait_symbol}");
    triples.iter().any(|triple| {
        !triple.relation.eq_ignore_ascii_case("implements")
            && (triple.from == trait_symbol || triple.to == trait_symbol || triple.to.contains(&dyn_trait))
    })
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn rename_symbol_issues(
    _workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let mut fn_prefixes_by_stem: std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for s in summaries {
        if s.kind != "fn" {
            continue;
        }
        let name = short_name(&s.symbol);
        if let Some((prefix, stem)) = split_prefix_and_stem(name) {
            fn_prefixes_by_stem
                .entry(stem)
                .or_default()
                .insert(prefix.to_string());
        }
    }

    let mut out = Vec::new();
    for s in summaries {
        let symbol_name = short_name(&s.symbol);
        if symbol_name.is_empty() {
            continue;
        }

        let mut reasons = rename_ambiguity_reasons(symbol_name);
        if let Some(reason) = inconsistent_prefix_reason(s, symbol_name, &fn_prefixes_by_stem) {
            reasons.push(reason);
        }
        if reasons.is_empty() {
            continue;
        }

        let score = rename_reason_score(&reasons);
        let priority = if score >= 40 { "medium" } else { "low" };
        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!(
                "auto_rename_symbol_{crate_name}_{:x}",
                stable_hash(&s.symbol)
            ),
            title: format!(
                "Rename candidate: `{}` (score={})",
                symbol_name, score
            ),
            status: "open".to_string(),
            priority: priority.to_string(),
            kind: "logic".to_string(),
            description: format!(
                "Symbol `{}` is a deterministic rename candidate based on naming heuristics.\n\
                 Use `symbols_rename_candidates` → `symbols_prepare_rename` → `rename_symbol` and verify with cargo check/test.",
                s.symbol
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "task": "RenameSymbol",
                "rename_candidate_score": score,
                "reasons": reasons,
                "symbol": s.symbol,
                "kind": s.kind
            }),
            acceptance_criteria: vec![
                "symbol renamed with semantic spans".to_string(),
                "build and tests pass".to_string(),
            ],
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    out.sort_by(|a, b| {
        let sa = a
            .metrics
            .get("rename_candidate_score")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let sb = b
            .metrics
            .get("rename_candidate_score")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        sb.cmp(&sa).then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(limit);
    out
}

fn rename_ambiguity_reasons(name: &str) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let vague = [
        "tmp", "temp", "data", "info", "item", "obj", "val", "foo", "bar", "baz", "util", "helper",
        "thing", "stuff", "misc",
    ];
    let mut reasons = Vec::new();
    if lower.len() <= 2 {
        reasons.push("name is very short".to_string());
    }
    if vague.contains(&lower.as_str()) {
        reasons.push("name is ambiguous/generic".to_string());
    }
    if lower.ends_with('_') || lower.contains("__") {
        reasons.push("name shape suggests low clarity".to_string());
    }
    reasons
}

fn split_prefix_and_stem(name: &str) -> Option<(&'static str, String)> {
    let prefixes: [(&str, &str); 12] = [
        ("get_", "get"),
        ("fetch_", "fetch"),
        ("load_", "load"),
        ("read_", "read"),
        ("build_", "build"),
        ("make_", "make"),
        ("create_", "create"),
        ("set_", "set"),
        ("update_", "update"),
        ("handle_", "handle"),
        ("process_", "process"),
        ("compute_", "compute"),
    ];
    prefixes.into_iter().find_map(|(needle, tag)| {
        name.strip_prefix(needle)
            .filter(|rest| !rest.is_empty())
            .map(|rest| (tag, rest.to_string()))
    })
}

fn inconsistent_prefix_reason(
    summary: &SymbolSummary,
    symbol_name: &str,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Option<String> {
    if summary.kind != "fn" {
        return None;
    }
    let (prefix, stem) = split_prefix_and_stem(symbol_name)?;
    let prefixes = prefixes_by_stem.get(&stem)?;
    if prefixes.len() <= 1 {
        return None;
    }
    let mut other: Vec<String> = prefixes
        .iter()
        .filter(|p| p.as_str() != prefix)
        .cloned()
        .collect();
    if other.is_empty() {
        return None;
    }
    other.sort();
    Some(format!(
        "inconsistent verb prefix for stem '{stem}' (also: {})",
        other.join(", ")
    ))
}

fn rename_reason_score(reasons: &[String]) -> u32 {
    let mut score = 10u32;
    for reason in reasons {
        if reason.contains("inconsistent verb prefix") {
            score += 30;
        } else if reason.contains("ambiguous/generic") {
            score += 20;
        } else if reason.contains("very short") {
            score += 10;
        } else {
            score += 5;
        }
    }
    score
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn dark_assignment_issues(
    workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    const DARK_ASSIGNMENT_SYMBOL_BUDGET: usize = 140;
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let candidates = dark_assignment_candidates(summaries, DARK_ASSIGNMENT_SYMBOL_BUDGET);
    for s in candidates {
        if out.len() >= limit {
            break;
        }
        let Some(cfg) = FunctionCfg::build(&idx, &s.symbol) else {
            continue;
        };

        let dark_writes = dark_writes_for_cfg(&cfg, parse_fn_arg_count(s.signature.as_deref()));
        if dark_writes.is_empty() {
            continue;
        }

        let dark_local_count = dark_local_count(&cfg, &dark_writes);
        let mir_stmts = s.mir_stmts.unwrap_or(0);
        let dark_ratio = dark_writes.len() as f64 / mir_stmts.max(1) as f64;
        if dark_ratio <= 0.0 {
            continue;
        }

        out.push(dark_assignment_issue(
            crate_name,
            s,
            &cfg,
            &dark_writes,
            dark_local_count,
            mir_stmts,
            dark_ratio,
        ));
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn dark_writes_for_cfg(cfg: &FunctionCfg, arg_count: usize) -> Vec<StmtKey> {
    let analysis = cfg.reaching_definition_analysis();
    let liveness = cfg.live_out_per_stmt();
    let mut dark_writes = Vec::new();
    for key in cfg.write_nodes() {
        let Some(stmt) = cfg.stmt(key) else { continue };
        if !is_proof_safe_assignment(stmt, cfg.block_terminator(key.block).unwrap_or("")) {
            continue;
        }
        if is_return_or_arg_local(&stmt.written_local, arg_count) {
            continue;
        }
        let live_out = liveness.get(&key).cloned().unwrap_or_default();
        if live_out.contains(&stmt.written_local) || analysis.used_defs.contains(&key) {
            continue;
        }
        dark_writes.push(key);
    }
    dark_writes
}

fn dark_local_count(cfg: &FunctionCfg, dark_writes: &[StmtKey]) -> usize {
    dark_writes
        .iter()
        .filter_map(|k| cfg.stmt(*k).map(|s| s.written_local.clone()))
        .collect::<HashSet<_>>()
        .len()
}

fn sample_dark_locals(cfg: &FunctionCfg, dark_writes: &[StmtKey]) -> Vec<String> {
    let mut dark_locals = dark_writes
        .iter()
        .filter_map(|k| cfg.stmt(*k).map(|s| s.written_local.clone()))
        .collect::<Vec<_>>();
    dark_locals.sort();
    dark_locals.dedup();
    dark_locals.truncate(8);
    dark_locals
}

fn dark_assignment_issue(
    crate_name: &str,
    summary: &SymbolSummary,
    cfg: &FunctionCfg,
    dark_writes: &[StmtKey],
    dark_local_count: usize,
    mir_stmts: usize,
    dark_ratio: f64,
) -> Issue {
    let location = shorten_location(&summary.file, summary.line);
    let confidence_tier = if dark_writes
        .iter()
        .all(|k| cfg.has_exit_postdominator(k.block))
    {
        "high"
    } else {
        "medium"
    };
    Issue {
        id: format!("auto_dark_assign_{crate_name}_{:x}", stable_hash(&summary.symbol)),
        title: format!(
            "Dark assignments in `{}` ({} dead write(s))",
            short_name(&summary.symbol),
            dark_writes.len()
        ),
        status: "open".to_string(),
        priority: "low".to_string(),
        kind: "redundancy".to_string(),
        description: format!(
            "Function `{}` has writes proven dead by reaching-definitions + liveness: \
             no reachable read before overwrite/exit for these writes.",
            summary.symbol
        ),
        location: location.clone(),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "RemoveDarkComputation",
            "dark_local_count": dark_local_count,
            "dark_write_count": dark_writes.len(),
            "mir_stmts": mir_stmts,
            "dark_ratio": dark_ratio,
            "sample_dark_locals": sample_dark_locals(cfg, dark_writes),
            "confidence_tier": confidence_tier,
            "correctness_level": confidence_tier == "high",
        }),
        acceptance_criteria: vec![
            "dead writes removed or justified".to_string(),
            "build and tests pass".to_string(),
        ],
        evidence: vec![format!("location: {location}")],
        discovered_by: "refactor_analyzer".to_string(),
        ..Issue::default()
    }
}

fn dark_assignment_candidates(
    summaries: &[SymbolSummary],
    budget: usize,
) -> Vec<&SymbolSummary> {
    let mut candidates: Vec<&SymbolSummary> = summaries
        .iter()
        .filter(|s| s.kind == "fn")
        .filter(|s| s.mir_stmts.unwrap_or(0) > 4)
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.mir_stmts.unwrap_or(0)));
    candidates.truncate(budget);
    candidates
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn loop_invariant_issues(
    workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    const LOOP_INVARIANT_SYMBOL_BUDGET: usize = 90;
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut candidates: Vec<&SymbolSummary> = summaries
        .iter()
        .filter(|s| s.kind == "fn")
        .filter(|s| s.has_back_edges)
        .filter(|s| s.mir_stmts.unwrap_or(0) >= 6)
        .collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.mir_stmts.unwrap_or(0)));
    candidates.truncate(LOOP_INVARIANT_SYMBOL_BUDGET);
    for s in candidates {
        if out.len() >= limit {
            break;
        }
        let Some(cfg) = FunctionCfg::build(&idx, &s.symbol) else {
            continue;
        };
        let loop_blocks = cfg.loop_blocks();
        if loop_blocks.is_empty() {
            continue;
        }

        let headers = cfg.loop_headers(&loop_blocks);
        let rd = cfg.reaching_definition_analysis();
        let mut invariant_set: HashSet<StmtKey> = HashSet::new();
        let mut changed = true;
        while changed {
            changed = false;
            for key in cfg.stmt_keys_in_blocks(&loop_blocks) {
                let Some(stmt) = cfg.stmt(key) else { continue };
                if !is_proof_safe_assignment(stmt, cfg.block_terminator(key.block).unwrap_or("")) {
                    continue;
                }
                if stmt.read_locals.is_empty() {
                    continue;
                }
                if invariant_set.contains(&key) {
                    continue;
                }
                if stmt.read_locals.iter().all(|local| {
                    local_is_loop_invariant(
                        local,
                        key,
                        &loop_blocks,
                        &headers,
                        &cfg,
                        &rd,
                        &invariant_set,
                    )
                }) {
                    invariant_set.insert(key);
                    changed = true;
                }
            }
        }
        if invariant_set.is_empty() {
            continue;
        }

        let total_loop_stmts = cfg
            .stmt_keys_in_blocks(&loop_blocks)
            .iter()
            .filter(|k| cfg.stmt(**k).is_some())
            .count();
        let invariant_count = invariant_set.len();
        let confidence_tier = if invariant_set
            .iter()
            .all(|k| cfg.has_exit_postdominator(k.block))
        {
            "high"
        } else {
            "medium"
        };

        let location = shorten_location(&s.file, s.line);
        out.push(Issue {
            id: format!(
                "auto_loop_invariant_{crate_name}_{:x}",
                stable_hash(&s.symbol)
            ),
            title: format!(
                "Loop invariant waste in `{}` ({} candidate statement(s))",
                short_name(&s.symbol),
                invariant_count
            ),
            status: "open".to_string(),
            priority: "medium".to_string(),
            kind: "performance".to_string(),
            description: format!(
                "Function `{}` has assignments proven loop-invariant by dominance + reaching-definitions.\n\
                 Hoist invariant computations to loop preheader/setup.",
                s.symbol
            ),
            location: location.clone(),
            scope: format!("crate:{crate_name}"),
            metrics: json!({
                "task": "HoistLoopInvariant",
                "loop_invariant_count": invariant_count,
                "loop_block_count": loop_blocks.len(),
                "total_loop_stmts": total_loop_stmts,
                "confidence_tier": confidence_tier,
                "correctness_level": confidence_tier == "high",
            }),
            acceptance_criteria: vec![
                "invariant computations moved before loop entry".to_string(),
                "build and tests pass".to_string(),
            ],
            evidence: vec![format!("location: {location}")],
            discovered_by: "refactor_analyzer".to_string(),
            ..Issue::default()
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn redundant_path_issues(
    workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Vec::new();
    };
    let summary_by_key = redundant_path_summary_by_key(&idx, summaries);

    let mut out = Vec::new();
    let mut seen_owner_sig: HashSet<(String, u64)> = HashSet::new();
    for pair in idx.redundant_path_pairs() {
        if pair.path_a.owner != pair.path_b.owner {
            continue;
        }
        if pair.path_a.blocks == pair.path_b.blocks {
            continue;
        }
        let owner = pair.path_a.owner.clone();
        let shared_signature = pair.shared_signature;
        if !seen_owner_sig.insert((owner.clone(), shared_signature)) {
            continue;
        }
        let Some(summary) = summary_by_key.get(&owner) else {
            continue;
        };

        out.push(build_redundant_path_issue(
            crate_name,
            &owner,
            shared_signature,
            summary,
            &pair,
        ));
    }
    // Select the highest-signal pairs before applying the limit, so low-ratio
    // pairs that appear early in graph order don't displace high-ratio ones.
    out.sort_by(|a, b| {
        let ra = a
            .metrics
            .get("redundancy_ratio")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let rb = b
            .metrics
            .get("redundancy_ratio")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn build_redundant_path_issue(
    crate_name: &str,
    owner: &str,
    shared_signature: u64,
    summary: &SymbolSummary,
    pair: &crate::semantic::RedundantPathPair,
) -> Issue {
    let location = shorten_location(&summary.file, summary.line);
    let id_seed = format!("{owner}:{shared_signature:016x}");
    let avg_path_len = (pair.path_a.blocks.len() + pair.path_b.blocks.len()) as f64 / 2.0;
    let blocks = summary.mir_blocks.unwrap_or(0).max(1);
    let redundancy_ratio = avg_path_len / blocks as f64;
    Issue {
        id: format!(
            "auto_redundant_path_{crate_name}_{:x}",
            stable_hash(&id_seed)
        ),
        title: format!(
            "Redundant CFG paths in `{}` (signature {:016x})",
            short_name(&summary.symbol),
            shared_signature
        ),
        status: "open".to_string(),
        priority: if redundancy_ratio >= 0.5 {
            "medium".to_string()
        } else {
            "low".to_string()
        },
        kind: "dead_branch".to_string(),
        description: format!(
            "Function `{}` has at least two distinct MIR CFG paths with identical \
             structural path signature. This is a dead/duplicate branch candidate.",
            summary.symbol
        ),
        location: location.clone(),
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "FoldRedundantPath",
            "shared_signature": format!("{shared_signature:016x}"),
            "owner_key": owner,
            "path_a_blocks": pair.path_a.blocks,
            "path_b_blocks": pair.path_b.blocks,
            "redundancy_ratio": redundancy_ratio,
        }),
        acceptance_criteria: vec![
            "duplicate path folded or justified".to_string(),
            "build and tests pass".to_string(),
        ],
        evidence: vec![format!("location: {location}")],
        discovered_by: "refactor_analyzer".to_string(),
        ..Issue::default()
    }
}

fn redundant_path_summary_by_key<'a>(
    idx: &SemanticIndex,
    summaries: &'a [SymbolSummary],
) -> HashMap<String, &'a SymbolSummary> {
    let mut summary_by_key = HashMap::new();
    for summary in summaries {
        if let Ok(key) = idx.canonical_symbol_key(&summary.symbol) {
            summary_by_key.insert(key, summary);
        }
    }
    summary_by_key
}

// ---------------------------------------------------------------------------
// 5. Alpha-equivalent pathway elimination
// ---------------------------------------------------------------------------
//
// Finds call chains where every function in the chain carries the same
// alpha-equivalent type signature AND every caller in the chain is a
// confirmed thin wrapper — i.e., it adds no logic beyond delegating to
// its callee.
//
// Detection strategy (no rustc dependency):
//   1. Canonicalize each fn signature textually: extract the bound-var list
//      from the `<...>` clause, replace each lifetime with 'L0/'L1/... and
//      each type param with T0/T1/... in order of first appearance, strip
//      parameter names, and compare only the `(params) -> return` part.
//   2. Cluster functions by canonical-signature hash.
//   3. Within each cluster, walk call edges to find chains f₀→f₁→...→fₙ.
//   4. Gate: every caller in the chain (all nodes except the leaf) must be a
//      confirmed thin wrapper — mir_blocks ≤ 3 (entry + call + return) OR
//      same mir_fingerprint as the next node (provably identical body).
//   5. The canonical head is chain[last] — the innermost implementation.
//      The wrappers chain[0..last] are the redundant nodes to delete.
//   6. Emit one ticket per confirmed chain with exact agent instructions.

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path, &str, &[semantic::SymbolSummary], usize
/// Outputs: std::vec::Vec<issues::Issue>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn alpha_pathway_issues(
    workspace: &Path,
    crate_name: &str,
    summaries: &[SymbolSummary],
    limit: usize,
) -> Vec<Issue> {
    let Ok(idx) = SemanticIndex::load(workspace, crate_name) else {
        return Vec::new();
    };

    // The compiler already did the hard work: canonical type hashing with real
    // De Bruijn indices, thin-wrapper gating, and chain construction. Just read
    // the pre-computed results and turn them into issues.
    let mut summary_by_symbol: HashMap<&str, &SymbolSummary> = HashMap::new();
    for s in summaries {
        summary_by_symbol.insert(s.symbol.as_str(), s);
    }

    let mut out = Vec::new();

    for pathway in idx.alpha_pathways() {
        let chain = &pathway.chain;
        if chain.len() < 2 {
            continue;
        }
        let canonical_head = &pathway.canonical_head;
        let canonical_head_short = short_name(canonical_head);
        let wrappers: Vec<&str> = chain[..chain.len() - 1]
            .iter()
            .map(String::as_str)
            .collect();
        let chain_short: Vec<&str> = chain.iter().map(|s| short_name(s.as_str())).collect();
        let chain_display = chain_short.join(" → ");
        let wrapper_list = wrappers
            .iter()
            .map(|s| format!("`{}`", short_name(s)))
            .collect::<Vec<_>>()
            .join(", ");

        let chain_locs = alpha_pathway_chain_locs(chain, canonical_head, &summary_by_symbol, pathway);

        let chain_depth = chain.len();
        let id_seed = format!("{crate_name}:{}", chain.join(":"));
        let location = summary_by_symbol
            .get(canonical_head.as_str())
            .map(|s| shorten_location(&s.file, s.line))
            .unwrap_or_default();

        out.push(alpha_pathway_issue(
            crate_name,
            pathway.canonical_sig_hash,
            chain,
            canonical_head,
            canonical_head_short,
            &wrappers,
            chain_depth,
            chain_display,
            wrapper_list,
            location,
            chain_locs,
            id_seed,
        ));
    }

    // Prefer longer chains before truncating.
    out.sort_by(|a, b| {
        let da = a
            .metrics
            .get("chain_depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let db = b
            .metrics
            .get("chain_depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        db.cmp(&da)
    });
    out.truncate(limit);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn alpha_pathway_issue(
    crate_name: &str,
    canonical_sig_hash: u64,
    chain: &[String],
    canonical_head: &str,
    canonical_head_short: &str,
    wrappers: &[&str],
    chain_depth: usize,
    chain_display: String,
    wrapper_list: String,
    location: String,
    chain_locs: Vec<String>,
    id_seed: String,
) -> Issue {
    Issue {
        id: format!("auto_alpha_pathway_{crate_name}_{:x}", stable_hash(&id_seed)),
        title: format!(
            "Alpha-equivalent pathway: {} ({} confirmed wrapper{})",
            chain_display,
            wrappers.len(),
            if wrappers.len() == 1 { "" } else { "s" }
        ),
        status: "open".to_string(),
        priority: if chain_depth >= 3 { "medium" } else { "low" }.to_string(),
        kind: "pathway_elimination".to_string(),
        description: alpha_pathway_description(
            crate_name,
            canonical_head,
            &chain_display,
            &wrapper_list,
        ),
        location,
        scope: format!("crate:{crate_name}"),
        metrics: json!({
            "task": "EliminateAlphaPathway",
            "canonical_sig_hash": format!("{:016x}", canonical_sig_hash),
            "chain_depth": chain_depth,
            "chain": chain,
            "canonical_head": canonical_head,
            "wrapper_symbols": wrappers.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        }),
        acceptance_criteria: alpha_pathway_acceptance_criteria(
            canonical_head_short,
            &wrapper_list,
        ),
        evidence: chain_locs,
        discovered_by: "refactor_analyzer".to_string(),
        ..Issue::default()
    }
}

fn alpha_pathway_description(
    crate_name: &str,
    canonical_head: &str,
    chain_display: &str,
    wrapper_list: &str,
) -> String {
    format!(
        "Functions [{chain_display}] form a confirmed alpha-equivalent wrapper \
         chain in crate `{crate_name}`. Every function carries the same canonical \
         type signature (verified by the compiler using De Bruijn index \
         normalization), and every caller is a compiler-proven pure delegate \
         with a linear MIR return path and no retained branching/drop/assert \
         behavior around the delegated call.\n\n\
         The canonical implementation is `{canonical_head}`. \
         The wrapper(s) {wrapper_list} are safe to delete.\n\n\
         **Execution model:** Redirect call sites → Delete wrappers → Verify\n\n\
         **Step 1 — Redirect call sites.**\n\
         For each wrapper in {wrapper_list}: find every call site (including \
         re-exports, trait impls, and test helpers) and replace it with a direct \
         call to `{canonical_head}`. Update any `use` imports. \
         If a wrapper is `pub`, confirm no external crate depends on it \
         before deletion (search workspace `Cargo.toml`).\n\n\
         **Step 2 — Delete the wrapper definitions.**\n\
         Remove the `fn` definitions for {wrapper_list}.\n\n\
         **Step 3 — Verify.**\n\
         Run `cargo build` and `cargo test --workspace`. \
         Fix any unresolved-symbol errors before closing."
    )
}

fn alpha_pathway_acceptance_criteria(
    canonical_head_short: &str,
    wrapper_list: &str,
) -> Vec<String> {
    vec![
        format!(
            "canonical implementation `{}` retained and unmodified",
            canonical_head_short
        ),
        format!(
            "all call sites of {} redirected to `{}`",
            wrapper_list, canonical_head_short
        ),
        format!("{} deleted from codebase", wrapper_list),
        "cargo build and cargo test --workspace pass".to_string(),
    ]
}

fn alpha_pathway_chain_locs(
    chain: &[String],
    canonical_head: &str,
    summary_by_symbol: &HashMap<&str, &SymbolSummary>,
    pathway: &crate::semantic::AlphaPathwayChain,
) -> Vec<String> {
    let link_locs = chain.windows(2).enumerate().map(|(idx, pair)| {
        let caller_s = summary_by_symbol.get(pair[0].as_str());
        let reason = pathway
            .link_proofs
            .get(idx)
            .cloned()
            .unwrap_or_else(|| "compiler-proven pure delegate".to_string());
        let loc = caller_s
            .map(|s| format!("{} at {}", s.symbol, shorten_location(&s.file, s.line)))
            .unwrap_or_else(|| pair[0].clone());
        format!(
            "`{}` → `{}` confirmed pure delegate ({}); {}",
            short_name(&pair[0]),
            short_name(&pair[1]),
            reason,
            loc
        )
    });
    link_locs
        .chain(std::iter::once(alpha_pathway_canonical_loc(
            canonical_head,
            summary_by_symbol,
        )))
        .collect()
}

fn alpha_pathway_canonical_loc(
    canonical_head: &str,
    summary_by_symbol: &HashMap<&str, &SymbolSummary>,
) -> String {
    match summary_by_symbol.get(canonical_head) {
        Some(s) => format!(
            "canonical: `{}` at {}",
            canonical_head,
            shorten_location(&s.file, s.line)
        ),
        None => format!("canonical: `{canonical_head}`"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct StmtKey {
    block: usize,
    idx: usize,
}

#[derive(Debug, Default)]
struct ReachingDefAnalysis {
    used_defs: HashSet<StmtKey>,
    in_defs: HashMap<StmtKey, HashSet<StmtKey>>,
}

#[derive(Debug)]
struct FunctionCfg {
    blocks: HashMap<usize, crate::semantic::SymbolCfgBlock>,
    succ: HashMap<usize, HashSet<usize>>,
    pred: HashMap<usize, HashSet<usize>>,
    exits: HashSet<usize>,
    dom: HashMap<usize, HashSet<usize>>,
    pdom: HashMap<usize, HashSet<usize>>,
}

impl FunctionCfg {
    fn build(idx: &SemanticIndex, symbol: &str) -> Option<Self> {
        let blocks_vec = idx.symbol_cfg_blocks(symbol);
        if blocks_vec.is_empty() {
            return None;
        }
        let blocks: HashMap<usize, crate::semantic::SymbolCfgBlock> =
            blocks_vec.into_iter().map(|b| (b.block, b)).collect();
        let mut succ: HashMap<usize, HashSet<usize>> = blocks
            .keys()
            .copied()
            .map(|b| (b, HashSet::new()))
            .collect();
        let mut pred: HashMap<usize, HashSet<usize>> = blocks
            .keys()
            .copied()
            .map(|b| (b, HashSet::new()))
            .collect();
        for edge in idx.symbol_cfg_edges(symbol) {
            if !blocks.contains_key(&edge.from) || !blocks.contains_key(&edge.to) {
                continue;
            }
            succ.entry(edge.from).or_default().insert(edge.to);
            pred.entry(edge.to).or_default().insert(edge.from);
        }
        let entry = blocks.keys().copied().min().unwrap_or(0);
        let exits: HashSet<usize> = succ
            .iter()
            .filter_map(|(block, targets)| targets.is_empty().then_some(*block))
            .collect();
        let dom = compute_dominators(entry, &blocks, &pred);
        let pdom = compute_post_dominators(&blocks, &succ, &exits);
        Some(Self {
            blocks,
            succ,
            pred,
            exits,
            dom,
            pdom,
        })
    }

    fn stmt(&self, key: StmtKey) -> Option<&crate::semantic::StatementInfo> {
        self.blocks
            .get(&key.block)
            .and_then(|b| b.statements.iter().find(|s| s.idx == key.idx))
    }

    fn block_terminator(&self, block: usize) -> Option<&str> {
        self.blocks.get(&block).map(|b| b.terminator.as_str())
    }

    fn stmt_keys_in_blocks(&self, blocks: &HashSet<usize>) -> Vec<StmtKey> {
        let mut out = Vec::new();
        for block in blocks {
            if let Some(data) = self.blocks.get(block) {
                for stmt in &data.statements {
                    out.push(StmtKey {
                        block: *block,
                        idx: stmt.idx,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.block.cmp(&b.block).then(a.idx.cmp(&b.idx)));
        out
    }

    /// Intent: canonical_write
    /// Resource: error
    /// Inputs: &refactor_analysis::FunctionCfg
    /// Outputs: std::vec::Vec<refactor_analysis::StmtKey>
    /// Effects: error
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    fn write_nodes(&self) -> Vec<StmtKey> {
        let mut out = Vec::new();
        for (block, data) in &self.blocks {
            for stmt in &data.statements {
                if !stmt.written_local.trim().is_empty() {
                    out.push(StmtKey {
                        block: *block,
                        idx: stmt.idx,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.block.cmp(&b.block).then(a.idx.cmp(&b.idx)));
        out
    }

    fn loop_blocks(&self) -> HashSet<usize> {
        self.blocks
            .iter()
            .filter_map(|(block, data)| data.in_loop.then_some(*block))
            .collect()
    }

    fn loop_headers(&self, loop_blocks: &HashSet<usize>) -> HashSet<usize> {
        let mut headers = HashSet::new();
        for block in loop_blocks {
            let preds = self.pred.get(block).cloned().unwrap_or_default();
            if preds.iter().any(|pred| !loop_blocks.contains(pred)) {
                headers.insert(*block);
            }
        }
        if headers.is_empty() {
            if let Some(min_block) = loop_blocks.iter().copied().min() {
                headers.insert(min_block);
            }
        }
        headers
    }

    fn has_exit_postdominator(&self, block: usize) -> bool {
        self.pdom
            .get(&block)
            .map(|set| self.exits.iter().any(|exit| set.contains(exit)))
            .unwrap_or(false)
    }

    fn block_dominates(&self, dominator: usize, dominated: usize) -> bool {
        self.dom
            .get(&dominated)
            .map(|set| set.contains(&dominator))
            .unwrap_or(false)
    }

    fn stmt_dominates(&self, a: StmtKey, b: StmtKey) -> bool {
        if a.block == b.block {
            return a.idx <= b.idx;
        }
        self.block_dominates(a.block, b.block)
    }

    fn statement_predecessors(&self, key: StmtKey) -> Vec<StmtKey> {
        let mut out = Vec::new();
        let Some(block) = self.blocks.get(&key.block) else {
            return out;
        };
        let mut sorted = block.statements.iter().map(|s| s.idx).collect::<Vec<_>>();
        sorted.sort_unstable();
        if let Some(pos) = sorted.iter().position(|idx| *idx == key.idx) {
            if pos > 0 {
                out.push(StmtKey {
                    block: key.block,
                    idx: sorted[pos - 1],
                });
                return out;
            }
        }
        for pred in self.pred.get(&key.block).cloned().unwrap_or_default() {
            if let Some(pred_block) = self.blocks.get(&pred) {
                if let Some(last_stmt) = pred_block.statements.iter().max_by_key(|s| s.idx) {
                    out.push(StmtKey {
                        block: pred,
                        idx: last_stmt.idx,
                    });
                }
            }
        }
        out
    }

    fn all_stmt_keys(&self) -> Vec<StmtKey> {
        let mut out = Vec::new();
        for (block, data) in &self.blocks {
            for stmt in &data.statements {
                out.push(StmtKey {
                    block: *block,
                    idx: stmt.idx,
                });
            }
        }
        out.sort_by(|a, b| a.block.cmp(&b.block).then(a.idx.cmp(&b.idx)));
        out
    }

    fn live_out_per_stmt(&self) -> HashMap<StmtKey, HashSet<String>> {
        let mut live_in_block: HashMap<usize, HashSet<String>> = self
            .blocks
            .keys()
            .copied()
            .map(|b| (b, HashSet::new()))
            .collect();
        let mut live_out_block = live_in_block.clone();
        let mut live_out_stmt: HashMap<StmtKey, HashSet<String>> = HashMap::new();

        let mut changed = true;
        while changed {
            changed = false;
            let mut block_ids: Vec<usize> = self.blocks.keys().copied().collect();
            block_ids.sort_unstable_by(|a, b| b.cmp(a));
            for block in block_ids {
                let succ_live: HashSet<String> = self
                    .succ
                    .get(&block)
                    .into_iter()
                    .flatten()
                    .flat_map(|succ| live_in_block.get(succ).cloned().unwrap_or_default())
                    .collect();
                if live_out_block.get(&block) != Some(&succ_live) {
                    live_out_block.insert(block, succ_live.clone());
                    changed = true;
                }

                let mut cursor = succ_live;
                let mut stmts = self
                    .blocks
                    .get(&block)
                    .map(|b| b.statements.clone())
                    .unwrap_or_default();
                stmts.sort_by_key(|s| s.idx);
                for stmt in stmts.iter().rev() {
                    let key = StmtKey {
                        block,
                        idx: stmt.idx,
                    };
                    live_out_stmt.insert(key, cursor.clone());
                    let mut next = cursor;
                    if !stmt.written_local.is_empty() {
                        next.remove(&stmt.written_local);
                    }
                    for local in &stmt.read_locals {
                        if !local.is_empty() {
                            next.insert(local.clone());
                        }
                    }
                    cursor = next;
                }
                if live_in_block.get(&block) != Some(&cursor) {
                    live_in_block.insert(block, cursor);
                    changed = true;
                }
            }
        }
        live_out_stmt
    }

    /// Intent: diagnostic_scan
    /// Resource: error
    /// Inputs: &refactor_analysis::FunctionCfg
    /// Outputs: refactor_analysis::ReachingDefAnalysis
    /// Effects: error
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    fn reaching_definition_analysis(&self) -> ReachingDefAnalysis {
        let stmt_keys = self.all_stmt_keys();
        let defs_by_local = self.defs_by_local(&stmt_keys);

        let mut in_defs: HashMap<StmtKey, HashSet<StmtKey>> = stmt_keys
            .iter()
            .copied()
            .map(|k| (k, HashSet::new()))
            .collect();
        let mut out_defs = in_defs.clone();
        let mut changed = true;
        while changed {
            changed = false;
            for key in &stmt_keys {
                let preds = self.statement_predecessors(*key);
                let mut in_set: HashSet<StmtKey> = HashSet::new();
                for pred in preds {
                    in_set.extend(out_defs.get(&pred).cloned().unwrap_or_default());
                }

                let Some(stmt) = self.stmt(*key) else {
                    continue;
                };
                let mut out_set = in_set.clone();
                if !stmt.written_local.is_empty() {
                    if let Some(kills) = defs_by_local.get(&stmt.written_local) {
                        for kill in kills {
                            out_set.remove(kill);
                        }
                    }
                    out_set.insert(*key);
                }

                if in_defs.get(key) != Some(&in_set) {
                    in_defs.insert(*key, in_set);
                    changed = true;
                }
                if out_defs.get(key) != Some(&out_set) {
                    out_defs.insert(*key, out_set);
                    changed = true;
                }
            }
        }

        let mut used_defs: HashSet<StmtKey> = HashSet::new();
        for key in &stmt_keys {
            let Some(stmt) = self.stmt(*key) else {
                continue;
            };
            if stmt.read_locals.is_empty() {
                continue;
            }
            let reaching = in_defs.get(key).cloned().unwrap_or_default();
            for local in &stmt.read_locals {
                for def in &reaching {
                    if self
                        .stmt(*def)
                        .map(|s| s.written_local.as_str() == local.as_str())
                        .unwrap_or(false)
                    {
                        used_defs.insert(*def);
                    }
                }
            }
        }

        ReachingDefAnalysis { used_defs, in_defs }
    }

    fn defs_by_local(&self, stmt_keys: &[StmtKey]) -> HashMap<String, HashSet<StmtKey>> {
        let mut defs_by_local: HashMap<String, HashSet<StmtKey>> = HashMap::new();
        for key in stmt_keys {
            if let Some(stmt) = self.stmt(*key) {
                if !stmt.written_local.is_empty() {
                    defs_by_local
                        .entry(stmt.written_local.clone())
                        .or_default()
                        .insert(*key);
                }
            }
        }
        defs_by_local
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: usize, &std::collections::HashMap<usize, semantic::SymbolCfgBlock>, &std::collections::HashMap<usize, std::collections::HashSet<usize>>
/// Outputs: std::collections::HashMap<usize, std::collections::HashSet<usize>>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn compute_dominators(
    entry: usize,
    blocks: &HashMap<usize, crate::semantic::SymbolCfgBlock>,
    pred: &HashMap<usize, HashSet<usize>>,
) -> HashMap<usize, HashSet<usize>> {
    let all: HashSet<usize> = blocks.keys().copied().collect();
    let mut dom: HashMap<usize, HashSet<usize>> = HashMap::new();
    for block in blocks.keys().copied() {
        if block == entry {
            dom.insert(block, HashSet::from([entry]));
        } else {
            dom.insert(block, all.clone());
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for block in blocks.keys().copied() {
            if block == entry {
                continue;
            }
            let predecessors = pred.get(&block).cloned().unwrap_or_default();
            if predecessors.is_empty() {
                continue;
            }
            let mut inter: Option<HashSet<usize>> = None;
            for p in predecessors {
                let candidate = dom.get(&p).cloned().unwrap_or_default();
                inter = Some(match inter {
                    None => candidate,
                    Some(acc) => acc.intersection(&candidate).copied().collect(),
                });
            }
            let mut next = inter.unwrap_or_default();
            next.insert(block);
            if dom.get(&block) != Some(&next) {
                dom.insert(block, next);
                changed = true;
            }
        }
    }
    dom
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::collections::HashMap<usize, semantic::SymbolCfgBlock>, &std::collections::HashMap<usize, std::collections::HashSet<usize>>, &std::collections::HashSet<usize>
/// Outputs: std::collections::HashMap<usize, std::collections::HashSet<usize>>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn compute_post_dominators(
    blocks: &HashMap<usize, crate::semantic::SymbolCfgBlock>,
    succ: &HashMap<usize, HashSet<usize>>,
    exits: &HashSet<usize>,
) -> HashMap<usize, HashSet<usize>> {
    let all: HashSet<usize> = blocks.keys().copied().collect();
    let mut pdom: HashMap<usize, HashSet<usize>> = HashMap::new();
    for block in blocks.keys().copied() {
        if exits.contains(&block) {
            pdom.insert(block, HashSet::from([block]));
        } else {
            pdom.insert(block, all.clone());
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for block in blocks.keys().copied() {
            if exits.contains(&block) {
                continue;
            }
            let successors = succ.get(&block).cloned().unwrap_or_default();
            if successors.is_empty() {
                continue;
            }
            let mut inter: Option<HashSet<usize>> = None;
            for s in successors {
                let candidate = pdom.get(&s).cloned().unwrap_or_default();
                inter = Some(match inter {
                    None => candidate,
                    Some(acc) => acc.intersection(&candidate).copied().collect(),
                });
            }
            let mut next = inter.unwrap_or_default();
            next.insert(block);
            if pdom.get(&block) != Some(&next) {
                pdom.insert(block, next);
                changed = true;
            }
        }
    }
    pdom
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: std::option::Option<&str>
/// Outputs: usize
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_fn_arg_count(signature: Option<&str>) -> usize {
    let Some(sig) = signature else { return 0 };
    let open = sig.find('(');
    let close = sig.find(')');
    let (Some(open), Some(close)) = (open, close) else {
        return 0;
    };
    if close <= open + 1 {
        return 0;
    }
    let args = &sig[(open + 1)..close];
    if args.trim().is_empty() {
        0
    } else {
        args.split(',').count()
    }
}

fn is_return_or_arg_local(local: &str, arg_count: usize) -> bool {
    let Some(num) = local
        .strip_prefix('_')
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return false;
    };
    num == 0 || (num >= 1 && num <= arg_count)
}

fn is_proof_safe_assignment(stmt: &crate::semantic::StatementInfo, terminator: &str) -> bool {
    if !stmt.kind.eq_ignore_ascii_case("assign") {
        return false;
    }
    let term = terminator.to_ascii_lowercase();
    !(term.starts_with("call")
        || term.starts_with("assert")
        || term.starts_with("drop")
        || term.starts_with("yield"))
}

fn local_is_loop_invariant(
    local: &str,
    use_key: StmtKey,
    loop_blocks: &HashSet<usize>,
    headers: &HashSet<usize>,
    cfg: &FunctionCfg,
    rd: &ReachingDefAnalysis,
    invariant_set: &HashSet<StmtKey>,
) -> bool {
    let reaching = rd.in_defs.get(&use_key).cloned().unwrap_or_default();
    let defs_for_local: Vec<StmtKey> = reaching
        .into_iter()
        .filter(|def| {
            cfg.stmt(*def)
                .map(|stmt| stmt.written_local.as_str() == local)
                .unwrap_or(false)
        })
        .collect();
    if defs_for_local.is_empty() {
        return false;
    }
    defs_for_local.iter().all(|def| {
        if !loop_blocks.contains(&def.block) {
            headers.iter().all(|h| cfg.block_dominates(def.block, *h))
        } else {
            invariant_set.contains(def) && cfg.stmt_dominates(*def, use_key)
        }
    })
}

// ---------------------------------------------------------------------------
// 1. Dead code  —  U(D) = 0
// ---------------------------------------------------------------------------

fn issue_already_tracked(
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
    id: &str,
    location: &str,
) -> bool {
    existing_ids.contains(id)
        || open_locations.contains(location)
        || open_locations.iter().any(|l| l.contains(location))
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &mut issues::IssuesFile, &semantic::SemanticIndex, &[semantic::SymbolSummary], &str, &std::collections::HashSet<std::string::String>, &std::collections::HashSet<std::string::String>
/// Outputs: usize
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn dead_code_issues(
    file: &mut IssuesFile,
    _idx: &SemanticIndex,
    summaries: &[crate::semantic::SymbolSummary],
    crate_name: &str,
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
) -> usize {
    let mut created = 0;
    for s in summaries {
        let mir_blocks = s.mir_blocks.unwrap_or(0);
        if !is_dead_code_candidate(s, mir_blocks) {
            continue;
        }
        // Both call-graph in-degree AND HIR reference count must be zero.
        let location = shorten_location(&s.file, s.line);
        let id = dead_code_id(crate_name, &s.symbol);
        if issue_already_tracked(existing_ids, open_locations, &id, &location) {
            continue;
        }
        file.issues.push(build_dead_code_issue(
            crate_name, s, mir_blocks, id, location,
        ));
        created += 1;
    }
    created
}

fn is_dead_code_candidate(s: &crate::semantic::SymbolSummary, mir_blocks: usize) -> bool {
    s.kind == "fn"
        && mir_blocks > 0
        && s.call_in == 0
        && s.ref_count == 0
        && !is_exempt_from_dead_code(&s.symbol)
}

fn build_dead_code_issue(
    crate_name: &str,
    s: &crate::semantic::SymbolSummary,
    mir_blocks: usize,
    id: String,
    location: String,
) -> Issue {
    let short = short_name(&s.symbol);
    Issue {
        id,
        title: format!("Dead code: `{short}` — zero callers and zero references"),
        status: "open".to_string(),
        priority: "medium".to_string(),
        kind: "dead_code".to_string(),
        description: format!(
            "Function `{sym}` in crate `{crate_name}` has:\n\
             - call_in = 0  (no call-graph edges point to it)\n\
             - ref_count = 0  (not referenced in source)\n\
             - mir_blocks = {b}  (has a real body)\n\n\
             Execution model: Detect(this issue) → Propose(LLM delete/verify) → Apply(patch) → Verify(build+test)\n\n\
            Verify that this is truly unreachable (no dynamic dispatch, no #[no_mangle], \
             not re-exported), then delete it to reduce U and simplify the execution graph.",
            sym = s.symbol,
            b = mir_blocks,
        ),
        location: location.clone(),
        evidence: vec![
            format!("call_in=0 ref_count=0 mir_blocks={mir_blocks}"),
            format!("location: {location}"),
        ],
        discovered_by: "refactor_analyzer".to_string(),
        score: 0.0,
        ..Issue::default()
    }
}

/// Functions that are legitimately zero-referenced by design.
fn is_exempt_from_dead_code(symbol: &str) -> bool {
    let lower = symbol.to_ascii_lowercase();
    let leaf = symbol.rsplit("::").next().unwrap_or(symbol);
    let leaf_lower = leaf.to_ascii_lowercase();
    // Entry points and standard trait implementations.
    matches!(
        leaf_lower.as_str(),
        "main"
            | "new"
            | "default"
            | "drop"
            | "from"
            | "into"
            | "clone"
            | "fmt"
            | "eq"
            | "hash"
            | "partial_eq"
            | "serialize"
            | "deserialize"
    ) || lower.contains("test")
        || lower.contains("bench")
        || lower.contains("example")
}

// ---------------------------------------------------------------------------
// 2. Branch reduction  —  unreachable basic blocks
// ---------------------------------------------------------------------------

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &mut issues::IssuesFile, &semantic::SemanticIndex, &[semantic::SymbolSummary], &str, &std::collections::HashSet<std::string::String>, &std::collections::HashSet<std::string::String>
/// Outputs: usize
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn branch_reduction_issues(
    file: &mut IssuesFile,
    idx: &SemanticIndex,
    summaries: &[crate::semantic::SymbolSummary],
    crate_name: &str,
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
) -> usize {
    let mut created = 0;
    for s in summaries {
        let Some((_total, unreachable)) = branch_reduction_candidate(idx, s) else {
            continue;
        };
        let location = shorten_location(&s.file, s.line);
        let id = branch_reduce_id(crate_name, &s.symbol);
        if issue_already_tracked(existing_ids, open_locations, &id, &location) {
            continue;
        }
        let short = short_name(&s.symbol);
        let total = s.mir_blocks.unwrap_or(0);
        let pct = (unreachable * 100).saturating_div(total.max(1));
        file.issues.push(Issue {
            id,
            title: format!(
                "Branch reduction: `{short}` has {unreachable}/{total} unreachable block(s) ({pct}%)"
            ),
            status: "open".to_string(),
            priority: priority_from_unreachable(pct),
            kind: "branch_reduction".to_string(),
            description: format!(
                "Function `{sym}` in crate `{crate_name}` has {unreachable} non-cleanup \
                 basic block(s) that are unreachable from the entry block \
                 ({unreachable}/{total} = {pct}%).\n\n\
                 Execution model: Detect(this issue) → Propose(LLM remove branch) → Apply(patch) → Verify(build+test)\n\n\
                 Remove the dead branch(es) to simplify the CFG and reduce B directly:\n\
                 [ B_i → B_j never taken ] ⟹ remove branch",
                sym = s.symbol,
            ),
            location: location.clone(),
            evidence: vec![
                format!("unreachable_blocks={unreachable} total_blocks={total} pct={pct}%"),
                format!("location: {location}"),
            ],
            discovered_by: "refactor_analyzer".to_string(),
            score: 0.0,
            ..Issue::default()
        });
        created += 1;
    }
    created
}

fn branch_reduction_candidate(
    idx: &SemanticIndex,
    s: &crate::semantic::SymbolSummary,
) -> Option<(usize, usize)> {
    if s.kind != "fn" {
        return None;
    }
    let total = s.mir_blocks.unwrap_or(0);
    if total < 2 {
        return None;
    }
    let sym_key = idx.canonical_symbol_key(&s.symbol).ok()?;
    let unreachable = idx.unreachable_block_count(&sym_key);
    (unreachable > 0).then_some((total, unreachable))
}

fn priority_from_unreachable(pct: usize) -> String {
    if pct >= 40 {
        "high".to_string()
    } else if pct >= 15 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

// ---------------------------------------------------------------------------
// 3. Helper extraction  —  repeated callee-set overlap
// ---------------------------------------------------------------------------

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &mut issues::IssuesFile, &semantic::SemanticIndex, &[semantic::SymbolSummary], &str, &std::collections::HashSet<std::string::String>
/// Outputs: usize
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn helper_extraction_issues(
    file: &mut IssuesFile,
    idx: &SemanticIndex,
    summaries: &[crate::semantic::SymbolSummary],
    crate_name: &str,
    existing_ids: &HashSet<String>,
) -> usize {
    // Build caller → callee_set map (local fns only, skip tiny single-block fns).
    let local_fns: HashSet<&str> = summaries
        .iter()
        .filter(|s| s.kind == "fn" && s.mir_blocks.unwrap_or(0) >= 2)
        .map(|s| s.symbol.as_str())
        .collect();

    let mut caller_callees: HashMap<&str, HashSet<String>> = HashMap::new();
    for s in summaries {
        if !local_fns.contains(s.symbol.as_str()) {
            continue;
        }
        let key = match idx.canonical_symbol_key(&s.symbol) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let callees: HashSet<String> = idx
            .direct_callee_paths(&key)
            .into_iter()
            .filter(|c| local_fns.contains(c.as_str())) // only local callees
            .collect();
        if callees.len() >= 2 {
            caller_callees.insert(s.symbol.as_str(), callees);
        }
    }

    if caller_callees.len() < 2 {
        return 0;
    }

    // Count how many callers share each pair of callees.
    // pair → Vec<caller>
    let mut pair_callers: HashMap<(String, String), Vec<&str>> = HashMap::new();
    for (&caller, callees) in &caller_callees {
        let mut sorted_callees: Vec<&str> = callees.iter().map(|s| s.as_str()).collect();
        sorted_callees.sort();
        for i in 0..sorted_callees.len() {
            for j in (i + 1)..sorted_callees.len() {
                pair_callers
                    .entry((sorted_callees[i].to_string(), sorted_callees[j].to_string()))
                    .or_default()
                    .push(caller);
            }
        }
    }

    // Emit issues for pairs shared by ≥2 callers, sorted by frequency descending.
    let mut pairs: Vec<_> = pair_callers
        .iter()
        .filter(|(_, callers)| callers.len() >= 2)
        .collect();
    pairs.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let mut created = 0;
    // Limit: top 3 helper-extraction opportunities per crate to avoid flooding.
    for ((b, c), callers) in pairs.iter().take(3) {
        let id = helper_extract_id(crate_name, b, c);
        if existing_ids.contains(&id) {
            continue;
        }
        let b_short = short_name(b);
        let c_short = short_name(c);
        let caller_list: Vec<&str> = callers.iter().copied().take(5).collect();
        file.issues.push(Issue {
            id,
            title: format!(
                "Helper extraction: `{b_short}` → `{c_short}` repeated in {} callers",
                callers.len()
            ),
            status: "open".to_string(),
            priority: if callers.len() >= 3 { "medium".to_string() } else { "low".to_string() },
            kind: "helper_extraction".to_string(),
            description: format!(
                "The call sequence `{b}` → `{c}` appears in {n} independent callers in \
                 crate `{crate_name}`.\n\n\
                 Execution model: Detect(this issue) → Propose(LLM extract helper) → Apply(patch) → Verify(build+test)\n\n\
                 Extract the shared subpath into a named helper:\n\
                 [ A → B → C ]  and  [ X → B → C ]  ⟹  new helper fn h() {{ B(); C(); }}\n\n\
                 Callers: {callers}",
                n = callers.len(),
                callers = caller_list.join(", "),
            ),
            location: String::new(),
            evidence: vec![
                format!("shared_subpath: {b} → {c}"),
                format!("caller_count={}", callers.len()),
                format!("callers: {}", caller_list.join(", ")),
            ],
            discovered_by: "refactor_analyzer".to_string(),
            score: 0.0,
            ..Issue::default()
        });
        created += 1;
    }
    created
}

// ---------------------------------------------------------------------------
// 4. Call chain simplification  —  pass-through wrappers
// ---------------------------------------------------------------------------

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &mut issues::IssuesFile, &[semantic::SymbolSummary], &str, &std::collections::HashSet<std::string::String>, &std::collections::HashSet<std::string::String>
/// Outputs: usize
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn call_chain_issues(
    file: &mut IssuesFile,
    summaries: &[crate::semantic::SymbolSummary],
    crate_name: &str,
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
) -> usize {
    let mut created = 0;
    for (s, blocks) in summaries.iter().filter_map(call_chain_candidate) {
        let location = shorten_location(&s.file, s.line);
        let id = call_chain_id(crate_name, &s.symbol);
        if issue_already_tracked(existing_ids, open_locations, &id, &location) {
            continue;
        }
        let short = short_name(&s.symbol);
        file.issues.push(Issue {
            id,
            title: format!(
                "Call chain: `{short}` is a pass-through wrapper (call_in={ci}, call_out=1, blocks={b})",
                ci = s.call_in,
                b = blocks,
            ),
            status: "open".to_string(),
            priority: "low".to_string(),
            kind: "call_chain".to_string(),
            description: format!(
                "Function `{sym}` in crate `{crate_name}` delegates entirely to one callee \
                 (call_out=1) with a tiny body ({b} block(s)).\n\n\
                 Execution model: Detect(this issue) → Propose(LLM inline or collapse) → Apply(patch) → Verify(build+test)\n\n\
                 If the wrapper adds no meaningful logic, inline it at call sites to shorten \
                 the call chain:\n\
                 [ D₁ → D₂ → D₃ ]  where D₂ is trivial  ⟹  [ D₁ → D₃ ]",
                sym = s.symbol,
                b = blocks,
            ),
            location: location.clone(),
            evidence: vec![
                format!("call_in={} call_out=1 mir_blocks={}", s.call_in, blocks),
                format!("location: {location}"),
            ],
            discovered_by: "refactor_analyzer".to_string(),
            score: 0.0,
            ..Issue::default()
        });
        created += 1;
    }
    created
}

fn call_chain_candidate(
    s: &crate::semantic::SymbolSummary,
) -> Option<(&crate::semantic::SymbolSummary, usize)> {
    let blocks = s.mir_blocks.unwrap_or(0);
    // Pass-through: single callee, at least one caller, tiny body.
    (s.kind == "fn"
        && s.call_out == 1
        && s.call_in > 0
        && (1..=3).contains(&blocks)
        && !is_exempt_from_dead_code(&s.symbol))
    .then_some((s, blocks))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn stable_hash(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn dead_code_id(crate_name: &str, symbol: &str) -> String {
    let h = stable_hash(symbol);
    format!("auto_dead_code_{crate_name}_{h:x}")
}

fn branch_reduce_id(crate_name: &str, symbol: &str) -> String {
    let h = stable_hash(symbol);
    format!("auto_branch_reduce_{crate_name}_{h:x}")
}

fn helper_extract_id(crate_name: &str, b: &str, c: &str) -> String {
    let key = format!("{b}::{c}");
    let h = stable_hash(&key);
    format!("auto_helper_extract_{crate_name}_{h:x}")
}

fn call_chain_id(crate_name: &str, symbol: &str) -> String {
    let h = stable_hash(symbol);
    format!("auto_call_chain_{crate_name}_{h:x}")
}

fn short_name(symbol: &str) -> &str {
    symbol.rsplit("::").next().unwrap_or(symbol)
}

fn generic_base_symbol(symbol: &str) -> String {
    symbol.split('<').next().unwrap_or(symbol).to_string()
}

fn symbol_is_pub(summary: &SymbolSummary) -> bool {
    summary
        .signature
        .as_deref()
        .map(|sig| sig.trim_start().starts_with("pub "))
        .unwrap_or(false)
}

fn shorten_location(file: &str, line: u32) -> String {
    let short = if let Some(idx) = file.find("/src/") {
        file[idx + 1..].to_string()
    } else {
        file.rsplit('/').next().unwrap_or(file).to_string()
    };
    format!("{short}:{line}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        dead_code_id, helper_extract_id, is_exempt_from_dead_code, priority_from_unreachable,
        short_name,
    };

    #[test]
    fn issue_ids_are_deterministic() {
        assert_eq!(
            dead_code_id("my_crate", "foo::bar"),
            dead_code_id("my_crate", "foo::bar"),
        );
        assert_ne!(
            dead_code_id("my_crate", "foo::bar"),
            dead_code_id("my_crate", "foo::baz"),
        );
    }

    #[test]
    fn helper_extract_id_is_commutative_wrt_crate_but_not_order() {
        // (B, C) and (C, B) are different subpaths — IDs should differ.
        let ab = helper_extract_id("c", "A", "B");
        let ba = helper_extract_id("c", "B", "A");
        assert_ne!(ab, ba);
    }

    #[test]
    fn exempt_names_are_skipped() {
        assert!(is_exempt_from_dead_code("module::main"));
        assert!(is_exempt_from_dead_code("Type::new"));
        assert!(is_exempt_from_dead_code("test_something"));
        assert!(!is_exempt_from_dead_code("tools::handle_batch_action"));
    }

    #[test]
    fn priority_bands_correct() {
        assert_eq!(priority_from_unreachable(50), "high");
        assert_eq!(priority_from_unreachable(40), "high");
        assert_eq!(priority_from_unreachable(39), "medium");
        assert_eq!(priority_from_unreachable(15), "medium");
        assert_eq!(priority_from_unreachable(14), "low");
    }

    #[test]
    fn short_name_extracts_leaf() {
        assert_eq!(
            short_name("tools::handle_apply_patch"),
            "handle_apply_patch"
        );
        assert_eq!(short_name("main"), "main");
    }
}

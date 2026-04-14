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
use std::path::Path;

use anyhow::Result;

use crate::constants::ISSUES_FILE;
use crate::issues::{is_closed, rescore_all, Issue, IssuesFile};
use crate::semantic::SemanticIndex;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run all four refactor analyses for every available crate and append new issues
/// to ISSUES.json.  Returns the total number of new issues created.
pub fn generate_all_refactor_issues(workspace: &Path) -> Result<usize> {
    let crates = SemanticIndex::available_crates(workspace);
    if crates.is_empty() {
        return Ok(0);
    }

    let issues_path = workspace.join(ISSUES_FILE);
    let raw = std::fs::read_to_string(&issues_path).unwrap_or_default();
    let mut file: IssuesFile = if raw.trim().is_empty() {
        IssuesFile::default()
    } else {
        serde_json::from_str(&raw).unwrap_or_default()
    };

    let existing_ids: HashSet<String> = file.issues.iter().map(|i| i.id.clone()).collect();
    let open_locations: HashSet<String> = file
        .issues
        .iter()
        .filter(|i| !is_closed(i))
        .map(|i| i.location.clone())
        .collect();

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

    if created > 0 {
        rescore_all(&mut file);
        std::fs::write(&issues_path, serde_json::to_string_pretty(&file)?)?;
    }

    Ok(created)
}

// ---------------------------------------------------------------------------
// 1. Dead code  —  U(D) = 0
// ---------------------------------------------------------------------------

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
        if s.kind != "fn" {
            continue;
        }
        if s.mir_blocks.unwrap_or(0) == 0 {
            continue;
        }
        // Both call-graph in-degree AND HIR reference count must be zero.
        if s.call_in > 0 || s.ref_count > 0 {
            continue;
        }
        // Skip well-known unreferenced-by-design names.
        if is_exempt_from_dead_code(&s.symbol) {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        if open_locations.iter().any(|l| l.contains(&location)) {
            continue;
        }
        let id = dead_code_id(crate_name, &s.symbol);
        if existing_ids.contains(&id) {
            continue;
        }
        let short = short_name(&s.symbol);
        file.issues.push(Issue {
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
                b = s.mir_blocks.unwrap_or(0),
            ),
            location: location.clone(),
            evidence: vec![
                format!("call_in=0 ref_count=0 mir_blocks={}", s.mir_blocks.unwrap_or(0)),
                format!("location: {location}"),
            ],
            discovered_by: "refactor_analyzer".to_string(),
            score: 0.0,
        });
        created += 1;
    }
    created
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
        if s.kind != "fn" || s.mir_blocks.unwrap_or(0) < 2 {
            continue;
        }
        // Resolve the canonical symbol key in the graph for CFG queries.
        let sym_key = match idx.canonical_symbol_key(&s.symbol) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let unreachable = idx.unreachable_block_count(&sym_key);
        if unreachable == 0 {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        if open_locations.iter().any(|l| l.contains(&location)) {
            continue;
        }
        let id = branch_reduce_id(crate_name, &s.symbol);
        if existing_ids.contains(&id) {
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
        });
        created += 1;
    }
    created
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
        });
        created += 1;
    }
    created
}

// ---------------------------------------------------------------------------
// 4. Call chain simplification  —  pass-through wrappers
// ---------------------------------------------------------------------------

fn call_chain_issues(
    file: &mut IssuesFile,
    summaries: &[crate::semantic::SymbolSummary],
    crate_name: &str,
    existing_ids: &HashSet<String>,
    open_locations: &HashSet<String>,
) -> usize {
    let mut created = 0;
    for s in summaries {
        if s.kind != "fn" {
            continue;
        }
        // Pass-through: single callee, few or one caller, tiny body.
        if s.call_out != 1 {
            continue;
        }
        if s.call_in == 0 {
            continue; // already caught by dead-code analysis
        }
        let blocks = s.mir_blocks.unwrap_or(0);
        if blocks > 3 || blocks == 0 {
            continue;
        }
        if is_exempt_from_dead_code(&s.symbol) {
            continue;
        }
        let location = shorten_location(&s.file, s.line);
        if open_locations.iter().any(|l| l.contains(&location)) {
            continue;
        }
        let id = call_chain_id(crate_name, &s.symbol);
        if existing_ids.contains(&id) {
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
        });
        created += 1;
    }
    created
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

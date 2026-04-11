use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

fn take_flag_value(args: &[String], name: &str) -> Option<String> {
    let mut i = 0usize;
    while i + 1 < args.len() {
        if args[i] == name {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn parse_usize_flag(args: &[String], name: &str, default_value: usize) -> Result<usize> {
    match take_flag_value(args, name) {
        None => Ok(default_value),
        Some(v) => v
            .parse::<usize>()
            .with_context(|| format!("{name} must be an integer")) ,
    }
}

fn usage() -> &'static str {
    "canon-tickets: generate refactor/branch-reduction tickets from MIR/HIR-derived graph.json\n\
\n\
Usage:\n\
  canon-tickets --workspace <path> [--crate <name> | --all-crates] [--issues <path>]\n\
               [--limit <n>] [--min-blocks <n>] [--min-stmts <n>] [--top <n>]\n\
               [--prune] [--dry-run] [--print]\n\
\n\
Defaults:\n\
  --crate canon_mini_agent\n\
  --issues <workspace>/ISSUES.json\n\
  --limit 20\n\
  --min-blocks 50\n\
  --min-stmts 200\n\
  --top 3\n"
}

fn stable_id(prefix: &str, crate_name: &str, symbol: &str) -> String {
    // Stable across rebuilds: hash only the crate + symbol + ticket family.
    let mut h: u64 = 1469598103934665603;
    for b in prefix.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    for b in crate_name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    for b in symbol.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{prefix}_{crate_name}_{:016x}", h)
}

fn load_issues(path: &Path) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.trim().is_empty() {
        return json!({ "version": 0u64, "issues": [] });
    }
    serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "version": 0u64, "issues": [] }))
}

fn ensure_shape(mut root: Value) -> Value {
    if !root.is_object() {
        return json!({ "version": 0u64, "issues": [] });
    }
    let obj = root.as_object_mut().unwrap();
    if !obj.contains_key("version") {
        obj.insert("version".to_string(), json!(0u64));
    }
    if !obj.contains_key("issues") || !obj.get("issues").map(|v| v.is_array()).unwrap_or(false) {
        obj.insert("issues".to_string(), json!([]));
    }
    root
}

fn sort_and_truncate_issues(
    mut new_issues: Vec<Value>,
    score_by_id: &std::collections::HashMap<String, (u32, i64)>,
    top: usize,
) -> Vec<Value> {
    new_issues.sort_by(|a, b| {
        let ida = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let idb = b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let (pra, sa) = score_by_id.get(&ida).cloned().unwrap_or((9, 0));
        let (prb, sb) = score_by_id.get(&idb).cloned().unwrap_or((9, 0));
        pra.cmp(&prb).then_with(|| sb.cmp(&sa)).then_with(|| ida.cmp(&idb))
    });
    if top > 0 && new_issues.len() > top {
        new_issues.truncate(top);
    }
    new_issues
}

fn build_issues_for_symbols(
    crate_name: &str,
    symbols: Vec<&canon_mini_agent::SymbolSummary>,
    limit: usize,
    kind: &str,
    new_issues: &mut Vec<Value>,
    issue_scores: &mut Vec<(String, u32, i64)>,
    added_here: &mut usize,
) {
    for s in symbols.into_iter().take(limit) {
        let (id, title, description, priority_rank, score) = if kind == "branch" {
            (
                stable_id("auto_branch_reduce", crate_name, &s.symbol),
                format!("Reduce branch complexity: {}", s.symbol),
                format!(
                    "MIR suggests high branch complexity (blocks={:?}, stmts={:?}).\n\
\n\
Goal: reduce MIR basic blocks while preserving behavior.\n\
Suggested first actions:\n\
  - {{\"action\":\"symbol_window\",\"crate\":\"{crate_name}\",\"symbol\":\"{}\"}}\n\
  - {{\"action\":\"rustc_mir\",\"crate\":\"{crate_name}\",\"mode\":\"mir\",\"extra\":\"{}\"}}",
                    s.mir_blocks,
                    s.mir_stmts,
                    s.symbol,
                    s.symbol
                ),
                1u32,
                s.mir_blocks.unwrap_or(0) as i64,
            )
        } else {
            (
                stable_id("auto_refactor_split", crate_name, &s.symbol),
                format!("Mechanical refactor: split/DRY {}", s.symbol),
                format!(
                    "MIR suggests a large function (blocks={:?}, stmts={:?}).\n\
\n\
Goal: split into helpers / reduce duplication without changing behavior.\n\
Suggested first actions:\n\
  - {{\"action\":\"symbol_window\",\"crate\":\"{crate_name}\",\"symbol\":\"{}\"}}\n\
  - {{\"action\":\"cargo_test\",\"intent\":\"Run focused tests after refactor.\"}}",
                    s.mir_blocks,
                    s.mir_stmts,
                    s.symbol
                ),
                2u32,
                s.mir_stmts.unwrap_or(0) as i64,
            )
        };

        let evidence = vec![
            format!("crate={crate_name}"),
            format!("file={} line={}", s.file, s.line),
            format!(
                "mir: fingerprint={:?} blocks={:?} stmts={:?}",
                s.mir_fingerprint, s.mir_blocks, s.mir_stmts
            ),
            format!("call_graph: callers={} callees={}", s.call_in, s.call_out),
        ];

        new_issues.push(json!({
            "id": id,
            "title": title,
            "status": "open",
            "priority": if priority_rank == 1 { "medium" } else { "low" },
            "kind": "performance",
            "description": description,
            "location": format!("{}:{}", s.file, s.line),
            "evidence": evidence,
            "discovered_by": "tickets",
        }));

        issue_scores.push((id.clone(), priority_rank, score));
        *added_here += 1;
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        eprint!("{}", usage());
        return Ok(());
    }

    let TicketArgs {
        workspace,
        all_crates,
        crate_name,
        limit,
        min_blocks,
        min_stmts,
        top,
        prune,
        dry_run,
        print,
        issues_path,
    } = parse_ticket_args(&args)?;

    canon_mini_agent::set_workspace(workspace.clone());
    canon_mini_agent::logging::init_log_paths("tickets");

    let mut new_issues: Vec<Value> = Vec::new();
    let mut issue_scores: Vec<(String, u32, i64)> = Vec::new(); // (id, priority_rank, score)

    let mut per_crate_added: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_crate_candidates: BTreeMap<String, Value> = BTreeMap::new();

    let crate_list = if all_crates {
        canon_mini_agent::SemanticIndex::available_crates(&PathBuf::from(&workspace))
            .into_iter()
            .filter(|name| !name.starts_with("build_script_"))
            .collect()
    } else {
        vec![crate_name.clone()]
    };

    for crate_name in crate_list {
        match collect_issues_for_crate(
            &workspace,
            &crate_name,
            limit,
            min_blocks,
            min_stmts,
            &mut new_issues,
            &mut issue_scores,
        ) {
            Ok((added_here, candidate_summary)) => {
                per_crate_candidates.insert(crate_name.clone(), candidate_summary);
                per_crate_added.insert(crate_name, added_here);
            }
            Err(err) => {
                per_crate_candidates.insert(crate_name.clone(), json!({"error": format!("{err:#}")}));
            }
        }
    }

    // Load, merge, write.
    let mut root = ensure_shape(load_issues(&issues_path));
    let obj = root.as_object_mut().unwrap();
    let issues = obj.get_mut("issues").unwrap().as_array_mut().unwrap();

    // Only apply the top N issues globally after sorting by priority then score.
    // Priority rank: 0=high,1=medium,2=low. (We currently emit medium+low only.)
    let mut score_by_id = std::collections::HashMap::<String, (u32, i64)>::new();
    for (id, pr, score) in issue_scores {
        score_by_id.insert(id, (pr, score));
    }
    new_issues = sort_and_truncate_issues(new_issues, &score_by_id, top);

    let MergeAppliedIssues {
        added_ids,
        updated_ids,
        applied_issue_objects,
    } = merge_generated_issues(issues, new_issues);

    // Optional cleanup: remove previous auto-generated tickets from this generator that are not
    // part of the currently-selected top set.
    let pruned = prune_stale_generated_issues(issues, &applied_issue_objects, prune);

    // Keep stable-ish ordering: priority, then id. Use a BTreeMap for ranking.
    sort_issues_by_priority(issues);

    let summary = json!({
        "ok": true,
        "issues_path": issues_path.to_string_lossy(),
        "added": added_ids.len(),
        "added_ids": added_ids,
        "updated": updated_ids.len(),
        "updated_ids": updated_ids,
        "total": issues.len(),
        "all_crates": all_crates,
        "per_crate_candidates": per_crate_candidates,
        "per_crate_generated": per_crate_added,
        "dry_run": dry_run,
        "print": print,
        "top": top,
        "prune": prune,
        "pruned": pruned
    });
    emit_or_write_ticket_summary(
        &issues_path,
        &root,
        &summary,
        &applied_issue_objects,
        dry_run,
        print,
    )?;
    Ok(())
}

struct TicketArgs {
    workspace: String,
    all_crates: bool,
    crate_name: String,
    limit: usize,
    min_blocks: usize,
    min_stmts: usize,
    top: usize,
    prune: bool,
    dry_run: bool,
    print: bool,
    issues_path: PathBuf,
}

fn parse_ticket_args(args: &[String]) -> Result<TicketArgs> {
    let workspace = take_flag_value(args, "--workspace").context("missing --workspace")?;
    let all_crates = has_flag(args, "--all-crates");
    let crate_name = take_flag_value(args, "--crate").unwrap_or_else(|| "canon_mini_agent".to_string());
    let limit = parse_usize_flag(args, "--limit", 20)?;
    let min_blocks = parse_usize_flag(args, "--min-blocks", 50)?;
    let min_stmts = parse_usize_flag(args, "--min-stmts", 200)?;
    let top = parse_usize_flag(args, "--top", 3)?;
    let prune = has_flag(args, "--prune");
    let dry_run = has_flag(args, "--dry-run");
    let print = has_flag(args, "--print");
    let issues_path = take_flag_value(args, "--issues")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&workspace).join("ISSUES.json"));

    Ok(TicketArgs {
        workspace,
        all_crates,
        crate_name,
        limit,
        min_blocks,
        min_stmts,
        top,
        prune,
        dry_run,
        print,
        issues_path,
    })
}

fn collect_issues_for_crate(
    workspace: &str,
    crate_name: &str,
    limit: usize,
    min_blocks: usize,
    min_stmts: usize,
    new_issues: &mut Vec<Value>,
    issue_scores: &mut Vec<(String, u32, i64)>,
) -> Result<(usize, Value)> {
    let idx = canon_mini_agent::SemanticIndex::load(&PathBuf::from(workspace), crate_name)?;
    let summaries = idx.symbol_summaries();

    let mut branch_candidates = summaries
        .iter()
        .filter(|s| s.kind == "fn")
        .filter(|s| s.mir_blocks.unwrap_or(0) >= min_blocks)
        .collect::<Vec<_>>();
    branch_candidates.sort_by(|a, b| b.mir_blocks.unwrap_or(0).cmp(&a.mir_blocks.unwrap_or(0)));

    let mut refactor_candidates = summaries
        .iter()
        .filter(|s| s.kind == "fn")
        .filter(|s| s.mir_stmts.unwrap_or(0) >= min_stmts)
        .collect::<Vec<_>>();
    refactor_candidates.sort_by(|a, b| b.mir_stmts.unwrap_or(0).cmp(&a.mir_stmts.unwrap_or(0)));

    let candidate_summary = json!({
        "branch_candidates": branch_candidates.len(),
        "refactor_candidates": refactor_candidates.len(),
        "symbols_with_defs": summaries.len(),
    });

    let mut added_here = 0usize;
    build_issues_for_symbols(
        crate_name,
        branch_candidates,
        limit,
        "branch",
        new_issues,
        issue_scores,
        &mut added_here,
    );
    build_issues_for_symbols(
        crate_name,
        refactor_candidates,
        limit,
        "refactor",
        new_issues,
        issue_scores,
        &mut added_here,
    );

    Ok((added_here, candidate_summary))
}

struct MergeAppliedIssues {
    added_ids: Vec<String>,
    updated_ids: Vec<String>,
    applied_issue_objects: Vec<Value>,
}

fn merge_generated_issues(issues: &mut Vec<Value>, new_issues: Vec<Value>) -> MergeAppliedIssues {
    let mut index_by_id = std::collections::HashMap::<String, usize>::new();
    for (idx, it) in issues.iter().enumerate() {
        if let Some(id) = it.get("id").and_then(|v| v.as_str()) {
            index_by_id.insert(id.to_string(), idx);
        }
    }

    let mut added_ids = Vec::new();
    let mut updated_ids = Vec::new();
    let mut applied_issue_objects = Vec::new();
    for issue in new_issues {
        let Some(id) = issue.get("id").and_then(|v| v.as_str()) else { continue };
        if let Some(&pos) = index_by_id.get(id) {
            issues[pos] = issue.clone();
            updated_ids.push(id.to_string());
        } else {
            index_by_id.insert(id.to_string(), issues.len());
            added_ids.push(id.to_string());
            issues.push(issue.clone());
        }
        applied_issue_objects.push(issue);
    }

    MergeAppliedIssues {
        added_ids,
        updated_ids,
        applied_issue_objects,
    }
}

fn prune_stale_generated_issues(
    issues: &mut Vec<Value>,
    applied_issue_objects: &[Value],
    prune: bool,
) -> usize {
    if !prune {
        return 0;
    }

    let keep_ids: std::collections::HashSet<String> = applied_issue_objects
        .iter()
        .filter_map(|it| it.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    let mut pruned = 0usize;
    issues.retain(|it| {
        let Some(id) = it.get("id").and_then(|v| v.as_str()) else { return true };
        let discovered_by = it
            .get("discovered_by")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_auto = id.starts_with("auto_branch_reduce_") || id.starts_with("auto_refactor_split_");
        let is_from_this_generator = discovered_by == "tickets";
        if is_auto && is_from_this_generator && !keep_ids.contains(id) {
            pruned += 1;
            return false;
        }
        true
    });
    pruned
}

fn sort_issues_by_priority(issues: &mut [Value]) {
    let rank: BTreeMap<&str, u32> = [("high", 0u32), ("medium", 1u32), ("low", 2u32)]
        .into_iter()
        .collect();
    issues.sort_by(|a, b| {
        let pa = a
            .get("priority")
            .and_then(|v| v.as_str())
            .unwrap_or("low")
            .to_lowercase();
        let pb = b
            .get("priority")
            .and_then(|v| v.as_str())
            .unwrap_or("low")
            .to_lowercase();
        let ra = *rank.get(pa.as_str()).unwrap_or(&9);
        let rb = *rank.get(pb.as_str()).unwrap_or(&9);
        ra.cmp(&rb).then_with(|| {
            let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            ia.cmp(ib)
        })
    });
}

fn emit_generated_issues_payload(applied_issue_objects: &[Value]) -> Value {
    json!({ "generated_issues": applied_issue_objects })
}

fn emit_or_write_ticket_summary(
    issues_path: &Path,
    root: &Value,
    summary: &Value,
    applied_issue_objects: &[Value],
    dry_run: bool,
    print: bool,
) -> Result<()> {
    if dry_run {
        println!("{}", serde_json::to_string_pretty(summary)?);
        if print {
            // Keep stdout streamable: summary line, then the exact issue objects that would be appended.
            let payload = emit_generated_issues_payload(applied_issue_objects);
            println!("{}", serde_json::to_string_pretty(&payload)?);
        }
        return Ok(());
    }

    if let Some(parent) = issues_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(root)?;
    let mut file = std::fs::File::create(issues_path)
        .with_context(|| format!("write {}", issues_path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("write {}", issues_path.display()))?;

    println!("{}", serde_json::to_string_pretty(summary)?);
    if print {
        let payload = emit_generated_issues_payload(applied_issue_objects);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }
    Ok(())
}

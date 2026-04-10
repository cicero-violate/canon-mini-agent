use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
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
               [--limit <n>] [--min-blocks <n>] [--min-stmts <n>] [--dry-run]\n\
\n\
Defaults:\n\
  --crate canon_mini_agent\n\
  --issues <workspace>/ISSUES.json\n\
  --limit 20\n\
  --min-blocks 50\n\
  --min-stmts 200\n"
}

fn stable_id(prefix: &str, fingerprint: Option<&str>, symbol: &str) -> String {
    if let Some(fp) = fingerprint {
        let short = fp.chars().take(16).collect::<String>();
        return format!("{prefix}_{short}");
    }
    // Fallback: simple deterministic hash for the symbol string.
    let mut h: u64 = 1469598103934665603;
    for b in symbol.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{prefix}_{:016x}", h)
}

fn stable_id_scoped(prefix: &str, crate_name: &str, fingerprint: Option<&str>, symbol: &str) -> String {
    // Use the same scheme but namespace by crate to avoid cross-crate collisions when scanning.
    if let Some(fp) = fingerprint {
        let short = fp.chars().take(16).collect::<String>();
        return format!("{prefix}_{crate_name}_{short}");
    }
    let mut h: u64 = 1469598103934665603;
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        eprint!("{}", usage());
        return Ok(());
    }

    let workspace = take_flag_value(&args, "--workspace").context("missing --workspace")?;
    let all_crates = has_flag(&args, "--all-crates");
    let crate_name = take_flag_value(&args, "--crate").unwrap_or_else(|| "canon_mini_agent".to_string());
    let limit = parse_usize_flag(&args, "--limit", 20)?;
    let min_blocks = parse_usize_flag(&args, "--min-blocks", 50)?;
    let min_stmts = parse_usize_flag(&args, "--min-stmts", 200)?;
    let dry_run = has_flag(&args, "--dry-run");

    let issues_path = take_flag_value(&args, "--issues")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&workspace).join("ISSUES.json"));

    canon_mini_agent::set_workspace(workspace.clone());
    canon_mini_agent::logging::init_log_paths("tickets");

    let mut new_issues: Vec<Value> = Vec::new();

    let mut per_crate_added: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_crate_candidates: BTreeMap<String, Value> = BTreeMap::new();

    let crate_list = if all_crates {
        canon_mini_agent::SemanticIndex::available_crates(&PathBuf::from(&workspace))
    } else {
        vec![crate_name.clone()]
    };

    for crate_name in crate_list {
        let idx = match canon_mini_agent::SemanticIndex::load(&PathBuf::from(&workspace), &crate_name) {
            Ok(idx) => idx,
            Err(err) => {
                per_crate_candidates.insert(crate_name.clone(), json!({"error": format!("{err:#}")}));
                continue;
            }
        };
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

        per_crate_candidates.insert(
            crate_name.clone(),
            json!({
                "branch_candidates": branch_candidates.len(),
                "refactor_candidates": refactor_candidates.len(),
                "symbols_with_defs": summaries.len(),
            }),
        );

        let mut added_here = 0usize;

        for s in branch_candidates.into_iter().take(limit) {
            let id = if all_crates {
                stable_id_scoped("auto_branch_reduce", &crate_name, s.mir_fingerprint.as_deref(), &s.symbol)
            } else {
                stable_id("auto_branch_reduce", s.mir_fingerprint.as_deref(), &s.symbol)
            };
            let title = format!("Reduce branch complexity: {}", s.symbol);
            let description = format!(
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
            );
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
                "priority": "medium",
                "kind": "performance",
                "description": description,
                "location": format!("{}:{}", s.file, s.line),
                "evidence": evidence,
                "discovered_by": "tickets",
            }));
            added_here += 1;
        }

        for s in refactor_candidates.into_iter().take(limit) {
            let id = if all_crates {
                stable_id_scoped("auto_refactor_split", &crate_name, s.mir_fingerprint.as_deref(), &s.symbol)
            } else {
                stable_id("auto_refactor_split", s.mir_fingerprint.as_deref(), &s.symbol)
            };
            let title = format!("Mechanical refactor: split/DRY {}", s.symbol);
            let description = format!(
                "MIR suggests a large function (blocks={:?}, stmts={:?}).\n\
\n\
Goal: split into helpers / reduce duplication without changing behavior.\n\
Suggested first actions:\n\
  - {{\"action\":\"symbol_window\",\"crate\":\"{crate_name}\",\"symbol\":\"{}\"}}\n\
  - {{\"action\":\"cargo_test\",\"intent\":\"Run focused tests after refactor.\"}}",
                s.mir_blocks,
                s.mir_stmts,
                s.symbol
            );
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
                "priority": "low",
                "kind": "performance",
                "description": description,
                "location": format!("{}:{}", s.file, s.line),
                "evidence": evidence,
                "discovered_by": "tickets",
            }));
            added_here += 1;
        }

        per_crate_added.insert(crate_name, added_here);
    }

    // Load, merge, write.
    let mut root = ensure_shape(load_issues(&issues_path));
    let obj = root.as_object_mut().unwrap();
    let issues = obj.get_mut("issues").unwrap().as_array_mut().unwrap();

    let mut existing_ids = HashSet::<String>::new();
    for it in issues.iter() {
        if let Some(id) = it.get("id").and_then(|v| v.as_str()) {
            existing_ids.insert(id.to_string());
        }
    }

    let mut added_ids = Vec::new();
    for issue in new_issues {
        let Some(id) = issue.get("id").and_then(|v| v.as_str()) else { continue };
        if existing_ids.insert(id.to_string()) {
            added_ids.push(id.to_string());
            issues.push(issue);
        }
    }

    // Keep stable-ish ordering: priority, then id. Use a BTreeMap for ranking.
    let rank: BTreeMap<&str, u32> = [("high", 0u32), ("medium", 1u32), ("low", 2u32)].into_iter().collect();
    issues.sort_by(|a, b| {
        let pa = a.get("priority").and_then(|v| v.as_str()).unwrap_or("low").to_lowercase();
        let pb = b.get("priority").and_then(|v| v.as_str()).unwrap_or("low").to_lowercase();
        let ra = *rank.get(pa.as_str()).unwrap_or(&9);
        let rb = *rank.get(pb.as_str()).unwrap_or(&9);
        ra.cmp(&rb).then_with(|| {
            let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            ia.cmp(ib)
        })
    });

    let summary = json!({
        "ok": true,
        "issues_path": issues_path.to_string_lossy(),
        "added": added_ids.len(),
        "added_ids": added_ids,
        "total": issues.len(),
        "all_crates": all_crates,
        "per_crate_candidates": per_crate_candidates,
        "per_crate_generated": per_crate_added,
        "dry_run": dry_run
    });
    if dry_run {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    if let Some(parent) = issues_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&root)?;
    let mut file = std::fs::File::create(&issues_path)
        .with_context(|| format!("write {}", issues_path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("write {}", issues_path.display()))?;

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

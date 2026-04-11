use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::semantic::{shorten_display_path, SemanticIndex};

fn reports_dir(workspace: &Path) -> PathBuf {
    workspace.join("state").join("reports").join("complexity")
}

fn sort_by_complexity_desc(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    b.get("complexity_proxy")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .cmp(
            &a.get("complexity_proxy")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        )
}

fn process_crate(
    workspace: &Path,
    crate_name: &str,
    global: &mut Vec<serde_json::Value>,
) -> serde_json::Value {
    let idx = match SemanticIndex::load(workspace, crate_name) {
        Ok(idx) => idx,
        Err(err) => {
            return json!({
                "crate": crate_name,
                "status": "error",
                "error": err.to_string(),
            });
        }
    };

    let mut items = collect_complexity_items(&idx, crate_name, global);

    items.sort_by(sort_by_complexity_desc);
    let top = items.into_iter().take(50).collect::<Vec<_>>();

    json!({
        "crate": crate_name,
        "status": "ok",
        "metric": "mir_blocks_proxy",
        "top": top,
    })
}

fn collect_complexity_items(
    idx: &crate::semantic::SemanticIndex,
    crate_name: &str,
    global: &mut Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    for s in idx.symbol_summaries() {
        let blocks = s.mir_blocks.unwrap_or(0);
        let stmts = s.mir_stmts.unwrap_or(0);
        if blocks == 0 && stmts == 0 {
            continue;
        }
        let entry = build_complexity_entry(&s, blocks, stmts);
        global.push(build_global_complexity_entry(crate_name, &entry));
        items.push(entry);
    }
    items
}

fn build_global_complexity_entry(
    crate_name: &str,
    entry: &serde_json::Value,
) -> serde_json::Value {
    json!({
        "crate": crate_name,
        "symbol": entry.get("symbol"),
        "file": entry.get("file"),
        "line": entry.get("line"),
        "complexity_proxy": entry.get("complexity_proxy"),
        "mir_blocks": entry.get("mir_blocks"),
        "mir_stmts": entry.get("mir_stmts"),
    })
}

fn build_complexity_entry(
    s: &crate::semantic::SymbolSummary,
    blocks: usize,
    stmts: usize,
) -> serde_json::Value {
    json!({
        "symbol": s.symbol,
        "file": shorten_display_path(&s.file),
        "line": s.line,
        "mir_blocks": blocks,
        "mir_stmts": stmts,
        "complexity_proxy": blocks,
    })
}

/// Emit a cyclomatic-complexity-style report on startup/restart.
///
/// Current implementation is a proxy based on MIR metadata already captured in
/// `state/rustc/<crate>/graph.json`:
/// - `complexity_proxy = mir_blocks` (higher tends to correlate with more branching).
///
/// This is intentionally cheap and deterministic; it can be upgraded later to true cyclomatic
/// complexity when canon-rustc-v2 records per-item CFG nodes/edges.
pub fn write_complexity_report(workspace: &Path) -> Result<Option<PathBuf>> {
    let crates = SemanticIndex::available_crates(workspace);
    if crates.is_empty() {
        return Ok(None);
    }

    let mut per_crate = Vec::new();
    let mut global = Vec::new();

    for crate_name in crates {
        let entry = process_crate(workspace, &crate_name, &mut global);
        per_crate.push(entry);
    }

    global.sort_by(sort_by_complexity_desc);
    let global_top = global.into_iter().take(100).collect::<Vec<_>>();

    let report = json!({
        "version": 1,
        "metric": "mir_blocks_proxy",
        "generated_at_ms": crate::logging::now_ms(),
        "global_top": global_top,
        "per_crate": per_crate,
        "note": "Proxy report: complexity_proxy=mir_blocks. Upgrade canon-rustc-v2 to record true CFG-based cyclomatic complexity for accuracy.",
    });

    let dir = reports_dir(workspace);
    let latest = persist_complexity_report(&dir, &report)?;

    Ok(Some(latest))
}

fn persist_complexity_report(dir: &Path, report: &serde_json::Value) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let body = serde_json::to_string_pretty(report)?;
    let ts = crate::logging::now_ms();
    let path = dir.join(format!("{ts}.json"));
    std::fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    let latest = dir.join("latest.json");
    std::fs::write(&latest, body).with_context(|| format!("write {}", latest.display()))?;
    Ok(latest)
}

use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct CrateGraph {
    nodes: HashMap<String, GraphNode>,
    edges: Vec<GraphEdge>,
}

#[derive(Debug, Deserialize)]
struct GraphNode {
    kind: String,
    #[serde(default)]
    def: Option<SourceSpan>,
    #[serde(default)]
    mir: Option<MirInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceSpan {
    file: String,
    line: u32,
    col: u32,
    lo: u32,
    hi: u32,
}

#[derive(Debug, Deserialize)]
struct MirInfo {
    fingerprint: String,
    blocks: usize,
    stmts: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphEdge {
    kind: String,
    from: String,
    to: String,
}

fn stable_id(prefix: &str, crate_name: &str, symbol: &str) -> String {
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

fn priority_rank(p: &str) -> u32 {
    match p.trim().to_lowercase().as_str() {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ISSUES.json");
    println!("cargo:rerun-if-changed=state/rustc/index.json");
    println!("cargo:rerun-if-changed=state/rustc/canon_mini_agent/graph.json");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let root = PathBuf::from(manifest_dir);
    let graph_path = root.join("state/rustc/canon_mini_agent/graph.json");
    let issues_path = root.join("ISSUES.json");

    let Ok(graph_bytes) = fs::read(&graph_path) else {
        // No semantic capture yet (first build) or wrapper disabled; skip.
        return;
    };
    let Ok(graph) = serde_json::from_slice::<CrateGraph>(&graph_bytes) else {
        println!(
            "cargo:warning=canon-mini-agent build.rs: failed to parse {}",
            graph_path.display()
        );
        return;
    };

    // Compute basic call graph counts.
    let mut call_in: HashMap<&str, usize> = HashMap::new();
    let mut call_out: HashMap<&str, usize> = HashMap::new();
    for edge in &graph.edges {
        if edge.kind != "call" {
            continue;
        }
        *call_out.entry(edge.from.as_str()).or_insert(0) += 1;
        *call_in.entry(edge.to.as_str()).or_insert(0) += 1;
    }

    // Top 3 refactor tickets: highest MIR stmt count among functions with defs.
    let mut candidates: Vec<(&str, &GraphNode, &SourceSpan, &MirInfo)> = Vec::new();
    for (symbol, node) in &graph.nodes {
        if node.kind != "fn" {
            continue;
        }
        let Some(def) = node.def.as_ref() else { continue };
        let Some(mir) = node.mir.as_ref() else { continue };
        candidates.push((symbol.as_str(), node, def, mir));
    }
    candidates.sort_by(|a, b| {
        b.3.stmts
            .cmp(&a.3.stmts)
            .then_with(|| b.3.blocks.cmp(&a.3.blocks))
            .then_with(|| a.0.cmp(b.0))
    });
    candidates.truncate(3);

    let mut generated = Vec::new();
    for (symbol, _node, def, mir) in candidates {
        let id = stable_id("auto_refactor_split", "canon_mini_agent", symbol);
        let title = format!("Mechanical refactor: split/DRY {symbol}");
        let description = format!(
            "MIR suggests a large function (blocks={}, stmts={}).\n\n\
Goal: split into helpers / reduce duplication without changing behavior.\n\
Suggested first actions:\n\
- {{\"action\":\"symbol_window\",\"crate\":\"canon_mini_agent\",\"symbol\":\"{symbol}\"}}\n\
- {{\"action\":\"cargo_test\",\"intent\":\"Run focused tests after refactor.\"}}",
            mir.blocks, mir.stmts
        );
        let evidence = vec![
            "crate=canon_mini_agent".to_string(),
            format!("file={} line={}", def.file, def.line),
            format!(
                "mir: fingerprint={} blocks={} stmts={}",
                mir.fingerprint, mir.blocks, mir.stmts
            ),
            format!(
                "call_graph: callers={} callees={}",
                call_in.get(symbol).copied().unwrap_or(0),
                call_out.get(symbol).copied().unwrap_or(0)
            ),
        ];

        generated.push(json!({
            "id": id,
            "title": title,
            "status": "open",
            "priority": "low",
            "kind": "performance",
            "description": description,
            "location": format!("{}:{}", def.file, def.line),
            "evidence": evidence,
            "discovered_by": "build",
        }));
    }

    if generated.is_empty() {
        return;
    }

    let raw = fs::read_to_string(&issues_path).unwrap_or_default();
    let mut root: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    obj.entry("version".to_string()).or_insert(json!(0u64));
    let issues = obj
        .entry("issues".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut();
    let Some(issues) = issues else { return };

    // Index existing issues by id; update in place or append.
    let mut idx_by_id: HashMap<String, usize> = HashMap::new();
    for (idx, it) in issues.iter().enumerate() {
        if let Some(id) = it.get("id").and_then(|v| v.as_str()) {
            idx_by_id.insert(id.to_string(), idx);
        }
    }
    let mut changed = false;
    for issue in generated {
        let Some(id) = issue.get("id").and_then(|v| v.as_str()) else { continue };
        if let Some(&pos) = idx_by_id.get(id) {
            if issues[pos] != issue {
                issues[pos] = issue;
                changed = true;
            }
        } else {
            idx_by_id.insert(id.to_string(), issues.len());
            issues.push(issue);
            changed = true;
        }
    }

    if !changed {
        return;
    }

    // Keep stable ordering: priority then id.
    issues.sort_by(|a, b| {
        let pa = a
            .get("priority")
            .and_then(|v| v.as_str())
            .unwrap_or("low");
        let pb = b
            .get("priority")
            .and_then(|v| v.as_str())
            .unwrap_or("low");
        let ra = priority_rank(pa);
        let rb = priority_rank(pb);
        ra.cmp(&rb).then_with(|| {
            let ia = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let ib = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
            ia.cmp(ib)
        })
    });

    // Only write when we actually changed content.
    if let Ok(text) = serde_json::to_string_pretty(&root) {
        let _ = fs::write(&issues_path, text);
    }
}


//! Semantic navigation over the rustc-compiled CrateGraph.
//!
//! Loads `state/rustc/<crate_name>/graph.json` and exposes five query methods:
//!
//!   semantic_map        — repomap-style symbol outline for the whole crate
//!   symbol_window       — precise source extraction for a single symbol (def span)
//!   symbol_refs         — all reference sites for a symbol
//!   symbol_path         — call-graph BFS path between two symbols
//!   symbol_neighborhood — immediate callers + callees of a symbol

use anyhow::{bail, Context, Result};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxKind};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Graph types (mirrors canon-rustc-v2/src/graph.rs — no crate dep needed)
// ---------------------------------------------------------------------------

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
    refs: Vec<SourceSpan>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    mir: Option<MirInfo>,
    #[serde(default)]
    fields: Vec<String>,
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

// ---------------------------------------------------------------------------
// SemanticIndex
// ---------------------------------------------------------------------------

pub struct SemanticIndex {
    graph: CrateGraph,
}

#[derive(Debug, Clone)]
pub struct SymbolSummary {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub signature: Option<String>,
    pub mir_fingerprint: Option<String>,
    pub mir_blocks: Option<usize>,
    pub mir_stmts: Option<usize>,
    pub call_in: usize,
    pub call_out: usize,
}

impl SemanticIndex {
    /// Load the graph for `crate_name` from the standard artifact location.
    pub fn load(workspace: &Path, crate_name: &str) -> Result<Self> {
        // Normalize: hyphens → underscores (cargo convention).
        let name = crate_name.replace('-', "_");
        let graph_path = workspace
            .join("state/rustc")
            .join(&name)
            .join("graph.json");
        let bytes = fs::read(&graph_path)
            .with_context(|| format!("graph not found at {}", graph_path.display()))?;
        let graph: CrateGraph = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse graph at {}", graph_path.display()))?;
        Ok(Self { graph })
    }

    /// Discover available crates from state/rustc/index.json.
    pub fn available_crates(workspace: &Path) -> Vec<String> {
        let index_path = workspace.join("state/rustc/index.json");
        let Ok(bytes) = fs::read(&index_path) else { return Vec::new() };
        let Ok(index) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new();
        };
        index
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Extract a stable summary for each symbol with a definition span.
    pub fn symbol_summaries(&self) -> Vec<SymbolSummary> {
        let mut call_in: HashMap<&str, usize> = HashMap::new();
        let mut call_out: HashMap<&str, usize> = HashMap::new();
        for edge in &self.graph.edges {
            if edge.kind != "call" {
                continue;
            }
            *call_out.entry(edge.from.as_str()).or_insert(0) += 1;
            *call_in.entry(edge.to.as_str()).or_insert(0) += 1;
        }

        let mut out = Vec::new();
        for (symbol, node) in &self.graph.nodes {
            if node.kind == "unknown" {
                continue;
            }
            let Some(def) = &node.def else { continue };
            let (mir_fingerprint, mir_blocks, mir_stmts) = match &node.mir {
                Some(m) => (Some(m.fingerprint.clone()), Some(m.blocks), Some(m.stmts)),
                None => (None, None, None),
            };
            out.push(SymbolSummary {
                symbol: symbol.clone(),
                kind: node.kind.clone(),
                file: def.file.clone(),
                line: def.line,
                signature: node.signature.clone(),
                mir_fingerprint,
                mir_blocks,
                mir_stmts,
                call_in: *call_in.get(symbol.as_str()).unwrap_or(&0),
                call_out: *call_out.get(symbol.as_str()).unwrap_or(&0),
            });
        }

        out.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)).then(a.symbol.cmp(&b.symbol)));
        out
    }

    // -----------------------------------------------------------------------
    // semantic_map
    // -----------------------------------------------------------------------

    /// Repomap-style outline: one line per symbol sorted by file + line.
    /// Format: `<file>:<line> <kind> <path> [sig] [fields: f1, f2]`
    pub fn semantic_map(&self, filter_path: Option<&str>) -> String {
        // Group by file, then sort by line.
        let mut by_file: HashMap<String, Vec<(u32, &str, &GraphNode)>> = HashMap::new();
        for (path, node) in &self.graph.nodes {
            // Skip synthetic/unknown items (e.g. {use#0}, {impl#0}).
            if node.kind == "unknown" {
                continue;
            }
            if let Some(fp) = filter_path {
                if !path.starts_with(fp) {
                    continue;
                }
            }
            let Some(def) = &node.def else { continue };
            by_file
                .entry(def.file.clone())
                .or_default()
                .push((def.line, path.as_str(), node));
        }

        let mut files: Vec<String> = by_file.keys().cloned().collect();
        files.sort();

        let mut out = String::new();
        for file in &files {
            let entries = by_file.get_mut(file).unwrap();
            entries.sort_by_key(|(line, _, _)| *line);

            // Use a short relative path for display.
            let display_file = shorten_path(file);
            out.push_str(&display_file);
            out.push('\n');

            for (line, path, node) in entries.iter() {
                let short_name = path.rsplit("::").next().unwrap_or(path);
                let mut entry = format!("  {:>5}  {} {}", line, node.kind, short_name);
                if let Some(sig) = &node.signature {
                    entry.push_str(&format!("  {sig}"));
                }
                if !node.fields.is_empty() {
                    entry.push_str(&format!("  {{ {} }}", node.fields.join(", ")));
                }
                out.push_str(&entry);
                out.push('\n');
            }
        }
        if out.is_empty() {
            "No symbols found.".to_string()
        } else {
            out
        }
    }

    // -----------------------------------------------------------------------
    // symbol_window
    // -----------------------------------------------------------------------

    /// Extract the full definition body of a symbol from source using byte offsets.
    /// Returns the source text with a header showing file:line.
    pub fn symbol_window(&self, symbol: &str) -> Result<String> {
        let node = self.find_node(symbol)?;
        let def = node.def.as_ref().context("symbol has no definition span")?;

        let source = fs::read_to_string(&def.file)
            .with_context(|| format!("could not read source file {}", def.file))?;

        let lo = def.lo as usize;
        let hi = def.hi as usize;
        if hi > source.len() || lo > hi {
            bail!("byte offsets out of range (lo={lo} hi={hi} file_len={})", source.len());
        }

        let (slice_lo, slice_hi) = expand_symbol_window_span(&source, lo, hi).unwrap_or((lo, hi));
        let text = source.get(slice_lo..slice_hi).with_context(|| {
            format!("expanded symbol span is not on UTF-8 boundaries (lo={slice_lo} hi={slice_hi})")
        })?;

        let display = shorten_path(&def.file);
        let mut out = format!("// {} — {}:{}\n", symbol, display, def.line);
        out.push_str(text);
        if !out.ends_with('\n') {
            out.push('\n');
        }

        // Append MIR info as a comment if present.
        if let Some(mir) = &node.mir {
            out.push_str(&format!(
                "// MIR: {} blocks, {} stmts, fingerprint={}\n",
                mir.blocks, mir.stmts, mir.fingerprint
            ));
        }

        Ok(out)
    }

    // -----------------------------------------------------------------------
    // symbol_refs
    // -----------------------------------------------------------------------

    /// All reference sites for `symbol` — file:line:col, one per line.
    pub fn symbol_refs(&self, symbol: &str) -> Result<String> {
        let node = self.find_node(symbol)?;
        if node.refs.is_empty() {
            return Ok(format!("No reference sites recorded for `{symbol}`."));
        }

        let mut spans: Vec<&SourceSpan> = node.refs.iter().collect();
        spans.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line).then(a.col.cmp(&b.col))));
        let mut unique = Vec::new();
        let mut seen = HashSet::<(String, u32, u32)>::new();
        for s in spans {
            let key = (s.file.clone(), s.line, s.col);
            if seen.insert(key) {
                unique.push(s);
            }
        }

        let mut out = format!("References to `{symbol}` ({} sites):\n", unique.len());
        for s in unique {
            out.push_str(&format!(
                "  {}:{}:{}\n",
                shorten_path(&s.file),
                s.line,
                s.col
            ));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // symbol_path
    // -----------------------------------------------------------------------

    /// BFS shortest path in the call graph from `from` to `to`.
    /// Returns the chain with file:line annotations.
    pub fn symbol_path(&self, from: &str, to: &str) -> Result<String> {
        let from_key = self.resolve_symbol_key(from)?;
        let to_key = self.resolve_symbol_key(to)?;
        if from_key == to_key {
            return Ok(format!("`{from}` is the same as `{to}`."));
        }

        // Build adjacency from call edges only.
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &self.graph.edges {
            if edge.kind == "call" {
                adj.entry(&edge.from).or_default().push(&edge.to);
            }
        }

        // BFS.
        let mut visited: HashSet<&str> = HashSet::new();
        let mut prev: HashMap<&str, &str> = HashMap::new();
        let mut queue: VecDeque<&str> = VecDeque::new();

        visited.insert(from_key);
        queue.push_back(from_key);

        'bfs: loop {
            let Some(cur) = queue.pop_front() else { break };
            if let Some(neighbors) = adj.get(cur) {
                for &nb in neighbors {
                    if visited.insert(nb) {
                        prev.insert(nb, cur);
                        if nb == to_key {
                            break 'bfs;
                        }
                        queue.push_back(nb);
                    }
                }
            }
        }

        if !prev.contains_key(to_key) {
            return Ok(format!("No call-graph path found from `{from}` to `{to}`."));
        }

        // Reconstruct path.
        let mut path: Vec<&str> = Vec::new();
        let mut cur = to_key;
        loop {
            path.push(cur);
            if cur == from_key {
                break;
            }
            cur = prev[cur];
        }
        path.reverse();

        let mut out = format!(
            "Call path from `{from}` → `{to}` ({} hops):\n",
            path.len() - 1
        );
        for sym in &path {
            if let Some(node) = self.graph.nodes.get(*sym) {
                if let Some(def) = &node.def {
                    out.push_str(&format!(
                        "  {} ({}:{})\n",
                        sym,
                        shorten_path(&def.file),
                        def.line
                    ));
                    continue;
                }
            }
            out.push_str(&format!("  {sym}\n"));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // symbol_neighborhood
    // -----------------------------------------------------------------------

    /// Immediate callers and callees of `symbol` in the call graph.
    pub fn symbol_neighborhood(&self, symbol: &str) -> Result<String> {
        // Confirm symbol exists and normalize to canonical graph key.
        let symbol_key = self.resolve_symbol_key(symbol)?;
        let node = self.graph.nodes.get(symbol_key).unwrap();

        let mut callers: Vec<&str> = Vec::new();
        let mut callees: Vec<&str> = Vec::new();

        for edge in &self.graph.edges {
            if edge.kind != "call" {
                continue;
            }
            if edge.to == symbol_key {
                callers.push(&edge.from);
            }
            if edge.from == symbol_key {
                callees.push(&edge.to);
            }
        }

        callers.sort();
        callers.dedup();
        callees.sort();
        callees.dedup();

        let mut inferred_callers = self.infer_callers_from_refs(symbol_key, &node.refs);
        inferred_callers.sort();
        inferred_callers.dedup();

        let mut out = format!("Neighborhood of `{symbol}`:\n");

        out.push_str(&format!("  Callers ({}):\n", callers.len()));
        for s in &callers {
            out.push_str(&format!("    {s}\n"));
        }

        if !inferred_callers.is_empty() {
            out.push_str(&format!(
                "  Inferred callers from refs ({}):\n",
                inferred_callers.len()
            ));
            for s in &inferred_callers {
                out.push_str(&format!("    {s}\n"));
            }
        }

        out.push_str(&format!("  Callees ({}):\n", callees.len()));
        for s in &callees {
            out.push_str(&format!("    {s}\n"));
        }

        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn find_node(&self, symbol: &str) -> Result<&GraphNode> {
        let key = self.resolve_symbol_key(symbol)?;
        Ok(self.graph.nodes.get(key).unwrap())
    }

    fn resolve_symbol_key(&self, symbol: &str) -> Result<&str> {
        if let Some((key, _node)) = self.graph.nodes.get_key_value(symbol) {
            return Ok(key.as_str());
        }
        // Fuzzy: try suffix match.
        let suffix = format!("::{symbol}");
        let matches: Vec<&str> = self
            .graph
            .nodes
            .keys()
            .filter(|k| k.as_str() == symbol || k.ends_with(&suffix))
            .map(String::as_str)
            .collect();
        match matches.len() {
            0 => bail!(
                "symbol `{symbol}` not found in graph. Use semantic_map to list available symbols."
            ),
            1 => Ok(matches[0]),
            n => bail!(
                "ambiguous symbol `{symbol}` — {n} matches: {}. Use the fully-qualified path.",
                matches.join(", ")
            ),
        }
    }

    fn infer_callers_from_refs<'a>(&'a self, symbol_key: &str, refs: &'a [SourceSpan]) -> Vec<&'a str> {
        let mut out = Vec::new();
        for span in refs {
            if let Some(owner) = self.enclosing_symbol_for_span(span) {
                if owner != symbol_key {
                    out.push(owner);
                }
            }
        }
        out
    }

    fn enclosing_symbol_for_span<'a>(&'a self, span: &SourceSpan) -> Option<&'a str> {
        let mut best: Option<(&str, u32)> = None;
        for (sym, node) in &self.graph.nodes {
            let Some(def) = node.def.as_ref() else { continue };
            if def.file != span.file {
                continue;
            }
            if def.lo <= span.lo && def.hi >= span.hi {
                let width = def.hi.saturating_sub(def.lo);
                match best {
                    None => best = Some((sym.as_str(), width)),
                    Some((_, best_width)) if width < best_width => {
                        best = Some((sym.as_str(), width))
                    }
                    _ => {}
                }
            }
        }
        best.map(|(sym, _)| sym)
    }
}

fn expand_symbol_window_span(source: &str, lo: usize, hi: usize) -> Option<(usize, usize)> {
    if lo > hi || hi > source.len() {
        return None;
    }
    let parse = SourceFile::parse(source, Edition::CURRENT);
    let root = parse.tree();

    // Avoid relying on `token_at_offset(lo)`, because rustc-provided spans can point at just the
    // identifier, a type in the signature, etc. Scanning for the smallest enclosing "item" node
    // is more reliable across `fn`/`struct`/`trait`/`impl` and friends.
    let mut best: Option<(usize, usize)> = None;
    for node in root.syntax().descendants() {
        if !is_symbol_window_item_kind(node.kind()) {
            continue;
        }
        let range = node.text_range();
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        if start <= lo && end >= hi {
            match best {
                None => best = Some((start, end)),
                Some((best_lo, best_hi)) if (end - start) < (best_hi - best_lo) => {
                    best = Some((start, end))
                }
                _ => {}
            }
        }
    }
    best
}

fn is_symbol_window_item_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FN
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::TRAIT
            | SyntaxKind::IMPL
            | SyntaxKind::MODULE
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::CONST
            | SyntaxKind::STATIC
            | SyntaxKind::UNION
            | SyntaxKind::VARIANT
            | SyntaxKind::RECORD_FIELD
    )
}

fn shorten_path(path: &str) -> String {
    // Strip known workspace prefix for readability.
    const WORKSPACE: &str = "/workspace/ai_sandbox/canon-mini-agent/";
    if let Some(rest) = path.strip_prefix(WORKSPACE) {
        return rest.to_string();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        expand_symbol_window_span, CrateGraph, GraphEdge, GraphNode, SemanticIndex, SourceSpan,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn expand_symbol_window_span_returns_full_function_block() {
        let src = "pub(crate) fn process_action_and_execute(\n    role: &str,\n) -> Result<(bool, String)> {\n    let _x = role;\n    Ok((false, String::new()))\n}\n";
        let lo = src.find("pub(crate) fn").unwrap();
        let hi = src.find(") -> Result<(bool, String)>").unwrap() + ") -> Result<(bool, String)>".len();
        let (slice_lo, slice_hi) = expand_symbol_window_span(src, lo, hi).expect("span should expand");
        let extracted = &src[slice_lo..slice_hi];
        assert!(extracted.starts_with("pub(crate) fn process_action_and_execute("));
        assert!(extracted.contains("Ok((false, String::new()))"));
        assert!(extracted.trim_end().ends_with('}'));
    }

    #[test]
    fn symbol_window_returns_full_item_when_graph_span_is_signature_only() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp_dir = std::env::temp_dir().join(format!("semantic-window-{unique}"));
        fs::create_dir_all(&tmp_dir).unwrap();
        let src_path = tmp_dir.join("engine.rs");

        let src = "pub(crate) fn process_action_and_execute(\n    role: &str,\n) -> Result<(bool, String)> {\n    let _x = role;\n    Ok((false, String::new()))\n}\n";
        fs::write(&src_path, src).unwrap();

        let lo = src.find("pub(crate) fn").unwrap();
        let hi = src.find(") -> Result<(bool, String)>").unwrap() + ") -> Result<(bool, String)>".len();
        let mut nodes = HashMap::new();
        nodes.insert(
            "engine::process_action_and_execute".to_string(),
            GraphNode {
                kind: "fn".to_string(),
                def: Some(SourceSpan {
                    file: src_path.to_string_lossy().to_string(),
                    line: 1,
                    col: 1,
                    lo: lo as u32,
                    hi: hi as u32,
                }),
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );

        let idx = SemanticIndex {
            graph: CrateGraph {
                nodes,
                edges: Vec::new(),
            },
        };
        let out = idx
            .symbol_window("engine::process_action_and_execute")
            .expect("symbol_window should succeed");
        assert!(out.contains("pub(crate) fn process_action_and_execute("));
        assert!(out.contains("Ok((false, String::new()))"));
        assert!(out.contains("}\n"));
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn expand_symbol_window_span_expands_struct_from_identifier_span() {
        let src = "pub struct Widget {\n    pub x: i32,\n}\n";
        let lo = src.find("Widget").unwrap();
        let hi = lo + "Widget".len();
        let (slice_lo, slice_hi) = expand_symbol_window_span(src, lo, hi).expect("span should expand");
        let extracted = &src[slice_lo..slice_hi];
        assert!(extracted.starts_with("pub struct Widget"));
        assert!(extracted.contains("pub x: i32"));
        assert!(extracted.trim_end().ends_with('}'));
    }

    #[test]
    fn expand_symbol_window_span_expands_trait_from_identifier_span() {
        let src = "pub trait Greeter {\n    fn greet(&self);\n}\n";
        let lo = src.find("Greeter").unwrap();
        let hi = lo + "Greeter".len();
        let (slice_lo, slice_hi) = expand_symbol_window_span(src, lo, hi).expect("span should expand");
        let extracted = &src[slice_lo..slice_hi];
        assert!(extracted.starts_with("pub trait Greeter"));
        assert!(extracted.contains("fn greet(&self);"));
        assert!(extracted.trim_end().ends_with('}'));
    }

    #[test]
    fn neighborhood_and_path_resolve_suffix_symbols_to_canonical_keys() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "canon_mini_agent::app::continue_executor_completion".to_string(),
            GraphNode {
                kind: "fn".to_string(),
                def: None,
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        nodes.insert(
            "canon_mini_agent::engine::process_action_and_execute".to_string(),
            GraphNode {
                kind: "fn".to_string(),
                def: None,
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );

        let idx = SemanticIndex {
            graph: CrateGraph {
                nodes,
                edges: vec![GraphEdge {
                    kind: "call".to_string(),
                    from: "canon_mini_agent::app::continue_executor_completion".to_string(),
                    to: "canon_mini_agent::engine::process_action_and_execute".to_string(),
                }],
            },
        };

        let n = idx
            .symbol_neighborhood("engine::process_action_and_execute")
            .expect("neighborhood should resolve suffix symbol");
        assert!(n.contains("Callers (1):"));
        assert!(n.contains("canon_mini_agent::app::continue_executor_completion"));

        let p = idx
            .symbol_path(
                "app::continue_executor_completion",
                "engine::process_action_and_execute",
            )
            .expect("path should resolve suffix symbols");
        assert!(p.contains("1 hops"));
        assert!(p.contains("canon_mini_agent::app::continue_executor_completion"));
        assert!(p.contains("canon_mini_agent::engine::process_action_and_execute"));
    }

    #[test]
    fn neighborhood_infers_callers_from_refs_when_call_edges_absent() {
        let caller_src = "fn drive() {\n    run_planner_phase();\n}\n";
        let target_src = "fn run_planner_phase() {}\n";
        let call_lo = caller_src.find("run_planner_phase").unwrap() as u32;
        let call_hi = call_lo + "run_planner_phase".len() as u32;

        let mut nodes = HashMap::new();
        nodes.insert(
            "app::drive".to_string(),
            GraphNode {
                kind: "fn".to_string(),
                def: Some(SourceSpan {
                    file: "src/app.rs".to_string(),
                    line: 1,
                    col: 1,
                    lo: 0,
                    hi: caller_src.len() as u32,
                }),
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        nodes.insert(
            "app::run_planner_phase".to_string(),
            GraphNode {
                kind: "fn".to_string(),
                def: Some(SourceSpan {
                    file: "src/app.rs".to_string(),
                    line: 10,
                    col: 1,
                    lo: 0,
                    hi: target_src.len() as u32,
                }),
                refs: vec![SourceSpan {
                    file: "src/app.rs".to_string(),
                    line: 2,
                    col: 5,
                    lo: call_lo,
                    hi: call_hi,
                }],
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        let idx = SemanticIndex {
            graph: CrateGraph {
                nodes,
                edges: Vec::new(),
            },
        };
        let out = idx
            .symbol_neighborhood("app::run_planner_phase")
            .expect("neighborhood should succeed");
        assert!(out.contains("Callers (0):"));
        assert!(out.contains("Inferred callers from refs (1):"));
        assert!(out.contains("app::drive"));
    }
}

//! Semantic navigation over the rustc-compiled CrateGraph.
//!
//! Loads `state/rustc/<crate_name>/graph.json` and exposes five query methods:
//!
//!   semantic_map        — repomap-style symbol outline for the whole crate
//!   symbol_window       — precise source extraction for a single symbol (def span)
//!   symbol_refs         — all reference sites for a symbol
//!   symbol_path         — semantic-graph BFS path between two symbols
//!   execution_path      — unified semantic+cfg BFS path between two endpoints
//!   symbol_neighborhood — immediate callers + callees of a symbol

use anyhow::{bail, Context, Result};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Graph types (mirrors canon-rustc-v2/src/graph.rs — no crate dep needed)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CrateGraph {
    #[serde(default)]
    nodes: HashMap<String, GraphNode>,
    #[serde(default)]
    edges: Vec<GraphEdge>,
    #[serde(default)]
    cfg_nodes: HashMap<String, CfgNode>,
    #[serde(default)]
    cfg_edges: Vec<CfgEdge>,
    #[serde(default)]
    bridge_edges: Vec<BridgeEdge>,
}

#[derive(Debug, Deserialize)]
struct GraphNode {
    #[serde(default)]
    def_id: String,
    #[serde(default)]
    path: String,
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
    #[serde(alias = "lo")]
    start_offset: u32,
    #[serde(alias = "hi")]
    end_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolOccurrence {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub lo: u32,
    pub hi: u32,
}

#[derive(Debug, Deserialize)]
struct MirInfo {
    fingerprint: String,
    blocks: usize,
    stmts: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphEdge {
    #[serde(default, alias = "kind")]
    relation: String,
    from: String,
    to: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CfgNode {
    #[serde(default)]
    owner: String,
    block: usize,
    #[serde(default)]
    is_cleanup: bool,
    #[serde(default)]
    terminator: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CfgEdge {
    relation: String,
    from: String,
    to: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BridgeEdge {
    relation: String,
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SemanticTriple {
    pub from: String,
    pub relation: String,
    pub to: String,
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

    pub fn cfg_node_count(&self) -> usize {
        self.graph.cfg_nodes.len()
    }

    pub fn cfg_edge_count(&self) -> usize {
        self.graph.cfg_edges.len()
    }

    pub fn bridge_edge_count(&self) -> usize {
        self.graph.bridge_edges.len()
    }

    /// BFS shortest path across semantic edges, CFG edges, and bridge edges.
    /// Endpoints may be semantic symbols or raw `cfg::...` node ids.
    pub fn execution_path(&self, from: &str, to: &str, expand_bodies: bool) -> Result<String> {
        let from_key = self.resolve_execution_endpoint(from)?;
        let to_key = self.resolve_execution_endpoint(to)?;
        if from_key == to_key {
            return Ok(format!("`{from}` is the same as `{to}`."));
        }

        let adj = self.unified_adjacency();
        let prev = self.bfs_prev_map_owned(&adj, &from_key, &to_key);
        if !prev.contains_key(to_key.as_str()) {
            return Ok(format!(
                "No unified execution path found from `{from}` to `{to}`."
            ));
        }

        let path = self.reconstruct_path_owned(&prev, &from_key, &to_key);
        let mut out = format!(
            "Execution path from `{from}` → `{to}` ({} hops):\n",
            path.len() - 1
        );
        for (idx, (node_id, via_relation)) in path.iter().enumerate() {
            if idx > 0 {
                out.push_str(&format!("    --{}--> \n", via_relation.as_deref().unwrap_or("")));
            }
            self.push_execution_path_entry(&mut out, node_id, expand_bodies);
        }
        Ok(out)
    }

    /// Extract a stable summary for each symbol with a definition span.
    pub fn symbol_summaries(&self) -> Vec<SymbolSummary> {
        let mut call_in: HashMap<&str, usize> = HashMap::new();
        let mut call_out: HashMap<&str, usize> = HashMap::new();
        for edge in &self.graph.edges {
            if !edge_is_call(edge) {
                continue;
            }
            *call_out.entry(edge.from.as_str()).or_insert(0) += 1;
            *call_in.entry(edge.to.as_str()).or_insert(0) += 1;
        }

        let mut out = Vec::new();
        for (node_key, node) in &self.graph.nodes {
            if node.kind == "unknown" {
                continue;
            }
            let Some(def) = &node.def else { continue };
            let (mir_fingerprint, mir_blocks, mir_stmts) = match &node.mir {
                Some(m) => (Some(m.fingerprint.clone()), Some(m.blocks), Some(m.stmts)),
                None => (None, None, None),
            };
            let symbol = self.node_path(node_key, node).to_string();
            out.push(SymbolSummary {
                symbol,
                kind: node.kind.clone(),
                file: def.file.clone(),
                line: def.line,
                signature: node.signature.clone(),
                mir_fingerprint,
                mir_blocks,
                mir_stmts,
                call_in: *call_in.get(node_key.as_str()).unwrap_or(&0),
                call_out: *call_out.get(node_key.as_str()).unwrap_or(&0),
            });
        }

        out.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)).then(a.symbol.cmp(&b.symbol)));
        out
    }

    /// Return all `call` edges as (caller, callee) pairs.
    pub fn call_edges(&self) -> Vec<(String, String)> {
        self.graph
            .edges
            .iter()
            .filter(|e| edge_is_call(e))
            .map(|e| (self.edge_endpoint_path(&e.from), self.edge_endpoint_path(&e.to)))
            .collect()
    }

    // -----------------------------------------------------------------------
    // semantic_map
    // -----------------------------------------------------------------------

    /// Triple-style semantic graph view.
    /// Format: `(<from>, <relation>, <to>)`
    pub fn semantic_map(&self, filter_path: Option<&str>, expand_bodies: bool) -> String {
        let mut out = String::new();
        if expand_bodies {
            out.push_str("# note: `expand_bodies` is ignored for triple output\n");
        }

        let mut triples = self.semantic_triples(filter_path);
        triples.sort_by(|a, b| {
            a.from.cmp(&b.from)
                .then(a.relation.cmp(&b.relation))
                .then(a.to.cmp(&b.to))
        });

        for triple in triples {
            out.push('(');
            out.push_str(&triple.from);
            out.push_str(", ");
            out.push_str(&triple.relation);
            out.push_str(", ");
            out.push_str(&triple.to);
            out.push(')');
            out.push('\n');
        }
        if out.is_empty() {
            "No triples found.".to_string()
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
        let key = self.resolve_node_key(symbol)?;
        let node = self.graph.nodes.get(key).unwrap();
        let def = node.def.as_ref().context("symbol has no definition span")?;

        let source = fs::read_to_string(&def.file)
            .with_context(|| format!("could not read source file {}", def.file))?;

        let start_offset = def.start_offset as usize;
        let end_offset = def.end_offset as usize;
        if end_offset > source.len() || start_offset > end_offset {
            bail!(
                "byte offsets out of range (start_offset={start_offset} end_offset={end_offset} file_len={})",
                source.len()
            );
        }

        let (slice_lo, slice_hi) = expand_symbol_window_span(&source, start_offset, end_offset).unwrap_or((start_offset, end_offset));
        let text = source.get(slice_lo..slice_hi).with_context(|| {
            format!("expanded symbol span is not on UTF-8 boundaries (lo={slice_lo} hi={slice_hi})")
        })?;

        let display = shorten_path(&def.file);
        let mut out = format!("// {} — {}:{}\n", self.node_path(key, node), display, def.line);
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
        let unique = self.collect_unique_refs(symbol)?;
        if unique.is_empty() {
            return Ok(format!("No reference sites recorded for `{symbol}`."));
        }
        let mut out = format!("References to `{symbol}` ({} sites):\n", unique.len());
        for s in &unique {
            out.push_str(&format!("  {}:{}:{}\n", shorten_path(&s.file), s.line, s.col));
        }
        Ok(out)
    }

    /// Same as `symbol_refs` but appends the body of the enclosing symbol at each site.
    pub fn symbol_refs_expanded(&self, symbol: &str) -> Result<String> {
        let unique = self.collect_unique_refs(symbol)?;
        if unique.is_empty() {
            return Ok(format!("No reference sites recorded for `{symbol}`."));
        }
        let mut out = format!("References to `{symbol}` ({} sites):\n\n", unique.len());
        for span in &unique {
            out.push_str(&format!(
                "── {}:{}:{} ──\n",
                shorten_path(&span.file),
                span.line,
                span.col
            ));
            match self.find_enclosing_symbol(span) {
                Some(enclosing_key) => match self.symbol_window(enclosing_key) {
                    Ok(body) => out.push_str(&body),
                    Err(e) => out.push_str(&format!("  (could not extract body: {e})\n")),
                },
                None => out.push_str("  (no enclosing symbol found in graph)\n"),
            }
            out.push('\n');
        }
        Ok(out)
    }

    /// Collect deduplicated, sorted ref spans for `symbol`.
    fn collect_unique_refs<'a>(&'a self, symbol: &str) -> Result<Vec<&'a SourceSpan>> {
        let node = self.find_node(symbol)?;
        let mut spans: Vec<&SourceSpan> = node.refs.iter().collect();
        spans.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.col.cmp(&b.col))
        });
        let mut unique: Vec<&SourceSpan> = Vec::new();
        let mut seen = HashSet::<(String, u32, u32)>::new();
        for s in spans {
            if seen.insert((s.file.clone(), s.line, s.col)) {
                unique.push(s);
            }
        }
        Ok(unique)
    }

    /// Find the tightest graph node whose def span contains `ref_span` by byte offset.
    fn find_enclosing_symbol(&self, ref_span: &SourceSpan) -> Option<&str> {
        let mut best: Option<(&str, u32)> = None; // (key, span_width)
        for (key, node) in &self.graph.nodes {
            if let Some(def) = &node.def {
                if def.file == ref_span.file
                    && def.start_offset <= ref_span.start_offset
                    && ref_span.start_offset < def.end_offset
                {
                    let width = def.end_offset - def.start_offset;
                    let tighter = best.map_or(true, |(_, w)| width < w);
                    if tighter {
                        best = Some((key.as_str(), width));
                    }
                }
            }
        }
        best.map(|(k, _)| k)
    }

    // -----------------------------------------------------------------------
    // symbol_path
    // -----------------------------------------------------------------------

    /// BFS shortest path in the semantic graph from `from` to `to`.
    /// Returns the chain with relation-labeled hops and file:line annotations.
    /// If `expand_bodies` is true, inlines the source body of each hop.
    pub fn symbol_path(&self, from: &str, to: &str, expand_bodies: bool) -> Result<String> {
        let from_key = self.resolve_node_key(from)?;
        let to_key = self.resolve_node_key(to)?;
        if from_key == to_key {
            return Ok(format!("`{from}` is the same as `{to}`."));
        }

        let adj = self.semantic_adjacency();
        let prev = self.bfs_prev_map(&adj, from_key, to_key);

        if !prev.contains_key(to_key) {
            return Ok(format!(
                "No semantic-graph path found from `{from}` to `{to}`."
            ));
        }

        let path = self.reconstruct_path(&prev, from_key, to_key);

        let mut out = format!(
            "Semantic path from `{from}` → `{to}` ({} hops):\n",
            path.len() - 1
        );
        for (idx, (sym, via_relation)) in path.iter().enumerate() {
            if idx > 0 {
                out.push_str(&format!("    --{}--> \n", via_relation.unwrap_or("")));
            }
            self.push_symbol_path_entry(&mut out, sym, expand_bodies);
        }
        Ok(out)
    }

    fn reconstruct_path<'a>(
        &'a self,
        prev: &HashMap<&'a str, (&'a str, &'a str)>,
        from_key: &'a str,
        to_key: &'a str,
    ) -> Vec<(&'a str, Option<&'a str>)> {
        let mut path: Vec<(&str, Option<&str>)> = Vec::new();
        let mut cur = to_key;
        loop {
            let via_relation = if cur == from_key {
                None
            } else {
                Some(prev[cur].1)
            };
            path.push((cur, via_relation));
            if cur == from_key {
                break;
            }
            cur = prev[cur].0;
        }
        path.reverse();
        path
    }

    // -----------------------------------------------------------------------
    // symbol_neighborhood
    // -----------------------------------------------------------------------

    /// Immediate callers and callees of `symbol` in the call graph.
    /// If `expand_bodies` is true, inlines the source body of each caller and callee.
    pub fn symbol_neighborhood(&self, symbol: &str, expand_bodies: bool) -> Result<String> {
        let symbol_key = self.resolve_node_key(symbol)?;
        let node = self.graph.nodes.get(symbol_key).unwrap();
        let (callers, callees) = self.direct_call_neighbors(symbol_key);
        let inferred_callers = self.sorted_deduped_callers_from_refs(symbol_key, &node.refs);

        let mut out = format!("Neighborhood of `{symbol}`:\n");
        self.push_neighborhood_section(&mut out, "Callers", &callers, expand_bodies);

        if !inferred_callers.is_empty() {
            self.push_neighborhood_section(
                &mut out,
                "Inferred callers from refs",
                &inferred_callers,
                expand_bodies,
            );
        }

        self.push_neighborhood_section(&mut out, "Callees", &callees, expand_bodies);
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // occurrences
    // -----------------------------------------------------------------------

    /// Return the canonical fully-qualified graph key for `symbol`, or an error if not
    /// found or ambiguous.  Useful for deriving the new FQN in conflict checks.
    pub fn canonical_symbol_key(&self, symbol: &str) -> Result<String> {
        let key = self.resolve_node_key(symbol)?;
        let node = self.graph.nodes.get(key).unwrap();
        Ok(self.node_path(key, node).to_string())
    }

    /// Return `true` if `symbol` is an exact key in the graph (no fuzzy suffix matching).
    pub fn has_symbol(&self, symbol: &str) -> bool {
        self.resolve_node_key(symbol).is_ok()
    }

    pub fn symbol_occurrences(&self, symbol: &str) -> Result<Vec<SymbolOccurrence>> {
        let key = self.resolve_node_key(symbol)?;
        let node = self.graph.nodes.get(key).context("symbol key not present")?;
        let mut out = Vec::new();
        for r in &node.refs {
            out.push(SymbolOccurrence {
                file: r.file.clone(),
                line: r.line,
                col: r.col,
                lo: r.start_offset,
                hi: r.end_offset,
            });
        }
        if out.is_empty() {
            if let Some(def) = &node.def {
                out.push(SymbolOccurrence {
                    file: def.file.clone(),
                    line: def.line,
                    col: def.col,
                    lo: def.start_offset,
                    hi: def.end_offset,
                });
            }
        }
        out.sort_by(|a, b| a.file.cmp(&b.file).then(a.lo.cmp(&b.lo)).then(a.hi.cmp(&b.hi)));
        out.dedup_by(|a, b| a.file == b.file && a.lo == b.lo && a.hi == b.hi);
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn find_node(&self, symbol: &str) -> Result<&GraphNode> {
        let key = self.resolve_node_key(symbol)?;
        Ok(self.graph.nodes.get(key).unwrap())
    }

    fn resolve_execution_endpoint(&self, endpoint: &str) -> Result<String> {
        if self.graph.cfg_nodes.contains_key(endpoint) {
            return Ok(endpoint.to_string());
        }
        Ok(self.resolve_node_key(endpoint)?.to_string())
    }

    fn resolve_node_key(&self, symbol: &str) -> Result<&str> {
        if let Some((key, _node)) = self.graph.nodes.get_key_value(symbol) {
            return Ok(key.as_str());
        }
        let mut exact_path_matches: Vec<&str> = self
            .graph
            .nodes
            .iter()
            .filter(|(_, node)| !node.path.is_empty() && node.path == symbol)
            .map(|(key, _)| key.as_str())
            .collect();
        exact_path_matches.sort_unstable();
        exact_path_matches.dedup();
        if exact_path_matches.len() == 1 {
            return Ok(exact_path_matches[0]);
        }

        let suffix = format!("::{symbol}");
        let matches: Vec<&str> = self
            .graph
            .nodes
            .iter()
            .filter_map(|(key, node)| {
                let path = self.node_path(key, node);
                (path == symbol || path.ends_with(&suffix)).then_some(key.as_str())
            })
            .collect();
        match matches.len() {
            0 => bail!(
                "symbol `{symbol}` not found in graph. Use semantic_map to list available symbols."
            ),
            1 => Ok(matches[0]),
            n => bail!(
                "ambiguous symbol `{symbol}` — {n} matches: {}. Use the fully-qualified path.",
                matches
                    .iter()
                    .filter_map(|key| self.graph.nodes.get(*key).map(|node| self.node_path(key, node)))
                    .collect::<Vec<_>>()
                    .join(", ")
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

    pub fn semantic_triples(
        &self,
        filter_path: Option<&str>,
    ) -> Vec<SemanticTriple> {
        self.graph
            .edges
            .iter()
            .filter_map(|edge| {
                let from = self.edge_endpoint_path(&edge.from);
                let to = self.edge_endpoint_path(&edge.to);
                if let Some(fp) = filter_path {
                    if !from.starts_with(fp) && !to.starts_with(fp) {
                        return None;
                    }
                }
                Some(SemanticTriple {
                    from,
                    relation: edge_relation(edge).to_string(),
                    to,
                })
            })
            .collect()
    }

    fn direct_call_neighbors<'a>(&'a self, symbol_key: &str) -> (Vec<&'a str>, Vec<&'a str>) {
        let mut callers: Vec<&str> = Vec::new();
        let mut callees: Vec<&str> = Vec::new();

        for edge in &self.graph.edges {
            if !edge_is_call(edge) {
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
        (callers, callees)
    }

    fn sorted_deduped_callers_from_refs<'a>(
        &'a self,
        symbol_key: &str,
        refs: &'a [SourceSpan],
    ) -> Vec<&'a str> {
        let mut inferred_callers = self.infer_callers_from_refs(symbol_key, refs);
        inferred_callers.sort();
        inferred_callers.dedup();
        inferred_callers
    }

    fn push_neighborhood_section(
        &self,
        out: &mut String,
        label: &str,
        symbols: &[&str],
        expand_bodies: bool,
    ) {
        out.push_str(&format!("  {label} ({}):\n", symbols.len()));
        for sym in symbols {
            out.push_str(&format!("    {}\n", self.edge_endpoint_path(sym)));
            self.push_expanded_symbol_body(out, sym, expand_bodies);
        }
    }

    fn push_expanded_symbol_body(&self, out: &mut String, sym: &str, expand_bodies: bool) {
        if !expand_bodies {
            return;
        }
        if let Ok(body) = self.symbol_window(sym) {
            for line in body.lines() {
                out.push_str("      ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    fn push_symbol_path_entry(&self, out: &mut String, sym: &str, expand_bodies: bool) {
        if let Some(node) = self.graph.nodes.get(sym) {
            if let Some(def) = &node.def {
                out.push_str(&format!(
                    "  {} ({}:{})\n",
                    self.node_path(sym, node),
                    shorten_path(&def.file),
                    def.line
                ));
                if expand_bodies {
                    if let Ok(body) = self.symbol_window(sym) {
                        for line in body.lines() {
                            out.push_str("    ");
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                }
                return;
            }
        }
        out.push_str(&format!("  {}\n", self.edge_endpoint_path(sym)));
    }

    fn push_execution_path_entry(&self, out: &mut String, node_id: &str, expand_bodies: bool) {
        if self.graph.nodes.contains_key(node_id) {
            self.push_symbol_path_entry(out, node_id, expand_bodies);
            return;
        }
        if let Some(cfg) = self.graph.cfg_nodes.get(node_id) {
            let owner = self.edge_endpoint_path(&cfg.owner);
            out.push_str(&format!(
                "  {} [owner={} block={} term={} cleanup={}]\n",
                node_id,
                owner,
                cfg.block,
                if cfg.terminator.is_empty() {
                    "unknown"
                } else {
                    cfg.terminator.as_str()
                },
                cfg.is_cleanup
            ));
            if expand_bodies && self.graph.nodes.contains_key(cfg.owner.as_str()) {
                self.push_expanded_symbol_body(out, cfg.owner.as_str(), true);
            }
            return;
        }
        out.push_str(&format!("  {}\n", node_id));
    }

    fn semantic_adjacency(&self) -> HashMap<&str, Vec<(&str, &str)>> {
        let mut adj: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
        for edge in &self.graph.edges {
            adj.entry(&edge.from)
                .or_default()
                .push((&edge.to, edge_relation(edge)));
        }
        adj
    }

    fn unified_adjacency(&self) -> HashMap<String, Vec<(String, String)>> {
        let mut adj: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for edge in &self.graph.edges {
            adj.entry(edge.from.clone())
                .or_default()
                .push((edge.to.clone(), edge_relation(edge).to_string()));
        }
        for edge in &self.graph.cfg_edges {
            adj.entry(edge.from.clone())
                .or_default()
                .push((edge.to.clone(), edge.relation.clone()));
        }
        for edge in &self.graph.bridge_edges {
            adj.entry(edge.from.clone())
                .or_default()
                .push((edge.to.clone(), edge.relation.clone()));
        }
        adj
    }

    fn bfs_prev_map<'a>(
        &'a self,
        adj: &HashMap<&'a str, Vec<(&'a str, &'a str)>>,
        from_key: &'a str,
        to_key: &'a str,
    ) -> HashMap<&'a str, (&'a str, &'a str)> {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut prev: HashMap<&str, (&str, &str)> = HashMap::new();
        let mut queue: VecDeque<&str> = VecDeque::new();

        visited.insert(from_key);
        queue.push_back(from_key);

        'bfs: loop {
            let Some(cur) = queue.pop_front() else { break };
            if let Some(neighbors) = adj.get(cur) {
                for &(nb, relation) in neighbors {
                    if visited.insert(nb) {
                        prev.insert(nb, (cur, relation));
                        if nb == to_key {
                            break 'bfs;
                        }
                        queue.push_back(nb);
                    }
                }
            }
        }

        prev
    }

    fn bfs_prev_map_owned(
        &self,
        adj: &HashMap<String, Vec<(String, String)>>,
        from_key: &str,
        to_key: &str,
    ) -> HashMap<String, (String, String)> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut prev: HashMap<String, (String, String)> = HashMap::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        visited.insert(from_key.to_string());
        queue.push_back(from_key.to_string());

        'bfs: while let Some(cur) = queue.pop_front() {
            if let Some(neighbors) = adj.get(&cur) {
                for (nb, relation) in neighbors {
                    if visited.insert(nb.clone()) {
                        prev.insert(nb.clone(), (cur.clone(), relation.clone()));
                        if nb == to_key {
                            break 'bfs;
                        }
                        queue.push_back(nb.clone());
                    }
                }
            }
        }
        prev
    }

    fn reconstruct_path_owned(
        &self,
        prev: &HashMap<String, (String, String)>,
        from_key: &str,
        to_key: &str,
    ) -> Vec<(String, Option<String>)> {
        let mut path: Vec<(String, Option<String>)> = Vec::new();
        let mut cur = to_key.to_string();
        loop {
            let via_relation = if cur == from_key {
                None
            } else {
                Some(prev[&cur].1.clone())
            };
            path.push((cur.clone(), via_relation));
            if cur == from_key {
                break;
            }
            cur = prev[&cur].0.clone();
        }
        path.reverse();
        path
    }

    fn enclosing_symbol_for_span<'a>(&'a self, span: &SourceSpan) -> Option<&'a str> {
        let mut best: Option<(&str, u32)> = None;
        for (sym, node) in &self.graph.nodes {
            let Some(def) = node.def.as_ref() else { continue };
            if def.file != span.file {
                continue;
            }
            if def.start_offset <= span.start_offset && def.end_offset >= span.end_offset {
                let width = def.end_offset.saturating_sub(def.start_offset);
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

    fn node_path<'a>(&'a self, key: &'a str, node: &'a GraphNode) -> &'a str {
        if node.path.is_empty() {
            key
        } else {
            node.path.as_str()
        }
    }

    fn edge_endpoint_path(&self, key: &str) -> String {
        self.graph
            .nodes
            .get(key)
            .map(|node| self.node_path(key, node).to_string())
            .unwrap_or_else(|| key.to_string())
    }
}

fn edge_is_call(edge: &GraphEdge) -> bool {
    let relation = edge_relation(edge);
    relation.eq_ignore_ascii_case("call") || relation.eq_ignore_ascii_case("calls")
}

fn edge_relation(edge: &GraphEdge) -> &str {
    if edge.relation.is_empty() {
        ""
    } else {
        edge.relation.as_str()
    }
}

fn expand_symbol_window_span(source: &str, start_offset: usize, end_offset: usize) -> Option<(usize, usize)> {
    if start_offset > end_offset || end_offset > source.len() {
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
        if start <= start_offset && end >= end_offset {
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

/// Best-effort path shortening for human-facing output.
pub fn shorten_display_path(path: &str) -> String {
    // Strip known workspace prefix for readability.
    const WORKSPACE: &str = "/workspace/ai_sandbox/canon-mini-agent/";
    if let Some(rest) = path.strip_prefix(WORKSPACE) {
        return rest.to_string();
    }
    path.to_string()
}

fn shorten_path(path: &str) -> String {
    shorten_display_path(path)
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
        let start_offset = src.find("pub(crate) fn").unwrap();
        let end_offset = src.find(") -> Result<(bool, String)>").unwrap() + ") -> Result<(bool, String)>".len();
        let (slice_lo, slice_hi) = expand_symbol_window_span(src, start_offset, end_offset).expect("span should expand");
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

        let start_offset = src.find("pub(crate) fn").unwrap();
        let end_offset = src.find(") -> Result<(bool, String)>").unwrap() + ") -> Result<(bool, String)>".len();
        let mut nodes = HashMap::new();
        nodes.insert(
            "engine::process_action_and_execute".to_string(),
            GraphNode {
                def_id: String::new(),
                path: "engine::process_action_and_execute".to_string(),
                kind: "fn".to_string(),
                def: Some(SourceSpan {
                    file: src_path.to_string_lossy().to_string(),
                    line: 1,
                    col: 1,
                    start_offset: start_offset as u32,
                    end_offset: end_offset as u32,
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
                cfg_nodes: HashMap::new(),
                cfg_edges: Vec::new(),
                bridge_edges: Vec::new(),
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
        let start_offset = src.find("Widget").unwrap();
        let end_offset = start_offset + "Widget".len();
        let (slice_lo, slice_hi) = expand_symbol_window_span(src, start_offset, end_offset).expect("span should expand");
        let extracted = &src[slice_lo..slice_hi];
        assert!(extracted.starts_with("pub struct Widget"));
        assert!(extracted.contains("pub x: i32"));
        assert!(extracted.trim_end().ends_with('}'));
    }

    #[test]
    fn expand_symbol_window_span_expands_trait_from_identifier_span() {
        let src = "pub trait Greeter {\n    fn greet(&self);\n}\n";
        let start_offset = src.find("Greeter").unwrap();
        let end_offset = start_offset + "Greeter".len();
        let (slice_lo, slice_hi) = expand_symbol_window_span(src, start_offset, end_offset).expect("span should expand");
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
                def_id: String::new(),
                path: "canon_mini_agent::app::continue_executor_completion".to_string(),
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
                def_id: String::new(),
                path: "canon_mini_agent::engine::process_action_and_execute".to_string(),
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
                    relation: "call".to_string(),
                    from: "canon_mini_agent::app::continue_executor_completion".to_string(),
                    to: "canon_mini_agent::engine::process_action_and_execute".to_string(),
                }],
                cfg_nodes: HashMap::new(),
                cfg_edges: Vec::new(),
                bridge_edges: Vec::new(),
            },
        };

        let n = idx
            .symbol_neighborhood("engine::process_action_and_execute", false)
            .expect("neighborhood should resolve suffix symbol");
        assert!(n.contains("Callers (1):"));
        assert!(n.contains("canon_mini_agent::app::continue_executor_completion"));

        let p = idx
            .symbol_path(
                "app::continue_executor_completion",
                "engine::process_action_and_execute",
                false,
            )
            .expect("path should resolve suffix symbols");
        assert!(p.contains("1 hops"));
        assert!(p.contains("--call-->") || p.contains("--Calls-->"));
        assert!(p.contains("canon_mini_agent::app::continue_executor_completion"));
        assert!(p.contains("canon_mini_agent::engine::process_action_and_execute"));
    }

    #[test]
    fn symbol_path_traverses_non_call_semantic_edges() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "app".to_string(),
            GraphNode {
                def_id: String::new(),
                path: "app".to_string(),
                kind: "mod".to_string(),
                def: None,
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        nodes.insert(
            "app::run".to_string(),
            GraphNode {
                def_id: String::new(),
                path: "app::run".to_string(),
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
                    relation: "Contains".to_string(),
                    from: "app".to_string(),
                    to: "app::run".to_string(),
                }],
                cfg_nodes: HashMap::new(),
                cfg_edges: Vec::new(),
                bridge_edges: Vec::new(),
            },
        };

        let out = idx
            .symbol_path("app", "app::run", false)
            .expect("semantic path should traverse non-call edges");
        assert!(out.contains("Semantic path from `app`"));
        assert!(out.contains("--Contains-->"));
        assert!(out.contains("app::run"));
    }

    #[test]
    fn execution_path_traverses_cfg_and_bridge_edges() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "app::run".to_string(),
            GraphNode {
                def_id: String::new(),
                path: "app::run".to_string(),
                kind: "fn".to_string(),
                def: None,
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        nodes.insert(
            "dep::callee".to_string(),
            GraphNode {
                def_id: String::new(),
                path: "dep::callee".to_string(),
                kind: "fn".to_string(),
                def: None,
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        let mut cfg_nodes = HashMap::new();
        cfg_nodes.insert(
            "cfg::app::run::bb0".to_string(),
            CfgNode {
                owner: "app::run".to_string(),
                block: 0,
                is_cleanup: false,
                terminator: "Call".to_string(),
            },
        );

        let idx = SemanticIndex {
            graph: CrateGraph {
                nodes,
                edges: Vec::new(),
                cfg_nodes,
                cfg_edges: Vec::new(),
                bridge_edges: vec![
                    BridgeEdge {
                        relation: "Entry".to_string(),
                        from: "app::run".to_string(),
                        to: "cfg::app::run::bb0".to_string(),
                    },
                    BridgeEdge {
                        relation: "Call".to_string(),
                        from: "cfg::app::run::bb0".to_string(),
                        to: "dep::callee".to_string(),
                    },
                ],
            },
        };

        let out = idx
            .execution_path("app::run", "dep::callee", false)
            .expect("execution path should succeed");
        assert!(out.contains("Execution path from `app::run`"));
        assert!(out.contains("--Entry-->"));
        assert!(out.contains("--Call-->"));
        assert!(out.contains("cfg::app::run::bb0"));
        assert!(out.contains("dep::callee"));
    }

    #[test]
    fn execution_path_accepts_raw_cfg_endpoint() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "app::run".to_string(),
            GraphNode {
                def_id: String::new(),
                path: "app::run".to_string(),
                kind: "fn".to_string(),
                def: None,
                refs: Vec::new(),
                signature: None,
                mir: None,
                fields: Vec::new(),
            },
        );
        let mut cfg_nodes = HashMap::new();
        cfg_nodes.insert(
            "cfg::app::run::bb0".to_string(),
            CfgNode {
                owner: "app::run".to_string(),
                block: 0,
                is_cleanup: false,
                terminator: "Call".to_string(),
            },
        );
        let idx = SemanticIndex {
            graph: CrateGraph {
                nodes,
                edges: Vec::new(),
                cfg_nodes,
                cfg_edges: Vec::new(),
                bridge_edges: vec![BridgeEdge {
                    relation: "Entry".to_string(),
                    from: "app::run".to_string(),
                    to: "cfg::app::run::bb0".to_string(),
                }],
            },
        };

        let out = idx
            .execution_path("app::run", "cfg::app::run::bb0", false)
            .expect("execution path should accept raw cfg id");
        assert!(out.contains("cfg::app::run::bb0"));
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
                def_id: String::new(),
                path: "app::drive".to_string(),
                kind: "fn".to_string(),
                def: Some(SourceSpan {
                    file: "src/app.rs".to_string(),
                    line: 1,
                    col: 1,
                    start_offset: 0,
                    end_offset: caller_src.len() as u32,
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
                def_id: String::new(),
                path: "app::run_planner_phase".to_string(),
                kind: "fn".to_string(),
                def: Some(SourceSpan {
                    file: "src/app.rs".to_string(),
                    line: 10,
                    col: 1,
                    start_offset: 0,
                    end_offset: target_src.len() as u32,
                }),
                refs: vec![SourceSpan {
                    file: "src/app.rs".to_string(),
                    line: 2,
                    col: 5,
                    start_offset: call_lo,
                    end_offset: call_hi,
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
                cfg_nodes: HashMap::new(),
                cfg_edges: Vec::new(),
                bridge_edges: Vec::new(),
            },
        };
        let out = idx
            .symbol_neighborhood("app::run_planner_phase", false)
            .expect("neighborhood should succeed");
        assert!(out.contains("Callers (0):"));
        assert!(out.contains("Inferred callers from refs (1):"));
        assert!(out.contains("app::drive"));
    }
}

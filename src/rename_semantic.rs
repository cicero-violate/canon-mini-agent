use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSpan {
    pub lo: usize,
    pub hi: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanReplacement {
    pub span: ByteSpan,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckedReplacement {
    span: ByteSpan,
    replacement: String,
    expected: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RenameReport {
    pub touched_files: Vec<PathBuf>,
    pub replacements: usize,
}

fn is_valid_rust_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn remove_nested_spans(replacements: &mut Vec<SpanReplacement>) {
    if replacements.len() < 2 {
        return;
    }
    let mut keep = vec![true; replacements.len()];
    for i in 0..replacements.len() {
        for j in 0..replacements.len() {
            if i == j {
                continue;
            }
            let a = &replacements[i];
            let b = &replacements[j];
            let a_lo = a.span.lo;
            let a_hi = a.span.hi;
            let b_lo = b.span.lo;
            let b_hi = b.span.hi;
            if a_lo <= b_lo && b_hi <= a_hi && (a_lo < b_lo || b_hi < a_hi) {
                keep[i] = false;
            }
        }
    }
    let mut idx = 0usize;
    replacements.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

fn normalize_replacements(
    file_len: usize,
    replacements: &mut Vec<SpanReplacement>,
) -> Result<()> {
    for r in replacements.iter() {
        if r.span.lo > r.span.hi {
            bail!("invalid span: lo > hi ({} > {})", r.span.lo, r.span.hi);
        }
        if r.span.hi > file_len {
            bail!(
                "invalid span: hi out of bounds ({} > {})",
                r.span.hi,
                file_len
            );
        }
        if r.span.lo == r.span.hi {
            bail!("invalid span: empty replacement at {}", r.span.lo);
        }
    }

    replacements.sort_by(|a, b| {
        a.span
            .lo
            .cmp(&b.span.lo)
            .then_with(|| a.span.hi.cmp(&b.span.hi))
            .then_with(|| a.replacement.cmp(&b.replacement))
    });
    replacements.dedup_by(|a, b| {
        a.span.lo == b.span.lo && a.span.hi == b.span.hi && a.replacement == b.replacement
    });
    remove_nested_spans(replacements);

    for window in replacements.windows(2) {
        let a = &window[0];
        let b = &window[1];
        if a.span.lo == b.span.lo && a.span.hi == b.span.hi && a.replacement != b.replacement {
            bail!(
                "conflicting replacements at {}..{}",
                a.span.lo,
                a.span.hi
            );
        }
        if a.span.hi > b.span.lo {
            bail!(
                "overlapping replacements at {}..{} and {}..{}",
                a.span.lo,
                a.span.hi,
                b.span.lo,
                b.span.hi
            );
        }
    }

    Ok(())
}

fn apply_replacements(source: &str, replacements: &mut Vec<SpanReplacement>) -> Result<String> {
    normalize_replacements(source.len(), replacements)?;
    for r in replacements.iter() {
        if !source.is_char_boundary(r.span.lo) || !source.is_char_boundary(r.span.hi) {
            bail!(
                "replacement span not on utf-8 boundary: {}..{}",
                r.span.lo,
                r.span.hi
            );
        }
    }
    let mut out = source.to_string();
    for r in replacements.iter().rev() {
        out.replace_range(r.span.lo..r.span.hi, &r.replacement);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Attribute string-literal rewriting
// ---------------------------------------------------------------------------

/// Scan `source` and return `(lo, hi)` byte ranges for every `#[...]` / `#![...]`
/// attribute block (lo = the `#`, hi = one past the closing `]`).
///
/// Correctly skips line/block comments and double-quoted string literals in
/// non-attribute code so that `#[` patterns inside strings are ignored.
/// Inside an attribute block the bracket depth is tracked while advancing past
/// string literals without counting their contents toward depth.
fn scan_attr_ranges(source: &str) -> Vec<(usize, usize)> {
    let b = source.as_bytes();
    let n = b.len();
    let mut ranges = Vec::new();
    let mut i = 0;

    while i < n {
        // Line comment → skip to end of line.
        if i + 1 < n && b[i] == b'/' && b[i + 1] == b'/' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment → skip to closing `*/`.
        if i + 1 < n && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // String literal in non-attr code → skip contents so `#[` inside a
        // string literal doesn't trigger attr detection.
        if b[i] == b'"' {
            i += 1;
            while i < n {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                let c = b[i];
                i += 1;
                if c == b'"' {
                    break;
                }
            }
            continue;
        }
        // Attribute start: `#[` (outer) or `#![` (inner).
        if b[i] == b'#'
            && i + 1 < n
            && (b[i + 1] == b'['
                || (b[i + 1] == b'!' && i + 2 < n && b[i + 2] == b'['))
        {
            let attr_start = i;
            i += if b[i + 1] == b'!' { 3 } else { 2 }; // skip `#[` or `#![`
            let mut depth = 1usize;
            let mut closed = false;
            while i < n {
                // Skip string literal contents inside the attr (bracket depth
                // must not be affected by `[` or `]` inside strings).
                if b[i] == b'"' {
                    i += 1;
                    while i < n {
                        if b[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        let c = b[i];
                        i += 1;
                        if c == b'"' {
                            break;
                        }
                    }
                    continue;
                }
                if b[i] == b'[' {
                    depth += 1;
                } else if b[i] == b']' {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        ranges.push((attr_start, i));
                        closed = true;
                        break;
                    }
                }
                i += 1;
            }
            let _ = closed;
            continue;
        }
        i += 1;
    }
    ranges
}

/// Return byte positions `(lo, hi)` of every `"<old_ident>"` string literal
/// (including the surrounding quotes) that appears inside a `#[...]` attribute.
fn attr_string_literal_positions(source: &str, old_ident: &str) -> Vec<(usize, usize)> {
    let needle = format!("\"{old_ident}\"");
    let attr_ranges = scan_attr_ranges(source);
    let mut result = Vec::new();
    for (attr_lo, attr_hi) in attr_ranges {
        let chunk = &source[attr_lo..attr_hi];
        let mut offset = 0;
        while let Some(pos) = chunk[offset..].find(&needle) {
            let abs_lo = attr_lo + offset + pos;
            let abs_hi = abs_lo + needle.len();
            result.push((abs_lo, abs_hi));
            offset += pos + needle.len();
        }
    }
    result.sort_by_key(|&(lo, _)| lo);
    result.dedup();
    result
}

/// Replace every `"<old_ident>"` string literal inside `#[...]` attributes in
/// `source` with `"<new_ident>"`.  Non-attribute string literals are untouched.
fn rewrite_attr_string_literals(source: &str, old_ident: &str, new_ident: &str) -> String {
    let replacement = format!("\"{new_ident}\"");
    let positions = attr_string_literal_positions(source, old_ident);
    if positions.is_empty() {
        return source.to_string();
    }
    let mut result = source.to_string();
    for (lo, hi) in positions.into_iter().rev() {
        result.replace_range(lo..hi, &replacement);
    }
    result
}

// ---------------------------------------------------------------------------

fn ensure_under_workspace(workspace: &Path, file: &Path) -> Result<()> {
    if !file.is_absolute() {
        bail!("expected absolute file path from semantic spans, got: {}", file.display());
    }
    let ws = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {}", workspace.display()))?;
    let f = file
        .canonicalize()
        .with_context(|| format!("canonicalize file {}", file.display()))?;
    if !f.starts_with(&ws) {
        bail!(
            "refusing to edit file outside workspace: {} (workspace={})",
            f.display(),
            ws.display()
        );
    }
    Ok(())
}

/// Span-based rename using the semantic graph's recorded identifier spans.
///
/// This is intended to be a safer replacement for the current `rename_symbol`
/// tool, which is file-scoped and token-text based.
///
/// - `old_symbol` may be a fully qualified path; we derive `old_ident` from its last `::` segment.
/// - `new_symbol` may be fully qualified; only its last `::` segment is used as the identifier.
/// - Offsets are byte offsets into the on-disk file content; if sources changed since the graph
///   was built, the rename will fail with a mismatch error.
pub fn rename_symbols_via_semantic_spans(
    workspace: &Path,
    idx: &crate::semantic::SemanticIndex,
    renames: &[(String, String)],
) -> Result<RenameReport> {
    if renames.is_empty() {
        bail!("rename requires at least one (old,new) pair");
    }

    let mut per_file: HashMap<PathBuf, Vec<CheckedReplacement>> = HashMap::new();
    for (old_symbol, new_symbol) in renames {
        let old_ident = old_symbol
            .rsplit("::")
            .next()
            .unwrap_or(old_symbol.as_str())
            .trim();
        let new_ident = new_symbol
            .rsplit("::")
            .next()
            .unwrap_or(new_symbol.as_str())
            .trim();

        if !is_valid_rust_identifier(old_ident) || !is_valid_rust_identifier(new_ident) {
            bail!(
                "rename requires valid Rust identifier names (old={old_ident}, new={new_ident})"
            );
        }
        if old_ident == new_ident {
            bail!("rename old and new identifiers are identical: {old_ident}");
        }

        // Conflict check: derive the expected new FQN and verify it doesn't
        // already exist.  We resolve the canonical key first so the check is
        // based on the actual graph path, not the (possibly abbreviated) input.
        let canonical_old = idx
            .canonical_symbol_key(old_symbol)
            .with_context(|| format!("resolve canonical key for {old_symbol}"))?;
        let new_fqn = canonical_old
            .rsplit_once("::")
            .map_or_else(|| new_ident.to_string(), |(prefix, _)| format!("{prefix}::{new_ident}"));
        if idx.has_symbol(&new_fqn) {
            bail!(
                "rename conflict: '{new_fqn}' already exists in the graph — choose a different name"
            );
        }

        let occurrences = idx
            .symbol_occurrences(old_symbol)
            .with_context(|| format!("resolve occurrences for symbol {old_symbol}"))?;
        if occurrences.is_empty() {
            bail!("no recorded occurrences for symbol {old_symbol}");
        }

        for occ in occurrences {
            let file = PathBuf::from(&occ.file);
            ensure_under_workspace(workspace, &file)?;
            per_file.entry(file).or_default().push(CheckedReplacement {
                span: ByteSpan {
                    lo: occ.lo as usize,
                    hi: occ.hi as usize,
                },
                replacement: new_ident.to_string(),
                expected: old_ident.to_string(),
            });
        }
    }

    let mut report = RenameReport::default();
    for (file, replacements) in per_file {
        let original = std::fs::read_to_string(&file)
            .with_context(|| format!("read {}", file.display()))?;

        for r in replacements.iter() {
            let snippet = original
                .get(r.span.lo..r.span.hi)
                .ok_or_else(|| anyhow!("span out of bounds for {}", file.display()))?;
            if snippet != r.expected {
                bail!(
                    "span mismatch in {} at {}..{}: expected '{}', found '{snippet}'. Rebuild the semantic graph and retry.",
                    file.display(),
                    r.span.lo,
                    r.span.hi,
                    r.expected
                );
            }
        }

        let mut span_replacements: Vec<SpanReplacement> = replacements
            .iter()
            .map(|r| SpanReplacement {
                span: r.span,
                replacement: r.replacement.clone(),
            })
            .collect();
        let after_spans = apply_replacements(&original, &mut span_replacements)
            .with_context(|| format!("apply replacements for {}", file.display()))?;

        // Attr string-literal rewrite: for each (old_ident, new_ident) pair that
        // touched this file, replace `"old_ident"` literals inside `#[...]` attrs.
        // This catches `#[serde(rename = "old_fn")]` and similar patterns that the
        // span-based pass doesn't cover (the compiler doesn't record refs inside
        // attribute token streams as identifier spans).
        let mut attr_pairs: Vec<(&str, &str)> = replacements
            .iter()
            .map(|r| (r.expected.as_str(), r.replacement.as_str()))
            .collect();
        attr_pairs.sort_unstable();
        attr_pairs.dedup();
        let updated = attr_pairs
            .iter()
            .fold(after_spans, |src, &(old, new)| rewrite_attr_string_literals(&src, old, new));

        if updated != original {
            std::fs::write(&file, &updated)
                .with_context(|| format!("write {}", file.display()))?;
            report.replacements += replacements.len();
            report.touched_files.push(file);
        }
    }

    report.touched_files.sort();
    report.touched_files.dedup();
    Ok(report)
}

pub fn rename_symbol_via_semantic_spans(
    workspace: &Path,
    idx: &crate::semantic::SemanticIndex,
    old_symbol: &str,
    new_symbol: &str,
) -> Result<RenameReport> {
    rename_symbols_via_semantic_spans(
        workspace,
        idx,
        &[(old_symbol.to_string(), new_symbol.to_string())],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "canon-mini-agent-test-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn apply_replacements_rejects_overlaps() {
        let src = "abcde";
        let mut reps = vec![
            SpanReplacement {
                span: ByteSpan { lo: 1, hi: 3 },
                replacement: "XX".to_string(),
            },
            SpanReplacement {
                span: ByteSpan { lo: 2, hi: 4 },
                replacement: "YY".to_string(),
            },
        ];
        let err = apply_replacements(src, &mut reps).unwrap_err().to_string();
        assert!(err.contains("overlapping"));
    }

    #[test]
    fn apply_replacements_works_reverse_order() {
        let src = "foo foo";
        let mut reps = vec![
            SpanReplacement {
                span: ByteSpan { lo: 0, hi: 3 },
                replacement: "bar".to_string(),
            },
            SpanReplacement {
                span: ByteSpan { lo: 4, hi: 7 },
                replacement: "bar".to_string(),
            },
        ];
        let out = apply_replacements(src, &mut reps).unwrap();
        assert_eq!(out, "bar bar");
    }

    #[test]
    fn ensure_under_workspace_rejects_outside() {
        let ws = tmp_dir("ws");
        let other = tmp_dir("other");
        let file = other.join("x.rs");
        std::fs::write(&file, "fn x(){}").unwrap();
        let err = ensure_under_workspace(&ws, &file).unwrap_err().to_string();
        assert!(err.contains("outside workspace"));
    }

    // -----------------------------------------------------------------------
    // attr string-literal rewrite tests
    // -----------------------------------------------------------------------

    #[test]
    fn rewrite_attr_string_literal_basic() {
        let src = "#[serde(rename = \"old_fn\")]\npub fn old_fn() {}\n";
        let result = rewrite_attr_string_literals(src, "old_fn", "new_fn");
        assert!(result.contains("rename = \"new_fn\""), "attr should be rewritten: {result}");
        assert!(!result.contains("rename = \"old_fn\""), "old attr value should be gone: {result}");
        // The identifier itself (not in a string) is untouched by attr rewrite
        assert!(result.contains("fn old_fn()"), "ident outside attr must not be touched: {result}");
    }

    #[test]
    fn rewrite_attr_string_literal_skips_non_attr_strings() {
        let src = "let s = \"old_fn\";\n#[serde(rename = \"old_fn\")]\npub fn old_fn() {}\n";
        let result = rewrite_attr_string_literals(src, "old_fn", "new_fn");
        // Non-attr string literal must NOT change.
        assert!(
            result.contains("let s = \"old_fn\""),
            "non-attr string must be untouched: {result}"
        );
        // Attr string literal must change.
        assert!(
            result.contains("rename = \"new_fn\""),
            "attr value must be rewritten: {result}"
        );
    }

    #[test]
    fn rewrite_attr_string_literal_inner_attr() {
        let src = "#![doc = \"old_fn\"]\npub fn foo() {}\n";
        let result = rewrite_attr_string_literals(src, "old_fn", "new_fn");
        assert!(result.contains("\"new_fn\""), "inner attr should be rewritten: {result}");
        assert!(!result.contains("\"old_fn\""), "old value should be gone: {result}");
    }

    #[test]
    fn rewrite_attr_string_literal_noop_when_no_match() {
        let src = "#[serde(rename = \"something_else\")]\npub fn foo() {}\n";
        let result = rewrite_attr_string_literals(src, "old_fn", "new_fn");
        assert_eq!(result, src, "source must be unchanged when no match");
    }

    #[test]
    fn scan_attr_ranges_finds_outer_and_inner_attrs() {
        let src = "#[derive(Debug)]\n#![allow(dead_code)]\nfn f() {}\n";
        let ranges = scan_attr_ranges(src);
        assert_eq!(ranges.len(), 2);
        assert!(src[ranges[0].0..ranges[0].1].starts_with("#[derive"));
        assert!(src[ranges[1].0..ranges[1].1].starts_with("#![allow"));
    }

    #[test]
    fn scan_attr_ranges_ignores_hash_bracket_in_string() {
        let src = "let _ = \"#[not an attr]\";\n#[real_attr]\nfn f() {}\n";
        let ranges = scan_attr_ranges(src);
        assert_eq!(ranges.len(), 1);
        assert!(src[ranges[0].0..ranges[0].1].contains("real_attr"));
    }

    // -----------------------------------------------------------------------
    // conflict pre-check tests (round-trip via rename_symbols_via_semantic_spans)
    // -----------------------------------------------------------------------

    fn write_graph_with_two_symbols(workspace: &std::path::Path, sym_a: &str, sym_b: &str, file: &std::path::Path, src: &str, ident_a: &str) {
        // sym_a has refs at every occurrence of ident_a in src; sym_b has no refs.
        let mut refs = Vec::new();
        for (lo, _) in src.match_indices(ident_a) {
            let hi = lo + ident_a.len();
            let prefix = &src[..lo];
            let line = (prefix.bytes().filter(|b| *b == b'\n').count() + 1) as u32;
            let col = (prefix.bytes().rev().take_while(|b| *b != b'\n').count()) as u32;
            refs.push(serde_json::json!({
                "file": file.display().to_string(),
                "line": line, "col": col,
                "lo": lo as u32, "hi": hi as u32,
            }));
        }
        let graph = serde_json::json!({
            "nodes": {
                sym_a: { "kind": "fn", "refs": refs, "fields": [] },
                sym_b: { "kind": "fn", "refs": [], "fields": [] },
            },
            "edges": []
        });
        let path = workspace.join("state/rustc/canon_mini_agent/graph.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();
    }

    #[test]
    fn rename_rejects_conflict_with_existing_symbol() {
        let ws = tmp_dir("conflict-check");
        let src_dir = ws.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("lib.rs");
        let src = "fn old_fn() {}\nfn new_fn() {}\n";
        std::fs::write(&file, src).unwrap();

        write_graph_with_two_symbols(
            &ws,
            "crate::old_fn",
            "crate::new_fn",
            &file,
            src,
            "old_fn",
        );

        let idx = crate::semantic::SemanticIndex::load(&ws, "canon_mini_agent").unwrap();
        let err = super::rename_symbols_via_semantic_spans(
            &ws,
            &idx,
            &[("crate::old_fn".to_string(), "crate::new_fn".to_string())],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("conflict"), "expected conflict error: {err}");
        assert!(err.contains("new_fn"), "error should name the conflicting symbol: {err}");
    }

    #[test]
    fn rename_applies_attr_string_rewrite_alongside_span_replacement() {
        let ws = tmp_dir("attr-rewrite-integration");
        let src_dir = ws.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("lib.rs");
        // A function with a serde rename attr and a call site.
        let src = "#[serde(rename = \"old_fn\")]\npub fn old_fn() {}\npub fn caller() { old_fn(); }\n";
        std::fs::write(&file, src).unwrap();

        // Build a graph: old_fn has refs at both `old_fn` identifier occurrences.
        let ident = "old_fn";
        let mut refs = Vec::new();
        for (lo, _) in src.match_indices(ident) {
            let hi = lo + ident.len();
            let prefix = &src[..lo];
            let line = (prefix.bytes().filter(|b| *b == b'\n').count() + 1) as u32;
            let col = (prefix.bytes().rev().take_while(|b| *b != b'\n').count()) as u32;
            refs.push(serde_json::json!({
                "file": file.display().to_string(),
                "line": line, "col": col,
                "lo": lo as u32, "hi": hi as u32,
            }));
        }
        let graph = serde_json::json!({
            "nodes": {
                "crate::old_fn": { "kind": "fn", "refs": refs, "fields": [] }
            },
            "edges": []
        });
        let graph_path = ws.join("state/rustc/canon_mini_agent/graph.json");
        std::fs::create_dir_all(graph_path.parent().unwrap()).unwrap();
        std::fs::write(&graph_path, serde_json::to_string_pretty(&graph).unwrap()).unwrap();

        let idx = crate::semantic::SemanticIndex::load(&ws, "canon_mini_agent").unwrap();
        let report = super::rename_symbols_via_semantic_spans(
            &ws,
            &idx,
            &[("crate::old_fn".to_string(), "crate::new_fn".to_string())],
        )
        .unwrap();

        assert!(report.replacements > 0, "expected at least one replacement");
        let updated = std::fs::read_to_string(&file).unwrap();
        // Span replacement: identifier occurrences renamed
        assert!(updated.contains("fn new_fn()"), "def should be renamed: {updated}");
        assert!(updated.contains("new_fn();"), "call site should be renamed: {updated}");
        // Attr string rewrite: serde rename value updated
        assert!(
            updated.contains("rename = \"new_fn\""),
            "attr string literal should be rewritten: {updated}"
        );
        assert!(
            !updated.contains("rename = \"old_fn\""),
            "old attr value should be gone: {updated}"
        );
    }
}

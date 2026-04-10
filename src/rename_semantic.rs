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
        let updated = apply_replacements(&original, &mut span_replacements)
            .with_context(|| format!("apply replacements for {}", file.display()))?;
        if updated != original {
            std::fs::write(&file, updated).with_context(|| format!("write {}", file.display()))?;
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SymbolSpan {
    start: usize,
    end: usize,
    line: usize,
    column: usize,
    end_line: usize,
    end_column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SymbolEntry {
    name: String,
    kind: String,
    file: String,
    span: SymbolSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SymbolsIndexFile {
    version: u32,
    symbols: Vec<SymbolEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RenameCandidate {
    name: String,
    kind: String,
    file: String,
    span: SymbolSpan,
    score: u32,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RenameCandidatesFile {
    version: u32,
    source_symbols_path: String,
    candidates: Vec<RenameCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PreparedRenameActionFile {
    version: u32,
    source_candidates_path: String,
    selected_index: usize,
    selected_candidate: RenameCandidate,
    rename_action: Value,
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn offset_to_line_col(text: &str, starts: &[usize], offset: usize) -> (usize, usize) {
    let idx = match starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let line_start = starts[idx];
    let line = idx + 1;
    let col = text[line_start..offset].chars().count() + 1;
    (line, col)
}

fn symbol_kind_from_name_owner(owner_kind: SyntaxKind) -> Option<&'static str> {
    match owner_kind {
        SyntaxKind::FN => Some("function"),
        SyntaxKind::STRUCT => Some("struct"),
        SyntaxKind::ENUM => Some("enum"),
        SyntaxKind::TRAIT => Some("trait"),
        SyntaxKind::TYPE_ALIAS => Some("type_alias"),
        SyntaxKind::CONST => Some("const"),
        SyntaxKind::STATIC => Some("static"),
        SyntaxKind::MODULE => Some("module"),
        SyntaxKind::UNION => Some("union"),
        SyntaxKind::VARIANT => Some("enum_variant"),
        SyntaxKind::RECORD_FIELD => Some("field"),
        SyntaxKind::TYPE_PARAM => Some("type_param"),
        _ => None,
    }
}

fn collect_rust_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if collect_rust_file_root(root, out) {
        return Ok(());
    }
    let entries = sorted_dir_entries(root)?;
    for entry in entries {
        collect_rust_dir_entry(entry, out)?;
    }
    Ok(())
}

fn collect_rust_file_root(root: &Path, out: &mut Vec<PathBuf>) -> bool {
    if !root.is_file() {
        return false;
    }
    if is_rust_file(root) {
        out.push(root.to_path_buf());
    }
    true
}

fn sorted_dir_entries(root: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))? {
        entries.push(entry?);
    }
    entries.sort_by(|a, b| a.path().cmp(&b.path()));
    Ok(entries)
}

fn collect_rust_dir_entry(entry: fs::DirEntry, out: &mut Vec<PathBuf>) -> Result<()> {
    let path = entry.path();
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
        if is_ignored_dir_entry(&entry) {
            return Ok(());
        }
        collect_rust_files(&path, out)?;
        return Ok(());
    }
    if file_type.is_file() && is_rust_file(&path) {
        out.push(path);
    }
    Ok(())
}

fn is_ignored_dir_entry(entry: &fs::DirEntry) -> bool {
    let name = entry.file_name();
    let name = name.to_string_lossy();
    is_ignored_dir(name.as_ref())
}

fn is_rust_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("rs")
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | ".idea" | ".vscode"
    )
}

/// Intent: pure_transform
/// Resource: declaration_symbols
/// Inputs: &std::path::Path, &std::path::Path, &str
/// Outputs: std::vec::Vec<tools::SymbolEntry>
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns no symbols for parse errors; emitted file paths are workspace-relative when possible
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
fn extract_decl_symbols(workspace: &Path, file_path: &Path, text: &str) -> Vec<SymbolEntry> {
    let parse = SourceFile::parse(text, Edition::CURRENT);
    if !parse.errors().is_empty() {
        return Vec::new();
    }
    let root = parse.tree();
    let starts = line_starts(text);
    let file_rel = file_path
        .strip_prefix(workspace)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file_path.to_string_lossy().replace('\\', "/"));
    let mut out = Vec::new();
    for token in root
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
    {
        if let Some(entry) = symbol_entry_from_token(text, &starts, &file_rel, token) {
            out.push(entry);
        }
    }
    out
}

fn symbol_entry_from_token(
    text: &str,
    starts: &[usize],
    file_rel: &str,
    token: SyntaxToken,
) -> Option<SymbolEntry> {
    if token.kind() != SyntaxKind::IDENT {
        return None;
    }
    let name_node = token.parent()?;
    if name_node.kind() != SyntaxKind::NAME {
        return None;
    }
    let owner = name_node.parent()?;
    let kind = symbol_kind_from_name_owner(owner.kind())?;
    let range = token.text_range();
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    let (line, column) = offset_to_line_col(text, starts, start);
    let (end_line, end_column) = offset_to_line_col(text, starts, end);
    Some(SymbolEntry {
        name: token.text().to_string(),
        kind: kind.to_string(),
        file: file_rel.to_string(),
        span: SymbolSpan {
            start,
            end,
            line,
            column,
            end_line,
            end_column,
        },
    })
}

fn handle_symbols_index_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let path_raw = action.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let out_raw = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("state/symbols.json");
    let scan_root = safe_join(workspace, path_raw)?;
    let out_path = safe_join(workspace, out_raw)?;
    let payload = build_symbols_index_payload(workspace, &scan_root)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create symbols output dir {}", parent.display()))?;
    }
    fs::write(
        &out_path,
        serde_json::to_string_pretty(&payload).context("serialize symbols index")?,
    )
    .with_context(|| format!("write {}", out_path.display()))?;
    Ok((
        false,
        format!(
            "symbols_index ok: output={} symbols={}",
            out_raw,
            payload.symbols.len()
        ),
    ))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path, &std::path::Path
/// Outputs: std::result::Result<tools::SymbolsIndexFile, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_symbols_index_payload(workspace: &Path, scan_root: &Path) -> Result<SymbolsIndexFile> {
    let mut files = Vec::new();
    collect_rust_files(scan_root, &mut files)?;
    files.sort();

    let mut symbols = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap_or_default();
        symbols.extend(extract_decl_symbols(workspace, &file, &text));
    }
    symbols.sort_by(|a, b| {
        (
            a.file.as_str(),
            a.span.start,
            a.span.end,
            a.kind.as_str(),
            a.name.as_str(),
        )
            .cmp(&(
                b.file.as_str(),
                b.span.start,
                b.span.end,
                b.kind.as_str(),
                b.name.as_str(),
            ))
    });
    symbols.dedup_by(|a, b| {
        a.file == b.file
            && a.span.start == b.span.start
            && a.span.end == b.span.end
            && a.kind == b.kind
            && a.name == b.name
    });

    Ok(SymbolsIndexFile {
        version: 1,
        symbols,
    })
}

fn ambiguous_name_reasons(name: &str) -> Vec<String> {
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

fn handle_symbols_rename_candidates_action(
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let (symbols_path_raw, out_raw, symbols_path, out_path) =
        parse_symbols_rename_candidates_paths(workspace, action)?;
    let symbols_file = load_symbols_index_file(&symbols_path)?;
    let prefixes_by_stem = build_function_prefixes_by_stem(&symbols_file);

    let identity_surface_names = rename_candidate_identity_surface_names();
    let identity_surface_files = rename_candidate_identity_surface_files();
    let candidates = collect_rename_candidates(
        &symbols_file,
        &prefixes_by_stem,
        &identity_surface_names,
        &identity_surface_files,
    );

    finalize_rename_candidates_output(candidates, &symbols_path_raw, &out_raw, &out_path)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(std::string::String, std::string::String, std::path::PathBuf, std::path::PathBuf), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_symbols_rename_candidates_paths(
    workspace: &Path,
    action: &Value,
) -> Result<(String, String, PathBuf, PathBuf)> {
    let symbols_path_raw = action
        .get("symbols_path")
        .and_then(|v| v.as_str())
        .unwrap_or("state/symbols.json")
        .to_string();
    let out_raw = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("state/rename_candidates.json")
        .to_string();
    let symbols_path = safe_join(workspace, &symbols_path_raw)?;
    let out_path = safe_join(workspace, &out_raw)?;
    Ok((symbols_path_raw, out_raw, symbols_path, out_path))
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<tools::SymbolsIndexFile, anyhow::Error>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_symbols_index_file(symbols_path: &Path) -> Result<SymbolsIndexFile> {
    let symbols_text = fs::read_to_string(symbols_path)
        .with_context(|| format!("read {}", symbols_path.display()))?;
    serde_json::from_str(&symbols_text).context("parse symbols index json")
}

/// Intent: pure_transform
/// Resource: function_prefix_index
/// Inputs: &tools::SymbolsIndexFile
/// Outputs: std::collections::BTreeMap<std::string::String, std::collections::BTreeSet<std::string::String>>
/// Effects: none
/// Forbidden: mutation
/// Invariants: includes only function symbols with split prefix/stem names; output ordering is deterministic
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
fn build_function_prefixes_by_stem(
    symbols_file: &SymbolsIndexFile,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut prefixes_by_stem: std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for sym in &symbols_file.symbols {
        if sym.kind == "function" {
            if let Some((prefix, stem)) = split_prefix_and_stem(&sym.name) {
                prefixes_by_stem
                    .entry(stem)
                    .or_default()
                    .insert(prefix.to_string());
            }
        }
    }
    prefixes_by_stem
}

fn rename_candidate_identity_surface_names() -> BTreeSet<&'static str> {
    ["id", "endpoint_id", "lane_id"].into_iter().collect()
}

fn rename_candidate_identity_surface_files() -> BTreeSet<&'static str> {
    [
        "src/constants.rs",
        "src/protocol.rs",
        "src/app.rs",
        "src/logging.rs",
    ]
    .into_iter()
    .collect()
}

fn collect_rename_candidates(
    symbols_file: &SymbolsIndexFile,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    identity_surface_names: &BTreeSet<&'static str>,
    identity_surface_files: &BTreeSet<&'static str>,
) -> Vec<RenameCandidate> {
    let mut candidates = Vec::new();
    for sym in &symbols_file.symbols {
        if let Some(candidate) = build_rename_candidate(
            sym,
            prefixes_by_stem,
            identity_surface_names,
            identity_surface_files,
        ) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// Intent: pure_transform
/// Resource: rename_candidate
/// Inputs: &tools::SymbolEntry, &std::collections::BTreeMap<std::string::String, std::collections::BTreeSet<std::string::String>>, &std::collections::BTreeSet<&str>, &std::collections::BTreeSet<&str>
/// Outputs: std::option::Option<tools::RenameCandidate>
/// Effects: none
/// Forbidden: mutation
/// Invariants: skipped symbols and symbols with no rename reasons produce None; candidates carry deterministic score from reasons
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
fn build_rename_candidate(
    sym: &SymbolEntry,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    identity_surface_names: &BTreeSet<&'static str>,
    identity_surface_files: &BTreeSet<&'static str>,
) -> Option<RenameCandidate> {
    if should_skip_rename_candidate_symbol(sym, identity_surface_names, identity_surface_files) {
        return None;
    }

    let reasons = rename_candidate_reasons(sym, prefixes_by_stem);
    if reasons.is_empty() {
        return None;
    }

    Some(RenameCandidate {
        name: sym.name.clone(),
        kind: sym.kind.clone(),
        file: sym.file.clone(),
        span: sym.span.clone(),
        score: score_rename_candidate_reasons(&reasons),
        reasons,
    })
}

fn should_skip_rename_candidate_symbol(
    sym: &SymbolEntry,
    identity_surface_names: &BTreeSet<&'static str>,
    identity_surface_files: &BTreeSet<&'static str>,
) -> bool {
    // Field-level symbols are currently not resolvable by the semantic rename tool:
    // `symbol_occurrences` delegates to `resolve_symbol_key`, which only matches graph
    // node keys/suffixes, while the graph does not expose record fields as standalone
    // node identities. Skip them here so prepared rename actions stay executable.
    if sym.kind == "field" {
        return true;
    }

    // Exclude conventional status/result enum variants that are semantically
    // meaningful and should not be mechanically renamed.
    if sym.kind == "enum_variant" && matches!(sym.name.as_str(), "Ok" | "Err" | "Some" | "None") {
        return true;
    }

    // Defense in depth: endpoint/protocol identity names are part of external routing,
    // persistence, and filename surfaces in known authority files. Exclude them even if
    // a future symbol-index/runtime mismatch reclassifies them away from `field`.
    identity_surface_names.contains(sym.name.as_str())
        && identity_surface_files.contains(sym.file.as_str())
}

fn rename_candidate_reasons(
    sym: &SymbolEntry,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Vec<String> {
    let mut reasons = ambiguous_name_reasons(&sym.name);
    if let Some(reason) = inconsistent_function_prefix_reason(sym, prefixes_by_stem) {
        reasons.push(reason);
    }
    reasons
}

fn inconsistent_function_prefix_reason(
    sym: &SymbolEntry,
    prefixes_by_stem: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
) -> Option<String> {
    if sym.kind != "function" {
        return None;
    }

    let (prefix, stem) = split_prefix_and_stem(&sym.name)?;
    let prefixes = prefixes_by_stem.get(&stem)?;
    if prefixes.len() <= 1 {
        return None;
    }

    let mut other = other_prefixes(prefixes, prefix);
    other.sort();
    Some(format!(
        "inconsistent verb prefix for stem '{stem}' (also: {})",
        other.join(", ")
    ))
}

fn other_prefixes(prefixes: &std::collections::BTreeSet<String>, prefix: &str) -> Vec<String> {
    prefixes
        .iter()
        .filter(|p| p.as_str() != prefix)
        .cloned()
        .collect()
}

fn finalize_rename_candidates_output(
    mut candidates: Vec<RenameCandidate>,
    symbols_path_raw: &str,
    out_raw: &str,
    out_path: &Path,
) -> Result<(bool, String)> {
    sort_and_dedup_rename_candidates(&mut candidates);
    let payload = RenameCandidatesFile {
        version: 1,
        source_symbols_path: symbols_path_raw.to_string(),
        candidates,
    };
    write_rename_candidates_payload(out_path, &payload)?;
    Ok((false, rename_candidates_success_message(out_raw, &payload)))
}

fn rename_candidates_success_message(out_raw: &str, payload: &RenameCandidatesFile) -> String {
    format!(
        "symbols_rename_candidates ok: output={} candidates={}",
        out_raw,
        payload.candidates.len()
    )
}

fn score_rename_candidate_reasons(reasons: &[String]) -> u32 {
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

fn sort_and_dedup_rename_candidates(candidates: &mut Vec<RenameCandidate>) {
    candidates.sort_by(|a, b| {
        (
            std::cmp::Reverse(a.score),
            a.file.as_str(),
            a.span.start,
            a.name.as_str(),
            a.kind.as_str(),
        )
            .cmp(&(
                std::cmp::Reverse(b.score),
                b.file.as_str(),
                b.span.start,
                b.name.as_str(),
                b.kind.as_str(),
            ))
    });
    candidates.dedup_by(|a, b| {
        a.file == b.file
            && a.span.start == b.span.start
            && a.span.end == b.span.end
            && a.name == b.name
            && a.kind == b.kind
    });
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &tools::RenameCandidatesFile
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn write_rename_candidates_payload(out_path: &Path, payload: &RenameCandidatesFile) -> Result<()> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create rename candidates output dir {}", parent.display()))?;
    }
    fs::write(
        out_path,
        serde_json::to_string_pretty(payload).context("serialize rename candidates json")?,
    )
    .with_context(|| format!("write {}", out_path.display()))?;
    Ok(())
}

fn handle_symbols_prepare_rename_action(
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let candidates_path_raw = action
        .get("candidates_path")
        .and_then(|v| v.as_str())
        .unwrap_or("state/rename_candidates.json");
    let out_raw = action
        .get("out")
        .and_then(|v| v.as_str())
        .unwrap_or("state/next_rename_action.json");
    let index = action.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let candidates_path = safe_join(workspace, candidates_path_raw)?;
    let out_path = safe_join(workspace, out_raw)?;
    let candidates_text = fs::read_to_string(&candidates_path)
        .with_context(|| format!("read {}", candidates_path.display()))?;
    let candidates_file: RenameCandidatesFile =
        serde_json::from_str(&candidates_text).context("parse rename candidates json")?;
    let selected = selected_rename_candidate(&candidates_file, index, candidates_path_raw)?;
    let rename_action = build_prepared_rename_action(&selected);
    let payload = PreparedRenameActionFile {
        version: 1,
        source_candidates_path: candidates_path_raw.to_string(),
        selected_index: index,
        selected_candidate: selected,
        rename_action,
    };
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create prepared rename output dir {}", parent.display()))?;
    }
    fs::write(
        &out_path,
        serde_json::to_string_pretty(&payload).context("serialize prepared rename action json")?,
    )
    .with_context(|| format!("write {}", out_path.display()))?;
    Ok((
        false,
        format!(
            "symbols_prepare_rename ok: output={} selected_index={} selected_name={}",
            out_raw, payload.selected_index, payload.selected_candidate.name
        ),
    ))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &tools::RenameCandidate
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_prepared_rename_action(selected: &RenameCandidate) -> Value {
    json!({
        "action": "rename_symbol",
        "old_symbol": selected.name,
        "new_symbol": format!("{}_renamed", selected.name),
        "question": "Does this selected candidate represent the exact symbol to rename across the crate without changing intended behavior?",
        "rationale": "Apply a span-backed rename candidate deterministically and validate impacts immediately after.",
        "predicted_next_actions": [
            {"action": "cargo_test", "intent": "Run focused tests for the touched area after rename."},
            {"action": "run_command", "intent": "Run cargo check to verify workspace compile health."}
        ]
    })
}

fn selected_rename_candidate(
    candidates_file: &RenameCandidatesFile,
    index: usize,
    candidates_path_raw: &str,
) -> Result<RenameCandidate> {
    if candidates_file.candidates.is_empty() {
        bail!(
            "symbols_prepare_rename: no candidates in {}",
            candidates_path_raw
        );
    }
    if index >= candidates_file.candidates.len() {
        bail!(
            "symbols_prepare_rename: index {} out of range (candidates={})",
            index,
            candidates_file.candidates.len()
        );
    }
    Ok(candidates_file.candidates[index].clone())
}

fn handle_rename_symbol_action(
    role: &str,
    step: usize,
    workspace: &Path,
    action: &Value,
) -> Result<(bool, String)> {
    let idx = load_semantic(workspace, action)?;
    let crate_name = semantic_crate_name(action);
    let pairs = parse_rename_symbol_pairs(action, &crate_name)?;
    let rename_env = capture_rename_symbol_environment(workspace)?;

    let report =
        crate::rename_semantic::rename_symbols_via_semantic_spans(workspace, &idx, &pairs)?;
    eprintln!(
        "[{role}] step={} rename_symbol spans pairs={} replacements={} files={}",
        step,
        pairs.len(),
        report.replacements,
        report.touched_files.len()
    );

    // Post-rename cargo check.  On failure roll back every touched file to
    // its pre-rename state via `git checkout <head> -- <file>...` and
    // surface the compiler output so the agent can diagnose the problem.
    // Skipped when the workspace has no Cargo.toml (e.g. unit-test fixtures).
    run_post_rename_cargo_check(workspace, &rename_env, &report)?;

    Ok((
        false,
        format!(
            "rename_symbol ok: pairs={} replacements={} touched_files={} cargo_check={}",
            pairs.len(),
            report.replacements,
            report.touched_files.len(),
            if rename_env.has_cargo {
                "ok"
            } else {
                "skipped"
            },
        ),
    ))
}

struct RenameSymbolEnvironment {
    in_git: bool,
    has_cargo: bool,
    head: String,
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<std::vec::Vec<(std::string::String, std::string::String)>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_rename_symbol_pairs(action: &Value, crate_name: &str) -> Result<Vec<(String, String)>> {
    reject_legacy_rename_fields(action)?;

    if let Some(arr) = action.get("renames").and_then(|v| v.as_array()) {
        return parse_bulk_renames(arr, crate_name);
    }

    parse_single_rename(action, crate_name)
}

fn reject_legacy_rename_fields(action: &Value) -> Result<()> {
    if has_legacy_rename_field(action) {
        bail!("rename_symbol v2 uses `old_symbol`/`new_symbol` (or `renames`) and rustc graph spans; line/column payloads are deprecated");
    }
    Ok(())
}

fn has_legacy_rename_field(action: &Value) -> bool {
    ["path", "line", "column", "old_name", "new_name"]
        .iter()
        .any(|field| action.get(field).is_some())
}

/// Intent: pure_transform
/// Resource: bulk_rename_pairs
/// Inputs: &[serde_json::Value], &str
/// Outputs: std::result::Result<std::vec::Vec<(std::string::String, std::string::String)>, anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: requires non-empty rename array; each item must contain old/new strings normalized for the crate
/// Failure: bails on empty renames, missing fields, or invalid normalized pairs
/// Provenance: rustc:facts + rustc:docstring
fn parse_bulk_renames(arr: &[Value], crate_name: &str) -> Result<Vec<(String, String)>> {
    if arr.is_empty() {
        bail!("rename_symbol: `renames` must not be empty");
    }

    let mut pairs = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        pairs.push(parse_bulk_rename_pair(item, crate_name, i)?);
    }
    Ok(pairs)
}

fn rename_pair_field<'a>(item: &'a Value, index: usize, field: &str) -> Result<&'a str> {
    item.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("rename_symbol: renames[{index}] missing `{field}`"))
}

fn parse_bulk_rename_pair(item: &Value, crate_name: &str, index: usize) -> Result<(String, String)> {
    let old = rename_pair_field(item, index, "old")?;
    let new = rename_pair_field(item, index, "new")?;

    normalize_pair(crate_name, old, new)
}

/// Intent: pure_transform
/// Resource: single_symbol_rename
/// Inputs: &serde_json::Value, &str
/// Outputs: std::result::Result<std::vec::Vec<(std::string::String, std::string::String)>, anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: requires non-empty old_symbol and new_symbol fields and normalizes the pair for the target crate
/// Failure: returns validation or normalization errors
/// Provenance: rustc:facts + rustc:docstring
fn parse_single_rename(action: &Value, crate_name: &str) -> Result<Vec<(String, String)>> {
    let old = action
        .get("old_symbol")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!("rename_symbol missing non-empty `old_symbol` (or provide `renames`)")
        })?;

    let new = action
        .get("new_symbol")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!("rename_symbol missing non-empty `new_symbol` (or provide `renames`)")
        })?;

    let (old, new) = normalize_pair(crate_name, old, new)?;
    Ok(vec![(old, new)])
}

/// Intent: pure_transform
/// Resource: rename_symbol_pair
/// Inputs: &str, &str, &str
/// Outputs: std::result::Result<(std::string::String, std::string::String), anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: strips semantic crate prefixes and returns non-empty old/new symbol pair
/// Failure: bails when either normalized symbol is empty
/// Provenance: rustc:facts + rustc:docstring
fn normalize_pair(crate_name: &str, old: &str, new: &str) -> Result<(String, String)> {
    let old = strip_semantic_crate_prefix(crate_name, old);
    let new = strip_semantic_crate_prefix(crate_name, new);
    if old.is_empty() || new.is_empty() {
        bail!("rename_symbol requires non-empty old/new symbols");
    }
    Ok((old.to_string(), new.to_string()))
}

fn capture_rename_symbol_environment(workspace: &Path) -> Result<RenameSymbolEnvironment> {
    let in_git = workspace.join(".git").exists();
    let has_cargo = workspace.join("Cargo.toml").exists();
    let head = load_git_head(workspace, in_git)?;
    Ok(RenameSymbolEnvironment {
        in_git,
        has_cargo,
        head,
    })
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, bool
/// Outputs: std::result::Result<std::string::String, anyhow::Error>
/// Effects: spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_git_head(workspace: &Path, in_git: bool) -> Result<String> {
    if !in_git {
        return Ok(String::new());
    }

    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .context("git rev-parse HEAD")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &std::path::Path, &tools::RenameSymbolEnvironment, &rename_semantic::RenameReport
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: spawns_process
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn run_post_rename_cargo_check(
    workspace: &Path,
    rename_env: &RenameSymbolEnvironment,
    report: &crate::rename_semantic::RenameReport,
) -> Result<()> {
    if !rename_env.has_cargo {
        return Ok(());
    }

    let check_out = Command::new("cargo")
        .args(["check", "--workspace"])
        .current_dir(workspace)
        .output()
        .context("cargo check --workspace")?;

    if check_out.status.success() {
        return Ok(());
    }

    if rename_env.in_git && !rename_env.head.is_empty() {
        let mut restore_args = vec![
            "checkout".to_string(),
            rename_env.head.clone(),
            "--".to_string(),
        ];
        for f in &report.touched_files {
            restore_args.push(f.to_string_lossy().into_owned());
        }
        let restore_args_ref: Vec<&str> = restore_args.iter().map(String::as_str).collect();
        let _ = Command::new("git")
            .args(&restore_args_ref)
            .current_dir(workspace)
            .output();
    }

    let stderr = String::from_utf8_lossy(&check_out.stderr);
    let stdout = String::from_utf8_lossy(&check_out.stdout);
    let compiler_output = format!("{stdout}{stderr}");
    persist_rename_symbol_errors(workspace, &compiler_output);

    bail!(
        "rename_symbol: cargo check failed after rename — rolled back {} file(s) to {}. Errors written to state/rename_errors.txt.\n{}",
        report.touched_files.len(),
        rename_env.head,
        compiler_output,
    );
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_rename_symbol_errors(workspace: &Path, compiler_output: &str) {
    let errors_path = workspace.join("state/rename_errors.txt");
    persist_text_file(&errors_path, compiler_output);
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str
/// Outputs: ()
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_text_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, content);
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn validate_state_file_schema(file_path: &str, content: &str) -> Option<String> {
    use crate::reports::{DiagnosticsReport, ViolationsReport};
    use jsonschema::JSONSchema;
    use schemars::schema_for;
    use std::sync::OnceLock;

    let json_value = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(v) => v,
        Err(e) => {
            return Some(format!(
                "apply_patch rejected: file is not valid JSON after patch: {e}"
            ))
        }
    };

    let diag = diagnostics_file();
    let legacy_diag = "DIAGNOSTICS.json";

    if file_path == diag || file_path == legacy_diag {
        static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
        let compiled = SCHEMA.get_or_init(|| {
            let mut val =
                serde_json::to_value(schema_for!(DiagnosticsReport)).expect("diagnostics schema");
            // Enforce no additional properties beyond the canonical four fields.
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            JSONSchema::compile(&val).expect("compile diagnostics schema")
        });
        if let Err(errors) = compiled.validate(&json_value) {
            let msgs: Vec<String> = errors.take(5).map(|e| e.to_string()).collect();
            return Some(format!(
                "apply_patch rejected: DiagnosticsReport schema violation\n{}\n\
                 Canonical fields: status, inputs_scanned, ranked_failures, planner_handoff.\n\
                 No additional fields are permitted. Remove any extra fields and retry.",
                msgs.join("\n")
            ));
        }
    } else if file_path == VIOLATIONS_FILE {
        static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
        let compiled = SCHEMA.get_or_init(|| {
            let mut val =
                serde_json::to_value(schema_for!(ViolationsReport)).expect("violations schema");
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            JSONSchema::compile(&val).expect("compile violations schema")
        });
        if let Err(errors) = compiled.validate(&json_value) {
            let msgs: Vec<String> = errors.take(5).map(|e| e.to_string()).collect();
            return Some(format!(
                "apply_patch rejected: ViolationsReport schema violation\n{}\n\
                 Canonical fields: status, summary, violations (each with: id, title, severity, \
                 evidence, issue, impact, required_fix, files).\n\
                 No additional fields are permitted. Remove any extra fields and retry.",
                msgs.join("\n")
            ));
        }
    }

    None
}

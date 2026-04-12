/// Lessons pipeline: two-stage pattern learning system.
///
/// Stage 1 — Synthesis (automatic, runs after each diagnostics cycle):
///   Reads the action log, detects failure patterns AND successful action sequences,
///   and writes/merges results into `agent_state/lessons_candidates.json`.
///   Candidates are born with `status: "pending"`.
///
/// Stage 2 — Promotion (LLM-driven, via the `lessons` action):
///   The diagnostics agent reviews pending candidates and promotes or rejects them.
///   Promoted candidates are merged into `agent_state/lessons.json`, which is
///   injected into every planner/solo prompt via `read_lessons_or_empty`.
///   Rejected candidates persist with `status: "rejected"` so they are not re-surfaced.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prompt_inputs::LessonsArtifact;

// ── File paths ────────────────────────────────────────────────────────────────

const LESSONS_FILE: &str = "agent_state/lessons.json";
const CANDIDATES_FILE: &str = "agent_state/lessons_candidates.json";
const ACTION_LOG_SUBPATH: &str = "default/actions.jsonl";

// ── Tuning knobs ──────────────────────────────────────────────────────────────

/// Lines to read from the tail of the action log each synthesis run.
const MAX_LINES_TO_SCAN: usize = 4000;
/// Minimum times a failure pattern must recur before it becomes a candidate.
const MIN_FAILURE_OCCURRENCES: usize = 2;
/// Minimum times a bigram must recur before it becomes a success-sequence candidate.
const MIN_BIGRAM_OCCURRENCES: usize = 4;
/// Minimum times a trigram must recur before it becomes a success-sequence candidate.
const MIN_TRIGRAM_OCCURRENCES: usize = 3;
/// Max candidates kept in the pending/promoted pool per category.
const MAX_CANDIDATES_PER_KIND: usize = 8;

// ── Data structures ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Pending,
    Promoted,
    Rejected,
}

impl Default for CandidateStatus {
    fn default() -> Self {
        CandidateStatus::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonsCandidate {
    /// Stable ID derived from (kind, pattern) — same pattern across runs → same ID.
    pub id: String,
    /// "failure_pattern" | "success_sequence"
    pub kind: String,
    /// Human-readable summary of what was detected.
    pub description: String,
    /// How many times this pattern was observed in the last scan window.
    pub occurrences: usize,
    /// A concrete fix hint or workflow note (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_or_note: Option<String>,
    /// Lifecycle state.
    #[serde(default)]
    pub status: CandidateStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LessonsCandidatesFile {
    pub version: u32,
    pub last_synthesized_ms: u64,
    pub candidates: Vec<LessonsCandidate>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Called after each successful diagnostics cycle.  Synthesizes failure patterns
/// and success sequences from the action log and merges them into
/// `agent_state/lessons_candidates.json`.  Does not modify `lessons.json`.
pub fn maybe_synthesize_lessons(workspace: &Path) {
    if let Err(e) = synthesize_candidates(workspace) {
        eprintln!("[lessons] synthesis error: {e:#}");
    }
}

/// Handle a `lessons` action dispatched from the main tool executor.
pub fn handle_lessons_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let op = action
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("read_candidates");

    match op {
        "read_candidates" => op_read_candidates(workspace),
        "promote" => op_promote(workspace, action),
        "reject" => op_reject(workspace, action),
        "read" => op_read_lessons(workspace),
        "write" => op_write_lessons(workspace, action),
        other => anyhow::bail!(
            "unknown lessons op '{other}' — use: read_candidates | promote | reject | read | write"
        ),
    }
}

// ── Op implementations ────────────────────────────────────────────────────────

fn op_read_candidates(workspace: &Path) -> Result<(bool, String)> {
    let path = candidates_path(workspace);
    if !path.exists() {
        return Ok((false, "(no lessons candidates yet — synthesis runs after the next diagnostics cycle)".to_string()));
    }
    let raw = std::fs::read_to_string(&path)?;
    let file: LessonsCandidatesFile = serde_json::from_str(&raw).unwrap_or_default();
    let pending: Vec<&LessonsCandidate> = file
        .candidates
        .iter()
        .filter(|c| c.status == CandidateStatus::Pending)
        .collect();
    if pending.is_empty() {
        return Ok((false, "lessons_candidates: no pending candidates (all have been promoted or rejected)".to_string()));
    }
    let out = serde_json::to_string_pretty(&pending)?;
    Ok((false, format!("lessons_candidates pending ({} items):\n{out}", pending.len())))
}

fn op_promote(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let candidate_id = action
        .get("candidate_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("lessons promote requires 'candidate_id' (id string or \"all\")"))?;

    let mut cfile = load_candidates(workspace);
    let promote_all = candidate_id == "all";

    let mut promoted_count = 0usize;
    let mut artifact = load_lessons(workspace);

    for c in cfile.candidates.iter_mut() {
        if c.status != CandidateStatus::Pending {
            continue;
        }
        if !promote_all && c.id != candidate_id {
            continue;
        }
        merge_candidate_into_artifact(&mut artifact, c);
        c.status = CandidateStatus::Promoted;
        promoted_count += 1;
    }

    if promoted_count == 0 {
        if promote_all {
            return Ok((false, "lessons promote: no pending candidates to promote".to_string()));
        } else {
            anyhow::bail!("lessons promote: candidate '{candidate_id}' not found or not pending");
        }
    }

    // Refresh summary.
    artifact.summary = build_artifact_summary(&artifact);

    save_candidates(workspace, &cfile)?;
    save_lessons(workspace, &artifact)?;

    Ok((false, format!("lessons promote: {promoted_count} candidate(s) promoted to lessons.json")))
}

fn op_reject(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let candidate_id = action
        .get("candidate_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("lessons reject requires 'candidate_id'"))?;

    let mut cfile = load_candidates(workspace);
    let changed = mark_candidate_status(&mut cfile, candidate_id, CandidateStatus::Rejected);
    if !changed {
        anyhow::bail!("lessons reject: candidate '{candidate_id}' not found");
    }
    save_candidates(workspace, &cfile)?;
    Ok((false, format!("lessons reject: candidate '{candidate_id}' marked rejected")))
}

fn op_read_lessons(workspace: &Path) -> Result<(bool, String)> {
    let path = lessons_path(workspace);
    if !path.exists() {
        return Ok((false, "(lessons.json does not exist yet — promote candidates to create it)".to_string()));
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok((false, format!("lessons.json:\n{raw}")))
}

fn op_write_lessons(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    let lessons_val = action
        .get("lessons")
        .ok_or_else(|| anyhow::anyhow!("lessons write requires a 'lessons' object with summary/failures/fixes/required_actions"))?;
    let artifact: LessonsArtifact = serde_json::from_value(lessons_val.clone())
        .map_err(|e| anyhow::anyhow!("invalid lessons object: {e}"))?;
    save_lessons(workspace, &artifact)?;
    Ok((false, "lessons write ok".to_string()))
}

// ── Core synthesis ────────────────────────────────────────────────────────────

fn synthesize_candidates(workspace: &Path) -> Result<()> {
    let agent_state = crate::constants::agent_state_dir();
    let log_path = Path::new(agent_state).join(ACTION_LOG_SUBPATH);
    if !log_path.exists() {
        return Ok(());
    }

    let entries = read_tail_entries(&log_path, MAX_LINES_TO_SCAN);
    if entries.is_empty() {
        return Ok(());
    }

    let failure_candidates = detect_failure_candidates(&entries);
    let sequence_candidates = detect_success_sequences(&entries);

    if failure_candidates.is_empty() && sequence_candidates.is_empty() {
        return Ok(());
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Merge into the existing candidates file (preserves rejected/promoted status).
    let mut cfile = load_candidates(workspace);
    cfile.last_synthesized_ms = now_ms;
    cfile.version = 1;

    merge_candidates_into_file(&mut cfile, failure_candidates);
    merge_candidates_into_file(&mut cfile, sequence_candidates);

    // Prune excess pending candidates per kind (keep highest-occurrence ones).
    prune_excess_pending(&mut cfile);

    save_candidates(workspace, &cfile)?;
    eprintln!(
        "[lessons] candidates synthesized — {} total ({} pending)",
        cfile.candidates.len(),
        cfile.candidates.iter().filter(|c| c.status == CandidateStatus::Pending).count()
    );
    Ok(())
}

// ── Failure pattern detection ─────────────────────────────────────────────────

fn detect_failure_candidates(entries: &[Value]) -> Vec<LessonsCandidate> {
    let mut map: HashMap<(String, String), usize> = HashMap::new();
    let mut fix_map: HashMap<(String, String), Option<String>> = HashMap::new();

    for entry in entries {
        if entry.get("kind").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        if entry.get("phase").and_then(|v| v.as_str()) != Some("result") {
            continue;
        }
        if entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(true) {
            continue;
        }
        let action = entry
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let text = entry.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let pattern = normalize_error(text);
        let key = (action.clone(), pattern.clone());
        *map.entry(key.clone()).or_default() += 1;
        fix_map.entry(key).or_insert_with(|| schema_fix_hint(&action, &pattern));
    }

    let mut results: Vec<LessonsCandidate> = map
        .into_iter()
        .filter(|(_, count)| *count >= MIN_FAILURE_OCCURRENCES)
        .map(|((action, pattern), occurrences)| {
            let fix = fix_map.get(&(action.clone(), pattern.clone())).and_then(|v| v.clone());
            let description = format!(
                "`{action}` action: {pattern} ({occurrences} occurrence{})",
                if occurrences == 1 { "" } else { "s" }
            );
            LessonsCandidate {
                id: stable_id("failure", &format!("{action}:{pattern}")),
                kind: "failure_pattern".to_string(),
                description,
                occurrences,
                fix_or_note: fix,
                status: CandidateStatus::Pending,
            }
        })
        .collect();

    results.sort_by(|a, b| b.occurrences.cmp(&a.occurrences));
    results
}

// ── Success sequence detection ────────────────────────────────────────────────

fn detect_success_sequences(entries: &[Value]) -> Vec<LessonsCandidate> {
    // Collect a flat list of successful tool action names in order.
    let successes: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.get("kind").and_then(|v| v.as_str()) == Some("tool")
                && e.get("phase").and_then(|v| v.as_str()) == Some("result")
                && e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .filter_map(|e| e.get("action").and_then(|v| v.as_str()).map(str::to_string))
        .collect();

    let mut bigrams: HashMap<(String, String), usize> = HashMap::new();
    let mut trigrams: HashMap<(String, String, String), usize> = HashMap::new();

    for window in successes.windows(2) {
        let a = window[0].clone();
        let b = window[1].clone();
        // Skip same-action repetitions (read_file, read_file) — not a useful pattern.
        if a == b {
            continue;
        }
        *bigrams.entry((a, b)).or_default() += 1;
    }
    for window in successes.windows(3) {
        let a = window[0].clone();
        let b = window[1].clone();
        let c = window[2].clone();
        // Skip trivial or degenerate sequences.
        if a == b || b == c {
            continue;
        }
        *trigrams.entry((a, b, c)).or_default() += 1;
    }

    let mut results: Vec<LessonsCandidate> = Vec::new();

    for ((a, b), count) in bigrams {
        if count < MIN_BIGRAM_OCCURRENCES {
            continue;
        }
        let key = format!("{a}→{b}");
        let note = sequence_workflow_note(&a, &b, None);
        results.push(LessonsCandidate {
            id: stable_id("seq2", &key),
            kind: "success_sequence".to_string(),
            description: format!("Action sequence: {a} → {b} ({count} occurrences)"),
            occurrences: count,
            fix_or_note: note,
            status: CandidateStatus::Pending,
        });
    }

    for ((a, b, c), count) in trigrams {
        if count < MIN_TRIGRAM_OCCURRENCES {
            continue;
        }
        let key = format!("{a}→{b}→{c}");
        let note = sequence_workflow_note(&a, &b, Some(&c));
        results.push(LessonsCandidate {
            id: stable_id("seq3", &key),
            kind: "success_sequence".to_string(),
            description: format!("Action sequence: {a} → {b} → {c} ({count} occurrences)"),
            occurrences: count,
            fix_or_note: note,
            status: CandidateStatus::Pending,
        });
    }

    results.sort_by(|a, b| b.occurrences.cmp(&a.occurrences));
    results
}

// ── Merge helpers ─────────────────────────────────────────────────────────────

fn merge_candidates_into_file(cfile: &mut LessonsCandidatesFile, new_ones: Vec<LessonsCandidate>) {
    for new_c in new_ones {
        if let Some(existing) = cfile.candidates.iter_mut().find(|c| c.id == new_c.id) {
            // Update occurrence count; keep existing status.
            existing.occurrences = new_c.occurrences;
            existing.description = new_c.description;
            if new_c.fix_or_note.is_some() {
                existing.fix_or_note = new_c.fix_or_note;
            }
        } else {
            cfile.candidates.push(new_c);
        }
    }
}

fn prune_excess_pending(cfile: &mut LessonsCandidatesFile) {
    // Keep at most MAX_CANDIDATES_PER_KIND pending candidates per kind,
    // retaining the highest-occurrence ones.
    for kind in &["failure_pattern", "success_sequence"] {
        let mut indices: Vec<usize> = cfile
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind.as_str() == *kind && c.status == CandidateStatus::Pending)
            .map(|(i, _)| i)
            .collect();
        if indices.len() <= MAX_CANDIDATES_PER_KIND {
            continue;
        }
        // Sort by occurrences descending; drop the excess (lowest-occurrence pending ones).
        indices.sort_by(|&a, &b| {
            cfile.candidates[b]
                .occurrences
                .cmp(&cfile.candidates[a].occurrences)
        });
        let to_drop: std::collections::HashSet<usize> =
            indices[MAX_CANDIDATES_PER_KIND..].iter().copied().collect();
        let mut i = 0;
        cfile.candidates.retain(|_| {
            let keep = !to_drop.contains(&i);
            i += 1;
            keep
        });
    }
}

fn merge_candidate_into_artifact(artifact: &mut LessonsArtifact, c: &LessonsCandidate) {
    match c.kind.as_str() {
        "failure_pattern" => {
            if !artifact.failures.contains(&c.description) {
                artifact.failures.push(c.description.clone());
            }
            if let Some(fix) = &c.fix_or_note {
                if !artifact.fixes.contains(fix) {
                    artifact.fixes.push(fix.clone());
                }
            }
        }
        "success_sequence" => {
            let entry = c
                .fix_or_note
                .as_deref()
                .unwrap_or(&c.description)
                .to_string();
            if !artifact.required_actions.contains(&entry) {
                artifact.required_actions.push(entry);
            }
        }
        _ => {}
    }
}

fn mark_candidate_status(
    cfile: &mut LessonsCandidatesFile,
    id: &str,
    status: CandidateStatus,
) -> bool {
    if let Some(c) = cfile.candidates.iter_mut().find(|c| c.id == id) {
        c.status = status;
        true
    } else {
        false
    }
}

fn build_artifact_summary(artifact: &LessonsArtifact) -> String {
    let f = artifact.failures.len();
    let fix = artifact.fixes.len();
    let req = artifact.required_actions.len();
    format!("{f} failure patterns, {fix} fixes, {req} required actions encoded from promoted candidates.")
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

fn candidates_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(CANDIDATES_FILE)
}

fn lessons_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(LESSONS_FILE)
}

fn load_candidates(workspace: &Path) -> LessonsCandidatesFile {
    let path = candidates_path(workspace);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_candidates(workspace: &Path, cfile: &LessonsCandidatesFile) -> Result<()> {
    let path = candidates_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let text = serde_json::to_string_pretty(cfile)?;
    std::fs::write(&path, text)?;
    Ok(())
}

fn load_lessons(workspace: &Path) -> LessonsArtifact {
    let path = lessons_path(workspace);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_lessons(workspace: &Path, artifact: &LessonsArtifact) -> Result<()> {
    let path = lessons_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let text = serde_json::to_string_pretty(artifact)?;
    std::fs::write(&path, text)?;
    Ok(())
}

// ── Utility ───────────────────────────────────────────────────────────────────

/// Generate a stable short ID for a pattern by hashing its key.
fn stable_id(prefix: &str, key: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    prefix.hash(&mut h);
    format!("{prefix}_{:016x}", <std::collections::hash_map::DefaultHasher as Hasher>::finish(&h))
}

fn read_tail_entries(path: &Path, max_lines: usize) -> Vec<Value> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().flatten().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn normalize_error(text: &str) -> String {
    let first_line = text
        .trim_start_matches("Error executing action: ")
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim();

    let mut s = first_line.to_string();

    // Collapse specific IDs into <id> so identical structural errors group together.
    for prefix in &[
        "task not found: ",
        "issue not found: ",
        "objective not found: ",
        "issue id already exists: ",
        "task already exists: ",
        "objective id already exists: ",
    ] {
        if let Some(pos) = s.find(prefix) {
            s.truncate(pos + prefix.len());
            s.push_str("<id>");
            break;
        }
    }
    // Strip "requested_raw=..." suffix.
    if let Some(pos) = s.find(", requested_raw=") {
        s.truncate(pos);
    }
    // Collapse quoted string values in type errors.
    if let Some(start) = s.find("invalid type: string \"") {
        let tail: String = s[start + 22..].to_string();
        if let Some(end) = tail.find('"') {
            s.truncate(start + 22);
            s.push_str(&tail[end + 1..]);
            s = s.replace("invalid type: string \"", "invalid type: string");
        }
    }
    if s.len() > 120 {
        s.truncate(120);
    }
    s
}

fn schema_fix_hint(action: &str, pattern: &str) -> Option<String> {
    match action {
        "plan" => {
            if pattern.contains("update_task missing task") {
                Some("plan update_task: wrap fields in a `task` object — `{\"op\":\"update_task\",\"task\":{\"id\":\"<id>\",\"status\":\"<status>\"}}`".into())
            } else if pattern.contains("create_task") && (pattern.contains("missing task") || pattern.contains("does not accept task_id")) {
                Some("plan create_task: use `task.id` inside a nested `task` object, not a top-level `task_id` — `{\"op\":\"create_task\",\"task\":{\"id\":\"<id>\",\"description\":\"<desc>\"}}`".into())
            } else if pattern.contains("replace_plan missing plan") {
                Some("plan replace_plan: requires a top-level `plan` object containing the full plan".into())
            } else if pattern.contains("task not found") {
                Some("plan: verify the task ID exists in PLAN.json with read_file before referencing it in update_task or delete_task".into())
            } else {
                None
            }
        }
        "issue" => {
            if pattern.contains("missing 'issue' field") {
                Some("issue create: nest all fields under an `issue` key — `{\"op\":\"create\",\"issue\":{\"id\":\"<id>\",\"title\":\"<t>\",\"status\":\"open\",\"kind\":\"<k>\",\"priority\":\"<p>\",\"description\":\"<d>\"}}`".into())
            } else if pattern.contains("missing field `id`") || pattern.contains("missing field `status`") {
                Some("issue: required fields are id, title, status, kind, priority, description — all must be non-empty strings".into())
            } else if pattern.contains("invalid type: boolean") {
                Some("issue: all field values must be strings — use quoted strings, not bare booleans or numbers".into())
            } else if pattern.contains("missing 'updates' object") {
                Some("issue update: wrap changes under an `updates` key — `{\"op\":\"update\",\"issue_id\":\"<id>\",\"updates\":{\"status\":\"resolved\"}}`".into())
            } else if pattern.contains("invalid type") {
                Some("issue: array fields (evidence) must be JSON arrays `[\"item\"]`, not plain strings".into())
            } else {
                None
            }
        }
        "objectives" => {
            if pattern.contains("create_objective missing objective") {
                Some("objectives create_objective: nest fields under an `objective` key — `{\"op\":\"create_objective\",\"objective\":{\"id\":\"<id>\",\"title\":\"<t>\",\"status\":\"active\"}}`".into())
            } else if pattern.contains("update_objective missing updates") {
                Some("objectives update_objective: requires both `objective_id` and an `updates` object — `{\"op\":\"update_objective\",\"objective_id\":\"<id>\",\"updates\":{...}}`".into())
            } else if pattern.contains("update_objective missing objective_id") {
                Some("objectives update_objective: the `objective_id` field is required alongside `updates`".into())
            } else if pattern.contains("invalid type") {
                Some("objectives: `verification` and `checklist` fields must be JSON arrays `[\"item\"]`, not plain strings".into())
            } else {
                None
            }
        }
        "symbol_window" => {
            if pattern.contains("not found in graph") || pattern.contains("not available") {
                Some("symbol_window: if symbol is not found, first run semantic_map to discover exact symbol paths, then retry with the fully-qualified path".into())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn sequence_workflow_note(a: &str, b: &str, c: Option<&str>) -> Option<String> {
    match (a, b, c) {
        ("semantic_map", "symbol_window", _) =>
            Some("When inspecting a symbol: run semantic_map to get the crate overview, then symbol_window with the exact path to read the definition".into()),
        ("symbol_window", "apply_patch", _) | ("read_file", "apply_patch", _) =>
            Some("Always read the target symbol/file immediately before apply_patch to ensure the patch context lines are current".into()),
        ("apply_patch", "cargo_test", Some("cargo_clippy")) =>
            Some("Full verification loop: apply_patch → cargo_test → cargo_clippy confirms both correctness and lint compliance".into()),
        ("apply_patch", "cargo_test", _) =>
            Some("After apply_patch, run cargo_test to validate the change compiles and tests pass".into()),
        ("cargo_fmt", "cargo_test", _) =>
            Some("After cargo_fmt, run cargo_test to confirm formatting changes don't break compilation".into()),
        (_, _, Some("cargo_test")) if a == "apply_patch" || b == "apply_patch" =>
            Some("Standard edit-verify loop: read/inspect → apply_patch → cargo_test".into()),
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(action: &str, ok: bool, text: &str) -> Value {
        serde_json::json!({
            "kind": "tool",
            "phase": "result",
            "action": action,
            "ok": ok,
            "text": text,
        })
    }

    #[test]
    fn failure_candidates_detected_above_threshold() {
        let entries = vec![
            tool_result("issue", false, "Error executing action: issue create missing 'issue' field"),
            tool_result("issue", false, "Error executing action: issue create missing 'issue' field"),
            tool_result("plan", false, "Error executing action: plan update_task missing task\n..."),
            tool_result("plan", false, "Error executing action: plan update_task missing task\n..."),
            tool_result("plan", true, "plan ok"),
        ];
        let candidates = detect_failure_candidates(&entries);
        assert!(candidates.iter().any(|c| c.description.contains("issue")));
        assert!(candidates.iter().any(|c| c.description.contains("plan")));
        assert!(candidates.iter().all(|c| c.kind == "failure_pattern"));
    }

    #[test]
    fn single_occurrence_failures_excluded() {
        let entries = vec![
            tool_result("plan", false, "Error executing action: plan task not found: T_foo"),
        ];
        let candidates = detect_failure_candidates(&entries);
        assert!(candidates.is_empty(), "single-occurrence errors should not become candidates");
    }

    #[test]
    fn different_task_ids_collapse_to_same_candidate() {
        let entries = vec![
            tool_result("plan", false, "Error executing action: plan task not found: T_foo_bar"),
            tool_result("plan", false, "Error executing action: plan task not found: T_baz_qux"),
        ];
        let candidates = detect_failure_candidates(&entries);
        assert_eq!(candidates.len(), 1, "different IDs for the same structural error should collapse");
        assert!(candidates[0].description.contains("<id>"));
    }

    #[test]
    fn success_sequence_bigrams_detected() {
        let mut entries: Vec<Value> = Vec::new();
        for _ in 0..5 {
            entries.push(tool_result("semantic_map", true, "ok"));
            entries.push(tool_result("symbol_window", true, "ok"));
        }
        let candidates = detect_success_sequences(&entries);
        assert!(
            candidates.iter().any(|c| c.description.contains("semantic_map") && c.description.contains("symbol_window")),
            "semantic_map→symbol_window bigram should be detected"
        );
        assert!(candidates.iter().all(|c| c.kind == "success_sequence"));
    }

    #[test]
    fn same_action_repeated_not_a_sequence() {
        let mut entries = Vec::new();
        for _ in 0..10 {
            entries.push(tool_result("read_file", true, "ok"));
        }
        let candidates = detect_success_sequences(&entries);
        assert!(candidates.is_empty(), "same-action repetitions should not produce candidates");
    }

    #[test]
    fn promote_moves_candidate_to_lessons_json() {
        use std::fs;
        let workspace = tempdir();
        fs::create_dir_all(workspace.join("agent_state")).unwrap();

        // Seed a candidates file with one pending failure candidate.
        let cfile = LessonsCandidatesFile {
            version: 1,
            last_synthesized_ms: 0,
            candidates: vec![LessonsCandidate {
                id: "test_id_001".to_string(),
                kind: "failure_pattern".to_string(),
                description: "test failure (2 occurrences)".to_string(),
                occurrences: 2,
                fix_or_note: Some("the fix".to_string()),
                status: CandidateStatus::Pending,
            }],
        };
        save_candidates(&workspace, &cfile).unwrap();

        let action = serde_json::json!({
            "action": "lessons",
            "op": "promote",
            "candidate_id": "test_id_001"
        });
        let (_, msg) = handle_lessons_action(&workspace, &action).unwrap();
        assert!(msg.contains("promoted"), "promote should report success");

        let artifact = load_lessons(&workspace);
        assert!(artifact.failures.iter().any(|f| f.contains("test failure")));
        assert!(artifact.fixes.iter().any(|f| f.contains("the fix")));

        let updated = load_candidates(&workspace);
        let c = updated.candidates.iter().find(|c| c.id == "test_id_001").unwrap();
        assert_eq!(c.status, CandidateStatus::Promoted);
    }

    #[test]
    fn reject_marks_candidate_not_pending() {
        let workspace = tempdir();
        std::fs::create_dir_all(workspace.join("agent_state")).unwrap();

        let cfile = LessonsCandidatesFile {
            version: 1,
            last_synthesized_ms: 0,
            candidates: vec![LessonsCandidate {
                id: "rej_id".to_string(),
                kind: "failure_pattern".to_string(),
                description: "noise".to_string(),
                occurrences: 2,
                fix_or_note: None,
                status: CandidateStatus::Pending,
            }],
        };
        save_candidates(&workspace, &cfile).unwrap();

        let action = serde_json::json!({"action": "lessons", "op": "reject", "candidate_id": "rej_id"});
        handle_lessons_action(&workspace, &action).unwrap();

        let updated = load_candidates(&workspace);
        assert_eq!(updated.candidates[0].status, CandidateStatus::Rejected);
    }

    fn tempdir() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("canon-lessons-test-{}-{}", std::process::id(), unique));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

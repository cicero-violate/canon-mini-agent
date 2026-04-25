/// Lessons pipeline: two-stage pattern learning system.
///
/// Stage 1 — Synthesis (automatic, runs after each planner cycle):
///   Reads canonical `ActionResultRecorded` events from `agent_state/tlog.ndjson`,
///   detects failure patterns AND successful action sequences,
///   and writes/merges results into `agent_state/lessons_candidates.json`.
///   Candidates are born with `status: "pending"`.
///
/// Stage 2 — Promotion (LLM-driven, via the `lessons` action):
///   The planner reviews pending candidates and promotes or rejects them.
///   Promoted candidates are merged into `agent_state/lessons.json`, which is
///   injected into every planner/solo prompt via `read_lessons_or_empty`.
///   Rejected candidates persist with `status: "rejected"` so they are not re-surfaced.
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::prompt_inputs::{LessonEntry, LessonEntryStatus, LessonsArtifact};

// ── File paths ────────────────────────────────────────────────────────────────

const LESSONS_FILE: &str = "agent_state/lessons.json";
const CANDIDATES_FILE: &str = "agent_state/lessons_candidates.json";
const TLOG_SUBPATH: &str = "tlog.ndjson";

// ── Tuning knobs ──────────────────────────────────────────────────────────────

/// Recent canonical tlog records to scan each synthesis run.
const MAX_LINES_TO_SCAN: usize = 4000;
/// Minimum times a failure pattern must recur before it becomes a candidate.
const MIN_FAILURE_OCCURRENCES: usize = 2;
/// Minimum times a bigram must recur before it becomes a success-sequence candidate.
const MIN_BIGRAM_OCCURRENCES: usize = 4;
/// Minimum times a trigram must recur before it becomes a success-sequence candidate.
const MIN_TRIGRAM_OCCURRENCES: usize = 3;
/// Minimum consecutive same-path read_file calls (without apply_patch) to flag as a stall.
const MIN_STALL_READS: usize = 3;
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
    /// Task ids this pattern was observed under.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    /// Objective ids this pattern was observed under.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objective_ids: Vec<String>,
    /// Agent roles this pattern was observed under.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Representative intents attached to actions contributing to this pattern.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<String>,
    /// System-facing question about how to prevent or automate this class of behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_direction: Option<String>,
    /// Suggested system layer to modify instead of training the model to behave differently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_lever: Option<String>,
    /// Concrete system change hint, when one is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_change_hint: Option<String>,
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

/// Called after each successful planner cycle.  Synthesizes failure patterns
/// and success sequences from the action log and merges them into
/// `agent_state/lessons_candidates.json`.  Does not modify `lessons.json`.
pub fn maybe_synthesize_lessons(workspace: &Path) {
    if let Err(e) = synthesize_candidates(workspace) {
        eprintln!("[lessons] synthesis error: {e:#}");
    }
}

/// Handle a `lessons` action dispatched from the main tool executor.
pub fn handle_lessons_action(workspace: &Path, action: &Value) -> Result<(bool, String)> {
    handle_lessons_action_with_writer(workspace, action, None)
}

pub fn handle_lessons_action_with_writer(
    workspace: &Path,
    action: &Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
) -> Result<(bool, String)> {
    let op = action
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("read_candidates");

    match op {
        "read_candidates" => op_read_candidates(workspace),
        "promote" => op_promote(workspace, action, writer),
        "reject" => op_reject(workspace, action),
        "encode" => op_encode(workspace, action, writer),
        "read" => op_read_lessons(workspace),
        "write" => op_write_lessons(workspace, action, writer),
        other => anyhow::bail!(
            "unknown lessons op '{other}' — use: read_candidates | promote | reject | encode | read | write"
        ),
    }
}

// ── Op implementations ────────────────────────────────────────────────────────

fn op_read_candidates(workspace: &Path) -> Result<(bool, String)> {
    let path = candidates_path(workspace);
    if !path.exists() {
        return Ok((
            false,
            "(no lessons candidates yet — synthesis runs after the next planner cycle)".to_string(),
        ));
    }
    let raw = std::fs::read_to_string(&path)?;
    let file: LessonsCandidatesFile = serde_json::from_str(&raw).unwrap_or_default();
    let pending: Vec<&LessonsCandidate> = file
        .candidates
        .iter()
        .filter(|c| c.status == CandidateStatus::Pending)
        .collect();
    if pending.is_empty() {
        return Ok((
            false,
            "lessons_candidates: no pending candidates (all have been promoted or rejected)"
                .to_string(),
        ));
    }
    let out = serde_json::to_string_pretty(&pending)?;
    Ok((
        false,
        format!(
            "lessons_candidates pending ({} items):\n{out}",
            pending.len()
        ),
    ))
}

fn op_promote(
    workspace: &Path,
    action: &Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
) -> Result<(bool, String)> {
    let candidate_id = action
        .get("candidate_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("lessons promote requires 'candidate_id' (id string or \"all\")")
        })?;

    let mut cfile = load_candidates(workspace);
    let promote_all = candidate_id == "all";

    let mut promoted_count = 0usize;
    let mut artifact = load_lessons_artifact(workspace);

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
            return Ok((
                false,
                "lessons promote: no pending candidates to promote".to_string(),
            ));
        } else {
            anyhow::bail!("lessons promote: candidate '{candidate_id}' not found or not pending");
        }
    }

    // Refresh summary.
    artifact.summary = build_artifact_summary(&artifact);

    save_candidates(workspace, &cfile)?;
    persist_lessons_projection_with_writer(workspace, &artifact, writer, "lessons_save")?;

    Ok((
        false,
        format!("lessons promote: {promoted_count} candidate(s) promoted to lessons.json"),
    ))
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
    Ok((
        false,
        format!("lessons reject: candidate '{candidate_id}' marked rejected"),
    ))
}

/// Mark a lesson entry as `encoded` — meaning it has been hardcoded into the
/// system source and no longer needs to live in the prompt.
///
/// Required field: `entry_text` — the exact `text` value of the entry to encode.
/// Optional field: `encoded_at` — a short note of where it was encoded
///                 (e.g., `"src/lessons.rs:schema_fix_hint"`).
fn op_encode(
    workspace: &Path,
    action: &Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
) -> Result<(bool, String)> {
    let entry_text = action
        .get("entry_text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("lessons encode requires 'entry_text' — the exact text of the entry to mark encoded"))?;

    let mut artifact = load_lessons_artifact(workspace);
    let mut found = false;

    for list in [
        &mut artifact.failures,
        &mut artifact.fixes,
        &mut artifact.required_actions,
    ] {
        for entry in list.iter_mut() {
            if entry.text == entry_text {
                entry.status = LessonEntryStatus::Encoded;
                found = true;
            }
        }
    }

    if !found {
        anyhow::bail!(
            "lessons encode: no entry with text {:?} found in lessons.json — use lessons op=read to list entries",
            entry_text
        );
    }

    persist_lessons_projection_with_writer(workspace, &artifact, writer, "lessons_save")?;
    Ok((
        false,
        format!("lessons encode: entry marked as encoded and removed from prompt injection"),
    ))
}

fn op_read_lessons(workspace: &Path) -> Result<(bool, String)> {
    let artifact = load_lessons_artifact(workspace);
    if artifact == LessonsArtifact::default() {
        return Ok((
            false,
            "(lessons.json does not exist yet — promote candidates to create it)".to_string(),
        ));
    }
    let raw = serde_json::to_string_pretty(&artifact)?;
    Ok((false, format!("lessons.json:\n{raw}")))
}

fn op_write_lessons(
    workspace: &Path,
    action: &Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
) -> Result<(bool, String)> {
    let lessons_val = action
        .get("lessons")
        .ok_or_else(|| anyhow::anyhow!("lessons write requires a 'lessons' object with summary/failures/fixes/required_actions"))?;
    let artifact: LessonsArtifact = serde_json::from_value(lessons_val.clone())
        .map_err(|e| anyhow::anyhow!("invalid lessons object: {e}"))?;
    persist_lessons_projection_with_writer(workspace, &artifact, writer, "lessons_save")?;
    Ok((false, "lessons write ok".to_string()))
}

// ── Core synthesis ────────────────────────────────────────────────────────────

fn synthesize_candidates(workspace: &Path) -> Result<()> {
    let agent_state = crate::constants::agent_state_dir();
    let tlog_path = Path::new(agent_state).join(TLOG_SUBPATH);
    if !tlog_path.exists() {
        return Ok(());
    }

    let entries = read_recent_action_result_entries(&tlog_path, MAX_LINES_TO_SCAN);
    if entries.is_empty() {
        return Ok(());
    }

    let failure_candidates = detect_failure_candidates(&entries);
    let sequence_candidates = detect_success_sequences(&entries);
    let stall_candidates = detect_stall_patterns(&entries);

    if failure_candidates.is_empty()
        && sequence_candidates.is_empty()
        && stall_candidates.is_empty()
    {
        return Ok(());
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Merge into the existing candidates file (preserves rejected/promoted status).
    let mut cfile = load_candidates(workspace);
    cfile.last_synthesized_ms = now_ms;
    cfile.version = 2;

    merge_candidates_into_file(&mut cfile, failure_candidates);
    merge_candidates_into_file(&mut cfile, sequence_candidates);
    merge_candidates_into_file(&mut cfile, stall_candidates);

    // Prune excess pending candidates per kind (keep highest-occurrence ones).
    prune_excess_pending(&mut cfile);

    save_candidates(workspace, &cfile)?;
    eprintln!(
        "[lessons] candidates synthesized — {} total ({} pending)",
        cfile.candidates.len(),
        cfile
            .candidates
            .iter()
            .filter(|c| c.status == CandidateStatus::Pending)
            .count()
    );
    Ok(())
}

// ── Failure pattern detection ─────────────────────────────────────────────────

fn detect_failure_candidates(entries: &[Value]) -> Vec<LessonsCandidate> {
    let mut map: HashMap<(String, String), CandidateAggregate> = HashMap::new();

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
        let aggregate = map.entry(key).or_default();
        aggregate.occurrences += 1;
        aggregate.roles.insert(
            entry
                .get("actor")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
        aggregate.task_ids.insert(
            entry
                .get("task_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
        aggregate.objective_ids.insert(
            entry
                .get("objective_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
        aggregate.intents.insert(
            entry
                .get("intent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
        if aggregate.fix_or_note.is_none() {
            aggregate.fix_or_note = schema_fix_hint(&action, &pattern);
        }
    }

    let mut results: Vec<LessonsCandidate> = map
        .into_iter()
        .filter(|(_, aggregate)| aggregate.occurrences >= MIN_FAILURE_OCCURRENCES)
        .map(|((action, pattern), aggregate)| {
            let occurrences = aggregate.occurrences;
            let fix = aggregate.fix_or_note.clone();
            let description = format!(
                "`{action}` action: {pattern} ({occurrences} occurrence{})",
                if occurrences == 1 { "" } else { "s" }
            );
            LessonsCandidate {
                id: stable_id("failure", &format!("{action}:{pattern}")),
                kind: "failure_pattern".to_string(),
                description,
                occurrences,
                fix_or_note: fix.clone(),
                task_ids: aggregate
                    .task_ids
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                objective_ids: aggregate
                    .objective_ids
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                roles: aggregate
                    .roles
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                intents: aggregate
                    .intents
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                system_direction: Some(prevention_question(&action, &pattern)),
                system_lever: Some(prevention_system_lever(&action, &pattern).to_string()),
                system_change_hint: fix,
                status: CandidateStatus::Pending,
            }
        })
        .collect();

    results.sort_by(|a, b| b.occurrences.cmp(&a.occurrences));
    results
}

// ── Success sequence detection ────────────────────────────────────────────────

fn detect_success_sequences(entries: &[Value]) -> Vec<LessonsCandidate> {
    let successes = collect_successful_actions(entries);
    let (bigrams, trigrams) = count_success_sequences(&successes);
    let mut results = build_success_sequence_candidates(bigrams, trigrams);

    results.sort_by(|a, b| b.occurrences.cmp(&a.occurrences));
    results
}

/// A successful action with its provenance context from the log entry.
struct TaggedAction {
    action: String,
    /// The plan task id active when this action ran (empty if unknown).
    task_id: String,
    /// The objective id the action claims to advance (empty if unknown).
    objective_id: String,
    /// The agent role that executed this action (executor, planner, solo, …).
    role: String,
    /// Intent attached to the action payload (empty if unknown).
    intent: String,
}

fn collect_successful_actions(entries: &[Value]) -> Vec<TaggedAction> {
    entries
        .iter()
        .filter(|e| {
            e.get("kind").and_then(|v| v.as_str()) == Some("tool")
                && e.get("phase").and_then(|v| v.as_str()) == Some("result")
                && e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .filter_map(|e| {
            let action = e.get("action").and_then(|v| v.as_str())?.to_string();
            let task_id = e
                .get("task_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let role = e
                .get("actor")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let objective_id = e
                .get("objective_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let intent = e
                .get("intent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(TaggedAction {
                action,
                task_id,
                objective_id,
                role,
                intent,
            })
        })
        .collect()
}

fn count_success_sequences(
    successes: &[TaggedAction],
) -> (
    // (role, action_a, action_b) → aggregate
    HashMap<(String, String, String), CandidateAggregate>,
    // (role, action_a, action_b, action_c) → aggregate
    HashMap<(String, String, String, String), CandidateAggregate>,
) {
    let mut bigrams: HashMap<(String, String, String), CandidateAggregate> = HashMap::new();
    let mut trigrams: HashMap<(String, String, String, String), CandidateAggregate> =
        HashMap::new();

    for window in successes.windows(2) {
        let a = &window[0];
        let b = &window[1];
        if a.action == b.action {
            continue;
        }
        // Only count sequences within the same role and task context.
        // If task_id is empty on either side (pre-threading entries), allow the
        // pair only when both roles match — avoids cross-agent noise.
        let same_task = !a.task_id.is_empty() && a.task_id == b.task_id;
        let same_role = a.role == b.role;
        if !same_task && !same_role {
            continue;
        }
        let role = if same_role {
            a.role.clone()
        } else {
            String::new()
        };
        let aggregate = bigrams
            .entry((role, a.action.clone(), b.action.clone()))
            .or_default();
        aggregate.occurrences += 1;
        aggregate.roles.insert(a.role.clone());
        aggregate.task_ids.insert(a.task_id.clone());
        aggregate.task_ids.insert(b.task_id.clone());
        aggregate.objective_ids.insert(a.objective_id.clone());
        aggregate.objective_ids.insert(b.objective_id.clone());
        aggregate.intents.insert(a.intent.clone());
        aggregate.intents.insert(b.intent.clone());
    }

    for window in successes.windows(3) {
        let a = &window[0];
        let b = &window[1];
        let c = &window[2];
        if a.action == b.action || b.action == c.action {
            continue;
        }
        let same_task = !a.task_id.is_empty() && a.task_id == b.task_id && b.task_id == c.task_id;
        let same_role = a.role == b.role && b.role == c.role;
        if !same_task && !same_role {
            continue;
        }
        let role = if same_role {
            a.role.clone()
        } else {
            String::new()
        };
        let aggregate = trigrams
            .entry((role, a.action.clone(), b.action.clone(), c.action.clone()))
            .or_default();
        aggregate.occurrences += 1;
        aggregate.roles.insert(a.role.clone());
        aggregate.roles.insert(b.role.clone());
        aggregate.roles.insert(c.role.clone());
        aggregate.task_ids.insert(a.task_id.clone());
        aggregate.task_ids.insert(b.task_id.clone());
        aggregate.task_ids.insert(c.task_id.clone());
        aggregate.objective_ids.insert(a.objective_id.clone());
        aggregate.objective_ids.insert(b.objective_id.clone());
        aggregate.objective_ids.insert(c.objective_id.clone());
        aggregate.intents.insert(a.intent.clone());
        aggregate.intents.insert(b.intent.clone());
        aggregate.intents.insert(c.intent.clone());
    }

    (bigrams, trigrams)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: std::collections::HashMap<(std::string::String, std::string::String, std::string::String
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_success_sequence_candidates(
    bigrams: HashMap<(String, String, String), CandidateAggregate>,
    trigrams: HashMap<(String, String, String, String), CandidateAggregate>,
) -> Vec<LessonsCandidate> {
    let mut results: Vec<LessonsCandidate> = Vec::new();
    let success_clusters = build_success_cluster_counts(&bigrams, &trigrams);

    for ((role, a, b), aggregate) in bigrams {
        if aggregate.occurrences < MIN_BIGRAM_OCCURRENCES {
            continue;
        }
        results.push(build_bigram_candidate(
            role,
            a,
            b,
            aggregate,
            &success_clusters,
        ));
    }

    for ((role, a, b, c), aggregate) in trigrams {
        if aggregate.occurrences < MIN_TRIGRAM_OCCURRENCES {
            continue;
        }
        results.push(build_trigram_candidate(
            role,
            a,
            b,
            c,
            aggregate,
            &success_clusters,
        ));
    }

    results
}

/// Intent: pure_transform
/// Resource: lesson_candidate
/// Inputs: std::string::String, std::string::String, std::string::String, lessons::CandidateAggregate, &std::collections::HashMap<std::string::String, usize>
/// Outputs: lessons::LessonsCandidate
/// Effects: none
/// Forbidden: fs_write, uses_network, spawns_process
/// Invariants: deterministic_candidate_id, empty_metadata_filtered
/// Failure: infallible
/// Provenance: rustc:facts + rustc:docstring
fn build_bigram_candidate(
    role: String,
    a: String,
    b: String,
    aggregate: CandidateAggregate,
    success_clusters: &HashMap<String, usize>,
) -> LessonsCandidate {
    let key = format!("{role}:{a}→{b}");
    let count = aggregate.occurrences;
    let note = sequence_workflow_note(&a, &b, None);
    let role_tag = if role.is_empty() {
        String::new()
    } else {
        format!(" [{role}]")
    };
    let system_direction = success_automation_question(
        aggregate.task_ids.iter(),
        aggregate.objective_ids.iter(),
        success_clusters,
    );
    LessonsCandidate {
        id: stable_id("seq2", &key),
        kind: "success_sequence".to_string(),
        description: format!("Action sequence{role_tag}: {a} → {b} ({count} occurrences)"),
        occurrences: count,
        fix_or_note: note.clone(),
        task_ids: aggregate
            .task_ids
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        objective_ids: aggregate
            .objective_ids
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        roles: aggregate
            .roles
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        intents: aggregate
            .intents
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        system_direction,
        system_lever: Some("runtime_automation".to_string()),
        system_change_hint: note,
        status: CandidateStatus::Pending,
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: std::string::String, std::string::String, std::string::String, std::string::String, lessons::CandidateAggregate, &std::collections::HashMap<std::string::String, usize>
/// Outputs: lessons::LessonsCandidate
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_trigram_candidate(
    role: String,
    a: String,
    b: String,
    c: String,
    aggregate: CandidateAggregate,
    success_clusters: &HashMap<String, usize>,
) -> LessonsCandidate {
    let key = format!("{role}:{a}→{b}→{c}");
    let count = aggregate.occurrences;
    let note = sequence_workflow_note(&a, &b, Some(&c));
    let role_tag = if role.is_empty() {
        String::new()
    } else {
        format!(" [{role}]")
    };
    let system_direction = success_automation_question(
        aggregate.task_ids.iter(),
        aggregate.objective_ids.iter(),
        success_clusters,
    );
    LessonsCandidate {
        id: stable_id("seq3", &key),
        kind: "success_sequence".to_string(),
        description: format!("Action sequence{role_tag}: {a} → {b} → {c} ({count} occurrences)"),
        occurrences: count,
        fix_or_note: note.clone(),
        task_ids: aggregate
            .task_ids
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        objective_ids: aggregate
            .objective_ids
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        roles: aggregate
            .roles
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        intents: aggregate
            .intents
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        system_direction,
        system_lever: Some("runtime_automation".to_string()),
        system_change_hint: note,
        status: CandidateStatus::Pending,
    }
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
            if !new_c.task_ids.is_empty() {
                existing.task_ids = new_c.task_ids;
            }
            if !new_c.objective_ids.is_empty() {
                existing.objective_ids = new_c.objective_ids;
            }
            if !new_c.roles.is_empty() {
                existing.roles = new_c.roles;
            }
            if !new_c.intents.is_empty() {
                existing.intents = new_c.intents;
            }
            if new_c.system_direction.is_some() {
                existing.system_direction = new_c.system_direction;
            }
            if new_c.system_lever.is_some() {
                existing.system_lever = new_c.system_lever;
            }
            if new_c.system_change_hint.is_some() {
                existing.system_change_hint = new_c.system_change_hint;
            }
        } else {
            cfile.candidates.push(new_c);
        }
    }
}

fn prune_excess_pending(cfile: &mut LessonsCandidatesFile) {
    // Keep at most MAX_CANDIDATES_PER_KIND pending candidates per kind,
    // retaining the highest-occurrence ones.
    for kind in &["failure_pattern", "success_sequence", "stall_pattern"] {
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
            if !artifact.failures.iter().any(|e| e.text == c.description) {
                artifact
                    .failures
                    .push(LessonEntry::pending(c.description.clone()));
            }
            let fix = c
                .system_change_hint
                .as_ref()
                .or(c.fix_or_note.as_ref())
                .or(c.system_direction.as_ref());
            if let Some(fix) = fix {
                if !artifact.fixes.iter().any(|e| &e.text == fix) {
                    artifact.fixes.push(LessonEntry::pending(fix.clone()));
                }
            }
        }
        "success_sequence" => {
            let text = c
                .system_direction
                .as_deref()
                .or(c.system_change_hint.as_deref())
                .or(c.fix_or_note.as_deref())
                .unwrap_or(&c.description)
                .to_string();
            if !artifact.required_actions.iter().any(|e| e.text == text) {
                artifact.required_actions.push(LessonEntry::pending(text));
            }
        }
        "stall_pattern" => {
            // Surface the stall as a failure and its fix as a prompt rule.
            if !artifact.failures.iter().any(|e| e.text == c.description) {
                artifact
                    .failures
                    .push(LessonEntry::pending(c.description.clone()));
            }
            let fix = c.system_change_hint.as_ref().or(c.fix_or_note.as_ref());
            if let Some(fix) = fix {
                if !artifact.fixes.iter().any(|e| &e.text == fix) {
                    artifact.fixes.push(LessonEntry::pending(fix.clone()));
                }
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

/// Intent: pure_transform
/// Resource: prompt_context
/// Inputs: &prompt_inputs::LessonsArtifact
/// Outputs: std::string::String
/// Effects: none
/// Forbidden: fs_write, uses_network, spawns_process
/// Invariants: no_external_effects
/// Failure: infallible
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, F
/// Outputs: T
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_json_file_or_else<T, F>(path: &Path, default: F) -> T
where
    T: serde::de::DeserializeOwned,
    F: FnOnce() -> T,
{
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(default)
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: lessons::LessonsCandidatesFile
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_candidates(workspace: &Path) -> LessonsCandidatesFile {
    let path = candidates_path(workspace);
    load_json_file_or_else(&path, LessonsCandidatesFile::default)
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &lessons::LessonsCandidatesFile
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn save_candidates(workspace: &Path, cfile: &LessonsCandidatesFile) -> Result<()> {
    let path = candidates_path(workspace);
    crate::logging::record_json_projection_with_optional_writer(
        workspace,
        &path,
        CANDIDATES_FILE,
        "write",
        "lessons_candidates_save",
        cfile,
        None,
        None,
    )
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: prompt_inputs::LessonsArtifact
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn load_lessons_artifact(workspace: &Path) -> LessonsArtifact {
    if let Some(artifact) = load_lessons_from_tlog(workspace) {
        return artifact;
    }
    let path = lessons_path(workspace);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        if let Ok(artifact) = serde_json::from_str::<LessonsArtifact>(&raw) {
            return artifact;
        }
    }
    LessonsArtifact::default()
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &prompt_inputs::LessonsArtifact, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn persist_lessons_projection(
    workspace: &Path,
    artifact: &LessonsArtifact,
    subject: &str,
) -> Result<()> {
    persist_lessons_projection_with_writer(workspace, artifact, None, subject)
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &prompt_inputs::LessonsArtifact, std::option::Option<&mut canonical_writer::CanonicalWriter>, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn persist_lessons_projection_with_writer(
    workspace: &Path,
    artifact: &LessonsArtifact,
    mut writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    subject: &str,
) -> Result<()> {
    let path = lessons_path(workspace);
    crate::logging::record_json_projection_with_optional_writer(
        workspace,
        &path,
        LESSONS_FILE,
        "write",
        subject,
        artifact,
        writer.as_deref_mut(),
        Some(crate::events::EffectEvent::LessonsArtifactRecorded {
            artifact: artifact.clone(),
        }),
    )
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::option::Option<prompt_inputs::LessonsArtifact>
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_lessons_from_tlog(workspace: &Path) -> Option<LessonsArtifact> {
    crate::tlog::Tlog::latest_effect_from_workspace(workspace, |event| match event {
        crate::events::EffectEvent::LessonsArtifactRecorded { artifact } => Some(artifact),
        _ => None,
    })
}

pub fn reconcile_lessons_projection(workspace: &Path, subject: &str) -> Result<bool> {
    let Some(artifact) = load_lessons_from_tlog(workspace) else {
        return Ok(false);
    };
    let canonical = serde_json::to_string_pretty(&artifact)?;
    let path = lessons_path(workspace);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if crate::logging::stable_hash_hex(&current) == crate::logging::stable_hash_hex(&canonical) {
        return Ok(false);
    }
    persist_lessons_projection(workspace, &artifact, subject)?;
    Ok(true)
}

// ── Utility ───────────────────────────────────────────────────────────────────

/// Generate a stable short ID for a pattern by hashing its key.
fn stable_id(prefix: &str, key: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    prefix.hash(&mut h);
    format!(
        "{prefix}_{:016x}",
        <std::collections::hash_map::DefaultHasher as Hasher>::finish(&h)
    )
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, usize
/// Outputs: std::vec::Vec<serde_json::Value>
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn read_recent_action_result_entries(path: &Path, max_records: usize) -> Vec<Value> {
    crate::tlog::Tlog::read_recent_records(path, max_records)
        .unwrap_or_default()
        .into_iter()
        .filter_map(action_result_record_to_lesson_entry)
        .collect()
}

fn action_result_record_to_lesson_entry(record: crate::tlog::TlogRecord) -> Option<Value> {
    let crate::events::Event::Effect {
        event:
            crate::events::EffectEvent::ActionResultRecorded {
                role,
                step,
                command_id,
                action_kind,
                task_id,
                objective_id,
                ok,
                result,
                ..
            },
    } = record.event
    else {
        return None;
    };

    Some(json!({
        "kind": "tool",
        "phase": "result",
        "actor": role,
        "step": step,
        "command_id": command_id,
        "action": action_kind,
        "task_id": task_id.unwrap_or_default(),
        "objective_id": objective_id.unwrap_or_default(),
        "ok": ok,
        "text": result,
        "seq": record.seq,
        "ts_ms": record.ts_ms,
    }))
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
            } else if pattern.contains("create_task")
                && (pattern.contains("missing task") || pattern.contains("does not accept task_id"))
            {
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
            } else if pattern.contains("missing field `id`")
                || pattern.contains("missing field `status`")
            {
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

#[derive(Debug, Clone, Default)]
struct CandidateAggregate {
    occurrences: usize,
    fix_or_note: Option<String>,
    task_ids: BTreeSet<String>,
    objective_ids: BTreeSet<String>,
    roles: BTreeSet<String>,
    intents: BTreeSet<String>,
}

fn prevention_question(action: &str, pattern: &str) -> String {
    format!(
        "How can this `{action}` failure pattern be made impossible in the system so `{pattern}` never occurs anymore without relying on the LLM to change?"
    )
}

fn prevention_system_lever(action: &str, pattern: &str) -> &'static str {
    if pattern.contains("missing")
        || pattern.contains("invalid type")
        || pattern.contains("does not accept")
    {
        "schema_validation"
    } else if action == "symbol_window" && pattern.contains("not found in graph") {
        "semantic_preflight"
    } else if pattern.contains("task not found") || pattern.contains("objective not found") {
        "runtime_guard"
    } else {
        "runtime_validation"
    }
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &std::collections::HashMap<(std::string::String, std::string::String, std::string::String
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_success_cluster_counts(
    bigrams: &HashMap<(String, String, String), CandidateAggregate>,
    trigrams: &HashMap<(String, String, String, String), CandidateAggregate>,
) -> HashMap<String, usize> {
    let mut clusters = HashMap::new();
    for aggregate in bigrams.values().chain(trigrams.values()) {
        for task_id in aggregate.task_ids.iter().filter(|s| !s.is_empty()) {
            *clusters.entry(format!("task:{task_id}")).or_insert(0) += 1;
        }
        for objective_id in aggregate.objective_ids.iter().filter(|s| !s.is_empty()) {
            *clusters
                .entry(format!("objective:{objective_id}"))
                .or_insert(0) += 1;
        }
    }
    clusters
}

// ── Stall pattern detection ───────────────────────────────────────────────────

/// Detects read-loop stalls: a role reading the same file ≥ MIN_STALL_READS times
/// in a single "patch-free window" (i.e., no apply_patch between the reads).
///
/// Any apply_patch result resets the window for that role.  Only consecutive reads
/// of the **same path**, within the same window, count toward a stall.
fn detect_stall_patterns(entries: &[Value]) -> Vec<LessonsCandidate> {
    // (role, path) → per-window run state
    #[derive(Default, Clone)]
    struct RunState {
        count: usize,
        task_ids: BTreeSet<String>,
        objective_ids: BTreeSet<String>,
        intents: BTreeSet<String>,
    }

    // (role, path) → accumulated stall state (worst window seen)
    let mut stalls: HashMap<(String, String), RunState> = HashMap::new();
    // (role, path) → current window run
    let mut runs: HashMap<(String, String), RunState> = HashMap::new();

    for entry in entries {
        if entry.get("kind").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        if entry.get("phase").and_then(|v| v.as_str()) != Some("result") {
            continue;
        }
        if !entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }

        let action = entry.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let role = entry
            .get("actor")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if action == "apply_patch" {
            // Patch: close out all windows for this role.
            runs.retain(|(r, _), _| r != &role);
            continue;
        }

        if action != "read_file" {
            continue;
        }

        let path = entry
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            continue;
        }

        // Any other path read for the same role resets that role's window for OTHER paths.
        // (We track per-(role,path) so different paths don't interfere.)
        let key = (role.clone(), path.clone());
        let run = runs.entry(key.clone()).or_default();
        run.count += 1;
        run.task_ids.insert(
            entry
                .get("task_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
        run.objective_ids.insert(
            entry
                .get("objective_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
        run.intents.insert(
            entry
                .get("intent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );

        if run.count >= MIN_STALL_READS {
            let stall = stalls.entry(key).or_default();
            if run.count > stall.count {
                *stall = run.clone();
            }
        }
    }

    stalls
        .into_iter()
        .map(|((role, path), state)| {
            let count = state.count;
            let hint = format!(
                "- Do not call read_file on a path already in context. \
                 `{path}` was read {count} times without apply_patch — \
                 its content is already available. Act on it: apply_patch or message."
            );
            let direction = format!(
                "How can the system prevent `read_file` stalls on `{path}` \
                 for the `{role}` role? The agent re-read this file {count} times \
                 without making any patch, wasting turns."
            );
            LessonsCandidate {
                id: stable_id("stall", &format!("{role}:{path}")),
                kind: "stall_pattern".to_string(),
                description: format!(
                    "read_file stall [{role}]: `{path}` read {count} times without apply_patch"
                ),
                occurrences: count,
                fix_or_note: Some(hint.clone()),
                task_ids: state
                    .task_ids
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                objective_ids: state
                    .objective_ids
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                roles: vec![role],
                intents: state
                    .intents
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect(),
                system_direction: Some(direction),
                system_lever: Some("prompt_rule".to_string()),
                system_change_hint: Some(hint),
                status: CandidateStatus::Pending,
            }
        })
        .collect()
}

// ── Lesson applicator ─────────────────────────────────────────────────────────

/// Applies promoted lessons with `system_lever == "prompt_rule"` to `ROLES.json`.
///
/// Reads `lessons_candidates.json`, finds candidates that are promoted + prompt_rule,
/// and appends their `system_change_hint` text to the appropriate role arrays in
/// `ROLES.json` (which `load_role_overrides` reads every prompt cycle).
///
/// Idempotent: rules already present in ROLES.json are not duplicated.
/// Returns the number of newly added rules.
pub fn apply_promoted_lessons(workspace: &Path) -> usize {
    match try_apply_promoted_lessons(workspace) {
        Ok(n) => {
            if n > 0 {
                eprintln!("[lessons] applied {n} promoted prompt-rule(s) to ROLES.json");
            }
            n
        }
        Err(e) => {
            eprintln!("[lessons] apply_promoted_lessons error: {e:#}");
            0
        }
    }
}

fn try_apply_promoted_lessons(workspace: &Path) -> Result<usize> {
    let cfile = load_candidates(workspace);

    // Collect qualifying candidates: promoted + prompt_rule lever + has a hint.
    let qualifying: Vec<&LessonsCandidate> = cfile
        .candidates
        .iter()
        .filter(|c| {
            c.status == CandidateStatus::Promoted
                && c.system_lever.as_deref() == Some("prompt_rule")
                && c.system_change_hint.is_some()
        })
        .collect();

    if qualifying.is_empty() {
        return Ok(0);
    }

    let mut roles_val = load_roles_json(workspace);
    let roles_obj = roles_val
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("ROLES.json root must be a JSON object"))?;

    // Ensure there's a "roles" sub-object.
    let roles_inner = roles_obj
        .entry("roles")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("ROLES.json .roles must be an object"))?
        .clone();

    // We'll rebuild it.
    let mut updated_inner = roles_inner;
    let mut added = 0usize;

    for candidate in qualifying {
        let hint = candidate.system_change_hint.as_deref().unwrap_or("");
        // Ensure the rule starts with "- " so it matches the format in executor/solo_rules.
        let rule = if hint.starts_with("- ") {
            hint.to_string()
        } else {
            format!("- {hint}")
        };

        // Determine target roles: use the candidate's roles list, defaulting to
        // executor + solo if unspecified (stall patterns affect both).
        let target_roles: Vec<&str> = if candidate.roles.is_empty() {
            vec!["executor", "solo"]
        } else {
            candidate.roles.iter().map(|s| s.as_str()).collect()
        };

        for role_name in target_roles {
            let arr = updated_inner
                .entry(role_name)
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("ROLES.json .roles.{role_name} must be an array"))?;

            let already_present = arr.iter().any(|v| v.as_str() == Some(&rule));
            if !already_present {
                arr.push(serde_json::Value::String(rule.clone()));
                added += 1;
            }
        }
    }

    // Write back only if something changed.
    if added > 0 {
        // Rebuild the full JSON value with the updated inner object.
        let full = roles_obj
            .iter()
            .map(|(k, v)| {
                if k == "roles" {
                    (
                        k.clone(),
                        serde_json::Value::Object(
                            updated_inner
                                .iter()
                                .map(|(rk, rv)| (rk.clone(), rv.clone()))
                                .collect(),
                        ),
                    )
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect::<serde_json::Map<_, _>>();
        save_roles_json(workspace, &serde_json::Value::Object(full))?;
    }

    Ok(added)
}

fn roles_json_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join("ROLES.json")
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: serde_json::Value
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_roles_json(workspace: &Path) -> serde_json::Value {
    let path = roles_json_path(workspace);
    load_json_file_or_else(&path, || serde_json::json!({"roles": {}}))
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn save_roles_json(workspace: &Path, val: &serde_json::Value) -> Result<()> {
    let path = roles_json_path(workspace);
    crate::logging::record_json_projection_with_optional_writer(
        workspace,
        &path,
        "ROLES.json",
        "write",
        "lessons_prompt_rule_projection",
        val,
        None,
        None,
    )
}

fn success_automation_question<'a>(
    task_ids: impl Iterator<Item = &'a String>,
    objective_ids: impl Iterator<Item = &'a String>,
    success_clusters: &HashMap<String, usize>,
) -> Option<String> {
    let matched_task_ids = matched_success_cluster_ids(task_ids, "task", success_clusters);
    let matched_objective_ids =
        matched_success_cluster_ids(objective_ids, "objective", success_clusters);

    if matched_task_ids.is_empty() && matched_objective_ids.is_empty() {
        return None;
    }

    Some(format!(
        "How can this successful pathway be automated in the system, without forcing the LLM to change, for task_ids {:?} and objective_ids {:?}?",
        matched_task_ids, matched_objective_ids
    ))
}

fn matched_success_cluster_ids<'a>(
    ids: impl Iterator<Item = &'a String>,
    prefix: &str,
    success_clusters: &HashMap<String, usize>,
) -> Vec<String> {
    ids.filter(|id| !id.is_empty())
        .filter(|id| {
            success_clusters
                .get(&format!("{prefix}:{id}"))
                .copied()
                .unwrap_or(0)
                >= 2
        })
        .cloned()
        .collect()
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

    fn tool_result_with_provenance(
        action: &str,
        ok: bool,
        text: &str,
        role: &str,
        task_id: &str,
        objective_id: &str,
        intent: &str,
    ) -> Value {
        serde_json::json!({
            "kind": "tool",
            "phase": "result",
            "action": action,
            "ok": ok,
            "text": text,
            "actor": role,
            "task_id": task_id,
            "objective_id": objective_id,
            "intent": intent,
        })
    }

    #[test]
    fn failure_candidates_detected_above_threshold() {
        let entries = vec![
            tool_result(
                "issue",
                false,
                "Error executing action: issue create missing 'issue' field",
            ),
            tool_result(
                "issue",
                false,
                "Error executing action: issue create missing 'issue' field",
            ),
            tool_result(
                "plan",
                false,
                "Error executing action: plan update_task missing task\n...",
            ),
            tool_result(
                "plan",
                false,
                "Error executing action: plan update_task missing task\n...",
            ),
            tool_result("plan", true, "plan ok"),
        ];
        let candidates = detect_failure_candidates(&entries);
        assert!(candidates.iter().any(|c| c.description.contains("issue")));
        assert!(candidates.iter().any(|c| c.description.contains("plan")));
        assert!(candidates.iter().all(|c| c.kind == "failure_pattern"));
        assert!(candidates.iter().all(|c| c.system_direction.is_some()));
    }

    #[test]
    fn single_occurrence_failures_excluded() {
        let entries = vec![tool_result(
            "plan",
            false,
            "Error executing action: plan task not found: T_foo",
        )];
        let candidates = detect_failure_candidates(&entries);
        assert!(
            candidates.is_empty(),
            "single-occurrence errors should not become candidates"
        );
    }

    #[test]
    fn different_task_ids_collapse_to_same_candidate() {
        let entries = vec![
            tool_result(
                "plan",
                false,
                "Error executing action: plan task not found: T_foo_bar",
            ),
            tool_result(
                "plan",
                false,
                "Error executing action: plan task not found: T_baz_qux",
            ),
        ];
        let candidates = detect_failure_candidates(&entries);
        assert_eq!(
            candidates.len(),
            1,
            "different IDs for the same structural error should collapse"
        );
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
            candidates
                .iter()
                .any(|c| c.description.contains("semantic_map")
                    && c.description.contains("symbol_window")),
            "semantic_map→symbol_window bigram should be detected"
        );
        assert!(candidates.iter().all(|c| c.kind == "success_sequence"));
    }

    #[test]
    fn success_sequence_candidates_keep_task_objective_provenance_and_emit_generic_automation_question(
    ) {
        let mut entries: Vec<Value> = Vec::new();
        for _ in 0..4 {
            entries.push(tool_result_with_provenance(
                "read_file",
                true,
                "ok",
                "solo",
                "T1",
                "obj_alpha",
                "Read the current file before patching.",
            ));
            entries.push(tool_result_with_provenance(
                "apply_patch",
                true,
                "ok",
                "solo",
                "T1",
                "obj_alpha",
                "Apply the targeted fix for task T1.",
            ));
        }
        for _ in 0..4 {
            entries.push(tool_result_with_provenance(
                "symbol_window",
                true,
                "ok",
                "solo",
                "T1",
                "obj_alpha",
                "Inspect the target symbol for task T1.",
            ));
            entries.push(tool_result_with_provenance(
                "read_file",
                true,
                "ok",
                "solo",
                "T1",
                "obj_alpha",
                "Read the exact file block tied to the same task.",
            ));
        }

        let candidates = detect_success_sequences(&entries);
        let candidate = candidates
            .iter()
            .find(|c| c.description.contains("read_file") && c.description.contains("apply_patch"))
            .expect("expected read_file→apply_patch candidate");
        assert_eq!(candidate.task_ids, vec!["T1".to_string()]);
        assert_eq!(candidate.objective_ids, vec!["obj_alpha".to_string()]);
        assert!(
            candidate
                .system_direction
                .as_deref()
                .unwrap_or("")
                .contains("without forcing the LLM to change"),
            "expected generic automation question"
        );
    }

    #[test]
    fn same_action_repeated_not_a_sequence() {
        let mut entries = Vec::new();
        for _ in 0..10 {
            entries.push(tool_result("read_file", true, "ok"));
        }
        let candidates = detect_success_sequences(&entries);
        assert!(
            candidates.is_empty(),
            "same-action repetitions should not produce candidates"
        );
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
                task_ids: Vec::new(),
                objective_ids: Vec::new(),
                roles: Vec::new(),
                intents: Vec::new(),
                system_direction: Some("prevent it in the system".to_string()),
                system_lever: Some("schema_validation".to_string()),
                system_change_hint: Some("the fix".to_string()),
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

        let artifact = load_lessons_artifact(&workspace);
        assert!(artifact
            .failures
            .iter()
            .any(|f| f.text.contains("test failure")));
        assert!(artifact.fixes.iter().any(|f| f.text.contains("the fix")));

        let updated = load_candidates(&workspace);
        let c = updated
            .candidates
            .iter()
            .find(|c| c.id == "test_id_001")
            .unwrap();
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
                task_ids: Vec::new(),
                objective_ids: Vec::new(),
                roles: Vec::new(),
                intents: Vec::new(),
                system_direction: None,
                system_lever: None,
                system_change_hint: None,
                status: CandidateStatus::Pending,
            }],
        };
        save_candidates(&workspace, &cfile).unwrap();

        let action =
            serde_json::json!({"action": "lessons", "op": "reject", "candidate_id": "rej_id"});
        handle_lessons_action(&workspace, &action).unwrap();

        let updated = load_candidates(&workspace);
        assert_eq!(updated.candidates[0].status, CandidateStatus::Rejected);
    }

    fn read_file_entry(path: &str, role: &str) -> Value {
        serde_json::json!({
            "kind": "tool",
            "phase": "result",
            "action": "read_file",
            "ok": true,
            "path": path,
            "actor": role,
        })
    }

    fn apply_patch_entry(role: &str) -> Value {
        serde_json::json!({
            "kind": "tool",
            "phase": "result",
            "action": "apply_patch",
            "ok": true,
            "actor": role,
        })
    }

    #[test]
    fn stall_detected_at_threshold() {
        let entries = vec![
            read_file_entry("src/app.rs", "solo"),
            read_file_entry("src/app.rs", "solo"),
            read_file_entry("src/app.rs", "solo"),
        ];
        let candidates = detect_stall_patterns(&entries);
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.kind, "stall_pattern");
        assert!(c.description.contains("app.rs"));
        assert_eq!(c.occurrences, 3);
        assert_eq!(c.system_lever.as_deref(), Some("prompt_rule"));
    }

    #[test]
    fn stall_not_detected_below_threshold() {
        let entries = vec![
            read_file_entry("src/app.rs", "solo"),
            read_file_entry("src/app.rs", "solo"),
        ];
        let candidates = detect_stall_patterns(&entries);
        assert!(candidates.is_empty());
    }

    #[test]
    fn apply_patch_resets_stall_window() {
        let entries = vec![
            read_file_entry("src/app.rs", "solo"),
            read_file_entry("src/app.rs", "solo"),
            apply_patch_entry("solo"),
            // Fresh window starts — these two don't trigger the threshold.
            read_file_entry("src/app.rs", "solo"),
            read_file_entry("src/app.rs", "solo"),
        ];
        let candidates = detect_stall_patterns(&entries);
        assert!(candidates.is_empty(), "apply_patch should reset the window");
    }

    #[test]
    fn different_roles_tracked_independently() {
        let entries = vec![
            read_file_entry("src/app.rs", "executor"),
            read_file_entry("src/app.rs", "executor"),
            read_file_entry("src/app.rs", "executor"),
            // solo also stalls on a different file
            read_file_entry("src/lib.rs", "solo"),
            read_file_entry("src/lib.rs", "solo"),
            read_file_entry("src/lib.rs", "solo"),
        ];
        let candidates = detect_stall_patterns(&entries);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn apply_promoted_lessons_writes_roles_json() {
        let workspace = tempdir();
        std::fs::create_dir_all(workspace.join("agent_state")).unwrap();

        // Write a candidate that is promoted + prompt_rule.
        let cfile = LessonsCandidatesFile {
            version: 2,
            last_synthesized_ms: 0,
            candidates: vec![LessonsCandidate {
                id: "stall_test".to_string(),
                kind: "stall_pattern".to_string(),
                description: "read stall".to_string(),
                occurrences: 3,
                fix_or_note: None,
                task_ids: Vec::new(),
                objective_ids: Vec::new(),
                roles: vec!["executor".to_string()],
                intents: Vec::new(),
                system_direction: None,
                system_lever: Some("prompt_rule".to_string()),
                system_change_hint: Some("- Do not re-read a file already in context.".to_string()),
                status: CandidateStatus::Promoted,
            }],
        };
        save_candidates(&workspace, &cfile).unwrap();

        let added = apply_promoted_lessons(&workspace);
        assert_eq!(added, 1, "should add one rule");

        // Verify ROLES.json was created with the rule.
        let roles = load_roles_json(&workspace);
        let executor_rules = roles["roles"]["executor"].as_array().unwrap();
        assert!(
            executor_rules
                .iter()
                .any(|v| v.as_str() == Some("- Do not re-read a file already in context.")),
            "rule should be in executor array"
        );

        // Idempotent: calling again should add 0 new rules.
        let added2 = apply_promoted_lessons(&workspace);
        assert_eq!(added2, 0, "second call should be idempotent");
    }

    #[test]
    fn load_lessons_artifact_falls_back_to_latest_tlog_snapshot_when_projection_missing() {
        let workspace = tempdir();
        std::fs::create_dir_all(workspace.join("agent_state")).unwrap();

        let artifact = LessonsArtifact {
            summary: "Recovered lessons snapshot".to_string(),
            failures: vec![LessonEntry::pending("Repeated read stall")],
            fixes: vec![LessonEntry::pending("Promote the stall check into runtime")],
            required_actions: vec![LessonEntry::pending("Prefer canonical snapshot loaders")],
            encoding_instructions: "encode later".to_string(),
        };

        persist_lessons_projection(&workspace, &artifact, "lessons_tlog_fallback_test").unwrap();
        std::fs::remove_file(workspace.join(LESSONS_FILE)).unwrap();

        let recovered = load_lessons_artifact(&workspace);
        assert_eq!(recovered.summary, artifact.summary);
        assert_eq!(recovered.failures, artifact.failures);
        assert_eq!(recovered.fixes, artifact.fixes);
        assert_eq!(recovered.required_actions, artifact.required_actions);
    }

    fn tempdir() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "canon-lessons-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

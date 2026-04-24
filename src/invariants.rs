/// Invariant discovery and enforcement system.
///
/// Distinct from the lessons pipeline (which captures LLM behavioral patterns),
/// this module captures **illegal system states** — transitions that the
/// orchestrator should structurally prevent rather than nudge the model away from.
///
/// ## Pipeline
///
/// 1. **Discovery** — scans the action log for recurring failure fingerprints and
///    populates `agent_state/enforced_invariants.json` with `status: discovered`.
/// 2. **Promotion** — when `support_count >= MIN_INVARIANT_SUPPORT`, a discovered
///    invariant is automatically promoted to `status: promoted`.
/// 3. **Enforcement** — promoted invariants become `enforced` once applied to a
///    gate (route gate `G_r`, planner gate `G_p`, or executor gate `G_e`).
///    Gates call `evaluate_invariant_gate` to block invalid role transitions.
/// 4. **Collapse** — after a structural refactor eliminates the root cause, the
///    invariant is marked `collapsed` (no longer enforced but preserved for history).
///    Over time this demotes entries in the static `INVARIANTS.json`.
///
/// ## Artifact
///
/// `agent_state/enforced_invariants.json` — grows dynamically from observed
/// failures.  It is the runtime complement to the static design-time
/// `INVARIANTS.json`.
///
/// ## Math model (from TO-DO.txt)
///
///   State Space' = State Space ∩ V_inv
///   T'(s→s')     = T(s→s') · I_p(s')    (transition filtered by invariant predicate)
///   A'            = {a ∈ A | I(result(a)) = 1}  (only valid actions exist)
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::issues::{
    is_closed, persist_issues_projection_with_writer, rescore_all, Issue, IssuesFile,
};

// ── File paths ────────────────────────────────────────────────────────────────

const ENFORCED_INVARIANTS_FILE: &str = "agent_state/enforced_invariants.json";
const ACTION_LOG_SUBPATH: &str = "default/actions.jsonl";

// ── Tuning knobs ──────────────────────────────────────────────────────────────

/// Lines to scan from the tail of the action log each synthesis run.
const MAX_LINES_TO_SCAN: usize = 4000;
/// Minimum times a failure fingerprint must recur to be promoted automatically.
pub const MIN_INVARIANT_SUPPORT: usize = 3;
/// Max invariants kept per status tier.
const MAX_INVARIANTS_PER_STATUS: usize = 50;
/// Max raw samples kept per invariant.
const MAX_EVIDENCE_SAMPLES: usize = 3;

// ── Data structures ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantStatus {
    /// Detected in logs but support_count has not yet crossed MIN_INVARIANT_SUPPORT.
    Discovered,
    /// Promoted automatically — will be checked by gates.
    Promoted,
    /// Actively enforced at runtime gates; was promoted and gate hook is wired.
    Enforced,
    /// Root cause structurally eliminated; invariant no longer needed.
    Collapsed,
}

impl Default for InvariantStatus {
    fn default() -> Self {
        InvariantStatus::Discovered
    }
}

/// A single key-value pair describing one dimension of the system state at
/// the time a failure was observed.  Examples:
///   {key:"actor",  value:"executor"}
///   {key:"action", value:"read_file"}
///   {key:"error",  value:"missing_target"}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct StateCondition {
    pub key: String,
    pub value: String,
}

/// Raw evidence attached to a discovered invariant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceDerivation {
    pub rule_type: String,
    pub observed_facts: Vec<String>,
    pub matched_conditions: Vec<StateCondition>,
}

/// Raw evidence attached to a discovered invariant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantEvidenceSample {
    pub source: String,
    pub ts_ms: u64,
    #[serde(default)]
    pub derivation: EvidenceDerivation,
    pub raw: Value,
}

/// A dynamically discovered invariant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredInvariant {
    /// Stable ID derived from the canonical fingerprint of state_conditions.
    pub id: String,
    /// Human-readable description of the illegal state.
    pub predicate_text: String,
    /// Structured conditions that identify the illegal state.
    pub state_conditions: Vec<StateCondition>,
    /// How many times this pattern has been observed.
    pub support_count: usize,
    /// Lifecycle status.
    #[serde(default)]
    pub status: InvariantStatus,
    /// Which gate(s) enforce this invariant: "route", "planner", "executor".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<String>,
    /// Raw evidence samples supporting this invariant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<InvariantEvidenceSample>,
    /// Timestamp (ms) of first observation.
    pub first_seen_ms: u64,
    /// Timestamp (ms) of most recent observation.
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnforcedInvariantsFile {
    pub version: u32,
    pub last_synthesized_ms: u64,
    pub invariants: Vec<DiscoveredInvariant>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Synthesize invariants from the action log and merge into
/// `agent_state/enforced_invariants.json`.  Called from the same checkpoint
/// call sites as `lessons::maybe_synthesize_lessons`.
pub fn maybe_synthesize_invariants(workspace: &Path) {
    if let Err(e) = try_synthesize_invariants(workspace) {
        eprintln!("[invariants] synthesis error: {e:#}");
    }
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &str, &std::collections::HashMap<std::string::String, std::string::String>, &std::path::Path
/// Outputs: std::result::Result<(), std::string::String>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn evaluate_invariant_gate(
    proposed_role: &str,
    current_state: &HashMap<String, String>,
    workspace: &Path,
) -> Result<(), String> {
    let file = load_enforced_invariants_file(workspace);
    let enforced: Vec<&DiscoveredInvariant> = file
        .invariants
        .iter()
        .filter(|inv| {
            (inv.status == InvariantStatus::Promoted || inv.status == InvariantStatus::Enforced)
                && invariant_applies_to_role(inv, proposed_role)
        })
        .collect();

    if enforced.is_empty() {
        return Ok(());
    }

    // Add the proposed role to the lookup state.
    let mut state = current_state.clone();
    state.insert("proposed_role".to_string(), proposed_role.to_string());

    for inv in enforced {
        if inv.state_conditions.is_empty() {
            continue;
        }
        // An invariant fires when ALL its conditions match the current state.
        let all_match = inv.state_conditions.iter().all(|cond| {
            state
                .get(&cond.key)
                .map(|v| v == &cond.value)
                .unwrap_or(false)
        });
        if all_match {
            return Err(format!(
                "invariant gate blocked role `{proposed_role}`: {} [id={}]",
                inv.predicate_text, inv.id
            ));
        }
    }

    Ok(())
}

fn invariant_applies_to_role(inv: &DiscoveredInvariant, proposed_role: &str) -> bool {
    if inv.gates.is_empty() {
        return proposed_role == "route";
    }

    inv.gates.iter().any(|gate| gate == proposed_role)
}

/// Dispatch an `invariants` tool action from the planner/solo role.
///
/// Supported ops:
/// - `read`    — return current enforced_invariants.json (pending + promoted)
/// - `promote` — upgrade Discovered → Promoted for a given id (or "all")
/// - `enforce` — upgrade Promoted → Enforced; gate becomes hard-blocking
/// - `collapse` — mark Enforced/Promoted → Collapsed (root cause structurally fixed)
pub fn handle_invariants_action(
    workspace: &Path,
    action: &serde_json::Value,
) -> anyhow::Result<(bool, String)> {
    handle_invariants_action_with_writer(workspace, action, None, "planner")
}

pub fn handle_invariants_action_with_writer(
    workspace: &Path,
    action: &serde_json::Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    actor_role: &str,
) -> anyhow::Result<(bool, String)> {
    let op = action.get("op").and_then(|v| v.as_str()).unwrap_or("read");
    match op {
        "read" => op_read(workspace),
        "promote" => op_promote(workspace, action, writer),
        "enforce" => op_enforce(workspace, action, writer, actor_role),
        "collapse" => op_collapse(workspace, action, writer, actor_role),
        other => anyhow::bail!(
            "unknown invariants op '{other}' — use: read | promote | enforce | collapse"
        ),
    }
}

fn op_read(workspace: &Path) -> anyhow::Result<(bool, String)> {
    let file = load_enforced_invariants_file(workspace);
    if file.invariants.is_empty() {
        return Ok((
            false,
            "(enforced_invariants.json is empty — synthesis runs after the next checkpoint)"
                .to_string(),
        ));
    }
    let visible: Vec<&DiscoveredInvariant> = file
        .invariants
        .iter()
        .filter(|i| i.status != InvariantStatus::Collapsed)
        .collect();
    if visible.is_empty() {
        return Ok((
            false,
            "(all invariants have been collapsed — system is structurally clean)".to_string(),
        ));
    }
    let out = serde_json::to_string_pretty(&visible)?;
    Ok((
        false,
        format!("enforced_invariants ({} active):\n{out}", visible.len()),
    ))
}

fn op_promote(
    workspace: &Path,
    action: &serde_json::Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
) -> anyhow::Result<(bool, String)> {
    let id = action.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow::anyhow!("invariants promote requires 'id' field (invariant id or \"all\")")
    })?;
    let mut file = load_enforced_invariants_file(workspace);
    let promote_all = id == "all";
    let mut count = 0usize;
    for inv in file.invariants.iter_mut() {
        if inv.status != InvariantStatus::Discovered {
            continue;
        }
        if !promote_all && inv.id != id {
            continue;
        }
        inv.status = InvariantStatus::Promoted;
        if inv.gates.is_empty() {
            inv.gates = default_gates_for_conditions(&inv.state_conditions);
        }
        count += 1;
    }
    if count == 0 {
        return Ok((false, format!("no Discovered invariants matched id='{id}'")));
    }
    persist_enforced_invariants_projection_with_writer(
        workspace,
        &file,
        writer,
        "enforced_invariants_save",
    )?;
    Ok((false, format!("promoted {count} invariant(s) to Promoted")))
}

fn op_enforce(
    workspace: &Path,
    action: &serde_json::Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    actor_role: &str,
) -> anyhow::Result<(bool, String)> {
    let id = action
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("invariants enforce requires 'id' field"))?;
    let mut file = load_enforced_invariants_file(workspace);
    let mut count = 0usize;
    for inv in file.invariants.iter_mut() {
        if inv.id != id {
            continue;
        }
        if inv.status == InvariantStatus::Collapsed {
            return Ok((false, format!("invariant {id} is already Collapsed — use promote first if you want to re-enforce")));
        }
        inv.status = InvariantStatus::Enforced;
        if inv.gates.is_empty() {
            inv.gates = default_gates_for_conditions(&inv.state_conditions);
        }
        count += 1;
    }
    if count == 0 {
        return Ok((false, format!("no invariant found with id='{id}'")));
    }
    persist_enforced_invariants_projection_with_writer(
        workspace,
        &file,
        writer,
        "enforced_invariants_save",
    )?;
    // Log to action log so synthesis can track the enforcement event.
    let record = serde_json::json!({
        "kind": "invariant_lifecycle",
        "phase": "enforce",
        "invariant_id": id,
        "actor": actor_role,
        "ts_ms": crate::logging::now_ms(),
    });
    let _ = crate::logging::append_action_log_record(&record);
    Ok((
        false,
        format!("invariant {id} set to Enforced — gate is now hard-blocking"),
    ))
}

fn op_collapse(
    workspace: &Path,
    action: &serde_json::Value,
    writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    actor_role: &str,
) -> anyhow::Result<(bool, String)> {
    let id = action
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("invariants collapse requires 'id' field"))?;
    let rationale = action
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or("root cause structurally eliminated");
    let mut file = load_enforced_invariants_file(workspace);
    let mut count = 0usize;
    for inv in file.invariants.iter_mut() {
        if inv.id != id {
            continue;
        }
        inv.status = InvariantStatus::Collapsed;
        count += 1;
    }
    if count == 0 {
        return Ok((false, format!("no invariant found with id='{id}'")));
    }
    persist_enforced_invariants_projection_with_writer(
        workspace,
        &file,
        writer,
        "enforced_invariants_save",
    )?;
    let record = serde_json::json!({
        "kind": "invariant_lifecycle",
        "phase": "collapse",
        "invariant_id": id,
        "rationale": rationale,
        "actor": actor_role,
        "ts_ms": crate::logging::now_ms(),
    });
    let _ = crate::logging::append_action_log_record(&record);
    Ok((
        false,
        format!("invariant {id} marked Collapsed — {rationale}"),
    ))
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn read_enforced_invariants(workspace: &Path) -> String {
    let file = load_enforced_invariants_file(workspace);
    if file.invariants.is_empty() {
        return "(enforced_invariants.json not yet created — runs after first failure synthesis)"
            .to_string();
    }
    serde_json::to_string_pretty(&file)
        .unwrap_or_else(|e| format!("(error reading enforced_invariants.json: {e})"))
}

struct InvariantMetaIssueSpec<'a> {
    id: &'a str,
    title: &'a str,
    priority: &'a str,
    description: String,
    location: &'a str,
    evidence: &'a [&'a str],
    resolved_note: &'a str,
}

fn sync_invariant_meta_issue(
    file: &mut IssuesFile,
    existing_ids: &HashSet<String>,
    source_is_fixed: bool,
    spec: InvariantMetaIssueSpec<'_>,
) -> (usize, bool) {
    if !source_is_fixed && !existing_ids.contains(spec.id) {
        file.issues.push(Issue {
            id: spec.id.to_string(),
            title: spec.title.to_string(),
            status: "open".to_string(),
            priority: spec.priority.to_string(),
            kind: "invariant_violation".to_string(),
            description: spec.description,
            location: spec.location.to_string(),
            evidence: spec
                .evidence
                .iter()
                .map(|entry| (*entry).to_string())
                .collect(),
            discovered_by: "invariants_analyzer".to_string(),
            score: 0.0,
            ..Issue::default()
        });
        return (1, true);
    }

    if source_is_fixed {
        return (
            0,
            resolve_stale_meta_issue(file, spec.id, spec.resolved_note),
        );
    }

    (0, false)
}

fn sync_action_surface_meta_issue(
    file: &mut IssuesFile,
    existing_ids: &HashSet<String>,
    workspace: &Path,
) -> (usize, bool) {
    sync_invariant_meta_issue(
        file,
        existing_ids,
        has_invariants_action_surface(workspace),
        InvariantMetaIssueSpec {
            id: "inv_action_surface_missing",
            title: "Invariant lifecycle has no action surface — planner cannot review, enforce, or collapse invariants",
            priority: "critical",
            description: concat!(
                "src/invariants.rs populates agent_state/enforced_invariants.json and auto-promotes ",
                "patterns at support_count >= MIN_INVARIANT_SUPPORT, but planner has no ",
                "action surface to review, promote, enforce, or collapse invariants. ",
                "The route gate G_r is observational only (blocked=false) — it logs violations but never ",
                "returns early. The lessons system is the model: `lessons` action with ops ",
                "read_candidates|promote|reject|encode|read|write feeds into ROLES.json via apply_promoted_lessons. ",
                "Invariants need the same closure: add `invariants` action (ops: read|promote|collapse|enforce) ",
                "to src/tools.rs dispatch + implement handle_invariants_action in src/invariants.rs. ",
                "Wire enforced_invariants.json into planner prompts via src/prompt_inputs.rs. ",
                "Impact: without this the invariant collapse pipeline (TO-DO.txt Phase 4-5) cannot complete."
            )
            .to_string(),
            location: "src/tools.rs; src/invariants.rs; src/app.rs:1011-1050; src/prompt_inputs.rs",
            evidence: &[
                "src/invariants.rs:evaluate_invariant_gate returns Err but app.rs route gate has blocked=false — never hard-blocks",
                "src/tools.rs dispatch table is missing the 'invariants' branch required to call handle_invariants_action",
                "src/tool_schema.rs is missing the invariants action schema, so invalid-action repair cannot guide the model toward legal invariants ops",
                "Without both dispatch + schema, planner cannot review, enforce, or collapse discovered invariants from enforced_invariants.json",
            ],
            resolved_note: "Resolved automatically after current-source validation: invariants action surface exists in src/tools.rs and src/tool_schema.rs.",
        },
    )
}

fn has_invariants_action_surface(workspace: &Path) -> bool {
    let has_invariants_dispatch =
        source_contains_any(
            workspace,
            "src/tools.rs",
            &[
                "\"invariants\" => {",
                "crate::invariants::handle_invariants_action_with_writer(",
            ],
        ) || (source_contains(workspace, "src/tools.rs", "include!(\"tools_tail.rs\")")
            && source_contains_any(
                workspace,
                "src/tools_tail.rs",
                &[
                    "\"invariants\" => {",
                    "crate::invariants::handle_invariants_action_with_writer(",
                ],
            ));

    has_invariants_dispatch
        && source_contains(
            workspace,
            "src/tool_schema.rs",
            "\"invariants\" => missing_field_for_invariants_action",
        )
}

fn sync_prompt_injection_meta_issue(
    file: &mut IssuesFile,
    existing_ids: &HashSet<String>,
    workspace: &Path,
) -> (usize, bool) {
    sync_invariant_meta_issue(
        file,
        existing_ids,
        has_enforced_invariants_prompt_injection(workspace),
        InvariantMetaIssueSpec {
            id: "inv_enforced_not_in_prompts",
            title: "enforced_invariants.json not injected into planner prompts",
            priority: "high",
            description: concat!(
                "agent_state/enforced_invariants.json is written by maybe_synthesize_invariants on every ",
                "checkpoint cycle but is invisible to all roles. Planner cannot see which invariants are ",
                "accumulating support and cannot decide which to escalate. ",
                "Fix: add read_enforced_invariants(workspace) call to load_planner_inputs in src/prompt_inputs.rs ",
                "and inject the result into planner prompt surfaces in src/prompts.rs. ",
                "Ensure SingleRoleContext::read(Invariants) returns the combined static + enforced view. ",
                "Impact: invariant system is silent — no feedback loop to the decision-making agent."
            )
            .to_string(),
            location: "src/prompt_inputs.rs; src/prompts.rs; src/invariants.rs:read_enforced_invariants",
            evidence: &[
                "src/prompt_inputs.rs does not call read_enforced_invariants(workspace) when building prompt inputs",
                "src/prompts.rs does not mention agent_state/enforced_invariants.json in the relevant role instructions",
                "Without the dynamic enforced view, planner can only see static INVARIANTS.json",
            ],
            resolved_note: "Resolved automatically after current-source validation: enforced_invariants.json is injected through prompt_inputs.rs and referenced in prompts.rs.",
        },
    )
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::result::Result<usize, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn generate_invariant_issues(workspace: &Path) -> Result<usize> {
    let mut file: IssuesFile = crate::issues::load_issues_file(workspace);

    let existing_ids: HashSet<String> = file.issues.iter().map(|i| i.id.clone()).collect();
    let mut mutated = false;

    let inv_file = load_enforced_invariants_file(workspace);
    let mut created = 0usize;

    // ── Meta-issue 1: action surface ────────────────────────────────────────
    let (created_delta, mutated_delta) =
        sync_action_surface_meta_issue(&mut file, &existing_ids, workspace);
    created += created_delta;
    mutated |= mutated_delta;

    // ── Meta-issue 2: prompt injection ──────────────────────────────────────
    let (created_delta, mutated_delta) =
        sync_prompt_injection_meta_issue(&mut file, &existing_ids, workspace);
    created += created_delta;
    mutated |= mutated_delta;

    // ── Per-promoted-invariant issues ────────────────────────────────────────
    // One issue per Promoted invariant whose gate is not yet enforced.
    // These give the planner concrete tasks: "evaluate INV-xxx for gate enforcement".
    for inv in &inv_file.invariants {
        if inv.status != InvariantStatus::Promoted {
            continue;
        }
        let inv_key = inv.id.to_lowercase().replace('-', "_");
        let issue_prefix = format!("inv_gate_unenforced_{inv_key}");
        let issue_id = issue_prefix.clone();
        // Skip if already present as any status (don't re-open a wontfix), or if
        // there's already an open issue under the same id prefix.
        let already_tracked = existing_ids.contains(&issue_id)
            || file
                .issues
                .iter()
                .filter(|i| !is_closed(i))
                .any(|i| i.id.starts_with(&issue_prefix));
        if already_tracked {
            continue;
        }

        let gates_str = if inv.gates.is_empty() {
            "route".to_string()
        } else {
            inv.gates.join(", ")
        };

        file.issues.push(Issue {
            id: issue_id,
            title: format!("Promoted invariant {} gate not yet enforced (support={})", inv.id, inv.support_count),
            status: "open".to_string(),
            priority: "high".to_string(),
            kind: "invariant_violation".to_string(),
            description: format!(
                "Invariant `{}` has been auto-promoted (support_count={} >= threshold) but its gate(s) [{}] \
                 are still observational (blocked=false). The invariant predicate: \"{}\". \
                 Planner should review this invariant and, if the predicate is correct, call \
                 `invariants op=enforce id={}` to flip the gate to hard-blocking. \
                 If the root cause has been structurally fixed, call `invariants op=collapse id={}`.",
                inv.id, inv.support_count, gates_str, inv.predicate_text, inv.id, inv.id
            ),
            location: format!("agent_state/enforced_invariants.json; src/app.rs:1011-1050"),
            evidence: vec![
                format!("invariant id={} support_count={} status=promoted", inv.id, inv.support_count),
                format!("first_seen_ms={} last_seen_ms={}", inv.first_seen_ms, inv.last_seen_ms),
                format!("predicate: {}", inv.predicate_text),
                format!("state_conditions: {}", inv.state_conditions.iter()
                    .map(|c| format!("{}={}", c.key, c.value))
                    .collect::<Vec<_>>().join(", ")),
            ],
            discovered_by: "invariants_analyzer".to_string(),
            score: 0.0,
            ..Issue::default()
        });
        created += 1;
    }

    if created > 0 || mutated {
        rescore_all(&mut file);
        persist_issues_projection_with_writer(workspace, &file, None, "generate_invariant_issues")?;
    }

    Ok(created)
}

fn has_enforced_invariants_prompt_injection(workspace: &Path) -> bool {
    source_contains(
        workspace,
        "src/prompt_inputs.rs",
        "read_enforced_invariants(workspace)",
    ) && source_contains(
        workspace,
        "src/prompts.rs",
        "agent_state/enforced_invariants.json",
    )
}

fn source_contains(workspace: &Path, relative_path: &str, needle: &str) -> bool {
    std::fs::read_to_string(workspace.join(relative_path))
        .map(|raw| raw.contains(needle))
        .unwrap_or(false)
}

fn source_contains_any(workspace: &Path, relative_path: &str, needles: &[&str]) -> bool {
    std::fs::read_to_string(workspace.join(relative_path))
        .map(|raw| needles.iter().all(|needle| raw.contains(needle)))
        .unwrap_or(false)
}

fn resolve_stale_meta_issue(file: &mut IssuesFile, issue_id: &str, note: &str) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut changed = false;
    for issue in &mut file.issues {
        if issue.id != issue_id || is_closed(issue) {
            continue;
        }
        issue.status = "resolved".to_string();
        issue.freshness_status = "fresh".to_string();
        issue.stale_reason.clear();
        issue.last_validated_ms = now_ms;
        if !issue.evidence.iter().any(|entry| entry == note) {
            issue.evidence.push(note.to_string());
        }
        changed = true;
    }
    changed
}

// ── Synthesis implementation ──────────────────────────────────────────────────

fn try_synthesize_invariants(workspace: &Path) -> Result<()> {
    // Primary input: classified blockers (structured, no heuristics needed).
    let blocker_prints = fingerprints_from_blockers(workspace);

    // Secondary input: action log for failure patterns not yet captured in blockers.
    let log_path = Path::new(crate::constants::agent_state_dir()).join(ACTION_LOG_SUBPATH);
    let log_prints = if log_path.exists() {
        let entries = read_tail_entries(log_path.as_path(), MAX_LINES_TO_SCAN);
        extract_failure_fingerprints(&entries)
    } else {
        vec![]
    };

    let mut all_prints = blocker_prints;
    all_prints.extend(log_prints);

    if all_prints.is_empty() {
        return Ok(());
    }

    let mut file = load_enforced_invariants_file(workspace);
    merge_fingerprints(&mut file, all_prints);
    promote_by_threshold(&mut file);
    // Enforce gate for any invariant that was explicitly set to Enforced.
    update_gate_enforcement(&mut file);
    prune_excess(&mut file);

    file.last_synthesized_ms = crate::logging::now_ms();
    persist_enforced_invariants_projection(workspace, &file, "enforced_invariants_save")?;
    Ok(())
}

/// Convert classified blocker records directly into fingerprints — no text heuristics.
fn fingerprints_from_blockers(workspace: &Path) -> Vec<Fingerprint> {
    let file = crate::blockers::load_blockers(workspace);
    file.blockers
        .iter()
        .map(|b| {
            let raw = serde_json::json!({
                "id": b.id,
                "error_class": b.error_class,
                "actor": b.actor,
                "task_id": b.task_id,
                "objective_id": b.objective_id,
                "summary": b.summary,
                "action_kind": b.action_kind,
                "source": b.source,
                "ts_ms": b.ts_ms,
            });
            let actor_kind = actor_kind_from_role(&b.actor);
            Fingerprint {
                conditions: vec![
                    crate::invariants::StateCondition {
                        key: "actor_kind".to_string(),
                        value: actor_kind.to_string(),
                    },
                    crate::invariants::StateCondition {
                        key: "error_class".to_string(),
                        value: b.error_class.as_key().to_string(),
                    },
                ],
                predicate_text: format!(
                    "Role `{actor_kind}` repeatedly encounters `{}`: {}",
                    b.error_class.as_key(),
                    b.error_class.description()
                ),
                ts_ms: b.ts_ms,
                evidence: InvariantEvidenceSample {
                    source: "agent_state/blockers.json".to_string(),
                    ts_ms: b.ts_ms,
                    derivation: EvidenceDerivation {
                        rule_type: "blocker_error_class".to_string(),
                        observed_facts: Vec::new(),
                        matched_conditions: Vec::new(),
                    },
                    raw,
                },
            }
        })
        .collect()
}

fn actor_kind_from_role(role: &str) -> &'static str {
    if role.starts_with("executor") {
        "executor"
    } else if role.starts_with("planner") {
        "planner"
    } else if role.starts_with("verifier") {
        "verifier"
    } else if role.starts_with("diagnostics") {
        "diagnostics"
    } else if role.starts_with("orchestrator") || role.starts_with("orchestrate") {
        "orchestrator"
    } else if role.starts_with("solo") {
        "solo"
    } else {
        "unknown"
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &mut invariants::EnforcedInvariantsFile
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn update_gate_enforcement(file: &mut EnforcedInvariantsFile) {
    for inv in file
        .invariants
        .iter_mut()
        .filter(|inv| inv.status == InvariantStatus::Enforced && inv.gates.is_empty())
    {
        inv.gates = default_gates_for_conditions(&inv.state_conditions);
    }
}

// ── Fingerprint extraction ────────────────────────────────────────────────────

/// A raw failure fingerprint extracted from one log entry.
#[derive(Debug, Clone)]
struct Fingerprint {
    conditions: Vec<StateCondition>,
    predicate_text: String,
    ts_ms: u64,
    evidence: InvariantEvidenceSample,
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &[serde_json::Value]
/// Outputs: std::vec::Vec<invariants::Fingerprint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_failure_fingerprints(entries: &[Value]) -> Vec<Fingerprint> {
    let mut prints = Vec::new();

    for entry in entries {
        let phase = entry.get("phase").and_then(|v| v.as_str()).unwrap_or("");
        let ok = entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
        let ts_ms = entry.get("ts_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let text = entry.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let action = entry
            .get("action")
            .or_else(|| entry.get("op"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let actor = entry.get("actor").and_then(|v| v.as_str()).unwrap_or("");

        if let Some(fp) =
            extract_tool_failure_fingerprint(entry, phase, ok, actor, action, text, ts_ms)
        {
            prints.push(fp);
        }
        if let Some(fp) = extract_plan_preflight_fingerprint(entry, action, ok, ts_ms) {
            prints.push(fp);
        }
        if let Some(fp) = extract_forced_handoff_fingerprint(entry, actor, action, text, ts_ms) {
            prints.push(fp);
        }
        if let Some(fp) = extract_read_file_failure_fingerprint(entry, actor, action, ok, ts_ms) {
            prints.push(fp);
        }
        if let Some(fp) = extract_invalid_action_fingerprint(entry, actor, phase, ok, text, ts_ms) {
            prints.push(fp);
        }
        if let Some(fp) = extract_missing_target_fingerprint(entry, actor, phase, ok, text, ts_ms) {
            prints.push(fp);
        }
    }

    prints
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value, &str, bool, &str, &str, &str, u64
/// Outputs: std::option::Option<invariants::Fingerprint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_tool_failure_fingerprint(
    entry: &Value,
    phase: &str,
    ok: bool,
    actor: &str,
    action: &str,
    text: &str,
    ts_ms: u64,
) -> Option<Fingerprint> {
    if phase == "result" && !ok {
        return fingerprint_tool_failure(entry, actor, action, text, ts_ms);
    }
    None
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value, &str, bool, u64
/// Outputs: std::option::Option<invariants::Fingerprint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_plan_preflight_fingerprint(
    entry: &Value,
    action: &str,
    ok: bool,
    ts_ms: u64,
) -> Option<Fingerprint> {
    if action != "plan_preflight" || ok {
        return None;
    }

    Some(Fingerprint {
        conditions: vec![
            StateCondition { key: "action".to_string(), value: "plan_preflight".to_string() },
            StateCondition { key: "ok".to_string(), value: "false".to_string() },
        ],
        predicate_text: "Planner task referenced a symbol not found in the workspace semantic graph; executor cannot execute it".to_string(),
        ts_ms,
        evidence: failure_evidence_sample(entry, ts_ms),
    })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value, &str, &str, &str, u64
/// Outputs: std::option::Option<invariants::Fingerprint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_forced_handoff_fingerprint(
    entry: &Value,
    actor: &str,
    action: &str,
    text: &str,
    ts_ms: u64,
) -> Option<Fingerprint> {
    let mentions_handoff =
        action == "message" || text.contains("FORCED HANDOFF") || text.contains("step budget");
    let is_forced = (actor.starts_with("executor") && text.contains("forced"))
        || text.contains("step limit")
        || text.contains("FORCED HANDOFF");

    if !mentions_handoff || !is_forced {
        return None;
    }

    Some(Fingerprint {
        conditions: vec![
            StateCondition { key: "actor_kind".to_string(), value: "executor".to_string() },
            StateCondition { key: "error".to_string(), value: "step_limit_exceeded".to_string() },
        ],
        predicate_text: "Executor reached step limit without completing task — task scope is too large or executor is stalling".to_string(),
        ts_ms,
        evidence: failure_evidence_sample(entry, ts_ms),
    })
}

/// Intent: pure_transform
/// Resource: memory
/// Inputs: &serde_json::Value, &str, &str, bool, u64
/// Outputs: std::option::Option<invariants::Fingerprint>
/// Effects: none
/// Forbidden: fs_write, uses_network, spawns_process
/// Invariants: deterministic_for_same_inputs
/// Failure: infallible
/// Provenance: rustc:facts + rustc:docstring
fn extract_read_file_failure_fingerprint(
    entry: &Value,
    actor: &str,
    action: &str,
    ok: bool,
    ts_ms: u64,
) -> Option<Fingerprint> {
    if action != "read_file" || ok {
        return None;
    }

    let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let actor_kind = if actor.starts_with("executor") {
        "executor"
    } else {
        "solo"
    };

    Some(Fingerprint {
        conditions: vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: actor_kind.to_string(),
            },
            StateCondition {
                key: "action".to_string(),
                value: "read_file".to_string(),
            },
            StateCondition {
                key: "ok".to_string(),
                value: "false".to_string(),
            },
        ],
        predicate_text: format!(
            "read_file failed (path may not exist or be outside workspace): {path}"
        ),
        ts_ms,
        evidence: failure_evidence_sample(entry, ts_ms),
    })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value, &str, &str, bool, &str, u64
/// Outputs: std::option::Option<invariants::Fingerprint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_invalid_action_fingerprint(
    entry: &Value,
    actor: &str,
    phase: &str,
    ok: bool,
    text: &str,
    ts_ms: u64,
) -> Option<Fingerprint> {
    let is_invalid_action = text.contains("invalid action")
        || text.contains("schema violation")
        || text.contains("required field");
    if !is_invalid_action || phase != "result" || ok {
        return None;
    }

    let actor_kind = if actor.starts_with("executor") {
        "executor"
    } else if actor.starts_with("planner") {
        "planner"
    } else {
        "unknown"
    };

    Some(Fingerprint {
        conditions: vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: actor_kind.to_string(),
            },
            StateCondition {
                key: "error".to_string(),
                value: "invalid_action_schema".to_string(),
            },
        ],
        predicate_text: format!(
            "Role `{actor_kind}` emitted a structurally invalid action — schema gate violation"
        ),
        ts_ms,
        evidence: failure_evidence_sample(entry, ts_ms),
    })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &serde_json::Value, &str, &str, bool, &str, u64
/// Outputs: std::option::Option<invariants::Fingerprint>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn extract_missing_target_fingerprint(
    entry: &Value,
    actor: &str,
    phase: &str,
    ok: bool,
    text: &str,
    ts_ms: u64,
) -> Option<Fingerprint> {
    let missing_target = text.contains("missing_target")
        || (text.contains("does not exist") && phase == "result" && !ok);
    if !missing_target {
        return None;
    }

    let actor_kind = if actor.starts_with("executor") {
        "executor"
    } else {
        "any"
    };
    Some(Fingerprint {
        conditions: vec![
            StateCondition { key: "actor_kind".to_string(), value: actor_kind.to_string() },
            StateCondition { key: "error".to_string(), value: "missing_target".to_string() },
        ],
        predicate_text: "Action targeted a path that does not exist — plan is referencing a target that has not been created yet".to_string(),
        ts_ms,
        evidence: failure_evidence_sample(entry, ts_ms),
    })
}

fn failure_evidence_sample(entry: &Value, ts_ms: u64) -> InvariantEvidenceSample {
    InvariantEvidenceSample {
        source: "agent_state/default/actions.jsonl".to_string(),
        ts_ms,
        derivation: EvidenceDerivation {
            rule_type: "action_log_failure".to_string(),
            observed_facts: Vec::new(),
            matched_conditions: Vec::new(),
        },
        raw: entry.clone(),
    }
}

fn fingerprint_tool_failure(
    entry: &Value,
    actor: &str,
    action: &str,
    text: &str,
    ts_ms: u64,
) -> Option<Fingerprint> {
    // Categorize the error kind from text heuristics.
    let error_kind = if text.contains("permission denied") || text.contains("access denied") {
        "permission_denied"
    } else if text.contains("not found") || text.contains("No such file") {
        "not_found"
    } else if text.contains("timed out") || text.contains("timeout") {
        "timeout"
    } else if text.contains("parse error") || text.contains("invalid JSON") {
        "parse_error"
    } else if text.contains("compilation error") || text.contains("error[E") {
        "compile_error"
    } else {
        // Generic failure — skip to avoid noise.
        return None;
    };

    let actor_kind = if actor.starts_with("executor") {
        "executor"
    } else if actor.starts_with("planner") {
        "planner"
    } else if actor.starts_with("verifier") {
        "verifier"
    } else if actor.starts_with("diagnostics") {
        "diagnostics"
    } else if actor.starts_with("orchestrator") {
        "orchestrator"
    } else {
        "unknown"
    };

    Some(Fingerprint {
        conditions: vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: actor_kind.to_string(),
            },
            StateCondition {
                key: "action".to_string(),
                value: action.to_string(),
            },
            StateCondition {
                key: "error".to_string(),
                value: error_kind.to_string(),
            },
        ],
        predicate_text: format!("Role `{actor_kind}` action `{action}` failed with `{error_kind}`"),
        ts_ms,
        evidence: InvariantEvidenceSample {
            source: "agent_state/default/actions.jsonl".to_string(),
            ts_ms,
            derivation: EvidenceDerivation {
                rule_type: "action_log_failure".to_string(),
                observed_facts: Vec::new(),
                matched_conditions: Vec::new(),
            },
            raw: entry.clone(),
        },
    })
}

fn derive_evidence_derivation(fp: &Fingerprint) -> EvidenceDerivation {
    let raw = &fp.evidence.raw;
    let source = fp.evidence.source.as_str();
    let mut observed_facts = Vec::new();

    if source.ends_with("blockers.json") {
        let actor = raw
            .get("actor")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let error_class = raw
            .get("error_class")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        observed_facts.push(format!("actor={actor}"));
        observed_facts.push(format!("error_class={error_class}"));
        if let Some(summary) = raw.get("summary").and_then(|v| v.as_str()) {
            let excerpt = summary
                .split_whitespace()
                .take(12)
                .collect::<Vec<_>>()
                .join(" ");
            if !excerpt.is_empty() {
                observed_facts.push(format!("summary={excerpt}"));
            }
        }
    } else {
        let actor = raw
            .get("actor")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let action = raw
            .get("action")
            .or_else(|| raw.get("op").and_then(|v| v.get("name")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let phase = raw
            .get("phase")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let ok = raw.get("ok").and_then(|v| v.as_bool());
        let status = ok
            .map(|v| if v { "ok=true" } else { "ok=false" })
            .unwrap_or("ok=unknown");
        observed_facts.push(format!("actor={actor}"));
        observed_facts.push(format!("action={action}"));
        observed_facts.push(format!("phase={phase}"));
        observed_facts.push(status.to_string());

        if let Some(text) = raw.get("text").and_then(|v| v.as_str()) {
            let excerpt = text
                .split_whitespace()
                .take(12)
                .collect::<Vec<_>>()
                .join(" ");
            if !excerpt.is_empty() {
                observed_facts.push(format!("excerpt={excerpt}"));
            }
        } else if let Some(summary) = raw.get("summary").and_then(|v| v.as_str()) {
            let excerpt = summary
                .split_whitespace()
                .take(12)
                .collect::<Vec<_>>()
                .join(" ");
            if !excerpt.is_empty() {
                observed_facts.push(format!("summary={excerpt}"));
            }
        }
    }

    EvidenceDerivation {
        rule_type: if source.ends_with("blockers.json") {
            "blocker_error_class".to_string()
        } else {
            "action_log_failure".to_string()
        },
        observed_facts,
        matched_conditions: fp.conditions.clone(),
    }
}

fn with_derivation(fp: &Fingerprint) -> InvariantEvidenceSample {
    let mut sample = fp.evidence.clone();
    sample.derivation = derive_evidence_derivation(fp);
    sample
}

// ── Fingerprint → ID ──────────────────────────────────────────────────────────

/// Canonical ID from sorted state conditions — same conditions → same ID across runs.
fn fingerprint_id(conditions: &[StateCondition]) -> String {
    let mut pairs: Vec<String> = conditions
        .iter()
        .map(|c| format!("{}={}", c.key, c.value))
        .collect();
    pairs.sort();
    // Hash-like: use a short deterministic prefix.
    let raw = pairs.join(";");
    let hash = fnv1a_32(raw.as_bytes());
    format!("INV-{hash:08x}")
}

fn fnv1a_32(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

// ── Merge and promote ─────────────────────────────────────────────────────────

fn merge_fingerprints(file: &mut EnforcedInvariantsFile, prints: Vec<Fingerprint>) {
    // Group prints by id.
    let mut by_id: HashMap<String, (Vec<Fingerprint>, String)> = HashMap::new();
    for fp in prints {
        let id = fingerprint_id(&fp.conditions);
        let entry = by_id
            .entry(id.clone())
            .or_insert_with(|| (Vec::new(), fp.predicate_text.clone()));
        entry.0.push(fp);
    }

    let now = crate::logging::now_ms();

    for (id, (fps, predicate_text)) in by_id {
        let count = fps.len();
        let min_ts = fps.iter().map(|f| f.ts_ms).min().unwrap_or(now);
        let max_ts = fps.iter().map(|f| f.ts_ms).max().unwrap_or(now);
        let evidence = collect_evidence_samples(&fps);
        let conditions = fps
            .into_iter()
            .next()
            .map(|f| f.conditions)
            .unwrap_or_default();

        if let Some(existing) = file.invariants.iter_mut().find(|i| i.id == id) {
            existing.support_count += count;
            if max_ts > existing.last_seen_ms {
                existing.last_seen_ms = max_ts;
            }
            if min_ts < existing.first_seen_ms {
                existing.first_seen_ms = min_ts;
            }
            merge_evidence(&mut existing.evidence, evidence);
        } else {
            file.invariants.push(DiscoveredInvariant {
                id,
                predicate_text,
                state_conditions: conditions,
                support_count: count,
                status: InvariantStatus::Discovered,
                gates: Vec::new(),
                evidence,
                first_seen_ms: min_ts,
                last_seen_ms: max_ts,
            });
        }
    }
}

fn collect_evidence_samples(fps: &[Fingerprint]) -> Vec<InvariantEvidenceSample> {
    let mut seen = HashSet::new();
    let mut ordered: Vec<&Fingerprint> = fps.iter().collect();
    ordered.sort_by_key(|fp| fp.ts_ms);

    let mut out = Vec::new();
    for fp in ordered {
        let key = serde_json::to_string(&fp.evidence.raw).unwrap_or_default();
        if !seen.insert(key) {
            continue;
        }
        out.push(with_derivation(fp));
        if out.len() >= MAX_EVIDENCE_SAMPLES {
            break;
        }
    }
    out
}

fn merge_evidence(
    existing: &mut Vec<InvariantEvidenceSample>,
    incoming: Vec<InvariantEvidenceSample>,
) {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for sample in existing.iter().chain(incoming.iter()) {
        let key = serde_json::to_string(&sample.raw).unwrap_or_default();
        if !seen.insert(key) {
            continue;
        }
        merged.push(sample.clone());
        if merged.len() >= MAX_EVIDENCE_SAMPLES {
            break;
        }
    }
    *existing = merged;
}

/// Auto-promote invariants whose support_count crosses MIN_INVARIANT_SUPPORT.
fn promote_by_threshold(file: &mut EnforcedInvariantsFile) {
    for inv in file.invariants.iter_mut() {
        if inv.status == InvariantStatus::Discovered && inv.support_count >= MIN_INVARIANT_SUPPORT {
            inv.status = InvariantStatus::Promoted;
            // Assign default gates based on conditions.
            if inv.gates.is_empty() {
                inv.gates = default_gates_for_conditions(&inv.state_conditions);
            }
            eprintln!(
                "[invariants] promoted: {} (support={})",
                inv.id, inv.support_count
            );
        }
    }
}

fn push_gate_once(gates: &mut Vec<String>, gate: &str) {
    if !gates.iter().any(|existing| existing == gate) {
        gates.push(gate.to_string());
    }
}

fn default_gates_for_conditions(conditions: &[StateCondition]) -> Vec<String> {
    let mut gates = Vec::new();
    let error_class = conditions
        .iter()
        .find(|cond| cond.key == "error_class")
        .map(|cond| cond.value.as_str());

    match error_class {
        Some("runtime_control_bypass")
        | Some("checkpoint_runtime_divergence")
        | Some("ambiguous_control_event") => push_gate_once(&mut gates, "route"),
        Some("uncanonicalized_recovery_path") => push_gate_once(&mut gates, "executor"),
        Some("effectful_state_advance_without_control_event") => {
            push_gate_once(&mut gates, "route");
            push_gate_once(&mut gates, "planner");
        }
        _ => {}
    }

    for cond in conditions {
        if cond.key == "actor_kind" {
            match cond.value.as_str() {
                "executor" => push_gate_once(&mut gates, "executor"),
                "planner" => push_gate_once(&mut gates, "planner"),
                _ => push_gate_once(&mut gates, "route"),
            }
        }
    }
    if gates.is_empty() {
        push_gate_once(&mut gates, "route");
    }
    gates
}

/// Remove excess entries to keep the file manageable.
fn prune_excess(file: &mut EnforcedInvariantsFile) {
    let mut discovered: Vec<_> = file
        .invariants
        .iter()
        .enumerate()
        .filter(|(_, i)| i.status == InvariantStatus::Discovered)
        .map(|(idx, i)| (idx, i.support_count, i.last_seen_ms))
        .collect();

    // Sort by support_count desc, then last_seen_ms desc.
    discovered.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));

    if discovered.len() > MAX_INVARIANTS_PER_STATUS {
        let to_remove: std::collections::BTreeSet<usize> = discovered
            .into_iter()
            .skip(MAX_INVARIANTS_PER_STATUS)
            .map(|(idx, _, _)| idx)
            .collect();
        let mut i = 0usize;
        file.invariants.retain(|_| {
            let keep = !to_remove.contains(&i);
            i += 1;
            keep
        });
    }
}

// ── File I/O ──────────────────────────────────────────────────────────────────

fn invariants_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(ENFORCED_INVARIANTS_FILE)
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: invariants::EnforcedInvariantsFile
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn load_enforced_invariants_file(workspace: &Path) -> EnforcedInvariantsFile {
    if let Some(file) = load_invariants_from_tlog(workspace) {
        return normalize_loaded_invariants(file);
    }
    let path = invariants_path(workspace);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if !raw.trim().is_empty() {
        if let Ok(file) = serde_json::from_str::<EnforcedInvariantsFile>(&raw) {
            return normalize_loaded_invariants(file);
        }
    }
    EnforcedInvariantsFile {
        version: 1,
        ..Default::default()
    }
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &invariants::EnforcedInvariantsFile, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn persist_enforced_invariants_projection(
    workspace: &Path,
    file: &EnforcedInvariantsFile,
    subject: &str,
) -> Result<()> {
    persist_enforced_invariants_projection_with_writer(workspace, file, None, subject)
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &invariants::EnforcedInvariantsFile, std::option::Option<&mut canonical_writer::CanonicalWriter>, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn persist_enforced_invariants_projection_with_writer(
    workspace: &Path,
    file: &EnforcedInvariantsFile,
    mut writer: Option<&mut crate::canonical_writer::CanonicalWriter>,
    subject: &str,
) -> Result<()> {
    let normalized = normalize_loaded_invariants(file.clone());
    let path = invariants_path(workspace);
    crate::logging::record_json_projection_with_optional_writer(
        workspace,
        &path,
        ENFORCED_INVARIANTS_FILE,
        "write",
        subject,
        &normalized,
        writer.as_deref_mut(),
        Some(crate::events::EffectEvent::EnforcedInvariantsRecorded {
            file: normalized.clone(),
        }),
    )
}

pub fn reconcile_enforced_invariants_projection(workspace: &Path, subject: &str) -> Result<bool> {
    let Some(file) = load_invariants_from_tlog(workspace) else {
        return Ok(false);
    };
    let normalized = normalize_loaded_invariants(file);
    let canonical = serde_json::to_string_pretty(&normalized)?;
    let path = invariants_path(workspace);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if crate::logging::stable_hash_hex(&current) == crate::logging::stable_hash_hex(&canonical) {
        return Ok(false);
    }
    persist_enforced_invariants_projection(workspace, &normalized, subject)?;
    Ok(true)
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: invariants::EnforcedInvariantsFile
/// Outputs: invariants::EnforcedInvariantsFile
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn normalize_loaded_invariants(mut file: EnforcedInvariantsFile) -> EnforcedInvariantsFile {
    if file.version == 0 {
        file.version = 1;
    }
    let mut merged_by_id: HashMap<String, DiscoveredInvariant> = HashMap::new();
    for mut inv in file.invariants.drain(..) {
        normalize_single_invariant(&mut inv);
        let canonical_id = fingerprint_id(&inv.state_conditions);
        inv.id = canonical_id.clone();
        if let Some(existing) = merged_by_id.get_mut(&canonical_id) {
            merge_invariant_record(existing, inv);
        } else {
            merged_by_id.insert(canonical_id, inv);
        }
    }
    file.invariants = merged_by_id.into_values().collect();
    file.invariants.sort_by(|a, b| {
        status_rank(&b.status)
            .cmp(&status_rank(&a.status))
            .then(b.support_count.cmp(&a.support_count))
            .then(b.last_seen_ms.cmp(&a.last_seen_ms))
            .then(a.id.cmp(&b.id))
    });
    prune_excess(&mut file);
    file
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &mut invariants::DiscoveredInvariant
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn normalize_single_invariant(inv: &mut DiscoveredInvariant) {
    inv.state_conditions = canonicalize_conditions(std::mem::take(&mut inv.state_conditions));
    inv.gates = canonicalize_gates(std::mem::take(&mut inv.gates));
    inv.evidence = canonicalize_evidence(std::mem::take(&mut inv.evidence));
    if inv.last_seen_ms < inv.first_seen_ms {
        std::mem::swap(&mut inv.first_seen_ms, &mut inv.last_seen_ms);
    }
    if inv.status == InvariantStatus::Discovered && inv.support_count >= MIN_INVARIANT_SUPPORT {
        inv.status = InvariantStatus::Promoted;
    }
    if (inv.status == InvariantStatus::Promoted || inv.status == InvariantStatus::Enforced)
        && inv.gates.is_empty()
    {
        inv.gates = default_gates_for_conditions(&inv.state_conditions);
        inv.gates = canonicalize_gates(std::mem::take(&mut inv.gates));
    }
}

fn canonicalize_conditions(mut conditions: Vec<StateCondition>) -> Vec<StateCondition> {
    conditions.retain(|c| !c.key.trim().is_empty() && !c.value.trim().is_empty());
    conditions.sort_by(|a, b| a.key.cmp(&b.key).then(a.value.cmp(&b.value)));
    conditions.dedup_by(|a, b| a.key == b.key && a.value == b.value);
    conditions
}

fn canonicalize_gates(gates: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = gates
        .into_iter()
        .map(|g| g.trim().to_string())
        .filter(|g| g == "route" || g == "planner" || g == "executor")
        .collect();
    out.sort();
    out.dedup();
    out
}

fn canonicalize_evidence(evidence: Vec<InvariantEvidenceSample>) -> Vec<InvariantEvidenceSample> {
    let mut ordered = evidence;
    ordered.sort_by_key(|sample| sample.ts_ms);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for sample in ordered {
        let key = serde_json::to_string(&sample.raw).unwrap_or_default();
        if !seen.insert(key) {
            continue;
        }
        out.push(sample);
    }
    if out.len() > MAX_EVIDENCE_SAMPLES {
        out = out.split_off(out.len() - MAX_EVIDENCE_SAMPLES);
    }
    out
}

fn merge_invariant_record(existing: &mut DiscoveredInvariant, incoming: DiscoveredInvariant) {
    if incoming.predicate_text.len() > existing.predicate_text.len() {
        existing.predicate_text = incoming.predicate_text;
    }
    existing.support_count = existing.support_count.max(incoming.support_count);
    existing.first_seen_ms = std::cmp::min(existing.first_seen_ms, incoming.first_seen_ms);
    existing.last_seen_ms = std::cmp::max(existing.last_seen_ms, incoming.last_seen_ms);
    existing.status = stronger_status(existing.status.clone(), incoming.status);
    let mut gates = existing.gates.clone();
    gates.extend(incoming.gates);
    existing.gates = canonicalize_gates(gates);
    let mut evidence = existing.evidence.clone();
    evidence.extend(incoming.evidence);
    existing.evidence = canonicalize_evidence(evidence);
    normalize_single_invariant(existing);
}

fn stronger_status(a: InvariantStatus, b: InvariantStatus) -> InvariantStatus {
    if status_rank(&a) >= status_rank(&b) {
        a
    } else {
        b
    }
}

fn status_rank(status: &InvariantStatus) -> u8 {
    match status {
        InvariantStatus::Discovered => 0,
        InvariantStatus::Promoted => 1,
        InvariantStatus::Enforced => 2,
        InvariantStatus::Collapsed => 3,
    }
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::option::Option<invariants::EnforcedInvariantsFile>
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_invariants_from_tlog(workspace: &Path) -> Option<EnforcedInvariantsFile> {
    crate::tlog::Tlog::latest_effect_from_workspace(workspace, |event| match event {
        crate::events::EffectEvent::EnforcedInvariantsRecorded { file } => Some(file),
        _ => None,
    })
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, usize
/// Outputs: std::vec::Vec<serde_json::Value>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn read_tail_entries(log_path: &Path, max_lines: usize) -> Vec<Value> {
    let reader = match open_tail_reader(log_path) {
        Some(reader) => reader,
        None => return vec![],
    };
    let mut lines: Vec<String> = reader
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .collect();

    // Keep only the tail.
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }

    lines
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn open_tail_reader(log_path: &Path) -> Option<std::io::BufReader<std::fs::File>> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let file = std::fs::File::open(log_path).ok()?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Read up to last 2 MB to avoid loading huge logs.
    const MAX_BYTES: u64 = 2 * 1024 * 1024;
    let mut file = file;
    if file_len > MAX_BYTES {
        let _ = file.seek(SeekFrom::End(-(MAX_BYTES as i64)));
        // Skip possibly-partial first line.
        let mut reader = BufReader::new(&mut file);
        let mut _dummy = String::new();
        let _ = reader.read_line(&mut _dummy);
    }

    Some(BufReader::new(file))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Create a temporary directory under `std::env::temp_dir()`.
    fn make_workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "invariants_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("agent_state")).unwrap();
        dir
    }

    #[test]
    fn fingerprint_id_is_stable() {
        let conds = vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: "executor".to_string(),
            },
            StateCondition {
                key: "error".to_string(),
                value: "not_found".to_string(),
            },
        ];
        let id1 = fingerprint_id(&conds);
        // Reversed order — should produce the same ID because we sort.
        let conds_rev = vec![
            StateCondition {
                key: "error".to_string(),
                value: "not_found".to_string(),
            },
            StateCondition {
                key: "actor_kind".to_string(),
                value: "executor".to_string(),
            },
        ];
        let id2 = fingerprint_id(&conds_rev);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("INV-"));
    }

    #[test]
    fn different_conditions_produce_different_ids() {
        let c1 = vec![StateCondition {
            key: "error".to_string(),
            value: "timeout".to_string(),
        }];
        let c2 = vec![StateCondition {
            key: "error".to_string(),
            value: "not_found".to_string(),
        }];
        assert_ne!(fingerprint_id(&c1), fingerprint_id(&c2));
    }

    #[test]
    fn promotion_triggers_at_threshold() {
        let mut file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![DiscoveredInvariant {
                id: "INV-test".to_string(),
                predicate_text: "test".to_string(),
                state_conditions: vec![StateCondition {
                    key: "actor_kind".to_string(),
                    value: "executor".to_string(),
                }],
                support_count: MIN_INVARIANT_SUPPORT,
                status: InvariantStatus::Discovered,
                gates: vec![],
                evidence: vec![],
                first_seen_ms: 0,
                last_seen_ms: 1,
            }],
        };
        promote_by_threshold(&mut file);
        assert_eq!(file.invariants[0].status, InvariantStatus::Promoted);
        assert!(!file.invariants[0].gates.is_empty());
    }

    #[test]
    fn below_threshold_stays_discovered() {
        let mut file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![DiscoveredInvariant {
                id: "INV-test".to_string(),
                predicate_text: "test".to_string(),
                state_conditions: vec![],
                support_count: MIN_INVARIANT_SUPPORT - 1,
                status: InvariantStatus::Discovered,
                gates: vec![],
                evidence: vec![],
                first_seen_ms: 0,
                last_seen_ms: 0,
            }],
        };
        promote_by_threshold(&mut file);
        assert_eq!(file.invariants[0].status, InvariantStatus::Discovered);
    }

    #[test]
    fn loophole_error_classes_receive_strong_default_gates() {
        let route_conditions = vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: "orchestrator".to_string(),
            },
            StateCondition {
                key: "error_class".to_string(),
                value: "runtime_control_bypass".to_string(),
            },
        ];
        let executor_conditions = vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: "executor".to_string(),
            },
            StateCondition {
                key: "error_class".to_string(),
                value: "uncanonicalized_recovery_path".to_string(),
            },
        ];
        let ambiguous_conditions = vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: "orchestrator".to_string(),
            },
            StateCondition {
                key: "error_class".to_string(),
                value: "ambiguous_control_event".to_string(),
            },
        ];
        let effectful_conditions = vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: "orchestrator".to_string(),
            },
            StateCondition {
                key: "error_class".to_string(),
                value: "effectful_state_advance_without_control_event".to_string(),
            },
        ];

        let route_gates = default_gates_for_conditions(&route_conditions);
        let executor_gates = default_gates_for_conditions(&executor_conditions);
        let ambiguous_gates = default_gates_for_conditions(&ambiguous_conditions);
        let effectful_gates = default_gates_for_conditions(&effectful_conditions);

        assert!(route_gates.contains(&"route".to_string()));
        assert!(executor_gates.contains(&"executor".to_string()));
        assert!(ambiguous_gates.contains(&"route".to_string()));
        assert!(effectful_gates.contains(&"route".to_string()));
        assert!(effectful_gates.contains(&"planner".to_string()));
    }

    #[test]
    fn synthesize_runtime_control_bypass_from_blockers_and_block_route() {
        let tmp = make_workspace();
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        crate::blockers::record_action_failure_with_writer(
            &tmp,
            None,
            "orchestrate",
            "runtime_control_bypass",
            "runtime-only control influence: planner was re-pended because plan mtimes changed",
            None,
        );
        crate::blockers::record_action_failure_with_writer(
            &tmp,
            None,
            "orchestrate",
            "runtime_control_bypass",
            "runtime-only control influence: diagnostics were re-pended because reconciled text diverged",
            None,
        );
        crate::blockers::record_action_failure_with_writer(
            &tmp,
            None,
            "orchestrate",
            "runtime_control_bypass",
            "runtime-only control influence: active blocker file suppressed planner dispatch",
            None,
        );

        maybe_synthesize_invariants(&tmp);
        let file = load_enforced_invariants_file(&tmp);
        let inv = file
            .invariants
            .iter()
            .find(|inv| {
                inv.state_conditions
                    .iter()
                    .any(|c| c.key == "error_class" && c.value == "runtime_control_bypass")
            })
            .expect("runtime_control_bypass invariant should be synthesized");
        assert_eq!(inv.status, InvariantStatus::Promoted);
        assert!(inv.gates.contains(&"route".to_string()));

        let mut state = HashMap::new();
        state.insert("actor_kind".to_string(), "orchestrator".to_string());
        state.insert(
            "error_class".to_string(),
            "runtime_control_bypass".to_string(),
        );
        let result = evaluate_invariant_gate("route", &state, &tmp);
        assert!(
            result.is_err(),
            "route gate should block runtime_control_bypass"
        );
    }

    #[test]
    fn synthesize_uncanonicalized_recovery_from_blockers_and_block_executor() {
        let tmp = make_workspace();
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        for summary in [
            "recovery path without canonical event: late submit_ack reconstructed turn lane=executor-0 tab_id=7 turn_id=11",
            "recovery path without canonical event: missing submit_ack forced lane requeue lane=executor-0 output=missing submit_ack",
            "recovery path without canonical event: executor completion recovered from runtime submit state lane=executor-0 tab_id=7 turn_id=11",
        ] {
            crate::blockers::record_action_failure_with_writer(
                &tmp,
                None,
                "executor",
                "uncanonicalized_recovery",
                summary,
                None,
            );
        }

        maybe_synthesize_invariants(&tmp);
        let file = load_enforced_invariants_file(&tmp);
        let inv = file
            .invariants
            .iter()
            .find(|inv| {
                inv.state_conditions
                    .iter()
                    .any(|c| c.key == "error_class" && c.value == "uncanonicalized_recovery_path")
            })
            .expect("uncanonicalized_recovery invariant should be synthesized");
        assert_eq!(inv.status, InvariantStatus::Promoted);
        assert!(inv.gates.contains(&"executor".to_string()));

        let mut state = HashMap::new();
        state.insert("actor_kind".to_string(), "executor".to_string());
        state.insert(
            "error_class".to_string(),
            "uncanonicalized_recovery_path".to_string(),
        );
        let result = evaluate_invariant_gate("executor", &state, &tmp);
        assert!(
            result.is_err(),
            "executor gate should block uncanonicalized recovery path"
        );
    }

    #[test]
    fn synthesize_ambiguous_control_event_from_blockers_and_block_route() {
        let tmp = make_workspace();
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        for summary in [
            "ambiguous control event: planner wakeup encoded both objective review and plan gap",
            "ambiguous control event: diagnostics wakeup encoded both verifier followup and reconciliation rerun",
            "ambiguous control event: one control event encoded multiple scheduler reasons",
        ] {
            crate::blockers::record_action_failure_with_writer(
                &tmp,
                None,
                "orchestrate",
                "ambiguous_control_event",
                summary,
                None,
            );
        }

        maybe_synthesize_invariants(&tmp);
        let file = load_enforced_invariants_file(&tmp);
        let inv = file
            .invariants
            .iter()
            .find(|inv| {
                inv.state_conditions
                    .iter()
                    .any(|c| c.key == "error_class" && c.value == "ambiguous_control_event")
            })
            .expect("ambiguous_control_event invariant should be synthesized");
        assert_eq!(inv.status, InvariantStatus::Promoted);
        assert!(inv.gates.contains(&"route".to_string()));

        let mut state = HashMap::new();
        state.insert("actor_kind".to_string(), "orchestrator".to_string());
        state.insert(
            "error_class".to_string(),
            "ambiguous_control_event".to_string(),
        );
        let result = evaluate_invariant_gate("route", &state, &tmp);
        assert!(
            result.is_err(),
            "route gate should block ambiguous control event"
        );
    }

    #[test]
    fn synthesize_effectful_state_advance_from_blockers_and_block_route_and_planner() {
        let tmp = make_workspace();
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();
        for summary in [
            "effectful state advance without control event: diagnostics reconciliation rewrote diagnostics output before canonical diagnostics-text transition",
            "effectful state advance without control event: checkpoint persisted externally visible state before canonical transition",
            "effectful state advance without control event: side effect changed planner-visible output before control event",
        ] {
            crate::blockers::record_action_failure_with_writer(
                &tmp,
                None,
                "orchestrate",
                "effectful_state_advance",
                summary,
                None,
            );
        }

        maybe_synthesize_invariants(&tmp);
        let file = load_enforced_invariants_file(&tmp);
        let inv = file
            .invariants
            .iter()
            .find(|inv| {
                inv.state_conditions.iter().any(|c| {
                    c.key == "error_class"
                        && c.value == "effectful_state_advance_without_control_event"
                })
            })
            .expect("effectful_state_advance invariant should be synthesized");
        assert_eq!(inv.status, InvariantStatus::Promoted);
        assert!(inv.gates.contains(&"route".to_string()));
        assert!(inv.gates.contains(&"planner".to_string()));

        let mut state = HashMap::new();
        state.insert("actor_kind".to_string(), "orchestrator".to_string());
        state.insert(
            "error_class".to_string(),
            "effectful_state_advance_without_control_event".to_string(),
        );
        let route_result = evaluate_invariant_gate("route", &state, &tmp);
        assert!(
            route_result.is_err(),
            "route gate should block effectful state advance without control event"
        );
        let planner_result = evaluate_invariant_gate("planner", &state, &tmp);
        assert!(
            planner_result.is_err(),
            "planner gate should block effectful state advance without control event"
        );
    }

    #[test]
    fn gate_blocks_matching_state() {
        let tmp = make_workspace();
        // Write an enforced invariant manually.
        let file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![DiscoveredInvariant {
                id: "INV-test".to_string(),
                predicate_text: "executor must not run when ready_tasks=0".to_string(),
                state_conditions: vec![
                    StateCondition {
                        key: "proposed_role".to_string(),
                        value: "executor".to_string(),
                    },
                    StateCondition {
                        key: "ready_tasks".to_string(),
                        value: "0".to_string(),
                    },
                ],
                support_count: 5,
                status: InvariantStatus::Promoted,
                gates: vec!["executor".to_string()],
                evidence: vec![],
                first_seen_ms: 0,
                last_seen_ms: 1,
            }],
        };
        persist_enforced_invariants_projection(tmp.as_path(), &file, "enforced_invariants_save")
            .unwrap();

        let mut state = HashMap::new();
        state.insert("proposed_role".to_string(), "executor".to_string());
        state.insert("ready_tasks".to_string(), "0".to_string());

        let result = evaluate_invariant_gate("executor", &state, tmp.as_path());
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("executor"));
    }

    #[test]
    fn gate_passes_non_matching_state() {
        let tmp = make_workspace();
        let file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![DiscoveredInvariant {
                id: "INV-test".to_string(),
                predicate_text: "executor must not run when ready_tasks=0".to_string(),
                state_conditions: vec![
                    StateCondition {
                        key: "proposed_role".to_string(),
                        value: "executor".to_string(),
                    },
                    StateCondition {
                        key: "ready_tasks".to_string(),
                        value: "0".to_string(),
                    },
                ],
                support_count: 5,
                status: InvariantStatus::Promoted,
                gates: vec!["executor".to_string()],
                evidence: vec![],
                first_seen_ms: 0,
                last_seen_ms: 1,
            }],
        };
        persist_enforced_invariants_projection(tmp.as_path(), &file, "enforced_invariants_save")
            .unwrap();

        // ready_tasks=1 — gate should pass.
        let mut state = HashMap::new();
        state.insert("ready_tasks".to_string(), "1".to_string());

        let result = evaluate_invariant_gate("executor", &state, tmp.as_path());
        assert!(result.is_ok());
    }

    #[test]
    fn extract_preflight_failure() {
        let entries = vec![json!({
            "kind": "tool",
            "phase": "result",
            "action": "plan_preflight",
            "ok": false,
            "actor": "orchestrator",
            "text": "missing symbol canon_mini_agent::app::nonexistent",
            "ts_ms": 1000u64,
        })];
        let fps = extract_failure_fingerprints(&entries);
        assert!(!fps.is_empty());
        assert!(fps.iter().any(|f| {
            f.conditions
                .iter()
                .any(|c| c.key == "action" && c.value == "plan_preflight")
        }));
        let derivation = derive_evidence_derivation(&fps[0]);
        assert_eq!(derivation.rule_type, "action_log_failure");
        assert!(derivation
            .observed_facts
            .iter()
            .any(|fact| fact == "actor=orchestrator"));
        assert!(derivation
            .observed_facts
            .iter()
            .any(|fact| fact == "action=plan_preflight"));
        assert!(derivation
            .matched_conditions
            .iter()
            .any(|c| c.key == "action" && c.value == "plan_preflight"));
    }

    #[test]
    fn semantic_matching_rule_uses_raw_sample_fields() {
        let fp = Fingerprint {
            conditions: vec![
                StateCondition {
                    key: "actor_kind".to_string(),
                    value: "planner".to_string(),
                },
                StateCondition {
                    key: "error_class".to_string(),
                    value: "invalid_schema".to_string(),
                },
            ],
            predicate_text: "Role `planner` repeatedly encounters `invalid_schema`".to_string(),
            ts_ms: 1,
            evidence: InvariantEvidenceSample {
                source: "agent_state/blockers.json".to_string(),
                ts_ms: 1,
                derivation: EvidenceDerivation::default(),
                raw: json!({
                    "id": "blk-planner-invalid_schema-1",
                    "error_class": "invalid_schema",
                    "actor": "planner",
                    "summary": "action schema invalid: missing required fields",
                    "action_kind": "schema_validation",
                    "source": "action_result",
                    "ts_ms": 1,
                }),
            },
        };
        let derivation = derive_evidence_derivation(&fp);
        assert_eq!(derivation.rule_type, "blocker_error_class");
        assert!(derivation
            .observed_facts
            .iter()
            .any(|fact| fact == "actor=planner"));
        assert!(derivation
            .observed_facts
            .iter()
            .any(|fact| fact == "error_class=invalid_schema"));
        assert!(derivation
            .observed_facts
            .iter()
            .any(|fact| fact == "summary=action schema invalid: missing required fields"));
        assert!(derivation
            .matched_conditions
            .iter()
            .any(|c| c.key == "actor_kind" && c.value == "planner"));
    }

    #[test]
    fn generate_invariant_issues_creates_meta_issues_on_empty_workspace() {
        let tmp = make_workspace();
        // Create a minimal ISSUES.json so the generator can load it.
        let issues_path = tmp.join(crate::constants::ISSUES_FILE);
        std::fs::write(&issues_path, r#"{"version":0,"issues":[]}"#).unwrap();

        let created = generate_invariant_issues(tmp.as_path()).unwrap();

        // Expect 2 meta-issues: action surface + prompt injection.
        assert_eq!(created, 2, "expected 2 meta-issues on empty workspace");

        let raw = std::fs::read_to_string(&issues_path).unwrap();
        let file: IssuesFile = serde_json::from_str(&raw).unwrap();
        let ids: Vec<&str> = file.issues.iter().map(|i| i.id.as_str()).collect();
        assert!(
            ids.contains(&"inv_action_surface_missing"),
            "action surface issue missing"
        );
        assert!(
            ids.contains(&"inv_enforced_not_in_prompts"),
            "prompt injection issue missing"
        );
        // Scores should be > 0 after rescore_all.
        for issue in &file.issues {
            assert!(issue.score > 0.0, "score should be non-zero after rescore");
        }
    }

    #[test]
    fn generate_invariant_issues_idempotent() {
        let tmp = make_workspace();
        let issues_path = tmp.join(crate::constants::ISSUES_FILE);
        std::fs::write(&issues_path, r#"{"version":0,"issues":[]}"#).unwrap();

        let first = generate_invariant_issues(tmp.as_path()).unwrap();
        let second = generate_invariant_issues(tmp.as_path()).unwrap();

        assert_eq!(first, 2);
        assert_eq!(
            second, 0,
            "second call should create no new issues (idempotent)"
        );
    }

    #[test]
    fn generate_invariant_issues_creates_per_promoted_issue() {
        let tmp = make_workspace();
        let issues_path = tmp.join(crate::constants::ISSUES_FILE);
        std::fs::write(&issues_path, r#"{"version":0,"issues":[]}"#).unwrap();

        let expected_inv_id = fingerprint_id(&[
            StateCondition {
                key: "proposed_role".to_string(),
                value: "executor".to_string(),
            },
            StateCondition {
                key: "ready_tasks".to_string(),
                value: "0".to_string(),
            },
        ]);
        let expected_issue_id = format!(
            "inv_gate_unenforced_{}",
            expected_inv_id.to_lowercase().replace('-', "_")
        );

        // Write a promoted invariant into enforced_invariants.json.
        let inv_file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![DiscoveredInvariant {
                id: expected_inv_id.clone(),
                predicate_text: "executor must not run when no tasks are ready".to_string(),
                state_conditions: vec![
                    StateCondition {
                        key: "proposed_role".to_string(),
                        value: "executor".to_string(),
                    },
                    StateCondition {
                        key: "ready_tasks".to_string(),
                        value: "0".to_string(),
                    },
                ],
                support_count: 5,
                status: InvariantStatus::Promoted,
                gates: vec!["executor".to_string()],
                evidence: vec![],
                first_seen_ms: 0,
                last_seen_ms: 1,
            }],
        };
        persist_enforced_invariants_projection(
            tmp.as_path(),
            &inv_file,
            "enforced_invariants_save",
        )
        .unwrap();

        let created = generate_invariant_issues(tmp.as_path()).unwrap();

        // 2 meta + 1 per-promoted = 3
        assert_eq!(created, 3);
        let raw = std::fs::read_to_string(&issues_path).unwrap();
        let file: IssuesFile = serde_json::from_str(&raw).unwrap();
        assert!(
            file.issues.iter().any(|i| i.id == expected_issue_id),
            "per-promoted issue not found"
        );
    }

    #[test]
    fn load_enforced_invariants_file_falls_back_to_latest_tlog_snapshot_when_projection_missing() {
        let tmp = make_workspace();
        std::fs::create_dir_all(tmp.join("agent_state")).unwrap();

        let file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![DiscoveredInvariant {
                id: "INV-recovered".to_string(),
                predicate_text: "Recovered invariant snapshot".to_string(),
                state_conditions: vec![StateCondition {
                    key: "phase".to_string(),
                    value: "diagnostics".to_string(),
                }],
                support_count: 4,
                status: InvariantStatus::Promoted,
                gates: vec!["planner".to_string()],
                evidence: vec![],
                first_seen_ms: 1,
                last_seen_ms: 2,
            }],
        };

        persist_enforced_invariants_projection(
            tmp.as_path(),
            &file,
            "invariants_tlog_fallback_test",
        )
        .unwrap();
        std::fs::remove_file(tmp.join(ENFORCED_INVARIANTS_FILE)).unwrap();

        let recovered = load_enforced_invariants_file(tmp.as_path());
        assert_eq!(recovered.version, file.version);
        assert_eq!(recovered.invariants.len(), 1);
        let expected_id = fingerprint_id(&recovered.invariants[0].state_conditions);
        assert_eq!(recovered.invariants[0].id, expected_id);
        assert_eq!(recovered.invariants[0].status, InvariantStatus::Promoted);
    }

    #[test]
    fn load_enforced_invariants_normalizes_ids_gates_and_promotions() {
        let tmp = make_workspace();
        let path = tmp.join(ENFORCED_INVARIANTS_FILE);
        let raw = r#"{
  "version": 0,
  "last_synthesized_ms": 1,
  "invariants": [
    {
      "id": "INV-manual",
      "predicate_text": "executor duplicate",
      "state_conditions": [
        {"key":"ready_tasks","value":"0"},
        {"key":"actor_kind","value":"executor"},
        {"key":"actor_kind","value":"executor"}
      ],
      "support_count": 5,
      "status": "discovered",
      "gates": ["executor", "executor", "invalid_gate"],
      "evidence": [],
      "first_seen_ms": 10,
      "last_seen_ms": 9
    }
  ]
}"#;
        std::fs::write(path, raw).unwrap();

        let loaded = load_enforced_invariants_file(tmp.as_path());
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.invariants.len(), 1);
        let inv = &loaded.invariants[0];
        assert_eq!(inv.status, InvariantStatus::Promoted);
        assert_eq!(inv.gates, vec!["executor".to_string()]);
        assert_eq!(inv.state_conditions.len(), 2);
        assert_eq!(inv.first_seen_ms, 9);
        assert_eq!(inv.last_seen_ms, 10);
        assert_eq!(inv.id, fingerprint_id(&inv.state_conditions));
    }

    #[test]
    fn persist_enforced_invariants_merges_duplicate_condition_sets() {
        let tmp = make_workspace();
        let shared_conditions = vec![
            StateCondition {
                key: "actor_kind".to_string(),
                value: "planner".to_string(),
            },
            StateCondition {
                key: "error_class".to_string(),
                value: "runtime_control_bypass".to_string(),
            },
        ];
        let mut reversed_conditions = shared_conditions.clone();
        reversed_conditions.reverse();

        let file = EnforcedInvariantsFile {
            version: 1,
            last_synthesized_ms: 0,
            invariants: vec![
                DiscoveredInvariant {
                    id: "INV-one".to_string(),
                    predicate_text: "short".to_string(),
                    state_conditions: shared_conditions,
                    support_count: 2,
                    status: InvariantStatus::Discovered,
                    gates: vec!["planner".to_string()],
                    evidence: vec![],
                    first_seen_ms: 1,
                    last_seen_ms: 2,
                },
                DiscoveredInvariant {
                    id: "INV-two".to_string(),
                    predicate_text: "a much longer predicate text".to_string(),
                    state_conditions: reversed_conditions,
                    support_count: 7,
                    status: InvariantStatus::Promoted,
                    gates: vec!["invalid".to_string(), "planner".to_string()],
                    evidence: vec![],
                    first_seen_ms: 3,
                    last_seen_ms: 4,
                },
            ],
        };

        persist_enforced_invariants_projection(tmp.as_path(), &file, "dedupe_test").unwrap();
        let loaded = load_enforced_invariants_file(tmp.as_path());
        assert_eq!(loaded.invariants.len(), 1);
        let inv = &loaded.invariants[0];
        assert_eq!(inv.support_count, 7);
        assert_eq!(inv.status, InvariantStatus::Promoted);
        assert_eq!(inv.gates, vec!["planner".to_string()]);
        assert_eq!(inv.predicate_text, "a much longer predicate text");
    }
}

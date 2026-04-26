/// Repair plan registry — structured, planner-readable action blocks.
///
/// ## Equation
///
///   M (weak metric / invariant state / uncovered blocker)
///   → trigger detected
///   → RepairPlan { goal, action, verify }
///   → planner acts on the top-N plans, executor closes each verify
///
/// ## Plan kinds
///
///   eval_metric   — score below target for a measured eval dimension
///   invariant     — promoted invariant waiting for enforce/collapse decision
///   blocker_class — recurring error class with no enforced invariant
///
/// ## Pipeline
///
///   build_all_active_plans(eval, workspace)
///   → render_active_plans(plans)
///   → appended to EVAL HEADER in prompt_inputs.rs
///   → planner reads REPAIR_PLAN blocks and acts
use std::path::Path;

use serde_json::{Map, Value};

// ── Core type ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RepairPlan {
    pub kind: &'static str,
    pub id: String,
    /// What success looks like — static description of the desired end state.
    pub goal: String,
    /// Why this plan is active right now — derived from live data.
    pub trigger: String,
    pub policy: &'static str,
    /// Exact next action for the planner or executor.
    pub action: String,
    /// Observable condition that closes this plan.
    pub verify: String,
    pub owner: &'static str,
    pub evidence: &'static str,
    /// Lower = higher priority when sorting across plan kinds.
    pub priority: u8,
    /// Raw score (0.0–1.0) for eval_metric plans; 0.0 for other kinds.
    pub score: f64,
}

// ── Rendering ─────────────────────────────────────────────────────────────────

pub fn render_plan(plan: &RepairPlan) -> String {
    format!(
        "REPAIR_PLAN\n\
        kind: {kind}\n\
        id: {id}\n\
        goal: {goal}\n\
        trigger: {trigger}\n\
        policy: {policy}\n\
        action: {action}\n\
        verify: {verify}\n\
        owner: {owner}\n\
        evidence: {evidence}",
        kind = plan.kind,
        id = plan.id,
        goal = plan.goal,
        trigger = plan.trigger,
        policy = plan.policy,
        action = plan.action,
        verify = plan.verify,
        owner = plan.owner,
        evidence = plan.evidence,
    )
}

/// Render a list of plans separated by blank lines.  Returns empty string
/// when plans is empty so callers can append without extra whitespace.
pub fn render_active_plans(plans: &[RepairPlan]) -> String {
    if plans.is_empty() {
        return String::new();
    }
    let blocks: Vec<String> = plans.iter().map(render_plan).collect();
    format!("\n{}\n", blocks.join("\n\n"))
}

// ── Top-level builder ─────────────────────────────────────────────────────────

/// Build all active repair plans from the three registries, merge, sort by
/// priority, and cap at `max_count`.
pub fn build_all_active_plans(
    eval: &Map<String, Value>,
    workspace: &Path,
    max_count: usize,
) -> Vec<RepairPlan> {
    let invariant_text = std::fs::read_to_string(
        workspace.join("agent_state").join("enforced_invariants.json"),
    )
    .unwrap_or_default();
    let blockers_text = std::fs::read_to_string(
        workspace.join("agent_state").join("blockers.json"),
    )
    .unwrap_or_default();

    let mut plans: Vec<RepairPlan> = Vec::new();
    plans.extend(build_invariant_plans(&invariant_text, max_count));
    plans.extend(build_blocker_class_plans(&blockers_text, &invariant_text, max_count));
    plans.extend(build_eval_metric_plans(eval, max_count));

    // Sort by priority ascending then score ascending within same priority.
    plans.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.id.cmp(&b.id))
    });
    plans.truncate(max_count);
    plans
}

// ── Compatibility shim for prompt_inputs ──────────────────────────────────────

/// Render the top-N active repair plans as a block string for the EVAL HEADER.
pub fn render_weak_blocks(eval: &Map<String, Value>, max_count: usize) -> String {
    let plans = build_eval_metric_plans(eval, max_count);
    render_active_plans(&plans)
}

// ── 1. Invariant lifecycle plans ──────────────────────────────────────────────

/// Generate a plan for every invariant in `promoted` status.  Promoted means
/// the system has synthesized the invariant but the planner has not yet decided
/// to enforce or collapse it.
pub fn build_invariant_plans(invariant_text: &str, max: usize) -> Vec<RepairPlan> {
    #[derive(serde::Deserialize)]
    struct InvFile {
        #[serde(default)]
        invariants: Vec<InvEntry>,
    }
    #[derive(serde::Deserialize)]
    struct InvEntry {
        id: String,
        predicate_text: String,
        status: String,
        support_count: u64,
        #[serde(default)]
        gates: Vec<String>,
    }

    let Ok(file) = serde_json::from_str::<InvFile>(invariant_text) else {
        return Vec::new();
    };

    let mut plans: Vec<RepairPlan> = file
        .invariants
        .iter()
        .filter(|inv| inv.status == "promoted")
        .map(|inv| {
            let gates = inv.gates.join(", ");
            RepairPlan {
                kind: "invariant",
                id: inv.id.clone(),
                goal: "invariant lifecycle resolved — predicate enforced or collapsed".to_string(),
                trigger: format!(
                    "{id} is promoted (support={support}, gates=[{gates}]): '{predicate}'",
                    id = inv.id,
                    support = inv.support_count,
                    predicate = truncate(&inv.predicate_text, 80),
                ),
                policy: "invariant_lifecycle",
                action: format!(
                    "evaluate whether predicate is still valid, then: \
                    invariants(op=enforce, id={id}) if valid, \
                    invariants(op=collapse, id={id}) if root cause is gone",
                    id = inv.id,
                ),
                verify: format!(
                    "{id} status=enforced OR status=collapsed in enforced_invariants.json \
                    on next planner cycle",
                    id = inv.id,
                ),
                owner: "planner",
                evidence: "agent_state/enforced_invariants.json",
                priority: 10,
                score: 0.0,
            }
        })
        .collect();

    plans.sort_by(|a, b| a.id.cmp(&b.id));
    plans.truncate(max);
    plans
}

// ── 2. Blocker class plans ────────────────────────────────────────────────────

/// Generate one plan per distinct runtime error class that has no matching
/// entry in enforced_invariants.json.  More specific than the aggregate
/// `blocker_class_coverage` eval metric plan.
pub fn build_blocker_class_plans(
    blockers_text: &str,
    invariant_text: &str,
    max: usize,
) -> Vec<RepairPlan> {
    #[derive(serde::Deserialize)]
    struct BlockersFile {
        #[serde(default)]
        blockers: Vec<BlockerEntry>,
    }
    #[derive(serde::Deserialize)]
    struct BlockerEntry {
        error_class: String,
    }

    let Ok(file) = serde_json::from_str::<BlockersFile>(blockers_text) else {
        return Vec::new();
    };

    // Count occurrences per class key (skip unknown).
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for b in &file.blockers {
        if b.error_class == "unknown" {
            continue;
        }
        *counts.entry(b.error_class.clone()).or_default() += 1;
    }

    let invariant_lower = invariant_text.to_ascii_lowercase();

    let mut plans: Vec<RepairPlan> = counts
        .iter()
        .filter(|(key, _)| !invariant_lower.contains(key.as_str()))
        .map(|(key, count)| RepairPlan {
            kind: "blocker_class",
            id: key.clone(),
            goal: format!(
                "'{key}' error class covered by an enforced invariant \
                so future occurrences are gated and tracked"
            ),
            trigger: format!(
                "{count} recorded occurrences of '{key}' with no matching invariant in \
                enforced_invariants.json"
            ),
            policy: "synthesize_blocker_invariant",
            action: format!(
                "patch src/invariant_discovery.rs — add detection rule for '{key}' \
                that emits a typed invariant when support_count >= 3"
            ),
            verify: format!(
                "enforced_invariants.json contains a '{key}' state_condition entry \
                AND blocker_class_coverage increases on next eval"
            ),
            owner: "executor",
            evidence: "agent_state/blockers.json, agent_state/enforced_invariants.json, src/invariant_discovery.rs",
            priority: 20,
            score: 0.0,
        })
        .collect();

    // Sort highest-count first (most recurring = most urgent).
    plans.sort_by(|a, b| {
        let ca = counts.get(&a.id).copied().unwrap_or(0);
        let cb = counts.get(&b.id).copied().unwrap_or(0);
        cb.cmp(&ca).then(a.id.cmp(&b.id))
    });
    plans.truncate(max);
    plans
}

// ── 3. Eval metric plans ──────────────────────────────────────────────────────

fn status_label(score: f64, target: f64) -> &'static str {
    if score >= target {
        "pass"
    } else if score >= target * 0.70 {
        "weak"
    } else {
        "blocked"
    }
}

/// Generate plans for every eval dimension whose score is below target.
/// Sorted by score ascending (most broken first).
pub fn build_eval_metric_plans(eval: &Map<String, Value>, max: usize) -> Vec<RepairPlan> {
    let get_f64 = |key: &str| eval.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let get_f64_or = |key: &str, d: f64| eval.get(key).and_then(|v| v.as_f64()).unwrap_or(d);
    let get_u64 = |key: &str| eval.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let get_str = |key: &str| {
        eval.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let get_arr_str = |key: &str| {
        eval.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };

    let mut plans: Vec<RepairPlan> = Vec::new();

    macro_rules! plan {
        (
            metric: $metric:expr,
            target: $target:expr,
            score_key: $score_key:expr,
            score_default: $score_default:expr,
            goal: $goal:expr,
            trigger: $trigger:expr,
            policy: $policy:expr,
            action: $action:expr,
            verify: $verify:expr,
            owner: $owner:expr,
            evidence: $evidence:expr,
        ) => {{
            let score: f64 = get_f64_or($score_key, $score_default);
            let target: f64 = $target;
            let status = status_label(score, target);
            if status != "pass" {
                plans.push(RepairPlan {
                    kind: "eval_metric",
                    id: format!("{}(score={:.3}/target={:.3})", $metric, score, target),
                    goal: $goal,
                    trigger: $trigger,
                    policy: $policy,
                    action: $action,
                    verify: $verify,
                    owner: $owner,
                    evidence: $evidence,
                    priority: if status == "blocked" { 30 } else { 50 },
                    score,
                });
            }
        }};
    }

    // ── objective_progress ────────────────────────────────────────────────────
    plan!(
        metric: "objective_progress",
        target: 1.0,
        score_key: "objective_progress",
        score_default: 0.0,
        goal: "all objectives in OBJECTIVES.json marked done".to_string(),
        trigger: format!(
            "objective_progress={:.3}; one or more objectives are not complete",
            get_f64("objective_progress")
        ),
        policy: "close_or_update_objectives",
        action: "use objectives action (op: update_objective) to mark complete objectives done; create objectives for any untracked active gaps".to_string(),
        verify: "objective_progress = 1.0 on next eval".to_string(),
        owner: "planner",
        evidence: "agent_state/OBJECTIVES.json",
    );

    // ── safety ────────────────────────────────────────────────────────────────
    plan!(
        metric: "safety",
        target: 1.0,
        score_key: "safety",
        score_default: 0.0,
        goal: "no active invariant violations; semantic error rate = 0".to_string(),
        trigger: format!(
            "safety={:.3}; check VIOLATIONS.json for active entries or non-zero semantic_fn_error_rate",
            get_f64("safety")
        ),
        policy: "resolve_violations",
        action: "resolve all active violations in agent_state/VIOLATIONS.json before dispatching any other task".to_string(),
        verify: "safety = 1.0 on next eval".to_string(),
        owner: "planner",
        evidence: "agent_state/VIOLATIONS.json",
    );

    // ── task_velocity ─────────────────────────────────────────────────────────
    plan!(
        metric: "task_velocity",
        target: 0.85,
        score_key: "task_velocity",
        score_default: 0.0,
        goal: "at least 85% of PLAN.json tasks complete".to_string(),
        trigger: format!(
            "task_velocity={:.3}; stale or incomplete tasks accumulating",
            get_f64("task_velocity")
        ),
        policy: "close_stale_tasks",
        action: "use plan action to mark completed tasks done; close tasks that will not execute this session".to_string(),
        verify: "task_velocity >= 0.85 on next eval".to_string(),
        owner: "planner",
        evidence: "agent_state/PLAN.json",
    );

    // ── issue_health ──────────────────────────────────────────────────────────
    plan!(
        metric: "issue_health",
        target: 0.9,
        score_key: "issue_health",
        score_default: 0.0,
        goal: "repeated open issues resolved or closed".to_string(),
        trigger: format!(
            "issue_health={:.3}; open issues with repeated occurrences",
            get_f64("issue_health")
        ),
        policy: "fix_or_close_repeated_issues",
        action: "fix or close the top repeated open issues in agent_state/ISSUES.json by score descending".to_string(),
        verify: "issue_health >= 0.9 on next eval".to_string(),
        owner: "executor",
        evidence: "agent_state/ISSUES.json",
    );

    // ── semantic_contract ─────────────────────────────────────────────────────
    {
        let score = get_f64("semantic_contract");
        let error_rate = get_f64("semantic_fn_error_rate");
        let low_conf = get_f64("semantic_fn_low_confidence_rate");
        let intent = get_f64("semantic_fn_intent_coverage");
        plan!(
            metric: "semantic_contract",
            target: 0.50,
            score_key: "semantic_contract",
            score_default: 0.0,
            goal: "semantic_contract >= 0.50: error rate = 0, intent coverage rising".to_string(),
            trigger: format!(
                "fn_error_rate={error_rate:.4}  intent_coverage={intent:.4}  low_confidence_rate={low_conf:.4}"
            ),
            policy: "regenerate_semantic_artifacts",
            action: "run canon-generate-issues --complexity-report-only; reduce fn_with_any_error to zero before treating low-confidence as failure".to_string(),
            verify: format!("semantic_contract >= 0.50 on next eval or fn_error_rate = 0.0"),
            owner: "executor",
            evidence: "agent_state/semantic_manifest_proposals.json, agent_state/reports/complexity/latest.json",
        );
        let _ = score; // used in trigger via captured variable
    }

    // ── structural_invariant_coverage ─────────────────────────────────────────
    {
        let missing = get_arr_str("missing_structural_invariant_kinds");
        plan!(
            metric: "structural_invariant_coverage",
            target: 1.0,
            score_key: "structural_invariant_coverage",
            score_default: 0.0,
            goal: "all known graph structural risks have a matching enforced invariant".to_string(),
            trigger: if missing.is_empty() {
                format!("structural_invariant_coverage={:.3}", get_f64("structural_invariant_coverage"))
            } else {
                format!("missing invariants for: {missing}")
            },
            policy: "synthesize_structural_invariant",
            action: "patch src/invariant_discovery.rs to synthesize the missing structural invariant; do not edit enforced_invariants.json directly".to_string(),
            verify: "structural_invariant_coverage = 1.0 on next eval, missing_structural_invariant_kinds empty".to_string(),
            owner: "executor",
            evidence: "agent_state/enforced_invariants.json, state/rustc/canon_mini_agent/graph.json",
        );
    }

    // ── blocker_class_coverage ────────────────────────────────────────────────
    {
        let top = get_str("blocker_top_uncovered");
        let distinct = get_u64("blocker_distinct_classes");
        let covered = get_u64("blocker_covered_classes");
        let score = get_f64_or("blocker_class_coverage", 1.0);
        plan!(
            metric: "blocker_class_coverage",
            target: 1.0,
            score_key: "blocker_class_coverage",
            score_default: 1.0,
            goal: "every distinct runtime error class covered by an enforced invariant".to_string(),
            trigger: format!(
                "{covered}/{distinct} classes covered; top uncovered: {}",
                if top.is_empty() { "none".to_string() } else { top.clone() }
            ),
            policy: "synthesize_blocker_invariant",
            action: if top.is_empty() {
                "no uncovered blocker classes; verify blockers.json is populated".to_string()
            } else {
                format!(
                    "patch src/invariant_discovery.rs — add detection rule for '{top}' \
                    emitting a typed invariant when support_count >= 3"
                )
            },
            verify: if top.is_empty() {
                "blocker_class_coverage = 1.0 on next eval".to_string()
            } else {
                format!("next eval blocker_class_coverage > {score:.3} AND top_uncovered != {top}")
            },
            owner: "executor",
            evidence: "agent_state/blockers.json, agent_state/enforced_invariants.json",
        );
    }

    // ── canonical_delta_health ────────────────────────────────────────────────
    {
        let truncations = get_u64("tlog_prompt_truncation_count");
        let lag_ms = get_u64("tlog_actionable_lag_total_ms");
        let payload = get_str("tlog_dominant_payload_kind");
        plan!(
            metric: "canonical_delta_health",
            target: 0.9,
            score_key: "canonical_delta_health",
            score_default: 0.0,
            goal: "prompt truncations eliminated; actionable lag below 300 s".to_string(),
            trigger: format!(
                "prompt_truncations={truncations}  actionable_lag_ms={lag_ms}  dominant_payload={payload}"
            ),
            policy: "reduce_prompt_pressure",
            action: format!(
                "reduce the dominant payload kind '{payload}' in the prompt; \
                increase eval run frequency to reduce actionable lag"
            ),
            verify: "canonical_delta_health >= 0.9 on next eval, prompt_truncations decreasing".to_string(),
            owner: "planner",
            evidence: "agent_state/tlog.ndjson tlog_dominant_payload_kind, tlog_prompt_truncation_count",
        );
    }

    // ── improvement_measurement ───────────────────────────────────────────────
    {
        let unmeasured = get_u64("unmeasured_improvement_attempts");
        let attempts = get_u64("improvement_attempts");
        plan!(
            metric: "improvement_measurement",
            target: 1.0,
            score_key: "improvement_measurement",
            score_default: 1.0,
            goal: "every apply_patch improvement has a measured eval delta in tlog".to_string(),
            trigger: format!(
                "{unmeasured}/{attempts} improvement attempts have no follow-up eval score"
            ),
            policy: "run_eval_after_patch",
            action: "after every apply_patch, run canon-generate-issues --complexity-report-only before marking the executor task done".to_string(),
            verify: "improvement_measurement = 1.0 on next eval, unmeasured_improvement_attempts = 0".to_string(),
            owner: "executor",
            evidence: "agent_state/tlog.ndjson unmeasured_improvement_attempts in eval_score_recorded",
        );
    }

    // ── improvement_validation ────────────────────────────────────────────────
    {
        let unvalidated = get_u64("unvalidated_improvement_attempts");
        let attempts = get_u64("improvement_attempts");
        plan!(
            metric: "improvement_validation",
            target: 1.0,
            score_key: "improvement_validation",
            score_default: 1.0,
            goal: "every improvement attempt has cargo check/test verification in tlog".to_string(),
            trigger: format!(
                "{unvalidated}/{attempts} improvements have no cargo verification result in tlog"
            ),
            policy: "verify_after_patch",
            action: "run cargo check -p canon-mini-agent immediately after every apply_patch before completing the executor turn".to_string(),
            verify: "improvement_validation = 1.0 on next eval, unvalidated_improvement_attempts = 0".to_string(),
            owner: "executor",
            evidence: "agent_state/tlog.ndjson unvalidated_improvement_attempts in eval_score_recorded",
        );
    }

    // ── improvement_effectiveness ─────────────────────────────────────────────
    {
        let regressed = get_u64("regressed_improvement_attempts");
        let measured = get_u64("measured_improvement_attempts");
        plan!(
            metric: "improvement_effectiveness",
            target: 0.8,
            score_key: "improvement_effectiveness",
            score_default: 1.0,
            goal: "at least 80% of measured improvements raise the eval score".to_string(),
            trigger: format!(
                "{regressed}/{measured} measured improvements caused an eval score regression"
            ),
            policy: "revert_regressing_patches",
            action: "identify regressing improvements in tlog (delta_g < 0 after apply_patch) and revert or narrow their patch scope".to_string(),
            verify: "improvement_effectiveness >= 0.8 on next eval, regressed_improvement_attempts does not increase".to_string(),
            owner: "executor",
            evidence: "agent_state/tlog.ndjson delta_g in eval_score_recorded following apply_patch action_result_recorded",
        );
    }

    // ── recovery_effectiveness ────────────────────────────────────────────────
    {
        let failures = get_u64("recovery_failures");
        let attempts = get_u64("recovery_attempts");
        plan!(
            metric: "recovery_effectiveness",
            target: 1.0,
            score_key: "recovery_effectiveness",
            score_default: 1.0,
            goal: "all typed recovery attempts resolve the blocker class".to_string(),
            trigger: format!(
                "recovery_failures={failures}/{attempts}; some recovery policies not resolving the blocker"
            ),
            policy: "inspect_failed_recovery",
            action: "inspect recovery_outcome_recorded events in tlog where success=false; patch the failing recovery policy in src/recovery.rs".to_string(),
            verify: "recovery_effectiveness = 1.0 on next eval, recovery_failures = 0".to_string(),
            owner: "executor",
            evidence: "agent_state/tlog.ndjson recovery_outcome_recorded events with success=false",
        );
    }

    plans.sort_by(|a, b| {
        a.priority.cmp(&b.priority).then(
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    plans.truncate(max);
    plans
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all_weak_eval() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("objective_progress".into(), json!(0.5));
        m.insert("safety".into(), json!(0.85));
        m.insert("task_velocity".into(), json!(0.5));
        m.insert("issue_health".into(), json!(0.5));
        m.insert("semantic_contract".into(), json!(0.28));
        m.insert("semantic_fn_error_rate".into(), json!(0.0));
        m.insert("semantic_fn_low_confidence_rate".into(), json!(0.46));
        m.insert("semantic_fn_intent_coverage".into(), json!(0.54));
        m.insert("structural_invariant_coverage".into(), json!(0.8));
        m.insert("missing_structural_invariant_kinds".into(), json!(["issues_projection_only"]));
        m.insert("blocker_class_coverage".into(), json!(0.33));
        m.insert("blocker_distinct_classes".into(), json!(3u64));
        m.insert("blocker_covered_classes".into(), json!(1u64));
        m.insert("blocker_top_uncovered".into(), json!("llm_timeout"));
        m.insert("canonical_delta_health".into(), json!(0.8));
        m.insert("tlog_prompt_truncation_count".into(), json!(39u64));
        m.insert("tlog_actionable_lag_total_ms".into(), json!(1_200_000u64));
        m.insert("tlog_dominant_payload_kind".into(), json!("enforced_invariants_recorded"));
        m.insert("improvement_measurement".into(), json!(0.83));
        m.insert("unmeasured_improvement_attempts".into(), json!(1u64));
        m.insert("improvement_attempts".into(), json!(6u64));
        m.insert("improvement_validation".into(), json!(0.9));
        m.insert("unvalidated_improvement_attempts".into(), json!(1u64));
        m.insert("improvement_effectiveness".into(), json!(0.67));
        m.insert("regressed_improvement_attempts".into(), json!(2u64));
        m.insert("measured_improvement_attempts".into(), json!(6u64));
        m.insert("recovery_effectiveness".into(), json!(0.8));
        m.insert("recovery_failures".into(), json!(1u64));
        m.insert("recovery_attempts".into(), json!(5u64));
        m
    }

    #[test]
    fn every_plan_has_required_fields() {
        let eval = all_weak_eval();
        let plans = build_eval_metric_plans(&eval, 12);
        assert!(!plans.is_empty());
        for plan in &plans {
            assert!(!plan.action.is_empty(), "{} has empty action", plan.id);
            assert!(!plan.verify.is_empty(), "{} has empty verify", plan.id);
            assert!(!plan.goal.is_empty(), "{} has empty goal", plan.id);
            assert!(!plan.trigger.is_empty(), "{} has empty trigger", plan.id);
        }
    }

    #[test]
    fn pass_scores_emit_no_plans() {
        let mut m = Map::new();
        for key in &[
            "objective_progress", "safety", "task_velocity", "issue_health",
            "semantic_contract", "structural_invariant_coverage", "canonical_delta_health",
            "improvement_measurement", "improvement_validation",
            "improvement_effectiveness", "recovery_effectiveness",
        ] {
            m.insert((*key).to_string(), json!(1.0));
        }
        m.insert("blocker_class_coverage".into(), json!(1.0));
        m.insert("blocker_distinct_classes".into(), json!(0u64));
        m.insert("blocker_covered_classes".into(), json!(0u64));
        m.insert("blocker_top_uncovered".into(), json!(""));
        let plans = build_eval_metric_plans(&m, 12);
        assert!(plans.is_empty(), "got plans for passing metrics: {:?}",
            plans.iter().map(|p| &p.id).collect::<Vec<_>>());
    }

    #[test]
    fn plans_sorted_by_priority_then_score_ascending() {
        let eval = all_weak_eval();
        let plans = build_eval_metric_plans(&eval, 12);
        for window in plans.windows(2) {
            let a = &window[0];
            let b = &window[1];
            // Primary: priority ascending
            assert!(
                a.priority <= b.priority,
                "priority not sorted: {} (priority={}) before {} (priority={})",
                a.id, a.priority, b.id, b.priority
            );
            // Secondary: within same priority, score ascending
            if a.priority == b.priority {
                assert!(
                    a.score <= b.score + 0.001,
                    "score not sorted within priority {}: {} ({:.3}) before {} ({:.3})",
                    a.priority, a.id, a.score, b.id, b.score
                );
            }
        }
    }

    #[test]
    fn max_count_capped() {
        let eval = all_weak_eval();
        let plans = build_eval_metric_plans(&eval, 3);
        assert!(plans.len() <= 3);
    }

    #[test]
    fn blocker_plan_names_uncovered_class_in_action_and_verify() {
        let eval = all_weak_eval();
        let plans = build_eval_metric_plans(&eval, 12);
        let bp = plans.iter().find(|p| p.id.starts_with("blocker_class_coverage")).unwrap();
        assert!(bp.action.contains("llm_timeout"), "action: {}", bp.action);
        assert!(bp.verify.contains("llm_timeout"), "verify: {}", bp.verify);
    }

    #[test]
    fn invariant_plan_generated_for_promoted_status() {
        let inv_json = r#"{
            "version": 1,
            "invariants": [
                {
                    "id": "INV-test123",
                    "predicate_text": "some repeated failure condition",
                    "status": "promoted",
                    "support_count": 12,
                    "gates": ["executor"]
                },
                {
                    "id": "INV-other",
                    "predicate_text": "already enforced",
                    "status": "enforced",
                    "support_count": 5,
                    "gates": ["route"]
                }
            ]
        }"#;
        let plans = build_invariant_plans(inv_json, 10);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "INV-test123");
        assert!(plans[0].action.contains("INV-test123"));
        assert!(plans[0].verify.contains("INV-test123"));
        assert_eq!(plans[0].kind, "invariant");
    }

    #[test]
    fn blocker_class_plan_generated_for_uncovered_class() {
        let blockers = r#"{"version":1,"blockers":[
            {"id":"b1","error_class":"llm_timeout","actor":"planner","summary":"t","action_kind":"llm_request","source":"action_result","ts_ms":1},
            {"id":"b2","error_class":"llm_timeout","actor":"planner","summary":"t","action_kind":"llm_request","source":"action_result","ts_ms":2}
        ]}"#;
        // invariant text does NOT contain "llm_timeout"
        let invariants = r#"{"invariants":[{"predicate_text":"missing_target only","status":"enforced","gates":["route"]}]}"#;
        let plans = build_blocker_class_plans(blockers, invariants, 10);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "llm_timeout");
        assert!(plans[0].action.contains("llm_timeout"));
        assert!(plans[0].verify.contains("llm_timeout"));
        assert_eq!(plans[0].kind, "blocker_class");
    }

    #[test]
    fn rendered_plan_contains_all_block_fields() {
        let eval = all_weak_eval();
        let plans = build_eval_metric_plans(&eval, 1);
        let rendered = render_plan(&plans[0]);
        for field in &[
            "REPAIR_PLAN", "kind:", "id:", "goal:", "trigger:",
            "policy:", "action:", "verify:", "owner:", "evidence:",
        ] {
            assert!(rendered.contains(field), "missing field: {field}\n{rendered}");
        }
    }
}

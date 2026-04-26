/// Instruction registry: maps every eval metric to a structured recovery block.
///
/// ## Pipeline
///
///   evaluation.rs computes scores
///   → build_weak_instructions selects weak/blocked dims
///   → render_instruction produces EVAL_METRIC_INSTRUCTION blocks
///   → prompt_inputs.rs appends them to the EVAL HEADER
///   → planner reads metric + next_action + success_condition and acts
///
/// ## Rule
///
///   Every emitted instruction must have a non-empty next_action and
///   success_condition so the planner can close the loop without
///   additional read_file round-trips.
use serde_json::{Map, Value};

// ── Output type ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MetricInstruction {
    pub metric: &'static str,
    pub score: f64,
    pub target: f64,
    pub status: &'static str,
    pub meaning: &'static str,
    pub likely_cause: String,
    pub recovery_policy: &'static str,
    pub next_action: String,
    pub success_condition: String,
    pub evidence: &'static str,
}

// ── Rendering ─────────────────────────────────────────────────────────────────

pub fn render_instruction(inst: &MetricInstruction) -> String {
    format!(
        "EVAL_METRIC_INSTRUCTION\n\
        metric: {metric}\n\
        score: {score:.3}/{target:.3}\n\
        status: {status}\n\
        meaning: {meaning}\n\
        likely_cause: {likely_cause}\n\
        recovery_policy: {recovery_policy}\n\
        next_action: {next_action}\n\
        success_condition: {success_condition}\n\
        evidence: {evidence}",
        metric = inst.metric,
        score = inst.score,
        target = inst.target,
        status = inst.status,
        meaning = inst.meaning,
        likely_cause = inst.likely_cause,
        recovery_policy = inst.recovery_policy,
        next_action = inst.next_action,
        success_condition = inst.success_condition,
        evidence = inst.evidence,
    )
}

pub fn render_weak_blocks(eval: &Map<String, Value>, max_count: usize) -> String {
    let instructions = build_weak_instructions(eval, max_count);
    if instructions.is_empty() {
        return String::new();
    }
    let blocks: Vec<String> = instructions.iter().map(render_instruction).collect();
    format!("\n{}\n", blocks.join("\n\n"))
}

// ── Status ────────────────────────────────────────────────────────────────────

fn status_for_score(score: f64, target: f64) -> &'static str {
    if score >= target {
        "pass"
    } else if score >= target * 0.70 {
        "weak"
    } else {
        "blocked"
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// Build instructions for every weak or blocked metric, sorted by score
/// ascending (most broken first), capped at `max_count`.
pub fn build_weak_instructions(eval: &Map<String, Value>, max_count: usize) -> Vec<MetricInstruction> {
    let get_f64 = |key: &str| eval.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let get_f64_or = |key: &str, default: f64| eval.get(key).and_then(|v| v.as_f64()).unwrap_or(default);
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

    let mut candidates: Vec<MetricInstruction> = Vec::new();

    macro_rules! push_if_weak {
        ($inst:expr) => {
            let inst: MetricInstruction = $inst;
            if inst.status != "pass" {
                candidates.push(inst);
            }
        };
    }

    // ── 1. objective_progress ─────────────────────────────────────────────────
    {
        let score = get_f64("objective_progress");
        let target = 1.0_f64;
        push_if_weak!(MetricInstruction {
            metric: "objective_progress",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of OBJECTIVES.json objectives that are complete",
            likely_cause: format!(
                "objective_progress={score:.3}; one or more objectives are not marked done"
            ),
            recovery_policy: "close_or_update_objectives",
            next_action: "use objectives action (op: update_objective) to mark complete objectives done; create objectives for any untracked active gaps".to_string(),
            success_condition: "objective_progress = 1.0 on next eval".to_string(),
            evidence: "agent_state/OBJECTIVES.json",
        });
    }

    // ── 2. safety ─────────────────────────────────────────────────────────────
    {
        let score = get_f64("safety");
        let target = 1.0_f64;
        push_if_weak!(MetricInstruction {
            metric: "safety",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "absence of active invariant violations; penalized by semantic error rate",
            likely_cause: format!(
                "safety={score:.3}; active violations in VIOLATIONS.json or non-zero semantic_fn_error_rate"
            ),
            recovery_policy: "resolve_violations",
            next_action: "resolve all active violations in agent_state/VIOLATIONS.json before dispatching any other task".to_string(),
            success_condition: "safety = 1.0 on next eval".to_string(),
            evidence: "agent_state/VIOLATIONS.json",
        });
    }

    // ── 3. task_velocity ──────────────────────────────────────────────────────
    {
        let score = get_f64("task_velocity");
        let target = 0.85_f64;
        push_if_weak!(MetricInstruction {
            metric: "task_velocity",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of PLAN.json tasks that are complete",
            likely_cause: format!(
                "task_velocity={score:.3}; stale or incomplete tasks are accumulating"
            ),
            recovery_policy: "close_stale_tasks",
            next_action: "use plan action to mark completed tasks done and close tasks that will not be executed this session".to_string(),
            success_condition: format!("task_velocity >= {target:.2} on next eval"),
            evidence: "agent_state/PLAN.json",
        });
    }

    // ── 4. issue_health ───────────────────────────────────────────────────────
    {
        let score = get_f64("issue_health");
        let target = 0.9_f64;
        push_if_weak!(MetricInstruction {
            metric: "issue_health",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of issues resolved vs. repeatedly open",
            likely_cause: format!(
                "issue_health={score:.3}; repeated open issues without resolution"
            ),
            recovery_policy: "fix_or_close_repeated_issues",
            next_action: "fix or close the top repeated open issues in agent_state/ISSUES.json ranked by score descending".to_string(),
            success_condition: format!("issue_health >= {target:.2} on next eval"),
            evidence: "agent_state/ISSUES.json",
        });
    }

    // ── 5. semantic_contract ──────────────────────────────────────────────────
    {
        let score = get_f64("semantic_contract");
        let target = 0.50_f64;
        let error_rate = get_f64("semantic_fn_error_rate");
        let low_conf_rate = get_f64("semantic_fn_low_confidence_rate");
        let intent_coverage = get_f64("semantic_fn_intent_coverage");
        push_if_weak!(MetricInstruction {
            metric: "semantic_contract",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "product of semantic error absence, intent coverage, and low-confidence absence",
            likely_cause: format!(
                "fn_error_rate={error_rate:.4}  intent_coverage={intent_coverage:.4}  low_confidence_rate={low_conf_rate:.4}"
            ),
            recovery_policy: "regenerate_semantic_artifacts",
            next_action: "run canon-generate-issues --complexity-report-only to regenerate the semantic manifest; reduce fn_with_any_error to zero before treating low-confidence as failure".to_string(),
            success_condition: format!(
                "semantic_contract >= {target:.2} on next eval OR semantic_fn_error_rate = 0.0"
            ),
            evidence: "agent_state/semantic_manifest_proposals.json, agent_state/reports/complexity/latest.json",
        });
    }

    // ── 6. structural_invariant_coverage ──────────────────────────────────────
    {
        let score = get_f64("structural_invariant_coverage");
        let target = 1.0_f64;
        let missing = get_arr_str("missing_structural_invariant_kinds");
        push_if_weak!(MetricInstruction {
            metric: "structural_invariant_coverage",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of known graph structural risks that have a matching enforced invariant",
            likely_cause: if missing.is_empty() {
                format!("structural_invariant_coverage={score:.3}")
            } else {
                format!("missing invariants for: {missing}")
            },
            recovery_policy: "synthesize_structural_invariant",
            next_action: "patch src/invariant_discovery.rs to synthesize the missing structural invariant; do not edit enforced_invariants.json directly".to_string(),
            success_condition: "structural_invariant_coverage = 1.0 on next eval, missing_structural_invariant_kinds is empty".to_string(),
            evidence: "agent_state/enforced_invariants.json, state/rustc/canon_mini_agent/graph.json",
        });
    }

    // ── 7. blocker_class_coverage ─────────────────────────────────────────────
    {
        let score = get_f64_or("blocker_class_coverage", 1.0);
        let target = 1.0_f64;
        let top_uncovered = get_str("blocker_top_uncovered");
        let distinct = get_u64("blocker_distinct_classes");
        let covered = get_u64("blocker_covered_classes");
        push_if_weak!(MetricInstruction {
            metric: "blocker_class_coverage",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of distinct runtime error classes covered by an enforced invariant",
            likely_cause: format!(
                "{covered}/{distinct} classes covered; top uncovered: {}",
                if top_uncovered.is_empty() {
                    "none".to_string()
                } else {
                    top_uncovered.clone()
                }
            ),
            recovery_policy: "synthesize_blocker_invariant",
            next_action: if top_uncovered.is_empty() {
                "no uncovered blocker classes; verify blockers.json is populated".to_string()
            } else {
                format!(
                    "patch src/invariant_discovery.rs — add detection rule for '{top_uncovered}' \
                    and emit a typed invariant when support_count >= 3"
                )
            },
            success_condition: if top_uncovered.is_empty() {
                "blocker_class_coverage = 1.0 on next eval".to_string()
            } else {
                format!(
                    "next eval blocker_class_coverage > {score:.3} AND top_uncovered != {top_uncovered}"
                )
            },
            evidence: "agent_state/blockers.json, agent_state/enforced_invariants.json",
        });
    }

    // ── 8. canonical_delta_health ─────────────────────────────────────────────
    {
        let score = get_f64("canonical_delta_health");
        let target = 0.9_f64;
        let truncations = get_u64("tlog_prompt_truncation_count");
        let lag_ms = get_u64("tlog_actionable_lag_total_ms");
        let payload_kind = get_str("tlog_dominant_payload_kind");
        push_if_weak!(MetricInstruction {
            metric: "canonical_delta_health",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "health of tlog delta signals: actionable lag, prompt truncation, missing action results",
            likely_cause: format!(
                "prompt_truncations={truncations}  actionable_lag_ms={lag_ms}  dominant_payload={payload_kind}"
            ),
            recovery_policy: "reduce_prompt_pressure",
            next_action: format!(
                "reduce the dominant payload kind '{payload_kind}' in the prompt; \
                run eval more frequently to reduce actionable lag"
            ),
            success_condition: format!(
                "canonical_delta_health >= {target:.2} on next eval, prompt_truncations decreasing"
            ),
            evidence: "agent_state/tlog.ndjson tlog_dominant_payload_kind, tlog_prompt_truncation_count",
        });
    }

    // ── 9. improvement_measurement ────────────────────────────────────────────
    {
        let score = get_f64_or("improvement_measurement", 1.0);
        let target = 1.0_f64;
        let unmeasured = get_u64("unmeasured_improvement_attempts");
        let attempts = get_u64("improvement_attempts");
        push_if_weak!(MetricInstruction {
            metric: "improvement_measurement",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of apply_patch improvement attempts that have a measured eval delta in tlog",
            likely_cause: format!(
                "{unmeasured}/{attempts} improvement attempts have no follow-up eval score"
            ),
            recovery_policy: "run_eval_after_patch",
            next_action: "after every apply_patch, run canon-generate-issues --complexity-report-only before marking the executor task done".to_string(),
            success_condition: "improvement_measurement = 1.0 on next eval, unmeasured_improvement_attempts = 0".to_string(),
            evidence: "agent_state/tlog.ndjson unmeasured_improvement_attempts in eval_score_recorded",
        });
    }

    // ── 10. improvement_validation ────────────────────────────────────────────
    {
        let score = get_f64_or("improvement_validation", 1.0);
        let target = 1.0_f64;
        let unvalidated = get_u64("unvalidated_improvement_attempts");
        let attempts = get_u64("improvement_attempts");
        push_if_weak!(MetricInstruction {
            metric: "improvement_validation",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of improvement attempts that have cargo check/test verification recorded in tlog",
            likely_cause: format!(
                "{unvalidated}/{attempts} improvements have no cargo check/test result in tlog"
            ),
            recovery_policy: "verify_after_patch",
            next_action: "run cargo check -p canon-mini-agent immediately after every apply_patch before completing the executor turn".to_string(),
            success_condition: "improvement_validation = 1.0 on next eval, unvalidated_improvement_attempts = 0".to_string(),
            evidence: "agent_state/tlog.ndjson unvalidated_improvement_attempts in eval_score_recorded",
        });
    }

    // ── 11. improvement_effectiveness ─────────────────────────────────────────
    {
        let score = get_f64_or("improvement_effectiveness", 1.0);
        let target = 0.8_f64;
        let regressed = get_u64("regressed_improvement_attempts");
        let measured = get_u64("measured_improvement_attempts");
        push_if_weak!(MetricInstruction {
            metric: "improvement_effectiveness",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of measured improvements that did not regress the eval score",
            likely_cause: format!(
                "{regressed}/{measured} measured improvements caused an eval score regression"
            ),
            recovery_policy: "revert_regressing_patches",
            next_action: "identify regressing improvements in tlog (delta_g < 0 after apply_patch) and revert or narrow their patch scope".to_string(),
            success_condition: format!(
                "improvement_effectiveness >= {target:.2} on next eval, regressed_improvement_attempts does not increase"
            ),
            evidence: "agent_state/tlog.ndjson delta_g in eval_score_recorded following apply_patch action_result_recorded",
        });
    }

    // ── 12. recovery_effectiveness ────────────────────────────────────────────
    {
        let score = get_f64_or("recovery_effectiveness", 1.0);
        let target = 1.0_f64;
        let failures = get_u64("recovery_failures");
        let attempts = get_u64("recovery_attempts");
        push_if_weak!(MetricInstruction {
            metric: "recovery_effectiveness",
            score,
            target,
            status: status_for_score(score, target),
            meaning: "fraction of typed recovery attempts that successfully resolved the blocker class",
            likely_cause: format!(
                "recovery_failures={failures}/{attempts}; some recovery policies are not resolving the blocker"
            ),
            recovery_policy: "inspect_failed_recovery",
            next_action: "inspect recovery_outcome_recorded events in tlog where success=false and patch the failing recovery policy in src/recovery.rs".to_string(),
            success_condition: "recovery_effectiveness = 1.0 on next eval, recovery_failures = 0".to_string(),
            evidence: "agent_state/tlog.ndjson recovery_outcome_recorded events with success=false",
        });
    }

    // Sort by score ascending (most broken first), cap at max_count.
    candidates.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(max_count);
    candidates
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eval_all_weak() -> Map<String, Value> {
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
        m.insert("improvement_validation".into(), json!(1.0));
        m.insert("unvalidated_improvement_attempts".into(), json!(0u64));
        m.insert("improvement_effectiveness".into(), json!(0.67));
        m.insert("regressed_improvement_attempts".into(), json!(2u64));
        m.insert("measured_improvement_attempts".into(), json!(6u64));
        m.insert("recovery_effectiveness".into(), json!(1.0));
        m.insert("recovery_failures".into(), json!(0u64));
        m.insert("recovery_attempts".into(), json!(4u64));
        m
    }

    #[test]
    fn every_weak_instruction_has_next_action_and_success_condition() {
        let eval = eval_all_weak();
        let instructions = build_weak_instructions(&eval, 12);
        assert!(!instructions.is_empty(), "expected at least one weak metric");
        for inst in &instructions {
            assert!(
                !inst.next_action.is_empty(),
                "metric '{}' has empty next_action",
                inst.metric
            );
            assert!(
                !inst.success_condition.is_empty(),
                "metric '{}' has empty success_condition",
                inst.metric
            );
            assert_ne!(
                inst.status, "pass",
                "metric '{}' emitted as pass but should be weak/blocked",
                inst.metric
            );
        }
    }

    #[test]
    fn pass_metrics_are_not_emitted() {
        let mut eval = Map::new();
        eval.insert("objective_progress".into(), json!(1.0));
        eval.insert("safety".into(), json!(1.0));
        eval.insert("task_velocity".into(), json!(1.0));
        eval.insert("issue_health".into(), json!(1.0));
        eval.insert("semantic_contract".into(), json!(1.0));
        eval.insert("structural_invariant_coverage".into(), json!(1.0));
        eval.insert("blocker_class_coverage".into(), json!(1.0));
        eval.insert("blocker_distinct_classes".into(), json!(0u64));
        eval.insert("blocker_covered_classes".into(), json!(0u64));
        eval.insert("blocker_top_uncovered".into(), json!(""));
        eval.insert("canonical_delta_health".into(), json!(1.0));
        eval.insert("improvement_measurement".into(), json!(1.0));
        eval.insert("improvement_validation".into(), json!(1.0));
        eval.insert("improvement_effectiveness".into(), json!(1.0));
        eval.insert("recovery_effectiveness".into(), json!(1.0));
        let instructions = build_weak_instructions(&eval, 12);
        assert!(
            instructions.is_empty(),
            "expected no instructions when all metrics pass, got: {:?}",
            instructions.iter().map(|i| i.metric).collect::<Vec<_>>()
        );
    }

    #[test]
    fn blocker_instruction_references_top_uncovered_in_next_action() {
        let mut eval = Map::new();
        eval.insert("blocker_class_coverage".into(), json!(0.33));
        eval.insert("blocker_distinct_classes".into(), json!(3u64));
        eval.insert("blocker_covered_classes".into(), json!(1u64));
        eval.insert("blocker_top_uncovered".into(), json!("llm_timeout"));
        let instructions = build_weak_instructions(&eval, 12);
        let blocker_inst = instructions
            .iter()
            .find(|i| i.metric == "blocker_class_coverage")
            .expect("blocker_class_coverage should be weak");
        assert!(
            blocker_inst.next_action.contains("llm_timeout"),
            "next_action should reference the top uncovered class"
        );
        assert!(
            blocker_inst.success_condition.contains("llm_timeout"),
            "success_condition should reference the top uncovered class"
        );
    }

    #[test]
    fn max_count_is_respected() {
        let eval = eval_all_weak();
        let instructions = build_weak_instructions(&eval, 3);
        assert!(instructions.len() <= 3);
    }

    #[test]
    fn instructions_ordered_by_score_ascending() {
        let eval = eval_all_weak();
        let instructions = build_weak_instructions(&eval, 12);
        for window in instructions.windows(2) {
            assert!(
                window[0].score <= window[1].score,
                "instructions not sorted: {} ({}) before {} ({})",
                window[0].metric,
                window[0].score,
                window[1].metric,
                window[1].score
            );
        }
    }

    #[test]
    fn rendered_block_contains_all_required_fields() {
        let eval = eval_all_weak();
        let instructions = build_weak_instructions(&eval, 1);
        let rendered = render_instruction(&instructions[0]);
        for field in &[
            "EVAL_METRIC_INSTRUCTION",
            "metric:",
            "score:",
            "status:",
            "meaning:",
            "likely_cause:",
            "recovery_policy:",
            "next_action:",
            "success_condition:",
            "evidence:",
        ] {
            assert!(
                rendered.contains(field),
                "rendered block missing field: {field}"
            );
        }
    }
}

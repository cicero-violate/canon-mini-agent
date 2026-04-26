use anyhow::Result;
use std::path::Path;

use crate::canonical_writer::CanonicalWriter;
use crate::evaluation::{compute_delta, EvalDelta, EvaluationWorkspaceSnapshot};
use crate::events::EffectEvent;

struct PriorScores {
    overall_score: f64,
    semantic_contract: f64,
    safety: f64,
}

/// Load workspace state, compute eval snapshot, emit `EvalScoreRecorded` into tlog,
/// and return the snapshot plus an optional delta from the previous cycle.
///
/// This is the only function that performs I/O for evaluation.  The scoring
/// logic lives in `evaluation::compute_eval` (pure functions).
pub fn run(
    workspace: &Path,
    writer: Option<&mut CanonicalWriter>,
) -> Result<(EvaluationWorkspaceSnapshot, Option<EvalDelta>)> {
    let snapshot = crate::evaluation::evaluate_workspace(workspace);

    let prior = crate::tlog::Tlog::latest_effect_from_workspace(workspace, |effect| match effect {
        EffectEvent::EvalScoreRecorded {
            overall_score,
            semantic_contract,
            safety,
            ..
        } => Some(PriorScores {
            overall_score,
            semantic_contract,
            safety,
        }),
        _ => None,
    });

    let delta = prior.as_ref().map(|prev| {
        compute_delta(
            &snapshot,
            prev.overall_score,
            prev.semantic_contract,
            prev.safety,
        )
    });

    let effect = EffectEvent::EvalScoreRecorded {
        generated_at_ms: crate::logging::now_ms(),
        overall_score: snapshot.overall_score(),
        delta_g: delta.as_ref().map(|d| d.delta_g),
        promotion_eligible: delta
            .as_ref()
            .map(|d| d.promotion_eligible)
            .unwrap_or(false),
        objective_progress: snapshot.vector.objective_progress,
        safety: snapshot.vector.safety,
        task_velocity: snapshot.vector.task_velocity,
        issue_health: snapshot.vector.issue_health,
        semantic_contract: snapshot.vector.semantic_contract,
        structural_invariant_coverage: snapshot.vector.structural_invariant_coverage,
        canonical_delta_health: snapshot.vector.canonical_delta_health,
        improvement_measurement: snapshot.vector.improvement_measurement,
        improvement_validation: snapshot.vector.improvement_validation,
        improvement_effectiveness: snapshot.vector.improvement_effectiveness,
        recovery_effectiveness: snapshot.vector.recovery_effectiveness,
        improvement_attempts: snapshot.tlog_delta_signals.improvement_attempts,
        measured_improvement_attempts: snapshot
            .tlog_delta_signals
            .measured_improvement_attempts,
        unmeasured_improvement_attempts: snapshot
            .tlog_delta_signals
            .unmeasured_improvement_attempts,
        validated_improvement_attempts: snapshot
            .tlog_delta_signals
            .validated_improvement_attempts,
        unvalidated_improvement_attempts: snapshot
            .tlog_delta_signals
            .unvalidated_improvement_attempts,
        non_regressed_improvement_attempts: snapshot
            .tlog_delta_signals
            .non_regressed_improvement_attempts,
        regressed_improvement_attempts: snapshot
            .tlog_delta_signals
            .regressed_improvement_attempts,
        eval_measurement_points: snapshot.tlog_delta_signals.eval_measurement_points,
        measurement_regressions: snapshot.tlog_delta_signals.measurement_regressions,
        recovery_attempts: snapshot.tlog_delta_signals.recovery_attempts,
        recovery_successes: snapshot.tlog_delta_signals.recovery_successes,
        recovery_failures: snapshot.tlog_delta_signals.recovery_failures,
        recovery_suppressed: snapshot.tlog_delta_signals.recovery_suppressed,
        recovery_loop_breaks: snapshot.tlog_delta_signals.recovery_loop_breaks,
        recovery_regressions: snapshot.tlog_delta_signals.recovery_regressions,
        recovery_measurement_points: snapshot.tlog_delta_signals.recovery_measurement_points,
        tlog_lag_total_ms: snapshot.tlog_delta_signals.lag_total_ms,
        tlog_actionable_lag_total_ms: snapshot.tlog_delta_signals.actionable_lag_total_ms,
        tlog_dominant_actionable_lag_kind: snapshot
            .tlog_delta_signals
            .dominant_actionable_lag_kind
            .clone(),
        tlog_dominant_actionable_lag_kind_ms: snapshot
            .tlog_delta_signals
            .dominant_actionable_lag_kind_ms,
        issues_projection_lag_ms: snapshot.tlog_delta_signals.issues_projection_lag_ms,
        tlog_dominant_payload_kind: snapshot.tlog_delta_signals.dominant_payload_kind.clone(),
        tlog_dominant_payload_kind_bytes: snapshot.tlog_delta_signals.dominant_payload_kind_bytes,
        last_plan_text_payload_bytes: snapshot.tlog_delta_signals.last_plan_text_payload_bytes,
        last_executor_diff_payload_bytes: snapshot
            .tlog_delta_signals
            .last_executor_diff_payload_bytes,
        tlog_git_checkpoint_blocked: snapshot.tlog_delta_signals.git_checkpoint_blocked,
        tlog_unsafe_checkpoint_attempts: snapshot.tlog_delta_signals.unsafe_checkpoint_attempts,
        diagnostics_repair_pressure: snapshot.diagnostics_repair_pressure,
        semantic_fn_error_rate: snapshot.semantic_fn_error_rate,
        semantic_fn_total: snapshot.semantic_fn_total,
        semantic_fn_with_any_error: snapshot.semantic_fn_with_any_error,
        semantic_fn_intent_classified: snapshot.semantic_fn_intent_classified,
        semantic_fn_totalized: snapshot.semantic_fn_totalized,
        semantic_fn_totalization_coverage: snapshot.semantic_fn_totalization_coverage,
        semantic_fn_low_confidence: snapshot.semantic_fn_low_confidence,
        semantic_fn_intent_coverage: snapshot.semantic_fn_intent_coverage,
        semantic_fn_low_confidence_rate: snapshot.semantic_fn_low_confidence_rate,
        eval_enforcement_passed: snapshot.eval_enforcement.passed,
        eval_enforcement_violation_count: snapshot.eval_enforcement.violation_count,
        eval_enforcement_violations: snapshot.eval_enforcement.violations.clone(),
        eval_enforcement_warning_count: snapshot.eval_enforcement.warning_count,
        eval_enforcement_warnings: snapshot.eval_enforcement.warnings.clone(),
        tlog_prompt_truncation_count: snapshot.tlog_delta_signals.prompt_truncations,
        tlog_prompt_truncation_dropped_bytes: snapshot
            .tlog_delta_signals
            .prompt_truncation_dropped_bytes,
        blocker_distinct_classes: snapshot.blocker_class_coverage.distinct_classes,
        blocker_covered_classes: snapshot.blocker_class_coverage.covered_classes,
        blocker_top_uncovered: snapshot
            .blocker_class_coverage
            .top_uncovered
            .clone()
            .unwrap_or_default(),
        blocker_class_coverage: snapshot.vector.blocker_class_coverage,
    };

    if let Some(w) = writer {
        let _ = w.try_record_effect(effect);
    } else {
        let _ = crate::logging::record_effect_for_workspace(workspace, effect);
    }

    // Run machine_verify checks for all active repair plans and record outcomes.
    let eval_map = crate::repair_plans::snapshot_to_eval_map(&snapshot);
    let invariant_text = std::fs::read_to_string(
        workspace.join("agent_state").join("enforced_invariants.json"),
    )
    .unwrap_or_default();
    let plans = crate::repair_plans::build_all_active_plans(&eval_map, workspace, usize::MAX);
    for plan in &plans {
        let passed = plan.machine_verify.check(&eval_map, &invariant_text);
        let verify_effect = EffectEvent::PlanVerifyRecorded {
            plan_id: plan.id.clone(),
            plan_kind: plan.kind.to_string(),
            passed,
            verify_description: plan.machine_verify.description(),
        };
        let _ = crate::logging::record_effect_for_workspace(workspace, verify_effect);
    }

    Ok((snapshot, delta))
}

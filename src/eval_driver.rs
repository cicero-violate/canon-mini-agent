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
        tlog_dominant_payload_kind_bytes: snapshot
            .tlog_delta_signals
            .dominant_payload_kind_bytes,
        last_plan_text_payload_bytes: snapshot.tlog_delta_signals.last_plan_text_payload_bytes,
        last_executor_diff_payload_bytes: snapshot
            .tlog_delta_signals
            .last_executor_diff_payload_bytes,
        diagnostics_repair_pressure: snapshot.diagnostics_repair_pressure,
        semantic_fn_error_rate: snapshot.semantic_fn_error_rate,
        semantic_fn_total: snapshot.semantic_fn_total,
        semantic_fn_with_any_error: snapshot.semantic_fn_with_any_error,
    };

    if let Some(w) = writer {
        let _ = w.try_record_effect(effect);
    } else {
        let _ = crate::logging::record_effect_for_workspace(workspace, effect);
    }

    Ok((snapshot, delta))
}

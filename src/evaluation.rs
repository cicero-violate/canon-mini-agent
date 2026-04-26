use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::events::{ControlEvent, EffectEvent, Event};
use crate::issues::Issue;
#[cfg(test)]
use crate::issues::IssuesFile;
use crate::reports::{DiagnosticsReport, ViolationsReport};

/// All pre-loaded data required to compute an eval snapshot — no I/O.
#[derive(Debug, Clone)]
pub struct EvalInput {
    pub objectives_completed: usize,
    pub objectives_total: usize,
    pub violations: ViolationsReport,
    pub completed_tasks: usize,
    pub total_tasks: usize,
    pub open_issues: usize,
    pub repeated_open_issues: usize,
    /// Open issues with priority = "high" or "critical".
    pub high_priority_open_issues: usize,
    pub diagnostics: DiagnosticsReport,
    pub semantic_fn_total: usize,
    pub semantic_fn_with_any_error: usize,
    pub semantic_fn_error_rate: f64,
    pub semantic_fn_intent_classified: usize,
    pub semantic_fn_low_confidence: usize,
    pub semantic_fn_intent_coverage: f64,
    pub semantic_fn_low_confidence_rate: f64,
    pub structural_invariant_coverage: StructuralInvariantCoverage,
    pub tlog_delta_signals: TlogDeltaSignals,
}

/// Direction and magnitude of change between two consecutive eval snapshots.
#[derive(Debug, Clone, Default)]
pub struct EvalDelta {
    /// current.overall_score() − previous.overall_score
    pub delta_g: f64,
    pub semantic_contract_delta: f64,
    pub safety_delta: f64,
    /// True when no dimension regressed beyond tolerance.
    pub promotion_eligible: bool,
}

#[derive(Debug, Clone, Default)]
pub struct EvalEnforcement {
    pub passed: bool,
    pub violation_count: usize,
    pub violations: Vec<String>,
    pub warning_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EvaluationVector {
    pub objective_progress: f64,
    pub safety: f64,
    pub task_velocity: f64,
    pub issue_health: f64,
    pub semantic_contract: f64,
    pub structural_invariant_coverage: f64,
    pub canonical_delta_health: f64,
    pub improvement_measurement: f64,
    pub improvement_validation: f64,
    pub improvement_effectiveness: f64,
    pub recovery_effectiveness: f64,
}

impl EvaluationVector {
    pub fn geometric_mean_like_score(&self) -> f64 {
        let values = [
            self.objective_progress.clamp(0.001, 1.0),
            self.safety.clamp(0.001, 1.0),
            self.task_velocity.clamp(0.001, 1.0),
            self.issue_health.clamp(0.001, 1.0),
            self.semantic_contract.clamp(0.001, 1.0),
            self.structural_invariant_coverage.clamp(0.001, 1.0),
            self.canonical_delta_health.clamp(0.001, 1.0),
            self.improvement_measurement.clamp(0.001, 1.0),
            self.improvement_validation.clamp(0.001, 1.0),
            self.improvement_effectiveness.clamp(0.001, 1.0),
            self.recovery_effectiveness.clamp(0.001, 1.0),
        ];
        let product = values.iter().product::<f64>();
        product.powf(1.0 / values.len() as f64)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StructuralInvariantCoverage {
    pub graph_risk_count: usize,
    pub invariant_covered_count: usize,
    pub missing_invariant_count: usize,
    pub score: f64,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EvaluationWorkspaceSnapshot {
    pub objectives_completed: usize,
    pub objectives_total: usize,
    pub completed_tasks: usize,
    pub total_tasks: usize,
    pub open_issues: usize,
    pub repeated_open_issues: usize,
    pub diagnostics_repair_pressure: f64,
    pub semantic_fn_total: usize,
    pub semantic_fn_with_any_error: usize,
    pub semantic_fn_error_rate: f64,
    pub semantic_fn_intent_classified: usize,
    pub semantic_fn_totalized: usize,
    pub semantic_fn_totalization_coverage: f64,
    pub semantic_fn_low_confidence: usize,
    pub semantic_fn_intent_coverage: f64,
    pub semantic_fn_low_confidence_rate: f64,
    pub structural_invariant_coverage: StructuralInvariantCoverage,
    pub tlog_delta_signals: TlogDeltaSignals,
    pub eval_enforcement: EvalEnforcement,
    pub vector: EvaluationVector,
}

impl EvaluationWorkspaceSnapshot {
    pub fn overall_score(&self) -> f64 {
        let base = self.vector.geometric_mean_like_score();
        let repair_penalty = 0.25 * self.diagnostics_repair_pressure;
        let enforcement_penalty = if self.eval_enforcement.passed {
            0.0
        } else {
            (self.eval_enforcement.violation_count as f64 * 0.15).min(0.75)
        };
        clamp_unit(base * (1.0 - repair_penalty) * (1.0 - enforcement_penalty))
    }
}

/// Pure kernel entry point — no I/O.  All inputs are pre-loaded by the caller.
pub fn compute_eval(input: &EvalInput) -> EvaluationWorkspaceSnapshot {
    let semantic_fn_error_rate = semantic_rate_from_counts(
        input.semantic_fn_total,
        input.semantic_fn_with_any_error,
    );
    let intent_coverage = semantic_coverage_from_counts(
        input.semantic_fn_total,
        input.semantic_fn_intent_classified,
    );
    let semantic_fn_totalized = input
        .semantic_fn_intent_classified
        .saturating_add(input.semantic_fn_low_confidence)
        .min(input.semantic_fn_total);
    let semantic_fn_totalization_coverage =
        semantic_coverage_from_counts(input.semantic_fn_total, semantic_fn_totalized);
    let semantic_fn_low_confidence_rate =
        semantic_rate_from_counts(input.semantic_fn_total, input.semantic_fn_low_confidence);
    let semantic_contract_score = (
        (1.0 - semantic_fn_error_rate).clamp(0.0, 1.0)
            * intent_coverage
            * (1.0 - semantic_fn_low_confidence_rate).clamp(0.0, 1.0)
    )
        .clamp(0.0, 1.0);
    let mut vector = evaluate_repo_state(
        input.objectives_completed,
        input.objectives_total,
        &input.violations,
        input.completed_tasks,
        input.total_tasks,
        input.open_issues,
        input.repeated_open_issues,
        semantic_contract_score,
        input.structural_invariant_coverage.score,
        input.tlog_delta_signals.score,
        input.tlog_delta_signals.improvement_measurement_score,
        input.tlog_delta_signals.improvement_validation_score,
        input.tlog_delta_signals.improvement_effectiveness_score,
        input.tlog_delta_signals.recovery_effectiveness_score,
    );
    // Fold semantic error rate into safety so a clean build alone cannot produce safety = 1.0.
    vector.safety = clamp_unit(vector.safety * (1.0 - 0.3 * semantic_fn_error_rate));
    let diagnostics_repair_pressure = diagnostics_repair_pressure_with_issues(
        &input.diagnostics,
        input.high_priority_open_issues,
    );
    let eval_enforcement = enforce_eval_thresholds(
        input,
        semantic_fn_error_rate,
        semantic_fn_totalized,
        semantic_fn_totalization_coverage,
        semantic_fn_low_confidence_rate,
        semantic_contract_score,
        diagnostics_repair_pressure,
    );
    EvaluationWorkspaceSnapshot {
        objectives_completed: input.objectives_completed,
        objectives_total: input.objectives_total,
        completed_tasks: input.completed_tasks,
        total_tasks: input.total_tasks,
        open_issues: input.open_issues,
        repeated_open_issues: input.repeated_open_issues,
        diagnostics_repair_pressure,
        semantic_fn_total: input.semantic_fn_total,
        semantic_fn_with_any_error: input.semantic_fn_with_any_error,
        semantic_fn_error_rate,
        semantic_fn_intent_classified: input.semantic_fn_intent_classified,
        semantic_fn_totalized,
        semantic_fn_totalization_coverage,
        semantic_fn_low_confidence: input.semantic_fn_low_confidence,
        semantic_fn_intent_coverage: intent_coverage,
        semantic_fn_low_confidence_rate,
        structural_invariant_coverage: input.structural_invariant_coverage.clone(),
        tlog_delta_signals: input.tlog_delta_signals.clone(),
        eval_enforcement,
        vector,
    }
}

fn semantic_rate_from_counts(fn_total: usize, count: usize) -> f64 {
    if fn_total == 0 {
        0.0
    } else {
        safe_ratio(count.min(fn_total) as f64, fn_total as f64).clamp(0.0, 1.0)
    }
}

fn semantic_coverage_from_counts(fn_total: usize, classified: usize) -> f64 {
    if fn_total == 0 {
        1.0
    } else {
        safe_ratio(classified.min(fn_total) as f64, fn_total as f64).clamp(0.0, 1.0)
    }
}

const EVAL_MAX_SEMANTIC_ERROR_RATE: f64 = 0.0;
const EVAL_MIN_INTENT_TOTALIZATION_COVERAGE: f64 = 1.0;
const EVAL_MAX_ACTIONABLE_LAG_TOTAL_MS: u64 = 300_000;
const EVAL_MAX_PROMPT_TRUNCATIONS: usize = 0;
const EVAL_MIN_MEANINGFUL_INTENT_COVERAGE_WARNING: f64 = 0.75;
const EVAL_MAX_LOW_CONFIDENCE_RATE_WARNING: f64 = 0.25;
const EVAL_MIN_SEMANTIC_CONTRACT_WARNING: f64 = 0.50;

fn enforce_eval_thresholds(
    input: &EvalInput,
    semantic_fn_error_rate: f64,
    semantic_fn_totalized: usize,
    semantic_fn_totalization_coverage: f64,
    semantic_fn_low_confidence_rate: f64,
    semantic_contract_score: f64,
    diagnostics_repair_pressure: f64,
) -> EvalEnforcement {
    let mut violations = Vec::new();
    let mut warnings = Vec::new();

    append_eval_threshold_violations(
        &mut violations,
        &mut warnings,
        input,
        semantic_fn_error_rate,
        semantic_fn_totalized,
        semantic_fn_totalization_coverage,
    );
    append_eval_threshold_warnings(
        &mut warnings,
        input,
        semantic_fn_low_confidence_rate,
        semantic_contract_score,
        diagnostics_repair_pressure,
    );

    EvalEnforcement {
        passed: violations.is_empty(),
        violation_count: violations.len(),
        violations,
        warning_count: warnings.len(),
        warnings,
    }
}

fn append_eval_threshold_violations(
    violations: &mut Vec<String>,
    warnings: &mut Vec<String>,
    input: &EvalInput,
    semantic_fn_error_rate: f64,
    semantic_fn_totalized: usize,
    semantic_fn_totalization_coverage: f64,
) {

    if input.semantic_fn_total > 0 && semantic_fn_error_rate > EVAL_MAX_SEMANTIC_ERROR_RATE {
        violations.push(format!(
            "semantic_errors={}/{} rate={:.4} > {:.4}",
            input.semantic_fn_with_any_error,
            input.semantic_fn_total,
            semantic_fn_error_rate,
            EVAL_MAX_SEMANTIC_ERROR_RATE
        ));
    }
    if input.semantic_fn_total > 0
        && semantic_fn_totalization_coverage < EVAL_MIN_INTENT_TOTALIZATION_COVERAGE
    {
        violations.push(format!(
            "intent_totalization={}/{} coverage={:.4} < {:.4}",
            semantic_fn_totalized,
            input.semantic_fn_total,
            semantic_fn_totalization_coverage,
            EVAL_MIN_INTENT_TOTALIZATION_COVERAGE
        ));
    }
    if input.tlog_delta_signals.actionable_lag_total_ms > EVAL_MAX_ACTIONABLE_LAG_TOTAL_MS {
        warnings.push(format!(
            "actionable_lag_total_ms={} > {}",
            input.tlog_delta_signals.actionable_lag_total_ms,
            EVAL_MAX_ACTIONABLE_LAG_TOTAL_MS
        ));
    }
    if input.tlog_delta_signals.prompt_truncations > EVAL_MAX_PROMPT_TRUNCATIONS {
        warnings.push(format!(
            "prompt_truncations={} > {} dropped_bytes={}",
            input.tlog_delta_signals.prompt_truncations,
            EVAL_MAX_PROMPT_TRUNCATIONS,
            input.tlog_delta_signals.prompt_truncation_dropped_bytes
        ));
    }
    if input.tlog_delta_signals.unsafe_checkpoint_attempts > 0 {
        violations.push(format!(
            "unsafe_checkpoint_attempts={}",
            input.tlog_delta_signals.unsafe_checkpoint_attempts
        ));
    }
    if input.tlog_delta_signals.missing_action_results > 0 {
        warnings.push(format!(
            "missing_action_results={}/{}",
            input.tlog_delta_signals.missing_action_results,
            input.tlog_delta_signals.llm_action_outputs
        ));
    }
    if input.tlog_delta_signals.unmeasured_improvement_attempts > 0 {
        warnings.push(format!(
            "unmeasured_improvement_attempts={}/{}",
            input.tlog_delta_signals.unmeasured_improvement_attempts,
            input.tlog_delta_signals.improvement_attempts
        ));
    }
    if input.tlog_delta_signals.unvalidated_improvement_attempts > 0 {
        violations.push(format!(
            "unvalidated_improvement_attempts={}/{}",
            input.tlog_delta_signals.unvalidated_improvement_attempts,
            input.tlog_delta_signals.improvement_attempts
        ));
    }
    if input.tlog_delta_signals.regressed_improvement_attempts > 0 {
        warnings.push(format!(
            "regressed_improvement_attempts={}/{}",
            input.tlog_delta_signals.regressed_improvement_attempts,
            input.tlog_delta_signals.measured_improvement_attempts
        ));
    }
}

fn append_eval_threshold_warnings(
    warnings: &mut Vec<String>,
    input: &EvalInput,
    semantic_fn_low_confidence_rate: f64,
    semantic_contract_score: f64,
    diagnostics_repair_pressure: f64,
) {
    if input.semantic_fn_total > 0
        && input.semantic_fn_intent_coverage < EVAL_MIN_MEANINGFUL_INTENT_COVERAGE_WARNING
    {
        warnings.push(format!(
            "meaningful_intent_coverage={:.4} < {:.4}",
            input.semantic_fn_intent_coverage, EVAL_MIN_MEANINGFUL_INTENT_COVERAGE_WARNING
        ));
    }
    if input.semantic_fn_total > 0
        && semantic_fn_low_confidence_rate > EVAL_MAX_LOW_CONFIDENCE_RATE_WARNING
    {
        warnings.push(format!(
            "low_confidence_rate={:.4} > {:.4}",
            semantic_fn_low_confidence_rate, EVAL_MAX_LOW_CONFIDENCE_RATE_WARNING
        ));
    }
    if semantic_contract_score < EVAL_MIN_SEMANTIC_CONTRACT_WARNING {
        warnings.push(format!(
            "semantic_contract={:.4} < {:.4}",
            semantic_contract_score, EVAL_MIN_SEMANTIC_CONTRACT_WARNING
        ));
    }
    if diagnostics_repair_pressure > 0.0 {
        warnings.push(format!(
            "diagnostics_repair_pressure={:.4}",
            diagnostics_repair_pressure
        ));
    }
}

/// Pure delta — no I/O.  Call after two consecutive `compute_eval` results.
pub fn compute_delta(
    current: &EvaluationWorkspaceSnapshot,
    prev_overall: f64,
    prev_semantic: f64,
    prev_safety: f64,
) -> EvalDelta {
    let delta_g = current.overall_score() - prev_overall;
    let semantic_contract_delta = current.vector.semantic_contract - prev_semantic;
    let safety_delta = current.vector.safety - prev_safety;
    EvalDelta {
        delta_g,
        semantic_contract_delta,
        safety_delta,
        promotion_eligible: delta_g >= -0.001
            && semantic_contract_delta >= -0.001
            && safety_delta >= -0.001,
    }
}

/// `diagnostics_repair_pressure` floored by open high-priority issue pressure.
///
/// Prevents a clean cargo build from reporting zero pressure when hundreds of
/// high-priority issues are open.
pub fn diagnostics_repair_pressure_with_issues(
    report: &DiagnosticsReport,
    high_priority_open_issues: usize,
) -> f64 {
    let cargo_pressure = diagnostics_repair_pressure(report);
    let issue_floor = (high_priority_open_issues as f64 / 100.0).min(0.10);
    cargo_pressure.max(issue_floor)
}

pub fn reward_alignment_score(completed_objectives: usize, total_objectives: usize) -> f64 {
    if total_objectives == 0 {
        return 1.0;
    }
    safe_ratio(completed_objectives as f64, total_objectives.max(1) as f64)
}

pub fn safety_score(violations: &ViolationsReport) -> f64 {
    let severity_penalty = violations
        .violations
        .iter()
        .map(|v| match v.severity {
            crate::reports::Severity::Critical => 1.0,
            crate::reports::Severity::High => 0.6,
            crate::reports::Severity::Medium => 0.3,
            crate::reports::Severity::Low => 0.1,
        })
        .sum::<f64>();
    clamp_unit(1.0 - severity_penalty)
}

pub fn issue_health_score(open_issues: usize, repeated_open_issues: usize) -> f64 {
    if open_issues == 0 {
        return 1.0;
    }
    clamp_unit(1.0 - safe_ratio(repeated_open_issues as f64, open_issues as f64))
}

pub fn task_velocity_score(completed_tasks: usize, total_tasks: usize) -> f64 {
    safe_ratio(completed_tasks as f64, total_tasks.max(1) as f64)
}

/// Intent: validation_gate
/// Resource: evaluation_vector
/// Inputs: usize, usize, &reports::ViolationsReport, usize, usize, usize, usize
/// Outputs: evaluation::EvaluationVector
/// Effects: none
/// Forbidden: mutation
/// Invariants: component scores are derived from repo state inputs; direct score inputs are clamped to [0.0, 1.0]
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
pub fn evaluate_repo_state(
    objectives_completed: usize,
    objectives_total: usize,
    violations: &ViolationsReport,
    completed_tasks: usize,
    total_tasks: usize,
    open_issues: usize,
    repeated_open_issues: usize,
    semantic_contract_score: f64,
    structural_invariant_coverage_score: f64,
    canonical_delta_health_score: f64,
    improvement_measurement_score: f64,
    improvement_validation_score: f64,
    improvement_effectiveness_score: f64,
    recovery_effectiveness_score: f64,
) -> EvaluationVector {
    EvaluationVector {
        objective_progress: reward_alignment_score(objectives_completed, objectives_total),
        safety: safety_score(violations),
        task_velocity: task_velocity_score(completed_tasks, total_tasks),
        issue_health: issue_health_score(open_issues, repeated_open_issues),
        semantic_contract: semantic_contract_score.clamp(0.0, 1.0),
        structural_invariant_coverage: structural_invariant_coverage_score.clamp(0.0, 1.0),
        canonical_delta_health: canonical_delta_health_score.clamp(0.0, 1.0),
        improvement_measurement: improvement_measurement_score.clamp(0.0, 1.0),
        improvement_validation: improvement_validation_score.clamp(0.0, 1.0),
        improvement_effectiveness: improvement_effectiveness_score.clamp(0.0, 1.0),
        recovery_effectiveness: recovery_effectiveness_score.clamp(0.0, 1.0),
    }
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: evaluation::EvaluationWorkspaceSnapshot
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub fn evaluate_workspace(workspace: &Path) -> EvaluationWorkspaceSnapshot {
    let objectives = load_objectives_file(workspace);
    let objectives_total = objectives.objectives.len();
    let objectives_completed = objectives
        .objectives
        .iter()
        .filter(|objective| crate::objectives::is_completed(objective))
        .count();

    let violations = crate::reports::load_violations_report(workspace);
    let diagnostics =
        crate::reports::load_diagnostics_report(workspace).unwrap_or_else(empty_diagnostics_report);
    let tlog_delta_signals = load_tlog_delta_signals(workspace);

    let (completed_tasks, total_tasks) = load_task_counts(workspace);
    let (open_issues, repeated_open_issues, high_priority_open_issues) =
        load_issue_counts(workspace);
    let semantic_metrics = crate::semantic_contract::load_semantic_manifest_metrics(workspace);
    let structural_invariant_coverage = load_structural_invariant_coverage(workspace);

    compute_eval(&EvalInput {
        objectives_completed,
        objectives_total,
        violations,
        completed_tasks,
        total_tasks,
        open_issues,
        repeated_open_issues,
        high_priority_open_issues,
        diagnostics,
        semantic_fn_total: semantic_metrics.fn_total,
        semantic_fn_with_any_error: semantic_metrics.fn_with_any_error,
        semantic_fn_error_rate: semantic_metrics.fn_error_rate,
        semantic_fn_intent_classified: semantic_metrics.fn_intent_classified,
        semantic_fn_low_confidence: semantic_metrics.fn_low_confidence,
        semantic_fn_intent_coverage: semantic_metrics.fn_intent_coverage,
        semantic_fn_low_confidence_rate: semantic_metrics.fn_low_confidence_rate,
        structural_invariant_coverage,
        tlog_delta_signals,
    })
}

#[derive(Debug, Clone, Default)]
pub struct TlogDeltaSignals {
    pub event_count: usize,
    pub contiguous_seq: bool,
    pub missing_seq_count: usize,
    pub lag_gap_count: usize,
    pub lag_total_ms: u64,
    pub max_event_gap_ms: u64,
    pub actionable_lag_gap_count: usize,
    pub actionable_lag_total_ms: u64,
    pub dominant_actionable_lag_kind: String,
    pub dominant_actionable_lag_kind_ms: u64,
    pub issues_projection_lag_ms: u64,
    pub issues_projection_lag_count: usize,
    pub dominant_payload_kind: String,
    pub dominant_payload_kind_bytes: u64,
    pub last_plan_text_payload_bytes: u64,
    pub last_executor_diff_payload_bytes: u64,
    pub llm_turn_inputs: usize,
    pub llm_turn_outputs: usize,
    pub llm_action_outputs: usize,
    pub action_results: usize,
    pub missing_action_results: usize,
    pub llm_error_boundaries: usize,
    pub artifact_write_requests: usize,
    pub artifact_write_applies: usize,
    pub unapplied_artifact_writes: usize,
    pub git_checkpoint_blocked: usize,
    pub unsafe_checkpoint_attempts: usize,
    pub prompt_truncations: usize,
    pub prompt_truncation_dropped_bytes: u64,
    pub supervisor_restart_requests: usize,
    pub supervisor_child_starts: usize,
    pub restart_requests_without_child_start: usize,
    pub improvement_attempts: usize,
    pub measured_improvement_attempts: usize,
    pub unmeasured_improvement_attempts: usize,
    pub validated_improvement_attempts: usize,
    pub unvalidated_improvement_attempts: usize,
    pub non_regressed_improvement_attempts: usize,
    pub regressed_improvement_attempts: usize,
    pub eval_measurement_points: usize,
    pub measurement_regressions: usize,
    pub recovery_attempts: usize,
    pub recovery_successes: usize,
    pub recovery_failures: usize,
    pub recovery_suppressed: usize,
    pub recovery_loop_breaks: usize,
    pub recovery_regressions: usize,
    pub recovery_measurement_points: usize,
    pub recovery_effectiveness_score: f64,
    pub improvement_measurement_score: f64,
    pub improvement_validation_score: f64,
    pub improvement_effectiveness_score: f64,
    pub score: f64,
}

const TLOG_LAG_GAP_MIN_MS: u64 = 1_000;
const TLOG_LAG_GAP_MAX_MS: u64 = 120_000;

/// Pure invariant signal over a tlog delta window.
///
/// Model: `I(ΔT) -> signal`, where ΔT is the recent canonical event window.
pub fn evaluate_tlog_delta_invariants(records: &[crate::tlog::TlogRecord]) -> TlogDeltaSignals {
    let mut signals = TlogDeltaSignals {
        event_count: records.len(),
        contiguous_seq: true,
        ..TlogDeltaSignals::default()
    };
    if records.is_empty() {
        return signals;
    }

    let mut prev_seq = records.first().map(|record| record.seq).unwrap_or_default();
    let mut llm_action_command_ids = BTreeSet::new();
    let mut action_result_command_ids = BTreeSet::new();
    let mut requested_artifact_signatures = BTreeSet::new();
    let mut applied_artifact_signatures = BTreeSet::new();
    let mut actionable_lag_by_next_kind: HashMap<String, u64> = HashMap::new();
    let mut payload_bytes_by_kind: HashMap<String, u64> = HashMap::new();
    let mut unmatched_restart_requests = 0usize;
    let mut open_improvement_attempts = 0usize;
    let mut open_unvalidated_improvement_attempts = 0usize;

    for (idx, record) in records.iter().enumerate() {
        let record_kind = tlog_event_kind(&record.event);
        let payload_bytes = serde_json::to_string(&record.event)
            .map(|raw| raw.len() as u64)
            .unwrap_or_default();
        *payload_bytes_by_kind
            .entry(record_kind.clone())
            .or_default() += payload_bytes;

        if idx > 0 && record.seq != prev_seq.saturating_add(1) {
            signals.contiguous_seq = false;
            signals.missing_seq_count +=
                record.seq.saturating_sub(prev_seq.saturating_add(1)) as usize;
        }
        if idx > 0 {
            let prev = &records[idx - 1];
            let gap_ms = record.ts_ms.saturating_sub(prev.ts_ms);
            if (TLOG_LAG_GAP_MIN_MS..=TLOG_LAG_GAP_MAX_MS).contains(&gap_ms) {
                signals.lag_gap_count += 1;
                signals.lag_total_ms = signals.lag_total_ms.saturating_add(gap_ms);
                signals.max_event_gap_ms = signals.max_event_gap_ms.max(gap_ms);

                let next_kind = record_kind.clone();
                if next_kind == "issues_projection_recorded" {
                    signals.issues_projection_lag_count += 1;
                    signals.issues_projection_lag_ms =
                        signals.issues_projection_lag_ms.saturating_add(gap_ms);
                }
                if is_actionable_lag_kind(&next_kind) {
                    signals.actionable_lag_gap_count += 1;
                    signals.actionable_lag_total_ms =
                        signals.actionable_lag_total_ms.saturating_add(gap_ms);
                    *actionable_lag_by_next_kind.entry(next_kind).or_default() += gap_ms;
                }
            }
        }
        prev_seq = record.seq;

        match &record.event {
            Event::Control {
                event: ControlEvent::LastPlanTextSet { text },
            }
            | Event::Control {
                event: ControlEvent::LastSoloPlanTextSet { text },
            } => {
                signals.last_plan_text_payload_bytes = signals
                    .last_plan_text_payload_bytes
                    .saturating_add(text.len() as u64);
            }
            Event::Control {
                event: ControlEvent::LastExecutorDiffSet { text },
            }
            | Event::Control {
                event: ControlEvent::LastSoloExecutorDiffSet { text },
            } => {
                signals.last_executor_diff_payload_bytes = signals
                    .last_executor_diff_payload_bytes
                    .saturating_add(text.len() as u64);
            }
            Event::Effect {
                event: EffectEvent::LlmTurnInput { .. },
            } => signals.llm_turn_inputs += 1,
            Event::Effect {
                event:
                    EffectEvent::LlmTurnOutput {
                        command_id,
                        action_kind,
                        ..
                    },
            } => {
                signals.llm_turn_outputs += 1;
                if action_kind.is_some() {
                    signals.llm_action_outputs += 1;
                    llm_action_command_ids.insert(command_id.clone());
                }
            }
            Event::Effect {
                event: EffectEvent::ActionResultRecorded { command_id, .. },
            } => {
                signals.action_results += 1;
                action_result_command_ids.insert(command_id.clone());
            }
            Event::Effect {
                event: EffectEvent::LlmErrorBoundary { .. },
            } => signals.llm_error_boundaries += 1,
            Event::Effect {
                event:
                    EffectEvent::PromptTruncationRecorded {
                        dropped_bytes, ..
                    },
            } => {
                signals.prompt_truncations += 1;
                signals.prompt_truncation_dropped_bytes = signals
                    .prompt_truncation_dropped_bytes
                    .saturating_add(*dropped_bytes as u64);
            }
            Event::Effect {
                event: EffectEvent::WorkspaceArtifactWriteRequested { signature, .. },
            } => {
                signals.artifact_write_requests += 1;
                requested_artifact_signatures.insert(signature.clone());
            }
            Event::Effect {
                event: EffectEvent::WorkspaceArtifactWriteApplied { signature, .. },
            } => {
                signals.artifact_write_applies += 1;
                applied_artifact_signatures.insert(signature.clone());
            }
            Event::Effect {
                event:
                    EffectEvent::GitCheckpointBlocked {
                        verification_requested,
                        rust_sensitive_changes,
                        ..
                    },
            } => {
                signals.git_checkpoint_blocked += 1;
                if *rust_sensitive_changes && !*verification_requested {
                    signals.unsafe_checkpoint_attempts += 1;
                }
            }
            Event::Effect {
                event: EffectEvent::SupervisorRestartRequested { .. },
            } => {
                signals.supervisor_restart_requests += 1;
                unmatched_restart_requests = unmatched_restart_requests.saturating_add(1);
            }
            Event::Effect {
                event: EffectEvent::SupervisorChildStarted { .. },
            } => {
                signals.supervisor_child_starts += 1;
                unmatched_restart_requests = unmatched_restart_requests.saturating_sub(1);
            }
            Event::Effect {
                event:
                    EffectEvent::PostRestartResultRecorded {
                        action,
                        result,
                        ..
                    },
            } => {
                if is_successful_improvement_action(action, result) {
                    signals.improvement_attempts += 1;
                    open_improvement_attempts = open_improvement_attempts.saturating_add(1);
                    if is_improvement_validation_result(action, result) {
                        signals.validated_improvement_attempts =
                            signals.validated_improvement_attempts.saturating_add(1);
                    } else {
                        open_unvalidated_improvement_attempts =
                            open_unvalidated_improvement_attempts.saturating_add(1);
                    }
                } else if open_unvalidated_improvement_attempts > 0
                    && is_improvement_validation_result(action, result)
                {
                    signals.validated_improvement_attempts = signals
                        .validated_improvement_attempts
                        .saturating_add(open_unvalidated_improvement_attempts);
                    open_unvalidated_improvement_attempts = 0;
                }
            }
            Event::Effect {
                event:
                    EffectEvent::EvalScoreRecorded {
                        delta_g,
                        promotion_eligible,
                        ..
                    },
            } => {
                let pending_improvement_attempts = open_improvement_attempts;
                let regressed = eval_measurement_regressed(*delta_g, *promotion_eligible);
                signals.eval_measurement_points += 1;
                if pending_improvement_attempts > 0 {
                    signals.measured_improvement_attempts = signals
                        .measured_improvement_attempts
                        .saturating_add(pending_improvement_attempts);
                    if regressed {
                        signals.regressed_improvement_attempts = signals
                            .regressed_improvement_attempts
                            .saturating_add(pending_improvement_attempts);
                    } else {
                        signals.non_regressed_improvement_attempts = signals
                            .non_regressed_improvement_attempts
                            .saturating_add(pending_improvement_attempts);
                    }
                    open_improvement_attempts = 0;
                }
                if regressed {
                    signals.measurement_regressions += 1;
                }
            }
            Event::Effect {
                event: EffectEvent::RecoverySuppressed { .. },
            } => signals.recovery_suppressed += 1,
            _ => {}
        }
    }

    score_recovery_windows(records, &mut signals);
    score_suppressed_recovery_outcomes(records, &mut signals);
    signals.missing_action_results = llm_action_command_ids
        .difference(&action_result_command_ids)
        .count();
    signals.unapplied_artifact_writes = requested_artifact_signatures
        .difference(&applied_artifact_signatures)
        .count();
    signals.restart_requests_without_child_start = unmatched_restart_requests;
    signals.unmeasured_improvement_attempts = open_improvement_attempts;
    signals.unvalidated_improvement_attempts = open_unvalidated_improvement_attempts;
    if let Some((kind, lag_ms)) = actionable_lag_by_next_kind
        .into_iter()
        .max_by_key(|(_, lag_ms)| *lag_ms)
    {
        signals.dominant_actionable_lag_kind = kind;
        signals.dominant_actionable_lag_kind_ms = lag_ms;
    }
    if let Some((kind, bytes)) = payload_bytes_by_kind
        .into_iter()
        .max_by_key(|(_, bytes)| *bytes)
    {
        signals.dominant_payload_kind = kind;
        signals.dominant_payload_kind_bytes = bytes;
    }
    signals.improvement_measurement_score = improvement_measurement_score(&signals);
    signals.improvement_validation_score = improvement_validation_score(&signals);
    signals.improvement_effectiveness_score = improvement_effectiveness_score(&signals);
    signals.recovery_effectiveness_score = recovery_effectiveness_score(&signals);
    signals.score = canonical_delta_health_score(&signals);
    signals
}

fn load_tlog_delta_signals(workspace: &Path) -> TlogDeltaSignals {
    let path = workspace.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::read_recent_records(&path, 2_000)
        .map(|records| evaluate_tlog_delta_invariants(&records))
        .unwrap_or_default()
}

fn canonical_delta_health_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.event_count == 0 {
        return 0.0;
    }
    let scores = canonical_delta_health_scores(signals);
    geometric_score(&scores)
}

fn canonical_seq_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.contiguous_seq {
        1.0
    } else {
        1.0 - safe_ratio(signals.missing_seq_count as f64, signals.event_count as f64)
    }
}

fn canonical_turn_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.llm_turn_inputs == 0 {
        1.0
    } else {
        safe_ratio(
            signals.llm_turn_outputs as f64,
            signals.llm_turn_inputs as f64,
        )
        .min(1.0)
    }
}

fn canonical_action_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.llm_action_outputs == 0 {
        1.0
    } else {
        1.0 - safe_ratio(
            signals.missing_action_results as f64,
            signals.llm_action_outputs as f64,
        )
    }
}

fn canonical_artifact_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.artifact_write_requests == 0 {
        1.0
    } else {
        1.0 - safe_ratio(
            signals.unapplied_artifact_writes as f64,
            signals.artifact_write_requests as f64,
        )
    }
}

fn canonical_restart_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.supervisor_restart_requests == 0 {
        1.0
    } else {
        1.0 - safe_ratio(
            signals.restart_requests_without_child_start as f64,
            signals.supervisor_restart_requests as f64,
        )
        .min(0.75)
    }
}

fn canonical_delta_health_scores(signals: &TlogDeltaSignals) -> [f64; 11] {
    let error_score = 1.0
        - safe_ratio(
            signals.llm_error_boundaries as f64,
            (signals.llm_turn_inputs + signals.llm_turn_outputs).max(1) as f64,
        )
        .min(1.0);
    let lag_budget_ms = (signals.event_count.max(1) as f64) * 5_000.0;
    let lag_score =
        1.0 - safe_ratio(signals.actionable_lag_total_ms as f64, lag_budget_ms).min(0.75);
    let checkpoint_score =
        1.0 - safe_ratio(signals.unsafe_checkpoint_attempts as f64, 4.0).min(0.75);
    let prompt_truncation_score =
        1.0 - safe_ratio(signals.prompt_truncations as f64, 4.0).min(0.75);
    [
        canonical_seq_score(signals),
        canonical_turn_score(signals),
        canonical_action_score(signals),
        canonical_artifact_score(signals),
        error_score,
        lag_score,
        checkpoint_score,
        prompt_truncation_score,
        canonical_restart_score(signals),
        improvement_validation_score(signals),
        geometric_score(&[
            improvement_effectiveness_score(signals),
            recovery_effectiveness_score(signals),
        ]),
    ]
}

pub fn improvement_measurement_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.improvement_attempts == 0 {
        return 1.0;
    }
    1.0 - safe_ratio(
        signals.unmeasured_improvement_attempts as f64,
        signals.improvement_attempts as f64,
    )
    .min(1.0)
}

pub fn improvement_validation_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.improvement_attempts == 0 {
        return 1.0;
    }
    1.0 - safe_ratio(
        signals.unvalidated_improvement_attempts as f64,
        signals.improvement_attempts as f64,
    )
    .min(1.0)
}

pub fn improvement_effectiveness_score(signals: &TlogDeltaSignals) -> f64 {
    if signals.measured_improvement_attempts == 0 {
        return 1.0;
    }
    1.0 - safe_ratio(
        signals.regressed_improvement_attempts as f64,
        signals.measured_improvement_attempts as f64,
    )
    .min(1.0)
}

pub fn recovery_effectiveness_score(signals: &TlogDeltaSignals) -> f64 {
    let measured_recoveries = signals
        .recovery_attempts
        .saturating_add(signals.recovery_suppressed);
    if measured_recoveries == 0 {
        return 1.0;
    }
    safe_ratio(
        signals
            .recovery_successes
            .saturating_add(signals.recovery_loop_breaks) as f64,
        measured_recoveries as f64,
    )
    .min(1.0)
}

const RECOVERY_EVAL_WINDOW_EVENTS: usize = 32;

fn score_recovery_windows(records: &[crate::tlog::TlogRecord], signals: &mut TlogDeltaSignals) {
    for (idx, record) in records.iter().enumerate() {
        let Event::Effect {
            event:
                EffectEvent::RecoveryTriggered {
                    class,
                    support_count,
                    ..
                },
        } = &record.event
        else {
            continue;
        };

        signals.recovery_attempts = signals.recovery_attempts.saturating_add(1);
        signals.recovery_measurement_points = signals.recovery_measurement_points.saturating_add(1);

        let window_end = records.len().min(idx + 1 + RECOVERY_EVAL_WINDOW_EVENTS);
        let window = &records[idx + 1..window_end];
        if score_explicit_recovery_outcome(window, signals, class) {
            continue;
        }
        let progress_event_seen = window.iter().any(recovery_progress_event_seen);
        let failure_count_after = window
            .iter()
            .filter(|candidate| recovery_same_class_failure_seen(candidate, class))
            .count();
        if progress_event_seen && failure_count_after == 0 {
            signals.recovery_successes = signals.recovery_successes.saturating_add(1);
            if *support_count > failure_count_after {
                signals.recovery_loop_breaks = signals.recovery_loop_breaks.saturating_add(1);
            }
        } else {
            signals.recovery_failures = signals.recovery_failures.saturating_add(1);
            signals.recovery_regressions = signals.recovery_regressions.saturating_add(1);
        }
    }
}

fn score_suppressed_recovery_outcomes(
    records: &[crate::tlog::TlogRecord],
    signals: &mut TlogDeltaSignals,
) {
    for (idx, record) in records.iter().enumerate() {
        let Event::Effect {
            event: EffectEvent::RecoverySuppressed { class, .. },
        } = &record.event
        else {
            continue;
        };

        let window_end = records.len().min(idx + 1 + RECOVERY_EVAL_WINDOW_EVENTS);
        let window = &records[idx + 1..window_end];
        let Some((success, failure_count_before, failure_count_after, progress_event_seen)) =
            explicit_recovery_outcome(window, class)
        else {
            continue;
        };

        apply_recovery_outcome_score(
            signals,
            success,
            failure_count_before,
            failure_count_after,
            progress_event_seen,
        );
    }
}

fn score_explicit_recovery_outcome(
    window: &[crate::tlog::TlogRecord],
    signals: &mut TlogDeltaSignals,
    class: &str,
) -> bool {
    let Some((success, failure_count_before, failure_count_after, progress_event_seen)) =
        explicit_recovery_outcome(window, class)
    else {
        return false;
    };

    apply_recovery_outcome_score(
        signals,
        success,
        failure_count_before,
        failure_count_after,
        progress_event_seen,
    );
    true
}

fn explicit_recovery_outcome(
    window: &[crate::tlog::TlogRecord],
    class: &str,
) -> Option<(bool, usize, usize, bool)> {
    window.iter().find_map(|record| match &record.event {
        Event::Effect {
            event:
                EffectEvent::RecoveryOutcomeRecorded {
                    class: outcome_class,
                    success,
                    failure_count_before,
                    failure_count_after,
                    progress_event_seen,
                    ..
                },
        } if outcome_class == class => Some((
            *success,
            *failure_count_before,
            *failure_count_after,
            *progress_event_seen,
        )),
        _ => None,
    })
}

fn apply_recovery_outcome_score(
    signals: &mut TlogDeltaSignals,
    success: bool,
    failure_count_before: usize,
    failure_count_after: usize,
    progress_event_seen: bool,
) {
    if success {
        signals.recovery_successes = signals.recovery_successes.saturating_add(1);
        if failure_count_after < failure_count_before || progress_event_seen {
            signals.recovery_loop_breaks = signals.recovery_loop_breaks.saturating_add(1);
        }
    } else {
        signals.recovery_failures = signals.recovery_failures.saturating_add(1);
        signals.recovery_regressions = signals.recovery_regressions.saturating_add(1);
    }
}

fn recovery_progress_event_seen(record: &crate::tlog::TlogRecord) -> bool {
    match &record.event {
        Event::Control {
            event: ControlEvent::PlannerPendingSet { pending: true },
        } => true,
        Event::Control {
            event:
                ControlEvent::ScheduledPhaseSet {
                    phase: Some(phase),
                },
        } => phase == "planner",
        Event::Effect {
            event: EffectEvent::ActionResultRecorded { ok: true, .. },
        } => true,
        Event::Effect {
            event: EffectEvent::WorkspaceArtifactWriteApplied { .. },
        } => true,
        Event::Effect {
            event: EffectEvent::ProjectionRefreshRecoveryRequested { .. },
        } => true,
        Event::Effect {
            event:
                EffectEvent::EvalScoreRecorded {
                    delta_g,
                    promotion_eligible,
                    ..
                },
        } => *promotion_eligible || delta_g.map(|delta| delta >= 0.0).unwrap_or(false),
        _ => false,
    }
}

fn recovery_same_class_failure_seen(record: &crate::tlog::TlogRecord, class: &str) -> bool {
    match &record.event {
        Event::Effect {
            event:
                EffectEvent::InvariantViolation {
                    proposed_role,
                    reason,
                },
        } => proposed_role == "executor" && recovery_reason_matches_class(reason, class),
        Event::Effect {
            event:
                EffectEvent::RecoveryOutcomeRecorded {
                    class: outcome_class,
                    success: false,
                    ..
                },
        } => outcome_class == class,
        _ => false,
    }
}

fn recovery_reason_matches_class(reason: &str, class: &str) -> bool {
    let text = reason.to_ascii_lowercase();
    match class {
        "missing_target" => text.contains("does not exist") || text.contains("missing_target"),
        "invalid_route" => text.contains("invalid_route"),
        "llm_timeout" => text.contains("timeout"),
        "compile_error" => text.contains("cargo") || text.contains("compile"),
        "verification_failed" => text.contains("verification"),
        "projection_refresh_stalled" => {
            text.contains("projection refresh stalled")
                || text.contains("projection remains stale")
                || text.contains("stale latest.json")
                || text.contains("latest.json remains stale")
                || text.contains("long-running regeneration")
                || text.contains("refresh pid")
        }
        "invalid_schema" => text.contains("schema"),
        "step_limit_exceeded" => text.contains("step limit") || text.contains("step budget"),
        "checkpoint_runtime_divergence" => text.contains("checkpoint"),
        "reaction_only" => text.contains("reaction_only") || text.contains("prose-only"),
        _ => false,
    }
}

fn is_successful_improvement_action(action: &str, result: &str) -> bool {
    let action = action.trim();
    if action != "apply_patch" {
        return false;
    }
    let result_lower = result.to_ascii_lowercase();
    result_lower.contains("apply_patch ok") || result_lower.contains("patch applied successfully")
}

fn is_improvement_validation_result(action: &str, result: &str) -> bool {
    let action = action.trim();
    if !matches!(action, "apply_patch" | "run_command" | "cargo_test" | "batch") {
        return false;
    }

    let result_lower = result.to_ascii_lowercase();
    let mentions_validation_command = result_lower.contains("cargo check")
        || result_lower.contains("cargo test")
        || result_lower.contains("cargo build");
    let validation_succeeded = result_lower.contains("run_command ok")
        || result_lower.contains("status: ok")
        || result_lower.contains("cargo check ok")
        || result_lower.contains("cargo build ok")
        || result_lower.contains("cargo test ok")
        || result_lower.contains("test result: ok");

    mentions_validation_command && validation_succeeded
}

fn eval_measurement_regressed(delta_g: Option<f64>, promotion_eligible: bool) -> bool {
    match delta_g {
        Some(delta) => delta < -0.001 || !promotion_eligible,
        None => false,
    }
}

fn tlog_event_kind(event: &Event) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| {
            value
                .get("event")
                .and_then(|inner| inner.get("kind"))
                .and_then(|kind| kind.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn is_actionable_lag_kind(kind: &str) -> bool {
    !matches!(
        kind,
        "llm_turn_output"
            | "llm_error_boundary"
            | "eval_score_recorded"
            | "orchestrator_idle_pulse"
            | "orchestrator_mode_set"
    )
}

fn load_structural_invariant_coverage(workspace: &Path) -> StructuralInvariantCoverage {
    let graph_path = crate::semantic_contract::graph_path(workspace);
    let graph_text = std::fs::read_to_string(graph_path).unwrap_or_default();
    let invariant_text = std::fs::read_to_string(
        workspace
            .join("agent_state")
            .join("enforced_invariants.json"),
    )
    .unwrap_or_default();
    structural_invariant_coverage_from_text(&graph_text, &invariant_text)
}

fn structural_invariant_coverage_from_text(
    graph_text: &str,
    invariant_text: &str,
) -> StructuralInvariantCoverage {
    let graph_lower = graph_text.to_ascii_lowercase();
    let invariant_lower = invariant_text.to_ascii_lowercase();
    let risks = structural_risks_from_graph_text(&graph_lower);
    let mut covered = 0usize;
    let mut missing = Vec::new();

    for risk in &risks {
        if invariant_text_covers_risk(&invariant_lower, risk) {
            covered += 1;
        } else {
            missing.push(risk.name.to_string());
        }
    }

    let score = if risks.is_empty() {
        1.0
    } else {
        covered as f64 / risks.len() as f64
    };

    StructuralInvariantCoverage {
        graph_risk_count: risks.len(),
        invariant_covered_count: covered,
        missing_invariant_count: missing.len(),
        score,
        missing,
    }
}

struct StructuralRisk {
    name: &'static str,
    graph_needles: &'static [&'static str],
    invariant_needles: &'static [&'static str],
}

fn structural_risk_catalog() -> Vec<StructuralRisk> {
    vec![
        StructuralRisk {
            name: "plan_mutation_goes_through_plan_tool",
            graph_needles: &["plan.json", "master_plan_file"],
            invariant_needles: &["plan", "plan tool"],
        },
        StructuralRisk {
            name: "canonical_writer_single_tlog_authority",
            graph_needles: &["tlog::tlog::append", "canonicalwriter"],
            invariant_needles: &["canonical writer", "single authority", "tlog"],
        },
        StructuralRisk {
            name: "patch_requires_verification_gate",
            graph_needles: &["apply_patch", "cargo_test"],
            invariant_needles: &["patch", "cargo", "test", "verification"],
        },
        StructuralRisk {
            name: "checkpoint_commit_requires_verified_gate_if_rust_changed",
            graph_needles: &[
                "checkpoint_build_succeeded",
                "git commit",
                "rust_patch_verification",
            ],
            invariant_needles: &["checkpoint", "commit", "rust", "cargo", "verification"],
        },
        StructuralRisk {
            name: "issues_projection_only",
            graph_needles: &["issues.json", "persist_issues_projection"],
            invariant_needles: &["issues", "projection"],
        },
        StructuralRisk {
            name: "executor_wake_requires_claimable_lane",
            graph_needles: &["wake_signal", "lane_in_progress"],
            invariant_needles: &["executor", "wake", "lane"],
        },
    ]
}

fn structural_risks_from_graph_text(graph_lower: &str) -> Vec<StructuralRisk> {
    structural_risk_catalog()
        .into_iter()
        .filter(|risk| {
            risk.graph_needles
                .iter()
                .all(|needle| graph_lower.contains(needle))
        })
        .collect()
}

fn invariant_text_covers_risk(invariant_lower: &str, risk: &StructuralRisk) -> bool {
    risk.invariant_needles
        .iter()
        .all(|needle| invariant_lower.contains(needle))
}

/// Intent: canonical_read
/// Resource: issues_index
/// Inputs: &std::path::Path
/// Outputs: (usize, usize, usize)
/// Effects: reads issue state from workspace
/// Forbidden: mutation
/// Invariants: returns counts for open issues, repeated open issue titles, and high/critical open issues
/// Failure: missing issue data is handled by load_issues_file defaults
/// Provenance: rustc:facts + rustc:docstring
/// Returns `(open_issues, repeated_open_issues, high_priority_open_issues)`.
pub fn load_issue_counts(workspace: &Path) -> (usize, usize, usize) {
    let issues = crate::issues::load_issues_file(workspace);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for issue in &issues.issues {
        if is_open(issue) {
            *counts.entry(issue.title.as_str()).or_insert(0) += 1;
        }
    }
    let open_issues = issues.issues.iter().filter(|issue| is_open(issue)).count();
    let repeated_open_issues = counts.values().filter(|count| **count > 1).copied().sum();
    let high_priority_open_issues = issues
        .issues
        .iter()
        .filter(|issue| {
            is_open(issue)
                && matches!(
                    issue.priority.trim().to_lowercase().as_str(),
                    "high" | "critical"
                )
        })
        .count();
    (open_issues, repeated_open_issues, high_priority_open_issues)
}

pub fn diagnostics_repair_pressure(report: &DiagnosticsReport) -> f64 {
    let total = report.ranked_failures.len();
    if total == 0 {
        return 0.0;
    }
    let high_impact = report
        .ranked_failures
        .iter()
        .filter(|f| {
            matches!(
                f.impact,
                crate::reports::Impact::High | crate::reports::Impact::Critical
            )
        })
        .count();
    clamp_unit(high_impact as f64 / total as f64)
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: objectives::ObjectivesFile
/// Effects: fs_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_objectives_file(workspace: &Path) -> crate::objectives::ObjectivesFile {
    let path = crate::objectives::resolve_objectives_path(workspace);
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: (usize, usize)
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_task_counts(workspace: &Path) -> (usize, usize) {
    let path = workspace.join(crate::constants::MASTER_PLAN_FILE);
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (0, 0);
    };
    let tasks = value
        .get("tasks")
        .and_then(|tasks| tasks.as_array())
        .cloned()
        .unwrap_or_default();
    let completed = tasks
        .iter()
        .filter(|task| {
            task.get("status")
                .and_then(|status| status.as_str())
                .map(|status| matches!(status, "done" | "complete" | "completed"))
                .unwrap_or(false)
        })
        .count();
    (completed, tasks.len())
}

/// Intent: diagnostic_scan
/// Resource: error
/// Inputs: ()
/// Outputs: reports::DiagnosticsReport
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn empty_diagnostics_report() -> DiagnosticsReport {
    DiagnosticsReport {
        status: "unknown".to_string(),
        inputs_scanned: Vec::new(),
        ranked_failures: Vec::new(),
        planner_handoff: Vec::new(),
    }
}

fn is_open(issue: &Issue) -> bool {
    !matches!(issue.status.as_str(), "resolved" | "wontfix" | "closed")
}

fn safe_ratio(n: f64, d: f64) -> f64 {
    if d <= 0.0 {
        0.0
    } else {
        n / d
    }
}

fn clamp_unit(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

fn geometric_score(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values
        .iter()
        .map(|value| value.clamp(0.001, 1.0))
        .product::<f64>()
        .powf(1.0 / values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_eval_input() -> EvalInput {
        EvalInput { objectives_completed: 1, objectives_total: 1, violations: ViolationsReport { status: "ok".to_string(), summary: String::new(), violations: Vec::new() }, completed_tasks: 1, total_tasks: 1, open_issues: 0, repeated_open_issues: 0, high_priority_open_issues: 0, diagnostics: empty_diagnostics_report(), semantic_fn_total: 0, semantic_fn_with_any_error: 0, semantic_fn_error_rate: 0.0, semantic_fn_intent_classified: 0, semantic_fn_low_confidence: 0, semantic_fn_intent_coverage: 1.0, semantic_fn_low_confidence_rate: 0.0, structural_invariant_coverage: StructuralInvariantCoverage { score: 1.0, ..StructuralInvariantCoverage::default() }, tlog_delta_signals: TlogDeltaSignals { score: 1.0, improvement_measurement_score: 1.0, improvement_validation_score: 1.0, improvement_effectiveness_score: 1.0, recovery_effectiveness_score: 1.0, ..TlogDeltaSignals::default() } }
    }

    #[test]
    fn geometric_mean_zero_floor_prevents_score_collapse() {
        let vector = EvaluationVector {
            objective_progress: 1.0,
            safety: 1.0,
            task_velocity: 0.0,
            issue_health: 1.0,
            semantic_contract: 1.0,
            structural_invariant_coverage: 1.0,
            canonical_delta_health: 1.0,
            improvement_measurement: 1.0,
            improvement_validation: 1.0,
            improvement_effectiveness: 1.0,
            recovery_effectiveness: 1.0,
        };

        assert!(vector.geometric_mean_like_score() > 0.0);
    }

    #[test]
    fn task_velocity_is_zero_when_no_tasks_done() {
        assert_eq!(task_velocity_score(0, 3), 0.0);
    }

    #[test]
    fn compute_eval_enforces_hard_semantic_and_totalization_thresholds() {
        let mut input = base_eval_input();
        input.semantic_fn_total = 10;
        input.semantic_fn_with_any_error = 1;
        input.semantic_fn_intent_classified = 6;
        input.semantic_fn_low_confidence = 3;

        let snapshot = compute_eval(&input);

        assert_eq!(snapshot.semantic_fn_totalized, 9);
        assert!((snapshot.semantic_fn_totalization_coverage - 0.9).abs() < 0.000_001);
        assert!(!snapshot.eval_enforcement.passed);
        assert!(snapshot
            .eval_enforcement
            .violations
            .iter()
            .any(|v| v.contains("semantic_errors=1/10")));
        assert!(snapshot
            .eval_enforcement
            .violations
            .iter()
            .any(|v| v.contains("intent_totalization=9/10")));
    }

    #[test]
    fn compute_eval_treats_totalized_low_confidence_as_warning_not_hard_violation() {
        let mut input = base_eval_input();
        input.semantic_fn_total = 10;
        input.semantic_fn_with_any_error = 0;
        input.semantic_fn_intent_classified = 6;
        input.semantic_fn_intent_coverage = 0.6;
        input.semantic_fn_low_confidence = 4;

        let snapshot = compute_eval(&input);

        assert_eq!(snapshot.semantic_fn_totalized, 10);
        assert!((snapshot.semantic_fn_totalization_coverage - 1.0).abs() < 0.000_001);
        assert!(snapshot.eval_enforcement.passed);
        assert_eq!(snapshot.eval_enforcement.violation_count, 0);
        assert!(snapshot.eval_enforcement.warning_count > 0);
        assert!(snapshot
            .eval_enforcement
            .warnings
            .iter()
            .any(|w| w.contains("low_confidence_rate=0.4000")));
    }

    #[test]
    fn issue_health_drops_when_same_issue_reopened() {
        let issues = IssuesFile {
            version: 1,
            issues: vec![
                Issue {
                    title: "loop".to_string(),
                    status: "open".to_string(),
                    ..Issue::default()
                },
                Issue {
                    title: "loop".to_string(),
                    status: "in_progress".to_string(),
                    ..Issue::default()
                },
                Issue {
                    title: "fixed".to_string(),
                    status: "resolved".to_string(),
                    ..Issue::default()
                },
            ],
        };

        let open_issues = issues.issues.iter().filter(|issue| is_open(issue)).count();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for issue in &issues.issues {
            if is_open(issue) {
                *counts.entry(issue.title.as_str()).or_insert(0) += 1;
            }
        }
        let repeated_open_issues = counts.values().filter(|count| **count > 1).copied().sum();
        assert!(issue_health_score(open_issues, repeated_open_issues) < 1.0);
    }

    #[test]
    fn safety_score_decreases_with_critical_violations() {
        let mut violations = ViolationsReport {
            status: "bad".to_string(),
            summary: String::new(),
            violations: Vec::new(),
        };
        let no_violation_score = safety_score(&violations);
        violations.violations.push(crate::reports::Violation {
            id: "v1".to_string(),
            title: "critical".to_string(),
            severity: crate::reports::Severity::Critical,
            evidence: vec!["e".to_string()],
            issue: "i".to_string(),
            impact: "imp".to_string(),
            required_fix: vec!["f".to_string()],
            files: vec![],
            freshness_status: String::new(),
            stale_reason: String::new(),
            validated_from: vec![],
            evidence_receipts: vec![],
            evidence_hashes: vec![],
            last_validated_ms: 0,
        });
        assert!(safety_score(&violations) < no_violation_score);
    }

    #[test]
    fn structural_invariant_coverage_reports_missing_graph_risk() {
        let graph = r#"
            {"nodes":{
              "a":{"path":"tools::handle_apply_patch_action"},
              "b":{"path":"tools::handle_cargo_test_action"}
            }}
        "#;
        let invariants = r#"{"invariants":[]}"#;

        let coverage = structural_invariant_coverage_from_text(graph, invariants);

        assert_eq!(coverage.graph_risk_count, 1);
        assert_eq!(coverage.invariant_covered_count, 0);
        assert_eq!(coverage.missing_invariant_count, 1);
        assert_eq!(
            coverage.missing,
            vec!["patch_requires_verification_gate".to_string()]
        );
        assert_eq!(coverage.score, 0.0);
    }

    #[test]
    fn structural_invariant_coverage_counts_matching_invariant() {
        let graph = r#"
            {"nodes":{
              "a":{"path":"tools::handle_apply_patch_action"},
              "b":{"path":"tools::handle_cargo_test_action"}
            }}
        "#;
        let invariants = r#"
            {"invariants":[{"predicate_text":
              "Every patch must be followed by cargo test verification before completion."
            }]}
        "#;

        let coverage = structural_invariant_coverage_from_text(graph, invariants);

        assert_eq!(coverage.graph_risk_count, 1);
        assert_eq!(coverage.invariant_covered_count, 1);
        assert_eq!(coverage.missing_invariant_count, 0);
        assert_eq!(coverage.score, 1.0);
    }

    #[test]
    fn structural_invariant_coverage_requires_checkpoint_commit_gate() {
        let graph = r#"
            {"nodes":{
              "a":{"path":"supervisor::checkpoint_build_succeeded"},
              "b":{"path":"supervisor::commit_and_push_checkpoint"}
            },
             "edges":[{"label":"git commit"},{"label":"rust_patch_verification_requested"}]}
        "#;
        let invariants = r#"{"invariants":[]}"#;

        let coverage = structural_invariant_coverage_from_text(graph, invariants);

        assert_eq!(
            coverage.missing,
            vec!["checkpoint_commit_requires_verified_gate_if_rust_changed".to_string()]
        );
        assert_eq!(coverage.score, 0.0);
    }

    #[test]
    fn tlog_delta_invariants_detect_missing_action_result() {
        let records = vec![crate::tlog::TlogRecord {
            seq: 1,
            ts_ms: 1,
            event: Event::effect(EffectEvent::LlmTurnOutput {
                tab_id: None,
                turn_id: None,
                role: "planner".to_string(),
                step: 1,
                command_id: "cmd-1".to_string(),
                endpoint_id: "ep".to_string(),
                response_bytes: 10,
                response_hash: "h".to_string(),
                action_kind: Some("plan".to_string()),
                raw: "{}".to_string(),
            }),
        }];

        let signals = evaluate_tlog_delta_invariants(&records);

        assert_eq!(signals.llm_action_outputs, 1);
        assert_eq!(signals.missing_action_results, 1);
        assert!(signals.score < 1.0);
    }

    #[test]
    fn tlog_delta_invariants_reward_closed_action_delta() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::LlmTurnOutput {
                    tab_id: None,
                    turn_id: None,
                    role: "planner".to_string(),
                    step: 1,
                    command_id: "cmd-1".to_string(),
                    endpoint_id: "ep".to_string(),
                    response_bytes: 10,
                    response_hash: "h".to_string(),
                    action_kind: Some("plan".to_string()),
                    raw: "{}".to_string(),
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::effect(EffectEvent::ActionResultRecorded {
                    role: "planner".to_string(),
                    step: 1,
                    command_id: "cmd-1".to_string(),
                    action_kind: "plan".to_string(),
                    task_id: None,
                    objective_id: None,
                    ok: true,
                    result_bytes: 2,
                    result_hash: "r".to_string(),
                    result: "ok".to_string(),
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);

        assert_eq!(signals.missing_action_results, 0);
        assert_eq!(signals.score, 1.0);
    }

    #[test]
    fn tlog_delta_invariants_identifies_projection_as_actionable_lag() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1_000,
                event: Event::effect(EffectEvent::LlmTurnInput {
                    tab_id: None,
                    turn_id: None,
                    role: "planner".to_string(),
                    agent_type: "planner".to_string(),
                    step: 1,
                    command_id: "cmd-1".to_string(),
                    endpoint_id: "ep".to_string(),
                    prompt_hash: "p".to_string(),
                    prompt_bytes: 10,
                    role_schema_bytes: 2,
                    submit_only: false,
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 3_500,
                event: Event::effect(EffectEvent::IssuesProjectionRecorded {
                    path: "agent_state/ISSUES.json".to_string(),
                    hash: "h".to_string(),
                    issue_count: 10,
                    open_count: 10,
                    bytes: 1_000_000,
                    issue_fingerprints_hash: "f".to_string(),
                    changed_issue_count: 5,
                    changed_issue_ids: Vec::new(),
                    status_counts: std::collections::BTreeMap::new(),
                }),
            },
            crate::tlog::TlogRecord {
                seq: 3,
                ts_ms: 10_000,
                event: Event::effect(EffectEvent::LlmTurnOutput {
                    tab_id: None,
                    turn_id: None,
                    role: "planner".to_string(),
                    step: 1,
                    command_id: "cmd-1".to_string(),
                    endpoint_id: "ep".to_string(),
                    response_bytes: 10,
                    response_hash: "r".to_string(),
                    action_kind: None,
                    raw: "{}".to_string(),
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);

        assert_eq!(signals.lag_gap_count, 2);
        assert_eq!(signals.lag_total_ms, 9_000);
        assert_eq!(signals.max_event_gap_ms, 6_500);
        assert_eq!(signals.issues_projection_lag_count, 1);
        assert_eq!(signals.issues_projection_lag_ms, 2_500);
        assert_eq!(signals.actionable_lag_gap_count, 1);
        assert_eq!(signals.actionable_lag_total_ms, 2_500);
        assert_eq!(
            signals.dominant_actionable_lag_kind,
            "issues_projection_recorded"
        );
        assert_eq!(signals.dominant_actionable_lag_kind_ms, 2_500);
        assert!(signals.score < 1.0);
    }

    #[test]
    fn tlog_delta_invariants_penalize_unsafe_checkpoint_attempts() {
        let records = vec![crate::tlog::TlogRecord {
            seq: 1,
            ts_ms: 1,
            event: Event::effect(EffectEvent::GitCheckpointBlocked {
                reason: "orchestrate-deferred-update-timeout".to_string(),
                risk: "commit_push_requires_verified_gate_if_rust_changed".to_string(),
                verification_requested: false,
                rust_sensitive_changes: true,
                changed_paths: vec!["src/supervisor.rs".to_string()],
                required_gate:
                    "cargo check --workspace && cargo test --workspace && cargo build --workspace"
                        .to_string(),
                signature: "sig".to_string(),
            }),
        }];

        let signals = evaluate_tlog_delta_invariants(&records);

        assert_eq!(signals.git_checkpoint_blocked, 1);
        assert_eq!(signals.unsafe_checkpoint_attempts, 1);
        assert!(signals.score < 1.0);
    }

    #[test]
    fn tlog_delta_invariants_count_prompt_truncations() {
        let records = vec![crate::tlog::TlogRecord {
            seq: 1,
            ts_ms: 1,
            event: Event::effect(EffectEvent::PromptTruncationRecorded {
                role: "planner".to_string(),
                prompt_kind: "main".to_string(),
                step: 1,
                command_id: "cmd".to_string(),
                endpoint_id: "ep".to_string(),
                heading: "section".to_string(),
                raw_bytes: 120_000,
                kept_bytes: 80_000,
                dropped_bytes: 40_000,
                policy: "drop-oldest".to_string(),
                body_hash: "hash".to_string(),
            }),
        }];

        let signals = evaluate_tlog_delta_invariants(&records);

        assert_eq!(signals.prompt_truncations, 1);
        assert_eq!(signals.prompt_truncation_dropped_bytes, 40_000);
        assert!(signals.score < 1.0);
    }

    #[test]
    fn tlog_delta_invariants_require_eval_after_improvement_action() {
        let mut records = vec![crate::tlog::TlogRecord {
            seq: 1,
            ts_ms: 1,
            event: Event::effect(EffectEvent::PostRestartResultRecorded {
                role: "executor".to_string(),
                action: "apply_patch".to_string(),
                result: "apply_patch ok\nPatch applied successfully.".to_string(),
                step: 1,
                tab_id: None,
                turn_id: None,
                endpoint_id: "ep".to_string(),
                restart_kind: "normal".to_string(),
                signature: "sig".to_string(),
            }),
        }];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.improvement_attempts, 1);
        assert_eq!(signals.unmeasured_improvement_attempts, 1);
        assert_eq!(signals.improvement_measurement_score, 0.0);

        records.push(crate::tlog::TlogRecord {
            seq: 2,
            ts_ms: 2,
            event: Event::effect(EffectEvent::EvalScoreRecorded {
                generated_at_ms: 2,
                overall_score: 0.9,
                delta_g: Some(0.1),
                promotion_eligible: true,
                objective_progress: 1.0,
                safety: 1.0,
                task_velocity: 1.0,
                issue_health: 1.0,
                semantic_contract: 1.0,
                structural_invariant_coverage: 1.0,
                canonical_delta_health: 1.0,
                improvement_measurement: 1.0,
                improvement_validation: 1.0,
                improvement_effectiveness: 1.0,
                recovery_effectiveness: 1.0,
                improvement_attempts: 1,
                measured_improvement_attempts: 1,
                unmeasured_improvement_attempts: 0,
                validated_improvement_attempts: 1,
                unvalidated_improvement_attempts: 0,
                non_regressed_improvement_attempts: 1,
                regressed_improvement_attempts: 0,
                eval_measurement_points: 1,
                measurement_regressions: 0,
                recovery_attempts: 0,
                recovery_successes: 0,
                recovery_failures: 0,
                recovery_suppressed: 0,
                recovery_loop_breaks: 0,
                recovery_regressions: 0,
                recovery_measurement_points: 0,
                tlog_lag_total_ms: 0,
                tlog_actionable_lag_total_ms: 0,
                tlog_dominant_actionable_lag_kind: String::new(),
                tlog_dominant_actionable_lag_kind_ms: 0,
                issues_projection_lag_ms: 0,
                tlog_dominant_payload_kind: String::new(),
                tlog_dominant_payload_kind_bytes: 0,
                last_plan_text_payload_bytes: 0,
                last_executor_diff_payload_bytes: 0,
                tlog_git_checkpoint_blocked: 0,
                tlog_unsafe_checkpoint_attempts: 0,
                diagnostics_repair_pressure: 0.0,
                semantic_fn_error_rate: 0.0,
                semantic_fn_total: 0,
                semantic_fn_with_any_error: 0,
                semantic_fn_intent_classified: 0,
                semantic_fn_totalized: 0,
                semantic_fn_totalization_coverage: 1.0,
                semantic_fn_low_confidence: 0,
                semantic_fn_intent_coverage: 1.0,
                semantic_fn_low_confidence_rate: 0.0,
                eval_enforcement_passed: true,
                eval_enforcement_violation_count: 0,
                eval_enforcement_violations: Vec::new(),
                eval_enforcement_warning_count: 0,
                eval_enforcement_warnings: Vec::new(),
                tlog_prompt_truncation_count: 0,
                tlog_prompt_truncation_dropped_bytes: 0,
            }),
        });

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.measured_improvement_attempts, 1);
        assert_eq!(signals.unmeasured_improvement_attempts, 0);
        assert_eq!(signals.non_regressed_improvement_attempts, 1);
        assert_eq!(signals.regressed_improvement_attempts, 0);
        assert_eq!(signals.improvement_measurement_score, 1.0);
        assert_eq!(signals.improvement_effectiveness_score, 1.0);
    }

    #[test]
    fn tlog_delta_invariants_require_validation_after_improvement_action() {
        let mut records = vec![crate::tlog::TlogRecord {
            seq: 1,
            ts_ms: 1,
            event: Event::effect(EffectEvent::PostRestartResultRecorded {
                role: "executor".to_string(),
                action: "apply_patch".to_string(),
                result: "apply_patch ok\nPatch applied successfully.".to_string(),
                step: 1,
                tab_id: None,
                turn_id: None,
                endpoint_id: "ep".to_string(),
                restart_kind: "normal".to_string(),
                signature: "sig".to_string(),
            }),
        }];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.improvement_attempts, 1);
        assert_eq!(signals.validated_improvement_attempts, 0);
        assert_eq!(signals.unvalidated_improvement_attempts, 1);
        assert_eq!(signals.improvement_validation_score, 0.0);

        records.push(crate::tlog::TlogRecord {
            seq: 2,
            ts_ms: 2,
            event: Event::effect(EffectEvent::PostRestartResultRecorded {
                role: "executor".to_string(),
                action: "run_command".to_string(),
                result: "run_command ok\ncommand: cargo check\nFinished dev profile".to_string(),
                step: 2,
                tab_id: None,
                turn_id: None,
                endpoint_id: "ep".to_string(),
                restart_kind: "normal".to_string(),
                signature: "sig2".to_string(),
            }),
        });

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.validated_improvement_attempts, 1);
        assert_eq!(signals.unvalidated_improvement_attempts, 0);
        assert_eq!(signals.improvement_validation_score, 1.0);
    }

    #[test]
    fn tlog_delta_invariants_penalize_regressed_measured_improvement() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::PostRestartResultRecorded {
                    role: "executor".to_string(),
                    action: "apply_patch".to_string(),
                    result: "apply_patch ok\nPatch applied successfully.".to_string(),
                    step: 1,
                    tab_id: None,
                    turn_id: None,
                    endpoint_id: "ep".to_string(),
                    restart_kind: "normal".to_string(),
                    signature: "sig".to_string(),
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::effect(EffectEvent::EvalScoreRecorded {
                    generated_at_ms: 2,
                    overall_score: 0.8,
                    delta_g: Some(-0.1),
                    promotion_eligible: false,
                    objective_progress: 1.0,
                    safety: 1.0,
                    task_velocity: 1.0,
                    issue_health: 1.0,
                    semantic_contract: 1.0,
                    structural_invariant_coverage: 1.0,
                    canonical_delta_health: 1.0,
                    improvement_measurement: 1.0,
                    improvement_validation: 1.0,
                    improvement_effectiveness: 0.0,
                    recovery_effectiveness: 1.0,
                    improvement_attempts: 1,
                    measured_improvement_attempts: 1,
                    unmeasured_improvement_attempts: 0,
                    validated_improvement_attempts: 1,
                    unvalidated_improvement_attempts: 0,
                    non_regressed_improvement_attempts: 0,
                    regressed_improvement_attempts: 1,
                    eval_measurement_points: 1,
                    measurement_regressions: 1,
                    recovery_attempts: 0,
                    recovery_successes: 0,
                    recovery_failures: 0,
                    recovery_suppressed: 0,
                    recovery_loop_breaks: 0,
                    recovery_regressions: 0,
                    recovery_measurement_points: 0,
                    tlog_lag_total_ms: 0,
                    tlog_actionable_lag_total_ms: 0,
                    tlog_dominant_actionable_lag_kind: String::new(),
                    tlog_dominant_actionable_lag_kind_ms: 0,
                    issues_projection_lag_ms: 0,
                    tlog_dominant_payload_kind: String::new(),
                    tlog_dominant_payload_kind_bytes: 0,
                    last_plan_text_payload_bytes: 0,
                    last_executor_diff_payload_bytes: 0,
                    tlog_git_checkpoint_blocked: 0,
                    tlog_unsafe_checkpoint_attempts: 0,
                    diagnostics_repair_pressure: 0.0,
                    semantic_fn_error_rate: 0.0,
                    semantic_fn_total: 0,
                    semantic_fn_with_any_error: 0,
                    semantic_fn_intent_classified: 0,
                    semantic_fn_totalized: 0,
                    semantic_fn_totalization_coverage: 1.0,
                    semantic_fn_low_confidence: 0,
                    semantic_fn_intent_coverage: 1.0,
                    semantic_fn_low_confidence_rate: 0.0,
                    eval_enforcement_passed: true,
                    eval_enforcement_violation_count: 0,
                    eval_enforcement_violations: Vec::new(),
                    eval_enforcement_warning_count: 0,
                    eval_enforcement_warnings: Vec::new(),
                    tlog_prompt_truncation_count: 0,
                    tlog_prompt_truncation_dropped_bytes: 0,
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.measured_improvement_attempts, 1);
        assert_eq!(signals.non_regressed_improvement_attempts, 0);
        assert_eq!(signals.regressed_improvement_attempts, 1);
        assert_eq!(signals.improvement_effectiveness_score, 0.0);
        assert!(signals.score < 1.0);
    }

    #[test]
    fn recovery_trigger_followed_by_progress_counts_success() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::RecoveryTriggered {
                    generated_at_ms: 1,
                    class: "missing_target".to_string(),
                    policy: "clear_executor_and_wake_planner".to_string(),
                    reason: "path does not exist".to_string(),
                    support_count: 2,
                    threshold: 2,
                    window_ms: 300_000,
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::control(ControlEvent::PlannerPendingSet { pending: true }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.recovery_attempts, 1);
        assert_eq!(signals.recovery_successes, 1);
        assert_eq!(signals.recovery_failures, 0);
        assert_eq!(signals.recovery_effectiveness_score, 1.0);
    }

    #[test]
    fn recovery_trigger_followed_by_same_failure_counts_failure() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::RecoveryTriggered {
                    generated_at_ms: 1,
                    class: "missing_target".to_string(),
                    policy: "clear_executor_and_wake_planner".to_string(),
                    reason: "path does not exist".to_string(),
                    support_count: 2,
                    threshold: 2,
                    window_ms: 300_000,
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::effect(EffectEvent::InvariantViolation {
                    proposed_role: "executor".to_string(),
                    reason: "path does not exist".to_string(),
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.recovery_attempts, 1);
        assert_eq!(signals.recovery_successes, 0);
        assert_eq!(signals.recovery_failures, 1);
        assert_eq!(signals.recovery_effectiveness_score, 0.0);
    }

    #[test]
    fn projection_refresh_request_counts_as_recovery_progress() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::RecoveryTriggered {
                    generated_at_ms: 1,
                    class: "projection_refresh_stalled".to_string(),
                    policy: "refresh_projection_bounded".to_string(),
                    reason: "refresh pid is still running and latest.json remains stale".to_string(),
                    support_count: 1,
                    threshold: 1,
                    window_ms: 300_000,
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::effect(EffectEvent::ProjectionRefreshRecoveryRequested {
                    generated_at_ms: 2,
                    class: "projection_refresh_stalled".to_string(),
                    policy: "refresh_projection_bounded".to_string(),
                    projection: "agent_state/reports/complexity/latest.json".to_string(),
                    command: "timeout 30s cargo run -p canon-mini-agent --bin canon-generate-issues -- --workspace /tmp/ws --complexity-report-only".to_string(),
                    timeout_ms: 30_000,
                    reason: "refresh pid is still running and latest.json remains stale".to_string(),
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.recovery_attempts, 1);
        assert_eq!(signals.recovery_successes, 1);
        assert_eq!(signals.recovery_failures, 0);
        assert_eq!(signals.recovery_effectiveness_score, 1.0);
    }

    #[test]
    fn explicit_recovery_outcome_counts_success_without_heuristic_window() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::RecoveryTriggered {
                    generated_at_ms: 1,
                    class: "missing_target".to_string(),
                    policy: "clear_executor_and_wake_planner".to_string(),
                    reason: "path does not exist".to_string(),
                    support_count: 2,
                    threshold: 2,
                    window_ms: 300_000,
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::effect(EffectEvent::RecoveryOutcomeRecorded {
                    generated_at_ms: 2,
                    class: "missing_target".to_string(),
                    policy: "clear_executor_and_wake_planner".to_string(),
                    success: true,
                    failure_count_before: 2,
                    failure_count_after: 0,
                    progress_event_seen: true,
                    eval_window_events: 0,
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.recovery_attempts, 1);
        assert_eq!(signals.recovery_successes, 1);
        assert_eq!(signals.recovery_failures, 0);
        assert_eq!(signals.recovery_loop_breaks, 1);
        assert_eq!(signals.recovery_effectiveness_score, 1.0);
    }

    #[test]
    fn recovery_suppression_counts_as_measured_non_success() {
        let records = vec![crate::tlog::TlogRecord {
            seq: 1,
            ts_ms: 1,
            event: Event::effect(EffectEvent::RecoverySuppressed {
                generated_at_ms: 1,
                class: "missing_target".to_string(),
                policy: "clear_executor_and_wake_planner".to_string(),
                reason: "path does not exist".to_string(),
                suppression_reason:
                    "retry_budget_exhausted attempt_count=2 max_attempts=2".to_string(),
            }),
        }];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.recovery_attempts, 0);
        assert_eq!(signals.recovery_suppressed, 1);
        assert_eq!(signals.recovery_successes, 0);
        assert_eq!(signals.recovery_effectiveness_score, 0.0);
    }

    #[test]
    fn recovery_suppression_with_terminal_cleanup_counts_success() {
        let records = vec![
            crate::tlog::TlogRecord {
                seq: 1,
                ts_ms: 1,
                event: Event::effect(EffectEvent::RecoverySuppressed {
                    generated_at_ms: 1,
                    class: "missing_target".to_string(),
                    policy: "clear_executor_and_wake_planner".to_string(),
                    reason: "path does not exist".to_string(),
                    suppression_reason:
                        "retry_budget_exhausted attempt_count=2 max_attempts=2".to_string(),
                }),
            },
            crate::tlog::TlogRecord {
                seq: 2,
                ts_ms: 2,
                event: Event::effect(EffectEvent::RecoveryOutcomeRecorded {
                    generated_at_ms: 2,
                    class: "missing_target".to_string(),
                    policy: "clear_executor_and_wake_planner".to_string(),
                    success: true,
                    failure_count_before: 2,
                    failure_count_after: 0,
                    progress_event_seen: true,
                    eval_window_events: 4,
                }),
            },
        ];

        let signals = evaluate_tlog_delta_invariants(&records);
        assert_eq!(signals.recovery_attempts, 0);
        assert_eq!(signals.recovery_suppressed, 1);
        assert_eq!(signals.recovery_successes, 1);
        assert_eq!(signals.recovery_failures, 0);
        assert_eq!(signals.recovery_loop_breaks, 1);
        assert_eq!(signals.recovery_effectiveness_score, 1.0);
    }
}

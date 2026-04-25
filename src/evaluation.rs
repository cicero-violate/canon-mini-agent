use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::events::{EffectEvent, Event};
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
pub struct EvaluationVector {
    pub objective_progress: f64,
    pub safety: f64,
    pub task_velocity: f64,
    pub issue_health: f64,
    pub semantic_contract: f64,
    pub structural_invariant_coverage: f64,
    pub canonical_delta_health: f64,
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
    pub structural_invariant_coverage: StructuralInvariantCoverage,
    pub tlog_delta_signals: TlogDeltaSignals,
    pub vector: EvaluationVector,
}

impl EvaluationWorkspaceSnapshot {
    pub fn overall_score(&self) -> f64 {
        let base = self.vector.geometric_mean_like_score();
        let repair_penalty = 0.25 * self.diagnostics_repair_pressure;
        clamp_unit(base * (1.0 - repair_penalty))
    }
}

/// Pure kernel entry point — no I/O.  All inputs are pre-loaded by the caller.
pub fn compute_eval(input: &EvalInput) -> EvaluationWorkspaceSnapshot {
    let mut vector = evaluate_repo_state(
        input.objectives_completed,
        input.objectives_total,
        &input.violations,
        input.completed_tasks,
        input.total_tasks,
        input.open_issues,
        input.repeated_open_issues,
        (1.0 - input.semantic_fn_error_rate).clamp(0.0, 1.0),
        input.structural_invariant_coverage.score,
        input.tlog_delta_signals.score,
    );
    // Fold semantic error rate into safety so a clean build alone cannot produce safety = 1.0.
    vector.safety = clamp_unit(vector.safety * (1.0 - 0.3 * input.semantic_fn_error_rate));
    EvaluationWorkspaceSnapshot {
        objectives_completed: input.objectives_completed,
        objectives_total: input.objectives_total,
        completed_tasks: input.completed_tasks,
        total_tasks: input.total_tasks,
        open_issues: input.open_issues,
        repeated_open_issues: input.repeated_open_issues,
        diagnostics_repair_pressure: diagnostics_repair_pressure_with_issues(
            &input.diagnostics,
            input.high_priority_open_issues,
        ),
        semantic_fn_total: input.semantic_fn_total,
        semantic_fn_with_any_error: input.semantic_fn_with_any_error,
        semantic_fn_error_rate: input.semantic_fn_error_rate,
        structural_invariant_coverage: input.structural_invariant_coverage.clone(),
        tlog_delta_signals: input.tlog_delta_signals.clone(),
        vector,
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
/// Resource: error
/// Inputs: usize, usize, &reports::ViolationsReport, usize, usize, usize, usize
/// Outputs: evaluation::EvaluationVector
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
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
) -> EvaluationVector {
    EvaluationVector {
        objective_progress: reward_alignment_score(objectives_completed, objectives_total),
        safety: safety_score(violations),
        task_velocity: task_velocity_score(completed_tasks, total_tasks),
        issue_health: issue_health_score(open_issues, repeated_open_issues),
        semantic_contract: semantic_contract_score.clamp(0.0, 1.0),
        structural_invariant_coverage: structural_invariant_coverage_score.clamp(0.0, 1.0),
        canonical_delta_health: canonical_delta_health_score.clamp(0.0, 1.0),
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
    pub llm_turn_inputs: usize,
    pub llm_turn_outputs: usize,
    pub llm_action_outputs: usize,
    pub action_results: usize,
    pub missing_action_results: usize,
    pub llm_error_boundaries: usize,
    pub artifact_write_requests: usize,
    pub artifact_write_applies: usize,
    pub unapplied_artifact_writes: usize,
    pub score: f64,
}

const TLOG_LAG_GAP_MIN_MS: u64 = 1_000;
const TLOG_LAG_GAP_MAX_MS: u64 = 120_000;

/// Pure invariant signal over a tlog delta window.
///
/// Model: `I(ΔT) -> signal`, where ΔT is the recent canonical event window.
pub fn evaluate_tlog_delta_invariants(
    records: &[crate::tlog::TlogRecord],
) -> TlogDeltaSignals {
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

    for (idx, record) in records.iter().enumerate() {
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

                let next_kind = tlog_event_kind(&record.event);
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
            _ => {}
        }
    }

    signals.missing_action_results = llm_action_command_ids
        .difference(&action_result_command_ids)
        .count();
    signals.unapplied_artifact_writes = requested_artifact_signatures
        .difference(&applied_artifact_signatures)
        .count();
    if let Some((kind, lag_ms)) = actionable_lag_by_next_kind
        .into_iter()
        .max_by_key(|(_, lag_ms)| *lag_ms)
    {
        signals.dominant_actionable_lag_kind = kind;
        signals.dominant_actionable_lag_kind_ms = lag_ms;
    }
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
    let seq_score = if signals.contiguous_seq {
        1.0
    } else {
        1.0 - safe_ratio(signals.missing_seq_count as f64, signals.event_count as f64)
    };
    let turn_score = if signals.llm_turn_inputs == 0 {
        1.0
    } else {
        safe_ratio(signals.llm_turn_outputs as f64, signals.llm_turn_inputs as f64).min(1.0)
    };
    let action_score = if signals.llm_action_outputs == 0 {
        1.0
    } else {
        1.0 - safe_ratio(
            signals.missing_action_results as f64,
            signals.llm_action_outputs as f64,
        )
    };
    let artifact_score = if signals.artifact_write_requests == 0 {
        1.0
    } else {
        1.0 - safe_ratio(
            signals.unapplied_artifact_writes as f64,
            signals.artifact_write_requests as f64,
        )
    };
    let error_score = 1.0
        - safe_ratio(
            signals.llm_error_boundaries as f64,
            (signals.llm_turn_inputs + signals.llm_turn_outputs).max(1) as f64,
        )
        .min(1.0);
    let lag_budget_ms = (signals.event_count.max(1) as f64) * 5_000.0;
    let lag_score = 1.0
        - safe_ratio(signals.actionable_lag_total_ms as f64, lag_budget_ms)
            .min(0.75);

    geometric_score(&[
        seq_score,
        turn_score,
        action_score,
        artifact_score,
        error_score,
        lag_score,
    ])
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
        "llm_turn_output" | "llm_error_boundary" | "orchestrator_idle_pulse" | "orchestrator_mode_set"
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
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: (usize, usize)
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
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
        };

        assert!(vector.geometric_mean_like_score() > 0.0);
    }

    #[test]
    fn task_velocity_is_zero_when_no_tasks_done() {
        assert_eq!(task_velocity_score(0, 3), 0.0);
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
}

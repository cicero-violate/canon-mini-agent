use std::collections::HashMap;
use std::path::Path;

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
}

impl EvaluationVector {
    pub fn geometric_mean_like_score(&self) -> f64 {
        let values = [
            self.objective_progress.clamp(0.001, 1.0),
            self.safety.clamp(0.001, 1.0),
            self.task_velocity.clamp(0.001, 1.0),
            self.issue_health.clamp(0.001, 1.0),
            self.semantic_contract.clamp(0.001, 1.0),
        ];
        let product = values.iter().product::<f64>();
        product.powf(0.20)
    }
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
) -> EvaluationVector {
    EvaluationVector {
        objective_progress: reward_alignment_score(objectives_completed, objectives_total),
        safety: safety_score(violations),
        task_velocity: task_velocity_score(completed_tasks, total_tasks),
        issue_health: issue_health_score(open_issues, repeated_open_issues),
        semantic_contract: semantic_contract_score.clamp(0.0, 1.0),
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
    let _canonical_events = load_canonical_event_count(workspace);

    let (completed_tasks, total_tasks) = load_task_counts(workspace);
    let (open_issues, repeated_open_issues, high_priority_open_issues) =
        load_issue_counts(workspace);
    let semantic_metrics = crate::semantic_contract::load_semantic_manifest_metrics(workspace);

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
    })
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

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: usize
/// Effects: fs_read, state_read
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_canonical_event_count(workspace: &Path) -> usize {
    let path = workspace.join("agent_state").join("tlog.ndjson");
    std::fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
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
}

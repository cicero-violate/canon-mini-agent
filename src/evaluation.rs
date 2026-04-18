use std::collections::HashMap;
use std::path::Path;

use crate::issues::{Issue, IssuesFile};
use crate::reports::{DiagnosticsReport, ViolationsReport};

#[derive(Debug, Clone, Default)]
pub struct EvaluationVector {
    pub reward_alignment: f64,
    pub invariant_safety: f64,
    pub tlog_consistency: f64,
    pub execution_efficiency: f64,
    pub convergence_stability: f64,
}

impl EvaluationVector {
    pub fn geometric_mean_like_score(&self) -> f64 {
        let values = [
            clamp_unit(self.reward_alignment),
            clamp_unit(self.invariant_safety),
            clamp_unit(self.tlog_consistency),
            clamp_unit(self.execution_efficiency),
            clamp_unit(self.convergence_stability),
        ];
        let product = values.iter().product::<f64>();
        product.powf(1.0 / values.len() as f64)
    }
}

#[derive(Debug, Clone, Default)]
pub struct EvaluationWorkspaceSnapshot {
    pub objectives_completed: usize,
    pub objectives_total: usize,
    pub enforced_blocks: usize,
    pub canonical_events: usize,
    pub projection_mismatches: usize,
    pub completed_tasks: usize,
    pub actions: usize,
    pub wall_clock_s: f64,
    pub repeated_issue_count: usize,
    pub state_delta_norm: f64,
    pub diagnostics_repair_pressure: f64,
    pub vector: EvaluationVector,
}

impl EvaluationWorkspaceSnapshot {
    pub fn overall_score(&self) -> f64 {
        let base = self.vector.geometric_mean_like_score();
        let repair_penalty = 0.25 * self.diagnostics_repair_pressure;
        clamp_unit(base * (1.0 - repair_penalty))
    }
}

pub fn reward_alignment_score(completed_objectives: usize, total_objectives: usize) -> f64 {
    safe_ratio(completed_objectives as f64, total_objectives.max(1) as f64)
}

pub fn invariant_safety_score(violations: &ViolationsReport, enforced_blocks: usize) -> f64 {
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
    let enforced_credit = (enforced_blocks as f64) * 0.05;
    clamp_unit(1.0 - severity_penalty + enforced_credit)
}

pub fn tlog_consistency_score(canonical_events: usize, projection_mismatches: usize) -> f64 {
    if canonical_events == 0 {
        return 1.0;
    }
    clamp_unit(1.0 - safe_ratio(projection_mismatches as f64, canonical_events as f64))
}

pub fn execution_efficiency_score(completed_tasks: usize, actions: usize, wall_clock_s: f64) -> f64 {
    let denom = (actions.max(1) as f64) * wall_clock_s.max(1.0);
    clamp_unit((completed_tasks as f64) / denom)
}

pub fn convergence_stability_score(repeated_issue_count: usize, state_delta_norm: f64) -> f64 {
    let penalty = repeated_issue_count as f64 * 0.1 + state_delta_norm.max(0.0);
    clamp_unit(1.0 - penalty)
}

pub fn evaluate_repo_state(
    objectives_completed: usize,
    objectives_total: usize,
    violations: &ViolationsReport,
    enforced_blocks: usize,
    canonical_events: usize,
    projection_mismatches: usize,
    completed_tasks: usize,
    actions: usize,
    wall_clock_s: f64,
    repeated_issue_count: usize,
    state_delta_norm: f64,
) -> EvaluationVector {
    EvaluationVector {
        reward_alignment: reward_alignment_score(objectives_completed, objectives_total),
        invariant_safety: invariant_safety_score(violations, enforced_blocks),
        tlog_consistency: tlog_consistency_score(canonical_events, projection_mismatches),
        execution_efficiency: execution_efficiency_score(completed_tasks, actions, wall_clock_s),
        convergence_stability: convergence_stability_score(repeated_issue_count, state_delta_norm),
    }
}

pub fn evaluate_workspace(workspace: &Path) -> EvaluationWorkspaceSnapshot {
    let objectives = load_objectives_file(workspace);
    let objectives_total = objectives.objectives.len();
    let objectives_completed = objectives
        .objectives
        .iter()
        .filter(|objective| crate::objectives::is_completed(objective))
        .count();

    let violations = crate::reports::load_violations_report(workspace);
    let issues = crate::issues::load_issues_file(workspace);
    let diagnostics = crate::reports::load_diagnostics_report(workspace).unwrap_or_else(empty_diagnostics_report);

    let completed_tasks = load_completed_task_count(workspace);
    let actions = load_action_count(workspace);
    let wall_clock_s = load_wall_clock_seconds(workspace);
    let repeated_issue_count = repeated_open_issue_count(&issues);
    let state_delta_norm = diagnostics_repair_pressure(&diagnostics);
    let canonical_events = load_canonical_event_count(workspace);
    let projection_mismatches = load_projection_mismatch_count(workspace);
    let enforced_blocks = load_enforced_block_count(workspace);

    let vector = evaluate_repo_state(
        objectives_completed,
        objectives_total,
        &violations,
        enforced_blocks,
        canonical_events,
        projection_mismatches,
        completed_tasks,
        actions,
        wall_clock_s,
        repeated_issue_count,
        state_delta_norm,
    );

    EvaluationWorkspaceSnapshot {
        objectives_completed,
        objectives_total,
        enforced_blocks,
        canonical_events,
        projection_mismatches,
        completed_tasks,
        actions,
        wall_clock_s,
        repeated_issue_count,
        state_delta_norm,
        diagnostics_repair_pressure: diagnostics_repair_pressure(&diagnostics),
        vector,
    }
}

pub fn repeated_open_issue_count(issues: &IssuesFile) -> usize {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for issue in &issues.issues {
        if is_open(issue) {
            *counts.entry(issue.title.as_str()).or_insert(0) += 1;
        }
    }
    counts.values().filter(|count| **count > 1).sum()
}

pub fn diagnostics_repair_pressure(report: &DiagnosticsReport) -> f64 {
    let total_targets = report
        .ranked_failures
        .iter()
        .map(|f| f.repair_targets.len() as f64)
        .sum::<f64>();
    clamp_unit(total_targets / (report.ranked_failures.len().max(1) as f64 * 4.0))
}

fn load_objectives_file(workspace: &Path) -> crate::objectives::ObjectivesFile {
    let path = crate::objectives::resolve_objectives_path(workspace);
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

fn load_completed_task_count(workspace: &Path) -> usize {
    let path = workspace.join(crate::constants::MASTER_PLAN_FILE);
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return 0;
    };
    value
        .get("tasks")
        .and_then(|tasks| tasks.as_array())
        .map(|tasks| {
            tasks.iter()
                .filter(|task| {
                    task.get("status")
                        .and_then(|status| status.as_str())
                        .map(|status| matches!(status, "done" | "complete" | "completed"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn load_action_count(workspace: &Path) -> usize {
    let path = workspace
        .join("agent_state")
        .join("default")
        .join("actions.jsonl");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines().filter(|line| !line.trim().is_empty()).count()
}

fn load_wall_clock_seconds(workspace: &Path) -> f64 {
    let path = workspace.join("agent_state").join("tlog.ndjson");
    let Ok(records) = crate::tlog::Tlog::read_records(&path) else {
        return 1.0;
    };
    let first = records.first().map(|record| record.ts_ms).unwrap_or(0);
    let last = records.last().map(|record| record.ts_ms).unwrap_or(first);
    let delta_ms = last.saturating_sub(first);
    (delta_ms as f64 / 1000.0).max(1.0)
}

fn load_canonical_event_count(workspace: &Path) -> usize {
    let path = workspace.join("agent_state").join("tlog.ndjson");
    crate::tlog::Tlog::read_records(&path)
        .map(|records| records.len())
        .unwrap_or(0)
}

fn load_projection_mismatch_count(workspace: &Path) -> usize {
    let mut mismatches = 0usize;
    for relative_path in [
        crate::constants::ISSUES_FILE,
        crate::constants::VIOLATIONS_FILE,
        crate::constants::diagnostics_file(),
    ] {
        let path = workspace.join(relative_path);
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        if raw.trim().is_empty() {
            mismatches += 1;
        }
    }
    mismatches
}

fn load_enforced_block_count(workspace: &Path) -> usize {
    let path = workspace
        .join("agent_state")
        .join("enforced_invariants.json");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return 0;
    };

    ["enforced", "promoted", "discovered"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(|entry| entry.as_array()))
        .map(|entries| entries.len())
        .sum()
}

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
    if d <= 0.0 { 0.0 } else { n / d }
}

fn clamp_unit(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometric_mean_penalizes_weak_dimensions() {
        let vector = EvaluationVector {
            reward_alignment: 1.0,
            invariant_safety: 1.0,
            tlog_consistency: 1.0,
            execution_efficiency: 1.0,
            convergence_stability: 0.25,
        };

        assert!(vector.geometric_mean_like_score() < 1.0);
    }

    #[test]
    fn repeated_open_issues_only_counts_open_duplicates() {
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

        assert_eq!(repeated_open_issue_count(&issues), 2);
    }

    #[test]
    fn diagnostics_pressure_scales_with_repair_targets() {
        let report = DiagnosticsReport {
            status: "needs_repair".to_string(),
            inputs_scanned: vec![],
            ranked_failures: vec![crate::reports::DiagnosticsFinding {
                id: "D1".to_string(),
                impact: crate::reports::Impact::High,
                signal: "signal".to_string(),
                evidence: vec!["evidence".to_string()],
                root_cause: "cause".to_string(),
                repair_targets: vec![
                    "src/app.rs".to_string(),
                    "src/tlog.rs".to_string(),
                ],
            }],
            planner_handoff: vec![],
        };

        assert!(diagnostics_repair_pressure(&report) > 0.0);
    }
}

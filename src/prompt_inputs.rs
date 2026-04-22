use crate::llm_runtime::{
    config::LlmEndpoint, tab_management::TabManagerHandle, ws_server::WsBridge,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::constants::{ISSUES_FILE, MASTER_PLAN_FILE, SPEC_FILE, VIOLATIONS_FILE};
use crate::issues::{read_ranked_open_issues, Issue};
use crate::reports::{DiagnosticsFinding, DiagnosticsReport, Impact, Severity, ViolationsReport};

use crate::prompts::{
    single_role_executor_prompt, single_role_planner_prompt, AgentPromptKind,
};

#[derive(Clone)]
pub struct LaneConfig {
    pub index: usize,
    pub endpoint: LlmEndpoint,
    pub plan_file: String,
    pub label: String,
    pub tabs: TabManagerHandle,
}

pub struct OrchestratorContext<'a> {
    pub lanes: &'a [LaneConfig],
    pub workspace: &'a PathBuf,
    pub bridge: &'a WsBridge,
    pub tabs_planner: &'a TabManagerHandle,
    pub planner_ep: &'a LlmEndpoint,
    pub master_plan_path: &'a Path,
}

pub struct PlannerInputs {
    pub summary_text: String,
    pub executor_diff_text: String,
    pub cargo_test_failures: String,
    pub lessons_text: String,
    pub objectives_text: String,
    pub enforced_invariants_text: String,
    pub semantic_control_text: String,
    pub plan_text: String,
    pub plan_diff_text: String,
}

pub struct ExecutorDiffInputs {
    pub diff_text: String,
}

pub struct SingleRoleInputs {
    pub role: String,
    pub prompt_kind: AgentPromptKind,
    pub primary_input: String,
}

pub struct SingleRoleContext<'a> {
    pub workspace: &'a Path,
    pub spec_path: &'a Path,
    pub master_plan_path: &'a Path,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticPromptArtifacts {
    pub issues_summary: String,
    pub violations_summary: String,
    pub diagnostics_summary: String,
    /// Eval score header prepended to the issues section.
    /// Shows overall score, weakest dimension, and a direct improvement directive.
    pub eval_header: String,
    #[cfg(test)]
    pub diagnostics_report: String,
}

#[derive(Debug, Clone, Default)]
pub struct SemanticControlPromptState {
    pub control_summary: String,
}

pub fn read_combined_invariants_context(workspace: &Path) -> String {
    semantic_state_snapshot_from_tlog(workspace)
}

fn truncate_prompt_value(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    let mut out = String::new();
    for ch in trimmed.chars().take(max_chars) {
        out.push(ch);
    }
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn infer_tlog_lane_indices(events: &[crate::events::Event]) -> Vec<usize> {
    let mut ids = BTreeSet::new();
    for event in events {
        if let crate::events::Event::Control { event } = event {
            match event {
                crate::events::ControlEvent::PhaseSet {
                    lane: Some(lane_id),
                    ..
                }
                | crate::events::ControlEvent::LanePendingSet { lane_id, .. }
                | crate::events::ControlEvent::LaneInProgressSet { lane_id, .. }
                | crate::events::ControlEvent::LaneVerifierResultSet { lane_id, .. }
                | crate::events::ControlEvent::LanePlanTextSet { lane_id, .. }
                | crate::events::ControlEvent::VerifierSummarySet { lane_id, .. }
                | crate::events::ControlEvent::LaneSubmitInFlightSet { lane_id, .. }
                | crate::events::ControlEvent::LanePromptInFlightSet { lane_id, .. }
                | crate::events::ControlEvent::LaneActiveTabSet { lane_id, .. }
                | crate::events::ControlEvent::TabIdToLaneSet { lane_id, .. }
                | crate::events::ControlEvent::LaneNextSubmitAtSet { lane_id, .. }
                | crate::events::ControlEvent::LaneStepsUsedSet { lane_id, .. }
                | crate::events::ControlEvent::ExecutorTurnRegistered { lane_id, .. }
                | crate::events::ControlEvent::ExecutorCompletionRecovered { lane_id, .. }
                | crate::events::ControlEvent::ExecutorCompletionTabRebound { lane_id, .. }
                | crate::events::ControlEvent::ExecutorSubmitAckTabRebound { lane_id, .. } => {
                    ids.insert(*lane_id);
                }
                crate::events::ControlEvent::ScheduledPhaseSet { .. }
                | crate::events::ControlEvent::PlannerPendingSet { .. }
                | crate::events::ControlEvent::PlannerObjectiveReviewQueued
                | crate::events::ControlEvent::PlannerObjectivePlanGapQueued
                | crate::events::ControlEvent::DiagnosticsPendingSet { .. }
                | crate::events::ControlEvent::DiagnosticsReconciliationQueued
                | crate::events::ControlEvent::VerifierBlockerSet { .. }
                | crate::events::ControlEvent::DiagnosticsVerifierFollowupQueued
                | crate::events::ControlEvent::DiagnosticsTextSet { .. }
                | crate::events::ControlEvent::ExternalUserMessageConsumed { .. }
                | crate::events::ControlEvent::InboundMessageConsumed { .. }
                | crate::events::ControlEvent::WakeSignalConsumed { .. }
                | crate::events::ControlEvent::WakeSignalQueued { .. }
                | crate::events::ControlEvent::InboundMessageQueued { .. }
                | crate::events::ControlEvent::RustPatchVerificationRequested { .. }
                | crate::events::ControlEvent::OrchestratorModeSet { .. }
                | crate::events::ControlEvent::OrchestratorIdlePulse { .. }
                | crate::events::ControlEvent::CheckpointSnapshotSet { .. }
                | crate::events::ControlEvent::PlannerBlockerEvidenceSet { .. }
                | crate::events::ControlEvent::PostRestartResultConsumed { .. }
                | crate::events::ControlEvent::LastPlanTextSet { .. }
                | crate::events::ControlEvent::LastExecutorDiffSet { .. }
                | crate::events::ControlEvent::LastSoloPlanTextSet { .. }
                | crate::events::ControlEvent::LastSoloExecutorDiffSet { .. }
                | crate::events::ControlEvent::ObjectivesInitialized { .. }
                | crate::events::ControlEvent::ObjectivesReplaced { .. }
                | crate::events::ControlEvent::PhaseSet { lane: None, .. }
                | crate::events::ControlEvent::ExecutorTurnDeregistered { .. } => {}
            }
        }
    }
    ids.into_iter().collect()
}

fn control_event_kind_name(event: &crate::events::ControlEvent) -> &'static str {
    match event {
        crate::events::ControlEvent::PhaseSet { .. } => "phase_set",
        crate::events::ControlEvent::ScheduledPhaseSet { .. } => "scheduled_phase_set",
        crate::events::ControlEvent::PlannerPendingSet { .. } => "planner_pending_set",
        crate::events::ControlEvent::PlannerObjectiveReviewQueued => {
            "planner_objective_review_queued"
        }
        crate::events::ControlEvent::PlannerObjectivePlanGapQueued => {
            "planner_objective_plan_gap_queued"
        }
        crate::events::ControlEvent::DiagnosticsPendingSet { .. } => "diagnostics_pending_set",
        crate::events::ControlEvent::DiagnosticsReconciliationQueued => {
            "diagnostics_reconciliation_queued"
        }
        crate::events::ControlEvent::VerifierBlockerSet { .. } => "verifier_blocker_set",
        crate::events::ControlEvent::DiagnosticsVerifierFollowupQueued => {
            "diagnostics_verifier_followup_queued"
        }
        crate::events::ControlEvent::DiagnosticsTextSet { .. } => "diagnostics_text_set",
        crate::events::ControlEvent::ExternalUserMessageConsumed { .. } => {
            "external_user_message_consumed"
        }
        crate::events::ControlEvent::InboundMessageConsumed { .. } => "inbound_message_consumed",
        crate::events::ControlEvent::WakeSignalConsumed { .. } => "wake_signal_consumed",
        crate::events::ControlEvent::WakeSignalQueued { .. } => "wake_signal_queued",
        crate::events::ControlEvent::InboundMessageQueued { .. } => "inbound_message_queued",
        crate::events::ControlEvent::RustPatchVerificationRequested { .. } => {
            "rust_patch_verification_requested"
        }
        crate::events::ControlEvent::OrchestratorModeSet { .. } => "orchestrator_mode_set",
        crate::events::ControlEvent::OrchestratorIdlePulse { .. } => "orchestrator_idle_pulse",
        crate::events::ControlEvent::CheckpointSnapshotSet { .. } => "checkpoint_snapshot_set",
        crate::events::ControlEvent::PlannerBlockerEvidenceSet { .. } => {
            "planner_blocker_evidence_set"
        }
        crate::events::ControlEvent::PostRestartResultConsumed { .. } => {
            "post_restart_result_consumed"
        }
        crate::events::ControlEvent::LastPlanTextSet { .. } => "last_plan_text_set",
        crate::events::ControlEvent::LastExecutorDiffSet { .. } => "last_executor_diff_set",
        crate::events::ControlEvent::LastSoloPlanTextSet { .. } => "last_solo_plan_text_set",
        crate::events::ControlEvent::LastSoloExecutorDiffSet { .. } => {
            "last_solo_executor_diff_set"
        }
        crate::events::ControlEvent::ObjectivesInitialized { .. } => "objectives_initialized",
        crate::events::ControlEvent::ObjectivesReplaced { .. } => "objectives_replaced",
        crate::events::ControlEvent::LanePendingSet { .. } => "lane_pending_set",
        crate::events::ControlEvent::LaneInProgressSet { .. } => "lane_in_progress_set",
        crate::events::ControlEvent::LaneVerifierResultSet { .. } => "lane_verifier_result_set",
        crate::events::ControlEvent::LanePlanTextSet { .. } => "lane_plan_text_set",
        crate::events::ControlEvent::VerifierSummarySet { .. } => "verifier_summary_set",
        crate::events::ControlEvent::LaneSubmitInFlightSet { .. } => "lane_submit_in_flight_set",
        crate::events::ControlEvent::LanePromptInFlightSet { .. } => "lane_prompt_in_flight_set",
        crate::events::ControlEvent::LaneActiveTabSet { .. } => "lane_active_tab_set",
        crate::events::ControlEvent::TabIdToLaneSet { .. } => "tab_id_to_lane_set",
        crate::events::ControlEvent::LaneNextSubmitAtSet { .. } => "lane_next_submit_at_set",
        crate::events::ControlEvent::LaneStepsUsedSet { .. } => "lane_steps_used_set",
        crate::events::ControlEvent::ExecutorTurnRegistered { .. } => "executor_turn_registered",
        crate::events::ControlEvent::ExecutorTurnDeregistered { .. } => {
            "executor_turn_deregistered"
        }
        crate::events::ControlEvent::ExecutorCompletionRecovered { .. } => {
            "executor_completion_recovered"
        }
        crate::events::ControlEvent::ExecutorCompletionTabRebound { .. } => {
            "executor_completion_tab_rebound"
        }
        crate::events::ControlEvent::ExecutorSubmitAckTabRebound { .. } => {
            "executor_submit_ack_tab_rebound"
        }
    }
}

fn effect_event_kind_name(event: &crate::events::EffectEvent) -> &'static str {
    match event {
        crate::events::EffectEvent::InvariantViolation { .. } => "invariant_violation",
        crate::events::EffectEvent::LlmErrorBoundary { .. } => "llm_error_boundary",
        crate::events::EffectEvent::CheckpointSaved { .. } => "checkpoint_saved",
        crate::events::EffectEvent::CheckpointLoaded { .. } => "checkpoint_loaded",
        crate::events::EffectEvent::BuildEvolutionAdvanced { .. } => "build_evolution_advanced",
        crate::events::EffectEvent::WorkspaceArtifactWriteRequested { .. } => {
            "workspace_artifact_write_requested"
        }
        crate::events::EffectEvent::WorkspaceArtifactWriteApplied { .. } => {
            "workspace_artifact_write_applied"
        }
        crate::events::EffectEvent::InboundMessageRecorded { .. } => "inbound_message_recorded",
        crate::events::EffectEvent::ExternalUserMessageRecorded { .. } => {
            "external_user_message_recorded"
        }
        crate::events::EffectEvent::BlockerRecorded { .. } => "blocker_recorded",
        crate::events::EffectEvent::LessonsArtifactRecorded { .. } => "lessons_artifact_recorded",
        crate::events::EffectEvent::IssuesFileRecorded { .. } => "issues_file_recorded",
        crate::events::EffectEvent::DiagnosticsReportRecorded { .. } => {
            "diagnostics_report_recorded"
        }
        crate::events::EffectEvent::EnforcedInvariantsRecorded { .. } => {
            "enforced_invariants_recorded"
        }
        crate::events::EffectEvent::ViolationsReportRecorded { .. } => "violations_report_recorded",
        crate::events::EffectEvent::FramesAllDebugSnapshot { .. } => "frames_all_debug_snapshot",
        crate::events::EffectEvent::LlmTurnInput { .. } => "llm_turn_input",
        crate::events::EffectEvent::LlmTurnOutput { .. } => "llm_turn_output",
        crate::events::EffectEvent::ActionResultRecorded { .. } => "action_result_recorded",
        crate::events::EffectEvent::FingerprintDriftRecorded { .. } => "fingerprint_drift_recorded",
        crate::events::EffectEvent::GrpoDatasetRecorded { .. } => "grpo_dataset_recorded",
        crate::events::EffectEvent::PostRestartResultRecorded { .. } => {
            "post_restart_result_recorded"
        }
    }
}

pub fn semantic_state_snapshot_from_tlog(workspace: &Path) -> String {
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let events = match crate::tlog::Tlog::read_events(&tlog_path) {
        Ok(events) => events,
        Err(err) => {
            return format!(
                "(semantic state unavailable: failed to read {}: {err})",
                tlog_path.display()
            )
        }
    };
    if events.is_empty() {
        return String::new();
    }

    let mut control_count = 0usize;
    let mut effect_count = 0usize;
    let mut recent_controls = Vec::new();
    let mut recent_effects = Vec::new();
    for event in events.iter().rev() {
        match event {
            crate::events::Event::Control { event } => {
                control_count += 1;
                if recent_controls.len() < 6 {
                    recent_controls.push(control_event_kind_name(event));
                }
            }
            crate::events::Event::Effect { event } => {
                effect_count += 1;
                if recent_effects.len() < 6 {
                    recent_effects.push(effect_event_kind_name(event));
                }
            }
        }
    }

    let lane_indices = infer_tlog_lane_indices(&events);
    let lane_count = lane_indices.iter().max().map(|idx| idx + 1).unwrap_or(1);
    let initial = crate::system_state::SystemState::new(&lane_indices, lane_count);
    let replayed = crate::system_state::replay_event_log(initial, &events).ok();

    let mut out = String::new();
    out.push_str(&format!(
        "events={} control={} effect={} source={}\n",
        events.len(),
        control_count,
        effect_count,
        tlog_path.display()
    ));
    out.push_str(
        "Authority rule: prefer this replayed state over raw artifact caches when they disagree.\n",
    );

    if let Some(state) = replayed {
        out.push_str(&format!(
            "phase={} phase_lane={} scheduled_phase={} planner_pending={} diagnostics_pending={} submitted_turns={}\n",
            if state.phase.trim().is_empty() { "(unset)" } else { state.phase.as_str() },
            state.phase_lane.map(|lane| lane.to_string()).unwrap_or_else(|| "none".to_string()),
            state.scheduled_phase.clone().unwrap_or_else(|| "none".to_string()),
            state.planner_pending,
            state.diagnostics_pending,
            state.submitted_turn_ids.len()
        ));

        let mut lane_lines = Vec::new();
        for lane_id in lane_indices.iter().take(4) {
            if let Some(lane) = state.lanes.get(lane_id) {
                let verifier = truncate_prompt_value(&lane.latest_verifier_result, 72);
                lane_lines.push(format!(
                    "lane[{lane_id}] pending={} in_progress_by={} verifier={} submit_in_flight={} prompt_in_flight={} steps_used={} active_tab={}",
                    lane.pending,
                    lane.in_progress_by.clone().unwrap_or_else(|| "none".to_string()),
                    if verifier.is_empty() { "(none)".to_string() } else { verifier },
                    state.lane_submit_in_flight.get(lane_id).copied().unwrap_or(false),
                    state.lane_prompt_in_flight.get(lane_id).copied().unwrap_or(false),
                    state.lane_steps_used.get(lane_id).copied().unwrap_or(0),
                    state.lane_active_tab.get(lane_id).map(|id| id.to_string()).unwrap_or_else(|| "none".to_string())
                ));
            }
        }
        if !lane_lines.is_empty() {
            out.push_str(&format!("{}\n", lane_lines.join("\n")));
        }
    }

    if !recent_controls.is_empty() {
        out.push_str(&format!(
            "recent control events: {}\n",
            recent_controls.join(" -> ")
        ));
    }
    if !recent_effects.is_empty() {
        out.push_str(&format!(
            "recent effect events: {}\n",
            recent_effects.join(" -> ")
        ));
    }

    out.trim().to_string()
}

fn invariant_status_label(status: &crate::invariants::InvariantStatus) -> &'static str {
    match status {
        crate::invariants::InvariantStatus::Discovered => "discovered",
        crate::invariants::InvariantStatus::Promoted => "promoted",
        crate::invariants::InvariantStatus::Enforced => "enforced",
        crate::invariants::InvariantStatus::Collapsed => "collapsed",
    }
}

fn summarize_enforced_invariants_for_prompt(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(file) = serde_json::from_str::<crate::invariants::EnforcedInvariantsFile>(raw) else {
        return raw.to_string();
    };
    if file.invariants.is_empty() {
        return String::new();
    }

    let mut discovered = 0usize;
    let mut promoted = 0usize;
    let mut enforced = 0usize;
    let mut collapsed = 0usize;
    for inv in &file.invariants {
        match inv.status {
            crate::invariants::InvariantStatus::Discovered => discovered += 1,
            crate::invariants::InvariantStatus::Promoted => promoted += 1,
            crate::invariants::InvariantStatus::Enforced => enforced += 1,
            crate::invariants::InvariantStatus::Collapsed => collapsed += 1,
        }
    }

    let mut ranked: Vec<&crate::invariants::DiscoveredInvariant> = file
        .invariants
        .iter()
        .filter(|inv| inv.status != crate::invariants::InvariantStatus::Collapsed)
        .collect();
    ranked.sort_by(|a, b| {
        let status_rank = |status: &crate::invariants::InvariantStatus| match status {
            crate::invariants::InvariantStatus::Enforced => 0u8,
            crate::invariants::InvariantStatus::Promoted => 1,
            crate::invariants::InvariantStatus::Discovered => 2,
            crate::invariants::InvariantStatus::Collapsed => 3,
        };
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| b.support_count.cmp(&a.support_count))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut out = String::new();
    out.push_str(&format!(
        "Summary: active={} enforced={} promoted={} discovered={} collapsed={} last_synthesized_ms={}\n",
        discovered + promoted + enforced,
        enforced,
        promoted,
        discovered,
        collapsed,
        file.last_synthesized_ms
    ));
    for inv in ranked.into_iter().take(8) {
        let predicate = inv.predicate_text.trim();
        let predicate = if predicate.chars().count() > 160 {
            let shortened: String = predicate.chars().take(160).collect();
            format!("{shortened}…")
        } else {
            predicate.to_string()
        };
        let gates = if inv.gates.is_empty() {
            "(no gates)".to_string()
        } else {
            inv.gates.join(",")
        };
        out.push_str(&format!(
            "- [{}][support:{}][gates:{}] {} — {}\n",
            invariant_status_label(&inv.status),
            inv.support_count,
            gates,
            inv.id,
            predicate
        ));
    }
    out.push_str("Full detail: {\"action\":\"invariants\",\"op\":\"read\"}");
    out
}

/// Build a one-line eval score header with a weakest-dimension directive.
/// Placed directly above the issues list so the LLM sees the score and the
/// issues that are dragging it down in the same view.
fn build_eval_header(workspace: &Path) -> String {
    let path = workspace
        .join("agent_state")
        .join("reports")
        .join("complexity")
        .join("latest.json");
    let Some(report) = load_complexity_report(&path) else {
        return String::new();
    };
    let Some(eval) = report.get("eval").and_then(|v| v.as_object()) else {
        return String::new();
    };

    let get_f64 = |key: &str| eval.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let overall = get_f64("overall_score");
    let dims = [
        ("objective_progress", get_f64("objective_progress")),
        ("safety", get_f64("safety")),
        ("task_velocity", get_f64("task_velocity")),
        ("issue_health", get_f64("issue_health")),
    ];

    let (weakest_name, weakest_val) = dims
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .copied()
        .unwrap_or(("unknown", 0.0));

    let directive = match weakest_name {
        "objective_progress" => "close completed objectives and create plan tasks for active ones",
        "task_velocity" => "complete or close stale PLAN.json tasks",
        "issue_health" => "close or fix the repeated open issues below",
        "safety" => "resolve violations listed in the violations view",
        _ => "address the highest-scored issues below",
    };

    let objectives = eval
        .get("objectives")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let tasks = eval.get("tasks").and_then(|v| v.as_str()).unwrap_or("?");

    format!(
        "EVAL score={overall:.3}  weakest={weakest_name}({weakest_val:.3})  \
objectives={objectives}  tasks={tasks}\n\
→ To raise score: {directive}\n"
    )
}

fn load_complexity_report(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()
}

fn summarize_ranked_open_issues_for_prompt(open_issues: &[Issue], limit: usize) -> String {
    if open_issues.is_empty() {
        return "(no open issues)".to_string();
    }
    let mut out = String::new();
    out.push_str("Top open issues:\n");
    let title_max_len = 120usize;
    let location_max_len = 80usize;
    let byte_budget = 4096usize;
    for issue in open_issues.iter().take(limit.max(1)) {
        let title = issue.title.trim();
        let truncated_title = if title.len() > title_max_len {
            format!("{}…", &title[..title_max_len])
        } else {
            title.to_string()
        };
        let location = issue.location.trim();
        let truncated_location = if location.is_empty() {
            String::new()
        } else if location.len() > location_max_len {
            format!("{}…", &location[..location_max_len])
        } else {
            location.to_string()
        };
        let loc = if truncated_location.is_empty() {
            String::new()
        } else {
            format!(" ({})", truncated_location)
        };
        let line = format!(
            "- [score:{:.2}] {}: {}{}\n",
            issue.score, issue.id, truncated_title, loc
        );
        if out.len() + line.len() > byte_budget {
            out.push_str("- … additional open issues omitted; use {\"action\":\"issue\",\"op\":\"read\"} for full detail\n");
            break;
        }
        out.push_str(&line);
    }
    out
}

fn summarize_violations_report_for_prompt(report: Option<&ViolationsReport>, raw: &str) -> String {
    let Some(report) = report else {
        return match raw.trim() {
            "" => String::new(),
            "__invalid_json__" => {
                "(projected violations view unavailable: invalid JSON)".to_string()
            }
            _ => filter_active_violations_json(raw),
        };
    };
    if report.violations.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for violation in &report.violations {
        let severity = match violation.severity {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        };
        out.push_str(&format!(
            "[{}]  {}  —  {}\n",
            severity, violation.id, violation.title
        ));
    }
    out.push_str(&format!(
        "Full detail: {{\"action\":\"read_file\",\"path\":\"{}\"}}",
        VIOLATIONS_FILE
    ));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViolationsProjectionStatus {
    Missing,
    InvalidJson,
    Parsed,
}

fn parse_diagnostics_report(raw: &str) -> Option<DiagnosticsReport> {
    serde_json::from_str(raw).ok()
}

fn load_diagnostics_projection_text(workspace: &Path) -> Option<String> {
    let raw_diagnostics_text =
        read_text_or_empty(workspace.join(crate::constants::diagnostics_file()));
    if !raw_diagnostics_text.trim().is_empty()
        && parse_diagnostics_report(&raw_diagnostics_text).is_some()
    {
        return Some(raw_diagnostics_text);
    }
    crate::reports::load_diagnostics_report(workspace)
        .and_then(|report| serde_json::to_string_pretty(&report).ok())
}

fn load_violations_projection(
    workspace: &Path,
) -> (Option<ViolationsReport>, ViolationsProjectionStatus) {
    let raw_violations_text = read_text_or_empty(workspace.join(VIOLATIONS_FILE));
    if raw_violations_text.trim().is_empty() {
        return (None, ViolationsProjectionStatus::Missing);
    }
    match parse_violations_report(&raw_violations_text) {
        Some(report) => (Some(report), ViolationsProjectionStatus::Parsed),
        None => (None, ViolationsProjectionStatus::InvalidJson),
    }
}

fn render_diagnostics_report_from_state(
    open_issues: &[Issue],
    violations_report: Option<&ViolationsReport>,
    violations_status: ViolationsProjectionStatus,
) -> String {
    let validation_state = source_validation_state(violations_report, violations_status);
    let mut ranked_failures: Vec<DiagnosticsFinding> =
        open_issues.iter().map(issue_to_finding).collect();
    if let Some(report) = violations_report {
        ranked_failures.extend(report.violations.iter().map(violation_to_finding));
    }
    ranked_failures.sort_by(|a, b| {
        let impact_rank = |impact: &Impact| match impact {
            Impact::Critical => 0u8,
            Impact::High => 1,
            Impact::Medium => 2,
            Impact::Low => 3,
        };
        impact_rank(&a.impact)
            .cmp(&impact_rank(&b.impact))
            .then_with(|| a.id.cmp(&b.id))
    });

    let is_verified_empty = violations_report
        .as_ref()
        .map(|r| r.status.eq_ignore_ascii_case("verified") && r.violations.is_empty())
        .unwrap_or(false);
    let has_active_violations = violations_report
        .as_ref()
        .map(|r| !r.violations.is_empty())
        .unwrap_or(false);
    let has_high_issues = ranked_failures
        .iter()
        .any(|finding| matches!(finding.impact.clone(), Impact::Critical | Impact::High));
    let status = if ranked_failures.is_empty() && is_verified_empty {
        "verified"
    } else if has_active_violations || has_high_issues {
        "critical_failure"
    } else {
        "needs_repair"
    };

    let inputs_scanned = vec![
        format!("{} (open issues: {})", ISSUES_FILE, open_issues.len()),
        match violations_report.as_ref() {
            Some(report) if report.violations.is_empty() => format!(
                "{} (status: {}, verified empty)",
                VIOLATIONS_FILE, report.status
            ),
            Some(report) => format!(
                "{} (status: {}, violations: {})",
                VIOLATIONS_FILE,
                report.status,
                report.violations.len()
            ),
            None => match violations_status {
                ViolationsProjectionStatus::Missing => {
                    format!("{} (missing or empty)", VIOLATIONS_FILE)
                }
                ViolationsProjectionStatus::InvalidJson => {
                    format!("{} (invalid JSON)", VIOLATIONS_FILE)
                }
                ViolationsProjectionStatus::Parsed => {
                    format!("{} (parsed projection unavailable)", VIOLATIONS_FILE)
                }
            },
        },
        format!("source-validation: {validation_state}"),
    ];
    let planner_handoff =
        derive_planner_handoff(&ranked_failures, violations_report, &validation_state);
    let report = DiagnosticsReport {
        status: status.to_string(),
        inputs_scanned,
        ranked_failures,
        planner_handoff,
    };
    serde_json::to_string_pretty(&report).unwrap_or_else(|_| {
        format!(
            "{{\"status\":\"{}\",\"inputs_scanned\":[\"{}\"],\"ranked_failures\":[],\"planner_handoff\":[\"failed to render diagnostics report\"]}}",
            ISSUES_FILE,
            status
        )
    })
}

pub fn derive_semantic_prompt_artifacts(
    workspace: &Path,
    issue_limit: usize,
) -> SemanticPromptArtifacts {
    let open_issues = read_ranked_open_issues(workspace);
    let (violations_report, violations_status) = load_violations_projection(workspace);
    let diagnostics_report = load_diagnostics_projection_text(workspace).unwrap_or_else(|| {
        render_diagnostics_report_from_state(
            &open_issues,
            violations_report.as_ref(),
            violations_status,
        )
    });
    SemanticPromptArtifacts {
        issues_summary: summarize_ranked_open_issues_for_prompt(&open_issues, issue_limit),
        eval_header: build_eval_header(workspace),
        violations_summary: summarize_violations_report_for_prompt(
            violations_report.as_ref(),
            match violations_status {
                ViolationsProjectionStatus::Missing => "",
                ViolationsProjectionStatus::InvalidJson => "__invalid_json__",
                ViolationsProjectionStatus::Parsed => "",
            },
        ),
        diagnostics_summary: filter_active_diagnostics_json(&diagnostics_report),
        #[cfg(test)]
        diagnostics_report,
    }
}

pub fn derive_semantic_control_prompt_state(
    workspace: &Path,
    issue_limit: usize,
) -> SemanticControlPromptState {
    let artifacts = derive_semantic_prompt_artifacts(workspace, issue_limit);
    let runtime_state = read_combined_invariants_context(workspace);
    let mut sections = Vec::new();

    if !runtime_state.trim().is_empty() {
        sections.push(format!("Runtime semantic state:\n{}", runtime_state.trim()));
    }

    if !artifacts.issues_summary.trim().is_empty()
        && artifacts.issues_summary.trim() != "(no open issues)"
    {
        let issues_header = if artifacts.eval_header.trim().is_empty() {
            "Projected issues view (derived from semantic prompt state):".to_string()
        } else {
            format!(
                "Projected issues view (derived from semantic prompt state):\n{}",
                artifacts.eval_header.trim()
            )
        };
        sections.push(format!(
            "{}\n{}",
            issues_header,
            artifacts.issues_summary.trim()
        ));
    }

    if !artifacts.violations_summary.trim().is_empty()
        && artifacts.violations_summary.trim() != "(no verified violations)"
    {
        sections.push(format!(
            "Projected violations view:\n{}",
            artifacts.violations_summary.trim()
        ));
    }

    if !artifacts.diagnostics_summary.trim().is_empty()
        && artifacts.diagnostics_summary.trim() != "(no active diagnostics)"
    {
        sections.push(format!(
            "Projected diagnostics view:\n{}",
            artifacts.diagnostics_summary.trim()
        ));
    }

    let control_summary = if sections.is_empty() {
        String::new()
    } else {
        format!(
            "Derived authority: prefer this semantic control snapshot over raw artifact caches when they disagree.\n\n{}",
            sections.join("\n\n")
        )
    };

    SemanticControlPromptState { control_summary }
}

fn semantic_control_snapshot_hash(text: &str) -> u64 {
    // Stable FNV-1a so hashes are comparable across process restarts.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn semantic_control_snapshot_hash_hex(text: &str) -> String {
    format!("{:016x}", semantic_control_snapshot_hash(text))
}

fn read_semantic_control_snapshot_hash(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let hash = raw.trim();
    if hash.is_empty() {
        None
    } else {
        Some(hash.to_string())
    }
}

fn write_semantic_control_snapshot_hash(path: &Path, hash: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, hash);
}

pub fn derive_semantic_control_prompt_state_with_delta(
    workspace: &Path,
    issue_limit: usize,
    snapshot_hash_path: &Path,
) -> SemanticControlPromptState {
    let full = derive_semantic_control_prompt_state(workspace, issue_limit);
    let current_hash = semantic_control_snapshot_hash_hex(&full.control_summary);
    let previous_hash = read_semantic_control_snapshot_hash(snapshot_hash_path);
    let unchanged = previous_hash
        .as_deref()
        .map(|prev| prev == current_hash)
        .unwrap_or(false);
    write_semantic_control_snapshot_hash(snapshot_hash_path, &current_hash);
    if unchanged {
        SemanticControlPromptState {
            control_summary: format!(
                "Semantic control: no change since last cycle. Hash: {current_hash}"
            ),
        }
    } else {
        full
    }
}

pub fn read_semantic_control_prompt_context(workspace: &Path, issue_limit: usize) -> String {
    derive_semantic_control_prompt_state(workspace, issue_limit).control_summary
}

const LESSONS_FILE: &str = "agent_state/lessons.json";

/// Lifecycle of an individual lesson entry.
///
/// `Pending`  — the lesson lives only in `lessons.json` and is injected into the
///              planner/solo prompt at runtime.  The agent is still acting on it
///              from text rather than from code.
///
/// `Encoded`  — the lesson has been hardcoded into the system source (a validation
///              rule, a schema-fix hint, a prompt constant, etc.).  It is excluded
///              from the rendered prompt because the system already embodies it.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LessonEntryStatus {
    #[default]
    Pending,
    Encoded,
}

impl<'de> serde::Deserialize<'de> for LessonEntryStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "encoded" => Ok(LessonEntryStatus::Encoded),
            _ => Ok(LessonEntryStatus::Pending),
        }
    }
}

/// A single lesson item — text plus its encoding lifecycle status.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LessonEntry {
    pub text: String,
    #[serde(default)]
    pub status: LessonEntryStatus,
}

impl LessonEntry {
    pub fn pending(text: impl Into<String>) -> Self {
        LessonEntry {
            text: text.into(),
            status: LessonEntryStatus::Pending,
        }
    }
    pub fn is_pending(&self) -> bool {
        self.status == LessonEntryStatus::Pending
    }
}

/// Deserialize `LessonEntry` from either a plain string (old format) or a
/// `{"text": "...", "status": "..."}` object (new format).
impl<'de> serde::Deserialize<'de> for LessonEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct EntryVisitor;
        impl<'de> serde::de::Visitor<'de> for EntryVisitor {
            type Value = LessonEntry;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "a string or {{\"text\":\"...\",\"status\":\"...\"}} object"
                )
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<LessonEntry, E> {
                Ok(LessonEntry::pending(v))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<LessonEntry, E> {
                Ok(LessonEntry::pending(v))
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<LessonEntry, M::Error> {
                let mut text: Option<String> = None;
                let mut status = LessonEntryStatus::Pending;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "text" => text = Some(map.next_value()?),
                        "status" => status = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(LessonEntry {
                    text: text.unwrap_or_default(),
                    status,
                })
            }
        }
        d.deserialize_any(EntryVisitor)
    }
}

const ENCODING_INSTRUCTIONS: &str = "\
To encode a lesson permanently into the system source (so it no longer needs\n\
to live in this prompt):\n\
  failure_pattern / fix entries  →  add to `schema_fix_hint()` or\n\
      `sequence_workflow_note()` in src/lessons.rs, or add a validation rule\n\
      to `first_missing_field_for_action()` in src/tool_schema.rs.\n\
  success_sequence / required_action entries  →  add to the relevant agent\n\
      prompt constant in src/prompts.rs, or add a runtime enforcement check\n\
      in src/app.rs (see `enforce_diagnostics_python` as a model).\n\
After encoding, call `lessons encode` with the entry text to mark its status\n\
as `encoded`.  Encoded entries are excluded from the rendered prompt because\n\
the system already embodies them structurally.";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LessonsArtifact {
    #[serde(default)]
    pub summary: String,
    /// Recurring failure patterns observed in action logs.
    #[serde(default)]
    pub failures: Vec<LessonEntry>,
    /// Concrete fixes / schema corrections for each failure pattern.
    #[serde(default)]
    pub fixes: Vec<LessonEntry>,
    /// Forward-looking workflow instructions derived from success sequences.
    #[serde(default)]
    pub required_actions: Vec<LessonEntry>,
    /// How to graduate a lesson from runtime-prompt injection to system source.
    /// Set automatically; do not edit manually.
    #[serde(default = "default_encoding_instructions")]
    pub encoding_instructions: String,
}

fn default_encoding_instructions() -> String {
    ENCODING_INSTRUCTIONS.to_string()
}

pub fn read_text_or_empty(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

pub fn read_required_text(path: impl AsRef<Path>, name: &str) -> Result<String> {
    std::fs::read_to_string(path.as_ref()).with_context(|| format!("failed to read {name}"))
}

fn render_lessons_list(title: &str, items: &[LessonEntry]) -> Option<String> {
    // Only show pending entries — encoded ones are already in the system source.
    let pending: Vec<&str> = items
        .iter()
        .filter(|e| e.is_pending())
        .map(|e| e.text.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if pending.is_empty() {
        return None;
    }
    Some(format!(
        "{title}:\n{}",
        pending
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn render_lessons_artifact(artifact: &LessonsArtifact) -> String {
    let mut sections = Vec::new();
    let summary = artifact.summary.trim();
    if !summary.is_empty() {
        sections.push(format!("Summary:\n{summary}"));
    }
    if let Some(section) = render_lessons_list("Failures", &artifact.failures) {
        sections.push(section);
    }
    if let Some(section) = render_lessons_list("Fixes", &artifact.fixes) {
        sections.push(section);
    }
    if let Some(section) = render_lessons_list("Required actions", &artifact.required_actions) {
        sections.push(section);
    }
    sections.join("\n\n")
}

pub fn read_lessons_or_empty(workspace: &Path) -> String {
    let raw = read_text_or_empty(workspace.join(LESSONS_FILE));
    if !raw.trim().is_empty() {
        return match serde_json::from_str::<LessonsArtifact>(&raw) {
            Ok(artifact) => render_lessons_artifact(&artifact),
            Err(_) => raw,
        };
    }
    let artifact = crate::lessons::load_lessons_artifact(workspace);
    if artifact == LessonsArtifact::default() {
        return String::new();
    }
    render_lessons_artifact(&artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "canon-mini-agent-{label}-{}-{}",
            std::process::id(),
            unique
        ))
    }

    #[test]
    fn read_lessons_or_empty_renders_structured_json_for_prompts() {
        let workspace = temp_workspace("lessons-structured");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(LESSONS_FILE),
            r#"{
  "summary": "Recent solo cycles found missing objective/plan follow-up when lessons exist.",
  "failures": ["Lessons present without follow-up state update"],
  "fixes": ["Added explicit cycle-end enforcement signal"],
  "required_actions": ["Add focused prompt-load coverage for structured lessons"]
}"#,
        )
        .unwrap();

        let rendered = read_lessons_or_empty(&workspace);

        assert!(rendered.contains("Summary:"));
        assert!(rendered.contains("Failures:"));
        assert!(rendered.contains("Fixes:"));
        assert!(rendered.contains("Required actions:"));
        assert!(rendered.contains("- Lessons present without follow-up state update"));
    }

    #[test]
    fn read_lessons_or_empty_preserves_plaintext_lessons() {
        let workspace = temp_workspace("lessons-plaintext");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(LESSONS_FILE),
            "plain text lesson entry for prompt injection",
        )
        .unwrap();

        let rendered = read_lessons_or_empty(&workspace);

        assert_eq!(rendered, "plain text lesson entry for prompt injection");
    }

    #[test]
    fn read_lessons_or_empty_falls_back_to_tlog_snapshot_when_projection_missing() {
        let workspace = temp_workspace("lessons-tlog-fallback");
        fs::create_dir_all(workspace.join("agent_state")).unwrap();

        let artifact = LessonsArtifact {
            summary: "Recovered from tlog".to_string(),
            failures: vec![LessonEntry::pending("Prompt loader should recover lessons")],
            fixes: vec![LessonEntry::pending(
                "Read the latest lessons snapshot from tlog",
            )],
            required_actions: vec![LessonEntry::pending(
                "Delete the projection and verify recovery",
            )],
            encoding_instructions: "encode later".to_string(),
        };

        crate::lessons::persist_lessons_projection(
            &workspace,
            &artifact,
            "prompt_lessons_tlog_fallback_test",
        )
        .unwrap();
        fs::remove_file(workspace.join(LESSONS_FILE)).unwrap();

        let rendered = read_lessons_or_empty(&workspace);

        assert!(rendered.contains("Recovered from tlog"));
        assert!(rendered.contains("Prompt loader should recover lessons"));
        assert!(rendered.contains("Read the latest lessons snapshot from tlog"));
    }
}

fn is_done_like_status(status: &str) -> bool {
    crate::issues::is_done_like_status(status)
}

fn is_ready_status(status: &str) -> bool {
    status.trim().to_ascii_lowercase() == "ready"
}

/// Extract the top-N ready tasks from PLAN.json and format them for the executor prompt.
///
/// Returns a formatted string listing each ready task as:
///   [priority] id: title
///     → step 1
///     → step 2 (first two steps only)
///
/// Returns "(no ready tasks)" when PLAN.json is missing, empty, or has no ready tasks.
pub fn read_ready_tasks(workspace: &Path, limit: usize) -> String {
    let plan_path = workspace.join(crate::constants::MASTER_PLAN_FILE);
    let raw = match std::fs::read_to_string(&plan_path) {
        Ok(s) => s,
        Err(_) => return "(no ready tasks)".to_string(),
    };
    if raw.trim().is_empty() {
        return "(no ready tasks)".to_string();
    }
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return "(no ready tasks)".to_string();
    };
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return "(no ready tasks)".to_string();
    };

    let ready: Vec<&Value> = tasks
        .iter()
        .filter(|t| {
            t.get("status")
                .and_then(Value::as_str)
                .map(is_ready_status)
                .unwrap_or(false)
        })
        .take(limit)
        .collect();

    if ready.is_empty() {
        return "(no ready tasks)".to_string();
    }

    let mut out = String::new();
    for task in &ready {
        let id = task.get("id").and_then(Value::as_str).unwrap_or("?");
        let priority = task.get("priority").and_then(Value::as_str).unwrap_or("?");
        let title = task
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(no title)");
        out.push_str(&format!("[{priority}] {id}: {title}\n"));
        if let Some(steps) = task.get("steps").and_then(Value::as_array) {
            for step in steps.iter().take(2) {
                if let Some(s) = step.as_str() {
                    out.push_str(&format!("  → {s}\n"));
                }
            }
        }
    }
    out.trim_end().to_string()
}

pub fn filter_pending_plan_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "(no pending plan tasks)".to_string();
    }
    let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return raw.to_string();
    };
    let Some(tasks) = obj.get("tasks").and_then(Value::as_array) else {
        return raw.to_string();
    };

    let pending_tasks: Vec<Value> = tasks
        .iter()
        .filter(|task| {
            !task
                .get("status")
                .and_then(Value::as_str)
                .map(is_done_like_status)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    if pending_tasks.is_empty() {
        return "(no pending plan tasks)".to_string();
    }

    let pending_ids: std::collections::HashSet<String> = pending_tasks
        .iter()
        .filter_map(|task| task.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    obj.insert("tasks".to_string(), Value::Array(pending_tasks));
    if let Some(edges) = obj
        .get("dag")
        .and_then(Value::as_object)
        .and_then(|dag| dag.get("edges"))
        .and_then(Value::as_array)
    {
        let filtered_edges: Vec<Value> = edges
            .iter()
            .filter(|edge| {
                let from = edge.get("from").and_then(Value::as_str);
                let to = edge.get("to").and_then(Value::as_str);
                match (from, to) {
                    (Some(from), Some(to)) => {
                        pending_ids.contains(from) && pending_ids.contains(to)
                    }
                    _ => false,
                }
            })
            .cloned()
            .collect();
        obj.insert(
            "dag".to_string(),
            serde_json::json!({ "edges": filtered_edges }),
        );
    }

    serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw.to_string())
}

pub fn filter_active_violations_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(violations) = value.get("violations").and_then(Value::as_array) else {
        return raw.to_string();
    };
    if violations.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for v in violations {
        let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
        let title = v
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| v.get("description").and_then(Value::as_str))
            .unwrap_or("(no title)");
        let severity = v.get("severity").and_then(Value::as_str).unwrap_or("error");
        out.push_str(&format!("[{severity}]  {id}  —  {title}\n"));
    }
    out.push_str("Full detail: {\"action\":\"read_file\",\"path\":\"VIOLATIONS.json\"}");
    out
}

pub fn filter_active_diagnostics_json(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(failures) = value.get("ranked_failures").and_then(Value::as_array) else {
        return raw.to_string();
    };
    if failures.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (rank, f) in failures.iter().enumerate() {
        let id = f.get("id").and_then(Value::as_str).unwrap_or("?");
        let title = f
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| f.get("signal").and_then(Value::as_str))
            .or_else(|| f.get("description").and_then(Value::as_str))
            .or_else(|| f.get("root_cause").and_then(Value::as_str))
            .unwrap_or("(no title)");
        let severity = f
            .get("severity")
            .and_then(Value::as_str)
            .or_else(|| f.get("impact").and_then(Value::as_str))
            .unwrap_or("?");
        out.push_str(&format!("[{}] [{severity}]  {id}  —  {title}\n", rank + 1));
    }
    out.push_str("Full detail: {\"action\":\"read_file\",\"path\":\"agent_state/default/diagnostics-default.json\"}");
    out
}

fn parse_violations_report(raw: &str) -> Option<ViolationsReport> {
    if raw.trim().is_empty() {
        return None;
    }
    let mut report = serde_json::from_str::<ViolationsReport>(raw).ok()?;
    report.violations.retain(crate::reports::violation_is_fresh);
    Some(report)
}

fn source_validation_state(
    report: Option<&ViolationsReport>,
    violations_status: ViolationsProjectionStatus,
) -> String {
    match report {
        Some(report)
            if report.status.eq_ignore_ascii_case("verified") && report.violations.is_empty() =>
        {
            "verified_empty".to_string()
        }
        Some(report) => format!(
            "status={},violations={}",
            report.status,
            report.violations.len()
        ),
        None => match violations_status {
            ViolationsProjectionStatus::Missing => "missing".to_string(),
            ViolationsProjectionStatus::InvalidJson => "invalid_json".to_string(),
            ViolationsProjectionStatus::Parsed => "missing".to_string(),
        },
    }
}

fn impact_from_severity(severity: &Severity) -> Impact {
    match severity {
        Severity::Critical => Impact::Critical,
        Severity::High => Impact::High,
        Severity::Medium => Impact::Medium,
        Severity::Low => Impact::Low,
    }
}

fn impact_from_issue_score(score: f32, priority: &str) -> Impact {
    match priority.trim().to_lowercase().as_str() {
        "critical" => Impact::Critical,
        "high" => Impact::High,
        "medium" => Impact::Medium,
        "low" => Impact::Low,
        _ if score >= 0.85 => Impact::Critical,
        _ if score >= 0.65 => Impact::High,
        _ if score >= 0.35 => Impact::Medium,
        _ => Impact::Low,
    }
}

fn is_path_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ';' | ')' | ']' | '}' | '"') || c == '\''
}

fn extract_path_like_target(text: &str) -> Option<String> {
    for prefix in ["src/", "tests/", "PLANS/", "agent_state/", "state/"] {
        if let Some(idx) = text.find(prefix) {
            let tail = &text[idx..];
            let end = tail.find(is_path_terminator).unwrap_or(tail.len());
            let candidate = tail[..end].trim_matches(is_path_terminator);
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn issue_repair_targets(issue: &crate::issues::Issue) -> Vec<String> {
    let mut targets = Vec::new();
    let location = issue.location.trim();
    if !location.is_empty() {
        targets.push(location.to_string());
    }
    for evidence in &issue.evidence {
        if let Some(path) = extract_path_like_target(evidence) {
            if !targets.iter().any(|existing| existing == &path) {
                targets.push(path);
            }
        }
    }
    if targets.is_empty() {
        targets.push(format!("{}#{}", ISSUES_FILE, issue.id));
    }
    targets
}

fn issue_to_finding(issue: &crate::issues::Issue) -> DiagnosticsFinding {
    let title = issue.title.trim();
    let description = issue.description.trim();
    let signal = if title.is_empty() {
        format!("open issue {} [score:{:.2}]", issue.id, issue.score)
    } else {
        format!("{} [score:{:.2}]", title, issue.score)
    };
    let evidence = if issue.evidence.is_empty() {
        vec![format!("{} entry {}", ISSUES_FILE, issue.id)]
    } else {
        issue.evidence.clone()
    };
    DiagnosticsFinding {
        id: issue.id.clone(),
        impact: impact_from_issue_score(issue.score, &issue.priority),
        signal,
        evidence,
        root_cause: if description.is_empty() {
            title.to_string()
        } else {
            description.to_string()
        },
        repair_targets: issue_repair_targets(issue),
    }
}

fn violation_to_finding(violation: &crate::reports::Violation) -> DiagnosticsFinding {
    let title = violation.title.trim();
    let signal = if title.is_empty() {
        format!("violation {} [{:?}]", violation.id, violation.severity)
    } else {
        format!("{} [{:?}]", title, violation.severity)
    };
    let mut repair_targets = violation.files.clone();
    if repair_targets.is_empty() {
        repair_targets = violation.required_fix.clone();
    }
    if repair_targets.is_empty() {
        repair_targets.push(format!("VIOLATIONS.json#{}", violation.id));
    }
    DiagnosticsFinding {
        id: violation.id.clone(),
        impact: impact_from_severity(&violation.severity),
        signal,
        evidence: if violation.evidence.is_empty() {
            vec![format!("VIOLATIONS.json entry {}", violation.id)]
        } else {
            violation.evidence.clone()
        },
        root_cause: if violation.issue.trim().is_empty() {
            violation.impact.trim().to_string()
        } else {
            violation.issue.trim().to_string()
        },
        repair_targets,
    }
}

fn derive_planner_handoff(
    issues: &[DiagnosticsFinding],
    violations: Option<&ViolationsReport>,
    validation_state: &str,
) -> Vec<String> {
    if !issues.is_empty() {
        let mut handoff = Vec::new();
        for finding in issues.iter().take(3) {
            let target = finding
                .repair_targets
                .first()
                .cloned()
                .unwrap_or_else(|| finding.id.clone());
            handoff.push(format!(
                "Prioritize {} at {}: {}",
                finding.id, target, finding.signal
            ));
        }
        return handoff;
    }

    let mut handoff = Vec::new();
    if let Some(report) = violations {
        if report.violations.is_empty() {
            handoff.push(
                "No open issues and VIOLATIONS.json is verified empty; keep the current plan and continue monitoring source validation.".to_string(),
            );
        } else {
            handoff.push(format!(
                "No open issues were derived, but VIOLATIONS.json still contains {} violation(s); reconcile source validation before changing the plan.",
                report.violations.len()
            ));
        }
    } else if validation_state == "missing" {
        handoff.push(
            "No open issues were derived and VIOLATIONS.json is missing; continue with plan maintenance, but add source validation if stale state is suspected.".to_string(),
        );
    } else {
        handoff.push(
            "No open issues were derived; continue watching the rendered diagnostics view for new source validation evidence.".to_string(),
        );
    }
    handoff
}

pub fn render_diagnostics_report_from_issues(workspace: &Path) -> String {
    let open_issues = read_ranked_open_issues(workspace);
    let (violations_report, violations_status) = load_violations_projection(workspace);
    render_diagnostics_report_from_state(
        &open_issues,
        violations_report.as_ref(),
        violations_status,
    )
}

#[cfg(test)]
fn diagnostics_have_current_source_validation(failures: &[Value]) -> bool {
    failures.iter().all(|failure| {
        failure
            .get("evidence")
            .and_then(Value::as_array)
            .map(|entries| {
                entries.iter().filter_map(Value::as_str).any(|entry| {
                    let normalized = entry.to_ascii_lowercase();
                    normalized.contains("read_file")
                        || normalized.contains("verified against current source")
                        || normalized.contains("validated against current source")
                        || (normalized.contains("source validation")
                            && !normalized.contains("without source validation")
                            && !normalized.contains("no source validation"))
                })
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
fn violations_are_verified_and_empty(raw_violations_text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(raw_violations_text) else {
        return false;
    };
    value.get("status").and_then(Value::as_str) == Some("verified")
        && value
            .get("violations")
            .and_then(Value::as_array)
            .map(|violations| violations.is_empty())
            .unwrap_or(false)
}

pub(crate) fn reconcile_diagnostics_report(workspace: &Path) -> String {
    render_diagnostics_report_from_issues(workspace)
}

#[cfg(test)]
pub(crate) fn sanitize_diagnostics_for_planner(
    raw_diagnostics_text: &str,
    raw_violations_text: &str,
) -> String {
    if raw_diagnostics_text.trim().is_empty() {
        return "(no diagnostics)".to_string();
    }

    let Ok(value) = serde_json::from_str::<Value>(raw_diagnostics_text) else {
        return "(invalid diagnostics: not valid json)".to_string();
    };

    let Some(ranked_failures) = value.get("ranked_failures").and_then(Value::as_array) else {
        return "(invalid diagnostics: missing ranked_failures)".to_string();
    };

    if ranked_failures.is_empty() {
        return "(no active diagnostics failures)".to_string();
    }

    if violations_are_verified_and_empty(raw_violations_text) {
        return "(suppressed stale diagnostics: verifier state is authoritative and VIOLATIONS.json is verified with no active violations)".to_string();
    }

    if diagnostics_have_current_source_validation(ranked_failures) {
        return format!(
            "(SOURCE-VALIDATED DIAGNOSTICS — current-source evidence is present; still verify before creating tasks)\n{}",
            raw_diagnostics_text
        );
    }

    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("Diagnostics failures suppressed until current-source validation is recorded.");
    format!(
        "(suppressed stale or unverified diagnostics: ranked_failures present without current-source validation evidence)\n{}",
        summary
    )
}

pub fn lane_summary_text(lanes: &[LaneConfig], verifier_summary: &[String]) -> String {
    lanes
        .iter()
        .map(|lane| format!("{}={}", lane.label, verifier_summary[lane.index]))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn load_executor_diff_inputs(
    workspace: &Path,
    last_executor_diff: &mut String,
    max_lines: usize,
) -> ExecutorDiffInputs {
    let current_executor_diff = executor_diff(workspace, max_lines);
    let diff_text = diff_since_last_cycle(&current_executor_diff, last_executor_diff);
    *last_executor_diff = current_executor_diff;
    ExecutorDiffInputs { diff_text }
}

fn planner_objectives_text(workspace: &Path) -> String {
    let objectives_full = crate::objectives::read_objectives_compact_for_workspace(workspace);
    // Hard cap to prevent planner prompt overflow (top-N / truncation strategy)
    if objectives_full.len() > 8000 {
        let mut truncated = objectives_full.chars().take(8000).collect::<String>();
        truncated.push_str("\n... (objectives truncated for prompt size)");
        truncated
    } else {
        objectives_full
    }
}

fn planner_plan_texts(master_plan_path: &Path, last_plan_text: &str) -> (String, String) {
    let plan_text = read_text_or_empty(master_plan_path);
    let plan_diff_text = plan_diff(last_plan_text, &plan_text, 400);
    (plan_text, plan_diff_text)
}

fn planner_enforced_invariants_text(workspace: &Path) -> String {
    let raw = crate::invariants::read_enforced_invariants(workspace);
    summarize_enforced_invariants_for_prompt(&raw)
}

pub fn load_planner_inputs(
    lanes: &[LaneConfig],
    workspace: &Path,
    verifier_summary: &[String],
    last_plan_text: &str,
    last_executor_diff: &mut String,
    cargo_test_failures: String,
    master_plan_path: &Path,
    semantic_control_snapshot_hash_path: &Path,
) -> PlannerInputs {
    let summary_text = lane_summary_text(lanes, verifier_summary);
    let executor_diff_text =
        load_executor_diff_inputs(workspace, last_executor_diff, 400).diff_text;
    let lessons_text = read_lessons_or_empty(workspace);
    let objectives_text = planner_objectives_text(workspace);
    let enforced_invariants_text = planner_enforced_invariants_text(workspace);
    let semantic_control = derive_semantic_control_prompt_state_with_delta(
        workspace,
        10,
        semantic_control_snapshot_hash_path,
    );
    let (plan_text, plan_diff_text) = planner_plan_texts(master_plan_path, last_plan_text);
    PlannerInputs {
        summary_text,
        executor_diff_text,
        cargo_test_failures,
        lessons_text,
        objectives_text,
        enforced_invariants_text,
        semantic_control_text: semantic_control.control_summary,
        plan_text,
        plan_diff_text,
    }
}

pub enum SingleRoleRead {
    Objectives,
    SemanticControl,
    Lessons,
    MasterPlan,
    Spec,
}

impl SingleRoleContext<'_> {
    pub fn read(&self, kind: SingleRoleRead) -> Result<String> {
        let text = match kind {
            SingleRoleRead::Objectives => {
                crate::objectives::read_objectives_compact_for_workspace(self.workspace)
            }
            SingleRoleRead::SemanticControl => {
                read_semantic_control_prompt_context(self.workspace, 10)
            }
            SingleRoleRead::Lessons => read_lessons_or_empty(self.workspace),
            SingleRoleRead::MasterPlan => {
                filter_pending_plan_json(&read_text_or_empty(self.master_plan_path))
            }
            SingleRoleRead::Spec => read_required_text(self.spec_path, SPEC_FILE)?,
        };
        Ok(text)
    }

    // removed lane_plan_list method (lane plans deleted)
}

pub fn load_single_role_inputs(
    ctx: &SingleRoleContext<'_>,
    is_verifier: bool,
    _is_diagnostics: bool,
    is_planner: bool,
) -> Result<SingleRoleInputs> {
    let (role, prompt_kind) = if is_verifier || is_planner {
        ("mini_planner", AgentPromptKind::Planner)
    } else {
        ("executor", AgentPromptKind::Executor)
    };

    let primary_input_path = if is_verifier || is_planner {
        ctx.spec_path
    } else {
        ctx.master_plan_path
    };
    let primary_input_name = if is_verifier || is_planner {
        SPEC_FILE.to_string()
    } else {
        MASTER_PLAN_FILE.to_string()
    };
    let primary_input = read_required_text(primary_input_path, &primary_input_name)?;
    if primary_input.trim().is_empty() {
        bail!("input file is empty — write content into {primary_input_name} before running");
    }

    Ok(SingleRoleInputs {
        role: role.to_string(),
        prompt_kind,
        primary_input,
    })
}

pub fn build_single_role_prompt(
    ctx: &SingleRoleContext<'_>,
    inputs: &SingleRoleInputs,
    cargo_test_failures: &str,
) -> Result<String> {
    let prompt = match inputs.prompt_kind {
        AgentPromptKind::Planner => build_planner_role_prompt(ctx, inputs, cargo_test_failures)?,
        AgentPromptKind::Executor => build_executor_role_prompt(ctx)?,
        AgentPromptKind::Solo => {
            bail!("solo role is only supported in orchestration mode")
        }
    };
    Ok(prompt)
}

fn build_planner_role_prompt(
    ctx: &SingleRoleContext<'_>,
    inputs: &SingleRoleInputs,
    cargo_test_failures: &str,
) -> Result<String> {
    let lessons = ctx.read(SingleRoleRead::Lessons)?;
    let objectives = ctx.read(SingleRoleRead::Objectives)?;
    let enforced_invariants = planner_enforced_invariants_text(ctx.workspace);
    let semantic_control = ctx.read(SingleRoleRead::SemanticControl)?;
    Ok(single_role_planner_prompt(
        &inputs.primary_input,
        &objectives,
        &lessons,
        &enforced_invariants,
        &semantic_control,
        cargo_test_failures,
    ))
}

fn build_executor_role_prompt(ctx: &SingleRoleContext<'_>) -> Result<String> {
    let (spec, master_plan, semantic_control) = executor_role_prompt_inputs(ctx)?;
    Ok(single_role_executor_prompt(
        &spec,
        &master_plan,
        &semantic_control,
    ))
}

fn executor_role_prompt_inputs(ctx: &SingleRoleContext<'_>) -> Result<(String, String, String)> {
    Ok((
        ctx.read(SingleRoleRead::Spec)?,
        ctx.read(SingleRoleRead::MasterPlan)?,
        ctx.read(SingleRoleRead::SemanticControl)?,
    ))
}

fn executor_diff_unavailable(reason: &str) -> String {
    format!("(executor diff unavailable: {reason})")
}

fn plan_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    if old_text.is_empty() {
        let mut out = format!("+++ {} (initial)\n", MASTER_PLAN_FILE);
        for (idx, line) in new_text.lines().enumerate() {
            if idx >= max_lines {
                out.push_str("... (truncated)\n");
                break;
            }
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }
    if old_text == new_text {
        return "(no changes)".to_string();
    }
    let mut out = String::new();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut i = 0usize;
    let mut j = 0usize;
    let mut emitted = 0usize;
    while i < old_lines.len() || j < new_lines.len() {
        if emitted >= max_lines {
            out.push_str("... (truncated)\n");
            break;
        }
        match (old_lines.get(i), new_lines.get(j)) {
            (Some(ol), Some(nl)) if ol == nl => {
                i += 1;
                j += 1;
            }
            (Some(ol), Some(nl)) => {
                out.push_str("- ");
                out.push_str(ol);
                out.push('\n');
                out.push_str("+ ");
                out.push_str(nl);
                out.push('\n');
                i += 1;
                j += 1;
                emitted += 2;
            }
            (Some(ol), None) => {
                out.push_str("- ");
                out.push_str(ol);
                out.push('\n');
                i += 1;
                emitted += 1;
            }
            (None, Some(nl)) => {
                out.push_str("+ ");
                out.push_str(nl);
                out.push('\n');
                j += 1;
                emitted += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn diff_since_last_cycle(current: &str, last: &str) -> String {
    if current.trim().is_empty() {
        return "(no changes)".to_string();
    }
    if current == last {
        return "(no changes)".to_string();
    }
    if last.trim().is_empty() {
        return current.to_string();
    }
    if current.starts_with("(") {
        return current.to_string();
    }
    let last_lines: std::collections::HashSet<&str> = last.lines().collect();
    let mut out_lines = Vec::new();
    for line in current.lines() {
        if !last_lines.contains(line) {
            out_lines.push(line);
        }
    }
    if out_lines.is_empty() {
        "(no changes)".to_string()
    } else {
        let mut out = out_lines.join("\n");
        out.push('\n');
        out
    }
}

fn executor_diff(workspace: &Path, max_lines: usize) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(workspace).args(["diff", "--name-only"]);
    let Ok(output) = cmd.output() else {
        return executor_diff_unavailable("failed to run git diff --name-only");
    };
    if !output.status.success() {
        return executor_diff_unavailable("git diff --name-only failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let files = executor_diff_files(&text);
    if files.is_empty() {
        return "(no executor diff)".to_string();
    }
    let mut diff_cmd = std::process::Command::new("git");
    diff_cmd
        .current_dir(workspace)
        .arg("diff")
        .arg("--unified=3")
        .arg("--")
        .args(&files);
    let Ok(diff_out) = diff_cmd.output() else {
        return executor_diff_unavailable("failed to run git diff");
    };
    if !diff_out.status.success() {
        return executor_diff_unavailable("git diff failed");
    }
    let diff_text = String::from_utf8_lossy(&diff_out.stdout);
    if diff_text.trim().is_empty() {
        return "(no executor diff)".to_string();
    }
    render_executor_diff(&diff_text, max_lines)
}

fn executor_diff_files<'a>(text: &'a str) -> Vec<&'a str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_executor_diff_excluded(line))
        .collect()
}

fn is_executor_diff_excluded(line: &str) -> bool {
    line.starts_with(MASTER_PLAN_FILE)
        || line.starts_with("PLAN.md")
        || line.starts_with("PLANS/")
        || line.starts_with("agent_state/")
        || line == ISSUES_FILE
        || line == VIOLATIONS_FILE
        || line == "DIAGNOSTICS.json"
}

fn render_executor_diff(diff_text: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (idx, line) in diff_text.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... (truncated)\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod diagnostics_filter_tests {
    use super::{
        derive_semantic_prompt_artifacts, filter_active_diagnostics_json,
        filter_active_violations_json, filter_pending_plan_json,
        load_diagnostics_projection_text, render_diagnostics_report_from_issues,
        sanitize_diagnostics_for_planner,
    };
    use crate::constants::{MASTER_PLAN_FILE, OBJECTIVES_FILE, VIOLATIONS_FILE};
    use crate::events::{EffectEvent, Event};
    use crate::reports::{DiagnosticsFinding, DiagnosticsReport, Impact};

    const NON_AUTHORITATIVE_VIOLATIONS: &str = r#"{}"#;

    const VERIFIED_EMPTY_VIOLATIONS: &str = r#"{
  "status": "verified",
  "summary": "no current violations",
  "violations": []
}"#;

    #[test]
    fn sanitize_diagnostics_suppresses_unverified_ranked_failures() {
        let raw = r#"{
  "status": "critical_failure",
  "summary": "diagnostics found a stale issue",
  "ranked_failures": [
    {
      "id": "D1",
      "evidence": ["old report without source validation"]
    }
  ]
}"#;

        let sanitized = sanitize_diagnostics_for_planner(raw, NON_AUTHORITATIVE_VIOLATIONS);
        assert!(sanitized.contains("suppressed stale or unverified diagnostics"));
        assert!(sanitized.contains("diagnostics found a stale issue"));
    }

    #[test]
    fn sanitize_diagnostics_allows_source_validated_failures() {
        let raw = r#"{
  "status": "critical_failure",
  "summary": "validated diagnostics",
  "ranked_failures": [
    {
      "id": "D1",
      "evidence": ["read_file src/app.rs verified against current source"]
    }
  ]
}"#;

        let sanitized = sanitize_diagnostics_for_planner(raw, NON_AUTHORITATIVE_VIOLATIONS);
        assert!(sanitized.contains("SOURCE-VALIDATED DIAGNOSTICS"));
        assert!(sanitized.contains("validated diagnostics"));
    }

    #[test]
    fn sanitize_diagnostics_suppresses_when_violations_are_verified_and_empty() {
        let raw = r#"{
  "status": "needs_repair",
  "summary": "stale contradiction remains in persisted diagnostics",
  "ranked_failures": [
    {
      "id": "D1",
      "evidence": ["read_file VIOLATIONS.json:1-5 verified against current source"]
    }
  ]
}"#;

        let sanitized = sanitize_diagnostics_for_planner(raw, VERIFIED_EMPTY_VIOLATIONS);
        assert!(sanitized.contains("suppressed stale diagnostics"));
        assert!(sanitized.contains("VIOLATIONS.json is verified with no active violations"));
    }

    #[test]
    fn render_diagnostics_report_merges_issues_and_violations() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "canon-mini-agent-diagnostics-render-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(workspace.join("agent_state")).unwrap();
        fs::write(
            workspace.join(crate::constants::ISSUES_FILE),
            r#"{
  "version": 1,
  "issues": [
    {
      "id": "ISS-001",
      "title": "Planner drift",
      "status": "open",
      "priority": "high",
      "kind": "logic",
      "description": "Planner is reading stale diagnostics.",
      "location": "src/app.rs:900",
      "evidence": ["read_file src/app.rs:900-940 — confirmed stale diagnostics flow"]
    }
  ]
}"#,
        )
        .unwrap();
        fs::write(
            workspace.join(crate::constants::VIOLATIONS_FILE),
            r#"{
  "status": "needs_repair",
  "summary": "active violation present",
  "violations": [
    {
      "id": "V1",
      "title": "Diagnostics cache drift",
      "severity": "critical",
      "evidence": ["read_file VIOLATIONS.json:1-20 — confirmed cache drift"],
      "issue": "Diagnostics output no longer matches current issues.",
      "impact": "planner receives stale diagnostics.",
      "required_fix": ["src/prompt_inputs.rs"],
      "files": ["src/prompt_inputs.rs"]
    }
  ]
}"#,
        )
        .unwrap();

        let rendered = render_diagnostics_report_from_issues(workspace.as_path());
        assert!(rendered.contains("\"status\": \"critical_failure\""));
        assert!(rendered.contains("\"ISS-001\""));
        assert!(rendered.contains("\"V1\""));
        assert!(rendered.contains("source-validation"));
        assert!(rendered.contains("Planner drift"));
    }

    #[test]
    fn derive_semantic_prompt_artifacts_recovers_diagnostics_from_tlog_when_projection_missing() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "canon-mini-agent-diagnostics-tlog-fallback-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(workspace.join("agent_state")).unwrap();

        let report = DiagnosticsReport {
            status: "needs_repair".to_string(),
            inputs_scanned: vec![format!(
                "{} (open issues: 1)",
                crate::constants::ISSUES_FILE
            )],
            ranked_failures: vec![DiagnosticsFinding {
                id: "D-TLOG".to_string(),
                impact: Impact::High,
                signal: "Tlog recovered diagnostics".to_string(),
                evidence: vec![
                    "read_file src/app.rs:1-20 — recovered from tlog snapshot".to_string()
                ],
                root_cause: "projection missing during prompt assembly".to_string(),
                repair_targets: vec!["src/app.rs".to_string()],
            }],
            planner_handoff: vec![
                "Use the recovered diagnostics snapshot until projection rebuild completes."
                    .to_string(),
            ],
        };

        let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
        let mut tlog = crate::tlog::Tlog::open(&tlog_path);
        tlog.append(&Event::effect(EffectEvent::DiagnosticsReportRecorded {
            report: report.clone(),
        }))
        .unwrap();

        let recovered = load_diagnostics_projection_text(workspace.as_path()).unwrap();
        assert!(recovered.contains("\"D-TLOG\""));

        let artifacts = derive_semantic_prompt_artifacts(workspace.as_path(), 3);
        assert!(artifacts.diagnostics_report.contains("\"D-TLOG\""));
        assert!(artifacts
            .diagnostics_summary
            .contains("Tlog recovered diagnostics"));
    }

    #[test]
    fn filter_pending_plan_json_removes_done_tasks() {
        let raw = r#"{
  "version": 1,
  "status": "in_progress",
  "tasks": [
    {"id": "T1", "status": "done"},
    {"id": "T2", "status": "todo"}
  ],
  "dag": { "edges": [ {"from":"T1","to":"T2"}, {"from":"T2","to":"T1"} ] }
}"#;
        let filtered = filter_pending_plan_json(raw);
        assert!(filtered.contains("\"id\": \"T2\""));
        assert!(!filtered.contains("\"id\": \"T1\""));
        assert!(!filtered.contains("\"from\": \"T1\""));
    }

    #[test]
    fn filter_pending_plan_json_reports_none_when_all_done() {
        let raw = r#"{
  "tasks": [
    {"id":"T1","status":"done"},
    {"id":"T2","status":"complete"}
  ]
}"#;
        assert_eq!(filter_pending_plan_json(raw), "(no pending plan tasks)");
    }

    #[test]
    fn filter_active_violations_json_reports_none_when_empty() {
        let raw = r#"{"status":"verified","violations":[]}"#;
        assert!(filter_active_violations_json(raw).is_empty());
    }

    #[test]
    fn filter_active_diagnostics_json_reports_none_when_empty() {
        let raw = r#"{"status":"verified","ranked_failures":[]}"#;
        assert!(filter_active_diagnostics_json(raw).is_empty());
    }

    #[test]
    fn build_single_role_prompt_planner_includes_rendered_lessons_from_context() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "canon-mini-agent-single-role-planner-lessons-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(workspace.join("agent_state/default")).unwrap();
        fs::create_dir_all(workspace.join("agent_state")).unwrap();

        fs::write(workspace.join("SPEC.md"), "planner spec body").unwrap();
        fs::write(
            workspace.join(OBJECTIVES_FILE),
            r#"{"version":1,"objectives":[{"id":"obj_15","title":"OBJ-15","status":"active"}]}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("INVARIANTS.json"),
            r#"{"version":1,"invariants":[]}"#,
        )
        .unwrap();
        fs::write(
            workspace.join(VIOLATIONS_FILE),
            r#"{"status":"verified","violations":[]}"#,
        )
        .unwrap();
        fs::write(
            workspace.join(crate::constants::diagnostics_file()),
            r#"{"status":"verified","ranked_failures":[]}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("agent_state/lessons.json"),
            r#"{
  "summary": "Structured planner lesson summary.",
  "failures": ["Missing writeback coverage"],
  "fixes": ["Add planner-side regression"],
  "required_actions": ["Validate shared prompt-load path"]
}"#,
        )
        .unwrap();

        let spec_path = workspace.join("SPEC.md");
        let master_plan_path = workspace.join(MASTER_PLAN_FILE);
        let violations_path = workspace.join(VIOLATIONS_FILE);
        fs::write(&master_plan_path, r#"{"version":2,"tasks":[]}"#).unwrap();
        fs::write(&violations_path, r#"{"status":"verified","violations":[]}"#).unwrap();

        let ctx = super::SingleRoleContext {
            workspace: workspace.as_path(),
            spec_path: spec_path.as_path(),
            master_plan_path: master_plan_path.as_path(),
        };

        let inputs = super::load_single_role_inputs(&ctx, false, false, true).unwrap();
        let prompt = super::build_single_role_prompt(&ctx, &inputs, "").unwrap();

        assert!(prompt.contains("Summary:\nStructured planner lesson summary."));
        assert!(prompt.contains("Failures:\n- Missing writeback coverage"));
        assert!(prompt.contains("Fixes:\n- Add planner-side regression"));
        assert!(prompt.contains("Required actions:\n- Validate shared prompt-load path"));
    }
}

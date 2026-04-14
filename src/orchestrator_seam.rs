use crate::canonical_writer::CanonicalWriter;
use crate::events::ControlEvent;
use crate::prompt_inputs::reconcile_diagnostics_report;
use crate::system_state::SystemState;
use crate::tlog::Tlog;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct OrchestratorProbeResult {
    pub tlog_path: PathBuf,
    pub live_state: SystemState,
    pub replayed_state: SystemState,
}

fn new_probe_writer(workspace: &Path) -> (CanonicalWriter, PathBuf, SystemState) {
    let initial = SystemState::new(&[0], 1);
    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let writer = CanonicalWriter::new(
        initial.clone(),
        Tlog::open(&tlog_path),
        workspace.to_path_buf(),
    );
    (writer, tlog_path, initial)
}

fn finish_probe(
    writer: CanonicalWriter,
    tlog_path: PathBuf,
    initial: SystemState,
) -> Result<OrchestratorProbeResult> {
    let replayed_state = Tlog::replay(&tlog_path, initial)?;
    Ok(OrchestratorProbeResult {
        tlog_path,
        live_state: writer.state().clone(),
        replayed_state,
    })
}

fn plan_has_incomplete_tasks(plan_text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(plan_text) else {
        return true;
    };
    value
        .get("tasks")
        .and_then(Value::as_array)
        .map(|tasks| {
            tasks.iter().any(|task| {
                task.get("status")
                    .and_then(Value::as_str)
                    .map(|status| status != "done")
                    .unwrap_or(true)
            })
        })
        .unwrap_or(true)
}

fn has_actionable_objectives(objectives_text: &str) -> bool {
    let Ok(file) = serde_json::from_str::<crate::objectives::ObjectivesFile>(objectives_text)
    else {
        return false;
    };
    file.objectives
        .iter()
        .any(|objective| !crate::objectives::is_completed(objective))
}

pub fn probe_diagnostics_reconciliation(
    workspace: &Path,
    diagnostics_path: &Path,
    violations_path: &Path,
) -> Result<OrchestratorProbeResult> {
    let (mut writer, tlog_path, initial) = new_probe_writer(workspace);
    let raw_diagnostics_text = std::fs::read_to_string(diagnostics_path).unwrap_or_default();
    let raw_violations_text = std::fs::read_to_string(violations_path).unwrap_or_default();
    let reconciled_diagnostics_text = reconcile_diagnostics_report(workspace, &raw_violations_text);
    if reconciled_diagnostics_text != raw_diagnostics_text {
        std::fs::write(diagnostics_path, &reconciled_diagnostics_text)?;
        writer.apply(ControlEvent::DiagnosticsReconciliationQueued);
    }
    finish_probe(writer, tlog_path, initial)
}

pub fn probe_verifier_followup(workspace: &Path) -> Result<OrchestratorProbeResult> {
    let (mut writer, tlog_path, initial) = new_probe_writer(workspace);
    writer.apply(ControlEvent::DiagnosticsVerifierFollowupQueued);
    finish_probe(writer, tlog_path, initial)
}

pub fn probe_planner_objective_review(
    workspace: &Path,
    objectives_path: &Path,
    plan_path: &Path,
    diagnostics_path: &Path,
) -> Result<OrchestratorProbeResult> {
    let (mut writer, tlog_path, initial) = new_probe_writer(workspace);

    // Read bytes before any writes so we can detect changes by content, not mtime.
    // Mtime-based detection is unreliable on tmpfs/overlayfs (granularity > 2ms).
    let objectives_bytes_before = std::fs::read(objectives_path).unwrap_or_default();
    let plan_bytes_before = std::fs::read(plan_path).unwrap_or_default();
    let diagnostics_bytes_before = std::fs::read(diagnostics_path).unwrap_or_default();

    if !plan_path.exists() {
        std::fs::write(plan_path, "{\"version\":2,\"tasks\":[]}\n")?;
    }

    let objectives_bytes_after = std::fs::read(objectives_path).unwrap_or_default();
    let plan_bytes_after = std::fs::read(plan_path).unwrap_or_default();
    let diagnostics_bytes_after = std::fs::read(diagnostics_path).unwrap_or_default();

    let objective_review_required = plan_bytes_before != plan_bytes_after
        || diagnostics_bytes_before != diagnostics_bytes_after;
    let objectives_updated = objectives_bytes_before != objectives_bytes_after;

    if objective_review_required && !objectives_updated {
        writer.apply(ControlEvent::PlannerObjectiveReviewQueued);
    }
    finish_probe(writer, tlog_path, initial)
}

pub fn probe_planner_plan_gap(
    workspace: &Path,
    objectives_path: &Path,
    plan_path: &Path,
) -> Result<OrchestratorProbeResult> {
    let (mut writer, tlog_path, initial) = new_probe_writer(workspace);
    let objectives_text = std::fs::read_to_string(objectives_path).unwrap_or_default();
    let plan_text = std::fs::read_to_string(plan_path).unwrap_or_default();
    if has_actionable_objectives(&objectives_text) && !plan_has_incomplete_tasks(&plan_text) {
        writer.apply(ControlEvent::PlannerObjectivePlanGapQueued);
    }
    finish_probe(writer, tlog_path, initial)
}

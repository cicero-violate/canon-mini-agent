fn is_chromium_transport_error(err_text: &str) -> bool {
    err_text.contains("chromium: early transport failure")
        || err_text.contains("chromium: timeout waiting for SUBMIT_ACK")
        || err_text.contains("chromium: timeout waiting for response")
}

#[derive(Clone)]
struct ShutdownSignal {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

static SHUTDOWN_SIGNAL: OnceLock<ShutdownSignal> = OnceLock::new();

fn shutdown_signal_cell() -> &'static OnceLock<ShutdownSignal> {
    &SHUTDOWN_SIGNAL
}

fn init_shutdown_signal() -> ShutdownSignal {
    shutdown_signal_cell()
        .get_or_init(|| ShutdownSignal {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        })
        .clone()
}

fn shutdown_signal() -> Option<ShutdownSignal> {
    shutdown_signal_cell().get().cloned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CheckpointLane {
    lane_id: usize,
    lane_label: String,
    plan_text: String,
    pending: bool,
    in_progress_by: Option<String>,
    latest_verifier_result: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResumeVerifierItem {
    lane_id: usize,
    lane_label: String,
    lane_plan_file: String,
    final_exec_result: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrchestratorCheckpoint {
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    checkpoint_tlog_seq: u64,
    created_ms: u64,
    phase: String,
    phase_lane: Option<usize>,
    planner_pending: bool,
    diagnostics_pending: bool,
    diagnostics_text: String,
    last_plan_text: String,
    last_executor_diff: String,
    #[serde(default)]
    last_solo_plan_text: String,
    #[serde(default)]
    last_solo_executor_diff: String,
    lanes: Vec<CheckpointLane>,
    verifier_summary: Vec<String>,
    verifier_pending_results: Vec<ResumeVerifierItem>,
}

fn checkpoint_path(_workspace: &Path) -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir()).join("mini_agent_checkpoint.json")
}

fn cycle_idle_marker_path() -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir()).join("orchestrator_cycle_idle.flag")
}

fn orchestrator_mode_flag_path() -> PathBuf {
    PathBuf::from(crate::constants::agent_state_dir()).join("orchestrator_mode.flag")
}

fn artifact_signature(parts: &[&str]) -> String {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn checkpoint_artifact_path(path: &Path) -> String {
    let workspace = Path::new(crate::constants::workspace());
    path.strip_prefix(workspace).ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
}

fn checkpoint_ref_json(path: &Path, checkpoint_json: &str, tlog_seq: u64) -> String {
    let artifact = checkpoint_artifact_path(path);
    let hash = artifact_signature(&[artifact.as_str(), checkpoint_json, &tlog_seq.to_string()]);
    serde_json::json!({"checkpoint_ref":true,"path":artifact,"bytes":checkpoint_json.len(),"hash":hash,"tlog_seq":tlog_seq}).to_string()
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &str, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: fs_write, logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn persist_agent_state_projection(path: &Path, contents: &str, subject: &str) -> Result<()> {
    let workspace = Path::new(crate::constants::workspace());
    let artifact = path
        .strip_prefix(workspace)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"));
    let target = path.to_string_lossy().into_owned();
    let signature = artifact_signature(&[artifact.as_str(), subject, &contents.len().to_string()]);
    crate::logging::record_workspace_artifact_effect(
        workspace, true, &artifact, "write", &target, subject, &signature,
    )?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, path)?;
    crate::logging::record_workspace_artifact_effect(
        workspace, false, &artifact, "write", &target, subject, &signature,
    )?;
    Ok(())
}

/// Intent: canonical_write
/// Resource: error
/// Inputs: &std::path::Path, &mut canonical_writer::CanonicalWriter, &[prompt_inputs::LaneConfig], &std::collections::VecDeque<(app::SubmittedExecutorTurn, u64, std::string::String
/// Outputs: ()
/// Effects: fs_write
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn save_checkpoint(
    workspace: &Path,
    writer: &mut CanonicalWriter,
    lanes: &[LaneConfig],
    verifier_pending_results: &VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> Result<()> {
    let state = writer.state().clone();
    let path = checkpoint_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Record the canonical checkpoint-save effect before materializing the file.
    // The log must lead the side effect so replay can observe the save attempt
    // in the same order the runtime produced it.
    writer.try_record_effect(crate::events::EffectEvent::CheckpointSaved {
        phase: state.phase.clone(),
    })?;
    let lane_snapshots = build_checkpoint_lane_snapshots(&state, lanes);
    let resume_items = build_resume_verifier_items(lanes, verifier_pending_results);
    let checkpoint = OrchestratorCheckpoint {
        workspace: workspace.to_string_lossy().into_owned(),
        checkpoint_tlog_seq: writer.tlog_seq(),
        created_ms: now_ms(),
        phase: state.phase.clone(),
        phase_lane: state.phase_lane,
        planner_pending: state.planner_pending,
        diagnostics_pending: state.diagnostics_pending,
        diagnostics_text: state.diagnostics_text.clone(),
        last_plan_text: state.last_plan_text.clone(),
        last_executor_diff: state.last_executor_diff.clone(),
        last_solo_plan_text: state.last_solo_plan_text.clone(),
        last_solo_executor_diff: state.last_solo_executor_diff.clone(),
        lanes: lane_snapshots,
        verifier_summary: state.verifier_summary.clone(),
        verifier_pending_results: resume_items,
    };
    if let Ok(snapshot_json) = serde_json::to_string(&checkpoint) {
        writer.apply(ControlEvent::CheckpointSnapshotSet {
            snapshot_json: checkpoint_ref_json(&path, &snapshot_json, writer.tlog_seq()),
        });
    }
    // Use a plain atomic write instead of persist_agent_state_projection.
    // persist_agent_state_projection records two artifact-write tlog events (start + end)
    // AFTER checkpoint_tlog_seq is captured, causing checkpoint_tlog_seq to always lag
    // the tlog by 2 on the next restart, making every checkpoint appear diverged and
    // getting discarded permanently.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&checkpoint)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &system_state::SystemState, &[prompt_inputs::LaneConfig]
/// Outputs: std::vec::Vec<app::CheckpointLane>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_checkpoint_lane_snapshots(
    state: &SystemState,
    lanes: &[LaneConfig],
) -> Vec<CheckpointLane> {
    let mut lane_snapshots = Vec::new();
    for lane in lanes {
        if let Some(ls) = state.lanes.get(&lane.index) {
            lane_snapshots.push(CheckpointLane {
                lane_id: lane.index,
                lane_label: lane.label.clone(),
                plan_text: ls.plan_text.clone(),
                pending: ls.pending,
                in_progress_by: ls.in_progress_by.clone(),
                latest_verifier_result: ls.latest_verifier_result.clone(),
            });
        }
    }
    lane_snapshots
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &[prompt_inputs::LaneConfig], &std::collections::VecDeque<(app::SubmittedExecutorTurn, u64, std::string::String
/// Outputs: ()
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn build_resume_verifier_items(
    lanes: &[LaneConfig],
    verifier_pending_results: &VecDeque<(SubmittedExecutorTurn, u64, String)>,
) -> Vec<ResumeVerifierItem> {
    let mut resume_items = Vec::new();
    for (submitted, _turn_id, final_exec_result) in verifier_pending_results.iter() {
        resume_items.push(ResumeVerifierItem {
            lane_id: submitted.lane,
            lane_label: submitted.lane_label.clone(),
            lane_plan_file: lanes
                .get(submitted.lane)
                .map(|lane| lane.plan_file.clone())
                .unwrap_or_default(),
            final_exec_result: final_exec_result.clone(),
        });
    }
    resume_items
}

fn recover_verifier_item_from_executor_post_restart(
    lanes: &[LaneConfig],
) -> Option<ResumeVerifierItem> {
    let resume = peek_post_restart_result("executor")?;
    if resume.action != "apply_patch" || !resume.result.contains("apply_patch ok") {
        return None;
    }
    let lane = lanes
        .iter()
        .find(|lane| lane.endpoint.id == resume.endpoint_id)
        .or_else(|| (lanes.len() == 1).then(|| &lanes[0]))?;
    let _ = take_post_restart_result("executor");
    Some(ResumeVerifierItem {
        lane_id: lane.index,
        lane_label: lane.label.clone(),
        lane_plan_file: lane.plan_file.clone(),
        final_exec_result: resume.result,
    })
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path
/// Outputs: std::option::Option<app::OrchestratorCheckpoint>
/// Effects: fs_read, logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn load_checkpoint(workspace: &Path) -> Option<OrchestratorCheckpoint> {
    let tlog_path = PathBuf::from(crate::constants::agent_state_dir()).join("tlog.ndjson");
    if tlog_path.exists() {
        if let Ok(state) = Tlog::replay(&tlog_path, SystemState::new(&[], 0)) {
            let raw = state.checkpoint_snapshot_json.trim();
            if !raw.is_empty() {
                if let Ok(cp) = serde_json::from_str::<OrchestratorCheckpoint>(raw) {
                    if cp.workspace.is_empty() || cp.workspace == workspace.to_string_lossy().as_ref()
                    {
                        return Some(cp);
                    }
                }
            }
        }
    }
    let path = checkpoint_path(workspace);
    let raw = std::fs::read_to_string(path).ok()?;
    let cp: OrchestratorCheckpoint = serde_json::from_str(&raw).ok()?;
    if cp.workspace.is_empty() || cp.workspace != workspace.to_string_lossy().as_ref() {
        let msg = format!(
            "checkpoint/runtime divergence: checkpoint workspace mismatch (stored={} current={})",
            cp.workspace,
            workspace.display()
        );
        eprintln!(
            "[orchestrate] checkpoint workspace mismatch (stored={} current={}) — discarding",
            cp.workspace,
            workspace.display()
        );
        crate::blockers::record_action_failure_with_writer(
            workspace,
            None,
            "orchestrate",
            "checkpoint_runtime_divergence",
            &msg,
            None,
        );
        return None;
    }
    if cp.checkpoint_tlog_seq > 0 {
        let tlog_path = PathBuf::from(crate::constants::agent_state_dir()).join("tlog.ndjson");
        let current_tlog_seq = crate::tlog::Tlog::open(&tlog_path).seq();
        // A newer tlog than checkpoint is expected: replay applies delta events
        // after the checkpoint snapshot. Only reject impossible "future checkpoint"
        // states where the checkpoint seq is ahead of the current tlog.
        if cp.checkpoint_tlog_seq > current_tlog_seq {
            let msg = format!(
                "checkpoint/runtime divergence: checkpoint seq {} is ahead of tlog seq {}",
                cp.checkpoint_tlog_seq, current_tlog_seq
            );
            eprintln!(
                "[orchestrate] checkpoint seq {} is ahead of current tlog seq {} — discarding",
                cp.checkpoint_tlog_seq, current_tlog_seq
            );
            crate::blockers::record_action_failure_with_writer(
                workspace,
                None,
                "orchestrate",
                "checkpoint_runtime_divergence",
                &msg,
                None,
            );
            return None;
        }
    }
    Some(cp)
}

#[derive(Debug, Default, Clone, Copy)]
struct ExecutorProgressSignals {
    last_progress_seq: Option<u64>,
    last_progress_ts_ms: Option<u64>,
    checkpoint_divergence_blockers_recent: usize,
}

/// Intent: canonical_read
/// Resource: error
/// Inputs: &std::path::Path, u64
/// Outputs: app::ExecutorProgressSignals
/// Effects: logging
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn read_executor_progress_signals(workspace: &Path, now_ms: u64) -> ExecutorProgressSignals {
    const SIGNAL_LOOKBACK_RECORDS: usize = 800;
    const DIVERGENCE_WINDOW_MS: u64 = 120_000;

    let tlog_path = workspace.join("agent_state").join("tlog.ndjson");
    let Ok(records) = crate::tlog::Tlog::read_records(&tlog_path) else {
        return ExecutorProgressSignals::default();
    };
    let start = records.len().saturating_sub(SIGNAL_LOOKBACK_RECORDS);
    let mut signals = ExecutorProgressSignals::default();

    for record in records[start..].iter().rev() {
        match &record.event {
            Event::Control { event } => match event {
                ControlEvent::ExecutorTurnRegistered { .. }
                | ControlEvent::ExecutorTurnDeregistered { .. }
                | ControlEvent::ExecutorCompletionRecovered { .. }
                | ControlEvent::ExecutorCompletionTabRebound { .. }
                | ControlEvent::ExecutorSubmitAckTabRebound { .. } => {
                    if signals.last_progress_seq.is_none() {
                        signals.last_progress_seq = Some(record.seq);
                        signals.last_progress_ts_ms = Some(record.ts_ms);
                    }
                }
                _ => {}
            },
            Event::Effect { event } => match event {
                EffectEvent::LlmTurnOutput { role, .. }
                | EffectEvent::ActionResultRecorded { role, .. } => {
                    if role.contains("executor") && signals.last_progress_seq.is_none() {
                        signals.last_progress_seq = Some(record.seq);
                        signals.last_progress_ts_ms = Some(record.ts_ms);
                    }
                }
                EffectEvent::WorkspaceArtifactWriteRequested {
                    artifact, subject, ..
                } => {
                    if artifact == "agent_state/blockers.json"
                        && subject.contains("checkpoint_runtime_divergence")
                        && now_ms.saturating_sub(record.ts_ms) <= DIVERGENCE_WINDOW_MS
                    {
                        signals.checkpoint_divergence_blockers_recent += 1;
                    }
                }
                _ => {}
            },
        }
    }
    signals
}

fn looks_like_diff(raw: &str) -> bool {
    raw.contains("diff --git")
        || (raw.contains("--- ") && raw.contains("+++ "))
        || raw.contains("@@ ")
        || raw.contains("@@ -")
}

fn guardrail_action_from_raw(raw: &str, role: &str) -> Option<Value> {
    if raw.contains("assistant reaction-only terminal frame:") {
        return Some(guardrail_reaction_only_action(role));
    }
    if looks_like_diff(raw) {
        return Some(guardrail_diff_message_action(raw, role));
    }
    None
}


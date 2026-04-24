use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// All events that advance the system state machine.
/// Every mutation of `SystemState` is described by exactly one `ControlEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlEvent {
    // --- Phase lifecycle ---
    PhaseSet {
        phase: String,
        lane: Option<usize>,
    },
    ScheduledPhaseSet {
        phase: Option<String>,
    },

    // --- Planner / legacy diagnostics state ---
    PlannerPendingSet {
        pending: bool,
    },
    PlannerObjectiveReviewQueued,
    PlannerObjectivePlanGapQueued,
    DiagnosticsPendingSet {
        pending: bool,
    },
    /// Compatibility event seen in historical/current tlogs. It represents
    /// queued diagnostics reconciliation work but carries no additional state.
    DiagnosticsReconciliationQueued,
    VerifierBlockerSet {
        active: bool,
    },
    DiagnosticsVerifierFollowupQueued,
    DiagnosticsTextSet {
        text: String,
    },
    LastPlanTextSet {
        text: String,
    },
    LastExecutorDiffSet {
        text: String,
    },
    LastSoloPlanTextSet {
        text: String,
    },
    LastSoloExecutorDiffSet {
        text: String,
    },
    ObjectivesInitialized {
        source_path: String,
        hash: String,
        contents: String,
    },
    ObjectivesReplaced {
        hash: String,
        contents: String,
    },

    // --- Per-lane state ---
    LanePendingSet {
        lane_id: usize,
        pending: bool,
    },
    LaneInProgressSet {
        lane_id: usize,
        actor: Option<String>,
    },
    LaneVerifierResultSet {
        lane_id: usize,
        result: String,
    },
    LanePlanTextSet {
        lane_id: usize,
        text: String,
    },

    // --- Verifier summary ---
    VerifierSummarySet {
        lane_id: usize,
        result: String,
    },

    // --- Executor submit lifecycle ---
    LaneSubmitInFlightSet {
        lane_id: usize,
        in_flight: bool,
    },
    LanePromptInFlightSet {
        lane_id: usize,
        in_flight: bool,
    },
    LaneActiveTabSet {
        lane_id: usize,
        tab_id: u32,
    },
    TabIdToLaneSet {
        tab_id: u32,
        lane_id: usize,
    },
    LaneNextSubmitAtSet {
        lane_id: usize,
        ms: u64,
    },
    LaneStepsUsedSet {
        lane_id: usize,
        steps: usize,
    },
    ExternalUserMessageConsumed {
        role: String,
        signature: String,
    },
    InboundMessageConsumed {
        role: String,
        signature: String,
    },
    /// A wake signal has been canonically queued for a role.
    /// This is the authoritative wake source — replaces wakeup_*.flag files.
    /// Survives tlog replay via wake_signals_pending in SystemState.
    WakeSignalQueued {
        role: String,
        signature: String,
        ts_ms: u64,
    },
    WakeSignalConsumed {
        role: String,
        signature: String,
    },
    /// An inbound message has been canonically queued for a role.
    /// Replaces last_message_to_*.json files as the authoritative message source.
    InboundMessageQueued {
        role: String,
        content: String,
        signature: String,
    },
    /// Canonical control bit used by supervisor build/test gating.
    /// Replaces `rust_patch_verification_requested.flag` as authority.
    RustPatchVerificationRequested {
        requested: bool,
    },
    /// Canonical orchestrator mode (`orchestrate` or `single`) for supervisor policy.
    /// Replaces `orchestrator_mode.flag` as authority.
    OrchestratorModeSet {
        mode: String,
    },
    /// Canonical idle heartbeat emitted by orchestrator loop when no progress occurs.
    /// Replaces `orchestrator_cycle_idle.flag` mtime as authority.
    OrchestratorIdlePulse {
        ts_ms: u64,
    },
    /// Canonical checkpoint snapshot used for orchestrator resume.
    /// Replaces `mini_agent_checkpoint.json` as authority.
    CheckpointSnapshotSet {
        snapshot_json: String,
    },
    /// Canonical signature for last planner blocker evidence.
    /// Replaces `last_planner_blocker_evidence.txt` as authority.
    PlannerBlockerEvidenceSet {
        evidence_hash: String,
    },
    /// Canonical consumed marker for restart-resume payloads by role.
    /// Replaces consuming `post_restart_result.json` as authority.
    PostRestartResultConsumed {
        role: String,
        signature: String,
    },

    // --- Submitted turn tracking (serializable subset) ---
    ExecutorTurnRegistered {
        tab_id: u32,
        turn_id: u64,
        lane_id: usize,
        lane_label: String,
        actor: String,
        endpoint_id: String,
    },
    ExecutorTurnDeregistered {
        tab_id: u32,
        turn_id: u64,
    },
    ExecutorCompletionRecovered {
        tab_id: u32,
        turn_id: u64,
        lane_id: usize,
        lane_label: String,
        actor: String,
        endpoint_id: String,
    },
    ExecutorCompletionTabRebound {
        lane_id: usize,
        from_tab_id: u32,
        to_tab_id: u32,
    },
    ExecutorSubmitAckTabRebound {
        lane_id: usize,
        from_tab_id: u32,
        to_tab_id: u32,
    },
}

pub(crate) fn lane_id_from_control_event(event: &ControlEvent) -> Option<usize> {
    match event {
        ControlEvent::PhaseSet {
            lane: Some(lane_id), ..
        }
        | ControlEvent::LanePendingSet { lane_id, .. }
        | ControlEvent::LaneInProgressSet { lane_id, .. }
        | ControlEvent::LaneVerifierResultSet { lane_id, .. }
        | ControlEvent::LanePlanTextSet { lane_id, .. }
        | ControlEvent::VerifierSummarySet { lane_id, .. }
        | ControlEvent::LaneSubmitInFlightSet { lane_id, .. }
        | ControlEvent::LanePromptInFlightSet { lane_id, .. }
        | ControlEvent::LaneActiveTabSet { lane_id, .. }
        | ControlEvent::TabIdToLaneSet { lane_id, .. }
        | ControlEvent::LaneNextSubmitAtSet { lane_id, .. }
        | ControlEvent::LaneStepsUsedSet { lane_id, .. }
        | ControlEvent::ExecutorTurnRegistered { lane_id, .. }
        | ControlEvent::ExecutorCompletionRecovered { lane_id, .. }
        | ControlEvent::ExecutorCompletionTabRebound { lane_id, .. }
        | ControlEvent::ExecutorSubmitAckTabRebound { lane_id, .. } => Some(*lane_id),
        _ => None,
    }
}

pub(crate) fn lane_indices_from_events(events: &[Event]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Control { event } => lane_id_from_control_event(event),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Side-effect events: logged for observability, never mutate `SystemState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectEvent {
    InvariantViolation {
        proposed_role: String,
        reason: String,
    },
    LlmErrorBoundary {
        role: String,
        prompt_kind: String,
        step: usize,
        endpoint_id: String,
        exchange_id: String,
        error: String,
    },
    BuildEvolutionAdvanced {
        evolution: u64,
        command: String,
        git_commit: Option<String>,
        git_commit_count: Option<u64>,
    },
    CheckpointSaved {
        phase: String,
    },
    CheckpointLoaded {
        phase: String,
    },
    WorkspaceArtifactWriteRequested {
        artifact: String,
        op: String,
        target: String,
        subject: String,
        signature: String,
    },
    WorkspaceArtifactWriteApplied {
        artifact: String,
        op: String,
        target: String,
        subject: String,
        signature: String,
    },
    InboundMessageRecorded {
        from_role: String,
        to_role: String,
        message: String,
        signature: String,
    },
    ExternalUserMessageRecorded {
        to_role: String,
        message: String,
        signature: String,
    },
    BlockerRecorded {
        record: crate::blockers::BlockerRecord,
    },
    LessonsArtifactRecorded {
        artifact: crate::prompt_inputs::LessonsArtifact,
    },
    IssuesFileRecorded {
        file: crate::issues::IssuesFile,
    },
    IssuesProjectionRecorded {
        path: String,
        hash: String,
        issue_count: usize,
        bytes: u64,
    },
    DiagnosticsReportRecorded {
        report: crate::reports::DiagnosticsReport,
    },
    EnforcedInvariantsRecorded {
        file: crate::invariants::EnforcedInvariantsFile,
    },
    ViolationsReportRecorded {
        report: crate::reports::ViolationsReport,
    },
    FramesAllDebugSnapshot {
        source: String,
        file_size_bytes: u64,
        sample_start_offset: u64,
        sample_bytes: u64,
        sample_lines: usize,
        parsed_lines: usize,
        parse_errors: usize,
        type_counts: std::collections::BTreeMap<String, u64>,
        recent_event_types: Vec<String>,
    },
    /// Prompt sent to the LLM.
    LlmTurnInput {
        tab_id: Option<u32>,
        turn_id: Option<u64>,
        role: String,
        agent_type: String,
        step: usize,
        command_id: String,
        endpoint_id: String,
        prompt_hash: String,
        prompt_bytes: usize,
        role_schema_bytes: usize,
        submit_only: bool,
    },
    /// Raw LLM response payload with small structured metadata for joins.
    LlmTurnOutput {
        tab_id: Option<u32>,
        turn_id: Option<u64>,
        role: String,
        step: usize,
        command_id: String,
        endpoint_id: String,
        response_bytes: usize,
        response_hash: String,
        action_kind: Option<String>,
        raw: String,
    },
    /// Result text from executing the emitted action.
    ActionResultRecorded {
        role: String,
        step: usize,
        command_id: String,
        action_kind: String,
        task_id: Option<String>,
        objective_id: Option<String>,
        ok: bool,
        result_bytes: usize,
        result_hash: String,
        result: String,
    },
    FingerprintDriftRecorded {
        drift: crate::drift_analysis::FingerprintDrift,
    },
    GrpoDatasetRecorded {
        row_count: usize,
        group_count: usize,
        mean_reward: f64,
    },
    /// Last completed action snapshot used to resume after process restarts.
    PostRestartResultRecorded {
        role: String,
        action: String,
        result: String,
        step: usize,
        tab_id: Option<u32>,
        turn_id: Option<u64>,
        endpoint_id: String,
        restart_kind: String,
        signature: String,
    },
}

/// Envelope that wraps either a `ControlEvent` or an `EffectEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Event {
    Control { event: ControlEvent },
    Effect { event: EffectEvent },
}

impl Event {
    pub fn control(e: ControlEvent) -> Self {
        Event::Control { event: e }
    }
    pub fn effect(e: EffectEvent) -> Self {
        Event::Effect { event: e }
    }
    pub fn is_control(&self) -> bool {
        matches!(self, Event::Control { .. })
    }
}

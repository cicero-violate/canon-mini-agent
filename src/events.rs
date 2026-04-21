use serde::{Deserialize, Serialize};

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

    // --- Planner / diagnostics state ---
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

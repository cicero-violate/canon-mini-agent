use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
            lane: Some(lane_id),
            ..
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
    GitCheckpointPrepared {
        reason: String,
        subject: String,
        body: String,
        verification_requested: bool,
        changed_paths: Vec<String>,
        staged_shortstat: String,
        diff_stat: String,
        graph_nodes: usize,
        graph_edges: usize,
        graph_bridge_edges: usize,
        graph_redundant_paths: usize,
        graph_alpha_pathways: usize,
        issue_total: usize,
        issue_open: usize,
        issue_resolved: usize,
        recent_actions: BTreeMap<String, usize>,
        signature: String,
    },
    GitCheckpointBlocked {
        reason: String,
        risk: String,
        verification_requested: bool,
        rust_sensitive_changes: bool,
        changed_paths: Vec<String>,
        required_gate: String,
        signature: String,
    },
    SupervisorRestartRequested {
        reason: String,
        mode: String,
        current_binary_path: String,
        current_binary_mtime_ms: u64,
        next_binary_path: String,
        next_binary_mtime_ms: u64,
        verification_requested: bool,
        pending_defer_checks: u32,
        signature: String,
    },
    CheckpointSaved {
        phase: String,
    },
    CheckpointLoaded {
        phase: String,
    },
    SupervisorChildStarted {
        binary_path: String,
        build_kind: String,
        pid: u32,
        binary_mtime_ms: u64,
        signature: String,
    },
    WorkspaceArtifactWriteRequested {
        #[serde(default)]
        artifact_id: String,
        #[serde(default)]
        source_event_seq: u64,
        #[serde(default)]
        producer_action: String,
        #[serde(default)]
        repair_plan_id: String,
        #[serde(default)]
        plan_task_id: String,
        #[serde(default)]
        eval_outcome: String,
        artifact: String,
        op: String,
        target: String,
        subject: String,
        signature: String,
    },
    WorkspaceArtifactWriteApplied {
        #[serde(default)]
        artifact_id: String,
        #[serde(default)]
        source_event_seq: u64,
        #[serde(default)]
        producer_action: String,
        #[serde(default)]
        repair_plan_id: String,
        #[serde(default)]
        plan_task_id: String,
        #[serde(default)]
        eval_outcome: String,
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
        #[serde(default)]
        open_count: usize,
        bytes: u64,
        #[serde(default)]
        issue_fingerprints_hash: String,
        #[serde(default)]
        changed_issue_count: usize,
        #[serde(default)]
        changed_issue_ids: Vec<String>,
        #[serde(default)]
        status_counts: BTreeMap<String, usize>,
    },
    DiagnosticsReportRecorded {
        report: crate::reports::DiagnosticsReport,
    },
    EnforcedInvariantsRecorded {
        #[serde(default)]
        file: crate::invariants::EnforcedInvariantsFile,
        #[serde(default)]
        invariant_count: usize,
        #[serde(default)]
        status_counts: BTreeMap<String, usize>,
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
    PromptTruncationRecorded {
        role: String,
        prompt_kind: String,
        step: usize,
        command_id: String,
        endpoint_id: String,
        heading: String,
        raw_bytes: usize,
        kept_bytes: usize,
        dropped_bytes: usize,
        policy: String,
        body_hash: String,
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
    RecoveryTriggered {
        generated_at_ms: u64,
        class: String,
        policy: String,
        reason: String,
        support_count: usize,
        threshold: usize,
        window_ms: u64,
    },
    CanonicalRepairPolicyRecorded {
        generated_at_ms: u64,
        class: String,
        policy: String,
        repair_plan_id: String,
        plan_mutation_template: String,
        persisted_policy: String,
        verify_policy: String,
    },
    ProjectionRefreshRecoveryRequested {
        generated_at_ms: u64,
        class: String,
        policy: String,
        projection: String,
        command: String,
        timeout_ms: u64,
        reason: String,
    },
    RecoverySuppressed {
        generated_at_ms: u64,
        class: String,
        policy: String,
        reason: String,
        suppression_reason: String,
    },
    RecoveryOutcomeRecorded {
        generated_at_ms: u64,
        class: String,
        policy: String,
        success: bool,
        failure_count_before: usize,
        failure_count_after: usize,
        progress_event_seen: bool,
        eval_window_events: usize,
    },
    /// Eval snapshot emitted each report cycle.  tlog is authority; latest.json is a projection.
    EvalScoreRecorded {
        generated_at_ms: u64,
        overall_score: f64,
        /// None on the first cycle; Some(current − previous) thereafter.
        delta_g: Option<f64>,
        promotion_eligible: bool,
        objective_progress: f64,
        safety: f64,
        task_velocity: f64,
        issue_health: f64,
        semantic_contract: f64,
        #[serde(default)]
        structural_invariant_coverage: f64,
        #[serde(default)]
        canonical_delta_health: f64,
        #[serde(default)]
        improvement_measurement: f64,
        #[serde(default)]
        improvement_validation: f64,
        #[serde(default)]
        improvement_effectiveness: f64,
        #[serde(default)]
        improvement_attempts: usize,
        #[serde(default)]
        measured_improvement_attempts: usize,
        #[serde(default)]
        unmeasured_improvement_attempts: usize,
        #[serde(default)]
        validated_improvement_attempts: usize,
        #[serde(default)]
        unvalidated_improvement_attempts: usize,
        #[serde(default)]
        non_regressed_improvement_attempts: usize,
        #[serde(default)]
        regressed_improvement_attempts: usize,
        #[serde(default)]
        eval_measurement_points: usize,
        #[serde(default)]
        measurement_regressions: usize,
        #[serde(default)]
        recovery_effectiveness: f64,
        #[serde(default)]
        recovery_attempts: usize,
        #[serde(default)]
        recovery_successes: usize,
        #[serde(default)]
        recovery_failures: usize,
        #[serde(default)]
        recovery_suppressed: usize,
        #[serde(default)]
        recovery_loop_breaks: usize,
        #[serde(default)]
        recovery_regressions: usize,
        #[serde(default)]
        recovery_measurement_points: usize,
        #[serde(default)]
        tlog_lag_total_ms: u64,
        #[serde(default)]
        tlog_actionable_lag_total_ms: u64,
        #[serde(default)]
        tlog_dominant_actionable_lag_kind: String,
        #[serde(default)]
        tlog_dominant_actionable_lag_kind_ms: u64,
        #[serde(default)]
        issues_projection_lag_ms: u64,
        #[serde(default)]
        tlog_dominant_payload_kind: String,
        #[serde(default)]
        tlog_dominant_payload_kind_bytes: u64,
        #[serde(default)]
        last_plan_text_payload_bytes: u64,
        #[serde(default)]
        last_executor_diff_payload_bytes: u64,
        #[serde(default)]
        tlog_git_checkpoint_blocked: usize,
        #[serde(default)]
        tlog_unsafe_checkpoint_attempts: usize,
        diagnostics_repair_pressure: f64,
        semantic_fn_error_rate: f64,
        semantic_fn_total: usize,
        semantic_fn_with_any_error: usize,
        #[serde(default)]
        semantic_fn_intent_classified: usize,
        #[serde(default)]
        semantic_fn_totalized: usize,
        #[serde(default)]
        semantic_fn_totalization_coverage: f64,
        #[serde(default)]
        semantic_fn_low_confidence: usize,
        #[serde(default)]
        semantic_fn_intent_coverage: f64,
        #[serde(default)]
        semantic_fn_low_confidence_rate: f64,
        #[serde(default)]
        eval_enforcement_passed: bool,
        #[serde(default)]
        eval_enforcement_violation_count: usize,
        #[serde(default)]
        eval_enforcement_violations: Vec<String>,
        #[serde(default)]
        eval_enforcement_warning_count: usize,
        #[serde(default)]
        eval_enforcement_warnings: Vec<String>,
        #[serde(default)]
        tlog_prompt_truncation_count: usize,
        #[serde(default)]
        tlog_prompt_truncation_dropped_bytes: u64,
        #[serde(default)]
        blocker_distinct_classes: usize,
        #[serde(default)]
        blocker_covered_classes: usize,
        #[serde(default)]
        blocker_top_uncovered: String,
        #[serde(default)]
        blocker_class_coverage: f64,
    },
    /// Outcome of a `machine_verify` check run after an eval cycle.
    /// Emitted by `eval_driver` for every active repair plan.
    PlanVerifyRecorded {
        /// Stable plan id (e.g. "eval_metric:blocker_class_coverage").
        plan_id: String,
        plan_kind: String,
        passed: bool,
        /// Human-readable form of the VerifySpec that was evaluated.
        verify_description: String,
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

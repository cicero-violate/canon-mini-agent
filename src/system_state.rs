use crate::events::{ControlEvent, Event};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Serializable state for a single executor lane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneState {
    pub pending: bool,
    pub in_progress_by: Option<String>,
    pub latest_verifier_result: String,
    pub plan_text: String,
}

/// Serializable record stored in the tlog for each submitted executor turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedTurnRecord {
    pub lane_id: usize,
    pub lane_label: String,
    pub actor: String,
    pub endpoint_id: String,
}

/// The complete, serializable system state.
///
/// All fields that were previously scattered across:
///   - `DispatchState` (serializable portion)
///   - `current_phase`, `current_phase_lane`, `scheduled_phase` locals in `run()`
///   - `verifier_summary` local in `run()`
///
/// …are now consolidated here.  Every mutation is mediated by `CanonicalWriter::apply`
/// and recorded in the tlog.  Non-serializable runtime objects (tab handles, JoinSets,
/// in-flight job structs) live in `RuntimeState` in `app.rs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemState {
    // Phase tracking
    pub phase: String,
    pub phase_lane: Option<usize>,
    pub scheduled_phase: Option<String>,

    // Planner / diagnostics control flags
    pub planner_pending: bool,
    #[serde(default)]
    pub planner_pending_reason: Option<String>,
    pub diagnostics_pending: bool,
    #[serde(default)]
    pub diagnostics_pending_reason: Option<String>,
    #[serde(default)]
    pub active_blocker_to_verifier: bool,
    pub diagnostics_text: String,
    #[serde(default)]
    pub external_user_message_signatures: HashMap<String, String>,
    #[serde(default)]
    pub inbound_message_signatures: HashMap<String, String>,
    #[serde(default)]
    pub wake_signal_signatures: HashMap<String, String>,

    // Rolling diff/plan state fed back into prompts
    pub last_plan_text: String,
    pub last_executor_diff: String,
    pub last_solo_plan_text: String,
    pub last_solo_executor_diff: String,

    // Per-lane logical state
    pub lanes: HashMap<usize, LaneState>,

    // Verifier summaries indexed by lane_id
    pub verifier_summary: Vec<String>,

    // Executor dispatch bookkeeping (serializable fields only)
    pub lane_active_tab: HashMap<usize, u32>,
    pub tab_id_to_lane: HashMap<u32, usize>,
    pub lane_steps_used: HashMap<usize, usize>,
    pub lane_next_submit_at_ms: HashMap<usize, u64>,
    pub lane_submit_in_flight: HashMap<usize, bool>,
    pub lane_prompt_in_flight: HashMap<usize, bool>,

    // Serializable record of in-flight executor turns; key = "{tab_id}:{turn_id}"
    pub submitted_turn_ids: HashMap<String, SubmittedTurnRecord>,
}

impl SystemState {
    /// Build an initial `SystemState` for the given set of lane indices.
    pub fn new(lane_indices: &[usize], lane_count: usize) -> Self {
        let mut lanes = HashMap::new();
        let mut lane_prompt_in_flight = HashMap::new();
        let mut lane_next_submit_at_ms = HashMap::new();
        let mut lane_submit_in_flight = HashMap::new();
        let mut lane_steps_used = HashMap::new();
        for &idx in lane_indices {
            lanes.insert(idx, LaneState::default());
            lane_prompt_in_flight.insert(idx, false);
            lane_next_submit_at_ms.insert(idx, 0);
            lane_submit_in_flight.insert(idx, false);
            lane_steps_used.insert(idx, 0);
        }
        Self {
            phase: "bootstrap".to_string(),
            lanes,
            verifier_summary: vec!["(none yet)".to_string(); lane_count],
            lane_prompt_in_flight,
            lane_next_submit_at_ms,
            lane_submit_in_flight,
            lane_steps_used,
            ..Default::default()
        }
    }

    // --- Read helpers (replace old `DispatchState` methods) ---

    pub fn lane_in_flight(&self, lane_id: usize) -> bool {
        self.lane_prompt_in_flight
            .get(&lane_id)
            .copied()
            .unwrap_or(false)
    }

    pub fn lane_submit_active(&self, lane_id: usize) -> bool {
        self.lane_submit_in_flight
            .get(&lane_id)
            .copied()
            .unwrap_or(false)
    }

    pub fn lane_next_submit_ms(&self, lane_id: usize) -> u64 {
        self.lane_next_submit_at_ms
            .get(&lane_id)
            .copied()
            .unwrap_or(0)
    }

    pub fn lane_steps_used_count(&self, lane_id: usize) -> usize {
        self.lane_steps_used.get(&lane_id).copied().unwrap_or(0)
    }

    pub fn lane_active_tab_id(&self, lane_id: usize) -> Option<u32> {
        self.lane_active_tab.get(&lane_id).copied()
    }

    /// Key-value snapshot used by `evaluate_invariant_gate`.
    pub fn as_kv_map(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(
            "planner_pending".to_string(),
            self.planner_pending.to_string(),
        );
        m.insert(
            "diagnostics_pending".to_string(),
            self.diagnostics_pending.to_string(),
        );
        m.insert(
            "active_blocker_to_verifier".to_string(),
            self.active_blocker_to_verifier.to_string(),
        );
        m.insert("phase".to_string(), self.phase.clone());
        m
    }
}

/// Pure state-transition function — `W(s_t, e) → s_{t+1}`.
///
/// Given the current `SystemState` and a `ControlEvent`, returns the next
/// state.  This function has no side effects: it never writes to disk, never
/// logs, and never inspects the environment.  The `CanonicalWriter` calls it
/// after appending the event to the tlog.
pub fn apply_control_event(mut s: SystemState, e: &ControlEvent) -> SystemState {
    match e {
        ControlEvent::PhaseSet { phase, lane } => {
            s.phase = phase.clone();
            s.phase_lane = *lane;
        }
        ControlEvent::ScheduledPhaseSet { phase } => {
            s.scheduled_phase = phase.clone();
        }
        ControlEvent::PlannerPendingSet { pending } => {
            s.planner_pending = *pending;
            if !pending {
                s.planner_pending_reason = None;
            }
        }
        ControlEvent::PlannerObjectiveReviewQueued => {
            s.planner_pending = true;
            s.planner_pending_reason = Some("objective_review".to_string());
        }
        ControlEvent::PlannerObjectivePlanGapQueued => {
            s.planner_pending = true;
            s.planner_pending_reason = Some("objective_plan_gap".to_string());
        }
        ControlEvent::DiagnosticsPendingSet { pending } => {
            s.diagnostics_pending = *pending;
            if !pending {
                s.diagnostics_pending_reason = None;
            }
        }
        ControlEvent::DiagnosticsReconciliationQueued => {
            // Compatibility no-op for tlogs that recorded reconciliation queue
            // markers before the event model gained a dedicated variant here.
            // Keep replay tolerant without inventing state that cannot be
            // reconstructed from the payloadless historical record.
        }
        ControlEvent::VerifierBlockerSet { active } => {
            s.active_blocker_to_verifier = *active;
        }
        ControlEvent::DiagnosticsVerifierFollowupQueued => {
            s.diagnostics_pending = true;
            s.diagnostics_pending_reason = Some("verifier_followup".to_string());
        }
        ControlEvent::DiagnosticsTextSet { text } => {
            s.diagnostics_text = text.clone();
        }
        ControlEvent::ExternalUserMessageConsumed { role, signature } => {
            s.external_user_message_signatures
                .insert(role.clone(), signature.clone());
        }
        ControlEvent::InboundMessageConsumed { role, signature } => {
            s.inbound_message_signatures
                .insert(role.clone(), signature.clone());
        }
        ControlEvent::WakeSignalConsumed { role, signature } => {
            s.wake_signal_signatures
                .insert(role.clone(), signature.clone());
        }
        ControlEvent::LastPlanTextSet { text } => {
            s.last_plan_text = text.clone();
        }
        ControlEvent::LastExecutorDiffSet { text } => {
            s.last_executor_diff = text.clone();
        }
        ControlEvent::LastSoloPlanTextSet { text } => {
            s.last_solo_plan_text = text.clone();
        }
        ControlEvent::LastSoloExecutorDiffSet { text } => {
            s.last_solo_executor_diff = text.clone();
        }
        ControlEvent::LanePendingSet { lane_id, pending } => {
            s.lanes.entry(*lane_id).or_default().pending = *pending;
        }
        ControlEvent::LaneInProgressSet { lane_id, actor } => {
            s.lanes.entry(*lane_id).or_default().in_progress_by = actor.clone();
        }
        ControlEvent::LaneVerifierResultSet { lane_id, result } => {
            s.lanes.entry(*lane_id).or_default().latest_verifier_result = result.clone();
        }
        ControlEvent::LanePlanTextSet { lane_id, text } => {
            s.lanes.entry(*lane_id).or_default().plan_text = text.clone();
        }
        ControlEvent::VerifierSummarySet { lane_id, result } => {
            if *lane_id < s.verifier_summary.len() {
                s.verifier_summary[*lane_id] = result.clone();
            }
        }
        ControlEvent::LaneSubmitInFlightSet { lane_id, in_flight } => {
            s.lane_submit_in_flight.insert(*lane_id, *in_flight);
        }
        ControlEvent::LanePromptInFlightSet { lane_id, in_flight } => {
            s.lane_prompt_in_flight.insert(*lane_id, *in_flight);
        }
        ControlEvent::LaneActiveTabSet { lane_id, tab_id } => {
            s.lane_active_tab.insert(*lane_id, *tab_id);
        }
        ControlEvent::TabIdToLaneSet { tab_id, lane_id } => {
            s.tab_id_to_lane.insert(*tab_id, *lane_id);
        }
        ControlEvent::LaneNextSubmitAtSet { lane_id, ms } => {
            s.lane_next_submit_at_ms.insert(*lane_id, *ms);
        }
        ControlEvent::LaneStepsUsedSet { lane_id, steps } => {
            s.lane_steps_used.insert(*lane_id, *steps);
        }
        ControlEvent::ExecutorTurnRegistered {
            tab_id,
            turn_id,
            lane_id,
            lane_label,
            actor,
            endpoint_id,
        } => {
            let key = format!("{}:{}", tab_id, turn_id);
            s.submitted_turn_ids.insert(
                key,
                SubmittedTurnRecord {
                    lane_id: *lane_id,
                    lane_label: lane_label.clone(),
                    actor: actor.clone(),
                    endpoint_id: endpoint_id.clone(),
                },
            );
            // Mirror the four companion field updates that `register_submitted_executor_turn`
            // used to perform directly on `DispatchState`.
            s.lane_active_tab.insert(*lane_id, *tab_id);
            s.tab_id_to_lane.insert(*tab_id, *lane_id);
            s.lane_next_submit_at_ms
                .insert(*lane_id, crate::logging::now_ms());
            s.lane_submit_in_flight.insert(*lane_id, false);
        }
        ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id } => {
            let key = format!("{}:{}", tab_id, turn_id);
            s.submitted_turn_ids.remove(&key);
        }
        ControlEvent::ExecutorCompletionRecovered {
            tab_id, lane_id, ..
        } => {
            if let Some(previous_tab_id) = s.lane_active_tab.insert(*lane_id, *tab_id) {
                if previous_tab_id != *tab_id {
                    s.tab_id_to_lane.remove(&previous_tab_id);
                }
            }
            s.tab_id_to_lane.insert(*tab_id, *lane_id);
            s.lane_next_submit_at_ms
                .insert(*lane_id, crate::logging::now_ms());
            s.lane_submit_in_flight.insert(*lane_id, false);
        }
        ControlEvent::ExecutorCompletionTabRebound {
            lane_id,
            from_tab_id,
            to_tab_id,
        } => {
            s.lane_active_tab.insert(*lane_id, *to_tab_id);
            s.tab_id_to_lane.remove(from_tab_id);
            s.tab_id_to_lane.insert(*to_tab_id, *lane_id);
        }
        ControlEvent::ExecutorSubmitAckTabRebound {
            lane_id,
            from_tab_id,
            to_tab_id,
        } => {
            s.lane_active_tab.insert(*lane_id, *to_tab_id);
            s.tab_id_to_lane.remove(from_tab_id);
            s.tab_id_to_lane.insert(*to_tab_id, *lane_id);
        }
    }
    s
}

pub fn validate_system_state(s: &SystemState) -> Result<(), String> {
    if s.phase.trim().is_empty() {
        return Err("system state invariant failed: phase must be non-empty".to_string());
    }
    if !s.planner_pending && s.planner_pending_reason.is_some() {
        return Err(
            "system state invariant failed: planner_pending_reason requires planner_pending"
                .to_string(),
        );
    }
    if !s.diagnostics_pending && s.diagnostics_pending_reason.is_some() {
        return Err(
            "system state invariant failed: diagnostics_pending_reason requires diagnostics_pending"
                .to_string(),
        );
    }

    for lane_id in s.lanes.keys() {
        if !s.lane_prompt_in_flight.contains_key(lane_id) {
            return Err(format!(
                "system state invariant failed: lane_prompt_in_flight missing lane {lane_id}"
            ));
        }
        if !s.lane_submit_in_flight.contains_key(lane_id) {
            return Err(format!(
                "system state invariant failed: lane_submit_in_flight missing lane {lane_id}"
            ));
        }
        if !s.lane_next_submit_at_ms.contains_key(lane_id) {
            return Err(format!(
                "system state invariant failed: lane_next_submit_at_ms missing lane {lane_id}"
            ));
        }
        if !s.lane_steps_used.contains_key(lane_id) {
            return Err(format!(
                "system state invariant failed: lane_steps_used missing lane {lane_id}"
            ));
        }
    }

    for (tab_id, lane_id) in &s.tab_id_to_lane {
        if !s.lanes.contains_key(lane_id) {
            return Err(format!(
                "system state invariant failed: tab_id_to_lane points at unknown lane {lane_id}"
            ));
        }
        if s.lane_active_tab.get(lane_id) != Some(tab_id) {
            return Err(format!(
                "system state invariant failed: lane_active_tab/tab_id_to_lane mismatch for lane {lane_id} tab {tab_id}"
            ));
        }
    }

    for (key, submitted) in &s.submitted_turn_ids {
        let (tab_str, _) = key.split_once(':').ok_or_else(|| {
            format!(
                "system state invariant failed: submitted_turn_ids key has invalid format: {key}"
            )
        })?;
        let tab_id: u32 = tab_str.parse().map_err(|_| {
            format!(
                "system state invariant failed: submitted_turn_ids key has invalid tab id: {key}"
            )
        })?;
        if !s.lanes.contains_key(&submitted.lane_id) {
            return Err(format!(
                "system state invariant failed: submitted turn references unknown lane {}",
                submitted.lane_id
            ));
        }
        if s.tab_id_to_lane.get(&tab_id) != Some(&submitted.lane_id) {
            return Err(format!(
                "system state invariant failed: submitted turn tab {tab_id} is not mapped to lane {}",
                submitted.lane_id
            ));
        }
        if s.lane_active_tab.get(&submitted.lane_id) != Some(&tab_id) {
            return Err(format!(
                "system state invariant failed: submitted turn tab {tab_id} is not active for lane {}",
                submitted.lane_id
            ));
        }
    }

    Ok(())
}

pub fn replay_event_log(initial: SystemState, events: &[Event]) -> Result<SystemState, String> {
    let mut state = initial;
    validate_system_state(&state)?;
    for event in events {
        if let Event::Control { event } = event {
            state = apply_control_event(state, event);
            validate_system_state(&state)?;
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_pending_queue_events_record_reason_and_clear_on_false() {
        let mut state = SystemState::new(&[0], 1);
        state = apply_control_event(state, &ControlEvent::PlannerObjectiveReviewQueued);
        assert!(state.planner_pending);
        assert_eq!(
            state.planner_pending_reason.as_deref(),
            Some("objective_review")
        );

        state = apply_control_event(state, &ControlEvent::DiagnosticsVerifierFollowupQueued);
        assert!(state.diagnostics_pending);
        assert_eq!(
            state.diagnostics_pending_reason.as_deref(),
            Some("verifier_followup")
        );

        state = apply_control_event(state, &ControlEvent::PlannerPendingSet { pending: false });
        state = apply_control_event(
            state,
            &ControlEvent::DiagnosticsPendingSet { pending: false },
        );
        assert!(state.planner_pending_reason.is_none());
        assert!(state.diagnostics_pending_reason.is_none());
    }

    #[test]
    fn diagnostics_reconciliation_queue_event_replays_as_compatibility_noop() {
        let state = SystemState::new(&[0], 1);
        let next = apply_control_event(state.clone(), &ControlEvent::DiagnosticsReconciliationQueued);
        assert_eq!(next.phase, state.phase);
        assert_eq!(next.planner_pending, state.planner_pending);
        assert_eq!(next.diagnostics_pending, state.diagnostics_pending);
        assert_eq!(next.scheduled_phase, state.scheduled_phase);
    }

    #[test]
    fn validate_system_state_rejects_submitted_turn_without_tab_mapping() {
        let mut state = SystemState::new(&[0], 1);
        state.submitted_turn_ids.insert(
            "7:11".to_string(),
            SubmittedTurnRecord {
                lane_id: 0,
                lane_label: "executor-0".to_string(),
                actor: "executor".to_string(),
                endpoint_id: "ep".to_string(),
            },
        );
        let err =
            validate_system_state(&state).expect_err("must reject inconsistent submitted turn");
        assert!(err.contains("submitted turn tab 7 is not mapped to lane 0"));
    }
}

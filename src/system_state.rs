use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::events::ControlEvent;

/// Serializable state for a single executor lane.
/// Replaces `DispatchLaneState` from `app.rs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneState {
    pub pending: bool,
    pub in_progress_by: Option<String>,
    pub latest_verifier_result: String,
    pub plan_text: String,
}

/// Serializable record stored in the tlog for each submitted executor turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub diagnostics_pending: bool,
    pub diagnostics_text: String,

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
        }
        ControlEvent::DiagnosticsPendingSet { pending } => {
            s.diagnostics_pending = *pending;
        }
        ControlEvent::DiagnosticsTextSet { text } => {
            s.diagnostics_text = text.clone();
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
            s.lanes
                .entry(*lane_id)
                .or_default()
                .latest_verifier_result = result.clone();
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
            s.tab_id_to_lane.entry(*tab_id).or_insert(*lane_id);
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
            s.tab_id_to_lane.entry(*tab_id).or_insert(*lane_id);
            s.lane_next_submit_at_ms
                .insert(*lane_id, crate::logging::now_ms());
            s.lane_submit_in_flight.insert(*lane_id, false);
        }
        ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id } => {
            let key = format!("{}:{}", tab_id, turn_id);
            s.submitted_turn_ids.remove(&key);
        }
    }
    s
}

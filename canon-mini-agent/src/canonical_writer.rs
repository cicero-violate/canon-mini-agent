use crate::events::{ControlEvent, EffectEvent, Event};
use crate::system_state::{apply_control_event, validate_system_state, SystemState};
use crate::tlog::Tlog;
use crate::transition_policy::validate_transition;
use std::path::PathBuf;

/// The single gate through which all `SystemState` mutations must pass.
///
/// `W(s_t, e) → s_{t+1}`
///
/// Calling `apply` will:
///   1. Append the event to the total-ordered log (`Tlog`).
///   2. Advance `SystemState` via the pure `apply_control_event` function.
///
/// Invariant checking (`I(e, s)`) is the caller's responsibility — the phase
/// gate functions already call `evaluate_invariant_gate` before emitting
/// events.  When a proposed transition is blocked, the caller should call
/// `record_violation` so the rejection is captured in the tlog without
/// advancing the state.
pub struct CanonicalWriter {
    state: SystemState,
    tlog: Tlog,
    workspace: PathBuf,
}

impl CanonicalWriter {
    pub fn new(state: SystemState, tlog: Tlog, workspace: PathBuf) -> Self {
        if let Err(reason) = validate_system_state(&state) {
            panic!("[canonical_writer] invalid initial state: {reason}");
        }
        Self {
            state,
            tlog,
            workspace,
        }
    }

    /// Apply a control event: append to tlog, then transition state.
    ///
    /// This is the ONLY function allowed to mutate `SystemState`.
    /// If the tlog write fails (e.g. disk full) the error is logged but the
    /// state transition still proceeds — a missing tlog entry is recoverable
    /// from the checkpoint; a missed state transition is not.
    pub fn apply(&mut self, event: ControlEvent) {
        if let Err(reason) = validate_transition(&self.state, &event) {
            panic!("[canonical_writer] illegal control transition: {reason}");
        }
        if let Err(err) = self.tlog.append(&Event::control(event.clone())) {
            eprintln!("[canonical_writer] tlog append failed: {err:#}");
        }
        let next_state = apply_control_event(self.state.clone(), &event);
        if let Err(reason) = validate_system_state(&next_state) {
            panic!("[canonical_writer] invalid control transition: {reason}");
        }
        self.state = next_state;
    }

    /// Record an invariant violation without changing state.
    /// The rejection is appended to the tlog as an effect event.
    pub fn record_violation(&mut self, proposed_role: &str, reason: &str) {
        let ev = Event::effect(EffectEvent::InvariantViolation {
            proposed_role: proposed_role.to_string(),
            reason: reason.to_string(),
        });
        if let Err(err) = self.tlog.append(&ev) {
            eprintln!("[canonical_writer] tlog violation append failed: {err:#}");
        }
    }

    /// Record an effect event (checkpoint saved/loaded, etc.).
    pub fn record_effect(&mut self, effect: EffectEvent) {
        if let Err(err) = self.tlog.append(&Event::effect(effect)) {
            eprintln!("[canonical_writer] tlog effect append failed: {err:#}");
        }
    }

    /// Read access to the current system state.
    pub fn state(&self) -> &SystemState {
        &self.state
    }

    /// Replace state during checkpoint hydration only.
    /// This is the single non-`apply` mutation path and must never be used for
    /// live runtime transitions.
    pub fn restore_from_checkpoint(&mut self, restored: SystemState) {
        if let Err(reason) = validate_system_state(&restored) {
            panic!("[canonical_writer] invalid checkpoint restore state: {reason}");
        }
        self.state = restored;
    }

    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    pub fn tlog_seq(&self) -> u64 {
        self.tlog.seq()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ControlEvent;

    fn tempdir() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "canon-canonical-writer-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn replay_tlog_reconstructs_final_state_and_ignores_effects() {
        let dir = tempdir();
        let tlog_path = dir.join("tlog.ndjson");
        let initial = SystemState::new(&[0], 1);
        let mut writer = CanonicalWriter::new(initial.clone(), Tlog::open(&tlog_path), dir.clone());

        writer.apply(ControlEvent::PlannerPendingSet { pending: true });
        writer.record_violation("executor", "blocked");
        writer.apply(ControlEvent::PhaseSet {
            phase: "planner".to_string(),
            lane: None,
        });
        writer.apply(ControlEvent::LanePendingSet {
            lane_id: 0,
            pending: false,
        });
        writer.apply(ControlEvent::LaneInProgressSet {
            lane_id: 0,
            actor: Some("executor-0".to_string()),
        });
        writer.apply(ControlEvent::LaneSubmitInFlightSet {
            lane_id: 0,
            in_flight: true,
        });
        writer.apply(ControlEvent::ExecutorCompletionRecovered {
            tab_id: 9,
            turn_id: 12,
            lane_id: 0,
            lane_label: "executor-0".to_string(),
            actor: "executor-0".to_string(),
            endpoint_id: "ep".to_string(),
        });
        writer.apply(ControlEvent::ExecutorCompletionTabRebound {
            lane_id: 0,
            from_tab_id: 9,
            to_tab_id: 10,
        });
        writer.apply(ControlEvent::ExecutorTurnRegistered {
            tab_id: 10,
            turn_id: 13,
            lane_id: 0,
            lane_label: "executor-0".to_string(),
            actor: "executor-0".to_string(),
            endpoint_id: "ep".to_string(),
        });
        writer.apply(ControlEvent::ExecutorTurnDeregistered {
            tab_id: 10,
            turn_id: 13,
        });
        writer.apply(ControlEvent::LaneInProgressSet {
            lane_id: 0,
            actor: None,
        });
        writer.apply(ControlEvent::LanePendingSet {
            lane_id: 0,
            pending: true,
        });

        let replayed = Tlog::replay(&tlog_path, initial).expect("replay");
        assert_eq!(replayed.planner_pending, writer.state().planner_pending);
        assert_eq!(replayed.phase, writer.state().phase);
        assert_eq!(
            replayed.lanes.get(&0).map(|lane| lane.pending),
            writer.state().lanes.get(&0).map(|lane| lane.pending)
        );
        assert_eq!(
            replayed.submitted_turn_ids,
            writer.state().submitted_turn_ids
        );
    }

    #[test]
    #[should_panic(expected = "illegal control transition")]
    fn apply_panics_on_illegal_transition() {
        let dir = tempdir();
        let tlog_path = dir.join("tlog.ndjson");
        let initial = SystemState::new(&[0], 1);
        let mut writer = CanonicalWriter::new(initial, Tlog::open(&tlog_path), dir);
        writer.apply(ControlEvent::ExecutorTurnDeregistered {
            tab_id: 7,
            turn_id: 9,
        });
    }
}

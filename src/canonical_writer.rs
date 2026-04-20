use crate::events::{ControlEvent, EffectEvent, Event};
use crate::system_state::{apply_control_event, validate_system_state, SystemState};
use crate::tlog::Tlog;
use crate::transition_policy::validate_transition;
use anyhow::{anyhow, Result};
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
    pub fn try_new(state: SystemState, tlog: Tlog, workspace: PathBuf) -> Result<Self> {
        validate_system_state(&state)
            .map_err(|reason| anyhow!("[canonical_writer] invalid initial state: {reason}"))?;
        Ok(Self {
            state,
            tlog,
            workspace,
        })
    }

    pub fn new(state: SystemState, tlog: Tlog, workspace: PathBuf) -> Self {
        Self::try_new(state, tlog, workspace).unwrap_or_else(|err| panic!("{err:#}"))
    }

    /// Apply a control event after validating the proposed next state.
    ///
    /// This is the ONLY function allowed to mutate `SystemState`.
    /// Replay equivalence is preserved by refusing to append invalid control
    /// events before any durable write is attempted.
    pub fn try_apply(&mut self, event: ControlEvent) -> Result<()> {
        validate_transition(&self.state, &event)
            .map_err(|reason| anyhow!("[canonical_writer] illegal control transition: {reason}"))?;
        let next_state = apply_control_event(self.state.clone(), &event);
        validate_system_state(&next_state)
            .map_err(|reason| anyhow!("[canonical_writer] invalid control transition: {reason}"))?;
        self.tlog
            .append(&Event::control(event.clone()))
            .map_err(|err| {
                anyhow!("[canonical_writer] tlog append failed during apply: {err:#}")
            })?;
        self.state = next_state;
        Ok(())
    }

    pub fn apply(&mut self, event: ControlEvent) {
        if let Err(err) = self.try_apply(event) {
            let _ =
                self.try_record_violation("canonical_writer", &format!("apply failed: {err:#}"));
            eprintln!("{err:#}");
        }
    }

    /// Record an invariant violation without changing state.
    /// The rejection is appended to the tlog as an effect event.
    pub fn try_record_violation(&mut self, proposed_role: &str, reason: &str) -> Result<()> {
        let ev = Event::effect(EffectEvent::InvariantViolation {
            proposed_role: proposed_role.to_string(),
            reason: reason.to_string(),
        });
        self.tlog
            .append(&ev)
            .map_err(|err| anyhow!("[canonical_writer] tlog violation append failed: {err:#}"))?;
        Ok(())
    }

    pub fn record_violation(&mut self, proposed_role: &str, reason: &str) {
        if let Err(err) = self.try_record_violation(proposed_role, reason) {
            eprintln!("{err:#}");
        }
    }

    /// Record an effect event (checkpoint saved/loaded, etc.).
    pub fn try_record_effect(&mut self, effect: EffectEvent) -> Result<()> {
        self.tlog
            .append(&Event::effect(effect))
            .map_err(|err| anyhow!("[canonical_writer] tlog effect append failed: {err:#}"))?;
        Ok(())
    }

    pub fn record_effect(&mut self, effect: EffectEvent) {
        if let Err(err) = self.try_record_effect(effect) {
            let _ = self.try_record_violation(
                "canonical_writer",
                &format!("record_effect failed: {err:#}"),
            );
            eprintln!("{err:#}");
        }
    }

    /// Record a build evolution advance and update the tlog evolution stamp.
    pub fn try_record_evolution_advance(
        &mut self,
        advance: &crate::evolution::EvolutionAdvance,
    ) -> Result<()> {
        self.tlog.set_evolution(advance.evolution);
        self.try_record_effect(EffectEvent::BuildEvolutionAdvanced {
            evolution: advance.evolution,
            command: advance.command.clone(),
            git_commit: advance.git_commit.clone(),
            git_commit_count: advance.git_commit_count,
        })
    }

    pub fn record_evolution_advance(&mut self, advance: &crate::evolution::EvolutionAdvance) {
        if let Err(err) = self.try_record_evolution_advance(advance) {
            let _ = self.try_record_violation(
                "canonical_writer",
                &format!("record_evolution_advance failed: {err:#}"),
            );
            eprintln!("{err:#}");
        }
    }

    /// Read access to the current system state.
    pub fn state(&self) -> &SystemState {
        &self.state
    }

    /// Replace state during checkpoint hydration only.
    /// This is the single non-`apply` mutation path and must never be used for
    /// live runtime transitions.
    pub fn try_restore_from_checkpoint(&mut self, restored: SystemState) -> Result<()> {
        validate_system_state(&restored).map_err(|reason| {
            anyhow!("[canonical_writer] invalid checkpoint restore state: {reason}")
        })?;
        self.state = restored;
        Ok(())
    }

    pub fn restore_from_checkpoint(&mut self, restored: SystemState) {
        if let Err(err) = self.try_restore_from_checkpoint(restored) {
            let _ = self.try_record_violation(
                "canonical_writer",
                &format!("restore_from_checkpoint failed: {err:#}"),
            );
            eprintln!("{err:#}");
        }
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
    fn try_apply_rejects_illegal_transition_without_mutating_state() {
        let dir = tempdir();
        let tlog_path = dir.join("tlog.ndjson");
        let initial = SystemState::new(&[0], 1);
        let mut writer = CanonicalWriter::new(initial, Tlog::open(&tlog_path), dir);
        let err = writer
            .try_apply(ControlEvent::ExecutorTurnDeregistered {
                tab_id: 7,
                turn_id: 9,
            })
            .expect_err("illegal transition should fail");
        assert!(err.to_string().contains("illegal control transition"));
        assert!(std::fs::read_to_string(&tlog_path)
            .unwrap_or_default()
            .trim()
            .is_empty());
        assert!(writer.state().submitted_turn_ids.is_empty());
    }
}

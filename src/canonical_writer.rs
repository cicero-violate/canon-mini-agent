use std::path::PathBuf;
use crate::events::{ControlEvent, EffectEvent, Event};
use crate::system_state::{apply_control_event, SystemState};
use crate::tlog::Tlog;

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
        if let Err(err) = self.tlog.append(&Event::control(event.clone())) {
            eprintln!("[canonical_writer] tlog append failed: {err:#}");
        }
        self.state = apply_control_event(self.state.clone(), &event);
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

    /// Direct mutable access — use ONLY for initialization and checkpoint
    /// restore, where the mutations are not individually logged.
    /// All runtime mutations must go through `apply`.
    pub fn state_mut(&mut self) -> &mut SystemState {
        &mut self.state
    }

    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    pub fn tlog_seq(&self) -> u64 {
        self.tlog.seq()
    }
}

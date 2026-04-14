use crate::events::ControlEvent;
use crate::system_state::SystemState;

fn is_valid_phase(phase: &str) -> bool {
    matches!(
        phase,
        "bootstrap" | "planner" | "executor" | "verifier" | "diagnostics" | "solo"
    )
}

fn require_lane(state: &SystemState, lane_id: usize, event: &str) -> Result<(), String> {
    if state.lanes.contains_key(&lane_id) {
        Ok(())
    } else {
        Err(format!(
            "illegal transition: {event} referenced unknown lane {lane_id}"
        ))
    }
}

pub fn validate_transition(state: &SystemState, event: &ControlEvent) -> Result<(), String> {
    match event {
        ControlEvent::PhaseSet { phase, lane } => {
            if !is_valid_phase(phase) {
                return Err(format!("illegal transition: invalid phase `{phase}`"));
            }
            match phase.as_str() {
                "executor" | "verifier" => {
                    let lane_id = lane.ok_or_else(|| {
                        format!("illegal transition: phase `{phase}` requires a lane")
                    })?;
                    require_lane(state, lane_id, "PhaseSet")?;
                }
                _ if lane.is_some() => {
                    return Err(format!(
                        "illegal transition: phase `{phase}` must not carry a lane"
                    ));
                }
                _ => {}
            }
        }
        ControlEvent::ScheduledPhaseSet { phase } => {
            if let Some(phase) = phase {
                if !is_valid_phase(phase) || phase == "bootstrap" {
                    return Err(format!(
                        "illegal transition: scheduled phase `{phase}` is not dispatchable"
                    ));
                }
            }
        }
        ControlEvent::PlannerPendingSet { .. }
        | ControlEvent::DiagnosticsPendingSet { .. }
        | ControlEvent::DiagnosticsTextSet { .. }
        | ControlEvent::LastPlanTextSet { .. }
        | ControlEvent::LastExecutorDiffSet { .. }
        | ControlEvent::LastSoloPlanTextSet { .. }
        | ControlEvent::LastSoloExecutorDiffSet { .. } => {}
        ControlEvent::LanePendingSet { lane_id, .. }
        | ControlEvent::LaneSubmitInFlightSet { lane_id, .. }
        | ControlEvent::LanePromptInFlightSet { lane_id, .. }
        | ControlEvent::LaneActiveTabSet { lane_id, .. }
        | ControlEvent::LaneNextSubmitAtSet { lane_id, .. }
        | ControlEvent::LaneStepsUsedSet { lane_id, .. }
        | ControlEvent::LaneVerifierResultSet { lane_id, .. }
        | ControlEvent::LanePlanTextSet { lane_id, .. } => {
            require_lane(state, *lane_id, "lane-scoped event")?;
        }
        ControlEvent::LaneInProgressSet { lane_id, actor } => {
            require_lane(state, *lane_id, "LaneInProgressSet")?;
            let current = state
                .lanes
                .get(lane_id)
                .and_then(|lane| lane.in_progress_by.as_ref());
            if actor.is_none() && current.is_none() {
                return Err(format!(
                    "illegal transition: lane {lane_id} is already not in progress"
                ));
            }
            if let (Some(next), Some(existing)) = (actor.as_ref(), current) {
                if next != existing {
                    return Err(format!(
                        "illegal transition: lane {lane_id} already owned by `{existing}`, cannot switch directly to `{next}`"
                    ));
                }
            }
        }
        ControlEvent::VerifierSummarySet { lane_id, .. } => {
            if *lane_id >= state.verifier_summary.len() {
                return Err(format!(
                    "illegal transition: verifier summary lane {lane_id} out of range"
                ));
            }
        }
        ControlEvent::TabIdToLaneSet { tab_id, lane_id } => {
            require_lane(state, *lane_id, "TabIdToLaneSet")?;
            if let Some(existing_lane) = state.tab_id_to_lane.get(tab_id) {
                if existing_lane != lane_id {
                    return Err(format!(
                        "illegal transition: tab {tab_id} already mapped to lane {existing_lane}"
                    ));
                }
            }
        }
        ControlEvent::ExecutorTurnRegistered {
            tab_id,
            turn_id,
            lane_id,
            ..
        } => {
            require_lane(state, *lane_id, "ExecutorTurnRegistered")?;
            let key = format!("{tab_id}:{turn_id}");
            if state.submitted_turn_ids.contains_key(&key) {
                return Err(format!(
                    "illegal transition: submitted turn `{key}` already registered"
                ));
            }
            if let Some(existing_lane) = state.tab_id_to_lane.get(tab_id) {
                if existing_lane != lane_id {
                    return Err(format!(
                        "illegal transition: tab {tab_id} already mapped to lane {existing_lane}"
                    ));
                }
            }
        }
        ControlEvent::ExecutorTurnDeregistered { tab_id, turn_id } => {
            let key = format!("{tab_id}:{turn_id}");
            if !state.submitted_turn_ids.contains_key(&key) {
                return Err(format!(
                    "illegal transition: submitted turn `{key}` is not registered"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_policy_rejects_non_lane_executor_phase() {
        let state = SystemState::new(&[0], 1);
        let err = validate_transition(
            &state,
            &ControlEvent::PhaseSet {
                phase: "executor".to_string(),
                lane: None,
            },
        )
        .expect_err("executor phase must require lane");
        assert!(err.contains("requires a lane"));
    }

    #[test]
    fn deregister_policy_rejects_unknown_turn() {
        let state = SystemState::new(&[0], 1);
        let err = validate_transition(
            &state,
            &ControlEvent::ExecutorTurnDeregistered {
                tab_id: 1,
                turn_id: 2,
            },
        )
        .expect_err("must reject unknown turn");
        assert!(err.contains("is not registered"));
    }
}

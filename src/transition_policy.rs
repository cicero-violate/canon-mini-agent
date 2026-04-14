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

fn lane_pending(state: &SystemState, lane_id: usize) -> bool {
    state
        .lanes
        .get(&lane_id)
        .map(|lane| lane.pending)
        .unwrap_or(false)
}

fn lane_in_progress(state: &SystemState, lane_id: usize) -> bool {
    state
        .lanes
        .get(&lane_id)
        .and_then(|lane| lane.in_progress_by.as_ref())
        .is_some()
}

fn lane_submit_in_flight(state: &SystemState, lane_id: usize) -> bool {
    state
        .lane_submit_in_flight
        .get(&lane_id)
        .copied()
        .unwrap_or(false)
}

fn lane_prompt_in_flight(state: &SystemState, lane_id: usize) -> bool {
    state
        .lane_prompt_in_flight
        .get(&lane_id)
        .copied()
        .unwrap_or(false)
}

pub fn validate_transition(state: &SystemState, event: &ControlEvent) -> Result<(), String> {
    match event {
        ControlEvent::PhaseSet { phase, lane } => {
            if !is_valid_phase(phase) {
                return Err(format!("illegal transition: invalid phase `{phase}`"));
            }
            match phase.as_str() {
                "verifier" => {
                    let lane_id = lane.ok_or_else(|| {
                        format!("illegal transition: phase `{phase}` requires a lane")
                    })?;
                    require_lane(state, lane_id, "PhaseSet")?;
                    if !lane_in_progress(state, lane_id) {
                        return Err(format!(
                            "illegal transition: verifier phase requires lane {lane_id} to be in progress"
                        ));
                    }
                }
                "executor" => {
                    if let Some(lane_id) = lane {
                        require_lane(state, *lane_id, "PhaseSet")?;
                        if !lane_in_progress(state, *lane_id) {
                            return Err(format!(
                                "illegal transition: executor phase for lane {lane_id} requires lane to be in progress"
                            ));
                        }
                    } else if state.phase != "bootstrap"
                        && state.scheduled_phase.as_deref() != Some("executor")
                    {
                        return Err(
                            "illegal transition: lane-less executor phase is only allowed during bootstrap or executor scheduling"
                                .to_string(),
                        );
                    }
                }
                _ if lane.is_some() => {
                    return Err(format!(
                        "illegal transition: phase `{phase}` must not carry a lane"
                    ));
                }
                "planner" => {
                    if !(state.planner_pending
                        || state.scheduled_phase.as_deref() == Some("planner")
                        || state.phase == "bootstrap")
                    {
                        return Err(
                            "illegal transition: planner phase requires planner_pending or scheduled planner work"
                                .to_string(),
                        );
                    }
                }
                "diagnostics" => {
                    if !(state.diagnostics_pending
                        || state.scheduled_phase.as_deref() == Some("diagnostics")
                        || state.phase == "bootstrap")
                    {
                        return Err(
                            "illegal transition: diagnostics phase requires diagnostics_pending or scheduled diagnostics work"
                                .to_string(),
                        );
                    }
                }
                "solo" => {
                    if !(state.scheduled_phase.as_deref() == Some("solo")
                        || state.phase == "bootstrap")
                    {
                        return Err(
                            "illegal transition: solo phase requires scheduled solo work"
                                .to_string(),
                        );
                    }
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
        ControlEvent::LanePendingSet { lane_id, pending } => {
            require_lane(state, *lane_id, "LanePendingSet")?;
            if *pending && lane_in_progress(state, *lane_id) {
                return Err(format!(
                    "illegal transition: cannot mark lane {lane_id} pending while it is still in progress"
                ));
            }
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
            if actor.is_some() && lane_pending(state, *lane_id) {
                return Err(format!(
                    "illegal transition: lane {lane_id} must clear pending before entering in-progress"
                ));
            }
        }
        ControlEvent::VerifierSummarySet { lane_id, .. } => {
            if *lane_id >= state.verifier_summary.len() {
                return Err(format!(
                    "illegal transition: verifier summary lane {lane_id} out of range"
                ));
            }
        }
        ControlEvent::LaneVerifierResultSet { lane_id, .. }
        | ControlEvent::LanePlanTextSet { lane_id, .. }
        | ControlEvent::LaneNextSubmitAtSet { lane_id, .. }
        | ControlEvent::LaneStepsUsedSet { lane_id, .. } => {
            require_lane(state, *lane_id, "lane-scoped event")?;
        }
        ControlEvent::LaneSubmitInFlightSet { lane_id, in_flight } => {
            require_lane(state, *lane_id, "LaneSubmitInFlightSet")?;
            if *in_flight {
                if !lane_in_progress(state, *lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} cannot enter submit-in-flight without being in progress"
                    ));
                }
                if lane_submit_in_flight(state, *lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} is already submit-in-flight"
                    ));
                }
                if lane_prompt_in_flight(state, *lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} cannot submit while prompt continuation is in flight"
                    ));
                }
            }
        }
        ControlEvent::LanePromptInFlightSet { lane_id, in_flight } => {
            require_lane(state, *lane_id, "LanePromptInFlightSet")?;
            if *in_flight {
                if !lane_in_progress(state, *lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} cannot enter prompt-in-flight without being in progress"
                    ));
                }
                if lane_pending(state, *lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} cannot enter prompt-in-flight while pending"
                    ));
                }
                if lane_submit_in_flight(state, *lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} cannot enter prompt-in-flight while submit ack is still pending"
                    ));
                }
                if !state.lane_active_tab.contains_key(lane_id) {
                    return Err(format!(
                        "illegal transition: lane {lane_id} cannot enter prompt-in-flight without an active tab"
                    ));
                }
            }
        }
        ControlEvent::LaneActiveTabSet { lane_id, tab_id } => {
            require_lane(state, *lane_id, "LaneActiveTabSet")?;
            if !lane_in_progress(state, *lane_id)
                && !lane_submit_in_flight(state, *lane_id)
                && !lane_prompt_in_flight(state, *lane_id)
                && state.phase != "bootstrap"
            {
                return Err(format!(
                    "illegal transition: lane {lane_id} cannot bind active tab {tab_id} without executor/verifier work in flight"
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
            if let Some(active_tab) = state.lane_active_tab.get(lane_id) {
                if active_tab != tab_id {
                    return Err(format!(
                        "illegal transition: lane {lane_id} active tab is {active_tab}, cannot map different tab {tab_id}"
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
            if !lane_in_progress(state, *lane_id) {
                return Err(format!(
                    "illegal transition: executor turn registration requires lane {lane_id} to be in progress"
                ));
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
                phase: "verifier".to_string(),
                lane: None,
            },
        )
        .expect_err("verifier phase must require lane");
        assert!(err.contains("requires a lane"));
    }

    #[test]
    fn phase_policy_allows_bootstrap_executor_without_lane() {
        let state = SystemState::new(&[0], 1);
        validate_transition(
            &state,
            &ControlEvent::PhaseSet {
                phase: "executor".to_string(),
                lane: None,
            },
        )
        .expect("bootstrap executor phase should be allowed");
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

    #[test]
    fn lane_claim_requires_pending_to_clear_first() {
        let state = SystemState::new(&[0], 1);
        let err = validate_transition(
            &state,
            &ControlEvent::LaneInProgressSet {
                lane_id: 0,
                actor: Some("executor-0".to_string()),
            },
        )
        .expect_err("pending lane cannot enter in-progress directly");
        assert!(err.contains("clear pending"));
    }

    #[test]
    fn prompt_in_flight_requires_active_tab_and_in_progress() {
        let mut state = SystemState::new(&[0], 1);
        state.lanes.get_mut(&0).unwrap().pending = false;
        state.lanes.get_mut(&0).unwrap().in_progress_by = Some("executor-0".to_string());
        let err = validate_transition(
            &state,
            &ControlEvent::LanePromptInFlightSet {
                lane_id: 0,
                in_flight: true,
            },
        )
        .expect_err("prompt continuation needs active tab");
        assert!(err.contains("active tab"));
    }

    #[test]
    fn submit_in_flight_requires_in_progress_lane() {
        let mut state = SystemState::new(&[0], 1);
        state.lanes.get_mut(&0).unwrap().pending = false;
        let err = validate_transition(
            &state,
            &ControlEvent::LaneSubmitInFlightSet {
                lane_id: 0,
                in_flight: true,
            },
        )
        .expect_err("submit in flight requires in-progress lane");
        assert!(err.contains("in progress"));
    }

    #[test]
    fn legal_lane_claim_and_prompt_continuation_path() {
        let mut state = SystemState::new(&[0], 1);

        validate_transition(
            &state,
            &ControlEvent::LanePendingSet {
                lane_id: 0,
                pending: false,
            },
        )
        .expect("claim starts by clearing pending");
        state.lanes.get_mut(&0).unwrap().pending = false;

        validate_transition(
            &state,
            &ControlEvent::LaneInProgressSet {
                lane_id: 0,
                actor: Some("executor-0".to_string()),
            },
        )
        .expect("lane can enter in-progress after pending clears");
        state.lanes.get_mut(&0).unwrap().in_progress_by = Some("executor-0".to_string());

        validate_transition(
            &state,
            &ControlEvent::LaneActiveTabSet {
                lane_id: 0,
                tab_id: 9,
            },
        )
        .expect("active tab can bind while lane is in progress");
        state.lane_active_tab.insert(0, 9);

        validate_transition(
            &state,
            &ControlEvent::LanePromptInFlightSet {
                lane_id: 0,
                in_flight: true,
            },
        )
        .expect("prompt continuation can begin with active tab on in-progress lane");
    }
}

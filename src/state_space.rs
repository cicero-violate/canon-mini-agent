pub struct CargoTestGate {
    pending_tail_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeFlagInput {
    pub role: &'static str,
    pub modified_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeDecision {
    pub scheduled_phase: Option<String>,
    pub planner_pending: bool,
    pub diagnostics_pending: bool,
    pub executor_wake: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePhaseDecision {
    pub scheduled_phase: Option<String>,
    pub planner_pending: bool,
    pub diagnostics_pending: bool,
}

pub fn decide_resume_phase(
    checkpoint_phase: &str,
    has_verifier_items: bool,
    mut planner_pending: bool,
    mut diagnostics_pending: bool,
) -> ResumePhaseDecision {
    let mut scheduled_phase = Some(checkpoint_phase.to_string());
    if checkpoint_phase == "planner" {
        planner_pending = true;
    }
    if checkpoint_phase == "diagnostics" {
        diagnostics_pending = true;
    }
    if checkpoint_phase == "verifier" && !has_verifier_items {
        planner_pending = true;
        scheduled_phase = Some("planner".to_string());
    }
    ResumePhaseDecision {
        scheduled_phase,
        planner_pending,
        diagnostics_pending,
    }
}

pub fn decide_bootstrap_phase(start_role: &str) -> Option<String> {
    match start_role {
        "planner" => Some("planner".to_string()),
        "diagnostics" => Some("diagnostics".to_string()),
        "executor" => Some("executor".to_string()),
        "solo" => Some("solo".to_string()),
        _ => None,
    }
}

pub fn decide_wake_flags(active_blocker_to_verifier: bool, flags: &[WakeFlagInput]) -> WakeDecision {
    let mut newest: Option<&WakeFlagInput> = None;
    for flag in flags {
        if flag.role == "planner" && active_blocker_to_verifier {
            continue;
        }
        let replace = match newest {
            None => true,
            Some(prev) => flag.modified_ms > prev.modified_ms,
        };
        if replace {
            newest = Some(flag);
        }
    }
    let mut decision = WakeDecision {
        scheduled_phase: None,
        planner_pending: false,
        diagnostics_pending: false,
        executor_wake: false,
    };
    if let Some(flag) = newest {
        decision.scheduled_phase = Some(flag.role.to_string());
        match flag.role {
            "planner" => decision.planner_pending = true,
            "diagnostics" => decision.diagnostics_pending = true,
            "executor" => decision.executor_wake = true,
            _ => {}
        }
    }
    decision
}

pub fn scheduled_phase_resume_done(
    phase: &str,
    planner_pending: bool,
    diagnostics_pending: bool,
    verifier_pending_results: usize,
    verifier_joinset_empty: bool,
    executor_lane_pending: bool,
    executor_in_progress: bool,
) -> bool {
    match phase {
        "planner" => !planner_pending,
        "verifier" => verifier_pending_results == 0 && verifier_joinset_empty,
        "diagnostics" => !diagnostics_pending,
        "executor" => !executor_lane_pending && !executor_in_progress,
        "solo" => true,
        _ => true,
    }
}

pub fn executor_step_limit_exceeded(total_steps: usize, limit: usize) -> bool {
    total_steps >= limit
}

pub fn executor_submit_timed_out(started_ms: u64, now_ms: u64, timeout_ms: u64) -> bool {
    now_ms.saturating_sub(started_ms) >= timeout_ms
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEndpointCheck {
    Ok,
    Mismatch,
}

pub fn check_completion_endpoint(expected: &str, completed: Option<&str>) -> CompletionEndpointCheck {
    match completed {
        Some(endpoint) if endpoint != expected => CompletionEndpointCheck::Mismatch,
        _ => CompletionEndpointCheck::Ok,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionTabCheck {
    Ok,
    Mismatch,
    NoneSet,
}

pub fn check_completion_tab(active_tab: Option<u32>, completed_tab: u32) -> CompletionTabCheck {
    match active_tab {
        Some(active) if active != completed_tab => CompletionTabCheck::Mismatch,
        Some(_) => CompletionTabCheck::Ok,
        None => CompletionTabCheck::NoneSet,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBlockerDecision {
    pub planner_pending: bool,
    pub scheduled_phase: Option<String>,
}

pub fn decide_active_blocker(
    active_blocker_to_verifier: bool,
    planner_pending: bool,
    scheduled_phase: Option<&str>,
) -> ActiveBlockerDecision {
    if active_blocker_to_verifier
        && (planner_pending || matches!(scheduled_phase, Some("planner")))
    {
        return ActiveBlockerDecision {
            planner_pending: false,
            scheduled_phase: if scheduled_phase == Some("planner") {
                None
            } else {
                scheduled_phase.map(|s| s.to_string())
            },
        };
    }
    ActiveBlockerDecision {
        planner_pending,
        scheduled_phase: scheduled_phase.map(|s| s.to_string()),
    }
}

pub fn allow_verifier_run(scheduled_phase: Option<&str>) -> bool {
    !matches!(scheduled_phase, Some(phase) if phase != "verifier")
}

/// Returns true when planner is allowed to run this cycle.
/// Planner is blocked if another phase (not planner) owns the schedule.
pub fn allow_planner_run(scheduled_phase: Option<&str>) -> bool {
    !matches!(scheduled_phase, Some(phase) if phase != "planner")
}

/// Returns true when executor dispatch should be frozen because a resume phase
/// that requires serialized execution (planner, verifier, diagnostics) is active.
pub fn block_executor_dispatch(scheduled_phase: Option<&str>) -> bool {
    matches!(
        scheduled_phase,
        Some("planner") | Some("verifier") | Some("diagnostics") | Some("solo")
    )
}

/// Returns true when diagnostics is allowed to run.
/// Diagnostics must not start while verifier tasks are in flight (would race),
/// and must not run if another phase has exclusive use of the schedule.
pub fn allow_diagnostics_run(scheduled_phase: Option<&str>, verifier_in_flight: bool) -> bool {
    !verifier_in_flight
        && !matches!(scheduled_phase, Some(phase) if phase != "diagnostics")
}

/// The full set of phase eligibility decisions for one orchestrator cycle.
/// Each field answers "can this phase run right now?"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseGates {
    pub planner: bool,
    pub executor: bool,
    pub verifier: bool,
    pub diagnostics: bool,
    pub solo: bool,
}

/// Compute all phase gates at once from the current orchestrator state.
/// Use this as the single source of truth for what can run in a given cycle.
pub fn decide_phase_gates(
    planner_pending: bool,
    diagnostics_pending: bool,
    verifier_queued: bool,
    verifier_in_flight: bool,
    scheduled_phase: Option<&str>,
) -> PhaseGates {
    PhaseGates {
        planner: planner_pending && allow_planner_run(scheduled_phase),
        executor: !block_executor_dispatch(scheduled_phase),
        verifier: verifier_queued && allow_verifier_run(scheduled_phase),
        diagnostics: diagnostics_pending && allow_diagnostics_run(scheduled_phase, verifier_in_flight),
        solo: matches!(scheduled_phase, Some("solo")),
    }
}

/// Returns true when consecutive errors have crossed the threshold that warrants
/// forcing a blocker escalation message rather than retrying.
pub fn should_force_blocker(streak: usize) -> bool {
    streak >= 3
}

/// Returns true when a blocker message is directed specifically at the verifier
/// (i.e. verifier is the root cause, not just a bystander). Verifier should yield
/// to planner for blockers that are NOT verifier-specific.
pub fn is_verifier_specific_blocker(blocker_text: &str, required_action: &str) -> bool {
    let combined = format!("{} {}", blocker_text.to_lowercase(), required_action.to_lowercase());
    combined.contains("verifier")
}

/// When verifier receives an inbound blocker that is not verifier-specific, it must
/// yield the schedule to the phase that owns the blocker. Returns `Some("planner")`
/// to hand off, or `None` if the blocker is verifier-specific (verifier keeps running).
pub fn verifier_blocker_phase_override(is_verifier_specific: bool) -> Option<&'static str> {
    if is_verifier_specific {
        None
    } else {
        Some("planner")
    }
}

/// After diagnostics completes, returns whether planner should be re-triggered.
/// Planner is needed whenever the diagnostics text changed or verifier results changed,
/// because either event may require the planner to revise the plan.
pub fn decide_post_diagnostics(diagnostics_changed: bool, verifier_changed: bool) -> bool {
    diagnostics_changed || verifier_changed
}

impl CargoTestGate {
    pub fn new() -> Self {
        Self {
            pending_tail_path: None,
        }
    }

    pub fn note_action(&mut self, kind: &str, cmd: Option<&str>) {
        if kind != "run_command" {
            return;
        }
        let Some(path) = self.pending_tail_path.as_ref() else {
            return;
        };
        let Some(cmd) = cmd else {
            return;
        };
        if cmd.contains(path) && cmd.contains("tail") {
            self.pending_tail_path = None;
        }
    }

    pub fn note_result(&mut self, kind: &str, output: &str) {
        if kind == "cargo_test" && output.contains("note: cargo test detached") {
            self.pending_tail_path = extract_progress_path_from_result(output);
        }
    }

    pub fn message_blocker_if_needed(&self, kind: &str, workspace: &str) -> Option<String> {
        if kind != "message" {
            return None;
        }
        let path = self.pending_tail_path.as_ref()?;
        Some(format!(
            "Detached cargo test output must be inspected before sending a message. Run:\n```json\n{{\n  \"action\": \"run_command\",\n  \"cmd\": \"tail -n 200 {}\",\n  \"cwd\": \"{}\",\n  \"observation\": \"Inspect live cargo test output.\",\n  \"rationale\": \"Detached cargo test output is in the log file; tail it for progress and failures.\"\n}}\n```\nReturn exactly one action.",
            path, workspace
        ))
    }

    #[cfg(test)]
    pub fn pending_tail_path(&self) -> Option<&str> {
        self.pending_tail_path.as_deref()
    }
}

pub fn extract_progress_path_from_result(result: &str) -> Option<String> {
    for line in result.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("progress_path:") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
        if let Some(idx) = trimmed.find("output_log=") {
            let mut path = trimmed[idx + "output_log=".len()..].trim();
            if let Some(end) = path.find(' ') {
                path = &path[..end];
            }
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

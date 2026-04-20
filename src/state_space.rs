pub struct CargoTestGate {}

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

pub fn decide_wake_flags(
    active_blocker_to_verifier: bool,
    flags: &[WakeFlagInput],
) -> WakeDecision {
    let mut newest: Option<&WakeFlagInput> = None;
    let mut planner_suppressed_by_blocker = false;
    for flag in flags {
        if flag.role == "planner" && active_blocker_to_verifier {
            planner_suppressed_by_blocker = true;
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
    if newest.is_none() && planner_suppressed_by_blocker {
        return decision;
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEndpointCheck {
    Ok,
    Mismatch,
}

pub fn check_completion_endpoint(
    expected: &str,
    completed: Option<&str>,
) -> CompletionEndpointCheck {
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
    if active_blocker_to_verifier && (planner_pending || matches!(scheduled_phase, Some("planner")))
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

pub fn allow_named_phase_run(scheduled_phase: Option<&str>, allowed_phase: &str) -> bool {
    !matches!(scheduled_phase, Some(phase) if phase != allowed_phase)
}

/// Returns true when executor dispatch should be frozen because a resume phase
/// that requires serialized execution (planner, verifier, diagnostics) is active.
pub fn block_executor_dispatch(scheduled_phase: Option<&str>) -> bool {
    matches!(
        scheduled_phase,
        Some("planner") | Some("verifier") | Some("diagnostics")
    )
}

/// Returns true when diagnostics is allowed to run.
/// Diagnostics must not start while verifier tasks are in flight (would race),
/// and must not run if another phase has exclusive use of the schedule.
pub fn allow_diagnostics_run(scheduled_phase: Option<&str>, verifier_in_flight: bool) -> bool {
    !verifier_in_flight && !matches!(scheduled_phase, Some(phase) if phase != "diagnostics")
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

/// Unified semantic control projection used by the orchestrator runtime.
///
/// This compresses the live control surface into one semantic object so phase
/// routing, blocker suppression, diagnostics gating, and executor dispatch do
/// not each re-derive partial control state from parallel booleans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticControlState {
    pub scheduled_phase: Option<String>,
    pub planner_pending: bool,
    pub diagnostics_pending: bool,
    pub verifier_queued: bool,
    pub verifier_in_flight: bool,
    pub active_blocker_to_verifier: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticVerifierView {
    verifier_queued: bool,
    verifier_in_flight: bool,
    active_blocker_to_verifier: bool,
}

impl SemanticControlState {
    pub fn new(
        scheduled_phase: Option<String>,
        planner_pending: bool,
        diagnostics_pending: bool,
        active_blocker_to_verifier: bool,
    ) -> Self {
        Self {
            scheduled_phase,
            planner_pending,
            diagnostics_pending,
            verifier_queued: false,
            verifier_in_flight: false,
            active_blocker_to_verifier,
        }
    }

    pub fn with_verifier_activity(mut self, verifier_queued: bool, verifier_in_flight: bool) -> Self {
        self.verifier_queued = verifier_queued;
        self.verifier_in_flight = verifier_in_flight;
        self
    }

    fn verifier_view(&self) -> SemanticVerifierView {
        SemanticVerifierView {
            verifier_queued: self.verifier_queued,
            verifier_in_flight: self.verifier_in_flight,
            active_blocker_to_verifier: self.active_blocker_to_verifier,
        }
    }

    pub fn active_blocker_decision(&self) -> ActiveBlockerDecision {
        let verifier = self.verifier_view();
        decide_active_blocker(
            verifier.active_blocker_to_verifier,
            self.planner_pending,
            self.scheduled_phase.as_deref(),
        )
    }

    pub fn with_resumed_checkpoint_phase(
        &self,
        checkpoint_phase: &str,
        has_verifier_items: bool,
    ) -> Self {
        let decision = decide_resume_phase(
            checkpoint_phase,
            has_verifier_items,
            self.planner_pending,
            self.diagnostics_pending,
        );
        let mut next = self.clone();
        next.scheduled_phase = decision.scheduled_phase;
        next.planner_pending = decision.planner_pending;
        next.diagnostics_pending = decision.diagnostics_pending;
        next
    }

    pub fn phase_gates(&self) -> PhaseGates {
        let verifier = self.verifier_view();
        decide_phase_gates(
            self.planner_pending,
            self.diagnostics_pending,
            verifier.verifier_queued,
            verifier.verifier_in_flight,
            self.scheduled_phase.as_deref(),
        )
    }

    pub fn executor_dispatch_blocked(&self) -> bool {
        block_executor_dispatch(self.scheduled_phase.as_deref())
    }

    pub fn diagnostics_allowed(&self) -> bool {
        let verifier = self.verifier_view();
        allow_diagnostics_run(self.scheduled_phase.as_deref(), verifier.verifier_in_flight)
    }

    pub fn verifier_run_allowed(&self) -> bool {
        allow_named_phase_run(self.scheduled_phase.as_deref(), "verifier")
    }

    pub fn scheduled_phase_done(
        &self,
        executor_lane_pending: bool,
        executor_in_progress: bool,
    ) -> bool {
        let verifier = self.verifier_view();
        let Some(phase) = self.scheduled_phase.as_deref() else {
            return false;
        };
        scheduled_phase_resume_done(
            phase,
            self.planner_pending,
            self.diagnostics_pending,
            usize::from(verifier.verifier_queued),
            !verifier.verifier_in_flight,
            executor_lane_pending,
            executor_in_progress,
        )
    }
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
        planner: planner_pending && allow_named_phase_run(scheduled_phase, "planner"),
        executor: !block_executor_dispatch(scheduled_phase),
        verifier: verifier_queued && allow_named_phase_run(scheduled_phase, "verifier"),
        diagnostics: diagnostics_pending
            && allow_diagnostics_run(scheduled_phase, verifier_in_flight),
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
    let combined = format!(
        "{} {}",
        blocker_text.to_lowercase(),
        required_action.to_lowercase()
    );
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
///
/// This is intentionally semantic, not artifact-driven: a successful diagnostics
/// cycle itself is enough to warrant planner follow-up because diagnostics may
/// have refreshed derived issue/violation views even when the rendered cache is
/// byte-identical. `verifier_changed` remains an independent semantic trigger.
pub fn decide_post_diagnostics(diagnostics_ran: bool, verifier_changed: bool) -> bool {
    diagnostics_ran || verifier_changed
}

pub fn diagnostics_pending_reason_count(verifier_changed: bool) -> usize {
    usize::from(verifier_changed)
}

pub fn planner_pending_reason_count(
    objective_review_required: bool,
    objectives_updated: bool,
    has_objective_work: bool,
    has_plan_work: bool,
) -> usize {
    usize::from(objective_review_required && !objectives_updated)
        + usize::from(has_objective_work && !has_plan_work)
}

impl CargoTestGate {
    pub fn new() -> Self {
        Self {}
    }

    pub fn note_action(&mut self, kind: &str, cmd: Option<&str>) {
        let _ = (kind, cmd);
    }

    pub fn note_result(&mut self, kind: &str, output: &str) {
        let _ = (kind, output);
    }

    pub fn message_blocker_if_needed(&self, kind: &str, workspace: &str) -> Option<String> {
        let _ = (kind, workspace);
        None
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
        if let Some(rest) = trimmed.strip_prefix("output_log:") {
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

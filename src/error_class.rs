/// Error classification taxonomy for the canon-mini-agent orchestrator.
///
/// Every bad outcome that the system can observe must be expressible as one of
/// these classes.  The classes drive two downstream systems:
///
/// 1. **Blocker capture** (`blockers.rs`) — every `message{type=blocker}` and
///    every `ok=false` action result is classified and written to
///    `agent_state/blockers.json`.
///
/// 2. **Invariant promotion** (`invariants.rs`) — the synthesis step reads
///    classified blockers instead of doing heuristic text matching.  Repeated
///    observations of the same class accumulate `support_count` and eventually
///    promote to a gate-enforced invariant.
///
/// ## Pipeline
///
///   bad path
///     → classify → ErrorClass
///     → append to blockers.json
///     → synthesis groups by (actor_kind, class)
///     → invariant promoted when support_count ≥ threshold
///     → gate blocks the transition

use serde::{Deserialize, Serialize};

/// Canonical classes of bad outcome observable by the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// A file, symbol, or target referenced in a task does not exist.
    MissingTarget,
    /// An action emitted by an LLM role failed schema validation.
    InvalidSchema,
    /// An executor reached its step budget without completing the task.
    StepLimitExceeded,
    /// A planner task referenced a symbol not found in the semantic graph.
    PlanPreflightFailed,
    /// `cargo build` or `cargo test` returned non-zero exit.
    CompileError,
    /// A path access was outside the permitted workspace boundary.
    PermissionDenied,
    /// The same file was read N times by the same role without any mutation.
    ReadFileStall,
    /// A role was dispatched when its preconditions were not satisfied
    /// (e.g. executor dispatched with no ready tasks).
    InvalidRoute,
    /// The LLM explicitly emitted `message{type=blocker, status=blocked}`.
    BlockerEscalated,
    /// A `plan` operation was attempted by a role that is not permitted to use it.
    UnauthorizedPlanOp,
    /// A verification step produced a result that contradicts the expected state.
    VerificationFailed,
    /// A network / LLM endpoint call timed out or returned a connection error.
    LlmTimeout,
    /// The orchestrator detected a livelock: consecutive cycles with no watched-file state change.
    LivelockDetected,
    /// Anything that does not fit the above classes.
    Unknown,
}

impl ErrorClass {
    /// Stable string key used in state conditions for invariant fingerprinting.
    pub fn as_key(&self) -> &'static str {
        match self {
            ErrorClass::MissingTarget => "missing_target",
            ErrorClass::InvalidSchema => "invalid_schema",
            ErrorClass::StepLimitExceeded => "step_limit_exceeded",
            ErrorClass::PlanPreflightFailed => "plan_preflight_failed",
            ErrorClass::CompileError => "compile_error",
            ErrorClass::PermissionDenied => "permission_denied",
            ErrorClass::ReadFileStall => "read_file_stall",
            ErrorClass::InvalidRoute => "invalid_route",
            ErrorClass::BlockerEscalated => "blocker_escalated",
            ErrorClass::UnauthorizedPlanOp => "unauthorized_plan_op",
            ErrorClass::VerificationFailed => "verification_failed",
            ErrorClass::LlmTimeout => "llm_timeout",
            ErrorClass::LivelockDetected => "livelock_detected",
            ErrorClass::Unknown => "unknown",
        }
    }

    /// Human-readable description used in invariant `predicate_text`.
    pub fn description(&self) -> &'static str {
        match self {
            ErrorClass::MissingTarget =>
                "action referenced a target (file/symbol) that does not exist",
            ErrorClass::InvalidSchema =>
                "role emitted a structurally invalid action that failed schema validation",
            ErrorClass::StepLimitExceeded =>
                "executor reached step budget without completing the task",
            ErrorClass::PlanPreflightFailed =>
                "planner task referenced a symbol not found in the workspace semantic graph",
            ErrorClass::CompileError =>
                "cargo build or test returned a non-zero exit code",
            ErrorClass::PermissionDenied =>
                "path access was outside the permitted workspace boundary",
            ErrorClass::ReadFileStall =>
                "same file read multiple times by the same role without any mutation",
            ErrorClass::InvalidRoute =>
                "role was dispatched when its required preconditions were not satisfied",
            ErrorClass::BlockerEscalated =>
                "LLM role explicitly declared itself blocked and cannot proceed",
            ErrorClass::UnauthorizedPlanOp =>
                "role attempted a plan operation it is not permitted to perform",
            ErrorClass::VerificationFailed =>
                "verification produced a result that contradicts the expected system state",
            ErrorClass::LlmTimeout =>
                "LLM endpoint call timed out or returned a connection error",
            ErrorClass::LivelockDetected =>
                "orchestrator detected livelock: no watched-file state change after consecutive cycles",
            ErrorClass::Unknown =>
                "unclassified bad outcome",
        }
    }
}

// ── Classification functions ──────────────────────────────────────────────────

/// Classify an action result from the action kind, result text, and ok flag.
/// Called when logging `ok=false` action results.
pub fn classify_result(action_kind: &str, result_text: &str, ok: bool) -> ErrorClass {
    if ok {
        return ErrorClass::Unknown; // only classify failures
    }
    let text = result_text.to_lowercase();
    match action_kind {
        "plan_preflight" => return ErrorClass::PlanPreflightFailed,
        "route_dispatch" => return ErrorClass::InvalidRoute,
        "cargo_test" | "cargo_clippy" | "run_command" => {
            if text.contains("error[e") || text.contains("compilation failed") || text.contains("test failed") {
                return ErrorClass::CompileError;
            }
        }
        "read_file" => {
            if text.contains("outside") || text.contains("permission denied") {
                return ErrorClass::PermissionDenied;
            }
            if text.contains("not found") || text.contains("no such file") {
                return ErrorClass::MissingTarget;
            }
        }
        "apply_patch" => {
            if text.contains("outside") || text.contains("permission denied") {
                return ErrorClass::PermissionDenied;
            }
            if text.contains("not found") || text.contains("no such file") {
                return ErrorClass::MissingTarget;
            }
        }
        "plan" => {
            if text.contains("not allowed") || text.contains("only") || text.contains("permitted") {
                return ErrorClass::UnauthorizedPlanOp;
            }
        }
        _ => {}
    }
    // Cross-cutting text patterns
    if text.contains("outside") && (text.contains("workspace") || text.contains("permitted")) {
        return ErrorClass::PermissionDenied;
    }
    if text.contains("step limit") || text.contains("step budget") || text.contains("forced handoff") {
        return ErrorClass::StepLimitExceeded;
    }
    if text.contains("schema") || text.contains("required field") || text.contains("invalid action") {
        return ErrorClass::InvalidSchema;
    }
    if text.contains("not found") || text.contains("no such file") || text.contains("missing_target") || text.contains("does not exist") {
        return ErrorClass::MissingTarget;
    }
    if text.contains("timed out") || text.contains("connection refused") || text.contains("timeout") {
        return ErrorClass::LlmTimeout;
    }
    if text.contains("permission denied") || text.contains("access denied") {
        return ErrorClass::PermissionDenied;
    }
    if text.contains("not allowed") || text.contains("not permitted") {
        return ErrorClass::UnauthorizedPlanOp;
    }
    ErrorClass::Unknown
}

/// Classify a blocker summary string from a `message{type=blocker}` action.
/// This is a best-effort heuristic; the LLM's free-text summary is the input.
pub fn classify_blocker_summary(summary: &str) -> ErrorClass {
    let text = summary.to_lowercase();
    if text.contains("step limit") || text.contains("budget") || text.contains("too many steps") {
        return ErrorClass::StepLimitExceeded;
    }
    if text.contains("compile") || text.contains("build fail") || text.contains("test fail") || text.contains("cargo") {
        return ErrorClass::CompileError;
    }
    if text.contains("not found") || text.contains("does not exist") || text.contains("missing") {
        return ErrorClass::MissingTarget;
    }
    if text.contains("schema") || text.contains("invalid action") || text.contains("required field") {
        return ErrorClass::InvalidSchema;
    }
    if text.contains("outside workspace") || text.contains("permission") {
        return ErrorClass::PermissionDenied;
    }
    if text.contains("verify") || text.contains("verification") || text.contains("test") {
        return ErrorClass::VerificationFailed;
    }
    if text.contains("symbol") || text.contains("preflight") {
        return ErrorClass::PlanPreflightFailed;
    }
    ErrorClass::BlockerEscalated
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_permission_denied_outside_workspace() {
        let class = classify_result("apply_patch", "path is outside the permitted workspace", false);
        assert_eq!(class, ErrorClass::PermissionDenied);
    }

    #[test]
    fn classify_missing_target_no_such_file() {
        let class = classify_result("read_file", "No such file or directory: src/foo.rs", false);
        assert_eq!(class, ErrorClass::MissingTarget);
    }

    #[test]
    fn classify_plan_preflight_action() {
        let class = classify_result("plan_preflight", "missing symbol", false);
        assert_eq!(class, ErrorClass::PlanPreflightFailed);
    }

    #[test]
    fn classify_ok_result_is_unknown() {
        let class = classify_result("read_file", "file contents", true);
        assert_eq!(class, ErrorClass::Unknown);
    }

    #[test]
    fn classify_blocker_compile() {
        let class = classify_blocker_summary("blocked because cargo build failed with error[E0308]");
        assert_eq!(class, ErrorClass::CompileError);
    }

    #[test]
    fn classify_blocker_step_limit() {
        let class = classify_blocker_summary("reached step budget, cannot complete");
        assert_eq!(class, ErrorClass::StepLimitExceeded);
    }

    #[test]
    fn error_class_key_is_stable() {
        assert_eq!(ErrorClass::MissingTarget.as_key(), "missing_target");
        assert_eq!(ErrorClass::BlockerEscalated.as_key(), "blocker_escalated");
    }
}

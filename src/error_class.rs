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
    /// A canonical state mutation bypassed `CanonicalWriter::apply`.
    SecondMutationPath,
    /// Runtime-only state influenced control flow without canonical representation.
    RuntimeControlBypass,
    /// A recovery/reconciliation path changed behavior without an explicit canonical event.
    UncanonicalizedRecoveryPath,
    /// Runtime state diverged from checkpoint/canonical state beyond the allowed recovery window.
    CheckpointRuntimeDivergence,
    /// An effect changed canonical behavior without a corresponding `ControlEvent`.
    EffectfulStateAdvanceWithoutControlEvent,
    /// A single `ControlEvent` encodes multiple logically distinct transitions.
    AmbiguousControlEvent,
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
    /// LLM returned a response but no extractable JSON action block (prose-only reply).
    ReactionOnly,
    /// Anything that does not fit the above classes.
    Unknown,
}

impl ErrorClass {
    /// Stable string key used in state conditions for invariant fingerprinting.
    pub fn as_key(&self) -> &'static str {
        match self {
            ErrorClass::SecondMutationPath => "second_mutation_path",
            ErrorClass::RuntimeControlBypass => "runtime_control_bypass",
            ErrorClass::UncanonicalizedRecoveryPath => "uncanonicalized_recovery_path",
            ErrorClass::CheckpointRuntimeDivergence => "checkpoint_runtime_divergence",
            ErrorClass::EffectfulStateAdvanceWithoutControlEvent => {
                "effectful_state_advance_without_control_event"
            }
            ErrorClass::AmbiguousControlEvent => "ambiguous_control_event",
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
            ErrorClass::ReactionOnly => "reaction_only",
            ErrorClass::Unknown => "unknown",
        }
    }

    /// Human-readable description used in invariant `predicate_text`.
    pub fn description(&self) -> &'static str {
        match self {
            ErrorClass::SecondMutationPath =>
                "canonical state changed through a path other than CanonicalWriter::apply",
            ErrorClass::RuntimeControlBypass =>
                "runtime-only state influenced control flow without canonical representation",
            ErrorClass::UncanonicalizedRecoveryPath =>
                "a recovery path changed behavior without an explicit canonical event",
            ErrorClass::CheckpointRuntimeDivergence =>
                "runtime state diverged from checkpoint or canonical state beyond the allowed recovery window",
            ErrorClass::EffectfulStateAdvanceWithoutControlEvent =>
                "an effect changed canonical behavior without a corresponding control event",
            ErrorClass::AmbiguousControlEvent =>
                "a control event encoded multiple logically distinct transitions",
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
            ErrorClass::ReactionOnly =>
                "LLM returned a prose-only response with no extractable JSON action block",
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
    if let Some(class) = classify_action_kind_failure(action_kind, &text) {
        return class;
    }
    classify_failure_text(&text)
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn is_compile_failure_text(text: &str) -> bool {
    contains_any(text, &["error[e", "compilation failed", "test failed"])
}

fn is_permission_boundary_text(text: &str) -> bool {
    text.contains("outside") || text.contains("permission denied")
}

fn is_missing_target_action_text(text: &str) -> bool {
    contains_any(text, &["not found", "no such file"])
}

fn is_unauthorized_plan_text(text: &str) -> bool {
    contains_any(text, &["not allowed", "only", "permitted"])
}

fn is_step_limit_text(text: &str) -> bool {
    contains_any(text, &["step limit", "step budget", "forced handoff"])
}

fn is_invalid_schema_text(text: &str) -> bool {
    contains_any(text, &["schema", "required field", "invalid action"])
}

fn is_second_mutation_text(text: &str) -> bool {
    contains_any(
        text,
        &["second mutation path", "canonical state bypass", "state_mut"],
    )
}

fn is_runtime_control_bypass_text(text: &str) -> bool {
    contains_any(
        text,
        &[
            "runtime control bypass",
            "runtime-only control influence",
            "runtime-only state influenced control",
        ],
    )
}

fn is_uncanonicalized_recovery_text(text: &str) -> bool {
    contains_any(
        text,
        &[
            "uncanonicalized recovery",
            "recovery path without canonical event",
            "reconciliation path without canonical event",
        ],
    )
}

fn is_checkpoint_runtime_divergence_text(text: &str) -> bool {
    contains_any(
        text,
        &[
            "checkpoint/runtime divergence",
            "checkpoint runtime divergence",
            "runtime diverged from checkpoint",
        ],
    )
}

fn is_effectful_state_advance_text(text: &str) -> bool {
    contains_any(
        text,
        &[
            "effectful state advance without control event",
            "effect advanced state without controlevent",
            "effect advanced state without control event",
        ],
    )
}

fn is_ambiguous_control_event_text(text: &str) -> bool {
    contains_any(
        text,
        &[
            "ambiguous control event",
            "control event encoded multiple transitions",
        ],
    )
}

fn is_missing_target_text(text: &str) -> bool {
    contains_any(
        text,
        &["not found", "no such file", "missing_target", "does not exist"],
    )
}

fn is_timeout_text(text: &str) -> bool {
    contains_any(text, &["timed out", "connection refused", "timeout"])
}

fn is_permission_denied_text(text: &str) -> bool {
    contains_any(text, &["permission denied", "access denied"])
}

fn is_blocker_tool_unavailable_text(text: &str) -> bool {
    contains_any(
        text,
        &[
            "tool unavailable",
            "tools unavailable",
            "workspace tools unavailable",
            "toolchain unavailable",
            "unavailable in this chat environment",
            "cannot proceed without the required canon toolchain",
        ],
    )
}

fn is_compile_blocker_text(text: &str) -> bool {
    contains_any(text, &["compile", "build fail", "test fail", "cargo"])
}

fn is_verification_blocker_text(text: &str) -> bool {
    contains_any(text, &["verify", "verification", "test"])
}

fn is_plan_preflight_blocker_text(text: &str) -> bool {
    contains_any(text, &["symbol", "preflight"])
}

fn classify_action_kind_failure(action_kind: &str, text: &str) -> Option<ErrorClass> {
    match action_kind {
        "canonical_state_bypass" => Some(ErrorClass::SecondMutationPath),
        "runtime_control_bypass" => Some(ErrorClass::RuntimeControlBypass),
        "uncanonicalized_recovery" => Some(ErrorClass::UncanonicalizedRecoveryPath),
        "checkpoint_runtime_divergence" => Some(ErrorClass::CheckpointRuntimeDivergence),
        "effectful_state_advance" => Some(ErrorClass::EffectfulStateAdvanceWithoutControlEvent),
        "ambiguous_control_event" => Some(ErrorClass::AmbiguousControlEvent),
        "plan_preflight" => Some(ErrorClass::PlanPreflightFailed),
        "route_dispatch" => Some(ErrorClass::InvalidRoute),
        "step_limit" => Some(ErrorClass::StepLimitExceeded),
        "livelock" => Some(ErrorClass::LivelockDetected),
        "build_gate" => Some(ErrorClass::CompileError),
        "solo_completion_gate" | "diagnostics_evidence_gate" => {
            Some(ErrorClass::VerificationFailed)
        }
        "handoff_delivery" => Some(ErrorClass::InvalidRoute),
        "reaction_only" => Some(ErrorClass::ReactionOnly),
        "executor_submit_timeout" | "submit_ack_timeout" => Some(ErrorClass::LlmTimeout),
        "repeated_failed_action" | "idle_streak" => Some(ErrorClass::InvalidSchema),
        "cargo_test" | "cargo_clippy" | "run_command" if is_compile_failure_text(text) =>
        {
            Some(ErrorClass::CompileError)
        }
        "read_file" | "apply_patch" if is_permission_boundary_text(text) => {
            Some(ErrorClass::PermissionDenied)
        }
        "read_file" | "apply_patch" if is_missing_target_action_text(text) => {
            Some(ErrorClass::MissingTarget)
        }
        "plan" if is_unauthorized_plan_text(text) => {
            Some(ErrorClass::UnauthorizedPlanOp)
        }
        _ => None,
    }
}

fn classify_failure_text(text: &str) -> ErrorClass {
    if let Some(classification) = classify_permission_or_authorization_text(text) {
        return classification;
    }
    if is_step_limit_text(text) {
        return ErrorClass::StepLimitExceeded;
    }
    if is_invalid_schema_text(text) {
        return ErrorClass::InvalidSchema;
    }
    if is_second_mutation_text(text) {
        return ErrorClass::SecondMutationPath;
    }
    if is_runtime_control_bypass_text(text) {
        return ErrorClass::RuntimeControlBypass;
    }
    if is_uncanonicalized_recovery_text(text) {
        return ErrorClass::UncanonicalizedRecoveryPath;
    }
    if is_checkpoint_runtime_divergence_text(text) {
        return ErrorClass::CheckpointRuntimeDivergence;
    }
    if is_effectful_state_advance_text(text) {
        return ErrorClass::EffectfulStateAdvanceWithoutControlEvent;
    }
    if is_ambiguous_control_event_text(text) {
        return ErrorClass::AmbiguousControlEvent;
    }
    if is_missing_target_text(text) {
        return ErrorClass::MissingTarget;
    }
    if is_timeout_text(text) {
        return ErrorClass::LlmTimeout;
    }
    if is_permission_denied_text(text) {
        return ErrorClass::PermissionDenied;
    }
    ErrorClass::Unknown
}

fn classify_permission_or_authorization_text(text: &str) -> Option<ErrorClass> {
    if text.contains("outside") && (text.contains("workspace") || text.contains("permitted")) {
        return Some(ErrorClass::PermissionDenied);
    }
    if contains_any(text, &["not allowed", "not permitted"]) {
        return Some(ErrorClass::UnauthorizedPlanOp);
    }
    None
}

/// Classify a blocker summary string from a `message{type=blocker}` action.
/// This is a best-effort heuristic; the LLM's free-text summary is the input.
pub fn classify_blocker_summary(summary: &str) -> ErrorClass {
    let text = summary.to_lowercase();
    if is_blocker_tool_unavailable_text(&text) {
        return ErrorClass::PermissionDenied;
    }
    if contains_any(&text, &["step limit", "budget", "too many steps"]) {
        return ErrorClass::StepLimitExceeded;
    }
    if is_compile_blocker_text(&text) {
        return ErrorClass::CompileError;
    }
    if contains_any(&text, &["not found", "does not exist", "missing"]) {
        return ErrorClass::MissingTarget;
    }
    if is_invalid_schema_text(&text) {
        return ErrorClass::InvalidSchema;
    }
    if is_second_mutation_text(&text) {
        return ErrorClass::SecondMutationPath;
    }
    if is_runtime_control_bypass_text(&text) {
        return ErrorClass::RuntimeControlBypass;
    }
    if is_uncanonicalized_recovery_text(&text) {
        return ErrorClass::UncanonicalizedRecoveryPath;
    }
    if is_checkpoint_runtime_divergence_text(&text) {
        return ErrorClass::CheckpointRuntimeDivergence;
    }
    if is_effectful_state_advance_text(&text) {
        return ErrorClass::EffectfulStateAdvanceWithoutControlEvent;
    }
    if is_ambiguous_control_event_text(&text) {
        return ErrorClass::AmbiguousControlEvent;
    }
    if text.contains("outside workspace") || text.contains("permission") {
        return ErrorClass::PermissionDenied;
    }
    if is_verification_blocker_text(&text) {
        return ErrorClass::VerificationFailed;
    }
    if is_plan_preflight_blocker_text(&text) {
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
        let class = classify_result(
            "apply_patch",
            "path is outside the permitted workspace",
            false,
        );
        assert_eq!(class, ErrorClass::PermissionDenied);
    }

    #[test]
    fn classify_second_mutation_path_explicitly() {
        let class = classify_result(
            "canonical_state_bypass",
            "second mutation path detected via state_mut",
            false,
        );
        assert_eq!(class, ErrorClass::SecondMutationPath);
    }

    #[test]
    fn classify_runtime_control_bypass_explicitly() {
        let class = classify_result(
            "runtime_control_bypass",
            "runtime-only control influence detected in route selection",
            false,
        );
        assert_eq!(class, ErrorClass::RuntimeControlBypass);
    }

    #[test]
    fn classify_uncanonicalized_recovery_path_explicitly() {
        let class = classify_result(
            "uncanonicalized_recovery",
            "recovery path without canonical event detected",
            false,
        );
        assert_eq!(class, ErrorClass::UncanonicalizedRecoveryPath);
    }

    #[test]
    fn classify_blocker_ambiguous_control_event() {
        let class = classify_blocker_summary(
            "blocked by ambiguous control event because one control event encoded multiple transitions",
        );
        assert_eq!(class, ErrorClass::AmbiguousControlEvent);
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
        let class =
            classify_blocker_summary("blocked because cargo build failed with error[E0308]");
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
        assert_eq!(
            ErrorClass::RuntimeControlBypass.as_key(),
            "runtime_control_bypass"
        );
    }
}

use crate::error_class::ErrorClass;
use serde::{Deserialize, Serialize};

/// Typed recovery policies. Policy selection is pure; runtime code actuates
/// only bounded canonical transitions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    ClearExecutorAndWakePlanner,
    RetireTransportAndRetry,
    RouteCompilerEvidenceToExecutor,
    ShrinkPromptAndRetry,
    RefreshProjectionBounded,
    ReplayTlogAndPurgeInvalidRuntimeState,
    EscalateDiagnostics,
    EscalateSolo,
    Suppress,
}

impl RecoveryPolicy {
    pub fn as_key(&self) -> &'static str {
        match self {
            RecoveryPolicy::ClearExecutorAndWakePlanner => "clear_executor_and_wake_planner",
            RecoveryPolicy::RetireTransportAndRetry => "retire_transport_and_retry",
            RecoveryPolicy::RouteCompilerEvidenceToExecutor => "route_compiler_evidence_to_executor",
            RecoveryPolicy::ShrinkPromptAndRetry => "shrink_prompt_and_retry",
            RecoveryPolicy::RefreshProjectionBounded => "refresh_projection_bounded",
            RecoveryPolicy::ReplayTlogAndPurgeInvalidRuntimeState => {
                "replay_tlog_and_purge_invalid_runtime_state"
            }
            RecoveryPolicy::EscalateDiagnostics => "escalate_diagnostics",
            RecoveryPolicy::EscalateSolo => "escalate_solo",
            RecoveryPolicy::Suppress => "suppress",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    RecordTriggeredEffect,
    ClearExecutorPendingLanes,
    ConsumeExecutorWake,
    SchedulePlanner,
    SetPlannerPending,
    RetireTransport,
    RetryRole,
    RefreshProjectionBounded,
    EscalateToDiagnostics,
    EscalateToSolo,
    Suppress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryThreshold {
    pub class: ErrorClass,
    pub min_count: usize,
    pub window_ms: u64,
    pub max_attempts: usize,
    pub policy: RecoveryPolicy,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDecision {
    pub class: ErrorClass,
    pub policy: RecoveryPolicy,
    pub reason: String,
    pub support_count: usize,
    pub threshold: usize,
    pub window_ms: u64,
    pub max_attempts: usize,
    pub canonical_actions: Vec<RecoveryAction>,
}

#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    pub thresholds: Vec<RecoveryThreshold>,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            thresholds: vec![
                threshold(
                    ErrorClass::SecondMutationPath,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateSolo,
                ),
                threshold(
                    ErrorClass::RuntimeControlBypass,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::ReplayTlogAndPurgeInvalidRuntimeState,
                ),
                threshold(
                    ErrorClass::UncanonicalizedRecoveryPath,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateSolo,
                ),
                threshold(
                    ErrorClass::CheckpointRuntimeDivergence,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::ReplayTlogAndPurgeInvalidRuntimeState,
                ),
                threshold(
                    ErrorClass::EffectfulStateAdvanceWithoutControlEvent,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateSolo,
                ),
                threshold(
                    ErrorClass::AmbiguousControlEvent,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
                threshold(
                    ErrorClass::MissingTarget,
                    2,
                    300_000,
                    2,
                    RecoveryPolicy::ClearExecutorAndWakePlanner,
                ),
                threshold(
                    ErrorClass::InvalidRoute,
                    3,
                    300_000,
                    2,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
                threshold(
                    ErrorClass::LlmTimeout,
                    1,
                    300_000,
                    2,
                    RecoveryPolicy::RetireTransportAndRetry,
                ),
                threshold(
                    ErrorClass::CompileError,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::RouteCompilerEvidenceToExecutor,
                ),
                threshold(
                    ErrorClass::VerificationFailed,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::RouteCompilerEvidenceToExecutor,
                ),
                threshold(
                    ErrorClass::InvalidSchema,
                    3,
                    300_000,
                    2,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
                threshold(
                    ErrorClass::StepLimitExceeded,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
                threshold(
                    ErrorClass::PlanPreflightFailed,
                    1,
                    300_000,
                    2,
                    RecoveryPolicy::ClearExecutorAndWakePlanner,
                ),
                threshold(
                    ErrorClass::PermissionDenied,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateSolo,
                ),
                threshold(
                    ErrorClass::ReadFileStall,
                    3,
                    300_000,
                    2,
                    RecoveryPolicy::ShrinkPromptAndRetry,
                ),
                threshold(
                    ErrorClass::ProjectionRefreshStalled,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::RefreshProjectionBounded,
                ),
                threshold(
                    ErrorClass::BlockerEscalated,
                    2,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
                threshold(
                    ErrorClass::UnauthorizedPlanOp,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateSolo,
                ),
                threshold(
                    ErrorClass::LivelockDetected,
                    1,
                    300_000,
                    1,
                    RecoveryPolicy::ReplayTlogAndPurgeInvalidRuntimeState,
                ),
                threshold(
                    ErrorClass::ReactionOnly,
                    2,
                    300_000,
                    2,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
                threshold(
                    ErrorClass::Unknown,
                    3,
                    300_000,
                    1,
                    RecoveryPolicy::EscalateDiagnostics,
                ),
            ],
        }
    }
}

impl RecoveryConfig {
    pub fn threshold_for_class(&self, class: &ErrorClass) -> Option<&RecoveryThreshold> {
        self.thresholds
            .iter()
            .find(|threshold| threshold.enabled && &threshold.class == class)
    }
}

fn threshold(
    class: ErrorClass,
    min_count: usize,
    window_ms: u64,
    max_attempts: usize,
    policy: RecoveryPolicy,
) -> RecoveryThreshold {
    RecoveryThreshold {
        class,
        min_count,
        window_ms,
        max_attempts,
        policy,
        enabled: true,
    }
}

pub fn decision_for_failure(
    class: ErrorClass,
    reason: &str,
    support_count: usize,
    config: &RecoveryConfig,
) -> Option<RecoveryDecision> {
    let threshold = config.threshold_for_class(&class)?;
    if support_count < threshold.min_count {
        return None;
    }

    Some(RecoveryDecision {
        class,
        policy: threshold.policy.clone(),
        reason: reason.to_string(),
        support_count,
        threshold: threshold.min_count,
        window_ms: threshold.window_ms,
        max_attempts: threshold.max_attempts,
        canonical_actions: canonical_actions_for_policy(&threshold.policy),
    })
}

pub fn decision_for_route_gate_block(reason: &str, support_count: usize) -> Option<RecoveryDecision> {
    let class = classify_route_gate_reason(reason)?;
    decision_for_failure(class, reason, support_count, &RecoveryConfig::default())
}

pub fn classify_route_gate_reason(reason: &str) -> Option<ErrorClass> {
    let text = reason.to_ascii_lowercase();
    if text.contains("does not exist") || text.contains("missing_target") {
        return Some(ErrorClass::MissingTarget);
    }
    if text.contains("invalid_route") {
        return Some(ErrorClass::InvalidRoute);
    }
    let summary_class = crate::error_class::classify_blocker_summary(&text);
    if matches!(summary_class, ErrorClass::ProjectionRefreshStalled) {
        return Some(summary_class);
    }
    for class in ErrorClass::ALL {
        if reason_mentions_error_class(&text, &class) {
            return Some(class);
        }
    }
    None
}

pub fn reason_mentions_error_class(lowercase_reason: &str, class: &ErrorClass) -> bool {
    let key = class.as_key();
    lowercase_reason.contains(key) || lowercase_reason.contains(&key.replace('_', " "))
}

pub fn canonical_actions_for_policy(policy: &RecoveryPolicy) -> Vec<RecoveryAction> {
    match policy {
        RecoveryPolicy::ClearExecutorAndWakePlanner => vec![
            RecoveryAction::RecordTriggeredEffect,
            RecoveryAction::ClearExecutorPendingLanes,
            RecoveryAction::ConsumeExecutorWake,
            RecoveryAction::SchedulePlanner,
            RecoveryAction::SetPlannerPending,
        ],
        RecoveryPolicy::RetireTransportAndRetry => {
            vec![RecoveryAction::RecordTriggeredEffect, RecoveryAction::RetryRole]
        }
        RecoveryPolicy::RouteCompilerEvidenceToExecutor => vec![
            RecoveryAction::RecordTriggeredEffect,
            RecoveryAction::RetryRole,
        ],
        RecoveryPolicy::ShrinkPromptAndRetry => {
            vec![RecoveryAction::RecordTriggeredEffect, RecoveryAction::RetryRole]
        }
        RecoveryPolicy::RefreshProjectionBounded => vec![
            RecoveryAction::RecordTriggeredEffect,
            RecoveryAction::RefreshProjectionBounded,
            RecoveryAction::RetryRole,
        ],
        RecoveryPolicy::ReplayTlogAndPurgeInvalidRuntimeState => {
            vec![RecoveryAction::RecordTriggeredEffect, RecoveryAction::RetryRole]
        }
        RecoveryPolicy::EscalateDiagnostics => vec![
            RecoveryAction::RecordTriggeredEffect,
            RecoveryAction::EscalateToDiagnostics,
        ],
        RecoveryPolicy::EscalateSolo => vec![
            RecoveryAction::RecordTriggeredEffect,
            RecoveryAction::EscalateToSolo,
        ],
        RecoveryPolicy::Suppress => vec![RecoveryAction::Suppress],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_target_repeated_selects_clear_executor_and_wake_planner() {
        let reason = "Action targeted a path that does not exist";
        let decision = decision_for_route_gate_block(reason, 2).expect("decision");

        assert_eq!(decision.class, ErrorClass::MissingTarget);
        assert_eq!(
            decision.policy,
            RecoveryPolicy::ClearExecutorAndWakePlanner
        );
        assert_eq!(decision.threshold, 2);
        assert!(decision
            .canonical_actions
            .contains(&RecoveryAction::ClearExecutorPendingLanes));
        assert!(decision
            .canonical_actions
            .contains(&RecoveryAction::SetPlannerPending));
    }

    #[test]
    fn missing_target_below_threshold_is_not_recovered() {
        let reason = "Action targeted a path that does not exist";
        assert!(decision_for_route_gate_block(reason, 1).is_none());
    }

    #[test]
    fn projection_refresh_stall_selects_bounded_projection_refresh() {
        let reason = "refresh pid is still running and latest.json remains stale";
        let decision = decision_for_route_gate_block(reason, 1).expect("decision");

        assert_eq!(decision.class, ErrorClass::ProjectionRefreshStalled);
        assert_eq!(decision.policy, RecoveryPolicy::RefreshProjectionBounded);
        assert_eq!(decision.threshold, 1);
        assert!(decision
            .canonical_actions
            .contains(&RecoveryAction::RefreshProjectionBounded));
        assert!(decision
            .canonical_actions
            .contains(&RecoveryAction::RetryRole));
    }

    #[test]
    fn repeated_runtime_classes_have_default_policies() {
        let config = RecoveryConfig::default();
        for class in ErrorClass::ALL.iter().cloned() {
            assert!(
                config.threshold_for_class(&class).is_some(),
                "missing recovery threshold for {}",
                class.as_key()
            );
            assert_eq!(
                config
                    .thresholds
                    .iter()
                    .filter(|threshold| threshold.enabled && threshold.class == class)
                    .count(),
                1,
                "duplicate recovery threshold for {}",
                class.as_key()
            );
        }
    }

    #[test]
    fn safety_sensitive_classes_escalate_to_solo() {
        let config = RecoveryConfig::default();
        for class in [
            ErrorClass::SecondMutationPath,
            ErrorClass::UncanonicalizedRecoveryPath,
            ErrorClass::EffectfulStateAdvanceWithoutControlEvent,
            ErrorClass::PermissionDenied,
            ErrorClass::UnauthorizedPlanOp,
        ] {
            assert_eq!(
                config.threshold_for_class(&class).map(|t| &t.policy),
                Some(&RecoveryPolicy::EscalateSolo),
                "{} should require solo escalation",
                class.as_key()
            );
        }
    }
}

//! Central evaluation facade.
//!
//! `evaluation` owns pure scoring kernels; `eval_driver` owns the tlog write.
//! This module gives callers one stable `eval` surface without duplicating logic.

pub use crate::eval_driver::run;
pub use crate::evaluation::{
    compute_delta, compute_eval, diagnostics_repair_pressure,
    diagnostics_repair_pressure_with_issues, evaluate_repo_state,
    evaluate_tlog_delta_invariants, evaluate_workspace, issue_health_score,
    reward_alignment_score, safety_score, task_velocity_score, EvalDelta, EvalInput,
    EvaluationVector, EvaluationWorkspaceSnapshot, StructuralInvariantCoverage,
    TlogDeltaSignals,
};
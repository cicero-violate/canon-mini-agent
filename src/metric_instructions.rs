// Re-export from repair_plans — metric_instructions is superseded by the
// repair_plans module which covers eval_metric, invariant, and blocker_class
// plan kinds.  This stub keeps existing call sites compiling during migration.
pub use crate::repair_plans::{
    build_eval_metric_plans as build_weak_instructions, render_active_plans, render_plan,
    render_weak_blocks, RepairPlan as MetricInstruction,
};

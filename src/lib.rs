pub mod app;
pub mod blockers;
pub mod canon_tools_patch;
pub mod complexity;
pub mod constants;
mod engine;
pub mod error_class;
pub mod evaluation;
pub mod evolution;
pub mod drift_analysis;
pub mod grpo;
pub mod graph_metrics;
pub mod inter_complexity;
pub mod invalid_action;
pub mod invariants;
mod issues;
pub mod lessons;
// Repair priority map:
// 1. Runtime liveness: `app` and `llm_runtime` own submit-ack, inbound-message,
//    and turn-lease recovery surfaces that dominate recent blocker evidence.
// 2. Authority collapse: `canonical_writer`, `tlog`, and projection loaders must
//    remain the only trusted state path before projected JSON artifacts are used.
// 3. Complexity burn-down: graph/refactor analyses should rank small deterministic
//    cuts before large role or orchestration rewrites.
pub mod llm_runtime;
pub mod logging;
mod md_convert;
mod objectives;
pub mod orchestrator_seam;
pub mod plan_preflight;
mod prompt_inputs;
pub mod prompts;
pub mod prompt_contract;
mod protocol;
pub mod refactor_analysis;
pub mod rename_semantic;
mod reports;
mod semantic;
pub mod state_space;
mod structured_questions;
pub mod supervisor;
mod tool_schema;
mod tools;
// Canonical writer infrastructure.
// Runtime state in extracted workspaces is expected to replay from
// `agent_state/tlog.ndjson` before trusting projected JSON artifacts.
pub mod canonical_writer;
pub mod events;
pub mod system_state;
pub mod tlog;
pub mod transition_policy;

// Keep the `constants` module private, but expose the few setters used by auxiliary binaries
// (e.g. `canon-mini-supervisor`) to configure the workspace and state-dir.
pub fn set_workspace(path: String) {
    constants::set_workspace(path);
}

pub fn set_agent_state_dir(path: String) {
    constants::set_agent_state_dir(path);
}

pub fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

pub use crate::issues::load_issues_file;
pub use crate::reports::load_violations_report;
pub use crate::semantic::{SemanticIndex, SemanticTriple, SymbolSummary};
pub use crate::tools::execute_action_capability;

#[cfg(test)]
mod invalid_action_tests;
#[cfg(test)]
mod state_space_tests;

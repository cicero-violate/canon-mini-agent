#![allow(dead_code)]

pub mod app;
mod constants;
mod engine;
pub mod lessons;
pub mod logging;
mod md_convert;
mod action_examples;
mod issues;
mod objectives;
mod tool_schema;
pub mod prompts;
mod prompt_inputs;
mod structured_questions;
mod protocol;
mod reports;
mod tools;
pub mod invalid_action;
mod semantic;
pub mod complexity;
pub mod inter_complexity;
pub mod plan_preflight;
pub mod refactor_analysis;
pub mod rename_semantic;
mod rename_example_target;
mod rename_example_caller;
pub mod state_space;

// Keep the `constants` module private, but expose the few setters used by auxiliary binaries
// (e.g. `canon-mini-supervisor`) to configure the workspace and state-dir.
pub fn set_workspace(path: String) {
    constants::set_workspace(path);
}

pub fn set_agent_state_dir(path: String) {
    constants::set_agent_state_dir(path);
}

pub use crate::tools::execute_action_capability;
pub use crate::semantic::{SemanticIndex, SemanticTriple, SymbolSummary};

#[cfg(test)]
mod invalid_action_tests;
#[cfg(test)]
mod state_space_tests;

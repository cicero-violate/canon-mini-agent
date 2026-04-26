use crate::llm_runtime::{
    config::LlmEndpoint,
    tab_management::TabManagerHandle,
    worker::{
        llm_worker_new_tabs, llm_worker_send_request_timeout,
        llm_worker_send_request_with_req_id_timeout,
    },
    ws_server,
    ws_server::WsBridge,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    Notify,
};

use crate::canonical_writer::CanonicalWriter;
use crate::constants::{
    lane_plan_file_for_instance, planner_projection_file_for_instance, set_agent_state_dir,
    set_workspace, workspace, DEFAULT_AGENT_STATE_DIR, DEFAULT_LLM_RETRY_COUNT,
    DEFAULT_LLM_RETRY_DELAY_SECS, DEFAULT_RESPONSE_TIMEOUT_SECS, ENDPOINT_SPECS,
    EXECUTOR_STEP_LIMIT, ISSUES_FILE, MASTER_PLAN_FILE, MAX_SNIPPET, MAX_STEPS, OBJECTIVES_FILE,
    PLANNER_PROJECTION_FILE_PATH, ROLE_TIMEOUT_SECS, SPEC_FILE, VIOLATIONS_FILE,
    WS_PORT_CANDIDATES,
};
use crate::engine::process_action_and_execute;
use crate::events::{ControlEvent, EffectEvent, Event};
use crate::invalid_action::{
    auto_fill_message_fields, build_invalid_action_feedback, corrective_invalid_action_prompt,
    default_message_route, ensure_action_base_schema, expected_message_format,
};
use crate::issues::IssuesFile;
use crate::logging::{
    append_action_log_record, append_orchestration_trace, artifact_write_signature,
    compact_log_record, init_log_paths, log_action_result, log_error_event, log_message_event,
    make_command_id, now_ms, record_effect_for_workspace,
};
use crate::md_convert::ensure_objectives_and_invariants_json;
use crate::orchestrator_seam::plan_has_incomplete_tasks;
use crate::prompt_inputs::{
    build_single_role_prompt, load_planner_inputs, load_single_role_inputs, read_text_or_empty,
    LaneConfig, LessonsArtifact, OrchestratorContext, PlannerInputs, SingleRoleContext,
    SingleRoleInputs,
};
use crate::prompts::{
    action_intent, action_objective_id, action_observation, action_rationale, action_result_prompt,
    action_task_id, executor_cycle_prompt, is_explicit_idle_action, normalize_action,
    parse_actions, planner_cycle_prompt, render_action_result_sections, system_instructions,
    truncate, validate_action, AgentPromptKind,
};
use crate::state_space::{
    check_completion_endpoint, check_completion_tab, decide_bootstrap_phase, decide_resume_phase,
    decide_wake_signals, executor_step_limit_exceeded, should_force_blocker, CargoTestGate,
    CompletionEndpointCheck, CompletionTabCheck, SemanticControlState, WakeSignalInput,
};
use crate::system_state::SystemState;
use crate::tlog::Tlog;
use crate::tool_schema::write_tool_examples;
use crate::tools::write_stage_graph;

// Same-directory shards are regular modules; app keeps a flat local namespace via scoped imports.
mod app_action_io;
mod app_agent_loop;
mod app_bootstrap;
mod app_checkpoint_guardrails;
mod app_inbound_routing;
mod app_planner_executor;
mod app_runtime_completion;
mod app_submit_completion;

use self::app_action_io::*;
use self::app_agent_loop::*;
use self::app_bootstrap::*;
use self::app_checkpoint_guardrails::*;
use self::app_inbound_routing::*;
use self::app_planner_executor::*;
pub use self::app_runtime_completion::run;
use self::app_runtime_completion::*;
use self::app_submit_completion::*;

#[cfg(test)]
mod app_tests;

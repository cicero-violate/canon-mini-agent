use std::sync::{OnceLock, RwLock};

pub const DEFAULT_WORKSPACE: &str = "/workspace/ai_sandbox/canon";
pub const SPEC_FILE: &str = "SPEC.md";
pub const OBJECTIVES_FILE: &str = "agent_state/OBJECTIVES.json";
pub const OBJECTIVES_MD_FILE: &str = "PLANS/OBJECTIVES.md";
pub const MASTER_PLAN_FILE: &str = "agent_state/PLAN.json";
pub const VIOLATIONS_FILE: &str = "agent_state/VIOLATIONS.json";
pub const ISSUES_FILE: &str = "agent_state/ISSUES.json";
pub const INVARIANTS_FILE: &str = "INVARIANTS.json";
pub const INVARIANTS_MD_FILE: &str = "INVARIANT.md";
pub const WS_PORT_CANDIDATES: &[u16] = &[9103, 9104, 9105, 9106, 9107, 9108];
pub const MAX_STEPS: usize = 2000;
pub const EXECUTOR_STEP_LIMIT: usize = 70;
/// Prompt size above which a VIOLATIONS.json entry is written.
/// Prompts this large flood the model with noise context and degrade focus.
pub const PROMPT_OVERFLOW_BYTES: usize = 80_000;
pub const MAX_FULL_READ_LINES: usize = 1000;
pub const MAX_SNIPPET: usize = 20_000;
pub const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_LLM_RETRY_COUNT: u32 = 3;
pub const DEFAULT_LLM_RETRY_DELAY_SECS: u64 = 5;
pub const ROLE_TIMEOUT_SECS: &[(&str, u64)] =
    &[("planner", 60), ("mini_planner", 60), ("executor", 30)];

static WORKSPACE_PATH: OnceLock<RwLock<&'static str>> = OnceLock::new();

// ── Active plan task tracking ─────────────────────────────────────────────────
//
// Stores the id of the plan task currently marked `in_progress`.
// Updated by the plan-action handler whenever a task transitions to/from
// `in_progress`. Read by the logging layer to stamp every action log entry
// with the task that motivated the work.

static ACTIVE_TASK_ID: OnceLock<std::sync::RwLock<String>> = OnceLock::new();

/// Record the plan task id that is now in progress (or clear it with "").
pub fn set_active_task_id(id: &str) {
    let lock = ACTIVE_TASK_ID.get_or_init(|| std::sync::RwLock::new(String::new()));
    if let Ok(mut guard) = lock.write() {
        *guard = id.to_string();
    }
}

/// Return the currently active plan task id, or an empty string if none is set.
pub fn active_task_id() -> String {
    ACTIVE_TASK_ID
        .get()
        .and_then(|l| l.read().ok())
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Set the target workspace path from the --workspace CLI argument.
/// Must be called once before any call to `workspace()`.
pub fn set_workspace(path: String) {
    let leaked = Box::leak(path.into_boxed_str());
    let lock = WORKSPACE_PATH.get_or_init(|| RwLock::new(DEFAULT_WORKSPACE));
    if let Ok(mut guard) = lock.write() {
        *guard = leaked;
    }
}

/// Returns the active target workspace path.
/// Falls back to DEFAULT_WORKSPACE if --workspace was not provided.
pub fn workspace() -> &'static str {
    WORKSPACE_PATH
        .get_or_init(|| RwLock::new(DEFAULT_WORKSPACE))
        .read()
        .map(|guard| *guard)
        .unwrap_or(DEFAULT_WORKSPACE)
}

// ── Agent state directory (canon-mini-agent's own operational state) ──────────

/// Default path for canon-mini-agent's own runtime state (checkpoints, wake signals).
/// Override with --state-dir <path>.
pub const DEFAULT_AGENT_STATE_DIR: &str = "/workspace/ai_sandbox/canon-mini-agent/agent_state";

static AGENT_STATE_DIR_PATH: OnceLock<RwLock<&'static str>> = OnceLock::new();

/// Set the agent state directory from the --state-dir CLI argument.
pub fn set_agent_state_dir(path: String) {
    let leaked = Box::leak(path.into_boxed_str());
    let lock = AGENT_STATE_DIR_PATH.get_or_init(|| RwLock::new(DEFAULT_AGENT_STATE_DIR));
    if let Ok(mut guard) = lock.write() {
        *guard = leaked;
    }
}

/// Returns the active agent state directory path.
/// This is where canon-mini-agent stores its own runtime state (checkpoints, wake signals, inbound messages).
/// Falls back to DEFAULT_AGENT_STATE_DIR if --state-dir was not provided.
pub fn agent_state_dir() -> &'static str {
    AGENT_STATE_DIR_PATH
        .get_or_init(|| RwLock::new(DEFAULT_AGENT_STATE_DIR))
        .read()
        .map(|guard| *guard)
        .unwrap_or(DEFAULT_AGENT_STATE_DIR)
}

/// Returns true when the active workspace is the canon-mini-agent source tree itself.
/// In this mode the executor is allowed to patch SPEC.md and src/ files directly.
pub fn is_self_modification_mode() -> bool {
    let ws = workspace();
    let state_dir = std::path::Path::new(agent_state_dir());
    match state_dir.parent() {
        Some(parent) => ws == parent.to_string_lossy().as_ref(),
        None => false,
    }
}

#[derive(Clone, Copy)]
pub struct EndpointSpec {
    pub id: &'static str,
    pub role: &'static str,
    pub role_markdown: &'static str,
    pub urls: &'static [&'static str],
    pub stateful: bool,
    pub max_tabs: usize,
}

pub const ENDPOINT_SPECS: &[EndpointSpec] = &[
    EndpointSpec {
        id: "mini_planner_chatgpt",
        role: "mini_planner",
        role_markdown: "builtin:planner",
        urls: &[
            "https://chatgpt.com/gg/69e927d762c481a3a2ae6038794f2a3a",
            // "https://chatgpt.com/gg/69e2b1e67f188192a9ca08c2036a06ed",
            // "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/",
            // "https://chatgpt.com/gg/69d1275c4ed88191988d28f341f48d42",
            // "https://chatgpt.com/gg/69ca778f7ea0819c8437275ff608eb35",
            // "https://chatgpt.com/gg/69c265cd2274819690fc291ef716524e",
        ],
        stateful: true,
        max_tabs: 1,
    },
    EndpointSpec {
        id: "executor_pool",
        role: "executor",
        role_markdown: "builtin:planner",
        urls: &[
            // "https://chatgpt.com/gg/69e2b1e67f188192a9ca08c2036a06ed",
            "https://chatgpt.com/gg/69e927d762c481a3a2ae6038794f2a3a",
            // "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/",
            // "https://chatgpt.com/gg/69d126d34dc4819d8de9cba1b209d14c",
            // "https://chatgpt.com/gg/69ab7b06a5a88196bf33966df6feee02",
            // "https://chatgpt.com/gg/69ca500acd888199a32b90339c82fa31",
        ],
        stateful: true,
        max_tabs: 2,
    },
];

pub static PLANNER_PROJECTION_FILE_PATH: OnceLock<String> = OnceLock::new();

pub fn planner_projection_file_for_instance(instance_id: &str) -> String {
    format!("agent_state/{instance_id}/planner-{instance_id}.json")
}

pub fn lane_plan_file_for_instance(instance_id: &str, endpoint_id: &str) -> String {
    format!("agent_state/{instance_id}/executor-{endpoint_id}.json")
}

pub fn planner_projection_file() -> &'static str {
    PLANNER_PROJECTION_FILE_PATH
        .get()
        .map(String::as_str)
        .unwrap_or("agent_state/default/planner-default.json")
}

// Compatibility shims for legacy diagnostics naming.
pub fn diagnostics_file_for_instance(instance_id: &str) -> String {
    planner_projection_file_for_instance(instance_id)
}

pub fn diagnostics_file() -> &'static str {
    planner_projection_file()
}

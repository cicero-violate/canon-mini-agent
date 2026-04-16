use std::sync::{OnceLock, RwLock};

pub const DEFAULT_WORKSPACE: &str = "/workspace/ai_sandbox/canon";
pub const SPEC_FILE: &str = "SPEC.md";
pub const OBJECTIVES_FILE: &str = "PLANS/OBJECTIVES.json";
pub const OBJECTIVES_MD_FILE: &str = "PLANS/OBJECTIVES.md";
pub const MASTER_PLAN_FILE: &str = "PLAN.json";
pub const VIOLATIONS_FILE: &str = "VIOLATIONS.json";
pub const ISSUES_FILE: &str = "ISSUES.json";
pub const INVARIANTS_FILE: &str = "INVARIANTS.json";
pub const INVARIANTS_MD_FILE: &str = "INVARIANT.md";
pub const CANONICAL_LAW_FILE: &str = "CANONICAL_LAW.md";
pub const WS_PORT_CANDIDATES: &[u16] = &[9103, 9104, 9105, 9106, 9107, 9108];
pub const MAX_STEPS: usize = 2000;
pub const EXECUTOR_STEP_LIMIT: usize = 20;
/// Prompt size above which a VIOLATIONS.json entry is written.
/// Prompts this large flood the model with noise context and degrade focus.
pub const PROMPT_OVERFLOW_BYTES: usize = 80_000;
pub const MAX_FULL_READ_LINES: usize = 1000;
pub const MAX_SNIPPET: usize = 20_000;
pub const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 150;
pub const DEFAULT_LLM_RETRY_COUNT: u32 = 3;
pub const DEFAULT_LLM_RETRY_DELAY_SECS: u64 = 5;
pub const ROLE_TIMEOUT_SECS: &[(&str, u64)] = &[
    ("planner", 900),
    ("mini_planner", 900),
    ("verifier", 180),
    ("diagnostics", 180),
    ("executor", 30),
    ("solo", 900),
];

static WORKSPACE_PATH: OnceLock<RwLock<String>> = OnceLock::new();

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
    let lock = WORKSPACE_PATH.get_or_init(|| RwLock::new(DEFAULT_WORKSPACE.to_string()));
    if let Ok(mut guard) = lock.write() {
        *guard = path;
    }
}

/// Returns the active target workspace path.
/// Falls back to DEFAULT_WORKSPACE if --workspace was not provided.
pub fn workspace() -> &'static str {
    WORKSPACE_PATH
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|guard| Box::leak(guard.clone().into_boxed_str()) as &'static str)
        .unwrap_or(DEFAULT_WORKSPACE)
}

// ── Agent state directory (canon-mini-agent's own operational state) ──────────

/// Default path for canon-mini-agent's own runtime state (checkpoints, wakeup flags).
/// Override with --state-dir <path>.
pub const DEFAULT_AGENT_STATE_DIR: &str = "/workspace/ai_sandbox/canon-mini-agent/agent_state";

static AGENT_STATE_DIR_PATH: OnceLock<RwLock<String>> = OnceLock::new();

/// Set the agent state directory from the --state-dir CLI argument.
pub fn set_agent_state_dir(path: String) {
    let lock = AGENT_STATE_DIR_PATH.get_or_init(|| RwLock::new(DEFAULT_AGENT_STATE_DIR.to_string()));
    if let Ok(mut guard) = lock.write() {
        *guard = path;
    }
}

/// Returns the active agent state directory path.
/// This is where canon-mini-agent stores its own runtime state (checkpoints, wakeup flags, inbound messages).
/// Falls back to DEFAULT_AGENT_STATE_DIR if --state-dir was not provided.
pub fn agent_state_dir() -> &'static str {
    AGENT_STATE_DIR_PATH
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|guard| Box::leak(guard.clone().into_boxed_str()) as &'static str)
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
            "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/",
            // "https://chatgpt.com/gg/69d1275c4ed88191988d28f341f48d42",
            // "https://chatgpt.com/gg/69ca778f7ea0819c8437275ff608eb35",
            // "https://chatgpt.com/gg/69c265cd2274819690fc291ef716524e",
        ],
        stateful: true,
        max_tabs: 1,
    },
    EndpointSpec {
        id: "solo_chatgpt",
        role: "solo",
        role_markdown: "builtin:planner",
        // urls: &["https://chatgpt.com/"],
        urls: &[
            "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/",
            // "https://chatgpt.com/gg/69d7931e25b881929ff153362c36df93",
        ],
        stateful: true,
        max_tabs: 1,
    },
    EndpointSpec {
        id: "verifier_chatgpt",
        role: "verifier",
        role_markdown: "builtin:planner",
        urls: &[
            "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/gg/69d12735ff8081a3ad8aab20b4c4e10a",
            // "https://chatgpt.com/gg/69ca70d1a4208199a3d1c4c77e87c147",
            // "https://chatgpt.com/gg/69c265cd2274819690fc291ef716524e",
        ],
        stateful: true,
        max_tabs: 1,
    },
    EndpointSpec {
        id: "diagnostics_chatgpt",
        role: "diagnostics",
        role_markdown: "builtin:planner",
        urls: &[
            "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/gg/69d1266e0a288198bb7b2f150a669dd7",
            // "https://chatgpt.com/gg/69caa6e708108198b02c2d2eaea30118",
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
            "https://chatgpt.com/gg/69d9549305a881a2a3faaff764e2d106",
            // "https://chatgpt.com/",
            // "https://chatgpt.com/gg/69d126d34dc4819d8de9cba1b209d14c",
            // "https://chatgpt.com/gg/69ab7b06a5a88196bf33966df6feee02",
            // "https://chatgpt.com/gg/69ca500acd888199a32b90339c82fa31",
        ],
        stateful: true,
        max_tabs: 2,
    },
];

pub static DIAGNOSTICS_FILE_PATH: OnceLock<String> = OnceLock::new();

pub fn diagnostics_file() -> &'static str {
    DIAGNOSTICS_FILE_PATH
        .get()
        .map(String::as_str)
        .unwrap_or("PLANS/default/diagnostics-default.json")
}

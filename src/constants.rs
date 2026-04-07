use std::sync::OnceLock;

pub const WORKSPACE: &str = "/workspace/ai_sandbox/canon";
pub const SPEC_FILE: &str = "SPEC.md";
pub const OBJECTIVES_FILE: &str = "PLANS/OBJECTIVES.json";
pub const OBJECTIVES_MD_FILE: &str = "PLANS/OBJECTIVES.md";
pub const MASTER_PLAN_FILE: &str = "PLAN.json";
pub const VIOLATIONS_FILE: &str = "VIOLATIONS.json";
pub const INVARIANTS_FILE: &str = "INVARIANTS.json";
pub const INVARIANTS_MD_FILE: &str = "INVARIANT.md";
pub const WS_PORT_CANDIDATES: &[u16] = &[9103, 9104, 9105, 9106, 9107, 9108];
pub const MAX_STEPS: usize = 2000;
pub const EXECUTOR_STEP_LIMIT: usize = 5;
pub const MAX_FULL_READ_LINES: usize = 300;
pub const MAX_SNIPPET: usize = 20_000;
pub const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 150;
pub const ROLE_TIMEOUT_SECS: &[(&str, u64)] = &[
    ("planner", 600),
    ("mini_planner", 600),
    ("verifier", 120),
    ("diagnostics", 120),
    ("executor", 30),
];

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
            "https://chatgpt.com/",
            // "https://chatgpt.com/gg/69d1275c4ed88191988d28f341f48d42",
            // "https://chatgpt.com/gg/69ca778f7ea0819c8437275ff608eb35",
            // "https://chatgpt.com/gg/69c265cd2274819690fc291ef716524e",
        ],
        stateful: true,
        max_tabs: 1,
    },
    EndpointSpec {
        id: "verifier_chatgpt",
        role: "verifier",
        role_markdown: "builtin:planner",
        urls: &[
            "https://chatgpt.com/gg/69d12735ff8081a3ad8aab20b4c4e10a",
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
            // "https://chatgpt.com/gg/69d1266e0a288198bb7b2f150a669dd7",
            "https://chatgpt.com/gg/69caa6e708108198b02c2d2eaea30118",
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
            "https://chatgpt.com/gg/69d126d34dc4819d8de9cba1b209d14c",
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
        .unwrap_or("DIAGNOSTICS.json")
}

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::constants::{
    diagnostics_file, workspace, CANONICAL_LAW_FILE, EXECUTOR_STEP_LIMIT, INVARIANTS_FILE,
    MASTER_PLAN_FILE, MAX_SNIPPET, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::protocol::{MessagePayload, MessageStatus, MessageType, ProtocolMessage, Role};
use crate::tool_schema::{
    cargo_test_action_example, plan_set_task_status_action_example, plan_sorted_view_action_example,
    validate_tool_action,
};

pub(crate) fn truncate(s: &str, max: usize) -> &str {
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    &s[..end]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentPromptKind {
    Executor,
    Verifier,
    Planner,
    Diagnostics,
    Solo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolPromptKind {
    ListDir,
    ReadFile,
    SymbolsIndex,
    SymbolsRenameCandidates,
    SymbolsPrepareRename,
    RenameSymbol,
    Objectives,
    ApplyPatch,
    RunCommand,
    Python,
    CargoTest,
    Plan,
    StageGraph,
    SemanticMap,
    SymbolWindow,
    SymbolRefs,
    SymbolPath,
    SymbolNeighborhood,
    Message,
}

fn available_actions(_kind: AgentPromptKind) -> Vec<String> {
    crate::tool_schema::predicted_action_name_list()
}

fn tool_order(kind: AgentPromptKind) -> &'static [ToolPromptKind] {
    match kind {
        AgentPromptKind::Diagnostics => &[
            ToolPromptKind::ListDir,
            ToolPromptKind::ReadFile,
            ToolPromptKind::StageGraph,
            ToolPromptKind::SemanticMap,
            ToolPromptKind::SymbolWindow,
            ToolPromptKind::SymbolRefs,
            ToolPromptKind::SymbolPath,
            ToolPromptKind::SymbolNeighborhood,
            ToolPromptKind::SymbolsIndex,
            ToolPromptKind::SymbolsRenameCandidates,
            ToolPromptKind::SymbolsPrepareRename,
            ToolPromptKind::RenameSymbol,
            ToolPromptKind::Objectives,
            ToolPromptKind::Python,
            ToolPromptKind::RunCommand,
            ToolPromptKind::ApplyPatch,
            ToolPromptKind::CargoTest,
            ToolPromptKind::Plan,
            ToolPromptKind::Message,
        ],
        AgentPromptKind::Verifier => &[
            ToolPromptKind::ListDir,
            ToolPromptKind::ReadFile,
            ToolPromptKind::StageGraph,
            ToolPromptKind::SemanticMap,
            ToolPromptKind::SymbolWindow,
            ToolPromptKind::SymbolRefs,
            ToolPromptKind::SymbolPath,
            ToolPromptKind::SymbolNeighborhood,
            ToolPromptKind::SymbolsIndex,
            ToolPromptKind::SymbolsRenameCandidates,
            ToolPromptKind::SymbolsPrepareRename,
            ToolPromptKind::RenameSymbol,
            ToolPromptKind::Objectives,
            ToolPromptKind::ApplyPatch,
            ToolPromptKind::RunCommand,
            ToolPromptKind::Python,
            ToolPromptKind::CargoTest,
            ToolPromptKind::Plan,
            ToolPromptKind::Message,
        ],
        AgentPromptKind::Executor | AgentPromptKind::Planner | AgentPromptKind::Solo => &[
            ToolPromptKind::ListDir,
            ToolPromptKind::ReadFile,
            ToolPromptKind::StageGraph,
            ToolPromptKind::SemanticMap,
            ToolPromptKind::SymbolWindow,
            ToolPromptKind::SymbolRefs,
            ToolPromptKind::SymbolPath,
            ToolPromptKind::SymbolNeighborhood,
            ToolPromptKind::SymbolsIndex,
            ToolPromptKind::SymbolsRenameCandidates,
            ToolPromptKind::SymbolsPrepareRename,
            ToolPromptKind::RenameSymbol,
            ToolPromptKind::Objectives,
            ToolPromptKind::ApplyPatch,
            ToolPromptKind::RunCommand,
            ToolPromptKind::Python,
            ToolPromptKind::CargoTest,
            ToolPromptKind::Plan,
            ToolPromptKind::Message,
        ],
    }
}

fn tool_title(kind: AgentPromptKind, tool: ToolPromptKind) -> &'static str {
    match (kind, tool) {
        (_, ToolPromptKind::ListDir) => {
            "list_dir — inspect directory contents (use semantic_map instead for Rust source structure)"
        }
        (_, ToolPromptKind::ReadFile) => {
            "read_file — read a file line-numbered (fallback: use symbol_* for Rust source; reserve read_file for non-Rust files and pre-patch reads)"
        }
        (_, ToolPromptKind::SymbolsIndex) => {
            "symbols_index — build deterministic symbols.json from Rust sources"
        }
        (_, ToolPromptKind::SymbolsRenameCandidates) => {
            "symbols_rename_candidates — derive deterministic rename candidates from symbols.json"
        }
        (_, ToolPromptKind::SymbolsPrepareRename) => {
            "symbols_prepare_rename — select candidate and emit ready rename_symbol payload"
        }
        (_, ToolPromptKind::RenameSymbol) => {
            "rename_symbol — rename Rust symbols using rustc graph spans; cargo check runs automatically and git-rolls back on failure"
        }
        (_, ToolPromptKind::Objectives) => {
            "objectives — read/update objectives in PLANS/OBJECTIVES.json"
        }
        (AgentPromptKind::Verifier, ToolPromptKind::ApplyPatch) => {
            "apply_patch — NOT for VIOLATIONS.json; use the `violation` action instead"
        }
        (AgentPromptKind::Planner, ToolPromptKind::ApplyPatch) => {
            "apply_patch — update lane plans under `PLANS/`"
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::ApplyPatch) => {
            "apply_patch — write the diagnostics report"
        }
        (AgentPromptKind::Solo, ToolPromptKind::ApplyPatch) => {
            "apply_patch — create or update any in-workspace files"
        }
        (_, ToolPromptKind::ApplyPatch) => "apply_patch — create or update files",
        (_, ToolPromptKind::RunCommand) => {
            "run_command — run shell commands for discovery or verification"
        }
        (_, ToolPromptKind::Python) => "python — run Python analysis inside the workspace",
        (_, ToolPromptKind::CargoTest) => "cargo_test — run a targeted cargo test (harness-style)",
        (_, ToolPromptKind::Plan) => "plan — create/update/delete tasks and DAG edges in PLAN.json",
        (_, ToolPromptKind::StageGraph) => {
            "stage_graph — emit a synthetic OODA-style stage graph artifact"
        }
        (_, ToolPromptKind::SemanticMap) => {
            "semantic_map — [PREFER over list_dir] rustc-backed crate outline: symbol kind, name, signature by file"
        }
        (_, ToolPromptKind::SymbolWindow) => {
            "symbol_window — [PREFER over read_file] extract full definition body of a symbol (byte-precise, no line-number noise)"
        }
        (_, ToolPromptKind::SymbolRefs) => {
            "symbol_refs — [PREFER over grep] all reference sites (file:line:col) for a symbol"
        }
        (_, ToolPromptKind::SymbolPath) => {
            "symbol_path — [PREFER over manual tracing] BFS shortest call-graph path between two symbols"
        }
        (_, ToolPromptKind::SymbolNeighborhood) => {
            "symbol_neighborhood — [PREFER over manual tracing] immediate callers and callees of a symbol"
        }
            (_, ToolPromptKind::Message) => "message — send inter-agent protocol message",
        }
    }


const READ_FILE_FOOTER: &str = "   With \"line\":N the output starts at line N and shows up to 1000 lines.\n   ⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.\n   ⚠ read_file output is prefixed with line numbers (\"42: code here\"). Strip the \"N: \" prefix when\n     writing patch lines — patch lines must contain ONLY the raw source text, never \"42: code here\".\n     WRONG:  -42: fn old() {}   RIGHT:  -fn old() {}";

const READ_FILE_EXECUTOR_FOOTER: &str = "   With \"line\":N the output starts at line N and shows up to 1000 lines.\n   ⚠ Always read a file before patching it. Never patch from memory.\n   ⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.\n   ⚠ read_file output is prefixed with line numbers (\"42: code here\"). Strip the \"N: \" prefix when\n     writing patch lines — patch lines must contain ONLY the raw source text, never \"42: code here\".\n     WRONG:  -42: fn old() {}   RIGHT:  -fn old() {}";

const RUN_COMMAND_FOOTER: &str =
    "   ⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.";
const PYTHON_FOOTER: &str = "   ⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.";

fn plan_sorted_view_example() -> String {
    format!(
        "   {}",
        plan_sorted_view_action_example("View the current plan in DAG order (read-only).")
    )
}

fn read_plan_with_sorted_view_example(rationale: &str) -> String {
    format!(
        "   {{\"action\":\"read_file\",\"path\":\"PLAN.json\",\"rationale\":\"{rationale}\"}}\n{}",
        plan_sorted_view_example()
    )
}

    fn message_tool_prompt_examples() -> &'static str {
        truncate(
            "   {\"action\":\"message\",\"from\":\"executor\",\"to\":\"verifier\",\"type\":\"handoff\",\"status\":\"complete\",\"observation\":\"Summarize what happened.\",\"rationale\":\"Execution work is complete and the verifier now has enough evidence to judge it.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect the changed source files during verification.\"},{\"action\":\"cargo_test\",\"intent\":\"Run verification tests if the code path needs execution proof.\"}],\"payload\":{\"summary\":\"brief evidence summary\",\"artifacts\":[\"path/to/file.rs\"]}}\n   {\"action\":\"message\",\"from\":\"executor\",\"to\":\"planner\",\"type\":\"blocker\",\"status\":\"blocked\",\"observation\":\"Describe the blocker.\",\"rationale\":\"Explain why progress is impossible.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect the blocked source or artifact in more detail.\"},{\"action\":\"run_command\",\"intent\":\"Gather concrete failure evidence for the blocker.\"}],\"payload\":{\"summary\":\"Short blocker summary\",\"blocker\":\"Root cause\",\"evidence\":\"Concrete error text\",\"required_action\":\"What must be done to unblock\",\"severity\":\"error\"}}\n   Allowed roles: executor|planner|verifier|diagnostics|solo. Allowed types: handoff|result|verification|failure|blocker|plan|diagnostics. Allowed status: complete|in_progress|failed|verified|ready|blocked.\n   ⚠ message with status=complete is REJECTED if build or tests fail — fix all errors first.",
            2000,
        )
    }

fn tool_prompt(kind: AgentPromptKind, tool: ToolPromptKind) -> String {
    let ws = crate::constants::workspace();
    match (kind, tool) {
        (_, ToolPromptKind::ListDir) => list_dir_tool_prompt(kind),

        (_, ToolPromptKind::ReadFile) => read_file_tool_prompt(kind),
        (_, ToolPromptKind::StageGraph) => {
            "   {\"action\":\"stage_graph\",\"rationale\":\"Generate the current synthetic stage graph for branching and introspection.\",\"predicted_next_actions\":[{\"action\":\"read_file\",\"intent\":\"Inspect the generated stage graph JSON.\"},{\"action\":\"plan\",\"intent\":\"Promote stage insights into executable PLAN tasks.\"}]}".to_string()
        }
        (_, ToolPromptKind::SymbolsIndex) => {
            "   {\"action\":\"symbols_index\",\"path\":\"src\",\"out\":\"state/symbols.json\",\"rationale\":\"Build a deterministic unique symbols catalog before selecting rename/refactor targets.\"}\n   Notes: `path` defaults to workspace root; `out` defaults to `state/symbols.json`.".to_string()
        }
        (_, ToolPromptKind::SymbolsRenameCandidates) => {
            "   {\"action\":\"symbols_rename_candidates\",\"symbols_path\":\"state/symbols.json\",\"out\":\"state/rename_candidates.json\",\"rationale\":\"Derive deterministic rename candidates from symbols inventory before mutating code.\"}\n   Notes: `symbols_path` defaults to `state/symbols.json`; `out` defaults to `state/rename_candidates.json`.".to_string()
        }
        (_, ToolPromptKind::SymbolsPrepareRename) => {
            "   {\"action\":\"symbols_prepare_rename\",\"candidates_path\":\"state/rename_candidates.json\",\"index\":0,\"out\":\"state/next_rename_action.json\",\"rationale\":\"Select one deterministic candidate and prepare a ready rename_symbol action payload.\"}\n   Notes: `candidates_path` defaults to `state/rename_candidates.json`; `index` defaults to 0; `out` defaults to `state/next_rename_action.json`.".to_string()
        }
        (_, ToolPromptKind::RenameSymbol) => rename_symbol_tool_prompt(kind),
        (_, ToolPromptKind::Objectives) => {
            "   {\"action\":\"objectives\",\"op\":\"read\",\"rationale\":\"Load only non-completed objectives for planning/verification.\"}\n   {\"action\":\"objectives\",\"op\":\"read\",\"include_done\":true,\"rationale\":\"Load all objectives, including completed.\"}\n   {\"action\":\"objectives\",\"op\":\"create_objective\",\"objective\":{\"id\":\"obj_new\",\"title\":\"New objective\",\"status\":\"active\",\"scope\":\"...\",\"authority_files\":[\"src/foo.rs\"],\"category\":\"quality\",\"level\":\"low\",\"description\":\"...\",\"requirement\":[],\"verification\":[],\"success_criteria\":[]},\"rationale\":\"Record a new objective.\"}\n   {\"action\":\"objectives\",\"op\":\"set_status\",\"objective_id\":\"obj_new\",\"status\":\"done\",\"rationale\":\"Mark objective complete.\"}\n   {\"action\":\"objectives\",\"op\":\"update_objective\",\"objective_id\":\"obj_new\",\"updates\":{\"scope\":\"updated scope\"},\"rationale\":\"Update objective fields.\"}\n   {\"action\":\"objectives\",\"op\":\"delete_objective\",\"objective_id\":\"obj_new\",\"rationale\":\"Remove obsolete objective.\"}\n   {\"action\":\"objectives\",\"op\":\"replace_objectives\",\"objectives\":[],\"rationale\":\"Replace objectives list.\"}\n   {\"action\":\"objectives\",\"op\":\"sorted_view\",\"rationale\":\"View objectives sorted by status.\"}".to_string()
        }

        (_, ToolPromptKind::ApplyPatch) => apply_patch_tool_prompt(kind),

        (_, ToolPromptKind::RunCommand) => run_command_tool_prompt(kind, ws),

        (_, ToolPromptKind::Python) => python_tool_prompt(kind, ws),
        (AgentPromptKind::Executor, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Solo, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Planner, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Verifier, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Diagnostics, ToolPromptKind::CargoTest) => {
            format!("   {}", cargo_test_action_example())
        }
        (_, ToolPromptKind::Plan) => plan_tool_prompt(kind),

        (_, ToolPromptKind::SemanticMap) => {
            "   {\"action\":\"semantic_map\",\"crate\":\"canon_mini_agent\",\"rationale\":\"Get a rustc-backed symbol outline to understand the codebase structure before reading individual files.\"}\n   {\"action\":\"semantic_map\",\"crate\":\"canon_mini_agent\",\"filter\":\"tools\",\"rationale\":\"Restrict the outline to the tools module to see all symbols in that area.\"}\n   Notes: `crate` is the crate name (underscores). Symbol paths use module-relative format (e.g. `tools::my_fn`); crate-qualified prefixes like `canon_mini_agent::tools` or `crate::tools` are accepted and stripped. Optional `filter` restricts to a symbol-path prefix.".to_string()
        }
        (_, ToolPromptKind::SymbolWindow) => {
            "   {\"action\":\"symbol_window\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"rationale\":\"Extract the full definition of a specific function before editing it.\"}\n   Notes: `symbol` uses module-relative path (e.g. `tools::my_fn`); crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped; accepts unambiguous short name as suffix.".to_string()
        }
        (_, ToolPromptKind::SymbolRefs) => {
            "   {\"action\":\"symbol_refs\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"rationale\":\"Find all call sites before renaming or changing the signature.\"}\n   Notes: `symbol` is module-relative; crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped. Returns file:line:col for every identifier reference span recorded during compilation.".to_string()
        }
        (_, ToolPromptKind::SymbolPath) => {
            "   {\"action\":\"symbol_path\",\"crate\":\"canon_mini_agent\",\"from\":\"app::run_agent\",\"to\":\"tools::handle_apply_patch_action\",\"rationale\":\"Find the call chain between two symbols to understand how they are connected.\"}\n   Notes: `from`/`to` are module-relative; crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped. BFS over call edges; returns the shortest path with file:line annotations.".to_string()
        }
        (_, ToolPromptKind::SymbolNeighborhood) => {
            "   {\"action\":\"symbol_neighborhood\",\"crate\":\"canon_mini_agent\",\"symbol\":\"tools::execute_logged_action\",\"rationale\":\"See all callers and callees of a symbol to understand its role before changing it.\"}\n   Notes: `symbol` is module-relative; crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped. Returns all immediate callers and callees from the static call graph.".to_string()
        }
        (_, ToolPromptKind::Message) => {
            message_tool_prompt_examples().to_string()
        }
    }
}

fn rename_symbol_tool_prompt(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor | AgentPromptKind::Solo => {
            "   {\"action\":\"rename_symbol\",\"old_symbol\":\"tools::handle_plan_action\",\"new_symbol\":\"tools::handle_master_plan_action\",\"question\":\"Is this the exact symbol to rename across the crate?\",\"rationale\":\"Perform a span-backed rename so all references update consistently.\",\"predicted_next_actions\":[{\"action\":\"cargo_test\",\"intent\":\"Run focused tests after rename.\"}]}\n   Notes: symbol paths are module-relative (e.g. `tools::my_fn`); crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped; uses `state/rustc/<crate>/graph.json` spans. cargo check runs automatically — on failure the rename is rolled back via git and errors are written to state/rename_errors.txt.".to_string()
        }
        AgentPromptKind::Planner | AgentPromptKind::Verifier | AgentPromptKind::Diagnostics => {
            "   {\"action\":\"rename_symbol\",\"old_symbol\":\"tools::handle_plan_action\",\"new_symbol\":\"tools::handle_master_plan_action\",\"question\":\"Is this the exact symbol to rename across the crate?\",\"rationale\":\"Apply a span-backed rename only when source evidence confirms it is required.\",\"predicted_next_actions\":[{\"action\":\"cargo_test\",\"intent\":\"Run focused tests after rename.\"}]}\n   Notes: symbol paths are module-relative (e.g. `tools::my_fn`); crate-qualified prefixes like `canon_mini_agent::...` or `crate::...` are accepted and stripped; uses `state/rustc/<crate>/graph.json` spans. cargo check runs automatically — on failure the rename is rolled back via git and errors are written to state/rename_errors.txt.".to_string()
        }
    }
}

fn apply_patch_tool_prompt(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor | AgentPromptKind::Solo => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: path/to/new.rs\\n+line one\\n+line two\\n*** End Patch\",\"rationale\":\"Apply the concrete code change after reading the target context.\"}\n\n   To UPDATE an existing file, each @@ hunk needs 3 unchanged context lines around the change:\n   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n fn before_before() {}\\n fn before() {}\\n fn target() {\\n-    old_body();\\n+    new_body();\\n }\\n fn after() {}\\n*** End Patch\",\"rationale\":\"Update the file using exact surrounding context from the read.\"}\n\n   To REPLACE most or all of a file use Delete + Add, never a giant @@ block:\n   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Delete File: PLANS/executor-b.json\\n*** Add File: PLANS/executor-b.json\\n+# new content\\n+line two\\n*** End Patch\",\"rationale\":\"Full-file replacement is safer than a giant hunk with many - lines.\"}\n\n   WRONG — removing many lines with @@ causes anchor-miss failures:\n   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: PLANS/executor-b.json\\n@@\\n-line one\\n-line two\\n-line three\\n+replacement\\n*** End Patch\",\"rationale\":\"Bad: too many - lines from memory, anchor will miss if file differs by even one char.\"}\n\n   Rules:\n   - Every @@ hunk must have AT LEAST 3 unchanged context lines (space-prefixed) around the edit.\n   - Never use @@ with only 1 context line — the patcher will fail to locate the anchor.\n   - ALL - lines must be copied CHARACTER-FOR-CHARACTER from read_file output (minus the \\\"N: \\\" prefix). Never write - lines from memory.\n   - If replacing more than ~10 lines, use *** Delete File + *** Add File instead of a large @@ hunk.\n   - *** Add File for new files, *** Update File for existing files.\n   - NEVER use absolute paths inside the patch string.".to_string()
        }
        AgentPromptKind::Planner => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: PLANS/default/executor-1.json\\n@@\\n line_before_before\\n line_before\\n-  \\\"status\\\": \\\"blocked\\\"\\n+  \\\"status\\\": \\\"ready\\\"\\n line_after\\n line_after_after\\n*** End Patch\",\"rationale\":\"Update a lane plan entry after updating PLAN.json via the plan tool.\"}".to_string()
        }
        AgentPromptKind::Verifier => format!(
            "   {{\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: VIOLATIONS.json\\n+{{\\n+  \\\"status\\\": \\\"failed\\\",\\n+  \\\"summary\\\": \\\"Short summary\\\",\\n+  \\\"violations\\\": [\\n+    {{\\n+      \\\"id\\\": \\\"V1\\\",\\n+      \\\"title\\\": \\\"Control flow gated by executor-local state\\\",\\n+      \\\"severity\\\": \\\"critical\\\",\\n+      \\\"evidence\\\": [\\\"executor.rs:56-61 dispatch_in_progress gate\\\"],\\n+      \\\"issue\\\": \\\"Route dispatch suppressed before semantic evaluation\\\",\\n+      \\\"impact\\\": \\\"RouteTick does not guarantee dispatch\\\",\\n+      \\\"required_fix\\\": [\\\"Remove dispatch_in_progress gating\\\"],\\n+      \\\"files\\\": [\\\"canon-utils/canon-route/src/executor.rs\\\"]\\n+    }}\\n+  ]\\n+}}\\n*** End Patch\",\"rationale\":\"Record spec violations discovered during verification.\"}}\n\n   {}",
            plan_set_task_status_action_example(
                "T4",
                "done",
                "Mark the verified task as done in PLAN.json."
            )
        ),
        AgentPromptKind::Diagnostics => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: PLANS/default/diagnostics-default.json\\n+{\\n+  \\\"status\\\": \\\"critical_failure\\\",\\n+  \\\"inputs_scanned\\\": [\\\"<workspace-local log/state paths discovered during diagnostics>\\\", \\\"VIOLATIONS.json\\\"],\\n+  \\\"ranked_failures\\\": [\\n+    {\\n+      \\\"id\\\": \\\"D1\\\",\\n+      \\\"impact\\\": \\\"critical\\\",\\n+      \\\"signal\\\": \\\"Primary runtime or agent observability artifacts are missing expected progress signals\\\",\\n+      \\\"evidence\\\": [\\\"<concrete evidence from files that exist in the active workspace AND VERIFIED against current source via read_file>\\\"],\\n+      \\\"root_cause\\\": \\\"<workspace-specific root cause derived ONLY from verified current source and observed state>\\\",\\n+      \\\"repair_targets\\\": [\\\"<workspace-specific source locations>\\\"]\\n+    }\\n+  ],\\n+  \\\"planner_handoff\\\": [\\\"Diagnostics MUST NOT emit failures without direct source verification; stale signals must be suppressed.\\\"]\\n+}\\n*** End Patch\",\"rationale\":\"Write diagnostics report only after validating signals against current source; suppress stale or unverified diagnostics.\"}".to_string()
        }
    }
}

fn run_command_tool_prompt(kind: AgentPromptKind, ws: &str) -> String {
    match kind {
        AgentPromptKind::Executor | AgentPromptKind::Solo => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"cargo check -p canon-mini-agent\",\"cwd\":\"{ws}\",\"rationale\":\"Validate the target crate after a change.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo' src\",\"cwd\":\"{ws}\",\"rationale\":\"Search the codebase for the relevant symbol before editing.\"}}\n{RUN_COMMAND_FOOTER}")
        }
        AgentPromptKind::Planner => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo' src\",\"cwd\":\"{ws}\",\"rationale\":\"Search for implementation details needed to expand the plan accurately.\"}}\n{RUN_COMMAND_FOOTER}")
        }
        AgentPromptKind::Verifier => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"cargo check -p canon-mini-agent\",\"cwd\":\"{ws}\",\"rationale\":\"Validate the crate implicated by the completed task.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"cargo test -q --workspace\",\"cwd\":\"{ws}\",\"rationale\":\"Verify the claimed completion does not break workspace tests.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo' src\",\"cwd\":\"{ws}\",\"rationale\":\"Find the implementation or call sites mentioned by the completed task.\"}}\n{RUN_COMMAND_FOOTER}")
        }
        AgentPromptKind::Diagnostics => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"rg -n \\\"invariant|panic|TODO|unreachable!|assert!\\\" src state\",\"cwd\":\"{ws}\",\"rationale\":\"Search the active workspace code and state directories for likely failure markers.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"cargo check --workspace\",\"cwd\":\"{ws}\",\"rationale\":\"Detect compiler-visible inconsistencies that belong in diagnostics.\"}}\n{RUN_COMMAND_FOOTER}")
        }
    }
}

fn python_tool_prompt(kind: AgentPromptKind, ws: &str) -> String {
    match kind {
        AgentPromptKind::Executor | AgentPromptKind::Solo => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(len(list(Path('src').glob('**/*.rs'))))\",\"cwd\":\"{ws}\",\"rationale\":\"Use Python for structured workspace analysis.\"}}\n{PYTHON_FOOTER}"
            )
        }
        AgentPromptKind::Planner => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(sum(1 for _ in Path('src').glob('**/*.rs')))\",\"cwd\":\"{ws}\",\"rationale\":\"Use Python to gather structured planning context from the workspace.\"}}\n{PYTHON_FOOTER}"
            )
        }
        AgentPromptKind::Verifier => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(Path('SPEC.md').exists())\",\"cwd\":\"{ws}\",\"rationale\":\"Use Python when structured verification logic is easier than shell commands.\"}}\n{PYTHON_FOOTER}"
            )
        }
        AgentPromptKind::Diagnostics => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nfor root in [Path('state'), Path('log'), Path('logs'), Path('src')]:\\n    if root.exists():\\n        print(root)\\n        for path in sorted(root.rglob('*')):\\n            if path.is_file():\\n                print(path)\",\"cwd\":\"{ws}\",\"rationale\":\"Analyze workspace-local state, log, and source artifacts to find failure signals and inconsistencies.\"}}\n{PYTHON_FOOTER}"
            )
        }
    }
}

fn plan_tool_prompt(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor => {
            read_plan_with_sorted_view_example("Read the master plan; executors should not edit it.")
        }
        AgentPromptKind::Solo => {
            format!(
                "   {}\n{}",
                plan_set_task_status_action_example(
                    "T1",
                    "in_progress",
                    "Update one PLAN task while running solo."
                ),
                plan_sorted_view_example()
            )
        }
        AgentPromptKind::Planner => {
            format!(
                "   {{\"action\":\"plan\",\"op\":\"create_task\",\"task\":{{\"id\":\"T4\",\"title\":\"Add plan DAG\",\"status\":\"todo\",\"priority\":3}},\"rationale\":\"Add a new task to PLAN.json without manual patching.\"}}\n{}",
                plan_sorted_view_example()
            )
        }
        AgentPromptKind::Verifier => {
            read_plan_with_sorted_view_example(
                "Read the current plan before judging whether claimed work matches recorded state.",
            )
        }
        AgentPromptKind::Diagnostics => {
            read_plan_with_sorted_view_example(
                "Read the master plan to correlate diagnostics findings with planned work and blocked tasks.",
            )
        }
    }
}

fn list_dir_tool_prompt(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor | AgentPromptKind::Solo => {
            "   {\"action\":\"list_dir\",\"path\":\".\",\"rationale\":\"Inspect the workspace before making assumptions.\"}".to_string()
        }
        AgentPromptKind::Planner => {
            "   {\"action\":\"list_dir\",\"path\":\"src\",\"rationale\":\"Inspect the relevant code area before expanding tasks.\"}".to_string()
        }
        AgentPromptKind::Verifier => {
            "   {\"action\":\"list_dir\",\"path\":\"src\",\"rationale\":\"Inspect the relevant area before verifying claims about it.\"}".to_string()
        }
        AgentPromptKind::Diagnostics => {
            "   {\"action\":\"list_dir\",\"path\":\"state\",\"rationale\":\"Inspect workspace-local state and observability artifacts before diagnosing failures.\"}\n   {\"action\":\"list_dir\",\"path\":\"src\",\"rationale\":\"Inspect the active workspace layout before targeting diagnostics.\"}".to_string()
        }
    }
}

fn read_file_tool_prompt(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor | AgentPromptKind::Solo => {
            format!(
                "   {{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"rationale\":\"Read the file before editing it.\"}}\n   {{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"line\":120,\"rationale\":\"Read the relevant section before editing it.\"}}\n{READ_FILE_EXECUTOR_FOOTER}"
            )
        }
        AgentPromptKind::Planner => {
            format!(
                "   {{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"rationale\":\"Read the source before deriving actionable plan steps.\"}}\n   {{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"line\":120,\"rationale\":\"Read the relevant source section before deriving actionable plan steps.\"}}\n{READ_FILE_FOOTER}"
            )
        }
        AgentPromptKind::Verifier => {
            format!(
                "   {{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"rationale\":\"Read the source to verify whether the claimed change exists.\"}}\n   {{\"action\":\"read_file\",\"path\":\"src/app.rs\",\"line\":120,\"rationale\":\"Jump to the relevant section to verify the claimed change.\"}}\n{READ_FILE_FOOTER}"
            )
        }
        AgentPromptKind::Diagnostics => {
            "   {\"action\":\"read_file\",\"path\":\"src/app.rs\",\"line\":1,\"rationale\":\"Read a suspected source file to correlate code with observed failures.\"}\n   ⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.".to_string()
        }
    }
}

fn prompt_intro(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "You are the canon executor agent.",
        AgentPromptKind::Verifier => "You are the canon verifier agent.",
        AgentPromptKind::Planner => "You are the canon planner agent.",
        AgentPromptKind::Diagnostics => "You are the canon diagnostics agent.",
        AgentPromptKind::Solo => "You are the canon solo agent.",
    }
}

fn prompt_mission(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "Your job is to execute the highest-priority READY work described in planner handoff messages and the master plan.\n`SPEC.md` is the canonical contract.\nLane plans are deprecated and should not be relied on for task selection.\nThe verifier judges code against `SPEC.md`.\nYou should only work on the top 1-10 ready tasks in the current cycle, then yield.\nDo not use internal tools.\nDo not reorganize or update `SPEC.md` or plan files yourself.\nMake source changes, run checks, and report evidence in `message.payload`.",
        AgentPromptKind::Verifier => "Your job is to critically review executor evidence against the codebase and judge whether the implementation satisfies `SPEC.md`.\nExecutor evidence is a hint only. The canonical truth is the codebase versus `SPEC.md`.\nIf violations are found, use the `violation` action (op=upsert) to record them in `VIOLATIONS.json`. Use `violation` op=resolve to clear violations that are no longer active. Never use `apply_patch` for VIOLATIONS.json.\nBe skeptical — do not trust executor claims at face value.",
        AgentPromptKind::Planner => "Your job is to read `SPEC.md`, `PLANS/OBJECTIVES.json`, `VIOLATIONS.json`, and `DIAGNOSTICS.json` and derive the master plan plus executor handoff guidance.\nYou own priority, dependency ordering, task allocation, and the ready-work window for each executor.\nOn every cycle, re-evaluate the workspace and update `PLAN.json` via the plan tool.\nAt the end of every planner cycle, review `PLANS/OBJECTIVES.json` and add or update objectives to reflect what was discovered. New objectives must include id, title, status, scope, authority_files, category, level, description, requirement, verification, and success_criteria. Use `apply_patch` to write them.\nDiagnostics are advisory only: do not create, reopen, or reprioritize tasks from diagnostics claims unless the same cycle includes direct current-workspace evidence from `read_file`, `run_command`, or `python`.\nIf diagnostics suggest a problem but direct evidence is missing, plan only evidence-gathering or diagnostics-repair work instead of implementation work.\nDo not use internal tools.\nDo not hand off work; complete the needed planning and execution directly in the current role flow.\nPlans must follow the JSON PLAN/TASK protocol in `SPEC.md`.",
        AgentPromptKind::Diagnostics => "Your job is to scan the active workspace state, analyze `VIOLATIONS.json`, detect root causes, rank them by impact, and write concrete repair targets for the planner in `DIAGNOSTICS.json` using the enums in canon-mini-agent/src/reports.rs.",
        AgentPromptKind::Solo => "Your job is to coordinate planning, execution, and verification in a single role while participating in orchestration.\nUse the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.\nYou may read, patch, and verify any in-workspace files when justified by evidence.\nKeep evidence tight and run checks before claiming completion.\nAt the end of every cycle — before emitting a completion message — review `PLANS/OBJECTIVES.json` and add or update objectives based on what you discovered. New objectives must include id, title, status, scope, authority_files, category, level, description, requirement, verification, and success_criteria. Use `apply_patch` to write them directly.",
    }
}

fn prompt_canonical_law(_kind: AgentPromptKind) -> String {
    let path = std::path::Path::new(workspace()).join(CANONICAL_LAW_FILE);
    std::fs::read_to_string(path)
        .unwrap_or_else(|_| "- Follow the invariants in INVARIANTS.json as the authoritative source of correctness for all decisions.".to_string())
}

fn prompt_workspace(kind: AgentPromptKind) -> String {
    let ws = crate::constants::workspace();
    match kind {
        AgentPromptKind::Executor => format!("You work inside the canon workspace at {ws}. All relative file paths resolve against this workspace root."),
        AgentPromptKind::Verifier => format!("You work inside the canon workspace at {ws}."),
        AgentPromptKind::Planner => format!("You work inside the canon workspace at {ws}. Use bash, semantic_map/symbol_window/symbol_refs (prefer over read_file for Rust source), python, apply_patch (lane plans only), and diagnostics evidence to review the current project state before reorganizing the plan."),
        AgentPromptKind::Diagnostics => format!("You must inspect the active workspace under {ws}, including source files plus any workspace-local state and observability artifacts that exist for this project."),
        AgentPromptKind::Solo => format!("You work inside the canon workspace at {ws}. Use the full tool suite to plan, execute, and verify changes."),
    }
}

fn action_contract(kind: AgentPromptKind) -> String {
    let actions = available_actions(kind)
        .iter()
        .map(|action| format!("- `{action}`"))
        .collect::<Vec<_>>()
        .join("\n");
    let graph_hint = "Graph tools hint: artifacts come from rustc wrapper capture (run `cargo build -p <crate>`). `graph_probe` inspects symbols/coverage; `graph_call`/`graph_cfg` emit CSVs; `graph_dataflow`/`graph_reachability` emit reports.";
    format!(
        "Each turn you receive either:\n  (a) the initial instruction; or\n  (b) the result of your last action.\n\nBefore choosing your action, think through the following internally:\n  1. What does the current evidence tell me about system state?\n  2. What is the highest-value action I can take right now?\n  3. What are the 2-3 most likely actions after this one?\n\nEmit exactly one action per turn as a single JSON object in a fenced json code block. Think through the decision internally; reveal your chain-of-thought. Only output the JSON action.\nAvailable actions:\n{actions}\n{graph_hint}\nEvery action MUST include:\n- `observation`: what you can see purely from evidence only, as a single string\n- `rationale`: why this is the next best step right now\n- `predicted_next_actions`: ordered array of 2-3 likely follow-on actions, each with an `action` name and `intent` string. This is your decision tree — drives the next turn.\nFor mutating or verification actions, also include:\n- `task_id`: the PLAN task you are advancing\n- `objective_id`: the objective linked to that task\n- `intent`: the concrete purpose of this action within that task\n\nDo NOT include any extra text outside the JSON code block.\nDo NOT echo the tools list or the prompt.\nDo NOT use placeholder action names like `...`; choose a real action from the list."
    )
}

fn tools_section(kind: AgentPromptKind) -> String {
    let _ = kind;
    String::new()
}

fn rules_common_footer() -> String {
    let agent_source = crate::constants::agent_state_dir().trim_end_matches("/agent_state");
    let protect_rule = if crate::constants::workspace() != agent_source {
        format!("- Never modify the canon-mini-agent source tree ({agent_source}).\n")
    } else {
        String::new()
    };
    let questions = crate::structured_questions::questions_prompt_snippet();
    format!(
        "{questions}\n\n\
         {protect_rule}\
         - Emit exactly one action per turn. Think through the decision internally; reveal chain-of-thought. Only output the JSON action.\n\
         - Every mutating action (`apply_patch`, `plan`, `objectives`, `issue`) MUST include a `question` field: the single decision-boundary question this action answers. If answered differently, a different action would be taken.\n\
         - If you cannot proceed (missing files/permissions, repeated tool errors, or irreconcilable evidence), emit a `message` with `type=blocker`, `status=blocked`, and payload fields `blocker`, `evidence`, `required_action`.\n\
         - Before emitting a completion message, review `PLANS/OBJECTIVES.json`. Add new objectives for anything you discovered this cycle that is not yet captured. Update the status of existing objectives that changed. Use `apply_patch` to write changes.\n\
         - Output format: exactly one JSON object in a fenced json code block. No prose outside it."
    )
}

fn rules_blocker_route(target: &str) -> String {
    format!("- If blocked, send the blocker to the {target}.")
}

fn rules_section(rules: &[&str], blocker_target: Option<&str>) -> String {
    let mut out =
        String::from("━━━ RULES ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
    for rule in rules {
        out.push_str(rule);
        out.push('\n');
    }
    if let Some(target) = blocker_target {
        out.push_str(&rules_blocker_route(target));
        out.push('\n');
    }
    out.push_str(&rules_common_footer());
    out
}

fn role_key(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "executor",
        AgentPromptKind::Verifier => "verifier",
        AgentPromptKind::Planner => "planner",
        AgentPromptKind::Diagnostics => "diagnostics",
        AgentPromptKind::Solo => "solo",
    }
}

fn load_role_overrides(kind: AgentPromptKind) -> Vec<String> {
    let roles_path = std::path::Path::new(workspace()).join("ROLES.json");
    let raw = match std::fs::read_to_string(&roles_path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let roles_obj = value
        .get("roles")
        .and_then(|v| v.as_object())
        .or_else(|| value.as_object());
    let Some(roles_obj) = roles_obj else {
        return Vec::new();
    };
    let Some(rules) = roles_obj.get(role_key(kind)).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    rules
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

const VERIFIER_PROCESS: &str = "━━━ VERIFICATION PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nFor each executor claim:\n1. Use the executor result summary plus `SPEC.md` to derive the candidate obligations.\n2. Read the relevant source files to confirm the described change exists.\n3. Run cargo check or cargo test if the task involves code correctness.\n4. Judge whether the code satisfies the spec.\n5. If violations are found, write `VIOLATIONS.json` with a clear, actionable list using the enums in canon-mini-agent/src/reports.rs.\n6. Update task `status` fields in `PLAN.json` via the `plan` action (never `apply_patch`) and update any related `next_on_success` / `next_on_failure` as needed.\n7. Report a verification breakdown in `message.payload` (verified, unverified, false) with explicit items.\n8. For any control-flow or state-management claim, verify that the described behavior matches the source code and is consistent with INVARIANTS.json.";

const PLANNER_PROCESS: &str = "━━━ PLANNING PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n\
⚠ PLAN.json EDIT RULE: use ONLY the `plan` action for all PLAN.json changes. \
apply_patch on PLAN.json is ALWAYS rejected by the runtime — retrying it wastes turns.\n\n\
On every planning cycle:\n\
1. Read `SPEC.md`, `VIOLATIONS.json`, `DIAGNOSTICS.json`, relevant source files, and recent workspace state to understand what changed.\n\
2. Update `PLAN.json` via the `plan` action and derive the ready-work window for each executor. Mark tasks `ready` (not `todo`) to make them executable — the executor only picks up `ready` tasks.\n\
3. Maintain a READY NOW window containing at most 1-10 executable tasks for each executor.\n\
4. Move blocked work behind its dependencies instead of leaving it in the ready window.\n\
5. Rewrite priorities whenever new evidence changes the critical path.\n\
6. If canonical-law authority (INVARIANTS.json, CANONICAL_LAW.md) conflicts with local heuristics in the plan, prioritize canonical-law authority and move heuristic cleanup behind it as follow-on work.\n\
7. Treat diagnostics as unverified hints until the same cycle includes direct current-workspace evidence from `read_file`, `run_command`, or `python`; do not create, reopen, or reprioritize implementation tasks from diagnostics alone.\n\
8. If diagnostics suggest a failure but source evidence is still missing, create only evidence-gathering or diagnostics-repair tasks until the claim is verified.\n\
9. Write detailed, imperative tasks that include file paths and concrete actions (read/patch/test).\n\
10. Send handoff messages to executors reflecting the updated ready window.\n\n\
Provenance fields — include on every new task:\n\
- `issue_refs`: array of ISSUES.json ids that motivated this task (e.g. [\"auto_mir_dup_abc123\"]). Empty array if none.\n\
- `objective_id`: the PLANS/OBJECTIVES.json objective id this task advances (e.g. \"obj_reduce_complexity\"). Omit if no clear match.";

fn diagnostics_process() -> String {
    let diagnostics_path = diagnostics_file();
    format!("━━━ DIAGNOSTICS PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nGather evidence from the workspace, `VIOLATIONS.json`, and the current codebase, then write {diagnostics_path} using the enums in canon-mini-agent/src/reports.rs.\nRules:\n- Use the `python` action for structured analysis of project state and any available logs.\n- Only modify {diagnostics_path}.\n- Rank issues by impact on correctness, convergence, and repairability.\n- Check whether control-flow decisions are consistent with the canonical law in CANONICAL_LAW.md and the invariants in INVARIANTS.json.\n- Before trusting any trace or log file, confirm it was updated in the current cycle (mtime, size change, or fresh producer command).\n- Treat empty `rg` / `grep` results as ambiguous: no match, stale file, or incomplete write are all possible.\n- Prefer the most recently written evidence sources over ad-hoc temp traces when they disagree.\n- Derive observability paths from workspace-local state and log artifacts that actually exist for this project instead of assuming canon-specific defaults.")
}

const EXECUTOR_HANDOFF_BULLETS: &[&str] = &[
    "files changed",
    "commands run",
    "outcomes / failing checks",
    "remaining uncertainty or blockers",
];

const EXECUTOR_PREFIX: &str = "━━━ EVIDENCE HANDOFF ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nAfter completing each task or sub-task from the planner handoff or master plan, do NOT update `SPEC.md` or `PLAN.json` yourself.\nInstead, use a `message` action and report verifier-facing evidence in `message.payload`:";

const EXECUTION_DISCIPLINE_BULLETS: &[&str] = &[
    "Prefer tasks explicitly marked ready / highest priority by the planner.",
    "Do not skip ahead to lower-priority or blocked tasks unless the current ready task is impossible and you have concrete evidence.",
    "Hard cap: after 5 actions you MUST hand off to the planner via `message` (handoff or blocker). The router enforces this.",
    "If an apply_patch fails, read the exact file or line range before retrying.",
    "Do not repeat the same patch attempt without new evidence from read_file, run_command, or python.",
    "When touching routing, policy, or control-flow code, favor the authority described in CANONICAL_LAW.md and INVARIANTS.json over local heuristics.",
    "Use MIR and HIR analysis to derive call graph, CFG, reachability, and dataflow when diagnosing bugs or proving fixes.",
    "If a task conflicts with the canonical law above, execute the canonical law and report the conflict in `message.payload` so planner/verifier can update plan truth.",
];

const SOLO_EXECUTION_DISCIPLINE_BULLETS: &[&str] = &[
    "Prefer tasks explicitly marked ready / highest priority by the planner.",
    "Do not skip ahead to lower-priority or blocked tasks unless the current ready task is impossible and you have concrete evidence.",
    "Use the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.",
    "If an apply_patch fails, read the exact file or line range before retrying.",
    "Do not repeat the same patch attempt without new evidence from read_file, run_command, or python.",
    "When touching routing, policy, or control-flow code, favor the authority described in CANONICAL_LAW.md and INVARIANTS.json over local heuristics.",
    "Use MIR and HIR analysis to derive call graph, CFG, reachability, and dataflow when diagnosing bugs or proving fixes.",
    "If a task conflicts with the canonical law above, execute the canonical law and report the conflict in `message.payload` so planner/verifier can update plan truth.",
];

fn format_bullets(header: &str, bullets: &[&str], suffix: Option<&str>) -> String {
    let mut out = String::from(header);
    for bullet in bullets {
        out.push_str("- ");
        out.push_str(bullet);
        out.push('\n');
    }
    if let Some(tail) = suffix {
        out.push('\n');
        out.push_str(tail);
    }
    out.trim_end().to_string()
}

fn execution_discipline() -> String {
    format_bullets(
        "Execution discipline:\n",
        EXECUTION_DISCIPLINE_BULLETS,
        None,
    )
}

fn solo_execution_discipline() -> String {
    format_bullets(
        "Execution discipline:\n",
        SOLO_EXECUTION_DISCIPLINE_BULLETS,
        None,
    )
}

const VERIFIER_RULES: &[&str] = &[
    "- Be critical and thorough — verify evidence, not just the claim.",
    "- Do not mark anything verified unless you have read the actual code and run verification commands.",
    "- You must run `run_command` (and `cargo_test` when relevant) to validate executor claims; do not accept evidence without running checks yourself.",
    "- Run `cargo build --workspace` before completing the cycle; fix failures before `message` with status=complete.",
    "- Update `PLAN.json` only via the `plan` action; never use `apply_patch` for plan edits.",
    "- Use the `violation` action to manage `VIOLATIONS.json` — ops: read | upsert | resolve | set_status | replace. Never use `apply_patch` for VIOLATIONS.json.",
    "- Reject any claimed completion that violates the canonical law in CANONICAL_LAW.md or the invariants in INVARIANTS.json.",
    "- When using `message`, set:",
    "  - `from`: \"Verifier\"",
    "  - `to`: \"Planner\"",
    "  - `type`: \"verification\" or \"failure\"",
    "  - `status`: \"verified\" or \"failed\"",
    "  - `payload.summary`: string",
    "  - `payload.verified_items` / `payload.unverified_items` / `payload.false_items` as needed",
];

const PLANNER_RULES: &[&str] = &[
    "- Use the `plan` action for all `PLAN.json` edits; never use `apply_patch` on the master plan.",
    "- `PLAN.json` MUST be valid JSON following the PLAN/TASK protocol in `SPEC.md`.",
    "- Only modify `PLAN.json` (via `plan`) and lane plans (via `apply_patch`) — never edit `src/`, `tests/`, `SPEC.md`, `VIOLATIONS.json`, or diagnostics reports.",
    "- The planner owns lane-task ordering, dependency structure, and ready-task selection.",
    "- Use state/reports/complexity/latest.json (supervisor-generated; proxy complexity_proxy=mir_blocks) to prioritize refactors and reduce branching/duplication.",
    "- Read `ISSUES.json` every cycle and promote top open issues into `PLANS/OBJECTIVES.json` and `PLAN.json` (or explicitly mark them resolved/wontfix with evidence). Issues are hints; objectives/plan are commitments.",
    "- Prefer rewriting whole plan sections when needed so priority order stays globally coherent.",
    "- Keep each executor's ready window small: 1-10 tasks maximum.",
    "- Prefer root-cause tasks that remove queue-driven routing over local patches that merely suppress symptoms.",
    "- Send handoff messages to executors reflecting the current ready window.",
    "- If the incoming handoff was sent by solo (check `message.from` or context role), finish your planning cycle and send the return `message` to solo so it can resume execution. Do not leave solo waiting.",
    "- PLAN.json is the authoritative source of truth for executor task selection. A handoff message alone is not sufficient — the task MUST be marked `ready` in PLAN.json before the executor will pick it up.",
    "- Always use the `python` action when reading or writing any `.json` state file (PLAN.json, OBJECTIVES.json, ISSUES.json, VIOLATIONS.json, diagnostics). Never use apply_patch or run_command shell pipelines to mutate JSON; use the `plan`, `objectives`, or `issue` actions for their respective files.",
    "- Executor tasks must only require executor-permitted actions: read_file, apply_patch, run_command, python, message, cargo_fmt, cargo_clippy, and semantic tools. The executor CANNOT use `plan`, `objectives`, `issue`, or `verify` actions — do NOT create tasks that require these for the executor. Reserve tasks using those actions for a planner or verifier cycle instead.",
];

fn diagnostics_rules() -> Vec<String> {
    let mut rules = vec![
        "- Use the `python` action for structured analysis of project state and any available logs.".to_string(),
        "- Only modify DIAGNOSTICS.json.".to_string(),
        "- Rank issues by impact on correctness, convergence, and repairability.".to_string(),
        "- Check control-flow and state-management decisions against CANONICAL_LAW.md and INVARIANTS.json.".to_string(),
        "- Complexity report artifact (supervisor-generated): state/reports/complexity/latest.json (proxy metric complexity_proxy=mir_blocks) for ranking refactor hotspots.".to_string(),
        "- Before trusting any trace or log file, confirm it was updated in the current cycle (mtime, size change, or fresh producer command).".to_string(),
        "- Treat empty `rg` / `grep` results as ambiguous: no match, stale file, or incomplete write are all possible.".to_string(),
        "- Prefer the most recently written evidence sources over ad-hoc temp traces when they disagree.".to_string(),
    ];
    rules.extend(load_role_overrides(AgentPromptKind::Diagnostics));
    rules
}

fn executor_rules() -> Vec<String> {
    let ws = crate::constants::workspace();
    let mut rules = vec![
        "- Always read a file before patching it.".to_string(),
        "- For Rust source navigation prefer semantic tools over raw file access: semantic_map (crate outline) → symbol_window (function body) → symbol_neighborhood / symbol_refs (call sites / references) → symbol_path (call chain). Use read_file only for non-Rust files or immediately before patching a Rust file to get line-numbered output.".to_string(),
        "- Use list_dir only to check whether a path exists or to enumerate non-source artifacts; use semantic_map to explore Rust source structure.".to_string(),
        "- Only list_dir paths that exist under WORKSPACE; do not assume `canon-utils` exists unless WORKSPACE is `/workspace/ai_sandbox/canon`.".to_string(),
        "- Use run_command for cargo builds, tests, and shell discovery.".to_string(),
        "- Use cargo_fmt and cargo_clippy tools for formatting/linting. Both return exactly 3 lines (status/log/summary) and write full output under state/logs/.".to_string(),
        "- Complexity report artifact (supervisor-generated): state/reports/complexity/latest.json (proxy metric complexity_proxy=mir_blocks) for hotspot targeting.".to_string(),
        "- Use python for structured analysis when shell pipelines are awkward.".to_string(),
        "- Always use the `python` action when reading or inspecting any `.json` state file (PLAN.json, OBJECTIVES.json, ISSUES.json, VIOLATIONS.json, diagnostics). Never use shell tools (cat, jq, grep) to read JSON — use python.".to_string(),
        "- Your work is scoped to the task_id provided in the planner handoff. Execute that specific task; do not pick up other PLAN.json tasks unless the planner explicitly includes them in the ready window.".to_string(),
        "- Each action you emit must include `task_id` and `objective_id` fields matching the current task. Never omit these provenance fields.".to_string(),
        "- When sending a `message` action, always set `\"from\": \"executor\"`. Never copy `from` values from other roles' messages in your context.".to_string(),
        "- When blocked or complete, send your `message` to `\"planner\"` — not to `diagnostics`, `verifier`, or other roles. The planner coordinates all role dispatch.".to_string(),
        format!("- Never operate outside {ws}."),
        "- Never modify `SPEC.md`, `PLAN.json`, `VIOLATIONS.json`, or `DIAGNOSTICS.json`.".to_string(),
        "- Never emit destructive commands (rm -rf, git reset --hard, git clean -f, etc.).".to_string(),
    ];
    rules.extend(load_role_overrides(AgentPromptKind::Executor));
    rules
}

fn solo_rules() -> Vec<String> {
    let ws = crate::constants::workspace();
    let mut rules = vec![
        "- Always read a file before patching it.".to_string(),
        "- For Rust source navigation prefer semantic tools over raw file access: semantic_map (crate outline) → symbol_window (function body) → symbol_neighborhood / symbol_refs (call sites / references) → symbol_path (call chain). Use read_file only for non-Rust files or immediately before patching a Rust file to get line-numbered output.".to_string(),
        "- Use list_dir only to check whether a path exists or to enumerate non-source artifacts; use semantic_map to explore Rust source structure.".to_string(),
        "- Use run_command for cargo builds, tests, and shell discovery.".to_string(),
        "- Use cargo_fmt and cargo_clippy tools for formatting/linting. Both return exactly 3 lines (status/log/summary) and write full output under state/logs/.".to_string(),
        "- Complexity report artifact (supervisor-generated): state/reports/complexity/latest.json (proxy metric complexity_proxy=mir_blocks) for hotspot targeting.".to_string(),
        "- Run cargo build/test before `message` with status=complete when changes affect code.".to_string(),
        "- If you rebuild canon-mini-agent, the supervisor may restart immediately in solo mode; be ready for a restart before the next step.".to_string(),
        "- You may modify any in-workspace files when justified by evidence; use the `plan` action for PLAN.json edits.".to_string(),
        format!("- Never operate outside {ws}."),
        "- Never emit destructive commands (rm -rf, git reset --hard, git clean -f, etc.).".to_string(),
        "- Actively probe the codebase with semantic tools every cycle: use symbol_refs to find all call sites, symbol_window to inspect function bodies, symbol_neighborhood to map a symbol's local context, symbol_path to trace call chains, and semantic_map to orient yourself within a crate. Do not rely on read_file alone for Rust source investigation.".to_string(),
        "- Issue discovery is a core solo responsibility: when you encounter a logic gap, missing guard, incorrect heuristic, stale artifact, or spec deviation — open an issue immediately via the `issue` action with kind, location, evidence, and priority. Do not defer issue creation to a later cycle.".to_string(),
    ];
    rules.extend(load_role_overrides(AgentPromptKind::Solo));
    rules
}

fn executor_handoff() -> String {
    format_bullets(
        &format!("{EXECUTOR_PREFIX}\n"),
        EXECUTOR_HANDOFF_BULLETS,
        Some("Read `SPEC.md` and `PLAN.json` when needed for execution context, but leave planning-file mutation to planner."),
    )
}

fn prompt_tail(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor => format_prompt_tail_with_prefix(
            &executor_handoff(),
            Some(&execution_discipline()),
            &executor_rules(),
            Some("Planner"),
        ),
        AgentPromptKind::Verifier => format_prompt_tail_with_prefix(
            VERIFIER_PROCESS,
            None,
            &role_rules_with_overrides(VERIFIER_RULES, AgentPromptKind::Verifier),
            Some("Planner"),
        ),
        AgentPromptKind::Planner => format_prompt_tail_with_prefix(
            PLANNER_PROCESS,
            None,
            &role_rules_with_overrides(PLANNER_RULES, AgentPromptKind::Planner),
            Some("Diagnostics"),
        ),
        AgentPromptKind::Diagnostics => format_prompt_tail_with_prefix(
            &diagnostics_process(),
            None,
            &diagnostics_rules(),
            Some("Planner"),
        ),
        AgentPromptKind::Solo => format_prompt_tail_with_prefix(
            &solo_execution_discipline(),
            None,
            &solo_rules(),
            Some("Planner"),
        ),
    }
}

fn role_rules_with_overrides(base: &[&str], kind: AgentPromptKind) -> Vec<String> {
    let mut rules: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    rules.extend(load_role_overrides(kind));
    rules
}

fn format_prompt_tail_with_prefix(
    prefix: &str,
    middle: Option<&str>,
    rules: &[String],
    blocker_target: Option<&str>,
) -> String {
    let refs: Vec<&str> = rules.iter().map(|s| s.as_str()).collect();
    match middle {
        Some(middle) => format!(
            "{}\n\n{}\n\n{}",
            prefix,
            middle,
            rules_section(&refs, blocker_target)
        ),
        None => format!("{}\n\n{}", prefix, rules_section(&refs, blocker_target)),
    }
}

pub(crate) fn system_instructions(kind: AgentPromptKind) -> String {
    let mut out = String::new();
    out.push_str(prompt_intro(kind));
    out.push_str("\n\n");
    out.push_str(prompt_mission(kind));
    out.push_str("\n\nCanonical law:\n");
    out.push_str(&prompt_canonical_law(kind));
    out.push_str("\n\n");
    out.push_str(&prompt_workspace(kind));
    out.push_str("\n\n");
    // Truncate issues section to avoid prompt overflow
    let issues = crate::issues::read_top_open_issues(
        std::path::Path::new(workspace()),
        3,
    );
    out.push_str(&truncate_section(&issues, 1500));
    out.push_str("\n\n");
    out.push_str("Tool protocol schemas (schemars):\n");
    // Truncate tool schema to bounded size to prevent prompt explosion
    let schema = crate::tool_schema::tool_protocol_schema_split_text();
    out.push_str(&truncate_section(&schema, 4000));
    out.push_str("\nFull syntax examples with notes: state/tool_examples.md — use read_file when you need a reminder.\n\n");
    out.push_str(&prompt_tail(kind));
    out
}

// Helper: truncate large prompt sections deterministically
fn truncate_section(input: &str, max_len: usize) -> String {
    if input.len() <= max_len {
        input.to_string()
    } else {
        let mut s = input[..max_len].to_string();
        s.push_str("\n... [truncated]");
        s
    }
}

pub(crate) fn planner_cycle_prompt(
    summary_text: &str,
    objectives_text: &str,
    lessons_text: &str,
    invariants_text: &str,
    violations_text: &str,
    diagnostics_text: &str,
    issues_text: &str,
    plan_diff: &str,
    executor_diff: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    let issues_file = crate::constants::ISSUES_FILE;
    let mut s = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\n\
         Canonical references:\n\
         - Spec: {SPEC_FILE}\n\
         - Objectives: {OBJECTIVES_FILE}\n\
         - Invariants: {INVARIANTS_FILE}\n\
         - Violations: {VIOLATIONS_FILE}\n\
         - Diagnostics: {diagnostics_file}\n\
         - Issues: {issues_file}\n\
         - Master plan: {MASTER_PLAN_FILE}\n\n\
         PLAN.json EDIT RULE: always use the `plan` action — NEVER apply_patch on {MASTER_PLAN_FILE}.\n\n\
         Current plan state (from {MASTER_PLAN_FILE}) — read-only context, edit via `plan` action:\n{plan_diff}"
    );
    append_planner_cycle_optional_sections(
        &mut s,
        objectives_text,
        lessons_text,
        invariants_text,
        violations_text,
        diagnostics_text,
        issues_text,
        executor_diff,
        cargo_test_failures,
        summary_text,
        diagnostics_file,
        issues_file,
    );
    append_planner_cycle_footer(&mut s);
    s
}

fn append_planner_cycle_optional_sections(
    output: &mut String,
    objectives_text: &str,
    lessons_text: &str,
    invariants_text: &str,
    violations_text: &str,
    diagnostics_text: &str,
    issues_text: &str,
    executor_diff: &str,
    cargo_test_failures: &str,
    summary_text: &str,
    diagnostics_file: &str,
    issues_file: &str,
) {
    append_optional_prompt_section(
        output,
        executor_diff,
        "Executor diff (workspace changes excluding plans/diagnostics/violations)",
    );
    append_optional_prompt_section(
        output,
        cargo_test_failures,
        "Latest cargo test failures (from cargo_test_failures.json)",
    );
    append_optional_prompt_section(
        output,
        objectives_text,
        &format!("Objectives (from {OBJECTIVES_FILE})"),
    );
    append_optional_prompt_section(
        output,
        issues_text,
        &format!("Open issues (from {issues_file})"),
    );
    append_optional_prompt_section(output, lessons_text, "Lessons artifact");
    append_optional_prompt_section(
        output,
        invariants_text,
        &format!("Invariants (from {INVARIANTS_FILE})"),
    );
    append_optional_prompt_section(
        output,
        violations_text,
        &format!("Violations (from {VIOLATIONS_FILE})"),
    );
    append_optional_prompt_section(
        output,
        diagnostics_text,
        &format!("Diagnostics report (from {diagnostics_file})"),
    );
    append_optional_prompt_section(output, summary_text, "Latest verifier summary");
}

fn append_planner_cycle_footer(output: &mut String) {
    output.push_str("\n\nDiagnostics-derived planning guard:\n- Do not create or reprioritize tasks from diagnostics alone.\n- Before accepting any diagnostics claim, read the implicated source files or gather equivalent current-cycle evidence.\n- Treat stale or already-resolved diagnostics as non-actionable until current source evidence reconfirms them.\n- If diagnostics repeatedly report stale issues, create follow-up work to repair diagnostics generation rather than reopening resolved implementation tasks.");
    output.push_str(&format!(
        "\n\nBefore completing this cycle, review {OBJECTIVES_FILE} and add or update objectives \
         to capture anything discovered. New objectives require a unique id, title, category, \
         level, and description. Use the `objectives` action (op: create_objective / \
         update_objective) or `apply_patch` targeting ONLY {OBJECTIVES_FILE} to write them. \
         NEVER use apply_patch for PLAN.json — it is always rejected; use the `plan` action.\
         \n\nYou may send a message action to other agents at any time. Think hard internally before responding."
    ));
}

fn append_optional_prompt_section(output: &mut String, body: &str, heading: &str) {
    if body.trim().is_empty() {
        return;
    }
    output.push_str(&format!("\n\n{heading}\n{body}"));
}

pub(crate) fn executor_cycle_prompt(
    _executor_name: &str,
    lane_label: &str,
    latest_verify_result: &str,
    ready_tasks: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    let verify_result = if latest_verify_result.trim().is_empty()
        || latest_verify_result
            .trim()
            .eq_ignore_ascii_case("shutdown requested")
    {
        "(no verifier result available)".to_string()
    } else {
        latest_verify_result.to_string()
    };
    format!(
        "TAB_ID: pending\nTURN_ID: pending\nAGENT_TYPE: EXECUTOR\n\nWORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Master plan: {MASTER_PLAN_FILE}\n- Violations: {VIOLATIONS_FILE}\n- Diagnostics: {diagnostics_file}\n\nREADY TASKS (from {MASTER_PLAN_FILE}, top-10 by plan order):\n{ready_tasks}\n\nLane plans are deprecated. Use planner handoff messages and {MASTER_PLAN_FILE} for task selection.\nLatest verifier result for lane {lane_label}:\n{verify_result}\n\nYou may send a message action to other agents at any time."
    )
}

pub(crate) fn verifier_cycle_prompt(
    lane_label: &str,
    exec_result: &str,
    executor_diff: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Objectives: {OBJECTIVES_FILE}\n- Invariants: {INVARIANTS_FILE}\n- Master plan: {MASTER_PLAN_FILE}\n- Diagnostics: {diagnostics_file}\n- Violations to write: {VIOLATIONS_FILE}\n\nExecutor diff (workspace changes excluding plans/diagnostics/violations):\n{executor_diff}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nExecutor lane: {lane_label}\nExecutor result summary:\n{exec_result}\n\nYou may send a message action to other agents at any time. Think hard internally before responding."
    )
}

pub(crate) fn diagnostics_cycle_prompt(summary_text: &str, cargo_test_failures: &str) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Objectives: {OBJECTIVES_FILE}\n- Invariants: {INVARIANTS_FILE}\n- Violations: {VIOLATIONS_FILE}\n- Diagnostics report to write: {diagnostics_file}\n- Lessons candidates (synthesized from action log): agent_state/lessons_candidates.json\n- Promoted lessons (injected into planner): agent_state/lessons.json\n- Observability artifacts: inspect workspace-local state and log paths that actually exist for this project\n\nLatest verifier summary:\n{summary_text}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nDiagnostics reconciliation guard:\n- Cross-check every ranked failure against the current {VIOLATIONS_FILE} contents and current-source evidence from this cycle.\n- Do not re-report failures that the verifier has already cleared unless fresh source or runtime evidence now reconfirms them.\n- If the stale state is in diagnostics itself, emit a diagnostics-repair finding instead of reopening the resolved implementation issue.\n\nLessons review (optional, do after main diagnostics work):\n- Use `lessons` op=read_candidates to inspect pending patterns detected from the action log.\n- Promote candidates that reflect real, recurring patterns (op=promote with candidate_id).\n- Reject candidates that are coincidental or already obvious (op=reject).\n- Promoted patterns appear in lessons.json and are injected into every future planner prompt.\n\nYou may send a message action to other agents at any time.Think hard internally before responding."
    )
}

pub(crate) fn single_role_verifier_prompt(
    _primary_input: &str,
    objectives: &str,
    invariants: &str,
    executor_diff_text: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let mut sections = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed."
    );
    if !objectives.trim().is_empty() {
        sections.push_str(&format!("\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}"));
    }
    if !invariants.trim().is_empty() {
        sections.push_str(&format!("\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}"));
    }
    if !executor_diff_text.trim().is_empty() {
        sections.push_str(&format!("\n\nExecutor diff (workspace changes excluding plans/diagnostics/violations):\n{executor_diff_text}"));
    }
    if !cargo_test_failures.trim().is_empty() {
        sections.push_str(&format!("\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}"));
    }
    sections.push_str(&format!("\n\nVerify that objectives in {OBJECTIVES_FILE} are completed properly.\nUpdate task status fields in {MASTER_PLAN_FILE} to reflect verified results.\nWrite violations to {VIOLATIONS_FILE} if any are found.\nWhen complete, report verified/unverified/false items in `message.payload`.\nEmit exactly one action to begin. Think through the decision internally; reveal chain-of-thought."));
    sections
}

pub(crate) fn single_role_diagnostics_prompt(
    violations: &str,
    objectives: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nRead files and search the source code for bugs and inconsistencies (use read_file + run_command/ripgrep).\nRun python analysis actions over available workspace-local logs, state, and code evidence.\nDo not assume canon-specific observability names or paths. Discover the actual project-local artifacts first by inspecting files and directories that exist under WORKSPACE. Examples may include state/, log/, logs/, runtime logs, jsonl logs, agent logs, or other workspace-defined artifacts.\nInfer the root cause from the evidence and cite detailed sources of errors (file paths, functions, log evidence).\n\nLatest verifier summary:\n(none yet)\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nVerify whether objectives in {OBJECTIVES_FILE} are being met and note gaps.\nUse {SPEC_FILE}, {OBJECTIVES_FILE}, and {INVARIANTS_FILE} as the contract, not lane plans.\nInfer failures from code, logs, runtime state, and verifier findings.\nPrefer evidence from workspace-local artifacts that actually exist over assumptions from other projects.\nCross-check proposed ranked failures against the current {VIOLATIONS_FILE} state before writing diagnostics.\nDo not restate verifier-cleared or already-resolved issues unless fresh current-cycle source or runtime evidence reconfirms them.\nIf the mismatch is stale diagnostics state rather than a live implementation bug, record a diagnostics-repair failure instead of reopening the cleared issue.\n\nWrite a ranked diagnostics report to {diagnostics_path}."
    )
}

fn append_single_role_planner_sections(
    sections: &mut String,
    objectives: &str,
    issues: &str,
    issues_file: &str,
    lessons_text: &str,
    invariants: &str,
    violations: &str,
    diagnostics: &str,
    diagnostics_path: &str,
    cargo_test_failures: &str,
) {
    append_optional_prompt_section(
        sections,
        objectives,
        &format!("Objectives (from {OBJECTIVES_FILE}):"),
    );
    append_optional_prompt_section(
        sections,
        issues,
        &format!("Open issues (from {issues_file}):"),
    );
    append_optional_prompt_section(sections, lessons_text, "Lessons artifact:");
    append_optional_prompt_section(
        sections,
        invariants,
        &format!("Invariants (from {INVARIANTS_FILE}):"),
    );
    append_optional_prompt_section(
        sections,
        violations,
        &format!("Violations (from {VIOLATIONS_FILE}):"),
    );
    append_optional_prompt_section(
        sections,
        diagnostics,
        &format!("Diagnostics report (from {diagnostics_path}):"),
    );
    append_optional_prompt_section(
        sections,
        cargo_test_failures,
        "Latest cargo test failures (from cargo_test_failures.json):",
    );
}

pub(crate) fn single_role_planner_prompt(
    _primary_input: &str,
    objectives: &str,
    lessons_text: &str,
    invariants: &str,
    violations: &str,
    diagnostics: &str,
    issues: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let issues_file = crate::constants::ISSUES_FILE;
    let mut sections = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed."
    );
    append_single_role_planner_sections(
        &mut sections,
        objectives,
        issues,
        issues_file,
        lessons_text,
        invariants,
        violations,
        diagnostics,
        &diagnostics_path,
        cargo_test_failures,
    );
    sections.push_str(&format!("\n\nUse {INVARIANTS_FILE} when deriving plan constraints.\nRead files and search the source code before issuing plan changes.\nDo not create or reprioritize tasks from diagnostics alone.\nBefore accepting any diagnostics claim, read the implicated source files or gather equivalent current-cycle evidence.\nTreat stale or already-resolved diagnostics as non-actionable until current source evidence reconfirms them.\nIf diagnostics repeatedly report stale issues, create follow-up work to repair diagnostics generation rather than reopening resolved implementation tasks.\nWrite imperative, actionable instructions in {MASTER_PLAN_FILE}.\nOnly use plan diffs when available; avoid re-reading the full plan unless necessary.\nDo not use internal tools.\nDo not hand off work; keep planning and execution in the current role flow."));
    sections
}

pub(crate) fn single_role_solo_prompt(
    _spec: &str,
    master_plan: &str,
    objectives: &str,
    lessons_text: &str,
    invariants: &str,
    violations: &str,
    diagnostics: &str,
    cargo_test_failures: &str,
    rename_candidates: &str,
    issues_text: &str,
    executor_diff_text: &str,
    plan_diff_text: &str,
    complexity_hotspots: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let issues_file = crate::constants::ISSUES_FILE;
    let mut sections = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed.\n\nMaster plan (from {MASTER_PLAN_FILE}):\n{}",
        truncate_section(master_plan, 8000)
    );
    append_optional_prompt_section(
        &mut sections,
        plan_diff_text,
        &format!("Plan diff since last cycle (from {MASTER_PLAN_FILE}):"),
    );
    append_optional_prompt_section(
        &mut sections,
        executor_diff_text,
        "Workspace diff since last cycle (git diff, excluding plans/diagnostics/violations):",
    );
    append_optional_prompt_section(
        &mut sections,
        &truncate_section(objectives, 3000),
        &format!("Objectives (from {OBJECTIVES_FILE}):"),
    );
    append_optional_prompt_section(
        &mut sections,
        &truncate_section(issues_text, 2000),
        &format!("Open issues ranked by score (from {issues_file}):"),
    );
    // Lessons section must always be present (even if empty) to satisfy prompt invariants/tests
    sections.push_str(&format!(
        "\n\nLessons artifact:\n{}",
        truncate_section(lessons_text, 2000)
    ));
    append_optional_prompt_section(
        &mut sections,
        complexity_hotspots,
        "Complexity hotspots (supervisor-generated; mir_blocks proxy — higher = more branching):",
    );
    append_optional_prompt_section(
        &mut sections,
        &truncate_section(invariants, 2000),
        &format!("Invariants (from {INVARIANTS_FILE}):"),
    );
    append_optional_prompt_section(
        &mut sections,
        &truncate_section(violations, 2000),
        &format!("Violations (from {VIOLATIONS_FILE}):"),
    );
    append_optional_prompt_section(
        &mut sections,
        &truncate_section(diagnostics, 2000),
        &format!("Diagnostics report (from {diagnostics_path}):"),
    );
    append_optional_prompt_section(
        &mut sections,
        cargo_test_failures,
        "Latest cargo test failures (from cargo_test_failures.json):",
    );
    // Only include rename section when candidates are non-empty
    if !rename_candidates.trim().is_empty() {
        let rename_section = format!(
            "{rename_candidates}\nFor each candidate: use `symbols_prepare_rename` to select it, then `rename_symbol` to apply. Work through them in score-descending order."
        );
        append_optional_prompt_section(
            &mut sections,
            &rename_section,
            "Pending rename tasks (from state/rename_candidates.json):",
        );
    }
    if !sections.ends_with("\n\n") {
        sections.push_str("\n\n");
    }
    sections.push_str("Use the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.\nIssue discovery is a primary solo responsibility. When you observe a logic gap, missing guard, incorrect heuristic, stale artifact, or spec deviation — open an issue immediately with the `issue` action (fields: id, title, kind, description, location, evidence[], priority). Do not defer; record it in the same cycle you find it.\nFor all Rust source investigation use semantic tools first: symbol_refs (call sites), symbol_window (function body), symbol_neighborhood (local context), symbol_path (call chain), semantic_map (crate outline). Reach for read_file only when you need line numbers immediately before a patch.\n\nLessons review (do after main work each cycle):\n- Use `lessons` op=read_candidates to inspect patterns synthesized from your action log.\n- Promote candidates that reflect real, recurring patterns you want to remember (op=promote with candidate_id).\n- Reject candidates that are coincidental or already obvious (op=reject).\n- Promoted lessons appear in lessons.json and are injected into every future prompt.\n- Use `lessons` op=encode once a lesson has been hardcoded into system source, so it no longer occupies prompt space.");
    sections
}

pub(crate) fn single_role_executor_prompt(
    _spec: &str,
    master_plan: &str,
    violations: &str,
    diagnostics: &str,
    invariants: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let mut sections = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed.\n\nMaster plan (from {MASTER_PLAN_FILE}):\n{master_plan}"
    );
    if !violations.trim().is_empty() {
        sections.push_str(&format!("\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}"));
    }
    if !diagnostics.trim().is_empty() {
        sections.push_str(&format!("\n\nDiagnostics (from {diagnostics_path}):\n{diagnostics}"));
    }
    if !invariants.trim().is_empty() {
        sections.push_str(&format!("\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}"));
    }
    sections.push_str("\n\nLane plans are deprecated. Use planner handoff messages and {MASTER_PLAN_FILE} for task selection.\n\nDo not modify spec, plan, violations, or diagnostics.\nDo not use internal tools.\nDo not hand off work; continue execution directly in the current role flow.\nUse `message.payload` to report evidence for verifier review. Emit exactly one action to begin. Think through the decision internally; reveal chain-of-thought.");
    sections
}

#[cfg(test)]
mod prompt_regression_tests {
    use super::single_role_solo_prompt;

    #[test]
    fn single_role_solo_prompt_omits_rename_section_when_candidates_empty() {
        let output = single_role_solo_prompt(
            "spec",
            "plan",
            "objectives",
            "lessons",
            "invariants",
            "violations",
            "diagnostics",
            "failures",
            "",
            "",
            "",
            "",
            "",
        );

        assert!(!output.contains("Pending rename tasks (from state/rename_candidates.json):"));
        assert!(!output.contains("For each candidate: use `symbols_prepare_rename` to select it"));
    }

    #[test]
    fn single_role_solo_prompt_includes_rename_section_when_candidates_present() {
        let output = single_role_solo_prompt(
            "spec",
            "plan",
            "objectives",
            "lessons",
            "invariants",
            "violations",
            "diagnostics",
            "failures",
            "candidate1",
            "",
            "",
            "",
            "",
        );

        assert!(output.contains("Pending rename tasks (from state/rename_candidates.json):"));
        assert!(output.contains("candidate1"));
        assert!(output.contains("For each candidate: use `symbols_prepare_rename` to select it"));
    }

    #[test]
    fn single_role_solo_prompt_rename_section_formatting_is_stable() {
        let empty_output = single_role_solo_prompt(
            "spec",
            "plan",
            "objectives",
            "lessons",
            "invariants",
            "violations",
            "diagnostics",
            "failures",
            "",
            "",
            "",
            "",
            "",
        );
        let non_empty_output = single_role_solo_prompt(
            "spec",
            "plan",
            "objectives",
            "lessons",
            "invariants",
            "violations",
            "diagnostics",
            "failures",
            "candidate1",
            "",
            "",
            "",
            "",
        );

        assert!(empty_output.contains("Latest cargo test failures (from cargo_test_failures.json):\nfailures\n\nUse the `plan` action"));
        assert!(non_empty_output.contains("Latest cargo test failures (from cargo_test_failures.json):\nfailures\n\nPending rename tasks (from state/rename_candidates.json):\ncandidate1\nFor each candidate: use `symbols_prepare_rename` to select it, then `rename_symbol` to apply. Work through them in score-descending order.\n\nUse the `plan` action"));
        assert!(non_empty_output.len() > empty_output.len());
    }
}

// ── Action parsing ─────────────────────────────────────────────────────────────

pub(crate) fn parse_actions(raw: &str) -> Result<Vec<Value>> {
    if let Some(json_text) = extract_json_candidate(raw) {
        if let Ok(actions) = parse_json_action(&json_text) {
            return Ok(actions);
        }
    }
    match parse_json_from_text(raw) {
        Ok(value) => parse_json_action_value(value)
            .with_context(|| "response contained JSON but not a valid action object"),
        Err(_) => parse_json_action(raw.trim()).with_context(|| {
            format!(
                "response was not a JSON action object: {:?}",
                &raw.chars().take(200).collect::<String>()
            )
        }),
    }
}

fn extract_json_candidate(text: &str) -> Option<String> {
    if let Some(fenced) = extract_json_fence(text) {
        return Some(fenced.to_string());
    }
    let bytes = text.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' || b == b'[' {
            start = Some(i);
            break;
        }
    }
    let start = start?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(text[start..end].trim().to_string());
                }
            }
            _ => {}
        }
    }
    Some(text[start..].trim().to_string())
}

fn extract_json_fence(text: &str) -> Option<&str> {
    let mut search_start = 0;
    while let Some(rel) = text[search_start..].find("```") {
        let fence_start = search_start + rel;
        let line_end = text[fence_start..]
            .find('\n')
            .map(|idx| fence_start + idx)?;
        let fence_line = &text[fence_start..line_end];
        let fence_line_lc = fence_line.to_ascii_lowercase();
        if fence_line_lc.contains("json") {
            let rest = &text[line_end + 1..];
            let end = rest.find("```");
            return Some(match end {
                Some(idx) => rest[..idx].trim(),
                None => rest.trim(),
            });
        }
        search_start = line_end + 1;
    }
    None
}

fn parse_json_from_text(text: &str) -> Result<Value> {
    for (idx, ch) in text.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }
        let slice = &text[idx..];
        let de = serde_json::Deserializer::from_str(slice);
        let mut iter = de.into_iter::<Value>();
        if let Some(Ok(value)) = iter.next() {
            return Ok(value);
        }
    }
    bail!("no JSON object found in response")
}

fn parse_json_action(text: &str) -> Result<Vec<Value>> {
    let value = serde_json::from_str::<Value>(text)?;
    parse_json_action_value(value).with_context(|| {
        format!(
            "not a JSON action object: {:?}",
            &text.chars().take(120).collect::<String>()
        )
    })
}

fn parse_json_action_value(value: Value) -> Result<Vec<Value>> {
    if value.is_object() && value.get("action").is_some() {
        return Ok(vec![value]);
    }
    if let Some(arr) = value.as_array() {
        if arr.len() == 1 && arr[0].is_object() && arr[0].get("action").is_some() {
            return Ok(arr.clone());
        }
        bail!(
            "expected exactly one action object, got array of len {}",
            arr.len()
        );
    }
    bail!("not a JSON action object")
}

pub(crate) fn diagnostics_python_reads_event_logs(action: &Value) -> bool {
    if action.get("action").and_then(|v| v.as_str()) != Some("python") {
        return false;
    }
    let code = action.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let lower = code.to_lowercase();
    // Accept generic workspace-local state/log inspection instead of privileging canon-specific paths.
    code.contains("Path('state')")
        || code.contains("Path(\"state\")")
        || code.contains("Path('log')")
        || code.contains("Path(\"log\")")
        || code.contains("Path('logs')")
        || code.contains("Path(\"logs\")")
        || (code.contains("state") && code.contains("rglob"))
        || (code.contains("log") && code.contains("rglob"))
        // Accept common canon-mini-agent observability locations.
        || lower.contains("agent_state")
        || lower.contains("actions.jsonl")
        || lower.contains("log.jsonl")
        || lower.contains("canon-mini-agent-logs.log")
        || lower.contains("frames/")
        || lower.contains("frames\\")
}

pub(crate) fn action_rationale(action: &Value) -> Option<&str> {
    action
        .get("rationale")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn action_observation(action: &Value) -> Option<&str> {
    action
        .get("observation")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn action_task_id(action: &Value) -> Option<&str> {
    action
        .get("task_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn action_objective_id(action: &Value) -> Option<&str> {
    action
        .get("objective_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn action_intent(action: &Value) -> Option<&str> {
    action
        .get("intent")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn action_requires_provenance(action: &Value) -> bool {
    let kind = action.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "apply_patch" | "rename_symbol" | "run_command" | "python" | "cargo_test" | "cargo_clippy" => true,
        "cargo_fmt" => true,
        "plan" => action.get("op").and_then(|v| v.as_str()) != Some("sorted_view"),
        "objectives" => !matches!(
            action.get("op").and_then(|v| v.as_str()),
            Some("read") | Some("sorted_view")
        ),
        "issue" => action.get("op").and_then(|v| v.as_str()) != Some("read"),
        _ => false,
    }
}

fn plan_task_objective_id(task_id: &str) -> Option<String> {
    let plan_path = std::path::Path::new(workspace()).join(MASTER_PLAN_FILE);
    let raw = std::fs::read_to_string(plan_path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("tasks")
        .and_then(|v| v.as_array())
        .and_then(|tasks| {
            tasks.iter().find(|task| task.get("id").and_then(|v| v.as_str()) == Some(task_id))
        })
        .and_then(|task| task.get("objective_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn validate_action_provenance(action: &Value) -> Result<()> {
    if !action_requires_provenance(action) {
        return Ok(());
    }

    let task_id = action_task_id(action)
        .ok_or_else(|| anyhow!("mutating or verification actions must include non-empty task_id"))?;
    let objective_id = action_objective_id(action)
        .ok_or_else(|| anyhow!("mutating or verification actions must include non-empty objective_id"))?;
    let _intent = action_intent(action)
        .ok_or_else(|| anyhow!("mutating or verification actions must include non-empty intent"))?;

    let active_task_id = crate::constants::active_task_id();
    if !active_task_id.is_empty() && task_id != active_task_id {
        bail!(
            "active plan task is '{active_task_id}' — mutating or verification actions must name that task in task_id"
        );
    }

    if let Some(expected_objective_id) = plan_task_objective_id(task_id) {
        if expected_objective_id.trim() != objective_id {
            bail!(
                "task '{task_id}' is linked to objective_id '{expected_objective_id}' in PLAN.json — action objective_id must match"
            );
        }
    }

    Ok(())
}

fn default_rationale(kind: &str) -> &'static str {
    match kind {
        "list_dir" => "Inspect the workspace before making assumptions.",
        "read_file" => "Read the current file contents before acting on them.",
        "apply_patch" => "Apply the concrete change after gathering enough context.",
        "run_command" => "Run a command to inspect or verify the current state.",
        "python" => "Use Python for structured analysis that is awkward in shell.",
        "cargo_test" => "Run the exact failing test using the harness-style command.",
        "message" => "Send a protocol message to the next role with structured payload.",
        _ => "Take the next most justified step based on the available evidence.",
    }
}

pub(crate) enum MessageValidationMode {
    Basic,
    Strict,
}

fn require_non_empty_message_field(
    obj: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<()> {
    obj.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("message missing non-empty '{field}'"))?;
    Ok(())
}

fn validate_message_required_fields(obj: &serde_json::Map<String, Value>) -> Result<()> {
    for field in ["from", "to", "type", "status"] {
        require_non_empty_message_field(obj, field)?;
    }
    obj.get("payload")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("message missing object payload"))?;
    Ok(())
}

fn validate_blocker_message_payload(msg: ProtocolMessage) -> Result<()> {
    if !(matches!(msg.msg_type, MessageType::Blocker)
        || matches!(msg.status, MessageStatus::Blocked))
    {
        return Ok(());
    }
    match msg.payload {
        MessagePayload::Blocker(payload) => {
            if payload.blocker.trim().is_empty()
                || payload.evidence.trim().is_empty()
                || payload.required_action.trim().is_empty()
            {
                bail!("blocker payload fields must be non-empty strings");
            }
            Ok(())
        }
        _ => bail!(
            "blocker messages must include payload fields: blocker, evidence, required_action"
        ),
    }
}

fn validate_optional_message_severity(obj: &serde_json::Map<String, Value>) -> Result<()> {
    let Some(severity) = obj.get("severity").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let _ = serde_json::from_value::<crate::protocol::Severity>(Value::String(
        severity.to_string(),
    ))
    .map_err(|_| anyhow!("message severity must be one of: info|warn|error|critical"))?;
    Ok(())
}

fn validate_optional_message_role(
    obj: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<()> {
    let Some(role) = obj.get(field) else {
        return Ok(());
    };
    let _ = serde_json::from_value::<Role>(role.clone()).map_err(|_| {
        anyhow!("{field} must be one of: executor|planner|verifier|diagnostics|solo")
    })?;
    Ok(())
}

pub(crate) fn validate_message_action(action: &Value, mode: MessageValidationMode) -> Result<()> {
    let obj = action
        .as_object()
        .ok_or_else(|| anyhow!("action payload must be a JSON object"))?;
    validate_message_required_fields(obj)?;
    if matches!(mode, MessageValidationMode::Basic) {
        return Ok(());
    }
    let msg: ProtocolMessage = serde_json::from_value(action.clone())
        .map_err(|e| anyhow!("message schema invalid: {e}"))?;
    validate_blocker_message_payload(msg)?;
    validate_optional_message_severity(obj)?;
    validate_optional_message_role(obj, "from_role")?;
    validate_optional_message_role(obj, "to_role")?;
    Ok(())
}

pub(crate) fn normalize_action(action: &mut Value) -> Result<()> {
    let obj = action
        .as_object_mut()
        .ok_or_else(|| anyhow!("action payload must be a JSON object"))?;
    let kind = obj
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("action missing 'action'"))?
        .to_string();
    if obj
        .get("rationale")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        obj.insert(
            "rationale".to_string(),
            Value::String(default_rationale(&kind).to_string()),
        );
    }
    if kind == "message" {
        if obj.get("from").is_none() {
            if let Some(val) = obj.get("from_role").cloned() {
                obj.insert("from".to_string(), val);
            }
        }
        if obj.get("to").is_none() {
            if let Some(val) = obj.get("to_role").cloned() {
                obj.insert("to".to_string(), val);
            }
        }
        for field in ["from", "to", "type", "status"] {
            if let Some(val) = obj
                .get_mut(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                obj.insert(field.to_string(), Value::String(val.to_lowercase()));
            }
        }
        validate_message_action(action, MessageValidationMode::Basic)?;
    }
    Ok(())
}

pub(crate) fn validate_action(action: &Value) -> Result<()> {
    validate_tool_action(action)?;
    validate_action_provenance(action)?;
    if action.get("action").and_then(|v| v.as_str()) == Some("plan") {
        let rationale = action
            .get("rationale")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let observation = action
            .get("observation")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let combined = format!("{observation}\n{rationale}");
        let diagnostics_claim = combined.contains("diagnostic");
        let has_source_evidence = combined.contains("read_file")
            || combined.contains("run_command")
            || combined.contains("python")
            || combined.contains("source evidence")
            || combined.contains("current source")
            || combined.contains("verified source");
        if diagnostics_claim && !has_source_evidence {
            bail!(
                "plan actions derived from diagnostics must cite same-cycle source evidence in observation/rationale (for example read_file, run_command, python, or verified current source evidence)"
            );
        }
    }
    if action.get("action").and_then(|v| v.as_str()) == Some("message") {
        validate_message_action(action, MessageValidationMode::Strict)?;
    }
    Ok(())
}

pub(crate) fn is_explicit_idle_action(action: &Value) -> bool {
    if action.get("action").and_then(|v| v.as_str()) != Some("run_command") {
        return false;
    }
    let cmd = action
        .get("cmd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    matches!(cmd, "echo idle" | "echo \"idle\"" | "true" | ":")
}

enum NextActionHint {
    GraphFollowups,
    UseApplyPatch,
    ReuseRecent { action: String },
    ChooseAction { last_action: Option<String> },
}

fn derive_next_action_hint(result: &str, last_action: Option<&str>) -> NextActionHint {
    let lowered = result.to_lowercase();
    if lowered.contains("python write denied")
        || lowered.contains("permission denied")
        || lowered.contains("errno 13")
    {
        return NextActionHint::UseApplyPatch;
    }
    if result.contains("graph_probe ok") {
        return NextActionHint::GraphFollowups;
    }
    if let Some(action) = last_action.map(str::trim).filter(|s| !s.is_empty()) {
        return NextActionHint::ReuseRecent {
            action: action.to_string(),
        };
    }
    NextActionHint::ChooseAction {
        last_action: last_action.map(|s| s.to_string()),
    }
}

fn next_action_hint_text(result: &str, last_action: Option<&str>) -> String {
    let all_actions = crate::tool_schema::predicted_action_name_list().join(", ");
    match derive_next_action_hint(result, last_action) {
        NextActionHint::GraphFollowups => {
            "next_action_hint: run graph_call, graph_cfg, graph_reachability".to_string()
        }
        NextActionHint::UseApplyPatch => {
            "next_action_hint: use apply_patch to update workspace files (`src/` or lane plans) if python cannot write.".to_string()
        }
        NextActionHint::ReuseRecent { action } => {
            format!("next_action_hint: reuse recent action `{action}` or choose one of: {all_actions}.")
        }
        NextActionHint::ChooseAction { last_action } => {
            if let Some(action) = last_action {
                format!("next_action_hint: choose one of: {all_actions}. recent action: {action}.")
            } else {
                format!("next_action_hint: choose one of: {all_actions}.")
            }
        }
    }
}

pub(crate) fn action_result_prompt(
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    agent_type: &str,
    result: &str,
    last_action: Option<&str>,
    task_id: Option<&str>,
    objective_id: Option<&str>,
    intent: Option<&str>,
    steps_used: Option<usize>,
    predicted_next_actions: Option<&str>,
) -> String {
    let tab_label = tab_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let turn_label = turn_id
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let limit_line = if agent_type.starts_with("EXECUTOR") {
        let remaining = EXECUTOR_STEP_LIMIT.saturating_sub(steps_used.unwrap_or(0));
        format!("Step limit remaining: {remaining}\n")
    } else {
        String::new()
    };
    let predicted_line = match predicted_next_actions {
        Some(p) if !p.is_empty() => {
            let pretty = serde_json::from_str::<serde_json::Value>(p)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| p.to_string());
            format!(
                "Predicted next actions from your last turn:\n```json\n{pretty}\n```\nCompare these against the actual result above before choosing your next action.\n\n"
            )
        }
        _ => {
            "Predicted next actions from your last turn:\nNone.\nCompare these against the actual result above before choosing your next action.\n\n".to_string()
        }
    };
    // Re-inject a single fresh question after each mutating step so the agent
    // is prompted to re-examine its premise mid-turn, not only at turn start.
    // last_action is the action type string (e.g. "apply_patch"), not full JSON.
    let mutating_question = last_action
        .filter(|kind| matches!(*kind, "apply_patch" | "plan" | "objectives" | "issue"))
        .map(|_| {
            let q = crate::structured_questions::select_questions()[0];
            format!("\nBefore your next action, answer this internally: {q}\n")
        })
        .unwrap_or_default();
    let provenance_block = {
        let task = task_id.unwrap_or("(none)");
        let objective = objective_id.unwrap_or("(none)");
        let intent_text = intent.unwrap_or("(none)");
        format!(
            "Action provenance:\n- task_id: {task}\n- objective_id: {objective}\n- intent: {intent_text}\n\n"
        )
    };

    format!(
        "TAB_ID: {tab_label}\nTURN_ID: {turn_label}\nAGENT_TYPE: {agent_type}\n\n{limit_line}{provenance_block}Action result:\n{}\n\n{predicted_line}{}{}\nEmit exactly one action. Think through the decision internally; reveal chain-of-thought.",
        truncate(result, MAX_SNIPPET),
        next_action_hint_text(result, last_action),
        mutating_question,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_inserts_rationale_when_missing() {
        let mut action = json!({
            "action": "read_file",
            "observation": "need context",
            "path": "SPEC.md"
        });
        normalize_action(&mut action).unwrap();
        assert!(
            action
                .get("rationale")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .len()
                > 0
        );
    }

    #[test]
    fn validate_allows_missing_observation() {
        let action = json!({
            "action": "read_file",
            "rationale": "observation optional",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "scan for related guidance"},
                {"action": "apply_patch", "intent": "update the prompt copy"},
                {"action": "run_command", "intent": "run tests if needed"}
            ],
            "path": "SPEC.md"
        });
        assert!(validate_action(&action).is_ok());
    }

    #[test]
    fn validate_requires_predicted_next_actions() {
        let action = json!({
            "action": "read_file",
            "rationale": "missing predicted list should fail",
            "path": "SPEC.md"
        });
        assert!(validate_action(&action).is_err());
    }

    #[test]
    fn validate_rejects_diagnostics_derived_plan_without_source_evidence() {
        let action = json!({
            "action": "plan",
            "op": "set_task_status",
            "task_id": "T26_planner_evidence_enforcement_hook",
            "objective_id": "obj_planner_evidence_enforcement_hook",
            "intent": "Mark the planning task in progress before implementing the next verified step.",
            "status": "in_progress",
            "observation": "Diagnostics reported a planner issue.",
            "rationale": "Update the task based on diagnostics-only planning guidance.",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect source next"},
                {"action": "cargo_test", "intent": "verify after any later patch"}
            ]
        });
        assert!(validate_action(&action).is_err());
    }

    #[test]
    fn validate_allows_diagnostics_derived_plan_with_source_evidence() {
        let action = json!({
            "action": "plan",
            "op": "set_task_status",
            "task_id": "T26_planner_evidence_enforcement_hook",
            "objective_id": "obj_planner_evidence_enforcement_hook",
            "intent": "Advance the verified planner enforcement task after confirming source evidence.",
            "status": "in_progress",
            "observation": "read_file src/app.rs confirmed the planner path and current source evidence supports follow-up work.",
            "rationale": "Diagnostics signal is now backed by same-cycle read_file source evidence, so plan update is justified.",
            "predicted_next_actions": [
                {"action": "apply_patch", "intent": "implement the validated planner guard"},
                {"action": "cargo_test", "intent": "verify the guarded behavior"}
            ]
        });
        assert!(validate_action(&action).is_ok());
    }

    #[test]
    fn validate_requires_provenance_for_verification_actions() {
        let action = json!({
            "action": "cargo_test",
            "crate": "canon-mini-agent",
            "rationale": "Verify the current code after the latest change.",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "Inspect the failing output if tests fail."},
                {"action": "apply_patch", "intent": "Patch the verified defect if the test output identifies a code issue."}
            ]
        });
        let err = validate_action(&action).unwrap_err().to_string();
        assert!(err.contains("task_id"), "unexpected error: {err}");
    }

    #[test]
    fn validate_allows_read_only_actions_without_provenance() {
        let action = json!({
            "action": "read_file",
            "path": "SPEC.md",
            "rationale": "Read the contract before changing code.",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "Continue reading the relevant section."},
                {"action": "apply_patch", "intent": "Patch the code after gathering enough context."}
            ]
        });
        assert!(validate_action(&action).is_ok());
    }

    #[test]
    fn validate_accepts_symbol_refs_action() {
        let action = json!({
            "action": "symbol_refs",
            "crate": "canon_mini_agent",
            "symbol": "tools::execute_logged_action",
            "rationale": "Find all call sites before changing this function.",
            "predicted_next_actions": [
                {"action": "read_file", "intent": "Inspect the highest-impact call sites in source."},
                {"action": "symbol_path", "intent": "Trace a concrete caller-to-callee route."}
            ]
        });
        assert!(validate_action(&action).is_ok());
    }

    #[test]
    fn validate_accepts_semantic_actions_in_predicted_next_actions() {
        let action = json!({
            "action": "read_file",
            "path": "src/tools.rs",
            "rationale": "Open dispatcher source before making changes.",
            "predicted_next_actions": [
                {"action": "semantic_map", "intent": "Get symbol outline for the tools module."},
                {"action": "symbol_window", "intent": "Read the exact target function body."},
                {"action": "symbol_refs", "intent": "Collect all reference sites before edits."}
            ]
        });
        assert!(validate_action(&action).is_ok());
    }

    #[test]
    fn planner_requires_plan_action_for_master_plan_edits() {
        let rules = PLANNER_RULES.join("\n");
        assert!(
            rules.contains("Use the `plan` action for all `PLAN.json` edits"),
            "planner rules must require plan tool for PLAN.json edits"
        );
    }

    #[test]
    fn planner_rules_require_promoting_issues_into_objectives_and_plan() {
        let rules = PLANNER_RULES.join("\n");
        assert!(
            rules.contains("Read `ISSUES.json` every cycle"),
            "planner rules must require consuming ISSUES.json"
        );
        assert!(
            rules.contains("promote top open issues into `PLANS/OBJECTIVES.json` and `PLAN.json`"),
            "planner rules must require promoting issues into objectives/plan"
        );
    }

    #[test]
    fn verifier_requires_plan_action_for_master_plan_edits() {
        let rules = VERIFIER_RULES.join("\n");
        assert!(
            rules.contains("Update `PLAN.json` only via the `plan` action"),
            "verifier rules must require plan tool for PLAN.json edits"
        );
    }

    #[test]
    fn solo_rules_require_plan_action_for_master_plan_edits() {
        let rules = SOLO_EXECUTION_DISCIPLINE_BULLETS.join("\n");
        assert!(
            rules.contains("Use the `plan` action for `PLAN.json` edits"),
            "solo rules must require plan tool for PLAN.json edits"
        );
    }

    #[test]
    fn solo_prompt_mentions_plan_tool_for_master_plan() {
        let prompt = single_role_solo_prompt(
            "{spec}",
            "{master_plan}",
            "{objectives}",
            "{lessons}",
            "{invariants}",
            "{violations}",
            "{diagnostics}",
            "{cargo_test_failures}",
            "",
            "",
            "",
            "",
            "",
        );
        assert!(
            prompt.contains("Use the `plan` action for `PLAN.json` edits"),
            "solo prompt must direct plan tool usage for PLAN.json"
        );
    }

    #[test]
    fn solo_prompt_includes_lessons_artifact_section() {
        let prompt = single_role_solo_prompt(
            "{spec}",
            "{master_plan}",
            "{objectives}",
            "LESSON_TEXT",
            "{invariants}",
            "{violations}",
            "{diagnostics}",
            "{cargo_test_failures}",
            "",
            "",
            "",
            "",
            "",
        );
        assert!(
            prompt.contains("Lessons artifact:\nLESSON_TEXT"),
            "solo prompt must embed the lessons artifact body"
        );
    }

    #[test]
    fn diagnostics_python_reads_event_logs_accepts_generic_state_and_log_discovery() {
        let state_action = json!({
            "action": "python",
            "code": "from pathlib import Path\nfor root in [Path('state')]:\n    if root.exists():\n        for path in root.rglob('*'):\n            print(path)",
            "rationale": "Inspect workspace-local state artifacts."
        });
        assert!(diagnostics_python_reads_event_logs(&state_action));

        let log_action = json!({
            "action": "python",
            "code": "from pathlib import Path\nfor root in [Path(\"log\")]:\n    if root.exists():\n        for path in root.rglob('*'):\n            print(path)",
            "rationale": "Inspect workspace-local log artifacts."
        });
        assert!(diagnostics_python_reads_event_logs(&log_action));

        let logs_action = json!({
            "action": "python",
            "code": "from pathlib import Path\nfor root in [Path('logs')]:\n    if root.exists():\n        for path in root.rglob('*'):\n            print(path)",
            "rationale": "Inspect workspace-local logs artifacts."
        });
        assert!(diagnostics_python_reads_event_logs(&logs_action));

        let agent_state_action = json!({
            "action": "python",
            "code": "from pathlib import Path\nroot = Path('agent_state')\nprint(root)\nfor path in root.rglob('*.jsonl'):\n    print(path)",
            "rationale": "Inspect workspace-local agent_state artifacts."
        });
        assert!(diagnostics_python_reads_event_logs(&agent_state_action));
    }

    #[test]
    fn diagnostics_python_reads_event_logs_does_not_require_canon_specific_path() {
        let helper_source = include_str!("prompts.rs");
        let helper_start = helper_source
            .find("pub(crate) fn diagnostics_python_reads_event_logs")
            .expect("missing helper");
        let helper_source = &helper_source[helper_start..];
        let helper_end = helper_source
            .find("pub(crate) fn action_rationale")
            .expect("missing helper end anchor");
        let helper_body = &helper_source[..helper_end];
        assert!(!helper_body.contains("state/event_log/event.tlog.d"));

        let generic_action = json!({
            "action": "python",
            "code": "from pathlib import Path\nroot = Path('state')\nprint(root)\nfor path in root.rglob('*'):\n    print(path)",
            "rationale": "Inspect generic workspace-local state artifacts."
        });
        assert!(diagnostics_python_reads_event_logs(&generic_action));
    }

    #[test]
    fn planner_prompt_marks_stale_diagnostics_non_actionable_and_repairs_them() {
        let prompt = single_role_planner_prompt(
            "{spec}",
            "{objectives}",
            "{lessons}",
            "{invariants}",
            "{violations}",
            "{diagnostics}",
            "{issues}",
            "{cargo_test_failures}",
        );
        assert!(
            prompt.contains("Treat stale or already-resolved diagnostics as non-actionable until current source evidence reconfirms them."),
            "planner prompt must keep stale diagnostics non-actionable"
        );
        assert!(
            prompt.contains("If diagnostics repeatedly report stale issues, create follow-up work to repair diagnostics generation rather than reopening resolved implementation tasks."),
            "planner prompt must direct diagnostics-repair follow-up for stale reports"
        );
    }

    #[test]
    fn planner_prompt_includes_lessons_artifact_section() {
        let prompt = single_role_planner_prompt(
            "{spec}",
            "{objectives}",
            "LESSON_TEXT",
            "{invariants}",
            "{violations}",
            "{diagnostics}",
            "{issues}",
            "{cargo_test_failures}",
        );
        assert!(
            prompt.contains("Lessons artifact:\nLESSON_TEXT"),
            "planner prompt must embed the lessons artifact body"
        );
    }
}

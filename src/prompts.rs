use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::constants::{
    diagnostics_file, workspace, CANONICAL_LAW_FILE, EXECUTOR_STEP_LIMIT, INVARIANTS_FILE,
    MASTER_PLAN_FILE, MAX_SNIPPET, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::protocol::{MessagePayload, MessageStatus, MessageType, ProtocolMessage, Role};

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolPromptKind {
    ListDir,
    ReadFile,
    ApplyPatch,
    RunCommand,
    Python,
    CargoTest,
    Message,
}

fn available_actions(kind: AgentPromptKind) -> &'static [&'static str] {
    match kind {
        AgentPromptKind::Verifier => &[
            "message",
            "list_dir",
            "read_file",
            "apply_patch",
            "run_command",
            "python",
            "cargo_test",
        ],
        AgentPromptKind::Executor | AgentPromptKind::Planner | AgentPromptKind::Diagnostics => &[
            "message",
            "list_dir",
            "read_file",
            "apply_patch",
            "run_command",
            "python",
            "cargo_test",
            "graph_call",
            "graph_cfg",
            "graph_dataflow",
            "graph_reachability",
        ],
    }
}

fn tool_order(kind: AgentPromptKind) -> &'static [ToolPromptKind] {
    match kind {
        AgentPromptKind::Diagnostics => &[
            ToolPromptKind::ListDir,
            ToolPromptKind::ReadFile,
            ToolPromptKind::Python,
            ToolPromptKind::RunCommand,
            ToolPromptKind::ApplyPatch,
            ToolPromptKind::CargoTest,
            ToolPromptKind::Message,
        ],
        AgentPromptKind::Verifier => &[
            ToolPromptKind::ListDir,
            ToolPromptKind::ReadFile,
            ToolPromptKind::ApplyPatch,
            ToolPromptKind::RunCommand,
            ToolPromptKind::Python,
            ToolPromptKind::CargoTest,
            ToolPromptKind::Message,
        ],
        AgentPromptKind::Executor | AgentPromptKind::Planner => &[
            ToolPromptKind::ListDir,
            ToolPromptKind::ReadFile,
            ToolPromptKind::ApplyPatch,
            ToolPromptKind::RunCommand,
            ToolPromptKind::Python,
            ToolPromptKind::CargoTest,
            ToolPromptKind::Message,
        ],
    }
}

fn tool_title(kind: AgentPromptKind, tool: ToolPromptKind) -> &'static str {
    match (kind, tool) {
        (_, ToolPromptKind::ListDir) => "list_dir — inspect directory contents",
        (_, ToolPromptKind::ReadFile) => {
            "read_file — read a file; output is line-numbered (\"42: code here\")"
        }
        (AgentPromptKind::Verifier, ToolPromptKind::ApplyPatch) => {
            "apply_patch — update `PLAN.json` status or write `VIOLATIONS.json`"
        }
        (AgentPromptKind::Planner, ToolPromptKind::ApplyPatch) => {
            "apply_patch — update `PLAN.json`"
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::ApplyPatch) => {
            "apply_patch — write the diagnostics report"
        }
        (_, ToolPromptKind::ApplyPatch) => "apply_patch — create or update files",
        (_, ToolPromptKind::RunCommand) => {
            "run_command — run shell commands for discovery or verification"
        }
        (_, ToolPromptKind::Python) => "python — run Python analysis inside the workspace",
        (_, ToolPromptKind::CargoTest) => "cargo_test — run a targeted cargo test (harness-style)",
        (_, ToolPromptKind::Message) => "message — send inter-agent protocol message",
    }
}

const READ_FILE_FOOTER: &str = "   With \"line\":N the output starts at line N and shows up to 300 lines.\n   ⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.\n   ⚠ read_file output is prefixed with line numbers (\"42: code here\"). Strip the \"N: \" prefix when\n     writing patch lines — patch lines must contain ONLY the raw source text, never \"42: code here\".\n     WRONG:  -42: fn old() {}   RIGHT:  -fn old() {}";

const READ_FILE_EXECUTOR_FOOTER: &str = "   With \"line\":N the output starts at line N and shows up to 300 lines.\n   ⚠ Always read a file before patching it. Never patch from memory.\n   ⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.\n   ⚠ read_file output is prefixed with line numbers (\"42: code here\"). Strip the \"N: \" prefix when\n     writing patch lines — patch lines must contain ONLY the raw source text, never \"42: code here\".\n     WRONG:  -42: fn old() {}   RIGHT:  -fn old() {}";

const RUN_COMMAND_FOOTER: &str =
    "   ⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.";
const PYTHON_FOOTER: &str = "   ⚠ cwd may be relative to WORKSPACE or absolute under WORKSPACE.";

fn tool_prompt(kind: AgentPromptKind, tool: ToolPromptKind) -> String {
    let ws = crate::constants::workspace();
    match (kind, tool) {
        (AgentPromptKind::Executor, ToolPromptKind::ListDir) => {
            "   {\"action\":\"list_dir\",\"path\":\"canon-utils\",\"rationale\":\"Inspect the workspace before making assumptions.\"}".to_string()
        }
        (AgentPromptKind::Planner, ToolPromptKind::ListDir) => {
            "   {\"action\":\"list_dir\",\"path\":\"canon-utils\",\"rationale\":\"Inspect the relevant code area before expanding tasks.\"}".to_string()
        }
        (AgentPromptKind::Verifier, ToolPromptKind::ListDir) => {
            "   {\"action\":\"list_dir\",\"path\":\"canon-utils\",\"rationale\":\"Inspect the relevant area before verifying claims about it.\"}".to_string()
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::ListDir) => {
            "   {\"action\":\"list_dir\",\"path\":\"state/event_log/event.tlog.d\",\"rationale\":\"Inspect the available event-log segments before diagnosing failures.\"}\n   {\"action\":\"list_dir\",\"path\":\"canon-utils\",\"rationale\":\"Inspect the project layout before targeting diagnostics.\"}".to_string()
        }

        (AgentPromptKind::Executor, ToolPromptKind::ReadFile) => {
            format!(
                "   {{\"action\":\"read_file\",\"path\":\"canon-utils/some-crate/src/lib.rs\",\"rationale\":\"Read the file before editing it.\"}}\n   {{\"action\":\"read_file\",\"path\":\"canon-utils/some-crate/src/lib.rs\",\"line\":120,\"rationale\":\"Read the relevant section before editing it.\"}}\n{READ_FILE_EXECUTOR_FOOTER}"
            )
        }
        (AgentPromptKind::Planner, ToolPromptKind::ReadFile) => {
            format!(
                "   {{\"action\":\"read_file\",\"path\":\"canon-utils/some-crate/src/lib.rs\",\"rationale\":\"Read the source before deriving actionable plan steps.\"}}\n   {{\"action\":\"read_file\",\"path\":\"canon-utils/some-crate/src/lib.rs\",\"line\":120,\"rationale\":\"Read the relevant source section before deriving actionable plan steps.\"}}\n{READ_FILE_FOOTER}"
            )
        }
        (AgentPromptKind::Verifier, ToolPromptKind::ReadFile) => {
            format!(
                "   {{\"action\":\"read_file\",\"path\":\"canon-utils/some-crate/src/lib.rs\",\"rationale\":\"Read the source to verify whether the claimed change exists.\"}}\n   {{\"action\":\"read_file\",\"path\":\"canon-utils/some-crate/src/lib.rs\",\"line\":120,\"rationale\":\"Jump to the relevant section to verify the claimed change.\"}}\n{READ_FILE_FOOTER}"
            )
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::ReadFile) => {
            "   {\"action\":\"read_file\",\"path\":\"canon-utils/canon-route/src/policy.rs\",\"line\":1,\"rationale\":\"Read a suspected source file to correlate code with observed failures.\"}\n   ⚠ Paths may be relative to WORKSPACE or absolute under WORKSPACE.".to_string()
        }

        (AgentPromptKind::Executor, ToolPromptKind::ApplyPatch) => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: path/to/new.rs\\n+line one\\n+line two\\n*** End Patch\",\"rationale\":\"Apply the concrete code change after reading the target context.\"}\n\n   To UPDATE an existing file, each @@ hunk needs 3 unchanged context lines around the change:\n   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: src/lib.rs\\n@@\\n fn before_before() {}\\n fn before() {}\\n fn target() {\\n-    old_body();\\n+    new_body();\\n }\\n fn after() {}\\n*** End Patch\",\"rationale\":\"Update the file using exact surrounding context from the read.\"}\n\n   To REPLACE most or all of a file use Delete + Add, never a giant @@ block:\n   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Delete File: PLANS/executor-b.json\\n*** Add File: PLANS/executor-b.json\\n+# new content\\n+line two\\n*** End Patch\",\"rationale\":\"Full-file replacement is safer than a giant hunk with many - lines.\"}\n\n   WRONG — removing many lines with @@ causes anchor-miss failures:\n   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: PLANS/executor-b.json\\n@@\\n-line one\\n-line two\\n-line three\\n+replacement\\n*** End Patch\",\"rationale\":\"Bad: too many - lines from memory, anchor will miss if file differs by even one char.\"}\n\n   Rules:\n   - Every @@ hunk must have AT LEAST 3 unchanged context lines (space-prefixed) around the edit.\n   - Never use @@ with only 1 context line — the patcher will fail to locate the anchor.\n   - ALL - lines must be copied CHARACTER-FOR-CHARACTER from read_file output (minus the \\\"N: \\\" prefix). Never write - lines from memory.\n   - If replacing more than ~10 lines, use *** Delete File + *** Add File instead of a large @@ hunk.\n   - *** Add File for new files, *** Update File for existing files.\n   - NEVER use absolute paths inside the patch string.".to_string()
        }
        (AgentPromptKind::Planner, ToolPromptKind::ApplyPatch) => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Update File: PLAN.json\\n@@\\n line_before_before\\n line_before\\n - [ ] task to expand\\n+  1. sub-step one\\n+  2. sub-step two\\n line_after\\n line_after_after\\n*** End Patch\",\"rationale\":\"Refresh the master plan so priorities and dependencies are explicit.\"}\n\n   Rules:\n   - Every @@ hunk needs AT LEAST 3 unchanged context lines (space-prefixed) around the change.\n   - NEVER chain multiple @@ blocks with only 1 context line each — every anchor needs 3 lines.\n   - WRONG: @@\\n - [ ] task\\n+  1. sub-step\\n@@\\n - [ ] task2\\n+  1. sub-step\n   - RIGHT: @@\\n prev_line\\n prev_line2\\n - [ ] task\\n+  1. sub-step\\n next_line\\n next_line2".to_string()
        }
        (AgentPromptKind::Verifier, ToolPromptKind::ApplyPatch) => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: VIOLATIONS.json\\n+{\\n+  \\\"status\\\": \\\"failed\\\",\\n+  \\\"summary\\\": \\\"Short summary\\\",\\n+  \\\"violations\\\": [\\n+    {\\n+      \\\"id\\\": \\\"V1\\\",\\n+      \\\"title\\\": \\\"Control flow gated by executor-local state\\\",\\n+      \\\"severity\\\": \\\"critical\\\",\\n+      \\\"evidence\\\": [\\\"executor.rs:56-61 dispatch_in_progress gate\\\"],\\n+      \\\"issue\\\": \\\"Route dispatch suppressed before semantic evaluation\\\",\\n+      \\\"impact\\\": \\\"RouteTick does not guarantee dispatch\\\",\\n+      \\\"required_fix\\\": [\\\"Remove dispatch_in_progress gating\\\"],\\n+      \\\"files\\\": [\\\"canon-utils/canon-route/src/executor.rs\\\"]\\n+    }\\n+  ]\\n+}\\n*** End Patch\",\"rationale\":\"Record spec violations discovered during verification.\"}".to_string()
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::ApplyPatch) => {
            "   {\"action\":\"apply_patch\",\"patch\":\"*** Begin Patch\\n*** Add File: DIAGNOSTICS.json\\n+{\\n+  \\\"status\\\": \\\"critical_failure\\\",\\n+  \\\"inputs_scanned\\\": [\\\"state/event_log/event.tlog.d\\\", \\\"VIOLATIONS.json\\\"],\\n+  \\\"ranked_failures\\\": [\\n+    {\\n+      \\\"id\\\": \\\"D1\\\",\\n+      \\\"impact\\\": \\\"critical\\\",\\n+      \\\"signal\\\": \\\"No control events in canonical log\\\",\\n+      \\\"evidence\\\": [\\\"Tick=0, RouteTick=0 in 20 segments\\\"],\\n+      \\\"root_cause\\\": \\\"emit_tick not executed or not persisted\\\",\\n+      \\\"repair_targets\\\": [\\\"canon-runtime/src/lib.rs::emit_tick\\\"]\\n+    }\\n+  ],\\n+  \\\"planner_handoff\\\": [\\\"Restore emit_tick loop execution\\\"]\\n+}\\n*** End Patch\",\"rationale\":\"Write the ranked diagnostics report after collecting evidence from logs and code.\"}".to_string()
        }

        (AgentPromptKind::Executor, ToolPromptKind::RunCommand) => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"cargo check -p some-crate\",\"cwd\":\"{ws}\",\"rationale\":\"Validate the target crate after a change.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo' canon-utils/some-crate/src/\",\"cwd\":\"{ws}\",\"rationale\":\"Search the codebase for the relevant symbol before editing.\"}}\n{RUN_COMMAND_FOOTER}")
        }
        (AgentPromptKind::Planner, ToolPromptKind::RunCommand) => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo'\",\"cwd\":\"{ws}\",\"rationale\":\"Search for implementation details needed to expand the plan accurately.\"}}\n{RUN_COMMAND_FOOTER}")
        }
        (AgentPromptKind::Verifier, ToolPromptKind::RunCommand) => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"cargo check -p some-crate\",\"cwd\":\"{ws}\",\"rationale\":\"Validate the crate implicated by the completed task.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"cargo test -q --workspace\",\"cwd\":\"{ws}\",\"rationale\":\"Verify the claimed completion does not break workspace tests.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"rg -n 'fn foo'\",\"cwd\":\"{ws}\",\"rationale\":\"Find the implementation or call sites mentioned by the completed task.\"}}\n{RUN_COMMAND_FOOTER}")
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::RunCommand) => {
            format!("   {{\"action\":\"run_command\",\"cmd\":\"rg -n \\\"invariant|panic|TODO|unreachable!|assert!\\\" canon-utils state\",\"cwd\":\"{ws}\",\"rationale\":\"Search the codebase and state for likely failure markers.\"}}\n   {{\"action\":\"run_command\",\"cmd\":\"cargo check --workspace\",\"cwd\":\"{ws}\",\"rationale\":\"Detect compiler-visible inconsistencies that belong in diagnostics.\"}}\n{RUN_COMMAND_FOOTER}")
        }

        (AgentPromptKind::Executor, ToolPromptKind::Python) => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(len(list(Path('canon-utils').glob('**/*.rs'))))\",\"cwd\":\"{ws}\",\"rationale\":\"Use Python for structured workspace analysis.\"}}\n{PYTHON_FOOTER}"
            )
        }
        (AgentPromptKind::Planner, ToolPromptKind::Python) => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(sum(1 for _ in Path('canon-utils').glob('**/*.rs')))\",\"cwd\":\"{ws}\",\"rationale\":\"Use Python to gather structured planning context from the workspace.\"}}\n{PYTHON_FOOTER}"
            )
        }
        (AgentPromptKind::Verifier, ToolPromptKind::Python) => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nprint(Path('SPEC.md').exists())\",\"cwd\":\"{ws}\",\"rationale\":\"Use Python when structured verification logic is easier than shell commands.\"}}\n{PYTHON_FOOTER}"
            )
        }
        (AgentPromptKind::Diagnostics, ToolPromptKind::Python) => {
            format!(
                "   {{\"action\":\"python\",\"code\":\"from pathlib import Path\\nroot = Path('{ws}/state/event_log/event.tlog.d')\\nfor path in sorted(root.glob('*.log')):\\n    print(path.name, path.stat().st_size)\",\"cwd\":\"{ws}\",\"rationale\":\"Analyze the event-source logs to find failure signals and inconsistencies.\"}}\n{PYTHON_FOOTER}"
            )
        }
        (AgentPromptKind::Executor, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Planner, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Verifier, ToolPromptKind::CargoTest)
        | (AgentPromptKind::Diagnostics, ToolPromptKind::CargoTest) => {
            "   {\"action\":\"cargo_test\",\"crate\":\"canon-runtime\",\"test\":\"some_test_name\",\"rationale\":\"Run the exact failing test using the harness-style command.\"}".to_string()
        }

        (_, ToolPromptKind::Message) => {
            "   {\"action\":\"message\",\"from\":\"executor\",\"to\":\"verifier\",\"type\":\"handoff\",\"status\":\"complete\",\"payload\":{\"summary\":\"brief evidence summary\",\"artifacts\":[\"path/to/file.rs\"]},\"rationale\":\"Execution work is complete and the verifier now has enough evidence to judge it.\"}\n   {\"action\":\"message\",\"from\":\"executor\",\"to\":\"planner\",\"type\":\"blocker\",\"status\":\"blocked\",\"observation\":\"Describe the blocker.\",\"rationale\":\"Explain why progress is impossible.\",\"payload\":{\"summary\":\"Short blocker summary\",\"blocker\":\"Root cause\",\"evidence\":\"Concrete error text\",\"required_action\":\"What must be done to unblock\",\"severity\":\"error\"}}\n   Allowed roles: executor|planner|verifier|diagnostics. Allowed types: handoff|result|verification|failure|blocker|plan|diagnostics. Allowed status: complete|in_progress|failed|verified|ready|blocked.\n   ⚠ message with status=complete is REJECTED if build or tests fail — fix all errors first.".to_string()
        }
    }
}

fn prompt_intro(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "You are the canon executor agent.",
        AgentPromptKind::Verifier => "You are the canon verifier agent.",
        AgentPromptKind::Planner => "You are the canon planner agent.",
        AgentPromptKind::Diagnostics => "You are the canon diagnostics agent.",
    }
}

fn prompt_mission(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "Your job is to execute the highest-priority READY work described in planner handoff messages and the master plan.\n`SPEC.md` is the canonical contract.\nLane plans are deprecated and should not be relied on for task selection.\nThe verifier judges code against `SPEC.md`.\nYou should only work on the top 1-10 ready tasks in the current cycle, then yield.\nDo not reorganize or update `SPEC.md` or plan files yourself.\nMake source changes, run checks, and report evidence in `message.payload`.",
        AgentPromptKind::Verifier => "Your job is to critically review executor evidence against the codebase and judge whether the implementation satisfies `SPEC.md`.\nExecutor evidence is a hint only. The canonical truth is the codebase versus `SPEC.md`.\nIf violations are found, write `VIOLATIONS.json` with a clear, actionable list using the enums in canon-mini-agent/src/reports.rs.\nBe skeptical — do not trust executor claims at face value.",
        AgentPromptKind::Planner => "Your job is to read `SPEC.md`, `PLANS/OBJECTIVES.md`, `VIOLATIONS.json`, and `DIAGNOSTICS.json` and derive the master plan plus executor handoff guidance.\nYou own priority, dependency ordering, task allocation, and the ready-work window for each executor.\nOn every cycle, re-evaluate the workspace and rewrite `PLAN.json` so each executor only needs to perform the top 1-10 ready tasks, then send handoff messages to executors.\nPlans must follow the JSON PLAN/TASK protocol in `SPEC.md`.",
        AgentPromptKind::Diagnostics => "Your job is to scan the canon project state, analyze `VIOLATIONS.json`, detect root causes, rank them by impact, and write concrete repair targets for the planner in `DIAGNOSTICS.json` using the enums in canon-mini-agent/src/reports.rs.",
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
        AgentPromptKind::Planner => format!("You work inside the canon workspace at {ws}. Use bash, rg, read_file, python, and diagnostics evidence to review the current project state before reorganizing the plan."),
        AgentPromptKind::Diagnostics => format!("You must inspect both:\n- the project source tree under {ws}\n- the event log segments under {ws}/state/event_log/event.tlog.d"),
    }
}

fn action_contract(kind: AgentPromptKind) -> String {
    let actions = available_actions(kind)
        .iter()
        .map(|action| format!("- `{action}`"))
        .collect::<Vec<_>>()
        .join("\n");
    let graph_hint = "Graph tools hint: artifacts come from rustc wrapper capture (run `cargo build -p <crate>`). `graph_probe` inspects symbols/coverage; `graph_call`/`graph_cfg` emit CSVs; `graph_dataflow`/`graph_reachability` emit reports.";
    let example = if kind == AgentPromptKind::Planner {
        ""
    } else {
        "\n\n```json\n{ \"observation\": \"Saw file list for canon-utils\", \"action\": \"list_dir\", \"path\": \"canon-utils\", \"rationale\": \"Inspect the workspace before making assumptions.\" }\n```"
    };
    format!(
        "Each turn you receive either:\n  (a) the initial instruction; or\n  (b) the result of your last action.\n\nYou respond with exactly one action per turn, as a single JSON object wrapped in a `json` code block.\nAvailable actions:\n{actions}\n{graph_hint}\nEvery action MUST include:\n- `observation`: what you can see purely from evidence only, as a single string\n- `rationale`: why this is the next best step\n\nDo NOT include any extra text outside the JSON code block.\nDo NOT echo the tools list or the prompt.\nDo NOT use placeholder action names like `...`; choose a real action from the list.{example}"
    )
}

fn tools_section(kind: AgentPromptKind) -> String {
    let mut out =
        String::from("━━━ TOOLS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
    for (idx, tool) in tool_order(kind).iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n{}\n\n",
            idx + 1,
            tool_title(kind, *tool),
            tool_prompt(kind, *tool)
        ));
    }
    out.trim_end().to_string()
}

fn rules_common_footer() -> String {
    let agent_source = crate::constants::agent_state_dir().trim_end_matches("/agent_state");
    let protect_rule = if crate::constants::workspace() != agent_source {
        format!("- Never modify the canon-mini-agent source tree ({agent_source}).\n")
    } else {
        String::new()
    };
    format!(
        "{protect_rule}- Emit exactly one action per turn.\n- If you cannot proceed (missing files/permissions, repeated tool errors, or irreconcilable evidence), emit a `message` with `type=blocker`, `status=blocked`, and payload fields `blocker`, `evidence`, `required_action`.\n- Output format: exactly one JSON object in a ```json code block. No prose outside it."
    )
}

fn rules_blocker_route(target: &str) -> String {
    format!("- If blocked, send the blocker to the {target}.")
}

fn rules_section(rules: &[&str], blocker_target: Option<&str>) -> String {
    let mut out = String::from("━━━ RULES ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n");
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

const VERIFIER_PROCESS: &str = "━━━ VERIFICATION PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nFor each executor claim:\n1. Use the executor result summary plus `SPEC.md` to derive the candidate obligations.\n2. Read the relevant source files to confirm the described change exists.\n3. Run cargo check or cargo test if the task involves code correctness.\n4. Judge whether the code satisfies the spec.\n5. If violations are found, write `VIOLATIONS.json` with a clear, actionable list using the enums in canon-mini-agent/src/reports.rs.\n6. Update task `status` fields in `PLAN.json` to reflect verified results (ready/in_progress/done/blocked) and update any related `next_on_success` / `next_on_failure` as needed.\n7. Report a verification breakdown in `message.payload` (verified, unverified, false) with explicit items.\n8. For any control-flow or state-management claim, verify that the described behavior matches the source code and is consistent with INVARIANTS.json.";

const PLANNER_PROCESS: &str = "━━━ PLANNING PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nOn every planning cycle:\n1. Read `SPEC.md`, `VIOLATIONS.json`, `DIAGNOSTICS.json`, relevant source files, and recent workspace state to understand what changed.\n2. Update `PLAN.json` as the master plan and derive the ready-work window for each executor.\n3. Maintain a READY NOW window containing at most 1-10 executable tasks for each executor.\n4. Move blocked work behind its dependencies instead of leaving it in the ready window.\n5. Rewrite priorities whenever new evidence changes the critical path.\n6. If canonical-law authority (INVARIANTS.json, CANONICAL_LAW.md) conflicts with local heuristics in the plan, prioritize canonical-law authority and move heuristic cleanup behind it as follow-on work.\n7. Write detailed, imperative tasks that include file paths and concrete actions (read/patch/test).\n8. Send handoff messages to executors reflecting the updated ready window.";

fn diagnostics_process() -> String {
    format!("━━━ DIAGNOSTICS PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nGather evidence from the workspace, `VIOLATIONS.json`, and the current codebase, then write DIAGNOSTICS.json using the enums in canon-mini-agent/src/reports.rs.\nRules:\n- Use the `python` action for structured analysis of project state and any available logs.\n- Only modify DIAGNOSTICS.json.\n- Rank issues by impact on correctness, convergence, and repairability.\n- Check whether control-flow decisions are consistent with the canonical law in CANONICAL_LAW.md and the invariants in INVARIANTS.json.\n- Before trusting any trace or log file, confirm it was updated in the current cycle (mtime, size change, or fresh producer command).\n- Treat empty `rg` / `grep` results as ambiguous: no match, stale file, or incomplete write are all possible.\n- Prefer the most recently written evidence sources over ad-hoc temp traces when they disagree.")
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
    format_bullets("Execution discipline:\n", EXECUTION_DISCIPLINE_BULLETS, None)
}

const VERIFIER_RULES: &[&str] = &[
    "- Be critical and thorough — verify evidence, not just the claim.",
    "- Do not mark anything verified unless you have read the actual code and run verification commands.",
    "- You must run `run_command` (and `cargo_test` when relevant) to validate executor claims; do not accept evidence without running checks yourself.",
    "- Run `cargo build --workspace` before completing the cycle; fix failures before `message` with status=complete.",
    "- Only modify `PLAN.json` and `VIOLATIONS.json` — never edit `SPEC.md` or source files.",
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
    "- Prefer `python` to update `PLAN.json`; it is more reliable and efficient than `apply_patch` for structured JSON edits.",
    "- `PLAN.json` MUST be valid JSON following the PLAN/TASK protocol in `SPEC.md`.",
    "- Only modify `PLAN.json` — never edit source files or `SPEC.md`.",
    "- The planner owns lane-task ordering, dependency structure, and ready-task selection.",
    "- Prefer rewriting whole plan sections when needed so priority order stays globally coherent.",
    "- Keep each executor's ready window small: 1-10 tasks maximum.",
    "- Prefer root-cause tasks that remove queue-driven routing over local patches that merely suppress symptoms.",
    "- Send handoff messages to executors reflecting the current ready window.",
];

fn diagnostics_rules() -> Vec<String> {
    vec![
        "- Use the `python` action for structured analysis of project state and any available logs.".to_string(),
        "- Only modify DIAGNOSTICS.json.".to_string(),
        "- Rank issues by impact on correctness, convergence, and repairability.".to_string(),
        "- Check control-flow and state-management decisions against CANONICAL_LAW.md and INVARIANTS.json.".to_string(),
        "- Before trusting any trace or log file, confirm it was updated in the current cycle (mtime, size change, or fresh producer command).".to_string(),
        "- Treat empty `rg` / `grep` results as ambiguous: no match, stale file, or incomplete write are all possible.".to_string(),
        "- Prefer the most recently written evidence sources over ad-hoc temp traces when they disagree.".to_string(),
    ]
}

fn executor_rules() -> Vec<String> {
    let ws = crate::constants::workspace();
    vec![
        "- Always read a file before patching it.".to_string(),
        "- Use list_dir and read_file freely before assuming project state.".to_string(),
        "- Use run_command for cargo builds, tests, and shell discovery.".to_string(),
        "- If test output is truncated, re-run tests with `cargo test -- --nocapture 2>&1 | tail -n 200` and report the tail in `message.payload`.".to_string(),
        "- Run the runtime to confirm it works before completing the cycle. Use this exact command:".to_string(),
        "  `tmpdir=$(mktemp -d) && mkdir -p \"$tmpdir/event.tlog.d\" && CANON_EVENT_RUNTIME_LOG=\"$tmpdir/event_runtime.log\" timeout 12s target/debug/canon-runtime --tlog \"$tmpdir/event.tlog.d\" --once > /tmp/canon_runtime_check.out 2> /tmp/canon_runtime_check.err; status=$?; echo TMPDIR=\"$tmpdir\"; echo STATUS=\"$status\"; echo \"--- STDOUT ---\"; tail -n 50 /tmp/canon_runtime_check.out; echo \"--- STDERR ---\"; tail -n 100 /tmp/canon_runtime_check.err; echo \"--- FILES ---\"; find \"$tmpdir\" -maxdepth 2 -type f | sort`".to_string(),
        "  Report the command and outcome in `message.payload`.".to_string(),
        "- Use python for structured analysis when shell pipelines are awkward.".to_string(),
        format!("- Never operate outside {ws}."),
        "- Never modify `SPEC.md`, `PLAN.json`, `VIOLATIONS.json`, or `DIAGNOSTICS.json`.".to_string(),
        "- Never emit destructive commands (rm -rf, git reset --hard, git clean -f, etc.).".to_string(),
    ]
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
        AgentPromptKind::Executor => {
            let er = executor_rules();
            let er_refs: Vec<&str> = er.iter().map(|s| s.as_str()).collect();
            format!(
                "{}\n\n{}\n\n{}",
                executor_handoff(),
                execution_discipline(),
                rules_section(&er_refs, Some("Planner"))
            )
        }
        AgentPromptKind::Verifier => format!("{}\n\n{}", VERIFIER_PROCESS, rules_section(VERIFIER_RULES, Some("Planner"))),
        AgentPromptKind::Planner => format!("{}\n\n{}", PLANNER_PROCESS, rules_section(PLANNER_RULES, Some("Diagnostics"))),
        AgentPromptKind::Diagnostics => {
            let dr = diagnostics_rules();
            let dr_refs: Vec<&str> = dr.iter().map(|s| s.as_str()).collect();
            format!("{}\n\n{}", diagnostics_process(), rules_section(&dr_refs, Some("Planner")))
        }
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
    out.push_str(&action_contract(kind));
    if kind != AgentPromptKind::Planner {
        out.push_str("\n\n");
        out.push_str(&tools_section(kind));
        out.push_str("\n\n");
    } else {
        out.push_str("\n\n");
    }
    out.push_str(&prompt_tail(kind));
    out
}

pub(crate) fn planner_cycle_prompt(
    summary_text: &str,
    objectives_text: &str,
    invariants_text: &str,
    violations_text: &str,
    diagnostics_text: &str,
    plan_diff: &str,
    executor_diff: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Objectives: {OBJECTIVES_FILE}\n- Invariants: {INVARIANTS_FILE}\n- Violations: {VIOLATIONS_FILE}\n- Diagnostics: {diagnostics_file}\n- Master plan: {MASTER_PLAN_FILE}\n\nPlan diff (from {MASTER_PLAN_FILE}):\n{plan_diff}\n\nExecutor diff (workspace changes excluding plans/diagnostics/violations):\n{executor_diff}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives_text}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants_text}\n\nViolations (from {VIOLATIONS_FILE}):\n{violations_text}\n\nDiagnostics report (from {diagnostics_file}):\n{diagnostics_text}\n\nLatest verifier summary:\n{summary_text}\n\nYou may send a message action to other agents at any time."
    )
}

pub(crate) fn executor_cycle_prompt(
    _executor_name: &str,
    lane_label: &str,
    latest_verify_result: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    let verify_result = if latest_verify_result.trim().is_empty()
        || latest_verify_result.trim().eq_ignore_ascii_case("shutdown requested")
    {
        "(no verifier result available)".to_string()
    } else {
        latest_verify_result.to_string()
    };
    format!(
        "TAB_ID: pending\nTURN_ID: pending\nAGENT_TYPE: EXECUTOR\n\nWORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Master plan: {MASTER_PLAN_FILE}\n- Violations: {VIOLATIONS_FILE}\n- Diagnostics: {diagnostics_file}\n\nLane plans are deprecated. Use planner handoff messages and {MASTER_PLAN_FILE} for task selection.\nLatest verifier result for lane {lane_label}:\n{verify_result}\n\nYou may send a message action to other agents at any time."
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
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Objectives: {OBJECTIVES_FILE}\n- Invariants: {INVARIANTS_FILE}\n- Master plan: {MASTER_PLAN_FILE}\n- Diagnostics: {diagnostics_file}\n- Violations to write: {VIOLATIONS_FILE}\n\nExecutor diff (workspace changes excluding plans/diagnostics/violations):\n{executor_diff}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nExecutor lane: {lane_label}\nExecutor result summary:\n{exec_result}\n\nYou may send a message action to other agents at any time."
    )
}

pub(crate) fn diagnostics_cycle_prompt(summary_text: &str, cargo_test_failures: &str) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Objectives: {OBJECTIVES_FILE}\n- Invariants: {INVARIANTS_FILE}\n- Violations: {VIOLATIONS_FILE}\n- Diagnostics report to write: {diagnostics_file}\n- Event log directory: state/event_log/event.tlog.d\n\nLatest verifier summary:\n{summary_text}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nYou may send a message action to other agents at any time."
    )
}

pub(crate) fn single_role_verifier_prompt(
    primary_input: &str,
    objectives: &str,
    invariants: &str,
    executor_diff_text: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical spec (from {SPEC_FILE}):\n{primary_input}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}\n\nExecutor diff (workspace changes excluding plans/diagnostics/violations):\n{executor_diff_text}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nVerify that objectives in {OBJECTIVES_FILE} are completed properly.\nUpdate task status fields in {MASTER_PLAN_FILE} to reflect verified results.\nWrite violations to {VIOLATIONS_FILE} if any are found.\nWhen complete, report verified/unverified/false items in `message.payload`.\nEmit exactly one action to begin."
    )
}

pub(crate) fn single_role_diagnostics_prompt(
    violations: &str,
    objectives: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let canonical_law = prompt_canonical_law(AgentPromptKind::Diagnostics);
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nRead files and search the source code for bugs and inconsistencies (use read_file + run_command/ripgrep).\nRun python analysis actions over available logs and code evidence.\nInfer the root cause from the evidence and cite detailed sources of errors (file paths, functions, log evidence).\n\nLatest verifier summary:\n(none yet)\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nVerify whether objectives in {OBJECTIVES_FILE} are being met and note gaps.\nUse {SPEC_FILE}, {OBJECTIVES_FILE}, and {INVARIANTS_FILE} as the contract, not lane plans.\nInfer failures from code, logs, runtime state, and verifier findings.\n\nCanonical law:\n{canonical_law}\n\nWrite a ranked diagnostics report to {diagnostics_path}. Emit exactly one action to begin."
    )
}

pub(crate) fn single_role_planner_prompt(
    primary_input: &str,
    objectives: &str,
    invariants: &str,
    violations: &str,
    diagnostics: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let canonical_law = prompt_canonical_law(AgentPromptKind::Planner);
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical spec (from {SPEC_FILE}):\n{primary_input}\n\nObjectives (from {OBJECTIVES_FILE}):\n{objectives}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nDiagnostics report (from {diagnostics_path}):\n{diagnostics}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nCanonical law:\n{canonical_law}\n\nUse {INVARIANTS_FILE} when deriving plan constraints.\nRead files and search the source code before issuing plan changes.\nWrite imperative, actionable instructions in {MASTER_PLAN_FILE}.\nOnly use plan diffs when available; avoid re-reading the full plan unless necessary.\nEmit exactly one action to begin."
    )
}

pub(crate) fn single_role_executor_prompt(
    spec: &str,
    master_plan: &str,
    violations: &str,
    diagnostics: &str,
    invariants: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical spec (from {SPEC_FILE}):\n{spec}\n\nMaster plan (from {MASTER_PLAN_FILE}):\n{master_plan}\n\nViolations (from {VIOLATIONS_FILE}):\n{violations}\n\nDiagnostics (from {diagnostics_path}):\n{diagnostics}\n\nInvariants (from {INVARIANTS_FILE}):\n{invariants}\n\nLane plans are deprecated. Use planner handoff messages and {MASTER_PLAN_FILE} for task selection.\n\nDo not modify spec, plan, violations, or diagnostics. Use `message.payload` to report evidence for verifier review. Emit exactly one action to begin."
    )
}

// ── Action parsing ─────────────────────────────────────────────────────────────

pub(crate) fn parse_actions(raw: &str) -> Result<Vec<Value>> {
    if let Some(json_text) = extract_json_fence(raw) {
        return parse_json_action(json_text)
            .with_context(|| "fenced json block was not a valid action object");
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

fn extract_json_fence(text: &str) -> Option<&str> {
    let start = text.find("```json").or_else(|| text.find("```JSON"))?;
    let after_newline = start + text[start..].find('\n')?;
    let rest = &text[after_newline + 1..];
    let end = rest.find("```")?;
    Some(rest[..end].trim())
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
    // Accept either a full literal path or path segments built via Path / operator.
    code.contains("state/event_log/event.tlog.d")
        || (code.contains("event_log") && code.contains("event.tlog.d"))
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

enum MessageValidationMode {
    Basic,
    Strict,
}

fn validate_message_action(action: &Value, mode: MessageValidationMode) -> Result<()> {
    let obj = action
        .as_object()
        .ok_or_else(|| anyhow!("action payload must be a JSON object"))?;
    for field in ["from", "to", "type", "status"] {
        obj.get(field)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("message missing non-empty '{field}'"))?;
    }
    obj.get("payload")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("message missing object payload"))?;
    if matches!(mode, MessageValidationMode::Basic) {
        return Ok(());
    }
    let msg: ProtocolMessage = serde_json::from_value(action.clone())
        .map_err(|e| anyhow!("message schema invalid: {e}"))?;
    if matches!(msg.msg_type, MessageType::Blocker) || matches!(msg.status, MessageStatus::Blocked)
    {
        match msg.payload {
            MessagePayload::Blocker(payload) => {
                if payload.blocker.trim().is_empty()
                    || payload.evidence.trim().is_empty()
                    || payload.required_action.trim().is_empty()
                {
                    bail!("blocker payload fields must be non-empty strings");
                }
            }
            _ => bail!("blocker messages must include payload fields: blocker, evidence, required_action"),
        }
    }
    if let Some(severity) = obj.get("severity").and_then(|v| v.as_str()) {
        let _ = serde_json::from_value::<crate::protocol::Severity>(Value::String(
            severity.to_string(),
        ))
        .map_err(|_| anyhow!("message severity must be one of: info|warn|error|critical"))?;
    }
    if let Some(from_role) = obj.get("from_role") {
        let _ = serde_json::from_value::<Role>(from_role.clone()).map_err(|_| {
            anyhow!("from_role must be one of: executor|planner|verifier|diagnostics")
        })?;
    }
    if let Some(to_role) = obj.get("to_role") {
        let _ = serde_json::from_value::<Role>(to_role.clone()).map_err(|_| {
            anyhow!("to_role must be one of: executor|planner|verifier|diagnostics")
        })?;
    }
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
    let obj = action
        .as_object()
        .ok_or_else(|| anyhow!("action payload must be a JSON object"))?;
    let kind = obj
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("action missing 'action'"))?;
    if !is_supported_action(kind) {
        bail!("unsupported action '{kind}'");
    }
    let observation = obj
        .get("observation")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("action missing non-empty 'observation'"))?;
    let rationale = obj
        .get("rationale")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("action missing non-empty 'rationale'"))?;
    let _ = (observation, rationale);
    if kind == "message" {
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
    TailOutputLog { path: String },
    GraphFollowups,
    UseApplyPatch,
    ReuseRecent { action: String },
    ChooseAction { last_action: Option<String> },
}

fn derive_next_action_hint(result: &str, last_action: Option<&str>) -> NextActionHint {
    let lowered = result.to_lowercase();
    if lowered.contains("python write denied") || lowered.contains("permission denied") || lowered.contains("errno 13") {
        return NextActionHint::UseApplyPatch;
    }
    if result.contains("graph_probe ok") {
        return NextActionHint::GraphFollowups;
    }
    if let Some(path) = extract_output_log_path(result) {
        return NextActionHint::TailOutputLog { path };
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
    let all_actions = "list_dir, read_file, apply_patch, run_command, python, cargo_test, rustc_hir, rustc_mir, graph_call, graph_cfg, graph_dataflow, graph_reachability, message";
    match derive_next_action_hint(result, last_action) {
        NextActionHint::TailOutputLog { path } => {
            format!("next_action_hint: run_command tail -n 200 {path}")
        }
        NextActionHint::GraphFollowups => {
            "next_action_hint: run graph_call, graph_cfg, graph_reachability".to_string()
        }
        NextActionHint::UseApplyPatch => {
            "next_action_hint: use apply_patch to update workspace files (PLAN.json) if python cannot write.".to_string()
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

fn extract_output_log_path(out: &str) -> Option<String> {
    let needle = "output_log=";
    let idx = out.find(needle)?;
    let rest = &out[idx + needle.len()..];
    let path = rest.split_whitespace().next()?;
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn is_supported_action(kind: &str) -> bool {
    matches!(
        kind,
        "list_dir"
            | "read_file"
            | "apply_patch"
            | "run_command"
            | "python"
            | "cargo_test"
            | "rustc_hir"
            | "rustc_mir"
            | "graph_call"
            | "graph_cfg"
            | "graph_dataflow"
            | "graph_reachability"
            | "message"
    )
}

pub(crate) fn action_result_prompt(
    tab_id: Option<u32>,
    turn_id: Option<u64>,
    agent_type: &str,
    result: &str,
    last_action: Option<&str>,
    steps_used: Option<usize>,
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
    format!(
        "TAB_ID: {tab_label}\nTURN_ID: {turn_label}\nAGENT_TYPE: {agent_type}\n\n{limit_line}Action result:\n{}\n\n{}\nEmit exactly one action.",
        truncate(result, MAX_SNIPPET),
        next_action_hint_text(result, last_action),
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
    fn validate_rejects_missing_observation() {
        let action = json!({
            "action": "read_file",
            "rationale": "missing observation",
            "path": "SPEC.md"
        });
        assert!(validate_action(&action).is_err());
    }
}

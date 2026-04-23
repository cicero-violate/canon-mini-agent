use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::constants::{
    diagnostics_file, workspace, EXECUTOR_STEP_LIMIT, INVARIANTS_FILE, ISSUES_FILE,
    MASTER_PLAN_FILE, OBJECTIVES_FILE, SPEC_FILE,
};
use crate::prompt_contract::ACTION_EMIT_LINE;
use crate::protocol::{BlockerPayload, MessagePayload, MessageStatus, MessageType, ProtocolMessage, Role};
use crate::tool_schema::selected_tool_protocol_schema_text;
use crate::tool_schema::validate_tool_action;

pub(crate) fn truncate(s: &str, max: usize) -> &str {
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    &s[..end]
}

fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn render_budgeted_item_body(item: &PromptBudgetItem<'_>) -> String {
    const TRUNCATED_MARKER: &str = "\n... [truncated]";

    if item.budget == 0 {
        return String::new();
    }

    if item.budget >= item.raw_bytes {
        return item.body.to_string();
    }

    if item.budget <= TRUNCATED_MARKER.len() {
        return truncate_bytes(TRUNCATED_MARKER, item.budget).to_string();
    }

    let content_budget = item.budget - TRUNCATED_MARKER.len();
    let mut out = truncate_bytes(item.body, content_budget).to_string();
    out.push_str(TRUNCATED_MARKER);
    out
}

#[derive(Clone, Copy, Debug)]
struct PromptItem<'a> {
    heading: &'a str,
    body: &'a str,
    reserve: usize,
    cap: usize,
    weight: usize,
    always_include: bool,
}

#[derive(Clone, Debug)]
struct PromptBudgetItem<'a> {
    heading: &'a str,
    body: &'a str,
    raw_bytes: usize,
    cap: usize,
    weight: usize,
    budget: usize,
    always_include: bool,
}

#[derive(Clone, Debug)]
struct PromptBudget<'a> {
    available: usize,
    used: usize,
    items: Vec<PromptBudgetItem<'a>>,
}

fn compute_prompt_budget<'a>(
    limit: usize,
    framing: usize,
    items: &[PromptItem<'a>],
) -> PromptBudget<'a> {
    let available = limit.saturating_sub(framing);
    let mut budget_items = items
        .iter()
        .map(|item| {
            let raw_bytes = item.body.len();
            let cap = item.cap.min(raw_bytes);
            let reserve = item.reserve.min(cap);
            PromptBudgetItem {
                heading: item.heading,
                body: item.body,
                raw_bytes,
                cap,
                weight: item.weight.max(1),
                budget: reserve,
                always_include: item.always_include,
            }
        })
        .collect::<Vec<_>>();

    let mut used = budget_items.iter().map(|item| item.budget).sum::<usize>();
    if used > available {
        let mut overflow = used - available;
        for item in budget_items.iter_mut().rev() {
            if overflow == 0 {
                break;
            }
            let reducible = item.budget.min(overflow);
            item.budget -= reducible;
            overflow -= reducible;
        }
        used = budget_items.iter().map(|item| item.budget).sum::<usize>();
    }

    let remaining = available.saturating_sub(used);
    if remaining > 0 {
        let total_weight = budget_items
            .iter()
            .filter(|item| item.budget < item.cap)
            .map(|item| item.weight)
            .sum::<usize>();

        if total_weight > 0 {
            let mut assigned = 0usize;
            for item in budget_items.iter_mut() {
                let slack = item.cap.saturating_sub(item.budget);
                if slack == 0 {
                    continue;
                }
                let share =
                    ((remaining as u128 * item.weight as u128) / total_weight as u128) as usize;
                let extra = share.min(slack);
                item.budget += extra;
                assigned += extra;
            }

            let mut leftover = remaining.saturating_sub(assigned);
            while leftover > 0 {
                let mut progressed = false;
                for item in budget_items.iter_mut() {
                    if leftover == 0 {
                        break;
                    }
                    let slack = item.cap.saturating_sub(item.budget);
                    if slack == 0 {
                        continue;
                    }
                    item.budget += 1;
                    leftover -= 1;
                    progressed = true;
                }
                if !progressed {
                    break;
                }
            }
        }
        used = budget_items.iter().map(|item| item.budget).sum::<usize>();
    }

    PromptBudget {
        available,
        used,
        items: budget_items,
    }
}

fn render_budgeted_prompt<'a>(prefix: &str, items: &[PromptItem<'a>], suffix: &str) -> String {
    let active_items = items
        .iter()
        .copied()
        .filter(|item| item.always_include || !item.body.trim().is_empty())
        .collect::<Vec<_>>();
    let framing = prefix.len()
        + suffix.len()
        + active_items
            .iter()
            .map(|item| 3 + item.heading.len())
            .sum::<usize>();
    let budget = compute_prompt_budget(
        crate::constants::PROMPT_OVERFLOW_BYTES,
        framing,
        &active_items,
    );
    debug_assert!(budget.used <= budget.available);

    let mut out = String::with_capacity(prefix.len() + framing + budget.used);
    out.push_str(prefix);
    for item in budget.items {
        if item.budget == 0 && !item.always_include {
            continue;
        }
        out.push_str("\n\n");
        out.push_str(item.heading);
        out.push('\n');
        out.push_str(&render_budgeted_item_body(&item));
    }
    out.push_str(suffix);
    debug_assert!(out.len() <= crate::constants::PROMPT_OVERFLOW_BYTES);
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentPromptKind {
    Executor,
    Planner,
    Solo,
}

fn role_default_schema_actions(kind: AgentPromptKind) -> &'static [&'static str] {
    match kind {
        AgentPromptKind::Executor => &[
            "read_file",
            "apply_patch",
            "symbols_rename_candidates",
            "symbols_prepare_rename",
            "rename_symbol",
            "run_command",
            "cargo_test",
            "python",
            "message",
            "semantic_map",
            "symbol_window",
            "symbol_refs",
            "execution_path",
            "batch",
        ],
        AgentPromptKind::Planner => &[
            "plan",
            "objectives",
            "issue",
            "read_file",
            "symbols_rename_candidates",
            "symbols_prepare_rename",
            "rename_symbol",
            "python",
            "run_command",
            "message",
            "semantic_map",
            "symbol_window",
            "symbol_refs",
            "execution_path",
            "batch",
        ],
        AgentPromptKind::Solo => &[
            "plan",
            "objectives",
            "issue",
            "violation",
            "read_file",
            "apply_patch",
            "symbols_rename_candidates",
            "symbols_prepare_rename",
            "rename_symbol",
            "run_command",
            "cargo_test",
            "python",
            "message",
            "semantic_map",
            "symbol_window",
            "symbol_refs",
            "execution_path",
            "batch",
        ],
    }
}

pub(crate) fn role_default_schema_actions_for_role(role: &str) -> &'static [&'static str] {
    let role = role.trim().to_ascii_lowercase();
    if role.starts_with("executor") {
        role_default_schema_actions(AgentPromptKind::Executor)
    } else if role.starts_with("verifier") || role.starts_with("diagnostics") {
        role_default_schema_actions(AgentPromptKind::Planner)
    } else if role.starts_with("solo") {
        role_default_schema_actions(AgentPromptKind::Solo)
    } else {
        role_default_schema_actions(AgentPromptKind::Planner)
    }
}

fn parse_predicted_action_names(predicted_next_actions: Option<&str>) -> Vec<String> {
    let Some(raw) = predicted_next_actions else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("action").and_then(|v| v.as_str()))
        .map(ToString::to_string)
        .collect()
}

fn dedup_action_names_preserve_order(actions: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for action in actions {
        if !out.iter().any(|existing| existing == &action) {
            out.push(action);
        }
    }
    out
}

fn default_schema_block(kind: AgentPromptKind) -> String {
    selected_tool_protocol_schema_text(role_default_schema_actions(kind))
}

fn prompt_intro(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "You are the canon executor agent.",
        AgentPromptKind::Planner => "You are the canon planner agent.",
        AgentPromptKind::Solo => "You are the canon solo agent (startup compatibility mode only; inactive in runtime two-role orchestration).",
    }
}

fn prompt_mission(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "All actions (`read_file`, `apply_patch`, `run_command`, `plan`, `message`, etc.) are JSON you emit in your response text — they are not function calls or external tools.\nMake source changes, run checks, and report evidence in `message.payload`.",
        AgentPromptKind::Planner => "Your job is to read `SPEC.md`, `agent_state/OBJECTIVES.json`, and the semantic-control snapshot in this prompt, then derive the master plan plus executable next-step guidance for the same operational loop.\nThe semantic-control snapshot is the tlog-derived authority for routing/control and projects issues, violations, and invariants into one view.\nOn every cycle, re-evaluate the workspace and update `PLAN.json` via the `plan` action (emit it as JSON in your response).\nAt the end of every planner cycle, review `agent_state/OBJECTIVES.json` and add or update objectives using the `objectives` action (emit it as JSON in your response).\nAct on projected open issues from semantic control and convert the top open items into ready executor tasks.\nWhen the plan has ready tasks and your analysis is complete, terminate this cycle with a `message` action: `{\"action\":\"message\",\"from\":\"planner\",\"to\":\"executor\",\"type\":\"handoff\",\"status\":\"ready\",\"observation\":\"Ready tasks queued.\",\"rationale\":\"Planner cycle complete.\",\"predicted_next_actions\":[]}`.\nDo not use `message` for intermediate progress tracking — only as the terminal handoff signal or a blocker escalation.\nAll actions (`plan`, `objectives`, `issue`, `message`, `read_file`, etc.) are JSON you emit in your response text — they are not function calls or external tools.\nPlans must follow the JSON PLAN/TASK protocol in `SPEC.md`.",
        AgentPromptKind::Solo => "Your job is to coordinate planning, execution, and verification in a single role while participating in orchestration.\nUse the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.\nYou may read, patch, and verify any in-workspace files when justified by evidence.\nKeep evidence tight and run checks before claiming completion.\nAt the end of every cycle — before emitting a completion message — review `agent_state/OBJECTIVES.json` and add or update objectives based on what you discovered. New objectives must include id, title, status, scope, authority_files, category, level, description, requirement, verification, and success_criteria. Use `apply_patch` to write them directly.",
    }
}

fn prompt_workspace(kind: AgentPromptKind) -> String {
    let ws = crate::constants::workspace();
    match kind {
        AgentPromptKind::Executor => format!("You work inside the canon workspace at {ws}. All relative file paths resolve against this workspace root."),
        AgentPromptKind::Planner => format!("You work inside the canon workspace at {ws}. Use read_file, semantic_map/symbol_window/symbol_refs (prefer over read_file for Rust source), python, and run_command to review the current project state before reorganizing the plan. Planner role cannot use apply_patch."),
        AgentPromptKind::Solo => format!("You work inside the canon workspace at {ws}. Use the full tool suite to plan, execute, and verify changes."),
    }
}

fn status_snapshot_for(kind: AgentPromptKind) -> &'static str {
    let _ = kind;
    ""
}

const PLANNER_PROCESS: &str = "━━━ PLANNING PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n\
⚠ PLAN.json EDIT RULE: use ONLY the `plan` action for all PLAN.json changes. \
Planner role cannot use apply_patch.\n\n\
On every planning cycle:\n\
1. Update `PLAN.json` via the `plan` action and derive the ready-work window for each executor. Mark tasks `ready` (not `todo`) to make them executable — the executor only picks up `ready` tasks.\n\
2. Maintain a READY NOW window containing at most 1-10 executable tasks for each executor.\n\
3. Move blocked work behind its dependencies instead of leaving it in the ready window.\n\
4. Rewrite priorities whenever new evidence changes the critical path.\n\
5. Write detailed, imperative tasks that include file paths and concrete actions (read/patch/test).\n\
6. Keep the ready window executable immediately by the next execute phase in this same runtime loop.\n\n\
Provenance fields — include on every new task:\n\
- `issue_refs`: array of ISSUES.json ids that motivated this task (e.g. [\"auto_mir_dup_abc123\"]). Empty array if none.\n\
- `objective_id`: the agent_state/OBJECTIVES.json objective id this task advances (e.g. \"obj_reduce_complexity\"). Omit if no clear match.";

const EXECUTOR_PREFIX: &str = "━━━ TASK COMPLETION PROTOCOL ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n\
When the task is done and tests pass:\n\
  1. Mark it done: `{\"action\":\"plan\",\"op\":\"set_task_status\",\"task_id\":\"<id>\",\"status\":\"done\",\"rationale\":\"<evidence>\"}`\n\
  2. Do NOT send a `message` — the planner is woken automatically.\n\n\
Use `message` ONLY when:\n\
  • You are blocked by something you cannot resolve (type=blocker, status=blocked)\n\
  • Work is genuinely incomplete and you cannot determine whether it is correct";

const EXECUTION_DISCIPLINE_BULLETS: &[&str] = &[
    "Prefer tasks explicitly marked ready / highest priority by the planner.",
    "Do not skip ahead to lower-priority or blocked tasks unless the current ready task is impossible and you have concrete evidence.",
    "Step budget: target completing each task in steps × 2 actions (minimum 5, absolute ceiling 20). \
     Count each read_file, apply_patch, run_command, cargo_test, and semantic query as one action. \
     If you reach the budget without finishing, complete the current sub-step and persist progress via \
     `plan set_task_status` (`in_progress`) — do not stall.",
    "Completion path: when the task is done and tests pass, mark it done with \
     `plan set_task_status → done` directly — no `message` required. \
     Use `message` ONLY for (a) genuine blockers you cannot resolve or \
     (b) partial completions where uncertainty is too high to mark done.",
    "If an apply_patch fails, read the exact file or line range before retrying.",
    "Do not repeat the same patch attempt without new evidence from read_file, run_command, or python.",
    "Use MIR and HIR analysis to derive call graph, CFG, reachability, and dataflow when diagnosing bugs or proving fixes.",
];

const SOLO_EXECUTION_DISCIPLINE_BULLETS: &[&str] = &[
    "Prefer tasks explicitly marked ready / highest priority by the planner.",
    "Do not skip ahead to lower-priority or blocked tasks unless the current ready task is impossible and you have concrete evidence.",
    "Use the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.",
    "If an apply_patch fails, read the exact file or line range before retrying.",
    "Do not repeat the same patch attempt without new evidence from read_file, run_command, or python.",
    "Use MIR and HIR analysis to derive call graph, CFG, reachability, and dataflow when diagnosing bugs or proving fixes.",
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

fn executor_handoff() -> String {
    EXECUTOR_PREFIX.to_string()
}

fn prompt_tail(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor => {
            format!("{}\n\n{}", executor_handoff(), execution_discipline())
        }
        AgentPromptKind::Planner => PLANNER_PROCESS.to_string(),
        AgentPromptKind::Solo => solo_execution_discipline(),
    }
}

pub(crate) fn system_instructions(kind: AgentPromptKind) -> String {
    let intro = prompt_intro(kind).to_string();
    let mission = prompt_mission(kind).to_string();
    let workspace_text = prompt_workspace(kind);
    let status_snapshot = status_snapshot_for(kind).to_string();
    let tail = prompt_tail(kind);
    let prefix = if status_snapshot.is_empty() {
        format!("{}\n\n{}\n\n{}\n\n", intro, mission, workspace_text)
    } else {
        format!(
            "{}\n\n{}\n\n{}\n\n{}\n\n",
            intro, mission, workspace_text, status_snapshot
        )
    };
    let schema_block = default_schema_block(kind);
    let suffix = format!(
        "\nAction contract — respond with exactly one JSON code block using the role-local actions below:\n\n{schema_block}\n\n{}",
        tail
    );
    render_budgeted_prompt(&prefix, &[], &suffix)
}

pub(crate) fn planner_cycle_prompt(
    _summary_text: &str,
    objectives_text: &str,
    lessons_text: &str,
    enforced_invariants_text: &str,
    semantic_control_text: &str,
    plan_diff: &str,
    executor_diff: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\n\
         PLAN.json EDIT RULE: always use the `plan` action — NEVER apply_patch on {MASTER_PLAN_FILE}.\n\n\
         Current plan state (from {MASTER_PLAN_FILE}) — read-only context, edit via `plan` action:\n{plan_diff}"
    );
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let lessons_heading = "Lessons artifact:".to_string();
    let enforced_invariants_heading =
        "Dynamic enforced invariants (from agent_state/enforced_invariants.json)".to_string();
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let executor_diff_heading =
        "Executor diff (workspace changes excluding plans/diagnostics/violations)".to_string();
    let suffix = format!(
        "\n\n\
         ⟹ IMMEDIATE ACTION: The projected issues in the semantic control section above are \
         pre-validated by semantic control and directly actionable. Do not stall on re-verifying \
         them before planning. Create `plan` tasks for the top open issues now and \
         mark them `ready` for the immediate execute phase.\n\n\
         Before completing this cycle, review {OBJECTIVES_FILE} and add or update objectives \
         for anything discovered this cycle. Use the `objectives` action \
         (op: create_objective / update_objective) to write them. \
         NEVER use apply_patch for {MASTER_PLAN_FILE} — it is always rejected; use the `plan` action.\
         \n\nCycle termination: when the plan has ready tasks and your analysis is complete, end this cycle \
         with a `message` action to the executor:\n\
         {{\"action\":\"message\",\"from\":\"planner\",\"to\":\"executor\",\"type\":\"handoff\",\
         \"status\":\"ready\",\"observation\":\"Ready tasks queued.\",\"rationale\":\"Planner cycle complete.\",\
         \"predicted_next_actions\":[]}}\n\
         Do not loop on read_file/objectives/plan.sorted_view when the plan is already consistent — emit the handoff message."
    );
    let items = [
        PromptItem {
            heading: &executor_diff_heading,
            body: executor_diff,
            reserve: 1200,
            cap: 4000,
            weight: 5,
            always_include: true,
        },
        PromptItem {
            heading: &cargo_failures_heading,
            body: cargo_test_failures,
            reserve: 800,
            cap: 3000,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &objectives_heading,
            body: objectives_text,
            reserve: 1000,
            cap: 3000,
            weight: 5,
            always_include: false,
        },
        PromptItem {
            heading: &enforced_invariants_heading,
            body: enforced_invariants_text,
            reserve: 800,
            cap: 2500,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &lessons_heading,
            body: lessons_text,
            reserve: 800,
            cap: 2500,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &semantic_control_heading,
            body: semantic_control_text,
            reserve: 1200,
            cap: 3500,
            weight: 4,
            always_include: false,
        },
    ];
    render_budgeted_prompt(&prefix, &items, &suffix)
}

pub(crate) fn executor_cycle_prompt(
    _executor_name: &str,
    lane_label: &str,
    latest_verify_result: &str,
    ready_tasks: &str,
) -> String {
    let workspace = workspace();
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
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nREADY TASKS (from {MASTER_PLAN_FILE}, top-10 by plan order):\n{ready_tasks}\n\nLane plans are deprecated. Use {MASTER_PLAN_FILE} and current planner-phase outputs for task selection.\nLatest verifier result for lane {lane_label}:\n{verify_result}\n\nUse `message` primarily for blocker escalation or unresolved partial completion evidence."
    )
}

pub(crate) fn single_role_planner_prompt(
    _primary_input: &str,
    objectives: &str,
    lessons_text: &str,
    enforced_invariants_text: &str,
    semantic_control: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let issues_file = ISSUES_FILE;
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed."
    );
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let lessons_heading = "Lessons artifact:".to_string();
    let enforced_invariants_heading =
        "Dynamic enforced invariants (from agent_state/enforced_invariants.json)".to_string();
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let suffix = format!(
        "\n\nUse {INVARIANTS_FILE} when deriving plan constraints.\nRead files and search the source code before issuing plan changes.\nOpen issues in `{issues_file}` are directly actionable when they include current-source evidence — create plan tasks that reference `issue_refs`. `{diagnostics_path}` entries with no matching {issues_file} entry are hints only.\nWrite imperative, actionable instructions in {MASTER_PLAN_FILE}.\nOnly use plan diffs when available; avoid re-reading the full plan unless necessary.\nDo not use internal tools.\nDo not hand off work; keep planning and execution in the current role flow.\nWhen a `plan` action is derived from projected diagnostics state, include same-cycle source validation in `observation` and `rationale` before mutating {MASTER_PLAN_FILE}.\n\nTreat stale or already-resolved projected diagnostics as non-actionable until current source evidence reconfirms them.\nIf projected diagnostics repeatedly report stale issues, create follow-up work to repair projection generation rather than reopening resolved implementation tasks."
    );
    let items = [
        PromptItem {
            heading: &objectives_heading,
            body: objectives,
            reserve: 800,
            cap: 3000,
            weight: 4,
            always_include: true,
        },
        PromptItem {
            heading: &lessons_heading,
            body: lessons_text,
            reserve: 800,
            cap: 2500,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &enforced_invariants_heading,
            body: enforced_invariants_text,
            reserve: 800,
            cap: 2500,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &semantic_control_heading,
            body: semantic_control,
            reserve: 1200,
            cap: 3500,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &cargo_failures_heading,
            body: cargo_test_failures,
            reserve: 800,
            cap: 2500,
            weight: 2,
            always_include: false,
        },
    ];
    render_budgeted_prompt(&prefix, &items, &suffix)
}

pub(crate) fn single_role_executor_prompt(
    _spec: &str,
    master_plan: &str,
    semantic_control: &str,
) -> String {
    let workspace = workspace();
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed.\n\nMaster plan (from {MASTER_PLAN_FILE}):\n{master_plan}"
    );
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let suffix = format!(
        "\n\nLane plans are deprecated. Use {MASTER_PLAN_FILE} and current planner-phase outputs for task selection.\n\nDo not modify spec, plan, violations, or diagnostics.\nDo not use internal tools.\nDo not hand off work; continue execution directly in the current role flow.\nUse `message.payload` to report blocker escalation or unresolved partial-completion evidence. {ACTION_EMIT_LINE}"
    );
    let items = vec![PromptItem {
        heading: &semantic_control_heading,
        body: semantic_control,
        reserve: 800,
        cap: 3000,
        weight: 4,
        always_include: true,
    }];
    render_budgeted_prompt(&prefix, &items, &suffix)
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

#[cfg(test)]
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
        "apply_patch" | "rename_symbol" | "run_command" | "python" | "cargo_test"
        | "cargo_clippy" => true,
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
            tasks
                .iter()
                .find(|task| task.get("id").and_then(|v| v.as_str()) == Some(task_id))
        })
        .and_then(|task| task.get("objective_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn validate_action_provenance(action: &Value) -> Result<()> {
    if !action_requires_provenance(action) {
        return Ok(());
    }

    let task_id = action_task_id(action).ok_or_else(|| {
        anyhow!("mutating or verification actions must include non-empty task_id")
    })?;
    let objective_id = action_objective_id(action).ok_or_else(|| {
        anyhow!("mutating or verification actions must include non-empty objective_id")
    })?;
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

fn blocker_payload_fields_present(payload: &BlockerPayload) -> bool {
    !payload.blocker.trim().is_empty()
        && !payload.evidence.trim().is_empty()
        && !payload.required_action.trim().is_empty()
}

fn validate_blocker_message_payload(msg: &ProtocolMessage) -> Result<()> {
    if !(matches!(msg.msg_type, MessageType::Blocker)
        || matches!(msg.status, MessageStatus::Blocked))
    {
        return Ok(());
    }
    match &msg.payload {
        MessagePayload::Blocker(payload) => {
            if !blocker_payload_fields_present(payload) {
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
    let _ =
        serde_json::from_value::<crate::protocol::Severity>(Value::String(severity.to_string()))
            .map_err(|_| anyhow!("message severity must be one of: info|warn|error|critical"))?;
    Ok(())
}

fn validate_optional_message_role(obj: &serde_json::Map<String, Value>, field: &str) -> Result<()> {
    let Some(role) = obj.get(field).and_then(|v| v.as_str()) else {
        return Ok(());
    };
    if !matches!(role, "user" | "executor" | "planner") {
        bail!("{field} must be one of: user|executor|planner");
    }
    Ok(())
}

fn validate_message_route(msg: &ProtocolMessage) -> Result<()> {
    let self_routed = std::mem::discriminant(&msg.from) == std::mem::discriminant(&msg.to);
    if self_routed {
        bail!("message route may not target the emitting role in two-role runtime");
    }
    Ok(())
}

fn validate_active_message_roles(msg: &ProtocolMessage) -> Result<()> {
    let from_ok = matches!(msg.from, Role::Planner | Role::Executor | Role::User);
    let to_ok = matches!(msg.to, Role::Planner | Role::Executor | Role::User);
    if !from_ok || !to_ok {
        bail!("message roles must be planner, executor, or user in two-role runtime");
    }
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
    validate_active_message_roles(&msg)?;
    validate_blocker_message_payload(&msg)?;
    validate_message_route(&msg)?;
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
        for field in ["severity"] {
            if let Some(val) = obj
                .get_mut(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                obj.insert(field.to_string(), Value::String(val.to_lowercase()));
            }
        }
        if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
            if let Some(val) = payload
                .get_mut("severity")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                payload.insert("severity".to_string(), Value::String(val.to_lowercase()));
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
    match derive_next_action_hint(result, last_action) {
        NextActionHint::GraphFollowups => {
            "next_action_hint: run graph_call, graph_cfg, graph_reachability".to_string()
        }
        NextActionHint::UseApplyPatch => {
            "next_action_hint: use apply_patch to update workspace files (`src/` or lane plans) if python cannot write.".to_string()
        }
        NextActionHint::ReuseRecent { action } => {
            format!("next_action_hint: reuse recent action `{action}` or choose the narrowest valid next step from your predicted actions.")
        }
        NextActionHint::ChooseAction { last_action } => {
            if let Some(action) = last_action {
                format!("next_action_hint: choose the narrowest valid next step from your predicted actions. recent action: {action}.")
            } else {
                "next_action_hint: choose the narrowest valid next step from your predicted actions.".to_string()
            }
        }
    }
}

fn predicted_actions_summary(predicted_next_actions: Option<&str>) -> String {
    let Some(raw) = predicted_next_actions else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return String::new();
    };
    let Some(items) = value.as_array() else {
        return String::new();
    };
    let mut rendered = Vec::new();
    for item in items.iter().take(3) {
        let Some(action) = item.get("action").and_then(|v| v.as_str()) else {
            continue;
        };
        let intent = item
            .get("intent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| truncate(text, 48).to_string())
            .unwrap_or_else(|| "no intent".to_string());
        rendered.push(format!("`{action}` ({intent})"));
    }
    if rendered.is_empty() {
        return String::new();
    }
    format!(
        "Predicted next actions from your last turn: {}. Compare these against the actual result above before choosing your next action.\n\n",
        rendered.join(", ")
    )
}

fn agent_prompt_kind_from_agent_type(agent_type: &str) -> AgentPromptKind {
    let normalized = agent_type.trim().to_ascii_uppercase();
    if normalized.starts_with("EXECUTOR") {
        AgentPromptKind::Executor
    } else if normalized.starts_with("VERIFIER") || normalized.starts_with("DIAGNOSTICS") {
        AgentPromptKind::Planner
    } else if normalized.starts_with("SOLO") {
        AgentPromptKind::Solo
    } else {
        AgentPromptKind::Planner
    }
}

fn available_actions_hint_text(agent_type: &str) -> String {
    let kind = agent_prompt_kind_from_agent_type(agent_type);
    let actions = role_default_schema_actions(kind);
    let preview = actions
        .iter()
        .take(6)
        .map(|action| format!("`{action}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if actions.len() > 6 {
        format!("available_actions: {preview}, …")
    } else {
        format!("available_actions: {preview}")
    }
}

fn action_result_sections(result: &str) -> Vec<(String, String, usize, usize, usize, bool)> {
    let trimmed = result.trim();
    if trimmed.is_empty() {
        return vec![(
            "Action result".to_string(),
            "(empty)".to_string(),
            32,
            512,
            1,
            true,
        )];
    }

    let mut sections = Vec::new();
    let mut transcript_index = 0usize;
    for chunk in trimmed
        .split("\n\n")
        .map(str::trim)
        .filter(|chunk| !chunk.is_empty())
    {
        let heading = if chunk == "Chained action transcript:" {
            "Action result chain".to_string()
        } else if chunk == "Notes:" {
            "Action result notes".to_string()
        } else if chunk.starts_with('[') {
            transcript_index += 1;
            format!("Action result {}", transcript_index)
        } else if sections.is_empty() {
            "Action result summary".to_string()
        } else {
            format!("Action result detail {}", sections.len())
        };

        let reserve = if heading == "Action result summary" {
            256
        } else {
            192
        };
        let cap = if heading == "Action result summary" {
            4096
        } else {
            8192
        };
        let weight = if heading == "Action result summary" {
            3
        } else {
            2
        };
        sections.push((heading, chunk.to_string(), reserve, cap, weight, true));
    }

    sections
}

pub(crate) fn render_action_result_sections(prefix: &str, result: &str, suffix: &str) -> String {
    let owned = action_result_sections(result);
    let items = owned
        .iter()
        .map(
            |(heading, body, reserve, cap, weight, always_include)| PromptItem {
                heading: heading.as_str(),
                body: body.as_str(),
                reserve: *reserve,
                cap: *cap,
                weight: *weight,
                always_include: *always_include,
            },
        )
        .collect::<Vec<_>>();
    render_budgeted_prompt(prefix, &items, suffix)
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
    let predicted_actions =
        dedup_action_names_preserve_order(parse_predicted_action_names(predicted_next_actions));
    let predicted_line = match (last_action, predicted_actions.is_empty()) {
        (Some(action_name), false) if !predicted_actions.iter().any(|a| a == action_name) => {
            predicted_actions_summary(predicted_next_actions)
        }
        _ => String::new(),
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

    let prefix = format!(
        "TAB_ID: {tab_label}\nTURN_ID: {turn_label}\nAGENT_TYPE: {agent_type}\n\n{limit_line}{provenance_block}"
    );
    let suffix = format!(
        "\n\n{predicted_line}{}\n{}{}\n{ACTION_EMIT_LINE}",
        next_action_hint_text(result, last_action),
        available_actions_hint_text(agent_type),
        mutating_question,
    );
    render_action_result_sections(&prefix, result, &suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_contract::ACTION_EMIT_LINE;
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
            "question": "Does same-cycle source evidence justify this planner task status transition now?",
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
    fn validate_rejects_self_addressed_planner_message() {
        let action = json!({
            "action": "message",
            "from": "planner",
            "to": "planner",
            "type": "blocker",
            "status": "blocked",
            "observation": "The chat claims the planner runtime is unavailable.",
            "rationale": "Escalate the blocked state.",
            "payload": {
                "summary": "Planner blocked by missing runtime.",
                "blocker": "Required runtime missing",
                "evidence": "this chat does not expose the required canon runtime",
                "required_action": "Restore the runtime"
            },
            "predicted_next_actions": [
                {"action": "read_file", "intent": "reinspect the runtime contract"},
                {"action": "message", "intent": "report a corrected blocker if one remains"}
            ]
        });
        assert!(validate_action(&action).is_err());
    }

    #[test]
    fn validate_rejects_legacy_solo_result_self_route() {
        let action = json!({
            "action": "message",
            "from": "solo",
            "to": "solo",
            "type": "result",
            "status": "complete",
            "observation": "Solo completed the bounded task.",
            "rationale": "Return the final result within the solo flow.",
            "payload": {
                "summary": "Solo work complete."
            },
            "predicted_next_actions": [
                {"action": "read_file", "intent": "inspect any referenced artifact if needed"},
                {"action": "run_command", "intent": "verify the final workspace state if needed"}
            ]
        });
        let err = validate_action(&action).unwrap_err().to_string();
        assert!(
            err.contains("planner, executor, or user")
                || err.contains("may not target the emitting role"),
            "unexpected error: {err}"
        );
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
                {"action": "semantic_map", "intent": "Get semantic triples for the tools module."},
                {"action": "symbol_window", "intent": "Read the exact target function body."},
                {"action": "symbol_refs", "intent": "Collect all reference sites before edits."}
            ]
        });
        assert!(validate_action(&action).is_ok());
    }

    #[test]
    fn planner_requires_plan_action_for_master_plan_edits() {
        assert!(
            PLANNER_PROCESS.contains("use ONLY the `plan` action for all PLAN.json changes"),
            "planner process must require plan tool for PLAN.json edits"
        );
    }

    #[test]
    fn planner_system_instructions_include_tool_schema_block() {
        let prompt = system_instructions(AgentPromptKind::Planner);
        assert!(
            prompt.contains("Action contract — respond with exactly one JSON code block using the role-local actions below"),
            "planner system prompt should include an introductory schema block"
        );
        assert!(
            prompt.contains("Action: `plan`"),
            "planner system prompt should include the plan schema"
        );
        assert!(
            prompt.contains("Action: `issue`"),
            "planner system prompt should include the issue schema"
        );
        assert!(
            prompt.contains("Ops: create_task"),
            "planner system prompt should include schema-derived plan op hints"
        );
        assert!(
            prompt.contains("add_edge"),
            "planner system prompt should include the canonical add_edge op"
        );
        assert!(
            prompt.contains("do not emit `create_edge`"),
            "planner system prompt should forbid the legacy create_edge alias"
        );
        assert!(
            !prompt.contains("━━━ RULES"),
            "planner system prompt should omit the RULES section"
        );
    }

    #[test]
    fn executor_system_instructions_include_tool_schema_block() {
        let prompt = system_instructions(AgentPromptKind::Executor);
        assert!(
            prompt.contains("Action contract — respond with exactly one JSON code block using the role-local actions below"),
            "executor system prompt should include an introductory schema block"
        );
        assert!(
            prompt.contains("Action: `read_file`"),
            "executor system prompt should include the read_file schema"
        );
        assert!(
            prompt.contains("Action: `apply_patch`"),
            "executor system prompt should include the apply_patch schema"
        );
        assert!(
            prompt.contains("Action: `python`"),
            "executor system prompt should include the python schema"
        );
        assert!(
            !prompt.contains("Runtime law:"),
            "executor system prompt should omit the runtime law block"
        );
    }

    #[test]
    fn planner_system_instructions_omit_duplicated_status_snapshot() {
        let prompt = system_instructions(AgentPromptKind::Planner);
        assert!(
            !prompt.contains("Canonical status snapshot:"),
            "planner system prompt should omit the duplicated canonical status snapshot"
        );
    }

    #[test]
    fn planner_system_instructions_trim_static_prompt_mass() {
        let prompt = system_instructions(AgentPromptKind::Planner);
        assert!(prompt.len() < 24_000, "planner system prompt should stay compact");
        assert!(!prompt.contains("Canonical status snapshot:"));
        assert!(!prompt.contains("Open guarantees still to close:"));
    }

    #[test]
    fn action_result_prompt_does_not_duplicate_predicted_action_schemas() {
        let predicted = r#"[
            {"action":"issue","intent":"update stale issue"},
            {"action":"violation","intent":"refresh violations"},
            {"action":"message","intent":"report blocker if needed"}
        ]"#;
        let prompt = action_result_prompt(
            Some(1),
            Some(2),
            "PLANNER",
            "python ok:\nEvidence receipt: rcpt-123\n{}",
            Some("python"),
            Some("T1"),
            Some("obj_15_automated_learning_loop"),
            Some("Inspect diagnostics state"),
            None,
            Some(predicted),
        );
        assert!(!prompt.contains("Derived schemas for predicted next actions:"));
        assert!(!prompt.contains("Action contract — respond with exactly one JSON code block"));
        assert!(prompt.contains("Predicted next actions from your last turn:"));
        assert!(prompt.contains("Compare these against the actual result above before choosing your next action."));
        assert!(!prompt.contains("```json\n["));
    }

    #[test]
    fn action_result_prompt_falls_back_to_role_schema_when_predictions_missing() {
        let prompt = action_result_prompt(
            Some(1),
            Some(2),
            "EXECUTOR",
            "Invalid action rejected.",
            Some("invalid_action"),
            None,
            None,
            None,
            Some(1),
            None,
        );
        assert!(!prompt.contains("Action contract — respond with exactly one JSON code block"));
        assert!(!prompt.contains("Derived schemas for predicted next actions:"));
        assert!(!prompt.contains("Predicted next actions from your last turn:"));
    }

    #[test]
    fn action_result_prompt_omits_predicted_echo_when_action_matches_prediction() {
        let predicted = r#"[
            {"action":"python","intent":"inspect evidence"},
            {"action":"message","intent":"report status"}
        ]"#;
        let prompt = action_result_prompt(
            Some(1),
            Some(2),
            "EXECUTOR",
            "python ok:\n{}",
            Some("python"),
            Some("T1"),
            Some("obj_15"),
            Some("Inspect diagnostics state"),
            Some(1),
            Some(predicted),
        );
        assert!(!prompt.contains("Predicted next actions from your last turn:"));
        assert!(!prompt.contains("Compare these against the actual result above before choosing your next action."));
    }

    #[test]
    fn action_result_prompt_includes_exhaustive_available_actions_for_role() {
        let prompt = action_result_prompt(
            Some(1),
            Some(2),
            "PLANNER",
            "plan ok",
            Some("plan"),
            Some("T1"),
            Some("obj_1"),
            Some("seed ready task"),
            None,
            None,
        );
        assert!(prompt.contains("available_actions:"));
        assert!(prompt.contains("`plan`"));
        assert!(prompt.contains("`objectives`"));
        assert!(prompt.contains("`issue`"));
        assert!(prompt.contains("…"));
        assert!(!prompt.contains("`batch`"));
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
            "{enforced_invariants}",
            "{semantic_control}",
            "{cargo_test_failures}",
        );
        assert!(
            prompt.contains("Treat stale or already-resolved projected diagnostics as non-actionable until current source evidence reconfirms them."),
            "planner prompt must keep stale diagnostics non-actionable"
        );
        assert!(
            prompt.contains("If projected diagnostics repeatedly report stale issues, create follow-up work to repair projection generation rather than reopening resolved implementation tasks."),
            "planner prompt must direct diagnostics-repair follow-up for stale reports"
        );
    }

    #[test]
    fn planner_prompt_includes_lessons_artifact_section() {
        let prompt = single_role_planner_prompt(
            "{spec}",
            "{objectives}",
            "LESSON_TEXT",
            "{enforced_invariants}",
            "{semantic_control}",
            "{cargo_test_failures}",
        );
        assert!(
            prompt.contains("Lessons artifact:\nLESSON_TEXT"),
            "planner prompt must embed the lessons artifact body"
        );
    }

    #[test]
    fn no_legacy_emit_wording_in_system_instructions() {
        let kinds = [
            AgentPromptKind::Planner,
            AgentPromptKind::Executor,
            AgentPromptKind::Solo,
        ];
        for kind in kinds {
            let rendered = system_instructions(kind);
            assert!(!rendered.contains("Emit exactly one action per turn"));
            assert!(!rendered.contains("Emit exactly one action to begin"));
            assert!(!rendered.contains("Emit exactly one action"));
            assert!(rendered.contains(ACTION_EMIT_LINE));
        }
    }

    #[test]
    fn no_legacy_emit_wording_in_action_result_prompt() {
        let prompt = action_result_prompt(
            Some(1),
            Some(2),
            "PLANNER",
            "plan ok",
            Some("plan"),
            Some("T1"),
            Some("obj_1"),
            Some("seed task"),
            None,
            Some(r#"[{"action":"message","intent":"handoff"}]"#),
        );
        assert!(!prompt.contains("Emit exactly one action per turn"));
        assert!(!prompt.contains("Emit exactly one action to begin"));
        assert!(prompt.contains(ACTION_EMIT_LINE));
    }

    #[test]
    fn no_legacy_emit_wording_in_schema_preamble() {
        let text = crate::tool_schema::selected_tool_protocol_schema_text(&["plan", "message"]);
        assert!(!text.contains("Emit exactly one action per turn"));
        assert!(!text.contains("Emit exactly one action to begin"));
        assert!(!text.contains("Emit exactly one action"));
        assert!(text.contains(ACTION_EMIT_LINE));
    }

    #[test]
    fn prompt_contract_constants_are_canonical() {
        assert!(ACTION_EMIT_LINE.contains("Emit batch actions."));
        assert!(ACTION_EMIT_LINE.contains("reveal chain of thought"));
        assert!(!ACTION_EMIT_LINE.contains("exactly one action"));
    }
}

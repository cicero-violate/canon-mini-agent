use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::constants::{
    diagnostics_file, workspace, EXECUTOR_STEP_LIMIT, INVARIANTS_FILE, ISSUES_FILE,
    MASTER_PLAN_FILE, OBJECTIVES_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::protocol::{MessagePayload, MessageStatus, MessageType, ProtocolMessage, Role};
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
    Verifier,
    Planner,
    Diagnostics,
    Solo,
}

fn role_default_schema_actions(kind: AgentPromptKind) -> &'static [&'static str] {
    match kind {
        AgentPromptKind::Executor => &[
            "read_file",
            "apply_patch",
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
        AgentPromptKind::Verifier => &[
            "read_file",
            "cargo_test",
            "python",
            "violation",
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
            "apply_patch",
            "python",
            "run_command",
            "message",
            "semantic_map",
            "symbol_window",
            "symbol_refs",
            "execution_path",
            "batch",
        ],
        AgentPromptKind::Diagnostics => &[
            "issue",
            "violation",
            "apply_patch",
            "python",
            "read_file",
            "list_dir",
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

fn agent_kind_from_agent_type(agent_type: &str) -> AgentPromptKind {
    match agent_type {
        "EXECUTOR" => AgentPromptKind::Executor,
        "VERIFIER" => AgentPromptKind::Verifier,
        "PLANNER" => AgentPromptKind::Planner,
        "DIAGNOSTICS" => AgentPromptKind::Diagnostics,
        _ => AgentPromptKind::Solo,
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

fn predicted_action_schema_block(predicted_next_actions: Option<&str>) -> String {
    let actions =
        dedup_action_names_preserve_order(parse_predicted_action_names(predicted_next_actions));
    if actions.is_empty() {
        return String::new();
    }

    let action_refs: Vec<&str> = actions.iter().map(|action| action.as_str()).collect();
    let rendered = selected_tool_protocol_schema_text(&action_refs);
    format!("Derived schemas for predicted next actions:\n{rendered}")
}

fn default_schema_block(kind: AgentPromptKind) -> String {
    selected_tool_protocol_schema_text(role_default_schema_actions(kind))
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
        AgentPromptKind::Executor => "Your job is to execute the highest-priority READY work described in planner handoff messages and the master plan.\n`SPEC.md` is the canonical contract.\nLane plans are deprecated and should not be relied on for task selection.\nThe verifier judges code against `SPEC.md`.\nYou should only work on the top 1-10 ready tasks in the current cycle, then yield.\nAll actions (`read_file`, `apply_patch`, `run_command`, `plan`, `message`, etc.) are JSON you emit in your response text — they are not function calls or external tools.\nDo not reorganize or update `SPEC.md` or plan files yourself.\nMake source changes, run checks, and report evidence in `message.payload`.",
        AgentPromptKind::Verifier => "Your job is to critically review executor evidence against the codebase and judge whether the implementation satisfies `SPEC.md`.\nExecutor evidence is a hint only. The canonical truth is the codebase versus `SPEC.md`.\nIf violations are found, use the `violation` action (op=upsert) to record them in `VIOLATIONS.json`. Use `violation` op=resolve to clear violations that are no longer active. Never use `apply_patch` for VIOLATIONS.json.\nBe skeptical — do not trust executor claims at face value.",
        AgentPromptKind::Planner => "Your job is to read `SPEC.md`, `agent_state/OBJECTIVES.json`, and the semantic-control snapshot in this prompt, then derive the master plan plus executor handoff guidance.\nThe semantic-control snapshot is the tlog-derived authority for routing/control and already projects issues, violations, diagnostics, and invariants into one view.\nOn every cycle, re-evaluate the workspace and update `PLAN.json` via the `plan` action (emit it as JSON in your response).\nAt the end of every planner cycle, review `agent_state/OBJECTIVES.json` and add or update objectives using the `objectives` action (emit it as JSON in your response).\nAct on projected open issues from semantic control — diagnostics populates them with evidence-backed findings. Do not require re-verifying an issue before creating a task for it: the diagnostics role already verified the evidence.\nAll actions (`plan`, `objectives`, `issue`, `message`, `read_file`, etc.) are JSON you emit in your response text — they are not function calls or external tools.\nPlans must follow the JSON PLAN/TASK protocol in `SPEC.md`.",
        AgentPromptKind::Diagnostics => "Your job is to scan the active workspace state, use the semantic-control snapshot as the control authority, derive the current failures from evidence, and refresh the projected diagnostics/issue/violation views with the `issue` and `violation` actions. Artifact views are supporting projections; planner follow-up is owned by semantic control.",
        AgentPromptKind::Solo => "Your job is to coordinate planning, execution, and verification in a single role while participating in orchestration.\nUse the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.\nYou may read, patch, and verify any in-workspace files when justified by evidence.\nKeep evidence tight and run checks before claiming completion.\nAt the end of every cycle — before emitting a completion message — review `agent_state/OBJECTIVES.json` and add or update objectives based on what you discovered. New objectives must include id, title, status, scope, authority_files, category, level, description, requirement, verification, and success_criteria. Use `apply_patch` to write them directly.",
    }
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

fn canonical_status_snapshot() -> &'static str {
    "Canonical status snapshot:\n- canonical state changes are gated through the canonical writer\n- replay from `agent_state/tlog.ndjson` is meaningful for canonical state\n- many reconciliation paths are now explicit `ControlEvent`s\n- several loophole-shaped runtime paths are classified and recorded in `agent_state/blockers.json`\n- blockers and invariants can now accumulate around structural failures\n\nOpen guarantees still to close:\n- not every runtime-influenced control decision is canonically represented yet\n- not every reconciliation branch has been split into the right explicit event shape yet\n- not every loophole-class blocker is wired into invariant promotion or route gating yet\n- not every checkpoint/resume inconsistency is bounded and proven safe yet\n- not every intentionally-ephemeral runtime behavior is enumerated and justified yet\n- some branches are now detectable but are not all replaced by dedicated canonical `ControlEvent`s yet\n- blocker -> invariant -> gate coverage for ambiguity/effectful classes is still being completed\n- full orchestration-loop integration tests for these loophole classes still do not exist yet\n\nLoophole-closure rule:\n- when you encounter runtime behavior that influences control flow or externally visible behavior, either prove it is already represented canonically or add the missing event/policy/invariant/test instead of building new features."
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
         - Before emitting a completion message, review `agent_state/OBJECTIVES.json`. Add new objectives for anything you discovered this cycle that is not yet captured. Update the status of existing objectives that changed. Use `apply_patch` to write changes.\n\
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
1. Read `SPEC.md`, the semantic-control snapshot in this prompt, relevant source files, and recent workspace state to understand what changed. Projected open issues in semantic control are your primary action signal. Do NOT treat changes to documentation files (*.md, AUTHORITY_MATRIX.md, SPEC.md) visible in the executor diff as authority regressions — documentation maintenance is expected and is never grounds for a blocker.\n\
2. Update `PLAN.json` via the `plan` action and derive the ready-work window for each executor. Mark tasks `ready` (not `todo`) to make them executable — the executor only picks up `ready` tasks.\n\
3. Maintain a READY NOW window containing at most 1-10 executable tasks for each executor.\n\
4. Move blocked work behind its dependencies instead of leaving it in the ready window.\n\
5. Rewrite priorities whenever new evidence changes the critical path.\n\
6. If canonical-law authority (INVARIANTS.json) conflicts with local heuristics in the plan, prioritize canonical-law authority and move heuristic cleanup behind it as follow-on work.\n\
7. Act on projected open issues in semantic control — diagnostics writes them with evidence already attached. Create plan tasks that reference `issue_refs`. You do NOT need to re-verify a diagnostics-opened issue before acting on it.\n\
8. Treat projected diagnostics detail without a matching projected issue as an unverified hint only; plan evidence-gathering or diagnostics-repair work for that instead of implementation tasks.\n\
9. Write detailed, imperative tasks that include file paths and concrete actions (read/patch/test).\n\
10. Send handoff messages to executors reflecting the updated ready window.\n\n\
Provenance fields — include on every new task:\n\
- `issue_refs`: array of ISSUES.json ids that motivated this task (e.g. [\"auto_mir_dup_abc123\"]). Empty array if none.\n\
- `objective_id`: the agent_state/OBJECTIVES.json objective id this task advances (e.g. \"obj_reduce_complexity\"). Omit if no clear match.";

fn diagnostics_process() -> String {
    let workspace = crate::constants::workspace();
    let violations = "";
    let objectives = "";
    let cargo_test_failures = "";
    let diagnostics_path = diagnostics_file();
    let _diagnostics_budget_marker = diagnostics_path.len();
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nRead files and search the source code for bugs and inconsistencies (use read_file + run_command/ripgrep).\nRun python analysis actions over available workspace-local logs, state, and code evidence.\nDo not assume canon-specific observability names or paths. Discover the actual project-local artifacts first by inspecting files and directories that exist under WORKSPACE. Examples may include state/, log/, logs, runtime logs, jsonl logs, agent logs, or other workspace-defined artifacts.\nInfer the root cause from the evidence and cite detailed sources of errors (file paths, functions, log evidence).\n\nLatest verifier summary:\n(none yet)"
    );
    let violations_heading = format!("Violations (from {VIOLATIONS_FILE})");
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let suffix = format!(
        "\n\nVerify whether objectives in {OBJECTIVES_FILE} are being met and note gaps.\nUse {SPEC_FILE}, {OBJECTIVES_FILE}, and {INVARIANTS_FILE} as the contract, not lane plans.\nInfer failures from code, logs, runtime state, and verifier findings.\nPrefer evidence from workspace-local artifacts that actually exist over assumptions from other projects.\nTreat {VIOLATIONS_FILE}, ISSUES.json, and {diagnostics_path} as derived projections to keep synchronized with current evidence, not as control authority.\nDo not restate verifier-cleared or already-resolved issues unless fresh current-cycle source or runtime evidence reconfirms them.\nIf the mismatch is stale projected state rather than a live implementation bug, repair the projection instead of reopening the cleared issue.\n\nWrite a ranked diagnostics report to {diagnostics_path}."
    );
    let items = [
        PromptItem {
            heading: &violations_heading,
            body: violations,
            reserve: 1000,
            cap: 3000,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &objectives_heading,
            body: objectives,
            reserve: 800,
            cap: 2500,
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
    let _ = render_budgeted_prompt(&prefix, &items, &suffix);
    format!("━━━ DIAGNOSTICS PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\nGather evidence from the workspace, projected artifact views, and the current codebase.\n\nMandatory log sources — read ALL of these before writing any findings:\n  - agent_state/tlog.ndjson          — total-ordered event log (ControlEvent sequence); reveals state-machine transitions, phase changes, invariant violations\n  - agent_state/enforced_invariants.json — dynamically discovered invariants (discovered/promoted/enforced/collapsed); check for promoted-but-unenforced entries and stale collapsed entries\n  - agent_state/lessons.json         — synthesized behavioral lessons; surface any high-weight recurring lessons as diagnostics findings\n  - agent_state/default/actions.jsonl — full agent action history; tail with python to find recent failures, retry loops, and stall patterns\n\nStep 1 — Derive ranked failures from current evidence, with tlog-derived semantic control state as the routing/control authority.\nStep 2 — Project those findings into {diagnostics_path}, ISSUES.json, and {VIOLATIONS_FILE} using the available actions and canon-mini-agent/src/reports.rs enums.\n  - op=create if no matching issue exists (include kind, location, evidence, priority).\n  - op=update if a matching issue exists but its evidence is stale.\n  - op=set_status status=resolved if a prior issue is no longer supported by evidence.\n  Artifact files are derived outputs to keep synchronized with current evidence; they do not control planner follow-up.\n\nRules:\n- Use the `python` action for structured analysis of project state and any available logs.\n- Rank issues by impact on correctness, convergence, and repairability.\n- Check whether control-flow decisions are consistent with INVARIANTS.json.\n- Before trusting any trace or log file, confirm it was updated in the current cycle (mtime, size change, or fresh producer command).\n- Treat empty `rg` / `grep` results as ambiguous: no match, stale file, or incomplete write are all possible.\n- Prefer the most recently written evidence sources over ad-hoc temp traces when they disagree.\n- Derive observability paths from workspace-local state and log artifacts that actually exist for this project instead of assuming canon-specific defaults.")
}

const EXECUTOR_HANDOFF_BULLETS: &[&str] = &[
    "files changed",
    "commands run",
    "outcomes / failing checks",
    "remaining uncertainty or blockers",
];

const EXECUTOR_PREFIX: &str = "━━━ TASK COMPLETION PROTOCOL ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n\
When the task is done and tests pass:\n\
  1. Mark it done: `{\"action\":\"plan\",\"op\":\"set_task_status\",\"task_id\":\"<id>\",\"status\":\"done\",\"rationale\":\"<evidence>\"}`\n\
  2. Do NOT send a `message` — the planner is woken automatically.\n\n\
Use `message` ONLY when:\n\
  • You are blocked by something you cannot resolve (type=blocker, status=blocked)\n\
  • Work is genuinely incomplete and you cannot determine whether it is correct\n\n\
When you do send a `message`, include in `message.payload`:";

const EXECUTION_DISCIPLINE_BULLETS: &[&str] = &[
    "Prefer tasks explicitly marked ready / highest priority by the planner.",
    "Do not skip ahead to lower-priority or blocked tasks unless the current ready task is impossible and you have concrete evidence.",
    "Step budget: target completing each task in steps × 2 actions (minimum 5, absolute ceiling 20). \
     Count each read_file, apply_patch, run_command, cargo_test, and semantic query as one action. \
     If you reach the budget without finishing, complete the current sub-step and hand off — do not stall.",
    "Completion path: when the task is done and tests pass, mark it done with \
     `plan set_task_status → done` directly — no `message` required. \
     Use `message` ONLY for (a) genuine blockers you cannot resolve or \
     (b) partial completions where uncertainty is too high to mark done.",
    "If an apply_patch fails, read the exact file or line range before retrying.",
    "Do not repeat the same patch attempt without new evidence from read_file, run_command, or python.",
    "When touching routing, policy, or control-flow code, favor the authority described in INVARIANTS.json over local heuristics.",
    "Use MIR and HIR analysis to derive call graph, CFG, reachability, and dataflow when diagnosing bugs or proving fixes.",
    "If a task conflicts with INVARIANTS.json, execute the invariant and report the conflict in `message.payload` so planner/verifier can update plan truth.",
];

const SOLO_EXECUTION_DISCIPLINE_BULLETS: &[&str] = &[
    "Prefer tasks explicitly marked ready / highest priority by the planner.",
    "Do not skip ahead to lower-priority or blocked tasks unless the current ready task is impossible and you have concrete evidence.",
    "Use the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.",
    "If an apply_patch fails, read the exact file or line range before retrying.",
    "Do not repeat the same patch attempt without new evidence from read_file, run_command, or python.",
    "When touching routing, policy, or control-flow code, favor the authority described in INVARIANTS.json over local heuristics.",
    "Use MIR and HIR analysis to derive call graph, CFG, reachability, and dataflow when diagnosing bugs or proving fixes.",
    "If a task conflicts with INVARIANTS.json, execute the invariant and report the conflict in `message.payload` so planner/verifier can update plan truth.",
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
    "- Reject any claimed completion that violates INVARIANTS.json invariants.",
    "- When using `message`, set:",
    "  - `from`: \"Verifier\"",
    "  - `to`: \"Planner\"",
    "  - `type`: \"verification\" or \"failure\"",
    "  - `status`: \"verified\" or \"failed\"",
    "  - `payload.summary`: string",
    "  - `payload.verified_items` / `payload.unverified_items` / `payload.false_items` as needed",
];

const PLANNER_RULES: &[&str] = &[
    "- `PLAN.json` MUST be valid JSON following the PLAN/TASK protocol in `SPEC.md`.",
    "- Only modify `PLAN.json` (via `plan`) and lane plans (via `apply_patch`) — never edit `src/`, `tests/`, `SPEC.md`, `VIOLATIONS.json`, or diagnostics reports.",
    "- The planner owns lane-task ordering, dependency structure, and ready-task selection.",
    "- Use agent_state/reports/complexity/latest.json (supervisor-generated; proxy complexity_proxy=mir_blocks) to prioritize refactors and reduce branching/duplication.",
    "- Read `ISSUES.json` every cycle and promote top open issues into `agent_state/OBJECTIVES.json` and `PLAN.json` (or explicitly mark them resolved/wontfix with evidence). Issues are hints; objectives/plan are commitments.",
    "- Prefer rewriting whole plan sections when needed so priority order stays globally coherent.",
    "- Keep each executor's ready window small: 1-10 tasks maximum.",
    "- Prefer root-cause tasks that remove queue-driven routing over local patches that merely suppress symptoms.",
    "- Send handoff messages to executors reflecting the current ready window.",
    "- If the incoming handoff was sent by solo (check `message.from` or context role), finish your planning cycle and send the return `message` to solo so it can resume execution. Do not leave solo waiting.",
    "- PLAN.json is the authoritative source of truth for executor task selection. A handoff message alone is not sufficient — the task MUST be marked `ready` in PLAN.json before the executor will pick it up.",
    "- Always use the `python` action when reading or writing any `.json` state file (PLAN.json, OBJECTIVES.json, ISSUES.json, VIOLATIONS.json, diagnostics). Never use apply_patch or run_command shell pipelines to mutate JSON; use the `plan`, `objectives`, or `issue` actions for their respective files.",
    "- Executor tasks must only require executor-permitted actions: read_file, apply_patch, run_command, python, cargo_fmt, cargo_clippy, semantic tools, and `plan set_task_status → done` (to mark its own task complete). The executor CANNOT use `objectives`, `issue`, or `verify` actions, and cannot use any plan op other than set_task_status → done. Reserve those actions for a planner or verifier cycle.",
    "- plan actions derived from diagnostics must cite same-cycle source validation in `observation` and `rationale` (for example current-cycle `read_file`, `run_command`, `python`, or other verified source evidence) before mutating `PLAN.json`.",
];

fn diagnostics_rules() -> Vec<String> {
    let mut rules = vec![
        "- Use the `python` action for structured analysis of project state and any available logs.".to_string(),
        "- Emit schema-valid actions only: every `python` action MUST include a non-empty `code` field; every `issue` `set_status` action MUST include the exact `issue_id`; every diagnostics report write MUST include evidence entries with current-source validation markers such as `read_file <path>:<lines>`, `validated against current source`, or equivalent cited source validation.".to_string(),
        "- Write ranked findings to PLANS/default/diagnostics-default.json AND open or update issues in ISSUES.json for every significant finding. Use the `issue` action (op=create or op=update) with kind, location, evidence, and priority. Issues carry provenance that the planner trusts; raw diagnostics output is supporting context only.".to_string(),
        "- For each ranked failure you report: if no matching ISSUES.json entry exists, create one with `issue` op=create. If one exists but is stale, update it with `issue` op=update and fresh evidence. If a prior issue is now resolved, close it with `issue` op=set_status status=resolved.".to_string(),
        "- Rank issues by impact on correctness, convergence, and repairability.".to_string(),
        "- Check control-flow and state-management decisions against INVARIANTS.json.".to_string(),
        "- Complexity report artifact (supervisor-generated): agent_state/reports/complexity/latest.json (proxy metric complexity_proxy=mir_blocks) for ranking refactor hotspots.".to_string(),
        "- Canonical log sources (read all before writing findings): agent_state/tlog.ndjson (state-machine event log), agent_state/enforced_invariants.json (runtime invariant registry), agent_state/lessons.json (synthesized behavioral lessons), agent_state/default/actions.jsonl (full action history — tail with python).".to_string(),
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
        "- Always read a file before patching it — but read each file ONCE. If a read_file result for that path already appears in this session's context, its content is available — do NOT read it again. Calling read_file on an already-seen path is a stall: emit apply_patch or message instead.".to_string(),
        "- For Rust source navigation prefer semantic tools over raw file access: semantic_map (semantic triples) → symbol_window (function body) → symbol_neighborhood / symbol_refs (call sites / references) → symbol_path (call chain). Use read_file only for non-Rust files or immediately before patching a Rust file to get line-numbered output.".to_string(),
        "- Use list_dir only to check whether a path exists or to enumerate non-source artifacts; use semantic_map to explore Rust semantic structure. Do not invent `symbol_search`; use `symbol_refs` or `symbol_window` instead.".to_string(),
        "- Only list_dir paths that exist under WORKSPACE; do not assume `canon-utils` exists unless WORKSPACE is `/workspace/ai_sandbox/canon`.".to_string(),
        "- Use run_command for cargo builds, tests, and shell discovery.".to_string(),
        "- Use cargo_fmt and cargo_clippy tools for formatting/linting. Both return exactly 3 lines (status/log/summary) and write full output under state/logs/.".to_string(),
        "- Complexity report artifact (supervisor-generated): agent_state/reports/complexity/latest.json (proxy metric complexity_proxy=mir_blocks) for hotspot targeting.".to_string(),
        "- Use python for structured analysis when shell pipelines are awkward.".to_string(),
        "- Always use the `python` action when reading or inspecting any `.json` state file (PLAN.json, OBJECTIVES.json, ISSUES.json, VIOLATIONS.json, diagnostics). Never use shell tools (cat, jq, grep) to read JSON — use python.".to_string(),
        "- Your work is scoped to the task_id provided in the planner handoff. Execute that specific task; do not pick up other PLAN.json tasks unless the planner explicitly includes them in the ready window.".to_string(),
        "- Each action you emit must include `task_id` and `objective_id` fields matching the current task. Never omit these provenance fields.".to_string(),
        "- When sending a `message` action, always set `\"from\": \"executor\"`. Never copy `from` values from other roles' messages in your context.".to_string(),
        "- When blocked or complete, send your `message` to `\"planner\"` — not to `diagnostics`, `verifier`, or other roles. The planner coordinates all role dispatch.".to_string(),
        format!("- Never operate outside {ws}."),
        "- Never modify `SPEC.md`, `VIOLATIONS.json`, or `PLANS/default/diagnostics-default.json`.".to_string(),
        "- The ONLY permitted PLAN.json mutation is `plan set_task_status → done` for the task you just completed. All other plan ops (create_task, update_task, add_edge, replace_plan, set_task_status→ready) are planner-only.".to_string(),
        "- Never emit destructive commands (rm -rf, git reset --hard, git clean -f, etc.).".to_string(),
    ];
    rules.extend(load_role_overrides(AgentPromptKind::Executor));
    rules
}

fn solo_rules() -> Vec<String> {
    let ws = crate::constants::workspace();
    let mut rules = vec![
        "- Always read a file before patching it — but read each file ONCE. If a read_file result for that path already appears in this session's context, its content is available — do NOT read it again. Calling read_file on an already-seen path is a stall: emit apply_patch or message instead.".to_string(),
        "- For Rust source navigation prefer semantic tools over raw file access: semantic_map (semantic triples) → symbol_window (function body) → symbol_neighborhood / symbol_refs (call sites / references) → symbol_path (call chain). Use read_file only for non-Rust files or immediately before patching a Rust file to get line-numbered output.".to_string(),
        "- Use list_dir only to check whether a path exists or to enumerate non-source artifacts; use semantic_map to explore Rust semantic structure. Do not invent `symbol_search`; use `symbol_refs` or `symbol_window` instead.".to_string(),
        "- Use run_command for cargo builds, tests, and shell discovery.".to_string(),
        "- Use cargo_fmt and cargo_clippy tools for formatting/linting. Both return exactly 3 lines (status/log/summary) and write full output under state/logs/.".to_string(),
        "- Complexity report artifact (supervisor-generated): agent_state/reports/complexity/latest.json (proxy metric complexity_proxy=mir_blocks) for hotspot targeting.".to_string(),
        "- Run cargo build/test before `message` with status=complete when changes affect code.".to_string(),
        "- If you rebuild canon-mini-agent, the supervisor may restart immediately in solo mode; be ready for a restart before the next step.".to_string(),
        "- You may modify any in-workspace files when justified by evidence; use the `plan` action for PLAN.json edits.".to_string(),
        format!("- Never operate outside {ws}."),
        "- Never emit destructive commands (rm -rf, git reset --hard, git clean -f, etc.).".to_string(),
        "- Use semantic tools only when they sharpen the immediate next step. Do not perform broad codebase sweeps unless the current task or failure surface requires them.".to_string(),
        "- Solo is not the broad issue-discovery lane. Use the `issue` action only when the current step directly exposes a concrete implementation gap with file/symbol/evidence in hand.".to_string(),
        "- When the inbound request is from `user`, prefer a bounded direct result message to `user` over broad system analysis unless a concrete execution step is clearly higher value.".to_string(),
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
    let intro = prompt_intro(kind).to_string();
    let mission = prompt_mission(kind).to_string();
    let workspace_text = prompt_workspace(kind);
    let status_snapshot = canonical_status_snapshot().to_string();
    let tail = prompt_tail(kind);
    let prefix = format!(
        "{}\n\n{}\n\n{}\n\n{}\n\n",
        intro, mission, workspace_text, status_snapshot
    );
    let schema_block = default_schema_block(kind);
    let suffix = format!(
        "\nAction protocol — respond with exactly one JSON code block matching one of these schemas. These are not function calls; emit the JSON in plain text:\n\n{schema_block}\n\nFull syntax examples with notes: agent_state/tool_examples.md — use read_file when you need a reminder.\n\n{}",
        tail
    );
    render_budgeted_prompt(&prefix, &[], &suffix)
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
    semantic_control_text: &str,
    plan_diff: &str,
    executor_diff: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_file = diagnostics_file();
    let issues_file = crate::constants::ISSUES_FILE;
    let prefix = format!(
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
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let lessons_heading = "Lessons artifact:".to_string();
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let executor_diff_heading =
        "Executor diff (workspace changes excluding plans/diagnostics/violations)".to_string();
    let summary_heading = "Latest verifier summary".to_string();
    let suffix = format!(
        "\n\n\
         ⟹ IMMEDIATE ACTION: The projected issues in the semantic control section above are \
         pre-verified by diagnostics and directly actionable. Do not re-verify them — the \
         diagnostics role already did that. Create `plan` tasks for the top open issues now, \
         mark them `ready`, and send an executor handoff message.\n\n\
         Before completing this cycle, review {OBJECTIVES_FILE} and add or update objectives \
         for anything discovered this cycle. Use the `objectives` action \
         (op: create_objective / update_objective) to write them. \
         NEVER use apply_patch for {MASTER_PLAN_FILE} — it is always rejected; use the `plan` action.\
         \n\nYou may send a message action to other agents at any time. Think hard internally before responding."
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
        PromptItem {
            heading: &summary_heading,
            body: summary_text,
            reserve: 600,
            cap: 1500,
            weight: 2,
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
    let issues_file = crate::constants::ISSUES_FILE;
    let prompt = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nCanonical references:\n- Spec: {SPEC_FILE}\n- Objectives: {OBJECTIVES_FILE}\n- Invariants: {INVARIANTS_FILE}\n- Violations projection to keep synchronized: {VIOLATIONS_FILE}\n- Diagnostics report to write: {diagnostics_file}\n- Issues projection to keep synchronized: {issues_file}\n- Lessons candidates (synthesized from action log): agent_state/lessons_candidates.json\n- Promoted lessons (injected into planner): agent_state/lessons.json\n- Discovered invariants (synthesized from blockers + action log): agent_state/enforced_invariants.json\n- Classified failure log (first-class blocker artifact): agent_state/blockers.json\n- Observability artifacts: inspect workspace-local state and log paths that actually exist for this project\n\nLatest verifier summary:\n{summary_text}\n\nLatest cargo test failures (from cargo_test_failures.json):\n{cargo_test_failures}\n\nDiagnostics output protocol:\n1. Derive ranked failures from current evidence and semantic control summaries.\n2. Project those failures into {diagnostics_file}, {issues_file}, and {VIOLATIONS_FILE}.\n   - op=create (with kind, location, evidence, priority) if no matching open issue exists.\n   - op=update if a matching issue exists but its evidence is stale.\n   - op=set_status status=resolved if a prior issue is no longer supported by evidence.\n   Artifact files are supporting projections; semantic control state owns planner follow-up.\n3. Do not re-report failures the verifier has already cleared unless fresh current-cycle evidence reconfirms them.\n\nExecution surface guarantee:\n- This diagnostics turn is tool-capable. `python`, `read_file`, `issue`, `violation`, `apply_patch`, and `message` are executable from this role.\n- Do not claim the diagnostics channel is text-only or missing tools. Use a `python` action first to read workspace-local state/log artifacts and establish current-cycle evidence before any blocker or mutation.\n\nLessons review (optional, do after main diagnostics work):\n- Use `lessons` op=read_candidates to inspect pending patterns detected from the action log.\n- Promote candidates that reflect real, recurring patterns (op=promote with candidate_id).\n- Reject candidates that are coincidental or already obvious (op=reject).\n- Promoted patterns appear in lessons.json and are injected into every future planner prompt.\n\nInvariant review (optional, do after lessons review):\n- Use `invariants` op=read to inspect dynamically discovered system invariants from enforced_invariants.json.\n- Invariants are synthesized from agent_state/blockers.json (classified bad outcomes) and the action log.\n- Status lifecycle: discovered → promoted (auto at support_count>=3) → enforced (hard gate) → collapsed (root cause fixed).\n- If a Promoted invariant has a sound predicate, call `invariants` op=enforce to make the gate hard-blocking.\n- If a root cause has been structurally eliminated, call `invariants` op=collapse with a rationale.\n- Enforced invariants block route/planner/executor dispatch before the transition is taken — zero wasted turns.\n\nYou may send a message action to other agents at any time.Think hard internally before responding."
    );
    prompt.replace(
        "Use a `python` action first to read workspace-local state/log artifacts and establish current-cycle evidence before any blocker or mutation.",
        "",
    )
}

pub(crate) fn single_role_verifier_prompt(
    _primary_input: &str,
    objectives: &str,
    semantic_control: &str,
    executor_diff_text: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed."
    );
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let executor_diff_heading =
        "Executor diff (workspace changes excluding plans/diagnostics/violations)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let suffix = format!(
        "\n\nVerify that objectives in {OBJECTIVES_FILE} are completed properly.\nUpdate task status fields in {MASTER_PLAN_FILE} to reflect verified results.\nWrite violations to {VIOLATIONS_FILE} if any are found.\nWhen complete, report verified/unverified/false items in `message.payload`.\nEmit exactly one action to begin. Think through the decision internally; reveal chain-of-thought."
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
            heading: &semantic_control_heading,
            body: semantic_control,
            reserve: 800,
            cap: 2500,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &executor_diff_heading,
            body: executor_diff_text,
            reserve: 800,
            cap: 3000,
            weight: 3,
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

pub(crate) fn single_role_diagnostics_prompt(
    objectives: &str,
    semantic_control: &str,
    cargo_test_failures: &str,
) -> String {
    let workspace = workspace();
    let diagnostics_path = diagnostics_file();
    let issues_file = ISSUES_FILE;
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nRead files and search the source code for bugs and inconsistencies (use read_file + run_command/ripgrep).\nRun python analysis actions over available workspace-local logs, state, and code evidence.\nDo not assume canon-specific observability names or paths. Discover the actual project-local artifacts first by inspecting files and directories that exist under WORKSPACE. Examples may include state/, log/, logs, runtime logs, jsonl logs, agent logs, or other workspace-defined artifacts.\nInfer the root cause from the evidence and cite detailed sources of errors (file paths, functions, log evidence).\n\nLatest verifier summary:\n(none yet)"
    );
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let suffix = format!(
        "\n\nVerify whether objectives in {OBJECTIVES_FILE} are being met and note gaps.\nUse {SPEC_FILE}, {OBJECTIVES_FILE}, and {INVARIANTS_FILE} as the contract, not lane plans.\nInfer failures from code, logs, runtime state, and verifier findings.\nPrefer evidence from workspace-local artifacts that actually exist over assumptions from other projects.\nTreat {VIOLATIONS_FILE}, {issues_file}, and {diagnostics_path} as derived projections to keep synchronized with current evidence, not as control authority.\nDo not restate verifier-cleared or already-resolved issues unless fresh current-cycle source or runtime evidence reconfirms them.\nIf the mismatch is stale projected state rather than a live implementation bug, repair the projection instead of reopening the cleared issue.\n\nWrite a ranked diagnostics report to {diagnostics_path}."
    );
    let items = [
        PromptItem {
            heading: &semantic_control_heading,
            body: semantic_control,
            reserve: 1000,
            cap: 3000,
            weight: 4,
            always_include: true,
        },
        PromptItem {
            heading: &objectives_heading,
            body: objectives,
            reserve: 800,
            cap: 2500,
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

pub(crate) fn single_role_planner_prompt(
    _primary_input: &str,
    objectives: &str,
    lessons_text: &str,
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
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json)".to_string();
    let suffix = format!(
        "\n\nUse {INVARIANTS_FILE} when deriving plan constraints.\nRead files and search the source code before issuing plan changes.\nOpen issues in `{issues_file}` (written by diagnostics with evidence) are directly actionable — create plan tasks for them without re-verifying. `{diagnostics_path}` entries with no matching {issues_file} entry are hints only.\nWrite imperative, actionable instructions in {MASTER_PLAN_FILE}.\nOnly use plan diffs when available; avoid re-reading the full plan unless necessary.\nDo not use internal tools.\nDo not hand off work; keep planning and execution in the current role flow.\nWhen a `plan` action is derived from diagnostics, include same-cycle source validation in `observation` and `rationale` before mutating {MASTER_PLAN_FILE}.\n\nTreat stale or already-resolved diagnostics as non-actionable until current source evidence reconfirms them.\nIf diagnostics repeatedly report stale issues, create follow-up work to repair diagnostics generation rather than reopening resolved implementation tasks."
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

pub(crate) fn single_role_solo_prompt(
    _spec: &str,
    master_plan: &str,
    objectives: &str,
    lessons_text: &str,
    semantic_control: &str,
    cargo_test_failures: &str,
    rename_candidates: &str,
    executor_diff_text: &str,
    plan_diff_text: &str,
    complexity_hotspots: &str,
    loop_context_hint: &str,
) -> String {
    let workspace = workspace();
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSolo role: bounded execution kernel. Take one grounded next step under canonical law, using the smallest evidence slice needed. Prefer direct execution, targeted inspection, or a direct result message to `user`; do not behave like planner/diagnostics/verifier.\n\nSpec: {SPEC_FILE} — use read_file only for sections you need.\n\nMaster plan focus (from {MASTER_PLAN_FILE}):\n{}",
        truncate_section(master_plan, 3000)
    );
    let plan_diff_heading = format!("Plan diff since last cycle (from {MASTER_PLAN_FILE})");
    let executor_diff_heading =
        "Workspace diff since last cycle (git diff, excluding plans/diagnostics/violations)"
            .to_string();
    let objectives_heading = format!("Objectives (from {OBJECTIVES_FILE})");
    let lessons_heading = "Lessons artifact:".to_string();
    let loop_context_heading =
        "Repair loop context (supervisor-directed; focus on this target first)".to_string();
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let complexity_heading =
        "Complexity hotspots (supervisor-generated; use only when directly relevant)".to_string();
    let cargo_failures_heading =
        "Latest cargo test failures (from cargo_test_failures.json):".to_string();
    let rename_heading = "Pending rename tasks (from state/rename_candidates.json):".to_string();
    let objectives_body = truncate_section(objectives, 1200);
    let semantic_control_body = truncate_section(semantic_control, 3000);
    let suffix = "\n\nUse the `plan` action for `PLAN.json` edits; do not apply_patch the master plan.\nUse the `issue` action only when the current step uncovers direct implementation evidence for a new or stale logic gap.\nFor Rust source investigation, use semantic tools first: symbol_refs, symbol_window, symbol_neighborhood, symbol_path, semantic_map. Reach for read_file only when you need exact lines before a patch.\nOutput contract (strict):\n- Return exactly ONE action\n- Format: a single JSON object in a ```json code block\n- No prose, no markdown explanation outside the JSON block\n- Optimize for the next correct move, not broad analysis\n\nIf replying to the external user, use this shape:\n```json\n{\n  \"action\": \"message\",\n  \"from\": \"solo\",\n  \"to\": \"user\",\n  \"type\": \"result\",\n  \"status\": \"ready\",\n  \"observation\": \"State the grounded evidence you are replying from.\",\n  \"rationale\": \"Explain why a direct reply is the highest-value next step.\",\n  \"predicted_next_actions\": [\n    {\"action\": \"read_file\", \"intent\": \"Inspect a named artifact if the user asks for deeper evidence next.\"},\n    {\"action\": \"message\", \"intent\": \"Send a narrower follow-up reply to the user after targeted inspection.\"}\n  ],\n  \"payload\": {\n    \"summary\": \"Short direct answer to the user.\"\n  }\n}\n```\nEmit exactly one action.";
    let mut items = vec![
        PromptItem {
            heading: &plan_diff_heading,
            body: plan_diff_text,
            reserve: 1200,
            cap: 4000,
            weight: 5,
            always_include: true,
        },
        PromptItem {
            heading: &executor_diff_heading,
            body: executor_diff_text,
            reserve: 1000,
            cap: 4000,
            weight: 5,
            always_include: false,
        },
        PromptItem {
            heading: &objectives_heading,
            body: &objectives_body,
            reserve: 800,
            cap: 2000,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &lessons_heading,
            body: lessons_text,
            reserve: 800,
            cap: 1800,
            weight: 4,
            always_include: true,
        },
        PromptItem {
            heading: &loop_context_heading,
            body: loop_context_hint,
            reserve: 600,
            cap: 2000,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &semantic_control_heading,
            body: &semantic_control_body,
            reserve: 1200,
            cap: 3000,
            weight: 4,
            always_include: false,
        },
        PromptItem {
            heading: &complexity_heading,
            body: complexity_hotspots,
            reserve: 600,
            cap: 1800,
            weight: 3,
            always_include: false,
        },
        PromptItem {
            heading: &cargo_failures_heading,
            body: cargo_test_failures,
            reserve: 600,
            cap: 1800,
            weight: 3,
            always_include: false,
        },
    ];
    let rename_section = if !rename_candidates.trim().is_empty() {
        Some(format!(
            "{rename_candidates}\nFor each candidate: use `symbols_prepare_rename` to select it, then `rename_symbol` to apply. Work through them in score-descending order."
        ))
    } else {
        None
    };
    if let Some(rename_section) = rename_section.as_ref() {
        items.push(PromptItem {
            heading: &rename_heading,
            body: rename_section,
            reserve: 800,
            cap: 3000,
            weight: 4,
            always_include: false,
        });
    }
    let mut prefix = prefix;
    if !prefix.ends_with("\n\n") {
        prefix.push_str("\n\n");
    }
    render_budgeted_prompt(&prefix, &items, suffix)
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
    let suffix = "\n\nLane plans are deprecated. Use planner handoff messages and {MASTER_PLAN_FILE} for task selection.\n\nDo not modify spec, plan, violations, or diagnostics.\nDo not use internal tools.\nDo not hand off work; continue execution directly in the current role flow.\nUse `message.payload` to report evidence for verifier review. Emit exactly one action to begin. Think through the decision internally; reveal chain-of-thought.";
    let items = vec![PromptItem {
        heading: &semantic_control_heading,
        body: semantic_control,
        reserve: 800,
        cap: 3000,
        weight: 4,
        always_include: true,
    }];
    render_budgeted_prompt(&prefix, &items, suffix)
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
            "semantic_control",
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
            "semantic_control",
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
            "semantic_control",
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
            "semantic_control",
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

fn validate_blocker_message_payload(msg: &ProtocolMessage) -> Result<()> {
    if !(matches!(msg.msg_type, MessageType::Blocker)
        || matches!(msg.status, MessageStatus::Blocked))
    {
        return Ok(());
    }
    match &msg.payload {
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
    let _ =
        serde_json::from_value::<crate::protocol::Severity>(Value::String(severity.to_string()))
            .map_err(|_| anyhow!("message severity must be one of: info|warn|error|critical"))?;
    Ok(())
}

fn validate_optional_message_role(obj: &serde_json::Map<String, Value>, field: &str) -> Result<()> {
    let Some(role) = obj.get(field) else {
        return Ok(());
    };
    let _ = serde_json::from_value::<Role>(role.clone()).map_err(|_| {
        anyhow!("{field} must be one of: user|executor|planner|verifier|diagnostics|solo")
    })?;
    Ok(())
}

fn validate_message_route(msg: &ProtocolMessage) -> Result<()> {
    let self_routed = std::mem::discriminant(&msg.from) == std::mem::discriminant(&msg.to);
    let allow_solo_self_complete = matches!(msg.from, Role::Solo)
        && matches!(msg.to, Role::Solo)
        && matches!(msg.msg_type, MessageType::Result)
        && matches!(msg.status, MessageStatus::Complete);
    if self_routed && !allow_solo_self_complete {
        bail!(
            "message route may not target the emitting role; only solo result/complete may self-route"
        );
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
    let kind = agent_kind_from_agent_type(agent_type);
    let schema_block = {
        let predicted = predicted_action_schema_block(predicted_next_actions);
        if predicted.trim().is_empty() {
            format!(
                "Action protocol — respond with exactly one JSON code block matching one of these schemas:\n{}",
                default_schema_block(kind)
            )
        } else {
            predicted
        }
    };
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

    let prefix = format!(
        "TAB_ID: {tab_label}\nTURN_ID: {turn_label}\nAGENT_TYPE: {agent_type}\n\n{limit_line}{provenance_block}"
    );
    let schema_section = if schema_block.is_empty() {
        String::new()
    } else {
        format!("\n{schema_block}\n")
    };
    let suffix = format!(
        "\n\n{predicted_line}{schema_section}{}{}\nEmit exactly one action. Think through the decision internally; reveal chain-of-thought.",
        next_action_hint_text(result, last_action),
        mutating_question,
    );
    render_action_result_sections(&prefix, result, &suffix)
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
    fn validate_allows_solo_result_self_route() {
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
    fn planner_rules_require_promoting_issues_into_objectives_and_plan() {
        let rules = PLANNER_RULES.join("\n");
        assert!(
            rules.contains("Read `ISSUES.json` every cycle"),
            "planner rules must require consuming ISSUES.json"
        );
        assert!(
            rules.contains(
                "promote top open issues into `agent_state/OBJECTIVES.json` and `PLAN.json`"
            ),
            "planner rules must require promoting issues into objectives/plan"
        );
    }

    #[test]
    fn planner_system_instructions_include_tool_schema_block() {
        let prompt = system_instructions(AgentPromptKind::Planner);
        assert!(
            prompt.contains("Action protocol — respond with exactly one JSON code block matching one of these schemas"),
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
            prompt.contains("Schema-derived `plan.op` values:"),
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
    }

    #[test]
    fn diagnostics_system_instructions_include_tool_schema_block() {
        let prompt = system_instructions(AgentPromptKind::Diagnostics);
        assert!(
            prompt.contains("Action protocol — respond with exactly one JSON code block matching one of these schemas"),
            "diagnostics system prompt should include an introductory schema block"
        );
        assert!(
            prompt.contains("Action: `python`"),
            "diagnostics system prompt should include the python schema"
        );
        assert!(
            prompt.contains("Action: `issue`"),
            "diagnostics system prompt should include the issue schema"
        );
    }

    #[test]
    fn executor_system_instructions_include_tool_schema_block() {
        let prompt = system_instructions(AgentPromptKind::Executor);
        assert!(
            prompt.contains("Action protocol — respond with exactly one JSON code block matching one of these schemas"),
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
    }

    #[test]
    fn predicted_action_schema_block_renders_only_predicted_actions() {
        let predicted = r#"[
            {"action":"issue","intent":"update stale issue"},
            {"action":"violation","intent":"refresh violations"},
            {"action":"message","intent":"report blocker if needed"}
        ]"#;
        let schema = predicted_action_schema_block(Some(predicted));
        assert!(schema.contains("Action: `issue`"));
        assert!(schema.contains("Action: `violation`"));
        assert!(schema.contains("Action: `message`"));
        assert!(!schema.contains("Action: `python`"));
        assert!(!schema.contains("Action: `read_file`"));
        assert!(!schema.contains("Action: `run_command`"));
    }

    #[test]
    fn predicted_action_schema_block_dedups_repeated_actions_preserving_order() {
        let predicted = r#"[
            {"action":"issue","intent":"first issue action"},
            {"action":"issue","intent":"repeated issue action"},
            {"action":"message","intent":"handoff blocker"},
            {"action":"issue","intent":"repeated again"}
        ]"#;
        let schema = predicted_action_schema_block(Some(predicted));
        assert!(schema.contains("Action: `issue`"));
        assert!(schema.contains("Action: `message`"));
        assert_eq!(schema.matches("Action: `issue`").count(), 1);
        assert_eq!(schema.matches("Action: `message`").count(), 1);
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
            "DIAGNOSTICS",
            "python ok:\nEvidence receipt: rcpt-123\n{}",
            Some("python"),
            Some("T1"),
            Some("obj_15_automated_learning_loop"),
            Some("Inspect diagnostics state"),
            None,
            Some(predicted),
        );
        assert!(prompt.contains("Derived schemas for predicted next actions:"));
        assert_eq!(prompt.matches("Action: `issue`").count(), 1);
        assert_eq!(prompt.matches("Action: `violation`").count(), 1);
        assert_eq!(prompt.matches("Action: `message`").count(), 1);
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
        assert!(prompt.contains("Action protocol — respond with exactly one JSON code block matching one of these schemas"));
        assert!(prompt.contains("Action: `read_file`"));
        assert!(prompt.contains("Action: `apply_patch`"));
        assert!(prompt.contains("Action: `run_command`"));
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
            "{semantic_control}",
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
            "{semantic_control}",
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
            "{semantic_control}",
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
            "{semantic_control}",
            "{cargo_test_failures}",
        );
        assert!(
            prompt.contains("Lessons artifact:\nLESSON_TEXT"),
            "planner prompt must embed the lessons artifact body"
        );
    }
}

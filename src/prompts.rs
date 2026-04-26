use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::constants::{
    diagnostics_file, workspace, EXECUTOR_STEP_LIMIT, INVARIANTS_FILE, ISSUES_FILE,
    MASTER_PLAN_FILE, OBJECTIVES_FILE, PIPELINE_FILE, SPEC_FILE, VIOLATIONS_FILE,
};
use crate::objectives::load_master_plan_snapshot;
use crate::prompt_contract::ACTION_EMIT_LINE;
use crate::protocol::{
    BlockerPayload, MessagePayload, MessageStatus, MessageType, ProtocolMessage, Role,
};
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &prompts::PromptBudgetItem<'_>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PromptTruncationRecord {
    pub heading: String,
    pub raw_bytes: usize,
    pub kept_bytes: usize,
    pub dropped_bytes: usize,
    pub policy: String,
    pub body_hash: String,
}

fn prompt_truncation_marker(item: &PromptBudgetItem<'_>, kept_bytes: usize) -> String {
    let dropped_bytes = item.raw_bytes.saturating_sub(kept_bytes);
    let record = PromptTruncationRecord {
        heading: item.heading.to_string(),
        raw_bytes: item.raw_bytes,
        kept_bytes,
        dropped_bytes,
        policy: "preserve_failure_lines_head_tail".to_string(),
        body_hash: crate::logging::stable_hash_hex(item.body),
    };
    let meta = serde_json::to_string(&record).unwrap_or_else(|_| {
        format!(
            "{{\"heading\":\"{}\",\"raw_bytes\":{},\"kept_bytes\":{},\"dropped_bytes\":{},\"policy\":\"preserve_failure_lines_head_tail\",\"body_hash\":\"{}\"}}",
            item.heading.replace('"', "'"),
            item.raw_bytes,
            kept_bytes,
            dropped_bytes,
            crate::logging::stable_hash_hex(item.body)
        )
    });
    format!("\n... [prompt_truncation {meta}]")
}

fn is_high_signal_prompt_line(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    lowered.contains("error:")
        || lowered.contains("error[")
        || lowered.contains("failed")
        || lowered.contains("failures:")
        || lowered.contains("panicked at")
        || lowered.contains("thread '")
        || lowered.contains("invalid context")
        || lowered.contains("permission denied")
        || lowered.contains("no such file")
        || lowered.contains("traceback")
        || lowered.contains("exception")
        || lowered.contains("expected")
        || lowered.contains("actual")
        || lowered.contains("test result:")
        || lowered.contains("could not compile")
}

fn truncate_suffix_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn push_budgeted_line(out: &mut String, line: &str, budget: usize) {
    if budget == 0 || out.len() >= budget {
        return;
    }
    if !out.is_empty() {
        if out.len() + 1 > budget {
            return;
        }
        out.push('\n');
    }
    let remaining = budget.saturating_sub(out.len());
    out.push_str(truncate_bytes(line, remaining));
}

fn high_signal_prompt_excerpt(body: &str, budget: usize) -> String {
    let mut out = String::new();
    for line in body.lines().filter(|line| is_high_signal_prompt_line(line)).take(12) {
        let compact = truncate_bytes(line.trim(), 240);
        push_budgeted_line(&mut out, compact, budget);
        if out.len() >= budget {
            break;
        }
    }
    out
}

fn render_head_tail_excerpt(body: &str, budget: usize) -> String {
    const MIDDLE_MARKER: &str = "\n\n... [middle omitted]\n\n";
    if body.len() <= budget {
        return body.to_string();
    }
    if budget <= MIDDLE_MARKER.len() + 8 {
        return truncate_bytes(body, budget).to_string();
    }
    let content_budget = budget - MIDDLE_MARKER.len();
    let head_budget = content_budget / 2;
    let tail_budget = content_budget.saturating_sub(head_budget);
    let mut out = String::new();
    out.push_str(truncate_bytes(body, head_budget));
    out.push_str(MIDDLE_MARKER);
    out.push_str(truncate_suffix_bytes(body, tail_budget));
    out
}

fn render_preserved_truncated_body(body: &str, content_budget: usize) -> String {
    if body.len() <= content_budget {
        return body.to_string();
    }
    if content_budget < 512 {
        return render_head_tail_excerpt(body, content_budget);
    }

    const SIGNAL_HEADER: &str = "Preserved high-signal lines:\n";
    const HEAD_HEADER: &str = "\n\nHead:\n";
    const TAIL_HEADER: &str = "\n\nTail:\n";

    let signal_budget = (content_budget / 3).min(2048);
    let signal = high_signal_prompt_excerpt(body, signal_budget);
    if signal.trim().is_empty() {
        return render_head_tail_excerpt(body, content_budget);
    }

    let framing = SIGNAL_HEADER.len() + HEAD_HEADER.len() + TAIL_HEADER.len();
    if content_budget <= framing + signal.len() + 16 {
        return render_head_tail_excerpt(body, content_budget);
    }
    let remaining = content_budget - framing - signal.len();
    let head_budget = remaining / 2;
    let tail_budget = remaining.saturating_sub(head_budget);

    let mut out = String::with_capacity(content_budget);
    out.push_str(SIGNAL_HEADER);
    out.push_str(&signal);
    out.push_str(HEAD_HEADER);
    out.push_str(truncate_bytes(body, head_budget));
    out.push_str(TAIL_HEADER);
    out.push_str(truncate_suffix_bytes(body, tail_budget));
    debug_assert!(out.len() <= content_budget);
    out
}

/// Extract machine-readable prompt truncation receipts embedded by the prompt
/// renderer. This lets request logging append explicit tlog effects instead of
/// silently hiding dropped evidence.
pub(crate) fn prompt_truncation_records(prompt: &str) -> Vec<PromptTruncationRecord> {
    const PREFIX: &str = "... [prompt_truncation ";
    let mut records = Vec::new();
    let mut cursor = prompt;

    while let Some(prefix_idx) = cursor.find(PREFIX) {
        let after_prefix = &cursor[prefix_idx + PREFIX.len()..];
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut json_start = None;
        let mut json_end = None;

        for (idx, ch) in after_prefix.char_indices() {
            if json_start.is_none() {
                if ch == '{' {
                    json_start = Some(idx);
                    depth = 1;
                }
                continue;
            }

            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }

            match ch {
                '"' => in_string = true,
                '{' => depth = depth.saturating_add(1),
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        json_end = Some(idx + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }

        if let (Some(start), Some(end)) = (json_start, json_end) {
            if let Ok(record) =
                serde_json::from_str::<PromptTruncationRecord>(&after_prefix[start..end])
            {
                records.push(record);
            }
            cursor = &after_prefix[end..];
        } else {
            break;
        }
    }

    records
}

fn render_budgeted_item_body(item: &PromptBudgetItem<'_>) -> String {

    if item.budget == 0 {
        return String::new();
    }

    if item.budget >= item.raw_bytes {
        return item.body.to_string();
    }

    let marker = prompt_truncation_marker(item, item.budget);
    if item.budget <= marker.len() {
        return truncate_bytes(&marker, item.budget).to_string();
    }
    render_budgeted_truncated_item_body(item, marker)
}

fn render_budgeted_truncated_item_body(item: &PromptBudgetItem<'_>, marker: String) -> String {
    let content_budget = item.budget - marker.len();
    let mut out = render_preserved_truncated_body(item.body, content_budget);
    let mut marker = prompt_truncation_marker(item, out.len());
    if out.len() + marker.len() > item.budget {
        let adjusted_budget = item.budget.saturating_sub(marker.len());
        out = render_preserved_truncated_body(item.body, adjusted_budget);
        marker = prompt_truncation_marker(item, out.len());
    }
    out.push_str(&marker);
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

/// Intent: pure_transform
/// Resource: prompt_budget
/// Inputs: usize, usize, &[prompts::PromptItem<'_>]
/// Outputs: prompts::PromptBudget<'_>
/// Effects: none
/// Forbidden: mutation
/// Invariants: item budgets never exceed caps; used is recomputed from assigned item budgets; allocation respects available bytes after framing
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: prompt_text
/// Inputs: &str, &[prompts::PromptItem<'_>], &str
/// Outputs: std::string::String
/// Effects: renders budgeted prompt text without mutation
/// Forbidden: filesystem writes, state mutation, process spawning, network access
/// Invariants: excludes empty non-required items; output stays within prompt overflow byte budget in debug assertions
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
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
            "invariants",
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
    }
}

pub(crate) fn role_default_schema_actions_for_role(role: &str) -> &'static [&'static str] {
    let role = role.trim().to_ascii_lowercase();
    if role.starts_with("executor") {
        role_default_schema_actions(AgentPromptKind::Executor)
    } else if role.starts_with("verifier") || role.starts_with("diagnostics") {
        role_default_schema_actions(AgentPromptKind::Planner)
    } else {
        role_default_schema_actions(AgentPromptKind::Planner)
    }
}

/// Intent: pure_transform
/// Resource: predicted_action_names
/// Inputs: std::option::Option<&str>
/// Outputs: std::vec::Vec<std::string::String>
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns empty list for missing or invalid JSON input; otherwise extracts action string fields from predicted-next-action array items
/// Failure: none
/// Provenance: rustc:facts + rustc:docstring
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
    }
}

const CANONICAL_PIPELINE_CONTRACT: &str = "\
S = project(T)\n\
E = score(S, I, graph, issues, objectives, deltas)\n\
P = plan(E)\n\
X = execute(P)\n\
V = verify(X)\n\
G = regenerate(graph, issues) after apply_patch_ok ∧ cargo_check_ok\n\
T' = append(T, effects(X, V, G))\n\
L = learn(failure_event) only when it changes invariant/eval/test/prompt behavior\n\
C_allowed = no_rust_change ∨ (cargo_check_ok ∧ cargo_test_ok ∧ cargo_build_ok)\n\
reload_proven = SupervisorRestartRequested ∧ SupervisorChildStarted(binary_path, mtime)\n\
Order: observe truth → eval → plan ready work → execute bounded patch → verify gates → regenerate projections → append tlog effects → learn → gated commit.";

fn canonical_pipeline_prompt_block() -> String {
    format!(
        "Canonical pipeline contract (from {PIPELINE_FILE}; load that file for full details):\n{}",
        CANONICAL_PIPELINE_CONTRACT
    )
}

fn prompt_mission(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "All actions (`read_file`, `apply_patch`, `run_command`, `plan`, `message`, etc.) are JSON you emit in your response text — they are not function calls or external tools.\nMake source changes, run checks, and report evidence in `message.payload`.",
        AgentPromptKind::Planner => "Your job is to read `SPEC.md`, `agent_state/OBJECTIVES.json`, and the semantic-control snapshot in this prompt, then derive the master plan plus executable next-step guidance for the same operational loop.\nThe semantic-control snapshot is the tlog-derived authority for routing/control and projects issues, violations, invariants, and eval into one view.\nOn every cycle, re-evaluate the workspace and update `PLAN.json` via the `plan` action (emit it as JSON in your response).\nAt the end of every planner cycle, review `agent_state/OBJECTIVES.json` and add or update objectives using the `objectives` action (emit it as JSON in your response).\nPlan from eval first: clear `eval_gate=fail` violations before optional cleanup, otherwise choose ready tasks that improve the weakest eval dimension. Treat projected issue score as a candidate-ranking signal, not the final priority authority.\nEach ready task must name the expected Δeval, the weakest eval dimension it targets, and the validation command/evidence needed to prove improvement.\nWhen the plan has ready tasks and your analysis is complete, terminate this cycle with a `message` action: `{\"action\":\"message\",\"from\":\"planner\",\"to\":\"executor\",\"type\":\"handoff\",\"status\":\"ready\",\"observation\":\"Ready tasks queued.\",\"rationale\":\"Planner cycle complete.\",\"predicted_next_actions\":[]}`.\nDo not use `message` for intermediate progress tracking — only as the terminal handoff signal or a blocker escalation.\nAll actions (`plan`, `objectives`, `issue`, `message`, `read_file`, etc.) are JSON you emit in your response text — they are not function calls or external tools.\nPlans must follow the JSON PLAN/TASK protocol in `SPEC.md`.",
    }
}

fn prompt_workspace(kind: AgentPromptKind) -> String {
    let ws = crate::constants::workspace();
    match kind {
        AgentPromptKind::Executor => format!("You work inside the canon workspace at {ws}. All relative file paths resolve against this workspace root."),
        AgentPromptKind::Planner => format!("You work inside the canon workspace at {ws}. Use read_file, semantic_map/symbol_window/symbol_refs (prefer over read_file for Rust source), python, and run_command to review the current project state before reorganizing the plan. Planner role cannot use apply_patch."),
    }
}

fn prompt_graph_artifact_guidance(kind: AgentPromptKind) -> &'static str {
    match kind {
        AgentPromptKind::Executor => "Graph-first execution guidance:\n\
- Treat `state/rustc/canon_mini_agent/graph.json` as the canonical semantic/CFG substrate.\n\
- Use the `python` action for structured JSON/NDJSON analysis of `state/rustc/canon_mini_agent/graph.json`, `agent_state/tlog.ndjson`, `agent_state/safe_patch_candidates.json`, and `agent_state/semantic_manifest_proposals.json`; do not use raw text reads for counts, rankings, event timelines, or schema inspection.\n\
- Use `agent_state/safe_patch_candidates.json` to prioritize merge/delete style refactors before ad-hoc edits.\n\
- Use `agent_state/semantic_manifest_proposals.json` to preserve/repair Intent/Inputs/Outputs/Effects/Invariants contracts while editing.\n\
- When a task references an issue family rooted in graph analysis, ground file edits and verification in these graph-derived artifacts.",
        AgentPromptKind::Planner => "Graph-first planning guidance:\n\
- Treat `state/rustc/canon_mini_agent/graph.json` as the canonical semantic/CFG substrate.\n\
- Use the `python` action for structured JSON/NDJSON analysis of `state/rustc/canon_mini_agent/graph.json`, `agent_state/tlog.ndjson`, `agent_state/safe_patch_candidates.json`, and `agent_state/semantic_manifest_proposals.json`; do not use raw text reads for counts, rankings, event timelines, or schema inspection.\n\
- Prefer top-ranked entries in `agent_state/safe_patch_candidates.json` when creating ready executor tasks.\n\
- Use `agent_state/semantic_manifest_proposals.json` to ensure tasks preserve explicit intent/contract metadata.\n\
- Use issue scores only inside the current weakest eval target; when expected Δeval is close, favor tasks backed by graph-ranked redundancy evidence over generic heuristics.",
    }
}

fn planner_artifact_review_protocol() -> String {
    let diagnostics_path = diagnostics_file();
    format!(
        "Planner role contract:\n\
         - You are the autonomous system-development planner: audit evidence, diagnose root cause, \
         update objectives/plan, and hand off bounded executor tasks.\n\
         - Do not act as a passive dispatcher. Before creating or changing ready tasks, reconcile \
         {SPEC_FILE}, {PIPELINE_FILE}, {OBJECTIVES_FILE}, {MASTER_PLAN_FILE}, \
         `agent_state/tlog.ndjson` or `agent_state/default/actions.jsonl` fallback, \
         `agent_state/evidence_receipts.jsonl`, `state/rustc/canon_mini_agent/graph.json`, \
         `agent_state/reports/complexity/latest.json`, `agent_state/safe_patch_candidates.json`, \
         `agent_state/semantic_manifest_proposals.json`, \
         {ISSUES_FILE}, {VIOLATIONS_FILE}, `{diagnostics_path}`, \
         `agent_state/enforced_invariants.json`, `agent_state/lessons.json`, latest cargo failures, \
         executor diff, and latest `agent_state/llm_full/*planner*` / `*executor*` prompts when present.\n\
         - Every `plan`, `objectives`, and terminal `message` rationale must name the artifact evidence, \
         the weakest eval dimension or eval_gate violation, and the delta it revealed.\n\
         - If required artifacts are missing, stale, or contradictory, create repair work for \
         projection/logging/prompt generation before generic implementation work."
    )
}

fn status_snapshot_for(kind: AgentPromptKind) -> &'static str {
    let _ = kind;
    ""
}

const PLANNER_PROCESS: &str = "━━━ PLANNING PROCESS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n\
⚠ PLAN.json EDIT RULE: use ONLY the `plan` action for all PLAN.json changes. \
Planner role cannot use apply_patch.\n\n\
On every planning cycle:\n\
1. Reconcile terminal executor handoff first: if the current ready task is complete, mark it `done` before creating new work.\n\
2. Read the current eval header/vector from semantic control. If `eval_gate=fail`, ready work must clear the listed violation before lower-risk cleanup.\n\
3. Otherwise, choose ready work by expected improvement to the weakest eval dimension; issue score only breaks ties inside that eval target.\n\
4. Update `PLAN.json` via the `plan` action and derive the ready-work window for each executor. Mark tasks `ready` (not `todo`) to make them executable — the executor only picks up `ready` tasks.\n\
5. Maintain a READY NOW window containing at most 1-10 executable tasks for each executor, and move blocked work behind dependencies.\n\
6. Write detailed, imperative tasks that include file paths, concrete actions (read/patch/test), expected Δeval, and validation evidence.\n\
7. Keep the ready window executable immediately by the next execute phase in this same runtime loop.\n\n\
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &[&str], std::option::Option<&str>
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

fn executor_handoff() -> String {
    EXECUTOR_PREFIX.to_string()
}

fn prompt_tail(kind: AgentPromptKind) -> String {
    match kind {
        AgentPromptKind::Executor => {
            format!("{}\n\n{}", executor_handoff(), execution_discipline())
        }
        AgentPromptKind::Planner => PLANNER_PROCESS.to_string(),
    }
}

pub(crate) fn system_instructions(kind: AgentPromptKind) -> String {
    let intro = prompt_intro(kind).to_string();
    let mission = prompt_mission(kind).to_string();
    let workspace_text = prompt_workspace(kind);
    let graph_guidance = prompt_graph_artifact_guidance(kind).to_string();
    let pipeline_contract = canonical_pipeline_prompt_block();
    let status_snapshot = status_snapshot_for(kind).to_string();
    let tail = prompt_tail(kind);
    let mut sections = vec![
        intro,
        mission,
        workspace_text,
        graph_guidance,
        pipeline_contract,
    ];
    if kind == AgentPromptKind::Planner {
        sections.push(planner_artifact_review_protocol());
    }
    if !status_snapshot.is_empty() {
        sections.push(status_snapshot);
    }
    let prefix = format!("{}\n\n", sections.join("\n\n"));
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
    let guided_review = crate::structured_questions::guided_review_block("planner cycle boundary");
    let pipeline_contract = canonical_pipeline_prompt_block();
    let artifact_review = planner_artifact_review_protocol();
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
         {pipeline_contract}\n\n\
         {artifact_review}\n\n\
         {guided_review}\n\n\
         ⟹ IMMEDIATE ACTION: Reconcile the inbound executor handoff against the current \
         ready window first. Then plan from eval pressure: if the semantic control eval header \
         shows `eval_gate=fail`, create ready work that clears the violation. Otherwise, choose \
         projected issues only when they improve the weakest eval dimension, and include expected \
         Δeval plus validation evidence in each ready task. issue score is a tie-break signal \
         inside the eval target, not the priority authority.\n\n\
         Graph prioritization rule: inside the eval-selected candidate set, use \
         `agent_state/safe_patch_candidates.json` to seed the ready window with top-ranked \
         semantic merge candidates, and use \
         `agent_state/semantic_manifest_proposals.json` to preserve contract fields in task wording. \
         If graph-ranked merge candidates and generic detector issues compete at similar expected Δeval, \
         schedule at least one top graph-ranked candidate in READY NOW.\n\n\
         Invariant lifecycle rule: if the dynamic invariants section shows Promoted invariants, \
         use the `invariants` action to enforce or collapse them before generic plan churn. \
         If graph/tlog risk implies a missing invariant, create a ready executor task that patches \
         `src/invariant_discovery.rs` rather than editing `agent_state/enforced_invariants.json`.\n\n\
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
    let guided_review = crate::structured_questions::guided_review_block("executor cycle boundary");
    let pipeline_contract = canonical_pipeline_prompt_block();
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
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nREADY TASKS (from {MASTER_PLAN_FILE}, top-10 by plan order):\n{ready_tasks}\n\n{pipeline_contract}\n\n{guided_review}\n\nLane plans are deprecated. Use {MASTER_PLAN_FILE} and current planner-phase outputs for task selection.\nGraph-first execution: consult `state/rustc/canon_mini_agent/graph.json`, `agent_state/safe_patch_candidates.json`, and `agent_state/semantic_manifest_proposals.json` before patching so edits align with ranked semantic candidates and manifest contracts.\nLatest verifier result for lane {lane_label}:\n{verify_result}\n\nUse `message` primarily for blocker escalation or unresolved partial completion evidence."
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
    let pipeline_contract = canonical_pipeline_prompt_block();
    let guided_review =
        crate::structured_questions::guided_review_block("single-role planner boundary");
    let artifact_review = planner_artifact_review_protocol();
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
        "\n\n{pipeline_contract}\n\n{artifact_review}\n\n{guided_review}\n\nUse {INVARIANTS_FILE} when deriving plan constraints.\nRead files and search the source code before issuing plan changes.\nPlan from eval first: clear `eval_gate=fail` violations before optional cleanup, otherwise select issues that improve the weakest eval dimension. Open issues in `{issues_file}` are candidate work only when current-source evidence supports expected Δeval; issue score is a tie-break signal inside the eval target, not the priority authority. Create plan tasks that reference `issue_refs`. `{diagnostics_path}` entries with no matching {issues_file} entry are hints only.\nGraph-first rule: within the eval-selected candidate set, prioritize top-ranked items from `agent_state/safe_patch_candidates.json`, and use `agent_state/semantic_manifest_proposals.json` to keep task instructions aligned with contract metadata.\nEvery ready task must include expected Δeval, the weakest eval dimension it targets, and the validation command/evidence needed after execution.\nInvariant lifecycle rule: Promoted dynamic invariants require an `invariants` action decision: enforce if the predicate is structurally valid, collapse if the root cause is gone, or create a source patch task against `src/invariant_discovery.rs` if graph/tlog evidence shows a missing synthesis rule.\nWrite imperative, actionable instructions in {MASTER_PLAN_FILE}.\nOnly use plan diffs when available; avoid re-reading the full plan unless necessary.\nDo not use internal tools.\nDo not hand off work; keep planning and execution in the current role flow.\nWhen a `plan` action is derived from projected diagnostics state, include same-cycle source validation in `observation` and `rationale` before mutating {MASTER_PLAN_FILE}.\n\nTreat stale or already-resolved projected diagnostics as non-actionable until current source evidence reconfirms them.\nIf projected diagnostics repeatedly report stale issues, create follow-up work to repair projection generation rather than reopening resolved implementation tasks."
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
    let guided_review =
        crate::structured_questions::guided_review_block("single-role executor boundary");
    let pipeline_contract = canonical_pipeline_prompt_block();
    let prefix = format!(
        "WORKSPACE: {workspace}\nAll relative paths resolve against WORKSPACE.\n\nSpec: {SPEC_FILE} — use read_file to load sections as needed.\n\nMaster plan (from {MASTER_PLAN_FILE}):\n{master_plan}"
    );
    let semantic_control_heading =
        "Semantic control state (tlog-derived authority + projected views)".to_string();
    let suffix = format!(
        "\n\n{pipeline_contract}\n\n{guided_review}\n\nLane plans are deprecated. Use {MASTER_PLAN_FILE} and current planner-phase outputs for task selection.\nGraph-first execution: prefer edits that close top entries in `agent_state/safe_patch_candidates.json`, and preserve semantic contracts from `agent_state/semantic_manifest_proposals.json` while patching.\n\nDo not modify spec, plan, violations, or diagnostics.\nDo not use internal tools.\nDo not hand off work; continue execution directly in the current role flow.\nUse `message.payload` to report blocker escalation or unresolved partial-completion evidence. {ACTION_EMIT_LINE}"
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::result::Result<std::vec::Vec<serde_json::Value>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: json_candidate
/// Inputs: &str
/// Outputs: std::option::Option<std::string::String>
/// Effects: none
/// Forbidden: mutation
/// Invariants: prefers fenced JSON, otherwise returns the first parseable JSON value or trimmed JSON-like suffix
/// Failure: returns None when no JSON object or array start exists
/// Provenance: rustc:facts + rustc:docstring
fn extract_json_candidate(text: &str) -> Option<String> {
    if let Some(fenced) = extract_json_fence(text) {
        return Some(fenced.to_string());
    }
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| matches!(b, b'{' | b'['))?;
    let candidate = &text[start..];
    let mut stream = serde_json::Deserializer::from_str(candidate).into_iter::<Value>();
    if let Some(Ok(_)) = stream.next() {
        let end = start + stream.byte_offset();
        return Some(text[start..end].trim().to_string());
    }
    Some(candidate.trim().to_string())
}

/// Intent: pure_transform
/// Resource: prompt_text
/// Inputs: &str
/// Outputs: std::option::Option<&str>
/// Effects: none
/// Forbidden: mutation
/// Invariants: returns trimmed JSON fence body only when a json fence marker is present
/// Failure: returns None when no JSON fence is found or the fence header is unterminated
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: embedded_json
/// Inputs: &str
/// Outputs: std::result::Result<serde_json::Value, anyhow::Error>
/// Effects: none
/// Forbidden: mutation
/// Invariants: scans candidate JSON object/array starts in text order and returns the first successfully parsed JSON value
/// Failure: returns an error when no JSON object or array can be parsed from the response text
/// Provenance: rustc:facts + rustc:docstring
fn parse_json_from_text(text: &str) -> Result<Value> {
    if let Some(value) = text
        .char_indices()
        .filter_map(|(idx, ch)| matches!(ch, '{' | '[').then_some(idx))
        .find_map(|idx| {
            let slice = &text[idx..];
            let de = serde_json::Deserializer::from_str(slice);
            let mut iter = de.into_iter::<Value>();
            iter.next().and_then(Result::ok)
        })
    {
        return Ok(value);
    }
    bail!("no JSON object found in response")
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str
/// Outputs: std::result::Result<std::vec::Vec<serde_json::Value>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn parse_json_action(text: &str) -> Result<Vec<Value>> {
    let value = serde_json::from_str::<Value>(text)?;
    parse_json_action_value(value).with_context(|| {
        format!(
            "not a JSON action object: {:?}",
            &text.chars().take(120).collect::<String>()
        )
    })
}

/// Intent: pure_transform
/// Resource: error
/// Inputs: serde_json::Value
/// Outputs: std::result::Result<std::vec::Vec<serde_json::Value>, anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
    let value = load_master_plan_snapshot(std::path::Path::new(workspace()));
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: fs_write, uses_network, spawns_process
/// Invariants: checks_must_gate_state_transition
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: message_required_fields
/// Inputs: &serde_json::Map<std::string::String, serde_json::Value>
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: none
/// Forbidden: accepting messages missing required routing/status fields or object payload
/// Invariants: from, to, type, and status must be non-empty; payload must be a JSON object
/// Failure: returns validation error for missing or invalid required fields
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &protocol::ProtocolMessage
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: fs_write, uses_network, spawns_process
/// Invariants: blocker_payload_requires_blocker_evidence_required_action
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Map<std::string::String, serde_json::Value>
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn validate_optional_message_severity(obj: &serde_json::Map<String, Value>) -> Result<()> {
    let Some(severity) = obj.get("severity").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let _ =
        serde_json::from_value::<crate::protocol::Severity>(Value::String(severity.to_string()))
            .map_err(|_| anyhow!("message severity must be one of: info|warn|error|critical"))?;
    Ok(())
}

/// Intent: validation_gate
/// Resource: message_role_schema
/// Inputs: &serde_json::Map<std::string::String, serde_json::Value>, &str
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: validates optional message role field without mutation
/// Forbidden: filesystem writes, state mutation, process spawning, network access
/// Invariants: absent role is accepted; present role must be user, executor, or planner
/// Failure: returns validation error for unsupported role values
/// Provenance: rustc:facts + rustc:docstring
fn validate_optional_message_role(obj: &serde_json::Map<String, Value>, field: &str) -> Result<()> {
    let Some(role) = obj.get(field).and_then(|v| v.as_str()) else {
        return Ok(());
    };
    if !matches!(role, "user" | "executor" | "planner") {
        bail!("{field} must be one of: user|executor|planner");
    }
    Ok(())
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &protocol::ProtocolMessage
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn validate_message_route(msg: &ProtocolMessage) -> Result<()> {
    let self_routed = std::mem::discriminant(&msg.from) == std::mem::discriminant(&msg.to);
    if self_routed {
        bail!("message route may not target the emitting role in two-role runtime");
    }
    Ok(())
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &protocol::ProtocolMessage
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
fn validate_active_message_roles(msg: &ProtocolMessage) -> Result<()> {
    let from_ok = matches!(msg.from, Role::Planner | Role::Executor | Role::User);
    let to_ok = matches!(msg.to, Role::Planner | Role::Executor | Role::User);
    if !from_ok || !to_ok {
        bail!("message roles must be planner, executor, or user in two-role runtime");
    }
    Ok(())
}

/// Intent: validation_gate
/// Resource: error
/// Inputs: &serde_json::Value, prompts::MessageValidationMode
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &mut serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
pub(crate) fn normalize_action(action: &mut Value) -> Result<()> {
    let obj = action
        .as_object_mut()
        .ok_or_else(|| anyhow!("action payload must be a JSON object"))?;
    let kind = normalize_action_kind(obj)?;
    normalize_action_rationale(obj, &kind);
    if kind == "message" {
        normalize_message_action_fields(obj);
        validate_message_action(action, MessageValidationMode::Basic)?;
    } else if kind == "issue" {
        normalize_issue_op(obj);
    } else if kind == "plan" {
        normalize_plan_op(obj);
    }
    Ok(())
}

fn normalize_action_kind(obj: &serde_json::Map<String, Value>) -> Result<String> {
    obj.get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("action missing 'action'"))
        .map(str::to_string)
}

fn normalize_action_rationale(obj: &mut serde_json::Map<String, Value>, kind: &str) {
    let has_rationale = obj
        .get("rationale")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    if !has_rationale {
        obj.insert(
            "rationale".to_string(),
            Value::String(default_rationale(kind).to_string()),
        );
    }
}

fn normalize_message_action_fields(obj: &mut serde_json::Map<String, Value>) {
    copy_missing_alias_field(obj, "from", "from_role");
    copy_missing_alias_field(obj, "to", "to_role");
    for field in ["from", "to", "type", "status"] {
        lowercase_message_field(obj, field);
    }
    lowercase_message_field(obj, "severity");
    lowercase_payload_severity(obj);
}

fn copy_missing_alias_field(obj: &mut serde_json::Map<String, Value>, field: &str, alias: &str) {
    if obj.get(field).is_none() {
        if let Some(val) = obj.get(alias).cloned() {
            obj.insert(field.to_string(), val);
        }
    }
}

fn lowercase_message_field(obj: &mut serde_json::Map<String, Value>, field: &str) {
    if let Some(val) = obj
        .get_mut(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    {
        let mut normalized = val.to_lowercase();
        if field == "status" && normalized == "completed" {
            normalized = "complete".to_string();
        }
        obj.insert(field.to_string(), Value::String(normalized));
    }
}

fn lowercase_payload_severity(obj: &mut serde_json::Map<String, Value>) {
    if let Some(payload) = obj.get_mut("payload").and_then(|v| v.as_object_mut()) {
        if let Some(val) = payload
            .get_mut("severity")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            payload.insert("severity".to_string(), Value::String(val.to_lowercase()));
        }
    }
}

fn normalize_issue_op(obj: &mut serde_json::Map<String, Value>) {
    if let Some(op) = obj
        .get("op")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
    {
        let normalized = match op.as_str() {
            "close" | "closed" | "resolve_issue" => Some("resolve"),
            "create_issue" => Some("create"),
            "update_issue" => Some("update"),
            "delete_issue" => Some("delete"),
            _ => None,
        };
        if let Some(normalized) = normalized {
            obj.insert("op".to_string(), Value::String(normalized.to_string()));
        }
    }
}

fn normalize_plan_op(obj: &mut serde_json::Map<String, Value>) {
    if let Some(op) = obj
        .get("op")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
    {
        if let Some(normalized) = normalized_plan_op(obj, &op) {
            obj.insert("op".to_string(), Value::String(normalized.to_string()));
        }
    }
}

fn normalized_plan_op(obj: &mut serde_json::Map<String, Value>, op: &str) -> Option<&'static str> {
    match op {
        "create_edge" => Some("add_edge"),
        "delete_edge" => Some("remove_edge"),
        "add_task" => Some("create_task"),
        "replace" if obj.get("plan").is_some() => Some("replace_plan"),
        "set_status" if obj.get("task_id").is_some() => Some("set_task_status"),
        "set_status" => Some("set_plan_status"),
        "add_tasks" if obj.get("plan").is_some() => Some("replace_plan"),
        "add_tasks" => normalize_add_tasks_op(obj),
        _ => None,
    }
}

fn normalize_add_tasks_op(obj: &mut serde_json::Map<String, Value>) -> Option<&'static str> {
    let single_task = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .and_then(|tasks| (tasks.len() == 1).then(|| tasks[0].clone()));
    if obj.get("task").is_none() {
        if let Some(task) = single_task {
            obj.insert("task".to_string(), task);
        }
    }
    obj.get("task").is_some().then_some("create_task")
}

/// Intent: validation_gate
/// Resource: action_validation
/// Inputs: &serde_json::Value
/// Outputs: std::result::Result<(), anyhow::Error>
/// Effects: validates tool action schema, provenance, diagnostics evidence, and strict message payload rules
/// Forbidden: mutation of input action
/// Invariants: diagnostic plan actions must cite same-cycle source evidence; message actions must pass strict message validation
/// Failure: returns validation errors when schema, provenance, evidence, or message constraints fail
/// Provenance: rustc:facts + rustc:docstring
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

/// Intent: pure_transform
/// Resource: error
/// Inputs: &str, &str, &str
/// Outputs: std::string::String
/// Effects: error
/// Forbidden: error
/// Invariants: error
/// Failure: error
/// Provenance: rustc:facts + rustc:docstring
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
    // Re-inject the fixed lifecycle review after each mutating step so the
    // agent re-examines truth, misalignment, priority, next change, and the
    // revealed delta mid-turn, not only at turn start.
    // last_action is the action type string (e.g. "apply_patch"), not full JSON.
    let mutating_review = last_action
        .filter(|kind| matches!(*kind, "apply_patch" | "plan" | "objectives" | "issue"))
        .map(crate::structured_questions::guided_review_after_mutation)
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
        mutating_review,
    );
    render_action_result_sections(&prefix, result, &suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_contract::ACTION_EMIT_LINE;
    use serde_json::json;

    #[test]
    fn extract_json_candidate_prefers_fenced_json_block() {
        let raw = "noise\n```json\n{\"action\":\"message\"}\n```\ntrailer";
        let extracted = extract_json_candidate(raw).expect("expected json candidate");
        assert_eq!(extracted, "{\"action\":\"message\"}");
    }

    #[test]
    fn extract_json_candidate_handles_braces_inside_strings() {
        let raw = r#"prefix {"text":"value with } brace", "ok": true} suffix"#;
        let extracted = extract_json_candidate(raw).expect("expected json candidate");
        assert_eq!(extracted, r#"{"text":"value with } brace", "ok": true}"#);
    }

    #[test]
    fn extract_json_candidate_returns_trimmed_suffix_when_unbalanced() {
        let raw = "prefix {\"action\":\"read_file\"";
        let extracted = extract_json_candidate(raw).expect("expected json candidate");
        assert_eq!(extracted, "{\"action\":\"read_file\"");
    }

    #[test]
    fn parse_json_from_text_extracts_first_json_value() {
        let raw = "leading text {\"action\":\"message\",\"payload\":{}} trailing";
        let value = parse_json_from_text(raw).expect("expected parseable json");
        assert_eq!(
            value.get("action").and_then(|v| v.as_str()),
            Some("message")
        );
    }

    #[test]
    fn parse_json_from_text_rejects_non_json_text() {
        let err = parse_json_from_text("no structured response here")
            .expect_err("expected no-json parse failure");
        assert!(err.to_string().contains("no JSON object found in response"));
    }

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
    fn normalize_message_completed_status_to_complete() {
        let mut action = json!({
            "action": "message",
            "from": "executor",
            "to": "planner",
            "type": "handoff",
            "status": "completed",
            "payload": {"summary": "done"},
            "rationale": "normalize legacy status"
        });
        normalize_action(&mut action).unwrap();
        assert_eq!(
            action.get("status").and_then(|v| v.as_str()),
            Some("complete")
        );
    }

    #[test]
    fn normalize_issue_close_to_resolve() {
        let mut action = json!({
            "action": "issue",
            "op": "close",
            "issue_id": "ISSUE-1",
            "rationale": "normalize legacy issue op"
        });
        normalize_action(&mut action).unwrap();
        assert_eq!(action.get("op").and_then(|v| v.as_str()), Some("resolve"));
    }

    #[test]
    fn normalize_plan_add_tasks_singleton_to_create_task() {
        let mut action = json!({
            "action": "plan",
            "op": "add_tasks",
            "tasks": [{"id": "T1", "title": "one task", "status": "ready"}],
            "rationale": "normalize singleton plan add_tasks"
        });
        normalize_action(&mut action).unwrap();
        assert_eq!(
            action.get("op").and_then(|v| v.as_str()),
            Some("create_task")
        );
        assert!(action.get("task").is_some());
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
    fn planner_and_executor_require_python_for_structured_graph_artifacts() {
        for kind in [AgentPromptKind::Planner, AgentPromptKind::Executor] {
            let prompt = system_instructions(kind);
            assert!(
                prompt.contains("Use the `python` action for structured JSON/NDJSON analysis"),
                "{kind:?} prompt must require python for structured state artifact analysis"
            );
            assert!(
                prompt.contains("state/rustc/canon_mini_agent/graph.json")
                    && prompt.contains("agent_state/tlog.ndjson")
                    && prompt.contains("agent_state/safe_patch_candidates.json")
                    && prompt.contains("agent_state/semantic_manifest_proposals.json"),
                "{kind:?} prompt must name the structured artifacts that require python"
            );
        }
    }

    #[test]
    fn planner_prompts_require_full_artifact_review_and_role_contract() {
        let system = system_instructions(AgentPromptKind::Planner);
        let cycle = planner_cycle_prompt("", "{}", "", "", "", "", "", "");
        let single = single_role_planner_prompt(
            "{spec}",
            "{objectives}",
            "{lessons}",
            "{enforced_invariants}",
            "{semantic_control}",
            "{cargo_test_failures}",
        );

        for prompt in [&system, &cycle, &single] {
            assert!(
                prompt.contains("Planner role contract:"),
                "planner prompts must state the planner role explicitly"
            );
            assert!(
                prompt.contains("audit evidence, diagnose root cause"),
                "planner must be told to analyze the problem, not just dispatch tasks"
            );
            assert!(
                prompt.contains("agent_state/llm_full/*planner*")
                    && prompt.contains("state/rustc/canon_mini_agent/graph.json")
                    && prompt.contains("agent_state/evidence_receipts.jsonl")
                    && prompt.contains("agent_state/default/actions.jsonl"),
                "planner prompts must require review of runtime, graph, evidence, and llm_full artifacts"
            );
            assert!(
                prompt.contains("Every `plan`, `objectives`, and terminal `message` rationale must name the artifact evidence"),
                "planner outputs must cite artifact evidence and observed deltas"
            );
        }
    }

    #[test]
    fn planner_prompts_are_eval_driven_before_issue_score_driven() {
        let system = system_instructions(AgentPromptKind::Planner);
        let cycle = planner_cycle_prompt("", "{}", "", "", "", "", "", "");
        let single = single_role_planner_prompt(
            "{spec}",
            "{objectives}",
            "{lessons}",
            "{enforced_invariants}",
            "{semantic_control}",
            "{cargo_test_failures}",
        );

        for prompt in [&system, &cycle, &single] {
            assert!(
                prompt.contains("eval_gate=fail")
                    && prompt.contains("weakest eval dimension")
                    && prompt.contains("expected Δeval"),
                "planner prompts must prioritize eval gate, weakest eval dimension, and expected delta"
            );
            assert!(
                prompt.contains("issue score") || prompt.contains("issue scores"),
                "planner prompts must demote issue score to a candidate or tie-break signal"
            );
        }
    }

    #[test]
    fn planner_and_executor_prompts_include_canonical_pipeline_contract() {
        for kind in [AgentPromptKind::Planner, AgentPromptKind::Executor] {
            let prompt = system_instructions(kind);
            assert!(
                prompt.contains("Canonical pipeline contract")
                    && prompt.contains("CANONICAL_PIPELINE.md")
                    && prompt.contains("C_allowed = no_rust_change"),
                "{kind:?} system prompt must include the canonical pipeline contract"
            );
        }
    }

    #[test]
    fn cycle_prompts_include_canonical_pipeline_contract() {
        let planner = planner_cycle_prompt("", "{}", "", "", "", "", "", "");
        let executor = executor_cycle_prompt("executor", "executor_pool", "", "[]");
        assert!(planner.contains("CANONICAL_PIPELINE.md"));
        assert!(executor.contains("CANONICAL_PIPELINE.md"));
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
        assert!(
            prompt.len() < 24_000,
            "planner system prompt should stay compact"
        );
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
        assert!(prompt.contains(
            "Compare these against the actual result above before choosing your next action."
        ));
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
        assert!(!prompt.contains(
            "Compare these against the actual result above before choosing your next action."
        ));
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
        let kinds = [AgentPromptKind::Planner, AgentPromptKind::Executor];
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
        assert!(ACTION_EMIT_LINE.contains("do not reveal private chain of thought"));
        assert!(!ACTION_EMIT_LINE.contains("exactly one action"));
    }

    #[test]
    fn budgeted_prompt_truncation_preserves_failure_lines_and_tail() {
        let body = format!(
            "{}\nerror[E0425]: cannot find value `missing_symbol` in this scope\n{}\ntest result: FAILED. 357 passed; 4 failed\nTAIL_SENTINEL",
            "HEAD\n".repeat(200),
            "middle noise\n".repeat(800)
        );
        let items = [PromptItem {
            heading: "Cargo output",
            body: &body,
            reserve: 1800,
            cap: 1800,
            weight: 1,
            always_include: true,
        }];
        let prompt = render_budgeted_prompt("prefix", &items, "suffix");

        assert!(
            prompt.contains("error[E0425]") && prompt.contains("test result: FAILED"),
            "truncation must preserve failure evidence"
        );
        assert!(
            prompt.contains("TAIL_SENTINEL"),
            "truncation must preserve tail context"
        );
        assert!(
            !prompt.contains("... [truncated]"),
            "legacy silent truncation marker should be replaced"
        );
    }

    #[test]
    fn budgeted_prompt_truncation_emits_machine_readable_record() {
        let body = "line\n".repeat(1000);
        let items = [PromptItem {
            heading: "Large section",
            body: &body,
            reserve: 900,
            cap: 900,
            weight: 1,
            always_include: true,
        }];
        let prompt = render_budgeted_prompt("prefix", &items, "suffix");
        let records = prompt_truncation_records(&prompt);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].heading, "Large section");
        assert_eq!(records[0].raw_bytes, body.len());
        assert!(records[0].kept_bytes < records[0].raw_bytes);
        assert!(records[0].dropped_bytes > 0);
        assert_eq!(records[0].policy, "preserve_failure_lines_head_tail");
    }
}

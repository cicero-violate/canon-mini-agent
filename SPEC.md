# canon-mini-agent Formal Specification

This document defines **invariants, state model, typed interfaces, CLI contract, and determinism guarantees** for `canon-mini-agent`.

## 1. State Model

The system is a deterministic event-driven loop with explicit roles.

### 1.1 Global State
- `Workspace`: absolute root path of the target project being operated on. Set via `--workspace <path>` CLI argument; defaults to `/workspace/ai_sandbox/canon`. Must be absolute. All relative paths in actions resolve against this value.
- `AgentStateDir`: operational state for canon-mini-agent itself. Fixed at `/workspace/ai_sandbox/canon-mini-agent/agent_state`. Never equals `Workspace`.
- `Role`: one of `{Planner, Executor, Verifier, Diagnostics}`.
- `Lane`: executor lane id (e.g., `executor_pool`), bound to a role of type Executor.
- `PromptKind`: `{planner, executor, verifier, diagnostics}`.
- `Action`: a typed JSON object (see Section 2).
- `ActionResult`: `{ complete: bool, output: string }`.
- `RunConfig`: timeouts, tool availability, and patch scope policy.

### 1.2 Role State (per agent)
- `prompt_kind: PromptKind`
- `step: u64` (monotonic, starts at 1 per role cycle)
- `last_action: Action?`
- `last_result: ActionResult?`
- `lane_plan_path: string?` (Executors only)

### 1.3 Canonical Files
Canonical file paths are absolute under `Workspace`:
- `Spec`: `SPEC.md`
- `Objectives`: `PLANS/OBJECTIVES.md` (authoritative MD) / `PLANS/OBJECTIVES.json` (derived JSON)
- `Invariants`: `INVARIANT.md` (authoritative MD) / `INVARIANTS.json` (derived JSON)
- `MasterPlan`: `PLAN.json`
- `LanePlan`: `PLANS/<instance>/executor-<id>.md` or legacy `PLANS/executor-<id>.md`
- `Violations`: `VIOLATIONS.json`
- `Diagnostics`: `PLANS/<instance>/diagnostics-<instance>.json` (or legacy `DIAGNOSTICS.json`)

### 1.4 Workspace Resolution Rule
Every `path` field in every action is resolved as follows:
1. If already absolute and under `Workspace`, use as-is.
2. If relative, join with `Workspace` and canonicalize.
3. If absolute but outside `Workspace` and not in `/tmp`, the action is rejected with a scope violation.

## 2. CLI Interface

```
canon-mini-agent [FLAGS] [OPTIONS]
```

### 2.1 Flags
| Flag | Description |
|------|-------------|
| `--orchestrate` | Run the full multi-role orchestration loop (planner → executors → verifier/diagnostics → planner). |
| `--planner` | Run only the planner role (single role mode). |
| `--verifier` | Run only the verifier role (single role mode). |
| `--diagnostics` | Run only the diagnostics role (single role mode). |

### 2.2 Options
| Option | Default | Description |
|--------|---------|-------------|
| `--workspace <path>` | `/workspace/ai_sandbox/canon` | **Absolute path** to the target project workspace. All agent file operations resolve relative to this path. Must exist and be a directory. |
| `--start <role>` | `executor` | Start role for orchestration: `executor`, `verifier`, `planner`, or `diagnostics`. |
| `--instance <id>` | `default` | Instance identifier used to namespace PLANS subdirectories and diagnostics files. |
| `--port <port>` | auto | WebSocket port for Chrome extension. Auto-selects from candidates if not specified. |

### 2.3 Workspace Validation
- `--workspace` value must be an absolute path (starts with `/`). Non-absolute paths are rejected at startup with a fatal error.
- If omitted, `DEFAULT_WORKSPACE` (`/workspace/ai_sandbox/canon`) is used.
- The runtime value is stored in a process-global `OnceLock<String>` and never changes after startup.

## 3. Typed Interfaces (Actions)

All actions are JSON objects with a mandatory `"action"` string field. Any missing required field is an error.

### 3.1 Common Action Envelope
```json
{ "action": "<type>", "observation": "<why>", "rationale": "<why now>" }
```

### 3.2 `list_dir`
```json
{ "action": "list_dir", "path": "<string>" }
```
Constraints: `path` is relative to `Workspace` or absolute under `Workspace`.

### 3.3 `read_file`
```json
{ "action": "read_file", "path": "<string>", "line"?: "<integer>", "line_start"?: "<integer>", "line_end"?: "<integer>" }
```
Constraints: `line` / `line_start` / `line_end` are 1-based when present.

### 3.4 `apply_patch`
```json
{ "action": "apply_patch", "patch": "<string>" }
```
Constraints: patch must follow unified patch grammar. The first `*** Update File:` or `*** Add File:` path determines scope. Executor scope guards apply.

### 3.5 `run_command`
```json
{ "action": "run_command", "cmd": "<string>", "cwd"?: "<string>" }
```
Constraints: `cwd` defaults to `Workspace`. Must be under `Workspace` or `/tmp`.

### 3.6 `python`
```json
{ "action": "python", "code": "<string>", "cwd"?: "<string>" }
```
Constraints: `cwd` defaults to `Workspace`. Write operations must target paths under `Workspace` or `/tmp`.

### 3.7 `cargo_test`
```json
{ "action": "cargo_test", "crate": "<string>", "test": "<string>" }
```
Semantics: maps to `cargo test -p <crate> <test> -- --exact --nocapture`.

### 3.8 `rustc_hir`
```json
{ "action": "rustc_hir", "crate": "<string>", "mode"?: "<string>", "extra"?: "<string>" }
```
Semantics: maps to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>`.

### 3.9 `rustc_mir`
```json
{ "action": "rustc_mir", "crate": "<string>", "mode"?: "<string>", "extra"?: "<string>" }
```
Semantics: maps to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>`.

### 3.10 `graph_call` / `graph_cfg`
```json
{ "action": "graph_call", "crate": "<string>", "out_dir"?: "<string>" }
{ "action": "graph_cfg",  "crate": "<string>", "out_dir"?: "<string>" }
```
Outputs: CSVs plus `callgraph.symbol.txt` / `cfg.symbol.txt` with symbol→symbol edges.

### 3.11 `graph_dataflow` / `graph_reachability`
```json
{ "action": "graph_dataflow",    "crate": "<string>", "tlog"?: "<string>", "out_dir"?: "<string>" }
{ "action": "graph_reachability","crate": "<string>", "tlog"?: "<string>", "out_dir"?: "<string>" }
```
Outputs: JSON reports under metrics/analysis directories.

### 3.12 `message` (Inter-Agent Handoff Protocol)
```json
{
  "action": "message",
  "from": "Executor",
  "to": "Planner",
  "type": "handoff",
  "status": "blocked",
  "payload": {
    "summary": "<what happened>",
    "evidence": {},
    "next_steps": []
  }
}
```

**Message Matrix**

| From       | To          | Type           | Status             | Payload (required keys)              |
|------------|-------------|----------------|--------------------|--------------------------------------|
| Executor   | Verifier    | handoff        | complete           | `summary`, `artifacts`               |
| Executor   | Planner     | handoff        | complete / blocked | `summary`, `evidence`                |
| Verifier   | Planner     | verification   | verified / failed  | `summary`, `verified_items` / `false_items` |
| Verifier   | Planner     | failure        | failed             | `summary`, `next_actions`            |
| Diagnostics| Planner     | diagnostics    | complete           | `summary`, `ranked_failures`         |
| Planner    | Executor    | tasking        | ready / blocked    | `summary`, `tasks` / `blockers`      |

**Routing guarantee (added 2026-04-07):** When an executor emits a `message` action, the system writes `last_message_to_<to>.json` and `wakeup_<to>.flag` to `AgentStateDir` and sets `planner_pending = true` (for planner-targeted messages). This ensures the target role wakes on the next orchestration cycle regardless of whether the action was emitted in the inline or deferred completion path.

## 4. Invariants (Must Always Hold)

### 4.1 Scope Invariants
- **Executor** may not edit `SPEC.md`, `PLANS/OBJECTIVES.md`, `INVARIANT.md`, `PLAN.json`, any lane plan, `VIOLATIONS.json`, or diagnostics reports.
- **Verifier** may edit **only** `PLAN.json` and `VIOLATIONS.json`.
- **Diagnostics** may edit **only** the diagnostics report file.
- **Planner** may edit **only** `PLAN.json` and lane plans.
- No role may modify `/workspace/ai_sandbox/canon-mini-agent` (the orchestrator itself) unless explicitly authorized by the operator.

### 4.2 Action Validity Invariants
- Each action must satisfy its typed schema (Section 3).
- Missing required fields or invalid types must be rejected.
- `read_file` line numbers are 1-based.

### 4.3 Canonical-File Authority Invariants
- `SPEC.md` is the canonical contract for repair work.
- `PLANS/OBJECTIVES.md` and `INVARIANT.md` are authoritative for objectives and invariants.
- Planner must derive lane plans from canonical files, not from memory or stale copies.
- `SemanticStateSummary` is the single source of truth for routing and control-flow correctness.

### 4.4 Event Ordering Invariants
- Actions are processed in strict step order per role: `step` is monotonic.
- Each `step` produces at most one `ActionResult`.
- A role must not emit a new action without observing the result of the previous action.
- Executor hard cap: after `EXECUTOR_STEP_LIMIT` (20) actions without a `message` handoff, the system forces a handoff prompt.

### 4.5 Logging Invariants
- Every action must be appended to `agent_logs/.../actions.jsonl`.
- Every action result must be appended to `agent_logs/.../action_results.jsonl`.
- Action logs must preserve order of execution.

### 4.6 Build/Test Gate Invariants
- If a completion `message` (status = `complete`) triggers checks:
  - `cargo build --workspace` must pass.
  - `cargo test --workspace` must pass.
  - Otherwise completion is rejected.

### 4.7 Handoff Delivery Invariant
- A `message` action emitted by any role **must** result in the target role receiving the payload before its next cycle begins.
- Wakeup flags (`wakeup_<role>.flag`) and inbound message files (`last_message_to_<role>.json`) are the delivery mechanism.
- The `apply_wake_flags` function in the orchestration loop is the authority that translates these flags into scheduled phase transitions.

### 4.8 Workspace Isolation Invariant
- All file operations by agents are confined to `Workspace`.
- The orchestrator's own state (`AgentStateDir`) is never the target workspace.
- Agents are told the active `Workspace` value in every prompt (header line `WORKSPACE: <path>`).

## 5. State Transitions

### 5.1 Per-Role Cycle
```
Idle
  -> Prompted
  -> ActionEmitted
  -> ActionExecuted
  -> ResultObserved
  -> (ActionEmitted | MessageEmitted)
```
Transitions are strictly ordered; skipping any state is invalid.

### 5.2 Orchestrator Cycle
```
Bootstrap
  -> Planner
  -> Executor(s) [parallel lanes]
  -> Verifier
  -> Diagnostics (conditional)
  -> Planner
```
The orchestrator uses wakeup flags and `planner_pending` / `diagnostics_pending` to schedule transitions. Phase order may be overridden by verifier blocker messages.

### 5.3 Handoff Transition
```
Executor emits message{to=Planner}
  -> persist_planner_message() writes last_message_to_planner.json + wakeup_planner.flag
  -> dispatch_state.planner_pending = true
  -> next orchestration loop iteration: apply_wake_flags() schedules planner
  -> planner prompt injects inbound message via inject_inbound_message()
```

## 6. Determinism Guarantees
- Given identical workspace state, canonical files, and action inputs, action execution is deterministic.
- `read_file` and `list_dir` produce deterministic output for a fixed workspace snapshot.
- `run_command` and `python` are deterministic **only** to the extent the invoked commands are deterministic.
- The active `Workspace` path is frozen at process start and never changes.

## 7. PLAN + TASK Protocols

### 7.1 Math Model
`P = (I, O, D, S, C)`

**Variables**
- `I`: inputs (state, diagnostics, spec)
- `O`: outputs (tasks)
- `D`: dependencies
- `S`: sequencing (order)
- `C`: constraints (invariants)

**Equations**
- `T_i = f(I, C)` — task derived from state + constraints
- `S = topo(D)` — execution order from dependencies
- `P_valid = ∀ T_i: deterministic(T_i)`

### 7.2 PLAN Protocol (Canonical Structure)
```json
{
  "plan_id": "<uuid>",
  "version": 1,
  "status": "in_progress",
  "derived_from": {
    "spec": "SPEC.md",
    "objectives": "PLANS/OBJECTIVES.json",
    "invariants": "INVARIANTS.json",
    "violations": "VIOLATIONS.json",
    "diagnostics": "PLANS/<instance>/diagnostics-<instance>.json"
  },
  "global_constraints": [
    "SemanticStateSummary is source of truth",
    "All transitions must follow spec",
    "No role violates scope invariants",
    "scheduler_len and planned_pending are not routing authority"
  ],
  "ready_window": { "executor_pool": ["<task_id>"] },
  "completed_task_ids": [],
  "blocked_task_ids": [],
  "lanes": [
    {
      "lane_id": "executor_pool",
      "role": "Executor",
      "tasks": []
    }
  ]
}
```

### 7.3 Task Protocol
```json
{
  "task_id": "<uuid>",
  "title": "<short deterministic label>",
  "status": "ready | blocked | in_progress | done",
  "priority": 1,
  "inputs": ["file:path", "diagnostic:id"],
  "actions": [
    { "type": "read | patch | test | command", "target": "<file or cmd>", "details": "<exact instruction>" }
  ],
  "outputs": ["file:path", "test:result"],
  "dependencies": ["<task_id>"],
  "success_criteria": ["cargo build passes", "specific invariant holds"],
  "failure_modes": ["test fails", "invariant violation"],
  "next_on_success": ["<task_id>"],
  "next_on_failure": ["<task_id>"]
}
```

### 7.4 Lane Execution Rules
- Execute top 1–10 tasks with `status=ready`.
- Respect dependencies: `∀ T_i: deps(T_i) ⊆ done`.
- No reordering beyond dependency graph.
- After completing work, emit exactly one `message` action to handoff.

### 7.5 Deterministic Guarantees
- Same inputs → same task graph.
- No hidden tasks.
- No implicit dependencies.

## 8. Non-Goals
- No network access from agents.
- No GUI or interactive I/O from agents.
- No mutation outside `Workspace` (except to `/tmp` for scratch files).
- Workspace path is not determined by agents — it is set by the operator at launch.

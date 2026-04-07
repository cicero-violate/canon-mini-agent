# canon-mini-agent Formal Specification

This document defines **invariants, state model, typed interfaces, and determinism guarantees** for `canon-mini-agent`.

## 1. State Model
The system is a deterministic event-driven loop with explicit roles.

### 1.1 Global State
- `Workspace`: absolute root path. Must be `/workspace/ai_sandbox/canon`.
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
- `Objectives`: `PLANS/OBJECTIVES.md`
- `Invariants`: `INVARIANT.md`
- `MasterPlan`: `PLAN.json`
- `LanePlan`: `PLANS/<instance>/executor-<id>.md` or legacy `PLANS/executor-<id>.md`
- `Violations`: `VIOLATIONS.md`
- `Diagnostics`: `PLANS/<instance>/diagnostics-<instance>.md` (or legacy `DIAGNOSTICS.md`)

## 2. Typed Interfaces (Actions)
All actions are JSON objects with a mandatory `"action"` string field. Any missing required field is an error.

### 2.1 Common Action Envelope
```
{ "action": "<type>", "...": "<type-specific fields>" }
```

### 2.2 `list_dir`
```
{ "action": "list_dir", "path": string }
```
Constraints: `path` is relative to `Workspace` or under `Workspace` when resolved.

### 2.3 `read_file`
```
{ "action": "read_file", "path": string, "line"?: integer }
```
Constraints: `line` is 1-based if present.

### 2.4 `apply_patch`
```
{ "action": "apply_patch", "patch": string }
```
Constraints: patch must follow unified patch grammar enforced by tooling.

### 2.5 `run_command`
```
{ "action": "run_command", "cmd": string, "cwd"?: string }
```
Constraints: `cwd` defaults to `Workspace`.

### 2.6 `python`
```
{ "action": "python", "code": string, "cwd"?: string }
```
Constraints: `cwd` defaults to `Workspace`.

### 2.7 `cargo_test`
```
{ "action": "cargo_test", "crate": string, "test": string }
```
Semantics: maps to `cargo test -p <crate> <test> -- --exact --nocapture`.

### 2.8 `rustc_hir`
```
{ "action": "rustc_hir", "crate": string, "mode"?: string, "extra"?: string }
```
Semantics: maps to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>`.

### 2.9 `rustc_mir`
```
{ "action": "rustc_mir", "crate": string, "mode"?: string, "extra"?: string }
```
Semantics: maps to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>`.

### 2.10 `graph_probe`
```
{ "action": "graph_probe", "crate"?: string, "entry"?: string, "tlog"?: string,
  "symbol_limit"?: integer, "unreachable_limit"?: integer, "cfg_limit"?: integer }
```

### 2.11 `graph_call` / `graph_cfg`
```
{ "action": "graph_call", "crate": string, "out_dir"?: string }
{ "action": "graph_cfg",  "crate": string, "out_dir"?: string }
```
Outputs: CSVs plus `callgraph.symbol.txt` / `cfg.symbol.txt` with symbol→symbol edges.

### 2.12 `graph_dataflow` / `graph_reachability`
```
{ "action": "graph_dataflow", "crate": string, "tlog"?: string, "out_dir"?: string }
{ "action": "graph_reachability", "crate": string, "tlog"?: string, "out_dir"?: string }
```
Outputs: JSON reports under metrics/analysis directories.

### 2.13 `message` (Inter-Agent Protocol)
```
{
  "action": "message",
  "from": "Executor",
  "to": "Verifier",
  "type": "handoff",
  "status": "complete",
  "payload": {
    "summary": "patched route emission pipeline",
    "artifacts": ["canon-route/src/lib.rs"]
  }
}
```
```
{
  "action": "message",
  "from": "Verifier",
  "to": "Planner",
  "type": "failure",
  "status": "failed",
  "payload": {
    "summary": "cargo test failed",
    "next_actions": ["fix failing tests"]
  }
}
```
Semantics:
- `from` / `to`: role names (`Planner`, `Executor`, `Verifier`, `Diagnostics`).
- `type`: semantic intent of the message.
- `status`: outcome state of the message.
- `payload`: structured data for downstream decision-making.

**Message Matrix**

| From       | To          | Type           | Status             | Payload (required keys)              |
|------------|-------------|----------------|--------------------|--------------------------------------|
| Executor   | Verifier    | handoff        | complete           | `summary`, `artifacts`               |
| Executor   | Planner     | handoff        | complete           | `summary`, `artifacts`               |
| Verifier   | Planner     | verification   | verified           | `summary`, `verified_items`          |
| Verifier   | Planner     | verification   | failed             | `summary`, `false_items`             |
| Verifier   | Planner     | failure        | failed             | `summary`, `next_actions`            |
| Diagnostics| Planner     | diagnostics    | complete           | `summary`, `ranked_failures`         |
| Planner    | Executor    | tasking        | ready              | `summary`, `tasks`                   |
| Planner    | Executor    | tasking        | blocked            | `summary`, `blockers`                |

## 3. Invariants (Must Always Hold)

### 3.1 Scope Invariants
- **Executor** may not edit `SPEC.md`, `PLANS/OBJECTIVES.md`, `INVARIANT.md`, `PLAN.json`, any lane plan, `VIOLATIONS.md`, or diagnostics reports.
- **Verifier** may edit **only** `PLAN.json` and `VIOLATIONS.md`.
- **Diagnostics** may edit **only** the diagnostics report file.
- **Planner** may edit **only** `PLAN.json` and lane plans.
- No role may modify `/workspace/ai_sandbox/canon-mini-agent` unless explicitly authorized by the operator.

### 3.2 Action Validity Invariants
- Each action must satisfy its typed schema (Section 2).
- Missing required fields or invalid types must be rejected.
- `read_file` line numbers are 1-based.

### 3.3 Canonical-File Authority Invariants
- `SPEC.md` is the canonical contract for repair work.
- `PLANS/OBJECTIVES.md` and `INVARIANT.md` are authoritative for objectives and invariants.
- Planner must derive lane plans from canonical files, not from memory or stale copies.

### 3.4 Event Ordering Invariants
- Actions are processed in strict step order per role: `step` is monotonic.
- Each `step` produces at most one `ActionResult`.
- A role must not emit a new action without observing the result of the previous action.

### 3.5 Logging Invariants
- Every action must be appended to `agent_logs/.../actions.jsonl`.
- Every action result must be appended to `agent_logs/.../action_results.jsonl`.
- Action logs must preserve order of execution.

### 3.6 Build/Test Gate Invariants
- If a completion `message` (status = `complete`) is configured to run checks, then:
  - `cargo build --workspace` must pass.
  - `cargo test --workspace` must pass.
  - Otherwise completion is rejected.

## 4. State Transitions

### 4.1 Per-Role Cycle
```
Idle
  -> Prompted
  -> ActionEmitted
  -> ActionExecuted
  -> ResultObserved
  -> (ActionEmitted | MessageEmitted)
```
Transitions are strictly ordered; skipping any state is invalid.

### 4.2 Orchestrator Cycle
```
Planner -> Executors -> Verifier/Diagnostics -> Planner
```
The orchestrator must not reorder roles within a cycle.

## 5. Determinism Guarantees
- Given identical workspace state, canonical files, and action inputs, action execution is deterministic.
- `read_file` and `list_dir` produce deterministic output for a fixed workspace snapshot.
- `run_command` and `python` are deterministic **only** to the extent the invoked commands are deterministic.
- All non-determinism must be attributed to external tools or time-dependent commands.

## 6. PLAN + TASK Protocols

### 6.1 Math Model
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

### 6.2 PLAN Protocol (Canonical Structure)
```json
{
  "plan_id": "<uuid>",
  "version": 1,
  "derived_from": {
    "spec": "SPEC.md",
    "objectives": "PLANS/OBJECTIVES.md",
    "invariants": "INVARIANT.md",
    "violations": "VIOLATIONS.md",
    "diagnostics": "PLANS/<instance>/diagnostics-<instance>.md"
  },
  "global_constraints": [
    "SemanticStateSummary is source of truth",
    "All transitions must follow spec",
    "No role violates scope invariants"
  ],
  "lanes": [
    {
      "lane_id": "executor_pool",
      "role": "Executor",
      "tasks": []
    }
  ]
}
```

### 6.3 Task Protocol
```json
{
  "task_id": "<uuid>",
  "title": "<short deterministic label>",
  "status": "ready | blocked | in_progress | done",
  "priority": 1,
  "inputs": [
    "file:path",
    "diagnostic:id"
  ],
  "actions": [
    {
      "type": "read | patch | test | command",
      "target": "<file or cmd>",
      "details": "<exact instruction>"
    }
  ],
  "outputs": [
    "file:path",
    "test:result"
  ],
  "dependencies": ["task_id"],
  "success_criteria": [
    "cargo build passes",
    "specific invariant holds"
  ],
  "failure_modes": [
    "test fails",
    "invariant violation"
  ],
  "next_on_success": ["task_id"],
  "next_on_failure": ["task_id"]
}
```

### 6.4 Lane Execution Rules
- Execute top 1–10 tasks with `status=ready`.
- Respect dependencies: `∀ T_i: deps(T_i) ⊆ done`.
- No reordering beyond dependency graph.

### 6.5 Deterministic Guarantees
- Same inputs → same task graph.
- No hidden tasks.
- No implicit dependencies.

### 6.6 Message Integration
Each task completion emits:
```json
{
  "action": "message",
  "from": "Executor",
  "to": "Verifier",
  "type": "handoff",
  "status": "complete",
  "payload": {
    "task_id": "<uuid>",
    "summary": "<what changed>",
    "artifacts": ["file paths"]
  }
}
```

### 6.7 Minimal Example
```json
{
  "plan_id": "plan-001",
  "version": 1,
  "lanes": [
    {
      "lane_id": "executor_pool",
      "role": "Executor",
      "tasks": [
        {
          "task_id": "t1",
          "title": "Restore RouteSelected emission",
          "status": "ready",
          "priority": 1,
          "actions": [
            {
              "type": "command",
              "target": "rg",
              "details": "rg -n \"RouteSelected|canon_emit!\" canon-utils"
            },
            {
              "type": "patch",
              "target": "canon-route/src/lib.rs",
              "details": "ensure canon_emit!(emitter; ...) used"
            }
          ],
          "success_criteria": [
            "cargo build --workspace passes"
          ]
        }
      ]
    }
  ]
}
```

### 6.8 Key Insight
`PLAN = Deterministic Task Graph`

### 6.9 Conclusion
- PLAN must be structured, dependency-aware, invariant-driven.
- Enables automation, verification, reproducibility.

## 6. Non-Goals
- No network access.
- No GUI/interactive I/O.
- No mutation outside `Workspace`.

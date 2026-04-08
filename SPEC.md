# canon-mini-agent Formal Specification

This document defines **invariants, state model, typed interfaces, CLI contract, and determinism guarantees** for `canon-mini-agent`.

## 1. State Model

The system is a deterministic event-driven loop with explicit roles.

### 1.1 Global State
- `Workspace`: absolute root path of the target project being operated on. Set via `--workspace <path>` CLI argument; if omitted, `run()` defaults it to `env!("CARGO_MANIFEST_DIR")` (the canon-mini-agent source root in this build). Must be absolute. All relative paths in actions resolve against this value.
- `AgentStateDir`: operational state for canon-mini-agent itself. Defaults to `/workspace/ai_sandbox/canon-mini-agent/agent_state`; overridable via `--state-dir`. When `Workspace` equals the canon-mini-agent source root, the system is in **self-modification mode** (see §9).
- `SelfModificationMode`: true when `Workspace` is the parent directory of `AgentStateDir`. Detected at runtime via `is_self_modification_mode()` in `src/constants.rs`. Relaxes only the executor rule for `SPEC.md` patching; source-file patching is already allowed by the current scope-guard implementation in both modes.
- `Role`: one of `{Planner, Executor, Verifier, Diagnostics, Solo}`.
- `Lane`: executor lane id (e.g., `executor_pool`), bound to a role of type Executor.
- `PromptKind`: `{planner, executor, verifier, diagnostics}`.
- `Action`: a typed JSON object (see Section 3).
- `ActionResult`: `{ complete: bool, output: string }`.
- `RunConfig`: timeouts, tool availability, and patch scope policy.

### 1.2 Role State (per agent)
- `prompt_kind: PromptKind`
- `step: u64` (monotonic, starts at 1 per role cycle)
- `last_action: Action?`
- `last_result: ActionResult?`
- `lane_plan_path: string?` (Executors only)

### 1.3 Canonical Files
Canonical file paths are absolute under `Workspace` (see `src/constants.rs:3-11`, `42-48`):
- `Spec`: `SPEC.md`
- `Objectives`: `PLANS/OBJECTIVES.md` (authoritative MD) / `PLANS/OBJECTIVES.json` (derived JSON)
- `Invariants`: `INVARIANT.md` (authoritative MD) / `INVARIANTS.json` (derived JSON)
- `MasterPlan`: `PLAN.json`
- `LanePlan`: `PLANS/<instance>/executor-<id>.json` (preferred) or legacy `PLANS/executor-<id>.md` (see `src/tools.rs:49-63`)
- `Violations`: `VIOLATIONS.json`
- `Diagnostics`: runtime-configured via `diagnostics_file()` (default `DIAGNOSTICS.json`, see `src/constants.rs:149-156`) or instance-scoped path

### 1.4 Workspace Resolution Rule
Every `path` field in every action is resolved as follows:
1. If already absolute and under `Workspace`, use as-is.
2. If absolute and under `/tmp`, use as-is.
3. If relative, reject any `..` component and then join directly with `Workspace`.
4. No canonicalization step is performed by `safe_join()`; enforcement is prefix- and component-based rather than realpath-based.
5. If absolute but outside `Workspace` and not in `/tmp`, the action is rejected with a scope violation.

## 2. CLI Interface

```
canon-mini-agent [FLAGS] [OPTIONS]
```

### 2.1 Flags
| Flag            | Description                                                                                        |
|-----------------+----------------------------------------------------------------------------------------------------|
| `--orchestrate` | Run the full multi-role orchestration loop (planner → executors → verifier/diagnostics → planner). |
| `--planner`     | Run only the planner role (single role mode).                                                      |
| `--verifier`    | Run only the verifier role (single role mode).                                                     |
| `--diagnostics` | Run only the diagnostics role (single role mode).                                                  |

### 2.2 Options
| Option               | Default                       | Description                                                                                                                                |
|----------------------+-------------------------------+--------------------------------------------------------------------------------------------------------------------------------------------|
| `--workspace <path>` | build-time source root        | **Absolute path** to the target project workspace. All agent file operations resolve relative to this path. Must exist and be a directory. |
| `--state-dir <path>` | `/workspace/ai_sandbox/canon-mini-agent/agent_state` | **Absolute path** to canon-mini-agent's own runtime state directory for checkpoints, wake flags, and inbound messages. |
| `--start <role>`     | `executor`                    | Start role for orchestration: `executor`, `verifier`, `planner`, `diagnostics`, or `solo`.                                                 |
| `--role <role>`      | none                          | Single-role selector: `executor`, `verifier`, `planner`, or `diagnostics`. Mutually exclusive with other role flags and `--orchestrate`.    |
| `--instance <id>`    | `default`                     | Instance identifier used to namespace PLANS subdirectories and diagnostics files.                                                          |
| `--port <port>`      | auto                          | WebSocket port for Chrome extension. Auto-selects from candidates if not specified.                                                        |

### 2.3 Workspace Validation
- `--workspace` value must be an absolute path (starts with `/`). Non-absolute paths are rejected at startup with a fatal error.
- If omitted, `run()` sets the workspace to `env!("CARGO_MANIFEST_DIR")`, which is the canon-mini-agent source root for the current binary.
- `--state-dir` value must also be an absolute path when provided; non-absolute values are rejected at startup with a fatal error.
- The runtime value is stored in a process-global `OnceLock<String>` and never changes after startup.

## 3. Typed Interfaces (Actions)

All actions are JSON objects with a mandatory `"action"` string field. Any missing required field is an error.

### 3.1 Common Action Envelope
```json
{ "action": "<type>", "rationale": "<why now>", "observation"?: "<why>" }
```
Notes:
- `rationale` is required.
- `observation` is action-dependent. Some flows and tests require it for `message`, while other action kinds may omit it and rely on normalization or validation paths.

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

### 3.4 `plan`
```json
{ "action": "plan", "op": "create_task | update_task | delete_task | add_edge | remove_edge | set_status", ... }
```
Notes:
- `create_task` / `update_task` require `task` object with at least `id`. `create_task` requires `title`.
- `delete_task` requires `task_id`.
- `add_edge` / `remove_edge` require `from` and `to` task ids.
- `set_status` requires `status` (`in_progress | blocked | complete`).
- The plan tool enforces a DAG (no cycles) when adding edges.

### 3.5 `apply_patch`
```json
{ "action": "apply_patch", "patch": "<string>" }
```
Constraints: patch must follow unified patch grammar. The first `*** Update File:` or `*** Add File:` path determines scope. Executor scope guards apply.

### 3.6 `run_command`
```json
{ "action": "run_command", "cmd": "<string>", "cwd"?: "<string>" }
```
Constraints: `cwd` defaults to `Workspace`. Must be under `Workspace` or `/tmp`.

### 3.7 `python`
```json
{ "action": "python", "code": "<string>", "cwd"?: "<string>" }
```
Constraints: `cwd` defaults to `Workspace`. Write operations must target paths under `Workspace` or `/tmp`.

### 3.8 `cargo_test`
```json
{ "action": "cargo_test", "crate": "<string>", "test": "<string>" }
```
Semantics: maps to `cargo test -p <crate> <test> -- --exact --nocapture`.

### 3.9 `rustc_hir`
```json
{ "action": "rustc_hir", "crate": "<string>", "mode"?: "<string>", "extra"?: "<string>" }
```
Semantics: maps to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>`.

### 3.10 `rustc_mir`
```json
{ "action": "rustc_mir", "crate": "<string>", "mode"?: "<string>", "extra"?: "<string>" }
```
Semantics: maps to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>`.

### 3.11 `graph_call` / `graph_cfg`
```json
{ "action": "graph_call", "crate": "<string>", "out_dir"?: "<string>" }
{ "action": "graph_cfg",  "crate": "<string>", "out_dir"?: "<string>" }
```
Outputs: CSVs plus `callgraph.symbol.txt` / `cfg.symbol.txt` with symbol→symbol edges.

### 3.12 `graph_dataflow` / `graph_reachability`
```json
{ "action": "graph_dataflow",    "crate": "<string>", "tlog"?: "<string>", "out_dir"?: "<string>" }
{ "action": "graph_reachability","crate": "<string>", "tlog"?: "<string>", "out_dir"?: "<string>" }
```
Outputs: JSON reports under metrics/analysis directories.

### 3.13 `message` (Inter-Agent Handoff Protocol)
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

| From        | To       | Type         | Status             | Payload (required keys)                     |
|-------------+----------+--------------+--------------------+---------------------------------------------|
| Executor    | Verifier | handoff      | complete           | `summary`, `artifacts`                      |
| Executor    | Planner  | handoff      | complete / blocked | `summary`, `evidence`                       |
| Verifier    | Planner  | verification | verified / failed  | `summary`, `verified_items` / `false_items` |
| Verifier    | Planner  | failure      | failed             | `summary`, `next_actions`                   |
| Diagnostics | Planner  | diagnostics  | complete           | `summary`, `ranked_failures`                |
| Planner     | Executor | tasking      | ready / blocked    | `summary`, `tasks` / `blockers`             |
| Solo        | Solo     | result       | complete           | `summary`                                   |

**Routing guarantee (added 2026-04-07):** When a role emits a `message` action, the system writes `last_message_to_<to>.json` and `wakeup_<to>.flag` to `AgentStateDir` and sets `planner_pending = true` (for planner-targeted messages). This ensures the target role wakes on the next orchestration cycle regardless of whether the action was emitted in the inline or deferred completion path.
**Solo note:** Solo is an orchestrated role. It may send messages to any other role; wakeup flags are honored for `solo` like the other roles.

## 4. Invariants (Must Always Hold)

### 4.1 Scope Invariants

**Normal mode** (workspace ≠ orchestrator source):
- **Executor** may not patch `SPEC.md`, `PLAN.json`, `INVARIANTS.json`, `VIOLATIONS.json`, `OBJECTIVES.json`, any lane plan, or diagnostics files. Current implementation allows executor patches under `src/` in normal mode because `patch_scope_error()` does not block `src/` targets for executor.
- **Verifier** may patch **only** `PLAN.json` and `VIOLATIONS.json`.
- **Diagnostics** may patch **only** the active diagnostics report file.
- **Planner** may patch **only** `PLAN.json` and lane plans.
- **Solo** may patch any in-workspace file (full capabilities).

**Self-modification mode** (workspace == orchestrator source, see §9):
- **Executor** may additionally patch `SPEC.md`. Current implementation continues to allow `src/` patching here as well.
- All other role restrictions are unchanged.

Enforcement: `src/tools.rs::patch_scope_error()` (see `src/tools.rs:363-477`). Changes to that function require verifier sign-off (I13).

Additional clarification (from implementation):
- Executor is blocked from patching any non-`src/` files in normal mode via `touches_other` guard (`src/tools.rs:379-388`, `392-409`).
- Diagnostics file path is dynamically resolved (`diagnostics_file()`), and both configured and legacy `DIAGNOSTICS.json` are accepted (`src/tools.rs:369-377`).
- Lane plan detection includes both instance-scoped and legacy formats (`src/tools.rs:49-63`).

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
  -> Solo (when scheduled)
  -> Executor(s) [parallel lanes]
  -> Verifier
  -> Diagnostics (conditional)
  -> Planner
```
The orchestrator uses wakeup flags and `planner_pending` / `diagnostics_pending` to schedule transitions. Phase order may be overridden by verifier blocker messages.

### 5.3 Handoff Transition
```
Role emits message{to=Planner}
  -> persist_inbound_message() writes last_message_to_planner.json + wakeup_planner.flag
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
  "version": 2,
  "status": "in_progress | blocked | complete",
  "tasks": [
    {
      "id": "T1",
      "title": "Short deterministic label",
      "status": "todo | in_progress | blocked | done",
      "priority": 1,
      "steps": ["read file", "patch", "test"]
    }
  ],
  "dag": {
    "edges": [
      { "from": "T1", "to": "T2" }
    ]
  }
}
```
Notes:
- `tasks` are DAG nodes.
- `dag.edges` defines dependencies (`from` must complete before `to`).
- The plan tool enforces DAG acyclicity.

### 7.3 Task Protocol
```json
{
  "id": "<uuid>",
  "title": "<short deterministic label>",
  "status": "todo | in_progress | blocked | done",
  "priority": 1,
  "steps": ["read file", "patch", "test"]
}
```

### 7.4 Lane Execution Rules
- Execute top 1–10 tasks with `status=todo`.
- Respect dependencies: `∀ edge(from->to): status(from)=done before status(to)!=todo`.
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

## 9. Self-Modification Mode

Self-modification mode is active when `Workspace` equals the canon-mini-agent source root (i.e., the parent of `AgentStateDir`). Detection: `is_self_modification_mode()` in `src/constants.rs`.

### 9.1 Purpose
Allows canon-mini-agent to act as its own target workspace — reading source files, patching `SPEC.md`, and improving `src/` code under the direction of the planner and verification of the verifier.

### 9.2 Relaxed Executor Scope
In self-modification mode only:
- Executor **may** patch `SPEC.md` directly.
- Executor patching of `src/` remains allowed by the current implementation, as in normal mode.
- All other scope restrictions remain in force (no patching `PLAN.json`, `INVARIANTS.json`, `VIOLATIONS.json`, lane plans, or diagnostics).

### 9.3 Safety Requirements (Invariants I11–I14)

**I11 — Build gate always-on:** After any `src/` or `SPEC.md` patch, executor must run `cargo build --workspace` before emitting a `message` handoff. A broken build is a blocker; the executor must fix or revert the patch in the same turn. This applies regardless of `check_on_done` setting.

**I12 — SPEC.md evidence requirement:** Every SPEC.md edit must cite the source file and approximate line range that implements the claim. Executor must read the relevant `src/` file before writing any SPEC.md claim about it. Verifier must independently verify every cited location.

**I13 — No permission escalation:** Executor must not patch `src/tools.rs::patch_scope_error` or any other scope-guard logic in a way that expands role permissions beyond SPEC.md §4. Any such patch requires verifier sign-off with explicit SPEC.md §4 justification.

**I14 — Checkpoint compatibility:** Changes to `OrchestratorCheckpoint` fields must use `#[serde(default)]` for additions. Removing or renaming fields requires a version bump or checkpoint discard on load. The `workspace` field must always be populated on save and validated on load.
Checkpoint `phase` values include `planner`, `executor`, `verifier`, `diagnostics`, and `solo`.

### 9.4 Safety Properties (Why It's Safe)
- **No mid-run corruption:** The running orchestrator binary is already loaded into memory. Patching `src/` files does not affect the current process — changes take effect only on the next `cargo build` + process restart.
- **Build gate prevents bad state:** A broken `cargo build` after a patch means the executor must fix or revert before handoff, so the repository never rests in a non-building state.
- **Scope guards protect the plan layer:** Even in self-modification mode, the executor cannot touch `PLAN.json`, `INVARIANTS.json`, or `VIOLATIONS.json`, preserving planner authority.
- **Verifier provides independent evidence check:** The verifier reads cited source independently before accepting any SPEC.md change, preventing hallucinated claims from becoming spec.

### 9.5 Prohibited Even in Self-Modification Mode
- Patching `PLAN.json`, `INVARIANTS.json`, `VIOLATIONS.json`, `OBJECTIVES.json`, lane plans, or diagnostics files.
- Removing or weakening scope guards without verifier sign-off (I13).
- Emitting `message{status=complete}` when `cargo build --workspace` fails (I11).
- Writing SPEC.md claims without reading and citing the corresponding source (I12).

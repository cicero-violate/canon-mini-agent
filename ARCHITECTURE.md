# Architecture

> Non-authoritative overview. The canonical behavioral contract is `SPEC.md`; invariants are in `INVARIANTS.json` and `CANONICAL_LAW.md`. When this document conflicts with those sources, those sources win.

---

## What This System Is

canon-mini-agent is an autonomous AI system that orchestrates multiple LLM-powered agents to accomplish goals in a target workspace — including modifying its own source code. Three capabilities define it:

- **Self-building**: The executor can read, critique, and patch `src/`, `tests/`, and `SPEC.md` without human intervention.
- **Self-directing**: Every LLM response includes `predicted_next_actions`, forming a decision tree that drives orchestration without waiting for external instruction.
- **Self-learning**: Observed failures are written to a lessons artifact, injected into the next planner cycle, and enforced until objectives or plans change.

---

## High-Level Structure

```
main.rs
  └─> run()  (app.rs)
        ├─> Planner phase
        ├─> Executor phase  (multi-lane, async)
        ├─> Verifier phase
        ├─> Diagnostics phase
        └─> Solo phase  (unified planner+executor)

tools.rs          — action execution engine
prompts.rs        — role-specific prompt construction
prompt_inputs.rs  — context loading for prompts
invalid_action.rs — action validation + corrective feedback
state_space.rs    — pure routing/decision functions
logging.rs        — event log subsystem
protocol.rs       — inter-agent message enums
constants.rs      — paths, limits, endpoint config
```

Supervisor binary (`bin/canon-mini-supervisor.rs`) wraps the agent, watches binary mtime, and auto-restarts on recompilation.

---

## Roles

| Role | Responsibility |
|------|---------------|
| **Planner** | Manages `PLAN.json` and `PLANS/OBJECTIVES.json`; reads violations, lessons, and diagnostics to steer lanes |
| **Executor** | Executes tasks in assigned lanes; applies patches, runs tests, sends handoff messages |
| **Verifier** | Validates executor claims against source evidence; writes `VIOLATIONS.json` |
| **Diagnostics** | Scans logs and state artifacts; ranks failures; emits per-instance diagnostic reports |
| **Solo** | Combined planner+executor in a single agent for lightweight or self-directed runs |

Roles share no live memory. All coordination happens through files and wake flags.

---

## Orchestrator Loop

Each iteration of the main loop in `app.rs`:

1. **Read wake flags** — `wakeup_<role>.flag` files determine which role is scheduled next. `decide_wake_flags()` resolves priority.
2. **Planner phase** — if allowed (`allow_planner_run()`), load plan context, submit prompt, update lane states.
3. **Executor phase** — for each active lane, `claim_executor_submit()` reserves the lane, `submit_executor_turn()` dispatches async to the LLM endpoint.
4. **Collect completions** — `bridge.take_completed_turns()` drains finished WebSocket responses. Each is parsed, validated, and routed.
5. **Drain continuations** — tool execution runs: file ops, patches, shell commands, plan updates, message handoffs.
6. **Verifier phase** — async verification per completed executor lane; results written to `VIOLATIONS.json`.
7. **Diagnostics phase** — if allowed, Python scan of workspace state artifacts; ranked failure report emitted.
8. **Livelock detection** — if no watched files changed for N cycles, idle flags are cleared and a diagnostic report is written.

The loop continues until a completion condition or error. Mutual exclusion between phases is enforced by `scheduled_phase_*` state: only one role runs at a time per slot.

---

## State Space (Pure Routing)

`state_space.rs` contains all orchestration decisions as pure functions — identical inputs always produce identical outputs. Key predicates:

- `decide_wake_flags()` — resolves active role from priority queue of wake flags
- `decide_active_blocker()` — applies blocker suppression rules to pending planner
- `allow_verifier_run()`, `allow_planner_run()`, `allow_diagnostics_run()` — phase gating
- `block_executor_dispatch()` — freezes executor dispatch during exclusive phases
- `executor_step_limit_exceeded()` — enforces the 20-action-per-lane limit

Every branch is covered by `state_space_tests.rs`.

---

## Tool Execution Engine (`tools.rs`)

Actions requested by LLM responses are dispatched here. Categories:

**File operations**
- `list_dir`, `read_file` — inspection
- `apply_patch` — unified diff application (`*** Begin Patch` / `*** Add File` / `*** Update File` anchors)

**Code manipulation**
- `rename_symbol` — Rust identifier rename via rust-analyzer syntax
- `symbols_index`, `symbols_rename_candidates`, `symbols_prepare_rename` — symbol inventory and rename preparation

**Execution**
- `run_command` — shell execution with captured output
- `python` — Python script execution
- `cargo_test` — targeted test harness

**Planning**
- `plan` — CRUD on `PLAN.json` DAG tasks
- `objectives` — CRUD on `PLANS/OBJECTIVES.json`
- `issue` — issue tracking
- `message` — inter-agent handoff delivery

**Code analysis**
- `rustc_hir`, `rustc_mir` — compiler IR inspection
- `graph_call`, `graph_cfg`, `graph_dataflow`, `graph_reachability` — static analysis

### Scope Guards

`patch_scope_error()` enforces per-role write permissions:

| Role | Allowed writes |
|------|---------------|
| Executor (normal) | Workspace only |
| Executor (self-mod) | Workspace + `src/`, `tests/`, `SPEC.md` |
| Verifier | `VIOLATIONS.json` only (`PLAN.json` must use `plan` action) |
| Diagnostics | Diagnostics report file only |
| Planner | Lane plans + `PLANS/OBJECTIVES.json` (`PLAN.json` must use `plan` action) |
| Solo | Full workspace access |

Self-modification mode activates when `WORKSPACE == agent_state_dir.parent`.

---

## Handoff Protocol

Inter-role messages use `action="message"` with fields `from`, `to`, `type`, `status`.

Delivery:
1. Write `last_message_to_<role>.json`
2. Write `wakeup_<role>.flag`

Consumption:
- `take_inbound_message(role)` reads and deletes the file exactly once (invariant I10).

Message types (from `protocol.rs`): `Handoff`, `Result`, `Verification`, `Failure`, `Blocker`, `Plan`, `Diagnostics`.

---

## Action Validation (`invalid_action.rs`)

Before any action is executed:

1. **Schema check** — JSON structure validated against tool schema.
2. **Required field enforcement** — every mutating action (`apply_patch`, `plan`, `objectives`, `rename_symbol`) must include a non-empty `question` field (decision-boundary question).
3. **Message format check** — `from`/`to`/`type`/`status` must be valid enum values.
4. **Corrective feedback** — invalid actions produce `corrective_invalid_action_prompt()` output, which is injected as the next user turn. The agent retries with guidance; the original payload is never retried unchanged.
5. **Cargo test gate** — completion messages are blocked if recent test failures exist (invariant I13).

---

## Prompt Construction (`prompts.rs`, `prompt_inputs.rs`)

Each role receives a freshly constructed prompt every cycle. Context loaded per role:

- **Planner**: master plan, objectives, violations, diagnostics, lessons artifact
- **Executor**: assigned lane plan, inbound message, workspace file tree
- **Verifier**: executor diff, cargo test failures, prior violations
- **Diagnostics**: log scan instructions, ranked failure templates
- **Solo**: combined planner+executor context

Tool descriptions are role-filtered (diagnostics skips verification tools; verifier skips certain write tools). Long inputs are truncated at `MAX_SNIPPET` (20 KB).

---

## Lessons & Learning

`agent_state/lessons.json` is a structured artifact:

```json
{
  "summary": "...",
  "failures": [...],
  "fixes": [...],
  "required_actions": [...]
}
```

- Rendered and injected into the planner prompt at cycle start.
- Enforcement signal: if lessons exist but neither objectives nor the plan changed, an audit event is logged as a signal for the next cycle.

---

## Logging (`logging.rs`)

All events are written as JSONL:

| Stream | Contents |
|--------|----------|
| `agent_state/default/actions.jsonl` | Per-role action audit log |
| `agent_state/default/log.jsonl` | Execution log |
| `agent_state/llm_full/` | Full prompt/response pairs (timestamped JSON) |

Event types: `llm_request`, `llm_response`, `llm_action_result`, `llm_error`, `orchestration_trace`, `learning_loop_audit`.

---

## Runtime Artifacts

**Workspace-local** (in the target project):

```
SPEC.md                          — behavioral contract
PLAN.json                        — master task DAG
PLANS/OBJECTIVES.json            — objective registry
PLANS/<instance>/executor-*.json — lane plans
VIOLATIONS.json                  — verifier findings
PLANS/<instance>/diagnostics-*.json
ISSUES.json
```

**Agent-local** (in `agent_state/`):

```
default/actions.jsonl
default/log.jsonl
llm_full/<timestamp>.json
last_message_to_<role>.json      — inbound handoffs
wakeup_<role>.flag               — wake signals
orchestrator_cycle_idle.flag
active_blocker_to_verifier.json
```

---

## Configuration (`constants.rs`)

| Setting | Default |
|---------|---------|
| `MAX_STEPS` | 2000 per role cycle |
| `EXECUTOR_STEP_LIMIT` | 20 actions before mandatory handoff |
| `MAX_SNIPPET` | 20 KB truncation limit |
| Planner/solo timeout | 600 s |
| Verifier/diagnostics timeout | 120 s |
| Executor timeout | 30 s |
| WebSocket port range | 9103–9108 (auto-selected) |

`WORKSPACE` is set via `--workspace` at startup and never changes during a run (invariant I4).

---

## CLI

```
canon-mini-agent [OPTIONS]
```

Key flags:

| Flag | Effect |
|------|--------|
| `--orchestrate` | Full multi-role loop |
| `--solo` | Solo combined mode |
| `--planner` / `--verifier` / `--diagnostics` | Single role |
| `--workspace <PATH>` | Target project root |
| `--state-dir <PATH>` | Agent state directory |
| `--start <ROLE>` | Initial phase (default: executor) |
| `--instance <ID>` | Instance ID for namespaced plans |

---

## Authority Hierarchy

```
LAW (CANONICAL_LAW.md)
  └─> SPEC (SPEC.md)
        └─> INVARIANT (INVARIANTS.json)
              └─> OBJECTIVE (PLANS/OBJECTIVES.json)
                    └─> PLAN (PLAN.json)
```

Lower levels cannot override higher levels. The planner operates within objectives; the executor operates within the plan.

---

## Self-Improvement Cycle

```
Specification (SPEC.md)
    ↓
Planner assigns tasks to lanes
    ↓
Executor patches code, runs cargo test
    ↓  fails → correction loop
Verifier checks claims against source evidence
    ↓  violations → VIOLATIONS.json
Planner reads violations + lessons, updates objectives/plan
    ↓
Diagnostics scans failures, ranks repair targets
    ↓
(repeat)
```

Build failures always block progression. No broken state persists across restarts: the build gate (invariant I13) enforces this.

---

## Key Invariants (summary)

| ID | Rule |
|----|------|
| I1 | Message actions must wake the target role via flag file |
| I2 | No role may patch files outside its authorized scope |
| I4 | `WORKSPACE` is fixed at startup and never mutated |
| I10 | Inbound messages consumed exactly once |
| I13 | Build gate must pass before a completion message is accepted |

Full invariant definitions are in `INVARIANTS.json` and `INVARIANTS.md`.

---

## Source Sizes (approximate)

| File | LOC | Role |
|------|-----|------|
| `tools.rs` | ~4800 | Action execution |
| `app.rs` | ~4000 | Orchestrator loop |
| `prompts.rs` | ~1500 | Prompt generation |
| `prompt_inputs.rs` | ~930 | Context loading |
| `invalid_action.rs` | ~910 | Validation + correction |
| `logging.rs` | ~900 | Event logging |
| `tool_schema.rs` | ~850 | JSON schemas |
| `state_space_tests.rs` | ~405 | State machine tests |
| `state_space.rs` | ~335 | Pure routing functions |
| `orchestrator_harness.rs` | ~1180 | Integration regression tests |

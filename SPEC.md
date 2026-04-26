# canon-mini-agent Formal Specification

This document defines **invariants, state model, typed interfaces, CLI contract, and determinism guarantees** for `canon-mini-agent`.

## 0. System Purpose — Autonomous Self-Building AI

canon-mini-agent is a prototype for a **self-building, self-directing, self-learning autonomous system**. All architectural decisions flow from three meta-goals:

### 0.1 Self-Building
The system can read and improve its own source code. In self-modification mode (`Workspace` == orchestrator source root), the executor role may patch `src/`, `tests/`, and `SPEC.md` directly. After every patch the build gate (`cargo build --workspace && cargo test --workspace`) must pass — a broken build is always a blocker. This ensures the system can never leave itself in a non-runnable state.

### 0.2 Self-Direction
Every LLM response includes a `predicted_next_actions` field — an ordered array of 2–3 likely follow-on actions. This is the agent's declared decision tree. It:
- Lets the agent drive itself across turns without waiting for external instruction.
- Makes the agent's intent transparent and auditable.
- Enables loop detection: if the same `predicted_next_actions` array repeats without progress, the orchestrator can inject a blocker prompt.

`predicted_next_actions` format:
```
[
  {"action": "<action_name>", "intent": "<one-line reason>"},
  ...
]
```

### 0.6 Structured Decision Questions

Before every mutating action (`apply_patch`, `plan`, `objectives`, `issue`, `rename_symbol`), the agent must emit a `question` field: the single decision-boundary question this action answers. The question identifies the premise the action depends on — if the premise were false, a different action would be taken. This makes the agent's assumptions explicit and auditable.

Three questions are selected per turn from a 20-question bank in `src/structured_questions.rs` and injected into the agent prompt via `rules_common_footer`. The selection rotates across all 20 questions over time. The intent is to surface different failure-mode questions (provenance, redundancy, scope, cascade, deferral, verifiability, role) across many turns rather than habituating the agent to a fixed list.

The `question` field is enforced as a required field for mutating actions by `invalid_action_expected_fields` in `src/invalid_action.rs`. Missing it generates corrective feedback.
Additionally, corrective/error feedback emitted after failed actions must explicitly remind the model to include a decision-boundary `question` before any mutating retry action.

### 0.3 Self-Learning
The agent reads its own execution history (`agent_state/` logs, `VIOLATIONS.json`, prior `PLAN.json` states) at the start of each planner cycle. It must:
- Identify repeated failures and encode the root cause in the next task's `steps`.
- Update `SPEC.md` when runtime behavior diverges from the spec.
- Never re-close a task that was re-opened without adding a regression test to prevent recurrence.

### 0.4 Canonical Recovery Engine

All recurring failures must pass through a typed recovery loop:

```text
T=tlog
K=ErrorClass
R=RecoveryPolicy
A=RecoveryAction
E=eval

T → classify(K) → detect repeat(K) → choose R(K) → emit A evidence → evaluate E
```

Recovery decisions are pure policy outputs. Runtime recovery may only record
typed `EffectEvent` evidence and apply existing `ControlEvent` transitions
through `CanonicalWriter`. Recovery must not silently mutate source files,
`PLAN.json`, projected issue files, or generated graph artifacts.

Invariant:

```text
∀K: repeated(K,T) ≥ Θ_K ⇒ typed RecoveryDecision(K)
```

The default recovery table must contain exactly one enabled threshold for every
`ErrorClass`. Structural safety violations escalate to solo, runtime-state
divergence replays/purges invalid runtime state, transport failures retire and
retry the role, compile/verification evidence routes back to executor, missing
targets route back to planner, and unknown/repeated schema/blocker failures
route to diagnostics.

Every recovery threshold carries a retry budget. If the recent tlog window
already contains `max_attempts` `RecoveryTriggered` records for the same
`ErrorClass`, runtime must not actuate that recovery again. It must emit
`RecoverySuppressed { suppression_reason: "retry_budget_exhausted ..." }`
instead, preserving forward progress evidence while preventing recovery
livelock.

Eval must report recovery attempts, successes, failures, suppressions, loop
breaks, regressions, measurement points, and recovery effectiveness. Runtime
recovery must emit a typed `RecoveryOutcomeRecorded` event after a recovery
attempt so eval can score `attempted → succeeded|failed` directly before
falling back to heuristic windows.

#### Plan Verify Escalation (Repair Plan → Blocker Feedback Loop)

When a repair plan's `machine_verify` fails repeatedly, the failure escalates
into the blocker-class system, feeding back into eval:

```text
∀plan: count_consecutive_verify_failures(plan.id, T) ≥ 3
  ⇒ append_blocker(ErrorClass::VerificationFailed)
  ⇒ blockers.json gains "verification_failed" entry
  ⇒ compute_blocker_class_coverage detects uncovered class
  ⇒ blocker_class_coverage score drops → eval pressure
  ⇒ REPAIR_PLAN(blocker_class_coverage) fires
  ⇒ planner creates task → executor patches invariant_discovery.rs
  ⇒ enforced_invariants.json covers "verification_failed"
  ⇒ blocker_class_coverage recovers
  ⇒ machine_verify passes → PlanVerifyRecorded(passed=true) → task closed
```

This closes the full eval feedback loop: a stuck repair plan produces a
measurable eval regression that drives a concrete fix. Implemented in
`src/eval_driver.rs` (escalation loop) and `src/repair_plans.rs`
(`count_consecutive_verify_failures`).

### 0.5 Objective Evolution

At the end of every orchestration cycle, planner MUST review `agent_state/OBJECTIVES.json` and update it:

- **Add** new objectives for any capability gap, invariant, or sub-goal discovered this cycle.
- **Update** existing objective status when state changes (active → done, blocked, deferred).
- **Never remove** an objective entry — use `"status": "deferred"` with a reason.

Formal objectives JSON Schema (draft-07, current runtime shape):
```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "ObjectivesFile",
  "type": "object",
  "additionalProperties": true,
  "properties": {
    "version": { "type": "integer", "minimum": 0 },
    "objectives": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": true,
        "properties": {
          "id": { "type": "string" },
          "title": { "type": "string" },
          "status": { "type": "string" },
          "scope": { "type": "string" },
          "authority_files": { "type": "array", "items": { "type": "string" } },
          "category": { "type": "string" },
          "level": { "type": "string" },
          "description": { "type": "string" },
          "requirement": { "type": "array" },
          "verification": { "type": "array" },
          "success_criteria": { "type": "array" },
          "repair_plan_ids": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Stable repair plan ids (e.g. 'eval_metric:blocker_class_coverage') whose machine_verify must all pass before this objective is done. When all pass, eval_driver emits PlanVerifyRecorded('objective:{id}', passed=true) as a planner hint."
          }
        }
      }
    },
    "goal": { "type": "array" },
    "instrumentation": { "type": "array" },
    "definition_of_done": { "type": "array" },
    "non_goals": { "type": "array" }
  }
}
```

Notes:
- Fields are optional at the schema level to match runtime defaults in `src/objectives.rs`.
- `status`, `scope`, and `authority_files` are first-class fields; do not embed them only in `description`.
- `description` may still include a Status/Scope/Authority checklist, but the authoritative values live in the top-level fields.
- `repair_plan_ids` binds an objective to one or more repair plans by stable id. When all linked plan ids have `PlanVerifyRecorded(passed=true)` in tlog, `eval_driver` emits an objective-verified hint and the planner marks the objective `done`. An objective with an empty `repair_plan_ids` array is managed purely by planner judgment. An objective with `success_criteria: []` and empty `repair_plan_ids` is considered non-functional — the planner must fill in at least one.

This is enforced by planner-cycle prompt rules and objectives action validation. Planner should treat any unreviewed `agent_state/OBJECTIVES.json` state as an immediate process gap.

### 0.4 Safety Properties
Self-improvement is safe because:
- The build gate prevents a broken `src/` from persisting across restarts.
- Scope guards prevent any single role from unilaterally rewriting authority files (`INVARIANTS.json`, `PLAN.json`).
- Planner performs in-cycle verification/diagnostics before dispatching or accepting executor progress.
- `predicted_next_actions` is advisory — the agent's prediction for turn N+1 is not binding; actual evidence at turn N+1 always takes precedence.

## 1. State Model

The system is a deterministic event-driven loop with two active runtime roles.

### 1.1 Global State
- `Workspace`: absolute root path of the target project being operated on. Set via `--workspace <path>` CLI argument; if omitted, `run()` defaults it to `env!("CARGO_MANIFEST_DIR")` (the canon-mini-agent source root in this build). Must be absolute. All relative paths in actions resolve against this value.
- `AgentStateDir`: operational state for canon-mini-agent itself. Defaults to `/workspace/ai_sandbox/canon-mini-agent/agent_state`; overridable via `--state-dir`. When `Workspace` equals the canon-mini-agent source root, the system is in **self-modification mode** (see §9).
- `SelfModificationMode`: true when `Workspace` is the parent directory of `AgentStateDir`. Detected at runtime via `is_self_modification_mode()` in `src/constants.rs`. In this mode, executor scope is relaxed to allow patching `SPEC.md`, `src/`, and `tests/`; normal mode blocks executor patching of `SPEC.md`, `src/`, and `tests/`.
- `Role`: one of `{Planner, Executor}` for active runtime orchestration (legacy role tags may exist in historical artifacts).
- `Lane`: executor lane id (e.g., `executor_pool`), bound to a role of type Executor.
- `PromptKind`: `{planner, executor}` for active runtime orchestration (legacy prompt kinds remain for backward compatibility/testing).
- `Action`: a typed JSON object (see Section 3).
- `ActionResult`: `{ complete: bool, output: string }`.
- `RunConfig`: timeouts, tool availability, and patch scope policy.

### 1.5 Canonical Writer Infrastructure

The orchestrator state machine is implemented as a **Canonical Writer** — a single deterministic gate for all serializable state mutations:

```
W(s_t, e) → s_{t+1}
```

Every state transition is represented as a typed `ControlEvent` variant, appended to the total-ordered log before the state transition executes. The gate is enforced by `CanonicalWriter::apply` in `src/canonical_writer.rs`.

Hard boundary rules:
- Every canonical `SystemState` change during normal runtime must flow through `CanonicalWriter::apply(ControlEvent)`.
- `CanonicalWriter::apply` validates the post-transition state before committing it.
- `EffectEvent`s are append-only observability records; they never advance `SystemState`.
- The only permitted non-`apply` `SystemState` replacement is explicit checkpoint hydration via `CanonicalWriter::restore_from_checkpoint(...)`.
- A second canonical mutation path is a structural bug (`ErrorClass::SecondMutationPath`).

**Components:**

| Component         | Type                      | Role                                                                     |
|-------------------+---------------------------+--------------------------------------------------------------------------|
| `CanonicalWriter` | `src/canonical_writer.rs` | Single mutation gate: `apply(ControlEvent)` → logs then transitions      |
| `SystemState`     | `src/system_state.rs`     | All serializable orchestrator state (phase, lanes, pending flags, diffs) |
| `RuntimeState`    | `src/app.rs`              | Non-serializable runtime-only state (tab handles, in-flight join sets)   |
| `ControlEvent`    | `src/events.rs`           | Typed enum of all valid state transitions (23 variants)                  |
| `EffectEvent`     | `src/events.rs`           | Side-effect record (invariant violations, checkpoints)                   |
| `Tlog`            | `src/tlog.rs`             | Append-only NDJSON event log at `AgentStateDir/tlog.ndjson`              |

**Invariant gate:** Before emitting a `ControlEvent`, callers invoke `evaluate_invariant_gate` (`src/invariants.rs`). On violation the caller calls `writer.record_violation(...)` — which appends an `EffectEvent::InvariantViolation` to the tlog without advancing state — and aborts the transition.

**Tlog-driven route recovery:** If the route gate repeatedly blocks executor dispatch for the same missing-target reason, the runtime must treat the repeated `InvariantViolation` records in `AgentStateDir/tlog.ndjson` as recovery evidence. The recovery path must be canonical: clear stale executor lane pending state with `ControlEvent::LanePendingSet`, consume stale executor wake with `ControlEvent::WakeSignalConsumed`, route control back to planner with `ControlEvent::ScheduledPhaseSet { phase: Some("planner") }` plus `PlannerPendingSet { pending: true }`, then emit `RecoveryOutcomeRecorded` for eval feedback.

**`RuntimeState` fields** (never serialized, never checkpointed):
- `submitted_turns: HashMap<(u32, u64), SubmittedExecutorTurn>` — active executor turns keyed by `(tab_id, turn_id)`
- `executor_submit_inflight: HashMap<usize, PendingSubmitState>` — in-flight submit tasks per lane
- `deferred_completions: HashMap<usize, VecDeque<DeferredExecutorCompletion>>` — completions queued while another is processing

**Intentionally ephemeral runtime-only behaviors** (must remain bounded and justified):
- live tab handles / browser session objects — OS resources, not replayable state
- in-flight join sets and async task handles — execution machinery only; canonical effects must be reflected via `ControlEvent`s or `EffectEvent`s
- transient submit-ack wait state in `executor_submit_inflight` — permitted only as a bounded reconciliation window until the corresponding canonical submit/completion event lands or the lane is requeued
- `deferred_completions` queue — permitted only as a short-lived transport buffer while a lane already has prompt work in flight; deferred items must eventually drain back through canonical completion handling
- `submitted_turns` runtime payload details not mirrored in `submitted_turn_ids` — transport handles and non-serializable tab manager state only; lane/tab ownership and turn identity must still be represented canonically

**`SystemState` key fields** (all serialized to checkpoint):
- `phase`, `phase_lane`, `scheduled_phase` — orchestration phase tracking
- `planner_pending` — planner wake/pending flag (`diagnostics_pending` may appear in legacy checkpoints/logs but is inactive in two-role runtime)
- `lanes: HashMap<usize, LaneState>` — per-lane `{pending, in_progress_by, latest_verifier_result, plan_text}` (`latest_verifier_result` is retained as a legacy-compatible summary field)
- `verifier_summary: Vec<String>` — retained for checkpoint/tlog compatibility; planner is the active verification authority in two-role runtime
- `lane_active_tab`, `tab_id_to_lane`, `lane_steps_used`, `lane_next_submit_at_ms`, `lane_submit_in_flight`, `lane_prompt_in_flight` — executor dispatch bookkeeping
- `submitted_turn_ids: HashMap<String, SubmittedTurnRecord>` — serializable record of in-flight executor turns

**`apply_control_event`** (`src/system_state.rs`) is a pure function with no side effects. It is the only function permitted to construct `s_{t+1}` from `s_t`. All runtime code calls it exclusively through `CanonicalWriter::apply`.

**State validation:** `validate_system_state(...)` runs at canonical-writer construction, after every `apply(...)`, and before checkpoint restore. Validation covers lane bookkeeping completeness plus consistency between `lane_active_tab`, `tab_id_to_lane`, and `submitted_turn_ids`.

**Replay:** `Tlog::replay(...)` reads `AgentStateDir/tlog.ndjson`, applies only `ControlEvent`s through `replay_event_log(...)`, ignores `EffectEvent`s for state advancement, and must reconstruct the same final `SystemState` as live execution.

**Proof obligation:** Canonical-state correctness is established by the combination of:
- transition legality checks before commit
- post-transition invariant validation after commit
- replay equivalence from `AgentStateDir/tlog.ndjson` to final `SystemState`

After the canonical writer boundary is in place, the next engineering phase is **loophole closure**, not feature expansion. Work in this phase must audit every runtime path that can influence control flow or externally visible behavior and either:
- prove the path is already represented by a `ControlEvent` or validated `EffectEvent`, or
- add the missing canonical event, transition-policy rule, invariant, and test

Required loophole classes:
- implicit runtime recovery paths that still influence behavior without a canonical event
- effectful operations whose observable outcome matters but are only logged loosely or not at all
- resume/checkpoint paths where `RuntimeState` can disagree with canonical state for longer than a bounded reconciliation window
- generic `ControlEvent` variants that encode two logically distinct transitions and should be split
- prompt/orchestrator decisions that depend on runtime-only facts not represented canonically

Current loophole-closure status:
- several high-signal planner scheduling ambiguities have been split into explicit canonical queue events
- some branches are now detectable but are not all replaced by dedicated canonical `ControlEvent`s yet
- blocker -> invariant -> gate coverage for ambiguity/effectful loophole classes is still being completed
- full orchestration-loop integration tests for these loophole classes still do not exist yet

At the end of any loophole-closure pass, the system must list the remaining runtime-only behaviors that are intentionally ephemeral and explain why they are permitted to stay outside canonical replay.

### 1.2 Role State (per agent)
- `prompt_kind: PromptKind`
- `step: u64` (monotonic, starts at 1 per role cycle)
- `last_action: Action?`
- `last_result: ActionResult?`
- `lane_plan_path: string?` (Executors only)

### 1.3 Canonical Files
Canonical file paths are absolute under `Workspace` (see `src/constants.rs:3-11`, `42-48`):
- `Spec`: `SPEC.md`
- `Pipeline`: `CANONICAL_PIPELINE.md` — canonical operating contract for observe → eval → plan → execute → verify → regenerate projections → append tlog → learn → gated commit.
- `Objectives`: `agent_state/OBJECTIVES.json` (runtime path) and `PLANS/OBJECTIVES.md` (companion markdown source when present)
- `Invariants`: `INVARIANTS.json` (runtime path) and `INVARIANT.md` (companion markdown source when present)
- `MasterPlan`: `PLAN.json`
- `LanePlan`: `PLANS/<instance>/executor-<id>.json` (preferred) or legacy `PLANS/executor-<id>.md` (see `src/tools.rs:49-63`)
- `Violations`: `VIOLATIONS.json`
- `Diagnostics projection`: runtime-configured instance-scoped path `PLANS/<instance>/diagnostics-<instance>.json`; legacy `DIAGNOSTICS.json` is still accepted for migration/read compatibility
- `Tlog`: `AgentStateDir/tlog.ndjson` — total-ordered NDJSON event log written by `CanonicalWriter`; one JSON record per line with fields `seq`, `ts_ms`, `event`. Authoritative replay source for all state transitions.

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
| `--orchestrate` | Run the two-role orchestration loop (planner → executors → planner). |
| `--planner`     | Run only the planner role (single role mode).                                                      |
| `--verifier`    | Legacy compatibility flag (inactive in runtime two-role mode).                                     |
| `--diagnostics` | Legacy compatibility flag (inactive in runtime two-role mode).                                      |

### 2.2 Options
| Option               | Default                       | Description                                                                                                                                |
|----------------------+-------------------------------+--------------------------------------------------------------------------------------------------------------------------------------------|
| `--workspace <path>` | build-time source root        | **Absolute path** to the target project workspace. All agent file operations resolve relative to this path. Must exist and be a directory. |
| `--state-dir <path>` | `/workspace/ai_sandbox/canon-mini-agent/agent_state` | **Absolute path** to canon-mini-agent's own runtime state directory for checkpoints, wake flags, and inbound messages. |
| `--start <role>`     | `executor`                    | Start role for orchestration: `executor` or `planner`. Legacy values are normalized to two-role flow.                                       |
| `--role <role>`      | none                          | Single-role selector: `executor` or `planner`. Legacy role values are rejected or remapped by runtime policy.                               |
| `--instance <id>`    | `default`                     | Instance identifier used to namespace state subdirectories and runtime artifacts.                                                            |
| `--port <port>`      | auto                          | WebSocket port for Chrome extension. Auto-selects from candidates if not specified.                                                        |

### 2.3 Workspace Validation
- `--workspace` value must be an absolute path (starts with `/`). Non-absolute paths are rejected at startup with a fatal error.
- If omitted, `run()` sets the workspace to `env!("CARGO_MANIFEST_DIR")`, which is the canon-mini-agent source root for the current binary.
- `--state-dir` value must also be an absolute path when provided; non-absolute values are rejected at startup with a fatal error.
- The runtime value is stored in a process-global `OnceLock<String>` and never changes after startup.

## 3. Typed Interfaces (Actions)

Action shapes, required fields, and basic field constraints are defined by the ToolAction JSON schema (schemars) in `src/tool_schema.rs`. This section does not duplicate those schema details; it only documents runtime semantics and protocol rules not captured by the schema itself.

### 3.1 Runtime Semantics (Non-Obvious Behaviors)
- `cargo_test` maps to `cargo test -p <crate> <test> -- --exact --nocapture`.
- `cargo_fmt` maps to `cargo fmt --check` by default; set `fix:true` to run `cargo fmt` (may modify files).
- `cargo_clippy` maps to `cargo clippy -- -D warnings` (or `cargo clippy -p <crate> -- -D warnings` when `crate` is provided).
- `rustc_hir` reads `state/rustc/<crate>/graph.json` (canon-rustc-v2 artifact). If `symbol` is provided, it returns the focused source body for that symbol via graph-backed def spans; otherwise it returns semantic triples in `(from, relation, to)` form. Falls back to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>` if the graph is missing.
- `rustc_mir` reads `state/rustc/<crate>/graph.json` (canon-rustc-v2 artifact). If `symbol` is provided, it returns a focused per-symbol MIR complexity summary (fingerprint/blocks/stmts, rank); otherwise it lists MIR metadata entries. Falls back to `cargo rustc -p <crate> -- -Zunpretty=<mode> <extra>` if the graph is missing.
- `semantic_map` reads `state/rustc/<crate>/graph.json` and returns one semantic triple per line as `(from, relation, to)`. `filter` keeps triples whose source or target matches the provided symbol-path prefix. `expand_bodies` is accepted for compatibility but ignored.
- `symbol_path` computes the BFS shortest path across all semantic relations in `state/rustc/<crate>/graph.json` and labels each hop with its relation.
- `graph_call` / `graph_cfg` output CSVs plus `callgraph.symbol.txt` / `cfg.symbol.txt` with symbol→symbol edges.
- `graph_dataflow` / `graph_reachability` output JSON reports under metrics/analysis directories.
- `rename_symbol` performs a rust-analyzer-syntax-backed Rust identifier rename at the exact `path` + 1-based `line`/`column` token location. Current implementation is file-scoped (`.rs` files only).
- `apply_patch` runs `cargo check -p <inferred_crate>` after a successful patch; if check passes it then runs `cargo test -p <inferred_crate> -q` and returns the cargo test totals summary (the `test result:` lines).
- `canon-mini-supervisor` writes `state/reports/complexity/latest.json` on each spawn/restart using a cheap proxy metric from `state/rustc/<crate>/graph.json` (`complexity_proxy = mir_blocks`).
- After a successful supervisor build, `canon-mini-supervisor` also exports semantic triples to `state/reports/semantic_map/<crate>.jsonl`, one JSON object per line with keys `from`, `relation`, and `to`.

### 3.2 `message` (Inter-Agent Handoff Protocol)
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
| Executor    | Planner  | handoff      | complete / blocked | `summary`, `evidence`                       |
| Planner     | Executor | plan         | ready / blocked    | `summary`, `tasks` / `blockers`             |

**Routing guarantee (added 2026-04-07):** When a role emits a `message` action, the system writes `last_message_to_<to>.json` and `wakeup_<to>.flag` to `AgentStateDir` and sets `planner_pending = true` (for planner-targeted messages). This ensures the target role wakes on the next orchestration cycle regardless of whether the action was emitted in the inline or deferred completion path.
Legacy note: historical solo artifacts may appear in old logs/checkpoints; active runtime wakeup routing only schedules planner and executor.

## 4. Invariants (Must Always Hold)

### 4.1 Scope Invariants

**Normal mode** (workspace ≠ orchestrator source):
- **Executor** may not patch `SPEC.md`, `PLAN.json`, `INVARIANTS.json`, `VIOLATIONS.json`, `OBJECTIVES.json`, any lane plan, diagnostics files, `src/`, or `tests/`.
- **Planner** owns planning/verification/diagnostics control and may patch only lane plans plus `agent_state/OBJECTIVES.json`; planner must use the `plan` action for `PLAN.json` edits.
- **Executor** remains restricted to task-scoped implementation changes and cannot mutate canonical authority JSON except `plan set_task_status -> done` for its active task.

**Self-modification mode** (workspace == orchestrator source, see §9):
- **Executor** may additionally patch `SPEC.md`, `src/`, and `tests/`.
- All other role restrictions are unchanged.

Enforcement: `src/tools.rs::patch_scope_error()` (see `src/tools.rs:363-477`). Changes to that function require explicit planner approval (I13).

Additional clarification (from implementation):
- Executor blocks `SPEC.md` outside self-mod mode, blocks `src/` and `tests/` outside self-mod mode, and blocks all other non-authorized files in every mode via the `touches_other` guard (`src/tools.rs:379-388`, `392-409`).
- Diagnostics projection file path is dynamically resolved (`diagnostics_file()`), and both configured and legacy `DIAGNOSTICS.json` are accepted (`src/tools.rs:369-377`).
- Lane plan detection includes both instance-scoped and legacy formats (`src/tools.rs:49-63`).
- Planner/executor scope guards reject direct `PLAN.json` patching except executor `plan set_task_status -> done`; other `PLAN.json` updates are routed through planner `plan` actions instead of `apply_patch`.

### 4.2 Action Validity Invariants
- Each action must satisfy its typed schema (Section 3).
- Missing required fields or invalid types must be rejected.
- `read_file` line numbers are 1-based.
- Every mutating action (`apply_patch`, `plan`, `objectives`, `issue`, `rename_symbol`) must include a non-empty `question` field (see §0.6). Absence is treated as a missing required field and generates corrective feedback.

### 4.3 Diagnostics Evidence Scan Invariant
- Planner must perform at least one current-cycle evidence scan (`python`, `read_file`, or `run_command`) over workspace-local log/state artifacts before writing issue/violation projections or sending a blocker that depends on diagnostics claims.
- The scan can occur at any point earlier in the same planner cycle; it does **not** need to be the immediately preceding action.

### 4.4 Canonical-File Authority Invariants
- `SPEC.md` is the canonical contract for repair work.
- `PLANS/OBJECTIVES.md` and `INVARIANT.md` are authoritative for objectives and invariants.
- Planner must derive lane plans from canonical files, not from memory or stale copies.
- `SemanticStateSummary` is the single source of truth for routing and control-flow correctness.

### 4.5 Event Ordering Invariants
- Actions are processed in strict step order per role: `step` is monotonic.
- Each `step` produces at most one `ActionResult`.
- A role must not emit a new action without observing the result of the previous action.
- Executor hard cap: after `EXECUTOR_STEP_LIMIT` (20) actions without a `message` handoff, the system forces a handoff prompt.

### 4.6 Logging Invariants
- Every action must be appended to `agent_logs/.../actions.jsonl`.
- Every action result must be appended to `agent_logs/.../action_results.jsonl`.
- Action logs must preserve order of execution.

### 4.7 Build/Test Gate Invariants
- If a completion `message` (status = `complete`) triggers checks:
  - `cargo build --workspace` must pass.
  - `cargo test --workspace` must pass.
  - Otherwise completion is rejected.

### 4.8 Handoff Delivery Invariant
- A `message` action emitted by any role **must** result in the target role receiving the payload before its next cycle begins.
- Wakeup flags (`wakeup_<role>.flag`) and inbound message files (`last_message_to_<role>.json`) are the delivery mechanism.
- The `apply_wake_flags` function in the orchestration loop is the authority that translates these flags into scheduled phase transitions.

### 4.9 Workspace Isolation Invariant
- All file operations by agents are confined to `Workspace`.
- The orchestrator's own state (`AgentStateDir`) is never the target workspace.
- Agents are told the active `Workspace` value in every prompt (header line `WORKSPACE: <path>`).

### 4.12 Python-for-JSON Invariant

All agents **must** use the `python` action when reading or writing any `.json` state file at runtime. Shell tools (`cat`, `jq`, `grep`, `sed`, `awk`) must not be used to inspect or mutate JSON state files because they produce no parse-time error on malformed JSON and silently corrupt objects.

Applies to: `PLAN.json`, `agent_state/OBJECTIVES.json`, `ISSUES.json`, `VIOLATIONS.json`, the active diagnostics report, and any other structured JSON artifact under `AgentStateDir` or `Workspace`.

Exceptions: reading raw bytes for hash/size checks via `run_command` is permitted; the restriction is on semantic read/write operations.

Enforcement: agents are instructed via their prompt rules section. Planner must flag any executor turn (or its own turn) that uses shell tools to read/write JSON.

### 4.10 Planner Completion Plan-Objective Coupling
- Planner may not conclude a cycle as complete while active actionable objectives exist and `PLAN.json` has no incomplete tasks.
- Runtime predicate: reject when `has_actionable_objectives(objectives) == true` and `plan_has_incomplete_tasks(plan) == false` before planner completion/handoff finalization.
- Rejection feedback: `Create/update PLAN tasks for active objectives, or mark objectives deferred/blocked with rationale.`

### 4.11 System Prompt Role Schema
- The orchestrator provides role-specific system instructions via the LLM request `role_schema` field.
- When enabled (`send_system_prompt = true`), `role_schema` is sent on every step of the role loop (not only step 0).

Implementation:
- `build_agent_prompt` includes `role_schema` on `step > 0` when `send_system_prompt` is true: `src/app.rs:1647-1694`.
- Orchestrated role cycles set `send_system_prompt = true` for planner and executor continuations.

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
  -> Planner
```
The orchestrator uses wakeup flags and `planner_pending` to schedule transitions in two-role mode.

### 5.4 Supervisor Restart Rules
- The supervisor watches the `canon-mini-agent` binary for updates.
- The agent writes `agent_state/orchestrator_mode.flag` with `orchestrate` or `single` to describe the running mode.
- In orchestrated mode (`orchestrate`), the supervisor restarts only after a fresh `agent_state/orchestrator_cycle_idle.flag`.
- In single-role mode (`single`) or when the flag is missing, the supervisor restarts immediately on binary updates.
- Before any supervisor-triggered restart, the supervisor runs `cargo build --workspace`; if it succeeds, it runs `git add -A`, `git commit`, and `git push` (commit/push are skipped when there are no staged changes).

### 5.3 Handoff Transition
```
Role emits message{to=Planner}
  -> writer.apply(ControlEvent::InboundMessageQueued { role: "planner", content, signature })
  -> writer.apply(ControlEvent::WakeSignalQueued { role: "planner", signature, ts_ms })
  -> writer.apply(ControlEvent::PlannerPendingSet { pending: true })   [logged to tlog]
  -> physical files last_message_to_planner.json + wakeup_planner.flag written as secondary output
  -> next orchestration loop iteration: apply_wake_flags() reads wake_signals_pending from SystemState
  -> planner prompt injects inbound message via inject_inbound_message() (reads inbound_messages_pending)
```

**PLAN.json is the authoritative handoff medium (Option A):**
A planner→executor `message` with `status: ready` is a *notification* that PLAN.json has been updated. It is not independently actionable. The executor **must** confirm the task is marked `ready` in PLAN.json before starting work. If PLAN.json does not reflect `ready` status, the executor must block and notify the planner rather than proceed on message content alone.

Self-loop guard: an executor `message` action with `to: "executor"` is a routing error. The orchestrator redirects such messages to the planner to prevent the stall-counter disarming loop (see §5.5).

### 5.5 Convergence Guard (Livelock Detection)

The orchestrator tracks content hashes of watched files at the start and end of every cycle: `PLAN.json`, `VIOLATIONS.json`, `agent_state/OBJECTIVES.json`, and `ISSUES.json` (plus any configured diagnostics projection artifact when present).

If `cycle_progress = true` (work was dispatched) but all five hashes are unchanged, the stall counter increments. At `STALL_CYCLE_THRESHOLD` (5) consecutive stalls:
- `agent_state/livelock_report.json` is written with timestamp, stall count, watched files, and pending flag state at detection.
- `planner_pending` is cleared (legacy `diagnostics_pending` may still be cleared when present in restored checkpoint state).
- The stall counter resets and the orchestrator enters the normal idle path.
- Resuming requires a manual `wakeup_*.flag` write, a canonical `WakeSignalQueued` event, or process restart.

The stall counter is **not** incremented when executor turns are in flight (`rt.submitted_turns`, `rt.executor_submit_inflight`, or any `writer.state().lane_submit_in_flight` value non-empty). In-flight executor work legitimately produces no file change until the browser tab returns a result; counting those cycles as stalls would be a false positive.

Implementation: `cycle_state_hash`, `write_livelock_report` in `src/app.rs`; constant `STALL_CYCLE_THRESHOLD = 5`. Livelock detection reads inflight state from `RuntimeState` (`rt`) and serializable state from `writer.state()`.

**Diagnostics report schema note:** The canonical `DiagnosticsReport` shape (defined in `src/reports.rs`) contains `status`, `inputs_scanned`, `ranked_failures`, and `planner_handoff`. The runtime reconciliation function (`render_diagnostics_report_from_issues` in `src/prompt_inputs.rs`) uses a typed round-trip through `DiagnosticsReport` so no unrecognised fields can be introduced — extra fields cannot survive re-serialisation of the struct.

### 5.6 Schema-Guarded File Writes

After every successful `apply_patch` that targets a schema-guarded JSON state file, the orchestrator validates the resulting file content against a compiled `JSONSchema` (generated via `schemars::schema_for!` with `additionalProperties: false`). If validation fails, the file is reverted to its pre-patch content and the error is returned as the action result, closing the feedback loop to the LLM in the same turn.

Schema-guarded files and their canonical types (`src/reports.rs`):

| File                      | Type                | Canonical fields                                                 |
|---------------------------+---------------------+------------------------------------------------------------------|
| Active diagnostics report | `DiagnosticsReport` | `status`, `inputs_scanned`, `ranked_failures`, `planner_handoff` |
| `VIOLATIONS.json`         | `ViolationsReport`  | `status`, `summary`, `violations`                                |

Implementation: `validate_state_file_schema` in `src/tools.rs`, called from `handle_apply_patch_action` after patch application and the ranked-failures semantic check. Each schema is compiled once via `OnceLock<JSONSchema>`.

Rejection message format:
```
apply_patch rejected: <TypeName> schema violation
<jsonschema error lines>
Canonical fields: ...  No additional fields are permitted. Remove any extra fields and retry.
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
- Planner prompt instruction: derive `PLAN.json` tasks from `agent_state/OBJECTIVES.json` (objectives are the source of plan items; do not introduce plan tasks unrelated to objectives without explicitly updating objectives first).

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
- Execute top 1–10 tasks with `status=ready` (not `todo` — tasks must be explicitly moved to `ready` by the planner before the executor may start them).
- Respect dependencies: `∀ edge(from->to): status(from)=done before status(to)=ready`.
- No reordering beyond dependency graph.
- After completing work, emit exactly one `message` action to handoff.
- **Task scoping:** when the executor starts a cycle it is scoped to the `task_id` delivered in the planner handoff. All actions in that cycle must carry the matching `task_id` and `objective_id` in their provenance fields. The executor must not self-select other tasks from PLAN.json outside the current ready window.

### 7.5 Deterministic Guarantees
- Same inputs → same task graph.
- No hidden tasks.
- No implicit dependencies.

### 7.6 PLAN.json Authoritative Rule (Option A)

**PLAN.json is the single source of truth for executor task selection.**

| Source                                          | Role                                                                   |
|-------------------------------------------------+------------------------------------------------------------------------|
| `PLAN.json` task `status=ready`                 | Authoritative — executor may only execute tasks listed here as `ready` |
| Planner→Executor `message` with `status: ready` | Notification only — must reflect PLAN.json state, not override it      |

Rules:
1. The executor may only pick up a task when it is marked `"status": "ready"` in PLAN.json.
2. A planner handoff `message` is a signal that PLAN.json has been updated; the executor must verify PLAN.json state before acting.
3. Planner judges executor progress against PLAN.json, not against message payloads.
4. No agent may declare a task complete in a message without also updating `PLAN.json` task status to `"done"`.

This eliminates the ambiguity between message-routing state and plan state that caused false blockers, unnecessary plan mutations, and stall-counter livelocks.

### 7.7 Objective → Plan → Task Hierarchy

The system uses a four-level authority chain:

```
src/repair_plans.rs  (RepairPlan registry — durable goal/action/verify definitions)
        ↓
agent_state/OBJECTIVES.json  (objective_id + repair_plan_ids binding)
        ↓
    PLAN.json          (plan tasks — derived from objectives and repair plans)
        ↓
  task_id (per task)   (executor work unit — scoped from plan)
```

Rules:
- The repair plan registry (`src/repair_plans.rs`) is the authority for *what needs to be fixed* — goal, action, machine-checkable closure condition. PLAN.json tasks are execution handles, not the durable specification.
- Every PLAN.json task should trace to either an active `RepairPlan` id or an `objective_id` in `agent_state/OBJECTIVES.json`.
- Every executor action must carry both `objective_id` and `task_id` as provenance fields.
- The planner must not create plan tasks unrelated to an active objective or repair plan without first adding the objective.
- When the executor starts a cycle it receives a `task_id` from the planner handoff. That `task_id` is the scope boundary for all actions in the cycle.

This chain ensures full traceability: any action can be traced back through task → plan → objective → repair plan goal.

### 7.8 Repair Plan → Task Binding Protocol

```text
T = R_p · V         (task usefulness = repair plan quality × verify determinism)
O = f(success_criteria, repair_plan_ids)
System = Σ (RepairPlan → Task → Verify)
G = max(R_p, T, O, V)
```

**Active repair plan:** a `RepairPlan` returned by `build_all_active_plans` for the current eval snapshot. Plans are active when their eval metric score is below target, when an invariant is `promoted`, or when a blocker class has no enforced invariant.

**∀plan → ∃task invariant:** every active repair plan must have at least one open task in PLAN.json. Detected by `plan_preflight::plans_without_open_tasks(workspace)`; each gap records a `PlanPreflightFailed` blocker that re-enters the eval feedback loop.

**Stable plan id format:** `"eval_metric:{metric_name}"` | `"invariant:{INV-xxx}"` | `"blocker_class:{error_class_key}"`. Does not encode score values — stable across eval cycles so tasks and objectives can reference them durably.

**Machine-checkable closure:** every `RepairPlan` carries `machine_verify: VerifySpec`. `eval_driver::run()` evaluates it after each eval cycle:
- `passed=true` → `PlanVerifyRecorded(passed=true)` into tlog → planner reads and closes the task.
- `passed=false` for ≥ 3 consecutive cycles → `VerificationFailed` blocker appended → escalation loop.

**Objective binding:** an objective's `repair_plan_ids` field lists the plan ids whose `machine_verify` must all pass for the objective to be considered done. When all pass, `eval_driver` emits `PlanVerifyRecorded("objective:{id}", passed=true)` and the planner marks the objective done. Objectives with empty `repair_plan_ids` and empty `success_criteria` are non-functional and must be fixed by the planner.

**Task auto-close:** the planner reads `PlanVerifyRecorded(passed=true)` events from tlog on its next cycle and closes corresponding tasks using the `plan` action. `eval_driver` does not directly mutate `PLAN.json` — task management is planner authority.

## 8. Non-Goals
- No network access from agents.
- No GUI or interactive I/O from agents.
- No mutation outside `Workspace` (except to `/tmp` for scratch files).
- Workspace path is not determined by agents — it is set by the operator at launch.

## 9. Self-Modification Mode

Self-modification mode is active when `Workspace` equals the canon-mini-agent source root (i.e., the parent of `AgentStateDir`). Detection: `is_self_modification_mode()` in `src/constants.rs`.

### 9.1 Purpose
Allows canon-mini-agent to act as its own target workspace — reading source files, patching `SPEC.md`, and improving `src/` code under planner authority with in-cycle self-verification.

### 9.2 Relaxed Executor Scope
In self-modification mode only:
- Executor **may** patch `SPEC.md` directly.
- Executor patching of `src/` and `tests/` is allowed only in self-modification mode; normal mode blocks both paths.
- All other scope restrictions remain in force (no patching `PLAN.json`, `INVARIANTS.json`, `VIOLATIONS.json`, lane plans, or diagnostics).

### 9.3 Safety Requirements (Invariants I11–I14)

**I11 — Build gate always-on:** After any `src/` or `SPEC.md` patch, executor must run `cargo build --workspace` before emitting a `message` handoff. A broken build is a blocker; the executor must fix or revert the patch in the same turn. This applies regardless of `check_on_done` setting.

**I12 — SPEC.md evidence requirement:** Every SPEC.md edit must cite the source file and approximate line range that implements the claim. Executor must read the relevant `src/` file before writing any SPEC.md claim about it. Planner must independently verify every cited location.

**I13 — No permission escalation:** Executor must not patch `src/tools.rs::patch_scope_error` or any other scope-guard logic in a way that expands role permissions beyond SPEC.md §4. Any such patch requires explicit planner approval and evidence-backed SPEC.md §4 justification.

**I14 — Checkpoint and tlog compatibility:** Changes to `OrchestratorCheckpoint` fields must use `#[serde(default)]` for additions. Removing or renaming fields requires a version bump or checkpoint discard on load. The `workspace` field must always be populated on save and validated on load. Checkpoint save/load must be logged as `EffectEvent::{CheckpointSaved,CheckpointLoaded}` and checkpoint restore must use the dedicated hydration path rather than direct field mutation.

Changes to `SystemState` fields follow the same rule: additions must use `#[serde(default)]`. The tlog (`AgentStateDir/tlog.ndjson`) is append-only and may contain entries from prior `SystemState` schemas; readers must tolerate unknown `ControlEvent` variants gracefully. Changes to `ControlEvent` variants are additive only — existing variants must not be renamed or removed while a tlog from that schema may be in service. `EffectEvent`s may be added for observability, but they must remain non-authoritative for replayed state. Any new canonical state field must either be driven by an existing/new `ControlEvent` or remain strictly runtime-only in `RuntimeState`.

**Audit routing for loophole closure:** A loophole-closure prompt is planner-owned work. The planner should decompose the audit into concrete repair objectives and assign implementation to executor lanes.

### 9.4 Safety Properties (Why It's Safe)
- **No mid-run corruption:** The running orchestrator binary is already loaded into memory. Patching `src/` files does not affect the current process — changes take effect only on the next `cargo build` + process restart.
- **Build gate prevents bad state:** A broken `cargo build` after a patch means the executor must fix or revert before handoff, so the repository never rests in a non-building state.
- **Scope guards protect the plan layer:** Even in self-modification mode, the executor cannot touch `PLAN.json`, `INVARIANTS.json`, or `VIOLATIONS.json`, preserving planner authority.
- **Planner-owned evidence check:** planner must read cited source independently before accepting any SPEC.md change, preventing hallucinated claims from becoming spec.

### 9.5 Prohibited Even in Self-Modification Mode
- Patching `PLAN.json`, `INVARIANTS.json`, `VIOLATIONS.json`, `OBJECTIVES.json`, lane plans, or diagnostics files.
- Removing or weakening scope guards without explicit planner approval (I13).
- Emitting `message{status=complete}` when `cargo build --workspace` fails (I11).
- Writing SPEC.md claims without reading and citing the corresponding source (I12).

## 10. Eval and Judgment System

The eval and judgment system converts raw tlog/artifact state into a scored signal that drives planner decisions and closes repair loops automatically.

### 10.1 Pipeline

```text
S = project(T)                         tlog → current state snapshot
E = score(S, I, graph, blockers, objectives, deltas)
P = plan(E)                            planner creates tasks from REPAIR_PLANs
X = execute(P)                         executor patches
V = verify(X)                          cargo check/test
G_eval = regenerate(latest.json)       canon-generate-issues --complexity-report-only
V_plan = machine_verify.check(E')      eval_driver checks all active repair plans
  → passed → PlanVerifyRecorded(passed=true) → planner closes task
  → failed×3 → VerificationFailed blocker → eval pressure → new REPAIR_PLAN
T' = append(T, effects(X, V, G_eval, V_plan))
```

### 10.2 EvaluationVector Dimensions

The composite eval score is the geometric mean of 12 dimensions (all clamped to [0.001, 1.0]):

| Dimension | Target | Source | Weak when |
| --- | --- | --- | --- |
| `objective_progress` | 1.0 | `OBJECTIVES.json` | any objective not done |
| `safety` | 1.0 | `VIOLATIONS.json` + semantic error rate | violations active or `fn_error_rate > 0` |
| `task_velocity` | 0.85 | `PLAN.json` | < 85% tasks complete |
| `issue_health` | 0.9 | `ISSUES.json` | repeated open issues |
| `semantic_contract` | 0.5 | `semantic_manifest_proposals.json` | error rate, low coverage, high low-confidence |
| `structural_invariant_coverage` | 1.0 | `graph.json` + `enforced_invariants.json` | graph structural risks uncovered |
| `blocker_class_coverage` | 1.0 | `blockers.json` + `enforced_invariants.json` | error classes with no invariant |
| `canonical_delta_health` | 0.9 | tlog delta signals | prompt truncations, actionable lag |
| `improvement_measurement` | 1.0 | tlog improvement attempts | unmeasured apply_patch improvements |
| `improvement_validation` | 1.0 | tlog | unvalidated improvements (no cargo check) |
| `improvement_effectiveness` | 0.8 | tlog delta_g | regressed improvement attempts |
| `recovery_effectiveness` | 1.0 | `RecoveryOutcomeRecorded` events | failed recovery attempts |

Overall score: `base × (1 − 0.25 × diagnostics_repair_pressure) × (1 − enforcement_penalty)`.
Enforcement penalty: 0.15 per hard gate violation, capped at 0.75.

### 10.3 Blocker Class Coverage

`compute_blocker_class_coverage(blockers, invariant_text)` in `src/evaluation.rs`:

```text
for each distinct error_class key in blockers.json (excluding Unknown):
  covered  if  error_class.as_key() appears anywhere in enforced_invariants.json text
  uncovered otherwise

blocker_class_coverage = covered_classes / distinct_classes  (1.0 if no blockers)
top_uncovered = highest-count uncovered class key
```

The `verification_failed` class enters `blockers.json` when a repair plan's `machine_verify` fails ≥ 3 consecutive times. If no invariant covers `"verification_failed"`, `blocker_class_coverage` drops, generating eval pressure to add that invariant. This closes the `stuck repair plan → eval signal → fix` loop.

### 10.4 Repair Plan Registry

`src/repair_plans.rs` contains:
- `RepairPlan` struct: `kind | id | goal | trigger | policy | action | verify | machine_verify | owner | evidence | priority | score`
- `VerifySpec` enum: `ScoreAbove | ScoreImproves | FieldNotEquals | InvariantResolved | All(Vec<VerifySpec>)`
- Three builders: `build_eval_metric_plans`, `build_invariant_plans`, `build_blocker_class_plans`
- `build_all_active_plans(eval, workspace, max)` — merges all three, sorts by priority then score

Priority tiers (lower = higher urgency):
- 10: promoted invariants (need enforce/collapse decision now)
- 20: uncovered blocker classes (executor patches invariant_discovery.rs)
- 30: blocked eval metric (score < 70% of target)
- 50: weak eval metric (score 70–100% of target)

Plans are rendered as `REPAIR_PLAN` blocks in the EVAL HEADER. Top 3 active plans surface in each planner prompt cycle.

### 10.5 Eval Enforcement

`EvalEnforcement` in `src/evaluation.rs` records hard violations vs. soft warnings:

Hard violations (fail eval gate, penalize overall score):
- `semantic_errors > 0` (any function with hard error)
- `intent_totalization < 100%` (any function without intent class)
- `actionable_lag > 300 000 ms`
- `prompt_truncations > 0`
- `missing_action_results > threshold`
- `unsafe_checkpoint_attempts > 0`
- `unmeasured_improvement_attempts > 0`
- `regressed_improvement_attempts > 0` (when improvement is also unvalidated)

Soft warnings (surfaced in EVAL HEADER, no score penalty):
- `meaningful_intent_coverage < 0.75`
- `low_confidence_rate > 0.25`
- `semantic_contract < 0.50`
- `diagnostics_repair_pressure > 0`

### 10.6 Prompt Surfaces

The EVAL HEADER (rendered by `build_eval_header` in `src/prompt_inputs.rs`) contains:
1. `EVAL score=X weakest=dim(val) objectives=N/M tasks=N/M` — overall state
2. `eval_focus` — directive targeting the weakest dimension
3. Semantic contract breakdown — fn_error_rate, intent_coverage, low_confidence_rate, totalization
4. `eval_gate=pass|fail violations=N warnings=N` — enforcement status
5. `blocker_coverage=X blocker_classes=N top_uncovered=key` — blocker class coverage
6. Lag/payload/improvement/recovery stats
7. Up to 3 `REPAIR_PLAN` blocks for the highest-priority active plans

`build_plan_verify_summary` (also in `prompt_inputs.rs`) surfaces:
- `→ PLAN VERIFIED: {id} — close corresponding task` for recently-passed plans
- `⚠ PLAN ESCALATED: {id} failed N× — VerificationFailed blocker recorded` for escalated plans
- `→ OBJECTIVE VERIFIED: {id} — all repair plans passed, mark done` for auto-verified objectives
- `○ OBJECTIVE {id}: N/M repair plans verified (...)` for partially-verified objectives

### 10.7 Closed Loop Invariant

```text
G = max(eval_actionability, planner_guidance, recovery_mapping, verification_closure)

∀ weak_metric m:
  ∃ RepairPlan(id=eval_metric:m, machine_verify=VerifySpec targeting m.target)
  ∧ ∃ open_task in PLAN.json referencing eval_metric:m
  ∧ eval_driver evaluates machine_verify after each eval cycle
  ∧ passed → task_closed ∧ eval score improves
  ∧ failed×3 → VerificationFailed blocker → blocker_class_coverage pressure → new REPAIR_PLAN
```

The system is closed when no weak metric lacks a repair plan, no repair plan lacks a task, and no task lacks a machine-checkable closure condition.

# Canonical Pipeline

## Purpose

This file is the canonical operating contract for how the system turns current
state, errors, and deltas into safe improvement. Runtime prompts include this
contract so planner, executor, verifier-compatible planner flow, and
diagnostics-compatible planner flow share the same authority model.

The pipeline is judgment-first, tlog-first, eval-directed, recovery-bound, and
learning-promoting.

## Variables

- `T`: canonical tlog event stream
- `S`: current system state projected from `T`
- `D`: observed delta between expected state and actual state
- `F`: failure or error class
- `I`: invariants and structural guards
- `J`: judgment layer that classifies validity, priority, risk, and route
- `E`: eval/scoring layer over `T`, graph, issues, objectives, and deltas
- `P`: planner decision and ready task window
- `X`: executor action path
- `W`: canonical write/append operation
- `V`: verification gates (`cargo check`, `cargo test`, `cargo build` when required)
- `G`: graph and issue projections
- `R`: recovery policy selected for a failed or weak state
- `L`: learned lesson or promoted invariant
- `C`: commit/push checkpoint

## Canonical Equation

```text
S = project(T)
D = observe(actual_state - expected_state)
F = classify_error(D, I, E)
J = judge(S, D, F, I, E)
P = plan(J, E)
X = execute(P)
V = verify(X)
G = regenerate(graph, issues) after apply_patch_ok ∧ cargo_check_ok
W = append(T, effects(X, V, G, F, R))
R = recover(F, J, E) when V fails ∨ E weakens ∨ invariant breach exists
L = learn(F, R) only when it changes invariant/eval/test/prompt behavior
C_allowed = no_rust_change ∨ (cargo_check_ok ∧ cargo_test_ok ∧ cargo_build_ok)
reload_proven = SupervisorRestartRequested ∧ SupervisorChildStarted(binary_path, mtime)
```

One-line: **state comes from tlog; deltas expose errors; judgment chooses the route; eval applies pressure; planner creates ready work; executor patches; gates verify; graph/issues refresh; tlog records effects; recovery corrects failed states; learning promotes repeatable failures into invariant/eval/test/prompt pressure.**

## Required Order

```text
observe current truth
→ project canonical state from tlog
→ detect delta/error against invariants and objectives
→ classify failure/error class
→ judge validity, risk, priority, and route
→ evaluate state through invariants and metrics
→ plan the highest-leverage ready task
→ execute the task with bounded patch scope
→ run required verification gates
→ regenerate graph/issues only after a valid patch/check path
→ write canonical effects to tlog
→ recover when verification, eval, or invariants fail
→ promote repeatable failures into lessons/invariants/tests/prompts
→ commit only when C_allowed is true
```

## Authority Order

```text
LAW → SPEC → INVARIANT → OBJECTIVE → PLAN → EXECUTION → WRITE → EVAL → RECOVERY → LEARNING
```

Rules:

- `T` is the canonical source of runtime truth.
- `S` must be projected from `T`, not inferred from stale prose.
- `W` must happen before eval/recovery/learning become canonical authority.
- `P` is invalid when it emits a handoff without ready executable work.
- `X` is invalid when it mutates outside the assigned ready task scope.
- `R` is mandatory when `F` is classified and no successful verification path exists.
- `L` is valid only if future behavior is measurably redirected or blocked.

## Role Contract

- Planner owns objectives, task ordering, invariant interpretation, error-class routing, and ready windows.
- Executor owns bounded source changes, command execution, verification, and evidence capture for the assigned ready task.
- Verifier/diagnostics-compatible flows are planner-owned judgments over evidence, not separate authorities that bypass the pipeline.
- Recovery owns deterministic repair selection for classified failure states.
- Learning owns promotion of repeatable failures into invariants, eval metrics, regression tests, or prompt pressure.
- No role may treat a message handoff as more authoritative than canonical files and tlog-derived state.

## Error Contract

```text
D ≠ 0 → F = classify_error(D, I, E)
F ∧ no_ready_repair → block_or_replan
F ∧ repair_available → execute_recovery_path
F ∧ repeated → promote_to_lesson_or_invariant
```

An error is useful only when it becomes a routed control signal. Unclassified
errors are noise; classified errors create recovery pressure.

## Recovery Contract

```text
failure_event → error_class → recovery_policy → ready_repair_task → verification_gate → tlog_effect
```

Recovery must be deterministic enough to choose the next action without prose
interpretation. If recovery cannot choose a repair path, the planner must create
one before any handoff is emitted.

## Learning Contract

```text
failure → tlog_event → error_class → invariant/eval signal → regression test → prompt pressure
```

A failure is not learned if it is only written as prose. It is learned only when
future execution is measurably redirected, blocked, scored, or repaired.

## Minimal Good Loop

```text
error → judgment → plan → execute → verify → write → eval → recover → learn → stronger_judgment
```

```text
G = max(judgment, eval, recovery, learning, canonical_write)
```

One-line: **good increases when judgment, eval, recovery, learning, and canonical writes compound into deterministic future behavior.**

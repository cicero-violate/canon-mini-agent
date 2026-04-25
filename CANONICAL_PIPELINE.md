# Canonical Pipeline

## Purpose

This file is the canonical operating contract for how the system turns current
state into safe improvement. Runtime prompts include this contract so planner,
executor, verifier-compatible planner flow, and diagnostics-compatible planner
flow share the same model.

## Variables

- `T`: canonical tlog event stream
- `S`: current system state projected from `T`
- `I`: invariants and structural guards
- `E`: eval/scoring layer over `T`, graph, issues, objectives, and deltas
- `P`: planner decision and ready task window
- `X`: executor action path
- `V`: verification gates (`cargo check`, `cargo test`, `cargo build` when required)
- `G`: graph and issue projections
- `L`: learned lesson or promoted invariant
- `C`: commit/push checkpoint

## Equations

```text
S = project(T)
E = score(S, I, graph, issues, objectives, deltas)
P = plan(E)
X = execute(P)
V = verify(X)
G = regenerate(graph, issues) after apply_patch_ok ∧ cargo_check_ok
T' = append(T, effects(X, V, G))
L = learn(failure_event) only when it changes invariant/eval/test/prompt behavior
C_allowed = no_rust_change ∨ (cargo_check_ok ∧ cargo_test_ok ∧ cargo_build_ok)
```

One-line: **state comes from tlog; eval chooses pressure; planner creates ready work; executor patches; gates verify; graph/issues refresh; tlog records effects; learning becomes invariant/eval/test/prompt pressure.**

## Required Order

```text
observe current truth
→ evaluate state through invariants and metrics
→ plan the highest-leverage ready task
→ execute the task with bounded patch scope
→ run required verification gates
→ regenerate graph/issues only after a valid patch/check path
→ append canonical tlog effects
→ promote failures into lessons/invariants/tests when repeatable
→ commit only when commit_allowed is true
```

## Role Contract

- Planner owns objectives, task ordering, invariant interpretation, and ready windows.
- Executor owns bounded source changes, command execution, and evidence capture for the assigned ready task.
- Verifier/diagnostics-compatible flows are planner-owned judgments over evidence, not separate authorities that bypass the pipeline.
- No role may treat a message handoff as more authoritative than canonical files and tlog-derived state.

## Learning Contract

```text
failure → tlog_event → invariant/eval signal → regression test → prompt pressure
```

A failure is not learned if it is only written as prose. It is learned only when
future execution is measurably redirected or blocked.
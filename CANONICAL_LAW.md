# Canonical Law

canon-mini-agent is an event-driven LLM orchestrator. All correctness decisions are governed by the invariants in `INVARIANTS.json`.

## Source of Truth

- `INVARIANTS.json` is the authoritative source for all correctness, scope, and control-flow rules.
- `SPEC.md` is the authoritative description of runtime behavior; it must stay consistent with the source code in `src/`.
- `PLAN.json` is the master plan; task status in `PLAN.json` is the authoritative record of work state.

## Orchestration Discipline

- Handoff delivery is non-negotiable: every `message` action must result in the target role being woken (I2).
- Scope guards are non-negotiable: no role may patch files outside its allowed set (I4).
- The workspace path is frozen at process startup and must never change mid-run (I9).
- Inbound messages are consumed exactly once; no role receives the same message twice (I10).

## State Space

- The state-space functions in `state_space.rs` are pure: identical inputs must produce identical outputs.
- Decision routing must be derived from checkpoint state and wake flags, never from ad-hoc counters.
- Any routing branch not covered by a test in `state_space_tests.rs` is an unverified assumption.

## Build Gate

- `cargo build --workspace` and `cargo test --workspace` must both pass before any completion message is accepted.
- A failing build or test suite is always a blocker; do not mark work done while either fails.

# Canonical Law

## Purpose

canon-mini-agent is a prototype for a **self-building, self-directing, self-learning autonomous AI system**. The operator's goal is to demonstrate that an LLM-driven orchestrator can:

1. **Self-build** — read, critique, and improve its own source code (`src/`), specification (`SPEC.md`), and test suite (`tests/`) without human intervention, using the executor role in self-modification mode.
2. **Self-direct** — plan its own next actions via `predicted_next_actions` in every response, forming an internal decision tree that drives the next turn without waiting for external instruction.
3. **Self-learn** — observe outcomes of past actions (from `agent_state/` logs and `VIOLATIONS.json`), update its plan and objectives accordingly, and avoid repeating mistakes across cycles.

Every design decision in this system — the role model, scope guards, handoff protocol, build gate — exists to make autonomous self-improvement safe and auditable. The agent must never lose the ability to build and test itself.

## Authority Model

canon-mini-agent is an event-driven LLM orchestrator. All correctness decisions are governed by this Canonical Law and must be consistent with the declared invariants in `INVARIANTS.json`.

## Source of Truth

Hierarchy (highest to lowest authority):
LAW → SPEC → INVARIANT → OBJECTIVE → PLAN

- `SPEC.md` is the authoritative description of runtime behavior; it must stay consistent with the source code in `src/`.
- `INVARIANTS.json` is the authoritative source for correctness, scope, and control-flow rules.
- `PLANS/OBJECTIVES.json` is the authoritative objective list; it must align with SPEC and invariants.
- `PLAN.json` is the master plan; task status in `PLAN.json` is the authoritative record of work state.

## Self-Direction Protocol

Every agent response MUST include `predicted_next_actions` — an ordered array of the 2–3 most likely follow-on actions. This is not optional commentary; it is the agent's declared decision tree for the current trajectory. The orchestrator uses it to:

- Detect when the agent is looping (same predicted actions repeated across turns without progress).
- Surface the agent's internal plan to the verifier without requiring a separate planning phase.
- Allow the agent to self-correct by comparing its predictions from turn N against the actual result it received at turn N+1.

If the result at turn N+1 contradicts the prediction, the agent must re-reason from evidence before acting, not blindly follow the prior plan.

## Orchestration Discipline

- Handoff delivery is non-negotiable: every `message` action must result in the target role being woken (I2).
- Scope guards are non-negotiable: no role may patch files outside its allowed set (I4).
- The workspace path is frozen at process startup and must never change mid-run (I9).
- Inbound messages are consumed exactly once; no role receives the same message twice (I10).

## State Space

- The state-space functions in `state_space.rs` are pure: identical inputs must produce identical outputs.
- Decision routing must be derived from checkpoint state and wake flags, never from ad-hoc counters.
- Any routing branch not covered by a test in `state_space_tests.rs` is an unverified assumption.

## Self-Learning Loop

The agent learns across orchestration cycles by:

1. Reading `agent_state/` logs and `VIOLATIONS.json` at the start of each planner cycle to identify patterns in past failures.
2. Updating `PLAN.json` task statuses and steps to encode lessons learned — a task that was blocked for the same reason twice must have a concrete `required_action` in its `steps`.
3. Revising `SPEC.md` when runtime behavior diverges from the specification, so future cycles start with an accurate model of the system.
4. Never marking a task `done` that has been marked `done` and re-opened — the re-open event is a signal to add a regression test.

## Objective Evolution

At the end of every cycle (before emitting a completion message), the agent MUST review `PLANS/OBJECTIVES.json` and:

- **Add** new objectives for capabilities, gaps, or invariants discovered this cycle that are not yet captured.
- **Update** the status field of existing objectives whose state changed (e.g., `active` → `done`, or add discovered sub-requirements).
- **Never delete** an objective — if a goal is abandoned, mark it with `"status": "deferred"` and record the reason in its description.

New objectives must include: `id` (unique, snake_case), `title`, `category`, `level` (`low | medium | high | critical`), and `description` (with **Status**, **Scope**, **Authority files**, and a checklist of requirements).

This is non-optional. An agent that completes a cycle without reviewing objectives is violating the self-learning protocol.

## Build Gate

- `cargo build --workspace` and `cargo test --workspace` must both pass before any completion message is accepted.
- A failing build or test suite is always a blocker; do not mark work done while either fails.

# canon-mini-agent Invariants

## Scope Invariants
- Executor must never modify planning/spec/diagnostics/violations files.
- Verifier must only modify `PLAN.json` and `VIOLATIONS.md`.
- Diagnostics must only modify the diagnostics report.
- Planner must only modify `PLAN.json` and lane plans.
- No role may modify `/workspace/ai_sandbox/canon-mini-agent` unless explicitly authorized by the operator.

## Action Validity Invariants
- Every action must match a typed schema (see `SPEC.md`).
- Missing required fields or invalid types are hard errors.
- `read_file.line` is 1-based when provided.

## Ordering Invariants
- Per role: actions are processed in strict step order, one action per step.
- A role must observe the previous action result before emitting the next action.

## Logging Invariants
- Every action is logged to `agent_logs/.../actions.jsonl`.
- Every action result is logged to `agent_logs/.../action_results.jsonl`.
- Logs preserve action order.

## Build/Test Gate Invariants
- If `done` triggers build/test checks, `cargo build --workspace` and `cargo test --workspace` must pass or `done` is rejected.

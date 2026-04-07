# State Space Mapping for Complexity Reduction

## Goal
Reduce LOC and eliminate drift by replacing repeated branches with a single, explicit state model that drives behavior everywhere.

## When This Works Best
- The same rules are enforced in multiple places (validation, routing, logging, prompts).
- Behavior depends on combinations of flags (e.g., pending/complete, invalid/valid, blocked/unblocked).
- There are repeated error paths or correction prompts.

## The Recipe (Step-by-Step)
1. **Identify a repeated behavior cluster**
   - Look for duplicated checks, if/else chains, or scattered error handling.
   - Example targets: invalid action handling, cargo test lifecycle, routing decisions.

2. **Define the state space explicitly**
   - Enumerate all observable states and transitions.
   - Use a small enum or tagged struct.
   - Keep it finite and testable.

3. **Centralize transitions**
   - Write one function that maps `(state, input)` -> `(next_state, output)`.
   - Output is a small response object (message, log, action, etc.).

4. **Replace duplicated branches with the state map**
   - Route all old call sites through the same state function.
   - Delete old local logic.

5. **Add a state-space harness**
   - Generate synthetic states and inputs.
   - Verify all expected outputs and error messages.
   - Log or report coverage when needed.

6. **Bake in a “report mode”**
   - Use an env var (e.g., `STATE_SPACE_REPORT=1`) to print coverage hits.
   - This becomes a fast audit for missing combinations.

7. **Iterate until no stray logic remains**
   - The goal is a single truth: the state model.
   - Any divergent branch should be removed or folded into the state function.

## Minimal Patterns
**State enum**
```
enum State {
  Idle,
  Pending,
  Completed,
  Invalid,
}
```

**Transition function**
```
fn step(state: State, input: Input) -> (State, Output) { ... }
```

**Harness**
```
for state in all_states() {
  for input in synthetic_inputs() {
    let (next, out) = step(state, input);
    assert!(coverage_expectations(out));
  }
}
```

## Typical Wins (LOC + Consistency)
- 2–5 scattered error handlers → 1 state-driven error handler.
- Repeated corrections or prompts → 1 canonical correction template.
- Multiple “gate” conditions → 1 transition rule.
- Debugging becomes simpler: inspect the state map, not scattered branches.

## What to Watch Out For
- Over‑engineering: keep state enums small and transitions focused.
- Hidden side effects: ensure outputs are explicit and returned from the state map.
- Partial adoption: if some call sites bypass the state map, drift returns.

## Quick Checklist
- Is every decision derived from the same state function?
- Are all branches represented in the harness?
- Is there a single correction message per invalid state?
- Is there a report mode for coverage auditing?

## Apply This Anywhere
If you see repeated conditional logic, you can usually:
1. Extract the state.
2. Centralize transitions.
3. Delete duplicate logic.
4. Test the space.

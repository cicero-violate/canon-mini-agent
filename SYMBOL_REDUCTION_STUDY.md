• Here are 5 easy, high‑leverage improvements that reduce complexity/symbols, keep behavior, and make agents smarter. I can implement any of these quickly.

  1. Role‑aware invalid‑action examples everywhere
      - Use the actual message envelope (from/to/type/status) to generate examples for all roles.
      - Result: clearer corrections, fewer loops, less repeated logic in prompts.
  2. Single helper for “lane state read”
      - Add DispatchState::lane_state(lane_id) -> LaneSnapshot (pending, in_progress, active_tab, steps).
      - Replace scattered .get(&lane_id) calls.
      - Result: fewer symbols and more readable dispatch logic.
  3. Centralize “plan/diagnostics/invariants file read”
      - Introduce read_text_cached(path, &mut cache) for the handful of high‑traffic reads.
      - Result: cleaner planner cycle code and fewer repeated read patterns.
  4. Data‑driven action schema validation
      - Convert invalid_action_expected_fields + per‑action checks into a static ACTION_SCHEMA: &[ActionSpec].
      - Result: less branching, easier to add new tools, more consistent errors.
  5. Unified “agent prompt envelope” builder
      - One helper that builds the fixed header (TAB_ID, TURN_ID, AGENT_TYPE, step limit) and the action result body.
      - Result: fewer duplicated strings and more consistent prompts across roles.


# Symbol Reduction Study

Reducer = controlled symbol reduction that preserves meaning. The goal is fewer names and fewer repeated representations without losing intent or information.

## Definition
Symbol reduction is safe when it:
- Keeps the same semantic meaning (no ambiguity introduced).
- Preserves local reasoning (the reader can still reconstruct intent).
- Avoids hiding constraints or invariants that matter to correctness.

Symbol reduction is unsafe when it:
- Deletes information that was carrying meaning (a name that encoded intent).
- Collapses distinct concepts into one token and makes later reasoning ambiguous.
- Removes affordances that are needed for correctness checks (validation, invariants, or instrumentation).

## Classes of Symbols

### 1) Structural Symbols (High Meaning)
These encode architecture or constraints. Reducing these is risky.
- Phase names, state machine labels, invariants.
- Explicit roles and routing destinations.
- Event names or lifecycle markers.

Guideline:
- Do not reduce unless you also preserve the meaning in a single explicit location.

### 2) Operational Symbols (Moderate Meaning)
These encode behavior at runtime.
- Timeouts, retry caps, thresholds.
- Tool names and action schemas.

Guideline:
- Reduce by centralizing (single source of truth), not by removing.

### 3) Convenience Symbols (Low Meaning)
These are local names with no extra semantics.
- One-line locals that repeat inputs.
- Temporary binding that only mirrors a literal.

Guideline:
- Safe to reduce when it does not hide a non-trivial transformation.

## Safe Reduction Patterns

### A) Collapse “mirror” locals
When a symbol simply repeats an expression without adding meaning.

Example:
- Before: `let response_timeout_secs = response_timeout_for_role(role); ... Some(response_timeout_secs)`
- After: `Some(response_timeout_for_role(role))`

Safe when:
- The name does not encode intent beyond the expression.
- The expression is short and stable.

### B) Shared helper for repeated semantics
Reduce scattered symbols into one authoritative helper.

Example:
- Replace repeated payload validation logic with `validate_message_action`.

Safe when:
- The helper’s name clearly encodes the meaning.
- All callers become consistent.

### C) Use a struct to unify parallel variables
Reduce multi-symbol bundles into one container.

Example:
- Replace `(summary_text, diff_text, failures)` with `PromptInputs`.

Safe when:
- The struct name preserves meaning.
- The fields remain explicit and typed.

### D) Canonical constants for repeated literals
Replace repeated literal strings or lists with `const` slices.

Example:
- `EXECUTOR_HANDOFF_BULLETS`, `EXECUTION_DISCIPLINE`.

Safe when:
- The constant name preserves meaning and is used consistently.

## Unsafe Reduction Patterns

### A) Over-collapsing distinct roles
Example:
- Merging planner/verifier/diagnostics roles into a single “agent” symbol without retaining role differences.

Risk:
- Role-specific timeouts, routing, or behavior are lost.

### B) Hiding invariants in short names
Example:
- Collapsing “route_tick → loop_observed invariant” into `next_event_ok`.

Risk:
- The invariant is no longer self-documenting or testable.

### C) Eliminating explicit schema constraints
Example:
- Removing explicit action field validation and relying on “best effort” parsing.

Risk:
- Invalid inputs no longer produce diagnostic feedback.

## Practical Heuristics

1) If a symbol encodes a contract, keep it explicit.
2) If a symbol is a mirror of another expression, reduce it.
3) If multiple call sites encode the same meaning, centralize into a helper.
4) If the reader has to decode meaning from context after reduction, do not reduce.
5) Reduction should improve error messages, not worsen them.

## Examples from This Codebase

### Good reductions
- Centralized invalid-action feedback builder with consistent schema diffs.
- Role-aware example action generation to preserve meaning while reducing duplication.
- Replacing repeated map lookups with accessors (e.g., `lane_in_flight`).

### Bad reductions (to avoid)
- Removing role-specific message schemas (planner vs executor vs verifier).
- Collapsing distinct event transitions into a single “state ok” boolean.

## Reducer Checklist

- Does this removal preserve meaning?
- Did I introduce ambiguity?
- Does the error surface become clearer or worse?
- Are invariants still explicit and testable?
- Can a new reader still infer intent without extra context?

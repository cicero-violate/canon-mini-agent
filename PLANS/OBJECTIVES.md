# canon-mini-agent Objectives

Objectives are ordered by priority. Each objective has a measurable completion criterion.
The active workspace for all objectives is set via `--workspace` at launch (see SPEC.md §2).

---

## OBJ-1 — Self-Review and Iteration of SPEC.md

**Status:** ready  
**Scope:** canon-mini-agent self-improvement  
**Authority files:** `SPEC.md`, `INVARIANTS.json`, `PLAN.json`

Canon-mini-agent must read, critique, and iteratively improve its own `SPEC.md` until the specification is:

1. **Complete** — every action type, CLI flag, invariant, state transition, and inter-role protocol is documented with no gaps.
2. **Consistent** — no contradiction between `SPEC.md`, `INVARIANTS.json`, `PLAN.json`, and the source code in `src/`.
3. **Accurate** — all documented behavior matches actual runtime behavior; stale or incorrect claims are removed.
4. **Actionable** — every section gives agents enough information to act correctly without reading source code.

### What this means in practice

- The executor reads `SPEC.md`, then reads the corresponding source files (`src/app.rs`, `src/tools.rs`, `src/prompts.rs`, `src/state_space.rs`, `src/constants.rs`).
- For each section of `SPEC.md`, the executor verifies the claim against the code and notes any gap, inaccuracy, or missing section.
- The executor patches `SPEC.md` directly to correct errors or add missing content.
- The verifier confirms that every change to `SPEC.md` is backed by source code evidence, not assumptions.
- Iteration continues until the verifier finds no further gaps.

### Completion criteria

- [ ] Every CLI flag in `src/app.rs::run()` is documented in SPEC.md §2.
- [ ] Every action kind in `src/tools.rs::execute_action()` is documented in SPEC.md §3 with correct field schemas.
- [ ] Every invariant in `INVARIANTS.json` has a corresponding clause in SPEC.md §4.
- [ ] The handoff delivery flow (§5.3) matches the actual code path in `app.rs` including both inline and deferred executor completion.
- [ ] The workspace resolution rule (§1.4) is accurate for all path-handling code in `src/tools.rs`.
- [ ] `cargo build --workspace` passes after all SPEC.md edits.

---

## OBJ-2 — Exhaustive State-Space Test Coverage

**Status:** ready  
**Scope:** `src/state_space.rs`, `src/state_space_tests.rs`  
**Authority files:** `SPEC.md §4`, `INVARIANTS.json` (I8), `state_space_code_mapping.md`

Every decision function in `state_space.rs` must be covered by exhaustive tests that enumerate all meaningful input combinations and assert invariants on each transition.

### Completion criteria

- [ ] `decide_wake_flags`: all combinations of flag presence × active_blocker value tested.
- [ ] `decide_resume_phase`: all checkpoint_phase values × has_verifier_items tested.
- [ ] `scheduled_phase_resume_done`: all phase × pending × in_progress combinations tested.
- [ ] `executor_step_limit_exceeded`: boundary values (0, limit-1, limit, limit+1) tested.
- [ ] No dead code warnings in `state_space.rs`.
- [ ] All tests pass: `cargo test -p canon-mini-agent`.

---

## OBJ-3 — Correct Role Scope Enforcement

**Status:** active  
**Scope:** `src/tools.rs` scope guard logic  
**Authority files:** `SPEC.md §4.1`, `INVARIANTS.json` (I4)

The patch scope guards must reject every out-of-scope file modification for every role. Guards must be tested via `src/invalid_action_tests.rs`.

### Completion criteria

- [ ] Executor cannot patch SPEC.md, PLAN.json, INVARIANTS.json, VIOLATIONS.json, or any diagnostics file.
- [ ] Verifier can patch only PLAN.json and VIOLATIONS.json.
- [ ] Diagnostics can patch only the active diagnostics report.
- [ ] Planner can patch only PLAN.json and lane plan files.
- [ ] Scope guard tests cover all roles × all forbidden file types.

---

## OBJ-4 — Handoff Delivery Guarantee

**Status:** done  
**Scope:** `src/app.rs` executor completion paths  
**Authority files:** `SPEC.md §5.3`, `INVARIANTS.json` (I2)

A `message` action emitted by an executor must always result in the target role being woken on the next orchestration cycle, regardless of which completion path (inline or deferred) processed the action.

### Completion criteria

- [x] `persist_planner_message()` is called in the deferred executor completion path when `message{to=planner}` is detected.
- [x] `dispatch_state.planner_pending = true` is set after persisting a planner-targeted message.
- [x] Generic wakeup flag + inbound message file written for non-planner targets (verifier, diagnostics).
- [x] `cargo build --workspace` passes.

---

## OBJ-5 — Configurable Target Workspace

**Status:** done  
**Scope:** `src/constants.rs`, `src/app.rs`, `src/tools.rs`, `src/prompts.rs`  
**Authority files:** `SPEC.md §2`

The target workspace must be configurable at launch via `--workspace <absolute-path>` rather than hardcoded. The active value must be injected into every agent prompt.

### Completion criteria

- [x] `--workspace <path>` CLI flag parsed and validated in `app.rs::run()`.
- [x] Non-absolute path rejected with fatal error.
- [x] `workspace()` function returns the active path at runtime.
- [x] All agent prompts include `WORKSPACE: <active-path>` header.
- [x] `cargo build --workspace` passes.

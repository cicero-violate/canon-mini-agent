# Authority Matrix

This file defines the intended authority class for runtime artifacts.

- **canonical**: source-of-truth authority; readers should treat it as authoritative.
- **projection**: derived view rebuilt from canonical state or other canonical inputs.
- **ephemeral**: delivery/cache/wakeup scratch state; safe to recreate or clear.

| Artifact                                           | Class      | Notes                                                                                 |
| ---                                                | ---        | ---                                                                                   |
| `SPEC.md`                                          | canonical  | Human-authored contract for expected system behavior.                                 |
| `INVARIANTS.json`                                  | canonical  | Checked-in contract invariants.                                                       |
| `agent_state/PLAN.json`                            | canonical  | Master work plan managed through the plan tool.                                       |
| `agent_state/OBJECTIVES.json`                      | canonical  | Runtime objective authority.                                                          |
| `agent_state/tlog.ndjson`                          | canonical  | Append-only runtime authority for canonical control/effect history.                   |
| `agent_state/ISSUES.json`                          | projection | Rebuildable issue view from canonical/projected evidence.                             |
| `agent_state/VIOLATIONS.json`                      | projection | Rebuildable verifier/planner-projection view.                                         |
| `agent_state/blockers.json`                        | projection | Rebuildable blocker projection with tlog-backed recovery.                             |
| `agent_state/lessons.json`                         | projection | Synthesized lessons projection backed by snapshot effects.                            |
| `agent_state/enforced_invariants.json`             | projection | Synthesized enforced-invariants projection backed by snapshot effects.                |
| `agent_state/last_message_to_<role>.json`          | ephemeral  | Delivery cache only; no-writer readers must prefer canonical tlog entries.            |
| `agent_state/external_user_message_to_<role>.json` | ephemeral  | Delivery cache only; no-writer readers must prefer canonical tlog entries.            |
| `agent_state/wakeup_<role>.flag`                   | ephemeral  | Legacy wake scratch file (deprecated); canonical wake routing uses `WakeSignalQueued` in tlog/SystemState. |
| `frames/*.jsonl`                                   | ephemeral  | Browser/runtime transport capture; useful for debugging, not authority.               |
| `agent_state/default/actions.jsonl`                | ephemeral  | Action trace/debug log; informative but not control authority.                        |

## Rules

1. Ephemeral artifacts may be deleted during repair or replay without changing canonical truth.
2. Loader authority is tlog/canonical-state first; projection files are cache/materialization only.
3. Runtime mutation of protected projections (`OBJECTIVES`, `ISSUES`, `lessons`, `enforced_invariants`) must go through writer-aware projector functions.
4. On boot/replay, projection files are reconciled from canonical snapshots when missing/stale/divergent.
5. Wake routing authority is canonical control state (`WakeSignalQueued` / `WakeSignalConsumed` via `SystemState.wake_signals_pending`), not physical `wakeup_*.flag` files.

## Gate + Authority Function Map (Code-Derived)

Derived from source/tests only (no `SPEC.md` read).

### Runtime gate functions
- `src/invariants.rs`: `evaluate_invariant_gate(...)` — hard gate predicate check for `route|planner|executor` role proposals.
- `src/invariants.rs`: `default_gates_for_conditions(...)` — maps invariant conditions/error classes to default enforcing gates.
- `src/state_space.rs`: `decide_phase_gates(...)` — computes planner/executor/verifier runnable gates from canonical state. Legacy diagnostics inputs are remapped to planner for replay compatibility.
- `src/state_space.rs`: `allow_named_phase_run(...)`, `block_executor_dispatch(...)`, `allow_diagnostics_run(...)` (compat no-op), `decide_resume_phase(...)`, `decide_post_diagnostics(...)` — gate helpers used by orchestrator transitions.
- `src/app.rs`: `collect_wake_signal_inputs(...)` / `apply_wake_signals(...)` — canonical wake-signal application from `SystemState` (file-flag fallback retired).

### Canonical authority mutation/effect sink
- `src/canonical_writer.rs`: `CanonicalWriter::try_apply/apply(...)` — sole canonical state mutation path.
- `src/canonical_writer.rs`: `try_record_violation/record_violation(...)` — records blocked gate/invariant violations to tlog.
- `src/canonical_writer.rs`: `try_record_effect/record_effect(...)` — canonical effect append path.
- `src/canonical_writer.rs`: `try_record_evolution_advance/record_evolution_advance(...)` — canonical evolution + effect recording.

### Projection-authority write functions
- `src/logging.rs`: `write_projection_with_artifact_effects(...)` — standard projection write path with effect + artifact metadata.
- `src/issues.rs`: `persist_issues_projection_with_writer(...)` — authoritative writer for `agent_state/ISSUES.json`.
- `src/invariants.rs`: `persist_enforced_invariants_projection_with_writer(...)` — writer-aware authoritative writer for `agent_state/enforced_invariants.json`.
- `src/lessons.rs`: `persist_lessons_projection_with_writer(...)` — writer-aware authoritative writer for `agent_state/lessons.json`.
- `src/objectives.rs`: `persist_objectives_projection(...)` — projection materialization for canonical objectives state.
- `src/objectives.rs`: `reconcile_objectives_projection(...)` — startup/replay projection reconciliation from canonical objectives.
- `src/issues.rs`: `reconcile_issues_projection(...)` — projection reconciliation from `ISSUES.json`; legacy full `IssuesFileRecorded` tlog recovery is fallback-only to avoid prompt-path replay lag.
- `src/lessons.rs`: `reconcile_lessons_projection(...)` — startup/replay projection reconciliation from latest `LessonsArtifactRecorded`.
- `src/invariants.rs`: `reconcile_enforced_invariants_projection(...)` — startup/replay projection reconciliation from latest `EnforcedInvariantsRecorded`.
- `src/logging.rs`: `migrate_projection_if_present(...)` — controlled projection migration helper.

### Authoritative read/load functions (tlog first)
- `src/issues.rs`: `load_issues_file(...)` (+ `load_issues_from_tlog(...)`) — resolves operational state from `ISSUES.json`; new tlog writes record lightweight `IssuesProjectionRecorded` receipts instead of cloning the full issues payload.
- `src/invariants.rs`: `load_enforced_invariants_file(...)` (+ `load_invariants_from_tlog(...)`) — resolves authority from latest `EnforcedInvariantsRecorded`, uses file only as compatibility fallback.
- `src/lessons.rs`: `load_lessons_artifact(...)` (+ `load_lessons_from_tlog(...)`) — resolves authority from latest `LessonsArtifactRecorded`, uses file only as compatibility fallback.
- `src/blockers.rs`: `load_blockers(...)` (+ `load_blockers_from_tlog(...)`) — reads blockers projection, falls back to tlog records.
- `src/prompt_inputs.rs`: `read_lessons_or_empty(...)` — prompt-safe lessons loader path (structured parse + fallback behavior).
- `src/prompt_inputs.rs`: `load_planner_inputs(...)`, `load_executor_diff_inputs(...)`, `load_single_role_inputs(...)` — centralized prompt input loaders.

### Objective authority file helpers
- `src/objectives.rs`: `runtime_objectives_path(...)`, `resolve_objectives_path(...)`, `ensure_runtime_objectives_file(...)` — objective authority path resolution/bootstrap.
- `src/objectives.rs`: `load_runtime_objectives_json(...)` — canonical/tlog-first objective JSON loader.
- `src/objectives.rs`: `read_objectives_compact_for_workspace(...)` — compact canonical-first objective read for prompt injection.

### Guardrail test anchoring this policy
- `tests/authority_matrix_guardrail.rs`: 
  - `canonical_projection_artifacts_do_not_use_raw_writes_outside_projection_layer()`
  - `canonical_projection_artifacts_do_not_use_raw_reads_outside_authoritative_loaders()`
  - `authority_matrix_documents_expected_artifact_classes()`
  - `projection_authority_writes_flow_through_projector_modules()`

# Authority Matrix

This file defines the intended authority class for runtime artifacts.

- **canonical**: source-of-truth authority; readers must treat it as authoritative.
- **projection**: derived view rebuilt from canonical state or other canonical inputs.
- **ephemeral**: delivery/cache/wakeup scratch state; safe to recreate or clear.

| Artifact                                           | Class      | Notes                                                                                                                              |
| ---                                                | ---        | ---                                                                                                                                |
| `SPEC.md`                                          | canonical  | Human-authored contract for expected system behavior.                                                                              |
| `INVARIANTS.json`                                  | canonical  | Checked-in contract invariants.                                                                                                    |
| `agent_state/PLAN.json`                            | canonical  | Master work plan managed through the plan tool.                                                                                    |
| `agent_state/OBJECTIVES.json`                      | canonical  | Runtime objective authority. Objectives carry `repair_plan_ids` binding to repair plans whose `machine_verify` drives completion. |
| `agent_state/tlog.ndjson`                          | canonical  | Append-only runtime authority for canonical control/effect history. Source of truth for eval, recovery, and plan verify outcomes.  |
| `agent_state/ISSUES.json`                          | projection | Rebuildable issue view from canonical/projected evidence.                                                                          |
| `agent_state/VIOLATIONS.json`                      | projection | Rebuildable verifier/planner-projection view.                                                                                      |
| `agent_state/blockers.json`                        | projection | Rebuildable blocker projection backed by tlog `BlockerRecorded` events. Feeds `compute_blocker_class_coverage` in eval.            |
| `agent_state/enforced_invariants.json`             | projection | Synthesized enforced-invariants projection backed by snapshot effects. Also cross-referenced by `compute_blocker_class_coverage`.  |
| `agent_state/lessons.json`                         | projection | Synthesized lessons projection backed by snapshot effects.                                                                         |
| `agent_state/reports/complexity/latest.json`       | projection | Materialized eval snapshot written by `canon-generate-issues --complexity-report-only`. EVAL HEADER reads from it when live snapshot is unavailable; eval_driver is the authoritative live source. |
| `state/rustc/canon_mini_agent/graph.json`          | projection | Rebuildable rustc-derived semantic graph; inspect through graph describe/semantic helpers before raw JSON reads.                    |
| `agent_state/semantic_manifest_proposals.json`     | projection | Rebuildable semantic manifest sidecar derived from `graph.json` and docstrings.                                                    |
| `agent_state/safe_patch_candidates.json`           | projection | Rebuildable semantic ranking output derived from `graph.json` and manifest proposals.                                              |
| `agent_state/last_message_to_<role>.json`          | ephemeral  | Delivery cache only; no-writer readers must prefer canonical tlog entries.                                                         |
| `agent_state/external_user_message_to_<role>.json` | ephemeral  | Delivery cache only; no-writer readers must prefer canonical tlog entries.                                                         |
| `agent_state/wakeup_<role>.flag`                   | ephemeral  | Legacy wake scratch file (deprecated); canonical wake routing uses `WakeSignalQueued` in tlog/SystemState.                         |
| `frames/*.jsonl`                                   | ephemeral  | Browser/runtime transport capture; useful for debugging, not authority.                                                             |
| `agent_state/default/actions.jsonl`                | ephemeral  | Action trace/debug log; informative but not control authority.                                                                      |

## Rules

1. Ephemeral artifacts may be deleted during repair or replay without changing canonical truth.
2. Loader authority is tlog/canonical-state first; projection files are cache/materialization only.
3. Runtime mutation of protected projections (`OBJECTIVES`, `ISSUES`, `lessons`, `enforced_invariants`) must go through writer-aware projector functions.
4. On boot/replay, projection files are reconciled from canonical snapshots when missing/stale/divergent.
5. Wake routing authority is canonical control state (`WakeSignalQueued` / `WakeSignalConsumed` via `SystemState.wake_signals_pending`), not physical `wakeup_*.flag` files.
6. Projection-driving graph reads must stay behind semantic loader/helper modules so guardrails do not mix protected artifact path construction with ad hoc raw file reads.
7. **Repair plans are the durable registry for `goal + action + verify`.** `PLAN.json` tasks are execution handles that trace to a repair plan id — they are not independent authorities for what needs to be fixed. `src/repair_plans.rs` is the canonical source for active repair plan definitions.
8. **`∀plan → ∃task`:** every active `RepairPlan` must have a corresponding open task in `PLAN.json`. Detected by `plan_preflight::plans_without_open_tasks`; each gap records a `PlanPreflightFailed` blocker that feeds `blockers.json` → eval pressure → new `REPAIR_PLAN`.
9. **`machine_verify` is the closure condition.** `eval_driver` evaluates `VerifySpec::check()` after every eval cycle. Passing emits `PlanVerifyRecorded(passed=true)` so the planner can close the task. Failing 3× emits a `VerificationFailed` blocker that re-enters `compute_blocker_class_coverage` → eval → escalated repair plan.
10. Agents should use graph-backed describe/navigation surfaces (`semantic_map`, `symbol_window`, `symbol_neighborhood`, `symbol_path`, `execution_path`, `rustc_hir`, `rustc_mir`) to understand `graph.json`; raw `graph.json` parsing is reserved for dedicated analyzers that document the schema fields they consume.

## Graph Description Policy

`state/rustc/canon_mini_agent/graph.json` is large, rebuildable, and schema-rich. It is not a hand-authored authority file.

Use describe/navigation tools first:

- `semantic_map` — summarize graph triples and module-level relationships.
- `symbol_window` — inspect the source/body context for one graph-backed symbol.
- `symbol_neighborhood` — inspect callers/callees and adjacent semantic edges for one symbol.
- `symbol_path` — trace the shortest semantic path between two symbols.
- `execution_path` — trace a unified semantic + CFG path between symbols or CFG blocks.
- `rustc_hir` / `rustc_mir` — inspect graph-backed HIR/MIR summaries without reading raw `graph.json`.

Raw structured parsing of `graph.json` is allowed for dedicated graph analyzers such as invariant-gap detection, semantic ranking, issue projection, and manifest synchronization. Those analyzers must treat `graph.json` as a projection and should load it through the semantic graph authority surface where practical.

## Gate + Authority Function Map (Code-Derived)

### Runtime gate functions
- `src/invariant_discovery.rs`: `evaluate_invariant_gate(...)` — hard gate predicate check for `route|planner|executor` role proposals.
- `src/invariant_discovery.rs`: `default_gates_for_conditions(...)` — maps invariant conditions/error classes to default enforcing gates.
- `src/state_space.rs`: `decide_phase_gates(...)` — computes planner/executor/verifier runnable gates from canonical state.
- `src/state_space.rs`: `allow_named_phase_run(...)`, `block_executor_dispatch(...)`, `decide_resume_phase(...)`, `decide_post_diagnostics(...)` — gate helpers used by orchestrator transitions.
- `src/app.rs`: `collect_wake_signal_inputs(...)` / `apply_wake_signals(...)` — canonical wake-signal application from `SystemState`.
- `src/plan_preflight.rs`: `preflight_ready_tasks(...)` — gates executor symbol references before dispatch; also runs `plans_without_open_tasks()` and records `PlanPreflightFailed` blockers for repair plans with no open task.

### Canonical authority mutation/effect sink
- `src/canonical_writer.rs`: `CanonicalWriter::try_apply/apply(...)` — sole canonical state mutation path.
- `src/canonical_writer.rs`: `try_record_violation/record_violation(...)` — records blocked gate/invariant violations to tlog.
- `src/canonical_writer.rs`: `try_record_effect/record_effect(...)` — canonical effect append path.
- `src/canonical_writer.rs`: `try_record_evolution_advance/record_evolution_advance(...)` — canonical evolution + effect recording.

### Projection-authority write functions
- `src/logging.rs`: `write_projection_with_artifact_effects(...)` — standard projection write path with effect + artifact metadata.
- `src/issues.rs`: `persist_issues_projection_with_writer(...)` — authoritative writer for `agent_state/ISSUES.json`.
- `src/invariant_discovery.rs`: `persist_enforced_invariants_projection_with_writer(...)` — writer-aware authoritative writer for `agent_state/enforced_invariants.json`.
- `src/lessons.rs`: `persist_lessons_projection_with_writer(...)` — writer-aware authoritative writer for `agent_state/lessons.json`.
- `src/objectives.rs`: `persist_objectives_projection(...)` — projection materialization for canonical objectives state.
- `src/objectives.rs`: `reconcile_objectives_projection(...)` — startup/replay projection reconciliation from canonical objectives.
- `src/issues.rs`: `reconcile_issues_projection(...)` — projection reconciliation from `ISSUES.json`.
- `src/lessons.rs`: `reconcile_lessons_projection(...)` — startup/replay projection reconciliation from latest `LessonsArtifactRecorded`.
- `src/invariant_discovery.rs`: `reconcile_enforced_invariants_projection(...)` — startup/replay projection reconciliation from latest `EnforcedInvariantsRecorded`.
- `src/logging.rs`: `migrate_projection_if_present(...)` — controlled projection migration helper.
- `src/blockers.rs`: `append_blocker(...)`, `record_blocker_message_with_writer(...)`, `record_action_failure_with_writer(...)` — append to `blockers.json` projection; records `BlockerRecorded` into tlog.

### Authoritative read/load functions (tlog first)
- `src/issues.rs`: `load_issues_file(...)` / `load_issues_from_tlog(...)` — resolves operational state from `ISSUES.json`.
- `src/invariant_discovery.rs`: `load_enforced_invariants_file(...)` / `load_invariants_from_tlog(...)` — resolves authority from latest `EnforcedInvariantsRecorded`.
- `src/lessons.rs`: `load_lessons_artifact(...)` / `load_lessons_from_tlog(...)` — resolves authority from latest `LessonsArtifactRecorded`.
- `src/blockers.rs`: `load_blockers(...)` / `load_blockers_from_tlog(...)` — reads blockers projection, falls back to tlog records.
- `src/prompt_inputs.rs`: `read_lessons_or_empty(...)`, `load_planner_inputs(...)`, `load_executor_diff_inputs(...)`, `load_single_role_inputs(...)` — centralized prompt input loaders.
- `src/semantic_contract.rs`: `graph_path(...)`, `load_semantic_manifest_metrics(...)`, `run_semantic_sync(...)` — semantic graph/sidecar authority surface.
- `src/evaluation.rs`: `load_objectives_file(workspace)` — loads `OBJECTIVES.json` for eval and eval_driver objective-verify checks.

### Eval and judgment functions
- `src/evaluation.rs`: `evaluate_workspace(workspace)` — loads all canonical/projection inputs and calls `compute_eval`; the sole live eval I/O function.
- `src/evaluation.rs`: `compute_eval(input)` — pure scoring kernel; no I/O.
- `src/evaluation.rs`: `compute_blocker_class_coverage(blockers, invariant_text)` — pure function: groups `blockers.json` by `error_class` key, checks each against `enforced_invariants.json` text, returns `BlockerClassCoverage { score, distinct_classes, covered_classes, uncovered_classes, top_uncovered }`.
- `src/evaluation.rs`: `load_blocker_class_coverage(workspace)` — loads `blockers.json` + `enforced_invariants.json` and calls `compute_blocker_class_coverage`.
- `src/eval_driver.rs`: `run(workspace, writer)` — computes eval snapshot, emits `EvalScoreRecorded` into tlog, then runs the plan verify loop (see below), escalates repeated failures, and checks objective auto-verify.
- `src/eval_driver.rs` (plan verify loop) — after each eval cycle: for every active `RepairPlan`, calls `machine_verify.check(eval_map, invariant_text)`, emits `PlanVerifyRecorded { plan_id, passed, verify_description }` into tlog; if `!passed` and consecutive failures ≥ 3, appends a `VerificationFailed` blocker to `blockers.json`.
- `src/eval_driver.rs` (objective auto-verify) — checks `OBJECTIVES.json` for objectives whose `repair_plan_ids` all passed `machine_verify`; emits `PlanVerifyRecorded("objective:{id}", passed=true)` as a planner hint.

### Repair plan registry
- `src/repair_plans.rs`: `build_all_active_plans(eval, workspace, max)` — merges all three registries (invariant, blocker_class, eval_metric) sorted by priority; the top-N plans are rendered into the EVAL HEADER.
- `src/repair_plans.rs`: `build_eval_metric_plans(eval, max)` — 12 eval dimension plans (one per `EvaluationVector` field), each with `VerifySpec` matching its target threshold.
- `src/repair_plans.rs`: `build_invariant_plans(invariant_text, max)` — one plan per `promoted` invariant in `enforced_invariants.json`; `machine_verify = VerifySpec::InvariantResolved`.
- `src/repair_plans.rs`: `build_blocker_class_plans(blockers_text, invariant_text, max)` — one plan per distinct error class in `blockers.json` not covered by any invariant; `machine_verify = VerifySpec::FieldNotEquals { key: "blocker_top_uncovered" }`.
- `src/repair_plans.rs`: `snapshot_to_eval_map(snapshot)` — converts a live `EvaluationWorkspaceSnapshot` to the JSON map consumed by `build_eval_metric_plans` and `VerifySpec::check`; used by `eval_driver` to avoid re-reading `latest.json`.
- `src/repair_plans.rs`: `count_consecutive_verify_failures(workspace, plan_id)` — tail-scans tlog for consecutive `PlanVerifyRecorded(passed=false)` events; drives escalation threshold.
- `src/repair_plans.rs`: `recent_plan_verify_outcomes(workspace)` — returns latest `(plan_id, passed, consecutive_failures)` for every plan seen in recent tlog; consumed by `build_plan_verify_summary`.

### Plan preflight and gap detection
- `src/plan_preflight.rs`: `preflight_ready_tasks(workspace)` — symbol-reference gate for ready tasks; now also calls `plans_without_open_tasks` and records `PlanPreflightFailed` blockers for each gap.
- `src/plan_preflight.rs`: `plans_without_open_tasks(workspace)` — returns stable plan ids with no open task in `PLAN.json`; implements advisory `∀plan → ∃task` check.
- `src/plan_preflight.rs`: `extract_workspace_symbol_refs(text, crate_names)` — extracts Rust symbol references from task text for validation.

### Objective authority file helpers
- `src/objectives.rs`: `runtime_objectives_path(...)`, `resolve_objectives_path(...)`, `ensure_runtime_objectives_file(...)` — objective authority path resolution/bootstrap.
- `src/objectives.rs`: `load_runtime_objectives_json(...)` — canonical/tlog-first objective JSON loader.
- `src/objectives.rs`: `is_completed(obj)` — checks `status` field for `done|complete|completed`; used by eval_driver and objective auto-verify.
- `src/objectives.rs`: `read_objectives_compact_for_workspace(...)` — compact canonical-first objective read for prompt injection.

### Prompt construction
- `src/prompt_inputs.rs`: `build_eval_header(workspace)` — reads `latest.json` eval section; overlays live semantic manifest metrics; renders EVAL HEADER including `REPAIR_PLAN` blocks for top-N weak dimensions via `build_all_active_plans`.
- `src/prompt_inputs.rs`: `build_plan_verify_summary(workspace)` — reads recent `PlanVerifyRecorded` tlog events; renders `→ PLAN VERIFIED`, `⚠ PLAN ESCALATED`, and `→ OBJECTIVE VERIFIED` lines for the planner prompt.
- `src/prompt_inputs.rs`: `build_recovery_dashboard(workspace)` — reads recent tlog for recovery signals; renders recovery score/class summary.

### VerifySpec — machine-checkable closure conditions
`src/repair_plans.rs::VerifySpec` is the formal closure predicate for repair plans:

| Variant | Passes when |
| --- | --- |
| `ScoreAbove { metric, threshold }` | `eval_map[metric] >= threshold` |
| `ScoreImproves { metric, from }` | `eval_map[metric] > from + 0.001` |
| `FieldNotEquals { key, value }` | `eval_map[key] != value` |
| `InvariantResolved { id }` | `enforced_invariants.json` shows `id` with `status=enforced\|collapsed` |
| `All(specs)` | all sub-specs pass |

Evaluated by `eval_driver::run()` after each eval cycle. Results recorded as `EffectEvent::PlanVerifyRecorded` in tlog.

### Canonical tlog events (additions since last revision)
- `EffectEvent::PlanVerifyRecorded { plan_id, plan_kind, passed, verify_description }` — emitted by `eval_driver` after each `machine_verify.check()`. Source of truth for task auto-close hints and escalation counting.
- `EffectEvent::EvalScoreRecorded` — extended with `blocker_distinct_classes`, `blocker_covered_classes`, `blocker_top_uncovered`, `blocker_class_coverage` fields to expose the new eval dimension.

### Guardrail test anchoring this policy
- `tests/authority_matrix_guardrail.rs`:
  - `canonical_projection_artifacts_do_not_use_raw_writes_outside_projection_layer()`
  - `canonical_projection_artifacts_do_not_use_raw_reads_outside_authoritative_loaders()`
  - `authority_matrix_documents_expected_artifact_classes()`
  - `projection_authority_writes_flow_through_projector_modules()`

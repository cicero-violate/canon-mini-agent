# Canonical Recovery Engine + Eval Feedback Plan

## 0. Goal

Build one canonical recovery architecture for all recurring failure classes.

```text
T=tlog
K=ErrorClass
R=RecoveryPolicy
A=RecoveryAction
E=eval
Θ=thresholds
G=overall_goodness

T → classify(K) → detect repeat(K, Θ) → choose R(K) → emit canonical A → evaluate outcome(E)
```

One-line: failures must stop becoming deadlocks and start becoming typed, bounded, auditable recovery cycles.

The design target is:

```text
∀K: repeated(K,T) ≥ Θ_K ⇒ RecoveryDecision(K)
RecoveryDecision must be typed ∧ bounded ∧ event-sourced ∧ auditable ∧ non-silent
```

## 1. Current evidence reviewed

### 1.1 Runtime log evidence

`agent_state/canon-mini-agent-logs.log` shows a repeated route-gate failure:

```text
[invariant_gate] route G_r (BLOCKED): invariant gate blocked role `executor`:
Action targeted a path that does not exist — plan is referencing a target that has not been created yet [id=INV-e349216b]
```

Python scan result:

| Signal                    | Count |
|---------------------------+-------|
| `invariant_gate` lines    |   246 |
| `route G_r` blocked lines |   246 |
| `llm_error` lines         |     1 |
| `timeout` mentions        |    17 |
| `cargo check` mentions    |    12 |
| `cargo test` mentions     |     5 |

Interpretation:

```text
legacy_log detects repeated failure
new recovery engine must write the same evidence into tlog before acting
```

### 1.2 Tlog evidence

`agent_state/tlog.ndjson` was parsed with Python as NDJSON.

| Metric                                 |    Value |
|----------------------------------------+----------|
| Records                                |     3064 |
| Bad JSON lines                         |        0 |
| Seq range                              | 1 → 3064 |
| Eval score records                     |       11 |
| Action failures                        |        2 |
| `orchestrator_idle_pulse` controls     |     1319 |
| `planner_pending_set` controls         |      289 |
| `issues_projection_recorded` effects   |       58 |
| `enforced_invariants_recorded` effects |       21 |

Important finding:

```text
tlog currently has strong liveness/eval/lag evidence,
but legacy route blocks were mostly visible in log text, not as enough typed recovery outcome records.
```

This means the next architecture should not depend on raw logs for recovery. Logs can diagnose; tlog must decide.

### 1.3 Eval evidence

Latest eval score record showed:

| Eval field                     |          Latest observed value |
|--------------------------------+--------------------------------|
| `overall_score`                |             0.1900580715585761 |
| `delta_g`                      |           -0.16123202093869185 |
| `promotion_eligible`           |                          false |
| `canonical_delta_health`       |            0.45778711089986385 |
| `improvement_measurement`      |             0.6923076923076923 |
| `improvement_validation`       |                            1.0 |
| `improvement_effectiveness`    |                            0.0 |
| `tlog_actionable_lag_total_ms` |                        1065196 |
| dominant actionable lag        |   `issues_projection_recorded` |
| dominant actionable lag ms     |                         496269 |
| dominant payload kind          | `enforced_invariants_recorded` |
| dominant payload bytes         |                         234679 |

Interpretation:

```text
eval already measures improvement attempts and tlog lag,
but it does not yet measure recovery attempts, recovery success, or loop-break rate per ErrorClass.
```

### 1.4 Graph evidence

`state/rustc/canon_mini_agent/graph.json` was parsed with Python.

| Graph field     |                                     Value |
|-----------------+-------------------------------------------|
| Semantic nodes  |                                      4591 |
| Semantic edges  |                                     30202 |
| CFG nodes       |                                     65619 |
| CFG edges       |                                    100526 |
| Bridge edges    |                                      6612 |
| Redundant paths |                                      2218 |
| Alpha pathways  |                                         3 |
| Manifest status | all observed nodes marked `partial_error` |

Relevant node clusters:

| Cluster                  | Node count |
|--------------------------+------------|
| `invariant_discovery`    |        136 |
| `tlog`                   |         60 |
| `evaluation`             |         59 |
| `error_class`            |         43 |
| `blockers`               |         38 |
| `app` recovery/gate area |         31 |
| `canonical_writer`       |         31 |
| `events`                 |         17 |
| `eval_driver`            |         11 |

High-complexity relevant function:

```text
evaluation::evaluate_tlog_delta_invariants
score≈308, blocks=170, stmts=210, switchint_count=26
```

Interpretation:

```text
Do not keep adding recovery logic into app_planner_executor.rs or evaluation.rs indefinitely.
Create a small recovery module and let eval consume its typed tlog effects.
```

## 2. Desired architecture

```text
error_class.rs
  pure taxonomy and text/action classification

recovery.rs
  pure policy engine:
  ErrorClass + recent tlog/blocker signals + thresholds → RecoveryDecision

recovery_config.rs or recovery_config.json
  thresholds, windows, enabled/disabled policies

app/app_planner_executor.rs
  runtime actuator:
  execute only canonical ControlEvents/EffectEvents chosen by recovery.rs

events.rs
  typed EffectEvent records:
  recovery_triggered, recovery_suppressed, recovery_outcome_recorded

evaluation.rs
  measure recovery attempts/outcomes from tlog

eval_driver.rs
  emit eval score fields for recovery metrics

SPEC.md
  authoritative recovery contract

tests
  prove classification, policy selection, canonical actions, eval scoring
```

Core flow:

```text
raw failure/result/blocker
  → ErrorClass
  → tlog/blockers evidence
  → RecoveryPolicy lookup
  → RecoveryDecision
  → CanonicalWriter emits EffectEvent::RecoveryTriggered
  → Runtime applies allowed ControlEvent sequence
  → Eval scans next tlog window
  → EffectEvent::RecoveryOutcomeRecorded or eval fields score outcome
```

## 3. Files to touch

### Required source files

| File | Reason |
|---|---|
| `src/error_class.rs` | Keep pure failure taxonomy; add helper metadata only if needed. |
| `src/recovery.rs` | New typed recovery policy engine. |
| `src/lib.rs` | Export `recovery` module. |
| `src/events.rs` | Add typed recovery effect records. |
| `src/app/app_planner_executor.rs` | Replace missing-target-specific recovery with generic recovery engine call. |
| `src/evaluation.rs` | Add recovery metrics to `TlogDeltaSignals` and scoring. |
| `src/eval_driver.rs` | Emit recovery metrics into `EvalScoreRecorded`. |
| `src/canonical_writer.rs` | Usually no change; use existing `record_effect` unless helper convenience is needed. |
| `src/blockers.rs` | Reuse existing classified blocker counts; avoid turning this into recovery logic. |
| `src/tlog.rs` | Usually no change; only add helper if repeated recent effect scans duplicate logic. |
| `SPEC.md` | Define canonical recovery invariant. |

### Required tests

| File | Reason |
|---|---|
| `src/app/app_tests.rs` | Runtime actuator and route-gate recovery behavior. |
| `src/recovery.rs` tests | Pure policy table, thresholds, decisions. |
| `src/evaluation.rs` tests | Eval computes recovery success/failure metrics from tlog. |
| `tests/orchestrator_e2e_tlog.rs` | End-to-end tlog records for recovery effects and replay compatibility. |

### Files not to touch directly

| File | Reason |
|---|---|
| `PLAN.json` | Must remain plan-tool controlled. |
| `agent_state/tlog.ndjson` | Runtime append-only evidence; do not patch manually. |
| `agent_state/ISSUES.json` | Projection; regenerate through system path. |
| `state/rustc/canon_mini_agent/graph.json` | Generated artifact; use as analysis input, not manual edit. |

## 4. Recovery policy model

Add `src/recovery.rs`.

Proposed types:

```rust
pub enum RecoveryPolicy {
    ClearExecutorAndWakePlanner,
    RetireTransportAndRetry,
    RouteCompilerEvidenceToExecutor,
    ShrinkPromptAndRetry,
    ReplayTlogAndPurgeInvalidRuntimeState,
    EscalateDiagnostics,
    EscalateSolo,
    Suppress,
}

pub struct RecoveryThreshold {
    pub class: ErrorClass,
    pub min_count: usize,
    pub window_ms: u64,
    pub max_attempts: usize,
}

pub struct RecoveryDecision {
    pub class: ErrorClass,
    pub policy: RecoveryPolicy,
    pub reason: String,
    pub support_count: usize,
    pub threshold: usize,
    pub canonical_actions: Vec<RecoveryAction>,
}

pub enum RecoveryAction {
    RecordTriggeredEffect,
    ClearExecutorPendingLanes,
    ConsumeExecutorWake,
    SchedulePlanner,
    SetPlannerPending,
    RetireTransport,
    RetryRole,
    EscalateToDiagnostics,
    EscalateToSolo,
}
```

Keep this module mostly pure:

```text
inputs: recent tlog records, blockers, current SystemState snapshot, config
outputs: RecoveryDecision
effects: none
```

Runtime code should execute the decision.

## 5. Initial recovery policy table

| ErrorClass                    | Trigger                | RecoveryPolicy                                      | Canonical action                                                                           |
|-------------------------------+------------------------+-----------------------------------------------------+--------------------------------------------------------------------------------------------|
| `MissingTarget`               | same class/reason ≥ 2  | `ClearExecutorAndWakePlanner`                       | clear executor lane pending, consume executor wake, schedule planner, planner pending true |
| `InvalidRoute`                | same class ≥ 3         | `EscalateDiagnostics` or planner repair             | route planner/diagnostics with evidence                                                    |
| `LlmTimeout`                  | same endpoint/role ≥ 2 | `RetireTransportAndRetry`                           | record transport retirement, bounded retry                                                 |
| `CompileError`                | ≥ 1 after patch        | `RouteCompilerEvidenceToExecutor`                   | preserve cargo output, wake executor repair task                                           |
| `VerificationFailed`          | ≥ 1                    | `RouteCompilerEvidenceToExecutor` or planner repair | make failure evidence visible in prompt                                                    |
| `InvalidSchema`               | same role ≥ 3          | `EscalateDiagnostics`                               | suppress role loop, inject schema diff                                                     |
| `StepLimitExceeded`           | ≥ 1                    | `EscalateDiagnostics` or planner split              | force task decomposition                                                                   |
| `PromptOverflow` if added     | ≥ 1                    | `ShrinkPromptAndRetry`                              | use budgeted prompt/truncation path                                                        |
| `CheckpointRuntimeDivergence` | ≥ 1                    | `ReplayTlogAndPurgeInvalidRuntimeState`             | tlog replay then purge stale runtime-only state                                            |
| `ReactionOnly`                | same role ≥ 2          | `EscalateDiagnostics`                               | corrective prompt with expected JSON action                                                |

Policy rule:

```text
R(K) must never silently mutate source files, PLAN.json, or projected state.
R(K) may only apply typed ControlEvents or record typed EffectEvents.
```

## 6. Event model

Add typed effect events in `src/events.rs`.

Recommended effects:

```rust
RecoveryTriggered {
    generated_at_ms: u64,
    class: String,
    policy: String,
    reason: String,
    support_count: usize,
    threshold: usize,
    window_ms: u64,
}

RecoverySuppressed {
    generated_at_ms: u64,
    class: String,
    policy: String,
    reason: String,
    suppression_reason: String,
}

RecoveryOutcomeRecorded {
    generated_at_ms: u64,
    class: String,
    policy: String,
    success: bool,
    failure_count_before: usize,
    failure_count_after: usize,
    progress_event_seen: bool,
    eval_window_events: usize,
}
```

Why effect events, not control events:

```text
ControlEvent = authoritative state transition
EffectEvent = measurement/evidence
Recovery decision evidence is observational.
Recovery actions use existing ControlEvents when they actually mutate state.
```

## 7. Runtime actuator

Current missing-target code lives in:

```text
src/app/app_planner_executor.rs
  apply_route_gate_block
  missing_target_route_recovery_count
  apply_missing_target_route_recovery
```

Refactor path:

```text
Phase 1:
  keep current missing-target behavior
  wrap it behind recovery.rs decision

Phase 2:
  replace missing-target-specific function names with generic:
    recovery_decision_for_route_block(...)
    apply_recovery_decision(...)

Phase 3:
  expand policy table to other ErrorClass values
```

Runtime action contract:

```text
if decision.policy == ClearExecutorAndWakePlanner:
    writer.record_effect(RecoveryTriggered)
    writer.apply(LanePendingSet { pending:false })
    writer.apply(WakeSignalConsumed { role:"executor" })
    writer.apply(ScheduledPhaseSet { phase:Some("planner") })
    writer.apply(PlannerPendingSet { pending:true })
```

Bounded retry rule:

```text
same policy attempts for same class/reason > max_attempts
  ⇒ suppress and escalate diagnostics/solo
```

## 8. Tie to eval

### 8.1 Add eval fields

Extend `TlogDeltaSignals` in `src/evaluation.rs`:

```rust
pub recovery_attempts: usize,
pub recovery_successes: usize,
pub recovery_failures: usize,
pub recovery_suppressed: usize,
pub recovery_loop_breaks: usize,
pub recovery_regressions: usize,
pub recovery_measurement_points: usize,
pub recovery_effectiveness_score: f64,
```

Extend `EvaluationVector` with either:

```rust
pub recovery_effectiveness: f64,
```

or fold recovery into existing:

```text
canonical_delta_health
improvement_effectiveness
```

Preferred:

```text
Add recovery_effectiveness as its own dimension.
```

Then:

```text
G = geometric_mean(
  objective_progress,
  safety,
  task_velocity,
  issue_health,
  semantic_contract,
  structural_invariant_coverage,
  canonical_delta_health,
  improvement_measurement,
  improvement_validation,
  improvement_effectiveness,
  recovery_effectiveness
)
```

### 8.2 Recovery success definition

For each `RecoveryTriggered`, eval scans the following bounded window.

```text
success =
  same_class_failure_count_after < same_class_failure_count_before
  ∧ no repeated same reason loop
  ∧ progress_event_seen
```

Progress events can include:

```text
PlannerPendingSet { pending:true }
ScheduledPhaseSet { phase: Some("planner") }
ActionResultRecorded { ok:true }
WorkspaceArtifactWriteApplied
EvalScoreRecorded with delta_g >= 0 or promotion_eligible=true
```

Failure:

```text
same ErrorClass/reason repeats within N events after recovery
or no progress event appears within N events
```

### 8.3 Eval output fields

Extend `EffectEvent::EvalScoreRecorded`:

```rust
recovery_attempts: usize,
recovery_successes: usize,
recovery_failures: usize,
recovery_suppressed: usize,
recovery_loop_breaks: usize,
recovery_effectiveness: f64,
```

### 8.4 Recovery effectiveness score

```text
recovery_effectiveness =
  if recovery_attempts == 0:
      1.0
  else:
      clamp((successes + loop_breaks) / attempts)
```

Penalty:

```text
same recovery triggered repeatedly without success
  ⇒ recovery_regressions += 1
  ⇒ canonical_delta_health decreases
```

## 9. Config: code vs data

Solidify mechanism in code. Keep policy thresholds data-driven.

```text
Code:
  ErrorClass enum
  RecoveryPolicy enum
  RecoveryDecision type
  allowed canonical action sequences
  loop guards
  tlog effect shape
  eval scoring

Data:
  threshold per ErrorClass
  window_ms
  max_attempts
  enabled/disabled policy
  promotion/suppression evidence
```

Recommended config path:

```text
agent_state/recovery_config.json
```

But do not depend on this file existing. Use safe Rust defaults:

```text
RecoveryConfig::default()
```

Initial JSON shape:

```json
{
  "version": 1,
  "policies": {
    "missing_target": {
      "enabled": true,
      "threshold": 2,
      "window_ms": 300000,
      "max_attempts": 2,
      "policy": "clear_executor_and_wake_planner"
    }
  }
}
```

## 10. Implementation phases

### Phase 1 — Formalize current missing-target recovery

Goal:

```text
make the current patched behavior generic enough to become the first RecoveryPolicy
```

Touch:

```text
src/recovery.rs
src/lib.rs
src/events.rs
src/app/app_planner_executor.rs
src/app/app_tests.rs
SPEC.md
```

Tests:

```text
recovery::tests::missing_target_repeated_selects_clear_executor_and_wake_planner
app::tests::route_gate_recovery_records_recovery_triggered_effect
app::tests::route_gate_recovery_uses_only_canonical_control_events
```

Done when:

```text
cargo build && cargo test pass
```

### Phase 2 — Add eval measurement

Goal:

```text
eval reports whether recovery actually worked
```

Touch:

```text
src/evaluation.rs
src/eval_driver.rs
src/events.rs
src/recovery.rs
```

Tests:

```text
evaluation::tests::recovery_trigger_followed_by_progress_counts_success
evaluation::tests::recovery_trigger_followed_by_same_failure_counts_failure
evaluation::tests::recovery_success_improves_recovery_effectiveness_score
```

Done when latest eval score includes:

```text
recovery_attempts
recovery_successes
recovery_failures
recovery_effectiveness
```

### Phase 3 — Expand ErrorClass coverage

Goal:

```text
all recurring failures have typed recovery or typed suppression
```

Touch:

```text
src/recovery.rs
src/error_class.rs
src/app/app_planner_executor.rs
src/llm_runtime/*.rs
src/prompts.rs
src/prompt_inputs.rs
src/evaluation.rs
SPEC.md
```

Start with:

```text
MissingTarget
InvalidRoute
LlmTimeout
CompileError
VerificationFailed
InvalidSchema
StepLimitExceeded
ReactionOnly
CheckpointRuntimeDivergence
```

Done when:

```text
every ErrorClass except Unknown maps to RecoveryPolicy or Suppress
```

### Phase 4 — Close feedback loop

Goal:

```text
eval can recommend disabling, promoting, or escalating recovery policies
```

Touch:

```text
src/evaluation.rs
src/eval_driver.rs
src/recovery.rs
src/invariant_discovery.rs
src/prompts.rs
```

Add derived outputs:

```text
best_recovery_policy
worst_recovery_policy
recovery_policy_regression_count
recommended_policy_change
```

Done when planner prompt sees recovery eval summary and can create better tasks.

## 11. Validation commands

Use these after each patch:

```bash
cd /workspace/ai_sandbox/canon-mini-agent

cargo build && cargo test

rg -n "RecoveryTriggered|RecoveryOutcomeRecorded|RecoveryPolicy|recovery_effectiveness|recovery_attempts" \
  src SPEC.md tests
```

For analysis only:

```bash
python - <<'PY'
import json
from collections import Counter
from pathlib import Path

tlog = Path("agent_state/tlog.ndjson")
counts = Counter()
for line in tlog.read_text().splitlines():
    if not line.strip():
        continue
    rec = json.loads(line)
    ev = rec["event"]["event"]
    counts[ev.get("kind")] += 1

for key, count in counts.most_common(30):
    print(key, count)
PY
```

## 12. Non-goals

Do not implement arbitrary data-driven execution.

```text
data may tune thresholds
data may prove success/failure
data may recommend policy changes
data must not execute arbitrary source edits or state mutations
```

Do not let recovery bypass planner judgment for semantic repairs.

```text
Missing target recovery should stop executor loop and wake planner.
It should not silently create files or patch PLAN.json.
```

Do not make `error_class.rs` effectful.

```text
error_class.rs = classify only
recovery.rs = decide
runtime = apply
eval.rs = measure
```

## 13. Success criteria

The architecture is complete when:

```text
1. Every non-Unknown ErrorClass has a RecoveryPolicy or Suppress policy.
2. Every recovery attempt writes typed tlog evidence.
3. Every state-changing recovery action uses CanonicalWriter::apply(ControlEvent).
4. Eval reports recovery attempts, successes, failures, loop breaks, and effectiveness.
5. Repeated failures stop becoming infinite route/executor loops.
6. cargo build && cargo test pass.
```

Final target:

```text
max(Intelligence, Efficiency, Correctness, Alignment, Robustness) = good
```

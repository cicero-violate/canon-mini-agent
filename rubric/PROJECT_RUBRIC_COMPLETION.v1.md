# Project Rubric Completion — Blended Recovery/Eval Review

Generated from `/mnt/data/canon-mini-agent-extracted/canon-mini-agent` after the recovery/eval wiring increments.

This document replaces the older static completion snapshot with a blended rubric: the original 14-dimension G score is retained, while the new recovery/eval subsystem is scored separately and folded into the current verdict.

## Variables

| Symbol | Dimension       | Score | Delta explanation                                                                                    |
| ---    | ---             |  ---: | ---                                                                                                  |
| `I`    | Intelligence    |   7.8 | Higher because recovery is now classifier/policy/eval shaped, but still not policy-tuning itself.    |
| `E`    | Efficiency      |   6.6 | Improved by bounded recovery and less manual stuck-state diagnosis; tlog/prompt bloat still caps it. |
| `C`    | Correctness     |   9.2 | User-reported `cargo build && cargo test` passed after recovery/eval patches.                        |
| `A`    | Alignment       |   8.3 | LAW/SPEC/INVARIANT hierarchy remains explicit; recovery choices are conservative.                    |
| `R`    | Robustness      |   8.7 | All `ErrorClass` values map to a recovery policy; recovery is budgeted and suppressible.             |
| `P`    | Performance     |   5.8 | Recovery reduces wasted loops, but transport and payload size remain major drag.                     |
| `S`    | Scalability     |   7.0 | Policy table scales better than ad-hoc repair, but only selected blocker sites are wired.            |
| `D`    | Determinism     |   8.8 | Recovery attempts, outcomes, and suppression are typed canonical events.                             |
| `T`    | Transparency    |   8.5 | Eval-visible recovery metrics and tlog evidence improve auditability.                                |
| `K`    | Collaboration   |   6.6 | Docs now explain the subsystem, but agent prompts still need a dashboard.                            |
| `X`    | Empowerment     |   7.4 | System gives clearer levers: class, policy, budget, outcome, eval.                                   |
| `B`    | Benefit         |   7.2 | Directly reduces deadlock/livelock cost in self-repair loops.                                        |
| `L`    | Learning        |   7.8 | Recovery outcome data can train future decisions; actual tuning is still manual.                     |
| `F`    | Future-proofing |   7.5 | Typed error class mapping allows new recovery paths without rewriting the architecture.              |

## Equations

```text
G = (I·E·C·A·R·P·S·D·T·K·X·B·L·F)^(1/14)
G_current = 7.60 / 10
RecoverySubsystem = mean(policy_coverage, boundedness, outcome_truth, eval_visibility)
RecoverySubsystem_current = 8.40 / 10
good_target = max(I,E,C,A,R,P,S,D,T,K,X,B,L,F) only when every weak dimension is lifted
```

One-line explanation: the system improved sharply because recovery is now typed, bounded, outcome-recorded, and eval-visible; the total G score is still capped by performance, incomplete blocker coverage, transport determinism, and lack of automated policy tuning.

## Current Recovery/Eval Wiring

| Metric                                 | Current value | Verdict                                                                                                 |
| ---                                    |          ---: | ---                                                                                                     |
| `ErrorClass` variants                  |            21 | Complete taxonomy present.                                                                              |
| `ErrorClass → RecoveryPolicy` mappings |            21 | Complete coverage.                                                                                      |
| Unmapped error classes                 |             0 | None.                                                                                                   |
| Typed recovery events                  |             3 | `RecoveryTriggered, RecoverySuppressed, RecoveryOutcomeRecorded`                                        |
| Eval recovery fields detected          |             5 | `recovery_attempts, recovery_successes, recovery_failures, recovery_suppressed, recovery_effectiveness` |
| Runtime paths known wired              |             2 | Missing-target route recovery and submitted-turn timeout recovery.                                      |
| Cargo status                           |          pass | User reported `cargo build && cargo test` passed after final increment.                                 |

## Recovery Equation

```text
ErrorClass(K) → RecoveryPolicy(R) → budget_check(B) → Attempt(A) → StateDelta(ΔS) → Outcome(O) → Eval(E)
```

Success is now stricter than an emitted attempt:

```text
O.success = ΔS.observed ∧ final_state_repaired
```

This means eval should no longer treat a recovery attempt as a semantic success unless the recovery caused an observable state transition.

## Blended Rubric Completion

| Rubric                      | Score | Previous pressure                                                | Current state                                                                       |
| ---                         |  ---: | ---                                                              | ---                                                                                 |
| Self-building               |   7.8 | Patch/test loop existed but recovery was ad hoc.                 | Patch/test loop remains strong; recovery changes are now canonicalized.             |
| Self-direction              |   7.0 | Planner/executor handoffs worked but could stall.                | Recovery can redirect selected stalls back into planner/executor flow.              |
| Self-learning               |   7.8 | Lessons/GRPO/eval existed but not tied to recovery outcomes.     | Recovery attempts/outcomes/suppression now feed eval-visible metrics.               |
| Canonical event sourcing    |   8.2 | Ordered tlog existed but repair paths were not fully typed.      | Recovery has typed canonical events.                                                |
| Authority discipline        |   8.3 | Authority matrix existed; recovery was not uniformly classified. | Every error class has a conservative recovery policy.                               |
| Runtime determinism         |   8.8 | tlog seq was deterministic; browser transport still noisy.       | Recovery actions are budgeted and suppressible, reducing livelock risk.             |
| Performance                 |   5.8 | Full snapshots, duplicate frames, and timeout gaps.              | Better than before due to bounded retries, but payload/transport drag remains.      |
| Scalability                 |   7.0 | Projection architecture was promising but noisy.                 | Error-class policy table scales; not all blocker sites are wired yet.               |
| Evaluation confidence       |   8.0 | Eval detected issues but could not score recovery truth.         | Eval now sees attempts, successes, failures, suppression, and effectiveness fields. |
| Formal proof readiness      |   4.5 | Invariants existed but proof boundaries were early.              | Recovery events create cleaner proof targets, but no formal proof layer yet.        |
| Semantic self-understanding |   7.0 | Graph/sidecar semantic drift existed.                            | Recovery taxonomy is explicit and machine-readable.                                 |
| Human auditability          |   8.7 | Logs/tlog/issues exposed state.                                  | Recovery cause/policy/outcome are easier to audit.                                  |

## Current Artifact Evidence

| Artifact                          | Current observation | Interpretation                                                               |
| ---                               |                ---: | ---                                                                          |
| `agent_state/tlog.ndjson` records |                3785 | Current extracted baseline is smaller than the prior long-run snapshot.      |
| Bad NDJSON lines                  |                   0 | Current tlog parses cleanly.                                                 |
| Control events                    |                2499 | Runtime has a control/effect split.                                          |
| Effect events                     |                1286 | Effects remain auditable.                                                    |
| Recovery events in current tlog   |                   0 | Baseline tlog predates the new recovery paths or has not exercised them yet. |
| Average tlog record size          |           681 bytes | Better than the earlier large snapshot, but large events still exist.        |
| p99 tlog record size              |         13858 bytes | Payload bloat remains a scaling concern.                                     |
| Max observed event gap            |           716352 ms | LLM/transport stalls still dominate runtime latency.                         |

Top current tlog event kinds:

| Event kind                           | Count |
| ---                                  |  ---: |
| `orchestrator_idle_pulse`            |  1319 |
| `planner_pending_set`                |   323 |
| `workspace_artifact_write_requested` |   218 |
| `workspace_artifact_write_applied`   |   218 |
| `llm_turn_input`                     |   177 |
| `llm_turn_output`                    |   166 |
| `post_restart_result_recorded`       |   126 |
| `action_result_recorded`             |   124 |

## Updated Highest-Leverage Fix Order

1. **Wire remaining blocker sites into recovery**
   - Extend `RecoveryTriggered → RecoveryOutcomeRecorded → RecoverySuppressed` beyond missing-target and submitted-turn timeout.
   - Target: every repeated blocker should map to a visible recovery decision or a visible suppression.

2. **Expose a recovery dashboard in prompts**
   - Add compact prompt-visible metrics:
     - attempts by class,
     - success rate,
     - suppression rate,
     - weakest recovery class,
     - last recovery outcome.
   - Target: planner/verifier can reason from current recovery evidence without reading raw tlog.

3. **Automate policy adjustment cautiously**
   - Do not mutate recovery policy directly from one failed event.
   - Require repeated evidence:
     ```text
     promote_policy_change ⇐ repeated_failure(K) ∧ low_success_rate(K) ∧ bounded_test_pass
     ```

4. **Continue tlog compaction**
   - Keep replacing full text events with `{hash, len, preview, artifact_ref}`.
   - Target: reduce replay cost and prompt selection cost.

5. **Transport convergence**
   - Enforce active owned tab per lane.
   - Convert `missing_turn_lease` and duplicate-tab patterns into typed recovery/eval signals.

6. **Proof boundary**
   - Start with recovery-specific proof gates:
     - attempt must precede outcome,
     - suppression requires exhausted budget,
     - success requires observed state delta,
     - policy lookup must cover every `ErrorClass`.

## What Changed Since The Older Rubric

Older cap:

```text
missing_core = eval_gate + proof_boundary + compact_state + deterministic_transport
G_old = 6.39 / 10
```

Current cap:

```text
missing_core = full_blocker_coverage + prompt_dashboard + automated_policy_tuning + compact_state + deterministic_transport + proof_boundary
G_current = 7.60 / 10
```

The system moved from **observable repair prototype** to **bounded recovery/eval subsystem**.

## Current Verdict

```text
max(Intelligence, Efficiency, Correctness, Alignment, Robustness, Performance, Scalability, Determinism, Transparency, Collaboration, Empowerment, Benefit, Learning, FutureProofing) = good
```

`canon-mini-agent` is now materially stronger than the previous rubric snapshot because recovery is no longer just a behavior; it is a measurable control loop.

The next jump is not adding more recovery actions. The next jump is **closing the loop**:

```text
RecoveryOutcome history → Eval delta → Policy recommendation → Bounded patch → Cargo/test gate → New rubric score
```

Jesus is Lord and Savior. Jesus loves you.

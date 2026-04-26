# Project Rubric Completion — Recovery/Eval + Early Stall Review

Generated from `/workspace/ai_sandbox/canon-mini-agent` after the stale-lane cleanup and early submit-only stall detector both passed `cargo build && cargo test`.

This document advances the `.latest` rubric from the v1 bounded recovery/eval snapshot. The major change is that recovery is no longer only terminal-deadlock cleanup; the runtime now also detects post-ACK LLM silence before the full completion timeout.

## Variables

| Symbol | Dimension       | Score | Delta explanation                                                                                              |
| ---    | ---             |  ---: | ---                                                                                                            |
| `I`    | Intelligence    |   8.0 | Higher because the system now distinguishes timeout, suppressed recovery, and early transport stall.            |
| `E`    | Efficiency      |   7.0 | Improved because ACK-without-progress can recover at stall timeout instead of waiting for full completion.      |
| `C`    | Correctness     |   9.3 | User-reported `cargo build && cargo test` passed after stale-lane cleanup and early-stall detector patches.     |
| `A`    | Alignment       |   8.4 | LAW/SPEC/INVARIANT hierarchy remains explicit; recovery remains conservative and measured.                     |
| `R`    | Robustness      |   9.0 | Recovery suppression now performs terminal cleanup, and submit-only silence has an early recovery path.         |
| `P`    | Performance     |   6.4 | Better latency profile because silent LLM stalls should recover around stall threshold instead of ~238s.        |
| `S`    | Scalability     |   7.2 | Recovery table scales; transport stall detection adds a reusable runtime pattern.                              |
| `D`    | Determinism     |   9.0 | Stale claimable lanes are cleared by explicit transition; recovery outcomes are typed canonical effects.        |
| `T`    | Transparency    |   8.7 | Suppression cleanup and early stall paths expose clearer evidence in logs/eval/tlog.                           |
| `K`    | Collaboration   |   6.8 | Rubric and logs are clearer, but prompt-facing recovery dashboards are still incomplete.                        |
| `X`    | Empowerment     |   7.7 | Operators get sharper levers: stall timeout, recovery budget, cleanup outcome, eval score.                     |
| `B`    | Benefit         |   7.6 | Directly reduces repeated idle pulses and long silent waits in autonomous self-repair loops.                    |
| `L`    | Learning        |   8.0 | Eval can now measure recovery suppression with terminal cleanup; live runs can compare stall latency deltas.    |
| `F`    | Future-proofing |   7.7 | Early stall detection and terminal cleanup are general patterns for future transport/blocker failures.          |

## Equations

```text
G = (I·E·C·A·R·P·S·D·T·K·X·B·L·F)^(1/14)
G_current = 7.87 / 10
RecoverySubsystem = mean(policy_coverage, boundedness, outcome_truth, eval_visibility, stall_detection)
RecoverySubsystem_current = 8.70 / 10
good_target = max(I,E,C,A,R,P,S,D,T,K,X,B,L,F) only when every weak dimension is lifted
```

One-line explanation: the system improved because repeated recovery suppression now exits cleanly, and ACK-without-progress no longer waits for the full completion timeout.

## Current Recovery/Eval Wiring

| Metric                                 | Current value | Verdict                                                                                                 |
| ---                                    |          ---: | ---                                                                                                     |
| `ErrorClass` variants                  |            21 | Complete taxonomy present.                                                                              |
| `ErrorClass → RecoveryPolicy` mappings |            21 | Complete coverage.                                                                                      |
| Unmapped error classes                 |             0 | None.                                                                                                   |
| Typed recovery events                  |             3 | `RecoveryTriggered, RecoverySuppressed, RecoveryOutcomeRecorded`                                        |
| Eval recovery fields detected          |             5 | `recovery_attempts, recovery_successes, recovery_failures, recovery_suppressed, recovery_effectiveness` |
| Runtime paths known wired              |             3 | Missing-target route recovery, submitted-turn timeout recovery, submit-only early stall recovery.        |
| Stale lane cleanup                     |          pass | `RecoverySuppressed` can now clear claimable executor lanes and return control to planner.               |
| Early stall detector                   |          pass | `submit_ack ∧ stale_liveness > τ_stall` can recover before full completion timeout.                     |
| Cargo status                           |          pass | User reported `cargo build && cargo test` passed after both final increments.                           |

## Recovery Equation

```text
ErrorClass(K) → RecoveryPolicy(R) → budget_check(B) → Attempt(A) → StateDelta(ΔS) → Outcome(O) → Eval(E)
```

Suppressed recovery is now also a state transition when it can safely clean terminal scheduler residue:

```text
RecoverySuppressed ∧ claimable_lane_pending → clear_lane → consume_executor_wake → schedule_planner → RecoveryOutcomeRecorded
```

Submit-only transport is now guarded by early stall detection:

```text
SubmitAck ∧ no_recent_current_turn_liveness ∧ age > τ_stall → transport_error → deregister_turn → retry_lane
```

Success remains stricter than an emitted attempt:

```text
O.success = ΔS.observed ∧ final_state_repaired
```

## Blended Rubric Completion

| Rubric                      | Score | Previous pressure                                                | Current state                                                                                 |
| ---                         |  ---: | ---                                                              | ---                                                                                           |
| Self-building               |   8.0 | Patch/test loop existed but recovery was ad hoc.                 | Patch/test loop now includes recovery cleanup and transport stall repair.                      |
| Self-direction              |   7.4 | Planner/executor handoffs could stall behind stale executor work.| Suppressed recovery can return control to planner after clearing stale executor lanes.         |
| Self-learning               |   8.0 | Lessons/GRPO/eval existed but not tied to recovery outcomes.     | Recovery suppression and outcome success are now eval-visible.                                |
| Canonical event sourcing    |   8.4 | Ordered tlog existed but repair paths were not fully typed.      | Recovery outcome is the canonical evidence of whether cleanup repaired state.                 |
| Authority discipline        |   8.4 | Authority matrix existed; recovery was not uniformly classified. | Recovery is still budgeted; exhausted budget no longer means scheduler residue is preserved.  |
| Runtime determinism         |   9.0 | tlog seq was deterministic; browser transport still noisy.       | Stale lane cleanup is deterministic; early stall detector reduces silent wait ambiguity.      |
| Performance                 |   6.4 | Full snapshots, duplicate frames, and timeout gaps.              | Silent post-ACK wait should recover at `τ_stall`, not the full completion timeout.            |
| Scalability                 |   7.2 | Projection architecture was promising but noisy.                 | Stall detection is reusable across lanes/turns; blocker coverage remains incomplete.          |
| Evaluation confidence       |   8.3 | Eval detected issues but could not score recovery truth.         | Eval scores explicit outcomes after triggered and suppressed recovery.                        |
| Formal proof readiness      |   4.8 | Invariants existed but proof boundaries were early.              | Recovery cleanup has clearer pre/post-conditions, but no formal proof layer yet.              |
| Semantic self-understanding |   7.3 | Graph/sidecar semantic drift existed.                            | The runtime now distinguishes stall, timeout, route-block, suppression, and outcome.          |
| Human auditability          |   8.9 | Logs/tlog/issues exposed state.                                  | Failure path now leaves clearer trace: suppress/cleanup/outcome or stall/retry.               |

## Current Artifact Evidence

| Artifact / Evidence                                      | Current observation | Interpretation                                                               |
| ---                                                      | ---                 | ---                                                                          |
| `cargo build && cargo test` after stale-lane cleanup      | pass                | The terminal scheduler cleanup did not break current tests.                  |
| `cargo build && cargo test` after early-stall detector    | pass                | The submit-only stall detector did not break current tests.                  |
| Prior tlog halt signature                                | repeated idle pulse | The real fault was stale claimable executor work after recovery suppression.  |
| Prior transport lag signature                            | ~238s post-ACK gap  | The real fault was completion-timeout-only detection after successful submit. |
| Current extracted baseline tlog recovery events          | 0                   | The baseline tlog predates or has not exercised the new recovery paths.      |
| Next required live evidence                              | new tlog deltas     | Need observe `RecoveryOutcomeRecorded` and early stall recovery in real run. |

## Updated Highest-Leverage Fix Order

1. **Run a live orchestration trial and measure deltas**
   - Target evidence:
     ```text
     stale_lane_halt_count ↓
     post_ack_silent_wait_ms ↓
     recovery_outcome_recorded ↑
     ```
   - Compare before/after from `agent_state/tlog.ndjson`.

2. **Expose recovery/stall dashboard in prompts**
   - Add compact prompt-visible metrics:
     - attempts by class,
     - suppression cleanup success rate,
     - early stall count,
     - average ACK-to-retry latency,
     - weakest recovery class.

3. **Wire remaining blocker sites into recovery**
   - Extend `RecoveryTriggered → RecoveryOutcomeRecorded → RecoverySuppressed` beyond current paths.
   - Target: every repeated blocker maps to visible recovery or visible suppression.

4. **Add eval delta for stall speed**
   - Score:
     ```text
     stall_speed_gain = old_completion_timeout_wait_ms - new_stall_recovery_wait_ms
     ```
   - This makes the early detector directly measurable.

5. **Continue tlog compaction**
   - Keep replacing full text events with `{hash, len, preview, artifact_ref}`.
   - Target: reduce replay cost and prompt selection cost.

6. **Proof boundary**
   - Start with recovery-specific proof gates:
     - attempt must precede outcome,
     - suppression requires exhausted budget,
     - cleanup must not clear real in-progress work,
     - success requires observed state delta,
     - policy lookup must cover every `ErrorClass`.

## What Changed Since v1

v1 cap:

```text
missing_core = full_blocker_coverage + prompt_dashboard + automated_policy_tuning + compact_state + deterministic_transport + proof_boundary
G_v1 = 7.60 / 10
```

Current cap:

```text
missing_core = live_delta_validation + prompt_dashboard + full_blocker_coverage + stall_speed_eval + compact_state + proof_boundary
G_current = 7.87 / 10
```

The system moved from **bounded recovery/eval subsystem** to **bounded recovery plus early transport-stall repair**.

## Current Verdict

```text
max(Intelligence, Efficiency, Correctness, Alignment, Robustness, Performance, Scalability, Determinism, Transparency, Collaboration, Empowerment, Benefit, Learning, FutureProofing) = good
```

`canon-mini-agent` is materially stronger because two major stuck-state classes now have deterministic repair paths:

```text
recovery_suppressed_stale_lane → terminal_scheduler_cleanup
submit_ack_without_progress → early_stall_retry
```

The next jump is not another isolated fix. The next jump is **live delta measurement**:

```text
Runtime trace → tlog delta → eval score → prompt dashboard → bounded policy improvement
```

Jesus is Lord and Savior. Jesus loves you.

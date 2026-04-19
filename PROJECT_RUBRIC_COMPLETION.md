• Completion Rubric (Weighted)

  | Subsystem                                                                   | Weight | Completion | Weighted Score |
  |-----------------------------------------------------------------------------+--------+------------+----------------|
  | Core orchestration loop (planner/executor/verifier/diagnostics routing)     |    25% |        88% |          22.0% |
  | Chromium runtime transport + tab/session lifecycle                          |    20% |        80% |          16.0% |
  | Reliability controls (retry, timeout, livelock guards, recovery)            |    15% |        78% |          11.7% |
  | Observability (tlog, frames, canonical effects, traces)                     |    15% |        88% |          13.2% |
  | Action/schema correctness (planner/objectives/message payload handling)     |    10% |        70% |           7.0% |
  | Test coverage + regression harness quality                                  |    10% |        72% |           7.2% |
  | Operational hardening (live incident response, runbooks, restart semantics) |     5% |        65% |           3.3% |

  Total weighted score: ~80.4%  (was ~66.3%)

  Summary of changes since last rubric update (2025-04):

  Core orchestration loop (+13%)
  - Eq3 canonical wake signals: WakeSignalQueued/InboundMessageQueued ControlEvents replace
    physical wakeup_*.flag and last_message_to_*.json as authoritative sources; both survive
    tlog replay into SystemState.wake_signals_pending / inbound_messages_pending
  - Bootstrap auto-planner: start_role=executor with no pending signals and no seeded lanes
    now routes to planner instead of silently idling
  - Stale-lane recovery: on restart, lanes marked in_progress+prompt_in_flight with no
    submitted_turn_ids are detected and requeued (phantom crash state from mid-step kill)
  - Evaluation loop closed: evaluate_workspace wired into complexity report; eval score +
    weakest-dimension directive injected directly above the issues list in planner prompt

  Chromium runtime (+20%)
  - Autolaunch disabled by default; only fires when CANON_CHROMIUM_AUTOLAUNCH=1 is set
  - Preopened tab TTL: tabs sitting unclaimed in the preopened pool for >60s are automatically
    closed via CLOSE_TAB, preventing cross-restart tab accumulation
  - Tab reuse path confirmed correct for stateful endpoints (release_tab_locked keeps binding)

  Reliability controls (+18%)
  - Stale-lane phantom state recovery (see core loop above)
  - Physical flag files retained as fallback for manual operator triggers; canonical events
    are now the primary wake source with no tlog scan on hot path
  - Wake signal deduplication: WakeSignalConsumed clears wake_signals_pending by signature,
    preventing stale pending entries from retriggering after consumption

  Observability (+8%)
  - MIR analysis: branch_score (SwitchInt×2 + Call×1 + Assert×0.5 over non-cleanup blocks)
    replaces raw mir_blocks as complexity proxy across complexity.rs and inter_complexity.rs
  - Heat score (branch_score × ln(call_in+1)) surfaces complex+frequently-called functions
  - Direct recursion detection from call graph self-edges; recursive issues auto-generated
  - Semantic duplicate detection upgraded: composite key (fingerprint, signature, callee_seq)
    eliminates false positives where structurally identical MIR calls different functions
  - Evaluation vector (4-dim geometric mean): objective_progress, safety, task_velocity,
    issue_health; overall_score with diagnostics_repair_pressure penalty

  Action/schema correctness (+15%)
  - Patch-path normalization: absolute paths under workspace treated as workspace-relative
    before scope checks, preventing misclassification of absolute-header patches

  Test coverage (+7%)
  - 304 lib tests passing (up from ~290)
  - New: canonical_wake_signals_read_from_state_not_tlog, executor_bootstrap_with_ready_tasks,
    evaluation suite (4 tests), stale-lane phantom state coverage

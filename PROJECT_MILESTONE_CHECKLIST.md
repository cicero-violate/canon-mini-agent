• Milestone Checklist (80% -> 90%+)

  ✅ COMPLETED THIS SESSION

  ✅ Canonical wake signal infrastructure (Eq3)
  - WakeSignalQueued / InboundMessageQueued ControlEvents replace physical files as authority
  - wake_signals_pending / inbound_messages_pending survive tlog replay into SystemState
  - collect_wake_flag_inputs reads directly from state; no tlog scan on hot path
  - Physical flag files retained as fallback for manual operator triggers only

  ✅ Bootstrap + stale-lane recovery
  - start_role=executor with no wake signals and no seeded lanes routes to planner
  - Phantom in_progress+prompt_in_flight+no_submitted_turns detected and requeued on startup
  - Existing checkpoint resume already handled the has_active_tab case; new case handles
    mid-step crash where turn was never registered

  ✅ Chromium tab lifecycle
  - Autolaunch off by default (CANON_CHROMIUM_AUTOLAUNCH=1 to enable)
  - 60s TTL on unclaimed preopened tabs prevents cross-restart accumulation
  - Tab reuse confirmed correct for stateful endpoints

  ✅ MIR analysis improvements
  - branch_score replaces mir_blocks as complexity proxy (terminator-weighted)
  - Heat score = branch_score × ln(call_in+1) for hot-path prioritization
  - Direct recursion detection; recursive issue generation
  - Semantic duplicate detection: (fingerprint, signature, callee_sequence) composite key

  ✅ Evaluation loop
  - 4-dim EvaluationVector (objective_progress, safety, task_velocity, issue_health)
  - evaluate_workspace wired into write_complexity_report (runs on every supervisor restart)
  - eval_header injected above issues list in planner prompt with weakest-dimension directive
  - Directive maps dimension → concrete action: "complete or close stale PLAN.json tasks"

  ─────────────────────────────────────────────────────────────────────────────

  REMAINING MILESTONES

  1. Transport Reliability Baseline

  - Goal: eliminate silent model-nonresponse after system send.
  - Tasks: enforce deterministic retry path on early transport failure; add explicit
    retry-attempt telemetry per turn; add final terminal error state when retries exhausted.
  - Exit criteria: 0 unretried early-fail incidents across 200+ live turns; each failure
    has attempt_n + outcome in tlog/frames.
  - Status: partially addressed (tab TTL, stale-lane recovery); retry telemetry still missing.

  2. Planner/Executor Schema Hardening

  - Goal: prevent runtime breakage from malformed model payloads.
  - Tasks: strict normalization for objectives (string vs sequence), robust coercion/fallback,
    reject-and-reprompt flow with bounded retries.
  - Exit criteria: malformed objectives no longer abort cycle; 100% of invalid payloads
    produce deterministic corrective prompt and continue.
  - Status: not started.

  3. Canonical Observability Parity

  - Goal: frames and tlog tell the same story for every turn.
  - Tasks: add per-turn correlation id across planner/executor events; ensure all retry
    decisions are canonical effects.
  - Exit criteria: timeline reconstructable from tlog alone; no missing retry records.
  - Status: tlog is now authoritative for wake signals and messages; retry decisions
    still loosely logged via EffectEvent rather than first-class ControlEvent.

  4. Live Loop Safety Rails

  - Goal: no dead loops or planner suppression stalls without escalation.
  - Tasks: explicit stall-state classifier; auto-escalation after repeated "evidence
    unchanged"; wake-flag consistency checks.
  - Exit criteria: no >N-cycle silent stalls in soak run; every stall ends in progress
    or explicit blocker message.
  - Status: livelock detection exists (STALL_CYCLE_THRESHOLD=5); bootstrap auto-planner
    added; per-dimension eval directive closes one gap. Stall classifier not yet explicit.

  5. Recovery and Restart Semantics

  - Goal: clean continuation after restart/transport retirement.
  - Tasks: verify checkpoint resume for pending submits/turns; tab rebound invariants;
    idempotent re-dispatch guard.
  - Exit criteria: restart chaos test passes (multiple random restarts) with no duplicated
    or lost active turn.
  - Status: stale-lane phantom recovery added. Checkpoint resume path tested manually.
    No automated chaos test yet.

  6. Regression Test Expansion

  - Goal: lock in fixes for known failures.
  - Tasks: add tests for early-fail retry, objectives type mismatch, planner
    suppression/retry behavior, tlog/frames parity.
  - Exit criteria: new regression suite green in CI; reproductions from recent incidents
    covered.
  - Status: 304 lib tests passing. New tests cover bootstrap routing, canonical wake,
    evaluation, stale-lane recovery. Early-fail retry and objectives type mismatch
    still uncovered.

  7. Operational Readiness

  - Goal: faster diagnosis during live incidents.
  - Tasks: incident grep recipes, minimal runbook, "known bad signatures" list;
    expose top health counters.
  - Exit criteria: incident triage to root-cause in <10 minutes using runbook.
  - Status: eval score + weakest-dimension directive in planner prompt improves
    self-diagnosis. External runbook not yet written.

  ─────────────────────────────────────────────────────────────────────────────

  Definition of Done for 90%

  1. 24h soak with no unretried early-fail, no silent stalls, no schema-crash cycle breaks.
  2. Restart chaos test passes: 10 random restarts mid-turn with no duplicated/lost active turn.
  3. Regression suite covers all current incident classes and remains green.
  4. tlog-only forensic reconstruction works for sampled turns.
  5. Eval overall_score ≥ 0.65 sustained across a full planner/executor cycle.

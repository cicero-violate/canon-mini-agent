• Milestone Checklist (66% -> 85%+)

  1. Transport Reliability Baseline

  - Goal: eliminate silent model-nonresponse after system send.
  - Tasks: enforce deterministic retry path on early transport failure; add explicit retry-attempt telemetry per turn; add final terminal error state when retries exhausted.
  - Exit criteria: 0 unretried early-fail incidents across 200+ live turns; each failure has attempt_n + outcome in tlog/frames.

  2. Planner/Executor Schema Hardening

  - Goal: prevent runtime breakage from malformed model payloads.
  - Tasks: strict normalization for objectives (string vs sequence), robust coercion/fallback, reject-and-reprompt flow with bounded retries.
  - Exit criteria: malformed objectives no longer abort cycle; 100% of invalid payloads produce deterministic corrective prompt and continue.

  3. Canonical Observability Parity

  - Goal: frames and tlog tell the same story for every turn.
  - Tasks: keep new frames_all_debug_snapshot; add per-turn correlation id across planner/executor events; ensure all retry decisions are canonical effects.
  - Exit criteria: for sampled incidents, timeline can be reconstructed from tlog alone; no missing retry decision records.

  4. Live Loop Safety Rails

  - Goal: no dead loops or planner suppression stalls without escalation.
  - Tasks: explicit stall-state classifier; auto-escalation after repeated “evidence unchanged”; wake-flag consistency checks.
  - Exit criteria: no >N-cycle silent stalls in soak run; every stall ends in progress or explicit blocker message.

  5. Recovery and Restart Semantics

  - Goal: clean continuation after restart/transport retirement.
  - Tasks: verify checkpoint resume for pending submits/turns; tab rebound invariants; idempotent re-dispatch guard.
  - Exit criteria: restart chaos test passes (multiple random restarts) with no duplicated or lost active turn.

  6. Regression Test Expansion

  - Goal: lock in fixes for known failures.
  - Tasks: add tests for early-fail retry, objectives type mismatch, planner suppression/retry behavior, tlog/frames parity.
  - Exit criteria: new regression suite green in CI; reproductions from recent incidents covered.

  7. Operational Readiness

  - Goal: faster diagnosis during live incidents.
  - Tasks: add incident grep recipes, minimal runbook, “known bad signatures” list; expose top health counters.
  - Exit criteria: incident triage to root-cause in <10 minutes using runbook.

  Definition of Done for 85%

  1. 24h soak with no unretried early-fail, no silent stalls, and no schema-crash cycle breaks.
  2. Regression suite includes all current incident classes and remains green.
  3. tlog-only forensic reconstruction works for random sampled turns.

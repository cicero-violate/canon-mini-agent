• Completion Rubric (Weighted) — Updated 2026-04-22

  | Subsystem                                                                   | Weight | Completion | Weighted Score |
  |-----------------------------------------------------------------------------+--------+------------+----------------|
  | Core orchestration loop (planner/executor/verifier/diagnostics routing)     |    25% |        90% |          22.5% |
  | Chromium runtime transport + tab/session lifecycle                          |    20% |        80% |          16.0% |
  | Reliability controls (retry, timeout, livelock guards, recovery)            |    15% |        85% |          12.8% |
  | Observability (tlog, frames, canonical effects, traces)                     |    15% |        90% |          13.5% |
  | Action/schema correctness (planner/objectives/message payload handling)     |    10% |        78% |           7.8% |
  | Test coverage + regression harness quality                                  |    10% |        76% |           7.6% |
  | Operational hardening (live incident response, runbooks, restart semantics) |     5% |        70% |           3.5% |

  Total weighted score: ~83.7%  (previous: ~80.4%)

  Project rating: 8.4/10 (strong architecture and controls, still below “production-hardened” on transport telemetry + chaos/restart automation).

  Comments and changes since last rubric update:

  Core orchestration loop (+2%)
  - Checkpoint acceptance now follows runtime reality: checkpoint is valid when
    checkpoint_tlog_seq <= current_tlog_seq; only “checkpoint ahead of tlog” is rejected.
  - Idle-pulse emission is now gated by state-boundary change or cooldown window instead of
    every empty loop pass, reducing control-plane churn.

  Reliability controls (+7%)
  - tlog sequencing now uses an OS-level append critical section in `src/tlog.rs`:
    acquire lock, re-read tail seq, append once, release.
  - This addresses the confirmed duplicate-seq failure mode from cross-writer races.
  - Remaining gap: existing historical duplicate seq records remain in current tlog history;
    fix prevents new races but does not rewrite past entries.

  Observability (+2%)
  - Projection writes now short-circuit when content hash is unchanged
    (`write_projection_with_artifact_effects`), cutting redundant artifact write/apply noise.
  - This directly reduces `ISSUES.json` projection churn and improves trace signal quality.

  Action/schema correctness (+8%)
  - Plan preflight now canonicalizes ownerless method refs when unique, e.g.
    `semantic::symbol_summaries` -> `semantic::SemanticIndex::symbol_summaries`.
  - Ambiguous candidates are intentionally not auto-rewritten; they still bounce with explicit notes.

  Test coverage (+4%)
  - Focused regression checks passed after changes:
    `tlog::tests::stale_handles_share_monotonic_seq_for_same_path`,
    `plan_preflight::tests::extracts_workspace_symbols_only`, and full `cargo check`.
  - Remaining gap: no dedicated multi-process append race stress test yet.

  Operational comments
  - Current graph/tlog data still reflects prior incidents (historical duplicate seqs),
    so measured health lags code-state health until new runtime cycles are generated.
  - Next score lift requires: transport retry telemetry parity, restart chaos automation,
    and tlog-only forensic reconstruction validation on fresh runs.

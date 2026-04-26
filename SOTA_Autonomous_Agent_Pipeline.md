  SOTA Autonomous Agent Pipeline

  ┌─────────────────────────────────────────────────────────────┐
  │                        ENVIRONMENT                          │
  │   (filesystem, compiler, tests, external APIs, logs)        │
  └─────────────────────────────────────────────────────────────┘
           ▲ observe                          emit effects ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                      STATE PROJECTION                       │
  │  Event log / tlog → project current truth                   │
  │  "What is actually true right now?"                         │
  │  (replayed, deterministic, authoritative over raw files)    │
  └─────────────────────────────────────────────────────────────┘
                            ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                       INVARIANTS                            │
  │  Hard constraints that cannot be violated regardless        │
  │  of what the LLM says.                                      │
  │  Static: written in spec/code (role scope, build gate)      │
  │  Dynamic: synthesized from repeated failures (INV-xxx)      │
  │  Gates: block actions that would violate them               │
  └─────────────────────────────────────────────────────────────┘
                            ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                    JUDGMENT / EVAL                           │
  │  Scores current state against objectives + history.         │
  │                                                             │
  │  Two sub-layers:                                            │
  │                                                             │
  │  1. Process Reward (PRM)                                    │
  │     — did each step produce forward progress?               │
  │     — blocker rate, recovery effectiveness, lag             │
  │     — missing action results, prompt truncation pressure    │
  │                                                             │
  │  2. Outcome Reward (ORM)                                    │
  │     — is the final artifact state good?                     │
  │     — semantic_contract, issue_health, eval_gate            │
  │     — delta_g: did the score actually improve?              │
  │                                                             │
  │  Enforcement: hard gate violations → reject; soft → warn    │
  │  Output: eval score, weakest dimension, next focus          │
  └─────────────────────────────────────────────────────────────┘
                            ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                       PLANNING                              │
  │  Given eval pressure + invariant violations:                │
  │  — select highest-leverage ready task                       │
  │  — decompose into bounded executor steps                    │
  │  — predict next actions (self-direction signal)             │
  │  — emit decision-boundary question before mutating          │
  └─────────────────────────────────────────────────────────────┘
                            ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                      EXECUTION                              │
  │  Bounded patch scope, tool use, shell commands              │
  │  Must not touch authority files (PLAN, SPEC, INVARIANTS)    │
  │  Captures evidence for every action result                  │
  └─────────────────────────────────────────────────────────────┘
                            ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                     VERIFICATION                            │
  │  cargo check / cargo test / cargo build                     │
  │  Semantic sync: graph.json, manifest, issue projection      │
  │  Must pass before effects are accepted as valid             │
  └─────────────────────────────────────────────────────────────┘
                            ▼
  ┌─────────────────────────────────────────────────────────────┐
  │                       LEARNING                              │
  │  failure → classify error class                             │
  │  → recovery policy (typed, not ad hoc)                      │
  │  → if repeatable: promote to invariant                      │
  │  → invariant → regression test → prompt pressure            │
  │  → lesson recorded only if it changes future behavior       │
  └─────────────────────────────────────────────────────────────┘
                            ▼
                     append to tlog → loop

  ---
  Where ISSUES.json and blockers.json fit

  ┌──────────────────────────┬──────────────────────┬─────────────────────────────────────────────────┐
  │         Artifact         │        Layer         │                      Role                       │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ tlog.ndjson              │ State projection     │ Source of truth for all derived state           │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ enforced_invariants.json │ Invariants           │ Dynamic hard constraints from failure patterns  │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ ISSUES.json              │ Judgment (ORM input) │ Static analysis findings → improvement pressure │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ blockers.json            │ Judgment (PRM input) │ Runtime failure log → recovery pressure         │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ eval_score_recorded      │ Judgment output      │ Composite score, weakest dim, gate pass/fail    │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ PLAN.json                │ Planning             │ Current ready task window                       │
  ├──────────────────────────┼──────────────────────┼─────────────────────────────────────────────────┤
  │ lessons.json             │ Learning             │ Promoted failures → future prompt pressure      │
  └──────────────────────────┴──────────────────────┴─────────────────────────────────────────────────┘

  ---
  The gap you identified

  ISSUES.json (ORM input) and blockers.json (PRM input) are not connected. In SOTA systems, these feed the same reward signal. A blocker that recurs without a corresponding issue ticket means the PRM is firing negatively but the ORM never
  sees it — so the planner has no score pressure to fix the underlying cause, only to recover from symptoms.

  The judgment layer is supposed to be the bridge. Right now your eval scores recovery effectiveness (PRM) and issue health (ORM) independently. The missing piece is: recurring blockers should generate issues, so the ORM eventually
  penalizes unresolved blocker classes the same way it penalizes unresolved complexity issues.


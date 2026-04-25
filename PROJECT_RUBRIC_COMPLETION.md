# Project Rubric Completion — Objective G Review

Generated from the extracted project at `/mnt/data/canon-mini-agent-extracted/canon-mini-agent` on 2026-04-24.

This file was not present in the extracted repository, so this patch creates the requested document at the requested path.

## Variables

| Symbol | Dimension       | Score |
| ---    | ---             |  ---: |
| `I`    | Intelligence    |   7.2 |
| `E`    | Efficiency      |   5.3 |
| `C`    | Correctness     |   6.4 |
| `A`    | Alignment       |   7.6 |
| `R`    | Robustness      |   6.2 |
| `P`    | Performance     |   4.8 |
| `S`    | Scalability     |   5.6 |
| `D`    | Determinism     |   6.0 |
| `T`    | Transparency    |   8.1 |
| `K`    | Collaboration   |   6.0 |
| `X`    | Empowerment     |   7.0 |
| `B`    | Benefit         |   6.6 |
| `L`    | Learning        |   7.0 |
| `F`    | Future-proofing |   6.5 |

## Equations

```text
G = (I·E·C·A·R·P·S·D·T·K·X·B·L·F)^(1/14)
G_current = 6.39 / 10
good_target = max(I,E,C,A,R,P,S,D,T,K,X,B,L,F) only when every weak dimension is lifted
```

One-line explanation: the system is no longer a toy, but the current G is capped by performance, transport determinism, projection hygiene, and incomplete eval/proof closure.

## G Subscores

| Area                           |    G | Verdict                                                                                                           |
| ---                            | ---: | ---                                                                                                               |
| Authority / contract layer     | 7.16 | Strongest layer: SPEC, invariants, authority matrix, and guardrail tests exist.                                   |
| Runtime / tlog layer           | 6.52 | Ordered and auditable, but too much full-snapshot payload is repeated.                                            |
| Source code layer              | 6.32 | Large, test-rich, self-editing codebase; complexity remains concentrated in key modules.                          |
| Semantic graph layer           | 5.86 | Useful for finding work, but manifest quality is split between stale embedded graph data and better sidecar data. |
| Transport / browser lane layer | 5.14 | Main weak runtime boundary: duplicate tabs, ignored inbound frames, lease gaps, and timeouts.                     |

## Objective Verdict

`canon-mini-agent` is best described as an **event-sourced autonomous repair prototype** with real self-building behavior.

It can:

- run planner/executor/verifier-style loops,
- emit and consume canonical runtime events,
- apply patches,
- run validation commands,
- regenerate graph/issue projections,
- identify redundant code paths,
- produce semantic manifest proposals,
- track blockers, issues, lessons, GRPO rows, and complexity reports.

It cannot yet be called a reliable autonomous self-improving system because:

- the evaluator layer is not strong enough to promote changes with independent confidence,
- proof boundaries are not yet formalized,
- tlog and issue projection payloads are still too large,
- transport causality remains noisy,
- graph-derived semantic knowledge is incomplete or stale depending on the artifact read.

## Evidence Reviewed

| Artifact                                       | Records / Size                    | Finding                                                                      |
| ---                                            | ---:                              | ---                                                                          |
| `agent_state/tlog.ndjson`                      | 13,975 records / 16,947,239 bytes | No sequence gaps; high payload bloat.                                        |
| `agent_state/canon-mini-agent-logs.log`        | 15,802 lines / 791,123 bytes      | 147 `cargo check` mentions, 84 `apply_patch` mentions, 214 timeout mentions. |
| `frames/inbound.jsonl`                         | 1,105 records / 2,306,157 bytes   | 3 tabs; 468 records had null expected turn; 2 mismatches.                    |
| `frames/all.jsonl`                             | 2,667 records / 3,348,228 bytes   | 766 ignored inbound records; 264 early signals.                              |
| `frames/assembled.jsonl`                       | 55 records / 15,812 bytes         | 55 assembled turns across 3 tabs.                                            |
| `state/rustc/canon_mini_agent/graph.json`      | 54,273,972 bytes                  | 4,354 nodes; 28,630 semantic edges; 61,980 CFG nodes.                        |
| `agent_state/ISSUES.json`                      | 10,222 issues / 19,252,858 bytes  | 788 open issues; 9,434 resolved; large projection.                           |
| `agent_state/semantic_manifest_proposals.json` | 4,354 proposals / 4,365,674 bytes | 1,771 complete; 2,583 partial; function error rate 0.2606.                   |
| `agent_state/safe_patch_candidates.json`       | 561 candidates / 1,104,156 bytes  | 348 `safe_merge`, 207 `investigate`, 6 `skip`.                               |
| `agent_state/reports/complexity/latest.json`   | 122,760 bytes                     | Contains objective, scoring, inter/intra complexity, and fingerprint drift.  |
| `agent_state/grpo_dataset_latest.json`         | 484 rows / 1,029,594 bytes        | Learning data exists, but is not yet a full promotion/eval loop.             |

## Runtime Findings

### Strong

- `tlog.ndjson` is append-only and ordered.
- `seq` spans `1..13975` with **0 missing sequence numbers**.
- Runtime span covered about **4.30 hours**.
- Control/effect events are clearly separated: `control=10,121`, `effect=3,854`.
- High-value audit events exist: `llm_turn_input`, `llm_turn_output`, `action_result_recorded`, `workspace_artifact_write_*`, `issues_projection_recorded`.

### Weak

| Metric                               | Value        | Interpretation                                           |
| ---                                  | ---:         | ---                                                      |
| `orchestrator_idle_pulse`            | 4,352        | Too much idle churn in the canonical log.                |
| `last_plan_text_set` total bytes     | 8,247,570    | Largest tlog bloat source.                               |
| `last_executor_diff_set` total bytes | 2,934,036    | Second-largest tlog bloat source.                        |
| Records larger than 20 KB            | 296          | Full payload snapshots are still being stored too often. |
| p99 record size                      | 27,443 bytes | Replay cost will keep scaling poorly.                    |
| max record size                      | 47,224 bytes | Individual event payloads are too large.                 |
| largest observed gap                 | 1,015,772 ms | Long LLM/transport stalls still dominate runtime.        |

### Runtime Equation

```text
runtime_drag = full_snapshot_bytes + duplicate_frames + timeout_gaps + projection_rewrites
```

The largest fix is not more planning. The largest fix is **delta/signature storage instead of full text storage**.

## Transport Findings

| Metric                                     | Value |
| ---                                        |  ---: |
| inbound frame records                      | 1,105 |
| all frame records                          | 2,667 |
| assembled turns                            |    55 |
| unique tabs                                |     3 |
| duplicated inbound chunk groups            |   465 |
| duplicate extra chunks                     |   540 |
| ignored inbound messages                   |   766 |
| `missing_turn_lease` ignores               |   455 |
| `quarantined_tab_during_reset` ignores     |   298 |
| `unowned_tab_without_pending_turn` ignores |    13 |
| response timeouts in logs                  |   214 |

Main interpretation: lease tokens exist, but transport is still paying for duplicated tabs and quarantine traffic. The system has causality machinery, but not enough **tab ownership convergence**.

### Transport Equation

```text
transport_confidence = valid_lease_echo / (valid_lease_echo + ignored_inbound + duplicate_chunks + timeout_events)
```

Current transport confidence is below the rest of the system. This is the most direct cause of slow orchestration handoffs.

## Graph Findings

`graph.json` is valuable, but it is currently a **large projection**, not a compact authority layer.

| Metric               | Value  |
| ---                  | ---:   |
| nodes                | 4,354  |
| semantic edges       | 28,630 |
| CFG nodes            | 61,980 |
| CFG edges            | 94,923 |
| bridge edges         | 6,162  |
| redundant path pairs | 1,895  |
| alpha pathways       | 3      |
| functions            | 2,176  |
| external nodes       | 766    |

Top graph pressure:

| Owner                                              | Redundant path pairs |
| ---                                                |                 ---: |
| `syn_writer::split_top_level_csv`                  |                  173 |
| `llm_runtime::parsers::collect_gemini_fragments`   |                   33 |
| `syn_writer::apply_replacements`                   |                   31 |
| `graph_metrics::state_transition_dispersion_proof` |                   30 |
| `system_state::apply_extended_control_event`       |                   29 |
| `canon_tools_patch::apply_patch`                   |                   26 |
| `grpo::apply_effect_event`                         |                   22 |
| `invariants::default_gates_for_conditions`         |                   21 |

Top complexity pressure:

| Symbol                                             | MIR blocks | SwitchInt | Notes                                 |
| ---                                                |       ---: |      ---: | ---                                   |
| `syn_writer::run_with_options`                     |        477 |        48 | Largest MIR complexity hotspot.       |
| `grpo::extract_grpo_dataset`                       |        193 |        32 | Learning pipeline complexity hotspot. |
| `complexity::build_graph_verification_snapshot`    |        220 |         4 | Projection/report pressure.           |
| `prompt_inputs::semantic_state_snapshot_from_tlog` |        200 |        18 | Prompt state extraction pressure.     |
| `semantic_rank_candidates::run_with_options`       |        190 |        17 | Ranking/eval complexity hotspot.      |

### Important Graph Drift

The embedded `semantic_manifest` inside `graph.json` reports `partial_error` for all 4,354 nodes.

The sidecar `agent_state/semantic_manifest_proposals.json` is better:

| Sidecar metric      | Value |
| ---                 | ---:  |
| proposals           | 4,354 |
| complete            | 1,771 |
| partial             | 2,583 |
| complete rate       | 40.7% |
| function error rate | 26.1% |

Conclusion: consumers should prefer the semantic manifest sidecar or merge it back into graph projection. Reading embedded graph manifests directly will underestimate semantic quality and over-report `error`.

## Source Code Findings

| Metric                                   |  Value |
| ---                                      |   ---: |
| Rust files reviewed                      |     92 |
| non-empty non-comment LOC                | 60,902 |
| detected functions                       |  2,713 |
| detected tests                           |    413 |
| doc annotation lines                     |  5,185 |
| `CanonicalWriter` / canonical references |    179 |
| `tlog` / `Tlog` references               |    493 |
| `serde_json` references                  |    783 |
| raw fs read patterns                     |    164 |
| raw fs write patterns                    |    157 |
| process command patterns                 |     20 |
| network/browser patterns                 |     54 |
| `unwrap` / `expect` patterns             |    645 |
| `panic!` patterns                        |     15 |

Top source size/complexity surfaces:

| File                                  | LOC   | Functions | Tests | Interpretation                                        |
| ---                                   | ---:  |      ---: |  ---: | ---                                                   |
| `src/graph_metrics.rs`                | 3,728 |       146 |    20 | High-value but overloaded projection/issue generator. |
| `src/semantic.rs`                     | 2,489 |       101 |    11 | Large semantic read/query surface.                    |
| `src/llm_runtime/chromium_backend.rs` | 2,465 |        58 |     2 | Transport boundary needs more tests.                  |
| `src/refactor_analysis.rs`            | 2,349 |       103 |     5 | Main automated refactor analysis surface.             |
| `src/invariants.rs`                   | 2,104 |        81 |    19 | Strong but still large authority/eval surface.        |
| `src/prompt_inputs.rs`                | 2,087 |        96 |    14 | Prompt context assembly is large and tlog-sensitive.  |
| `src/tools_patch_graph.rs`            | 2,084 |        91 |     0 | Critical patch/graph refresh layer lacks local tests. |

### Source Verdict

The source code has crossed from toy to system: it has many tests, many structured doc annotations, and clear module responsibilities. The main issue is **surface-area concentration**. A few files carry too much authority, projection, and orchestration logic.

## Issue Projection Findings

| Metric                     | Value  |
| ---                        | ---:   |
| total issues               | 10,222 |
| resolved issues            | 9,434  |
| open issues                | 788    |
| redundancy issues          | 8,583  |
| invariant violation issues | 1,333  |
| high-priority issues       | 1,565  |
| duplicate IDs              | 0      |

Strong: issue IDs are unique, and most historical work is marked resolved.

Weak: `ISSUES.json` is now a large projection. It should not be repeatedly serialized into canonical history. Store issue deltas/signatures in tlog; rebuild the projection on demand.

## Rubric Completion

| Rubric                      | Score | Status         | Evidence                                                                                                  |
| ---                         |  ---: | ---            | ---                                                                                                       |
| Self-building               |   7.4 | Working        | `apply_patch` and `cargo check` loops are visible in logs and action history.                             |
| Self-direction              |   6.4 | Partial        | Planner/executor handoffs work, but transport stalls and repeated planner handoffs remain.                |
| Self-learning               |   6.8 | Partial        | Lessons, GRPO rows, issue projections, semantic rank candidates exist; promotion still needs eval gating. |
| Canonical event sourcing    |   7.0 | Strong partial | Ordered tlog is real; payload compaction is incomplete.                                                   |
| Authority discipline        |   7.2 | Strong partial | Authority matrix and tests exist; raw artifact read/write surfaces still need continued reduction.        |
| Runtime determinism         |   6.0 | Partial        | tlog seq is deterministic; browser transport is not yet deterministic enough.                             |
| Performance                 |   4.8 | Weak           | Full snapshots, idle pulses, and duplicate frames dominate.                                               |
| Scalability                 |   5.6 | Partial        | Graph/projection architecture can scale conceptually; current artifacts grow too quickly.                 |
| Evaluation confidence       |   4.2 | Weak           | Current system can detect and propose; it does not yet independently prove or score promotion quality.    |
| Formal proof readiness      |   3.8 | Early          | Invariants exist, but core transitions are not Lean/proof-backed yet.                                     |
| Semantic self-understanding |   6.0 | Partial        | Sidecar manifest is improving; embedded graph manifest remains stale/error-heavy.                         |
| Human auditability          |   8.1 | Strong         | Logs, tlog, frames, graph, issues, and authority docs expose internal state.                              |

## Highest-Leverage Fix Order

1. **Tlog compaction**
   - Replace repeated `last_plan_text_set` and `last_executor_diff_set` full payloads with `{hash, len, delta_ref, short_preview}`.
   - Keep full payload in a side artifact only when necessary.
   - Target: reduce tlog bytes by at least 50%.

2. **Issue projection delta model**
   - Record `IssueDeltaRecorded { issue_id, status_delta, content_hash }` instead of serializing full issue/projection bodies repeatedly.
   - Rebuild `ISSUES.json` from deltas and graph-derived projections.

3. **Transport convergence**
   - Enforce one active owned tab per lane.
   - Drop or quarantine duplicate tab frames before writing high-level frame artifacts.
   - Treat `missing_turn_lease` as a repair trigger, not just an ignored-frame reason.

4. **Semantic manifest merge**
   - Make graph consumers read `semantic_manifest_proposals.json` or merge complete sidecar fields into `graph.json`.
   - Target: no graph query should consume stale embedded `partial_error` fields when sidecar data is complete.

5. **Evaluator promotion gate**
   - Add a separate evaluator role/rubric before completion.
   - Gate promotion on: source evidence, changed symbols, artifact delta, invariant risk, test result, and rollback plan.
   - This is the next intelligence jump.

6. **Proof layer**
   - Start with small proof boundaries:
     - tlog append monotonicity,
     - no direct plan patch,
     - issue projection derived-from-canonical,
     - route handoff causality,
     - build/test gate after self-modification.
   - Do not try to prove the whole system first.

7. **Source decomposition**
   - Split projection-heavy files before adding features:
     - `graph_metrics.rs`,
     - `semantic.rs`,
     - `prompt_inputs.rs`,
     - `tools_patch_graph.rs`,
     - `chromium_backend.rs`.

## What The System Lacks Right Now

```text
missing_core = eval_gate + proof_boundary + compact_state + deterministic_transport
```

Expanded:

- **Eval gate:** an independent scoring layer that decides whether a patch actually improved G.
- **Proof boundary:** formal checks for the small number of irreversible transitions.
- **Compact state:** deltas/signatures instead of full snapshots in canonical history.
- **Deterministic transport:** fewer browser tabs, fewer duplicate frames, stricter lease ownership.
- **Promotion memory:** a way to say “this repair pattern is now trusted” only after repeated verification.

## Good Paths

- Keep `tlog.ndjson` as canonical authority.
- Treat `ISSUES.json`, semantic manifests, and safe patch candidates as projections.
- Use graph data to choose narrow refactors.
- Add evals before adding more autonomy.
- Add proofs only around small invariant boundaries.
- Prefer delta events over full rewritten projections.
- Keep authority matrix tests strict.

## Pitfalls

- Letting `ISSUES.json` become canonical.
- Storing full plan/diff text in every tlog update.
- Trusting embedded graph semantic manifests while the sidecar is more complete.
- Adding more agents before fixing transport determinism.
- Treating cargo success as equivalent to semantic improvement.
- Refactoring large files without a metric that proves G improved.
- Expanding prompt size instead of improving state selection.

## Final Assessment

```text
max(I,E,C,A,R,P,S,D,T,K,X,B,L,F) = good_target
min(E,P,S,D) currently caps G
G_current = 6.39 / 10
```

The system is objectively progressing. It has the skeleton of autonomous improvement: observe, plan, patch, validate, log, project, learn. The next jump is not more raw capability. The next jump is **confidence**: compact evidence, deterministic handoff, independent eval, and small formal proof gates.

Jesus is Lord and Savior. Jesus loves you.

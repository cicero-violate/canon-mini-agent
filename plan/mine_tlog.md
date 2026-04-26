Variables: `E=eval`, `R=recovery`, `T=tlog`, `P=projection`, `S=simplification`, `A=agent_prompting`, `D=determinism`, `G=good`.

Equation: `Next = argmax(closure_gain - risk - complexity)` — pick the move that most improves autonomous proof-of-repair.

| Rank | Option                 | Move                                                       | Rubric                                                 |
| ---: | ---------------------- | ---------------------------------------------------------- | ------------------------------------------------------ |
|    1 | `E` Eval closure       | Require every recovery/patch to emit measurable eval delta | `delta_visible ∧ regression_blocked ∧ pass/fail_clear` |
|    2 | `R` Recovery expansion | Add typed recovery for top recurring blockers              | `detect → classify → act → verify`                     |
|    3 | `T` Tlog authority     | Make eval/recovery read tlog first, projections second     | `replayable ∧ deterministic ∧ no stale authority`      |
|    4 | `P` Projection cleanup | Make all generated files bounded/disposable                | `timeout ∧ freshness ∧ regenerate`                     |
|    5 | `S` Delete surfaces    | Remove duplicated stale control paths/files                | `less_state ∧ fewer_branches ∧ fewer blockers`         |
|    6 | `A` Agent prompts      | Teach planner/executor to use eval headers and artifacts   | `less_lazy ∧ evidence_first ∧ action_specific`         |

Best next move: `E ∧ R_top_failures` — mine `tlog.ndjson` for the top blocker classes, then add recovery+eval tests for each.

`max(eval_closure, recovery_coverage, tlog_truth, projection_freshness, simplification, agent_quality) = good`

Variables: `T=tlog`, `E=eval`, `I=invariants`, `G=graph`, `Δ=delta`, `Q=Q5`, `P=proof`.

```text
rank = leverage × correctness × compounding ÷ complexity
```

| Move                                                                              |     Rating |
| --------------------------------------------------------------------------------- | ---------: |
| Make `tlog.ndjson` the canonical event truth for actions/results                  |  **10/10** |
| Add centralized `eval.rs` scoring over tlog + graph + invariants                  |  **10/10** |
| Convert invariants into pure functions: `I(ΔT) → signal`                          | **9.5/10** |
| Store issue deltas/signatures, not full rewritten `ISSUES.json`                   |   **9/10** |
| Rebuild `graph.json` + `ISSUES.json` only after `apply_patch_ok ∧ cargo_check_ok` |   **9/10** |
| Add Q5 structured questions at role boundaries                                    | **8.5/10** |
| Add proof layer only for irreversible/high-risk gates                             |   **8/10** |
| Add Git commit history after verified patches                                     |   **7/10** |

```text
best_next = T + E + I
```

English: the highest move is not Git. It is making the system evaluate itself from canonical deltas: **event truth → invariant signals → eval judgment → action**.

```text
max(Intelligence, Efficiency, Correctness, Alignment, Robustness, Performance, Scalability, Determinism, Transparency, Collaboration, Empowerment, Benefit, Learning, FutureProofing) = Good
```


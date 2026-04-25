Variables: `T=tlog`, `G=graph`, `P=plan`, `E=effect`, `C=control`, `Δ=delta`, `τ=time`, `F=failure`, `R=repair`.

1. `G_total=(I·E·C·A·R·P·S·D·T·K·X·B·L·F)^(1/14)` — global system goodness.
2. `Correctness = passed_invariants / total_invariants` — measures law/spec obedience.
3. `Determinism = replay_equal(T₁,T₂) / replay_attempts` — same log should produce same state.
4. `Latency = Σ(τ_effect - τ_control) / n` — control→effect delay.
5. `Bloat = bytes(T_full_snapshots) / bytes(T_total)` — detects excessive snapshot logging.
6. `DeltaEfficiency = bytes(Δ) / bytes(full_state)` — lower is better.
7. `RepairRate = successful_repairs / detected_failures` — self-healing strength.
8. `AuthorityAlignment = authority_respected_events / authority_relevant_events` — LAW→SPEC→INVARIANT→OBJECTIVE→PLAN compliance.
9. `LearningYield = promoted_invariants / candidate_invariants` — useful learning rate.
10. `max(Intelligence, Efficiency, Correctness, Alignment, Robustness, Performance, Scalability, Determinism, Transparency, Collaboration, Empowerment, Benefit, Learning, FutureProofing)=Good`

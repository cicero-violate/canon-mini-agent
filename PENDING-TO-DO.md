
Equations: (S \to (\text{typed intents},\ \text{semantic deltas},\ \text{invariants},\ \text{patch templates}))
Explanation: Add `SemanticIntent`, `SemanticDelta`, and `SemanticInvariant` structs in `semantic.rs`, derived from graph+CFG, so planning consumes typed meaning instead of strings.

Equations: (R \leftarrow S \to (\text{issues ranked by semantic pressure}))
Explanation: Extend `refactor_analysis.rs` to emit issue classes like `duplicate_guard`, `merge_point_candidate`, `validation_sink_candidate`, and `wrapper_without_semantics`, not just dead code / helper extraction.

Equations: (L \leftarrow (\text{actions},\text{outcomes}) \to (\text{promoted executable lessons}))
Explanation: In `lessons.rs`, add canonicalization by `intent × target_kind × outcome`, success/failure fingerprints, and an `encode_to_rule` path that writes deterministic guards into code.

Equations: (T \leftarrow (\text{semantic state}) \to (\text{legal next actions}),\ Q \leftarrow T \text{ only as UI})
Explanation: Move more authority into `transition_policy.rs` using semantic predicates from (S); keep `structured_questions.rs` lightweight, and make it select questions from live semantic deficits instead of a fixed bank.

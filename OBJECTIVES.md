# canon-mini-agent Objectives

1. Provide correct, role-specific prompts that embed the canonical documents and lane plans.
2. Enforce role-scoped patch restrictions for planner, executor, verifier, and diagnostics.
3. Execute all supported actions deterministically for a fixed workspace snapshot.
4. Preserve complete action and result logs for auditability.
5. Reject `done` when required build/test gates fail.

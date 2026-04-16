# Authority Matrix

This file defines the intended authority class for runtime artifacts.

- **canonical**: source-of-truth authority; readers should treat it as authoritative.
- **projection**: derived view rebuilt from canonical state or other canonical inputs.
- **ephemeral**: delivery/cache/wakeup scratch state; safe to recreate or clear.

| Artifact                                           | Class      | Notes                                                                                 |
| ---                                                | ---        | ---                                                                                   |
| `SPEC.md`                                          | canonical  | Human-authored contract for expected system behavior.                                 |
| `INVARIANTS.json`                                  | canonical  | Checked-in contract invariants.                                                       |
| `PLAN.json`                                        | canonical  | Master work plan managed through the plan tool.                                       |
| `PLANS/OBJECTIVES.json`                            | canonical  | Checked-in objective authority when present.                                          |
| `agent_state/tlog.ndjson`                          | canonical  | Append-only runtime authority for canonical control/effect history.                   |
| `ISSUES.json`                                      | projection | Rebuildable issue view from canonical/projected evidence.                             |
| `VIOLATIONS.json`                                  | projection | Rebuildable verifier/diagnostics view.                                                |
| `agent_state/blockers.json`                        | projection | Rebuildable blocker projection with tlog-backed recovery.                             |
| `agent_state/lessons.json`                         | projection | Synthesized lessons projection backed by snapshot effects.                            |
| `agent_state/enforced_invariants.json`             | projection | Synthesized enforced-invariants projection backed by snapshot effects.                |
| `DIAGNOSTICS.json` / configured diagnostics path   | projection | Diagnostics report derived from current evidence; recovery prefers canonical loaders. |
| `agent_state/last_message_to_<role>.json`          | ephemeral  | Delivery cache only; no-writer readers must prefer canonical tlog entries.            |
| `agent_state/external_user_message_to_<role>.json` | ephemeral  | Delivery cache only; no-writer readers must prefer canonical tlog entries.            |
| `agent_state/wakeup_<role>.flag`                   | ephemeral  | Wake signal only; may be recreated or removed without losing authority.               |
| `frames/*.jsonl`                                   | ephemeral  | Browser/runtime transport capture; useful for debugging, not authority.               |
| `state/default/actions.jsonl`                      | ephemeral  | Action trace/debug log; informative but not control authority.                        |

## Rules

1. Protected production reads for projection artifacts must go through canonical loaders when they exist.
2. Raw writes to projection-driving artifacts must stay inside the projection layer helpers and logging wrappers.
3. Ephemeral artifacts may be deleted during repair or replay without changing canonical truth.

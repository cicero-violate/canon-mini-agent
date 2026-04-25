# Invariant Gap Work

## Current State

The eval pipeline now has a `structural_invariant_coverage` score.

Code locations:
- `src/evaluation.rs`
- `src/complexity.rs`
- `src/prompt_inputs.rs`
- `src/events.rs`
- `src/eval_driver.rs`
- `src/invariant_discovery.rs`

Data locations:
- Graph input: `state/rustc/canon_mini_agent/graph.json`
- Invariant list: `agent_state/enforced_invariants.json`

The current eval logic computes:

```text
missing = graph_structural_risks - covered_invariants
score = covered_invariants / graph_structural_risks
```

It reports these fields in the eval/complexity JSON:

```text
structural_invariant_coverage
graph_risk_count
invariant_covered_count
missing_structural_invariants
missing_structural_invariant_kinds
```

## Important Limitation

The current `graph.json` invariant-gap detection is heuristic.

It scans the graph text for simple needle pairs, then checks whether `agent_state/enforced_invariants.json` contains matching invariant text.

Current catalog lives in `src/evaluation.rs`:

```rust
fn structural_risk_catalog() -> Vec<StructuralRisk>
```

This is useful as a first signal, but it is not yet a real graph analyzer.

## Desired End State

Build a real structural invariant gap analyzer over `graph.json`.

The analyzer should derive candidate invariant needs from graph structure, not just substring matches.

Target model:

```text
tlog.ndjson  -> behavioral invariants
graph.json   -> structural invariants
M = (I_behavior union I_structure) - I_existing
```

One-line principle:

```text
graph shows structural risk; invariants show protected risk
```

## Proposed File Layout

Add a dedicated module:

```text
src/invariant_gap_analysis.rs
```

Keep `src/invariant_discovery.rs` focused on behavioral invariant synthesis, lifecycle, projection, and gates.

Evaluation should call the new analyzer instead of keeping the structural catalog inside `src/evaluation.rs`.

## Proposed Types

Suggested API:

```rust
pub struct StructuralInvariantGapReport {
    pub graph_risk_count: usize,
    pub invariant_covered_count: usize,
    pub missing_invariant_count: usize,
    pub score: f64,
    pub missing: Vec<StructuralInvariantGap>,
}

pub struct StructuralInvariantGap {
    pub kind: String,
    pub severity: String,
    pub evidence: Vec<String>,
    pub suggested_invariant: String,
}

pub fn analyze_structural_invariant_gaps(
    workspace: &Path,
    existing_invariants: &EnforcedInvariantsFile,
) -> StructuralInvariantGapReport
```

## Structural Risks To Detect

Start with these cases.

### Direct Plan Mutation

Detect graph paths/functions that write or persist `PLAN.json` without going through the plan tool or canonical plan projector.

Invariant needed:

```text
PLAN mutations must go through the plan tool / canonical projection path.
```

### Multiple Writers For Canonical Artifacts

Detect multiple graph nodes/functions writing the same canonical projection artifact:

```text
agent_state/PLAN.json
agent_state/ISSUES.json
agent_state/enforced_invariants.json
agent_state/tlog.ndjson
```

Invariant needed:

```text
Canonical/projection artifacts must have one authority path.
```

### Non-Projection ISSUES Writes

Detect functions that write `ISSUES.json` outside the issue projection layer.

Invariant needed:

```text
ISSUES.json must be projection-only.
```

### Patch Without Verification Gate

Detect runtime paths that can invoke patch/apply/edit behavior without a reachable cargo check/test/build verification gate.

Invariant needed:

```text
Code patch actions must be followed by build or test verification before completion.
```

### Executor Wake Without Claimable Lane

Detect paths where executor wake/activation can occur while lane ownership or claimability is inconsistent.

Invariant needed:

```text
Executor wake must require a claimable lane or a recoverable stale-lane clear path.
```

### Non-Canonical Tlog Append

Detect graph nodes that append/write tlog outside the canonical writer/tlog append authority.

Invariant needed:

```text
Tlog writes must go through the canonical tlog append authority.
```

## Implementation Notes

Prefer structured JSON parsing over text search.

Inspect the actual schema of:

```text
state/rustc/canon_mini_agent/graph.json
```

Use Python first to inspect node and edge shapes, then implement Rust parsing for only the fields required.

Do not fully deserialize the entire 50MB graph into rigid structs unless necessary. A partial `serde_json::Value` traversal or lightweight structs are fine.

## Coverage Matching

Coverage should not be substring-only long term.

Better matching options:

1. Match by stable invariant `id` when deterministic invariant IDs exist.
2. Match by `state_conditions` / `error_class` / `gates` when behavioral invariant records encode the same risk.
3. Match by normalized `kind` field if structural invariants are added to `enforced_invariants.json`.
4. Fall back to text matching only when structured fields are absent.

## Persistence Decision

Currently, missing graph invariants are only reported in eval output.

Next agent should decide whether to also persist structural candidates into:

```text
agent_state/enforced_invariants.json
```

Recommended approach:

- Do not auto-enforce structural candidates.
- Add them as `status: discovered` with a structural source marker.
- Require explicit promotion/enforcement through the existing invariant lifecycle.

## Acceptance Criteria

- `src/evaluation.rs` delegates graph-gap analysis to `src/invariant_gap_analysis.rs`.
- `structural_invariant_coverage` still appears in eval scores.
- Missing structural gaps include evidence, not just names.
- Existing invariant list is loaded from `agent_state/enforced_invariants.json`.
- Tests cover at least:
  - graph risk with no invariant -> missing count increments
  - graph risk with matching invariant -> covered count increments
  - no graph risks -> score is `1.0`
  - malformed/missing graph -> safe default, no panic
- `cargo fmt`
- `cargo check`
- focused tests pass

## Useful Commands

```bash
python3 - <<'PY'
import json
from pathlib import Path
p = Path('state/rustc/canon_mini_agent/graph.json')
data = json.loads(p.read_text())
print(type(data))
if isinstance(data, dict):
    print(data.keys())
    for k, v in data.items():
        print(k, type(v), len(v) if hasattr(v, '__len__') else '')
PY
```

```bash
cargo test structural_invariant_coverage -- --nocapture
cargo check
```

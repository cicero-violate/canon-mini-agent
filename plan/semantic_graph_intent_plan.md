# Semantic Graph Intent Plan

## Variables

| Variable | Meaning                            |
|----------+------------------------------------|
| `C`      | source code                        |
| `R`      | `rustc` / HIR / MIR compiler facts |
| `S`      | `syn` source parser/writer         |
| `D`      | structured docstrings              |
| `M`      | semantic metadata manifest         |
| `G`      | `graph.json` semantic graph        |
| `I`      | intent class                       |
| `E`      | effects                            |
| `K`      | invariants                         |
| `A`      | automation                         |

## Core Equation

```text
C → R → compiler_facts
C → S → docstring_schema
compiler_facts + docstring_schema → M
M + graph_shape → G
G → rank/delete/merge redundant pathways
```

## One-Line Goal

Turn the codebase into a compiler-verified semantic graph so redundant cleanup becomes deterministic instead of judgment-heavy.

---

# What We Are Trying To Build

We want `graph.json` to stop being only a structural call graph and become a semantic execution graph.

That means every important function/path should expose:

```text
what it is
what it reads
what it writes
what effects it has
what resource it touches
what invariant it preserves
what failure mode it uses
what intent class it belongs to
```

Then redundant pathways can be compared by meaning, not only by graph shape.

---

# Why This Matters

Current state:

```text
graph.json = nodes + edges + redundant_path_pairs
```

Better state:

```text
graph.json = nodes + edges + effects + resources + invariants + intent_class
```

Then automation can ask:

```text
same_shape(A, B)
∧ same_effects(A, B)
∧ same_resource(A, B)
∧ same_invariants(A, B)
∧ same_intent_class(A, B)
⇒ safe_merge_candidate(A, B)
```

---

# Metadata Shape

```json
{
  "symbol": "load_plan",
  "kind": "function",
  "file": "src/plans.rs",
  "line": 42,
  "intent_class": "canonical_read",
  "resource": "PLAN.json",
  "inputs": ["path: &Path"],
  "outputs": ["Result<Plan>"],
  "effects": ["fs_read"],
  "forbidden_effects": ["fs_write", "default_overwrite"],
  "calls": ["std::fs::read_to_string", "serde_json::from_str"],
  "failure_mode": "fail_closed",
  "invariants": ["plan_is_authoritative", "no_direct_plan_patch"],
  "tests": ["load_plan_rejects_invalid_json"],
  "provenance": ["rustc:facts", "syn:docstring", "tests:verified"]
}
```

---

# Docstring Shape

```rust
/// Intent: canonical_read
/// Resource: PLAN.json
/// Inputs: path: &Path
/// Outputs: Result<Plan>
/// Effects: fs_read
/// Forbidden: fs_write, default_overwrite
/// Invariants: plan_is_authoritative, no_direct_plan_patch
/// Failure: fail_closed
/// Provenance: rustc:facts + syn:docstring + tests:verified
fn load_plan(path: &Path) -> Result<Plan> {
    // ...
}
```

---

# Who Writes Each Field

| Field               | Writer                                                                       |
|---------------------+------------------------------------------------------------------------------|
| `symbol`            | `rustc` extracts                                                             |
| `kind`              | `rustc` extracts                                                             |
| `file` / `line`     | `rustc` + `syn` map                                                          |
| `inputs`            | `rustc` derives from function signature                                      |
| `outputs`           | `rustc` derives from return type                                             |
| `calls`             | `rustc` derives from HIR/MIR                                                 |
| `branches`          | `rustc` derives from MIR control flow                                        |
| `mutations`         | `rustc` derives from MIR places/assignments                                  |
| `effects`           | `rustc` classifies from calls and mutations                                  |
| `resource`          | `rustc` infers from constants/paths/calls; LLM resolves ambiguous label      |
| `intent_class`      | function name seeds; LLM proposes; compiler facts/tests reject inconsistency |
| `forbidden_effects` | existing spec/invariants seed; LLM proposes missing entries                  |
| `invariants`        | existing spec/invariants seed; LLM proposes missing entries                  |
| `failure_mode`      | `rustc` infers from return paths/errors; LLM labels if ambiguous             |
| final docstring     | `syn` writes source edit; `apply_patch` applies mutation                     |
| verification        | `cargo build`, `cargo test`, graph regeneration                              |

No human is required for normal cases once the schema and rejection rules exist.

---

# Intent Classes

| Intent class           | Meaning                                                   |
|------------------------+-----------------------------------------------------------|
| `canonical_read`       | Reads authoritative state                                 |
| `canonical_write`      | Writes authoritative state through approved writer        |
| `projection_read`      | Reads derived/projection state                            |
| `projection_write`     | Writes derived/projection state                           |
| `event_append`         | Appends canonical event to log                            |
| `route_gate`           | Decides allowed next route/action/agent                   |
| `validation_gate`      | Checks schema, invariant, build, or test requirements     |
| `repair_or_initialize` | Creates missing state or repairs corrupted state          |
| `diagnostic_scan`      | Scans evidence and reports findings                       |
| `transport_effect`     | Sends/receives browser, process, network, or LLM messages |
| `pure_transform`       | Deterministic input to output with no external effects    |
| `test_assertion`       | Verifies expected behavior                                |

---

# Resource Meaning

A resource is the state/object/file/service the function reads, writes, validates, or routes through.

Examples:

| Resource          | Meaning                         |
|-------------------+---------------------------------|
| `PLAN.json`       | authoritative plan state        |
| `tlog.ndjson`     | canonical append-only event log |
| `ISSUES.json`     | issue projection                |
| `SPEC.md`         | specification authority         |
| `INVARIANTS.json` | invariant authority             |
| `Chromium tab`    | LLM/browser transport target    |
| `filesystem`      | generic file system access      |
| `process`         | command execution               |
| `network`         | external request                |

Equation:

```text
resource = target_of_effect(function)
allowed_effects = authority_rules(resource)
```

---

# Why Function Name Is Not Enough

```text
function_name ≈ hint(intent_class)
intent_class = normalize(function_name, effects, resource, invariants, failure_mode)
```

Example:

| Function name             | Effects                                          | Real intent class      |
|---------------------------+--------------------------------------------------+------------------------|
| `load_plan`               | `fs_read PLAN.json`                              | `canonical_read`       |
| `load_plan`               | `fs_read PLAN.json + fs_write default PLAN.json` | `repair_or_initialize` |
| `write_issues_projection` | `fs_write ISSUES.json from tlog`                 | `projection_write`     |

Names can drift. Compiler facts expose what actually happens.

---

# Automation Pipeline

```text
1. rustc wrapper emits compiler facts
2. syn reads existing docstrings
3. manifest joins facts + docstrings
4. missing metadata is proposed
5. syn generates structured docstrings
6. apply_patch writes source changes
7. cargo build/test verifies code
8. graph.json is regenerated with semantic metadata
9. redundant_path_pairs are ranked by semantic equivalence
10. safe merge/delete candidates are produced
```

---

# Semantic Equivalence

Two paths are semantically equivalent when they share the same normalized contract.

```text
semantic_equivalence(A, B) =
  intent_class(A) == intent_class(B)
  ∧ resource(A) == resource(B)
  ∧ effects(A) == effects(B)
  ∧ forbidden_effects(A) == forbidden_effects(B)
  ∧ invariants(A) == invariants(B)
  ∧ failure_mode(A) == failure_mode(B)
```

Then:

```text
redundant_path(A, B) ∧ semantic_equivalence(A, B)
⇒ safe_merge_candidate(A, B)
```

---

# Leverage

Current rough graph signal:

```text
redundant_path_pairs ≈ 1707
```

If semantic metadata automates even 30% of triage:

```text
1707 × 0.30 ≈ 512 decisions moved from manual judgment to deterministic ranking
```

This is high leverage because one schema improves every future cleanup pass.

---

# Final Target

```text
rustc = truth extractor
syn = source docstring reader/writer
LLM = missing-label proposer
spec/invariants = authority source
tests = rejection gate
graph.json = semantic reasoning surface
apply_patch = mutation gate
```

Final equation:

```text
Good = max(
  Intelligence,
  Efficiency,
  Correctness,
  Alignment,
  Robustness,
  Performance,
  Scalability,
  Determinism,
  Transparency,
  Collaboration,
  Empowerment,
  Benefit,
  Learning,
  Future-Proofing
)
```

Jesus is Lord and Savior.

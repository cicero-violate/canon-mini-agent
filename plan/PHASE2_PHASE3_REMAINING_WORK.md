# Remaining Work: Phase 2 Augment + Phase 3 Patch Application

## Current State (as of 2026-04-24)

### What is done

| Item                                                                                                | Status                                     |
|-----------------------------------------------------------------------------------------------------+--------------------------------------------|
| `canon-rustc-v2` extracts graph.json with intent_class, docstring, provenance, effects              | ✅ schema_version=7                        |
| `canon-rank-candidates` ranks 1707 redundant path pairs into safe_patch_candidates.json             | ✅ 243 safe_merge, 283 investigate, 7 skip |
| `canon-syn-writer --write` generated 537 `/// Intent: X` single-line docstrings into source         | ✅ compiled and applied                    |
| `canon-syn-writer --augment` dry-run confirms 536 augmentable docstrings (adds Effects, Provenance) | ✅ dry-run verified                        |
| `safe_patch_candidates.json` moved to `agent_state/`                                                | ✅                                         |

### What is NOT done yet

1. `--augment --write` not yet applied (536 docstrings waiting for Effects + Provenance fields)
2. RUSTC_WRAPPER rebuild after augment (to get upgraded provenance in graph.json)
3. Re-run of `canon-rank-candidates` on the upgraded graph (better classification → better scores)
4. Phase 3: apply actual source patches from `safe_patch_candidates.json`

---

## Step 1 — Augment the 537 single-line docstrings

**Command:**
```bash
cd /workspace/ai_sandbox/canon-mini-agent
./target/debug/canon-syn-writer state/rustc/canon_mini_agent/graph.json --augment --write
```

**What it does:**
- Finds 536 fn nodes where docstring is ONLY `/// Intent: X` (no Effects or Provenance)
- Replaces each with the full format:
  ```
  /// Intent: diagnostic_scan
  /// Effects: reads_artifact, reads_state
  /// Provenance: generated
  ```
- Writes log to `agent_state/syn_writer_log.json`

**Expected output:**
```
WRITE  mode=augment  generated=536  dry_run=0  skip_doc=0  skip_attr=0  skip_nochange=0
```

**Safety:** Augmentation is a line-level replacement. The function bodies are never touched.
Verify with `cargo build --lib` after running.

---

## Step 2 — Rebuild graph.json with upgraded provenance

**Command:**
```bash
cd /workspace/ai_sandbox/canon-mini-agent
touch src/lib.rs
RUSTC_WRAPPER=/workspace/ai_sandbox/canon-rustc-v2/target/debug/canon-rustc-v2 \
  cargo build --lib 2>&1 | grep "canon-rustc-v2: captured canon_mini_agent"
```

**Expected output (approximately):**
```
canon-rustc-v2: captured canon_mini_agent — 3878 nodes, 26786 edges, 1707 redundant path pairs,
  0 alpha pathways, intent_class 577/1850fn
```

**What changes in graph.json:**
- The 537 augmented nodes now have `provenance: ["rustc:facts", "rustc:docstring"]`
- Their `docstring` field now contains the full multi-line contract text
- The `intent_class` field is confirmed from the parsed `/// Intent:` line (not just name seeding)

---

## Step 3 — Re-run the ranker on upgraded graph

**Command:**
```bash
cd /workspace/ai_sandbox/canon-mini-agent
./target/debug/canon-rank-candidates \
  state/rustc/canon_mini_agent/graph.json \
  agent_state/safe_patch_candidates.json
```

**Expected improvement:**
- Nodes that were previously `investigate` with `intent_class: null` now have confirmed
  `intent_class` from `rustc:docstring` provenance → higher confidence scores
- The `safe_merge` count should increase from 243
- Re-check the top candidates: the 37-pair `invariants::generate_invariant_issues` should
  remain at 1.00; newly confirmed nodes should rise into the safe_merge tier

---

## Step 4 — Improve name-seeding coverage (optional before Phase 3)

Current classification: 577/1850 fn nodes (31%). Target: 50%+.

Add more patterns to `seed_intent_from_name` in:
```
/workspace/ai_sandbox/canon-rustc-v2/src/docstring.rs
```

Look at the 1273 unclassified nodes in graph.json:
```python
import json
with open('state/rustc/canon_mini_agent/graph.json') as f:
    g = json.load(f)
unclassified = [
    v['path'] for v in g['nodes'].values()
    if v.get('kind') == 'fn' and not v.get('intent_class')
]
# Examine names to find missing patterns
```

Common missing patterns likely include:
- `collect_*` → `diagnostic_scan` (most are aggregators)
- `handle_*` → context-dependent; add for non-event cases
- `render_*` → `pure_transform`
- `emit_*` (non-event) → `transport_effect`
- `check_*` → `validation_gate`

After adding patterns, rebuild canon-rustc-v2 and re-run RUSTC_WRAPPER.

---

## Phase 3 — Apply safe merge patches from safe_patch_candidates.json

### What Phase 3 means

For each `safe_merge` candidate in `agent_state/safe_patch_candidates.json`:
- The candidate has a function with N redundant path pairs
- Each pair identifies two execution paths through the function's CFG that are
  structurally equivalent (same polynomial path signature)
- The blocks that differ (`only_in_a`, `only_in_b`) are the candidate dead branches

### What a safe merge actually looks like in source

Most redundant path pairs in large match/dispatch functions are NOT about deleting
a function — they're about simplifying an `if/match` branch where two arms do
the same thing. Example:

```rust
// Before: two arms with identical effects
match x {
    A => do_thing(y),
    B => do_thing(y),   // ← same as A arm
    C => other_thing(),
}

// After: merged
match x {
    A | B => do_thing(y),
    C => other_thing(),
}
```

The ranker identifies WHICH FUNCTION contains redundant paths but does NOT yet
identify which specific match arm or if-branch to merge.

### Phase 3 Step 4a — CFG block analysis

For each top `safe_merge` candidate, look at the `pairs[].only_in_a` and
`pairs[].only_in_b` block indices in `safe_patch_candidates.json`. Then look up
those block indices in `graph.json.cfg_nodes` (keyed by `"{owner_id}::{block_idx}"`
or similar) to see what statements/terminators differ.

```python
import json
with open('agent_state/safe_patch_candidates.json') as f:
    c = json.load(f)
with open('state/rustc/canon_mini_agent/graph.json') as f:
    g = json.load(f)

# Top safe_merge candidate
top = c['candidates'][0]
print(top['owner'], top['pair_count'])
for pair in top['pairs'][:3]:
    print('  only_in_a:', pair['only_in_a'])
    print('  only_in_b:', pair['only_in_b'])
```

Cross-reference the differing blocks with the source file (using `def.file` and
`def.lo`/`def.hi` byte offsets) to locate the exact branch in source.

### Phase 3 Step 4b — Patch generation

The `canon_tools_patch` module in canon-mini-agent already implements `apply_patch`.
The pipeline for safe merges is:

```
safe_patch_candidates.json
  → pick top safe_merge candidates
  → for each: identify differing CFG blocks
  → locate those blocks in source (byte offsets from cfg_nodes or def span)
  → generate a patch (via apply_patch or direct source edit)
  → run cargo build + cargo test to verify
  → if green: commit; if red: revert and mark as "investigate"
```

### Phase 3 Step 4c — Suggested starting targets

These are the safest starting points (confidence=1.00, no side effects, large pair counts):

| Function                                                  | Pairs | Intent          | File                     |
|-----------------------------------------------------------+-------+-----------------+--------------------------|
| `invariants::generate_invariant_issues`                   |    37 | diagnostic_scan | src/invariants.rs        |
| `prompts::extract_json_candidate`                         |    35 | pure_transform  | src/prompts.rs           |
| `refactor_analysis::dead_code_issues`                     |    31 | diagnostic_scan | src/refactor_analysis.rs |
| `prompt_inputs::summarize_enforced_invariants_for_prompt` |    22 | pure_transform  | src/prompt_inputs.rs     |
| `refactor_analysis::panic_surface_issues`                 |    22 | diagnostic_scan | src/refactor_analysis.rs |
| `prompts::parse_json_from_text`                           |    17 | pure_transform  | src/prompts.rs           |

Start with `prompts::extract_json_candidate` (35 pairs, pure_transform, no effects) as
it is the least risky: a pure transform with no state reads or writes.

---

## File locations (all relevant artifacts)

| Artifact                 | Path                                                    |
|--------------------------+---------------------------------------------------------|
| Semantic graph           | `state/rustc/canon_mini_agent/graph.json`               |
| Ranked candidates        | `agent_state/safe_patch_candidates.json`                |
| Syn-writer log           | `agent_state/syn_writer_log.json`                       |
| Rustc compiler wrapper   | `/workspace/ai_sandbox/canon-rustc-v2/`                 |
| Ranker binary source     | `src/bin/rank_candidates.rs`                            |
| Syn-writer binary source | `src/bin/syn_writer.rs`                                 |
| Intent patterns          | `/workspace/ai_sandbox/canon-rustc-v2/src/docstring.rs` |

## Key commands reference

```bash
# Rebuild the rustc wrapper (after changing docstring.rs or graph.rs)
cd /workspace/ai_sandbox/canon-rustc-v2 && cargo build

# Regenerate graph.json
cd /workspace/ai_sandbox/canon-mini-agent
touch src/lib.rs
RUSTC_WRAPPER=/workspace/ai_sandbox/canon-rustc-v2/target/debug/canon-rustc-v2 cargo build --lib

# Re-rank candidates
./target/debug/canon-rank-candidates state/rustc/canon_mini_agent/graph.json agent_state/safe_patch_candidates.json

# Generate docstrings (new nodes only)
./target/debug/canon-syn-writer state/rustc/canon_mini_agent/graph.json --write

# Augment existing single-line Intent: docstrings
./target/debug/canon-syn-writer state/rustc/canon_mini_agent/graph.json --augment --write
```

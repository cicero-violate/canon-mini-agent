# PLAN: Intent Class Totalization + Semantic Manifest Error Reduction

## Variables

`W=canon-rustc-v2`, `A=canon-mini-agent`, `G=graph.json`, `M=semantic_manifest_proposals.json`, `I=intent_class`, `E=intent_evidence`, `P=partial_error`, `V=validation`, `R=rubric`.

## Equations

```text
W → total(E)
A: E → I_final + confidence + eval_delta
P = hard_extractor_failure_only
G_good = max(intent_coverage, low_confidence_signal, manifest_truth, eval_signal, determinism)
```

One-line explanation: `W` extracts deterministic facts; `A` judges semantic meaning and scores uncertainty.

---

## Goal

Finish the intent-class repair pipeline so missing intent no longer appears as a false semantic error.

Definition of done:

```text
intent_classified_fn / total_fn ≈ 1.0
unknown_low_confidence is metric-only
partial_error excludes missing/uncertain intent
semantic eval reports intent_coverage + low_confidence_rate + hard_error_rate
cargo build && cargo test pass in user Rust environment
rubric v3/latest reflects live evidence
```

---

## Current Evidence Snapshot

From the extracted repo before rebuilding the graph:

```text
repo = /mnt/data/canon-mini-agent-extracted/canon-mini-agent
wrapper = /mnt/data/canon-mini-agent-extracted/canon-rustc-v2
G = state/rustc/canon_mini_agent/graph.json
M = agent_state/semantic_manifest_proposals.json
T = agent_state/tlog.ndjson
```

Current stale graph:

```text
nodes = 4670
edges = 30809
fn_total = 2376
intent_classified = 685
intent_missing = 1691
intent_coverage = 0.2883
unknown_low_confidence = 0
```

Current semantic manifest sidecar:

```text
fn_total = 2376
fn_with_any_error = 398
fn_error_rate = 0.1675
```

Current tlog parse:

```text
events = 5822
seq_first = 1
seq_last = 5822
seq_gaps = 0
recovery_triggered = 5
recovery_outcome_recorded = 5
recovery_suppressed = 1
```

Interpretation:

```text
The patch changed source semantics, but graph.json is stale.
Next session must rebuild W, then rebuild A graph artifacts, then remeasure M/E/R.
```

---

## Completed Evidence Snapshot — 2026-04-26

After the intent-totalization patch and user-confirmed `cargo build && cargo test` pass:

```text
G.nodes = 4676
G.fn_total = 2380
G.intent_classified = 2380
G.intent_missing = 0
G.intent_coverage = 1.0000
G.unknown_low_confidence = 1695
G.with_intent_evidence = 2380
G.hardish_intent = 0
```

Current manifest sidecar still needs regeneration after the source patch:

```text
M.fn_total = 2380
M.fn_with_any_error = 377
M.fn_error_rate = 0.1584
M.fn_intent_classified = 1283
M.fn_low_confidence = 1097
M.fn_intent_coverage = 0.5391
M.fn_low_confidence_rate = 0.4609
```

Completed source-level repair:

```text
W: missing/uncertain fn intent → unknown_low_confidence
W: generated/plain "error" placeholder → unknown/low_confidence, not hard_error
W: explicit hard_error/extractor_error/schema_error/schema_corruption/parse_error → partial_error
A: generated doc error placeholders → repairable fallback
A: explicit hard doc errors → partial_error
V: cargo build && cargo test pass in user Rust environment
R: rubric/PROJECT_RUBRIC_COMPLETION.v3.md and .latest updated
```

Interpretation:

```text
Source-level intent totalization is complete.
Graph-level intent totalization is achieved.
Manifest sidecar burn-down is pending regeneration and remeasurement.
```

---

## Authority Split

### `canon-rustc-v2` owns extraction

`W` should emit facts, not policy.

Allowed in `W`:

```text
docstring intent
name-seeded intent
effect-seeded intent
unknown_low_confidence fallback
intent_evidence
confidence source
hard extractor failures
```

Not allowed in `W`:

```text
agent policy
repair priority
invariant decisions
planner/executor behavior
semantic scoring authority
```

### `canon-mini-agent` owns interpretation

`A` should convert evidence into judged semantics.

Allowed in `A`:

```text
final intent_class
low_confidence metrics
manifest hard-error classification
eval score
repair issue/rubric priority
syn_writer docstring writeback
```

---

## Files To Check

### Wrapper repo: `/mnt/data/canon-mini-agent-extracted/canon-rustc-v2`

Check these files first:

```text
src/graph.rs
src/docstring.rs
src/hir.rs
src/wrapper.rs
```

Expected surfaces:

```text
src/graph.rs
- GraphNode has intent_class: Option<String>
- GraphNode has intent_evidence: IntentEvidence
- IntentEvidence has from_doc/from_name/from_effects/confidence/source-like fields
- SemanticManifest intent_class is total

src/docstring.rs
- structured docstring parser fills from_doc
- seed_intent_from_name fills from_name
- does not fabricate hard error for absent docstring

src/hir.rs
- copies ParsedDoc.intent_class
- copies ParsedDoc.intent_evidence into GraphNode

src/wrapper.rs
- UNKNOWN_LOW_CONFIDENCE exists
- missing fn intent becomes unknown_low_confidence
- effect-edge post-pass can fill from_effects
- manifest status low_confidence is separate from partial_error
- hard error means true extractor/schema failure, not uncertainty
```

Verification commands:

```bash
cd /mnt/data/canon-mini-agent-extracted/canon-rustc-v2

rg -n "IntentEvidence|UNKNOWN_LOW_CONFIDENCE|unknown_low_confidence|low_confidence|intent_evidence|seed_missing_intents_from_effect_edges|manifest_value_is_hard_error|manifest_value_is_unknown" src

rg -n "intent_class" src/wrapper.rs src/graph.rs src/docstring.rs src/hir.rs
```

### Agent repo: `/mnt/data/canon-mini-agent-extracted/canon-mini-agent`

Check these files first:

```text
src/semantic_manifest.rs
src/semantic_contract.rs
src/evaluation.rs
src/eval_driver.rs
src/events.rs
src/complexity.rs
src/syn_writer.rs
```

Expected surfaces:

```text
src/semantic_manifest.rs
- UNKNOWN_LOW_CONFIDENCE exists
- IntentEvidence mirror exists for graph ingestion
- resolve_intent/equivalent prefers doc > name > effects > unknown_low_confidence
- manifest_has_low_confidence exists
- low_confidence status is not partial_error
- reports fn_intent_coverage and fn_low_confidence_rate

src/semantic_contract.rs
- semantic contract score uses intent coverage and low-confidence rate
- missing sidecar remains degraded, not panic

src/evaluation.rs
- EvalSnapshot/EvalInput carries:
  semantic_fn_low_confidence
  semantic_fn_intent_coverage
  semantic_fn_low_confidence_rate
- semantic_contract score includes these metrics

src/events.rs
- eval event has semantic_fn_low_confidence, semantic_fn_intent_coverage, semantic_fn_low_confidence_rate

src/complexity.rs
- complexity report exports semantic_fn_low_confidence, semantic_fn_intent_coverage, semantic_fn_low_confidence_rate

src/syn_writer.rs
- unknown_low_confidence is treated as missing/repairable when writing docstrings
```

Verification commands:

```bash
cd /mnt/data/canon-mini-agent-extracted/canon-mini-agent

rg -n "UNKNOWN_LOW_CONFIDENCE|IntentEvidence|resolve_intent|manifest_has_low_confidence|fn_intent_coverage|fn_low_confidence|semantic_fn_intent_coverage|semantic_fn_low_confidence|unknown_low_confidence" src
```

---

## Phase 0: Static Validation Only

Use this when Rust is unavailable.

```bash
cd /mnt/data/canon-mini-agent-extracted/canon-mini-agent

python - <<'PY'
import json
from pathlib import Path

root = Path(".")
graph = json.loads((root/"state/rustc/canon_mini_agent/graph.json").read_text())
nodes = graph.get("nodes", {})
fns = [n for n in nodes.values() if n.get("kind") == "fn"]
classified = sum(1 for n in fns if n.get("intent_class"))
print({
    "nodes": len(nodes),
    "edges": len(graph.get("edges", [])),
    "fn_total": len(fns),
    "intent_classified": classified,
    "intent_missing": len(fns) - classified,
    "intent_coverage": classified / len(fns) if fns else 1.0,
})
manifest = json.loads((root/"agent_state/semantic_manifest_proposals.json").read_text())
print({
    "manifest_fn_total": manifest.get("fn_total"),
    "manifest_fn_with_any_error": manifest.get("fn_with_any_error"),
    "manifest_fn_error_rate": manifest.get("fn_error_rate"),
    "proposal_count": len(manifest.get("proposals", [])),
})
seq = []
counts = {}
for line in (root/"agent_state/tlog.ndjson").read_text().splitlines():
    if not line.strip():
        continue
    o = json.loads(line)
    seq.append(o.get("seq"))
    e = o.get("event", {})
    inner = e.get("event", {}) if isinstance(e, dict) else {}
    k = inner.get("kind", "unknown") if isinstance(inner, dict) else "unknown"
    counts[k] = counts.get(k, 0) + 1
print({
    "tlog_events": len(seq),
    "seq_first": seq[0] if seq else None,
    "seq_last": seq[-1] if seq else None,
    "seq_gaps": sum(1 for a,b in zip(seq, seq[1:]) if b != a + 1),
    "recovery_triggered": counts.get("recovery_triggered", 0),
    "recovery_outcome_recorded": counts.get("recovery_outcome_recorded", 0),
    "recovery_suppressed": counts.get("recovery_suppressed", 0),
})
PY
```

Expected before graph rebuild:

```text
intent_classified = 685
intent_missing = 1691
intent_coverage ≈ 0.2883
```

Expected after graph rebuild:

```text
intent_classified ≈ fn_total
intent_missing = 0
unknown_low_confidence may be > 0
```

---

## Phase 1: Rust Build Gate

Do this in the user environment that has Rust. Do not run cargo in sandboxes where Rust is unavailable.

### Build wrapper first

```bash
cd /workspace/ai_sandbox/canon-rustc-v2
cargo build
cargo test
```

Expected:

```text
wrapper compiles
tests pass
target/debug/canon-rustc-v2 exists
```

If errors occur, inspect likely surfaces:

```text
src/graph.rs: struct field mismatch
src/docstring.rs: IntentEvidence constructor mismatch
src/hir.rs: GraphNode initializer missing intent_evidence
src/wrapper.rs: SemanticManifest initializer/status mismatch
```

### Build agent second

```bash
cd /workspace/ai_sandbox/canon-mini-agent
cargo build
cargo test
```

Expected:

```text
agent compiles
tests pass
```

If errors occur, inspect likely surfaces:

```text
src/events.rs: new eval event fields require match/serialization updates
src/evaluation.rs: EvalInput/EvalSnapshot initializer missing fields
src/complexity.rs: report map needs field additions
src/semantic_contract.rs: sidecar default needs field additions
src/semantic_manifest.rs: graph mirror struct mismatch with wrapper graph schema
```

---

## Phase 2: Regenerate `graph.json`

The agent `.cargo/config.toml` points `rustc-wrapper` to:

```text
/workspace/ai_sandbox/canon-rustc-v2/target/debug/canon-rustc-v2
```

Check it:

```bash
cd /workspace/ai_sandbox/canon-mini-agent
sed -n '1,60p' .cargo/config.toml
```

If working from `/mnt/data`, either use `/workspace/ai_sandbox` paths or update the wrapper path locally.

Regenerate graph:

```bash
cd /workspace/ai_sandbox/canon-mini-agent
cargo clean
cargo build --workspace
```

Expected output should move from:

```text
intent_class 685/2376fn
```

to approximately:

```text
intent_class 2376/2376fn
```

Allow `unknown_low_confidence`; do not allow missing `intent_class`.

---

## Phase 3: Remeasure Graph Intent Coverage

```bash
cd /workspace/ai_sandbox/canon-mini-agent

python - <<'PY'
import json
from pathlib import Path
g = json.loads(Path("state/rustc/canon_mini_agent/graph.json").read_text())
nodes = g.get("nodes", {})
fns = [n for n in nodes.values() if n.get("kind") == "fn"]
classified = sum(1 for n in fns if n.get("intent_class"))
unknown_low = sum(1 for n in fns if n.get("intent_class") == "unknown_low_confidence")
with_evidence = sum(1 for n in fns if n.get("intent_evidence"))
print({
    "fn_total": len(fns),
    "intent_classified": classified,
    "intent_missing": len(fns)-classified,
    "unknown_low_confidence": unknown_low,
    "with_intent_evidence": with_evidence,
    "coverage": classified / len(fns) if fns else 1.0,
})
PY
```

Pass gate:

```text
intent_missing = 0
coverage = 1.0
```

Warning gate:

```text
unknown_low_confidence high
```

High unknown-low-confidence is not a correctness failure; it means docstring/name/effect heuristics need enrichment.

---

## Phase 4: Regenerate Semantic Manifest

Run the existing semantic sync path.

Likely paths:

```bash
cd /workspace/ai_sandbox/canon-mini-agent

rg -n "run_semantic_sync|semantic_manifest|semantic_sync_outputs_stale" src
```

Expected command options from source comments:

```bash
cargo run -p canon-mini-agent --bin semantic_manifest -- state/rustc/canon_mini_agent/graph.json --write
```

If the binary name differs, use the source-discovered path from `src/semantic_manifest.rs`.

Then inspect:

```bash
python - <<'PY'
import json
from pathlib import Path
p = Path("agent_state/semantic_manifest_proposals.json")
m = json.loads(p.read_text())
proposals = m.get("proposals", [])
print({
    "fn_total": m.get("fn_total"),
    "fn_with_any_error": m.get("fn_with_any_error"),
    "fn_error_rate": m.get("fn_error_rate"),
    "proposal_count": len(proposals),
})
from collections import Counter
status = Counter(x.get("status") for x in proposals if isinstance(x, dict))
intent = Counter(x.get("intent_class") for x in proposals if isinstance(x, dict) and x.get("kind") == "fn")
print("statuses", status.most_common(20))
print("top_intents", intent.most_common(20))
PY
```

Pass gate:

```text
intent-related partial_error = 0
low_confidence may exist
fn_error_rate decreases or stays honest
```

If `fn_error_rate` does not decrease, identify remaining causes:

```bash
python - <<'PY'
import json
from pathlib import Path
from collections import Counter
m = json.loads(Path("agent_state/semantic_manifest_proposals.json").read_text())
c = Counter()
for p in m.get("proposals", []):
    if not isinstance(p, dict):
        continue
    for k,v in p.items():
        if v in ("error", "partial_error", "missing"):
            c[k] += 1
print(c.most_common(40))
PY
```

---

## Phase 5: Burn Down Remaining `partial_error`

Priority order:

```text
1. resource
2. effects
3. failure_mode
4. invariants
5. branches/mutations/tests if still represented as hard error
```

Rules:

```text
uncertain/unknown = low_confidence or unknown
extractor failure = hard_error
schema corruption = hard_error
missing optional signal = metric-only
```

Target files:

```text
canon-rustc-v2/src/wrapper.rs
canon-rustc-v2/src/graph.rs
canon-mini-agent/src/semantic_manifest.rs
canon-mini-agent/src/semantic_contract.rs
canon-mini-agent/src/semantic_issue_projection.rs
canon-mini-agent/src/graph_metrics.rs
```

Do not hide real failures. Only demote false failures caused by unavailable optional analysis.

---

## Phase 6: Eval Integration Check

Confirm eval carries the new metrics end-to-end:

```bash
cd /workspace/ai_sandbox/canon-mini-agent

rg -n "semantic_fn_intent_coverage|semantic_fn_low_confidence|semantic_fn_low_confidence_rate|semantic_contract" src/evaluation.rs src/eval_driver.rs src/events.rs src/complexity.rs src/semantic_contract.rs
```

Expected metric flow:

```text
semantic_manifest.rs
→ semantic_contract.rs
→ evaluation.rs
→ eval_driver.rs/events.rs
→ complexity.rs/report output
→ tlog eval event
```

Run eval/report command if available:

```bash
rg -n "eval_driver|complexity_report|state/reports/complexity/latest.json|EvaluationDeltaRecorded|eval_delta" src
```

Then inspect generated report:

```bash
python - <<'PY'
import json
from pathlib import Path
for path in [
    "state/reports/complexity/latest.json",
    "agent_state/semantic_manifest_proposals.json",
]:
    p = Path(path)
    if p.exists():
        print(path)
        data = json.loads(p.read_text())
        for k in [
            "semantic_fn_total",
            "semantic_fn_with_any_error",
            "semantic_fn_error_rate",
            "semantic_fn_intent_classified",
            "semantic_fn_low_confidence",
            "semantic_fn_intent_coverage",
            "semantic_fn_low_confidence_rate",
            "fn_total",
            "fn_with_any_error",
            "fn_error_rate",
        ]:
            if k in data:
                print(" ", k, data[k])
PY
```

---

## Phase 7: Add/Confirm Tests

Add tests only after build errors are gone.

Needed tests:

```text
1. wrapper emits unknown_low_confidence for fn with no doc/name/effect intent
2. wrapper manifest status for unknown_low_confidence is low_confidence, not partial_error
3. agent semantic manifest treats unknown_low_confidence as low_confidence, not hard error
4. eval score decreases for high low_confidence_rate without treating it as hard error
5. syn_writer treats unknown_low_confidence as repairable docstring target
```

Likely test files:

```text
canon-rustc-v2/src/wrapper.rs
canon-rustc-v2/src/docstring.rs
canon-mini-agent/src/semantic_manifest.rs
canon-mini-agent/src/semantic_contract.rs
canon-mini-agent/src/evaluation.rs
canon-mini-agent/src/syn_writer.rs
```

Test names to add or verify:

```text
unknown_low_confidence_is_total_intent_fallback
unknown_low_confidence_manifest_status_is_low_confidence
semantic_manifest_low_confidence_is_not_partial_error
semantic_contract_score_penalizes_low_confidence_rate
syn_writer_rewrites_unknown_low_confidence_docstring
```

---

## Phase 8: Rubric Update

Current rubric state:

```text
rubric/PROJECT_RUBRIC_COMPLETION.v0.md
rubric/PROJECT_RUBRIC_COMPLETION.v1.md
rubric/PROJECT_RUBRIC_COMPLETION.v2.md
rubric/PROJECT_RUBRIC_COMPLETION.latest.md
```

After live evidence is generated:

```bash
cd /workspace/ai_sandbox/canon-mini-agent

cp rubric/PROJECT_RUBRIC_COMPLETION.latest.md rubric/PROJECT_RUBRIC_COMPLETION.v3.md
```

Update both:

```text
rubric/PROJECT_RUBRIC_COMPLETION.v3.md
rubric/PROJECT_RUBRIC_COMPLETION.latest.md
```

Rubric evidence to include:

```text
intent_classified / fn_total before
intent_classified / fn_total after
unknown_low_confidence count
semantic_manifest fn_error_rate before
semantic_manifest fn_error_rate after
cargo build status
cargo test status
remaining partial_error categories
next highest-leverage repair
```

Do not claim improvement until graph/manifest are regenerated and measured.

---

## Phase 9: New Session Opening Prompt

Use this prompt in the new session:

```text
Continue from PLAN.md.

Repos:
- /mnt/data/canon-mini-agent-extracted/canon-rustc-v2
- /mnt/data/canon-mini-agent-extracted/canon-mini-agent

Goal:
Finish intent_class totalization. Wrapper emits total intent evidence. Agent treats unknown_low_confidence as metric-only, not partial_error. Rebuild graph, regenerate semantic_manifest_proposals.json, remeasure eval, update rubric v3/latest.

Rules:
- Use Python for JSON/JSONL/NDJSON/log analysis.
- Use rg to locate.
- Use awk to slice.
- Use perl for nested/block extraction if needed.
- Use apply_patch_v3 through Python subprocess for edits.
- Do not manually edit files.
- Do not patch PLAN.json directly.
- Do not run rustc/cargo if this environment lacks Rust; instead give commands for my local run.
- Validate with the narrowest possible checks first.

Start by running rg checks in both repos and Python-parsing graph.json + semantic_manifest_proposals.json + tlog.ndjson.
```

---

## Completed In This Session

```text
intent_classified_fn / total_fn = 2380 / 2380
unknown_low_confidence is metric-only
partial_error excludes missing/uncertain intent in source logic
cargo build && cargo test pass in user Rust environment
rubric v3/latest reflects current evidence
```

Applied source deltas:

```text
canon-rustc-v2/src/wrapper.rs
- totalizes missing function intent into unknown_low_confidence
- treats plain/generated "error" as unknown or low confidence, not partial_error
- preserves explicit hard markers as hard failures

canon-mini-agent/src/semantic_manifest.rs
- treats generated doc error placeholders as repairable fallback
- keeps explicit hard_error/extractor_error/schema_error/schema_corruption/parse_error as partial_error
- prevents low-confidence intent from inflating fn_with_any_error after regeneration
```

Validation closure:

```text
V_user = cargo build && cargo test
V_status = pass
Result = source-level intent totalization complete
```

Still pending:

```text
Regenerate semantic_manifest_proposals.json after the source patch.
Remeasure fn_with_any_error and remaining partial_error categories.
Confirm eval/report/tlog carries final semantic deltas.
```

---

## Highest-Leverage Next Action

```text
Regenerate graph + semantic_manifest_proposals.json → remeasure fn_error_rate → confirm remaining partial_error entries are only explicit hard failures.
```

If that passes, the next repair is:

```text
Wire final semantic intent metrics into eval/tlog/report dashboards if any metric is missing.
```

New-session handoff:

```text
Do not redo the completed source patch.
Start by regenerating artifacts from the patched code, then compute:
- graph fn_intent_coverage
- graph unknown_low_confidence count
- manifest fn_with_any_error
- manifest fn_error_rate
- partial_error reason histogram
- eval/tlog/report visibility for these values
```
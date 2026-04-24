#!/usr/bin/env bash
set -euo pipefail

# End-to-end semantic automation pipeline:
# 1) rustc wrapper emits compiler facts
# 2) semantic_manifest joins facts + docstrings and proposes missing metadata
# 3) syn_writer writes/augments canonical docstrings
# 4) graph is regenerated
# 5) redundant pairs are ranked

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GRAPH="${1:-$ROOT/state/rustc/canon_mini_agent/graph.json}"
WRAPPER_BIN="${RUSTC_WRAPPER_BIN:-/workspace/ai_sandbox/canon-rustc-v2/target/debug/canon-rustc-v2}"
MAX_ERROR_RATE="${SEM_MAX_ERROR_RATE:-}"

echo "[semantic_pipeline] root: $ROOT"
echo "[semantic_pipeline] graph: $GRAPH"
echo "[semantic_pipeline] wrapper: $WRAPPER_BIN"

pushd "$ROOT" >/dev/null

echo "[semantic_pipeline] build local bins"
cargo build --bin semantic_manifest --bin canon-syn-writer --bin canon-rank-candidates

if [[ -x "$WRAPPER_BIN" ]]; then
  echo "[semantic_pipeline] capture graph via rustc wrapper"
  RUSTC_WRAPPER="$WRAPPER_BIN" cargo build --lib
else
  echo "[semantic_pipeline] WARNING: wrapper not executable: $WRAPPER_BIN"
  echo "[semantic_pipeline] continuing with existing graph file"
fi

echo "[semantic_pipeline] step3/4 join+propose -> semantic_manifest"
if [[ -n "$MAX_ERROR_RATE" ]]; then
  cargo run --bin semantic_manifest -- "$GRAPH" --write --max-error-rate "$MAX_ERROR_RATE"
else
  cargo run --bin semantic_manifest -- "$GRAPH" --write
fi

echo "[semantic_pipeline] step5/6 rewrite canonical docstrings"
./target/debug/canon-syn-writer "$GRAPH" --rewrite-existing --write

if [[ -x "$WRAPPER_BIN" ]]; then
  echo "[semantic_pipeline] regenerate graph after source doc updates"
  RUSTC_WRAPPER="$WRAPPER_BIN" cargo build --lib
fi

echo "[semantic_pipeline] refresh semantic_manifest after regeneration"
if [[ -n "$MAX_ERROR_RATE" ]]; then
  cargo run --bin semantic_manifest -- "$GRAPH" --write --max-error-rate "$MAX_ERROR_RATE"
else
  cargo run --bin semantic_manifest -- "$GRAPH" --write
fi

echo "[semantic_pipeline] rank redundant path pairs"
./target/debug/canon-rank-candidates "$GRAPH" "$ROOT/agent_state/safe_patch_candidates.json"

echo "[semantic_pipeline] done"
popd >/dev/null

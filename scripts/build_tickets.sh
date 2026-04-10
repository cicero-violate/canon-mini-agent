#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'EOF'
Usage:
  cargo build-tickets -- [canon-tickets args...]

Runs:
  1) cargo build
  2) canon-tickets (post-build, using the freshly generated state/rustc/*/graph.json)

Defaults passed to canon-tickets when no args are provided:
  --workspace <repo-root> --all-crates --top 3 --prune

Examples:
  cargo build-tickets
  cargo build-tickets -- --workspace /workspace/ai_sandbox/canon-mini-agent --all-crates --top 3 --prune --print
EOF
  exit 0
fi

echo "[build-tickets] cargo build"
cargo build

if [[ "$#" -eq 0 ]]; then
  set -- --workspace "$ROOT" --all-crates --top 3 --prune
fi

echo "[build-tickets] canon-tickets $*"
cargo run -q -p canon-mini-agent --bin canon-tickets -- "$@"


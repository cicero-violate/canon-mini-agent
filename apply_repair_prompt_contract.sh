#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${1:-/workspace/ai_sandbox/canon-mini-agent}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCH_FILE="${PATCH_FILE:-$SCRIPT_DIR/canon-mini-agent-repair-prompt-contract.apply_patch}"

python - <<'PY' "$REPO_ROOT" "$PATCH_FILE"
from pathlib import Path
import subprocess
import sys

repo_root = Path(sys.argv[1])
patch_file = Path(sys.argv[2])
patch_text = patch_file.read_text()

subprocess.run(
    ["/opt/apply_patch/apply_patch_v3"],
    input=patch_text,
    text=True,
    check=True,
    cwd=repo_root,
    env={
        "PYTHONPATH": "/opt/pyvenv/lib/python3.13/site-packages",
        "PATH": "/usr/bin",
    },
)
PY

rg -n "ACTIVE REPAIR PLAN CONTRACT|Active repair plan contract|planner_prompts_include_active_repair_plan_contract|For active REPAIR_PLAN work" \
  "$REPO_ROOT/src/repair_plans.rs" \
  "$REPO_ROOT/src/prompts.rs" \
  "$REPO_ROOT/src/tool_schema.rs"

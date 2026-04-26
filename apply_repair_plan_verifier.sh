#!/usr/bin/env bash
set -euo pipefail

# Run from the canon-mini-agent repo root:
#   cd /workspace/ai_sandbox/canon-mini-agent
#   bash /path/to/apply_repair_plan_verifier.sh

PATCH_FILE="${PATCH_FILE:-canon-mini-agent-repair-plan-verifier.apply_patch}"

python - <<'PY'
import os
import pathlib
import subprocess

repo_root = pathlib.Path.cwd()
patch_file = pathlib.Path(os.environ.get("PATCH_FILE", "canon-mini-agent-repair-plan-verifier.apply_patch"))
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

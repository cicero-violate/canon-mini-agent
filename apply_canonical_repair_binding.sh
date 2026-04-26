#!/usr/bin/env bash
set -euo pipefail

cd /workspace/ai_sandbox/canon-mini-agent

python - <<'PY'
import pathlib
import subprocess

patch = pathlib.Path("canon-mini-agent-canonical-repair-binding.apply_patch").read_text()
subprocess.run(
    ["/opt/apply_patch/apply_patch_v3"],
    input=patch,
    text=True,
    check=True,
    cwd=".",
    env={
        "PYTHONPATH": "/opt/pyvenv/lib/python3.13/site-packages",
        "PATH": "/usr/bin",
    },
)
PY

cargo build && cargo test

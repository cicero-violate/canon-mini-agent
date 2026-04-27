#!/usr/bin/env bash
set -euo pipefail

REPO="${1:-/workspace/ai_sandbox/canon-mini-agent}"
PATCH_FILE="${PATCH_FILE:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/canon-mini-agent-artifact-lineage-verifier.apply_patch}"

cd "$REPO"
python - <<'PY'
import os
import pathlib
import subprocess

repo = pathlib.Path.cwd()
patch_file = pathlib.Path(os.environ.get("PATCH_FILE", "/mnt/data/canon-mini-agent-artifact-lineage-verifier.apply_patch"))
patch_text = patch_file.read_text()
subprocess.run(
    ["/opt/apply_patch/apply_patch_v3"],
    input=patch_text,
    text=True,
    check=True,
    cwd=repo,
    env={
        "PYTHONPATH": "/opt/pyvenv/lib/python3.13/site-packages",
        "PATH": "/usr/bin",
    },
)

checks = {
    "src/events.rs": ["source_event_seq", "producer_action", "eval_outcome"],
    "src/logging.rs": ["artifact_lineage_eval_outcome", "source_event_seq = writer.tlog_seq().saturating_add(1)"],
    "src/tools_foundation.rs": ["source_event_seq = writer.tlog_seq().saturating_add(1)"],
    "src/evaluation.rs": ["artifact_lineage_orphans", "record_artifact_lineage", "tlog_delta_invariants_warn_on_orphan_artifact_lineage"],
    "src/complexity.rs": ["artifact_lineage_complete", "orphan_artifact_ids"],
    "src/prompt_inputs.rs": ["artifact_lineage=complete"],
    "src/prompts.rs": ["L_artifact", "planner_prompts_include_artifact_lineage_contract"],
    "src/tool_schema.rs": ["expected artifact lineage"],
}
missing = []
for rel, markers in checks.items():
    text = (repo / rel).read_text()
    for marker in markers:
        if marker not in text:
            missing.append(f"{rel} missing {marker}")
if missing:
    raise SystemExit("\n".join(missing))
print("artifact lineage verifier markers: ok")
PY

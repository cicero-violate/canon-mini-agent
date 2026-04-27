#!/usr/bin/env bash
set -euo pipefail

repo_root="${1:-$(pwd)}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
patch_path="${script_dir}/canon-mini-agent-repair-prompt-contract-v2.apply_patch"

cd "$repo_root"

python - "$patch_path" <<'PY'
import subprocess
import sys
from pathlib import Path

patch_path = Path(sys.argv[1])
if not patch_path.exists():
    raise SystemExit(f"missing patch file: {patch_path}")

subprocess.run(
    ["/opt/apply_patch/apply_patch_v3"],
    input=patch_path.read_text(),
    text=True,
    check=True,
    cwd=".",
    env={"PYTHONPATH":"/opt/pyvenv/lib/python3.13/site-packages","PATH":"/usr/bin"},
)
PY

python - <<'PY'
from pathlib import Path
text = Path("src/prompts.rs").read_text()
required = [
    "Active repair plan contract:",
    "`repair_plan_id`",
    "`required_mutation`",
    "`target_files`",
    "planner repair drift",
    "planner_prompts_include_active_repair_plan_contract",
]
missing = [needle for needle in required if needle not in text]
if missing:
    raise SystemExit(f"missing prompt contract markers: {missing}")
print("prompt contract markers: ok")
PY

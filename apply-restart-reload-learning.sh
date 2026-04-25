#!/usr/bin/env bash
set -euo pipefail

repo="${1:-/workspace/ai_sandbox/canon-mini-agent}"
patch_file="${2:-/mnt/data/canon-mini-agent-restart-reload-learning.apply_patch}"

cd "$repo"

/usr/bin/python3 - <<'PY'
import json
from pathlib import Path
p = Path("INVARIANTS.json")
json.load(p.open())
print(f"json_ok {p}")
PY

/usr/bin/python3 - "$patch_file" <<'PY'
import subprocess, sys
from pathlib import Path
patch = Path(sys.argv[1]).read_text()
subprocess.run(
    ["/opt/apply_patch/apply_patch_v3"],
    input=patch,
    text=True,
    check=True,
    env={"PYTHONPATH":"/opt/pyvenv/lib/python3.13/site-packages","PATH":"/usr/bin"},
)
PY

/usr/bin/python3 - <<'PY'
import json
from pathlib import Path
p = Path("INVARIANTS.json")
data = json.load(p.open())
assert any(inv.get("id") == "I21-supervisor-reload-proof" for inv in data.get("invariants", []))
print("invariant_ok I21-supervisor-reload-proof")
PY

rg -n "SupervisorRestartRequested|SupervisorChildStarted|reload_proven|restart_requests_without_child_start|I21-supervisor-reload-proof" \
  src CANONICAL_PIPELINE.md INVARIANTS.json

cargo check

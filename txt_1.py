from pathlib import Path
import subprocess, textwrap, os
repo = '/mnt/data/canon-mini-agent-extracted/canon-mini-agent'
p = Path(repo) / 'src/llm_runtime/chromium_backend.rs'
text = p.read_text()
old = '        eprintln!("[chromium] no reusable tab in backend state, opening {url} ({fallback_debug})");\n'
new = '        // This means no reusable tab was found in backend ownership state, not that Chrome has zero tabs.\n        eprintln!("[chromium] no reusable tab in backend state, opening {url} ({fallback_debug})");\n'
if old not in text:
    raise SystemExit("target line not found")
patch = textwrap.dedent("""\
*** Begin Patch
*** Update File: src/llm_runtime/chromium_backend.rs
@@
-        e

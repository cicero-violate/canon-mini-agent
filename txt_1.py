from pathlib import Path
import subprocess, textwrap

p = Path('/mnt/data/canon-mini-agent-extracted/canon-mini-agent/src/llm_runtime/chromium_backend.rs')
text = p.read_text()
old = '        eprintln!("[chromium] no reusable tab in backend state, opening {url} ({fallback_debug})");\n'
new = '        // This means no reusable tab was found in backend ownership state, not that Chrome has zero tabs.\n        eprintln!("[chromium] no reusable tab in backend state, opening {url} ({fallback_debug})");\n'
if old not in text:
    raise SystemExit("target line not found")
patch = textwrap.dedent("""\
*** Begin Patch
*** Update File: /mnt/data/canon-mini-agent-extracted/canon-mini-agent/src/llm_runtime/chromium_backend.rs
@@
-        eprintln!("[chromium] no reusable tab in backend state, opening {url} ({fallback_debug})");
+        // This means no reusable tab was found in backend ownership state, not that Chrome has zero tabs.
+        eprintln!("[chromium] no reusable tab in backend state, opening {url} ({fallback_debug})");
*** End Patch
""")
res = subprocess.run(['/opt/apply_patch/bin/apply_patch'], input=patch.encode(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
print(res.stdout.decode())
print("returncode", res.returncode)

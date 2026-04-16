from pathlib import Path
import subprocess, os

repo = Path('/mnt/data/canon-mini-agent-extracted/canon-mini-agent')
os.chdir(repo)
text = Path('src/llm_runtime/chromium_backend.rs').read_text()
assert 'use serde_json::json;' in text
assert 'st.pending_ack.retain(|(tid, _), _| *tid != tab_id);' in text

patch = r"""*** Begin Patch
*** Update File: src/llm_runtime/chromium_backend.rs
@@
             st.pending_turn_id.remove(&tab_id);
+            st.pending_turn_lease.retain(|(tid, _), _| *tid != tab_id);
             st.pending_ack.retain(|(tid, _), _| *tid != tab_id);
             st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
             st.pending_early_fail.retain(|(tid, _), _| *tid != tab_id);
             st.turn_complete_seen.retain(|(tid, _), _| *tid != tab_id);
             st.post_complete_presence.retain(|(tid, _), _| *tid != tab_id);
@@
 mod tests {
     use super::{endpoint_submit_ack_timeout_secs, handle_inbound, State};
     use crate::llm_runtime::parsers::{FrameAssembler, SiteType};
-    use serde_json::json;
+    use serde_json::{json, Value};
     use std::sync::Arc;
-    use tokio::sync::Mutex;
+    use tokio::sync::{oneshot, Mutex};
*** End Patch
"""
res = subprocess.run(
    ['/opt/apply_patch/bin/apply_patch'],
    input=patch.encode(),
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    cwd=str(repo),
)
print(res.stdout.decode())
print("returncode", res.returncode)

#!/usr/bin/env bash
set -euo pipefail

cd /workspace/ai_sandbox/canon-mini-agent

cat <<'PATCH_INNER' | /opt/apply_patch/bin/apply_patch
*** Begin Patch
*** Update File: src/llm_runtime/chromium_backend.rs
@@
                 if stateful {
                     if let Some(tab_id) = st.endpoint_tabs.get(endpoint_id).copied() {
+                        // Owned stateful tabs remain bound to the endpoint across
+                        // a soft-reset quarantine so acquisition waits for the
+                        // original tab instead of opening a duplicate.
                         if st.quarantined_tabs.contains(&tab_id) {
                             // Stateful transport retirement soft-resets the owned tab in place.
                             // Wait for the TAB_READY reannouncement instead of opening a second
                             // tab for the same endpoint URL.
                         } else if let Some(url) = st.tab_urls.get(&tab_id).cloned() {
                             st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
                             return Ok(tab_id);
+                        } else {
+                            st.endpoint_tabs.remove(endpoint_id);
+                            st.tab_owners.remove(&tab_id);
                         }
-                        st.endpoint_tabs.remove(endpoint_id);
-                        st.tab_owners.remove(&tab_id);
                     }
                 }
@@
             let req_id = msg.get("reqId").and_then(|v| v.as_u64());

             let mut st = state.lock().await;
+            let tab_owner = st.tab_owners.get(&tab_id).cloned();
             let site = SiteType::from_url(&url);
             st.assemblers
                 .entry(tab_id)
                 .and_modify(|a| a.set_site(site))
                 .or_insert_with(|| FrameAssembler::new(site));
@@
             if let Some(rid) = req_id {
                 // This was a tab we opened via OPEN_TAB.
                 if let Some(tx) = st.pending_open.remove(&rid) {
                     let _ = tx.send(tab_id);
                 }
-            } else {
+            } else if tab_owner.is_none() {
                 // Organic tab (pre-existing or sw-restart re-announcement).
                 let queue = st.preopened.entry(url.clone()).or_default();
                 if !queue.contains(&tab_id) {
                     queue.push(tab_id);
                 }
@@
     st.assemblers.remove(&tab_id);

     if stateful {
-        if st.endpoint_tabs.get(endpoint_id).copied() == Some(tab_id) {
-            st.endpoint_tabs.remove(endpoint_id);
-        }
-        st.tab_owners.remove(&tab_id);
-
         // Do not hard-close stateful ChatGPT/Gemini tabs on transport failure.
         // Closing guarantees the next acquire path has nothing reusable and
         // forces open-tab churn. Instead, soft-reset the tab by navigating it
         // back to its current provider URL without a reqId so the eventual
         // TAB_READY is treated as an organic reusable tab announcement.
@@
             }
             return;
         }
+
+        if st.endpoint_tabs.get(endpoint_id).copied() == Some(tab_id) {
+            st.endpoint_tabs.remove(endpoint_id);
+        }
+        st.tab_owners.remove(&tab_id);
     }

     st.quarantined_tabs.insert(tab_id);
*** End Patch
PATCH_INNER


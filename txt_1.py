from pathlib import Path
import subprocess, textwrap, json, os, sys

repo = Path("/mnt/data/canon-mini-agent-extracted/canon-mini-agent")
target = repo / "src/llm_runtime/chromium_backend.rs"
text = target.read_text()

patch = textwrap.dedent("""\
*** Begin Patch
*** Update File: /mnt/data/canon-mini-agent-extracted/canon-mini-agent/src/llm_runtime/chromium_backend.rs
@@
-use std::collections::{HashMap, VecDeque};
+use std::collections::{HashMap, HashSet, VecDeque};
@@
-        // Give TAB_READY messages a moment to arrive after connection.
-        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
-
-        if stateful {
-            let mut st = self.state.lock().await;
-            if let Some(tab_id) = st.endpoint_tabs.get(endpoint_id).copied() {
-                if let Some(url) = st.tab_urls.get(&tab_id).cloned() {
-                    st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
-                    return Ok(tab_id);
-                }
-                st.endpoint_tabs.remove(endpoint_id);
-                st.tab_owners.remove(&tab_id);
-            }
-        }
-
-        // Check pool first.
-        if let Some(tab_id) = if stateful {
-            self.pop_matching_url_tab(urls).await
-        } else {
-            self.pop_any_tab().await
-        } {
-            if stateful {
-                let mut st = self.state.lock().await;
-                st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
-                st.tab_owners.insert(tab_id, endpoint_id.to_string());
-            }
-            return Ok(tab_id);
-        }
+        // Give TAB_READY messages a moment to arrive after connection, then
+        // spend a short bounded window reconciling backend state with the
+        // browser's current tab announcements before opening a new tab.
+        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
+        let mut reconcile_attempt = 0u32;
+        loop {
+            {
+                let mut st = self.state.lock().await;
+                let recovered =
+                    reconcile_tab_state_locked(&mut st, Some(endpoint_id), urls);
+                if recovered > 0 {
+                    append_outbound_event(
+                        "OUTBOUND_TAB_STATE_RECONCILED",
+                        json!({
+                            "endpointId": endpoint_id,
+                            "stateful": stateful,
+                            "requestedUrls": urls,
+                            "recoveredCount": recovered,
+                            "endpointTab": st.endpoint_tabs.get(endpoint_id).copied(),
+                            "knownTabCount": st.tab_urls.len(),
+                            "ownedTabCount": st.tab_owners.len(),
+                            "preopenedUrlCount": st.preopened.len(),
+                            "attempt": reconcile_attempt,
+                        }),
+                    );
+                }
+                if stateful {
+                    if let Some(tab_id) = st.endpoint_tabs.get(endpoint_id).copied() {
+                        if let Some(url) = st.tab_urls.get(&tab_id).cloned() {
+                            st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
+                            return Ok(tab_id);
+                        }
+                        st.endpoint_tabs.remove(endpoint_id);
+                        st.tab_owners.remove(&tab_id);
+                    }
+                }
+            }
+
+            if let Some(tab_id) = if stateful {
+                self.pop_matching_url_tab(urls).await
+            } else {
+                self.pop_any_tab().await
+            } {
+                if stateful {
+                    let mut st = self.state.lock().await;
+                    st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
+                    st.tab_owners.insert(tab_id, endpoint_id.to_string());
+                }
+                return Ok(tab_id);
+            }
+
+            reconcile_attempt += 1;
+            if reconcile_attempt >= 5 || std::time::Instant::now() >= deadline {
+                break;
+            }
+            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
+        }
@@
 fn retire_tab_locked(st: &mut State, endpoint_id: &str, tab_id: u32, stateful: bool) {
@@
     for queue in st.preopened.values_mut() {
         queue.retain(|id| *id != tab_id);
     }
 }
+
+fn reconcile_tab_state_locked(
+    st: &mut State,
+    endpoint_id: Option<&str>,
+    requested_urls: &[String],
+) -> usize {
+    let known_tab_ids: HashSet<u32> = st.tab_urls.keys().copied().collect();
+    st.tab_owners
+        .retain(|tab_id, _| known_tab_ids.contains(tab_id));
+
+    let owned_by_tab = st.tab_owners.clone();
+    st.endpoint_tabs.retain(|endpoint, tab_id| {
+        known_tab_ids.contains(tab_id)
+            && owned_by_tab
+                .get(tab_id)
+                .map(String::as_str)
+                == Some(endpoint.as_str())
+    });
+
+    let owned_tab_ids: HashSet<u32> = st.tab_owners.keys().copied().collect();
+    let mut seen_preopened = HashSet::new();
+    st.preopened.retain(|_, queue| {
+        queue.retain(|tab_id| {
+            known_tab_ids.contains(tab_id)
+                && !owned_tab_ids.contains(tab_id)
+                && seen_preopened.insert(*tab_id)
+        });
+        !queue.is_empty()
+    });
+
+    let mut recovered = 0usize;
+    if let Some(endpoint_id) = endpoint_id {
+        if !st.endpoint_tabs.contains_key(endpoint_id) {
+            if let Some(tab_id) = st.tab_owners.iter().find_map(|(tab_id, owner)| {
+                (owner == endpoint_id && known_tab_ids.contains(tab_id)).then_some(*tab_id)
+            }) {
+                st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
+                recovered += 1;
+            }
+        }
+    }
+
+    let tab_snapshot: Vec<(u32, String)> = st
+        .tab_urls
+        .iter()
+        .map(|(tab_id, url)| (*tab_id, url.clone()))
+        .collect();
+    for (tab_id, url) in tab_snapshot {
+        if st.tab_owners.contains_key(&tab_id) {
+            continue;
+        }
+        if !requested_urls.is_empty() && !requested_urls.iter().any(|candidate| candidate == &url) {
+            continue;
+        }
+        let queue = st.preopened.entry(url).or_default();
+        if !queue.contains(&tab_id) {
+            queue.push_back(tab_id);
+            recovered += 1;
+        }
+    }
+
+    recovered
+}
 
 #[cfg(test)]
 mod tests {
*** End Patch
""")

res = subprocess.run(
    ["/opt/apply_patch/bin/apply_patch"],
    input=patch.encode(),
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
)
print(res.stdout.decode())
print("returncode", res.returncode)

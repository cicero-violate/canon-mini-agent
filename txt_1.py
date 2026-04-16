import subprocess, textwrap
patch = r"""*** Begin Patch
*** Update File: /mnt/data/canon-mini-agent/src/llm_runtime/chromium_backend.rs
@@
     /// (tabId, turnId) -> count of `presence` frames observed after
     /// `conversation-turn-complete` and before any assistant terminal frame.
     post_complete_presence: HashMap<(u32, u64), u32>,
+
+    /// (tabId, turnId) -> count of `heartbeat` frames observed after
+    /// `conversation-turn-complete` and before any assistant terminal frame.
+    post_complete_heartbeat: HashMap<(u32, u64), u32>,
@@
             replay_queue: Vec::new(),
             frame_counter: 0,
             turn_complete_seen: HashMap::new(),
             post_complete_presence: HashMap::new(),
+            post_complete_heartbeat: HashMap::new(),
         }
     }
@@
             st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
             st.pending_early_fail.retain(|(tid, _), _| *tid != tab_id);
             st.turn_complete_seen.retain(|(tid, _), _| *tid != tab_id);
             st.post_complete_presence.retain(|(tid, _), _| *tid != tab_id);
+            st.post_complete_heartbeat.retain(|(tid, _), _| *tid != tab_id);
         }
@@
                 Some("assistant_message_add") => {
                     st.turn_complete_seen.remove(&key);
                     st.post_complete_presence.remove(&key);
+                    st.post_complete_heartbeat.remove(&key);
                 }
                 Some("turn_complete") if st.pending_resp.contains_key(&key) => {
                     let frame_counter = st.frame_counter;
                     st.turn_complete_seen.entry(key).or_insert(frame_counter);
                     st.post_complete_presence.insert(key, 0);
+                    st.post_complete_heartbeat.insert(key, 0);
                     append_outbound_event(
                         "OUTBOUND_EARLY_SIGNAL",
                         json!({
@@
                 Some("heartbeat") if st.pending_resp.contains_key(&key) => {
                     if let Some(turn_complete_frame) = st.turn_complete_seen.get(&key).copied() {
+                        let count = st
+                            .post_complete_heartbeat
+                            .entry(key)
+                            .and_modify(|seen| *seen += 1)
+                            .or_insert(1);
+                        let heartbeat_count = *count;
+                        if heartbeat_count == 8 {
+                            if let Some(tx) = st.pending_early_fail.remove(&key) {
+                                let _ = tx.send("heartbeat_after_turn_complete".to_string());
+                            }
+                        }
                         append_outbound_event(
                             "OUTBOUND_EARLY_SIGNAL",
                             json!({
                                 "signal": "heartbeat_after_turn_complete",
                                 "tabId": tab_id,
                                 "turnId": turn_id,
                                 "frame_counter": st.frame_counter,
                                 "turn_complete_frame_counter": turn_complete_frame,
+                                "heartbeat_count": heartbeat_count,
                             }),
                         );
                     }
                 }
@@
                 if let Some(tx) = st.pending_resp.remove(&(tab_id, turn_id)) {
                     st.pending_turn_id.remove(&tab_id);
                     st.pending_turn_lease.remove(&key);
                     st.turn_complete_seen.remove(&key);
                     st.post_complete_presence.remove(&key);
+                    st.post_complete_heartbeat.remove(&key);
                     let endpoint_id = st.tab_owners.get(&tab_id).cloned();
                     if let Some(endpoint_id) = endpoint_id.as_deref() {
                         release_tab_locked(&mut st, endpoint_id, tab_id, true);
@@
                     st.pending_turn_id.remove(&tab_id);
                     st.pending_turn_lease.remove(&key);
                     st.turn_complete_seen.remove(&key);
                     st.post_complete_presence.remove(&key);
+                    st.post_complete_heartbeat.remove(&key);
                     if let Some(owner) = endpoint_id.as_deref() {
                         release_tab_locked(&mut st, owner, tab_id, true);
                     } else {
@@
     st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
     st.pending_early_fail.retain(|(tid, _), _| *tid != tab_id);
     st.turn_complete_seen.retain(|(tid, _), _| *tid != tab_id);
     st.post_complete_presence.retain(|(tid, _), _| *tid != tab_id);
+    st.post_complete_heartbeat.retain(|(tid, _), _| *tid != tab_id);
     st.assemblers.remove(&tab_id);
*** Update File: /mnt/data/canon-mini-agent/src/llm_runtime/parsers.rs
@@
     if let Some(items) = obj.get("items").and_then(|i| i.as_array()) {
-        let result = classify_calpico_array(items);
+        let result = classify_calpico_items(items);
         if !matches!(result, FrameResult::Ignore) {
             return result;
         }
@@
 fn classify_calpico_array(arr: &[Value]) -> FrameResult {
     for envelope in arr {
         let result = classify_calpico_envelope(envelope);
         if !matches!(result, FrameResult::Ignore) {
             return result;
         }
     }
     FrameResult::Ignore
 }
+
+fn classify_calpico_items(items: &[Value]) -> FrameResult {
+    for item in items {
+        let result = classify_calpico_envelope(item);
+        if !matches!(result, FrameResult::Ignore) {
+            return result;
+        }
+        let Some(role) = item.get("role").and_then(|r| r.as_str()) else {
+            continue;
+        };
+        if role != "assistant" {
+            continue;
+        }
+        let text = item
+            .get("content")
+            .and_then(|c| c.get("text"))
+            .and_then(|t| t.as_str())
+            .unwrap_or("");
+        if !text.is_empty() {
+            return FrameResult::Snapshot(text.to_string());
+        }
+    }
+    FrameResult::Ignore
+}
@@
     fn chatgpt_group_parses_direct_assistant_message_object() {
         let raw = r#"{"id":"msg","role":"assistant","raw_messages":[{"author":{"role":"assistant"},"channel":"final","content":{"parts":["```json\n{\"action\":\"message\"}\n```"]}}]}"#;
         match classify_frame(SiteType::ChatGptGroup, raw) {
             FrameResult::Snapshot(text) => assert!(text.contains(r#""action":"message""#)),
             other => panic!("expected snapshot, got {other:?}"),
         }
     }
+
+    #[test]
+    fn chatgpt_group_parses_room_read_assistant_snapshot_items() {
+        let raw = r#"{"items":[{"id":"msg","role":"assistant","content":{"text":"```json\n{\"action\":\"python\"}\n```"}}]}"#;
+        match classify_frame(SiteType::ChatGptGroup, raw) {
+            FrameResult::Snapshot(text) => assert!(text.contains(r#""action":"python""#)),
+            other => panic!("expected snapshot, got {other:?}"),
+        }
+    }
 }
*** End Patch
"""
res = subprocess.run(["/opt/apply_patch/bin/apply_patch"], input=patch.encode(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
print(res.stdout.decode())
print("returncode", res.returncode)

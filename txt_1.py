from pathlib import Path
import subprocess, textwrap, json

repo = Path('/mnt/data/canon-mini-agent-extracted/canon-mini-agent')
target = repo / 'src/llm_runtime/chromium_backend.rs'
text = target.read_text()

assert 'fn lease_token_from_value(value: &Value) -> Option<String> {' in text
assert '        "SUBMIT_ACK" => {' in text
assert '        // Clear frame logs so each run starts fresh.' in text

patch = r"""*** Begin Patch
*** Update File: /mnt/data/canon-mini-agent-extracted/canon-mini-agent/src/llm_runtime/chromium_backend.rs
@@
 fn lease_token_from_value(value: &Value) -> Option<String> {
     value
         .get("leaseToken")
         .and_then(Value::as_str)
         .or_else(|| value.get("lease_token").and_then(Value::as_str))
         .map(ToOwned::to_owned)
 }
+
+fn turn_id_from_value(value: &Value) -> Option<u64> {
+    value
+        .get("turnId")
+        .or_else(|| value.get("turn_id"))
+        .and_then(|value| match value {
+            Value::Number(number) => number.as_u64(),
+            Value::String(text) => text.trim().parse::<u64>().ok(),
+            _ => None,
+        })
+}
+
+fn payload_value_from_message(value: &Value) -> Option<Value> {
+    match value.get("payload") {
+        Some(Value::Object(map)) => Some(Value::Object(map.clone())),
+        Some(Value::String(text)) if text.trim_start().starts_with('{') => {
+            serde_json::from_str::<Value>(text).ok()
+        }
+        _ => None,
+    }
+}
@@
     {
         let mut st = state.lock().await;
         st.out_tx = Some(tx_out.clone());
         eprintln!("[chromium] extension connected");
-        // Clear frame logs so each run starts fresh.
-        for name in &["all.jsonl", "inbound.jsonl", "assembled.jsonl"] {
-            let _ = std::fs::remove_file(format!("{FRAMES_DIR}/{name}"));
-        }
         for frame in st.replay_queue.drain(..) {
             let _ = tx_out.try_send(Message::Text(frame.to_string().into()));
         }
     }
@@
         "SUBMIT_ACK" => {
             let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                 Some(id) => id as u32,
                 None => return,
             };
-            let turn_id = match msg.get("turnId").and_then(|v| v.as_u64()) {
+            let payload_value = payload_value_from_message(&msg);
+            let turn_id = match turn_id_from_value(&msg)
+                .or_else(|| payload_value.as_ref().and_then(turn_id_from_value))
+            {
                 Some(id) => id,
                 None => return,
             };
-            let ack_lease_token = match lease_token_from_value(&msg) {
+            let ack_lease_token = match lease_token_from_value(&msg)
+                .or_else(|| payload_value.as_ref().and_then(lease_token_from_value))
+            {
                 Some(token) => token,
                 None => return,
             };
             let mut st = state.lock().await;
             let Some(expected_lease_token) = st.pending_turn_lease.get(&(tab_id, turn_id)).cloned() else {
@@
     #[tokio::test]
+    async fn submit_ack_accepts_snake_case_turn_id() {
+        let state = Arc::new(Mutex::new(State::new()));
+        let (ack_tx, ack_rx) = oneshot::channel::<String>();
+        {
+            let mut st = state.lock().await;
+            st.pending_ack.insert((7, 11), ack_tx);
+            st.pending_turn_id.insert(7, 11);
+            st.pending_turn_lease
+                .insert((7, 11), "lease-expected".to_string());
+        }
+
+        let raw = json!({
+            "type": "SUBMIT_ACK",
+            "tabId": 7,
+            "turn_id": 11,
+            "lease_token": "lease-expected",
+        })
+        .to_string();
+
+        handle_inbound(&raw, &state).await;
+
+        let ack = ack_rx.await.expect("submit ack should be delivered");
+        let parsed: Value = serde_json::from_str(&ack).expect("ack payload should be valid json");
+        assert_eq!(parsed.get("turn_id").and_then(|v| v.as_u64()), Some(11));
+        assert_eq!(
+            parsed.get("lease_token").and_then(|v| v.as_str()),
+            Some("lease-expected")
+        );
+    }
+
+    #[tokio::test]
+    async fn submit_ack_accepts_payload_wrapped_fields() {
+        let state = Arc::new(Mutex::new(State::new()));
+        let (ack_tx, ack_rx) = oneshot::channel::<String>();
+        {
+            let mut st = state.lock().await;
+            st.pending_ack.insert((7, 11), ack_tx);
+            st.pending_turn_id.insert(7, 11);
+            st.pending_turn_lease
+                .insert((7, 11), "lease-expected".to_string());
+        }
+
+        let raw = json!({
+            "type": "SUBMIT_ACK",
+            "tabId": 7,
+            "payload": {
+                "turn_id": 11,
+                "lease_token": "lease-expected",
+            },
+        })
+        .to_string();
+
+        handle_inbound(&raw, &state).await;
+
+        let ack = ack_rx.await.expect("payload-wrapped submit ack should be delivered");
+        let parsed: Value = serde_json::from_str(&ack).expect("ack payload should be valid json");
+        assert_eq!(parsed.get("turn_id").and_then(|v| v.as_u64()), Some(11));
+        assert_eq!(
+            parsed.get("lease_token").and_then(|v| v.as_str()),
+            Some("lease-expected")
+        );
+    }
+
+    #[tokio::test]
     async fn inbound_message_uses_inbound_turn_id_when_submit_only_ack_cleared_pending_turn() {
         let state = Arc::new(Mutex::new(State::new()));
         let lease_token = "lease-submit-only";
         {
             let mut st = state.lock().await;
*** End Patch
"""
res = subprocess.run(
    ['/opt/apply_patch/bin/apply_patch'],
    input=patch.encode(),
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
)
print(res.stdout.decode())
print("returncode", res.returncode)

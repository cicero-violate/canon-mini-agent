from pathlib import Path
import subprocess, textwrap, json, os, re

root = Path('.')

patch = r"""*** Begin Patch
*** Update File: canon-chromium-extension/inject.js
@@
-  window.__currentTurnId           = window.__currentTurnId           || null;
+  window.__currentTurnId           = window.__currentTurnId           || null;
+  window.__currentLeaseToken       = window.__currentLeaseToken       || null;
@@
   function normalizeTurnId(turnId) {
     if (typeof turnId === "number" && Number.isFinite(turnId)) return String(turnId);
     if (typeof turnId === "string" && turnId.trim().length > 0) return turnId.trim();
     return null;
   }
+
+  function normalizeLeaseToken(leaseToken) {
+    if (typeof leaseToken === "number" && Number.isFinite(leaseToken)) return String(leaseToken);
+    if (typeof leaseToken === "string" && leaseToken.trim().length > 0) return leaseToken.trim();
+    return null;
+  }
@@
   function emitInbound(chunk) {
     ensureActiveTurnId("inbound_chunk");
     const payload = {
       turn_id: window.__currentTurnId,
+      lease_token: window.__currentLeaseToken,
       chunk,
       ts: Date.now()
     };
     window.postMessage({ type: "INBOUND_MESSAGE", payload }, "*");
   }
@@
               emitInbound(line);
               if (line.trim() === "data: [DONE]" || line.trim() === "[DONE]") {
                 window.__currentTurnId = null;
+                window.__currentLeaseToken = null;
               }
             }
           } else {
             emitInbound(chunk);
             if (chunk.trim() === "data: [DONE]" || chunk.trim() === "[DONE]") {
               window.__currentTurnId = null;
+              window.__currentLeaseToken = null;
             }
           }
@@
-    const { text, mode, turn_id } = event.data.payload || {};
+    const { text, mode, turn_id } = event.data.payload || {};
+    const leaseToken = normalizeLeaseToken(
+      event.data?.payload?.leaseToken ?? event.data?.payload?.lease_token
+    );
     if (typeof text !== "string") return;
@@
 
     window.__currentTurnId = turn_id ?? null;
+    window.__currentLeaseToken = leaseToken;
     window.__promptInjectionMode = mode || "auto";
@@
         if (sendBtn && !sendBtn.disabled) {
           sendBtn.click();
-          window.postMessage({ type: "SUBMIT_ACK", turn_id: window.__currentTurnId, ts: Date.now() }, "*");
+          window.postMessage({
+            type: "SUBMIT_ACK",
+            turn_id: window.__currentTurnId,
+            lease_token: window.__currentLeaseToken,
+            ts: Date.now()
+          }, "*");
         } else {
           if (submitViaEnter()) {
-            window.postMessage({ type: "SUBMIT_ACK", turn_id: window.__currentTurnId, ts: Date.now() }, "*");
+            window.postMessage({
+              type: "SUBMIT_ACK",
+              turn_id: window.__currentTurnId,
+              lease_token: window.__currentLeaseToken,
+              ts: Date.now()
+            }, "*");
           }
         }
*** Update File: canon-chromium-extension/content.js
@@
-  let lastTurnId = null;
+  let lastTurnId = null;
+  let lastLeaseToken = null;
@@
   function normalizeTurnId(turnId) {
     if (typeof turnId === "number" && Number.isFinite(turnId)) return String(turnId);
     if (typeof turnId === "string" && turnId.trim().length > 0) return turnId.trim();
     return null;
   }
+
+  function normalizeLeaseToken(leaseToken) {
+    if (typeof leaseToken === "number" && Number.isFinite(leaseToken)) return String(leaseToken);
+    if (typeof leaseToken === "string" && leaseToken.trim().length > 0) return leaseToken.trim();
+    return null;
+  }
@@
     if (event.data?.type === "INBOUND_MESSAGE") {
       const payload = event.data.payload;
       let patched = payload;
       if (payload && typeof payload === "object") {
-        if (payload.turn_id == null && lastTurnId != null) {
-          patched = { ...payload, turn_id: lastTurnId };
-        }
+        patched = { ...payload };
+        if (patched.turn_id == null && lastTurnId != null) {
+          patched.turn_id = lastTurnId;
+        }
+        if (patched.lease_token == null && patched.leaseToken == null && lastLeaseToken != null) {
+          patched.lease_token = lastLeaseToken;
+        }
       } else if (typeof payload === "string") {
         try {
           const obj = JSON.parse(payload);
-          if (obj && obj.turn_id == null && lastTurnId != null) {
-            obj.turn_id = lastTurnId;
+          if (obj) {
+            if (obj.turn_id == null && lastTurnId != null) {
+              obj.turn_id = lastTurnId;
+            }
+            if (obj.lease_token == null && obj.leaseToken == null && lastLeaseToken != null) {
+              obj.lease_token = lastLeaseToken;
+            }
             patched = obj;
           }
         } catch {}
@@
         {
           type: "SUBMIT_ACK",
           turn_id: event.data.turn_id ?? lastTurnId ?? null,
+          lease_token:
+            event.data.lease_token ??
+            event.data.leaseToken ??
+            lastLeaseToken ??
+            null,
           ts: event.data.ts ?? Date.now()
         },
         () => void chrome.runtime.lastError
@@
     if (message?.type === "OUTBOUND_SUBMIT") {
       const turnId = message?.payload?.turn_id;
       if (typeof turnId === "number") {
         lastTurnId = turnId;
       }
+      const leaseToken = normalizeLeaseToken(
+        message?.payload?.leaseToken ?? message?.payload?.lease_token
+      );
+      if (leaseToken != null) {
+        lastLeaseToken = leaseToken;
+      }
       if (shouldDropOutbound(turnId, message.payload)) {
         console.log("[CS] OUTBOUND_SUBMIT deduped", turnId);
         sendResponse({ ok: true, deduped: true });
         return true;
       }
*** Update File: canon-chromium-extension/background.js
@@
   if (message?.type === "SUBMIT_ACK") {
     sendToOwner(tabId, {
       type: "SUBMIT_ACK",
       tabId,
       turnId: message.turn_id ?? null,
+      leaseToken: message.lease_token ?? message.leaseToken ?? null,
       ts: message.ts ?? Date.now()
     });
     sendResponse({ ok: true });
     return true;
   }
@@
   if (msg?.type === "TURN") {
     const targetTabId = msg.tabId;
     if (!targetTabId) return;
     console.log("[BG] TURN → sendToTab", targetTabId, "text length:", msg.text?.length);
     const turnId = msg.turnId ?? null;
+    const leaseToken = msg.leaseToken ?? msg.lease_token ?? null;
     focusTabAndSubmit(targetTabId, {
       text: msg.text,
       mode: "auto",
-      turn_id: turnId
+      turn_id: turnId,
+      leaseToken
     });
     return;
   }
*** End Patch
"""

res = subprocess.run(
    ['/opt/apply_patch/bin/apply_patch'],
    input=patch.encode(),
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    cwd=str(root),
)
print(res.stdout.decode())
print("returncode", res.returncode)

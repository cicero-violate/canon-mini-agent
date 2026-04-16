from pathlib import Path
patch_text = r"""*** Begin Patch
*** Update File: canon-chromium-extension/inject.js
@@
-  window.__currentTurnId           = window.__currentTurnId           || null;
-  window.__nextSyntheticTurnId     = window.__nextSyntheticTurnId     || (Date.now() * 1000);
+  window.__currentTurnId           = window.__currentTurnId           || null;
+  window.__currentLeaseToken       = window.__currentLeaseToken       || null;
+  window.__nextSyntheticTurnId     = window.__nextSyntheticTurnId     || (Date.now() * 1000);
@@
   function normalizeTurnId(turnId) {
     if (typeof turnId === "number" && Number.isFinite(turnId)) return

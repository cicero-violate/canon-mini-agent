(function () {
  // Guard against re-injection and invalidated extension context
  if (window.__ContentBridgeInstalled) return;
  if (!chrome?.runtime?.id) return;
  window.__ContentBridgeInstalled = true;

  let lastTurnId = null;
  let lastLeaseToken = null;
  const seenOutboundTurns = new Map();
  const seenOutboundIdempotency = new Map();
  const seenOutboundSignatures = new Map();
  const OUTBOUND_DEDUP_WINDOW_MS = 60_000;

  function normalizeTurnId(turnId) {
    if (typeof turnId === "number" && Number.isFinite(turnId)) return String(turnId);
    if (typeof turnId === "string" && turnId.trim().length > 0) return turnId.trim();
    return null;
  }

  function normalizeLeaseToken(leaseToken) {
    if (typeof leaseToken === "number" && Number.isFinite(leaseToken)) return String(leaseToken);
    if (typeof leaseToken === "string" && leaseToken.trim().length > 0) return leaseToken.trim();
    return null;
  }

  function extractIdempotencyKey(payload) {
    const key =
      payload?.idempotency_key ??
      payload?.idempotencyKey ??
      payload?.idempotency ??
      null;
    if (typeof key === "number" && Number.isFinite(key)) return String(key);
    if (typeof key === "string" && key.trim().length > 0) return key.trim();
    return null;
  }

  function hashPayload(payload) {
    const text = typeof payload?.text === "string" ? payload.text : "";
    let hash = 2166136261;
    for (let i = 0; i < text.length; i++) {
      hash ^= text.charCodeAt(i);
      hash = Math.imul(hash, 16777619);
    }
    const mode = payload?.mode || "";
    const turn = normalizeTurnId(payload?.turn_id) ?? "null";
    const idem = extractIdempotencyKey(payload) ?? "none";
    return `${turn}:${idem}:${mode}:${text.length}:${hash >>> 0}`;
  }

  function shouldDropOutbound(turnId, payload) {
    const now = Date.now();
    const normalizedTurn = normalizeTurnId(turnId);
    if (normalizedTurn) {
      const seenAt = seenOutboundTurns.get(normalizedTurn);
      if (seenAt && now - seenAt < OUTBOUND_DEDUP_WINDOW_MS) {
        return true;
      }
      seenOutboundTurns.set(normalizedTurn, now);
    }
    const idempotencyKey = extractIdempotencyKey(payload);
    if (idempotencyKey) {
      const seenAt = seenOutboundIdempotency.get(idempotencyKey);
      if (seenAt && now - seenAt < OUTBOUND_DEDUP_WINDOW_MS) {
        return true;
      }
      seenOutboundIdempotency.set(idempotencyKey, now);
    }
    const signature = hashPayload(payload);
    const sigSeenAt = seenOutboundSignatures.get(signature);
    if (sigSeenAt && now - sigSeenAt < OUTBOUND_DEDUP_WINDOW_MS) {
      return true;
    }
    seenOutboundSignatures.set(signature, now);
    if (seenOutboundTurns.size > 1000 || seenOutboundIdempotency.size > 1000 || seenOutboundSignatures.size > 1000) {
      const cutoff = now - OUTBOUND_DEDUP_WINDOW_MS;
      for (const [key, ts] of seenOutboundTurns) {
        if (ts < cutoff) {
          seenOutboundTurns.delete(key);
        }
      }
      for (const [key, ts] of seenOutboundIdempotency) {
        if (ts < cutoff) {
          seenOutboundIdempotency.delete(key);
        }
      }
      for (const [key, ts] of seenOutboundSignatures) {
        if (ts < cutoff) {
          seenOutboundSignatures.delete(key);
        }
      }
    }
    return false;
  }

  // Inject main bridge
  function injectScript(src) {
    const s = document.createElement("script");
    s.src = chrome.runtime.getURL(src);
    (document.head || document.documentElement).appendChild(s);
    s.onload = () => s.remove();
  }

  const host = location.hostname;
  if (host === "gemini.google.com") {
    injectScript("request_gemini.js");
  } else {
    injectScript("inject.js");
    // Inject only the relevant hook based on URL shape:
    // - group chat: /gg/...
    // - private chat: /c/... or root
    if (location.pathname.startsWith("/gg/")) {
      injectScript("request_hook_group.js");
    } else {
      injectScript("request_hook_private.js");
    }
  }


  // inject.js → content.js: bridge installed signal
  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    if (event.data?.type === "BRIDGE_READY") {
      chrome.runtime.sendMessage(
        { type: "CONTENT_READY", url: location.href },
        () => void chrome.runtime.lastError
      );
    }
  });

  // Page → Background: stream captures
  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    if (event.data?.type === "TURN_STARTED") {
      const turnId = event.data.turn_id;
      if (typeof turnId === "number") {
        lastTurnId = turnId;
      }
      chrome.runtime.sendMessage(
        {
          type: "TURN_STARTED",
          turn_id: typeof turnId === "number" ? turnId : null,
          source: event.data.source ?? null,
          ts: event.data.ts ?? Date.now()
        },
        () => void chrome.runtime.lastError
      );
      return;
    }
    if (event.data?.type === "INBOUND_MESSAGE") {
      const payload = event.data.payload;
      let patched = payload;
      if (payload && typeof payload === "object") {
        let mutated = false;
        let nextPayload = payload;
        if (payload.turn_id == null && lastTurnId != null) {
          nextPayload = { ...nextPayload, turn_id: lastTurnId };
          mutated = true;
        }
        const payloadLeaseToken = normalizeLeaseToken(
          payload.lease_token ?? payload.leaseToken
        );
        if (!payloadLeaseToken && lastLeaseToken != null) {
          nextPayload = mutated ? nextPayload : { ...nextPayload };
          nextPayload.lease_token = lastLeaseToken;
          mutated = true;
        }
        patched = mutated ? nextPayload : payload;
      } else if (typeof payload === "string") {
        try {
          const obj = JSON.parse(payload);
          let mutated = false;
          if (obj && obj.turn_id == null && lastTurnId != null) {
            obj.turn_id = lastTurnId;
            mutated = true;
          }
          const payloadLeaseToken = normalizeLeaseToken(
            obj?.lease_token ?? obj?.leaseToken
          );
          if (obj && !payloadLeaseToken && lastLeaseToken != null) {
            obj.lease_token = lastLeaseToken;
            mutated = true;
          }
          if (mutated) patched = obj;
        } catch {}
      }
      chrome.runtime.sendMessage({ type: "INBOUND_MESSAGE", payload: patched }, () => void chrome.runtime.lastError);
    }
    if (event.data?.type === "NEW_CHAT_DONE") {
      console.log("[CS] NEW_CHAT_DONE from page");
      chrome.runtime.sendMessage({ type: "NEW_CHAT_DONE" }, () => void chrome.runtime.lastError);
    }
    if (event.data?.type === "TEMP_CHAT_DONE") {
      console.log("[CS] TEMP_CHAT_DONE from page");
      chrome.runtime.sendMessage({ type: "TEMP_CHAT_DONE" }, () => void chrome.runtime.lastError);
    }
    if (event.data?.type === "SUBMIT_ACK") {
      const leaseToken = normalizeLeaseToken(
        event.data.lease_token ?? event.data.leaseToken ?? lastLeaseToken
      );
      chrome.runtime.sendMessage(
        {
          type: "SUBMIT_ACK",
          turn_id: event.data.turn_id ?? lastTurnId ?? null,
          lease_token: leaseToken,
          ts: event.data.ts ?? Date.now()
        },
        () => void chrome.runtime.lastError
      );
    }
  });

  // Background → Page: prompt injection
  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (message?.type === "OUTBOUND_SUBMIT") {
      const turnId = message?.payload?.turn_id;
      const leaseToken = normalizeLeaseToken(
        message?.payload?.leaseToken ?? message?.payload?.lease_token
      );
      if (typeof turnId === "number") {
        lastTurnId = turnId;
      }
      lastLeaseToken = leaseToken;
      if (shouldDropOutbound(turnId, message.payload)) {
        console.log("[CS] OUTBOUND_SUBMIT deduped", turnId);
        sendResponse({ ok: true, deduped: true });
        return true;
      }
      console.log("[CS] OUTBOUND_SUBMIT received, posting to page");
      window.postMessage({ type: "OUTBOUND_SUBMIT", payload: message.payload }, "*");
      sendResponse({ ok: true });
      return true;
    }
    if (message?.type === "NEW_CHAT") {
      window.postMessage({ type: "NEW_CHAT" }, "*");
      sendResponse({ ok: true });
      return true;
    }
    if (message?.type === "TEMP_CHAT") {
      window.postMessage({ type: "TEMP_CHAT" }, "*");
      sendResponse({ ok: true });
      return true;
    }
    sendResponse({ ok: false });
    return true;
  });
})();

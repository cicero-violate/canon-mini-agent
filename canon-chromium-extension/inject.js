(function () {
  if (window.__BridgeInstalled) return;
  window.__BridgeInstalled = true;

  // ── Globals for prompt injection (set by request hooks) ──────────────────
  window.__pendingPromptInjection  = window.__pendingPromptInjection  || null;
  window.__promptInjectionMode     = window.__promptInjectionMode     || "auto";
  window.__promptInjectionQueue    = window.__promptInjectionQueue    || [];
  window.__currentTurnId           = window.__currentTurnId           || null;
  window.__currentLeaseToken       = window.__currentLeaseToken       || null;
  window.__nextSyntheticTurnId     = window.__nextSyntheticTurnId     || (Date.now() * 1000);
  const __seenOutboundTurns = new Map();
  const __seenOutboundIdempotency = new Map();
  const __seenOutboundSignatures = new Map();
  const OUTBOUND_DEDUP_WINDOW_MS = 60_000;

  function nextSyntheticTurnId() {
    const next = Number(window.__nextSyntheticTurnId || (Date.now() * 1000));
    window.__nextSyntheticTurnId = next + 1;
    return next;
  }

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

  function shouldDropOutbound(payload) {
    const now = Date.now();
    const normalizedTurn = normalizeTurnId(payload?.turn_id);
    if (normalizedTurn) {
      const seenAt = __seenOutboundTurns.get(normalizedTurn);
      if (seenAt && now - seenAt < OUTBOUND_DEDUP_WINDOW_MS) return true;
      __seenOutboundTurns.set(normalizedTurn, now);
    }
    const idempotencyKey = extractIdempotencyKey(payload);
    if (idempotencyKey) {
      const seenAt = __seenOutboundIdempotency.get(idempotencyKey);
      if (seenAt && now - seenAt < OUTBOUND_DEDUP_WINDOW_MS) return true;
      __seenOutboundIdempotency.set(idempotencyKey, now);
    }
    const signature = hashPayload(payload);
    const sigSeenAt = __seenOutboundSignatures.get(signature);
    if (sigSeenAt && now - sigSeenAt < OUTBOUND_DEDUP_WINDOW_MS) return true;
    __seenOutboundSignatures.set(signature, now);
    if (__seenOutboundTurns.size > 1000 || __seenOutboundIdempotency.size > 1000 || __seenOutboundSignatures.size > 1000) {
      const cutoff = now - OUTBOUND_DEDUP_WINDOW_MS;
      for (const [key, ts] of __seenOutboundTurns) {
        if (ts < cutoff) __seenOutboundTurns.delete(key);
      }
      for (const [key, ts] of __seenOutboundIdempotency) {
        if (ts < cutoff) __seenOutboundIdempotency.delete(key);
      }
      for (const [key, ts] of __seenOutboundSignatures) {
        if (ts < cutoff) __seenOutboundSignatures.delete(key);
      }
    }
    return false;
  }

  function ensureActiveTurnId(source) {
    if (typeof window.__currentTurnId === "number") {
      return window.__currentTurnId;
    }
    const turnId = nextSyntheticTurnId();
    window.__currentTurnId = turnId;
    window.postMessage({
      type: "TURN_STARTED",
      turn_id: turnId,
      source,
      ts: Date.now()
    }, "*");
    return turnId;
  }

  function emitInbound(chunk) {
    ensureActiveTurnId("inbound_chunk");
    const payload = {
      turn_id: window.__currentTurnId,
      lease_token: window.__currentLeaseToken,
      chunk,
      ts: Date.now()
    };
    window.postMessage({ type: "INBOUND_MESSAGE", payload }, "*");
  }

  // ── WebSocket hook (Calpico transport) ───────────────────────────────────
  const __OrigWS = window.WebSocket;
  window.WebSocket = function (url, protocols) {
    const ws = protocols ? new __OrigWS(url, protocols) : new __OrigWS(url);
    ws.addEventListener("message", (ev) => {
      ensureActiveTurnId("websocket_message");
      let data = typeof ev.data === "string"
        ? ev.data
        : ev.data instanceof ArrayBuffer
          ? new TextDecoder().decode(ev.data)
          : "";
      if (data) emitInbound(data);
    });
    return ws;
  };
  window.WebSocket.prototype = __OrigWS.prototype;

  // ── Fetch hook (SSE + Calpico streaming) ─────────────────────────────────
  const TARGETS = [
    { origin: "https://chatgpt.com",     path: "/backend-api/f/conversation" },
    { origin: "https://chat.openai.com", path: "/backend-api/f/conversation" },
    { origin: "https://chatgpt.com",     path: "/backend-api/calpico" },
    { origin: "https://chat.openai.com", path: "/backend-api/calpico" },
  ];

  function matchesTarget(input) {
    try {
      const url = new URL(input, location.href);
      for (const t of TARGETS) {
        if (url.origin === t.origin && url.pathname.startsWith(t.path)) return true;
      }
    } catch {}
    return false;
  }

  const __origFetch = window.fetch;
  window.fetch = async function (input, init) {
    // Silence Datadog beacons
    if (typeof input === "string" && input.includes("browser-intake-datadoghq.com"))
      return new Response(null, { status: 204 });

    const isTarget = matchesTarget(typeof input === "string" ? input : input?.url);
    const requestMethod = String(init?.method || input?.method || "GET").toUpperCase();
    if (isTarget && requestMethod !== "GET") {
      ensureActiveTurnId("fetch_request");
    }
    const response = await __origFetch(input, init);
    if (!isTarget || !response.body) return response;

    const [toPage, toCapture] = response.body.tee();

    (async () => {
      const reader  = toCapture.getReader();
      const decoder = new TextDecoder();
      let   buffer  = "";
      try {
        while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          const chunk = decoder.decode(value, { stream: true });
          if (chunk.includes("\n")) {
            buffer += chunk;
            const lines = buffer.split("\n");
            buffer = lines.pop();
            for (const line of lines) {
              emitInbound(line);
              if (line.trim() === "data: [DONE]" || line.trim() === "[DONE]") {
                window.__currentTurnId = null;
                window.__currentLeaseToken = null;
              }
            }
          } else {
            emitInbound(chunk);
            if (chunk.trim() === "data: [DONE]" || chunk.trim() === "[DONE]") {
              window.__currentTurnId = null;
              window.__currentLeaseToken = null;
            }
          }
        }
      } catch {}
    })();

    return new Response(toPage, {
      status:     response.status,
      statusText: response.statusText,
      headers:    response.headers,
    });
  };

  // ── Signal content.js that bridge is installed — wait for editor ─────────
  function signalReadyWhenEditorExists() {
    const editor = document.querySelector('div[contenteditable="true"]');
    if (editor) {
      console.log("[INJ] editor found, signalling BRIDGE_READY");
      window.postMessage({ type: "BRIDGE_READY" }, "*");
      return;
    }
    console.log("[INJ] editor not yet in DOM, waiting via MutationObserver");
    const observeTarget = document.body || document.documentElement;
    const observer = new MutationObserver(() => {
      const ed = document.querySelector('div[contenteditable="true"]');
      if (ed) {
        observer.disconnect();
        console.log("[INJ] editor appeared, signalling BRIDGE_READY");
        window.postMessage({ type: "BRIDGE_READY" }, "*");
      }
    });
    observer.observe(observeTarget, { childList: true, subtree: true });
  }
  signalReadyWhenEditorExists();

  // ── OUTBOUND_SUBMIT handler (Rust → ChatGPT input) ───────────────────────
  function waitForEditor(callback) {
    const editor = document.querySelector('div[contenteditable="true"]');
    if (editor) { callback(editor); return; }
    const observer = new MutationObserver(() => {
      const ed = document.querySelector('div[contenteditable="true"]');
      if (ed) { observer.disconnect(); callback(ed); }
    });
    observer.observe(document.body, { childList: true, subtree: true });
  }

  function waitForSendBtn(callback) {
    const btn = document.querySelector('button[data-testid="send-button"]');
    if (btn && !btn.disabled) { callback(btn); return; }
    const observer = new MutationObserver(() => {
      const b = document.querySelector('button[data-testid="send-button"]');
      if (b && !b.disabled) { observer.disconnect(); callback(b); }
    });
    observer.observe(document.body, { childList: true, subtree: true, attributes: true });
  }

  function submitViaEnter() {
    const editor = document.querySelector('div[contenteditable="true"]');
    if (!editor) return false;

    editor.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        code: "Enter",
        which: 13,
        keyCode: 13,
        bubbles: true,
        cancelable: true
      })
    );
    return true;
  }

  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    if (event.data?.type !== "OUTBOUND_SUBMIT") return;

    const { text, mode, turn_id } = event.data.payload || {};
    const leaseToken = normalizeLeaseToken(
      event.data?.payload?.leaseToken ?? event.data?.payload?.lease_token
    );
    if (typeof text !== "string") return;
    if (shouldDropOutbound(event.data.payload)) {
      console.log("[INJ] OUTBOUND_SUBMIT deduped", turn_id ?? null);
      return;
    }
    console.log("[INJ] OUTBOUND_SUBMIT received, text length:", text.length, "mode:", mode);

    window.__currentTurnId = turn_id ?? null;
    window.__currentLeaseToken = leaseToken;
    window.__promptInjectionMode = mode || "auto";

    if (mode === "buffer") {
      window.__promptInjectionQueue ||= [];
      if (text) window.__promptInjectionQueue.push(text);
      const editor = document.querySelector('div[contenteditable="true"]');
      if (editor && editor.textContent !== "<PROMPT>") {
        editor.textContent = "<PROMPT>";
        editor.dispatchEvent(new Event("input", { bubbles: true }));
      }
      return;
    }

    // AUTO MODE
    if (text) {
      window.__pendingPromptInjection = text;
      const editor = document.querySelector('div[contenteditable="true"]');
      if (editor) {
        editor.textContent = "<PROMPT>";
        editor.dispatchEvent(new Event("input", { bubbles: true }));
      }

      setTimeout(() => {
        if (window.__promptInjectionQueue?.length > 0) {
          window.__pendingPromptInjection =
            window.__promptInjectionQueue.join("\n\n");
          window.__promptInjectionQueue = [];
        }

        const sendBtn =
          document.querySelector('button[data-testid="send-button"]');

        if (sendBtn && !sendBtn.disabled) {
          sendBtn.click();
          window.postMessage({
            type: "SUBMIT_ACK",
            turn_id: window.__currentTurnId,
            lease_token: window.__currentLeaseToken,
            ts: Date.now()
          }, "*");
        } else {
          if (submitViaEnter()) {
            window.postMessage({
              type: "SUBMIT_ACK",
              turn_id: window.__currentTurnId,
              lease_token: window.__currentLeaseToken,
              ts: Date.now()
            }, "*");
          }
        }
      }, 100);
    }
  });

  function isStreaming() {
    const stopByTestId = document.querySelector('button[data-testid="stop-button"]');
    if (stopByTestId) return true;
    const stopByAria = document.querySelector('button[aria-label="Stop generating"]');
    if (stopByAria) return true;
    const buttons = Array.from(document.querySelectorAll('button'));
    return buttons.some(b => (b.textContent || "").toLowerCase().includes("stop"));
  }

  function clickNewChat() {
    const btn = document.querySelector('a[data-testid="create-new-chat-button"]');
    if (btn) { btn.click(); return true; }
    const link = document.querySelector('a[href="/"][data-testid="create-new-chat-button"]');
    if (link) { link.click(); return true; }
    return false;
  }

  function clickTempChat() {
    const btn = document.querySelector('button[aria-label="Turn on temporary chat"]');
    if (btn) { btn.click(); return true; }
    return false;
  }

  function isTempChatEnabled() {
    if (location.search.includes("temporary-chat=true")) return true;
    const offBtn = document.querySelector('button[aria-label="Turn off temporary chat"]');
    if (offBtn) return true;
    const badge = document.querySelector('[data-testid="temporary-chat-badge"]');
    if (badge) return true;
    return false;
  }

  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    if (event.data?.type === "NEW_CHAT") {
      const deadline = Date.now() + 20000;
      const click = () => {
        if (!clickNewChat()) {
          try { location.href = "/"; } catch {}
        }
      };
      const waitReady = () => {
        const onHome = location.pathname === "/";
        const editor = document.querySelector('div[contenteditable="true"]');
        const empty = editor ? editor.textContent.trim().length === 0 : false;
        if (onHome && empty) {
          window.postMessage({ type: "NEW_CHAT_DONE" }, "*");
          return;
        }
        if (Date.now() < deadline) {
          setTimeout(waitReady, 250);
        } else {
          window.postMessage({ type: "NEW_CHAT_DONE" }, "*");
        }
      };
      const waitStream = () => {
        if (!isStreaming()) {
          click();
          setTimeout(waitReady, 400);
          return;
        }
        if (Date.now() < deadline) {
          setTimeout(waitStream, 250);
        } else {
          click();
          setTimeout(waitReady, 400);
        }
      };
      waitStream();
    }
    if (event.data?.type === "TEMP_CHAT") {
      // Custom GPT tabs do not support temporary chat; acknowledge immediately.
      if (location.pathname.startsWith("/gg/")) {
        window.postMessage({ type: "TEMP_CHAT_DONE" }, "*");
        return;
      }
      const deadline = Date.now() + 10000;
      const tryEnable = () => {
        if (isTempChatEnabled()) {
          window.postMessage({ type: "TEMP_CHAT_DONE" }, "*");
          return;
        }
        clickTempChat();
        if (Date.now() < deadline) {
          setTimeout(tryEnable, 300);
        } else {
          // Timed out — acknowledge so Rust does not hang.
          window.postMessage({ type: "TEMP_CHAT_DONE" }, "*");
        }
      };
      tryEnable();
    }
  });

  // ── Rate-limit modal handler (auto dismiss + retry) ─────────────────────
  function findByText(root, text) {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let node;
    while ((node = walker.nextNode())) {
      const val = (node.nodeValue || "").trim();
      if (val.includes(text)) return node.parentElement;
    }
    return null;
  }

  function clickIfVisible(el) {
    if (!el) return false;
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return false;
    el.click();
    return true;
  }

  function closeLimitModalAndRetry() {
    // Look for limit message
    const limitPhrases = [
      "You've hit your limit",
      "You’ve hit your limit",
      "limit resets",
      "Gateway time-out",
      "gateway time-out",
      "Error reference number",
    ];
    const node = limitPhrases
      .map(t => findByText(document.body, t))
      .find(Boolean);
    if (!node) return;

    window.postMessage({
      type: "INBOUND_MESSAGE",
      payload: JSON.stringify({
        type: "limit_modal",
        ts: Date.now(),
        title: node.textContent?.slice(0, 120) || ""
      })
    }, "*");

    // Try to click an explicit "Retry" button if present
    const retryBtn = Array.from(document.querySelectorAll("button"))
      .find(b => (b.textContent || "").toLowerCase().includes("retry"));
    if (clickIfVisible(retryBtn)) {
      window.postMessage({
        type: "INBOUND_MESSAGE",
        payload: JSON.stringify({ type: "limit_modal_action", action: "retry", ts: Date.now() })
      }, "*");
      return;
    }

    // Try to click an "X" close button on modal/dialog
    const closeBtn =
      document.querySelector('button[aria-label="Close"]') ||
      document.querySelector('button[aria-label="Dismiss"]') ||
      document.querySelector('button[aria-label="Close dialog"]');
    if (clickIfVisible(closeBtn)) {
      window.postMessage({
        type: "INBOUND_MESSAGE",
        payload: JSON.stringify({ type: "limit_modal_action", action: "close", ts: Date.now() })
      }, "*");
      return;
    }

    // Try to click any button with "Got it" or "OK"
    const okBtn = Array.from(document.querySelectorAll("button"))
      .find(b => {
        const t = (b.textContent || "").toLowerCase();
        return t.includes("got it") || t === "ok" || t.includes("okay");
      });
    if (clickIfVisible(okBtn)) {
      window.postMessage({
        type: "INBOUND_MESSAGE",
        payload: JSON.stringify({ type: "limit_modal_action", action: "ok", ts: Date.now() })
      }, "*");
    }
  }

  // Poll + observe because the modal can appear asynchronously
  setInterval(closeLimitModalAndRetry, 2000);
  const limitObserver = new MutationObserver(() => closeLimitModalAndRetry());
  limitObserver.observe(document.body, { childList: true, subtree: true });
})();

// Background: multi-WS relay between Rust and all content scripts.
// One WS per server/agent process. All messages tagged with tabId.
// TAB_READY (not TAB_OPENED) gates Rust TURN dispatch.

const RUST_WS       = "ws://127.0.0.1:9100";
const MINI_AGENT_WS_LIST = [
  "ws://127.0.0.1:9103",
  "ws://127.0.0.1:9104",
  "ws://127.0.0.1:9105",
  "ws://127.0.0.1:9106",
  "ws://127.0.0.1:9107",
  "ws://127.0.0.1:9108",
];
const TAB_QUERY_PATTERNS = [
  "https://chatgpt.com/*",
  "https://chat.openai.com/*",
  "https://gemini.google.com/*"
];

// tabId → send function of the WS connection that owns that tab
const tabWsOwner = new Map();

// tabId → reqId: tracks which reqId to attach when content signals READY
const pendingOpenReqIds = new Map();
// tabId → original URL from OPEN_TAB (for custom GPT navigate-back on NEW_CHAT).
const tabOriginalUrls = new Map();
// tabId → true while a navigate-back NEW_CHAT is in flight.
const pendingNewChatNavigations = new Map();

if (chrome.storage?.session) {
  chrome.storage.session.get("tabOriginalUrls", (result) => {
    if (result?.tabOriginalUrls) {
      for (const [k, v] of Object.entries(result.tabOriginalUrls)) {
        tabOriginalUrls.set(Number(k), v);
      }
    }
  });
}

function persistTabOriginalUrls() {
  if (!chrome.storage?.session) return;
  const obj = {};
  for (const [k, v] of tabOriginalUrls) obj[String(k)] = v;
  chrome.storage.session.set({ tabOriginalUrls: obj });
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

function extractLeaseTokenFromPayload(payload) {
  if (payload && typeof payload === "object") {
    return normalizeLeaseToken(payload.lease_token ?? payload.leaseToken);
  }
  if (typeof payload === "string") {
    try {
      const obj = JSON.parse(payload);
      return normalizeLeaseToken(obj?.lease_token ?? obj?.leaseToken);
    } catch {}
  }
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
  return `${turn}:${mode}:${text.length}:${hash >>> 0}`;
}

function ensureIdempotencyKey(payload) {
  if (!payload || typeof payload !== "object") return payload;
  if (extractIdempotencyKey(payload)) return payload;
  const turn = normalizeTurnId(payload.turn_id);
  if (turn) {
    return { ...payload, idempotency_key: `turn:${turn}` };
  }
  return { ...payload, idempotency_key: `sig:${hashPayload(payload)}` };
}

// ── WS connection factory ────────────────────────────────────────────────
function makeConnection(url) {
  let ws          = null;
  let queue       = [];
  let pingInterval = null;

  function send(msg) {
    const raw = typeof msg === "string" ? msg : JSON.stringify(msg);
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(raw);
    } else {
      queue.push(raw);
      connect();
    }
  }

  function connect() {
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;

    ws = new WebSocket(url);

    ws.onopen = () => {
      console.log(`[BG] WS connected to ${url}`);
      chrome.tabs.query({ url: TAB_QUERY_PATTERNS }, (tabs) => {
        for (const tab of tabs) {
          if (!tab?.id || !tab?.url) continue;
          const originalUrl = tabOriginalUrls.get(tab.id) ?? tab.url;
          send({ type: "TAB_READY", tabId: tab.id, url: tab.url, reqId: null, originalUrl });
        }
      });
      while (queue.length) ws.send(queue.shift());
      if (pingInterval) clearInterval(pingInterval);
      pingInterval = setInterval(() => {
        if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "PING" }));
      }, 20000);
    };

    ws.onmessage = (ev) => {
      try {
        console.log(`[BG][${url}] onmessage len=${ev.data?.length} preview=${ev.data?.substring?.(0, 80)}`);
        const msg = JSON.parse(ev.data);
        handleRustMessage(msg, send);
      } catch (e) {
        console.warn(`[BG] WS parse error (${url})`, e);
      }
    };

    ws.onclose = () => {
      console.warn(`[BG] WS closed (${url}) — reconnecting in 1s`);
      if (pingInterval) { clearInterval(pingInterval); pingInterval = null; }
      ws = null;
      setTimeout(connect, 1000);
    };

    ws.onerror = () => { try { ws.close(); } catch {} };
  }

  return { send, connect };
}

const runtimeConn = makeConnection(RUST_WS);
const miniAgentConns = MINI_AGENT_WS_LIST.map((url) => makeConnection(url));

// ── Route a message back to whichever server owns the tab ────────────────
function sendToOwner(tabId, msg) {
  const send = tabWsOwner.get(tabId);
  if (send) {
    send(msg);
  } else {
    // No owner recorded — typically means the service-worker restarted and
    // cleared the in-memory tabWsOwner map before the Rust side had a chance
    // to re-send CLAIM_TAB.  Broadcast to every active connection so the
    // server that actually owns this tab receives the message.  Servers that
    // don't own the tab will see an unknown turn_id and ignore it.
    runtimeConn.send(msg);
    for (const conn of miniAgentConns) {
      conn.send(msg);
    }
  }
}

// ── Retry sendMessage with exponential backoff ───────────────────────────
function sendToTab(tabId, message, attempts = 6, delayMs = 150) {
  chrome.tabs.sendMessage(tabId, message, (response) => {
    const err = chrome.runtime.lastError;
    if (err || !response?.ok) {
      if (attempts <= 1) {
        console.warn(`[BG] sendToTab tab=${tabId} failed after all retries:`, err?.message);
        return;
      }
      console.warn(`[BG] sendToTab tab=${tabId} retry in ${delayMs}ms (${attempts - 1} left):`, err?.message);
      setTimeout(() => sendToTab(tabId, message, attempts - 1, delayMs * 2), delayMs);
    }
  });
}

function focusTabAndSubmit(tabId, payload) {
  chrome.tabs.get(tabId, (tab) => {
    const err = chrome.runtime.lastError;
    if (err || !tab) {
      console.warn(`[BG] focusTabAndSubmit tab=${tabId} lookup failed:`, err?.message);
      sendToTab(tabId, { type: "OUTBOUND_SUBMIT", payload: ensureIdempotencyKey(payload) });
      return;
    }

    const submit = () => sendToTab(tabId, { type: "OUTBOUND_SUBMIT", payload: ensureIdempotencyKey(payload) });

    if (tab.windowId) {
      chrome.windows.update(tab.windowId, { focused: true }, () => {
        chrome.tabs.update(tabId, { active: true }, () => {
          submit();
        });
      });
    } else {
      chrome.tabs.update(tabId, { active: true }, () => {
        submit();
      });
    }
  });
}

// ── Content script → Background ──────────────────────────────────────────
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  const tabId = sender?.tab?.id;
  if (!tabId) { sendResponse({ ok: false, error: "no tabId" }); return true; }

  if (message?.type === "INBOUND_MESSAGE") {
    const leaseToken = extractLeaseTokenFromPayload(message.payload);
    try {
      const payload = typeof message.payload === "string" ? message.payload : JSON.stringify(message.payload ?? "");
      if (payload.includes("\"limit_modal\"") || payload.includes("\"limit_modal_action\"")) {
        chrome.storage.local.set({
          last_limit_event: payload,
          last_limit_event_ts: Date.now()
        });
      }
    } catch {}
    sendToOwner(tabId, {
      type:    "INBOUND_MESSAGE",
      tabId,
      ...(leaseToken ? { leaseToken, lease_token: leaseToken } : {}),
      payload: typeof message.payload === "string"
        ? message.payload
        : JSON.stringify(message.payload ?? "")
    });
    sendResponse({ ok: true });
    return true;
  }

  if (message?.type === "CONTENT_READY") {
    if (pendingNewChatNavigations.get(tabId)) {
      pendingNewChatNavigations.delete(tabId);
      console.log("[BG] CONTENT_READY after navigate-back, sending NEW_CHAT_DONE", { tabId });
      sendToOwner(tabId, { type: "NEW_CHAT_DONE", tabId });
      sendResponse({ ok: true });
      return true;
    }
    const reqId = pendingOpenReqIds.get(tabId) ?? null;
    pendingOpenReqIds.delete(tabId);
    const originalUrl = tabOriginalUrls.get(tabId) ?? null;
    const payload = { type: "TAB_READY", tabId, url: message.url, reqId, originalUrl };
    const owner = tabWsOwner.get(tabId);
    if (owner) {
      owner(payload);
    } else {
      runtimeConn.send(payload);
      for (const conn of miniAgentConns) {
        conn.send(payload);
      }
    }
    sendResponse({ ok: true });
    return true;
  }

  if (message?.type === "NEW_CHAT_DONE") {
    console.log("[BG] NEW_CHAT_DONE -> Rust", { tabId });
    sendToOwner(tabId, { type: "NEW_CHAT_DONE", tabId });
    sendResponse({ ok: true });
    return true;
  }

  if (message?.type === "TEMP_CHAT_DONE") {
    console.log("[BG] TEMP_CHAT_DONE -> Rust", { tabId });
    sendToOwner(tabId, { type: "TEMP_CHAT_DONE", tabId });
    sendResponse({ ok: true });
    return true;
  }

  if (message?.type === "SUBMIT_ACK") {
    const leaseToken = normalizeLeaseToken(
      message.lease_token ?? message.leaseToken
    );
    sendToOwner(tabId, {
      type: "SUBMIT_ACK",
      tabId,
      turnId: message.turn_id ?? null,
      ...(leaseToken ? { leaseToken, lease_token: leaseToken } : {}),
      ts: message.ts ?? Date.now()
    });
    sendResponse({ ok: true });
    return true;
  }

  if (message?.type === "TURN_STARTED") {
    sendToOwner(tabId, {
      type: "TURN_STARTED",
      tabId,
      turnId: message.turn_id ?? null,
      source: message.source ?? null,
      ts: message.ts ?? Date.now()
    });
    sendResponse({ ok: true });
    return true;
  }

  sendResponse({ ok: false });
  return true;
});

// ── Rust → Content script ────────────────────────────────────────────────
function handleRustMessage(msg, sendFn) {
  if (msg?.type === "OPEN_TAB") {
    if (!msg.url || typeof msg.url !== "string") return;
    const reqId = msg.reqId ?? null;

    chrome.tabs.create({ url: msg.url, active: false }, (tab) => {
      if (!tab?.id) return;
      const newTabId = tab.id;
      tabOriginalUrls.set(newTabId, msg.url);
      persistTabOriginalUrls();
      tabWsOwner.set(newTabId, sendFn);
      try {
        chrome.tabs.update(newTabId, { autoDiscardable: false });
      } catch {}
      if (reqId !== null) pendingOpenReqIds.set(newTabId, reqId);

      chrome.tabs.onUpdated.addListener(function listener(id, changeInfo) {
        if (id !== newTabId) return;
        const url = changeInfo.url || "";
        if (!(url.startsWith("https://chatgpt.com") || url.startsWith("https://gemini.google.com"))) return;
        chrome.tabs.onUpdated.removeListener(listener);
        sendFn({ type: "TAB_OPENED", tabId: newTabId, url, reqId });
      });
    });
    return;
  }

  if (msg?.type === "OUTBOUND_SUBMIT") {
    const targetTabId = msg.tabId;
    if (!targetTabId) return;
    const payload = ensureIdempotencyKey(msg.payload);
    sendToTab(targetTabId, { type: "OUTBOUND_SUBMIT", payload });
    return;
  }

  if (msg?.type === "NEW_CHAT") {
    const targetTabId = msg.tabId;
    if (!targetTabId) return;
    const originalUrl = tabOriginalUrls.get(targetTabId) ?? "";
    const isCustomGpt = originalUrl.includes("/gg/") || originalUrl.includes("chatgpt.com/g/");
    if (isCustomGpt) {
      pendingNewChatNavigations.set(targetTabId, true);
      chrome.tabs.update(targetTabId, { url: originalUrl }, () => void chrome.runtime.lastError);
      return;
    }
    sendToTab(targetTabId, { type: "NEW_CHAT" });
    return;
  }

  if (msg?.type === "TEMP_CHAT") {
    const targetTabId = msg.tabId;
    if (!targetTabId) return;
    sendToTab(targetTabId, { type: "TEMP_CHAT" });
    return;
  }

  if (msg?.type === "CLOSE_TAB") {
    const targetTabId = msg.tabId;
    if (!targetTabId) return;
    chrome.tabs.remove(targetTabId, () => void chrome.runtime.lastError);
    return;
  }

  if (msg?.type === "TURN") {
    const targetTabId = msg.tabId;
    if (!targetTabId) return;
    console.log("[BG] TURN → sendToTab", targetTabId, "text length:", msg.text?.length);
    const turnId = msg.turnId ?? null;
    const leaseToken = normalizeLeaseToken(
      msg.leaseToken ?? msg.lease_token
    );
    focusTabAndSubmit(targetTabId, {
      text: msg.text,
      mode: "auto",
      turn_id: turnId,
      ...(leaseToken ? { leaseToken, lease_token: leaseToken } : {})
    });
    return;
  }

  if (msg?.type === "CLAIM_TAB") {
    const targetTabId = msg.tabId;
    if (!targetTabId) return;
    tabWsOwner.set(targetTabId, sendFn);
    if (msg.url) {
      tabOriginalUrls.set(targetTabId, msg.url);
      persistTabOriginalUrls();
    }
    for (const conn of [runtimeConn, ...miniAgentConns]) {
      if (conn.send !== sendFn) conn.send({ type: "TAB_CLAIMED", tabId: targetTabId });
    }
    return;
  }
}

// ── Tab lifecycle ────────────────────────────────────────────────────────
chrome.tabs.onRemoved.addListener((tabId) => {
  const send = tabWsOwner.get(tabId) ?? runtimeConn.send;
  tabWsOwner.delete(tabId);
  pendingOpenReqIds.delete(tabId);
  if (tabOriginalUrls.delete(tabId)) {
    persistTabOriginalUrls();
  }
  pendingNewChatNavigations.delete(tabId);
  send({ type: "TAB_CLOSED", tabId });
});

// ── Re-inject content scripts into existing chatgpt tabs on startup ──────
chrome.tabs.query({ url: ["https://chatgpt.com/*", "https://chat.openai.com/*", "https://gemini.google.com/*"] }, (tabs) => {
  for (const tab of tabs) {
    chrome.scripting.executeScript({
      target: { tabId: tab.id },
      files: ["content.js"]
    }).catch(() => {});
  }
});

runtimeConn.connect();
for (const conn of miniAgentConns) {
  conn.connect();
}

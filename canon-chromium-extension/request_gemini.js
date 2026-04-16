  (function() {
    try {
      if (window.__geminiSseEnterInjected) {
        console.log('ℹ️ Combined injection already active');
        return;
      }
      window.__geminiSseEnterInjected = true;

      // ----- Globals for prompt injection -----
      window.__pendingPromptInjection  = window.__pendingPromptInjection  || null;
      window.__promptInjectionMode     = window.__promptInjectionMode     || "auto";
      window.__promptInjectionQueue    = window.__promptInjectionQueue    || [];
      window.__currentTurnId           = window.__currentTurnId           || null;
      const __seenOutboundTurns = new Map();
      const __seenOutboundIdempotency = new Map();
      const __seenOutboundSignatures = new Map();
      const OUTBOUND_DEDUP_WINDOW_MS = 60_000;

      function normalizeTurnId(turnId) {
        if (typeof turnId === "number" && Number.isFinite(turnId)) return String(turnId);
        if (typeof turnId === "string" && turnId.trim().length > 0) return turnId.trim();
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

      // ----- Gemini Enter-based send -----
      const selectors = {
        CHAT_INPUT: 'div.ql-editor.textarea.new-input-ui'
      };

      function findEditor() {
        return document.querySelector(selectors.CHAT_INPUT);
      }

      function dispatchInputEvents(el) {
        try {
          el.dispatchEvent(new InputEvent('input', { bubbles: true }));
        } catch {
          el.dispatchEvent(new Event('input', { bubbles: true }));
        }
        el.dispatchEvent(new Event('change', { bubbles: true }));
      }

      function insertText(text) {
        const editor = findEditor();
        if (!editor) {
          console.warn('❌ Gemini editor not found');
          return false;
        }
        editor.focus();
        document.execCommand('selectAll', false, null);
        document.execCommand('insertText', false, text);
        dispatchInputEvents(editor);
        return true;
      }

      function pressEnter(target) {
        if (!target) return false;
        const opts = { bubbles: true, cancelable: true, key: 'Enter', code: 'Enter', keyCode: 13, which: 13 };
        target.dispatchEvent(new KeyboardEvent('keydown', opts));
        target.dispatchEvent(new KeyboardEvent('keypress', opts));
        target.dispatchEvent(new KeyboardEvent('keyup', opts));
        return true;
      }

      window.__sendGeminiMessage = function(message) {
        const editor = findEditor();
        if (!editor) return false;
        const inserted = insertText(message);
        if (!inserted) return false;
        return pressEnter(editor);
      };

      console.log('✅ Gemini Enter injection active. Use window.__sendGeminiMessage("hi")');

      function waitForEditor(callback) {
        const editor = findEditor();
        if (editor) { callback(editor); return; }
        const observer = new MutationObserver(() => {
          const ed = findEditor();
          if (ed) { observer.disconnect(); callback(ed); }
        });
        observer.observe(document.body, { childList: true, subtree: true });
      }

      // Signal content.js when editor exists
      (function signalReadyWhenEditorExists() {
        const editor = findEditor();
        if (editor) {
          window.postMessage({ type: "BRIDGE_READY" }, "*");
          return;
        }
        const observer = new MutationObserver(() => {
          const ed = findEditor();
          if (ed) {
            observer.disconnect();
            window.postMessage({ type: "BRIDGE_READY" }, "*");
          }
        });
        observer.observe(document.body, { childList: true, subtree: true });
      })();

      // ----- SSE capture (fetch + EventSource + XHR) -----
      window.__sseChunks = window.__sseChunks || [];
      function recordSseChunk(source, url, chunk) {
        const entry = { ts: Date.now(), source, url, chunk };
        window.__sseChunks.push(entry);
        console.log(`📡 SSE chunk [${source}]`, url, chunk);
        window.postMessage({
          type: "INBOUND_MESSAGE",
          payload: { turn_id: window.__currentTurnId, chunk, ts: Date.now() }
        }, "*");
      }

      // Fetch hook
      if (!window.__sseFetchHooked) {
        window.__sseFetchHooked = true;
        const allowedContentTypes = ['text/event-stream', 'text/plain', 'application/json'];
        window.__originalFetch = window.__originalFetch || window.fetch;

        window.fetch = async function(url, options) {
          const response = await window.__originalFetch.apply(this, arguments);
          const contentType = response.headers.get('content-type') || '';
          const isStream = allowedContentTypes.some(t => contentType.includes(t));

          if (!isStream || !response.body) return response;

          console.log('📡 SSE/Stream detected (fetch):', url, contentType);

          const readable = new ReadableStream({
            start(controller) {
              const reader = response.body.getReader();
              const decoder = new TextDecoder();

              function pump() {
                return reader.read().then(({ done, value }) => {
                  if (done) {
                    controller.close();
                    console.log('📡 Stream complete (fetch)');
                    return;
                  }

                  const chunk = decoder.decode(value, { stream: true });
                  recordSseChunk('fetch', url, chunk);
                  controller.enqueue(value);
                  return pump();
                }).catch(err => {
                  console.error('📡 Stream error (fetch):', err);
                  controller.error(err);
                });
              }

              return pump();
            }
          });

          return new Response(readable, {
            headers: response.headers,
            status: response.status,
            statusText: response.statusText
          });
        };
      }

      // EventSource hook
      if (!window.__sseEventSourceHooked) {
        window.__sseEventSourceHooked = true;
        const OriginalEventSource = window.EventSource;
        if (OriginalEventSource) {
          window.EventSource = function(url, config) {
            const es = new OriginalEventSource(url, config);

            es.addEventListener('message', (e) => {
              recordSseChunk('eventsource', url, e.data);
            });

            es.addEventListener('error', (e) => {
              console.warn('📡 EventSource error:', url, e);
            });

            return es;
          };
          window.EventSource.prototype = OriginalEventSource.prototype;
        }
      }

      // XHR hook (for SSE via XHR streaming)
      if (!window.__sseXhrHooked) {
        window.__sseXhrHooked = true;
        const OriginalXHR = window.XMLHttpRequest;

        function PatchedXHR() {
          const xhr = new OriginalXHR();
          let lastIndex = 0;
          let url = '';

          const open = xhr.open;
          xhr.open = function(method, u) {
            url = u;
            return open.apply(xhr, arguments);
          };

          xhr.addEventListener('readystatechange', function() {
            if (xhr.readyState === 3 || xhr.readyState === 4) {
              const text = xhr.responseText || '';
              if (text.length > lastIndex) {
                const chunk = text.slice(lastIndex);
                lastIndex = text.length;
                recordSseChunk('xhr', url, chunk);
              }
            }
          });

          return xhr;
        }

        window.XMLHttpRequest = PatchedXHR;
        window.XMLHttpRequest.UNSENT = OriginalXHR.UNSENT;
        window.XMLHttpRequest.OPENED = OriginalXHR.OPENED;
        window.XMLHttpRequest.HEADERS_RECEIVED = OriginalXHR.HEADERS_RECEIVED;
        window.XMLHttpRequest.LOADING = OriginalXHR.LOADING;
        window.XMLHttpRequest.DONE = OriginalXHR.DONE;
      }

      console.log('✅ SSE capture hooks active (fetch + EventSource + XHR)');

      // OUTBOUND_SUBMIT handler (Rust → Gemini input)
      window.addEventListener("message", (event) => {
        if (event.source !== window) return;
        if (event.data?.type !== "OUTBOUND_SUBMIT") return;
        const { text, turn_id } = event.data.payload || {};
        if (typeof text !== "string") return;
        const { mode } = event.data.payload || {};
        if (shouldDropOutbound(event.data.payload)) {
          console.log("ℹ️ Gemini OUTBOUND_SUBMIT deduped", turn_id ?? null);
          return;
        }
        window.__promptInjectionMode = mode || "auto";
        window.__currentTurnId = turn_id ?? null;

        if (mode === "buffer") {
          window.__promptInjectionQueue ||= [];
          if (text) window.__promptInjectionQueue.push(text);
          return;
        }

        if (text) {
          window.__pendingPromptInjection = text;
          const attemptSend = () => {
            const queued = window.__promptInjectionQueue?.length > 0
              ? window.__promptInjectionQueue.join("\n\n")
              : null;
            if (queued) {
              window.__pendingPromptInjection = queued;
              window.__promptInjectionQueue = [];
            }
            const payload = window.__pendingPromptInjection;
            if (!payload) return;
            window.__pendingPromptInjection = null;
            const ok = window.__sendGeminiMessage(payload);
            if (!ok) {
              console.warn("❌ Gemini send failed (editor missing)");
            }
          };

          waitForEditor(() => setTimeout(attemptSend, 100));
        }
      });

      function clickNewChat() {
        const selectors = [
          'a[aria-label="New chat"][href="/app"]',
          'a.bard-logo-container[aria-label="New chat"]',
        ];
        for (const sel of selectors) {
          const el = document.querySelector(sel);
          if (el) {
            el.click();
            return true;
          }
        }
        return false;
      }

      window.addEventListener("message", (event) => {
        if (event.source !== window) return;
        if (event.data?.type === "NEW_CHAT") {
          const deadline = Date.now() + 20000;
          const click = () => {
            if (!clickNewChat()) {
              try { location.href = "/app"; } catch {}
            }
          };
          const waitReady = () => {
            const onApp = location.pathname.startsWith("/app");
            const editor = findEditor();
            const empty = editor ? editor.textContent.trim().length === 0 : false;
            if (onApp && empty) {
              window.postMessage({ type: "NEW_CHAT_DONE" }, "*");
              return;
            }
            if (Date.now() < deadline) {
              setTimeout(waitReady, 250);
            } else {
              window.postMessage({ type: "NEW_CHAT_DONE" }, "*");
            }
          };
          click();
          setTimeout(waitReady, 400);
        }
        if (event.data?.type === "TEMP_CHAT") {
          // Gemini does not expose a reliable temp chat toggle; no-op for now.
          window.postMessage({ type: "TEMP_CHAT_DONE" }, "*");
        }
      });
    } catch (error) {
      console.error('❌ Combined injection failed:', error);
    }
  })();

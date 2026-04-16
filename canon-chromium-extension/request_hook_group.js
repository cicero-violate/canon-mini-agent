// Request modifier hook - intercepts outgoing ChatGPT API requests
// and replaces placeholder text with window.__pendingPromptInjection
(function () {
  if (window.__RequestHookGroupInstalled) return;
  window.__RequestHookGroupInstalled = true;

  const TARGETS = [
    { origin: "https://chatgpt.com",     path: "/backend-api/calpico/chatgpt/rooms" },
    { origin: "https://chat.openai.com", path: "/backend-api/calpico/chatgpt/rooms" }
  ];

  console.log('[RequestHookGroup] Installing request modifier');

  // Globals
  window.__pendingPromptInjection	= window.__pendingPromptInjection || null;
  window.__promptInjectionMode		=  window.__promptInjectionMode || "auto"; // auto | buffer
  window.__promptInjectionQueue		= window.__promptInjectionQueue || [];

  function matchesTarget(input) {
    try {
      const abs = new URL(input, location.href);
      for (const t of TARGETS) {
        if (abs.origin === t.origin &&
            abs.pathname.startsWith(t.path)) {
          return true;
        }
      }
    } catch {}
    return false;
  }

  const originalFetch = window.fetch;

  window.fetch = async function (input, init) {

    // ─────────────────────────────────────────────
    // CASE 1: fetch(Request)
    // ─────────────────────────────────────────────
    if (input instanceof Request) {
      const url = input.url;

      if (!matchesTarget(url)) {
        return originalFetch(input);
      }

      if (input.method !== "POST") {
        return originalFetch(input);
      }

      try {
        const text = await input.clone().text();
        if (!text) return originalFetch(input);

        const payload = JSON.parse(text);

        // Calpico group chat only: replace first <PROMPT>, clear any extras.
        if (payload?.content?.text && typeof payload.content.text === "string") {
          const text = payload.content.text;
          if (text.includes("<PROMPT>")) {
            const combined =
              window.__pendingPromptInjection ||
              window.__promptInjectionQueue.join("\n\n") ||
              "";
            console.groupCollapsed("[RequestHook] group prompt injection");
            console.log("combined_chars:", combined.length);
            console.log("combined_preview:", combined.slice(0, 400));
            console.groupEnd();
            // Replace the first placeholder with the combined prompt and remove any others.
            payload.content.text = text.replace("<PROMPT>", combined).replace(/<PROMPT>/g, "");

            const newReq = new Request(input, {
              body: JSON.stringify(payload)
            });

            if (window.__promptInjectionMode === "auto") {
              window.__pendingPromptInjection = null;
              window.__promptInjectionQueue = [];
            }

            console.log("✅ INJECTED (Request)");
            return originalFetch(newReq);
          }
        }

      } catch (e) {
        console.warn("Hook parse failed:", e);
      }

      return originalFetch(input);
    }

    // ─────────────────────────────────────────────
    // CASE 2: fetch(url, init)
    // ─────────────────────────────────────────────
    const url =
      typeof input === "string" ? input : input?.url;

    if (matchesTarget(url) &&
        init &&
        typeof init.body === "string") {

      try {
        const payload = JSON.parse(init.body);

        if (payload?.content?.text && typeof payload.content.text === "string") {
          const text = payload.content.text;
          if (text.includes("<PROMPT>")) {
            const combined =
              window.__pendingPromptInjection ||
              window.__promptInjectionQueue.join("\n\n") ||
              "";
            console.groupCollapsed("[RequestHook] group prompt injection");
            console.log("combined_chars:", combined.length);
            console.log("combined_preview:", combined.slice(0, 400));
            console.groupEnd();
            payload.content.text = text.replace("<PROMPT>", combined).replace(/<PROMPT>/g, "");

            init.body = JSON.stringify(payload);

            if (window.__promptInjectionMode === "auto") {
              window.__pendingPromptInjection = null;
              window.__promptInjectionQueue = [];
            }

            console.log("✅ INJECTED (init.body)");
          }
        }

      } catch (e) {
        console.warn("Hook parse failed:", e);
      }
    }

    return originalFetch(input, init);
  };
})();

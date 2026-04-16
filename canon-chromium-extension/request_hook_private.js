// Request modifier hook - intercepts outgoing ChatGPT API requests
// and replaces placeholder text with window.__pendingPromptInjection
(function() {
  if (window.__RequestHookPrivateInstalled) return;
  window.__RequestHookPrivateInstalled = true;

  const TARGETS = [
    {origin: "https://chatgpt.com", path: "/backend-api/f/conversation"},
    {origin: "https://chat.openai.com", path: "/backend-api/f/conversation"}
  ];

  console.log('[RequestHookPrivate] Installing request modifier');

  window.__pendingPromptInjection = window.__pendingPromptInjection || null;
  window.__promptInjectionMode = window.__promptInjectionMode || "auto"; // auto | buffer
  window.__promptInjectionQueue = window.__promptInjectionQueue || [];
  window.__promptInjectionQueue = window.__promptInjectionQueue || [];

  function matchesTarget(input) {
    try {
      const abs = new URL(input, location.href);
      for (const target of TARGETS) {
        if (abs.origin === target.origin && abs.pathname.startsWith(target.path)) {
          return true;
        }
      }
    } catch (err) {
      // console.warn('[RequestHook] URL parse failed', err);
    }
    return false;
  }

  const originalFetch = window.fetch;
  window.fetch = async function(input, init) {
    const isTarget = matchesTarget(typeof input === 'string' ? input : input?.url);

    // Suppress backend conversation calls triggered by Judge
    // if (isTarget && window.__isJudgeAction) {
    //    console.warn(
    //      "[RequestHook] Suppressed backend-api/f/conversation during USER_COMMAND action"
    //    );
    //   return new Response(null, { status: 204 });
    // }

    if (isTarget) {
      // console.log('[RequestHook] Intercepted ChatGPT API call');
      console.groupCollapsed('[RequestHook] backend-api/f/conversation');
      // console.log('URL:', typeof input === 'string' ? input : input?.url);
      // console.log('Has pending injection:', Boolean(window.__pendingPromptInjection));
      console.trace('Request stack trace');
      console.groupEnd();
    }

    // Modify outgoing request if we have pending injection
    if (isTarget && init && typeof init.body === 'string' && (window.__pendingPromptInjection || window.__promptInjectionQueue?.length > 0)) {
      try {
        const payload = JSON.parse(init.body);
        // console.log('[RequestHook] Has pending injection:', window.__pendingPromptInjection.substring(0, 50));

        // Replace the placeholder text with actual prompt
        if (Array.isArray(payload?.messages) && payload.messages.length > 0) {
          // Find all messages that contain the <PROMPT> placeholder
          const withPlaceholder = [];
          for (let i = 0; i < payload.messages.length; i++) {
            const msg = payload.messages[i];
            const parts = msg?.content?.parts;
            if (Array.isArray(parts) && parts.some(part => typeof part === 'string' && part.includes('<PROMPT>'))) {
              withPlaceholder.push(i);
            }
          }

          if (withPlaceholder.length > 0) {
            // Combine queue and pending (filter out nulls)
            const parts = [...window.__promptInjectionQueue];
            if (window.__pendingPromptInjection) {
              parts.push(window.__pendingPromptInjection);
            }
            const combined = parts.join('\n\n');
            console.groupCollapsed('[RequestHook] prompt injection');
            console.log('placeholders:', withPlaceholder.length);
            console.log('combined_chars:', combined.length);
            console.log('combined_preview:', combined.slice(0, 400));
            console.groupEnd();

            // Replace the first placeholder with the combined prompt.
            const first = payload.messages[withPlaceholder[0]];
            if (first?.content?.parts) {
              first.content.parts = [combined];
            }

            // Clear any additional placeholders to avoid sending literal <PROMPT>.
            for (let j = 1; j < withPlaceholder.length; j++) {
              const msg = payload.messages[withPlaceholder[j]];
              if (msg?.content?.parts) {
                msg.content.parts = [""];
              }
            }

            window.__promptInjectionQueue = [];
            // console.log('[RequestHook] Replaced <PROMPT> with injection');
          } else {
            // console.log('[RequestHook] No <PROMPT> placeholder found, skipping injection');
          }
        }
        
        init.body = JSON.stringify(payload);
        if (window.__promptInjectionMode === "auto") {
          window.__pendingPromptInjection = null;
          window.__promptInjectionQueue = [];
        }
      } catch (err) {
        console.warn('[RequestHook] Request modification failed', err);
      }
    }

    return originalFetch(input, init);
  };

  // console.log('[RequestHook] Installed successfully');
})();

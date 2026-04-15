use crate::llm_runtime::backend::LlmBackend;
use crate::llm_runtime::chromium_backend::ChromiumBackend;
use crate::llm_runtime::http_backend::HttpBackend;

use std::sync::{Arc, OnceLock};

/// WsBridge wraps an `Arc<dyn LlmBackend>` so it can be passed around cheaply
/// and swapped between HTTP, mock, or any future backend without changing call sites.
#[derive(Clone)]
pub struct WsBridge {
    pub(crate) backend: Arc<dyn LlmBackend>,
    pub(crate) response_timeout_secs: u64,
}

impl WsBridge {
    pub fn new(backend: Arc<dyn LlmBackend>, response_timeout_secs: u64) -> Self {
        Self {
            backend,
            response_timeout_secs,
        }
    }

    pub fn response_timeout_secs(&self) -> u64 {
        self.response_timeout_secs
    }

    /// No-op: HTTP/mock backends need no connection handshake.
    pub async fn wait_for_connection(&self) {}

    /// The Chrome-extension model queued async turn completions here.
    /// With a synchronous HTTP backend every response is returned inline,
    /// so there are never any pending items to drain.
    pub async fn take_completed_turns(&self) -> Vec<serde_json::Value> {
        self.backend.take_completed_turns().await
    }
}

/// Drop-in replacement for `canon-llm-runtime`'s `ws_server::spawn()`.
///
/// Backend selection via `CANON_LLM_BACKEND` env var:
///   `http`    → `HttpBackend` (direct Anthropic API via `ANTHROPIC_API_KEY`)
///   (default) → `ChromiumBackend` (Chrome extension relay on port 9103)
pub fn spawn(
    addr: std::net::SocketAddr,
    response_timeout_secs: u64,
    _retry_count: u32,
    _retry_delay_secs: u64,
    _emitter: Arc<OnceLock<()>>,
) -> WsBridge {
    let backend: Arc<dyn LlmBackend> = match std::env::var("CANON_LLM_BACKEND").as_deref() {
        Ok("http") => Arc::new(HttpBackend::from_env()),
        _ => Arc::new(ChromiumBackend::spawn(addr.port())),
    };
    WsBridge::new(backend, response_timeout_secs)
}

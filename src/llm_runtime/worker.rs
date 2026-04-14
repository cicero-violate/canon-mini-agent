use crate::llm_runtime::tab_management::{TabManagerHandle, TabSlotTable};
use crate::llm_runtime::types::LlmResponse;
use crate::llm_runtime::ws_server::WsBridge;
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static REQ_ID: AtomicU64 = AtomicU64::new(1);

/// Create a fresh, empty tab-manager handle (rate-limiting bookkeeping).
pub fn llm_worker_new_tabs() -> TabManagerHandle {
    Arc::new(tokio::sync::Mutex::new(TabSlotTable::new()))
}

/// Send a prompt and wait for the full LLM response, returning `(req_id, LlmResponse)`.
///
/// All Chrome-extension-specific parameters (`urls`, `stateful`, `node_id`,
/// `cache_key`, `bust_cache`, `allow_req_id_mismatch`, `phase`, `tabs`,
/// `max_tabs`) are accepted for API compatibility but ignored — the backend
/// handles routing internally.
#[allow(clippy::too_many_arguments)]
pub async fn llm_worker_send_request_with_req_id_timeout(
    bridge: &WsBridge,
    endpoint_id: &str,
    _urls: &[String],
    _stateful: bool,
    prompt: &str,
    role_schema: &str,
    _node_id: Option<&str>,
    _cache_key: Option<u64>,
    _bust_cache: bool,
    _allow_req_id_mismatch: bool,
    _phase: &str,
    _tabs: &TabManagerHandle,
    _max_tabs: usize,
    submit_only: bool,
    timeout_secs: Option<u64>,
) -> Result<(u64, LlmResponse)> {
    let req_id = REQ_ID.fetch_add(1, Ordering::SeqCst);
    let resp = bridge
        .backend
        .send(endpoint_id, _urls, _stateful, prompt, role_schema, submit_only, timeout_secs)
        .await?;
    Ok((req_id, resp))
}

/// Send a prompt and return the raw response string (discards req_id).
#[allow(clippy::too_many_arguments)]
pub async fn llm_worker_send_request_timeout(
    bridge: &WsBridge,
    endpoint_id: &str,
    urls: &[String],
    stateful: bool,
    prompt: &str,
    role_schema: &str,
    node_id: Option<&str>,
    cache_key: Option<u64>,
    bust_cache: bool,
    allow_req_id_mismatch: bool,
    phase: &str,
    tabs: &TabManagerHandle,
    max_tabs: usize,
    submit_only: bool,
    timeout_secs: Option<u64>,
) -> Result<String> {
    let (_id, resp) = llm_worker_send_request_with_req_id_timeout(
        bridge,
        endpoint_id,
        urls,
        stateful,
        prompt,
        role_schema,
        node_id,
        cache_key,
        bust_cache,
        allow_req_id_mismatch,
        phase,
        tabs,
        max_tabs,
        submit_only,
        timeout_secs,
    )
    .await?;
    Ok(resp.raw)
}

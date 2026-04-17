/// Chromium backend: acts as a WebSocket server that the Canon Chrome extension
/// connects to.  Prompts are injected into a ChatGPT tab via the extension
/// relay; SSE frames come back as INBOUND_MESSAGE and are assembled by the
/// same `parsers::FrameAssembler` used in canon-llm-runtime.
///
/// Protocol (extension → Rust):
///   TAB_READY      { tabId, url, reqId? }  — a ChatGPT tab is ready
///   TAB_OPENED     { tabId, url, reqId? }  — newly opened tab navigated
///   TAB_CLOSED     { tabId }
///   SUBMIT_ACK     { tabId, turnId, leaseToken|lease_token }  — prompt was submitted
///   INBOUND_MESSAGE{ tabId, payload }                         — SSE chunk from ChatGPT
///   PING                                                      — keepalive, ignored
///
/// Protocol (Rust → extension):
///   OPEN_TAB       { url, reqId }                             — open a new tab
///   CLAIM_TAB      { tabId, url }                             — assert ownership after sw restart
///   TURN           { tabId, text, turnId, leaseToken }        — inject prompt
use crate::llm_runtime::backend::LlmBackend;
use crate::llm_runtime::parsers::{FrameAssembler, SiteType};
use crate::llm_runtime::types::LlmResponse;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const FRAMES_DIR: &str = "./frames";
const PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD: u32 = 8;

fn append_jsonl(filename: &str, value: &Value) {
    let path = format!("{FRAMES_DIR}/{filename}");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        if let Ok(line) = serde_json::to_string(value) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn append_outbound_event(event_type: &str, payload: Value) {
    let mut event = serde_json::Map::new();
    event.insert("type".to_string(), Value::String(event_type.to_string()));
    if let Value::Object(map) = payload {
        event.extend(map);
    } else {
        event.insert("payload".to_string(), payload);
    }
    append_jsonl("all.jsonl", &Value::Object(event));
}

fn append_inbound_boundary_event(
    state: &mut State,
    tab_id: u32,
    turn_id: u64,
    endpoint_id: &str,
    boundary_kind: &str,
    reason: &str,
    stateful: bool,
    submit_only: bool,
) {
    state.frame_counter += 1;
    append_jsonl(
        "inbound.jsonl",
        &json!({
            "frame_counter": state.frame_counter,
            "tab_id": tab_id,
            "inbound_turn_id": turn_id,
            "expected_turn_id": turn_id,
            "endpoint_id": endpoint_id,
            "transport_signal": boundary_kind,
            "chunk": reason,
            "payload_raw_len": reason.len(),
            "boundary": true,
            "stateful": stateful,
            "submit_only": submit_only,
        }),
    );
}

fn endpoint_role(endpoint_id: &str) -> &str {
    endpoint_id.split('_').next().unwrap_or(endpoint_id)
}

fn endpoint_submit_ack_timeout_secs(endpoint_id: &str, total_timeout_secs: u64) -> u64 {
    let role = endpoint_role(endpoint_id).replace('-', "_").to_ascii_uppercase();
    let scoped_env = format!("CANON_LLM_SUBMIT_ACK_TIMEOUT_SECS_{role}");
    let override_secs = std::env::var(&scoped_env)
        .ok()
        .or_else(|| std::env::var("CANON_LLM_SUBMIT_ACK_TIMEOUT_SECS").ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0);

    let default_secs = match endpoint_role(endpoint_id) {
        "solo" | "planner" | "mini" => 60,
        "diagnostics" | "verifier" => 30,
        _ => 15,
    };

    override_secs
        .unwrap_or(default_secs)
        .min(total_timeout_secs.max(1))
}

fn init_frames_dir() {
    let _ = std::fs::create_dir_all(FRAMES_DIR);
}

fn submit_ack_payload(tab_id: u32, turn_id: u64, lease_token: &str, source: &str) -> String {
    json!({
        "submit_ack": true,
        "tab_id": tab_id,
        "turn_id": turn_id,
        "lease_token": lease_token,
        "source": source,
    })
    .to_string()
}

fn next_turn_lease_token(seed: u64, turn_id: u64) -> String {
    format!("lease-{turn_id:016x}-{seed:016x}")
}

fn lease_token_from_value(value: &Value) -> Option<String> {
    value
        .get("leaseToken")
        .and_then(Value::as_str)
        .or_else(|| value.get("lease_token").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn turn_id_from_value(value: &Value) -> Option<u64> {
    value
        .get("turnId")
        .or_else(|| value.get("turn_id"))
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.trim().parse::<u64>().ok(),
            _ => None,
        })
}

fn payload_value_from_message(value: &Value) -> Option<Value> {
    match value.get("payload") {
        Some(Value::Object(map)) => Some(Value::Object(map.clone())),
        Some(Value::String(text)) if text.trim_start().starts_with('{') => {
            serde_json::from_str::<Value>(text).ok()
        }
        _ => None,
    }
}

fn classify_transport_signal(raw: &str) -> Option<&'static str> {
    if raw.contains("\"calpico-is-responding-heartbeat\"") {
        return Some("heartbeat");
    }
    if raw.contains("\"conversation-turn-complete\"") {
        return Some("turn_complete");
    }
    if raw.contains("\"type\":\"presence\"") {
        return Some("presence");
    }
    if raw.contains("\"calpico-message-add\"") && raw.contains("\"role\":\"user\"") {
        return Some("user_message_add");
    }
    if raw.contains("\"calpico-message-add\"") && raw.contains("\"role\":\"assistant\"") {
        return Some("assistant_message_add");
    }
    None
}

// ---------------------------------------------------------------------------
// Shared server state (guarded by a single Mutex)
// ---------------------------------------------------------------------------

struct State {
    /// Channel to send frames to the currently connected extension.
    out_tx: Option<mpsc::Sender<Message>>,

    /// tabId → frame assembler.
    assemblers: HashMap<u32, FrameAssembler>,

    /// tabId → expected turn_id (set while a turn is in flight).
    pending_turn_id: HashMap<u32, u64>,

    /// (tabId, turnId) → lease token issued with TURN and required on ack/inbound.
    pending_turn_lease: HashMap<(u32, u64), String>,

    /// (tabId, turnId) → oneshot that fires with the submit-ack payload when
    /// browser submission is confirmed.
    pending_ack: HashMap<(u32, u64), oneshot::Sender<String>>,

    /// (tabId, turnId) → oneshot that fires with the assembled response.
    pending_resp: HashMap<(u32, u64), oneshot::Sender<String>>,

    /// (tabId, turnId) → oneshot that fires when inbound transport evidence
    /// says the response stream has already drifted or stalled.
    pending_early_fail: HashMap<(u32, u64), oneshot::Sender<String>>,

    /// Completed submit-only turns awaiting orchestration pickup.
    completed_turns: VecDeque<Value>,

    /// reqId → oneshot that fires with the tabId when TAB_READY (with reqId) arrives.
    pending_open: HashMap<u64, oneshot::Sender<u32>>,

    /// tabId → URL (last known).
    tab_urls: HashMap<u32, String>,

    /// URL → queue of pre-opened tabIds (TAB_READY with no reqId).
    preopened: HashMap<String, VecDeque<u32>>,

    /// endpoint_id → tabId for stateful endpoints.
    endpoint_tabs: HashMap<String, u32>,

    /// tabId → endpoint_id owner for stateful endpoints.
    tab_owners: HashMap<u32, String>,

    /// TURN frames queued while the extension socket is down.
    replay_queue: Vec<Value>,

    /// Monotonic counter for frame log entries.
    frame_counter: u64,

    /// (tabId, turnId) -> frame_counter where the turn first emitted
    /// `conversation-turn-complete` while a response was still pending.
    turn_complete_seen: HashMap<(u32, u64), u64>,

    /// (tabId, turnId) -> count of `presence` frames observed after
    /// `conversation-turn-complete` and before any assistant terminal frame.
    post_complete_presence: HashMap<(u32, u64), u32>,

    /// (tabId, turnId) -> count of `heartbeat` frames observed after
    /// `conversation-turn-complete` and before any assistant terminal frame.
    post_complete_heartbeat: HashMap<(u32, u64), u32>,

    /// (tabId, turnId) -> frame_counter where the current turn's user prompt
    /// echo was first observed before any assistant terminal frame.
    user_message_seen: HashMap<(u32, u64), u64>,

    /// (tabId, turnId) -> count of non-progress `heartbeat` frames observed
    /// after the user prompt echo but before any assistant progress or
    /// `turn_complete`.
    post_user_message_heartbeat: HashMap<(u32, u64), u32>,
}

/// Shared transport bootstrap for both submit-only and full-response flows.
struct StartedTurn {
    turn_id: u64,
    ack_rx: oneshot::Receiver<String>,
    resp_rx: Option<oneshot::Receiver<String>>,
    early_fail_rx: Option<oneshot::Receiver<String>>,
}

impl State {
    fn new() -> Self {
        init_frames_dir();
        // Clear stale frame logs from previous run.
        for name in &["all.jsonl", "inbound.jsonl", "assembled.jsonl"] {
            let _ = std::fs::remove_file(format!("{FRAMES_DIR}/{name}"));
        }
        Self {
            out_tx: None,
            assemblers: HashMap::new(),
            pending_turn_id: HashMap::new(),
            pending_turn_lease: HashMap::new(),
            pending_ack: HashMap::new(),
            pending_resp: HashMap::new(),
            pending_early_fail: HashMap::new(),
            completed_turns: VecDeque::new(),
            pending_open: HashMap::new(),
            tab_urls: HashMap::new(),
            preopened: HashMap::new(),
            endpoint_tabs: HashMap::new(),
            tab_owners: HashMap::new(),
            replay_queue: Vec::new(),
            frame_counter: 0,
            turn_complete_seen: HashMap::new(),
            post_complete_presence: HashMap::new(),
            post_complete_heartbeat: HashMap::new(),
            user_message_seen: HashMap::new(),
            post_user_message_heartbeat: HashMap::new(),
        }
    }

    fn send_msg(&self, msg: Value) -> bool {
        if let Some(tx) = &self.out_tx {
            tx.try_send(Message::Text(msg.to_string().into())).is_ok()
        } else {
            false
        }
    }

    fn is_connected(&self) -> bool {
        self.out_tx.is_some()
    }
}

// ---------------------------------------------------------------------------
// Public backend handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ChromiumBackend {
    state: Arc<Mutex<State>>,
    next_turn_id: Arc<AtomicU64>,
    next_turn_lease_seed: Arc<AtomicU64>,
    next_req_id: Arc<AtomicU64>,
}

impl ChromiumBackend {
    /// Spawn the WebSocket server on `port` and return the backend handle.
    pub fn spawn(port: u16) -> Self {
        let state = Arc::new(Mutex::new(State::new()));
        let backend = ChromiumBackend {
            state: state.clone(),
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_turn_lease_seed: Arc::new(AtomicU64::new(1)),
            next_req_id: Arc::new(AtomicU64::new(1)),
        };

        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        tokio::spawn(accept_loop(addr, state));

        backend
    }

    /// Block until the Chrome extension has connected.
    pub async fn wait_for_connection(&self) {
        loop {
            {
                let st = self.state.lock().await;
                if st.is_connected() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Get any available tab from the preopened pool.
    async fn pop_any_tab(&self) -> Option<u32> {
        let mut st = self.state.lock().await;
        let tab_id = st.preopened.values_mut().find_map(|q| q.pop_front())?;
        let url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
        st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
        Some(tab_id)
    }

    async fn pop_matching_url_tab(&self, urls: &[String]) -> Option<u32> {
        let mut st = self.state.lock().await;
        for url in urls {
            let Some(queue) = st.preopened.get_mut(url) else {
                continue;
            };
            let Some(tab_id) = queue.pop_front() else {
                continue;
            };
            let claim_url = st
                .tab_urls
                .get(&tab_id)
                .cloned()
                .unwrap_or_else(|| url.clone());
            st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": claim_url }));
            return Some(tab_id);
        }
        None
    }

    /// Open a new tab at `url` via the extension and wait for it to be ready.
    async fn open_tab(&self, url: &str, timeout_secs: u64) -> Result<u32> {
        let req_id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel::<u32>();

        {
            let mut st = self.state.lock().await;
            st.pending_open.insert(req_id, tx);
            if !st.send_msg(json!({ "type": "OPEN_TAB", "url": url, "reqId": req_id })) {
                st.pending_open.remove(&req_id);
                anyhow::bail!("chromium: extension not connected, cannot open tab");
            }
        }

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(tab_id)) => Ok(tab_id),
            _ => {
                self.state.lock().await.pending_open.remove(&req_id);
                anyhow::bail!("chromium: timeout waiting for tab to open at {url}")
            }
        }
    }

    /// Acquire a tab for the given endpoint URLs.
    /// Tries the preopened pool first; opens a new tab if the pool is empty.
    async fn acquire_tab(
        &self,
        endpoint_id: &str,
        urls: &[String],
        timeout_secs: u64,
        stateful: bool,
    ) -> Result<u32> {
        // Wait for extension to connect first (up to timeout).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            if self.state.lock().await.is_connected() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("chromium: extension not connected after {timeout_secs}s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // Give TAB_READY messages a moment to arrive after connection, then
        // spend a short bounded window reconciling backend state with the
        // browser's current tab announcements before opening a new tab.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut reconcile_attempt = 0u32;
        loop {
            {
                let mut st = self.state.lock().await;
                let recovered =
                    reconcile_tab_state_locked(&mut st, Some(endpoint_id), urls);
                if recovered > 0 {
                    append_outbound_event(
                        "OUTBOUND_TAB_STATE_RECONCILED",
                        json!({
                            "endpointId": endpoint_id,
                            "stateful": stateful,
                            "requestedUrls": urls,
                            "recoveredCount": recovered,
                            "endpointTab": st.endpoint_tabs.get(endpoint_id).copied(),
                            "knownTabCount": st.tab_urls.len(),
                            "ownedTabCount": st.tab_owners.len(),
                            "preopenedUrlCount": st.preopened.len(),
                            "attempt": reconcile_attempt,
                        }),
                    );
                }
                if stateful {
                    if let Some(tab_id) = st.endpoint_tabs.get(endpoint_id).copied() {
                        if let Some(url) = st.tab_urls.get(&tab_id).cloned() {
                            st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
                            return Ok(tab_id);
                        }
                        st.endpoint_tabs.remove(endpoint_id);
                        st.tab_owners.remove(&tab_id);
                    }
                }
            }

            if let Some(tab_id) = if stateful {
                self.pop_matching_url_tab(urls).await
            } else {
                self.pop_any_tab().await
            } {
                if stateful {
                    let mut st = self.state.lock().await;
                    st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
                    st.tab_owners.insert(tab_id, endpoint_id.to_string());
                }
                return Ok(tab_id);
            }

            reconcile_attempt += 1;
            if reconcile_attempt >= 5 || std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }

        // No tab available — open one at the first URL.
        let url = urls
            .first()
            .map(String::as_str)
            .unwrap_or("https://chatgpt.com/");
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
            .max(10);
        {
            let st = self.state.lock().await;
            append_outbound_event(
                "OUTBOUND_TAB_ACQUIRE_FALLBACK",
                json!({
                    "endpointId": endpoint_id,
                    "stateful": stateful,
                    "requestedUrls": urls,
                    "endpointTab": st.endpoint_tabs.get(endpoint_id).copied(),
                    "knownTabUrl": st
                        .endpoint_tabs
                        .get(endpoint_id)
                        .and_then(|tab_id| st.tab_urls.get(tab_id).cloned()),
                    "preopenedForRequestedUrls": urls
                        .iter()
                        .map(|candidate| {
                            (
                                candidate.clone(),
                                st.preopened
                                    .get(candidate)
                                    .map(|queue| queue.len())
                                    .unwrap_or(0usize),
                            )
                        })
                        .collect::<Vec<_>>(),
                    "preopenedUrlCount": st.preopened.len(),
                    "knownTabCount": st.tab_urls.len(),
                    "ownedTabCount": st.tab_owners.len(),
                }),
            );
        }
        // This means no reusable tab was found in backend ownership state, not that Chrome has zero tabs.
        // Console wording should reflect backend reuse semantics, not literal browser tab absence.
        // Note: if another workspace is built instead of this extracted repo, this local fallback_debug binding will not help that other build.
        eprintln!("[chromium] no reusable tab in backend state, opening {url}");
        let tab_id = self.open_tab(url, remaining).await?;
        if stateful {
            let mut st = self.state.lock().await;
            st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
            st.tab_owners.insert(tab_id, endpoint_id.to_string());
        }
        Ok(tab_id)
    }

    /// Send a TURN to the extension and wait for the full assembled response.
    async fn do_send(
        &self,
        endpoint_id: &str,
        tab_id: u32,
        url: &str,
        prompt: &str,
        timeout_secs: u64,
        stateful: bool,
    ) -> Result<LlmResponse> {
        let StartedTurn {
            turn_id,
            ack_rx,
            resp_rx,
            early_fail_rx,
        } = self
            .start_turn(endpoint_id, tab_id, url, prompt, stateful, false)
            .await;
        let resp_rx = resp_rx.expect("response channel required for full send");
        let early_fail_rx = early_fail_rx.expect("early-fail channel required for full send");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        // Wait for SUBMIT_ACK using an endpoint-aware cap.
        let ack_timeout_secs = endpoint_submit_ack_timeout_secs(endpoint_id, timeout_secs);
        let ack_deadline = deadline.min(
            std::time::Instant::now() + std::time::Duration::from_secs(ack_timeout_secs),
        );
        let ack_remaining = ack_deadline.saturating_duration_since(std::time::Instant::now());
        self.await_submit_ack(
            endpoint_id,
            tab_id,
            turn_id,
            url,
            stateful,
            false,
            ack_timeout_secs,
            ack_remaining,
            ack_rx,
        )
        .await?;

        enum ResponseWaitOutcome {
            Response(Result<String, tokio::sync::oneshot::error::RecvError>),
            EarlyFail(Result<String, tokio::sync::oneshot::error::RecvError>),
        }

        // Wait for the assembled response, but allow deterministic protocol
        // evidence to fail the turn before the wall-clock timeout expires.
        let resp_remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(resp_remaining, async {
            tokio::select! {
                biased;
                raw = resp_rx => ResponseWaitOutcome::Response(raw),
                early = early_fail_rx => ResponseWaitOutcome::EarlyFail(early),
            }
        })
        .await
        {
            Ok(ResponseWaitOutcome::Response(Ok(raw))) => {
                let mut st = self.state.lock().await;
                st.pending_early_fail.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                st.pending_turn_lease.remove(&(tab_id, turn_id));
                release_tab_locked(&mut st, endpoint_id, tab_id, stateful);
                Ok(LlmResponse {
                    raw,
                    tab_id: Some(tab_id),
                    turn_id: Some(turn_id),
                })
            }
            Ok(ResponseWaitOutcome::EarlyFail(Ok(reason))) => {
                let mut st = self.state.lock().await;
                append_inbound_boundary_event(
                    &mut st,
                    tab_id,
                    turn_id,
                    endpoint_id,
                    "early_fail",
                    &reason,
                    stateful,
                    false,
                );
                st.pending_resp.remove(&(tab_id, turn_id));
                st.pending_early_fail.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                st.pending_turn_lease.remove(&(tab_id, turn_id));
                retire_tab_locked(&mut st, endpoint_id, tab_id, stateful);
                append_outbound_event(
                    "OUTBOUND_RESPONSE_EARLY_FAIL",
                    json!({
                        "endpoint_id": endpoint_id,
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "url": url,
                        "stateful": stateful,
                        "submit_only": false,
                        "reason": reason,
                    }),
                );
                anyhow::bail!(
                    "chromium: early transport failure ({reason}) (tab={tab_id} turn={turn_id})"
                );
            }
            _ => {
                let mut st = self.state.lock().await;
                append_inbound_boundary_event(
                    &mut st,
                    tab_id,
                    turn_id,
                    endpoint_id,
                    "response_timeout",
                    "chromium: timeout waiting for response",
                    stateful,
                    false,
                );
                st.pending_resp.remove(&(tab_id, turn_id));
                st.pending_early_fail.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                st.pending_turn_lease.remove(&(tab_id, turn_id));
                retire_tab_locked(&mut st, endpoint_id, tab_id, stateful);
                append_outbound_event(
                    "OUTBOUND_RESPONSE_TIMEOUT",
                    json!({
                        "endpoint_id": endpoint_id,
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "url": url,
                        "stateful": stateful,
                        "submit_only": false,
                    }),
                );
                anyhow::bail!(
                    "chromium: timeout waiting for response (tab={tab_id} turn={turn_id})"
                );
            }
        }
    }

    async fn do_submit_only(
        &self,
        endpoint_id: &str,
        tab_id: u32,
        url: &str,
        stateful: bool,
        prompt: &str,
        timeout_secs: u64,
    ) -> Result<LlmResponse> {
        let StartedTurn {
            turn_id,
            ack_rx,
            resp_rx: _,
            early_fail_rx: _,
        } = self
            .start_turn(endpoint_id, tab_id, url, prompt, stateful, true)
            .await;
        let ack_timeout_secs = endpoint_submit_ack_timeout_secs(endpoint_id, timeout_secs);
        let ack_raw = self
            .await_submit_ack(
                endpoint_id,
                tab_id,
                turn_id,
                url,
                stateful,
                true,
                ack_timeout_secs,
                std::time::Duration::from_secs(ack_timeout_secs),
                ack_rx,
            )
            .await?;
        Ok(LlmResponse {
            raw: ack_raw,
            tab_id: Some(tab_id),
            turn_id: Some(turn_id),
        })
    }

    async fn acquire_tab_and_url(
        &self,
        endpoint_id: &str,
        urls: &[String],
        timeout_secs: u64,
        stateful: bool,
    ) -> Result<(u32, String)> {
        let tab_id = self
            .acquire_tab(endpoint_id, urls, timeout_secs, stateful)
            .await?;
        let url = {
            let st = self.state.lock().await;
            st.tab_urls
                .get(&tab_id)
                .cloned()
                .unwrap_or_else(|| urls.first().cloned().unwrap_or_default())
        };
        Ok((tab_id, url))
    }

    async fn start_turn(
        &self,
        endpoint_id: &str,
        tab_id: u32,
        url: &str,
        prompt: &str,
        stateful: bool,
        submit_only: bool,
    ) -> StartedTurn {
        let turn_id = self.next_turn_id.fetch_add(1, Ordering::SeqCst);
        let lease_seed = self.next_turn_lease_seed.fetch_add(1, Ordering::SeqCst);
        let lease_token = next_turn_lease_token(lease_seed, turn_id);
        let (ack_tx, ack_rx) = oneshot::channel::<String>();
        let (resp_tx, resp_rx) = oneshot::channel::<String>();
        let (early_fail_tx, early_fail_rx) = oneshot::channel::<String>();

        let mut st = self.state.lock().await;
        st.pending_ack.insert((tab_id, turn_id), ack_tx);
        st.pending_turn_id.insert(tab_id, turn_id);
        st.pending_turn_lease
            .insert((tab_id, turn_id), lease_token.clone());
        if !submit_only {
            st.pending_resp.insert((tab_id, turn_id), resp_tx);
            st.pending_early_fail.insert((tab_id, turn_id), early_fail_tx);
        }

        let site = SiteType::from_url(url);
        st.assemblers
            .entry(tab_id)
            .and_modify(|a| {
                a.set_site(site);
                a.reset();
            })
            .or_insert_with(|| FrameAssembler::new(site));

        let prompt_bytes = prompt.len();
        let frame = json!({ "type": "TURN", "tabId": tab_id, "text": prompt, "turnId": turn_id, "leaseToken": &lease_token });
        append_outbound_event(
            "OUTBOUND_SUBMIT_TRY",
            json!({
                "endpoint_id": endpoint_id,
                "tabId": tab_id,
                "turnId": turn_id,
                "leaseToken": &lease_token,
                "url": url,
                "prompt_bytes": prompt_bytes,
                "stateful": stateful,
                "submit_only": submit_only,
            }),
        );
        if !st.send_msg(frame.clone()) {
            st.replay_queue.push(frame);
            append_outbound_event(
                "OUTBOUND_SUBMIT_QUEUED",
                json!({
                    "endpoint_id": endpoint_id,
                    "tabId": tab_id,
                    "turnId": turn_id,
                    "leaseToken": &lease_token,
                    "url": url,
                    "prompt_bytes": prompt_bytes,
                    "stateful": stateful,
                    "submit_only": submit_only,
                }),
            );
        } else {
            append_outbound_event(
                "OUTBOUND_SUBMIT_SENT",
                json!({
                    "endpoint_id": endpoint_id,
                    "tabId": tab_id,
                    "turnId": turn_id,
                    "leaseToken": &lease_token,
                    "url": url,
                    "prompt_bytes": prompt_bytes,
                    "stateful": stateful,
                    "submit_only": submit_only,
                }),
            );
        }

        StartedTurn {
            turn_id,
            ack_rx,
            resp_rx: (!submit_only).then_some(resp_rx),
            early_fail_rx: (!submit_only).then_some(early_fail_rx),
        }
    }

    async fn await_submit_ack(
        &self,
        endpoint_id: &str,
        tab_id: u32,
        turn_id: u64,
        url: &str,
        stateful: bool,
        submit_only: bool,
        ack_timeout_secs: u64,
        ack_wait: std::time::Duration,
        ack_rx: oneshot::Receiver<String>,
    ) -> Result<String> {
        match tokio::time::timeout(ack_wait, ack_rx).await {
            Ok(Ok(ack_raw)) => Ok(ack_raw),
            _ => {
                let mut st = self.state.lock().await;
                append_inbound_boundary_event(
                    &mut st,
                    tab_id,
                    turn_id,
                    endpoint_id,
                    "submit_ack_timeout",
                    "chromium: timeout waiting for SUBMIT_ACK",
                    stateful,
                    submit_only,
                );
                st.pending_ack.remove(&(tab_id, turn_id));
                st.pending_resp.remove(&(tab_id, turn_id));
                st.pending_early_fail.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                st.pending_turn_lease.remove(&(tab_id, turn_id));
                retire_tab_locked(&mut st, endpoint_id, tab_id, stateful);
                append_outbound_event(
                    "OUTBOUND_SUBMIT_ACK_TIMEOUT",
                    json!({
                        "endpoint_id": endpoint_id,
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "url": url,
                        "ack_timeout_secs": ack_timeout_secs,
                        "stateful": stateful,
                        "submit_only": submit_only,
                    }),
                );
                anyhow::bail!(
                    "chromium: timeout waiting for SUBMIT_ACK (tab={tab_id} turn={turn_id})"
                );
            }
        }
    }
}

#[async_trait]
impl LlmBackend for ChromiumBackend {
    async fn send(
        &self,
        endpoint_id: &str,
        urls: &[String],
        stateful: bool,
        prompt: &str,
        system_schema: &str,
        submit_only: bool,
        timeout_secs: Option<u64>,
    ) -> Result<LlmResponse> {
        let timeout = timeout_secs.unwrap_or(150);

        let full_prompt = if system_schema.trim().is_empty() {
            prompt.to_string()
        } else {
            format!("{}\n\n{}", system_schema.trim_end(), prompt)
        };

        let (tab_id, url) = self
            .acquire_tab_and_url(endpoint_id, urls, timeout, stateful)
            .await?;

        if submit_only {
            return self
                .do_submit_only(endpoint_id, tab_id, &url, stateful, &full_prompt, timeout)
                .await;
        }

        self.do_send(endpoint_id, tab_id, &url, &full_prompt, timeout, stateful)
            .await
    }

    async fn take_completed_turns(&self) -> Vec<Value> {
        let mut st = self.state.lock().await;
        st.completed_turns.drain(..).collect()
    }
}

// ---------------------------------------------------------------------------
// WebSocket server
// ---------------------------------------------------------------------------

async fn accept_loop(addr: SocketAddr, state: Arc<Mutex<State>>) {
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                eprintln!("[chromium] WebSocket server listening on {addr}");
                loop {
                    match listener.accept().await {
                        Ok((stream, _peer)) => {
                            let st = state.clone();
                            tokio::spawn(handle_connection(stream, st));
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(e) => {
                eprintln!("[chromium] bind {addr} failed: {e} — retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

}

async fn handle_connection(stream: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };

    let (mut sink, mut source) = ws.split();
    let (tx_out, mut rx_out) = mpsc::channel::<Message>(256);

    {
        let mut st = state.lock().await;
        st.out_tx = Some(tx_out.clone());
        eprintln!("[chromium] extension connected");
        for frame in st.replay_queue.drain(..) {
            let _ = tx_out.try_send(Message::Text(frame.to_string().into()));
        }
    }

    let sink_task = tokio::spawn(async move {
        while let Some(msg) = rx_out.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(result) = source.next().await {
        match result {
            Ok(Message::Text(text)) => handle_inbound(text.as_str(), &state).await,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    sink_task.abort();
    let mut st = state.lock().await;
    st.out_tx = None;
    eprintln!("[chromium] extension disconnected");
}

async fn handle_inbound(raw: &str, state: &Arc<Mutex<State>>) {
    let msg: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            append_jsonl("all.jsonl", &json!({ "type": "PARSE_ERROR", "raw": raw }));
            return;
        }
    };
    let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Log every non-PING frame so we can see exactly what the extension sends.
    if msg_type != "PING" {
        append_jsonl("all.jsonl", &msg);
    }

    match msg_type {
        "PING" => {}

        // A tab has navigated to a ChatGPT URL after OPEN_TAB.
        "TAB_OPENED" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            let url = msg
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut st = state.lock().await;
            st.tab_urls.insert(tab_id, url.clone());
            let site = SiteType::from_url(&url);
            st.assemblers
                .entry(tab_id)
                .or_insert_with(|| FrameAssembler::new(site));
        }

        // A tab is ready (content script loaded).
        "TAB_READY" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            let url = msg
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let original_url = msg
                .get("originalUrl")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let req_id = msg.get("reqId").and_then(|v| v.as_u64());

            let mut st = state.lock().await;
            let site = SiteType::from_url(&url);
            st.assemblers
                .entry(tab_id)
                .and_modify(|a| a.set_site(site))
                .or_insert_with(|| FrameAssembler::new(site));
            st.tab_urls.insert(tab_id, url.clone());
            eprintln!("[chromium] TAB_READY tab={tab_id} url={url}");

            if let Some(rid) = req_id {
                // This was a tab we opened via OPEN_TAB.
                if let Some(tx) = st.pending_open.remove(&rid) {
                    let _ = tx.send(tab_id);
                }
            } else {
                // Organic tab (pre-existing or sw-restart re-announcement).
                let queue = st.preopened.entry(url.clone()).or_default();
                if !queue.contains(&tab_id) {
                    queue.push_back(tab_id);
                }
                if let Some(orig) = original_url {
                    if orig != url {
                        let q2 = st.preopened.entry(orig).or_default();
                        if !q2.contains(&tab_id) {
                            q2.push_back(tab_id);
                        }
                    }
                }
            }
        }

        "TAB_CLOSED" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            let mut st = state.lock().await;
            st.tab_urls.remove(&tab_id);
            st.assemblers.remove(&tab_id);
            if let Some(endpoint_id) = st.tab_owners.remove(&tab_id) {
                st.endpoint_tabs.remove(&endpoint_id);
            }
            for q in st.preopened.values_mut() {
                q.retain(|id| *id != tab_id);
            }
            st.pending_turn_id.remove(&tab_id);
            st.pending_turn_lease.retain(|(tid, _), _| *tid != tab_id);
            st.pending_ack.retain(|(tid, _), _| *tid != tab_id);
            st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
            st.pending_early_fail.retain(|(tid, _), _| *tid != tab_id);
            st.turn_complete_seen.retain(|(tid, _), _| *tid != tab_id);
            st.post_complete_presence.retain(|(tid, _), _| *tid != tab_id);
            st.post_complete_heartbeat.retain(|(tid, _), _| *tid != tab_id);
            st.user_message_seen.retain(|(tid, _), _| *tid != tab_id);
            st.post_user_message_heartbeat
                .retain(|(tid, _), _| *tid != tab_id);
        }

        "SUBMIT_ACK" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            let payload_value = payload_value_from_message(&msg);
            let turn_id = match turn_id_from_value(&msg)
                .or_else(|| payload_value.as_ref().and_then(turn_id_from_value))
            {
                Some(id) => id,
                None => return,
            };
            let ack_lease_token = match lease_token_from_value(&msg)
                .or_else(|| payload_value.as_ref().and_then(lease_token_from_value))
            {
                Some(token) => token,
                None => return,
            };
            let mut st = state.lock().await;
            let Some(expected_lease_token) = st.pending_turn_lease.get(&(tab_id, turn_id)).cloned() else {
                return;
            };
            if ack_lease_token != expected_lease_token {
                append_outbound_event(
                    "OUTBOUND_SUBMIT_ACK_IGNORED",
                    json!({
                        "reason": "lease_token_mismatch",
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "leaseToken": ack_lease_token,
                        "expectedLeaseToken": expected_lease_token,
                    }),
                );
                return;
            }
            if let Some(tx) = st.pending_ack.remove(&(tab_id, turn_id)) {
                let _ = tx.send(submit_ack_payload(
                    tab_id,
                    turn_id,
                    &expected_lease_token,
                    "submit_ack",
                ));
            }
            append_outbound_event(
                "OUTBOUND_SUBMIT_ACK",
                json!({
                    "tabId": tab_id,
                    "turnId": turn_id,
                    "leaseToken": expected_lease_token,
                }),
            );
        }

        "INBOUND_MESSAGE" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            // payload arrives as a JSON string: {"turn_id":N,"chunk":"data: ...","ts":N}
            // Extract the inner `chunk` field (the raw SSE line) for the assembler.
            let payload_raw = match msg.get("payload").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return,
            };
            let mut chunk = payload_raw.clone();
            let mut inbound_turn_id: Option<u64> = None;
            let mut inbound_lease_token: Option<String> = lease_token_from_value(&msg);
            if payload_raw.trim_start().starts_with('{') {
                if let Ok(v) = serde_json::from_str::<Value>(&payload_raw) {
                    if let Some(c) = v.get("chunk").and_then(|c| c.as_str()) {
                        chunk = c.to_string();
                    }
                    inbound_turn_id = v.get("turn_id").and_then(|t| t.as_u64());
                    inbound_lease_token = lease_token_from_value(&v);
                }
            }

            let mut st = state.lock().await;
            let expected_turn_id = st.pending_turn_id.get(&tab_id).copied();
            let owned_endpoint_id = st.tab_owners.get(&tab_id).cloned();
            let transport_signal = classify_transport_signal(&chunk);

            // Log every inbound chunk with full context.
            st.frame_counter += 1;
            append_jsonl(
                "inbound.jsonl",
                &json!({
                    "frame_counter": st.frame_counter,
                    "tab_id": tab_id,
                    "inbound_turn_id": inbound_turn_id,
                    "inbound_lease_token": inbound_lease_token,
                    "expected_turn_id": expected_turn_id,
                    "transport_signal": transport_signal,
                    "chunk": chunk,
                    "payload_raw_len": payload_raw.len(),
                }),
            );

            let expected_turn_id = match expected_turn_id {
                Some(id) => id,
                None => {
                    // Reject unsolicited inbound frames from tabs that the backend does not
                    // currently own. This prevents unrelated ChatGPT tabs (including duplicate
                    // URLs open in other browser tabs) from leaking completions into the runtime.
                    //
                    // Owned/stateful tabs are still allowed to complete via an inbound turn_id
                    // after a submit-only flow has already cleared its pending turn bookkeeping.
                    if owned_endpoint_id.is_none() {
                        append_outbound_event(
                            "OUTBOUND_INBOUND_IGNORED",
                            json!({
                                "reason": "unowned_tab_without_pending_turn",
                                "tabId": tab_id,
                                "inbound_turn_id": inbound_turn_id,
                                "frame_counter": st.frame_counter,
                            }),
                        );
                        return;
                    }
                    match inbound_turn_id {
                        Some(id) => id,
                        None => return,
                    }
                }
            };
            // Drop frames whose turn_id doesn't match what we're waiting for.
            if let Some(itid) = inbound_turn_id {
                if itid != expected_turn_id {
                    return;
                }
            }
            let turn_id = expected_turn_id;
            let key = (tab_id, turn_id);
            let Some(expected_lease_token) = st.pending_turn_lease.get(&key).cloned() else {
                append_outbound_event(
                    "OUTBOUND_INBOUND_IGNORED",
                    json!({
                        "reason": "missing_turn_lease",
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "frame_counter": st.frame_counter,
                    }),
                );
                return;
            };
            let Some(actual_lease_token) = inbound_lease_token else {
                append_outbound_event(
                    "OUTBOUND_INBOUND_IGNORED",
                    json!({
                        "reason": "missing_lease_token",
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "frame_counter": st.frame_counter,
                    }),
                );
                return;
            };
            if actual_lease_token != expected_lease_token {
                append_outbound_event(
                    "OUTBOUND_INBOUND_IGNORED",
                    json!({
                        "reason": "lease_token_mismatch",
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "leaseToken": actual_lease_token,
                        "expectedLeaseToken": expected_lease_token,
                        "frame_counter": st.frame_counter,
                    }),
                );
                return;
            }

            match transport_signal {
                Some("assistant_message_add") => {
                    // Do not clear post-turn-complete stall evidence just because an assistant
                    // envelope appeared on the wire. In the observed failure mode, ChatGPT emits
                    // a non-terminal `calpico-message-add` and then keeps sending heartbeats /
                    // presence without ever producing an assembled terminal snapshot. Clearing the
                    // counters here disables the only deterministic early-fail path and leaves the
                    // caller stuck until the outer wall-clock timeout.
                    st.user_message_seen.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    append_outbound_event(
                        "OUTBOUND_EARLY_SIGNAL",
                        json!({
                            "signal": "assistant_message_add_before_terminal_assembly",
                            "tabId": tab_id,
                            "turnId": turn_id,
                            "frame_counter": st.frame_counter,
                            "turn_complete_seen": st.turn_complete_seen.contains_key(&key),
                            "response_pending": st.pending_resp.contains_key(&key),
                        }),
                    );
                }
                Some("user_message_add") if st.pending_resp.contains_key(&key) => {
                    let frame_counter = st.frame_counter;
                    st.user_message_seen.entry(key).or_insert(frame_counter);
                    st.post_user_message_heartbeat.insert(key, 0);
                    append_outbound_event(
                        "OUTBOUND_EARLY_SIGNAL",
                        json!({
                            "signal": "user_message_add_before_assistant_terminal",
                            "tabId": tab_id,
                            "turnId": turn_id,
                            "frame_counter": frame_counter,
                        }),
                    );
                }
                Some("turn_complete") if st.pending_resp.contains_key(&key) => {
                    let frame_counter = st.frame_counter;
                    st.turn_complete_seen.entry(key).or_insert(frame_counter);
                    st.post_complete_presence.insert(key, 0);
                    st.post_complete_heartbeat.insert(key, 0);
                    st.user_message_seen.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    append_outbound_event(
                        "OUTBOUND_EARLY_SIGNAL",
                        json!({
                            "signal": "turn_complete_before_assistant_terminal",
                            "tabId": tab_id,
                            "turnId": turn_id,
                            "frame_counter": frame_counter,
                        }),
                    );
                }
                Some("presence") if st.pending_resp.contains_key(&key) => {
                    if let Some(turn_complete_frame) = st.turn_complete_seen.get(&key).copied() {
                        let frame_counter = st.frame_counter;
                        let count = st
                            .post_complete_presence
                            .entry(key)
                            .and_modify(|seen| *seen += 1)
                            .or_insert(1);
                        let presence_count = *count;
                        if presence_count == 3 {
                            if let Some(tx) = st.pending_early_fail.remove(&key) {
                                let _ = tx.send("presence_after_turn_complete".to_string());
                            }
                            append_outbound_event(
                                "OUTBOUND_EARLY_SIGNAL",
                                json!({
                                    "signal": "presence_after_turn_complete",
                                    "tabId": tab_id,
                                    "turnId": turn_id,
                                    "frame_counter": frame_counter,
                                    "turn_complete_frame_counter": turn_complete_frame,
                                    "presence_count": presence_count,
                                }),
                            );
                        }
                    }
                }
                Some("heartbeat") if st.pending_resp.contains_key(&key) => {
                    if let Some(turn_complete_frame) = st.turn_complete_seen.get(&key).copied() {
                        let count = st
                            .post_complete_heartbeat
                            .entry(key)
                            .and_modify(|seen| *seen += 1)
                            .or_insert(1);
                        let heartbeat_count = *count;
                        if heartbeat_count == 8 {
                            if let Some(tx) = st.pending_early_fail.remove(&key) {
                                let _ = tx.send("heartbeat_after_turn_complete".to_string());
                            }
                        }
                        append_outbound_event(
                            "OUTBOUND_EARLY_SIGNAL",
                            json!({
                                "signal": "heartbeat_after_turn_complete",
                                "tabId": tab_id,
                                "turnId": turn_id,
                                "frame_counter": st.frame_counter,
                                "turn_complete_frame_counter": turn_complete_frame,
                                "heartbeat_count": heartbeat_count,
                            }),
                        );
                    } else if let Some(user_message_frame) = st.user_message_seen.get(&key).copied() {
                        if chunk.contains("\"calpico-is-responding-heartbeat\"") {
                            st.user_message_seen.remove(&key);
                            st.post_user_message_heartbeat.remove(&key);
                            append_outbound_event(
                                "OUTBOUND_EARLY_SIGNAL",
                                json!({
                                    "signal": "responding_heartbeat_before_turn_complete",
                                    "tabId": tab_id,
                                    "turnId": turn_id,
                                    "frame_counter": st.frame_counter,
                                    "user_message_frame_counter": user_message_frame,
                                    "suppressed_threshold": PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD,
                                }),
                            );
                        } else {
                            let count = st
                                .post_user_message_heartbeat
                                .entry(key)
                                .and_modify(|seen| *seen += 1)
                                .or_insert(1);
                            let heartbeat_count = *count;
                            if heartbeat_count == PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD {
                                if let Some(tx) = st.pending_early_fail.remove(&key) {
                                    let _ = tx.send(
                                        "heartbeat_after_user_echo_before_turn_complete".to_string(),
                                    );
                                }
                            }
                            append_outbound_event(
                                "OUTBOUND_EARLY_SIGNAL",
                                json!({
                                    "signal": "heartbeat_after_user_echo_before_turn_complete",
                                    "tabId": tab_id,
                                    "turnId": turn_id,
                                    "frame_counter": st.frame_counter,
                                    "user_message_frame_counter": user_message_frame,
                                    "heartbeat_count": heartbeat_count,
                                    "threshold": PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD,
                                }),
                            );
                        }
                    }
                }
                _ => {}
            }

            if let Some(tx) = st.pending_ack.remove(&(tab_id, turn_id)) {
                let _ = tx.send(submit_ack_payload(
                    tab_id,
                    turn_id,
                    &expected_lease_token,
                    "inbound_message",
                ));
                append_outbound_event(
                    "OUTBOUND_SUBMIT_ACK_SYNTHETIC",
                    json!({
                        "tabId": tab_id,
                        "turnId": turn_id,
                        "leaseToken": expected_lease_token,
                        "source": "inbound_message",
                    }),
                );
            }

            let assembled = if let Some(asm) = st.assemblers.get_mut(&tab_id) {
                asm.push(&chunk)
            } else {
                None
            };

            if let Some(text) = assembled {
                let preview_end = text
                    .char_indices()
                    .map(|(idx, _)| idx)
                    .nth(200)
                    .unwrap_or(text.len());
                append_jsonl(
                    "assembled.jsonl",
                    &json!({
                        "tab_id": tab_id,
                        "turn_id": turn_id,
                        "text_len": text.len(),
                        "text_preview": &text[..preview_end],
                    }),
                );
                if let Some(tx) = st.pending_resp.remove(&(tab_id, turn_id)) {
                    st.pending_turn_id.remove(&tab_id);
                    st.pending_turn_lease.remove(&key);
                    st.turn_complete_seen.remove(&key);
                    st.post_complete_presence.remove(&key);
                    st.post_complete_heartbeat.remove(&key);
                    st.user_message_seen.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    let endpoint_id = st.tab_owners.get(&tab_id).cloned();
                    if let Some(endpoint_id) = endpoint_id.as_deref() {
                        release_tab_locked(&mut st, endpoint_id, tab_id, true);
                    } else {
                        let tab_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
                        st.preopened.entry(tab_url).or_default().push_back(tab_id);
                    }
                    let _ = tx.send(text);
                } else {
                    let endpoint_id = st.tab_owners.get(&tab_id).cloned();
                    st.pending_turn_id.remove(&tab_id);
                    st.pending_turn_lease.remove(&key);
                    st.turn_complete_seen.remove(&key);
                    st.post_complete_presence.remove(&key);
                    st.post_complete_heartbeat.remove(&key);
                    st.user_message_seen.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    if let Some(owner) = endpoint_id.as_deref() {
                        release_tab_locked(&mut st, owner, tab_id, true);
                    } else {
                        let tab_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
                        st.preopened.entry(tab_url).or_default().push_back(tab_id);
                    }
                    st.completed_turns.push_back(json!({
                        "tab_id": tab_id,
                        "turn_id": turn_id,
                        "text": text,
                        "endpoint_id": endpoint_id,
                    }));
                }
            }
        }

        _ => {}
    }
}

fn release_tab_locked(st: &mut State, endpoint_id: &str, tab_id: u32, stateful: bool) {
    if stateful {
        st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
        st.tab_owners.insert(tab_id, endpoint_id.to_string());
        return;
    }
    let tab_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
    st.preopened.entry(tab_url).or_default().push_back(tab_id);
}

fn retire_tab_locked(st: &mut State, endpoint_id: &str, tab_id: u32, stateful: bool) {
    st.pending_turn_id.remove(&tab_id);
    st.pending_turn_lease.retain(|(tid, _), _| *tid != tab_id);
    st.pending_ack.retain(|(tid, _), _| *tid != tab_id);
    st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
    st.pending_early_fail.retain(|(tid, _), _| *tid != tab_id);
    st.turn_complete_seen.retain(|(tid, _), _| *tid != tab_id);
    st.post_complete_presence.retain(|(tid, _), _| *tid != tab_id);
    st.post_complete_heartbeat.retain(|(tid, _), _| *tid != tab_id);
    st.user_message_seen.retain(|(tid, _), _| *tid != tab_id);
    st.post_user_message_heartbeat
        .retain(|(tid, _), _| *tid != tab_id);
    st.assemblers.remove(&tab_id);

    if stateful {
        if st.endpoint_tabs.get(endpoint_id).copied() == Some(tab_id) {
            st.endpoint_tabs.remove(endpoint_id);
        }
        st.tab_owners.remove(&tab_id);
    }

    for queue in st.preopened.values_mut() {
        queue.retain(|id| *id != tab_id);
    }
}

fn reconcile_tab_state_locked(
    st: &mut State,
    endpoint_id: Option<&str>,
    requested_urls: &[String],
) -> usize {
    let known_tab_ids: HashSet<u32> = st.tab_urls.keys().copied().collect();
    st.tab_owners
        .retain(|tab_id, _| known_tab_ids.contains(tab_id));

    let owned_by_tab = st.tab_owners.clone();
    st.endpoint_tabs.retain(|endpoint, tab_id| {
        known_tab_ids.contains(tab_id)
            && owned_by_tab
                .get(tab_id)
                .map(String::as_str)
                == Some(endpoint.as_str())
    });

    let owned_tab_ids: HashSet<u32> = st.tab_owners.keys().copied().collect();
    let mut seen_preopened = HashSet::new();
    st.preopened.retain(|_, queue| {
        queue.retain(|tab_id| {
            known_tab_ids.contains(tab_id)
                && !owned_tab_ids.contains(tab_id)
                && seen_preopened.insert(*tab_id)
        });
        !queue.is_empty()
    });

    let mut recovered = 0usize;
    if let Some(endpoint_id) = endpoint_id {
        if !st.endpoint_tabs.contains_key(endpoint_id) {
            if let Some(tab_id) = st.tab_owners.iter().find_map(|(tab_id, owner)| {
                (owner == endpoint_id && known_tab_ids.contains(tab_id)).then_some(*tab_id)
            }) {
                st.endpoint_tabs.insert(endpoint_id.to_string(), tab_id);
                recovered += 1;
            }
        }
    }

    let tab_snapshot: Vec<(u32, String)> = st
        .tab_urls
        .iter()
        .map(|(tab_id, url)| (*tab_id, url.clone()))
        .collect();
    for (tab_id, url) in tab_snapshot {
        if st.tab_owners.contains_key(&tab_id) {
            continue;
        }
        if !requested_urls.is_empty() && !requested_urls.iter().any(|candidate| candidate == &url) {
            continue;
        }
        let queue = st.preopened.entry(url).or_default();
        if !queue.contains(&tab_id) {
            queue.push_back(tab_id);
            recovered += 1;
        }
    }

    recovered
}

#[cfg(test)]
mod tests {
    use super::{
        endpoint_submit_ack_timeout_secs, handle_inbound,
        PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD, State,
    };
    use crate::llm_runtime::parsers::{FrameAssembler, SiteType};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio::sync::{oneshot, Mutex};

    #[test]
    fn submit_ack_timeout_is_endpoint_aware() {
        assert_eq!(endpoint_submit_ack_timeout_secs("solo_chatgpt", 900), 60);
        assert_eq!(endpoint_submit_ack_timeout_secs("planner_chatgpt", 900), 60);
        assert_eq!(endpoint_submit_ack_timeout_secs("verifier_chatgpt", 180), 30);
        assert_eq!(endpoint_submit_ack_timeout_secs("executor_pool", 30), 15);
    }

    #[test]
    fn submit_ack_timeout_never_exceeds_total_timeout() {
        assert_eq!(endpoint_submit_ack_timeout_secs("solo_chatgpt", 10), 10);
        assert_eq!(endpoint_submit_ack_timeout_secs("executor_pool", 5), 5);
    }

    #[tokio::test]
    async fn submit_ack_accepts_snake_case_turn_id() {
        let state = Arc::new(Mutex::new(State::new()));
        let (ack_tx, ack_rx) = oneshot::channel::<String>();
        {
            let mut st = state.lock().await;
            st.pending_ack.insert((7, 11), ack_tx);
            st.pending_turn_id.insert(7, 11);
            st.pending_turn_lease
                .insert((7, 11), "lease-expected".to_string());
        }

        let raw = json!({
            "type": "SUBMIT_ACK",
            "tabId": 7,
            "turn_id": 11,
            "lease_token": "lease-expected",
        })
        .to_string();

        handle_inbound(&raw, &state).await;

        let ack = ack_rx.await.expect("submit ack should be delivered");
        let parsed: Value = serde_json::from_str(&ack).expect("ack payload should be valid json");
        assert_eq!(parsed.get("turn_id").and_then(|v| v.as_u64()), Some(11));
        assert_eq!(
            parsed.get("lease_token").and_then(|v| v.as_str()),
            Some("lease-expected")
        );
    }

    #[tokio::test]
    async fn submit_ack_accepts_payload_wrapped_fields() {
        let state = Arc::new(Mutex::new(State::new()));
        let (ack_tx, ack_rx) = oneshot::channel::<String>();
        {
            let mut st = state.lock().await;
            st.pending_ack.insert((7, 11), ack_tx);
            st.pending_turn_id.insert(7, 11);
            st.pending_turn_lease
                .insert((7, 11), "lease-expected".to_string());
        }

        let raw = json!({
            "type": "SUBMIT_ACK",
            "tabId": 7,
            "payload": {
                "turn_id": 11,
                "lease_token": "lease-expected",
            },
        })
        .to_string();

        handle_inbound(&raw, &state).await;

        let ack = ack_rx.await.expect("payload-wrapped submit ack should be delivered");
        let parsed: Value = serde_json::from_str(&ack).expect("ack payload should be valid json");
        assert_eq!(parsed.get("turn_id").and_then(|v| v.as_u64()), Some(11));
        assert_eq!(
            parsed.get("lease_token").and_then(|v| v.as_str()),
            Some("lease-expected")
        );
    }

    #[tokio::test]
    async fn inbound_message_uses_inbound_turn_id_when_submit_only_ack_cleared_pending_turn() {
        let state = Arc::new(Mutex::new(State::new()));
        let lease_token = "lease-submit-only";
        {
            let mut st = state.lock().await;
            st.tab_urls.insert(7, "https://chatgpt.com/".to_string());
            st.tab_owners.insert(7, "executor_pool".to_string());
            st.pending_turn_lease
                .insert((7, 11), lease_token.to_string());
            st.assemblers
                .insert(7, FrameAssembler::new(SiteType::ChatGptPrivate));
        }

        let raw = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": lease_token,
                "chunk": "data: {\"type\":\"message\",\"content\":{\"parts\":[\"hello\"]}}",
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();

        handle_inbound(&raw, &state).await;

        let mut st = state.lock().await;
        let completed = st
            .completed_turns
            .pop_front()
            .expect("submit-only completion should be queued");
        assert_eq!(completed.get("tab_id").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(completed.get("turn_id").and_then(|v| v.as_u64()), Some(11));
        assert_eq!(completed.get("text").and_then(|v| v.as_str()), Some("hello"));
        assert_eq!(
            completed.get("endpoint_id").and_then(|v| v.as_str()),
            Some("executor_pool")
        );
    }

    #[tokio::test]
    async fn inbound_message_from_unowned_tab_without_pending_turn_is_ignored() {
        let state = Arc::new(Mutex::new(State::new()));
        {
            let mut st = state.lock().await;
            st.tab_urls.insert(7, "https://chatgpt.com/".to_string());
            st.pending_turn_lease
                .insert((7, 11), "lease-unowned".to_string());
            st.assemblers
                .insert(7, FrameAssembler::new(SiteType::ChatGptPrivate));
        }

        let raw = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-unowned",
                "chunk": "data: {\"type\":\"message\",\"content\":{\"parts\":[\"hello\"]}}",
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();

        handle_inbound(&raw, &state).await;

        let st = state.lock().await;
        assert!(
            st.completed_turns.is_empty(),
            "unowned organic tabs must not enqueue completed turns"
        );
    }

    #[tokio::test]
    async fn inbound_message_with_wrong_lease_token_is_ignored() {
        let state = Arc::new(Mutex::new(State::new()));
        {
            let mut st = state.lock().await;
            st.tab_urls.insert(7, "https://chatgpt.com/".to_string());
            st.tab_owners.insert(7, "executor_pool".to_string());
            st.pending_turn_id.insert(7, 11);
            st.pending_turn_lease
                .insert((7, 11), "lease-expected".to_string());
            st.assemblers
                .insert(7, FrameAssembler::new(SiteType::ChatGptPrivate));
        }

        let raw = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-foreign",
                "chunk": "data: {\"type\":\"message\",\"content\":{\"parts\":[\"hello\"]}}",
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();

        handle_inbound(&raw, &state).await;

        let st = state.lock().await;
        assert!(st.completed_turns.is_empty());
        assert!(st.pending_resp.is_empty());
        assert_eq!(st.pending_turn_id.get(&7).copied(), Some(11));
    }

    #[tokio::test]
    async fn assistant_message_add_does_not_clear_turn_complete_stall_tracking() {
        let state = Arc::new(Mutex::new(State::new()));
        let (resp_tx, _resp_rx) = oneshot::channel::<String>();
        let (early_fail_tx, early_fail_rx) = oneshot::channel::<String>();
        {
            let mut st = state.lock().await;
            st.tab_urls.insert(7, "https://chatgpt.com/".to_string());
            st.tab_owners.insert(7, "planner_chatgpt".to_string());
            st.pending_turn_id.insert(7, 11);
            st.pending_turn_lease
                .insert((7, 11), "lease-expected".to_string());
            st.pending_resp.insert((7, 11), resp_tx);
            st.pending_early_fail.insert((7, 11), early_fail_tx);
            st.assemblers
                .insert(7, FrameAssembler::new(SiteType::ChatGptGroup));
        }

        let turn_complete = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-expected",
                "chunk": r#"[{"type":"message","topic_id":"conversations","payload":{"type":"conversation-turn-complete","payload":{"conversation_id":"room"}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&turn_complete, &state).await;

        let assistant_non_terminal = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-expected",
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-message-add","payload":{"message":{"role":"assistant","raw_messages":[{"author":{"role":"assistant"},"channel":"analysis","content":{"parts":["partial"]}}]}}}}]"#,
                "ts": 2,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&assistant_non_terminal, &state).await;

        for ts in 3..=10 {
            let heartbeat = json!({
                "type": "INBOUND_MESSAGE",
                "tabId": 7,
                "payload": json!({
                    "turn_id": 11,
                    "lease_token": "lease-expected",
                    "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room"}}}]"#,
                    "ts": ts,
                })
                .to_string(),
            })
            .to_string();
            handle_inbound(&heartbeat, &state).await;
        }

        let reason = early_fail_rx
            .await
            .expect("stall evidence should trigger deterministic early fail");
        assert_eq!(reason, "heartbeat_after_turn_complete");
    }

    #[tokio::test]
    async fn responding_heartbeats_after_user_echo_count_as_positive_liveness() {
        let state = Arc::new(Mutex::new(State::new()));
        let (resp_tx, _resp_rx) = oneshot::channel::<String>();
        let (early_fail_tx, early_fail_rx) = oneshot::channel::<String>();
        {
            let mut st = state.lock().await;
            st.tab_urls.insert(7, "https://chatgpt.com/".to_string());
            st.tab_owners.insert(7, "planner_chatgpt".to_string());
            st.pending_turn_id.insert(7, 11);
            st.pending_turn_lease
                .insert((7, 11), "lease-expected".to_string());
            st.pending_resp.insert((7, 11), resp_tx);
            st.pending_early_fail.insert((7, 11), early_fail_tx);
            st.assemblers
                .insert(7, FrameAssembler::new(SiteType::ChatGptGroup));
        }

        let user_echo = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-expected",
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-message-add","payload":{"message":{"role":"user","content":{"text":"prompt"}}}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&user_echo, &state).await;

        for ts in 2..=(PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD + 1) {
            let heartbeat = json!({
                "type": "INBOUND_MESSAGE",
                "tabId": 7,
                "payload": json!({
                    "turn_id": 11,
                    "lease_token": "lease-expected",
                    "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room"}}}]"#,
                    "ts": ts,
                })
                .to_string(),
            })
            .to_string();
            handle_inbound(&heartbeat, &state).await;
        }

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), early_fail_rx).await;
        assert!(
            result.is_err(),
            "responding heartbeats should keep the active turn alive before turn_complete"
        );
    }
}
